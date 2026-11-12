use phi_sdk::agent_instance::{ExecutionProgress, ExecutionStatus, ExecutionTool};
use phi_sdk::tools::{NestedToolCall, ToolDisplay, ToolResultDetails};
use phi_sdk::{PhiEvent, PhiEventEnvelope, PhiFailure, PhiUsage, ProviderRecovery};
use serde::Deserialize;
use serde_json::Value;

/// Disposable observations; canonical history is read through REST.
#[derive(Debug)]
pub enum RemoteEvent {
    Phi(PhiEventEnvelope),
    Progress(ExecutionProgress),
    Boundary { id: String },
    Refresh { reason: RemoteRefresh },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteRefresh {
    History,
    Runtime,
    Interactions,
    Cancelled,
    Failed,
    Stopped,
    State,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum WireEvent {
    Phi {
        id: String,
        emitted_at: String,
        session_id: String,
        turn_id: Option<String>,
        message_id: Option<String>,
        agent_id: Option<String>,
        parent_agent_id: Option<String>,
        parent_tool_call_id: Option<String>,
        event: DisplayEvent,
    },
    Progress {
        turn_id: String,
        status: ExecutionStatus,
        cancellation_reason: Option<phi_sdk::CancellationReason>,
        started_at: String,
        duration_ms: u64,
        usage: PhiUsage,
        tool_uses: u64,
        current_tools: Vec<ExecutionTool>,
        last_tool: Option<ExecutionTool>,
    },
    Boundary {
        id: String,
    },
    Refresh {
        reason: RemoteRefresh,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum DisplayEvent {
    AssistantStarted {
        id: String,
    },
    AssistantDelta {
        display: String,
    },
    AssistantImage {
        source: phi_sdk::transcript::ImageSource,
    },
    ReasoningDelta {
        text: String,
    },
    ReasoningFinished {
        text: String,
    },
    ToolCallStarted {
        id: String,
        name: String,
        args: Value,
        display: Option<ToolDisplay>,
    },
    ToolCallFinished {
        id: String,
        output: String,
        is_error: bool,
        details: Option<ToolResultDetails>,
    },
    NestedToolCall {
        call: NestedToolCall,
    },
    AssistantFinished {
        id: String,
        content: Value,
        display: String,
        usage: PhiUsage,
        has_tool_calls: bool,
    },
    TurnStarted {
        submission_id: Option<String>,
    },
    TurnFinished {
        usage: PhiUsage,
        compacted: bool,
    },
    TurnFailed {
        failure: PhiFailure,
    },
    SubmissionDiscarded {
        submission_id: String,
        reason: phi_sdk::turn::SubmissionDiscardReason,
    },
    System {
        message: String,
    },
    ProviderRecovery {
        recovery: ProviderRecovery,
    },
}

impl RemoteEvent {
    pub(crate) fn decode(value: Value, selected: &str) -> anyhow::Result<Self> {
        Ok(match serde_json::from_value::<WireEvent>(value)? {
            WireEvent::Phi {
                id,
                emitted_at,
                session_id,
                turn_id,
                message_id,
                agent_id,
                parent_agent_id,
                parent_tool_call_id,
                event,
            } => {
                anyhow::ensure!(
                    session_id == selected,
                    "Gateway event belongs to another Session"
                );
                let event = match event {
                    DisplayEvent::AssistantStarted { id } => PhiEvent::AssistantStarted { id },
                    DisplayEvent::AssistantDelta { display } => PhiEvent::AssistantDelta {
                        original: String::new(),
                        display,
                    },
                    DisplayEvent::AssistantImage { source } => PhiEvent::AssistantImage { source },
                    DisplayEvent::ReasoningDelta { text } => PhiEvent::ReasoningDelta { text },
                    DisplayEvent::ReasoningFinished { text } => {
                        PhiEvent::ReasoningFinished { text }
                    }
                    DisplayEvent::ToolCallStarted {
                        id,
                        name,
                        args,
                        display,
                    } => PhiEvent::ToolCallStarted {
                        id,
                        name,
                        args,
                        display,
                        provider_call_id: None,
                        provider_item_id: None,
                    },
                    DisplayEvent::ToolCallFinished {
                        id,
                        output,
                        is_error,
                        details,
                    } => PhiEvent::ToolCallFinished {
                        id,
                        output,
                        is_error,
                        details,
                    },
                    DisplayEvent::NestedToolCall { call } => PhiEvent::NestedToolCall { call },
                    DisplayEvent::AssistantFinished {
                        id,
                        content,
                        display,
                        usage,
                        has_tool_calls,
                    } => PhiEvent::AssistantFinished {
                        id,
                        content: crate::snapshot::decode_live_content(content)?,
                        response: String::new(),
                        display,
                        usage,
                        has_tool_calls,
                    },
                    DisplayEvent::TurnStarted { submission_id } => {
                        let _ = submission_id;
                        // The wire omits prompt/source; only canonical history can supply them.
                        return Ok(Self::Refresh {
                            reason: RemoteRefresh::History,
                        });
                    }
                    DisplayEvent::TurnFinished { usage, compacted } => PhiEvent::TurnFinished {
                        response: String::new(),
                        display: String::new(),
                        usage,
                        compacted,
                    },
                    DisplayEvent::TurnFailed { failure } => PhiEvent::TurnFailed { failure },
                    DisplayEvent::SubmissionDiscarded {
                        submission_id,
                        reason,
                    } => PhiEvent::SubmissionDiscarded {
                        submission_id,
                        reason,
                    },
                    DisplayEvent::System { message } => PhiEvent::System { message },
                    DisplayEvent::ProviderRecovery { recovery } => {
                        PhiEvent::ProviderRecovery { recovery }
                    }
                };
                Self::Phi(PhiEventEnvelope {
                    id,
                    emitted_at: emitted_at.parse()?,
                    session_id,
                    turn_id,
                    message_id,
                    agent_id,
                    parent_agent_id,
                    parent_tool_call_id,
                    event,
                })
            }
            WireEvent::Progress {
                turn_id,
                status,
                cancellation_reason,
                started_at,
                duration_ms,
                usage,
                tool_uses,
                current_tools,
                last_tool,
            } => Self::Progress(ExecutionProgress {
                turn_id,
                status,
                cancellation_reason,
                started_at: started_at.parse()?,
                duration_ms,
                usage,
                tool_uses,
                current_tools,
                last_tool,
            }),
            WireEvent::Boundary { id } => Self::Boundary { id },
            WireEvent::Refresh { reason } => Self::Refresh { reason },
        })
    }
}
