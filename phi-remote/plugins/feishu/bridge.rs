use std::collections::{HashMap, HashSet};

use anyhow::{Context, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::super::PluginClient;
use super::super::activity::{Refresh, WorkState};
use super::super::client::session_path;
use super::api::{DEDUP_MILLIS, Envelope, Feishu, MessageEvent, now_millis, split_text};
use super::presentation::Presenter;
use super::store::{Binding, Config, Delivery, Route, State, Store};
use super::transport::{Acknowledgement, Incoming};

const HELP: &str = "Send text to Phi. In groups, @mention the bot.\n/session — show current Session and working directory\n/session new [directory] — create and switch (inherits current directory when omitted)\n/session list — list live and saved Sessions\n/session resume <id> — attach or restore and switch\n/new — create and switch in the Feishu default directory (ignores current binding)\n/stop — cancel current execution\n/status — show Session status\n/help — show commands\nPaths refer to the Gateway host. Switching does not stop the previous Session.\nPermissions must be answered in Gateway Web/TUI.";
const SELECT_SESSION: &str = "Choose a Session: multiple previous private-chat bindings exist. Use /session list and /session resume <id>, or /new to start in the default directory. Previous histories were not merged or reset.";
const SESSION_LIST_LIMIT: usize = 30;
const MAX_SEEN: usize = 20000;
const MAX_CHATS: usize = 256;
const MAX_OUTBOX: usize = 256;

enum SessionSource<'a> {
    ClientDefault,
    Current,
    Directory(&'a str),
}

#[derive(Debug)]
struct PersistenceFailure;
impl std::fmt::Display for PersistenceFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .write_str("bridge persistence or capacity failed; stopping before further effects")
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
    permissions: Vec<Value>,
}

pub(crate) struct Bridge {
    pub working_dir: std::path::PathBuf,
    pub config: Config,
    pub store: std::sync::Arc<Store>,
    pub state: State,
    pub gateway: PluginClient,
    pub feishu: Feishu,
    pub bot: String,
    pub presenter: Option<Presenter>,
}

impl Bridge {
    pub fn prepare(&mut self) -> anyhow::Result<()> {
        ensure!(
            self.state
                .app_id
                .as_ref()
                .is_none_or(|id| *id == self.config.app_id),
            "state belongs to another Feishu application; use a separate directory"
        );
        ensure!(
            self.state.direct_chats.iter().all(|(user, chat)| self
                .state
                .routes
                .get(chat)
                .is_some_and(|route| route.group.is_none() && &route.user == user)),
            "invalid private conversation identity index"
        );
        self.state.app_id = Some(self.config.app_id.clone());
        let uncertain: HashSet<_> = [self.state.inflight.take(), self.state.sending.take()]
            .into_iter()
            .flatten()
            .collect();
        for chat in uncertain {
            self.enqueue(&chat, "The bridge stopped during an operation or delivery. Its outcome may be unknown; it was not retried. Check /status or Gateway Web/TUI before resending.")?;
        }
        self.save()
    }

    async fn restore_sessions(&mut self) -> anyhow::Result<()> {
        let mut bindings: HashMap<String, Vec<String>> = HashMap::new();
        for (chat, binding) in &self.state.chats {
            if self.allowed_chat(chat) {
                bindings
                    .entry(binding.session.clone())
                    .or_default()
                    .push(chat.clone());
            }
        }
        if bindings.is_empty() {
            return Ok(());
        }
        let live: Vec<crate::RemoteSession> = self.gateway.get("api/sessions").await?;
        let live_ids: HashSet<_> = live.iter().map(|session| session.id.as_str()).collect();
        for (id, chats) in bindings {
            if live_ids.contains(id.as_str()) {
                continue;
            }
            let restored = async {
                session_path(&id, "snapshot")?;
                let session: crate::RemoteSession = self
                    .gateway
                    .post("api/sessions/resume", json!({"id":id}))
                    .await?;
                ensure!(session.id == id, "Gateway restored another Session");
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = restored {
                eprintln!("Feishu bound Session restoration failed: {error}");
                for chat in chats {
                    self.enqueue(&chat, "The previous Session could not be restored. Its chat binding was kept; no input was resent. Check /session list and use /session resume <id>, /new for the Feishu default directory, or /session new /absolute/path to start a different conversation.")?;
                }
            }
        }
        self.save()
    }

    pub async fn run(
        &mut self,
        mut incoming: mpsc::Receiver<Incoming>,
        acknowledgements: mpsc::Sender<Acknowledgement>,
        scheduler: &crate::GatewayScheduler,
    ) -> anyhow::Result<()> {
        self.restore_sessions().await?;
        let mut refresh = Refresh::new(scheduler);
        loop {
            self.deliver().await?;
            refresh.update(
                if self
                    .state
                    .chats
                    .iter()
                    .any(|(chat, binding)| binding.waiting && self.allowed_chat(chat))
                {
                    WorkState::Pending
                } else {
                    WorkState::Idle
                },
            );
            tokio::select! {
                message = incoming.recv() => {
                    let message = message.context("Feishu transport stopped")?;
                    let event = self.claim(&message.payload, now_millis()?)?;
                    acknowledgements.send(message.acknowledgement).await.context("Feishu acknowledgement channel closed")?;
                    if let Some(event) = event { self.apply(event).await?; }
                }
                _ = refresh.wait() => self.refresh().await?,
            }
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        self.store
            .save("state.json", &self.state)
            .map_err(|error| error.context(PersistenceFailure))
    }

    fn enqueue(&mut self, chat: &str, text: &str) -> anyhow::Result<()> {
        let chunks = split_text(text);
        if self.state.outbox.len() + chunks.len() > MAX_OUTBOX {
            return Err(PersistenceFailure.into());
        }
        self.state
            .outbox
            .extend(chunks.into_iter().map(|text| Delivery {
                chat: chat.into(),
                text,
            }));
        Ok(())
    }

    fn direct_chat(&self, user: &str, incoming_chat: &str) -> Option<String> {
        if let Some(chat) = self.state.direct_chats.get(user) {
            return Some(chat.clone());
        }
        let mut previous = self.state.routes.iter().filter(|(chat, route)| {
            route.group.is_none() && route.user == user && self.state.chats.contains_key(*chat)
        });
        if self.state.chats.contains_key(incoming_chat)
            && self
                .state
                .routes
                .get(incoming_chat)
                .is_some_and(|route| route.group.is_none() && route.user == user)
        {
            return Some(incoming_chat.into());
        }
        match (previous.next(), previous.next()) {
            (None, _) => Some(incoming_chat.into()),
            (Some((chat, _)), None) => Some(chat.clone()),
            _ => None,
        }
    }

    fn unresolved_private(&self, chat: &str) -> bool {
        self.state.routes.get(chat).is_some_and(|route| {
            route.group.is_none() && !self.state.direct_chats.contains_key(&route.user)
        })
    }

    fn recipient(&self, chat: &str) -> anyhow::Result<&str> {
        let route = self
            .state
            .routes
            .get(chat)
            .context("missing conversation route")?;
        Ok(route.group.as_deref().unwrap_or(&route.user))
    }

    fn claim(&mut self, payload: &[u8], now: u64) -> anyhow::Result<Option<MessageEvent>> {
        let Ok(envelope) = serde_json::from_slice::<Envelope>(payload) else {
            return Ok(None);
        };
        if envelope.schema != "2.0"
            || envelope.header.app_id != self.config.app_id
            || envelope.header.event_type != "im.message.receive_v1"
            || envelope.header.event_id.is_empty()
            || envelope.header.event_id.len() > 128
        {
            return Ok(None);
        }
        let Ok(event) = serde_json::from_value::<MessageEvent>(envelope.event) else {
            return Ok(None);
        };
        if event.authorize(&self.config, &self.bot, now).is_err() {
            return Ok(None);
        }
        let event_key = format!("event:{}", envelope.header.event_id);
        let message_key = format!("message:{}", event.message.message_id);
        self.state
            .seen
            .retain(|_, claimed| *claimed > now.saturating_sub(DEDUP_MILLIS));
        if self.state.seen.contains_key(&event_key) || self.state.seen.contains_key(&message_key) {
            return Ok(None);
        }
        let selected = if event.message.chat_type == "p2p" {
            self.direct_chat(&event.sender.sender_id.open_id, &event.message.chat_id)
        } else {
            Some(event.message.chat_id.clone())
        };
        let chat = selected
            .as_deref()
            .unwrap_or(&event.message.chat_id)
            .to_owned();
        if self.state.seen.len() + 2 > MAX_SEEN
            || (!self.state.routes.contains_key(&chat) && self.state.routes.len() >= MAX_CHATS)
        {
            return Err(PersistenceFailure.into());
        }
        ensure!(
            self.state
                .routes
                .get(&chat)
                .is_none_or(|route| if event.message.chat_type == "p2p" {
                    route.group.is_none() && route.user == event.sender.sender_id.open_id
                } else {
                    route.group.as_deref() == Some(event.message.chat_id.as_str())
                }),
            "conversation identity conflicts with existing route"
        );
        self.state.routes.insert(
            chat.clone(),
            Route {
                user: event.sender.sender_id.open_id.clone(),
                group: (event.message.chat_type == "group").then(|| event.message.chat_id.clone()),
            },
        );
        self.state.seen.insert(event_key, now);
        self.state.seen.insert(message_key, now);
        if event.message.chat_type == "p2p" && selected.is_some() {
            self.state
                .direct_chats
                .insert(event.sender.sender_id.open_id.clone(), chat.clone());
        }
        self.state.inflight = Some(chat);
        // Acknowledge only after a durable claim, before any Gateway side effect.
        self.save()?;
        Ok(Some(event))
    }

    async fn apply(&mut self, event: MessageEvent) -> anyhow::Result<()> {
        let conversation = self
            .state
            .inflight
            .clone()
            .context("missing claimed conversation")?;
        let chat = conversation.as_str();
        let was_waiting = self
            .state
            .chats
            .get(chat)
            .is_some_and(|binding| binding.waiting);
        let outcome = async {
            let text = event.text(&self.bot)?;
            match &text {
                Some(text) => self.text(chat, text).await?,
                None => self.enqueue(chat, "Only text messages are supported.")?,
            }
            Ok::<_, anyhow::Error>(text)
        }
        .await;
        let recipient = self.recipient(chat)?.to_owned();
        if let Ok(Some(text)) = &outcome
            && !was_waiting
            && !text.is_empty()
            && !text.starts_with('/')
            && let Some(binding) = self.state.chats.get(chat).filter(|binding| binding.waiting)
            && let Some(presenter) = &mut self.presenter
        {
            presenter
                .start(&recipient, &event.message.message_id, &binding.session)
                .await;
        }
        if let Err(error) = outcome {
            if error.downcast_ref::<PersistenceFailure>().is_some() {
                return Err(error);
            }
            eprintln!("Feishu chat operation failed; not retried: {error}");
            self.enqueue(chat, "Operation failed or its outcome is unknown. It was not retried. Check /status or Gateway Web/TUI before resending. Use /session list, /session resume <id>, /new or /session new /absolute/path if the bound Session is unavailable.")?;
        }
        self.state.inflight = None;
        self.save()
    }

    async fn text(&mut self, chat: &str, text: &str) -> anyhow::Result<()> {
        match text {
            "/help" | "/start" => self.enqueue(chat, HELP)?,
            "/status" => self.show_session(chat).await?,
            "/stop" => {
                if let Some(binding) = self.state.chats.get(chat) {
                    let _: Value = self
                        .gateway
                        .post(&session_path(&binding.session, "cancel")?, json!({}))
                        .await?;
                    self.enqueue(chat, "Cancellation requested.")?;
                } else {
                    self.enqueue(chat, "No Session to cancel.")?;
                }
            }
            "/new" => {
                let snapshot = self
                    .create_session(chat, SessionSource::ClientDefault)
                    .await?;
                self.announce_session(chat, &snapshot)?;
            }
            "" => {}
            text if text.split_whitespace().next() == Some("/session") => {
                self.session_command(chat, text.trim_start_matches("/session").trim())
                    .await?;
            }
            text if text.starts_with('/') => self.enqueue(chat, HELP)?,
            text => self.submit(chat, text).await?,
        }
        Ok(())
    }

    async fn show_session(&mut self, chat: &str) -> anyhow::Result<()> {
        if self.unresolved_private(chat) {
            return self.enqueue(chat, SELECT_SESSION);
        }
        if let Some(binding) = self.state.chats.get(chat) {
            let snapshot = self.snapshot(&binding.session).await?;
            self.enqueue(
                chat,
                &format!(
                    "Session: {}\nDirectory: {}\nStatus: {}",
                    snapshot.session.id,
                    snapshot.session.working_dir.display(),
                    snapshot.execution.status
                ),
            )
        } else {
            self.enqueue(
                chat,
                "No Session yet. Send text or use /session new [directory].",
            )
        }
    }

    async fn session_command(&mut self, chat: &str, arguments: &str) -> anyhow::Result<()> {
        let (command, argument) = arguments
            .split_once(char::is_whitespace)
            .map_or((arguments, ""), |(command, argument)| {
                (command, argument.trim())
            });
        let snapshot = match (command, argument) {
            ("", "") => return self.show_session(chat).await,
            ("list", "") => return self.list_sessions(chat).await,
            ("new", directory) => {
                if directory.is_empty() && self.unresolved_private(chat) {
                    return self.enqueue(chat, SELECT_SESSION);
                }
                let source = if directory.is_empty() {
                    SessionSource::Current
                } else {
                    SessionSource::Directory(directory)
                };
                self.create_session(chat, source).await?
            }
            ("resume", id) if !id.is_empty() && session_path(id, "snapshot").is_ok() => {
                self.ensure_binding_capacity(chat)?;
                let session: crate::RemoteSession = self
                    .gateway
                    .post("api/sessions/resume", json!({"id":id}))
                    .await?;
                ensure!(session.id == id, "Gateway returned another Session");
                self.bind(chat, session).await?
            }
            _ => return self.enqueue(chat, HELP),
        };
        self.announce_session(chat, &snapshot)
    }

    fn announce_session(&mut self, chat: &str, snapshot: &Snapshot) -> anyhow::Result<()> {
        self.enqueue(
            chat,
            &format!(
                "Session: {}\nDirectory: {}\nStatus: {}\nThe previous Session was not stopped.",
                snapshot.session.id,
                snapshot.session.working_dir.display(),
                snapshot.execution.status
            ),
        )
    }

    async fn list_sessions(&mut self, chat: &str) -> anyhow::Result<()> {
        let live: Vec<crate::RemoteSession> = self.gateway.get("api/sessions").await?;
        let saved: Vec<crate::SavedSession> = self.gateway.get("api/sessions/saved").await?;
        let live_ids: HashSet<_> = live.iter().map(|session| session.id.as_str()).collect();
        let current = self
            .state
            .chats
            .get(chat)
            .map(|binding| binding.session.as_str());
        let mut entries: Vec<_> = live
            .iter()
            .map(|session| {
                format!(
                    "{} [live{}] {}",
                    session.id,
                    if current == Some(session.id.as_str()) {
                        ", current"
                    } else {
                        ""
                    },
                    session.working_dir.display()
                )
            })
            .chain(
                saved
                    .iter()
                    .filter(|session| !live_ids.contains(session.id.as_str()))
                    .map(|session| {
                        format!(
                            "{} [saved{}] {}",
                            session.id,
                            if current == Some(session.id.as_str()) {
                                ", current"
                            } else {
                                ""
                            },
                            session.name.replace(['\r', '\n'], " ")
                        )
                    }),
            )
            .collect();
        if entries.is_empty() {
            return self.enqueue(chat, "No Sessions found. Use /session new [directory].");
        }
        let total = entries.len();
        entries.truncate(SESSION_LIST_LIMIT);
        let mut text = entries.join("\n");
        if total > SESSION_LIST_LIMIT {
            text.push_str(&format!(
                "\nShowing {SESSION_LIST_LIMIT} of {total} Sessions."
            ));
        }
        text.push_str("\n/session resume <id> — select a Session");
        self.enqueue(chat, &text)
    }

    fn ensure_binding_capacity(&self, chat: &str) -> anyhow::Result<()> {
        ensure!(
            self.state.chats.contains_key(chat) || self.state.chats.len() < MAX_CHATS,
            "conversation limit reached"
        );
        Ok(())
    }

    async fn create_session(
        &mut self,
        chat: &str,
        source: SessionSource<'_>,
    ) -> anyhow::Result<Snapshot> {
        self.ensure_binding_capacity(chat)?;
        let (path, payload) = match source {
            SessionSource::Directory(directory) => {
                let directory = if directory == "~" || directory.starts_with("~/") {
                    dirs::home_dir()
                        .context("cannot locate home directory")?
                        .join(directory.strip_prefix("~/").unwrap_or(""))
                } else {
                    self.working_dir.join(directory)
                };
                ("api/sessions".to_owned(), json!({"workingDir":directory}))
            }
            SessionSource::Current if self.state.chats.contains_key(chat) => (
                session_path(&self.state.chats[chat].session, "new")?,
                json!({}),
            ),
            SessionSource::ClientDefault | SessionSource::Current => (
                "api/sessions".to_owned(),
                json!({"workingDir":self.working_dir}),
            ),
        };
        let session = self.gateway.post(&path, payload).await?;
        self.bind(chat, session).await
    }

    async fn bind(
        &mut self,
        chat: &str,
        session: crate::RemoteSession,
    ) -> anyhow::Result<Snapshot> {
        let snapshot = self.snapshot(&session.id).await?;
        let waiting = !execution_settled(&snapshot);
        let recipient = self.recipient(chat)?.to_owned();
        if let Some(presenter) = &mut self.presenter {
            presenter.close(&recipient).await;
        }
        if let Some(route) = self
            .state
            .routes
            .get(chat)
            .filter(|route| route.group.is_none())
        {
            self.state
                .direct_chats
                .insert(route.user.clone(), chat.into());
        }
        self.state.chats.insert(
            chat.to_owned(),
            Binding {
                session: session.id,
                cursor: snapshot.history.leaf_id.clone(),
                waiting,
                permission_notified: false,
            },
        );
        self.save()?;
        Ok(snapshot)
    }

    async fn submit(&mut self, chat: &str, text: &str) -> anyhow::Result<()> {
        if self.unresolved_private(chat) {
            return self.enqueue(chat, SELECT_SESSION);
        }
        if self
            .state
            .chats
            .get(chat)
            .is_some_and(|binding| binding.waiting)
        {
            return self.enqueue(
                chat,
                "Phi is still working. Wait for the reply or use /stop.",
            );
        }
        if !self.state.chats.contains_key(chat) {
            self.create_session(chat, SessionSource::ClientDefault)
                .await?;
        }
        let session = self.state.chats[chat].session.clone();
        let snapshot = self.snapshot(&session).await?;
        let binding = self
            .state
            .chats
            .get_mut(chat)
            .context("missing chat binding")?;
        binding.cursor = snapshot.history.leaf_id;
        binding.waiting = true;
        binding.permission_notified = false;
        self.save()?;
        let receipt: Value = self
            .gateway
            .post(&session_path(&session, "message")?, json!({"message":text}))
            .await?;
        match receipt["status"].as_str() {
            Some("started" | "queued") => {}
            Some("handled") => {
                self.state
                    .chats
                    .get_mut(chat)
                    .context("missing chat binding")?
                    .waiting = false;
                self.enqueue(
                    chat,
                    receipt["reason"]
                        .as_str()
                        .unwrap_or("Input handled without starting a turn."),
                )?;
            }
            _ => bail!("invalid Gateway admission receipt"),
        }
        if let Some(notices) = receipt["notices"].as_array() {
            for notice in notices.iter().filter_map(Value::as_str) {
                self.enqueue(chat, notice)?;
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

    fn allowed_chat(&self, chat: &str) -> bool {
        self.state
            .routes
            .get(chat)
            .is_some_and(|route| match &route.group {
                Some(group) => group == chat && self.config.allowed_groups.contains(group),
                None => {
                    self.config.allowed_users.contains(&route.user)
                        && self
                            .state
                            .direct_chats
                            .get(&route.user)
                            .is_none_or(|selected| selected == chat)
                }
            })
    }

    async fn refresh(&mut self) -> anyhow::Result<()> {
        let chats: Vec<_> = self
            .state
            .chats
            .iter()
            .filter(|(chat, binding)| binding.waiting && self.allowed_chat(chat))
            .map(|(chat, binding)| (chat.clone(), binding.session.clone()))
            .collect();
        for (chat, session) in chats {
            if let Err(error) = self.refresh_chat(&chat, &session).await {
                if error.downcast_ref::<PersistenceFailure>().is_some() {
                    return Err(error);
                }
                eprintln!("Gateway observation unavailable; read will be retried");
            }
        }
        self.save()
    }

    async fn refresh_chat(&mut self, chat: &str, session: &str) -> anyhow::Result<()> {
        let snapshot = self.snapshot(session).await?;
        let view: OfficeView = self.gateway.get(&session_path(session, "view")?).await?;
        let binding = self
            .state
            .chats
            .get_mut(chat)
            .context("missing chat binding")?;
        if view.permissions.is_empty() {
            binding.permission_notified = false;
        } else if !binding.permission_notified {
            binding.permission_notified = true;
            self.enqueue(chat, "Phi needs permission. Review and answer in Gateway Web/TUI, or use /stop. Feishu cannot approve permissions.")?;
        }
        if !execution_settled(&snapshot) {
            return Ok(());
        }
        let cursor = self.state.chats[chat].cursor.clone();
        let replies = replies_since(&snapshot.history, cursor.as_deref());
        let binding = self
            .state
            .chats
            .get_mut(chat)
            .context("missing chat binding")?;
        binding.cursor = snapshot.history.leaf_id;
        binding.waiting = false;
        binding.permission_notified = false;
        let replies = match replies {
            Ok(replies) if replies.is_empty() => vec!["Execution settled without a new text reply. Check Gateway Web/TUI if the previous operation had an unknown outcome.".into()],
            Ok(replies) => replies,
            Err(_) => vec!["History changed or the saved cursor is unavailable. Read the reply in Gateway Web/TUI; older messages were not replayed.".into()],
        };
        // Commit the cursor before a final card write, just like at-most-once text delivery.
        self.save()?;
        let recipient = self.recipient(chat)?.to_owned();
        let delivered = if let Some(presenter) = &mut self.presenter {
            presenter.finish(&recipient, &replies.join("\n\n")).await
        } else {
            false
        };
        if !delivered {
            for reply in replies {
                self.enqueue(chat, &reply)?;
            }
        }
        Ok(())
    }

    async fn deliver(&mut self) -> anyhow::Result<()> {
        while !self.state.outbox.is_empty() {
            let delivery = self.state.outbox.remove(0);
            if !self.allowed_chat(&delivery.chat) {
                self.save()?;
                continue;
            }
            self.state.sending = Some(delivery.chat.clone());
            self.save()?;
            self.feishu
                .send(self.recipient(&delivery.chat)?, &delivery.text)
                .await?;
            self.state.sending = None;
            self.save()?;
        }
        Ok(())
    }
}

fn execution_settled(snapshot: &Snapshot) -> bool {
    snapshot.execution.active_turn_id.is_none()
        && snapshot.pending_submission_ids.is_empty()
        && matches!(
            snapshot.execution.status.as_str(),
            "idle" | "failed" | "stopped"
        )
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
    use proptest::prelude::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    #[test]
    fn history_recovery_rejects_branch_changes_missing_cursors_and_cycles() {
        let mut history: History = serde_json::from_value(json!({"leafId":"reply","entries":[
            {"id":"base","parentId":null,"content":{"kind":"omitted"}},
            {"id":"other","parentId":"base","content":{"kind":"omitted"}},
            {"id":"reply","parentId":"base","content":{"kind":"failure","failure":{"message":"failure text"}}}
        ]})).unwrap();
        assert_eq!(
            replies_since(&history, Some("base")).unwrap(),
            ["Execution failed: failure text"]
        );
        assert!(replies_since(&history, Some("other")).is_err());
        assert!(replies_since(&history, Some("missing")).is_err());
        history.entries[2].parent_id = Some("reply".into());
        assert!(replies_since(&history, Some("base")).is_err());
    }

    #[derive(Default)]
    struct Harness {
        sessions: usize,
        submissions: usize,
        cancellations: usize,
        sent: Vec<Value>,
        prompts: Vec<String>,
        blocked: bool,
        delay: bool,
        directories: HashMap<String, String>,
        saved_directories: HashMap<String, String>,
        requests: Vec<(String, Value)>,
        failing_path: Option<String>,
    }

    async fn server() -> (String, Arc<Mutex<Harness>>, tokio::task::JoinHandle<()>) {
        use axum::{
            Json,
            extract::{OriginalUri, State},
            http::{HeaderMap, StatusCode},
            response::IntoResponse,
        };
        let state = Arc::new(Mutex::new(Harness::default()));
        let app = axum::Router::new().fallback(
            |State(state): State<Arc<Mutex<Harness>>>, OriginalUri(uri): OriginalUri,
             headers: HeaderMap, body: axum::body::Bytes| async move {
                let mut harness = state.lock().await;
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let path = uri.path();
                if path.starts_with("/api/") {
                    assert_eq!(headers["authorization"], format!("Bearer {}", "a".repeat(64)));
                    if !body.is_null() { assert!(headers.contains_key("origin")); }
                }
                harness.requests.push((path.into(), body.clone()));
                if harness.failing_path.as_deref() == Some(path) {
                    return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"unavailable"}))).into_response();
                }
                let session = |id: &str, directory: &str| json!({"id":id,"name":"test","provider":"test","model":"test","workingDir":directory});
                let response = match path {
                    "/api/sessions" if body.is_null() => {
                        let mut sessions: Vec<_> = harness.directories.iter().map(|(id, directory)| session(id, directory)).collect();
                        sessions.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
                        json!(sessions)
                    }
                    "/api/sessions" => {
                        harness.sessions += 1;
                        let id = format!("session-{}", harness.sessions);
                        let directory = body["workingDir"].as_str().unwrap_or("/tmp").to_owned();
                        harness.directories.insert(id.clone(), directory.clone());
                        session(&id, &directory)
                    }
                    "/api/sessions/saved" => json!([
                        {"id":"saved-session","name":"saved history"},
                        {"id":"session-1","name":"also saved"}
                    ]),
                    "/api/sessions/resume" => {
                        let id = body["id"].as_str().unwrap();
                        if id == "saved-session" {
                            harness.directories.entry(id.into()).or_insert("/saved/project".into());
                        }
                        if let Some(directory) = harness.saved_directories.get(id).cloned() {
                            harness.directories.entry(id.into()).or_insert(directory);
                        }
                        let Some(directory) = harness.directories.get(id) else {
                            return (StatusCode::SERVICE_UNAVAILABLE, Json(json!({"error":"unknown Session"}))).into_response();
                        };
                        session(id, directory)
                    }
                    path if path.ends_with("/new") => {
                        let source = path.split('/').nth(3).unwrap();
                        let directory = harness.directories.get(source).unwrap().clone();
                        harness.sessions += 1;
                        let id = format!("session-{}", harness.sessions);
                        harness.directories.insert(id.clone(), directory.clone());
                        session(&id, &directory)
                    }
                    path if path.ends_with("/snapshot") => {
                        let id = path.split('/').nth(3).unwrap();
                        let Some(directory) = harness.directories.get(id) else {
                            return (StatusCode::NOT_FOUND, Json(json!({"error":"unknown Session"}))).into_response();
                        };
                        let completed = harness.submissions > 0 && !harness.blocked;
                        let mut entries = vec![json!({"id":"base","parentId":null,"content":{"kind":"omitted"}})];
                        if completed {
                            entries.push(json!({"id":"reply","parentId":"base","content":{"kind":"message","message":{"role":"assistant","content":[{"type":"thinking","text":"private reasoning"},{"type":"text","text":"final answer"},{"type":"tool_call","args":{"secret":"hidden"}}]}}}));
                        }
                        json!({"version":2,"session":session(id, directory),"history":{"entries":entries,"leafId":if completed {"reply"} else {"base"}},
                            "execution":{"status":if harness.blocked {"requires_action"} else {"idle"},"activeTurnId":if harness.blocked {Some("turn")} else {None}},"pendingSubmissionIds":[]})
                    }
                    path if path.ends_with("/message") => {
                        harness.submissions += 1;
                        harness.prompts.push(body["message"].as_str().unwrap().into());
                        let delay = harness.delay;
                        drop(harness);
                        if delay { tokio::time::sleep(Duration::from_secs(1)).await; }
                        return Json(json!({"kind":"submission","status":"started","turnId":"turn","notices":[]})).into_response();
                    }
                    path if path.ends_with("/cancel") => {
                        harness.cancellations += 1;
                        json!({"status":"cancelled"})
                    }
                    path if path.ends_with("/view") => json!({"permissions":if harness.blocked {
                        vec![json!({"agentId":"child","requestId":"request","tool":"bash","input":"secret input","canAllow":true})]
                    } else { vec![] }}),
                    "/open-apis/auth/v3/tenant_access_token/internal" => {
                        assert_eq!(body["app_id"], "cli_test");
                        json!({"code":0,"tenant_access_token":"test-token","expire":7200})
                    }
                    "/open-apis/cardkit/v1/cards" => json!({"code":0,"data":{"card_id":"card_1"}}),
                    "/open-apis/cardkit/v1/cards/card_1/elements/content/content"
                    | "/open-apis/cardkit/v1/cards/card_1/settings" => json!({"code":0}),
                    path if path.contains("/reactions") => json!({"code":0,"data":{"reaction_id":"reaction_1"}}),
                    "/open-apis/im/v1/messages" => {
                        assert_eq!(headers["authorization"], "Bearer test-token");
                        let recipient = body["receive_id"].as_str().unwrap();
                        assert_eq!(uri.query(), Some(if recipient.starts_with("ou_") { "receive_id_type=open_id" } else { "receive_id_type=chat_id" }));
                        assert!(matches!(body["msg_type"].as_str(), Some("text" | "interactive")));
                        harness.sent.push(body);
                        let delay = harness.delay;
                        drop(harness);
                        if delay { tokio::time::sleep(Duration::from_secs(1)).await; }
                        return Json(json!({"code":0,"data":{"message_id":"om_reply"}})).into_response();
                    }
                    _ => panic!("unexpected route {path}"),
                };
                Json(response).into_response()
            }).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let serving = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (endpoint, state, serving)
    }

    fn bridge(directory: &std::path::Path, endpoint: &str) -> Bridge {
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = std::sync::Arc::new(Store::open(directory).unwrap());
        let state = store.load("state.json").unwrap().unwrap_or_default();
        Bridge {
            working_dir: directory.join("workspace"),
            config: Config {
                app_id: "cli_test".into(),
                app_secret: "test".into(),
                allowed_users: vec!["ou_user".into(), "ou_second".into()],
                allowed_groups: vec!["oc_group".into()],
            },
            gateway: PluginClient::test_client(endpoint),
            feishu: Feishu::test_endpoint(endpoint),
            store,
            state,
            bot: "ou_bot".into(),
            presenter: None,
        }
    }

    fn message(id: &str, chat: &str, text: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"schema":"2.0","header":{"event_id":format!("event-{id}"),
            "event_type":"im.message.receive_v1","app_id":"cli_test"},
            "event":{"sender":{"sender_type":"user","sender_id":{"open_id":"ou_user"}},
            "message":{"message_id":format!("om_{id}"),"chat_id":chat,"chat_type":if chat == "oc_group" {"group"} else {"p2p"},
                "message_type":"text","create_time":now_millis().unwrap().to_string(),
                "content":json!({"text":text}).to_string(),
                "mentions":if chat == "oc_group" {vec![json!({"key":"@_user_1","id":{"open_id":"ou_bot"}})]} else {vec![]}}}})).unwrap()
    }

    async fn receive(bridge: &mut Bridge, payload: &[u8]) {
        if let Some(event) = bridge.claim(payload, now_millis().unwrap()).unwrap() {
            bridge.apply(event).await.unwrap();
        }
    }

    fn sent_text(message: &Value) -> String {
        let content: Value = serde_json::from_str(message["content"].as_str().unwrap()).unwrap();
        content["text"].as_str().unwrap().into()
    }

    #[test]
    fn private_routes_never_cross_users_or_groups() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), "http://127.0.0.1:1");
        for (chat, user, group) in [
            ("oc_private", "ou_user", None),
            ("oc_second", "ou_second", None),
            ("oc_group", "ou_user", Some("oc_group")),
        ] {
            client.state.routes.insert(
                chat.into(),
                Route {
                    user: user.into(),
                    group: group.map(str::to_owned),
                },
            );
            client.state.chats.insert(
                chat.into(),
                Binding {
                    session: format!("session-{chat}"),
                    ..Binding::default()
                },
            );
        }
        for selection in [None, Some("oc_private")] {
            if let Some(chat) = selection {
                client
                    .state
                    .direct_chats
                    .insert("ou_user".into(), chat.into());
            }
            proptest::test_runner::TestRunner::default()
                .run(&"[a-z0-9_]{1,32}", |suffix| {
                    let incoming = format!("oc_reopened_{suffix}");
                    prop_assert_eq!(
                        client.direct_chat("ou_user", &incoming),
                        Some("oc_private".into())
                    );
                    prop_assert_eq!(
                        client.direct_chat("ou_second", &incoming),
                        Some("oc_second".into())
                    );
                    prop_assert_eq!(client.direct_chat("ou_unknown", &incoming), Some(incoming));
                    Ok(())
                })
                .unwrap();
        }
    }

    #[tokio::test]
    async fn private_identity_survives_legacy_state_restart_and_changed_chat_id() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        let first = message("before_clear", "oc_private", "first question");
        receive(&mut client, &first).await;
        client.refresh().await.unwrap();
        client.deliver().await.unwrap();
        let binding = client.state.chats["oc_private"].clone();
        client
            .enqueue("oc_private", "queued before reopen")
            .unwrap();
        client.save().unwrap();
        let mut saved: Value = client.store.load("state.json").unwrap().unwrap();
        saved.as_object_mut().unwrap().remove("directChats");
        client.store.save("state.json", &saved).unwrap();
        drop(client);
        {
            let mut harness = harness.lock().await;
            harness.saved_directories = std::mem::take(&mut harness.directories);
        }
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        client.restore_sessions().await.unwrap();
        client.deliver().await.unwrap();
        receive(&mut client, &first).await;
        receive(
            &mut client,
            &message("after_clear", "oc_reopened", "continue old conversation"),
        )
        .await;
        assert_eq!(client.state.chats.len(), 1);
        assert_eq!(client.state.routes.len(), 1);
        assert_eq!(client.state.chats["oc_private"].session, binding.session);
        assert_eq!(client.state.chats["oc_private"].cursor, binding.cursor);
        let saved: Value = client.store.load("state.json").unwrap().unwrap();
        assert_eq!(saved["directChats"]["ou_user"], "oc_private");
        let harness = harness.lock().await;
        assert_eq!(harness.sessions, 1);
        assert_eq!(
            harness.prompts,
            ["first question", "continue old conversation"]
        );
        assert!(
            harness
                .sent
                .iter()
                .all(|value| value["receive_id"] == "ou_user")
        );
        assert!(
            harness
                .sent
                .iter()
                .any(|value| sent_text(value) == "queued before reopen")
        );
        serving.abort();
    }

    #[tokio::test]
    async fn ambiguous_legacy_private_bindings_require_explicit_selection() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        for (chat, session) in [
            ("oc_old_first", "session-1"),
            ("oc_old_second", "session-2"),
        ] {
            client.state.routes.insert(
                chat.into(),
                Route {
                    user: "ou_user".into(),
                    group: None,
                },
            );
            client.state.chats.insert(
                chat.into(),
                Binding {
                    session: session.into(),
                    cursor: Some("base".into()),
                    ..Binding::default()
                },
            );
            harness
                .lock()
                .await
                .directories
                .insert(session.into(), "/project".into());
        }
        receive(
            &mut client,
            &message("ambiguous", "oc_reopened", "continue"),
        )
        .await;
        assert_eq!(harness.lock().await.submissions, 0);
        assert_eq!(harness.lock().await.sessions, 0);
        assert_eq!(client.state.chats.len(), 2);
        assert!(
            client
                .state
                .outbox
                .last()
                .unwrap()
                .text
                .contains("Choose a Session")
        );
        receive(
            &mut client,
            &message("select", "oc_reopened", "/session resume session-2"),
        )
        .await;
        receive(
            &mut client,
            &message("selected", "oc_reopened_again", "selected conversation"),
        )
        .await;
        assert_eq!(client.state.chats.len(), 3);
        assert_eq!(client.state.chats["oc_reopened"].session, "session-2");
        assert!(!client.state.chats.contains_key("oc_reopened_again"));
        assert_eq!(harness.lock().await.prompts, ["selected conversation"]);
        serving.abort();
    }

    #[tokio::test]
    async fn pending_work_settles_without_input_then_sleeps_idle() {
        use std::os::unix::fs::MetadataExt;
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        harness.lock().await.blocked = true;
        receive(&mut client, &message("pending", "oc_private", "hello")).await;
        let (_incoming, receiver) = mpsc::channel(1);
        let (acknowledgements, _responses) = mpsc::channel(1);
        let (shutdown, closed) = tokio::sync::watch::channel(false);
        let (scheduler, task) = crate::gateway::scheduler::test_scheduler(closed);
        {
            let running = client.run(receiver, acknowledgements, &scheduler);
            tokio::pin!(running);
            tokio::select! {
                result = &mut running => panic!("bridge stopped: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(8), async {
                    harness.lock().await.blocked = false;
                    loop {
                        if harness.lock().await.sent.iter().any(|reply| sent_text(reply) == "final answer") { break; }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let requests = harness.lock().await.requests.len();
                    let state = std::fs::metadata(directory.path().join("state.json")).unwrap();
                    tokio::time::sleep(Duration::from_millis(2200)).await;
                    assert_eq!(harness.lock().await.requests.len(), requests);
                    assert_eq!(std::fs::metadata(directory.path().join("state.json")).unwrap().ino(), state.ino());
                }) => result.unwrap(),
            }
        }
        shutdown.send(true).unwrap();
        task.await.unwrap();
        serving.abort();
    }

    #[tokio::test]
    async fn gateway_restart_restores_bound_history_before_accepting_new_input() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        let first = message("before_restart", "oc_private", "first question");
        receive(&mut client, &first).await;
        client.refresh().await.unwrap();
        client.deliver().await.unwrap();
        let binding = client.state.chats["oc_private"].clone();
        drop(client);
        {
            let mut harness = harness.lock().await;
            harness.saved_directories = std::mem::take(&mut harness.directories);
        }
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        let (_incoming, receiver) = mpsc::channel(1);
        let (acknowledgements, _responses) = mpsc::channel(1);
        let (shutdown, closed) = tokio::sync::watch::channel(false);
        let (scheduler, scheduler_task) = crate::gateway::scheduler::test_scheduler(closed);
        {
            let running = client.run(receiver, acknowledgements, &scheduler);
            tokio::pin!(running);
            tokio::select! {
                result = &mut running => panic!("bridge stopped: {result:?}"),
                _ = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if harness.lock().await.directories.contains_key(&binding.session) { break; }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }) => { assert!(harness.lock().await.directories.contains_key(&binding.session), "bound Session was not restored after restart"); }
            }
        }
        assert_eq!(client.state.chats["oc_private"].session, binding.session);
        assert_eq!(client.state.chats["oc_private"].cursor, binding.cursor);
        shutdown.send(true).unwrap();
        scheduler_task.await.unwrap();
        receive(&mut client, &first).await;
        receive(
            &mut client,
            &message("after_restart", "oc_private", "continue the conversation"),
        )
        .await;
        {
            let harness = harness.lock().await;
            assert_eq!(harness.sessions, 1);
            assert_eq!(
                harness.prompts,
                ["first question", "continue the conversation"]
            );
            assert_eq!(
                harness
                    .requests
                    .iter()
                    .filter(|(path, _)| path == "/api/sessions/resume")
                    .count(),
                1
            );
        }
        assert_eq!(client.state.chats["oc_private"].session, binding.session);
        receive(
            &mut client,
            &message("new_after_restart", "oc_private", "/session new"),
        )
        .await;
        let harness = harness.lock().await;
        assert_eq!(harness.sessions, 2);
        assert!(
            harness
                .requests
                .iter()
                .any(|(path, _)| path == &format!("/api/sessions/{}/new", binding.session))
        );
        assert_eq!(harness.cancellations, 0);
        assert_ne!(client.state.chats["oc_private"].session, binding.session);
        serving.abort();
    }

    #[tokio::test]
    async fn startup_restoration_is_scoped_deduplicated_and_preserves_bindings_on_failure() {
        for failing in [false, true] {
            let (endpoint, harness, serving) = server().await;
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let mut client = bridge(directory.path(), &endpoint);
            for (chat, session, group) in [
                ("oc_private", "original-session", None),
                ("oc_second", "original-session", None),
                ("oc_live", "live-session", None),
                ("oc_denied", "denied-session", Some("oc_denied")),
            ] {
                client.state.chats.insert(
                    chat.into(),
                    Binding {
                        session: session.into(),
                        cursor: Some(format!("cursor-{chat}")),
                        waiting: true,
                        permission_notified: true,
                    },
                );
                client.state.routes.insert(
                    chat.into(),
                    Route {
                        user: "ou_user".into(),
                        group: group.map(str::to_owned),
                    },
                );
            }
            client.prepare().unwrap();
            let previous = serde_json::to_value(&client.state.chats).unwrap();
            {
                let mut harness = harness.lock().await;
                harness
                    .directories
                    .insert("live-session".into(), "/live/project".into());
                harness
                    .saved_directories
                    .insert("original-session".into(), "/original/project".into());
                if failing {
                    harness.failing_path = Some("/api/sessions/resume".into());
                }
            }
            client.restore_sessions().await.unwrap();
            assert_eq!(serde_json::to_value(&client.state.chats).unwrap(), previous);
            let harness = harness.lock().await;
            let restores: Vec<_> = harness
                .requests
                .iter()
                .filter(|(path, _)| path == "/api/sessions/resume")
                .collect();
            assert_eq!(restores.len(), 1);
            assert_eq!(restores[0].1["id"], "original-session");
            assert_eq!(harness.submissions, 0);
            assert_eq!(harness.sessions, 0);
            assert_eq!(
                harness.directories.contains_key("original-session"),
                !failing
            );
            assert_eq!(client.state.outbox.len(), if failing { 2 } else { 0 });
            if failing {
                assert!(
                    client
                        .state
                        .outbox
                        .iter()
                        .all(|delivery| delivery.text.contains("could not be restored"))
                );
            } else {
                assert_eq!(harness.directories["original-session"], "/original/project");
            }
            serving.abort();
        }
    }

    #[tokio::test]
    async fn settled_reply_uses_one_final_card_or_falls_back_to_text_without_replay() {
        for failing_path in [None, Some("/open-apis/cardkit/v1/cards")] {
            let (endpoint, harness, serving) = server().await;
            harness.lock().await.failing_path = failing_path.map(str::to_owned);
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let mut client = bridge(directory.path(), &endpoint);
            client.presenter = Some(Presenter::new(
                client.feishu.clone(),
                PluginClient::test_client(&endpoint),
            ));
            client.prepare().unwrap();
            let payload = message("card", "oc_private", "hello");
            receive(&mut client, &payload).await;
            assert!(harness.lock().await.sent.is_empty());
            client.refresh().await.unwrap();
            client.deliver().await.unwrap();
            receive(&mut client, &payload).await;
            client.refresh().await.unwrap();
            client.deliver().await.unwrap();
            let harness = harness.lock().await;
            assert_eq!(harness.submissions, 1);
            assert_eq!(harness.sent.len(), 1);
            if failing_path.is_some() {
                assert_eq!(sent_text(&harness.sent[0]), "final answer");
            } else {
                let updates: Vec<_> = harness
                    .requests
                    .iter()
                    .filter(|(path, _)| path.ends_with("/elements/content/content"))
                    .collect();
                assert_eq!(updates.last().unwrap().1["content"], "final answer");
                assert!(
                    !serde_json::to_string(&updates)
                        .unwrap()
                        .contains("private reasoning")
                );
                assert!(!serde_json::to_string(&updates).unwrap().contains("hidden"));
            }
            let saved: State = client.store.load("state.json").unwrap().unwrap();
            assert!(!saved.chats["oc_private"].waiting);
            assert_eq!(saved.chats["oc_private"].cursor.as_deref(), Some("reply"));
            serving.abort();
        }
    }

    #[tokio::test]
    async fn private_turn_survives_restart_without_replay_or_internal_output() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let payload = message("one", "oc_private", "hello");
        let help = message("help_once", "oc_private", "/help");
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        receive(&mut client, &help).await;
        client.deliver().await.unwrap();
        assert_eq!(harness.lock().await.sessions, 0);
        receive(&mut client, &payload).await;
        drop(client);
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        receive(&mut client, &payload).await;
        let mut replay: Value = serde_json::from_slice(&payload).unwrap();
        replay["header"]["event_id"] = json!("different-delivery-same-message");
        receive(&mut client, &serde_json::to_vec(&replay).unwrap()).await;
        replay["header"]["event_id"] = json!("event-one");
        replay["event"]["message"]["message_id"] = json!("om_different");
        receive(&mut client, &serde_json::to_vec(&replay).unwrap()).await;
        client.refresh().await.unwrap();
        client.deliver().await.unwrap();
        {
            let harness = harness.lock().await;
            assert_eq!(harness.submissions, 1);
            assert_eq!(harness.sessions, 1);
            let texts: Vec<_> = harness.sent.iter().map(sent_text).collect();
            assert_eq!(texts, [HELP, "final answer"]);
        }
        assert!(!client.state.chats["oc_private"].waiting);
        receive(&mut client, &help).await;
        client.refresh().await.unwrap();
        client.deliver().await.unwrap();
        let texts: Vec<_> = harness.lock().await.sent.iter().map(sent_text).collect();
        assert_eq!(texts, [HELP, "final answer"]);
        serving.abort();
    }

    #[tokio::test]
    async fn group_sessions_stay_separate() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        receive(
            &mut client,
            &message("ignored", "oc_group", "hello without mention"),
        )
        .await;
        assert_eq!(harness.lock().await.submissions, 0);
        receive(
            &mut client,
            &message("group", "oc_group", "@_user_1 hello group"),
        )
        .await;
        let mut second: Value =
            serde_json::from_slice(&message("groupstatus", "oc_group", "@_user_1 /status"))
                .unwrap();
        second["event"]["sender"]["sender_id"]["open_id"] = json!("ou_second");
        receive(&mut client, &serde_json::to_vec(&second).unwrap()).await;
        assert_eq!(harness.lock().await.sessions, 1);
        assert_eq!(harness.lock().await.prompts, ["hello group"]);
        receive(&mut client, &message("stop", "oc_group", "@_user_1 /stop")).await;
        assert_eq!(harness.lock().await.cancellations, 1);
        receive(&mut client, &message("new", "oc_group", "@_user_1 /new")).await;
        assert_eq!(client.state.chats["oc_group"].session, "session-2");
        receive(
            &mut client,
            &message("private", "oc_private", "independent"),
        )
        .await;
        assert_eq!(client.state.chats["oc_private"].session, "session-3");
        assert_eq!(harness.lock().await.cancellations, 1);
        serving.abort();
    }

    #[tokio::test]
    async fn new_uses_client_workspace_without_reading_the_previous_session() {
        for missing in [false, true] {
            let (endpoint, harness, serving) = server().await;
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            let mut client = bridge(directory.path(), &endpoint);
            let mut unbound: Value =
                serde_json::from_slice(&message("unbound", "oc_unbound", "/session new")).unwrap();
            unbound["event"]["sender"]["sender_id"]["open_id"] = json!("ou_second");
            receive(&mut client, &serde_json::to_vec(&unbound).unwrap()).await;
            assert_eq!(
                harness.lock().await.directories["session-1"],
                directory.path().join("workspace").to_str().unwrap()
            );
            receive(
                &mut client,
                &message("custom", "oc_private", "/session new /projects/custom"),
            )
            .await;
            let previous = client.state.chats["oc_private"].session.clone();
            let request_count = {
                let mut harness = harness.lock().await;
                if missing {
                    harness.directories.remove(&previous);
                }
                harness.failing_path = Some(format!("/api/sessions/{previous}/new"));
                harness.requests.len()
            };
            receive(&mut client, &message("default", "oc_private", "/new")).await;
            assert_eq!(client.state.chats["oc_private"].session, "session-3");
            let saved: State = client.store.load("state.json").unwrap().unwrap();
            assert_eq!(saved.chats["oc_private"].session, "session-3");
            let harness = harness.lock().await;
            assert_eq!(
                harness.directories["session-3"],
                directory.path().join("workspace").to_str().unwrap()
            );
            assert_eq!(harness.cancellations, 0);
            assert_eq!(harness.submissions, 0);
            assert_eq!(harness.requests[request_count].0, "/api/sessions");
            assert_eq!(
                harness.requests[request_count].1,
                json!({"workingDir":directory.path().join("workspace")})
            );
            assert!(
                !harness.requests[request_count..]
                    .iter()
                    .any(|(path, _)| path.contains(&previous))
            );
            serving.abort();
        }
    }

    #[tokio::test]
    async fn session_paths_resolve_from_the_client_workspace() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        for (index, path, expected) in [
            (
                0,
                "project with spaces",
                directory.path().join("workspace/project with spaces"),
            ),
            (
                1,
                "/projects/absolute",
                std::path::PathBuf::from("/projects/absolute"),
            ),
            (2, "~/project", dirs::home_dir().unwrap().join("project")),
        ] {
            receive(
                &mut client,
                &message(
                    &format!("path_{index}"),
                    "oc_private",
                    &format!("/session new {path}"),
                ),
            )
            .await;
            let id = &client.state.chats["oc_private"].session;
            assert_eq!(
                harness.lock().await.directories[id],
                expected.to_str().unwrap()
            );
        }
        assert_eq!(harness.lock().await.cancellations, 0);
        serving.abort();
    }

    #[tokio::test]
    async fn session_commands_select_directories_and_resume_without_history_replay() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        receive(
            &mut client,
            &message("inspect_empty", "oc_private", "/session"),
        )
        .await;
        assert!(client.state.chats.is_empty());
        receive(
            &mut client,
            &message("target", "oc_private", "/session new /projects/my project"),
        )
        .await;
        assert_eq!(client.state.chats["oc_private"].session, "session-1");
        receive(
            &mut client,
            &message("inherit", "oc_private", "/session new"),
        )
        .await;
        assert_eq!(client.state.chats["oc_private"].session, "session-2");
        assert_eq!(
            harness.lock().await.directories["session-2"],
            "/projects/my project"
        );
        receive(&mut client, &message("list", "oc_private", "/session list")).await;
        let listing = &client.state.outbox.last().unwrap().text;
        assert!(listing.contains("session-2 [live, current] /projects/my project"));
        assert!(listing.contains("saved-session [saved]"));
        assert_eq!(listing.matches("session-1 [").count(), 1);
        receive(
            &mut client,
            &message("live", "oc_private", "/session resume session-1"),
        )
        .await;
        assert_eq!(client.state.chats["oc_private"].session, "session-1");
        assert_eq!(harness.lock().await.sessions, 2);
        harness.lock().await.submissions = 1;
        receive(
            &mut client,
            &message("saved", "oc_private", "/session resume saved-session"),
        )
        .await;
        assert_eq!(
            client.state.chats["oc_private"].cursor.as_deref(),
            Some("reply")
        );
        assert_eq!(
            harness.lock().await.directories["saved-session"],
            "/saved/project"
        );
        client.refresh().await.unwrap();
        assert!(
            client
                .state
                .outbox
                .iter()
                .all(|delivery| delivery.text != "final answer")
        );
        harness.lock().await.blocked = true;
        receive(
            &mut client,
            &message("active", "oc_private", "/session resume session-1"),
        )
        .await;
        assert!(client.state.chats["oc_private"].waiting);
        client.refresh().await.unwrap();
        assert!(
            client
                .state
                .outbox
                .iter()
                .any(|delivery| delivery.text.contains("needs permission"))
        );
        let restored: State = client.store.load("state.json").unwrap().unwrap();
        assert_eq!(restored.chats["oc_private"].session, "session-1");
        let harness = harness.lock().await;
        assert_eq!(harness.cancellations, 0);
        assert_eq!(harness.submissions, 1);
        assert!(
            harness
                .requests
                .iter()
                .any(|(path, body)| path == "/api/sessions"
                    && body["workingDir"] == "/projects/my project")
        );
        assert!(
            harness
                .requests
                .iter()
                .any(|(path, _)| path == "/api/sessions/session-1/new")
        );
        serving.abort();
    }

    #[tokio::test]
    async fn failed_or_invalid_session_commands_preserve_the_previous_binding() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        receive(&mut client, &message("initial", "oc_private", "/new")).await;
        for (index, (command, failing_path)) in [
            ("/new", "/api/sessions"),
            ("/session new /missing", "/api/sessions"),
            ("/session new", "/api/sessions/session-1/new"),
            ("/session resume saved-session", "/api/sessions/resume"),
            (
                "/session resume saved-session",
                "/api/sessions/saved-session/snapshot",
            ),
            ("/session resume unknown-session", ""),
            ("/session resume ../escape", ""),
            ("/session resume", ""),
            ("/session use session-1", ""),
            ("/session list extra", ""),
        ]
        .into_iter()
        .enumerate()
        {
            harness.lock().await.failing_path = Some(failing_path.into());
            receive(
                &mut client,
                &message(&format!("failed_{index}"), "oc_private", command),
            )
            .await;
            assert_eq!(client.state.chats["oc_private"].session, "session-1");
            assert_eq!(
                client.state.chats["oc_private"].cursor.as_deref(),
                Some("base")
            );
            assert!(!client.state.chats["oc_private"].waiting);
        }
        let harness = harness.lock().await;
        assert_eq!(harness.sessions, 1);
        assert_eq!(harness.submissions, 0);
        assert_eq!(harness.cancellations, 0);
        assert!(
            !harness
                .requests
                .iter()
                .any(|(path, _)| path.contains("escape"))
        );
        serving.abort();
    }

    #[tokio::test]
    async fn session_commands_require_authorized_group_mentions() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        for (index, command) in [
            "/session new /projects/group",
            "/session list",
            "/session resume saved-session",
        ]
        .into_iter()
        .enumerate()
        {
            receive(
                &mut client,
                &message(&format!("unmentioned_{index}"), "oc_group", command),
            )
            .await;
            let mut unauthorized: Value = serde_json::from_slice(&message(
                &format!("unauthorized_{index}"),
                "oc_group",
                &format!("@_user_1 {command}"),
            ))
            .unwrap();
            unauthorized["event"]["sender"]["sender_id"]["open_id"] = json!("ou_stranger");
            receive(&mut client, &serde_json::to_vec(&unauthorized).unwrap()).await;
        }
        assert!(harness.lock().await.requests.is_empty());
        receive(
            &mut client,
            &message(
                "group_new",
                "oc_group",
                "@_user_1 /session new /projects/group",
            ),
        )
        .await;
        receive(
            &mut client,
            &message("private_new", "oc_private", "/session new"),
        )
        .await;
        assert_eq!(client.state.chats["oc_group"].session, "session-1");
        assert_eq!(client.state.chats["oc_private"].session, "session-2");
        assert_eq!(
            harness.lock().await.directories["session-2"],
            directory.path().join("workspace").to_str().unwrap()
        );
        serving.abort();
    }

    #[tokio::test]
    async fn persistence_failure_prevents_gateway_effects_and_policy_removal_drops_outbox() {
        let (endpoint, harness, serving) = server().await;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        client.prepare().unwrap();
        std::fs::set_permissions(
            directory.path().join("state.json"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(
            client
                .claim(
                    &message("fail", "oc_private", "hello"),
                    now_millis().unwrap()
                )
                .is_err()
        );
        assert_eq!(harness.lock().await.sessions, 0);
        std::fs::set_permissions(
            directory.path().join("state.json"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        receive(&mut client, &message("help", "oc_private", "/help")).await;
        client.config.allowed_users.clear();
        client.deliver().await.unwrap();
        assert!(harness.lock().await.sent.is_empty());
        serving.abort();
    }

    #[tokio::test]
    async fn interrupted_submission_and_send_are_reported_not_retried() {
        let (endpoint, harness, serving) = server().await;
        harness.lock().await.delay = true;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let payload = message("crash", "oc_private", "hello");
        let mut client = bridge(directory.path(), &endpoint);
        let event = client
            .claim(&payload, now_millis().unwrap())
            .unwrap()
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), client.apply(event))
                .await
                .is_err()
        );
        assert_eq!(harness.lock().await.submissions, 1);
        drop(client);
        let mut client = bridge(directory.path(), &endpoint);
        assert_eq!(client.state.inflight.as_deref(), Some("oc_private"));
        client.prepare().unwrap();
        receive(&mut client, &payload).await;
        assert_eq!(harness.lock().await.submissions, 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(200), client.deliver())
                .await
                .is_err()
        );
        assert_eq!(harness.lock().await.sent.len(), 1);
        drop(client);
        let mut client = bridge(directory.path(), &endpoint);
        assert_eq!(client.state.sending.as_deref(), Some("oc_private"));
        client.prepare().unwrap();
        harness.lock().await.delay = false;
        client.deliver().await.unwrap();
        let texts: Vec<_> = harness.lock().await.sent.iter().map(sent_text).collect();
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.contains("The bridge stopped"))
                .count(),
            2
        );
        assert_eq!(harness.lock().await.submissions, 1);
        serving.abort();
    }

    #[tokio::test]
    async fn permission_notice_is_scoped_and_never_forwards_tool_input_or_approves() {
        let (endpoint, harness, serving) = server().await;
        harness.lock().await.blocked = true;
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), &endpoint);
        receive(&mut client, &message("permission", "oc_private", "hello")).await;
        client.refresh().await.unwrap();
        client.refresh().await.unwrap();
        client.deliver().await.unwrap();
        let texts: Vec<_> = harness.lock().await.sent.iter().map(sent_text).collect();
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.contains("needs permission"))
                .count(),
            1
        );
        assert!(texts.iter().all(|text| !text.contains("secret input")));
        assert!(client.state.chats["oc_private"].waiting);
        serving.abort();
    }

    #[test]
    fn live_deduplication_claims_are_never_evicted_to_accept_more_work() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let mut client = bridge(directory.path(), "http://127.0.0.1:3141");
        let now = now_millis().unwrap();
        client.state.seen = (0..MAX_SEEN)
            .map(|index| (format!("claimed-{index}"), now))
            .collect();
        let payload = message("capacity", "oc_private", "hello");
        assert!(client.claim(&payload, now).is_err());
        assert_eq!(client.state.seen.len(), MAX_SEEN);
        assert!(client.state.inflight.is_none());
        assert!(client.state.routes.is_empty());
        assert!(client.store.load::<State>("state.json").unwrap().is_none());
        client.state.seen.clear();
        assert!(client.claim(&payload, now).unwrap().is_some());
        let created: Value = serde_json::from_slice(&payload).unwrap();
        let timestamp = created["event"]["message"]["create_time"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(
            client
                .claim(&payload, timestamp + DEDUP_MILLIS)
                .unwrap()
                .is_none()
        );
        let mut fresh = created;
        fresh["header"]["event_id"] = json!("new-event");
        fresh["event"]["message"]["message_id"] = json!("om_new");
        fresh["event"]["message"]["create_time"] = json!((now + DEDUP_MILLIS).to_string());
        assert!(
            client
                .claim(&serde_json::to_vec(&fresh).unwrap(), now + DEDUP_MILLIS)
                .unwrap()
                .is_some()
        );
        assert_eq!(client.state.seen.len(), 2);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]

        #[test]
        fn active_or_unknown_execution_never_settles(
            status in proptest::sample::select(vec!["idle", "failed", "stopped", "running", "requires_action", "unknown"]),
            active in proptest::prelude::any::<bool>(),
            pending in 0..4usize,
        ) {
            let snapshot: Snapshot = serde_json::from_value(json!({
                "version":2,
                "session":{"id":"session","name":"test","provider":"test","model":"test","workingDir":"/tmp"},
                "history":{"entries":[],"leafId":null},
                "execution":{"status":status,"activeTurnId":active.then_some("turn")},
                "pendingSubmissionIds":(0..pending).map(|index| index.to_string()).collect::<Vec<_>>()
            })).unwrap();
            if active || pending > 0 || matches!(status, "running" | "requires_action" | "unknown") {
                proptest::prop_assert!(!execution_settled(&snapshot));
            } else {
                proptest::prop_assert!(execution_settled(&snapshot));
            }
        }
    }
}
