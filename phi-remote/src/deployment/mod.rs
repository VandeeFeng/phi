//! Protected local Gateway deployment and durable device authorization.

mod authorization;
mod config;
#[cfg(unix)]
mod local;
mod storage;

#[cfg(unix)]
pub(crate) use local::LocalPairing;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::{Gateway, GatewayAccess, RemoteClient, RemoteHost};
use phi_sdk::agent_manager::AgentManager;
use serde::{Deserialize, Serialize};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(test)]
const TEST_NOW: u64 = 1_800_000_000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Endpoint {
    local_endpoint: String,
    browser_url: String,
    local_credential: String,
}

impl Endpoint {
    fn read(directory: &Path) -> anyhow::Result<Option<Self>> {
        let Some(bytes) = storage::read_optional(&directory.join("endpoint.json"))? else {
            return Ok(None);
        };
        let endpoint: Self = serde_json::from_slice(&bytes)
            .map_err(|_| anyhow::anyhow!("invalid Gateway endpoint"))?;
        endpoint.client()?;
        if endpoint.browser_url != endpoint.local_endpoint {
            crate::GatewayConfig::new("127.0.0.1:0".parse()?, Some(&endpoint.browser_url))?;
        }
        Ok(Some(endpoint))
    }

    pub fn client(&self) -> anyhow::Result<RemoteClient> {
        RemoteClient::connect(&self.local_endpoint, &self.local_credential)
    }

    pub fn browser_url(&self) -> &str {
        &self.browser_url
    }

    pub async fn is_available(&self) -> anyhow::Result<bool> {
        let client = self.client()?;
        Ok(matches!(
            tokio::time::timeout(DISCOVERY_TIMEOUT, client.sessions()).await,
            Ok(Ok(_))
        ))
    }

    pub fn retired(&self, directory: &Path) -> anyhow::Result<bool> {
        if matches!(Self::read(directory)?, Some(current) if current != *self) {
            return Ok(true);
        }
        let lock_path = directory.join("gateway.lock");
        match storage::ProcessLock::try_acquire_existing(&lock_path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

pub struct Registration {
    directory: PathBuf,
    endpoint: Endpoint,
    _authority: Arc<storage::PairingAuthority>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if matches!(Endpoint::read(&self.directory), Ok(Some(current)) if current == self.endpoint)
        {
            if let Err(error) = std::fs::remove_file(self.directory.join("endpoint.json")) {
                tracing::warn!(%error, "cannot retire Gateway endpoint");
            }
        }
    }
}

/// Reads pairing material for a local operator; callers own disclosure policy.
pub fn entry_token(directory: &Path) -> anyhow::Result<String> {
    storage::entry_token(directory)
}

pub fn discover(directory: &Path) -> anyhow::Result<Option<Endpoint>> {
    Endpoint::read(directory)
}

pub async fn start(
    directory: &Path,
    manager: AgentManager,
    host: impl RemoteHost,
    assets: PathBuf,
    port: Option<u16>,
) -> anyhow::Result<(Gateway, Registration, Endpoint)> {
    start_with_plugins(
        directory,
        manager,
        host,
        assets,
        port,
        crate::plugins::PluginRegistry::new(),
    )
    .await
}

/// Starts the managed deployment with caller-registered plugins alongside configured builtins.
/// Plugin entry points start after endpoint publication; they choose when to create clients.
pub async fn start_with_plugins(
    directory: &Path,
    manager: AgentManager,
    host: impl RemoteHost,
    assets: PathBuf,
    port: Option<u16>,
    plugins: crate::plugins::PluginRegistry,
) -> anyhow::Result<(Gateway, Registration, Endpoint)> {
    let config = config::GatewayConfig::load(directory, port)?;
    let plugins = config
        .plugins
        .register(directory, assets.clone())?
        .merge(plugins)?;
    let authority = Arc::new(storage::PairingAuthority::open_with_expiry(
        directory,
        config.expiry,
    )?);
    let local_credential = crate::pairing::random_credential()?;
    let mut gateway = Gateway::serve_paired(
        manager,
        host,
        assets,
        config.transport.with_plugins(plugins),
        local_credential.clone(),
        GatewayAccess::new(authority.clone(), config.trusted_proxies),
    )
    .await?;
    let endpoint = Endpoint {
        local_endpoint: gateway.local_url().to_owned(),
        browser_url: gateway.url().to_owned(),
        local_credential,
    };
    let published = storage::publish(&directory.join("endpoint.json"), &endpoint);
    let registration = Registration {
        directory: directory.to_owned(),
        endpoint: endpoint.clone(),
        _authority: authority,
    };
    if let Err(error) = published {
        let cleanup = gateway.close().await;
        drop(registration);
        cleanup?;
        return Err(error.context("cannot publish Gateway endpoint"));
    }
    gateway.start_plugins(directory);
    Ok((gateway, registration, endpoint))
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::agent_instance::AgentInstanceId;
    use serde_json::Value;
    use storage::tests::Fixture;

    pub(super) struct NoSessions;

    impl RemoteHost for NoSessions {
        type Recovery = Value;

        async fn recovery(&self, _: AgentInstanceId) -> anyhow::Result<crate::RemoteSnapshot> {
            anyhow::bail!("unsupported")
        }
        async fn configure(
            &self,
            _: AgentInstanceId,
            _: crate::RemoteSelection,
        ) -> anyhow::Result<crate::RemoteRuntimeUpdate<Value>> {
            anyhow::bail!("unsupported")
        }
        async fn reload(
            &self,
            _: AgentInstanceId,
        ) -> anyhow::Result<crate::RemoteRuntimeUpdate<Value>> {
            anyhow::bail!("unsupported")
        }
        async fn catalog(&self, _: AgentInstanceId) -> anyhow::Result<crate::RemoteCatalog> {
            anyhow::bail!("unsupported")
        }
        async fn new_session(&self, _: AgentInstanceId) -> anyhow::Result<crate::RemoteSession> {
            anyhow::bail!("unsupported")
        }
        async fn sessions(&self) -> anyhow::Result<Vec<crate::RemoteSession>> {
            Ok(Vec::new())
        }
        async fn create(&self, _: PathBuf) -> anyhow::Result<crate::RemoteSession> {
            anyhow::bail!("unsupported")
        }
        async fn saved(&self, _: String) -> anyhow::Result<Vec<crate::SavedSession>> {
            anyhow::bail!("unsupported")
        }
        async fn resume(&self, _: String) -> anyhow::Result<crate::RemoteSession> {
            anyhow::bail!("unsupported")
        }
        async fn close(&self, _: AgentInstanceId) -> anyhow::Result<()> {
            anyhow::bail!("unsupported")
        }
        async fn submit(
            &self,
            _: AgentInstanceId,
            _: phi_sdk::turn::TurnPrompt,
            _: phi_sdk::turn::SubmissionDispatch,
        ) -> anyhow::Result<crate::RemoteSubmission> {
            anyhow::bail!("unsupported")
        }
        async fn message(
            &self,
            _: AgentInstanceId,
            _: AgentInstanceId,
            _: String,
        ) -> anyhow::Result<phi_sdk::agent_manager::AgentMessageReceipt> {
            anyhow::bail!("unsupported")
        }
        async fn interactions(
            &self,
            _: AgentInstanceId,
        ) -> anyhow::Result<
            Vec<(
                AgentInstanceId,
                Vec<phi_sdk::interaction::InteractionEnvelope>,
            )>,
        > {
            anyhow::bail!("unsupported")
        }
        async fn respond(
            &self,
            _: AgentInstanceId,
            _: AgentInstanceId,
            _: String,
            _: phi_sdk::interaction::InteractionResponse,
        ) -> anyhow::Result<Option<String>> {
            anyhow::bail!("unsupported")
        }
    }

    #[tokio::test]
    async fn deployment_retains_ownership_until_finalization() {
        let fixture = Fixture::new();
        let assets = fixture.0.join("web");
        let start = || {
            start(
                &fixture.0,
                AgentManager::new(),
                NoSessions,
                assets.clone(),
                Some(0),
            )
        };
        assert!(discover(&fixture.0).unwrap().is_none());
        let (gateway, registration, endpoint) = start().await.unwrap();
        assert!(!assets.exists());
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let response = http
            .get(format!("{}app.js", gateway.local_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        let discovered = discover(&fixture.0).unwrap().unwrap();
        assert!(discovered == endpoint);
        assert!(discovered.is_available().await.unwrap());
        let token = entry_token(&fixture.0).unwrap();
        let error = start().await.err().unwrap();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let unrelated = Fixture::new();
        assert!(endpoint.retired(&unrelated.0).unwrap());
        assert!(!endpoint.retired(&fixture.0).unwrap());
        gateway.close().await.unwrap();
        assert!(!discovered.is_available().await.unwrap());
        assert!(!endpoint.retired(&fixture.0).unwrap());
        drop(registration);
        assert!(endpoint.retired(&fixture.0).unwrap());
        assert!(discover(&fixture.0).unwrap().is_none());

        let (gateway, registration, current) = start().await.unwrap();
        assert!(current != endpoint);
        assert!(endpoint.retired(&fixture.0).unwrap());
        assert_eq!(entry_token(&fixture.0).unwrap(), token);
        fixture.write("pairing.json", b"invalid pairing state");
        assert!(entry_token(&fixture.0).is_err());
        assert!(
            discover(&fixture.0)
                .unwrap()
                .unwrap()
                .is_available()
                .await
                .unwrap()
        );
        gateway.close().await.unwrap();
        drop(registration);
        assert!(discover(&fixture.0).unwrap().is_none());
    }

    #[tokio::test]
    async fn custom_plugins_use_the_public_deployment_path() {
        use crate::plugins::{Plugin, PluginRegistry};
        use axum::{Router, routing::get};

        let fixture = Fixture::new();
        let (started, mut ready) = tokio::sync::mpsc::unbounded_channel();
        let mut plugins = PluginRegistry::new();
        plugins
            .register(
                "example:custom",
                Plugin::new()
                    .with_routes(Router::new().route("/custom", get(|| async { "plugin" })))
                    .with_task(move |context| {
                        let started = started.clone();
                        async move {
                            let mut shutdown = context.shutdown();
                            let _ = started.send(context);
                            let _ = shutdown.wait_for(|stopping| *stopping).await;
                            Ok(())
                        }
                    }),
            )
            .unwrap();
        assert!(!fixture.0.join("plugins").exists());
        let (gateway, registration, endpoint) = start_with_plugins(
            &fixture.0,
            AgentManager::new(),
            NoSessions,
            fixture.0.clone(),
            Some(0),
            plugins,
        )
        .await
        .unwrap();
        let context = tokio::time::timeout(Duration::from_secs(5), ready.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.endpoint(), gateway.local_url());
        assert!(discover(&fixture.0).unwrap().is_some());
        assert!(!fixture.0.join("plugins").exists());
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("{}custom", gateway.local_url()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "plugin");
        assert!(endpoint.is_available().await.unwrap());
        gateway.close().await.unwrap();
        assert!(*context.shutdown().borrow());
        drop(registration);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn plugin_factory_enrolls_reconnects_and_never_reapproves_revoked_devices() {
        use crate::plugins::{Plugin, PluginRegistry};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = Fixture::new();
        let identity_directory = fixture.0.join("custom-identity");
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let calls = factory_calls.clone();
        let connect = Arc::new(tokio::sync::Semaphore::new(0));
        let gate = connect.clone();
        let identity = identity_directory.clone();
        let (started, mut ready) = tokio::sync::mpsc::unbounded_channel();
        let (connected, mut clients) = tokio::sync::mpsc::unbounded_channel();
        let mut plugins = PluginRegistry::new();
        plugins
            .register(
                "example:device",
                Plugin::new().with_task(move |context| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let started = started.clone();
                    let connected = connected.clone();
                    let gate = gate.clone();
                    let identity = identity.clone();
                    async move {
                        let mut shutdown = context.shutdown();
                        let _ = started.send(context.clone());
                        gate.acquire().await?.forget();
                        let _ = connected.send(context.client(&identity).await);
                        let _ = shutdown.wait_for(|stopping| *stopping).await;
                        Ok(())
                    }
                }),
            )
            .unwrap();
        assert_eq!(factory_calls.load(Ordering::SeqCst), 0);
        assert!(!identity_directory.exists());
        let (gateway, registration, endpoint) = start_with_plugins(
            &fixture.0,
            AgentManager::new(),
            NoSessions,
            fixture.0.clone(),
            Some(0),
            plugins,
        )
        .await
        .unwrap();
        let context = tokio::time::timeout(Duration::from_secs(5), ready.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
        assert!(!identity_directory.exists());
        assert!(!fixture.0.join("plugins").exists());
        let devices = endpoint.client().unwrap().devices().await.unwrap();
        assert!(devices.pending.is_empty());
        assert!(devices.approved.is_empty());
        assert!(clients.try_recv().is_err());

        connect.add_permits(1);
        let client = tokio::time::timeout(Duration::from_secs(5), clients.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(identity_directory.join("device.json").is_file());
        assert!(client.sessions().await.unwrap().is_empty());
        assert!(context.client(&identity_directory).await.is_err());
        assert!(client.view("../foreign").await.is_err());
        let operator = endpoint.client().unwrap();
        let devices = operator.devices().await.unwrap();
        assert_eq!(devices.approved.len(), 1);
        let device_id = devices.approved[0].device_id.clone();
        let identity = std::fs::read(identity_directory.join("device.json")).unwrap();
        let retained = client.clone();
        drop(client);
        assert!(context.client(&identity_directory).await.is_err());
        assert!(retained.sessions().await.unwrap().is_empty());
        drop(retained);
        let client = context.client(&identity_directory).await.unwrap();
        assert!(client.sessions().await.unwrap().is_empty());
        assert_eq!(operator.devices().await.unwrap().approved.len(), 1);
        let second_identity = fixture.0.join("second-identity");
        let second = context.client(&second_identity).await.unwrap();
        assert_eq!(operator.devices().await.unwrap().approved.len(), 2);
        operator.revoke_device(&device_id).await.unwrap();
        assert!(client.sessions().await.is_err());
        assert!(second.sessions().await.unwrap().is_empty());
        drop(client);
        assert!(context.client(&identity_directory).await.is_err());
        let devices = operator.devices().await.unwrap();
        assert_eq!(devices.approved.len(), 1);
        assert!(devices.pending.is_empty());
        assert_eq!(
            std::fs::read(identity_directory.join("device.json")).unwrap(),
            identity
        );
        assert!(endpoint.is_available().await.unwrap());
        gateway.close().await.unwrap();
        drop(registration);
    }

    #[tokio::test]
    async fn duplicate_builtin_registration_fails_before_publication() {
        use crate::plugins::{Plugin, PluginRegistry};

        let fixture = Fixture::new();
        fixture.write("config.json", br#"{"plugins":["+builtin:web"]}"#);
        let mut plugins = PluginRegistry::new();
        plugins.register("builtin:web", Plugin::new()).unwrap();
        let result = start_with_plugins(
            &fixture.0,
            AgentManager::new(),
            NoSessions,
            fixture.0.clone(),
            Some(0),
            plugins,
        )
        .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("already registered")
        );
        assert!(!fixture.0.join("pairing.json").exists());
        assert!(!fixture.0.join("endpoint.json").exists());
        assert!(!fixture.0.join("gateway.lock").exists());
    }

    #[tokio::test]
    async fn web_keeps_native_access() {
        let fixture = Fixture::new();
        let assets = fixture.0.join("assets");
        std::fs::create_dir(&assets).unwrap();
        std::fs::write(assets.join("index.html"), "<body data-mode=\"demo\">").unwrap();
        std::fs::write(assets.join("app.js"), "export {};").unwrap();
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        for (config, expected_status) in [
            ("{}", reqwest::StatusCode::NOT_FOUND),
            (r#"{"plugins":[]}"#, reqwest::StatusCode::NOT_FOUND),
            (
                r#"{"plugins":["-builtin:web","-builtin:telegram","-builtin:feishu"]}"#,
                reqwest::StatusCode::NOT_FOUND,
            ),
            (r#"{"plugins":["+builtin:web"]}"#, reqwest::StatusCode::OK),
        ] {
            fixture.write("config.json", config.as_bytes());
            let (gateway, registration, endpoint) = start(
                &fixture.0,
                AgentManager::new(),
                NoSessions,
                assets.clone(),
                Some(0),
            )
            .await
            .unwrap();
            assert!(endpoint.is_available().await.unwrap());
            for path in ["", "app.js"] {
                let response = http
                    .get(format!("{}{path}", gateway.local_url()))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected_status);
                if expected_status == reqwest::StatusCode::OK {
                    assert_eq!(response.headers()["x-content-type-options"], "nosniff");
                    if path.is_empty() {
                        assert!(
                            response
                                .text()
                                .await
                                .unwrap()
                                .contains("data-mode=\"live\"")
                        );
                    }
                    let forbidden = http
                        .get(format!("{}{path}", gateway.local_url()))
                        .header("Origin", "https://untrusted.example")
                        .send()
                        .await
                        .unwrap();
                    assert_eq!(forbidden.status(), reqwest::StatusCode::FORBIDDEN);
                }
            }
            let unauthorized = http
                .get(format!("{}api/sessions", gateway.local_url()))
                .send()
                .await
                .unwrap();
            assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
            gateway.close().await.unwrap();
            drop(registration);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clients_reject_unsafe_storage() {
        use std::os::unix::fs::PermissionsExt;
        for plugin in ["telegram", "feishu"] {
            for path in [
                "plugins".to_owned(),
                format!("plugins/{plugin}"),
                format!("plugins/{plugin}/workspace"),
                format!("plugins/{plugin}/bridge.lock"),
            ] {
                let fixture = Fixture::new();
                fixture.write(
                    "config.json",
                    &serde_json::to_vec(&serde_json::json!({
                        "plugins":[format!("+builtin:{plugin}")],
                        "telegram":{"botToken":"test","allowedUsers":[1]},
                        "feishu":{"appId":"cli_test","appSecret":"test","allowedUsers":["ou_user"]}
                    }))
                    .unwrap(),
                );
                let directory = fixture.0.join(format!("plugins/{plugin}"));
                std::fs::create_dir_all(&directory).unwrap();
                for directory in [fixture.0.join("plugins"), directory] {
                    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
                let unsafe_path = fixture.0.join(&path);
                if path.ends_with("bridge.lock") {
                    std::fs::write(&unsafe_path, []).unwrap();
                } else if !unsafe_path.exists() {
                    std::fs::create_dir(&unsafe_path).unwrap();
                }
                std::fs::set_permissions(&unsafe_path, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
                let result = start(
                    &fixture.0,
                    AgentManager::new(),
                    NoSessions,
                    fixture.0.clone(),
                    Some(0),
                )
                .await;
                assert!(result.is_err(), "unsafe storage accepted: {path}");
                assert!(discover(&fixture.0).unwrap().is_none());
                assert!(!fixture.0.join("pairing.json").exists());
                assert!(!fixture.0.join("gateway.lock").exists());
            }
        }
    }

    #[tokio::test]
    async fn configured_clients_stay_off() {
        let fixture = Fixture::new();
        fixture.write(
            "config.json",
            br#"{
            "telegram":{"botToken":"test","allowedUsers":[1]},
            "feishu":{"appId":"cli_test","appSecret":"test-secret","allowedUsers":["ou_user"]}
        }"#,
        );
        let (gateway, registration, endpoint) = start(
            &fixture.0,
            AgentManager::new(),
            NoSessions,
            fixture.0.clone(),
            Some(0),
        )
        .await
        .unwrap();
        assert!(endpoint.is_available().await.unwrap());
        assert!(!fixture.0.join("plugins").exists());
        gateway.close().await.unwrap();
        drop(registration);
    }

    #[tokio::test]
    async fn plugins_fail_before_publish() {
        for plugin in ["telegram", "feishu"] {
            let fixture = Fixture::new();
            fixture.write(
                "config.json",
                serde_json::json!({"plugins": [format!("+builtin:{plugin}")]})
                    .to_string()
                    .as_bytes(),
            );
            let error = start(
                &fixture.0,
                AgentManager::new(),
                NoSessions,
                fixture.0.clone(),
                Some(0),
            )
            .await
            .err()
            .unwrap();
            assert!(error.to_string().contains(plugin));
            assert!(!fixture.0.join("pairing.json").exists());
            assert!(!fixture.0.join("endpoint.json").exists());
            assert!(!fixture.0.join("gateway.lock").exists());
        }
    }

    #[tokio::test]
    async fn registration_preserves_a_replaced_endpoint() {
        let fixture = Fixture::new();
        let (gateway, registration, endpoint) = start(
            &fixture.0,
            AgentManager::new(),
            NoSessions,
            fixture.0.clone(),
            Some(0),
        )
        .await
        .unwrap();
        let mut replacement = endpoint.clone();
        replacement.local_credential = crate::pairing::random_credential().unwrap();
        storage::publish(&fixture.0.join("endpoint.json"), &replacement).unwrap();
        assert!(endpoint.retired(&fixture.0).unwrap());
        gateway.close().await.unwrap();
        drop(registration);
        assert!(discover(&fixture.0).unwrap() == Some(replacement));
    }

    #[tokio::test]
    async fn failed_publication_releases_ownership_without_overwriting() {
        let fixture = Fixture::new();
        std::fs::create_dir(fixture.0.join("endpoint.json")).unwrap();
        let start = || {
            start(
                &fixture.0,
                AgentManager::new(),
                NoSessions,
                fixture.0.clone(),
                Some(0),
            )
        };
        assert!(start().await.is_err());
        assert!(fixture.0.join("endpoint.json").is_dir());
        assert!(!fixture.0.join("gateway.lock").exists());
        assert!(std::fs::read_dir(&fixture.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
        std::fs::remove_dir(fixture.0.join("endpoint.json")).unwrap();
        let (gateway, registration, endpoint) = start().await.unwrap();
        assert!(endpoint.is_available().await.unwrap());
        gateway.close().await.unwrap();
        drop(registration);
    }

    #[test]
    fn discovery_rejects_invalid_descriptors() {
        let fixture = storage::tests::Fixture::new();
        assert!(Endpoint::read(&fixture.0).unwrap().is_none());
        let endpoint = Endpoint {
            local_endpoint: "http://127.0.0.1:3141/".into(),
            browser_url: "https://phi.example.com/".into(),
            local_credential: "a".repeat(64),
        };
        fixture.write("endpoint.json", &serde_json::to_vec(&endpoint).unwrap());
        assert!(Endpoint::read(&fixture.0).unwrap() == Some(endpoint.clone()));
        for (field, value) in [
            (
                "localEndpoint",
                serde_json::json!("https://phi.example.com/"),
            ),
            (
                "localEndpoint",
                serde_json::json!("http://127.0.0.1:3141/#token=secret"),
            ),
            (
                "browserUrl",
                serde_json::json!("https://phi.example.com/#token=secret"),
            ),
            ("localCredential", serde_json::json!("invalid")),
            ("unknown", serde_json::json!(true)),
        ] {
            let mut invalid = serde_json::to_value(&endpoint).unwrap();
            invalid[field] = value;
            fixture.write("endpoint.json", &serde_json::to_vec(&invalid).unwrap());
            assert!(Endpoint::read(&fixture.0).is_err(), "{field}");
        }
    }
}
