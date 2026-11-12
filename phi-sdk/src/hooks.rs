//! Hook registration and dispatch at lifecycle boundaries.
//! Execution owners apply callback outcomes and advance their own lifecycle.

use crate::core::lifecycle::*;
use futures::future::join_all;
use serde_json::Value;
use std::{
    any::{Any, TypeId},
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

mod command;
pub use command::CommandHooks;

/// Stable owner-qualified identity, independent of handler output.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct HookKey {
    extension: String,
    name: String,
}

impl HookKey {
    fn new(extension: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            extension: extension.into(),
            name: name.into(),
        }
    }
}

/// Plain strings match exact identities; regular expressions are explicit.
#[derive(Debug, Clone)]
pub enum HookMatcher {
    All,
    Exact(String),
    Regex(regex::Regex),
}

impl HookMatcher {
    pub fn exact(value: impl Into<String>) -> Self {
        Self::Exact(value.into())
    }
    pub fn regex(pattern: &str) -> HookResult<Self> {
        regex::Regex::new(pattern)
            .map(Self::Regex)
            .map_err(|error| HookError::new(error.to_string()))
    }
    fn matches(&self, value: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(expected) => expected == value,
            Self::Regex(pattern) => pattern.is_match(value),
        }
    }
}

impl<'de> serde::Deserialize<'de> for HookMatcher {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged, deny_unknown_fields)]
        enum Declaration {
            Text(String),
            Exact { exact: String },
            Regex { regex: String },
        }
        match Declaration::deserialize(deserializer)? {
            Declaration::Text(value) | Declaration::Exact { exact: value } => {
                Ok(if value.is_empty() || value == "*" {
                    Self::All
                } else {
                    Self::Exact(value)
                })
            }
            Declaration::Regex { regex } => Self::regex(&regex).map_err(serde::de::Error::custom),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub(crate) enum HookCondition {
    Equals {
        field: String,
        equals: Value,
    },
    Regex {
        field: String,
        #[serde(deserialize_with = "deserialize_regex")]
        regex: regex::Regex,
    },
}

fn deserialize_regex<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<regex::Regex, D::Error> {
    let pattern = <String as serde::Deserialize>::deserialize(deserializer)?;
    regex::Regex::new(&pattern).map_err(serde::de::Error::custom)
}

impl HookCondition {
    fn field(&self) -> &str {
        match self {
            Self::Equals { field, .. } | Self::Regex { field, .. } => field,
        }
    }
    fn matches(&self, payload: &Value) -> bool {
        let value = self
            .field()
            .split('.')
            .try_fold(payload, |value, field| value.get(field));
        match self {
            Self::Equals { equals, .. } => value.is_some_and(|value| value == equals),
            Self::Regex { regex, .. } => value
                .and_then(Value::as_str)
                .is_some_and(|value| regex.is_match(value)),
        }
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct HookOptions {
    id: Option<String>,
    once: bool,
    #[serde(rename = "if")]
    condition: Option<HookCondition>,
    timeout: Option<u64>,
}

type TypedCondition = (TypeId, Arc<dyn Fn(&dyn Any) -> bool + Send + Sync>);

#[derive(Clone, Default)]
pub(crate) struct HookControl {
    key: Option<HookKey>,
    once: bool,
    matcher: Option<HookMatcher>,
    condition: Option<HookCondition>,
    typed_condition: Option<TypedCondition>,
    start_mode: Option<SessionStartMode>,
    timeout: Option<Duration>,
    allowance: Arc<Mutex<Arc<AtomicBool>>>,
}

impl HookControl {
    pub(crate) fn validate_identities<'a>(
        controls: impl IntoIterator<Item = &'a Self>,
    ) -> HookResult<()> {
        let mut identities = BTreeSet::new();
        for control in controls {
            if let Some(identity) = control.identity()
                && !identities.insert(identity.clone())
            {
                return Err(HookError::new(format!(
                    "duplicate hook registration: {identity:?}"
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn from_options(
        owner: &str,
        event: &str,
        options: HookOptions,
        matcher: Option<HookMatcher>,
    ) -> HookResult<Self> {
        let control = Self {
            key: options.id.map(|name| HookKey::new(owner, name)),
            once: options.once,
            matcher,
            condition: options.condition,
            timeout: options.timeout.map(Duration::from_secs),
            ..Self::default()
        };
        control.validate(event, None)?;
        Ok(control)
    }

    pub(crate) fn with_default_id(mut self, owner: &str, event: &str, index: usize) -> Self {
        self.key
            .get_or_insert_with(|| HookKey::new(owner, format!("{event}:{index}")));
        self
    }

    fn fresh(&self) -> Self {
        Self {
            allowance: Arc::default(),
            ..self.clone()
        }
    }
    fn identity(&self) -> Option<HookKey> {
        self.key.clone()
    }
    fn inherit(&self, previous: &Self) {
        let previous = previous.allowance.lock().unwrap().clone();
        let mut current = self.allowance.lock().unwrap();
        if current.load(Ordering::SeqCst) {
            previous.store(true, Ordering::SeqCst);
        }
        *current = previous;
    }
    fn consume(&self) -> bool {
        !self.once || !self.allowance.lock().unwrap().swap(true, Ordering::SeqCst)
    }
    fn target_matches(&self, target: Option<&str>) -> bool {
        self.matcher.as_ref().is_none_or(|matcher| {
            matches!(matcher, HookMatcher::All)
                || target.is_some_and(|target| matcher.matches(target))
        })
    }
    fn start<I: HookInput>(&self, input: &I) -> bool {
        self.target_matches(input.target())
            && self
                .typed_condition
                .as_ref()
                .is_none_or(|(_, condition)| condition(input))
            && self.consume()
    }
    pub(crate) fn start_adapter(&self, event: &LifecycleEvent) -> bool {
        self.start_payload(adapter_target(event), &event.payload)
    }
    pub(crate) fn start_payload(&self, target: Option<&str>, payload: &Value) -> bool {
        self.target_matches(target)
            && self
                .condition
                .as_ref()
                .is_none_or(|condition| condition.matches(payload))
            && self.consume()
    }
    pub(crate) fn timeout(&self, default: Duration) -> Duration {
        self.timeout.unwrap_or(default)
    }
    fn validate(&self, event: &str, input_type: Option<TypeId>) -> HookResult<()> {
        let fields = event_fields(event)
            .ok_or_else(|| HookError::new(format!("unsupported hook event: {event}")))?;
        if self
            .key
            .as_ref()
            .is_some_and(|key| key.extension.trim().is_empty() || key.name.trim().is_empty())
        {
            return Err(HookError::new("hook owner and identity must not be empty"));
        }
        if self
            .matcher
            .as_ref()
            .is_some_and(|matcher| !matches!(matcher, HookMatcher::All))
            && !has_match_target(event)
        {
            return Err(HookError::new(format!("{event} has no matcher target")));
        }
        if let Some(condition) = &self.condition {
            let root = condition.field().split('.').next().unwrap_or_default();
            let nested_supported = !condition.field().contains('.')
                || matches!(
                    root,
                    "tool_input" | "tool_response" | "transcript" | "context" | "task" | "delta"
                );
            if !fields.contains(&root)
                || !nested_supported
                || condition.field().split('.').any(str::is_empty)
            {
                return Err(HookError::new(format!(
                    "unsupported {event} condition field: {}",
                    condition.field()
                )));
            }
        }
        if self
            .typed_condition
            .as_ref()
            .is_some_and(|(expected, _)| Some(*expected) != input_type)
        {
            return Err(HookError::new(format!(
                "condition input does not match {event}"
            )));
        }
        if self.timeout.is_some_and(|timeout| timeout.is_zero()) {
            return Err(HookError::new("hook timeout must be positive"));
        }
        Ok(())
    }
}

fn event_fields(event: &str) -> Option<&'static [&'static str]> {
    Some(match event {
        "SessionStart" => &["reason", "source", "previous_session_file"],
        "SessionEnd" => &["reason", "target_session_file"],
        "PreToolUse" => &["tool_name", "tool_use_id", "tool_input"],
        "PermissionRequest" | "PermissionDenied" => &["tool_name", "tool_input"],
        "PostToolUse" => &[
            "tool_name",
            "tool_use_id",
            "tool_input",
            "tool_response",
            "duration_ms",
        ],
        "PostToolUseFailure" => &[
            "tool_name",
            "tool_use_id",
            "tool_input",
            "error",
            "is_interrupt",
            "duration_ms",
        ],
        "Notification" => &["notification_type", "level", "message", "tool_name"],
        "UserPromptExpansion" => &[
            "expansion_type",
            "prompt",
            "command_name",
            "command_args",
            "command_source",
        ],
        "UserPromptSubmit" => &[
            "original_prompt",
            "prompt",
            "source",
            "dispatch",
            "delegated_child",
        ],
        "PromptContext" => &[
            "original_prompt",
            "prompt",
            "source",
            "dispatch",
            "working_dir",
            "delegated_child",
            "transcript",
        ],
        "BuildSystemPrompt" => &["system_prompt", "cwd"],
        "Stop" => &["response", "continuations", "stop_hook_active"],
        "AgentStart" | "AgentEnd" => &[
            "session_id",
            "agent_id",
            "parent_agent_id",
            "parent_tool_call_id",
        ],
        "AgentTurnStart" | "AgentTurnEnd" => &["turn_id", "index"],
        "context" | "BeforeProviderRequest" | "AfterProviderResponse" => &["turn_id"],
        "StopFailure" => &["error"],
        "PreCompact" => &["message_count"],
        "PostCompact" => &["message_count", "summary"],
        "PostToolBatch" => &["tool_calls"],
        "MessageDisplay" => &["turn_id", "message_id", "index", "final", "delta"],
        "TaskCreated" => &["context", "task"],
        "TaskCompleted" => &["context", "task", "trigger"],
        "TeammateIdle" => &["context"],
        "resources_discover" => &["reason", "cwd"],
        "session_tree" => &[
            "reason",
            "target_id",
            "old_leaf_id",
            "new_leaf_id",
            "editor_text",
        ],
        "command_before" => &["command"],
        "command_after" => &["command", "handled"],
        "ui_render" => &[],
        _ => return None,
    })
}

fn has_match_target(event: &str) -> bool {
    matches!(
        event,
        "SessionStart"
            | "SessionEnd"
            | "PreToolUse"
            | "PostToolUse"
            | "PostToolUseFailure"
            | "PermissionRequest"
            | "PermissionDenied"
            | "Notification"
            | "UserPromptExpansion"
            | "resources_discover"
            | "session_tree"
            | "command_before"
            | "command_after"
    )
}

fn adapter_target(event: &LifecycleEvent) -> Option<&str> {
    let field = match event.kind {
        LifecycleEventKind::SessionStart
        | LifecycleEventKind::SessionEnd
        | LifecycleEventKind::ResourcesDiscover
        | LifecycleEventKind::SessionTree => "reason",
        LifecycleEventKind::PreToolUse
        | LifecycleEventKind::PostToolUse
        | LifecycleEventKind::PostToolUseFailure
        | LifecycleEventKind::PermissionRequest
        | LifecycleEventKind::PermissionDenied => "tool_name",
        LifecycleEventKind::Notification => "notification_type",
        LifecycleEventKind::UserPromptExpansion => "command_name",
        LifecycleEventKind::CommandBefore | LifecycleEventKind::CommandAfter => "command",
        _ => return None,
    };
    event.payload.get(field).and_then(Value::as_str)
}

trait HookInput: Any {
    fn target(&self) -> Option<&str> {
        None
    }
}
macro_rules! plain_hook_inputs { ($($input:ty),* $(,)?) => { $(impl HookInput for $input {})* }; }
plain_hook_inputs!(
    PromptContextEvent,
    CompletionEvent,
    TaskCreatedEvent,
    TaskCompletedEvent,
    TeammateIdleEvent,
    BuildSystemPromptEvent,
    UserPromptSubmitEvent,
    ProviderContextEvent,
    AgentRunEvent,
    AgentTurnEvent,
    AgentTurnEndEvent
);
impl HookInput for ToolUseEvent {
    fn target(&self) -> Option<&str> {
        Some(&self.tool_name)
    }
}
impl HookInput for ToolResultEvent {
    fn target(&self) -> Option<&str> {
        Some(&self.tool_name)
    }
}
impl HookInput for ToolFailureEvent {
    fn target(&self) -> Option<&str> {
        Some(&self.tool_name)
    }
}
impl HookInput for NotificationEvent {
    fn target(&self) -> Option<&str> {
        Some(self.kind.as_str())
    }
}
impl HookInput for UserPromptExpansionEvent {
    fn target(&self) -> Option<&str> {
        Some(&self.command_name)
    }
}
fn start_reason(reason: SessionStartReason) -> &'static str {
    match reason {
        SessionStartReason::Startup => "startup",
        SessionStartReason::Reload => "reload",
        SessionStartReason::New => "new",
        SessionStartReason::Resume => "resume",
        SessionStartReason::Clear => "clear",
        SessionStartReason::Compact => "compact",
    }
}
impl HookInput for SessionStartEvent {
    fn target(&self) -> Option<&str> {
        Some(start_reason(self.reason))
    }
}
impl HookInput for SessionEndEvent {
    fn target(&self) -> Option<&str> {
        Some(self.reason.as_str())
    }
}

pub use crate::core::lifecycle::HookNotice;

/// A callback failure, distinct from an event's explicit gate outcome.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct HookError {
    message: String,
}

impl HookError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<String> for HookError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for HookError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Callback result; failure policy belongs to the lifecycle boundary.
pub type HookResult<T> = Result<T, HookError>;

#[derive(Default)]
struct StartupTasks {
    tasks: VecDeque<tokio::task::JoinHandle<HookResult<Vec<HookNotice>>>>,
}

impl Drop for StartupTasks {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub(crate) struct SystemPromptPreparation {
    event: BuildSystemPromptEvent,
    prompt: tokio::sync::OnceCell<String>,
}

impl SystemPromptPreparation {
    pub(crate) fn new(event: BuildSystemPromptEvent) -> Self {
        Self {
            event,
            prompt: tokio::sync::OnceCell::new(),
        }
    }
    pub(crate) async fn finish(&self, hooks: &HookRegistry) -> String {
        self.prompt
            .get_or_init(|| crate::context::finalize_system_prompt(hooks, self.event.clone()))
            .await
            .clone()
    }
}

#[derive(Default)]
struct HookRuntime {
    system_prompt: Mutex<Weak<SystemPromptPreparation>>,
    startup: tokio::sync::Mutex<StartupTasks>,
    notices: Mutex<Vec<HookNotice>>,
    notifications: Mutex<NotificationTasks>,
}

#[derive(Default)]
struct NotificationTasks {
    tasks: VecDeque<tokio::task::JoinHandle<()>>,
}

impl NotificationTasks {
    async fn finish(mut self) {
        while let Some(task) = self.tasks.front_mut() {
            let _ = task.await;
            self.tasks.pop_front();
        }
    }
}

impl Drop for NotificationTasks {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl HookRegistry {
    pub(crate) fn for_generation(&self) -> Self {
        Self {
            registrations: self
                .registrations
                .iter()
                .filter(|registration| match registration {
                    HookRegistration::Callback(_) => true,
                    HookRegistration::Handler(handler) => !handler.is_generation_bound(),
                })
                .map(|registration| match registration {
                    HookRegistration::Callback(hook) => HookRegistration::Callback(Hook {
                        control: hook.control.fresh(),
                        ..hook.clone()
                    }),
                    registration => registration.clone(),
                })
                .collect(),
            runtime: Arc::default(),
            timeouts: self.timeouts,
        }
    }

    pub(crate) fn continue_bindings(&self, previous: &Self) {
        let previous: BTreeMap<_, _> = previous
            .registrations
            .iter()
            .filter_map(|registration| {
                let control = registration.control()?;
                Some((control.identity()?, control))
            })
            .collect();
        for registration in &self.registrations {
            if let Some(control) = registration.control()
                && let Some(previous) = control
                    .identity()
                    .and_then(|identity| previous.get(&identity))
            {
                control.inherit(previous);
            }
        }
    }

    pub(crate) fn record_notices(&self, notices: impl IntoIterator<Item = HookNotice>) {
        self.runtime.notices.lock().unwrap().extend(notices);
    }

    pub(crate) fn take_notices(&self) -> Vec<HookNotice> {
        std::mem::take(&mut *self.runtime.notices.lock().unwrap())
    }

    fn record_hook_error(&self, kind: &str, error: &impl fmt::Display) {
        let message = format!("{kind} hook failed: {error}");
        tracing::warn!(kind, %error, "lifecycle hook failed");
        self.record_notices([HookNotice::Warning { message }]);
    }

    fn record_hook_result(&self, result: HookResult<Vec<HookNotice>>) {
        self.record_notices(result.unwrap_or_else(|error| {
            vec![HookNotice::Warning {
                message: error.to_string(),
            }]
        }));
    }

    fn task_snapshot(&self) -> Self {
        Self {
            registrations: self.registrations.clone(),
            runtime: Arc::default(),
            timeouts: self.timeouts,
        }
    }

    pub(crate) fn schedule_notification(&self, event: NotificationEvent) {
        let snapshot = self.task_snapshot();
        let runtime = Arc::downgrade(&self.runtime);
        let mut notifications = self.runtime.notifications.lock().unwrap();
        notifications.tasks.retain(|task| !task.is_finished());
        notifications.tasks.push_back(tokio::spawn(async move {
            emit_notification(&snapshot, event).await;
            if let Some(runtime) = runtime.upgrade() {
                runtime
                    .notices
                    .lock()
                    .unwrap()
                    .extend(snapshot.take_notices());
            }
        }));
    }

    #[cfg(test)]
    async fn flush_notifications(&self) {
        let pending = std::mem::take(&mut *self.runtime.notifications.lock().unwrap());
        pending.finish().await;
    }

    pub(crate) fn defer_system_prompt(&self, preparation: &Arc<SystemPromptPreparation>) {
        *self.runtime.system_prompt.lock().unwrap() = Arc::downgrade(preparation);
    }

    pub(crate) async fn ready(&self) {
        self.await_startup().await;
        let preparation = self.runtime.system_prompt.lock().unwrap().upgrade();
        if let Some(preparation) = preparation {
            preparation.finish(self).await;
        }
    }

    async fn await_startup(&self) {
        let mut startup = self.runtime.startup.lock().await;
        while let Some(task) = startup.tasks.front_mut() {
            let result = task.await;
            startup.tasks.pop_front();
            let result = result
                .map_err(|error| HookError::new(format!("startup hook task failed: {error}")))
                .and_then(|result| result);
            self.record_hook_result(result);
        }
    }

    pub(crate) async fn prepare_context(
        &self,
        event: PromptContextEvent,
    ) -> HookResult<PromptContextOutcome> {
        self.validate_registrations()?;
        let input = LifecycleEvent::with_payload(
            LifecycleEventKind::PromptContext,
            serde_json::json!({
                "original_prompt": event.original_prompt, "prompt": event.prompt,
                "source": event.source.as_str(), "dispatch": event.dispatch.as_str(),
                "transcript": event.transcript, "working_dir": event.working_dir, "delegated_child": event.delegated_child,
            }),
        );
        let results = join_all(self.registrations.iter().map(|registration| async {
            match registration {
                HookRegistration::Callback(registration) => {
                    let RegisteredHook::PromptContext(hook) = &registration.hook else {
                        return Ok(Vec::new());
                    };
                    registration
                        .invoke(hook, event.clone(), PromptContextOutcome::default())
                        .await
                        .map(|outcome| vec![outcome])
                }
                HookRegistration::Handler(handler) => {
                    let result = run_handler(handler.as_ref(), &input, self.timeouts.hook_timeout)
                        .await
                        .map_err(|error| HookError::new(error.to_string()))?;
                    let Some(result) = result else {
                        return Ok(Vec::new());
                    };
                    if result.should_stop() {
                        return Err(HookError::new(
                            result
                                .reason
                                .unwrap_or_else(|| "context preparation blocked".into()),
                        ));
                    }
                    result.into_payload().map_or_else(
                        || Ok(Vec::new()),
                        |payload| {
                            serde_json::from_value::<Vec<PromptContextOutcome>>(payload).map_err(
                                |error| {
                                    HookError::new(format!("invalid PromptContext output: {error}"))
                                },
                            )
                        },
                    )
                }
            }
        }))
        .await;
        let mut merged = PromptContextOutcome::default();
        for result in results {
            for outcome in result? {
                merged.contexts.extend(
                    outcome
                        .contexts
                        .into_iter()
                        .filter(|context| !context.content.is_empty()),
                );
                merged.notices.extend(outcome.notices);
            }
        }
        Ok(merged)
    }

    pub(crate) fn validate_registrations(&self) -> HookResult<()> {
        for registration in &self.registrations {
            if let HookRegistration::Callback(hook) = registration {
                let (event, input_type) = hook.hook.contract();
                hook.control.validate(event, Some(input_type))?;
            }
        }
        HookControl::validate_identities(
            self.registrations
                .iter()
                .filter_map(HookRegistration::control),
        )
    }
}

const MAX_HOOK_FEEDBACK_BYTES: usize = 16 * 1024;

fn validate_hook_feedback(feedback: &str, boundary: &str) -> HookResult<()> {
    if feedback.trim().is_empty() || feedback.len() > MAX_HOOK_FEEDBACK_BYTES {
        return Err(HookError::new(format!(
            "{boundary} feedback must be nonempty and at most 16 KiB"
        )));
    }
    Ok(())
}

impl TeammateIdleOutcome {
    fn validate(&self) -> HookResult<()> {
        if let Self::Continue { feedback } = self {
            validate_hook_feedback(feedback, "TeammateIdle")?;
        }
        Ok(())
    }
}

pub(crate) type TeammateIdleFuture<'a> =
    Pin<Box<dyn Future<Output = HookResult<TeammateIdleOutcome>> + Send + 'a>>;

pub(crate) type StopHookFuture<'a> =
    Pin<Box<dyn Future<Output = HookResult<CompletionOutcome>> + Send + 'a>>;

pub(crate) type TaskGateFuture<'a> =
    Pin<Box<dyn Future<Output = HookResult<TaskGateOutcome>> + Send + 'a>>;

type HookFuture<T> = Pin<Box<dyn Future<Output = HookResult<T>> + Send>>;
type HookCallback<I, O> = dyn Fn(I, Duration) -> HookFuture<O> + Send + Sync;

/// Default callback timeout and the independent total SessionEnd budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HookTimeouts {
    /// Default callback deadline; an individual hook may replace it.
    pub hook_timeout: Duration,
    /// Total budget for end hooks and notification drain.
    pub session_end_timeout: Duration,
}

impl Default for HookTimeouts {
    fn default() -> Self {
        Self {
            hook_timeout: DEFAULT_HOOK_TIMEOUT,
            session_end_timeout: DEFAULT_SESSION_END_TIMEOUT,
        }
    }
}

/// When SessionStart completion is required. Neither mode is fire-and-forget.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStartMode {
    /// Finish during the SessionStart operation.
    #[default]
    Blocking,
    /// Start now; readiness and prompt preparation await completion.
    BeforeFirstPrompt,
}

#[derive(Clone)]
enum RegisteredHook {
    Notification(Arc<HookCallback<NotificationEvent, ()>>),
    Stop(Arc<HookCallback<CompletionEvent, CompletionOutcome>>),
    TaskCreated(Arc<HookCallback<TaskCreatedEvent, TaskGateOutcome>>),
    TaskCompleted(Arc<HookCallback<TaskCompletedEvent, TaskGateOutcome>>),
    TeammateIdle(Arc<HookCallback<TeammateIdleEvent, TeammateIdleOutcome>>),
    SessionStart(Arc<HookCallback<SessionStartEvent, Vec<HookNotice>>>),
    PromptContext(Arc<HookCallback<PromptContextEvent, PromptContextOutcome>>),
    SessionEnd(Arc<HookCallback<SessionEndEvent, Vec<HookNotice>>>),
    BuildSystemPrompt(Arc<HookCallback<BuildSystemPromptEvent, BuildSystemPromptOutcome>>),
    UserPromptSubmit(Arc<HookCallback<UserPromptSubmitEvent, UserPromptSubmitOutcome>>),
    UserPromptExpansion(Arc<HookCallback<UserPromptExpansionEvent, UserPromptExpansionOutcome>>),
    ProviderContext(Arc<HookCallback<ProviderContextEvent, ProviderContextOutcome>>),
    AgentStart(Arc<HookCallback<AgentRunEvent, ()>>),
    AgentEnd(Arc<HookCallback<AgentRunEvent, ()>>),
    AgentTurnStart(Arc<HookCallback<AgentTurnEvent, ()>>),
    AgentTurnEnd(Arc<HookCallback<AgentTurnEndEvent, AgentTurnEndOutcome>>),
    PreToolUse(Arc<HookCallback<ToolUseEvent, PreToolUseOutcome>>),
    PostToolUse(Arc<HookCallback<ToolResultEvent, PostToolUseOutcome>>),
    PostToolFailure(Arc<HookCallback<ToolFailureEvent, PostToolFailureOutcome>>),
}

/// One typed callback registration with an optional timeout.
#[derive(Clone)]
pub struct Hook {
    hook: RegisteredHook,
    control: HookControl,
}

impl Hook {
    fn new(hook: RegisteredHook) -> Self {
        Self {
            hook,
            control: HookControl::default(),
        }
    }

    /// Consumes one invocation at execution start, regardless of its outcome.
    pub fn with_once(mut self, once: bool) -> Self {
        self.control.once = once;
        self
    }

    /// Names a registration across reloads; unnamed registrations use their position.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.control.key = Some(HookKey::new("rust", id));
        self
    }

    /// Adds a typed condition; a mismatched event type fails validation.
    pub fn when<E: 'static>(
        mut self,
        matches: impl Fn(&E) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.control.typed_condition = Some((
            TypeId::of::<E>(),
            Arc::new(move |input| input.downcast_ref::<E>().is_some_and(&matches)),
        ));
        self
    }

    pub fn with_matcher(mut self, matcher: HookMatcher) -> Self {
        self.control.matcher = Some(matcher);
        self
    }
    async fn invoke<I: HookInput, O>(
        &self,
        callback: &Arc<HookCallback<I, O>>,
        input: I,
        skipped: O,
    ) -> HookResult<O> {
        if !self.control.start(&input) {
            return Ok(skipped);
        }
        callback(input, self.control.timeout(DEFAULT_HOOK_TIMEOUT)).await
    }

    /// Overrides the callback deadline, not the SessionEnd budget.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.control.timeout = Some(timeout);
        self
    }

    /// Observes Session initialization with explicit readiness timing.
    pub fn session_start_with_mode<F, Fut>(mode: SessionStartMode, hook: F) -> Self
    where
        F: Fn(SessionStartEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<Vec<HookNotice>>> + Send + 'static,
    {
        let mut registration = Self::new(RegisteredHook::SessionStart(box_hook(hook)));
        registration.control.start_mode = Some(mode);
        registration
    }

    /// Prepares post-transform context; contributions commit with admission.
    pub fn prompt_context<F, Fut>(hook: F) -> Self
    where
        F: Fn(PromptContextEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<PromptContextOutcome>> + Send + 'static,
    {
        Self::prompt_context_with_recovery(hook, |_, error| Err(error))
    }

    /// Applies domain recovery after callback errors or the configured timeout.
    pub fn prompt_context_with_recovery<F, Fut, R>(hook: F, recover: R) -> Self
    where
        F: Fn(PromptContextEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<PromptContextOutcome>> + Send + 'static,
        R: Fn(PromptContextEvent, HookError) -> HookResult<PromptContextOutcome>
            + Send
            + Sync
            + 'static,
    {
        let hook = box_hook(hook);
        let recover = Arc::new(recover);
        let callback = Arc::new(
            move |event: PromptContextEvent, timeout| -> HookFuture<PromptContextOutcome> {
                let hook = hook.clone();
                let recover = recover.clone();
                Box::pin(async move {
                    hook(event.clone(), timeout)
                        .await
                        .or_else(|error| recover(event, error))
                })
            },
        );
        Self::new(RegisteredHook::PromptContext(callback))
    }

    /// Observes notifications without gating execution.
    pub fn notification<F, Fut>(hook: F) -> Self
    where
        F: Fn(NotificationEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<()>> + Send + 'static,
    {
        Self::new(RegisteredHook::Notification(box_hook(hook)))
    }

    /// Gates response completion, not Agent shutdown.
    pub fn stop<F, Fut>(hook: F) -> Self
    where
        F: Fn(CompletionEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<CompletionOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::Stop(box_hook(hook)))
    }

    /// Gates a managed teammate's transition to idle.
    pub fn teammate_idle<F, Fut>(hook: F) -> Self
    where
        F: Fn(TeammateIdleEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<TeammateIdleOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::TeammateIdle(box_hook(hook)))
    }

    /// Gates checklist Task creation, not Agent creation.
    pub fn task_created<F, Fut>(hook: F) -> Self
    where
        F: Fn(TaskCreatedEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<TaskGateOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::TaskCreated(box_hook(hook)))
    }

    /// Gates Task completion or teammate end-turn validation.
    pub fn task_completed<F, Fut>(hook: F) -> Self
    where
        F: Fn(TaskCompletedEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<TaskGateOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::TaskCompleted(box_hook(hook)))
    }

    /// Observes Session initialization with blocking readiness.
    pub fn session_start<F, Fut>(hook: F) -> Self
    where
        F: Fn(SessionStartEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<Vec<HookNotice>>> + Send + 'static,
    {
        Self::session_start_with_mode(SessionStartMode::Blocking, hook)
    }

    /// Observes Session stop/replacement, not detach.
    pub fn session_end<F, Fut>(hook: F) -> Self
    where
        F: Fn(SessionEndEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<Vec<HookNotice>>> + Send + 'static,
    {
        Self::new(RegisteredHook::SessionEnd(box_hook(hook)))
    }

    /// Transforms the complete runtime system prompt.
    pub fn build_system_prompt<F, Fut>(hook: F) -> Self
    where
        F: Fn(BuildSystemPromptEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<BuildSystemPromptOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::BuildSystemPrompt(box_hook(hook)))
    }

    /// Transforms or handles input before admission.
    pub fn user_prompt_submit<F, Fut>(hook: F) -> Self
    where
        F: Fn(UserPromptSubmitEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<UserPromptSubmitOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::UserPromptSubmit(box_hook(hook)))
    }

    /// Gates a recognized command before application-owned expansion.
    pub fn user_prompt_expansion<F, Fut>(hook: F) -> Self
    where
        F: Fn(UserPromptExpansionEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<UserPromptExpansionOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::UserPromptExpansion(box_hook(hook)))
    }

    /// Adds request-local context without changing history.
    pub fn provider_context<F, Fut>(hook: F) -> Self
    where
        F: Fn(ProviderContextEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<ProviderContextOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::ProviderContext(box_hook(hook)))
    }

    /// Observes run start, not AgentInstance creation.
    pub fn agent_start<F, Fut>(hook: F) -> Self
    where
        F: Fn(AgentRunEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<()>> + Send + 'static,
    {
        Self::new(RegisteredHook::AgentStart(box_hook(hook)))
    }

    /// Observes runner exit; cannot reject completion.
    pub fn agent_end<F, Fut>(hook: F) -> Self
    where
        F: Fn(AgentRunEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<()>> + Send + 'static,
    {
        Self::new(RegisteredHook::AgentEnd(box_hook(hook)))
    }

    /// Observes inner round start; retries remain in the round.
    pub fn agent_turn_start<F, Fut>(hook: F) -> Self
    where
        F: Fn(AgentTurnEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<()>> + Send + 'static,
    {
        Self::new(RegisteredHook::AgentTurnStart(box_hook(hook)))
    }

    /// Revises future model context after the inner round's messages are committed.
    pub fn agent_turn_end<F, Fut>(hook: F) -> Self
    where
        F: Fn(AgentTurnEndEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<AgentTurnEndOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::AgentTurnEnd(box_hook(hook)))
    }

    /// Gates/rewrites a tool call without bypassing authorization.
    pub fn pre_tool_use<F, Fut>(hook: F) -> Self
    where
        F: Fn(ToolUseEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<PreToolUseOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::PreToolUse(box_hook(hook)))
    }

    /// Observes/rewrites successful tool output, never its effects.
    pub fn post_tool_use<F, Fut>(hook: F) -> Self
    where
        F: Fn(ToolResultEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<PostToolUseOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::PostToolUse(box_hook(hook)))
    }

    /// Observes/rewrites tool failure without retrying.
    pub fn post_tool_failure<F, Fut>(hook: F) -> Self
    where
        F: Fn(ToolFailureEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = HookResult<PostToolFailureOutcome>> + Send + 'static,
    {
        Self::new(RegisteredHook::PostToolFailure(box_hook(hook)))
    }
}

impl RegisteredHook {
    fn contract(&self) -> (&'static str, TypeId) {
        match self {
            Self::Notification(_) => ("Notification", TypeId::of::<NotificationEvent>()),
            Self::Stop(_) => ("Stop", TypeId::of::<CompletionEvent>()),
            Self::TaskCreated(_) => ("TaskCreated", TypeId::of::<TaskCreatedEvent>()),
            Self::TaskCompleted(_) => ("TaskCompleted", TypeId::of::<TaskCompletedEvent>()),
            Self::TeammateIdle(_) => ("TeammateIdle", TypeId::of::<TeammateIdleEvent>()),
            Self::SessionStart(_) => ("SessionStart", TypeId::of::<SessionStartEvent>()),
            Self::SessionEnd(_) => ("SessionEnd", TypeId::of::<SessionEndEvent>()),
            Self::PromptContext(_) => ("PromptContext", TypeId::of::<PromptContextEvent>()),
            Self::BuildSystemPrompt(_) => {
                ("BuildSystemPrompt", TypeId::of::<BuildSystemPromptEvent>())
            }
            Self::UserPromptSubmit(_) => {
                ("UserPromptSubmit", TypeId::of::<UserPromptSubmitEvent>())
            }
            Self::UserPromptExpansion(_) => (
                "UserPromptExpansion",
                TypeId::of::<UserPromptExpansionEvent>(),
            ),
            Self::ProviderContext(_) => ("context", TypeId::of::<ProviderContextEvent>()),
            Self::AgentStart(_) => ("AgentStart", TypeId::of::<AgentRunEvent>()),
            Self::AgentEnd(_) => ("AgentEnd", TypeId::of::<AgentRunEvent>()),
            Self::AgentTurnStart(_) => ("AgentTurnStart", TypeId::of::<AgentTurnEvent>()),
            Self::AgentTurnEnd(_) => ("AgentTurnEnd", TypeId::of::<AgentTurnEndEvent>()),
            Self::PreToolUse(_) => ("PreToolUse", TypeId::of::<ToolUseEvent>()),
            Self::PostToolUse(_) => ("PostToolUse", TypeId::of::<ToolResultEvent>()),
            Self::PostToolFailure(_) => ("PostToolUseFailure", TypeId::of::<ToolFailureEvent>()),
        }
    }
}

impl fmt::Debug for Hook {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Hook").finish_non_exhaustive()
    }
}

async fn run_handler(
    handler: &dyn HookHandler,
    event: &LifecycleEvent,
    timeout: Duration,
) -> anyhow::Result<Option<HookResponse>> {
    let result = match handler.timeout(timeout) {
        Some(timeout) => tokio::time::timeout(timeout, handler.handle(event))
            .await
            .map_err(|_| anyhow::anyhow!("{} hook timed out", event.kind.as_str()))?,
        None => handler.handle(event).await,
    }?;
    anyhow::ensure!(
        event.kind != LifecycleEventKind::Notification
            || !result.as_ref().is_some_and(HookResponse::should_stop),
        "Notification cannot control execution"
    );
    Ok(result)
}

fn box_hook<I, O: 'static, F, Fut>(hook: F) -> Arc<HookCallback<I, O>>
where
    F: Fn(I) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = HookResult<O>> + Send + 'static,
{
    Arc::new(move |input, timeout| Box::pin(run_hook(hook(input), timeout)))
}

async fn run_hook<T>(
    future: impl Future<Output = HookResult<T>>,
    timeout: Duration,
) -> HookResult<T> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| HookError::new("lifecycle hook timed out"))?
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub(crate) enum HookOutput {
    Payload(Value),
    Action(HookAction),
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HookResponse {
    pub cancel: bool,
    pub block: bool,
    pub reason: Option<String>,
    pub output: Option<HookOutput>,
}

impl HookResponse {
    pub fn should_stop(&self) -> bool {
        self.cancel || self.block
    }

    pub fn into_payload(self) -> Option<Value> {
        match self.output {
            Some(HookOutput::Payload(payload)) => Some(payload),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn into_action(self) -> Option<HookAction> {
        match self.output {
            Some(HookOutput::Action(action)) => Some(action),
            _ => None,
        }
    }
}

pub(crate) type HookHandlerFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Option<HookResponse>>> + Send + 'a>>;

pub(crate) trait HookHandler: Send + Sync {
    fn control(&self) -> Option<&HookControl> {
        None
    }
    fn handle<'a>(&'a self, event: &'a LifecycleEvent) -> HookHandlerFuture<'a>;

    fn timeout(&self, default: Duration) -> Option<Duration> {
        Some(default)
    }

    fn is_generation_bound(&self) -> bool {
        false
    }

    fn completion_check<'a>(&'a self, event: &'a CompletionEvent) -> StopHookFuture<'a> {
        Box::pin(async move {
            let input = LifecycleEvent::with_payload(
                LifecycleEventKind::Stop,
                serde_json::json!({
                    "response": event.response,
                    "continuations": event.continuations,
                    "stop_hook_active": event.continuations > 0,
                }),
            );
            let result = self
                .handle(&input)
                .await
                .map_err(|error| HookError::new(error.to_string()))?;
            completion_outcome(result)
        })
    }

    fn has_agent_turn_end(&self) -> bool {
        true
    }

    fn has_task_created(&self) -> bool {
        true
    }

    fn has_task_completed(&self) -> bool {
        true
    }

    fn task_created<'a>(&'a self, _event: &'a TaskCreatedEvent) -> TaskGateFuture<'a> {
        Box::pin(async { Ok(TaskGateOutcome::Allow) })
    }

    fn task_completed<'a>(&'a self, _event: &'a TaskCompletedEvent) -> TaskGateFuture<'a> {
        Box::pin(async { Ok(TaskGateOutcome::Allow) })
    }

    fn teammate_idle<'a>(&'a self, _event: &'a TeammateIdleEvent) -> TeammateIdleFuture<'a> {
        Box::pin(async { Ok(TeammateIdleOutcome::Allow) })
    }
}

#[derive(Clone)]
enum HookRegistration {
    Callback(Hook),
    Handler(Arc<dyn HookHandler>),
}

impl HookRegistration {
    fn control(&self) -> Option<&HookControl> {
        match self {
            Self::Callback(hook) => Some(&hook.control),
            Self::Handler(handler) => handler.control(),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct HookRegistry {
    registrations: Vec<HookRegistration>,
    runtime: Arc<HookRuntime>,
    timeouts: HookTimeouts,
}

const DEFAULT_HOOK_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SESSION_END_TIMEOUT: Duration = Duration::from_millis(1500);

impl HookRegistry {
    pub(crate) async fn completion_check(
        &self,
        event: &CompletionEvent,
    ) -> HookResult<CompletionOutcome> {
        let outcomes = join_all(self.registrations.iter().map(|registration| async {
            match registration {
                HookRegistration::Callback(registration) => match &registration.hook {
                    RegisteredHook::Stop(hook) => {
                        registration
                            .invoke(hook, event.clone(), CompletionOutcome::Allow)
                            .await
                    }
                    _ => Ok(CompletionOutcome::Allow),
                },
                HookRegistration::Handler(handler) => {
                    run_handler_gate(
                        handler.as_ref(),
                        handler.completion_check(event),
                        self.timeouts.hook_timeout,
                    )
                    .await
                }
            }
        }))
        .await;
        let mut feedback = Vec::new();
        let mut failures = Vec::new();
        for outcome in outcomes {
            match outcome? {
                CompletionOutcome::Allow => {}
                CompletionOutcome::Continue { feedback: value } => {
                    validate_hook_feedback(&value, "Stop")?;
                    feedback.push(value);
                }
                CompletionOutcome::Fail { reason } => failures.push(reason),
            }
        }
        Ok(if !failures.is_empty() {
            CompletionOutcome::Fail {
                reason: failures.join("\n"),
            }
        } else if !feedback.is_empty() {
            let feedback = feedback.join("\n");
            validate_hook_feedback(&feedback, "Stop")?;
            CompletionOutcome::Continue { feedback }
        } else {
            CompletionOutcome::Allow
        })
    }

    pub(crate) async fn teammate_idle(
        &self,
        event: TeammateIdleEvent,
    ) -> HookResult<TeammateIdleOutcome> {
        let outcomes = join_all(self.registrations.iter().map(|registration| async {
            match registration {
                HookRegistration::Callback(registration) => match &registration.hook {
                    RegisteredHook::TeammateIdle(hook) => {
                        registration
                            .invoke(hook, event.clone(), TeammateIdleOutcome::Allow)
                            .await
                    }
                    _ => Ok(TeammateIdleOutcome::Allow),
                },
                HookRegistration::Handler(handler) => {
                    run_handler_gate(
                        handler.as_ref(),
                        handler.teammate_idle(&event),
                        self.timeouts.hook_timeout,
                    )
                    .await
                }
            }
        }))
        .await;
        let mut feedback = Vec::new();
        let mut stop = false;
        for outcome in outcomes {
            let outcome = outcome?;
            outcome.validate()?;
            match outcome {
                TeammateIdleOutcome::Allow => {}
                TeammateIdleOutcome::Continue { feedback: value } => feedback.push(value),
                TeammateIdleOutcome::Stop => stop = true,
            }
        }
        let outcome = if stop {
            TeammateIdleOutcome::Stop
        } else if !feedback.is_empty() {
            TeammateIdleOutcome::Continue {
                feedback: feedback.join("\n"),
            }
        } else {
            TeammateIdleOutcome::Allow
        };
        outcome.validate()?;
        Ok(outcome)
    }

    pub(crate) fn has_agent_turn_end(&self) -> bool {
        self.registrations
            .iter()
            .any(|registration| match registration {
                HookRegistration::Callback(hook) => {
                    matches!(hook.hook, RegisteredHook::AgentTurnEnd(_))
                }
                HookRegistration::Handler(handler) => handler.has_agent_turn_end(),
            })
    }

    pub(crate) fn has_task_created(&self) -> bool {
        self.registrations
            .iter()
            .any(|registration| match registration {
                HookRegistration::Callback(hook) => {
                    matches!(hook.hook, RegisteredHook::TaskCreated(_))
                }
                HookRegistration::Handler(handler) => handler.has_task_created(),
            })
    }

    pub(crate) fn has_task_completed(&self) -> bool {
        self.registrations
            .iter()
            .any(|registration| match registration {
                HookRegistration::Callback(hook) => {
                    matches!(hook.hook, RegisteredHook::TaskCompleted(_))
                }
                HookRegistration::Handler(handler) => handler.has_task_completed(),
            })
    }

    pub(crate) async fn task_created(
        &self,
        event: TaskCreatedEvent,
    ) -> HookResult<TaskGateOutcome> {
        let outcomes = join_all(self.registrations.iter().map(|registration| async {
            match registration {
                HookRegistration::Callback(registration) => match &registration.hook {
                    RegisteredHook::TaskCreated(hook) => {
                        registration
                            .invoke(hook, event.clone(), TaskGateOutcome::Allow)
                            .await
                    }
                    _ => Ok(TaskGateOutcome::Allow),
                },
                HookRegistration::Handler(handler) => {
                    run_handler_gate(
                        handler.as_ref(),
                        handler.task_created(&event),
                        self.timeouts.hook_timeout,
                    )
                    .await
                }
            }
        }))
        .await;
        merge_task_gates(outcomes)
    }

    pub(crate) async fn task_completed(
        &self,
        event: TaskCompletedEvent,
    ) -> HookResult<TaskGateOutcome> {
        let outcomes = join_all(self.registrations.iter().map(|registration| async {
            match registration {
                HookRegistration::Callback(registration) => match &registration.hook {
                    RegisteredHook::TaskCompleted(hook) => {
                        registration
                            .invoke(hook, event.clone(), TaskGateOutcome::Allow)
                            .await
                    }
                    _ => Ok(TaskGateOutcome::Allow),
                },
                HookRegistration::Handler(handler) => {
                    run_handler_gate(
                        handler.as_ref(),
                        handler.task_completed(&event),
                        self.timeouts.hook_timeout,
                    )
                    .await
                }
            }
        }))
        .await;
        merge_task_gates(outcomes)
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_timeouts(timeouts: HookTimeouts) -> Self {
        Self {
            timeouts,
            ..Self::default()
        }
    }

    pub(crate) fn register_hook(&mut self, mut hook: Hook) {
        hook.control
            .timeout
            .get_or_insert(self.timeouts.hook_timeout);
        hook.control = hook.control.fresh().with_default_id(
            "rust",
            hook.hook.contract().0,
            self.registrations.len(),
        );
        self.registrations.push(HookRegistration::Callback(hook));
    }

    fn handlers(&self) -> impl Iterator<Item = &Arc<dyn HookHandler>> {
        self.registrations
            .iter()
            .filter_map(|registration| match registration {
                HookRegistration::Handler(handler) => Some(handler),
                HookRegistration::Callback(_) => None,
            })
    }

    pub(crate) fn callback_timeout(&self) -> Duration {
        self.timeouts.hook_timeout
    }

    pub(crate) fn register_handler(&mut self, handler: Arc<dyn HookHandler>) {
        self.registrations.push(HookRegistration::Handler(handler));
    }

    async fn call_handler(
        &self,
        handler: &dyn HookHandler,
        event: &LifecycleEvent,
    ) -> Option<HookResponse> {
        match run_handler(handler, event, self.timeouts.hook_timeout).await {
            Ok(result) => result,
            Err(error) => {
                if matches!(
                    event.kind,
                    LifecycleEventKind::PreToolUse
                        | LifecycleEventKind::UserPromptSubmit
                        | LifecycleEventKind::UserPromptExpansion
                ) {
                    let kind = event.kind.as_str();
                    tracing::warn!(kind, %error, "lifecycle hook failed");
                    Some(HookResponse {
                        block: true,
                        reason: Some(format!("{kind} hook failed: {error}")),
                        ..Default::default()
                    })
                } else {
                    self.record_hook_error(event.kind.as_str(), &error);
                    None
                }
            }
        }
    }

    pub async fn emit(&self, event: &LifecycleEvent) -> Vec<HookResponse> {
        join_all(
            self.handlers()
                .map(|handler| self.call_handler(handler.as_ref(), event)),
        )
        .await
        .into_iter()
        .flatten()
        .collect()
    }
}

pub(crate) async fn emit_user_prompt_submit(
    bus: &HookRegistry,
    event: UserPromptSubmitEvent,
) -> UserPromptSubmitAction {
    let input = LifecycleEvent::with_payload(
        LifecycleEventKind::UserPromptSubmit,
        serde_json::json!({
            "original_prompt": event.original_prompt, "prompt": event.prompt,
            "source": event.source.as_str(), "dispatch": event.dispatch.as_str(),
            "delegated_child": event.delegated_child,
        }),
    );
    let outcomes = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::UserPromptSubmit(hook) = &registration.hook else {
                    return Ok(None);
                };
                registration
                    .invoke(
                        hook,
                        event.clone(),
                        UserPromptSubmitOutcome::Continue {
                            prompt: event.prompt.clone(),
                            injected_context: Vec::new(),
                            notices: Vec::new(),
                        },
                    )
                    .await
                    .map(Some)
            }
            HookRegistration::Handler(handler) => {
                let Some(result) = bus.call_handler(handler.as_ref(), &input).await else {
                    return Ok(None);
                };
                submission_outcome(result, &event.prompt).map(Some)
            }
        }
    }))
    .await;
    let mut prompt = event.prompt.clone();
    let mut injected_context = Vec::new();
    let mut tool_policy = crate::core::turn::TurnToolPolicy::default();
    let mut handled = false;
    let mut reasons = Vec::new();
    for outcome in outcomes {
        let outcome = match outcome {
            Ok(Some(outcome)) => outcome,
            Ok(None) => continue,
            Err(error) => UserPromptSubmitOutcome::Handled {
                reason: Some(error.to_string()),
            },
        };
        let (next, contexts, notices) = match outcome {
            UserPromptSubmitOutcome::Continue {
                prompt,
                injected_context,
                notices,
            } => (prompt, injected_context, notices),
            UserPromptSubmitOutcome::ContinueWithPolicy {
                prompt,
                injected_context,
                tool_policy: contribution,
                notices,
            } => {
                tool_policy.merge(&contribution);
                (prompt, injected_context, notices)
            }
            UserPromptSubmitOutcome::Handled { reason } => {
                handled = true;
                reasons.extend(reason);
                continue;
            }
        };
        if next != event.prompt {
            prompt = next;
        }
        injected_context.extend(contexts);
        bus.record_notices(notices);
    }
    if handled {
        UserPromptSubmitAction::Handled {
            reason: (!reasons.is_empty()).then(|| reasons.join("\n")),
        }
    } else {
        UserPromptSubmitAction::Continue {
            prompt,
            injected_context,
            tool_policy,
        }
    }
}

pub(crate) async fn emit_prompt_expansion(
    bus: &HookRegistry,
    event: &UserPromptExpansionEvent,
) -> HookResult<UserPromptExpansionOutcome> {
    let input = LifecycleEvent::with_payload(
        LifecycleEventKind::UserPromptExpansion,
        serde_json::json!({
            "expansion_type": "slash_command", "prompt": event.prompt,
            "command_name": event.command_name, "command_args": event.command_args,
            "command_source": event.command_source,
        }),
    );
    let outcomes = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::UserPromptExpansion(hook) = &registration.hook else {
                    return Ok(UserPromptExpansionOutcome::Allow);
                };
                registration
                    .invoke(hook, event.clone(), UserPromptExpansionOutcome::Allow)
                    .await
            }
            HookRegistration::Handler(handler) => {
                let Some(result) = bus.call_handler(handler.as_ref(), &input).await else {
                    return Ok(UserPromptExpansionOutcome::Allow);
                };
                if result.should_stop() {
                    return Ok(UserPromptExpansionOutcome::Block {
                        reason: result
                            .reason
                            .unwrap_or_else(|| "prompt expansion blocked".into()),
                    });
                }
                let payload = result.into_payload().unwrap_or_default();
                if payload.get("decision").and_then(Value::as_str) == Some("block") {
                    return Ok(UserPromptExpansionOutcome::Block {
                        reason: payload
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("prompt expansion blocked")
                            .into(),
                    });
                }
                let mut contexts: Vec<crate::context::InjectedContext> = payload
                    .get("injected_context")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|error| {
                        HookError::new(format!("invalid UserPromptExpansion context: {error}"))
                    })?
                    .unwrap_or_default();
                let specific = payload.get("hookSpecificOutput").unwrap_or(&payload);
                if let Some(content) = specific
                    .get("additionalContext")
                    .or_else(|| specific.get("additional_context"))
                    .and_then(Value::as_str)
                {
                    contexts.push(crate::context::InjectedContext {
                        source: crate::transcript::ContextSource::Hook,
                        content: content.into(),
                    });
                }
                Ok(UserPromptExpansionOutcome::AddContext { contexts })
            }
        }
    }))
    .await;
    let mut contexts = Vec::new();
    let mut reasons = Vec::new();
    for outcome in outcomes {
        match outcome? {
            UserPromptExpansionOutcome::Allow => {}
            UserPromptExpansionOutcome::AddContext {
                contexts: contribution,
            } => contexts.extend(contribution),
            UserPromptExpansionOutcome::Block { reason } => reasons.push(reason),
        }
    }
    Ok(if reasons.is_empty() {
        UserPromptExpansionOutcome::AddContext { contexts }
    } else {
        UserPromptExpansionOutcome::Block {
            reason: reasons.join("\n"),
        }
    })
}

pub(crate) async fn emit_build_system_prompt(
    bus: &HookRegistry,
    event: BuildSystemPromptEvent,
) -> BuildSystemPromptResult {
    let input = LifecycleEvent::with_payload(
        LifecycleEventKind::BuildSystemPrompt,
        serde_json::json!({"system_prompt": event.system_prompt, "cwd": event.cwd}),
    );
    let outcomes = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::BuildSystemPrompt(hook) = &registration.hook else {
                    return None;
                };
                match registration
                    .invoke(
                        hook,
                        event.clone(),
                        BuildSystemPromptOutcome {
                            system_prompt: event.system_prompt.clone(),
                        },
                    )
                    .await
                {
                    Ok(outcome) => Some(outcome.system_prompt),
                    Err(error) => {
                        bus.record_hook_error("BuildSystemPrompt", &error);
                        None
                    }
                }
            }
            HookRegistration::Handler(handler) => bus
                .call_handler(handler.as_ref(), &input)
                .await
                .and_then(HookResponse::into_payload)
                .and_then(|payload| {
                    payload
                        .get("system_prompt")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
        }
    }))
    .await;
    BuildSystemPromptResult {
        system_prompt: outcomes
            .into_iter()
            .flatten()
            .filter(|prompt| prompt != &event.system_prompt)
            .last(),
    }
}

pub(crate) async fn emit_session_start(
    bus: &HookRegistry,
    reason: SessionStartReason,
    previous_session_file: Option<&str>,
) -> Vec<HookResponse> {
    bus.await_startup().await;
    let event = SessionStartEvent {
        reason,
        related_session: previous_session_file.map(str::to_string),
    };
    let input = LifecycleEvent::with_payload(
        LifecycleEventKind::SessionStart,
        serde_json::json!({
            "source": reason.as_str(), "reason": start_reason(reason), "previous_session_file": previous_session_file,
        }),
    );
    let results = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::SessionStart(hook) = &registration.hook else {
                    return None;
                };
                if registration.control.start_mode == Some(SessionStartMode::BeforeFirstPrompt) {
                    let hook = hook.clone();
                    let registration = registration.clone();
                    let event = event.clone();
                    bus.runtime
                        .startup
                        .lock()
                        .await
                        .tasks
                        .push_back(tokio::spawn(async move {
                            registration.invoke(&hook, event, Vec::new()).await
                        }));
                } else {
                    bus.record_hook_result(
                        registration.invoke(hook, event.clone(), Vec::new()).await,
                    );
                }
                None
            }
            HookRegistration::Handler(handler) => bus.call_handler(handler.as_ref(), &input).await,
        }
    }))
    .await;
    results.into_iter().flatten().collect()
}

pub(crate) async fn emit_session_end(
    bus: &HookRegistry,
    reason: SessionEndReason,
    target_session_file: Option<&str>,
) -> Vec<HookResponse> {
    let ending = async {
        let pending = std::mem::take(&mut *bus.runtime.notifications.lock().unwrap());
        let event = SessionEndEvent {
            reason,
            related_session: target_session_file.map(str::to_string),
        };
        let input = LifecycleEvent::with_payload(
            LifecycleEventKind::SessionEnd,
            serde_json::json!({"reason": reason.as_str(), "target_session_file": target_session_file}),
        );
        let results = join_all(bus.registrations.iter().map(|registration| async {
            match registration {
                HookRegistration::Callback(registration) => {
                    if let RegisteredHook::SessionEnd(hook) = &registration.hook {
                        bus.record_hook_result(
                            registration.invoke(hook, event.clone(), Vec::new()).await,
                        );
                    }
                    None
                }
                HookRegistration::Handler(handler) => {
                    bus.call_handler(handler.as_ref(), &input).await
                }
            }
        }))
        .await;
        pending.finish().await;
        results.into_iter().flatten().collect()
    };
    match tokio::time::timeout(bus.timeouts.session_end_timeout, ending).await {
        Ok(results) => results,
        Err(_) => {
            tracing::warn!("SessionEnd hooks timed out");
            bus.record_notices([HookNotice::Warning {
                message: "SessionEnd timed out".into(),
            }]);
            Vec::new()
        }
    }
}

pub(crate) async fn emit_permission_request(bus: &HookRegistry, tool: &str, input: &str) {
    bus.emit(&LifecycleEvent::with_payload(
        LifecycleEventKind::PermissionRequest,
        serde_json::json!({ "tool_name": tool, "tool_input": input }),
    ))
    .await;
}

pub(crate) async fn emit_permission_denied(bus: &HookRegistry, tool: &str, input: &str) {
    bus.emit(&LifecycleEvent::with_payload(
        LifecycleEventKind::PermissionDenied,
        serde_json::json!({ "tool_name": tool, "tool_input": input }),
    ))
    .await;
}

pub(crate) async fn emit_pre_compact(bus: &HookRegistry, message_count: usize) {
    bus.emit(&LifecycleEvent::with_payload(
        LifecycleEventKind::PreCompact,
        serde_json::json!({ "message_count": message_count }),
    ))
    .await;
}

pub(crate) async fn emit_post_compact(bus: &HookRegistry, message_count: usize, summary: &str) {
    bus.emit(&LifecycleEvent::with_payload(
        LifecycleEventKind::PostCompact,
        serde_json::json!({ "message_count": message_count, "summary": summary }),
    ))
    .await;
}

async fn emit_observation<I: Clone + HookInput>(
    bus: &HookRegistry,
    event: I,
    input: LifecycleEvent,
    select: fn(&RegisteredHook) -> Option<&Arc<HookCallback<I, ()>>>,
) {
    join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                if let Some(hook) = select(&registration.hook)
                    && let Err(error) = registration.invoke(hook, event.clone(), ()).await
                {
                    bus.record_hook_error(input.kind.as_str(), &error);
                }
            }
            HookRegistration::Handler(handler) => {
                bus.call_handler(handler.as_ref(), &input).await;
            }
        }
    }))
    .await;
}

fn agent_run_payload(event: &AgentRunEvent) -> Value {
    serde_json::json!({"session_id": event.session_id, "agent_id": event.agent_id, "parent_agent_id": event.parent_agent_id, "parent_tool_call_id": event.parent_tool_call_id})
}

pub(crate) async fn emit_agent_start(bus: &HookRegistry, event: AgentRunEvent) {
    let input =
        LifecycleEvent::with_payload(LifecycleEventKind::AgentStart, agent_run_payload(&event));
    emit_observation(bus, event, input, |hook| match hook {
        RegisteredHook::AgentStart(hook) => Some(hook),
        _ => None,
    })
    .await;
}

pub(crate) async fn emit_agent_end(bus: &HookRegistry, event: AgentRunEvent) {
    let input =
        LifecycleEvent::with_payload(LifecycleEventKind::AgentEnd, agent_run_payload(&event));
    emit_observation(bus, event, input, |hook| match hook {
        RegisteredHook::AgentEnd(hook) => Some(hook),
        _ => None,
    })
    .await;
}

pub(crate) async fn emit_agent_turn_start(bus: &HookRegistry, event: AgentTurnEvent) {
    let input = LifecycleEvent::with_payload(
        LifecycleEventKind::AgentTurnStart,
        serde_json::json!({"turn_id": event.turn_id, "index": event.index}),
    );
    emit_observation(bus, event, input, |hook| match hook {
        RegisteredHook::AgentTurnStart(hook) => Some(hook),
        _ => None,
    })
    .await;
}

pub(crate) async fn emit_agent_turn_end(
    bus: &HookRegistry,
    event: AgentTurnEndEvent,
) -> AgentTurnEndOutcome {
    let input = std::sync::LazyLock::new(|| {
        LifecycleEvent::with_payload(
            LifecycleEventKind::AgentTurnEnd,
            serde_json::json!({"turn_id": event.turn.turn_id, "index": event.turn.index, "transcript": event.transcript}),
        )
    });
    let outcomes = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::AgentTurnEnd(hook) = &registration.hook else {
                    return Ok(None);
                };
                registration
                    .invoke(hook, event.clone(), AgentTurnEndOutcome::default())
                    .await
                    .map(Some)
            }
            HookRegistration::Handler(handler) => {
                if !handler.has_agent_turn_end() {
                    return Ok(None);
                }
                let response = bus.call_handler(handler.as_ref(), &input).await;
                response
                    .and_then(HookResponse::into_payload)
                    .map(serde_json::from_value::<AgentTurnEndOutcome>)
                    .transpose()
                    .map_err(|error| HookError::new(error.to_string()))
            }
        }
    }))
    .await;
    let mut merged = AgentTurnEndOutcome::default();
    for outcome in outcomes {
        match outcome {
            Ok(Some(outcome)) => merged.context_edits.extend(outcome.context_edits),
            Ok(None) => {}
            Err(error) => bus.record_hook_result(Err(error)),
        }
    }
    merged
}

pub(crate) async fn emit_before_provider_request(bus: &HookRegistry, turn_id: &str) {
    bus.emit(&LifecycleEvent::with_payload(
        LifecycleEventKind::BeforeProviderRequest,
        serde_json::json!({ "turn_id": turn_id }),
    ))
    .await;
}

pub(crate) async fn emit_context(bus: &HookRegistry, turn_id: &str) -> Vec<String> {
    let input = LifecycleEvent::with_payload(
        LifecycleEventKind::Context,
        serde_json::json!({"turn_id": turn_id}),
    );
    let additions = join_all(bus.registrations.iter().map(|registration| async {
        let mut additions = Vec::new();
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::ProviderContext(hook) = &registration.hook else {
                    return additions;
                };
                let input = ProviderContextEvent {
                    turn_id: turn_id.into(),
                };
                match registration
                    .invoke(
                        hook,
                        input,
                        ProviderContextOutcome {
                            additions: Vec::new(),
                        },
                    )
                    .await
                {
                    Ok(outcome) => additions.extend(outcome.additions),
                    Err(error) => {
                        bus.record_hook_error("Context", &error);
                    }
                }
            }
            HookRegistration::Handler(handler) => {
                if let Some(payload) = bus
                    .call_handler(handler.as_ref(), &input)
                    .await
                    .and_then(HookResponse::into_payload)
                {
                    let values = match payload {
                        Value::Array(values) => values,
                        value => vec![value],
                    };
                    additions.extend(
                        values
                            .into_iter()
                            .filter_map(|value| value.as_str().map(str::to_string)),
                    );
                }
            }
        }
        additions
    }))
    .await;
    additions.into_iter().flatten().collect()
}

pub(crate) async fn emit_after_provider_response(bus: &HookRegistry, turn_id: &str) {
    bus.emit(&LifecycleEvent::with_payload(
        LifecycleEventKind::AfterProviderResponse,
        serde_json::json!({ "turn_id": turn_id }),
    ))
    .await;
}

pub(crate) fn notify_completion(bus: &HookRegistry, response: &str) {
    bus.schedule_notification(NotificationEvent {
        kind: NotificationKind::IdlePrompt,
        level: "info".into(),
        message: response.into(),
        tool_name: None,
    });
}

async fn emit_notification(bus: &HookRegistry, event: NotificationEvent) {
    let input = LifecycleEvent::with_payload(
        LifecycleEventKind::Notification,
        serde_json::json!({
            "notification_type": event.kind.as_str(), "level": event.level,
            "message": event.message, "tool_name": event.tool_name,
        }),
    );
    join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                if let RegisteredHook::Notification(hook) = &registration.hook {
                    if let Err(error) = registration.invoke(hook, event.clone(), ()).await {
                        bus.record_hook_error("Notification", &error);
                    }
                }
            }
            HookRegistration::Handler(handler) => {
                bus.call_handler(handler.as_ref(), &input).await;
            }
        }
    }))
    .await;
}

pub(crate) async fn emit_stop_failure(bus: &HookRegistry, error: &str) {
    bus.emit(&LifecycleEvent::with_payload(
        LifecycleEventKind::StopFailure,
        serde_json::json!({ "error": error }),
    ))
    .await;
}

pub(crate) async fn emit_pre_tool_use(bus: &HookRegistry, event: ToolUseEvent) -> PreToolUseResult {
    let results = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::PreToolUse(hook) = &registration.hook else {
                    return None;
                };
                match registration
                    .invoke(hook, event.clone(), PreToolUseOutcome::Continue)
                    .await
                {
                    Ok(PreToolUseOutcome::Continue) => None,
                    Ok(PreToolUseOutcome::Rewrite { tool_input }) => {
                        Some(lifecycle_action(HookAction::PreToolUse(PreToolUseResult {
                            updated_input: Some(tool_input),
                            ..Default::default()
                        })))
                    }
                    Ok(PreToolUseOutcome::Deny { reason }) => Some(denied_pre_tool_result(reason)),
                    Err(error) => Some(denied_pre_tool_result(format!(
                        "PreToolUse hook failed: {error}"
                    ))),
                }
            }
            HookRegistration::Handler(handler) => {
                let input = LifecycleEvent::with_payload(
                    LifecycleEventKind::PreToolUse,
                    serde_json::json!({
                        "tool_name": event.tool_name,
                        "tool_use_id": event.tool_use_id,
                        "tool_input": event.tool_input,
                    }),
                );
                bus.call_handler(handler.as_ref(), &input).await
            }
        }
    }))
    .await;
    let mut merged = PreToolUseResult::default();
    for result in results.into_iter().flatten() {
        let stop = result.should_stop();
        if stop {
            merge_permission_decision(&mut merged, PermissionDecision::Deny, result.reason);
        }
        if let Some(HookOutput::Action(HookAction::PreToolUse(action))) = result.output {
            if let Some(decision) = action.permission_decision {
                merge_permission_decision(&mut merged, decision, action.reason);
            }
            if let Some(input) = action.updated_input {
                merged.updated_input = Some(input);
            }
            merged.additional_context.extend(action.additional_context);
        }
    }
    merged
}

pub(crate) async fn emit_post_tool_use(
    bus: &HookRegistry,
    event: ToolResultEvent,
) -> PostToolUseResult {
    let results = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::PostToolUse(hook) = &registration.hook else {
                    return None;
                };
                match registration
                    .invoke(hook, event.clone(), PostToolUseOutcome::Continue)
                    .await
                {
                    Ok(PostToolUseOutcome::Rewrite { tool_output }) => Some(lifecycle_action(
                        HookAction::PostToolUse(PostToolUseResult {
                            updated_output: Some(tool_output),
                            ..Default::default()
                        }),
                    )),
                    Err(error) => {
                        bus.record_hook_error("PostToolUse", &error);
                        None
                    }
                    Ok(PostToolUseOutcome::Continue) => None,
                }
            }
            HookRegistration::Handler(handler) => {
                let input = LifecycleEvent::with_payload(
                    LifecycleEventKind::PostToolUse,
                    serde_json::json!({
                        "tool_name": event.tool_name,
                        "tool_use_id": event.tool_use_id,
                        "tool_input": event.tool_input,
                        "tool_response": event.tool_output,
                        "duration_ms": event.duration_ms,
                    }),
                );
                bus.call_handler(handler.as_ref(), &input).await
            }
        }
    }))
    .await;
    let mut merged = PostToolUseResult::default();
    for result in results.into_iter().flatten() {
        merge_post_tool_use_result(&mut merged, result);
    }
    merged
}

pub(crate) async fn emit_post_tool_use_failure(
    bus: &HookRegistry,
    event: ToolFailureEvent,
) -> PostToolUseResult {
    let results = join_all(bus.registrations.iter().map(|registration| async {
        match registration {
            HookRegistration::Callback(registration) => {
                let RegisteredHook::PostToolFailure(hook) = &registration.hook else {
                    return None;
                };
                match registration
                    .invoke(hook, event.clone(), PostToolFailureOutcome::Continue)
                    .await
                {
                    Ok(PostToolFailureOutcome::Rewrite { error }) => Some(lifecycle_action(
                        HookAction::PostToolUse(PostToolUseResult {
                            updated_output: Some(Value::String(error)),
                            ..Default::default()
                        }),
                    )),
                    Err(error) => {
                        bus.record_hook_error("PostToolUseFailure", &error);
                        None
                    }
                    Ok(PostToolFailureOutcome::Continue) => None,
                }
            }
            HookRegistration::Handler(handler) => {
                let input = LifecycleEvent::with_payload(
                    LifecycleEventKind::PostToolUseFailure,
                    serde_json::json!({
                        "tool_name": event.tool_name,
                        "tool_use_id": event.tool_use_id,
                        "tool_input": event.tool_input,
                        "error": event.error,
                        "is_interrupt": event.is_interrupt,
                        "duration_ms": event.duration_ms,
                    }),
                );
                bus.call_handler(handler.as_ref(), &input).await
            }
        }
    }))
    .await;
    let mut merged = PostToolUseResult::default();
    for result in results.into_iter().flatten() {
        merge_post_tool_use_result(&mut merged, result);
    }
    merged
}

pub(crate) async fn emit_post_tool_batch(
    bus: &HookRegistry,
    event: ToolBatchEvent,
) -> PostToolBatchResult {
    let lifecycle_event = LifecycleEvent::with_payload(
        LifecycleEventKind::PostToolBatch,
        serde_json::json!({ "tool_calls": event.tool_calls }),
    );
    merge_post_tool_batch_results(bus.emit(&lifecycle_event).await)
}

pub(crate) async fn emit_message_display(
    bus: &HookRegistry,
    event: MessageDisplayEvent,
) -> MessageDisplayResult {
    let lifecycle_event = LifecycleEvent::with_payload(
        LifecycleEventKind::MessageDisplay,
        serde_json::json!({
            "turn_id": event.turn_id,
            "message_id": event.message_id,
            "index": event.index,
            "final": event.final_batch,
            "delta": event.delta,
        }),
    );
    merge_message_display_results(bus.emit(&lifecycle_event).await)
}

fn denied_pre_tool_result(reason: String) -> HookResponse {
    lifecycle_action(HookAction::PreToolUse(PreToolUseResult {
        permission_decision: Some(PermissionDecision::Deny),
        reason: Some(reason),
        ..PreToolUseResult::default()
    }))
}

fn lifecycle_action(action: HookAction) -> HookResponse {
    HookResponse {
        output: Some(HookOutput::Action(action)),
        ..HookResponse::default()
    }
}

async fn run_handler_gate<T>(
    handler: &dyn HookHandler,
    future: impl Future<Output = HookResult<T>>,
    default: Duration,
) -> HookResult<T> {
    match handler.timeout(default) {
        Some(timeout) => run_hook(future, timeout).await,
        None => future.await,
    }
}

fn completion_outcome(result: Option<HookResponse>) -> HookResult<CompletionOutcome> {
    let Some(result) = result else {
        return Ok(CompletionOutcome::Allow);
    };
    let block = result.should_stop();
    let reason = result.reason.clone();
    let payload = result.into_payload().unwrap_or_default();
    if block || payload.get("decision").and_then(Value::as_str) == Some("block") {
        let feedback = reason
            .or_else(|| {
                payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(|| HookError::new("Stop rejection requires feedback"))?;
        return Ok(CompletionOutcome::Continue { feedback });
    }
    Ok(CompletionOutcome::Allow)
}

fn submission_outcome(result: HookResponse, prompt: &str) -> HookResult<UserPromptSubmitOutcome> {
    if result.should_stop() {
        return Ok(UserPromptSubmitOutcome::Handled {
            reason: result.reason,
        });
    }
    let payload = result.into_payload().unwrap_or_default();
    if payload.get("action").and_then(Value::as_str) == Some("handled") {
        return Ok(UserPromptSubmitOutcome::Handled {
            reason: payload
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string),
        });
    }
    let contexts = payload
        .get("injected_context")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| HookError::new(format!("invalid UserPromptSubmit context: {error}")))?
        .unwrap_or_default();
    Ok(UserPromptSubmitOutcome::Continue {
        prompt: payload
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or(prompt)
            .to_string(),
        injected_context: contexts,
        notices: Vec::new(),
    })
}

fn merge_task_gates(outcomes: Vec<HookResult<TaskGateOutcome>>) -> HookResult<TaskGateOutcome> {
    let mut reasons = Vec::new();
    for outcome in outcomes {
        if let TaskGateOutcome::Reject { reason } = outcome? {
            reasons.push(reason);
        }
    }
    Ok(if reasons.is_empty() {
        TaskGateOutcome::Allow
    } else {
        TaskGateOutcome::Reject {
            reason: reasons.join("\n"),
        }
    })
}

fn merge_permission_decision(
    result: &mut PreToolUseResult,
    next: PermissionDecision,
    reason: Option<String>,
) {
    let current_rank = result.permission_decision.map(decision_rank).unwrap_or(0);
    let next_rank = decision_rank(next);
    if next_rank > current_rank {
        result.permission_decision = Some(next);
        result.reason = reason;
    } else if next_rank == current_rank
        && let Some(reason) = reason
    {
        result.reason = Some(match result.reason.take() {
            Some(previous) if previous != reason => format!("{previous}\n{reason}"),
            Some(previous) => previous,
            None => reason,
        });
    }
}

fn decision_rank(decision: PermissionDecision) -> u8 {
    match decision {
        PermissionDecision::Allow => 1,
        PermissionDecision::Ask => 2,
        PermissionDecision::Defer => 3,
        PermissionDecision::Deny => 4,
    }
}

fn merge_post_tool_use_result(merged: &mut PostToolUseResult, result: HookResponse) {
    merged.block |= result.block;
    merged.reason = result.reason.or(merged.reason.take());
    if let Some(HookOutput::Action(HookAction::PostToolUse(action))) = result.output {
        merged.block |= action.block;
        merged.reason = action.reason.or(merged.reason.take());
        if action.updated_output.is_some() {
            merged.updated_output = action.updated_output;
        }
        merged.additional_context.extend(action.additional_context);
    }
}

fn merge_post_tool_batch_results(results: Vec<HookResponse>) -> PostToolBatchResult {
    let mut merged = PostToolBatchResult::default();
    for result in results {
        let HookResponse {
            block,
            reason,
            output,
            ..
        } = result;
        merged.block |= block;
        merged.reason = reason.or(merged.reason);
        if let Some(HookOutput::Action(HookAction::PostToolBatch(action))) = output {
            merged.block |= action.block;
            merged.reason = action.reason.or(merged.reason);
            merged.additional_context.extend(action.additional_context);
        }
    }
    merged
}

fn merge_message_display_results(results: Vec<HookResponse>) -> MessageDisplayResult {
    let mut merged = MessageDisplayResult::default();
    for result in results {
        if let Some(payload) = result.into_payload() {
            let hook_payload = payload.get("hookSpecificOutput").unwrap_or(&payload);
            if let Some(content) = hook_payload
                .get("displayContent")
                .or_else(|| hook_payload.get("display_content"))
                .and_then(Value::as_str)
            {
                merged.display_content = Some(content.to_string());
            }
        }
    }
    merged
}

#[cfg(test)]
mod tests;
