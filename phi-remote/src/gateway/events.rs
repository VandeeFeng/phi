use phi_sdk::PhiEvent;
use phi_sdk::agent_instance::AgentInstanceEvent;
use serde_json::{Value, json};

pub(super) fn project(event: AgentInstanceEvent) -> Value {
    match event {
        AgentInstanceEvent::Phi(envelope) => {
            let event = match envelope.event {
                PhiEvent::AssistantStarted { id } => json!({"type":"assistant_started","id":id}),
                PhiEvent::AssistantDelta { display, .. } => {
                    json!({"type":"assistant_delta","display":display})
                }
                PhiEvent::AssistantImage { source } => {
                    json!({"type":"assistant_image","source":source})
                }
                PhiEvent::ReasoningDelta { text } => json!({"type":"reasoning_delta","text":text}),
                PhiEvent::ReasoningFinished { text } => {
                    json!({"type":"reasoning_finished","text":text})
                }
                PhiEvent::ToolCallStarted {
                    id,
                    name,
                    args,
                    display,
                    ..
                } => {
                    json!({"type":"tool_call_started","id":id,"name":name,"args":args,"display":display})
                }
                PhiEvent::ToolCallFinished {
                    id,
                    output,
                    is_error,
                    details,
                } => {
                    json!({"type":"tool_call_finished","id":id,"output":output,"is_error":is_error,"details":details})
                }
                PhiEvent::NestedToolCall { call } => json!({"type":"nested_tool_call","call":call}),
                PhiEvent::AssistantFinished {
                    id,
                    content,
                    display,
                    response,
                    usage,
                    has_tool_calls,
                } => json!({
                    "type":"assistant_finished","id":id,"content":crate::snapshot::live_content(content),
                    "display":if display.is_empty() { response } else { display },"usage":usage,"has_tool_calls":has_tool_calls,
                }),
                PhiEvent::TurnStarted { submission_id, .. } => {
                    json!({"type":"turn_started","submission_id":submission_id})
                }
                PhiEvent::TurnFinished {
                    usage, compacted, ..
                } => json!({"type":"turn_finished","usage":usage,"compacted":compacted}),
                PhiEvent::TurnFailed { failure } => json!({"type":"turn_failed","failure":failure}),
                PhiEvent::SubmissionDiscarded {
                    submission_id,
                    reason,
                } => {
                    json!({"type":"submission_discarded","submission_id":submission_id,"reason":reason})
                }
                PhiEvent::System { message } => json!({"type":"system","message":message}),
                PhiEvent::ProviderRecovery { recovery } => {
                    json!({"type":"provider_recovery","recovery":{
                        "provider":recovery.provider,"model":recovery.model,"reason":recovery.reason,
                        "attempt":recovery.attempt,"delay_ms":recovery.delay_ms,
                    }})
                }
                PhiEvent::UserInputInjected { .. } => return refresh("history"),
            };
            json!({"kind":"phi","id":envelope.id,"emittedAt":envelope.emitted_at,
                "sessionId":envelope.session_id,"turnId":envelope.turn_id,"messageId":envelope.message_id,
                "agentId":envelope.agent_id,"parentAgentId":envelope.parent_agent_id,
                "parentToolCallId":envelope.parent_tool_call_id,"event":event})
        }
        AgentInstanceEvent::Progress(progress) => {
            json!({"kind":"progress","turnId":progress.turn_id,
            "status":progress.status,"cancellationReason":progress.cancellation_reason,"startedAt":progress.started_at,
            "durationMs":progress.duration_ms,"usage":progress.usage,"toolUses":progress.tool_uses,
            "currentTools":progress.current_tools,"lastTool":progress.last_tool})
        }
        AgentInstanceEvent::ViewBoundary { id } => json!({"kind":"boundary","id":id}),
        AgentInstanceEvent::RunCancelled { .. } => refresh("cancelled"),
        AgentInstanceEvent::InteractionRequested(_)
        | AgentInstanceEvent::InteractionClosed { .. } => refresh("interactions"),
        AgentInstanceEvent::RuntimeReplaced => refresh("runtime"),
        AgentInstanceEvent::HistoryChanged | AgentInstanceEvent::ContextReset { .. } => {
            refresh("history")
        }
        AgentInstanceEvent::PumpFailed { .. } => refresh("failed"),
        AgentInstanceEvent::Stopped => refresh("stopped"),
        _ => refresh("state"),
    }
}

fn refresh(reason: &str) -> Value {
    json!({"kind":"refresh","reason":reason})
}
