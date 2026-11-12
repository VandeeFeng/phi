use crate::transcript::Usage;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PhiFailureKind {
    Authentication,
    RateLimited,
    ProviderUnavailable,
    ProviderRejected,
    InvalidResponse,
    MaxTurns,
    Cancelled,
    InvalidToolCall,
    Delegation,
    Internal,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CancellationReason {
    UserRequested,
    Interrupted,
    AgentStopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
#[non_exhaustive]
pub struct PhiFailure {
    pub kind: PhiFailureKind,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_status: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_reason: Option<CancellationReason>,
}

impl PhiFailure {
    pub fn new(
        kind: PhiFailureKind,
        message: impl Into<String>,
        provider_status: Option<u16>,
    ) -> Self {
        Self {
            kind,
            message: message.into(),
            provider_status,
            cancellation_reason: None,
        }
    }

    pub fn cancelled(reason: CancellationReason, message: impl Into<String>) -> Self {
        Self {
            cancellation_reason: Some(reason),
            ..Self::new(PhiFailureKind::Cancelled, message, None)
        }
    }

    pub fn is_user_cancelled(&self) -> bool {
        self.kind == PhiFailureKind::Cancelled
            && self.cancellation_reason == Some(CancellationReason::UserRequested)
    }

    pub fn is_retryable(&self) -> bool {
        matches!(
            self.kind,
            PhiFailureKind::RateLimited | PhiFailureKind::ProviderUnavailable
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolResultDetails {
    pub diff: Option<ToolDiffDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<ToolOutputDetails>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested_calls: Option<NestedCalls>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<crate::transcript::ImageSource>,
    /// UTF-8 byte offsets in the rendered output; empty means trailing images.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_offsets: Vec<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedCalls {
    pub calls: Vec<NestedToolCall>,
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NestedCallStatus {
    Running,
    Ok,
    Error,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestedToolCall {
    pub id: String,
    pub parent_tool_call_id: String,
    pub name: String,
    pub status: NestedCallStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Value>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Box<ToolResultDetails>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolOutputDetails {
    pub total_bytes: u64,
    pub total_lines: u64,
    pub truncated: bool,
    pub full_output_path: Option<std::path::PathBuf>,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryReason {
    ProviderRetry,
    ContextOverflow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderRecovery {
    pub reason: RecoveryReason,
    pub attempt: usize,
    pub delay_ms: u64,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDiffDetails {
    pub hunks: Vec<ToolDiffHunk>,
    /// Standard unified patch for structured consumers like exports or integrations.
    pub patch: Option<String>,
    /// First changed line in the new file for editor navigation.
    pub first_changed_line: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiffHunk {
    pub changes: Vec<ToolDiffChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDiffChange {
    pub kind: ToolDiffChangeKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolDiffChangeKind {
    Equal,
    Insert,
    Delete,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    NestedToolCall {
        entry_id: Option<CompactString>,
        call: NestedToolCall,
    },
    ProviderRecovery {
        recovery: ProviderRecovery,
    },
    AssistantInterrupted {
        turn_id: CompactString,
        provider: CompactString,
        api: Option<crate::provider::ProviderApi>,
        model: CompactString,
    },
    SubmissionDiscarded {
        submission_id: crate::core::turn::SubmissionId,
    },
    UserInputInjected {
        submission_id: Option<crate::core::turn::SubmissionId>,
        prompt: CompactString,
        source: crate::core::lifecycle::UserPromptSubmitSource,
        contexts: Vec<crate::context::InjectedContext>,
        content: Vec<crate::transcript::UserContentBlock>,
    },
    AssistantStarted {
        id: CompactString,
    },
    AssistantDelta {
        original: CompactString,
        display: CompactString,
    },
    AssistantImage {
        source: crate::transcript::ImageSource,
    },
    Reasoning(CompactString),
    ReasoningDone,
    ToolCall {
        id: CompactString,
        provider_call_id: Option<CompactString>,
        provider_item_id: Option<CompactString>,
        name: CompactString,
        args: serde_json::Value,
    },
    ToolResult {
        id: CompactString,
        entry_id: CompactString,
        output: CompactString,
        is_error: bool,
        details: Option<ToolResultDetails>,
    },
    AssistantFinished {
        id: CompactString,
        provider_message_id: Option<CompactString>,
        turn_id: CompactString,
        provider: CompactString,
        api: Option<crate::provider::ProviderApi>,
        model: CompactString,
        content: Vec<crate::transcript::ContentBlock>,
        response: CompactString,
        display: CompactString,
        usage: Usage,
        stop_reason: CompactString,
        has_tool_calls: bool,
    },
    RunFinished {
        response: CompactString,
        usage: Usage,
    },
    Error(PhiFailure),
}

#[derive(Debug, Clone)]
pub struct CoreEventEnvelope {
    pub id: CompactString,
    pub emitted_at: chrono::DateTime<chrono::Utc>,
    pub scope: CoreEventScope,
    pub event: CoreEvent,
}

#[derive(Debug, Clone, Default)]
pub struct CoreEventScope {
    pub session_id: Option<CompactString>,
    /// Outer core-turn ID, shared by all inner agent-loop turns for one response.
    pub turn_id: Option<CompactString>,
    pub agent_id: Option<CompactString>,
    pub parent_agent_id: Option<CompactString>,
    pub parent_tool_call_id: Option<CompactString>,
}

impl CoreEventScope {
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn agent_id(&self) -> Option<&str> {
        self.agent_id.as_deref()
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }

    pub fn parent_agent_id(&self) -> Option<&str> {
        self.parent_agent_id.as_deref()
    }

    pub fn parent_tool_call_id(&self) -> Option<&str> {
        self.parent_tool_call_id.as_deref()
    }
}

/// Internal runtime event emitted by the core agent loop/reducer.
/// First-party adapters may consume this for precise projection; public SDK users get `PhiEvent`.
#[derive(Debug, Clone)]
pub enum CoreEvent {
    ProviderRecovery {
        recovery: ProviderRecovery,
    },
    SubmissionDiscarded {
        submission_id: crate::core::turn::SubmissionId,
        reason: crate::core::turn::SubmissionDiscardReason,
    },
    /// An outer core turn started from immediate or follow-up dispatch.
    TurnStarted {
        submission_id: Option<crate::core::turn::SubmissionId>,
        prompt: CompactString,
        source: crate::core::lifecycle::UserPromptSubmitSource,
    },
    UserInputInjected {
        submission_id: Option<crate::core::turn::SubmissionId>,
        prompt: CompactString,
        source: crate::core::lifecycle::UserPromptSubmitSource,
    },
    AssistantStarted {
        id: CompactString,
    },
    AssistantDelta {
        original: CompactString,
        display: CompactString,
    },
    AssistantImage {
        source: crate::transcript::ImageSource,
    },
    ReasoningDelta {
        text: CompactString,
    },
    ReasoningFinished {
        text: CompactString,
    },
    NestedToolCall {
        call: NestedToolCall,
    },
    ToolCallStarted {
        id: CompactString,
        provider_call_id: Option<CompactString>,
        provider_item_id: Option<CompactString>,
        name: CompactString,
        args: serde_json::Value,
    },
    ToolCallFinished {
        id: CompactString,
        output: CompactString,
        is_error: bool,
        details: Option<ToolResultDetails>,
    },
    AssistantFinished {
        id: CompactString,
        content: Vec<crate::transcript::ContentBlock>,
        response: CompactString,
        display: CompactString,
        usage: Usage,
        has_tool_calls: bool,
    },
    TurnFinished {
        response: CompactString,
        display: CompactString,
        usage: Usage,
        compacted: bool,
    },
    TurnFailed {
        failure: PhiFailure,
    },
}

/// Public, serialization-friendly SDK event derived from `CoreEvent`.
/// This is the stable event surface for SDK streams, RPC, and external consumers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PhiEvent {
    ProviderRecovery {
        recovery: ProviderRecovery,
    },
    SubmissionDiscarded {
        submission_id: crate::core::turn::SubmissionId,
        #[serde(default)]
        reason: crate::core::turn::SubmissionDiscardReason,
    },
    System {
        message: String,
    },
    TurnStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submission_id: Option<crate::core::turn::SubmissionId>,
        prompt: String,
        source: String,
    },
    UserInputInjected {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        submission_id: Option<crate::core::turn::SubmissionId>,
        prompt: String,
        source: String,
    },
    AssistantStarted {
        id: String,
    },
    AssistantDelta {
        original: String,
        display: String,
    },
    AssistantImage {
        source: crate::transcript::ImageSource,
    },
    ReasoningDelta {
        text: String,
    },
    ReasoningFinished {
        text: String,
    },
    NestedToolCall {
        call: NestedToolCall,
    },
    ToolCallStarted {
        id: String,
        provider_call_id: Option<String>,
        provider_item_id: Option<String>,
        name: String,
        args: serde_json::Value,
        display: Option<ToolDisplay>,
    },
    ToolCallFinished {
        id: String,
        output: String,
        is_error: bool,
        details: Option<crate::core::event::ToolResultDetails>,
    },
    AssistantFinished {
        id: String,
        content: Vec<crate::transcript::ContentBlock>,
        response: String,
        display: String,
        usage: PhiUsage,
        has_tool_calls: bool,
    },
    TurnFinished {
        response: String,
        display: String,
        usage: PhiUsage,
        compacted: bool,
    },
    TurnFailed {
        failure: crate::core::event::PhiFailure,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhiEventEnvelope {
    pub id: String,
    pub emitted_at: chrono::DateTime<chrono::Utc>,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub message_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub parent_tool_call_id: Option<String>,
    pub event: PhiEvent,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PhiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cost: f64,
}

impl From<CoreEventEnvelope> for PhiEventEnvelope {
    fn from(envelope: CoreEventEnvelope) -> Self {
        let mut message_id = None;
        let event = match envelope.event {
            CoreEvent::SubmissionDiscarded {
                submission_id,
                reason,
            } => PhiEvent::SubmissionDiscarded {
                submission_id,
                reason,
            },
            CoreEvent::TurnStarted {
                submission_id,
                prompt,
                source,
            } => PhiEvent::TurnStarted {
                submission_id,
                prompt: prompt.to_string(),
                source: source.as_str().to_string(),
            },
            CoreEvent::UserInputInjected {
                submission_id,
                prompt,
                source,
            } => PhiEvent::UserInputInjected {
                submission_id,
                prompt: prompt.to_string(),
                source: source.as_str().to_string(),
            },
            CoreEvent::AssistantStarted { id } => {
                let id = id.to_string();
                message_id = Some(id.clone());
                PhiEvent::AssistantStarted { id }
            }
            CoreEvent::AssistantDelta { original, display } => PhiEvent::AssistantDelta {
                original: original.to_string(),
                display: display.to_string(),
            },
            CoreEvent::AssistantImage { source } => PhiEvent::AssistantImage { source },
            CoreEvent::ReasoningDelta { text } => PhiEvent::ReasoningDelta {
                text: text.to_string(),
            },
            CoreEvent::ReasoningFinished { text } => PhiEvent::ReasoningFinished {
                text: text.to_string(),
            },
            CoreEvent::NestedToolCall { call } => PhiEvent::NestedToolCall { call },
            CoreEvent::ToolCallStarted {
                id,
                provider_call_id,
                provider_item_id,
                name,
                args,
            } => PhiEvent::ToolCallStarted {
                id: id.to_string(),
                provider_call_id: provider_call_id.map(|value| value.to_string()),
                provider_item_id: provider_item_id.map(|value| value.to_string()),
                name: name.to_string(),
                args,
                display: None,
            },
            CoreEvent::ToolCallFinished {
                id,
                output,
                is_error,
                details,
            } => PhiEvent::ToolCallFinished {
                id: id.to_string(),
                output: output.to_string(),
                is_error,
                details,
            },
            CoreEvent::AssistantFinished {
                id,
                content,
                response,
                display,
                usage,
                has_tool_calls,
            } => {
                let id = id.to_string();
                message_id = Some(id.clone());
                PhiEvent::AssistantFinished {
                    id,
                    content,
                    response: response.to_string(),
                    display: display.to_string(),
                    usage: PhiUsage::from(usage),
                    has_tool_calls,
                }
            }
            CoreEvent::TurnFinished {
                response,
                display,
                usage,
                compacted,
            } => PhiEvent::TurnFinished {
                response: response.to_string(),
                display: display.to_string(),
                usage: PhiUsage::from(usage),
                compacted,
            },
            CoreEvent::TurnFailed { failure } => PhiEvent::TurnFailed { failure },
            CoreEvent::ProviderRecovery { recovery } => PhiEvent::ProviderRecovery { recovery },
        };
        Self {
            id: envelope.id.to_string(),
            emitted_at: envelope.emitted_at,
            session_id: envelope
                .scope
                .session_id()
                .map(ToString::to_string)
                .unwrap_or_default(),
            turn_id: envelope.scope.turn_id().map(ToString::to_string),
            message_id,
            agent_id: envelope.scope.agent_id().map(ToString::to_string),
            parent_agent_id: envelope.scope.parent_agent_id().map(ToString::to_string),
            parent_tool_call_id: envelope
                .scope
                .parent_tool_call_id()
                .map(ToString::to_string),
            event,
        }
    }
}

impl From<crate::transcript::Usage> for PhiUsage {
    fn from(usage: crate::transcript::Usage) -> Self {
        Self {
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            cost: usage.cost,
        }
    }
}

impl From<PhiUsage> for crate::transcript::Usage {
    fn from(usage: PhiUsage) -> Self {
        Self {
            input: usage.input_tokens,
            output: usage.output_tokens,
            total_tokens: usage.input_tokens.saturating_add(usage.output_tokens),
            cost: usage.cost,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolDisplay {
    /// Tool label shown before the result is available.
    pub title: Option<String>,
    /// Optional presentation of the invocation input, separate from its label and result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Optional metadata displayed after the title and input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_suffix: Option<String>,
    /// Whether the result block should be expanded by default.
    pub expand: bool,
    /// Optional display content prepended to the raw tool output.
    pub content: Option<String>,
}

impl ToolDisplay {
    pub fn title(title: impl Into<String>) -> Self {
        Self {
            title: Some(title.into()),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::turn::SubmissionDiscardReason;

    proptest::proptest! {
        #[test]
        fn tool_display_preserves_input_and_suffix(input in ".*", suffix in ".*") {
            let display = ToolDisplay {
                title: Some("label".into()),
                input: Some(input),
                title_suffix: Some(suffix),
                ..ToolDisplay::default()
            };
            let encoded = serde_json::to_value(&display).unwrap();
            let restored: ToolDisplay = serde_json::from_value(encoded).unwrap();
            proptest::prop_assert_eq!(restored, display);
        }
    }

    #[test]
    fn legacy_tool_display_without_input_remains_valid() {
        let display: ToolDisplay = serde_json::from_value(serde_json::json!({
            "title": "custom title", "expand": false, "content": null
        }))
        .unwrap();
        assert_eq!(display.input, None);
        assert_eq!(display.title_suffix, None);
    }

    #[test]
    fn cancellation_provenance_round_trips_without_guessing_legacy_source() {
        for reason in [
            CancellationReason::UserRequested,
            CancellationReason::Interrupted,
            CancellationReason::AgentStopped,
        ] {
            let failure = PhiFailure::cancelled(reason, "execution ended");
            let mut encoded = serde_json::to_value(&failure).unwrap();
            let restored: PhiFailure = serde_json::from_value(encoded.clone()).unwrap();
            assert_eq!(restored, failure);
            assert_eq!(
                restored.is_user_cancelled(),
                reason == CancellationReason::UserRequested
            );
            encoded
                .as_object_mut()
                .unwrap()
                .remove("cancellation_reason");
            let legacy: PhiFailure = serde_json::from_value(encoded).unwrap();
            assert_eq!(legacy.cancellation_reason, None);
            assert!(!legacy.is_user_cancelled());
        }
    }

    #[test]
    fn discarded_submission_reason_round_trips_and_defaults_to_legacy_failure() {
        for reason in [
            SubmissionDiscardReason::Cancelled,
            SubmissionDiscardReason::DispatchFailed,
        ] {
            let event = PhiEvent::SubmissionDiscarded {
                submission_id: "queued-input".into(),
                reason,
            };
            let mut encoded = serde_json::to_value(event).unwrap();
            assert!(matches!(
                serde_json::from_value::<PhiEvent>(encoded.clone()).unwrap(),
                PhiEvent::SubmissionDiscarded { submission_id, reason: decoded }
                    if submission_id == "queued-input" && decoded == reason
            ));
            encoded.as_object_mut().unwrap().remove("reason");
            assert!(matches!(
                serde_json::from_value::<PhiEvent>(encoded).unwrap(),
                PhiEvent::SubmissionDiscarded {
                    reason: SubmissionDiscardReason::DispatchFailed,
                    ..
                }
            ));
        }
    }
}
