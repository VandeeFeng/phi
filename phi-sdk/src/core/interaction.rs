use std::future::Future;

use tokio::sync::{mpsc, oneshot};

use crate::core::event::CoreEventScope;

tokio::task_local! {
    static ACTIVE_INTERACTION_SCOPE: CoreEventScope;
    static ACTIVE_TOOL_CALL_ID: String;
    static ACTIVE_ORIGINAL_BASH_COMMAND: Option<String>;
}

#[derive(Debug, Clone)]
pub struct InteractionEnvelope {
    pub request_id: String,
    pub emitted_at: chrono::DateTime<chrono::Utc>,
    pub scope: CoreEventScope,
    pub request: InteractionRequest,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "details", rename_all = "snake_case")]
pub enum InteractionRequest {
    Permission(PermissionInteraction),
    McpElicitation(McpElicitationRequest),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PermissionInteraction {
    pub tool: String,
    pub input: String,
    pub preview: Option<String>,
    pub origin: Option<String>,
}

#[derive(Debug, Clone)]
pub enum InteractionResponse {
    Permission(PermissionDecision),
    McpElicitation(McpElicitationResponse),
}

#[derive(Debug, Clone)]
pub enum PermissionDecision {
    AllowOnce,
    AllowAlways(String),
    Deny,
}

pub struct InteractionAnswer {
    pub status: String,
    pub allow_always: Option<(String, String)>,
}

pub(crate) type PermissionSender = mpsc::Sender<PermissionRequest>;
pub(crate) type PermissionReceiver = mpsc::Receiver<PermissionRequest>;

#[derive(Debug)]
pub(crate) struct PermissionRequest {
    pub scope: CoreEventScope,
    pub permission: PermissionInteraction,
    pub reply: oneshot::Sender<PermissionDecision>,
    pub applied: oneshot::Receiver<()>,
}

pub(crate) async fn with_interaction_scope<T>(
    scope: CoreEventScope,
    future: impl Future<Output = T>,
) -> T {
    ACTIVE_INTERACTION_SCOPE.scope(scope, future).await
}

pub(crate) async fn with_tool_call_scope<T>(
    tool_call_id: String,
    future: impl Future<Output = T>,
) -> T {
    ACTIVE_TOOL_CALL_ID.scope(tool_call_id, future).await
}

pub(crate) fn active_tool_call_id() -> Option<String> {
    ACTIVE_TOOL_CALL_ID.try_with(Clone::clone).ok()
}

pub(crate) async fn with_original_bash_command<T>(
    command: Option<String>,
    future: impl Future<Output = T>,
) -> T {
    ACTIVE_ORIGINAL_BASH_COMMAND.scope(command, future).await
}

pub(crate) fn active_original_bash_command() -> Option<String> {
    ACTIVE_ORIGINAL_BASH_COMMAND
        .try_with(Clone::clone)
        .ok()
        .flatten()
}

pub(crate) fn active_interaction_scope() -> CoreEventScope {
    ACTIVE_INTERACTION_SCOPE
        .try_with(Clone::clone)
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum McpElicitationRequest {
    Form {
        server: String,
        protocol_request_id: serde_json::Value,
        parent_tool_call_id: Option<String>,
        message: String,
        requested_schema: serde_json::Value,
    },
    Url {
        server: String,
        protocol_request_id: serde_json::Value,
        parent_tool_call_id: Option<String>,
        message: String,
        url: String,
        elicitation_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum McpElicitationResponse {
    Accept {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<serde_json::Value>,
    },
    Decline,
    Cancel,
}

pub(crate) type McpElicitationSender = mpsc::Sender<McpElicitationInteraction>;
pub(crate) type McpElicitationReceiver = mpsc::Receiver<McpElicitationInteraction>;

#[derive(Debug)]
pub(crate) struct McpElicitationInteraction {
    pub scope: CoreEventScope,
    pub request: McpElicitationRequest,
    pub reply: oneshot::Sender<McpElicitationResponse>,
}
