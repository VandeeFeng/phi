use phi_sdk::interaction::{InteractionResponse, PermissionDecision};
use phi_sdk::lifecycle::UserPromptSubmitSource;
use phi_sdk::turn::{ImageInput, InjectedContext, TurnPrompt, TurnToolPolicy};
use phi_sdk::workflow::WorkflowRunId;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(super) const VERSION: u32 = 2;
const MAX_ID_BYTES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Incoming {
    Input {
        #[serde(deserialize_with = "identifier")]
        input_id: String,
        prompt: Prompt,
        dispatch: Dispatch,
        directed: Option<Directed>,
    },
    Control {
        #[serde(deserialize_with = "identifier")]
        control_id: String,
        request: Control,
    },
    Answer {
        #[serde(deserialize_with = "identifier")]
        question_id: String,
        response: Answer,
    },
}

fn identifier<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err(serde::de::Error::custom(
            "ID must contain 1..=128 UTF-8 bytes",
        ));
    }
    Ok(value)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Prompt {
    text: String,
    #[serde(default)]
    images: Vec<ImageInput>,
    #[serde(default)]
    context: Vec<String>,
    #[serde(default)]
    tool_policy: TurnToolPolicy,
}

impl From<Prompt> for TurnPrompt {
    fn from(prompt: Prompt) -> Self {
        Self {
            prompt: prompt.text,
            source: UserPromptSubmitSource::Rpc,
            images: prompt.images,
            tool_policy: prompt.tool_policy,
            injected_context: prompt
                .context
                .into_iter()
                .map(|content| InjectedContext {
                    source: phi_sdk::transcript::ContextSource::FileMention,
                    content,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Dispatch {
    Immediate,
    Steer,
    FollowUp,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Directed {
    pub(super) agent: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Startup {
    New {
        persistence: Persistence,
        provider: Option<String>,
        model: Option<String>,
        switch: Option<String>,
    },
    Resume {
        persistence: Persistence,
        session_id: String,
    },
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub(super) enum Persistence {
    Saved,
    Ephemeral,
}

impl Persistence {
    pub(super) fn saved(self) -> bool {
        matches!(self, Self::Saved)
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Control {
    Initialize { version: u32, startup: Startup },
    Interrupt,
    RuntimeInfo,
    Configure { selection: Selection },
    Reload,
    ExpandPrompt { text: String },
    TaskStop { task_id: String },
    WorkflowStop { run_id: WorkflowRunId },
    Close { mode: CloseMode },
}

impl Control {
    pub(super) fn during_drain(&self) -> bool {
        matches!(
            self,
            Self::Interrupt
                | Self::TaskStop { .. }
                | Self::WorkflowStop { .. }
                | Self::Close { .. }
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Selection {
    Model { selection: String },
    Switch { name: String },
}

#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum CloseMode {
    Drain,
    Cancel,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Answer {
    Permission {
        decision: Decision,
        pattern: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Decision {
    AllowOnce,
    AllowAlways,
    Deny,
}

impl Answer {
    pub(super) fn into_response(self) -> Result<InteractionResponse, Fault> {
        Ok(match self {
            Self::Permission {
                decision: Decision::AllowAlways,
                pattern: Some(pattern),
            } => InteractionResponse::Permission(PermissionDecision::AllowAlways(pattern)),
            Self::Permission {
                decision: Decision::AllowOnce,
                pattern: None,
            } => InteractionResponse::Permission(PermissionDecision::AllowOnce),
            Self::Permission {
                decision: Decision::Deny,
                pattern: None,
            } => InteractionResponse::Permission(PermissionDecision::Deny),
            Self::Permission { .. } => {
                return Err(Fault::new(
                    Code::InvalidResponse,
                    "pattern is required only for allow_always",
                ));
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Code {
    InvalidMessage,
    UnsupportedVersion,
    NotReady,
    DuplicateId,
    Busy,
    Closing,
    InvalidTarget,
    InteractionClosed,
    InvalidResponse,
    OperationFailed,
    HookNotice,
    ExtensionNotification,
    InputUnavailable,
    InputFailed,
    OutputUnavailable,
    ObservationLost,
    StartupFailed,
    FinalizationFailed,
    ExecutionFailed,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Fault {
    pub(super) code: Code,
    pub(super) message: String,
}

impl Fault {
    pub(super) fn new(code: Code, message: impl std::fmt::Display) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
    pub(super) fn operation(error: impl std::fmt::Display) -> Self {
        Self::new(Code::OperationFailed, error)
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.message.fmt(formatter)
    }
}
impl std::error::Error for Fault {}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Outgoing {
    Admission {
        input_id: String,
        disposition: Value,
    },
    ControlResult {
        control_id: String,
        result: ControlResult,
    },
    Event {
        agent_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        payload: Value,
    },
    Outcome {
        agent_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        turn_id: String,
        result: Value,
    },
    Question {
        question_id: String,
        agent_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<String>,
        request: Value,
    },
    QuestionClosed {
        question_id: String,
        status: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<Fault>,
        #[serde(skip_serializing_if = "Option::is_none")]
        persistence_warning: Option<String>,
    },
    Notice {
        code: Code,
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    Closed {
        status: &'static str,
        reason: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<Fault>,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum ControlResult {
    Ok { value: Value },
    Error { error: Fault },
}

impl Outgoing {
    pub(super) fn control(control_id: String, result: Result<Value, Fault>) -> Self {
        Self::ControlResult {
            control_id,
            result: match result {
                Ok(value) => ControlResult::Ok { value },
                Err(error) => ControlResult::Error { error },
            },
        }
    }
    pub(super) fn notice(error: Fault) -> Self {
        Self::Notice {
            code: error.code,
            message: error.message,
            agent_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strict_messages_reject_the_service_surface_and_caller_authority() {
        for value in [
            json!({"id": 1, "method": "initialize"}),
            json!({"type":"control","control_id":"x","request":{"kind":"subscribe"}}),
            json!({"type":"control","control_id":"x","request":{"kind":"mcp_status"}}),
            json!({"type":"control","control_id":"x","request":{"kind":"mcp_reconnect","server":"test"}}),
            json!({"type":"control","control_id":"x","request":{"kind":"mcp_cancel","server":"test"}}),
            json!({"type":"answer","question_id":"x","response":{"kind":"mcp_elicitation","response":{"action":"decline"}}}),
            json!({"type":"control","control_id":"x","request":{"kind":"initialize","version":2,"startup":{"kind":"resume","persistence":"saved","session_id":"x","session":{}}}}),
            json!({"type":"input","input_id":"x","dispatch":"immediate","prompt":{"text":"x","source":"memory"}}),
            json!({"type":"input","input_id":"x","dispatch":"immediate","prompt":{"text":"x"},"agent_id":"other"}),
        ] {
            assert!(serde_json::from_value::<Incoming>(value).is_err());
        }
        for length in [0, 1, MAX_ID_BYTES, MAX_ID_BYTES + 1] {
            let value = json!({"type":"control","control_id":"x".repeat(length),"request":{"kind":"interrupt"}});
            assert_eq!(
                serde_json::from_value::<Incoming>(value).is_ok(),
                (1..=MAX_ID_BYTES).contains(&length)
            );
        }
    }
    #[test]
    fn ids_and_startup_fields_do_not_smuggle_authority() {
        for id in [
            "a".repeat(1),
            "a".repeat(MAX_ID_BYTES),
            "é".repeat(MAX_ID_BYTES / 2),
        ] {
            assert!(
                serde_json::from_value::<Incoming>(
                    json!({"type":"control","control_id":id,"request":{"kind":"interrupt"}})
                )
                .is_ok()
            );
        }
        for id in [
            String::new(),
            "a".repeat(MAX_ID_BYTES + 1),
            "é".repeat(MAX_ID_BYTES / 2 + 1),
        ] {
            assert!(
                serde_json::from_value::<Incoming>(
                    json!({"type":"control","control_id":id,"request":{"kind":"interrupt"}})
                )
                .is_err()
            );
        }
        for kind in ["new", "resume"] {
            for field in [
                "session",
                "transcript",
                "working_dir",
                "tools",
                "sandbox",
                "permission_mode",
                "api_key",
                "runtime",
            ] {
                let mut startup = json!({"kind":kind,"persistence":"saved"});
                if kind == "resume" {
                    startup["session_id"] = json!("saved");
                }
                startup[field] = json!("forbidden");
                assert!(serde_json::from_value::<Incoming>(json!({"type":"control","control_id":"test","request":{"kind":"initialize","version":VERSION,"startup":startup}})).is_err());
            }
        }
    }
}
