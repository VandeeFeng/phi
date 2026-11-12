use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::catalog::{
    ModelCatalog, remove_store_source, store_source_sha256, validate_provider,
    validate_store_source,
};
use super::schema::{
    CatalogSource, CatalogStore, MODEL_CATALOG_SCHEMA_VERSION, SourceState, StoredProvider,
};
use super::sources::{
    MODELS_DEV_URL, OPENAI_CODEX_MODELS_URL, OPENROUTER_MODELS_URL, normalize_codex_bytes,
    normalize_models_dev_bytes, normalize_openrouter_bytes,
};

const FRESHNESS: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatalogUpdateResult {
    pub updated: Vec<String>,
    pub unchanged: Vec<String>,
    pub stale: Vec<String>,
    pub errors: Vec<CatalogUpdateError>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogUpdateError {
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone)]
struct SourceRequest {
    pub url: &'static str,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone)]
enum SourceResponse {
    NotModified,
    Modified {
        body: Vec<u8>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

trait CatalogSourceClient: Send + Sync {
    fn fetch(
        &self,
        request: SourceRequest,
    ) -> impl Future<Output = anyhow::Result<SourceResponse>> + Send;
}

impl CatalogSourceClient for reqwest::Client {
    async fn fetch(&self, request: SourceRequest) -> anyhow::Result<SourceResponse> {
        let mut builder = self
            .get(request.url)
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(etag) = request.etag {
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(last_modified) = request.last_modified {
            builder = builder.header(reqwest::header::IF_MODIFIED_SINCE, last_modified);
        }
        let response = builder.send().await?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            return Ok(SourceResponse::NotModified);
        }
        let response = response.error_for_status()?;
        let etag = header(&response, reqwest::header::ETAG);
        let last_modified = header(&response, reqwest::header::LAST_MODIFIED);
        Ok(SourceResponse::Modified {
            body: response.bytes().await?.to_vec(),
            etag,
            last_modified,
        })
    }
}

impl ModelCatalog {
    pub async fn bootstrap_default(&self) -> anyhow::Result<CatalogUpdateResult> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        self.bootstrap(&client).await
    }

    async fn bootstrap<C: CatalogSourceClient>(
        &self,
        client: &C,
    ) -> anyhow::Result<CatalogUpdateResult> {
        if !self.requires_bootstrap() {
            return Ok(CatalogUpdateResult::default());
        }
        if self.offline() {
            anyhow::bail!(
                "model catalog is unavailable: PHI_OFFLINE is enabled and models-store.json is incomplete"
            );
        }
        let result = self.update(client, true).await?;
        if self.requires_bootstrap() {
            let errors = result
                .errors
                .iter()
                .map(|error| format!("{}: {}", error.source, error.message))
                .collect::<Vec<_>>()
                .join("; ");
            anyhow::bail!("model catalog bootstrap failed: {errors}");
        }
        Ok(result)
    }

    pub async fn update_default(&self, force: bool) -> anyhow::Result<CatalogUpdateResult> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        self.update(&client, force).await
    }

    async fn update<C: CatalogSourceClient>(
        &self,
        client: &C,
        force: bool,
    ) -> anyhow::Result<CatalogUpdateResult> {
        if self.offline() {
            anyhow::bail!("model catalog update disabled by PHI_OFFLINE");
        }
        let _update_guard = self.update_lock.lock().await;
        let mut store = read_store(self.store_path())?;
        let now = chrono::Utc::now().timestamp_millis();
        let mut result = CatalogUpdateResult::default();
        let mut commit = false;
        let mut models_dev_updated = false;
        for source in CatalogSource::ALL {
            let invalidate = source == CatalogSource::OpenAiCodex && models_dev_updated;
            match refresh_source(client, source, &mut store, now, force, invalidate).await {
                Ok(SourceUpdate::Updated) => {
                    result.updated.push(source.as_str().to_string());
                    commit = true;
                    models_dev_updated |= source == CatalogSource::ModelsDev;
                }
                Ok(SourceUpdate::Unchanged { checked }) => {
                    result.unchanged.push(source.as_str().to_string());
                    commit |= checked;
                }
                Err(error) => {
                    let source = source.as_str().to_string();
                    result.stale.push(source.clone());
                    result.errors.push(CatalogUpdateError {
                        source,
                        message: error.to_string(),
                    });
                }
            }
        }
        if commit {
            let state = self.state_for_store(&store)?;
            write_store_atomic(self.store_path(), &store)?;
            self.publish_state(state);
        }
        Ok(result)
    }
}

enum SourceUpdate {
    Updated,
    Unchanged { checked: bool },
}

async fn refresh_source<C: CatalogSourceClient>(
    client: &C,
    source: CatalogSource,
    store: &mut CatalogStore,
    now: i64,
    force: bool,
    invalidate: bool,
) -> anyhow::Result<SourceUpdate> {
    let state = store
        .sources
        .get(source.as_str())
        .cloned()
        .unwrap_or_default();
    if !invalidate && !force && is_fresh(&state, now) {
        return Ok(SourceUpdate::Unchanged { checked: false });
    }
    if invalidate {
        store.sources.insert(
            source.as_str().to_string(),
            SourceState {
                sha256: state.sha256.clone(),
                ..SourceState::default()
            },
        );
    }
    let request_state = if invalidate {
        SourceState::default()
    } else {
        state
    };
    let response = client
        .fetch(SourceRequest {
            url: match source {
                CatalogSource::ModelsDev => MODELS_DEV_URL,
                CatalogSource::OpenAiCodex => OPENAI_CODEX_MODELS_URL,
                CatalogSource::OpenRouter => OPENROUTER_MODELS_URL,
            },
            etag: request_state.etag,
            last_modified: request_state.last_modified,
        })
        .await?;
    match response {
        SourceResponse::NotModified if invalidate => {
            anyhow::bail!(
                "{} returned 304 after its dependency changed",
                source.as_str()
            )
        }
        SourceResponse::NotModified => {
            store
                .sources
                .entry(source.as_str().to_string())
                .or_default()
                .checked_at = Some(now);
            Ok(SourceUpdate::Unchanged { checked: true })
        }
        SourceResponse::Modified {
            body,
            etag,
            last_modified,
        } => {
            let sha256 = apply_source_body(store, source, &body)?;
            store.sources.insert(
                source.as_str().to_string(),
                SourceState {
                    checked_at: Some(now),
                    etag,
                    last_modified,
                    sha256: Some(sha256),
                },
            );
            Ok(SourceUpdate::Updated)
        }
    }
}

fn apply_source_body(
    store: &mut CatalogStore,
    source: CatalogSource,
    body: &[u8],
) -> anyhow::Result<String> {
    let providers = match source {
        CatalogSource::ModelsDev => normalize_models_dev_bytes(body)?,
        CatalogSource::OpenAiCodex => {
            let openai = &store
                .providers
                .get("openai")
                .ok_or_else(|| anyhow::anyhow!("models.dev OpenAI catalog is unavailable"))?
                .models;
            BTreeMap::from([(
                "openai-codex".to_string(),
                normalize_codex_bytes(body, openai)?,
            )])
        }
        CatalogSource::OpenRouter => {
            BTreeMap::from([("openrouter".to_string(), normalize_openrouter_bytes(body)?)])
        }
    };
    // Stabilize normalized floats before hashing and persisting the source partition.
    let providers: super::sources::NormalizedCatalog =
        serde_json::from_slice(&serde_json::to_vec(&providers)?)?;
    for (provider, models) in &providers {
        if models.is_empty() {
            anyhow::bail!("{source:?} returned no models for {provider}");
        }
        validate_provider(provider, models)?;
    }
    for (provider, models) in providers {
        store.providers.insert(provider, StoredProvider { models });
    }
    store_source_sha256(store, source.as_str())
}

fn is_fresh(state: &SourceState, now: i64) -> bool {
    state
        .checked_at
        .is_some_and(|checked| now.saturating_sub(checked) < FRESHNESS.as_millis() as i64)
}

fn read_store(path: &Path) -> anyhow::Result<CatalogStore> {
    if !path.exists() {
        return Ok(empty_store());
    }
    let Ok(mut store) = serde_json::from_slice::<CatalogStore>(&std::fs::read(path)?) else {
        return Ok(empty_store());
    };
    if store.schema_version != MODEL_CATALOG_SCHEMA_VERSION {
        return Ok(empty_store());
    }
    for source in CatalogSource::ALL.map(CatalogSource::as_str) {
        if validate_store_source(&store, source).is_err() {
            remove_store_source(&mut store, source)?;
        }
    }
    Ok(store)
}

fn empty_store() -> CatalogStore {
    CatalogStore {
        schema_version: MODEL_CATALOG_SCHEMA_VERSION,
        sources: BTreeMap::new(),
        providers: BTreeMap::new(),
    }
}

fn write_store_atomic(path: &Path, store: &CatalogStore) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("model store path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".models-store-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        serde_json::to_writer_pretty(&mut file, store)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn header(response: &reqwest::Response, name: reqwest::header::HeaderName) -> Option<String> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use super::*;
    use crate::models::ModelCatalogOptions;

    enum FakeResponse {
        Ok(SourceResponse),
        Err(&'static str),
    }

    struct FakeClient {
        responses: Mutex<VecDeque<FakeResponse>>,
        requests: Mutex<Vec<SourceRequest>>,
    }

    impl FakeClient {
        fn new(responses: impl IntoIterator<Item = FakeResponse>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().collect()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl CatalogSourceClient for FakeClient {
        async fn fetch(&self, request: SourceRequest) -> anyhow::Result<SourceResponse> {
            self.requests.lock().unwrap().push(request);
            match self.responses.lock().unwrap().pop_front().unwrap() {
                FakeResponse::Ok(response) => Ok(response),
                FakeResponse::Err(message) => anyhow::bail!(message),
            }
        }
    }

    fn models_dev_body() -> Vec<u8> {
        models_dev_body_with_cost(1.0)
    }

    fn models_dev_body_with_cost(input_cost: f64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "openai": { "models": {
                "updated-openai": {
                    "name": "Updated OpenAI", "tool_call": true,
                    "limit": { "context": 100000, "output": 10000 }
                },
                "gpt-5.5": {
                    "name": "GPT-5.5", "tool_call": true, "reasoning": true,
                    "reasoning_options": [{ "type": "effort", "values": ["low", "xhigh"] }],
                    "modalities": { "input": ["text", "image"] },
                    "limit": { "context": 1050000, "output": 128000 },
                    "cost": { "input": input_cost, "output": 20.0 }
                }
            }},
            "anthropic": { "models": { "updated-anthropic": {
                "tool_call": true, "limit": { "context": 100000, "output": 10000 }
            }}},
            "google": { "models": { "updated-gemini": {
                "tool_call": true, "limit": { "context": 100000, "output": 10000 }
            }}},
            "deepseek": { "models": { "updated-deepseek": {
                "tool_call": true, "limit": { "context": 100000, "output": 10000 }
            }}}
        }))
        .unwrap()
    }

    fn codex_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "models": [{
            "slug": "gpt-5.5", "display_name": "GPT-5.5", "visibility": "list",
            "context_window": 272000, "input_modalities": ["text", "image"],
            "supported_reasoning_levels": [{ "effort": "low" }, { "effort": "xhigh" }]
        }] }))
        .unwrap()
    }

    fn openrouter_body() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({ "data": [{
            "id": "updated/openrouter", "name": "Updated OpenRouter",
            "supported_parameters": ["tools"],
            "pricing": { "prompt": "0.0000008", "completion": "0.0000016" },
            "context_length": 64000,
            "top_provider": { "max_completion_tokens": 8000 }
        }] }))
        .unwrap()
    }

    fn modified(body: Vec<u8>, etag: &str) -> FakeResponse {
        FakeResponse::Ok(SourceResponse::Modified {
            body,
            etag: Some(etag.to_string()),
            last_modified: Some("Wed, 20 Aug 2026 00:00:00 GMT".to_string()),
        })
    }

    fn catalog() -> (ModelCatalog, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("phi-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let catalog = ModelCatalog::load(ModelCatalogOptions {
            store_path: root.join("models-store.json"),
            user_paths: Vec::new(),
            offline: false,
        })
        .unwrap();
        (catalog, root)
    }

    #[tokio::test]
    async fn offline_mode_blocks_updates_without_requests() {
        let root = std::env::temp_dir().join(format!("phi-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let catalog = ModelCatalog::load(ModelCatalogOptions {
            store_path: root.join("models-store.json"),
            user_paths: Vec::new(),
            offline: true,
        })
        .unwrap();
        let client = FakeClient::new([]);

        assert_eq!(
            catalog.bootstrap(&client).await.unwrap_err().to_string(),
            "model catalog is unavailable: PHI_OFFLINE is enabled and models-store.json is incomplete"
        );
        assert_eq!(
            catalog.update(&client, true).await.unwrap_err().to_string(),
            "model catalog update disabled by PHI_OFFLINE"
        );
        assert!(client.requests.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn first_run_bootstraps_all_sources_and_publishes_snapshot() {
        let (catalog, root) = catalog();
        let client = FakeClient::new([
            modified(models_dev_body(), "models-etag"),
            modified(codex_body(), "codex-etag"),
            modified(openrouter_body(), "openrouter-etag"),
        ]);

        assert!(catalog.requires_bootstrap());
        let result = catalog.bootstrap(&client).await.unwrap();

        assert_eq!(result.updated, ["models.dev", "openai-codex", "openrouter"]);
        assert!(catalog.get("openai", "updated-openai").is_some());
        assert!(catalog.get("openai-codex", "gpt-5.5").is_some());
        assert!(catalog.get("openrouter", "updated/openrouter").is_some());
        assert!(!catalog.requires_bootstrap());
        let store_path = root.join("models-store.json");
        let store =
            serde_json::from_slice::<CatalogStore>(&std::fs::read(&store_path).unwrap()).unwrap();
        assert!(store.sources.values().all(|source| source.sha256.is_some()));

        let reloaded = ModelCatalog::load(ModelCatalogOptions {
            store_path,
            user_paths: Vec::new(),
            offline: false,
        })
        .unwrap();
        assert!(
            !reloaded.requires_bootstrap(),
            "providers={:?}, diagnostics={:?}",
            reloaded.provider_ids(),
            reloaded.diagnostics()
        );
        assert!(reloaded.diagnostics().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn openai_refresh_rebuilds_codex_metadata_without_conditional_headers() {
        let (catalog, root) = catalog();
        catalog
            .bootstrap(&FakeClient::new([
                modified(models_dev_body(), "models-etag"),
                modified(codex_body(), "codex-etag"),
                modified(openrouter_body(), "openrouter-etag"),
            ]))
            .await
            .unwrap();
        let client = FakeClient::new([
            modified(models_dev_body_with_cost(2.0), "models-etag-2"),
            modified(codex_body(), "codex-etag-2"),
            FakeResponse::Ok(SourceResponse::NotModified),
        ]);

        catalog.update(&client, true).await.unwrap();

        let requests = client.requests.lock().unwrap();
        assert_eq!(requests[1].url, OPENAI_CODEX_MODELS_URL);
        assert!(requests[1].etag.is_none());
        assert!(requests[1].last_modified.is_none());
        assert_eq!(
            catalog.get("openai-codex", "gpt-5.5").unwrap().cost.input,
            2.0
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn bootstrap_reapplies_user_models_after_remote_data_arrives() {
        let root = std::env::temp_dir().join(format!("phi-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let user_path = root.join("models.json");
        std::fs::write(
            &user_path,
            br#"{"providers":{"openai":{"models":{"updated-openai":{"contextWindow":90000}}},"company-api":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"local":{}}}}}"#,
        )
        .unwrap();
        let catalog = ModelCatalog::load(ModelCatalogOptions {
            store_path: root.join("models-store.json"),
            user_paths: vec![user_path],
            offline: false,
        })
        .unwrap();
        let client = FakeClient::new([
            modified(models_dev_body(), "models-etag"),
            modified(codex_body(), "codex-etag"),
            modified(openrouter_body(), "openrouter-etag"),
        ]);

        catalog.bootstrap(&client).await.unwrap();

        assert_eq!(
            catalog
                .get("openai", "updated-openai")
                .unwrap()
                .context_window,
            90_000
        );
        assert!(catalog.custom_providers().contains_key("company-api"));
        assert!(catalog.diagnostics().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_store_write_preserves_all_published_state() {
        let root = std::env::temp_dir().join(format!("phi-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let user_path = root.join("models.json");
        std::fs::write(
            &user_path,
            br#"{"providers":{"before":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"local":{}}}}}"#,
        )
        .unwrap();
        let blocked_parent = root.join("blocked");
        let catalog = ModelCatalog::load(ModelCatalogOptions {
            store_path: blocked_parent.join("models-store.json"),
            user_paths: vec![user_path.clone()],
            offline: false,
        })
        .unwrap();
        std::fs::write(
            user_path,
            br#"{"providers":{"after":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"local":{}}}}}"#,
        )
        .unwrap();
        std::fs::write(&blocked_parent, b"not a directory").unwrap();
        let client = FakeClient::new([
            modified(models_dev_body(), "models-etag"),
            modified(codex_body(), "codex-etag"),
            modified(openrouter_body(), "openrouter-etag"),
        ]);

        assert!(catalog.update(&client, true).await.is_err());
        assert!(catalog.custom_providers().contains_key("before"));
        assert!(!catalog.custom_providers().contains_key("after"));
        assert!(catalog.get("openai", "updated-openai").is_none());
        assert!(catalog.diagnostics().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn commits_independent_source_when_others_fail() {
        let (catalog, root) = catalog();
        let client = FakeClient::new([
            FakeResponse::Err("models.dev unavailable"),
            FakeResponse::Err("Codex catalog unavailable"),
            modified(openrouter_body(), "openrouter-etag"),
        ]);

        let result = catalog.update(&client, true).await.unwrap();

        assert_eq!(result.stale, ["models.dev", "openai-codex"]);
        assert_eq!(result.updated, ["openrouter"]);
        assert!(catalog.get("openrouter", "updated/openrouter").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }
}
