use super::*;
use crate::agent::tools::BashTool;
use crate::harness::instance::bash::{
    BashExecution, BashLifetime, BashMode, BashOutcome, BashStatus, BashStopReason,
};
use rig::tool::Tool;

const BASH_EXECUTION_EVENT: &str = "phi_sdk.bash_execution";
const BASH_FEEDBACK_EVENT: &str = "phi_sdk.bash_feedback";

#[derive(serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BashReceipt {
    BackgroundStarted {
        execution_id: String,
        output_path: std::path::PathBuf,
    },
}

impl AgentInstanceState {
    pub(super) fn recover_bash_history(&mut self) {
        use crate::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind};
        let mut executions = std::collections::HashMap::new();
        let mut delivered = std::collections::HashSet::new();
        for (position, entry) in self.transcript.current_branch().into_iter().enumerate() {
            if matches!(entry.kind, TranscriptEntryKind::Reset { .. }) {
                executions.clear();
                delivered.clear();
                continue;
            }
            if let TranscriptEntryKind::Message {
                message:
                    AgentMessage::ToolResult {
                        tool_call_id,
                        tool_name,
                        content,
                        is_error: false,
                        details,
                        ..
                    },
            } = &entry.kind
                && tool_name == BashTool::NAME
                && details
                    .as_ref()
                    .is_none_or(|details| details.output.is_none())
                && let [ContentBlock::Text { text }] = content.as_slice()
                && let Ok(BashReceipt::BackgroundStarted {
                    execution_id,
                    output_path,
                }) = serde_json::from_str(text)
            {
                executions.entry(execution_id.clone()).or_insert_with(|| {
                    (
                        position,
                        BashExecution {
                            id: execution_id,
                            turn_id: String::new(),
                            tool_call_id: tool_call_id.to_string(),
                            command: String::new(),
                            mode: BashMode::Background,
                            status: BashStatus::Running,
                            output_path,
                            exit_code: None,
                            error: None,
                        },
                    )
                });
            }
            if let TranscriptEntryKind::Event { name, data } = &entry.kind {
                if name == BASH_EXECUTION_EVENT {
                    if let Ok(execution) = serde_json::from_value::<BashExecution>(data.clone()) {
                        executions.insert(execution.id.clone(), (position, execution));
                    }
                } else if name == BASH_FEEDBACK_EVENT
                    && let Some(id) = data["execution_id"].as_str()
                {
                    delivered.insert(id.to_owned());
                }
            }
        }
        let mut unresolved = executions
            .into_values()
            .filter(|(_, execution)| {
                execution.mode == BashMode::Background && !delivered.contains(&execution.id)
            })
            .collect::<Vec<_>>();
        unresolved.sort_by_key(|(position, _)| *position);
        for (_, execution) in unresolved {
            let state = if execution.status == BashStatus::Running {
                "Its terminal outcome is unknown to this runtime; a historical launch receipt is not evidence that it is still running. Check any recorded results before retrying."
            } else {
                "Its terminal state was recorded, but its result was not delivered to the model."
            };
            let prompt = format!(
                "{AUTOMATED_FEEDBACK_NOTICE}\n\nBackground Bash execution from a previous Agent runtime. {state} This execution is not managed by the current runtime. Do not wait for another completion notification or automatically recreate it. Check the output and side effects before deciding whether any work remains.\n\n{}",
                serde_json::json!({
                    "execution_id": execution.id,
                    "tool_call_id": execution.tool_call_id,
                    "status": if execution.status == BashStatus::Running {
                        serde_json::json!("unknown")
                    } else {
                        serde_json::json!(execution.status)
                    },
                    "command": (!execution.command.is_empty()).then_some(&execution.command),
                    "output_path": execution.output_path,
                    "exit_code": execution.exit_code,
                    "error": execution.error,
                }),
            );
            self.transcript.append_agent_message(&prompt);
            self.record_bash_feedback(&execution.id);
        }
    }

    fn record_bash_feedback(&mut self, id: &str) {
        self.transcript
            .append_entry(crate::transcript::TranscriptEntryKind::Event {
                name: BASH_FEEDBACK_EVENT.into(),
                data: serde_json::json!({ "execution_id": id }),
            });
    }

    fn record_bash_changes(&mut self) {
        for execution in self.bash_executions.take_changes() {
            self.transcript
                .append_entry(crate::transcript::TranscriptEntryKind::Event {
                    name: BASH_EXECUTION_EVENT.into(),
                    data: serde_json::to_value(execution).expect("serializable Bash execution"),
                });
        }
    }

    pub(crate) fn set_bash_owner_mode(&mut self, mode: DelegationMode) {
        self.bash_owner_mode = mode;
    }

    pub(crate) fn bash_cleanup_pending(&self) -> bool {
        self.bash_owner_mode == DelegationMode::Foreground && self.bash_executions.has_running()
    }

    pub(crate) fn poll_bash(&mut self) {
        self.bash_executions.poll();
        self.record_bash_control_outcomes();
        if self.bash_owner_mode == DelegationMode::Foreground && self.can_start_main() {
            self.bash_executions.cancel_all();
            self.bash_executions.discard_outcomes();
        }
    }

    pub(in crate::harness::instance) fn subscribe_bash_output(
        &self,
    ) -> super::super::bash::BashOutputSubscription {
        self.bash_executions.subscribe_output()
    }

    pub(crate) fn bash_executions(&self) -> Vec<BashExecution> {
        self.bash_executions.snapshots()
    }

    pub(crate) fn background_bash(
        &mut self,
        id: &str,
    ) -> Result<BashExecution, AgentInstanceError> {
        let result = self.bash_executions.background(id).map_err(bash_error);
        self.record_bash_changes();
        result
    }

    pub(crate) fn stop_bash(&mut self, id: &str) -> Result<(), AgentInstanceError> {
        self.bash_executions.stop(id).map_err(bash_error)
    }

    pub(crate) async fn shutdown_bash(&mut self) {
        let outcomes = self.bash_executions.shutdown().await;
        self.record_bash_changes();
        self.commit_bash_control_outcomes(outcomes);
    }

    pub(super) fn service_bash_requests(&mut self) {
        while let Some(mut request) = self
            .generation
            .bash_requests
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            if request.reply.is_closed() {
                continue;
            }
            if let Err(error) = self.validate_tool_scope(&request.scope) {
                let _ = request.reply.send(Err(error));
                continue;
            }
            request.lifetime = if self
                .generation
                .child_factory
                .runtime_services
                .interactive_enabled()
            {
                BashLifetime::Interactive
            } else {
                BashLifetime::Unattended
            };
            self.bash_executions.start(request);
        }
    }

    pub(super) fn record_bash_control_outcomes(&mut self) {
        self.record_bash_changes();
        let outcomes = self.bash_executions.take_control_outcomes();
        self.commit_bash_control_outcomes(outcomes);
    }

    fn commit_bash_control_outcomes(&mut self, outcomes: std::collections::VecDeque<BashOutcome>) {
        for outcome in outcomes {
            if matches!(outcome.stop_reason, Some(BashStopReason::UserCancelled)) {
                self.transcript
                    .append_agent_message(&bash_outcome_prompt(&outcome));
                self.record_bash_feedback(&outcome.execution.id);
            }
        }
    }

    pub(super) fn start_next_bash_outcome(&mut self) -> Result<bool, PhiTurnError> {
        self.record_bash_control_outcomes();
        let Some(outcome) = self.bash_executions.next_outcome() else {
            return Ok(false);
        };
        let id = outcome.execution.id.clone();
        let Poll::Ready(result) = self.start_internal_turn(bash_outcome_prompt(outcome)) else {
            return Ok(true);
        };
        self.record_bash_feedback(&id);
        self.bash_executions.take_outcome();
        result?;
        Ok(true)
    }
}

fn bash_outcome_prompt(outcome: &BashOutcome) -> String {
    let guidance = match outcome.stop_reason {
        Some(BashStopReason::UserCancelled) => CANCELLED_EXECUTION_GUIDANCE,
        Some(BashStopReason::OwnerEnded | BashStopReason::AgentStopped) => {
            "The owning run or Agent was stopped. Do not restart this work without a new user request."
        }
        _ if outcome.execution.status != BashStatus::Completed => {
            "This execution failed or was unexpectedly interrupted; it was not explicitly cancelled by the user. Recover the interrupted work if it is still required: inspect the error, output and current state, then safely resume or retry. Avoid duplicating side effects; do not blindly rerun a non-idempotent command or bypass permissions or execution limits."
        }
        _ => {
            "The execution completed. Use its result to continue the task; do not rerun completed work."
        }
    };
    format!(
        "{AUTOMATED_FEEDBACK_NOTICE}\n\nBash execution outcome. {guidance}\n\n{}",
        serde_json::json!({
            "execution_id": outcome.execution.id,
            "tool_call_id": outcome.execution.tool_call_id,
            "status": outcome.execution.status,
            "stop_reason": outcome.stop_reason,
            "command": outcome.execution.command,
            "output_path": outcome.execution.output_path,
            "exit_code": outcome.execution.exit_code,
            "error": outcome.execution.error,
            "output": outcome.output,
        }),
    )
}

fn bash_error(message: String) -> AgentInstanceError {
    AgentInstanceError::Operation {
        source: anyhow::anyhow!(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::instance::bash::{BashReply, BashRequest};
    use crate::sandbox::Sandbox;
    use std::time::Duration;

    #[tokio::test]
    async fn failed_feedback_preserves_bash_completion() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mut state = super::super::tests::test_client().await;
        let calls = Arc::new(AtomicUsize::new(0));
        state
            .generation
            .hooks
            .register_hook(crate::hooks::Hook::prompt_context({
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err(crate::hooks::HookError::new("context unavailable")) }
                }
            }));
        let (reply, result) = tokio::sync::oneshot::channel();
        state.bash_executions.start(BashRequest {
            command: Sandbox::new(false, std::path::Path::new("/tmp"))
                .unwrap()
                .wrap_command("printf terminal-output"),
            description: "terminal output".into(),
            scope: CoreEventScope {
                turn_id: Some("turn".into()),
                parent_tool_call_id: Some("call".into()),
                ..Default::default()
            },
            mode: BashMode::Background,
            timeout: None,
            lifetime: BashLifetime::Interactive,
            reply,
        });
        let instance =
            crate::harness::instance::AgentInstance::spawn(AgentInstanceCandidate { state });
        let timeout = Duration::from_secs(5);
        let BashReply::Background(execution) = tokio::time::timeout(timeout, result)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        else {
            panic!("expected background receipt");
        };
        let settled = tokio::time::timeout(timeout, instance.wait_for_quiescence(None)).await;
        instance.stop().await.unwrap();
        std::fs::remove_file(execution.output_path).unwrap();
        let view = settled
            .expect("failed Bash feedback never settled")
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            view.snapshot
                .latest_error
                .as_ref()
                .unwrap()
                .message
                .contains("context unavailable")
        );
        let transcript = &view.transcript;
        assert!(transcript.entries.iter().any(|entry| matches!(
            &entry.kind,
            crate::transcript::TranscriptEntryKind::Event { name, data }
                if name == BASH_EXECUTION_EVENT && data["id"] == execution.id
                    && data["status"] == "completed" && data["exit_code"] == 0
        )));
        assert_eq!(
            transcript
                .entries
                .iter()
                .filter(|entry| matches!(
                    &entry.kind,
                    crate::transcript::TranscriptEntryKind::Event { name, data }
                        if name == BASH_FEEDBACK_EVENT && data["execution_id"] == execution.id
                ))
                .count(),
            1
        );
        let failures = transcript
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                crate::transcript::TranscriptEntryKind::Event { name, data }
                    if name == INTERNAL_FEEDBACK_FAILURE_EVENT =>
                {
                    Some(data)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0]["prompt"]
                .as_str()
                .unwrap()
                .contains("terminal-output")
        );
        assert!(
            failures[0]["error"]
                .as_str()
                .unwrap()
                .contains("context unavailable")
        );
        assert!(
            crate::context::convert_history(transcript)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn shutdown_preserves_user_cancellation_without_waking() {
        for user_cancelled in [false, true] {
            let mut state = super::super::tests::test_client().await;
            let (reply, mut result) = tokio::sync::oneshot::channel();
            state.bash_executions.start(BashRequest {
                command: Sandbox::new(false, std::path::Path::new("/tmp"))
                    .unwrap()
                    .wrap_command("sleep 30"),
                description: "sleep 30".into(),
                scope: CoreEventScope {
                    turn_id: Some("turn".into()),
                    parent_tool_call_id: Some("call".into()),
                    ..Default::default()
                },
                mode: BashMode::Background,
                timeout: None,
                lifetime: BashLifetime::Interactive,
                reply,
            });
            let execution = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    state.bash_executions.poll();
                    if let Ok(reply) = result.try_recv() {
                        let BashReply::Background(execution) = reply.unwrap() else {
                            panic!("expected background receipt")
                        };
                        break execution;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            if user_cancelled {
                state.stop_bash(&execution.id).unwrap();
            }
            tokio::time::timeout(Duration::from_secs(5), state.shutdown_bash())
                .await
                .unwrap();
            std::fs::remove_file(execution.output_path).unwrap();
            let history = serde_json::to_string(&state.transcript).unwrap();
            assert_eq!(history.contains("user_cancelled"), user_cancelled);
            assert_eq!(history.contains("phi_sdk.bash_feedback"), user_cancelled);
            assert!(state.can_start_main());
            assert!(!state.bash_executions.pending());
            state.shutdown_bash().await;
            assert_eq!(serde_json::to_string(&state.transcript).unwrap(), history);
        }
    }
}
