use std::path::Path;

use anyhow::{Context, ensure};

use super::storage;
use crate::RemoteClient;

/// Short-lived local operator access for the existing device pairing workflow.
pub(crate) struct LocalPairing {
    client: RemoteClient,
    gateway_id: String,
    token: String,
}

impl LocalPairing {
    pub async fn discover(
        directory: &Path,
        endpoint: &str,
        pinned_gateway: Option<&str>,
    ) -> anyhow::Result<Self> {
        let descriptor = super::discover(directory)?
            .context("cannot discover local Gateway; start phi gateway separately")?;
        ensure!(
            endpoint.trim_end_matches('/') == descriptor.local_endpoint.trim_end_matches('/'),
            "configured Gateway does not match the running local Gateway"
        );
        ensure!(
            descriptor.is_available().await?,
            "Phi Gateway is unavailable; Run phi gateway first"
        );
        let document = storage::read_document(directory)?;
        ensure!(
            pinned_gateway.is_none_or(|id| id == document.gateway_id),
            "Gateway identity mismatch"
        );
        Ok(Self {
            client: descriptor.client()?,
            gateway_id: document.gateway_id,
            token: document.entry_token,
        })
    }

    pub fn gateway_id(&self) -> &str {
        &self.gateway_id
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn approve(&self, request_id: &str, device_id: &str) -> anyhow::Result<()> {
        ensure!(
            self.client.devices().await?.pending.iter().any(|request| {
                request.request_id == request_id && request.device_id == device_id
            }),
            "local approval does not match this bridge's pairing request"
        );
        self.client.approve_device(request_id).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deployment::{entry_token, start, storage::tests::Fixture, tests::NoSessions};
    use phi_sdk::agent_manager::AgentManager;
    use serde_json::json;

    #[tokio::test]
    async fn local_pairing_uses_the_published_deployment_without_owning_it() {
        let fixture = Fixture::new();
        let missing = fixture.0.join("absent");
        assert!(
            LocalPairing::discover(&missing, "http://127.0.0.1:3141", None)
                .await
                .is_err()
        );
        assert!(!missing.exists());
        let (gateway, registration, endpoint) = start(
            &fixture.0,
            AgentManager::new(),
            NoSessions,
            fixture.0.join("missing-assets"),
            Some(0),
        )
        .await
        .unwrap();
        let document = storage::read_document(&fixture.0).unwrap();
        let local = LocalPairing::discover(
            &fixture.0,
            gateway.local_url().trim_end_matches('/'),
            Some(&document.gateway_id),
        )
        .await
        .unwrap();
        assert_eq!(local.gateway_id(), document.gateway_id);
        assert_eq!(local.token(), entry_token(&fixture.0).unwrap());
        drop(local);
        assert!(endpoint.is_available().await.unwrap());
        assert!(!endpoint.retired(&fixture.0).unwrap());
        gateway.close().await.unwrap();
        drop(registration);
    }

    #[tokio::test]
    async fn local_operator_pins_identity_and_approves_only_the_matching_request() {
        use axum::{
            Json, Router,
            extract::Path,
            routing::{get, post},
        };
        use std::sync::{Arc, Mutex};

        let approvals = Arc::new(Mutex::new(Vec::new()));
        let recorded = approvals.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/api/sessions", get(|| async { Json(json!([])) }))
            .route("/api/gateway/devices", get(|| async {
                Json(json!({"pending":[
                    {"requestId":"a".repeat(64),"deviceId":"b".repeat(64),"requestedAt":1,"expiresAt":100},
                    {"requestId":"c".repeat(64),"deviceId":"d".repeat(64),"requestedAt":1,"expiresAt":100}
                ],"approved":[]}))
            }))
            .route("/api/gateway/devices/{identity}/approve", post(move |Path(identity): Path<String>| {
                let recorded = recorded.clone();
                async move {
                    recorded.lock().unwrap().push(identity);
                    Json(json!({}))
                }
            }));
        let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let fixture = Fixture::new();
        fixture.write(
            "endpoint.json",
            &serde_json::to_vec(&json!({
                "localEndpoint":endpoint,"browserUrl":endpoint,"localCredential":"e".repeat(64)
            }))
            .unwrap(),
        );
        fixture.write(
            "pairing.json",
            &serde_json::to_vec(&json!({
                "gatewayId":"f".repeat(64),"entryToken":"1".repeat(64),"pending":{},"approved":{}
            }))
            .unwrap(),
        );
        assert!(
            LocalPairing::discover(&fixture.0, &endpoint, Some(&"2".repeat(64)))
                .await
                .is_err()
        );
        let local = LocalPairing::discover(&fixture.0, &endpoint, Some(&"f".repeat(64)))
            .await
            .unwrap();
        assert_eq!(local.token(), "1".repeat(64));
        assert_eq!(local.gateway_id(), "f".repeat(64));
        assert!(
            local
                .approve(&"a".repeat(64), &"d".repeat(64))
                .await
                .is_err()
        );
        assert!(approvals.lock().unwrap().is_empty());
        local
            .approve(&"a".repeat(64), &"b".repeat(64))
            .await
            .unwrap();
        assert_eq!(*approvals.lock().unwrap(), ["a".repeat(64)]);
        for document in [
            json!({"gatewayId":"f".repeat(64),"entryToken":"1".repeat(64)}),
            json!({"gatewayId":"f".repeat(64),"entryToken":"1".repeat(64),"pending":[],"approved":{}}),
            json!({"gatewayId":"f".repeat(64),"entryToken":"1".repeat(64),"pending":{},"approved":{},"unknown":true}),
        ] {
            fixture.write("pairing.json", &serde_json::to_vec(&document).unwrap());
            assert!(
                LocalPairing::discover(&fixture.0, &endpoint, None)
                    .await
                    .is_err(),
                "invalid deployment state must not authorize local pairing"
            );
        }
        serving.abort();
    }

    #[tokio::test]
    async fn local_discovery_rejects_unavailable_or_mismatched_authority() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        fixture.write(
            "endpoint.json",
            &serde_json::to_vec(&json!({
                "localEndpoint": endpoint,
                "browserUrl": endpoint,
                "localCredential": "a".repeat(64),
            }))
            .unwrap(),
        );
        let error = LocalPairing::discover(&fixture.0, &endpoint, None)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "Phi Gateway is unavailable; Run phi gateway first"
        );
        let error = LocalPairing::discover(&fixture.0, "https://phi.example.com", None)
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "configured Gateway does not match the running local Gateway"
        );
        std::fs::set_permissions(&fixture.0, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            LocalPairing::discover(&fixture.0, &endpoint, None)
                .await
                .is_err()
        );
    }
}
