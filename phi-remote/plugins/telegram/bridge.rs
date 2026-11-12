use std::collections::{HashMap, HashSet};

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::PluginClient;
use super::super::activity::{Refresh, WorkState};
use super::super::client::session_path;
use super::api::{ApiFailure, Callback, MESSAGE_UNITS, Telegram, Update, split_text};
use super::feedback::{Feedback, Reaction};
use super::polling::Poller;
use super::store::{Binding, Config, Delivery, Question, State, Store};

const HELP: &str = "Send text to Phi.\n/new — create a fresh Session\n/stop — cancel current execution\n/status — show Session status\n/help — show commands";
const MAX_QUESTIONS: usize = 32;

#[derive(Debug)]
struct PersistenceFailure;

impl std::fmt::Display for PersistenceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("bridge persistence failed; stopping before further effects")
    }
}
impl std::error::Error for PersistenceFailure {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    version: u32,
    session: crate::RemoteSession,
    history: History,
    execution: Execution,
    pending_submission_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct History {
    entries: Vec<Entry>,
    leaf_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    id: String,
    parent_id: Option<String>,
    content: Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Execution {
    status: String,
    active_turn_id: Option<String>,
}

#[derive(Deserialize)]
struct OfficeView {
    permissions: Vec<Permission>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Permission {
    agent_id: String,
    request_id: String,
    tool: String,
    input: String,
    preview: Option<String>,
    can_allow: bool,
}

pub(crate) struct Bridge {
    pub feedback: HashMap<i64, Feedback>,
    pub working_dir: std::path::PathBuf,
    pub config: Config,
    pub store: std::sync::Arc<Store>,
    pub state: State,
    pub gateway: PluginClient,
    pub telegram: Telegram,
}

impl Bridge {
    pub async fn run(&mut self, scheduler: &crate::GatewayScheduler) -> anyhow::Result<()> {
        let bot = self.telegram.identity().await?;
        ensure!(bot.is_bot, "Telegram token does not identify a bot");
        ensure!(
            self.state.bot_id.is_none_or(|id| id == bot.id),
            "state belongs to another Telegram bot; use a separate state directory"
        );
        ensure!(self.state.offset >= 0, "invalid saved Telegram offset");
        self.state.bot_id = Some(bot.id);
        self.recover_uncertain();
        self.save()?;
        eprintln!("Telegram bridge ready; private allowlisted chats only");
        let mut poller = Poller::new(self.telegram.clone());
        let mut refresh = Refresh::new(scheduler);
        loop {
            self.deliver().await?;
            refresh.update(self.work_state());
            let updates = poller.next(self.state.offset);
            tokio::pin!(updates);
            let updates = loop {
                tokio::select! {
                    result = &mut updates => break result?,
                    _ = refresh.wait() => {
                        self.refresh().await?;
                        self.deliver().await?;
                        refresh.update(self.work_state());
                    }
                }
            };
            for update in updates {
                self.process(update).await?;
            }
        }
    }

    fn work_state(&self) -> WorkState {
        if self
            .state
            .chats
            .iter()
            .any(|(chat, binding)| binding.waiting && self.config.allowed_users.contains(chat))
        {
            WorkState::Pending
        } else {
            WorkState::Idle
        }
    }

    fn recover_uncertain(&mut self) {
        let chats: HashSet<_> = [self.state.inflight.take(), self.state.sending.take()]
            .into_iter()
            .flatten()
            .collect();
        for chat in chats {
            self.enqueue(chat, "The bridge stopped during an operation or delivery. Its outcome may be unknown; it was not retried. Check /status or the Gateway UI before resending.");
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        self.store
            .save("state.json", &self.state)
            .map_err(|error| error.context(PersistenceFailure))
    }

    fn enqueue(&mut self, chat: i64, text: &str) {
        for text in split_text(text) {
            self.state.outbox.push(Delivery {
                chat,
                text,
                question: None,
            });
        }
    }

    async fn process(&mut self, update: Update) -> anyhow::Result<()> {
        ensure!(update.update_id >= 0, "invalid Telegram update ID");
        if update.update_id < self.state.offset {
            return Ok(());
        }
        let chat = update.authorized_chat(&self.config.allowed_users);
        self.state.offset = update
            .update_id
            .checked_add(1)
            .context("Telegram update ID overflow")?;
        self.state.inflight = chat;
        // Never acknowledge or replay a mutation without a durable local claim.
        self.save()?;
        if let Some(chat) = chat {
            let outcome = if let Some(callback) = update.callback_query {
                self.callback(chat, callback).await
            } else if let Some(message) = update.message {
                match message.text {
                    Some(text) => self.text(chat, message.message_id, &text).await,
                    None => {
                        self.enqueue(chat, "Only text messages are supported.");
                        Ok(())
                    }
                }
            } else {
                Ok(())
            };
            if let Err(error) = outcome {
                if error.downcast_ref::<PersistenceFailure>().is_some() {
                    return Err(error);
                }
                eprintln!("Telegram chat operation failed: {error:#}");
                self.enqueue(chat, "Operation failed or its outcome is unknown. It was not retried. Check /status or the Gateway UI before resending.");
            }
        }
        self.state.inflight = None;
        self.save()
    }

    async fn text(&mut self, chat: i64, message: i64, text: &str) -> anyhow::Result<()> {
        match text.trim() {
            "/start" | "/help" => self.enqueue(chat, HELP),
            "/status" => {
                if let Some(binding) = self.state.chats.get(&chat) {
                    let snapshot = self.snapshot(&binding.session).await?;
                    self.enqueue(
                        chat,
                        &format!(
                            "Session: {}\nStatus: {}",
                            binding.session, snapshot.execution.status
                        ),
                    );
                } else {
                    self.enqueue(chat, "No Session yet. Send text or use /new.");
                }
            }
            "/stop" => {
                if let Some(binding) = self.state.chats.get(&chat) {
                    let _: Value = self
                        .gateway
                        .post(&session_path(&binding.session, "cancel")?, json!({}))
                        .await?;
                    self.enqueue(chat, "Cancellation requested.");
                } else {
                    self.enqueue(chat, "No Session to cancel.");
                }
            }
            "/new" => {
                if self
                    .state
                    .chats
                    .get(&chat)
                    .is_some_and(|binding| binding.waiting)
                {
                    self.enqueue(chat, "Wait for completion or use /stop before /new.");
                } else {
                    self.create(chat).await?;
                    self.enqueue(chat, "New Session created. Send text to begin.");
                }
            }
            text if text.starts_with('/') => self.enqueue(chat, HELP),
            "" => {}
            text => {
                if let Err(error) = self.submit(chat, message, text).await {
                    if error.downcast_ref::<PersistenceFailure>().is_none() {
                        self.finish_feedback(chat, Reaction::Error).await;
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn create(&mut self, chat: i64) -> anyhow::Result<()> {
        let session: crate::RemoteSession = self
            .gateway
            .post("api/sessions", json!({"workingDir":self.working_dir}))
            .await?;
        session_path(&session.id, "snapshot")?;
        self.state.chats.insert(
            chat,
            Binding {
                session: session.id,
                ..Default::default()
            },
        );
        self.save()
    }

    async fn finish_feedback(&mut self, chat: i64, reaction: Reaction) {
        if let Some(mut feedback) = self.feedback.remove(&chat) {
            feedback.stop_typing();
            feedback.update(&self.telegram, chat, reaction).await;
        }
    }

    async fn submit(&mut self, chat: i64, message: i64, text: &str) -> anyhow::Result<()> {
        if self
            .state
            .chats
            .get(&chat)
            .is_some_and(|binding| binding.waiting)
        {
            self.enqueue(
                chat,
                "Phi is still working. Wait for the reply or use /stop.",
            );
            return Ok(());
        }
        self.feedback
            .insert(chat, Feedback::start(&self.telegram, chat, message).await);
        if !self.state.chats.contains_key(&chat) {
            self.create(chat).await?;
        }
        let session = self.state.chats[&chat].session.clone();
        let snapshot = self.snapshot(&session).await?;
        let binding = self
            .state
            .chats
            .get_mut(&chat)
            .context("missing chat binding")?;
        binding.cursor = snapshot.history.leaf_id;
        binding.waiting = true;
        self.save()?;
        let receipt: Value = self
            .gateway
            .post(&session_path(&session, "message")?, json!({"message":text}))
            .await?;
        match receipt["status"].as_str() {
            Some("started") => {
                if let Some(feedback) = self.feedback.get_mut(&chat) {
                    feedback
                        .update(&self.telegram, chat, Reaction::Working)
                        .await;
                }
            }
            Some("queued") => {}
            Some("handled") => {
                self.finish_feedback(chat, Reaction::Clear).await;
                self.state
                    .chats
                    .get_mut(&chat)
                    .context("missing chat binding")?
                    .waiting = false;
                self.enqueue(
                    chat,
                    receipt["reason"]
                        .as_str()
                        .unwrap_or("Input handled without starting a turn."),
                );
            }
            _ => bail!("invalid Gateway admission receipt"),
        }
        if let Some(notices) = receipt["notices"].as_array() {
            for notice in notices.iter().filter_map(Value::as_str) {
                self.enqueue(chat, notice);
            }
        }
        self.save()
    }

    async fn snapshot(&self, session: &str) -> anyhow::Result<Snapshot> {
        let snapshot: Snapshot = self
            .gateway
            .get(&session_path(session, "snapshot")?)
            .await?;
        ensure!(
            snapshot.version == 2 && snapshot.session.id == session,
            "Gateway snapshot identity or version mismatch"
        );
        Ok(snapshot)
    }

    async fn refresh(&mut self) -> anyhow::Result<()> {
        let chats: Vec<_> = self
            .state
            .chats
            .iter()
            .filter(|(chat, binding)| binding.waiting && self.config.allowed_users.contains(chat))
            .map(|(chat, binding)| (*chat, binding.session.clone()))
            .collect();
        for (chat, session) in chats {
            if let Err(error) = self.refresh_chat(chat, &session).await {
                if error.downcast_ref::<PersistenceFailure>().is_some() {
                    return Err(error);
                }
                eprintln!("Gateway observation unavailable: {error:#}");
            }
        }
        self.save()
    }

    async fn refresh_chat(&mut self, chat: i64, session: &str) -> anyhow::Result<()> {
        let snapshot = self.snapshot(session).await?;
        if let Some(feedback) = self.feedback.get_mut(&chat) {
            let reaction = match snapshot.execution.status.as_str() {
                "requires_action" => Some(Reaction::Waiting),
                "running" => Some(Reaction::Working),
                _ => None,
            };
            if let Some(reaction) = reaction {
                feedback.update(&self.telegram, chat, reaction).await;
            }
        }
        let view: OfficeView = self.gateway.get(&session_path(session, "view")?).await?;
        let current: HashSet<_> = view
            .permissions
            .iter()
            .map(|question| (&question.agent_id, &question.request_id))
            .collect();
        let binding = self
            .state
            .chats
            .get_mut(&chat)
            .context("missing chat binding")?;
        binding
            .questions
            .retain(|_, question| current.contains(&(&question.agent, &question.request)));
        for permission in &view.permissions {
            if binding.questions.len() >= MAX_QUESTIONS {
                break;
            }
            if binding.questions.values().any(|question| {
                question.agent == permission.agent_id && question.request == permission.request_id
            }) {
                continue;
            }
            let token = crate::pairing::random_credential()?;
            let token = token[..32].to_owned();
            let text = format!(
                "Permission: {}\n\n{}{}",
                permission.tool,
                permission.input,
                permission
                    .preview
                    .as_ref()
                    .map(|preview| format!("\n\n{preview}"))
                    .unwrap_or_default()
            );
            let can_allow = permission.can_allow && text.encode_utf16().count() <= MESSAGE_UNITS;
            let text = if can_allow {
                text
            } else {
                format!(
                    "Permission details cannot be safely approved in Telegram. Review in Gateway Web/TUI. Only denial is available here.\nTool: {}",
                    permission.tool.chars().take(100).collect::<String>()
                )
            };
            binding.questions.insert(
                token.clone(),
                Question {
                    agent: permission.agent_id.clone(),
                    request: permission.request_id.clone(),
                    message_id: None,
                    can_allow,
                },
            );
            self.state.outbox.push(Delivery {
                chat,
                text,
                question: Some(token),
            });
        }
        if snapshot.execution.active_turn_id.is_some()
            || !snapshot.pending_submission_ids.is_empty()
            || !matches!(
                snapshot.execution.status.as_str(),
                "idle" | "failed" | "stopped"
            )
        {
            return Ok(());
        }
        let reaction = match snapshot.execution.status.as_str() {
            "failed" => Reaction::Error,
            "stopped" => Reaction::Stopped,
            _ => Reaction::Done,
        };
        self.finish_feedback(chat, reaction).await;
        let cursor = self.state.chats[&chat].cursor.clone();
        let replies = replies_since(&snapshot.history, cursor.as_deref());
        let binding = self
            .state
            .chats
            .get_mut(&chat)
            .context("missing chat binding")?;
        binding.cursor = snapshot.history.leaf_id;
        binding.waiting = false;
        binding.questions.clear();
        match replies {
            Ok(replies) if replies.is_empty() => self.enqueue(chat, "Execution settled without a new text reply. Check the Gateway UI if the previous operation had an unknown outcome."),
            Ok(replies) => { for reply in replies { self.enqueue(chat, &reply); } }
            Err(_) => self.enqueue(chat, "History changed or the saved cursor is unavailable. Read the reply in Gateway Web/TUI; older messages were not replayed."),
        }
        Ok(())
    }

    async fn callback(&mut self, chat: i64, callback: Callback) -> anyhow::Result<()> {
        let Some((decision, token)) = callback.data.as_deref().and_then(parse_callback) else {
            self.telegram
                .answer(&callback.id, "Invalid action.")
                .await?;
            return Ok(());
        };
        let Some(binding) = self.state.chats.get(&chat) else {
            return Ok(());
        };
        let session = binding.session.clone();
        let question = binding.questions.get(token).cloned();
        let Some(question) = question.filter(|question| {
            question.message_id == callback.message.as_ref().map(|message| message.message_id)
                && (decision == "deny" || question.can_allow)
        }) else {
            self.telegram
                .answer(
                    &callback.id,
                    "This permission action is stale or unavailable.",
                )
                .await?;
            return Ok(());
        };
        let view: OfficeView = self.gateway.get(&session_path(&session, "view")?).await?;
        ensure!(
            view.permissions
                .iter()
                .any(|current| current.agent_id == question.agent
                    && current.request_id == question.request
                    && (decision == "deny" || current.can_allow)),
            "permission is no longer answerable"
        );
        self.state
            .chats
            .get_mut(&chat)
            .context("missing chat binding")?
            .questions
            .remove(token);
        self.save()?;
        let _: Value = self
            .gateway
            .post(
                &session_path(&session, "permission")?,
                json!({"agentId":question.agent,"requestId":question.request,"decision":decision}),
            )
            .await?;
        self.telegram
            .answer(&callback.id, "Permission answered.")
            .await?;
        if let Some(message) = question.message_id
            && self.telegram.clear_buttons(chat, message).await.is_err()
        {
            eprintln!("Could not remove stale permission buttons");
        }
        Ok(())
    }

    async fn deliver(&mut self) -> anyhow::Result<()> {
        while !self.state.outbox.is_empty() {
            let delivery = self.state.outbox.remove(0);
            if !self.config.allowed_users.contains(&delivery.chat) {
                self.save()?;
                continue;
            }
            let question = delivery
                .question
                .as_ref()
                .and_then(|token| self.state.chats.get(&delivery.chat)?.questions.get(token));
            if delivery.question.is_some() && question.is_none() {
                self.save()?;
                continue;
            }
            let buttons = question.map(|question| {
                let token = delivery.question.as_ref().expect("question delivery");
                let mut row = Vec::new();
                if question.can_allow {
                    row.push(
                        json!({"text":"Allow once","callback_data":format!("allow_once:{token}")}),
                    );
                }
                row.push(json!({"text":"Deny","callback_data":format!("deny:{token}")}));
                json!({"inline_keyboard":[row]})
            });
            self.state.sending = Some(delivery.chat);
            self.save()?;
            let message = match self
                .telegram
                .send(delivery.chat, &delivery.text, buttons)
                .await
            {
                Ok(message) => message,
                Err(error)
                    if error.downcast_ref::<ApiFailure>().is_some_and(|failure| {
                        failure.code == reqwest::StatusCode::BAD_REQUEST.as_u16()
                    }) =>
                {
                    eprintln!(
                        "Telegram delivery rejected; not retried. Check Gateway Web/TUI: {error:#}"
                    );
                    self.state.sending = None;
                    self.save()?;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if let Some(question) = delivery.question.and_then(|token| {
                self.state
                    .chats
                    .get_mut(&delivery.chat)?
                    .questions
                    .get_mut(&token)
            }) {
                question.message_id = Some(message);
            }
            self.state.sending = None;
            self.save()?;
        }
        Ok(())
    }
}

fn parse_callback(value: &str) -> Option<(&str, &str)> {
    let (decision, token) = value.split_once(':')?;
    (matches!(decision, "allow_once" | "deny")
        && token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()))
    .then_some((decision, token))
}

fn replies_since(history: &History, cursor: Option<&str>) -> anyhow::Result<Vec<String>> {
    let entries: HashMap<_, _> = history
        .entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    ensure!(
        entries.len() == history.entries.len(),
        "duplicate history entry IDs"
    );
    let mut next = history.leaf_id.as_deref();
    let mut replies = Vec::new();
    let mut visited = HashSet::new();
    while next != cursor {
        let id = next.context("history cursor is no longer on the current branch")?;
        ensure!(visited.insert(id), "cyclic history");
        let entry = entries.get(id).context("incomplete history")?;
        match entry.content["kind"].as_str() {
            Some("failure") => {
                if let Some(message) = entry.content["failure"]["message"].as_str() {
                    replies.push(format!("Execution failed: {message}"));
                }
            }
            Some("message") if entry.content["message"]["role"] == "assistant" => {
                if let Some(content) = entry.content["message"]["content"].as_array() {
                    let text = content
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .filter_map(|block| block["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.is_empty() {
                        replies.push(text);
                    }
                }
            }
            _ => {}
        }
        next = entry.parent_id.as_deref();
    }
    replies.reverse();
    Ok(replies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn replies_follow_only_the_current_branch_and_exclude_internal_content() {
        let history: History = serde_json::from_value(json!({"leafId":"reply","entries":[
            {"id":"user","parentId":null,"content":{"kind":"message","message":{"role":"user","content":[]}}},
            {"id":"other","parentId":"user","content":{"kind":"message","message":{"role":"assistant","content":[{"type":"text","text":"wrong branch"}]}}},
            {"id":"reply","parentId":"user","content":{"kind":"message","message":{"role":"assistant","content":[{"type":"thinking","text":"private"},{"type":"text","text":"answer"},{"type":"tool_call","args":{"secret":"hidden"}}]}}}
        ]})).unwrap();
        assert_eq!(replies_since(&history, Some("user")).unwrap(), ["answer"]);
        assert!(replies_since(&history, Some("other")).is_err());
        assert!(replies_since(&history, Some("missing")).is_err());
    }

    #[derive(Default)]
    struct Harness {
        working_dirs: Vec<std::path::PathBuf>,
        submissions: usize,
        snapshot_reads: usize,
        polls: usize,
        permissions: usize,
        sent: Vec<Value>,
        feedback: Vec<(String, Value)>,
        feedback_fails: bool,
        send_fails: bool,
        send_attempts: usize,
        blocked: bool,
        delay: bool,
    }

    async fn server() -> (
        String,
        std::sync::Arc<tokio::sync::Mutex<Harness>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{
            Json,
            extract::{OriginalUri, State},
            http::HeaderMap,
        };
        let state = std::sync::Arc::new(tokio::sync::Mutex::new(Harness::default()));
        let app = axum::Router::new().fallback(|State(state): State<std::sync::Arc<tokio::sync::Mutex<Harness>>>, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: axum::body::Bytes| async move {
            let mut harness = state.lock().await;
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let path = uri.path();
            if path.starts_with("/api/") {
                assert_eq!(headers.get("authorization").unwrap(), format!("Bearer {}", "a".repeat(64)).as_str());
                if !body.is_null() { assert!(headers.contains_key("origin")); }
            }
            let session = json!({"id":"session-1","name":"test","provider":"test","model":"test","workingDir":"/tmp"});
            let response = match path {
                "/api/sessions" => {
                    harness.working_dirs.push(body["workingDir"].as_str().unwrap().into());
                    session
                },
                "/api/sessions/session-1/snapshot" => {
                    harness.snapshot_reads += 1;
                    let completed = harness.submissions > 0 && !harness.blocked;
                    let mut entries = vec![json!({"id":"base","parentId":null,"content":{"kind":"omitted"}})];
                    if completed {
                        entries.push(json!({"id":"reply","parentId":"base","content":{"kind":"message","message":{"role":"assistant","content":[{"type":"text","text":"final answer"}]}}}));
                    }
                    json!({"version":2,"session":session,"history":{"entries":entries,"leafId":if completed {"reply"} else {"base"}},
                        "execution":{"status":if harness.blocked {"requires_action"} else {"idle"},"activeTurnId":if harness.blocked {Some("turn")} else {None}},"pendingSubmissionIds":[]})
                }
                "/api/sessions/session-1/message" => {
                    harness.submissions += 1;
                    let delay = harness.delay;
                    drop(harness);
                    if delay { tokio::time::sleep(Duration::from_secs(1)).await; }
                    return Json(json!({"kind":"submission","status":"started","turnId":"turn","notices":[]}));
                }
                "/api/sessions/session-1/view" => json!({"permissions":if harness.blocked {
                    vec![json!({"agentId":"child-1","requestId":"request-1","tool":"bash","input":"echo safe","preview":null,"canAllow":true})]
                } else { vec![] }}),
                "/api/sessions/session-1/permission" => {
                    assert_eq!(body["agentId"], "child-1");
                    assert_eq!(body["requestId"], "request-1");
                    assert_eq!(body["decision"], "allow_once");
                    harness.permissions += 1;
                    json!({"status":"answered"})
                }
                "/bottest/sendChatAction" | "/bottest/setMessageReaction" => {
                    harness.feedback.push((path.to_owned(), body));
                    if harness.feedback_fails {
                        return Json(json!({"ok":false,"error_code":429}));
                    }
                    json!({"ok":true,"result":true})
                }
                "/bottest/sendMessage" => {
                    harness.send_attempts += 1;
                    if harness.send_fails {
                        return Json(json!({"ok":false,"error_code":400,"description":"Bad Request: invalid message"}));
                    }
                    harness.sent.push(body);
                    let response = json!({"ok":true,"result":{"message_id":harness.sent.len(),"chat":{"id":7,"type":"private"}}});
                    let delay = harness.delay;
                    drop(harness);
                    if delay { tokio::time::sleep(Duration::from_secs(1)).await; }
                    return Json(response);
                }
                "/bottest/getMe" => json!({"ok":true,"result":{"id":1,"is_bot":true}}),
                "/bottest/getUpdates" => {
                    assert_eq!(body["timeout"], 30);
                    harness.polls += 1;
                    drop(harness);
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return Json(json!({"ok":true,"result":[]}));
                }
                "/bottest/answerCallbackQuery" => json!({"ok":true,"result":true}),
                "/bottest/editMessageReplyMarkup" => json!({"ok":true,"result":{}}),
                _ => panic!("unexpected test route {path}"),
            };
            Json(response)
        }).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let serving = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (endpoint, state, serving)
    }

    fn test_bridge(directory: &std::path::Path, endpoint: &str) -> Bridge {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = std::sync::Arc::new(Store::open(directory).unwrap());
        let state = store.load("state.json").unwrap().unwrap_or_default();
        Bridge {
            feedback: Default::default(),
            working_dir: directory.join("workspace"),
            config: Config {
                bot_token: "test".into(),
                allowed_users: vec![7],
            },
            gateway: PluginClient::test_client(endpoint),
            telegram: Telegram::test_endpoint("test".into(), endpoint.into()).unwrap(),
            store,
            state,
        }
    }

    fn message(id: i64) -> Update {
        serde_json::from_value(json!({"update_id":id,"message":{"message_id":id,"from":{"id":7},"chat":{"id":7,"type":"private"},"text":"hello"}})).unwrap()
    }

    #[tokio::test]
    async fn active_refresh_continues_during_long_poll_then_sleeps_idle() {
        use std::os::unix::fs::MetadataExt;
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        harness.lock().await.blocked = true;
        bridge.process(message(10)).await.unwrap();
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let (scheduler, task) = crate::gateway::scheduler::test_scheduler(receiver);
        {
            let running = bridge.run(&scheduler);
            tokio::pin!(running);
            tokio::select! {
                result = &mut running => panic!("bridge stopped: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(8), async {
                    loop {
                        if harness.lock().await.polls > 0 { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    harness.lock().await.blocked = false;
                    loop {
                        if harness.lock().await.sent.iter().any(|reply| reply["text"] == "final answer") { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let reads = harness.lock().await.snapshot_reads;
                    let state = std::fs::metadata(directory.path().join("state.json")).unwrap();
                    tokio::time::sleep(Duration::from_millis(2200)).await;
                    assert_eq!(harness.lock().await.snapshot_reads, reads);
                    assert_eq!(std::fs::metadata(directory.path().join("state.json")).unwrap().ino(), state.ino());
                    assert_eq!(harness.lock().await.polls, 1);
                }) => result.unwrap(),
            }
        }
        shutdown.send(true).unwrap();
        task.await.unwrap();
        serving.abort();
    }

    #[tokio::test]
    async fn private_message_to_final_reply_survives_restart_without_resubmission() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        bridge.process(message(10)).await.unwrap();
        assert!(bridge.state.outbox.is_empty());
        assert!(bridge.state.chats[&7].waiting);
        assert_eq!(
            harness.lock().await.working_dirs,
            [directory.path().join("workspace")]
        );
        assert_eq!(harness.lock().await.submissions, 1);
        drop(bridge);
        let mut bridge = test_bridge(directory.path(), &endpoint);
        bridge.process(message(10)).await.unwrap();
        bridge.refresh().await.unwrap();
        bridge.deliver().await.unwrap();
        let state = harness.lock().await;
        assert_eq!(state.submissions, 1);
        assert!(
            state
                .sent
                .iter()
                .any(|message| message["text"] == "final answer")
        );
        assert!(!bridge.state.chats[&7].waiting);
        drop(state);
        serving.abort();
    }

    #[tokio::test]
    async fn feedback_tracks_turns_without_affecting_delivery() {
        for feedback_fails in [false, true] {
            let (endpoint, harness, serving) = server().await;
            harness.lock().await.feedback_fails = feedback_fails;
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let mut bridge = test_bridge(directory.path(), &endpoint);
            bridge.process(message(10)).await.unwrap();
            assert!(bridge.state.outbox.is_empty());
            bridge.refresh().await.unwrap();
            bridge.deliver().await.unwrap();
            assert!(bridge.feedback.is_empty());
            let state = harness.lock().await;
            assert_eq!(state.submissions, 1);
            assert_eq!(state.sent.len(), 1);
            assert_eq!(state.sent[0]["text"], "final answer");
            let reactions: Vec<_> = state
                .feedback
                .iter()
                .filter(|(path, _)| path.ends_with("setMessageReaction"))
                .map(|(_, body)| {
                    (
                        body["message_id"].clone(),
                        body["reaction"][0]["emoji"].clone(),
                    )
                })
                .collect();
            assert_eq!(
                reactions,
                [Reaction::Queued, Reaction::Working, Reaction::Done]
                    .map(|reaction| (json!(10), json!(reaction.emoji().unwrap())))
            );
            serving.abort();
        }
    }

    #[tokio::test]
    async fn rejected_delivery_is_not_retried_and_next_turn_still_runs() {
        let (endpoint, harness, serving) = server().await;
        harness.lock().await.send_fails = true;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        bridge.process(message(10)).await.unwrap();
        bridge.refresh().await.unwrap();
        bridge.deliver().await.unwrap();
        assert!(bridge.state.outbox.is_empty());
        assert!(bridge.state.sending.is_none());
        let saved: State = bridge.store.load("state.json").unwrap().unwrap();
        assert!(saved.sending.is_none());
        assert_eq!(harness.lock().await.send_attempts, 1);
        harness.lock().await.send_fails = false;
        bridge.process(message(11)).await.unwrap();
        bridge.refresh().await.unwrap();
        bridge.deliver().await.unwrap();
        let state = harness.lock().await;
        assert_eq!(state.submissions, 2);
        assert_eq!(state.send_attempts, 2);
        assert_eq!(state.sent.len(), 1);
        serving.abort();
    }

    #[tokio::test]
    async fn waiting_feedback_is_deduplicated_and_keeps_original_message() {
        let (endpoint, harness, serving) = server().await;
        harness.lock().await.blocked = true;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        bridge.process(message(10)).await.unwrap();
        bridge.refresh().await.unwrap();
        bridge.refresh().await.unwrap();
        bridge.process(message(11)).await.unwrap();
        let state = harness.lock().await;
        assert_eq!(state.submissions, 1);
        let reactions: Vec<_> = state
            .feedback
            .iter()
            .filter(|(path, _)| path.ends_with("setMessageReaction"))
            .collect();
        assert_eq!(reactions.len(), 3);
        assert!(reactions.iter().all(|(_, body)| body["message_id"] == 10));
        assert_eq!(
            reactions.last().unwrap().1["reaction"][0]["emoji"],
            Reaction::Waiting.emoji().unwrap()
        );
        serving.abort();
    }

    #[tokio::test]
    async fn crash_after_gateway_admission_never_replays_the_claimed_update() {
        let (endpoint, harness, serving) = server().await;
        harness.lock().await.delay = true;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), bridge.process(message(20)))
                .await
                .is_err()
        );
        assert_eq!(harness.lock().await.submissions, 1);
        drop(bridge);
        let mut bridge = test_bridge(directory.path(), &endpoint);
        assert_eq!(bridge.state.inflight, Some(7));
        bridge.recover_uncertain();
        bridge.save().unwrap();
        bridge.process(message(20)).await.unwrap();
        assert_eq!(harness.lock().await.submissions, 1);
        assert!(
            bridge
                .state
                .outbox
                .iter()
                .any(|delivery| delivery.text.contains("not retried"))
        );
        serving.abort();
    }

    #[tokio::test]
    async fn interrupted_delivery_is_reported_but_never_automatically_duplicated() {
        let (endpoint, harness, serving) = server().await;
        harness.lock().await.delay = true;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        bridge.enqueue(7, "original reply");
        bridge.save().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), bridge.deliver())
                .await
                .is_err()
        );
        assert_eq!(harness.lock().await.sent.len(), 1);
        drop(bridge);
        let mut bridge = test_bridge(directory.path(), &endpoint);
        assert_eq!(bridge.state.sending, Some(7));
        assert!(bridge.state.outbox.is_empty());
        bridge.recover_uncertain();
        harness.lock().await.delay = false;
        bridge.deliver().await.unwrap();
        let sent = &harness.lock().await.sent;
        assert_eq!(
            sent.iter()
                .filter(|message| message["text"] == "original reply")
                .count(),
            1
        );
        assert!(
            sent.iter()
                .any(|message| message["text"].as_str().unwrap().contains("not retried"))
        );
        serving.abort();
    }

    #[tokio::test]
    async fn failed_durable_claim_prevents_any_gateway_mutation() {
        use std::os::unix::fs::PermissionsExt;
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        bridge.save().unwrap();
        std::fs::set_permissions(
            directory.path().join("state.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(bridge.process(message(25)).await.is_err());
        assert_eq!(harness.lock().await.submissions, 0);
        serving.abort();
    }

    #[tokio::test]
    async fn permission_callback_is_bound_to_sender_chat_message_and_live_request() {
        let (endpoint, harness, serving) = server().await;
        harness.lock().await.blocked = true;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut bridge = test_bridge(directory.path(), &endpoint);
        bridge.process(message(30)).await.unwrap();
        bridge.refresh().await.unwrap();
        bridge.deliver().await.unwrap();
        let (token, question) = bridge.state.chats[&7].questions.iter().next().unwrap();
        let data = format!("allow_once:{token}");
        let message_id = question.message_id.unwrap();
        let callback = |id, sender, message_id| {
            serde_json::from_value::<Update>(json!({"update_id":id,"callback_query":{"id":"callback","from":{"id":sender},"message":{"message_id":message_id,"chat":{"id":7,"type":"private"}},"data":data}})).unwrap()
        };
        bridge.process(callback(31, 8, message_id)).await.unwrap();
        bridge
            .process(callback(32, 7, message_id + 1))
            .await
            .unwrap();
        assert_eq!(harness.lock().await.permissions, 0);
        bridge.process(callback(33, 7, message_id)).await.unwrap();
        bridge.process(callback(34, 7, message_id)).await.unwrap();
        assert_eq!(harness.lock().await.permissions, 1);
        serving.abort();
    }

    #[test]
    fn callbacks_cannot_request_persistent_grants_or_embed_arbitrary_ids() {
        let token = "a".repeat(32);
        assert!(parse_callback(&format!("allow_once:{token}")).is_some());
        assert!(parse_callback(&format!("deny:{token}")).is_some());
        for invalid in [
            format!("allow_always:{token}"),
            format!("allow_once:{token}:extra"),
            "deny:request-id".into(),
        ] {
            assert!(parse_callback(&invalid).is_none());
        }
    }
}
