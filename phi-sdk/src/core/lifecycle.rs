//! Lifecycle events and outcomes; no hook registration or execution.

use crate::context::InjectedContext;
use crate::transcript::{ContextSource, Transcript};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

/// Owned input for context preparation after prompt transformations.
#[derive(Debug, Clone)]
pub struct PromptContextEvent {
    pub original_prompt: String,
    pub prompt: String,
    pub source: UserPromptSubmitSource,
    pub dispatch: UserPromptDispatch,
    pub transcript: Arc<Transcript>,
    pub working_dir: PathBuf,
    pub delegated_child: bool,
}

/// Nonfatal hook reports and accepted context-insertion facts, not execution decisions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HookNotice {
    Info {
        message: String,
    },
    Warning {
        message: String,
    },
    ContextInjected {
        source: ContextSource,
        item_count: usize,
        requested: bool,
    },
}

/// Contributions are accepted together with the submission, never during preparation.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptContextOutcome {
    pub contexts: Vec<InjectedContext>,
    pub notices: Vec<HookNotice>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum UserPromptSubmitSource {
    Interactive,
    Internal,
    Print,
    Rpc,
    Sdk,
    Extension,
}

impl UserPromptSubmitSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Internal => "internal",
            Self::Print => "print",
            Self::Rpc => "rpc",
            Self::Sdk => "sdk",
            Self::Extension => "extension",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum UserPromptDispatch {
    Immediate,
    Steer,
    FollowUp,
}

impl UserPromptDispatch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Steer => "steer",
            Self::FollowUp => "follow_up",
        }
    }
}

/// Submitted input before admission; every hook receives the same snapshot.
#[derive(Debug, Clone)]
pub struct UserPromptSubmitEvent {
    pub original_prompt: String,
    pub prompt: String,
    pub source: UserPromptSubmitSource,
    pub dispatch: UserPromptDispatch,
    pub delegated_child: bool,
}

/// A recognized command before application-owned expansion or its side effects.
#[derive(Debug, Clone)]
pub struct UserPromptExpansionEvent {
    pub prompt: String,
    pub command_name: String,
    pub command_args: String,
    pub command_source: String,
}

#[derive(Debug, Clone, Default)]
pub enum UserPromptExpansionOutcome {
    #[default]
    Allow,
    AddContext {
        contexts: Vec<InjectedContext>,
    },
    Block {
        reason: String,
    },
}

/// Complete system-prompt construction, not request-local context.
#[derive(Debug, Clone)]
pub struct BuildSystemPromptEvent {
    pub system_prompt: String,
    pub cwd: Option<String>,
}

/// Session initialization, not attachment.
#[derive(Debug, Clone)]
pub struct SessionStartEvent {
    pub reason: SessionStartReason,
    pub related_session: Option<String>,
}

/// Session stop/replacement, not turn completion or detach.
#[derive(Debug, Clone)]
pub struct SessionEndEvent {
    pub reason: SessionEndReason,
    pub related_session: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    PermissionPrompt,
    IdlePrompt,
    Message,
}

impl NotificationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PermissionPrompt => "permission_prompt",
            Self::IdlePrompt => "idle_prompt",
            Self::Message => "message",
        }
    }
}

/// Generation-scoped notification, not a decision gate.
#[derive(Debug, Clone)]
pub struct NotificationEvent {
    pub kind: NotificationKind,
    pub level: String,
    pub message: String,
    pub tool_name: Option<String>,
}

/// Request-local context, not Transcript entries.
#[derive(Debug, Clone)]
pub struct ProviderContextEvent {
    pub turn_id: String,
}

/// Execution-run identity, not AgentInstance lifetime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentRunEvent {
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub parent_tool_call_id: Option<String>,
}

/// Inner model/tool round; retries remain in the round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentTurnEvent {
    pub run: AgentRunEvent,
    pub turn_id: String,
    pub index: usize,
}

/// Inner round end with committed source entries for context-only revisions.
#[derive(Debug, Clone)]
pub struct AgentTurnEndEvent {
    pub turn: AgentTurnEvent,
    pub transcript: Transcript,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AgentTurnEndOutcome {
    #[serde(default)]
    pub context_edits: Vec<ContextEditDraft>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContextEditDraft {
    pub target_id: String,
    #[serde(deserialize_with = "serde::Deserialize::deserialize")]
    pub replacement: Option<crate::transcript::ContextReplacement>,
}

/// Pre-execution tool input; rewrites still require authorization.
#[derive(Debug, Clone)]
pub struct ToolUseEvent {
    pub tool_name: String,
    pub tool_use_id: String,
    pub tool_input: Value,
}

/// Tool result before commit; effects have already occurred.
#[derive(Debug, Clone)]
pub struct ToolResultEvent {
    pub tool_name: String,
    pub tool_use_id: String,
    pub tool_input: Value,
    pub tool_output: Value,
    pub duration_ms: Option<u64>,
}

/// Tool failure before model-visible commit.
#[derive(Debug, Clone)]
pub struct ToolFailureEvent {
    pub tool_name: String,
    pub tool_use_id: String,
    pub tool_input: Value,
    pub error: String,
    pub is_interrupt: bool,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
/// Session initialization, not a turn or client-attachment boundary.
/// CLI startup uses Startup even for saved history; in-process switching uses New/Resume.
pub enum SessionStartReason {
    /// First standalone runtime, including --resume, --continue, and --session.
    Startup,
    /// Reload resources for the current Session without changing its identity.
    Reload,
    /// Create a Session inside an already-running application.
    New,
    /// Open saved history inside a running application; attaching to a live Agent emits no start.
    Resume,
    /// Context-clear reinitialization; retained for compatibility.
    Clear,
    /// Context reinitialization after SDK compaction, not process startup.
    Compact,
}

impl SessionStartReason {
    /// Legacy payload spelling; typed New and Reload both map to "startup".
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup | Self::Reload | Self::New => "startup",
            Self::Resume => "resume",
            Self::Clear => "clear",
            Self::Compact => "compact",
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
/// Explicit Session stop/replacement; client disconnect or detach is not Session end.
pub enum SessionEndReason {
    /// Stop the Agent or shut down its application; standalone TUI exit requests this.
    Quit,
    /// End the old runtime generation before Reload starts the replacement.
    Reload,
    /// End the old Session before an in-process New transition.
    New,
    /// End the old Session before an in-process Resume transition.
    Resume,
}

impl SessionEndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quit => "quit",
            Self::Reload => "reload",
            Self::New => "new",
            Self::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
/// Navigate the current Transcript tree without starting or ending a Session.
pub enum SessionTreeReason {
    Rewind,
    AnchorJump,
}

impl SessionTreeReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rewind => "rewind",
            Self::AnchorJump => "anchor_jump",
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
/// Discover runtime resources on initial load or reload, not on ordinary Session switches.
pub enum ResourcesDiscoverReason {
    Startup,
    Reload,
}

impl ResourcesDiscoverReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Reload => "reload",
        }
    }
}

/// An independent replacement for the complete system prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildSystemPromptOutcome {
    pub system_prompt: String,
}

/// An independent prompt contribution or terminal handling decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserPromptSubmitOutcome {
    Continue {
        prompt: String,
        injected_context: Vec<crate::context::InjectedContext>,
        notices: Vec<HookNotice>,
    },
    /// Adds outer-turn intent, not permission grants. Contributions are merged and validated.
    ContinueWithPolicy {
        prompt: String,
        injected_context: Vec<crate::context::InjectedContext>,
        tool_policy: crate::core::turn::TurnToolPolicy,
        notices: Vec<HookNotice>,
    },
    Handled {
        reason: Option<String>,
    },
}

/// Candidate response completion, not Agent shutdown.
#[derive(Clone, Debug)]
pub struct CompletionEvent {
    /// Canonical snapshot including the candidate response.
    pub transcript: crate::transcript::Transcript,
    pub response: String,
    /// Accepted automatic continuations in this run.
    pub continuations: usize,
}

/// Response-completion decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionOutcome {
    /// Accept the candidate; pending input may still continue execution.
    Allow,
    /// Continue with bounded, nonempty internal input.
    Continue { feedback: String },
    /// Fail this run.
    Fail { reason: String },
}

/// Provider-context additions for the current request only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProviderContextOutcome {
    pub additions: Vec<String>,
}

/// A pre-tool gate decision. Continue and rewrite still run normal authorization.
#[derive(Clone, Debug, PartialEq)]
pub enum PreToolUseOutcome {
    Continue,
    Rewrite { tool_input: Value },
    Deny { reason: String },
}

/// The next model-visible tool output before transcript commit.
#[derive(Clone, Debug, PartialEq)]
pub enum PostToolUseOutcome {
    Continue,
    Rewrite { tool_output: Value },
}

/// The next model-visible tool failure before transcript commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PostToolFailureOutcome {
    Continue,
    Rewrite { error: String },
}

/// Scoped checklist Task identity, not execution authority.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskLifecycleContext {
    pub instance_id: crate::harness::instance::AgentInstanceId,
    pub list_id: String,
    pub team: Option<crate::harness::manager::AgentTeamId>,
    pub alias: Option<crate::harness::manager::AgentAlias>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecycleStatus {
    Pending,
    InProgress,
    Completed,
}

/// Owned hook input, not a mutable Task record or storage capability.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskLifecycleTask {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub active_form: Option<String>,
    pub status: TaskLifecycleStatus,
    pub owner: Option<crate::harness::instance::AgentInstanceId>,
    pub blocked_by: Vec<String>,
    pub metadata: serde_json::Map<String, Value>,
}

/// Proposed Task creation, before commit.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskCreatedEvent {
    pub context: TaskLifecycleContext,
    pub task: TaskLifecycleTask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCompletionTrigger {
    ExplicitUpdate,
    TeammateEndTurn,
}

/// Task completion or end-turn validation; end-turn does not complete Tasks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TaskCompletedEvent {
    pub context: TaskLifecycleContext,
    pub task: TaskLifecycleTask,
    pub trigger: TaskCompletionTrigger,
}

/// Task operation/check decision, not Agent control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskGateOutcome {
    Allow,
    Reject { reason: String },
}

/// Managed teammate idle boundary.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TeammateIdleEvent {
    pub context: TaskLifecycleContext,
}

/// Idle policy; Stop ends the teammate, unlike CompletionOutcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeammateIdleOutcome {
    Allow,
    Continue { feedback: String },
    Stop,
}

/// Adapter vocabulary; not every event has a public Rust hook.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LifecycleEventKind {
    /// Discover external resources such as skills, prompts, themes, or tools.
    ResourcesDiscover,
    /// A session is initialized, resumed, or switched into.
    SessionStart,
    /// A session is shutting down or being replaced.
    SessionEnd,
    /// The active transcript tree changes or is inspected.
    SessionTree,
    /// The runtime/system prompt is being built; system prompt mutation belongs here.
    BuildSystemPrompt,
    /// An agent run has started; observational boundary event, not a mutation hook.
    AgentStart,
    /// An agent run has ended; observational boundary event for cleanup/telemetry.
    AgentEnd,
    /// An inner model/tool round is starting; provider retries remain within this round.
    AgentTurnStart,
    /// An inner model/tool round has ended, not the outer turn or Session.
    AgentTurnEnd,
    /// Assistant display text is about to be rendered or transformed for display.
    MessageDisplay,
    /// User input was submitted before model processing; prompt/context injection belongs here.
    UserPromptSubmit,
    /// A recognized command may be blocked before application-owned expansion.
    UserPromptExpansion,
    /// Context contributions are prepared after prompt transforms, before input admission.
    PromptContext,
    /// Provider-neutral context additions are assembled for the current model request only.
    Context,
    /// Observational boundary before a provider attempt; payload mutation is not exposed.
    BeforeProviderRequest,
    /// Observational boundary after a provider response stream has been consumed.
    AfterProviderResponse,
    /// A tool call is about to execute and may be adjusted or blocked.
    PreToolUse,
    /// A tool call requires an explicit permission decision.
    PermissionRequest,
    /// A tool call was denied before execution.
    PermissionDenied,
    /// A tool call completed successfully and its result can be inspected or adjusted.
    PostToolUse,
    /// A tool call failed and the failure can be inspected.
    PostToolUseFailure,
    /// A batch of tool calls completed before the next model turn.
    PostToolBatch,
    /// A candidate final response is ready; the completion gate can require more work.
    Stop,
    /// The runner is exiting with an error; this is observational, not a completion gate.
    StopFailure,
    /// Transcript/context compaction is about to run.
    PreCompact,
    /// Transcript/context compaction completed.
    PostCompact,
    /// A user-visible or integration-visible notification was emitted.
    Notification,
    /// A command is about to run and may be intercepted.
    CommandBefore,
    /// A command completed.
    CommandAfter,
    /// The UI is about to render or refresh.
    UiRender,
}

impl LifecycleEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ResourcesDiscover => "resources_discover",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::SessionTree => "session_tree",
            Self::BuildSystemPrompt => "BuildSystemPrompt",
            Self::AgentStart => "AgentStart",
            Self::AgentEnd => "AgentEnd",
            Self::AgentTurnStart => "AgentTurnStart",
            Self::AgentTurnEnd => "AgentTurnEnd",
            Self::MessageDisplay => "MessageDisplay",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::UserPromptExpansion => "UserPromptExpansion",
            Self::PromptContext => "PromptContext",
            Self::Context => "context",
            Self::BeforeProviderRequest => "BeforeProviderRequest",
            Self::AfterProviderResponse => "AfterProviderResponse",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PermissionDenied => "PermissionDenied",
            Self::PostToolUse => "PostToolUse",
            Self::PostToolUseFailure => "PostToolUseFailure",
            Self::PostToolBatch => "PostToolBatch",
            Self::Stop => "Stop",
            Self::StopFailure => "StopFailure",
            Self::PreCompact => "PreCompact",
            Self::PostCompact => "PostCompact",
            Self::Notification => "Notification",
            Self::CommandBefore => "command_before",
            Self::CommandAfter => "command_after",
            Self::UiRender => "ui_render",
        }
    }
}

/// Internal adapter payload at a lifecycle boundary.
#[derive(Debug, Clone)]
pub struct LifecycleEvent {
    pub kind: LifecycleEventKind,
    pub payload: Value,
}

impl LifecycleEvent {
    pub fn new(kind: LifecycleEventKind) -> Self {
        Self {
            kind,
            payload: Value::Null,
        }
    }

    pub fn with_payload(kind: LifecycleEventKind, payload: Value) -> Self {
        Self { kind, payload }
    }
}

#[derive(Debug, Clone)]
pub enum UserPromptSubmitAction {
    Continue {
        prompt: String,
        injected_context: Vec<crate::context::InjectedContext>,
        tool_policy: crate::core::turn::TurnToolPolicy,
    },
    Handled {
        reason: Option<String>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct BuildSystemPromptResult {
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolBatchEvent {
    pub tool_calls: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct MessageDisplayEvent {
    pub turn_id: String,
    pub message_id: String,
    pub index: usize,
    pub final_batch: bool,
    pub delta: String,
}

#[derive(Debug, Clone, Default)]
pub struct MessageDisplayResult {
    pub display_content: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
    Defer,
}

#[derive(Debug, Clone, Default)]
pub struct PreToolUseResult {
    pub permission_decision: Option<PermissionDecision>,
    pub reason: Option<String>,
    pub updated_input: Option<Value>,
    pub additional_context: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PostToolUseResult {
    pub block: bool,
    pub reason: Option<String>,
    pub updated_output: Option<Value>,
    pub additional_context: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct PostToolBatchResult {
    pub block: bool,
    pub reason: Option<String>,
    pub additional_context: Vec<String>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HookAction {
    PreToolUse(PreToolUseResult),
    PostToolUse(PostToolUseResult),
    PostToolBatch(PostToolBatchResult),
}
