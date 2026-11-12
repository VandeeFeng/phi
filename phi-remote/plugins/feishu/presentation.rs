use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use reqwest::Method;
use serde_json::{Value, json};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use super::super::PluginClient;
use super::api::Feishu;

const CLEANUP_TIMEOUT: Duration = Duration::from_secs(20);
const UPDATE_INTERVAL: Duration = Duration::from_millis(160);
const WORKING_TEXT: &str = "working...";
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CARD_BYTES: usize = 16 * 1024;
const MAX_DISPLAYS: usize = 256;
type Displays = Arc<Mutex<BTreeMap<String, Display>>>;

#[derive(Clone, Default)]
struct Display {
    message: String,
    reaction: Option<String>,
    card: Option<String>,
    sequence: u64,
}

#[derive(Clone)]
enum Command {
    Working,
    Finish(String),
    Close,
}

struct Task {
    command: watch::Sender<Command>,
    task: JoinHandle<bool>,
}

impl Drop for Task {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(crate) struct Presenter {
    api: Feishu,
    gateway: PluginClient,
    displays: Displays,
    tasks: HashMap<String, Task>,
}

impl Presenter {
    pub fn new(api: Feishu, gateway: PluginClient) -> Self {
        Self {
            api,
            gateway,
            displays: Arc::new(Mutex::new(BTreeMap::new())),
            tasks: HashMap::new(),
        }
    }

    pub async fn start(&mut self, recipient: &str, message: &str, session: &str) {
        self.close(recipient).await;
        if self.displays.lock().await.len() >= MAX_DISPLAYS {
            return;
        }
        let worker = Self {
            api: self.api.clone(),
            gateway: self.gateway.clone(),
            displays: self.displays.clone(),
            tasks: HashMap::new(),
        };
        let (command, receiver) = watch::channel(Command::Working);
        let target = recipient.to_owned();
        let message = message.to_owned();
        let session = session.to_owned();
        let task = tokio::spawn(async move {
            match worker.present(&target, &message, &session, receiver).await {
                Ok(delivered) => delivered,
                Err(_) => {
                    eprintln!("Feishu presentation unavailable; using ordinary replies");
                    false
                }
            }
        });
        self.tasks.insert(recipient.into(), Task { command, task });
    }

    pub async fn finish(&mut self, recipient: &str, text: &str) -> bool {
        let Some(task) = self.tasks.get_mut(recipient) else {
            return false;
        };
        let _ = task.command.send(Command::Finish(text.into()));
        let delivered = (&mut task.task).await.unwrap_or(false);
        self.tasks.remove(recipient);
        delivered
    }

    pub async fn close(&mut self, recipient: &str) {
        if let Some(task) = self.tasks.get_mut(recipient) {
            let _ = task.command.send(Command::Close);
            let _ = (&mut task.task).await;
            self.tasks.remove(recipient);
        }
    }

    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        let cleanup = async {
            for task in self.tasks.values() {
                let _ = task.command.send(Command::Close);
            }
            let mut task_failed = false;
            while let Some(key) = self.tasks.keys().next().cloned() {
                if let Some(task) = self.tasks.get_mut(&key) {
                    task_failed |= (&mut task.task).await.is_err();
                }
                self.tasks.remove(&key);
            }
            let displays = self.displays.lock().await.clone();
            for (key, mut display) in displays {
                self.cleanup(&key, &mut display).await;
            }
            ensure!(
                !task_failed,
                "Feishu presentation task failed during cleanup"
            );
            ensure!(
                self.displays.lock().await.is_empty(),
                "Feishu presentation cleanup failed; reactions or cards may remain active"
            );
            Ok(())
        };
        match tokio::time::timeout(CLEANUP_TIMEOUT, cleanup).await {
            Ok(result) => result,
            Err(error) => {
                self.abort().await;
                Err(error).context(
                    "Feishu presentation cleanup timed out; reactions or cards may remain active",
                )
            }
        }
    }

    pub async fn abort(&mut self) {
        for task in self.tasks.values() {
            task.task.abort();
        }
        while let Some(key) = self.tasks.keys().next().cloned() {
            if let Some(task) = self.tasks.get_mut(&key) {
                let _ = (&mut task.task).await;
            }
            self.tasks.remove(&key);
        }
    }

    async fn record(&self, key: &str, display: &Display) -> anyhow::Result<()> {
        let mut displays = self.displays.lock().await;
        ensure!(
            displays.contains_key(key) || displays.len() < MAX_DISPLAYS,
            "Feishu presentation capacity reached"
        );
        displays.insert(key.into(), display.clone());
        Ok(())
    }

    async fn cleanup(&self, key: &str, display: &mut Display) {
        if let Some(reaction) = display.reaction.clone()
            && self
                .api
                .presentation_request(
                    Method::DELETE,
                    &format!("im/v1/messages/{}/reactions/{reaction}", display.message),
                    json!({}),
                )
                .await
                .is_ok()
        {
            display.reaction = None;
        }
        if display.card.is_some() && self.close_card(key, display).await.is_ok() {
            display.card = None;
        }
        let mut displays = self.displays.lock().await;
        if display.reaction.is_none() && display.card.is_none() {
            displays.remove(key);
        } else {
            displays.insert(key.into(), display.clone());
        }
    }

    async fn close_card(&self, key: &str, display: &mut Display) -> anyhow::Result<()> {
        display.sequence = display
            .sequence
            .checked_add(1)
            .context("card sequence exhausted")?;
        self.record(key, display).await?;
        self.api.presentation_request(Method::PATCH,
            &format!("cardkit/v1/cards/{}/settings", display.card.as_deref().context("missing card")?),
            json!({"settings":json!({"config":{"streaming_mode":false,"summary":{"content":"Phi reply"}}}).to_string(),"sequence":display.sequence})).await?;
        Ok(())
    }

    async fn update(
        &self,
        recipient: &str,
        key: &str,
        display: &mut Display,
        content: &str,
    ) -> anyhow::Result<()> {
        ensure!(!content.trim().is_empty(), "empty card content");
        if display.card.is_none() {
            let value = self.api.presentation_request(
                Method::POST,
                "cardkit/v1/cards",
                json!({"type":"card_json","data":json!({
                    "schema":"2.0","config":{"streaming_mode":true,"summary":{"content":"Phi reply"}},
                    "body":{"elements":[{"tag":"markdown","element_id":"content","content":content}]}
                }).to_string()}),
            ).await?;
            display.card = Some(resource_id(&value, "card_id").context("missing card ID")?);
            self.record(key, display).await?;
            self.api
                .send_card(recipient, display.card.as_deref().context("missing card")?)
                .await?;
        }
        display.sequence = display
            .sequence
            .checked_add(1)
            .context("card sequence exhausted")?;
        self.record(key, display).await?;
        self.api
            .presentation_request(
                Method::PUT,
                &format!(
                    "cardkit/v1/cards/{}/elements/content/content",
                    display.card.as_deref().context("missing card")?
                ),
                json!({"content":content,"sequence":display.sequence}),
            )
            .await?;
        Ok(())
    }

    async fn present(
        &self,
        recipient: &str,
        key: &str,
        session: &str,
        mut command: watch::Receiver<Command>,
    ) -> anyhow::Result<bool> {
        let mut display = Display {
            message: key.into(),
            sequence: 1,
            ..Display::default()
        };
        self.record(key, &display).await?;
        if let Ok(value) = self
            .api
            .presentation_request(
                Method::POST,
                &format!("im/v1/messages/{key}/reactions"),
                json!({"reaction_type":{"emoji_type":"Typing"}}),
            )
            .await
        {
            display.reaction = resource_id(&value, "reaction_id");
            self.record(key, &display).await?;
        }
        let mut usable = true;
        let mut tick = tokio::time::interval(UPDATE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The bounded watch channel retains only the latest disposable overlay.
        let (overlay, mut overlays) = watch::channel(Preview::default());
        let gateway = self.gateway.clone();
        let session = session.to_owned();
        let mut observations = tokio::task::JoinSet::new();
        observations.spawn(async move {
            loop {
                if let Ok(mut stream) = gateway.watch(&session).await {
                    let mut preview = Preview::default();
                    if overlay.send(preview.clone()).is_err() {
                        return;
                    }
                    while let Ok(event) = stream.next().await {
                        preview.apply(&event);
                        if overlay.send(preview.clone()).is_err() {
                            return;
                        }
                    }
                }
                tokio::time::sleep(RECONNECT_INTERVAL).await;
            }
        });
        let mut sent = String::new();
        let delivered = loop {
            let current = command.borrow_and_update().clone();
            match current {
                Command::Finish(text) => {
                    let delivered = usable
                        && text.len() <= MAX_CARD_BYTES
                        && !text.trim().is_empty()
                        && self
                            .update(recipient, key, &mut display, &text)
                            .await
                            .is_ok();
                    break delivered;
                }
                Command::Close => break false,
                Command::Working => {}
            }
            tokio::select! {
                changed = command.changed() => if changed.is_err() { break false; },
                _ = tick.tick(), if usable => {
                    let mut content = overlays.borrow_and_update().text.clone();
                    if content.trim().is_empty() && sent.is_empty() {
                        content = WORKING_TEXT.into();
                    }
                    if !content.trim().is_empty() && content != sent {
                        if self.update(recipient, key, &mut display, &content).await.is_ok() { sent = content; }
                        else { usable = false; observations.abort_all(); }
                    }
                }
            }
        };
        observations.abort_all();
        while observations.join_next().await.is_some() {}
        self.cleanup(key, &mut display).await;
        Ok(delivered)
    }
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn resource_id(value: &Value, field: &str) -> Option<String> {
    value["data"][field]
        .as_str()
        .filter(|id| safe_id(id))
        .map(str::to_owned)
}

#[derive(Clone, Default)]
struct Preview {
    text: String,
}

impl Preview {
    fn apply(&mut self, value: &Value) {
        if value["kind"].as_str() == Some("phi") {
            match value["event"]["type"].as_str() {
                Some("assistant_delta") => {
                    if let Some(delta) = value["event"]["display"].as_str()
                        && self.text.len() + delta.len() <= MAX_CARD_BYTES
                    {
                        self.text.push_str(delta);
                    }
                }
                Some("assistant_started")
                    if !self.text.is_empty() && self.text.len() + 2 <= MAX_CARD_BYTES =>
                {
                    self.text.push_str("\n\n")
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[tokio::test]
    async fn canceled_close_keeps_ownership() {
        let (mut presenter, _, serving) = fixture().await;
        let (command, _receiver) = watch::channel(Command::Working);
        let task = tokio::spawn(std::future::pending::<bool>());
        let worker = task.abort_handle();
        presenter
            .tasks
            .insert("chat".into(), Task { command, task });
        {
            let closing = presenter.close("chat");
            tokio::pin!(closing);
            assert!(futures_util::poll!(&mut closing).is_pending());
        }
        assert!(presenter.tasks.contains_key("chat"));
        presenter.abort().await;
        assert!(worker.is_finished());
        assert!(presenter.tasks.is_empty());
        serving.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_aborts_and_joins_work_after_cleanup_deadline() {
        let (mut presenter, _, serving) = fixture().await;
        let (command, _receiver) = watch::channel(Command::Working);
        let task = tokio::spawn(std::future::pending::<bool>());
        let worker = task.abort_handle();
        presenter
            .tasks
            .insert("chat".into(), Task { command, task });
        let error = tokio::time::timeout(
            CLEANUP_TIMEOUT + Duration::from_secs(1),
            presenter.shutdown(),
        )
        .await
        .expect("presentation shutdown must enforce its cleanup deadline")
        .unwrap_err();
        assert!(error.to_string().contains("cleanup timed out"));
        assert!(worker.is_finished());
        assert!(presenter.tasks.is_empty());
        serving.abort();
    }

    #[tokio::test]
    async fn shutdown_rejects_unresolved_reactions_and_cards() {
        for path in [
            "/open-apis/im/v1/messages/om_input/reactions/reaction_1",
            "/open-apis/cardkit/v1/cards/card_1/settings",
        ] {
            let (mut presenter, harness, serving) = fixture().await;
            presenter.start("oc_chat", "om_input", "session-1").await;
            wait_for(&harness, "/open-apis/im/v1/messages").await;
            harness.lock().await.failure = Some(path.into());
            let error = presenter.shutdown().await.unwrap_err();
            assert!(error.to_string().contains("cleanup failed"));
            assert!(presenter.tasks.is_empty());
            assert!(!presenter.displays.lock().await.is_empty());
            harness.lock().await.failure = None;
            presenter.shutdown().await.unwrap();
            assert!(presenter.displays.lock().await.is_empty());
            assert_eq!(
                harness
                    .lock()
                    .await
                    .requests
                    .iter()
                    .filter(|(_, path, _)| path == "/open-apis/im/v1/messages")
                    .count(),
                1
            );
            serving.abort();
        }
    }

    #[tokio::test]
    async fn shutdown_reports_presentation_task_panics() {
        let (mut presenter, _, serving) = fixture().await;
        let (command, _receiver) = watch::channel(Command::Working);
        let task = tokio::spawn(async { panic!("presentation task panicked") });
        presenter
            .tasks
            .insert("chat".into(), Task { command, task });
        let error = presenter.shutdown().await.unwrap_err();
        assert!(error.to_string().contains("task failed"));
        assert!(presenter.tasks.is_empty());
        serving.abort();
    }

    #[derive(Default)]
    struct Harness {
        requests: Vec<(Method, String, Value)>,
        failure: Option<String>,
        failure_code: Option<i64>,
    }

    async fn fixture() -> (Presenter, Arc<Mutex<Harness>>, tokio::task::JoinHandle<()>) {
        use axum::{
            Json,
            extract::{OriginalUri, State},
            http::StatusCode,
        };
        let harness = Arc::new(Mutex::new(Harness::default()));
        let app = axum::Router::new().fallback(
            |State(harness): State<Arc<Mutex<Harness>>>, method: Method, OriginalUri(uri): OriginalUri, body: axum::body::Bytes| async move {
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let mut harness = harness.lock().await;
                let path = uri.path();
                harness.requests.push((method.clone(), path.into(), body.clone()));
                if harness.failure.as_deref() == Some(path) {
                    return (StatusCode::OK, Json(json!({"code":harness.failure_code.unwrap_or(123),"msg":"private rejection details"})));
                }
                let data = match (method, path) {
                    (Method::POST, "/open-apis/auth/v3/tenant_access_token/internal") => return (StatusCode::OK, Json(json!({"code":0,"tenant_access_token":"test-token","expire":7200}))),
                    (Method::POST, "/open-apis/im/v1/messages/om_input/reactions") => {
                        assert_eq!(body["reaction_type"]["emoji_type"], "Typing");
                        json!({"reaction_id":"reaction_1"})
                    },
                    (Method::POST, "/open-apis/cardkit/v1/cards") => {
                        let card: Value = serde_json::from_str(body["data"].as_str().unwrap()).unwrap();
                        assert_eq!(card["config"]["streaming_mode"], true);
                        assert!(!card["body"]["elements"][0]["content"].as_str().unwrap().trim().is_empty());
                        json!({"card_id":"card_1"})
                    },
                    (Method::POST, "/open-apis/im/v1/messages") => {
                        assert_eq!(body["msg_type"], "interactive");
                        let recipient = body["receive_id"].as_str().unwrap();
                        assert_eq!(uri.query(), Some(if recipient.starts_with("ou_") { "receive_id_type=open_id" } else { "receive_id_type=chat_id" }));
                        assert!(body.get("reply_to_message_id").is_none());
                        assert!(body.get("root_id").is_none());
                        json!({"message_id":"om_card"})
                    },
                    (Method::PUT, "/open-apis/cardkit/v1/cards/card_1/elements/content/content") => json!({}),
                    (Method::PATCH, "/open-apis/cardkit/v1/cards/card_1/settings") => {
                        let settings: Value = serde_json::from_str(body["settings"].as_str().unwrap()).unwrap();
                        assert_eq!(settings["config"]["streaming_mode"], false);
                        json!({})
                    },
                    (Method::DELETE, "/open-apis/im/v1/messages/om_input/reactions/reaction_1") => json!({}),
                    _ => return (StatusCode::NOT_FOUND, Json(json!({"code":404}))),
                };
                (StatusCode::OK, Json(json!({"code":0,"data":data})))
            }).with_state(harness.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let presenter = Presenter::new(
            Feishu::test_endpoint(&endpoint),
            PluginClient::test_client(&format!("{endpoint}/")),
        );
        (presenter, harness, server)
    }

    async fn wait_for(harness: &Mutex<Harness>, path: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if harness
                    .lock()
                    .await
                    .requests
                    .iter()
                    .any(|(_, request, _)| request == path)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn card_finishes_once() {
        let (mut presenter, harness, server) = fixture().await;
        presenter.start("ou_user", "om_input", "session-1").await;
        wait_for(&harness, "/open-apis/im/v1/messages/om_input/reactions").await;
        wait_for(
            &harness,
            "/open-apis/cardkit/v1/cards/card_1/elements/content/content",
        )
        .await;
        {
            let harness = harness.lock().await;
            let creation = harness
                .requests
                .iter()
                .find(|(_, path, _)| path == "/open-apis/cardkit/v1/cards")
                .expect("working card");
            let card: Value = serde_json::from_str(creation.2["data"].as_str().unwrap()).unwrap();
            assert_eq!(card["body"]["elements"][0]["content"], "working...");
        }
        assert!(presenter.finish("ou_user", "final answer").await);
        assert!(!presenter.finish("ou_user", "must not be resent").await);
        let harness = harness.lock().await;
        let sequences: Vec<_> = harness
            .requests
            .iter()
            .filter_map(|(_, _, value)| value["sequence"].as_u64())
            .collect();
        assert!(sequences.windows(2).all(|values| values[0] < values[1]));
        assert!(
            harness
                .requests
                .iter()
                .any(|(method, _, _)| *method == Method::DELETE)
        );
        assert_eq!(
            harness
                .requests
                .iter()
                .filter(|(_, path, _)| path == "/open-apis/cardkit/v1/cards")
                .count(),
            1
        );
        assert_eq!(
            harness
                .requests
                .iter()
                .filter(|(_, path, _)| path == "/open-apis/im/v1/messages")
                .count(),
            1
        );
        assert_eq!(
            harness
                .requests
                .iter()
                .rfind(|(method, _, _)| *method == Method::PUT)
                .unwrap()
                .2["content"],
            "final answer"
        );
        assert!(presenter.displays.lock().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn empty_final_never_clears_the_working_card() {
        let (mut presenter, harness, server) = fixture().await;
        presenter.start("ou_user", "om_input", "session-1").await;
        wait_for(&harness, "/open-apis/im/v1/messages").await;
        assert!(!presenter.finish("ou_user", " \n").await);
        assert!(
            harness
                .lock()
                .await
                .requests
                .iter()
                .filter(|(method, _, _)| *method == Method::PUT)
                .all(|(_, _, body)| body["content"] == WORKING_TEXT)
        );
        assert!(presenter.displays.lock().await.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn creation_send_and_final_update_failures_fall_back_without_retrying() {
        for (failure, code) in [
            ("/open-apis/cardkit/v1/cards", 123),
            ("/open-apis/im/v1/messages", 230011),
            (
                "/open-apis/cardkit/v1/cards/card_1/elements/content/content",
                231003,
            ),
        ] {
            let (mut presenter, harness, server) = fixture().await;
            {
                let mut harness = harness.lock().await;
                harness.failure = Some(failure.into());
                harness.failure_code = Some(code);
            }
            presenter.start("oc_chat", "om_input", "session-1").await;
            wait_for(&harness, "/open-apis/im/v1/messages/om_input/reactions").await;
            assert!(
                !presenter
                    .finish("oc_chat", "final reply for ordinary delivery")
                    .await
            );
            let harness = harness.lock().await;
            assert_eq!(
                harness
                    .requests
                    .iter()
                    .filter(|(_, path, _)| path == failure)
                    .count(),
                1
            );
            assert!(
                harness
                    .requests
                    .iter()
                    .any(|(method, _, _)| *method == Method::DELETE)
            );
            assert!(presenter.displays.lock().await.is_empty());
            server.abort();
        }
    }

    #[tokio::test]
    async fn switching_closes_display_without_sending_a_final_answer() {
        let (mut presenter, harness, server) = fixture().await;
        presenter.start("oc_chat", "om_input", "session-1").await;
        wait_for(&harness, "/open-apis/im/v1/messages/om_input/reactions").await;
        presenter.close("oc_chat").await;
        assert!(!presenter.finish("oc_chat", "outgoing Session answer").await);
        assert!(presenter.displays.lock().await.is_empty());
        assert!(
            !harness
                .lock()
                .await
                .requests
                .iter()
                .any(|(_, _, value)| value["content"] == "outgoing Session answer")
        );
        server.abort();
    }

    #[tokio::test]
    async fn shutdown_retries_in_memory_cleanup_without_republishing() {
        let (mut presenter, harness, server) = fixture().await;
        harness.lock().await.failure =
            Some("/open-apis/im/v1/messages/om_input/reactions/reaction_1".into());
        presenter.start("oc_chat", "om_input", "session-1").await;
        wait_for(&harness, "/open-apis/im/v1/messages").await;
        presenter.close("oc_chat").await;
        assert!(!presenter.displays.lock().await.is_empty());
        harness.lock().await.failure = None;
        presenter.shutdown().await.unwrap();
        assert!(presenter.displays.lock().await.is_empty());
        let harness = harness.lock().await;
        assert_eq!(
            harness
                .requests
                .iter()
                .filter(|(method, _, _)| *method == Method::DELETE)
                .count(),
            2
        );
        assert_eq!(
            harness
                .requests
                .iter()
                .filter(|(_, path, _)| path == "/open-apis/im/v1/messages")
                .count(),
            1
        );
        server.abort();
    }

    proptest! {
        #[test]
        fn preview_allows_only_answer_text(secret in ".{0,1000}", answer in ".{0,1000}") {
            let mut preview = Preview::default();
            for event in [
                json!({"type":"reasoning_delta","text":secret}),
                json!({"type":"reasoning_finished","text":secret}),
                json!({"type":"tool_call_started","name":secret,"args":{"secret":secret}}),
                json!({"type":"tool_call_finished","output":secret}),
                json!({"type":"assistant_finished","content":secret,"display":secret}),
                json!({"type":"system","message":secret}),
            ] {
                preview.apply(&json!({"kind":"phi","event":event}));
                prop_assert!(preview.text.is_empty());
            }
            preview.apply(&json!({"kind":"phi","event":{"type":"assistant_delta","display":answer}}));
            prop_assert_eq!(&preview.text, &answer);
            preview.apply(&json!({"kind":"progress","status":"requires_action","currentTools":[{"name":secret}]}));
            prop_assert_eq!(&preview.text, &answer);
            prop_assert!(preview.text.len() <= MAX_CARD_BYTES);
        }

        #[test]
        fn preview_never_exceeds_card_limit(deltas in prop::collection::vec(".{0,2000}", 0..40)) {
            let mut preview = Preview::default();
            for delta in deltas {
                preview.apply(&json!({"kind":"phi","event":{"type":"assistant_delta","display":delta}}));
                prop_assert!(preview.text.len() <= MAX_CARD_BYTES);
            }
        }
    }
}
