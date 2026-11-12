use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use sha2::{Digest, Sha256};

use super::schema::{
    CatalogManifest, CatalogSource, CatalogStore, DEFAULT_CUSTOM_MODEL_CONTEXT_WINDOW,
    DEFAULT_CUSTOM_MODEL_MAX_TOKENS, MODEL_CATALOG_SCHEMA_VERSION, ModelCost, ModelCostOverride,
    ModelCostTier, ModelDescriptor, ModelInput, ModelOverride, ProviderModels, UserCatalogConfig,
};
use crate::config::CustomProviderConfig;
use crate::provider::ProviderApi;

const SOURCE_PROVIDERS: &[(CatalogSource, &[&str])] = &[
    (
        CatalogSource::ModelsDev,
        &["openai", "anthropic", "google", "deepseek"],
    ),
    (CatalogSource::OpenAiCodex, &["openai-codex"]),
    (CatalogSource::OpenRouter, &["openrouter"]),
];

#[derive(Debug, Clone)]
pub struct CatalogSnapshot {
    manifest: CatalogManifest,
    providers: BTreeMap<String, ProviderModels>,
}

impl CatalogSnapshot {
    fn empty() -> Self {
        Self {
            manifest: CatalogManifest {
                schema_version: MODEL_CATALOG_SCHEMA_VERSION,
                generated_at: "empty".to_string(),
                providers: BTreeMap::new(),
            },
            providers: BTreeMap::new(),
        }
    }

    pub fn from_parts(
        manifest: CatalogManifest,
        providers: BTreeMap<String, ProviderModels>,
    ) -> anyhow::Result<Self> {
        let snapshot = Self {
            manifest,
            providers,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn manifest(&self) -> &CatalogManifest {
        &self.manifest
    }

    pub fn providers(&self) -> &BTreeMap<String, ProviderModels> {
        &self.providers
    }

    pub fn get(&self, provider: &str, model_id: &str) -> Option<&ModelDescriptor> {
        self.providers
            .get(canonical_provider(provider))?
            .get(model_id)
    }

    fn from_merged(
        manifest: CatalogManifest,
        providers: BTreeMap<String, ProviderModels>,
    ) -> anyhow::Result<Self> {
        for (provider, models) in &providers {
            validate_provider(provider, models)?;
        }
        Ok(Self {
            manifest,
            providers,
        })
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.manifest.schema_version != MODEL_CATALOG_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported model catalog schema version {}",
                self.manifest.schema_version
            );
        }
        if self.manifest.providers.len() != self.providers.len() {
            anyhow::bail!("model catalog manifest provider set does not match provider data");
        }
        for (provider, models) in &self.providers {
            if !self.manifest.providers.contains_key(provider) {
                anyhow::bail!("manifest is missing provider {provider}");
            }
            validate_provider(provider, models)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CatalogState {
    snapshot: Arc<CatalogSnapshot>,
    custom_providers: BTreeMap<String, CustomProviderConfig>,
    diagnostics: Vec<CatalogDiagnostic>,
}

#[derive(Debug)]
pub struct ModelCatalog {
    state: RwLock<Arc<CatalogState>>,
    pub(crate) update_lock: tokio::sync::Mutex<()>,
    discovery: RwLock<BTreeMap<String, BTreeSet<String>>>,
    store_path: PathBuf,
    user_paths: Vec<PathBuf>,
    offline: bool,
}

#[derive(Debug, Clone)]
pub struct ModelCatalogOptions {
    pub store_path: PathBuf,
    pub user_paths: Vec<PathBuf>,
    pub offline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogDiagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelDiscoveryResult {
    pub known: Vec<String>,
    pub unknown: Vec<String>,
}

impl ModelCatalog {
    pub fn empty() -> Self {
        Self::from_snapshot(CatalogSnapshot::empty())
    }

    fn from_snapshot(snapshot: CatalogSnapshot) -> Self {
        Self {
            state: RwLock::new(Arc::new(CatalogState {
                snapshot: Arc::new(snapshot),
                custom_providers: BTreeMap::new(),
                diagnostics: Vec::new(),
            })),
            update_lock: tokio::sync::Mutex::new(()),
            discovery: RwLock::new(BTreeMap::new()),
            store_path: PathBuf::new(),
            user_paths: Vec::new(),
            offline: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_model(model: ModelDescriptor) -> anyhow::Result<Self> {
        let providers = BTreeMap::from([(
            model.provider.clone(),
            BTreeMap::from([(model.id.clone(), model)]),
        )]);
        Ok(Self::from_snapshot(CatalogSnapshot::from_merged(
            runtime_manifest(&providers),
            providers,
        )?))
    }

    pub fn load(options: ModelCatalogOptions) -> anyhow::Result<Self> {
        let mut providers = BTreeMap::new();
        let mut diagnostics = Vec::new();
        let mut custom_providers = BTreeMap::new();
        load_store(&options.store_path, &mut providers, &mut diagnostics);
        for path in &options.user_paths {
            load_user_config(
                path,
                &mut providers,
                &mut custom_providers,
                &mut diagnostics,
            );
        }
        let snapshot = CatalogSnapshot::from_merged(runtime_manifest(&providers), providers)?;
        Ok(Self {
            state: RwLock::new(Arc::new(CatalogState {
                snapshot: Arc::new(snapshot),
                custom_providers,
                diagnostics,
            })),
            update_lock: tokio::sync::Mutex::new(()),
            discovery: RwLock::new(BTreeMap::new()),
            store_path: options.store_path,
            user_paths: options.user_paths,
            offline: options.offline,
        })
    }

    /// Copies the current catalog state without sharing subsequent updates.
    pub fn fork(&self) -> Self {
        Self {
            state: RwLock::new(self.state.read().unwrap().clone()),
            update_lock: tokio::sync::Mutex::new(()),
            discovery: RwLock::new(self.discovery.read().unwrap().clone()),
            store_path: self.store_path.clone(),
            user_paths: self.user_paths.clone(),
            offline: self.offline,
        }
    }

    pub fn snapshot(&self) -> Arc<CatalogSnapshot> {
        self.state.read().unwrap().snapshot.clone()
    }

    pub fn get(&self, provider: &str, model_id: &str) -> Option<ModelDescriptor> {
        self.snapshot().get(provider, model_id).cloned()
    }

    pub fn model_ids(&self, provider: &str) -> Vec<String> {
        self.snapshot()
            .providers
            .get(canonical_provider(provider))
            .map(|models| models.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn provider_ids(&self) -> Vec<String> {
        self.snapshot().providers.keys().cloned().collect()
    }

    pub fn custom_providers(&self) -> BTreeMap<String, CustomProviderConfig> {
        self.state.read().unwrap().custom_providers.clone()
    }

    pub fn diagnostics(&self) -> Vec<CatalogDiagnostic> {
        self.state.read().unwrap().diagnostics.clone()
    }

    pub fn offline(&self) -> bool {
        self.offline
    }

    pub fn requires_bootstrap(&self) -> bool {
        let snapshot = self.snapshot();
        SOURCE_PROVIDERS
            .iter()
            .flat_map(|(_, providers)| providers.iter().copied())
            .any(|provider| {
                snapshot
                    .providers
                    .get(provider)
                    .is_none_or(BTreeMap::is_empty)
            })
    }

    pub fn record_discovery(
        &self,
        provider: &str,
        model_ids: impl IntoIterator<Item = String>,
    ) -> anyhow::Result<ModelDiscoveryResult> {
        if self.offline {
            anyhow::bail!("model discovery disabled by PHI_OFFLINE");
        }
        let provider = canonical_provider(provider).to_string();
        let discovered = model_ids.into_iter().collect::<BTreeSet<_>>();
        let known = discovered
            .iter()
            .filter(|id| self.get(&provider, id).is_some())
            .cloned()
            .collect();
        let unknown = discovered
            .iter()
            .filter(|id| self.get(&provider, id).is_none())
            .cloned()
            .collect();
        self.discovery.write().unwrap().insert(provider, discovered);
        Ok(ModelDiscoveryResult { known, unknown })
    }

    pub fn discovered_model_ids(&self, provider: &str) -> Vec<String> {
        self.discovery
            .read()
            .unwrap()
            .get(canonical_provider(provider))
            .map(|models| models.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub(crate) fn state_for_store(&self, store: &CatalogStore) -> anyhow::Result<CatalogState> {
        if store.schema_version != MODEL_CATALOG_SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported model catalog cache schema {}",
                store.schema_version
            );
        }
        let mut providers = BTreeMap::new();
        for (provider, stored) in &store.providers {
            upsert_models(&mut providers, provider, stored.models.clone())?;
        }
        let mut custom_providers = BTreeMap::new();
        let mut diagnostics = Vec::new();
        for path in &self.user_paths {
            load_user_config(
                path,
                &mut providers,
                &mut custom_providers,
                &mut diagnostics,
            );
        }
        Ok(CatalogState {
            snapshot: Arc::new(CatalogSnapshot::from_merged(
                runtime_manifest(&providers),
                providers,
            )?),
            custom_providers,
            diagnostics,
        })
    }

    pub(crate) fn publish_state(&self, state: CatalogState) {
        *self.state.write().unwrap() = Arc::new(state);
    }
}

fn load_store(
    path: &Path,
    providers: &mut BTreeMap<String, ProviderModels>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    if !path.exists() {
        return;
    }
    let store = match read_json::<CatalogStore>(path) {
        Ok(store) if store.schema_version == MODEL_CATALOG_SCHEMA_VERSION => store,
        Ok(store) => {
            diagnostics.push(CatalogDiagnostic {
                path: path.to_path_buf(),
                message: format!(
                    "unsupported model catalog cache schema {}",
                    store.schema_version
                ),
            });
            return;
        }
        Err(error) => {
            diagnostics.push(CatalogDiagnostic {
                path: path.to_path_buf(),
                message: error.to_string(),
            });
            return;
        }
    };
    for source in CatalogSource::ALL.map(CatalogSource::as_str) {
        let result = validate_store_source(&store, source).and_then(|()| {
            let mut candidate = providers.clone();
            for provider in source_provider_ids(source)? {
                if let Some(stored) = store.providers.get(*provider) {
                    upsert_models(&mut candidate, provider, stored.models.clone())?;
                }
            }
            Ok(candidate)
        });
        apply_candidate(path, providers, diagnostics, result);
    }
}

pub(crate) fn validate_store_source(store: &CatalogStore, source: &str) -> anyhow::Result<()> {
    let provider_ids = source_provider_ids(source)?;
    let has_provider = provider_ids
        .iter()
        .any(|provider| store.providers.contains_key(*provider));
    let Some(state) = store.sources.get(source) else {
        if has_provider {
            anyhow::bail!("model catalog source {source} is missing source state");
        }
        return Ok(());
    };
    let expected = state
        .sha256
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("model catalog source {source} is missing sha256"))?;
    if store_source_sha256(store, source)? != expected {
        anyhow::bail!("model catalog source {source} failed sha256 validation");
    }
    Ok(())
}

pub(crate) fn remove_store_source(store: &mut CatalogStore, source: &str) -> anyhow::Result<()> {
    store.sources.remove(source);
    for provider in source_provider_ids(source)? {
        store.providers.remove(*provider);
    }
    Ok(())
}

fn source_provider_ids(source: &str) -> anyhow::Result<&'static [&'static str]> {
    SOURCE_PROVIDERS
        .iter()
        .find_map(|(candidate, providers)| (candidate.as_str() == source).then_some(*providers))
        .ok_or_else(|| anyhow::anyhow!("unknown model catalog source {source}"))
}

pub(crate) fn store_source_sha256(store: &CatalogStore, source: &str) -> anyhow::Result<String> {
    let provider_ids = source_provider_ids(source)?;
    let providers = provider_ids
        .iter()
        .map(|provider| {
            let models = &store
                .providers
                .get(*provider)
                .ok_or_else(|| anyhow::anyhow!("model catalog source {source} is incomplete"))?
                .models;
            Ok(((*provider).to_string(), models))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    Ok(sha256(&serde_json::to_vec(&providers)?))
}

fn load_user_config(
    path: &Path,
    providers: &mut BTreeMap<String, ProviderModels>,
    custom_providers: &mut BTreeMap<String, CustomProviderConfig>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    if !path.exists() {
        return;
    }
    let result = read_json::<UserCatalogConfig>(path).and_then(|config| {
        let mut model_candidate = providers.clone();
        let mut provider_candidate = custom_providers.clone();
        for (provider, config) in config.providers {
            apply_user_provider(
                &mut model_candidate,
                &mut provider_candidate,
                &provider,
                config,
            )?;
        }
        Ok((model_candidate, provider_candidate))
    });
    match result {
        Ok((models, runtime_providers)) => {
            *providers = models;
            *custom_providers = runtime_providers;
        }
        Err(error) => diagnostics.push(CatalogDiagnostic {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

fn apply_user_provider(
    providers: &mut BTreeMap<String, ProviderModels>,
    runtime_providers: &mut BTreeMap<String, CustomProviderConfig>,
    provider: &str,
    config: super::schema::UserProviderConfig,
) -> anyhow::Result<()> {
    let provider = canonical_provider(provider).to_string();
    let catalog_api = providers
        .get(&provider)
        .and_then(|models| models.values().next())
        .map(|model| model.api);
    if config.api.is_some() && catalog_api.is_some() && config.api != catalog_api {
        anyhow::bail!("provider {provider} cannot override its catalog API");
    }
    let api = config.api.or(catalog_api);
    let is_custom = catalog_api.is_none();
    if is_custom && config.base_url.as_deref().is_none_or(str::is_empty) {
        anyhow::bail!("custom provider {provider} requires baseUrl");
    }
    let api = api.ok_or_else(|| anyhow::anyhow!("custom provider {provider} requires api"))?;
    if config.api_key.as_deref().is_some_and(str::is_empty) {
        anyhow::bail!("provider {provider} has an empty apiKey");
    }
    if is_custom || config.base_url.is_some() || config.api_key.is_some() {
        runtime_providers.insert(
            provider.clone(),
            CustomProviderConfig {
                api,
                base_url: config.base_url,
                api_key: config.api_key,
            },
        );
    }
    apply_overrides(
        providers,
        &provider,
        config.models,
        is_custom.then_some(api),
    )
}

fn apply_candidate(
    path: &Path,
    providers: &mut BTreeMap<String, ProviderModels>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
    result: anyhow::Result<BTreeMap<String, ProviderModels>>,
) {
    match result {
        Ok(candidate) => *providers = candidate,
        Err(error) => diagnostics.push(CatalogDiagnostic {
            path: path.to_path_buf(),
            message: error.to_string(),
        }),
    }
}

fn upsert_models(
    providers: &mut BTreeMap<String, ProviderModels>,
    provider: &str,
    mut models: ProviderModels,
) -> anyhow::Result<()> {
    let provider = canonical_provider(provider).to_string();
    for model in models.values_mut() {
        model.provider = canonical_provider(&model.provider).to_string();
    }
    validate_provider(&provider, &models)?;
    providers.entry(provider).or_default().extend(models);
    Ok(())
}

fn apply_overrides(
    providers: &mut BTreeMap<String, ProviderModels>,
    provider: &str,
    overrides: BTreeMap<String, ModelOverride>,
    custom_api: Option<ProviderApi>,
) -> anyhow::Result<()> {
    let provider = canonical_provider(provider).to_string();
    let models = providers.entry(provider.clone()).or_default();
    for (id, patch) in overrides {
        match models.get_mut(&id) {
            Some(model) => {
                validate_override(model, &patch)?;
                apply_override(model, patch);
            }
            None => {
                let api =
                    custom_api.ok_or_else(|| anyhow::anyhow!("unknown model {provider}/{id}"))?;
                models.insert(id.clone(), custom_descriptor(&provider, id, api, patch)?);
            }
        }
    }
    validate_provider(&provider, models)
}

fn validate_override(model: &ModelDescriptor, patch: &ModelOverride) -> anyhow::Result<()> {
    if patch.reasoning == Some(true) && !model.reasoning {
        anyhow::bail!(
            "model {}/{} does not support reasoning",
            model.provider,
            model.id
        );
    }
    if let Some(levels) = &patch.thinking_level_map {
        let reasoning = patch.reasoning.unwrap_or(model.reasoning);
        if !reasoning && levels.values().any(Option::is_some) {
            anyhow::bail!(
                "model {}/{} does not support thinking levels",
                model.provider,
                model.id
            );
        }
        if levels.iter().any(|(level, value)| {
            value.is_some()
                && model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|known| known.get(level))
                    == Some(&None)
        }) {
            anyhow::bail!(
                "model {}/{} thinking level exceeds provider metadata",
                model.provider,
                model.id
            );
        }
    }
    if let Some(input) = &patch.input
        && input.iter().any(|value| !model.input.contains(value))
    {
        anyhow::bail!(
            "model {}/{} input capability exceeds provider metadata",
            model.provider,
            model.id
        );
    }
    if patch
        .context_window
        .is_some_and(|value| value == 0 || value > model.context_window)
    {
        anyhow::bail!(
            "model {}/{} context window exceeds provider limit",
            model.provider,
            model.id
        );
    }
    if patch
        .max_output_tokens
        .is_some_and(|value| value == 0 || value > model.max_output_tokens)
    {
        anyhow::bail!(
            "model {}/{} output limit exceeds provider limit",
            model.provider,
            model.id
        );
    }
    Ok(())
}

fn apply_override(model: &mut ModelDescriptor, patch: ModelOverride) {
    if let Some(value) = patch.reasoning {
        model.reasoning = value;
    }
    if let Some(value) = patch.thinking_level_map {
        model
            .thinking_level_map
            .get_or_insert_with(BTreeMap::new)
            .extend(value);
    }
    if let Some(value) = patch.input {
        model.input = value;
    }
    if let Some(value) = patch.context_window {
        model.context_window = value;
    }
    if let Some(value) = patch.max_output_tokens {
        model.max_output_tokens = value;
    }
    if let Some(value) = patch.cost {
        apply_cost_override(&mut model.cost, value);
    }
}

fn custom_descriptor(
    provider: &str,
    id: String,
    api: ProviderApi,
    patch: ModelOverride,
) -> anyhow::Result<ModelDescriptor> {
    let reasoning = patch.reasoning.unwrap_or(false);
    if !reasoning && patch.thinking_level_map.is_some() {
        anyhow::bail!("custom model {provider}/{id} requires reasoning for thinkingLevelMap");
    }
    let context_window = patch
        .context_window
        .unwrap_or(DEFAULT_CUSTOM_MODEL_CONTEXT_WINDOW);
    let max_output_tokens = patch
        .max_output_tokens
        .unwrap_or(DEFAULT_CUSTOM_MODEL_MAX_TOKENS);
    let mut cost = ModelCost::default();
    if let Some(patch) = patch.cost {
        apply_cost_override(&mut cost, patch);
    }
    Ok(ModelDescriptor {
        provider: provider.to_string(),
        name: id.clone(),
        id,
        api,
        reasoning,
        thinking_level_map: patch.thinking_level_map,
        input: patch.input.unwrap_or_else(|| vec![ModelInput::Text]),
        context_window,
        max_output_tokens,
        cost,
    })
}

fn apply_cost_override(cost: &mut ModelCost, patch: ModelCostOverride) {
    if let Some(value) = patch.input {
        cost.input = value;
    }
    if let Some(value) = patch.output {
        cost.output = value;
    }
    if let Some(value) = patch.cache_read {
        cost.cache_read = value;
    }
    if let Some(value) = patch.cache_write {
        cost.cache_write = value;
    }
    if let Some(value) = patch.tiers {
        cost.tiers = value;
    }
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(Into::into)
}

pub fn validate_provider(provider: &str, models: &ProviderModels) -> anyhow::Result<()> {
    if provider.trim().is_empty() {
        anyhow::bail!("model provider cannot be empty");
    }
    let mut previous_id: Option<&str> = None;
    for (id, model) in models {
        if id.trim().is_empty() || model.id.trim().is_empty() || model.name.trim().is_empty() {
            anyhow::bail!("model provider, id, and name must be non-empty");
        }
        if id != &model.id {
            anyhow::bail!(
                "model map key {id} does not match descriptor id {}",
                model.id
            );
        }
        if model.provider != provider {
            anyhow::bail!("model {id} belongs to {}, not {provider}", model.provider);
        }
        if previous_id.is_some_and(|previous| previous >= id.as_str()) {
            anyhow::bail!("models for {provider} are not strictly sorted");
        }
        previous_id = Some(id);
        validate_api(provider, model.api)?;
        if !model.input.contains(&ModelInput::Text) {
            anyhow::bail!("model {provider}/{id} does not accept text input");
        }
        if model.context_window == 0 || model.max_output_tokens == 0 {
            anyhow::bail!("model {provider}/{id} has invalid token limits");
        }
        validate_cost(provider, id, &model.cost)?;
        if let Some(levels) = &model.thinking_level_map
            && levels
                .values()
                .flatten()
                .any(|value| value.trim().is_empty())
        {
            anyhow::bail!("model {provider}/{id} has an empty thinking-level mapping");
        }
    }
    Ok(())
}

fn validate_api(provider: &str, api: ProviderApi) -> anyhow::Result<()> {
    let provider = canonical_provider(provider);
    let valid = match provider {
        "openai" => api == ProviderApi::OpenAiResponses,
        "openai-codex" | "chatgpt" => api == ProviderApi::OpenAiCodexResponses,
        "anthropic" => api == ProviderApi::AnthropicMessages,
        "google" => api == ProviderApi::GeminiGenerateContent,
        "deepseek" => api == ProviderApi::DeepSeekChat,
        "openrouter" => api == ProviderApi::OpenAiCompletions,
        "ollama" => api == ProviderApi::OllamaChat,
        _ => true,
    };
    if !valid {
        anyhow::bail!("provider {provider} is incompatible with protocol {api:?}");
    }
    Ok(())
}

fn validate_cost(provider: &str, id: &str, cost: &ModelCost) -> anyhow::Result<()> {
    validate_rates(
        provider,
        id,
        cost.input,
        cost.output,
        cost.cache_read,
        cost.cache_write,
    )?;
    let mut previous_threshold = None;
    for tier in &cost.tiers {
        if tier.input_tokens_above == 0
            || previous_threshold.is_some_and(|previous| previous >= tier.input_tokens_above)
        {
            anyhow::bail!("model {provider}/{id} has unordered pricing tiers");
        }
        previous_threshold = Some(tier.input_tokens_above);
        validate_tier(provider, id, tier)?;
    }
    Ok(())
}

fn validate_tier(provider: &str, id: &str, tier: &ModelCostTier) -> anyhow::Result<()> {
    validate_rates(
        provider,
        id,
        tier.input,
        tier.output,
        tier.cache_read,
        tier.cache_write,
    )
}

fn validate_rates(
    provider: &str,
    id: &str,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
) -> anyhow::Result<()> {
    if [input, output, cache_read, cache_write]
        .into_iter()
        .any(|value| !value.is_finite() || value < 0.0)
    {
        anyhow::bail!("model {provider}/{id} has invalid pricing");
    }
    Ok(())
}

pub fn canonical_provider(provider: &str) -> &str {
    if provider.eq_ignore_ascii_case("chatgpt") {
        "openai-codex"
    } else {
        provider
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn runtime_manifest(providers: &BTreeMap<String, ProviderModels>) -> CatalogManifest {
    let providers = providers
        .keys()
        .map(|provider| {
            let source = SOURCE_PROVIDERS
                .iter()
                .find_map(|(source, providers)| {
                    providers.contains(&provider.as_str()).then_some(*source)
                })
                .unwrap_or(CatalogSource::ModelsDev);
            (
                provider.clone(),
                super::schema::ManifestProvider {
                    source,
                    sha256: String::new(),
                },
            )
        })
        .collect();
    CatalogManifest {
        schema_version: MODEL_CATALOG_SCHEMA_VERSION,
        generated_at: "runtime".to_string(),
        providers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor() -> ModelDescriptor {
        ModelDescriptor {
            provider: "openai".to_string(),
            id: "model".to_string(),
            name: "Model".to_string(),
            api: ProviderApi::OpenAiResponses,
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            context_window: 128_000,
            max_output_tokens: 32_000,
            cost: ModelCost::default(),
        }
    }

    #[test]
    fn fork_retains_models_and_providers_after_refresh() {
        let root = std::env::temp_dir().join(format!("phi-catalog-fork-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("models.json");
        std::fs::write(&path, r#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}"#).unwrap();
        let catalog = ModelCatalog::load(ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![path.clone()],
            offline: true,
        })
        .unwrap();
        let retained = catalog.fork();
        std::fs::write(&path, r#"{"providers":{}}"#).unwrap();
        let store = CatalogStore {
            schema_version: MODEL_CATALOG_SCHEMA_VERSION,
            sources: BTreeMap::new(),
            providers: BTreeMap::new(),
        };
        catalog.publish_state(catalog.state_for_store(&store).unwrap());
        assert!(catalog.get("local", "test").is_none());
        assert!(catalog.custom_providers().is_empty());
        assert_eq!(
            retained.get("local", "test").unwrap().context_window,
            128_000
        );
        assert_eq!(
            retained.custom_providers()["local"].base_url.as_deref(),
            Some("http://localhost/v1")
        );
        assert!(retained.fork().get("local", "test").is_some());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn write_models_dev_store(path: &Path, context_window: u64) {
        let definitions = [
            ("openai", "gpt-5.5", ProviderApi::OpenAiResponses),
            ("anthropic", "test", ProviderApi::AnthropicMessages),
            ("google", "test", ProviderApi::GeminiGenerateContent),
            ("deepseek", "test", ProviderApi::DeepSeekChat),
            ("openai-codex", "gpt-5.5", ProviderApi::OpenAiCodexResponses),
        ];
        let providers = definitions
            .into_iter()
            .map(|(provider, id, api)| {
                let mut model = descriptor();
                model.provider = provider.to_string();
                model.id = id.to_string();
                model.api = api;
                model.context_window = context_window;
                (
                    provider.to_string(),
                    super::super::schema::StoredProvider {
                        models: BTreeMap::from([(id.to_string(), model)]),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut store = CatalogStore {
            schema_version: MODEL_CATALOG_SCHEMA_VERSION,
            sources: BTreeMap::new(),
            providers,
        };
        for source in ["models.dev", "openai-codex"] {
            let sha256 = store_source_sha256(&store, source).unwrap();
            store.sources.insert(
                source.to_string(),
                super::super::schema::SourceState {
                    sha256: Some(sha256),
                    ..Default::default()
                },
            );
        }
        std::fs::write(path, serde_json::to_vec(&store).unwrap()).unwrap();
    }

    #[test]
    fn user_overrides_merge_and_catalog_instances_stay_isolated() {
        let (first, first_root) = catalog_with_context_override(111_000);
        let (second, second_root) = catalog_with_context_override(222_000);

        assert_eq!(
            first.get("openai", "gpt-5.5").unwrap().context_window,
            111_000
        );
        assert_eq!(
            second.get("openai", "gpt-5.5").unwrap().context_window,
            222_000
        );
        std::fs::remove_dir_all(first_root).unwrap();
        std::fs::remove_dir_all(second_root).unwrap();
    }

    #[test]
    fn custom_provider_accepts_custom_model_ids() {
        let root = std::env::temp_dir().join(format!("phi-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let override_path = root.join("models.json");
        std::fs::write(
            &override_path,
            br#"{"providers":{"company-api":{"api":"openai-completions","baseUrl":"http://localhost:8080/v1","apiKey":"$COMPANY_API_KEY","models":{"company-coder-v2":{}}}}}"#,
        )
        .unwrap();

        let catalog = ModelCatalog::load(ModelCatalogOptions {
            store_path: root.join("models-store.json"),
            user_paths: vec![override_path],
            offline: false,
        })
        .unwrap();
        let model = catalog.get("company-api", "company-coder-v2").unwrap();

        assert_eq!(model.name, "company-coder-v2");
        assert_eq!(model.api, ProviderApi::OpenAiCompletions);
        assert_eq!(model.context_window, DEFAULT_CUSTOM_MODEL_CONTEXT_WINDOW);
        assert_eq!(model.max_output_tokens, DEFAULT_CUSTOM_MODEL_MAX_TOKENS);
        assert_eq!(model.cost, ModelCost::default());
        assert_eq!(model.input, vec![ModelInput::Text]);
        let providers = catalog.custom_providers();
        let provider = &providers["company-api"];
        assert_eq!(
            provider.base_url.as_deref(),
            Some("http://localhost:8080/v1")
        );
        assert_eq!(provider.api_key.as_deref(), Some("$COMPANY_API_KEY"));

        let invalid_patch = ModelOverride {
            thinking_level_map: Some(BTreeMap::from([(
                super::super::schema::ThinkingLevel::High,
                Some("high".to_string()),
            )])),
            ..ModelOverride::default()
        };
        assert!(
            custom_descriptor(
                "company-api",
                "company-coder-v2".to_string(),
                ProviderApi::OpenAiCompletions,
                invalid_patch,
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    fn catalog_with_context_override(context_window: u64) -> (ModelCatalog, PathBuf) {
        let root = std::env::temp_dir().join(format!("phi-catalog-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let store_path = root.join("models-store.json");
        let override_path = root.join("models.json");
        write_models_dev_store(&store_path, 700_000);
        std::fs::write(
            &override_path,
            serde_json::to_vec(&serde_json::json!({
                "providers": { "openai": { "models": {
                    "gpt-5.5": { "contextWindow": context_window }
                } } }
            }))
            .unwrap(),
        )
        .unwrap();
        let catalog = ModelCatalog::load(ModelCatalogOptions {
            store_path,
            user_paths: vec![override_path],
            offline: false,
        })
        .unwrap();
        (catalog, root)
    }

    #[test]
    fn discovery_separates_known_and_unknown_without_mutating_catalog() {
        let (catalog, root) = catalog_with_context_override(272_000);
        let known = "gpt-5.5".to_string();
        let original = catalog.model_ids("openai-codex");

        let result = catalog
            .record_discovery("openai-codex", [known.clone(), "unknown-model".to_string()])
            .unwrap();

        assert_eq!(result.known, [known]);
        assert_eq!(result.unknown, ["unknown-model"]);
        assert_eq!(catalog.model_ids("openai-codex"), original);
        assert_eq!(catalog.discovered_model_ids("openai-codex").len(), 2);
        std::fs::remove_dir_all(root).unwrap();

        let mut offline = ModelCatalog::empty();
        offline.offline = true;
        assert!(
            offline
                .record_discovery("openai", ["model".to_string()])
                .is_err()
        );
        assert!(offline.discovered_model_ids("openai").is_empty());
    }
}
