use super::*;
use axum::{
    Router,
    body::Body,
    http::{StatusCode, header},
    routing::post,
};
use phi_sdk::agent_instance::AgentInstance;
use phi_sdk::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind};
use serde_json::json;
use std::sync::Arc;

pub(super) async fn sdk_office(
    root: &std::path::Path,
    base_url: &str,
) -> (AgentManager, AgentInstance, phi_sdk::Phi) {
    let catalog_path = root.join("models.json");
    std::fs::write(
        &catalog_path,
        json!({"providers":{"local":{
            "api":"openai-completions", "baseUrl":base_url,
            "models":{"test":{"contextWindow":128000}}
        }}})
        .to_string(),
    )
    .unwrap();
    let catalog = Arc::new(
        phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![catalog_path],
            offline: true,
        })
        .unwrap(),
    );
    let phi = phi_sdk::Phi::builder()
        .provider("local")
        .model("test")
        .context(phi_sdk::context::load(true))
        .runtime_config(phi_sdk::config::RuntimeConfig {
            custom_providers: catalog.custom_providers().into_iter().collect(),
            ..Default::default()
        })
        .runtime_services(phi_sdk::PhiRuntimeServices::new().model_catalog(catalog))
        .build()
        .unwrap();
    let manager = AgentManager::new();
    let instance = manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.to_path_buf())
                .tools(Vec::<String>::new())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    (manager, instance, phi)
}

fn supervised_gateway(plugins: crate::plugins::PluginRegistry) -> Gateway {
    let (shutdown, mut closed) = watch::channel(false);
    let (scheduler, scheduler_task) = scheduler::GatewayScheduler::start(shutdown.subscribe());
    let plugin_tasks = crate::plugins::PluginTasks::new(shutdown.subscribe(), scheduler.clone());
    let mut gateway = Gateway {
        url: String::new(),
        local_url: String::new(),
        shutdown,
        task: Some(tokio::spawn(async move {
            let _ = closed.wait_for(|closed| *closed).await;
            Ok(())
        })),
        plugin_tasks,
        scheduler,
        scheduler_task: Some(scheduler_task),
        plugins: Some(plugins),
        shutdown_failures: Vec::new(),
    };
    gateway.start_plugins(std::path::Path::new("/tmp"));
    gateway
}

#[tokio::test]
async fn runtime_plugin_failure_does_not_fail_gateway_close() {
    let (failed, mut failures) = tokio::sync::mpsc::unbounded_channel();
    let mut plugins = crate::plugins::PluginRegistry::new();
    plugins
        .register(
            "test:failed",
            crate::plugins::Plugin::new().with_task(move |_| {
                let failed = failed.clone();
                async move {
                    failed.send(()).unwrap();
                    anyhow::bail!("client authorization rejected")
                }
            }),
        )
        .unwrap();
    let gateway = supervised_gateway(plugins);
    failures.recv().await.unwrap();
    assert!(!gateway.is_stopping());
    gateway.close().await.unwrap();
}

#[tokio::test]
async fn scheduler_failure_stops_gateway_and_closes_wakeups() {
    let mut gateway = supervised_gateway(crate::plugins::PluginRegistry::new());
    let mut wakeup = gateway.scheduler.wakeup();
    gateway.scheduler_task.as_ref().unwrap().abort();
    let result = tokio::time::timeout(Duration::from_secs(1), gateway.wait())
        .await
        .unwrap();
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("Gateway scheduler failed")
    );
    assert!(gateway.is_stopping());
    assert_eq!(wakeup.wait().await, None);
}

#[tokio::test]
async fn plugin_cleanup_survives_wait() {
    let (started, mut cleaning) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let finish = release.clone();
    let mut plugins = crate::plugins::PluginRegistry::new();
    plugins
        .register(
            "test:cleanup",
            crate::plugins::Plugin::new().with_task(move |context| {
                let finish = finish.clone();
                let started = started.clone();
                async move {
                    let mut closed = context.shutdown();
                    started.send(()).unwrap();
                    let _ = closed.wait_for(|closed| *closed).await;
                    started.send(()).unwrap();
                    finish.notified().await;
                    Ok(())
                }
            }),
        )
        .unwrap();
    let mut gateway = supervised_gateway(plugins);
    cleaning.recv().await.unwrap();
    gateway.request_shutdown();
    {
        let waiting = gateway.wait();
        tokio::pin!(waiting);
        tokio::select! {
            _ = cleaning.recv() => {},
            result = &mut waiting => panic!("premature cleanup: {result:?}"),
        }
    }
    let waiting = gateway.wait();
    tokio::pin!(waiting);
    assert!(futures_util::poll!(&mut waiting).is_pending());
    release.notify_one();
    waiting.await.unwrap();
}

#[tokio::test]
async fn plugin_controls_require_wait_and_leave_sibling_running() {
    use crate::plugins::{Plugin, PluginRegistry};
    let (started, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let mut plugins = PluginRegistry::new();
    for name in ["test:first", "test:sibling"] {
        let started = started.clone();
        plugins
            .register(
                name,
                Plugin::new().with_task(move |context| {
                    let started = started.clone();
                    async move {
                        let mut shutdown = context.shutdown();
                        started.send((name, context)).unwrap();
                        let _ = shutdown.wait_for(|stopping| *stopping).await;
                        Ok(())
                    }
                }),
            )
            .unwrap();
    }
    let mut gateway = supervised_gateway(plugins);
    let mut contexts = std::collections::HashMap::new();
    for _ in 0..2 {
        let (name, context) = ready.recv().await.unwrap();
        contexts.insert(name, context);
    }
    let control = gateway.plugin_control();
    {
        let stop = control.stop("test:first");
        tokio::pin!(stop);
        assert!(futures_util::poll!(&mut stop).is_pending());
        assert!(!*contexts["test:first"].shutdown().borrow());
        tokio::select! {
            result = &mut stop => result.unwrap(),
            result = gateway.wait() => panic!("unexpected Gateway termination: {result:?}"),
        }
    }
    assert!(*contexts["test:first"].shutdown().borrow());
    assert!(!*contexts["test:sibling"].shutdown().borrow());
    assert_eq!(
        contexts["test:first"].scheduler().wakeup().wait().await,
        None
    );
    let mut sibling = contexts["test:sibling"].scheduler().wakeup();
    sibling.arm(tokio::time::Instant::now());
    assert_eq!(sibling.wait().await, Some(()));
    tokio::select! {
        result = control.restart("test:first") => result.unwrap(),
        result = gateway.wait() => panic!("unexpected Gateway termination: {result:?}"),
    }
    let (name, context) = ready.recv().await.unwrap();
    assert_eq!(name, "test:first");
    assert!(!*context.shutdown().borrow());
    assert!(!*contexts["test:sibling"].shutdown().borrow());
    gateway.close().await.unwrap();
    assert!(control.restart("test:first").await.is_err());
}

#[tokio::test]
async fn shutdown_before_first_poll_does_not_invoke_plugin_factory() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let mut plugins = crate::plugins::PluginRegistry::new();
    plugins
        .register(
            "test:late",
            crate::plugins::Plugin::new().with_task(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            }),
        )
        .unwrap();
    supervised_gateway(plugins).close().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn close_timeout_joins_aborted_plugin_and_reports_identity() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Released(Arc<AtomicBool>);
    impl Drop for Released {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    let released = Arc::new(AtomicBool::new(false));
    let observed = released.clone();
    let (started, mut ready) = tokio::sync::mpsc::unbounded_channel();
    let mut plugins = crate::plugins::PluginRegistry::new();
    plugins
        .register(
            "test:stuck",
            crate::plugins::Plugin::new().with_task(move |_| {
                let observed = observed.clone();
                let started = started.clone();
                async move {
                    let _released = Released(observed);
                    started.send(()).unwrap();
                    std::future::pending().await
                }
            }),
        )
        .unwrap();
    let gateway = supervised_gateway(plugins);
    ready.recv().await.unwrap();
    let error = gateway.close().await.unwrap_err().to_string();
    assert!(error.contains("gateway shutdown timed out"));
    assert!(error.contains("test:stuck"));
    assert!(released.load(Ordering::SeqCst));
}

struct TestHost {
    manager: AgentManager,
    phi: phi_sdk::Phi,
    directory: PathBuf,
    sessions: std::sync::Mutex<Vec<RemoteSession>>,
    waiting: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl TestHost {
    async fn recover_root(
        &self,
        id: AgentInstanceId,
        boundary: Option<String>,
    ) -> anyhow::Result<RemoteSnapshot> {
        let instance = self.manager.get(&id)?;
        let view = match boundary {
            Some(boundary) => instance.view_with_event_boundary(boundary).await?,
            None => instance.view().await?,
        };
        let session = self.resume(id.to_string()).await?;
        let interactions = view
            .snapshot
            .pending_interactions
            .iter()
            .cloned()
            .map(|request| RemoteInteraction {
                agent_id: id.to_string(),
                request,
            })
            .collect();
        Ok(RemoteSnapshot {
            session,
            view,
            interactions,
            application: json!({}),
        })
    }
}

impl RemoteHost for TestHost {
    type Recovery = Value;

    async fn recovery(&self, id: AgentInstanceId) -> anyhow::Result<RemoteSnapshot> {
        self.recover_root(id, None).await
    }

    async fn recovery_at_boundary(
        &self,
        id: AgentInstanceId,
        boundary: String,
    ) -> anyhow::Result<RemoteSnapshot> {
        self.recover_root(id, Some(boundary)).await
    }

    async fn configure(
        &self,
        _: AgentInstanceId,
        _: crate::RemoteSelection,
    ) -> anyhow::Result<RemoteRuntimeUpdate<Value>> {
        anyhow::bail!("runtime configuration is not provided by this test host")
    }
    async fn reload(&self, _: AgentInstanceId) -> anyhow::Result<RemoteRuntimeUpdate<Value>> {
        anyhow::bail!("resource reload is not provided by this test host")
    }
    async fn catalog(&self, _: AgentInstanceId) -> anyhow::Result<crate::RemoteCatalog> {
        anyhow::bail!("catalog is not provided by this test host")
    }
    async fn new_session(&self, _: AgentInstanceId) -> anyhow::Result<RemoteSession> {
        self.create(self.directory.clone()).await
    }

    async fn saved(&self, _: String) -> anyhow::Result<Vec<SavedSession>> {
        Ok(Vec::new())
    }
    async fn resume(&self, id: String) -> anyhow::Result<RemoteSession> {
        self.sessions
            .lock()
            .unwrap()
            .iter()
            .find(|session| session.id == id)
            .cloned()
            .context("unknown Session")
    }
    async fn close(&self, id: AgentInstanceId) -> anyhow::Result<()> {
        self.manager.stop(&id).await?;
        self.sessions
            .lock()
            .unwrap()
            .retain(|session| session.id != id.as_str());
        Ok(())
    }
    async fn sessions(&self) -> anyhow::Result<Vec<RemoteSession>> {
        Ok(self.sessions.lock().unwrap().clone())
    }

    async fn create(&self, working_dir: PathBuf) -> anyhow::Result<RemoteSession> {
        let instance = self.manager.insert(
            self.phi
                .agent_instance_builder()
                .working_dir(working_dir.clone())
                .tools(Vec::<String>::new())
                .build()
                .await?,
        )?;
        let session = RemoteSession {
            id: instance.id().to_string(),
            working_dir,
            ..labels()
        };
        self.sessions.lock().unwrap().push(session.clone());
        Ok(session)
    }

    async fn submit(
        &self,
        session: AgentInstanceId,
        prompt: TurnPrompt,
        dispatch: SubmissionDispatch,
    ) -> anyhow::Result<RemoteSubmission> {
        if prompt.prompt == "  Build\n the office  " {
            self.waiting.notify_one();
            self.release.notified().await;
        }
        Ok(RemoteSubmission {
            disposition: self
                .manager
                .get(&session)?
                .dispatch(prompt, dispatch)
                .await?,
            notices: Vec::new(),
        })
    }

    async fn message(
        &self,
        session: AgentInstanceId,
        target: AgentInstanceId,
        message: String,
    ) -> anyhow::Result<phi_sdk::agent_manager::AgentMessageReceipt> {
        self.resume(session.to_string()).await?;
        let access = self.manager.for_agent(&session)?;
        ensure!(
            access
                .list()?
                .iter()
                .any(|member| member.instance.id == target
                    && member.created_by.as_ref() == Some(&session)),
            ErrorCode::Forbidden
        );
        Ok(access.send_message(&target, message).await?)
    }

    async fn interactions(
        &self,
        session: AgentInstanceId,
    ) -> anyhow::Result<Vec<(AgentInstanceId, Vec<InteractionEnvelope>)>> {
        self.resume(session.to_string()).await?;
        let pending = self.manager.get(&session)?.snapshot().pending_interactions;
        Ok(vec![(session, pending)])
    }

    async fn respond(
        &self,
        session: AgentInstanceId,
        agent: AgentInstanceId,
        request: String,
        response: InteractionResponse,
    ) -> anyhow::Result<Option<String>> {
        self.resume(session.to_string()).await?;
        ensure!(session == agent, ErrorCode::Forbidden);
        self.manager
            .get(&agent)?
            .respond_interaction(request, response)
            .await?;
        Ok(None)
    }
}

struct AdmissionStore(std::sync::atomic::AtomicBool, tokio::sync::Notify);

impl crate::pairing::PairingStore for AdmissionStore {
    fn gateway_id(&self) -> anyhow::Result<String> {
        Ok("a".repeat(64))
    }
    fn admit(
        &self,
        _: &crate::pairing::ChallengeRequest,
        _: &str,
    ) -> anyhow::Result<Option<String>> {
        anyhow::bail!("not enrolled")
    }
    fn complete(
        &self,
        _: &crate::pairing::DeviceChallenge,
        _: &str,
        _: u64,
    ) -> anyhow::Result<crate::pairing::PairingOutcome> {
        anyhow::bail!("not enrolled")
    }
    fn authenticate(
        &self,
        credential: &str,
    ) -> anyhow::Result<crate::pairing::DeviceAuthorization> {
        ensure!(
            credential == "d".repeat(64) && self.0.load(std::sync::atomic::Ordering::SeqCst),
            "not approved"
        );
        self.1.notify_one();
        Ok(crate::pairing::DeviceAuthorization {
            device_id: "e".repeat(64),
            approval_generation: "f".repeat(64),
            token_digest: crate::pairing::credential_digest(credential),
        })
    }
    fn authorize(&self, authorization: &crate::pairing::DeviceAuthorization) -> anyhow::Result<()> {
        ensure!(
            self.0.load(std::sync::atomic::Ordering::SeqCst)
                && authorization.device_id == "e".repeat(64)
                && authorization.approval_generation == "f".repeat(64)
                && authorization.token_digest == crate::pairing::credential_digest(&"d".repeat(64)),
            "not approved"
        );
        Ok(())
    }
    fn pairing_status(
        &self,
        _: &str,
        _: &str,
        _: u64,
    ) -> anyhow::Result<crate::pairing::PairingStatus> {
        Ok(crate::pairing::PairingStatus::Rejected)
    }
    fn devices(&self, _: u64) -> anyhow::Result<crate::pairing::DeviceList> {
        Ok(crate::pairing::DeviceList {
            pending: Vec::new(),
            approved: Vec::new(),
        })
    }
    fn approve(&self, _: &str, _: u64) -> anyhow::Result<()> {
        anyhow::bail!("no pending request")
    }
    fn reject(&self, _: &str, _: u64) -> anyhow::Result<()> {
        anyhow::bail!("no pending request")
    }
    fn revoke(&self, _: &str) -> anyhow::Result<()> {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn paired_transport_separates_proxy_devices_and_local_administration() {
    use crate::pairing::PairingStore;

    let directory = PathBuf::from("/tmp").join(format!("phi-paired-ws-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("app.js"), "").unwrap();
    std::fs::write(directory.join("index.html"), "<body data-mode=\"demo\">").unwrap();
    let (manager, _, phi) = sdk_office(&directory, "http://127.0.0.1:1/v1").await;
    let host = TestHost {
        manager: manager.clone(),
        phi,
        directory: directory.clone(),
        sessions: std::sync::Mutex::new(Vec::new()),
        waiting: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let store = Arc::new(AdmissionStore(
        std::sync::atomic::AtomicBool::new(true),
        tokio::sync::Notify::new(),
    ));
    let gateway = Gateway::serve_paired(
        manager.clone(),
        host,
        directory.clone(),
        crate::GatewayConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("https://phi.example.com"),
        )
        .unwrap(),
        "c".repeat(64),
        GatewayAccess::new(store.clone(), vec!["127.0.0.1".parse().unwrap()]),
    )
    .await
    .unwrap();
    assert_eq!(gateway.url(), "https://phi.example.com/");
    assert!(!gateway.local_url().contains("token"));
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let local = gateway.local_url().trim_end_matches('/');
    assert_eq!(
        http.get(format!("{local}/api/sessions"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for credential in ["c".repeat(64), "d".repeat(64)] {
        assert_eq!(
            http.get(format!("{local}/api/sessions"))
                .bearer_auth(credential)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
    }
    let public_read = || {
        http.get(format!("{local}/api/sessions"))
            .header("host", "phi.example.com")
            .header("origin", "https://phi.example.com")
            .header("x-forwarded-for", "192.0.2.10")
    };
    assert_eq!(
        public_read()
            .bearer_auth("d".repeat(64))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        public_read()
            .bearer_auth("c".repeat(64))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        public_read()
            .bearer_auth("d".repeat(64))
            .header("authorization", format!("Bearer {}", "c".repeat(64)))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.post(format!("{local}/api/gateway/stop"))
            .header("host", "phi.example.com")
            .header("origin", "https://phi.example.com")
            .header("x-forwarded-for", "192.0.2.10")
            .bearer_auth("d".repeat(64))
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    store.revoke("").unwrap();
    assert_eq!(
        public_read()
            .bearer_auth("d".repeat(64))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    store.0.store(true, std::sync::atomic::Ordering::SeqCst);
    let operator = crate::RemoteClient::connect(&local, &"c".repeat(64)).unwrap();
    assert!(operator.devices().await.unwrap().pending.is_empty());
    assert!(operator.approve_device(&"e".repeat(64)).await.is_err());
    assert!(operator.reject_device(&"e".repeat(64)).await.is_err());
    assert!(operator.approve_device("invalid").await.is_err());
    assert_eq!(
        http.get(format!("{local}/api/gateway/devices"))
            .header("host", "phi.example.com")
            .header("origin", "https://phi.example.com")
            .header("x-forwarded-for", "192.0.2.10")
            .bearer_auth("d".repeat(64))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    for path in ["/auth/challenge"] {
        assert_eq!(
            http.get(format!("{local}{path}"))
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        http.get(format!("{local}/healthz"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let endpoint = format!("ws://{}/ws", local.trim_start_matches("http://"));
    let mut request = endpoint.clone().into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "phi.remote.v1".parse().unwrap());
    let (mut client, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();
    let challenge: Value =
        serde_json::from_str(client.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(challenge["event"], "connect.challenge");
    client.send(Message::Text(json!({"type":"req","id":"1","method":"connect","params":{"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"c".repeat(64)}}}).to_string().into())).await.unwrap();
    let hello: Value =
        serde_json::from_str(client.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(hello["payload"]["status"], "ready");
    let fixtures: Value =
        serde_json::from_str(include_str!("../../fixtures/remote-v1.json")).unwrap();
    for frame in fixtures["invalid"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|frame| {
            frame["type"] == "req" && frame["id"] == "1" && frame.get("params").is_some()
        })
    {
        let (mut rejected, _) = tokio_tungstenite::connect_async(request.clone())
            .await
            .unwrap();
        rejected.next().await.unwrap().unwrap();
        rejected.send(Message::Text(json!({"type":"req","id":"1","method":"connect","params":{"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"c".repeat(64)}}}).to_string().into())).await.unwrap();
        let hello: Value =
            serde_json::from_str(rejected.next().await.unwrap().unwrap().to_text().unwrap())
                .unwrap();
        assert_eq!(hello["payload"]["status"], "ready");
        let mut frame = frame.clone();
        frame["id"] = json!("2");
        rejected
            .send(Message::Text(frame.to_string().into()))
            .await
            .unwrap();
        let closed = tokio::time::timeout(Duration::from_secs(5), rejected.next())
            .await
            .unwrap();
        assert!(
            matches!(closed, Some(Ok(Message::Close(_)))),
            "{frame}: {closed:?}"
        );
    }
    let mut request = endpoint.clone().into_client_request().unwrap();
    for (name, value) in [
        ("sec-websocket-protocol", "phi.remote.v1"),
        ("host", "phi.example.com"),
        ("origin", "https://phi.example.com"),
        ("x-forwarded-for", "192.0.2.10"),
    ] {
        request.headers_mut().insert(
            axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    let (mut browser, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();
    browser.next().await.unwrap().unwrap();
    browser.send(Message::Text(json!({"type":"req","id":"1","method":"connect","params":{"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"c".repeat(64)}}}).to_string().into())).await.unwrap();
    let denied: Value =
        serde_json::from_str(browser.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(denied["error"]["code"], "UNAUTHENTICATED");
    request.headers_mut().remove("origin");
    assert!(tokio_tungstenite::connect_async(request).await.is_err());
    let mut invalid = format!("{endpoint}?token=forbidden")
        .into_client_request()
        .unwrap();
    invalid
        .headers_mut()
        .insert("sec-websocket-protocol", "phi.remote.v1".parse().unwrap());
    assert!(tokio_tungstenite::connect_async(invalid).await.is_err());
    drop(client);
    assert!(!gateway.is_stopping());
    operator.stop().await.unwrap();
    gateway.close().await.unwrap();
    manager.shutdown().await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn gateway_isolates_sessions_and_observers_do_not_own_agents() {
    let directory = PathBuf::from("/tmp").join(format!("phi-remote-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(directory.join("assets")).unwrap();
    std::fs::write(directory.join("assets/app.js"), "").unwrap();
    std::fs::write(
        directory.join("assets/index.html"),
        "<body data-mode=\"demo\">",
    )
    .unwrap();
    let (manager, office, phi) = sdk_office(&directory, "http://127.0.0.1:1/v1").await;
    let first = office.id().to_string();
    let waiting = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let host = TestHost {
        manager: manager.clone(),
        phi,
        directory: directory.clone(),
        sessions: std::sync::Mutex::new(vec![RemoteSession {
            id: first.clone(),
            name: String::new(),
            ..labels()
        }]),
        waiting: waiting.clone(),
        release: release.clone(),
    };
    let token = "c".repeat(64);
    let mut config = crate::GatewayConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap();
    config.plugins = crate::plugins::Configuration::new(
        crate::plugins::Selection::try_from(vec!["+builtin:web".into()]).unwrap(),
        None,
        None,
    )
    .unwrap()
    .register(&directory, directory.join("assets"))
    .unwrap();
    let gateway = Gateway::serve(
        manager.clone(),
        host,
        directory.join("assets"),
        config,
        token.clone(),
    )
    .await
    .unwrap();
    let base = gateway.local_url().trim_end_matches('/');
    assert_eq!(gateway.url(), gateway.local_url());
    assert!(!gateway.url().contains('#'));
    let token = token.as_str();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let client = crate::RemoteClient::connect(gateway.local_url(), token).unwrap();
    assert_eq!(client.sessions().await.unwrap()[0].id, first);
    let initial = client.view(&first).await.unwrap();
    assert_eq!(initial["sessionId"], first);
    assert_eq!(initial["sessionName"], "");
    let snapshot: Value = http
        .get(format!("{base}/api/sessions/{first}/snapshot"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["version"], 2);
    assert!(snapshot.get("view").is_none());
    assert!(snapshot["history"].get("switch_snapshots").is_none());
    assert!(snapshot["execution"].get("bash_executions").is_none());
    assert_eq!(client.snapshot(&first).await.unwrap().session.id, first);
    assert_eq!(
        http.post(format!("{base}/api/sessions/{first}/submit"))
            .bearer_auth(token)
            .header("Origin", base)
            .json(&json!({"text":"inspect", "delivery":"steer", "source":"internal"}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert!(
        http.get(base)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap()
            .contains("data-mode=\"live\"")
    );
    assert_eq!(
        http.get(format!("{base}/api/sessions"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        http.get(format!("{base}/api/sessions"))
            .bearer_auth(token)
            .header("Origin", "http://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.post(format!("{base}/api/sessions"))
            .bearer_auth(token)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        http.post(format!("{base}/api/sessions/{first}/message"))
            .bearer_auth(token)
            .header("Origin", base)
            .json(&json!({"message":"x".repeat(MAX_BODY_BYTES)}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    for request in [json!({}), json!({"workingDir": null})] {
        assert_eq!(
            http.post(format!("{base}/api/sessions"))
                .bearer_auth(token)
                .header("Origin", base)
                .json(&request)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(client.sessions().await.unwrap().len(), 1);
    let second: RemoteSession = http
        .post(format!("{base}/api/sessions"))
        .bearer_auth(token)
        .header("Origin", base)
        .json(&json!({"workingDir": directory}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(first, second.id);
    assert_eq!(client.sessions().await.unwrap().len(), 2);
    assert_eq!(client.view(&first).await.unwrap()["sessionId"], first);
    assert_eq!(
        client.view(&second.id).await.unwrap()["sessionId"],
        second.id
    );
    assert_eq!(
        client.view(&second.id).await.unwrap()["sessionName"],
        "Office session"
    );
    let submitted = client
        .submit(
            &second.id,
            TurnPrompt {
                prompt: "inspect reference".into(),
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Internal,
                images: Vec::new(),
                injected_context: vec![phi_sdk::turn::InjectedContext {
                    source: phi_sdk::transcript::ContextSource::FileMention,
                    content: "reference contents".into(),
                }],
                tool_policy: Default::default(),
            },
            SubmissionDispatch::FollowUp,
        )
        .await
        .unwrap();
    assert!(matches!(submitted.disposition,
        SubmissionDisposition::Started { ref turn_id, ref prompt }
            if !turn_id.is_empty() && prompt == "inspect reference"
    ));
    let recovered = client.snapshot(&second.id).await.unwrap();
    assert!(recovered.view.transcript.entries.iter().any(|entry| matches!(
        &entry.kind,
        TranscriptEntryKind::Message { message: AgentMessage::Context { source, content, .. } }
            if *source == phi_sdk::transcript::ContextSource::FileMention
                && matches!(content.as_slice(), [ContentBlock::Text { text }] if text.as_str() == "reference contents")
    )));
    assert!(client.view("foreign").await.is_err());
    for invalid in [
        "",
        "../sessions",
        "id?query",
        "id#fragment",
        &"a".repeat(257),
    ] {
        assert!(client.view(invalid).await.is_err());
        assert!(client.resume(invalid).await.is_err());
    }
    assert!(
        http.post(format!("{base}/api/sessions/{first}/message"))
            .bearer_auth(token)
            .header("Origin", base)
            .json(&json!({"agentId":second.id,"message":"Do not deliver"}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .is_err()
    );
    assert!(client.message(&first, " ").await.is_err());
    let submission = http
        .post(format!("{base}/api/sessions/{first}/message"))
        .bearer_auth(token)
        .header("Origin", base)
        .json(&json!({"message":"  Build\n the office  "}));
    let submitted = tokio::spawn(async move { submission.send().await.unwrap() });
    tokio::time::timeout(Duration::from_secs(2), waiting.notified())
        .await
        .unwrap();
    assert_eq!(client.sessions().await.unwrap().len(), 2);
    release.notify_one();
    assert_eq!(submitted.await.unwrap().status(), StatusCode::OK);
    assert_eq!(
        client.view(&first).await.unwrap()["sessionName"],
        "Build the office"
    );
    drop(client);
    assert!(manager.get(&AgentInstanceId::new(&first)).is_ok());
    gateway.close().await.unwrap();
    assert!(manager.get(&AgentInstanceId::new(second.id)).is_ok());
    manager.shutdown().await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn http_revalidates_pending_bodies_and_deliveries_and_drains_without_waiters() {
    use crate::pairing::PairingStore;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let directory = PathBuf::from("/tmp").join(format!("phi-http-auth-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("app.js"), "").unwrap();
    let (manager, _, phi) = sdk_office(&directory, "http://127.0.0.1:1/v1").await;
    let waiting = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let host = TestHost {
        manager: manager.clone(),
        phi,
        directory: directory.clone(),
        sessions: std::sync::Mutex::new(Vec::new()),
        waiting: waiting.clone(),
        release: release.clone(),
    };
    let store = Arc::new(AdmissionStore(
        std::sync::atomic::AtomicBool::new(true),
        tokio::sync::Notify::new(),
    ));
    let gateway = Gateway::serve_paired(
        manager.clone(),
        host,
        directory.clone(),
        crate::GatewayConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            Some("https://phi.example.com"),
        )
        .unwrap(),
        "c".repeat(64),
        GatewayAccess::new(store.clone(), vec!["127.0.0.1".parse().unwrap()]),
    )
    .await
    .unwrap();
    let base = gateway.local_url().trim_end_matches('/').to_owned();
    let address = base.trim_start_matches("http://");
    let mut pending = tokio::net::TcpStream::connect(address).await.unwrap();
    pending.write_all(format!(
        "POST /api/sessions HTTP/1.1\r\nHost: phi.example.com\r\nOrigin: https://phi.example.com\r\nX-Forwarded-For: 192.0.2.10\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{",
        "d".repeat(64),
    ).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), store.1.notified())
        .await
        .unwrap();
    store.revoke("").unwrap();
    pending.write_all(b"}").await.unwrap();
    let mut denied = String::new();
    tokio::time::timeout(Duration::from_secs(5), pending.read_to_string(&mut denied))
        .await
        .unwrap()
        .unwrap();
    assert!(denied.starts_with("HTTP/1.1 401"), "{denied}");

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let sessions: Vec<RemoteSession> = http
        .get(format!("{base}/api/sessions"))
        .bearer_auth("c".repeat(64))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(sessions.is_empty());
    store.0.store(true, std::sync::atomic::Ordering::SeqCst);
    let created: RemoteSession = http
        .post(format!("{base}/api/sessions"))
        .bearer_auth("c".repeat(64))
        .header("origin", &base)
        .json(&json!({"workingDir": directory}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let root = manager.get(&AgentInstanceId::new(&created.id)).unwrap();
    let mut events = root.subscribe();
    for marker in ["1".repeat(64), "2".repeat(64)] {
        let mut url =
            reqwest::Url::parse(&format!("{base}/api/sessions/{}/snapshot", created.id)).unwrap();
        url.query_pairs_mut().append_pair("eventBoundary", &marker);
        let response: Value = http
            .get(url)
            .bearer_auth("c".repeat(64))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(response["eventBoundary"], marker);
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event, phi_sdk::agent_instance::AgentInstanceEvent::ViewBoundary { id } if id == marker)
        );
    }
    assert_eq!(
        http.get(format!(
            "{base}/api/sessions/{}/snapshot?eventBoundary=invalid",
            created.id
        ))
        .bearer_auth("c".repeat(64))
        .send()
        .await
        .unwrap()
        .status(),
        StatusCode::BAD_REQUEST
    );
    let mut submissions = Vec::new();
    for _ in 0..MAX_REQUESTS {
        let request = http
            .post(format!("{base}/api/sessions/{}/submit", created.id))
            .header("host", "phi.example.com")
            .header("origin", "https://phi.example.com")
            .header("x-forwarded-for", "192.0.2.10")
            .bearer_auth("d".repeat(64))
            .json(&json!({"text":"  Build\n the office  ", "delivery":"follow_up"}));
        submissions.push(tokio::spawn(async move { request.send().await.unwrap() }));
        tokio::time::timeout(Duration::from_secs(5), waiting.notified())
            .await
            .unwrap();
    }
    assert_eq!(
        http.get(format!("{base}/api/sessions"))
            .bearer_auth("c".repeat(64))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        http.post(format!("{base}/api/sessions/{}/cancel", created.id))
            .bearer_auth("c".repeat(64))
            .header("origin", &base)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        http.post(format!("{base}/api/sessions/{}/permission", created.id))
            .bearer_auth("c".repeat(64))
            .header("origin", &base)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );

    let abandoned = submissions.pop().unwrap();
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());
    store.revoke("").unwrap();
    assert_eq!(
        http.post(format!("{base}/api/gateway/stop"))
            .bearer_auth("c".repeat(64))
            .header("origin", &base)
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let mut closing = tokio::spawn(gateway.close());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut closing)
            .await
            .is_err()
    );
    release.notify_waiters();
    for submission in submissions {
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), submission)
                .await
                .unwrap()
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
    tokio::time::timeout(Duration::from_secs(5), closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(manager.get(&AgentInstanceId::new(created.id)).is_ok());
    manager.shutdown().await.unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn root_watches_stream_before_completion_and_keep_independent_selection() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;
    async fn receive(socket: &mut Socket) -> Value {
        let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_str(frame.to_text().unwrap()).unwrap()
    }
    async fn send(socket: &mut Socket, id: u64, method: &str, params: Value) {
        socket
            .send(Message::Text(
                json!({"type":"req","id":id.to_string(),"method":method,"params":params})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }
    let release = Arc::new(tokio::sync::Notify::new());
    let producer = release.clone();
    let provider = Router::new().route("/v1/chat/completions", post(move || {
        let release = producer.clone();
        async move {
            let chunks = futures_util::stream::once(async {
                Ok::<_, std::io::Error>("data: {\"id\":\"provider-private\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"first\\n\"},\"finish_reason\":null}]}\n\n")
            }).chain(futures_util::stream::once(async move {
                release.notified().await;
                Ok::<_, std::io::Error>("data: {\"id\":\"provider-private\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" last\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"provider-private\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
            }));
            ([(header::CONTENT_TYPE, "text/event-stream")], Body::from_stream(chunks))
        }
    }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let provider_task = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
    let directory = PathBuf::from("/tmp").join(format!("phi-root-stream-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("app.js"), "").unwrap();
    let (manager, unhosted, phi) = sdk_office(&directory, &format!("http://{address}/v1")).await;
    let host = TestHost {
        manager: manager.clone(),
        phi,
        directory: directory.clone(),
        sessions: std::sync::Mutex::new(Vec::new()),
        waiting: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
    };
    let gateway = Gateway::serve_paired(
        manager.clone(),
        host,
        directory.clone(),
        crate::GatewayConfig::new("127.0.0.1:0".parse().unwrap(), None).unwrap(),
        "c".repeat(64),
        GatewayAccess::new(
            Arc::new(AdmissionStore(
                std::sync::atomic::AtomicBool::new(true),
                tokio::sync::Notify::new(),
            )),
            Vec::new(),
        ),
    )
    .await
    .unwrap();
    let client = crate::RemoteClient::connect(gateway.local_url(), &"c".repeat(64)).unwrap();
    let first = client.create(&directory).await.unwrap();
    let target = directory.join("target project");
    std::fs::create_dir_all(&target).unwrap();
    let second = client.create(&target).await.unwrap();
    assert_eq!(second.working_dir, target);
    assert_eq!(
        manager
            .get(&AgentInstanceId::new(&second.id))
            .unwrap()
            .snapshot()
            .working_dir,
        target
    );
    let mut request = format!("{}ws", gateway.local_url().replacen("http://", "ws://", 1))
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "phi.remote.v1".parse().unwrap());
    let (mut left, _) = tokio_tungstenite::connect_async(request.clone())
        .await
        .unwrap();
    let (mut right, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    for socket in [&mut left, &mut right] {
        receive(socket).await;
        send(socket, 1, "connect", json!({"minProtocol":1,"maxProtocol":1,"auth":{"kind":"local","credential":"c".repeat(64)}})).await;
        assert_eq!(receive(socket).await["payload"]["status"], "ready");
    }
    send(&mut left, 2, "session.watch", json!({"sessionId":first.id})).await;
    assert_eq!(
        receive(&mut left).await["payload"],
        json!({"sessionId":first.id,"watchId":"2"})
    );
    send(
        &mut right,
        2,
        "session.watch",
        json!({"sessionId":second.id}),
    )
    .await;
    assert_eq!(receive(&mut right).await["payload"]["sessionId"], second.id);
    for (id, params, code) in [
        (3, json!({"sessionId":"unknown"}), "NOT_FOUND"),
        (4, json!({"sessionId":"../invalid"}), "INVALID_REQUEST"),
        (5, json!({"sessionId":unhosted.id().as_str()}), "NOT_FOUND"),
        (
            6,
            json!({"sessionId":second.id,"subscriptionId":"unsupported"}),
            "INVALID_REQUEST",
        ),
    ] {
        send(&mut right, id, "session.watch", params).await;
        assert_eq!(receive(&mut right).await["error"]["code"], code);
    }
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let marker = "4".repeat(64);
    let snapshot: Value = http
        .get(format!(
            "{}api/sessions/{}/snapshot?eventBoundary={marker}",
            gateway.local_url(),
            first.id
        ))
        .bearer_auth("c".repeat(64))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot["eventBoundary"], marker);
    let boundary = receive(&mut left).await;
    assert_eq!(boundary["event"], "session.event");
    assert_eq!(
        boundary["payload"],
        json!({"sessionId":first.id,"watchId":"2","event":{"kind":"boundary","id":marker}})
    );
    assert_eq!(
        client
            .watch("unknown")
            .await
            .err()
            .unwrap()
            .downcast_ref::<ErrorCode>(),
        Some(&ErrorCode::NotFound)
    );
    let rejected = crate::RemoteClient::connect(gateway.local_url(), &"a".repeat(64)).unwrap();
    assert_eq!(
        rejected
            .watch(&first.id)
            .await
            .err()
            .unwrap()
            .downcast_ref::<ErrorCode>(),
        Some(&ErrorCode::Unauthenticated)
    );
    let mut native = client.watch(&first.id).await.unwrap();
    assert!(native.next().await.is_err());
    let initial: RemoteSnapshot = native.recovery().await.unwrap();
    assert_eq!(initial.session.id, first.id);
    client.message(&first.id, "stream now").await.unwrap();
    let mut saw_progress = false;
    loop {
        let event = receive(&mut left).await;
        assert_eq!(event["payload"]["sessionId"], first.id);
        assert_eq!(event["payload"]["watchId"], "2");
        assert!(!event.to_string().contains("provider-private"));
        let event = &event["payload"]["event"];
        saw_progress |= event["kind"] == "progress";
        if event["kind"] == "phi"
            && event["event"]["type"] == "assistant_delta"
            && event["event"]["display"] != ""
        {
            assert_eq!(event["event"]["display"], "first\n");
            break;
        }
    }
    assert!(saw_progress);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), native.next())
            .await
            .unwrap()
            .unwrap();
        if let crate::RemoteEvent::Phi(envelope) = event
            && let phi_sdk::PhiEvent::AssistantDelta { display, original } = envelope.event
            && !display.is_empty()
        {
            assert_eq!(envelope.session_id, first.id);
            assert_eq!(display, "first\n");
            assert!(original.is_empty());
            break;
        }
    }
    assert!(
        manager
            .get(&AgentInstanceId::new(&first.id))
            .unwrap()
            .snapshot()
            .active_turn_id
            .is_some()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), right.next())
            .await
            .is_err()
    );
    release.notify_one();
    loop {
        let event = receive(&mut left).await;
        if event["payload"]["event"]["event"]["type"] == "turn_finished" {
            break;
        }
    }
    send(&mut left, 3, "session.unwatch", json!({})).await;
    loop {
        let event = receive(&mut left).await;
        if event["id"] == "3" {
            assert_eq!(event["ok"], true);
            break;
        }
    }
    manager
        .get(&AgentInstanceId::new(&first.id))
        .unwrap()
        .view_with_event_boundary("unwatched".into())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), left.next())
            .await
            .is_err()
    );
    send(
        &mut left,
        4,
        "session.watch",
        json!({"sessionId":second.id}),
    )
    .await;
    assert_eq!(receive(&mut left).await["payload"]["watchId"], "4");
    manager
        .get(&AgentInstanceId::new(&second.id))
        .unwrap()
        .view_with_event_boundary("shared".into())
        .await
        .unwrap();
    for (socket, watch) in [(&mut left, "4"), (&mut right, "2")] {
        let event = receive(socket).await;
        assert_eq!(event["payload"]["watchId"], watch);
        assert_eq!(event["payload"]["event"]["id"], "shared");
    }
    left.close(None).await.unwrap();
    drop(left);
    assert!(manager.get(&AgentInstanceId::new(&second.id)).is_ok());
    let recovered: RemoteSnapshot = native.recovery().await.unwrap();
    drop(native);
    assert!(
        serde_json::to_string(&recovered.view.transcript)
            .unwrap()
            .contains("first\\n last")
    );
    gateway.close().await.unwrap();
    manager.shutdown().await.unwrap();
    provider_task.abort();
    std::fs::remove_dir_all(directory).unwrap();
}

fn projected_event(event: phi_sdk::PhiEvent) -> Value {
    events::project(phi_sdk::agent_instance::AgentInstanceEvent::Phi(
        phi_sdk::PhiEventEnvelope {
            id: "event-1".into(),
            emitted_at: chrono::Utc::now(),
            session_id: "root".into(),
            turn_id: Some("turn-1".into()),
            message_id: Some("message-1".into()),
            agent_id: Some("root".into()),
            parent_agent_id: None,
            parent_tool_call_id: None,
            event,
        },
    ))
}

proptest::proptest! {
    #[test]
    fn stream_projection_preserves_display_but_not_provider_identity(provider_id in "provider-[a-z]{1,24}", display in ".{0,40}") {
        let delta = projected_event(phi_sdk::PhiEvent::AssistantDelta { original: provider_id.clone(), display: display.clone() });
        proptest::prop_assert_eq!(&delta["event"]["display"], &json!(display));
        proptest::prop_assert!(delta["event"].get("original").is_none());
        let decoded = crate::RemoteEvent::decode(delta, "root").unwrap();
        proptest::prop_assert!(matches!(decoded, crate::RemoteEvent::Phi(phi_sdk::PhiEventEnvelope {
            event: phi_sdk::PhiEvent::AssistantDelta { original, .. }, ..
        }) if original.is_empty()), "private original text must not be reconstructed");
        let started = projected_event(phi_sdk::PhiEvent::ToolCallStarted {
            id: "sdk-call".into(), provider_call_id: Some(provider_id.clone()), provider_item_id: Some(provider_id.clone()),
            name: "read".into(), args: json!({"path":"/tmp/example"}), display: None,
        });
        proptest::prop_assert_eq!(&started["event"]["id"], "sdk-call");
        proptest::prop_assert!(started["event"].get("provider_call_id").is_none());
        proptest::prop_assert!(started["event"].get("provider_item_id").is_none());
        let finished = projected_event(phi_sdk::PhiEvent::AssistantFinished {
            id: "sdk-message".into(), content: vec![ContentBlock::ToolCall {
                id: "sdk-call".into(), provider_call_id: Some(provider_id.clone().into()), provider_item_id: Some(provider_id.into()),
                name: "read".into(), args: json!({"path":"/tmp/example"}),
            }], response: "unselected-original".into(), display: "selected display".into(),
            usage: Default::default(), has_tool_calls: true,
        });
        proptest::prop_assert_eq!(&finished["event"]["display"], "selected display");
        proptest::prop_assert!(!finished.to_string().contains("unselected-original"));
        proptest::prop_assert!(finished["event"]["content"][0].get("provider_call_id").is_none());
        proptest::prop_assert!(finished["event"]["content"][0].get("provider_item_id").is_none());
        let decoded = crate::RemoteEvent::decode(finished, "root").unwrap();
        proptest::prop_assert!(matches!(decoded, crate::RemoteEvent::Phi(phi_sdk::PhiEventEnvelope {
            event: phi_sdk::PhiEvent::AssistantFinished { content, response, .. }, ..
        }) if response.is_empty() && matches!(&content[0], ContentBlock::ToolCall { provider_call_id: None, provider_item_id: None, .. })), "final content must retain only safe identities");
    }
}

#[test]
fn tool_and_reasoning_streams_keep_existing_projection_semantics() {
    let reasoning = projected_event(phi_sdk::PhiEvent::ReasoningDelta {
        text: "accumulated reasoning".into(),
    });
    assert_eq!(
        reasoning["event"],
        json!({"type":"reasoning_delta","text":"accumulated reasoning"})
    );
    let tool = projected_event(phi_sdk::PhiEvent::ToolCallFinished {
        id: "sdk-call".into(),
        output: "visible output".into(),
        is_error: true,
        details: None,
    });
    assert_eq!(
        tool["event"],
        json!({"type":"tool_call_finished","id":"sdk-call","output":"visible output","is_error":true,"details":null})
    );
    let progress = events::project(phi_sdk::agent_instance::AgentInstanceEvent::Progress(
        phi_sdk::agent_instance::ExecutionProgress {
            turn_id: "turn-1".into(),
            status: phi_sdk::agent_instance::ExecutionStatus::Running,
            cancellation_reason: None,
            started_at: chrono::Utc::now(),
            duration_ms: 15,
            usage: Default::default(),
            tool_uses: 1,
            current_tools: vec![phi_sdk::agent_instance::ExecutionTool {
                id: "sdk-call".into(),
                name: "read".into(),
            }],
            last_tool: None,
        },
    ));
    assert_eq!(
        progress["currentTools"],
        json!([{"id":"sdk-call","name":"read"}])
    );
    assert_eq!(progress["toolUses"], 1);
    assert!(
        matches!(crate::RemoteEvent::decode(reasoning, "root").unwrap(),
        crate::RemoteEvent::Phi(phi_sdk::PhiEventEnvelope {
            event: phi_sdk::PhiEvent::ReasoningDelta { text }, ..
        }) if text == "accumulated reasoning")
    );
    assert!(matches!(crate::RemoteEvent::decode(tool, "root").unwrap(),
        crate::RemoteEvent::Phi(phi_sdk::PhiEventEnvelope {
            event: phi_sdk::PhiEvent::ToolCallFinished { output, is_error: true, .. }, ..
        }) if output == "visible output"));
    assert!(
        matches!(crate::RemoteEvent::decode(progress, "root").unwrap(),
        crate::RemoteEvent::Progress(progress) if progress.tool_uses == 1 && progress.current_tools[0].id == "sdk-call")
    );
    let started = projected_event(phi_sdk::PhiEvent::TurnStarted {
        submission_id: Some("submission".into()),
        prompt: "not on the wire".into(),
        source: "interactive".into(),
    });
    assert!(matches!(
        crate::RemoteEvent::decode(started, "root").unwrap(),
        crate::RemoteEvent::Refresh {
            reason: crate::RemoteRefresh::History
        }
    ));
}

pub(super) fn labels() -> RemoteSession {
    RemoteSession {
        id: "test".into(),
        working_dir: "/tmp".into(),
        name: "Office session".into(),
        provider: "local".into(),
        model: "test".into(),
    }
}
