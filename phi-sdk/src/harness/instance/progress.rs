use crate::core::event::{PhiEvent, PhiEventEnvelope, PhiUsage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl ExecutionStatus {
    pub(super) fn from_failure(failure: &crate::PhiFailure) -> Self {
        if failure.kind == crate::PhiFailureKind::Cancelled {
            Self::Cancelled
        } else {
            Self::Failed
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionTool {
    pub id: String,
    pub name: String,
}

/// Facts for one outer turn, independent of checklist Task state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionProgress {
    pub turn_id: String,
    pub status: ExecutionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_reason: Option<crate::CancellationReason>,
    pub started_at: DateTime<Utc>,
    /// Sampled at an execution event or coherent view read; frozen after termination.
    pub duration_ms: u64,
    /// Committed response usage, following existing provider/transcript accounting.
    pub usage: PhiUsage,
    pub tool_uses: u64,
    pub current_tools: Vec<ExecutionTool>,
    pub last_tool: Option<ExecutionTool>,
}

pub(super) fn observe_progress(
    current: &mut Option<RunProgress>,
    envelope: &PhiEventEnvelope,
) -> Option<ExecutionProgress> {
    let started = matches!(envelope.event, PhiEvent::TurnStarted { .. });
    if started
        && let Some(turn_id) = &envelope.turn_id
        && current
            .as_ref()
            .is_none_or(|run| run.observed.turn_id != *turn_id)
    {
        *current = Some(RunProgress::new(turn_id.clone()));
    }
    let run = current.as_mut()?;
    let changed = run.observe(envelope);
    (started || changed).then(|| run.observed.clone())
}

pub(super) struct RunProgress {
    pub(super) observed: ExecutionProgress,
    started: Instant,
    tool_ids: HashSet<String>,
    usage_events: HashSet<String>,
}

impl RunProgress {
    pub(super) fn new(turn_id: String) -> Self {
        Self {
            observed: ExecutionProgress {
                turn_id,
                status: ExecutionStatus::Running,
                cancellation_reason: None,
                started_at: Utc::now(),
                duration_ms: 0,
                usage: PhiUsage::default(),
                tool_uses: 0,
                current_tools: Vec::new(),
                last_tool: None,
            },
            started: Instant::now(),
            tool_ids: HashSet::new(),
            usage_events: HashSet::new(),
        }
    }

    pub(super) fn snapshot(&self) -> ExecutionProgress {
        let mut progress = self.observed.clone();
        if progress.status == ExecutionStatus::Running {
            progress.duration_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        }
        progress
    }

    pub(super) fn finish(&mut self, status: ExecutionStatus) -> bool {
        if self.observed.status != ExecutionStatus::Running {
            return false;
        }
        self.observed = self.snapshot();
        self.observed.status = status;
        self.observed.current_tools.clear();
        self.tool_ids = HashSet::new();
        self.usage_events = HashSet::new();
        true
    }

    fn observe(&mut self, envelope: &PhiEventEnvelope) -> bool {
        if envelope.turn_id.as_deref() != Some(self.observed.turn_id.as_str())
            || self.observed.status != ExecutionStatus::Running
        {
            return false;
        }
        let changed = match &envelope.event {
            PhiEvent::ToolCallStarted { id, name, .. } if self.tool_ids.insert(id.clone()) => {
                let tool = ExecutionTool {
                    id: id.clone(),
                    name: name.clone(),
                };
                self.observed.tool_uses = self.observed.tool_uses.saturating_add(1);
                self.observed.last_tool = Some(tool.clone());
                self.observed.current_tools.push(tool);
                true
            }
            PhiEvent::ToolCallFinished { id, .. } => {
                let previous = self.observed.current_tools.len();
                self.observed.current_tools.retain(|tool| tool.id != *id);
                previous != self.observed.current_tools.len()
            }
            PhiEvent::AssistantFinished { usage, .. }
                if self.usage_events.insert(envelope.id.clone()) =>
            {
                let total = &mut self.observed.usage;
                total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
                total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
                total.cache_read = total.cache_read.saturating_add(usage.cache_read);
                total.cache_write = total.cache_write.saturating_add(usage.cache_write);
                total.cost += usage.cost;
                true
            }
            PhiEvent::TurnFinished { .. } => return self.finish(ExecutionStatus::Completed),
            PhiEvent::TurnFailed { failure } => {
                self.observed.cancellation_reason = failure.cancellation_reason;
                return self.finish(ExecutionStatus::from_failure(failure));
            }
            _ => false,
        };
        if changed {
            self.observed = self.snapshot();
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(turn_id: &str, event: PhiEvent) -> PhiEventEnvelope {
        PhiEventEnvelope {
            id: uuid::Uuid::new_v4().to_string(),
            emitted_at: Utc::now(),
            session_id: "instance".into(),
            turn_id: Some(turn_id.into()),
            message_id: None,
            agent_id: None,
            parent_agent_id: None,
            parent_tool_call_id: None,
            event,
        }
    }

    #[test]
    fn turn_start_preserves_current_run_and_resets_the_next() {
        let mut current = None;
        let mut start = event(
            "first",
            PhiEvent::TurnStarted {
                submission_id: None,
                prompt: "test".into(),
                source: "sdk".into(),
            },
        );
        let initial = observe_progress(&mut current, &start).unwrap();
        let usage = event(
            "first",
            PhiEvent::AssistantFinished {
                id: "response".into(),
                content: Vec::new(),
                response: String::new(),
                display: String::new(),
                usage: PhiUsage {
                    input_tokens: 5,
                    ..Default::default()
                },
                has_tool_calls: false,
            },
        );
        observe_progress(&mut current, &usage).unwrap();
        let repeated = observe_progress(&mut current, &start).unwrap();
        assert_eq!(repeated.started_at, initial.started_at);
        assert_eq!(repeated.usage.input_tokens, 5);

        current.as_mut().unwrap().finish(ExecutionStatus::Completed);
        let terminal = observe_progress(&mut current, &start).unwrap();
        assert_eq!(terminal.status, ExecutionStatus::Completed);
        assert_eq!(terminal.usage.input_tokens, 5);

        start.turn_id = Some("next".into());
        let next = observe_progress(&mut current, &start).unwrap();
        assert_eq!(next.turn_id, "next");
        assert_eq!(next.status, ExecutionStatus::Running);
        assert_eq!(next.usage, PhiUsage::default());
        assert_eq!(next.duration_ms, 0);
        assert!(observe_progress(&mut current, &usage).is_none());
    }

    #[test]
    fn duplicate_and_foreign_events_never_inflate_or_reopen_progress() {
        for tool_count in [0, 1, 2, 16] {
            for repetitions in [1, 2, 8] {
                let mut run = RunProgress::new("first".into());
                let responses: Vec<_> = (0..2)
                    .map(|_| {
                        event(
                            "first",
                            PhiEvent::AssistantFinished {
                                id: "reused-provider-id".into(),
                                content: Vec::new(),
                                response: String::new(),
                                display: String::new(),
                                usage: PhiUsage {
                                    input_tokens: 5,
                                    output_tokens: 2,
                                    ..Default::default()
                                },
                                has_tool_calls: true,
                            },
                        )
                    })
                    .collect();
                for _ in 0..repetitions {
                    for index in 0..tool_count {
                        let call = event(
                            "first",
                            PhiEvent::ToolCallStarted {
                                id: index.to_string(),
                                provider_call_id: None,
                                provider_item_id: None,
                                name: "Read".into(),
                                args: serde_json::json!({}),
                                display: None,
                            },
                        );
                        run.observe(&call);
                    }
                    for response in &responses {
                        run.observe(response);
                    }
                }
                assert_eq!(run.observed.tool_uses, tool_count);
                assert_eq!(run.observed.current_tools.len() as u64, tool_count);
                assert_eq!(run.observed.usage.input_tokens, 10);
                assert_eq!(run.observed.usage.output_tokens, 4);
                assert!(!run.observe(&event(
                    "other",
                    PhiEvent::TurnFinished {
                        response: String::new(),
                        display: String::new(),
                        usage: PhiUsage::default(),
                        compacted: false,
                    }
                )));
                assert!(run.finish(ExecutionStatus::Failed));
                let terminal = run.snapshot();
                assert!(!run.finish(ExecutionStatus::Completed));
                assert!(terminal.current_tools.is_empty());
                assert!(!run.observe(&responses[0]));
                assert_eq!(run.snapshot(), terminal);
            }
        }
    }
}
