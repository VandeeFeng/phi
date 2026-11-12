use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use reqwest::{Client, Url};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

#[path = "protocol.rs"]
pub(super) mod protocol;

use super::store::Store;
use protocol::{Device, DisplayWatch, Pairing, authenticate};

const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ERROR_CHARS: usize = 1024;
use crate::{RemoteSession, RemoteSnapshot, RemoteSubmission, SavedSession};

struct Identity {
    device: Device,
    _store: Arc<Store>,
}

/// A device-scoped Gateway client; dropping it never stops the Gateway or Agents.
#[derive(Clone)]
pub struct PluginClient {
    gateway: GatewayClient,
    identity: Option<Arc<Identity>>,
}

impl PluginClient {
    pub(super) async fn connect(
        directory: &Path,
        endpoint: &str,
        store: Arc<Store>,
    ) -> anyhow::Result<Self> {
        let mut device = match store.load::<Device>("device.json")? {
            Some(device) => device,
            None => {
                let device = Device::generate()?;
                store.save("device.json", &device)?;
                device
            }
        };
        if device.credential.is_none() {
            let local = crate::deployment::LocalPairing::discover(
                directory,
                endpoint,
                device.gateway.as_deref(),
            )
            .await?;
            authenticate(endpoint, &mut device, &store, Some(Pairing::Local(&local))).await?;
        } else {
            authenticate(endpoint, &mut device, &store, None).await?;
        }
        let gateway = GatewayClient::new(
            endpoint,
            device
                .credential
                .clone()
                .context("missing device credential")?,
        )?;
        Ok(Self {
            gateway,
            identity: Some(Arc::new(Identity {
                device,
                _store: store,
            })),
        })
    }

    pub async fn sessions(&self) -> anyhow::Result<Vec<RemoteSession>> {
        self.gateway.get("api/sessions").await
    }

    pub async fn create(&self, working_dir: &Path) -> anyhow::Result<RemoteSession> {
        self.gateway
            .post("api/sessions", json!({"workingDir":working_dir}))
            .await
    }

    pub async fn resume(&self, id: &str) -> anyhow::Result<RemoteSession> {
        crate::session_segment(id)?;
        self.gateway
            .post("api/sessions/resume", json!({"id":id}))
            .await
    }

    pub async fn saved(&self, query: &str) -> anyhow::Result<Vec<SavedSession>> {
        let mut url = reqwest::Url::parse(self.gateway.endpoint())?.join("api/sessions/saved")?;
        url.query_pairs_mut().append_pair("query", query);
        self.gateway.get(url.as_str()).await
    }

    pub async fn snapshot(&self, session: &str) -> anyhow::Result<RemoteSnapshot> {
        let snapshot: crate::snapshot::Snapshot<Value> = self
            .gateway
            .get(&session_path(session, "snapshot")?)
            .await?;
        snapshot.into_internal()
    }

    pub async fn view(&self, session: &str) -> anyhow::Result<Value> {
        self.gateway.get(&session_path(session, "view")?).await
    }

    pub async fn submit(
        &self,
        session: &str,
        prompt: phi_sdk::turn::TurnPrompt,
        dispatch: phi_sdk::turn::SubmissionDispatch,
    ) -> anyhow::Result<RemoteSubmission> {
        self.gateway
            .post(
                &session_path(session, "submit")?,
                serde_json::to_value(crate::wire::SubmitInput::from_sdk(prompt, dispatch)?)?,
            )
            .await
    }

    pub async fn cancel(&self, session: &str) -> anyhow::Result<Value> {
        self.gateway
            .post(&session_path(session, "cancel")?, json!({}))
            .await
    }

    pub async fn close_session(&self, session: &str) -> anyhow::Result<Value> {
        self.gateway
            .post(&session_path(session, "close")?, json!({}))
            .await
    }

    pub async fn permission(
        &self,
        session: &str,
        agent: &str,
        request: &str,
        decision: phi_sdk::interaction::PermissionDecision,
    ) -> anyhow::Result<Value> {
        let decision = match decision {
            phi_sdk::interaction::PermissionDecision::AllowOnce => "allow_once",
            phi_sdk::interaction::PermissionDecision::AllowAlways(_) => "allow_always",
            phi_sdk::interaction::PermissionDecision::Deny => "deny",
        };
        self.gateway
            .post(
                &session_path(session, "permission")?,
                json!({
                    "agentId":agent,"requestId":request,"decision":decision,
                }),
            )
            .await
    }

    /// Opens one raw Gateway event stream; reconnection policy belongs to the plugin.
    pub async fn watch(&self, session: &str) -> anyhow::Result<PluginWatch> {
        let identity = self
            .identity
            .clone()
            .context("missing plugin device identity")?;
        let stream = DisplayWatch::open(self.gateway.endpoint(), &identity.device, session).await?;
        Ok(PluginWatch {
            stream,
            _identity: identity,
        })
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        self.gateway.get(path).await
    }

    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: Value,
    ) -> anyhow::Result<T> {
        self.gateway.post(path, body).await
    }

    #[cfg(test)]
    pub(crate) fn test_client(endpoint: &str) -> Self {
        Self {
            gateway: GatewayClient::new(endpoint, "a".repeat(64)).unwrap(),
            identity: None,
        }
    }
}

#[derive(Clone)]
struct GatewayClient {
    client: Client,
    base: Url,
    credential: String,
}

impl GatewayClient {
    fn new(endpoint: &str, credential: String) -> anyhow::Result<Self> {
        let base = origin(endpoint)?;
        ensure!(
            crate::pairing::is_identity(&credential),
            "invalid device credential; pair this bridge first"
        );
        Ok(Self {
            client: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(35))
                .build()?,
            base,
            credential,
        })
    }

    fn endpoint(&self) -> &str {
        self.base.as_str()
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> anyhow::Result<T> {
        let response = self
            .client
            .get(self.base.join(path)?)
            .bearer_auth(&self.credential)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Gateway read failed"))?;
        response_body(response).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> anyhow::Result<T> {
        let response = self
            .client
            .post(self.base.join(path)?)
            .bearer_auth(&self.credential)
            .header("Origin", self.base.origin().ascii_serialization())
            .json(&body)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Gateway mutation outcome unknown; not retried"))?;
        response_body(response).await
    }
}

pub(crate) fn session_path(session: &str, operation: &str) -> anyhow::Result<String> {
    crate::session_segment(session)?;
    Ok(format!("api/sessions/{session}/{operation}"))
}

fn origin(endpoint: &str) -> anyhow::Result<Url> {
    let base = Url::parse(endpoint)?;
    ensure!(
        base.scheme() == "http"
            && base.host_str() == Some("127.0.0.1")
            && base.port().is_some()
            && base.username().is_empty()
            && base.password().is_none()
            && base.path() == "/"
            && base.query().is_none()
            && base.fragment().is_none(),
        "v1 requires a clean local Gateway origin: http://127.0.0.1:PORT"
    );
    Ok(base)
}

async fn response_body<T: DeserializeOwned>(mut response: reqwest::Response) -> anyhow::Result<T> {
    let status = response.status();
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("incomplete Gateway response; operation not retried"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_RESPONSE_BYTES,
            "Gateway response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let envelope: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        let reason: String = envelope["error"]
            .as_str()
            .unwrap_or("no structured error reason")
            .chars()
            .take(MAX_ERROR_CHARS)
            .flat_map(char::escape_debug)
            .collect();
        bail!("Gateway returned HTTP {status}: {reason}; operation not retried");
    }
    serde_json::from_slice(&bytes).context("invalid Gateway response; operation not retried")
}

/// Ordered events for one selected Session, without platform-specific projection.
pub struct PluginWatch {
    stream: DisplayWatch,
    _identity: Arc<Identity>,
}

impl PluginWatch {
    pub async fn next(&mut self) -> anyhow::Result<Value> {
        self.stream.next().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_and_session_paths_cannot_escape_the_gateway() {
        for endpoint in [
            "https://example.com",
            "http://localhost:3141",
            "http://127.0.0.1:3141/?token=secret",
            "http://user@127.0.0.1:3141",
            "http://127.0.0.1:3141/path",
        ] {
            assert!(origin(endpoint).is_err());
        }
        assert!(origin("http://127.0.0.1:3141").is_ok());
        for session in ["", "../gateway/stop", "x?y", "x/y"] {
            assert!(session_path(session, "snapshot").is_err());
        }
    }
}
