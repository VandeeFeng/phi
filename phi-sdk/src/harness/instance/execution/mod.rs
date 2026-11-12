use crate::harness::agentic_loop::{EventPipeline, envelope_core_events};

use crate::agent::custom::{AgentCatalog, AgentDefinition};
use crate::agent::options::AgentOptions;
use crate::agent::tools::AgentToolRequest;
use crate::config::RuntimeConfig;
use crate::context::{PromptPreparation, PromptPreparationError};
use crate::core::event::{
    CoreEvent, CoreEventEnvelope, CoreEventScope, PhiEvent, PhiEventEnvelope,
};
use crate::core::lifecycle::{UserPromptDispatch, UserPromptSubmitSource};
use crate::core::turn::{
    CoreTurnPreparation, ImageInputError, PreparedTurnPrompt, SubmissionId, TurnPrompt,
};
#[cfg(test)]
use crate::core::turn::{InjectedContext, QueuedSubmissionDrainMode};
use crate::extension::memory::{MemoryAgentKind, MemoryDispatch, MemorySubmission};
use crate::harness::agentic_loop::AgentRunner;
pub(crate) use crate::harness::instance::builder::{
    AgentInstanceBuilder, AgentInstanceCandidate, DirectedDelegation, RuntimeCandidate,
};
use crate::harness::instance::child::{
    ChildResult, SubagentActivity, SubagentAdmission, SubagentSnapshot, SubagentStatus,
};
use crate::harness::instance::handle::AgentInstanceError;
use crate::harness::instance::handle::{AgentInstanceId, PhiStreamError};
pub(crate) use crate::harness::instance::handle::{SubmissionDispatch, SubmissionDisposition};
use crate::harness::manager::preparation::{
    AgentCreation, AgentSpawnRequest, DelegationMode, SubagentPreparation,
};
use crate::harness::task::{TaskAccessError, TaskListAccess};
use crate::hooks::HookRegistry;
use crate::provider::{AnyClient, AnyRuntime};
use crate::tools::ToolDisplayRegistry;
#[cfg(test)]
use crate::transcript::ContextSource;
use crate::transcript::Transcript;
use compact_str::CompactString;
pub use delegation::PhiSubagentError;
use futures::{FutureExt, future::BoxFuture};
use std::sync::{Arc, OnceLock};
use std::task::Poll;

// Private execution state and ordinary turn/delegation progression for one actor.

mod bash;
mod runtime;
mod submission;
mod teammate;
mod workflow;
pub use submission::PhiTurnError;
use submission::{MAX_QUEUED_SUBMISSIONS, record_prepared_prompt, start_prepared_turn_with_role};
pub(crate) use submission::{SubmissionIntent, SubmissionPreparation};
pub(crate) use workflow::workflow_report_delivered;

use runtime::RuntimeSessionServices;
pub(crate) use runtime::WorkflowScope;
mod delegation;
pub(crate) use teammate::TeammateDecision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptPromptRole {
    User,
    Agent,
}

/// Result of starting an outer turn: one user-prompt-to-stop response that
/// may contain multiple agent-loop turns and tool batches.
pub(crate) struct TurnStart {
    pub(crate) submission_id: Option<SubmissionId>,
    pub(crate) turn_id: CompactString,
    pub(crate) prompt: String,
    pub(crate) runner: AgentRunner,
}

const SUBAGENT_CANCELLATION_MESSAGE: &str = "subagent cancelled";
const INTERNAL_FEEDBACK_FAILURE_EVENT: &str = "phi_sdk.internal_feedback_failed";
const AUTOMATED_FEEDBACK_NOTICE: &str =
    "Automated harness notification, not human input or approval.";
// Inspired by Claude Code's user-stopped subagent boundary, not its documented prompt text:
// https://code.claude.com/docs/en/sub-agents#resume-subagents
// TODO: refine Phi's cross-command/Agent recreation guidance; it is not semantic enforcement.
const CANCELLED_EXECUTION_GUIDANCE: &str = "The user explicitly cancelled this execution. Do not automatically retry or recreate the cancelled work through another command, Agent, or your own actions. This cancellation does not by itself withdraw the overall user goal; continue other requested work. Resume the cancelled work only when a newer user instruction requests it; ask if that intent is unclear. An Agent message is not renewed user consent.";
const USER_CANCELLED_AGENT_MESSAGE: &str = "This Agent was cancelled by the user. Agent messages and internal continuations cannot resume it. A direct user or host request must resume it first; do not create a replacement to bypass the cancellation.";

#[derive(Clone, Copy, PartialEq, Eq)]
enum AutomaticTurnPolicy {
    Enabled,
    UserCancelled,
}

struct ActiveDirectedDelegation {
    turn_id: String,
    constraint: DirectedDelegation,
    admitted: Arc<OnceLock<DelegationMode>>,
    attempted: bool,
    instance_id: Option<AgentInstanceId>,
}

struct DelegatedAgent {
    definition: AgentDefinition,
    scope: CoreEventScope,
    initial_completion: crate::harness::instance::child::InitialCompletion,
    outcome: Option<Result<String, crate::PhiFailure>>,
}

struct PendingInternalPrompt {
    prompt: String,
    preparation: BoxFuture<'static, Result<CoreTurnPreparation, PromptPreparationError>>,
}

struct PendingMessagePrompt {
    message: Arc<crate::harness::manager::message::MessageRoute>,
    dispatch: UserPromptDispatch,
    preparation: BoxFuture<'static, Option<Result<CoreTurnPreparation, PromptPreparationError>>>,
}

pub(crate) struct AgentInstanceState {
    pub(crate) delegated_child: bool,
    teammate_stop_requested: bool,
    automatic_turns: AutomaticTurnPolicy,
    pub(crate) task_list: Option<crate::tasks::TaskListBinding>,
    orchestration: Option<crate::harness::manager::policy::InstanceAgentScope>,
    workflow_scopes: Vec<std::sync::Weak<()>>,
    workflow_feedback: std::collections::VecDeque<workflow::WorkflowFeedback>,
    pub(crate) transcript: Transcript,
    pending_internal_prompt: Option<PendingInternalPrompt>,
    pending_message_prompt: Option<PendingMessagePrompt>,
    pub(super) preparing_submission: bool,
    directed_delegation: Option<ActiveDirectedDelegation>,
    delegated_agent: Option<DelegatedAgent>,
    pub(crate) generation: RuntimeGeneration,
    main_run: Option<EventPipeline>,
    bash_executions: crate::harness::instance::bash::BashExecutions,
    bash_owner_mode: DelegationMode,
    pending_creations: std::collections::VecDeque<delegation::PendingCreation>,
    cancelled_admission_tools: Vec<(String, crate::CancellationReason)>,
    agent_tool_replies: std::collections::HashMap<
        AgentInstanceId,
        tokio::sync::oneshot::Sender<Result<crate::agent::tools::AgentToolOutput, String>>,
    >,
    child_workers: std::collections::HashMap<AgentInstanceId, ChildWorker>,
    child_worker_messages: tokio::sync::mpsc::UnboundedReceiver<ChildWorkerMessage>,
    child_worker_sender: tokio::sync::mpsc::UnboundedSender<ChildWorkerMessage>,
    child_observations: std::collections::HashMap<AgentInstanceId, ChildObservation>,
    completed_child_ids: std::collections::VecDeque<AgentInstanceId>,
    admitted_subagent_outcomes: std::collections::VecDeque<delegation::AdmittedSubagentOutcome>,
    observer_subagent_activity: std::collections::VecDeque<SubagentActivity>,
    queued_steer_submissions:
        std::collections::VecDeque<crate::harness::agentic_loop::AgentSteerSubmission>,
    follow_up_submissions: std::collections::VecDeque<QueuedSubmission>,
    // Steer can be runner-owned; retain only identity until canonical commit or discard.
    pending_submissions: std::collections::VecDeque<crate::core::turn::SubmissionId>,
}

struct QueuedSubmission {
    prompt: PreparedTurnPrompt,
    memory: MemoryDispatch,
    message: Option<std::sync::Arc<crate::harness::manager::message::MessageRoute>>,
}

/// Process-local observation; canonical history stays with the child instance.
struct ChildObservation {
    status: SubagentStatus,
    completion: crate::harness::instance::child::InitialCompletion,
}

struct ChildWorker {
    creation: AgentCreation,
    handle: tokio::task::JoinHandle<()>,
    cancellation: tokio::sync::watch::Sender<bool>,
}

enum ChildWorkerMessage {
    Admitted(AgentInstanceId),
    Finished(ChildResult),
}

#[cfg(test)]
mod tests;

pub(crate) struct RuntimeGeneration {
    team_capability: crate::agent_manager::TeamCapability,
    client: AnyClient,
    runtime: AnyRuntime,
    descriptor: crate::models::ModelDescriptor,
    catalog: Arc<crate::models::ModelCatalog>,
    agent_options: AgentOptions,
    cfg: RuntimeConfig,
    services: RuntimeSessionServices,
    hooks: HookRegistry,
    tool_display: Arc<ToolDisplayRegistry>,
    agents: AgentCatalog,
    interactions: crate::harness::instance::interaction::InteractionBroker,
    agent_requests: Option<tokio::sync::mpsc::UnboundedReceiver<AgentToolRequest>>,
    task_requests:
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::agent::tools::task::TaskToolRequest>>,
    bash_requests:
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::harness::instance::bash::BashRequest>>,
    subagent_preparation: Option<Arc<dyn SubagentPreparation>>,
    child_factory: crate::factory::Phi,
    pub(crate) resources: crate::factory::AgentRuntimeResources,
}

impl AgentInstanceState {
    pub(crate) fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub(crate) fn reset_context(
        &mut self,
        handoff: Option<String>,
    ) -> anyhow::Result<compact_str::CompactString> {
        self.transcript.reset_context(handoff)
    }

    pub(crate) fn edit_context(
        &mut self,
        target_id: &str,
        replacement: Option<crate::transcript::ContextReplacement>,
    ) -> anyhow::Result<compact_str::CompactString> {
        self.transcript.append_context_edit(target_id, replacement)
    }

    fn validate_prompt(
        &self,
        prompt: &mut PreparedTurnPrompt,
    ) -> Result<Vec<crate::hooks::HookNotice>, PhiTurnError> {
        self.generation
            .runtime
            .validate_turn_policy(&prompt.tool_policy)
            .map_err(|source| PhiTurnError::InvalidToolPolicy { source })?;
        Ok(std::mem::take(&mut prompt.hook_notices))
    }

    fn prompt_preparation(&self) -> PromptPreparation {
        PromptPreparation {
            hooks: self.generation.hooks.clone(),
            expander: self.generation.child_factory.prompt_expander.clone(),
            transcript: Arc::new(self.transcript.clone()),
            working_dir: self.working_dir().to_path_buf(),
            delegated_child: self.delegated_child,
        }
    }
}

impl AgentInstanceState {
    fn abort_main(&mut self) -> Vec<CoreEventEnvelope> {
        let Some(mut active) = self.main_run.take() else {
            return Vec::new();
        };
        let events = active.abort(
            &mut self.transcript,
            &self.generation.descriptor.provider,
            &self.generation.descriptor.id,
        );
        self.record_failures(&events);
        events
    }

    pub(super) fn record_failure(&mut self, turn_id: Option<&str>, failure: &crate::PhiFailure) {
        self.transcript
            .append_entry(crate::transcript::TranscriptEntryKind::Failure {
                turn_id: turn_id.map(CompactString::new),
                failure: failure.clone(),
            });
    }

    fn record_failures(&mut self, events: &[CoreEventEnvelope]) {
        for envelope in events {
            if let CoreEvent::TurnFailed { failure } = &envelope.event {
                self.record_failure(envelope.scope.turn_id.as_deref(), failure);
            }
        }
    }

    fn start_main(&mut self, pipeline: EventPipeline) {
        self.pending_internal_prompt = None;
        self.abort_main();
        self.main_run = Some(pipeline);
    }

    fn activate_turn(
        &mut self,
        start: TurnStart,
        source: UserPromptSubmitSource,
    ) -> SubmissionDisposition {
        let TurnStart {
            submission_id,
            turn_id,
            runner,
            prompt,
        } = start;
        let scope = CoreEventScope {
            session_id: Some(self.transcript.id.clone()),
            turn_id: Some(turn_id.clone()),
            ..CoreEventScope::default()
        };
        let mut pipeline = EventPipeline::new(runner, scope).with_compaction(
            self.generation.client.clone(),
            crate::context::HistoryTarget {
                provider: self.generation.descriptor.provider.clone(),
                api: Some(self.generation.descriptor.api),
                model: self.generation.descriptor.id.clone(),
            },
            self.generation.cfg.clone(),
            self.generation.hooks.clone(),
        );
        if let Some(id) = &submission_id {
            self.pending_submissions.retain(|pending| pending != id);
        }
        pipeline.push_pending_event(CoreEvent::TurnStarted {
            submission_id,
            prompt: prompt.clone().into(),
            source,
        });
        self.cancelled_admission_tools.clear();
        self.start_main(pipeline);
        SubmissionDisposition::Started {
            turn_id: turn_id.to_string(),
            prompt,
        }
    }

    pub(crate) async fn drain_phi_events(
        &mut self,
    ) -> Result<(Vec<PhiEventEnvelope>, Option<PhiStreamError>), PhiStreamError> {
        let (events, failure) = self.drain_events().await?;
        Ok((self.project_phi_events(events), failure))
    }

    fn project_phi_events(&self, events: Vec<CoreEventEnvelope>) -> Vec<PhiEventEnvelope> {
        events
            .into_iter()
            .map(PhiEventEnvelope::from)
            .map(|mut envelope| {
                if let PhiEvent::ToolCallStarted {
                    name,
                    args,
                    display,
                    ..
                } = &mut envelope.event
                {
                    *display = self
                        .tool_display()
                        .format_tool_display(name, &args.to_string())
                        .ok()
                        .flatten();
                }
                envelope
            })
            .collect()
    }

    pub(crate) async fn drain_events(
        &mut self,
    ) -> Result<(Vec<CoreEventEnvelope>, Option<PhiStreamError>), PhiStreamError> {
        self.service_agent_tool_requests();
        self.service_task_tool_requests();
        self.service_bash_requests();
        self.bash_executions.poll();
        self.record_bash_control_outcomes();
        self.progress_pending_creations().await;
        self.progress_child_workers();
        let check_completion = self.completion_check();
        let mut events = match &mut self.main_run {
            Some(active) => {
                active.progress_round(
                    &mut self.transcript,
                    &self.generation.hooks,
                    check_completion,
                );
                active.drain(&mut self.transcript).await
            }
            None => Vec::new(),
        };
        self.service_agent_tool_requests();
        self.service_task_tool_requests();
        if let Some(agent) = self.delegated_agent.as_mut() {
            for envelope in &events {
                if envelope.scope.turn_id != agent.scope.turn_id {
                    continue;
                }
                match &envelope.event {
                    CoreEvent::TurnFinished { response, .. } => {
                        agent.outcome = Some(Ok(response.to_string()))
                    }
                    CoreEvent::TurnFailed { failure } => agent.outcome = Some(Err(failure.clone())),
                    _ => {}
                }
            }
        }
        self.retire_submission_events(&events);
        self.resolve_directed_outcome(&mut events);
        self.record_failures(&events);
        self.finish_main_if_closed().await?;
        let failure = if self.can_start_main() {
            self.dispatch_next_queued_turn(&mut events)
                .err()
                .map(PhiStreamError::from)
        } else {
            None
        };
        Ok((events, failure))
    }

    async fn finish_main_if_closed(&mut self) -> Result<(), PhiStreamError> {
        let steering = match self.main_run.as_ref() {
            Some(active) if active.is_closed() => active
                .close_and_take_pending_steering()
                .await
                .map_err(|source| PhiStreamError::Operation { source })?,
            _ => return Ok(()),
        };
        self.teammate_stop_requested = self
            .main_run
            .as_ref()
            .is_some_and(|run| run.stop_requested());
        self.main_run = None;
        if self.bash_owner_mode == DelegationMode::Foreground {
            self.shutdown_bash().await;
        }
        self.generation.services.memory.end_turn();
        for steer in steering.into_iter().rev() {
            self.queued_steer_submissions.push_front(steer);
        }
        Ok(())
    }

    fn start_recorded_turn(
        &mut self,
        prompt: PreparedTurnPrompt,
        transcript_role: TranscriptPromptRole,
    ) -> Result<SubmissionDisposition, PhiTurnError> {
        if self.delegated_agent.is_some() {
            return self.start_specialized_turn(prompt, transcript_role);
        }
        self.transcript
            .effective_context()
            .map_err(|source| PhiTurnError::Dispatch { source })?;
        let source = prompt.source;
        let start = start_prepared_turn_with_role(
            prompt,
            &mut self.transcript,
            &self.generation.runtime,
            self.generation.hooks.clone(),
            transcript_role,
        );
        Ok(self.activate_turn(start, source))
    }

    fn start_specialized_turn(
        &mut self,
        prompt: PreparedTurnPrompt,
        transcript_role: TranscriptPromptRole,
    ) -> Result<SubmissionDisposition, PhiTurnError> {
        let agent = self.delegated_agent.as_ref().expect("specialized instance");
        let mut scope = agent.scope.clone();
        let turn_id = uuid::Uuid::new_v4().to_string();
        scope.turn_id = Some(turn_id.clone().into());
        scope.parent_tool_call_id = None;
        self.transcript
            .effective_context()
            .map_err(|source| PhiTurnError::Dispatch { source })?;
        let pipeline = self
            .spawn_agent_run(&agent.definition, &prompt, scope.clone())
            .map_err(|source| PhiTurnError::Dispatch { source })?;
        record_prepared_prompt(&mut self.transcript, &prompt, transcript_role);
        if let Some(id) = &prompt.submission_id {
            self.pending_submissions.retain(|pending| pending != id);
        }
        self.activate_specialized_run(scope, pipeline);
        Ok(SubmissionDisposition::Started {
            turn_id,
            prompt: prompt.prompt,
        })
    }

    fn activate_specialized_run(&mut self, scope: CoreEventScope, pipeline: EventPipeline) {
        let agent = self.delegated_agent.as_mut().expect("specialized instance");
        agent.scope = scope;
        agent.outcome = None;
        if agent.initial_completion.is_complete() {
            self.bash_owner_mode = DelegationMode::Background;
        }
        self.cancelled_admission_tools.clear();
        self.start_main(pipeline);
    }

    pub(crate) fn suspend_automatic_turns(&mut self) {
        self.pending_internal_prompt = None;
        self.automatic_turns = AutomaticTurnPolicy::UserCancelled;
    }

    fn start_internal_turn(&mut self, prompt: String) -> Poll<Result<(), PhiTurnError>> {
        if self.automatic_turns == AutomaticTurnPolicy::UserCancelled {
            self.pending_internal_prompt = None;
            self.transcript.append_agent_message(&prompt);
            return Poll::Ready(Ok(()));
        }
        if self
            .pending_internal_prompt
            .as_ref()
            .is_none_or(|pending| pending.prompt != prompt)
        {
            let preparation = self.prompt_preparation();
            let turn_prompt = TurnPrompt {
                prompt: prompt.clone(),
                source: UserPromptSubmitSource::Internal,
                images: Vec::new(),
                injected_context: Vec::new(),
                tool_policy: Default::default(),
            };
            self.pending_internal_prompt = Some(PendingInternalPrompt {
                prompt,
                preparation: Box::pin(
                    preparation.prepare(turn_prompt, UserPromptDispatch::Immediate),
                ),
            });
        }
        let pending = self
            .pending_internal_prompt
            .as_mut()
            .expect("internal prompt initialized");
        // Hook waits must not hold the actor; its pump polls preparation again.
        let Some(prepared) = pending.preparation.as_mut().now_or_never() else {
            return Poll::Pending;
        };
        let pending = self
            .pending_internal_prompt
            .take()
            .expect("prepared feedback");
        let result = self.finish_internal_prompt(prepared);
        if let Err(error) = &result {
            // Retain rejected work without exposing it as model input.
            self.transcript
                .append_entry(crate::transcript::TranscriptEntryKind::Event {
                    name: INTERNAL_FEEDBACK_FAILURE_EVENT.into(),
                    data: serde_json::json!({"prompt": pending.prompt, "error": error.to_string()}),
                });
        }
        Poll::Ready(result)
    }

    fn finish_internal_prompt(
        &mut self,
        prepared: Result<CoreTurnPreparation, PromptPreparationError>,
    ) -> Result<(), PhiTurnError> {
        let registration = self.orchestration.clone();
        let admission = registration
            .as_ref()
            .map(|scope| scope.finalization.borrow());
        if admission.as_ref().is_some_and(|status| {
            !matches!(
                **status,
                crate::harness::manager::AgentFinalizationStatus::Open
            )
        }) {
            return Err(PhiTurnError::Closing);
        }
        if let CoreTurnPreparation::Ready(mut prompt) =
            prepared.map_err(|source| PhiTurnError::Preparation { source })?
        {
            let notices = self.validate_prompt(&mut prompt)?;
            self.start_recorded_turn(prompt, TranscriptPromptRole::Agent)?;
            self.generation.hooks.record_notices(notices);
        }
        Ok(())
    }
}

impl AgentInstanceState {
    pub(crate) fn is_idle(&self) -> bool {
        self.can_start_main()
            && self.queued_steer_submissions.is_empty()
            && self.follow_up_submissions.is_empty()
            && self.admitted_subagent_outcomes.is_empty()
            && self.workflow_feedback.is_empty()
            && self.pending_creations.is_empty()
            && !self.bash_executions.pending()
    }

    pub(crate) fn can_start_main(&self) -> bool {
        self.main_run.is_none()
    }

    pub(crate) fn has_pending_execution(
        &self,
    ) -> Result<bool, crate::harness::manager::AgentManagerError> {
        if self.bash_executions.pending() {
            return Ok(true);
        }
        self.orchestration
            .as_ref()
            .map_or(Ok(false), |scope| scope.has_pending_execution())
    }

    pub(crate) fn needs_progress(&self) -> bool {
        self.main_run.is_some()
            || self.pending_message_prompt.is_some()
            || self.bash_executions.pending()
            || !self.child_workers.is_empty()
            || !self.pending_creations.is_empty()
            || !self.queued_steer_submissions.is_empty()
            || !self.follow_up_submissions.is_empty()
            || !self.admitted_subagent_outcomes.is_empty()
            || !self.workflow_feedback.is_empty()
    }

    pub(super) fn active_turn_id(&self) -> Option<String> {
        self.main_run
            .as_ref()
            .and_then(EventPipeline::turn_id)
            .map(ToString::to_string)
    }

    pub(crate) fn abort(&mut self) -> Vec<PhiEventEnvelope> {
        self.pending_internal_prompt = None;
        if let Some(pending) = self.pending_message_prompt.take() {
            pending.message.release_admission();
        }
        if self.bash_owner_mode == DelegationMode::Foreground {
            self.bash_executions.cancel_all();
        } else {
            self.bash_executions.cancel_foreground();
        }
        self.service_agent_tool_requests();
        self.progress_child_workers();
        self.commit_ready_main_events();
        self.cancel_pending_creations();
        self.finalize_agent_admissions();
        self.cancel_pending_admissions();
        let mut events = self.abort_main();
        self.retire_submission_events(&events);
        self.queued_steer_submissions.clear();
        for submission in self.follow_up_submissions.drain(..) {
            if let Some(message) = submission.message {
                message.release_admission();
            }
        }
        events.extend(envelope_core_events(
            &CoreEventScope {
                session_id: Some(self.transcript.id.clone()),
                ..Default::default()
            },
            self.pending_submissions
                .drain(..)
                .map(|submission_id| CoreEvent::SubmissionDiscarded {
                    submission_id,
                    reason: crate::core::turn::SubmissionDiscardReason::Cancelled,
                })
                .collect(),
        ));
        self.directed_delegation = None;
        self.teammate_stop_requested = false;
        self.generation.services.memory.end_turn();
        self.project_phi_events(events)
    }

    pub(crate) fn shutdown(&mut self) -> Vec<PhiEventEnvelope> {
        let events = self.abort();
        self.flush_workflow_feedback();
        self.discard_subagent_outcomes();
        let ids = self.child_workers.keys().cloned().collect::<Vec<_>>();
        for id in ids {
            self.cancel_subagent(&id, crate::CancellationReason::AgentStopped);
        }
        events
    }

    fn commit_ready_main_events(&mut self) {
        if let Some(active) = self.main_run.as_mut() {
            let committed = active.commit_ready(&mut self.transcript);
            self.pending_submissions
                .retain(|id| !committed.contains(id));
        }
    }

    #[cfg(test)]
    pub(crate) fn install_test_main_run(
        &mut self,
        turn_id: &str,
    ) -> tokio::sync::mpsc::Sender<crate::core::event::AgentEvent> {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(8);
        let scope = CoreEventScope {
            session_id: Some(self.transcript.id.clone()),
            turn_id: Some(turn_id.into()),
            ..CoreEventScope::default()
        };
        self.start_main(EventPipeline::new(
            crate::harness::agentic_loop::AgentRunner {
                tool_policy: None,
                round_requests: None,
                event_rx,
                steering: Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                    crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
                )),
                task: tokio::spawn(async {}),
            },
            scope,
        ));
        event_tx
    }
}

impl AgentInstanceState {
    fn validate_tool_scope(&self, scope: &CoreEventScope) -> Result<(), String> {
        let caller = self.transcript.id.as_str();
        let valid_agent = scope.agent_id.as_deref().is_none_or(|id| id == caller);
        let active_turn = self
            .main_run
            .as_ref()
            .is_some_and(|run| run.matches_turn(scope));
        if scope.session_id.as_deref() != Some(caller) || !valid_agent || !active_turn {
            return Err("tool has invalid execution scope".into());
        }
        if scope
            .parent_tool_call_id
            .as_ref()
            .is_none_or(|id| id.is_empty())
        {
            return Err("tool requires trusted tool-call identity".into());
        }
        Ok(())
    }

    fn service_task_tool_requests(&mut self) {
        while let Some(request) = self
            .generation
            .task_requests
            .as_mut()
            .and_then(|receiver| receiver.try_recv().ok())
        {
            self.start_task_tool_request(request);
        }
    }

    fn start_task_tool_request(&self, request: crate::agent::tools::task::TaskToolRequest) {
        if request.reply.is_closed() || request.lifetime.is_closed() {
            return;
        }
        let access = match self.task_list_access(&request.scope) {
            Ok(access) => access.with_invocation(request.lifetime),
            Err(error) => {
                let _ = request.reply.send(Err(error.to_string()));
                return;
            }
        };
        let permission = self.generation.resources.permission().clone();
        let mut runtime = self.generation.runtime.clone();
        if let Some(agent) = &self.delegated_agent {
            runtime = runtime.tool_policy(
                agent.definition.tools_policy().map(<[String]>::to_vec),
                crate::extension::delegated_tool_denials(
                    agent.definition.disallowed_tools_policy().to_vec(),
                ),
            );
        }
        let hooks = self.generation.hooks.clone();
        tokio::spawn(async move {
            let mut reply = request.reply;
            let operation = async move {
                if !runtime.is_tool_available(crate::agent::tools::task::TASK_TOOL_NAME) {
                    anyhow::bail!("Task tool is unavailable in this execution scope");
                }
                let prepared = request.operation.prepare(access)?;
                crate::agent::tools::task::authorize_task(&permission, prepared.request()).await?;
                Ok::<_, anyhow::Error>(prepared.execute(hooks).await?)
            };
            let operation =
                crate::permission::execution_scope::OuterTurnPolicy::with_captured_async(
                    request.policy,
                    operation,
                );
            tokio::select! {
                biased;
                _ = reply.closed() => {},
                result = crate::core::interaction::with_interaction_scope(request.scope, operation) => {
                    let _ = reply.send(result.map_err(|error| error.to_string()));
                }
            }
        });
    }

    fn task_list_access(&self, scope: &CoreEventScope) -> Result<TaskListAccess, TaskAccessError> {
        use crate::harness::manager::AgentManagerError;

        self.validate_tool_scope(scope)
            .map_err(TaskAccessError::InvalidToolScope)?;
        let access = self
            .orchestration
            .as_ref()
            .ok_or(AgentManagerError::Unavailable)?
            .access()?
            .task_list()?;
        if Some(access.validate()?) != self.task_list.as_ref() {
            return Err(TaskAccessError::BindingChanged);
        }
        Ok(access)
    }
}

#[cfg(test)]
mod task_tool_tests {
    use super::*;
    use crate::harness::instance::builder::AgentInstanceCandidate;
    use crate::harness::instance::execution::tests::test_client;
    use std::sync::Arc;

    #[tokio::test]
    async fn task_tool_validates_scope() {
        use crate::harness::manager::policy::InstanceAgentScope;
        use crate::harness::manager::{AgentManager, AgentManagerError};

        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-task-tool-scope-{}", uuid::Uuid::new_v4()));
        let binding = crate::tasks::TaskListBinding::new(&root, "shared").unwrap();
        let mut state = test_client().await;
        state.task_list = Some(binding.clone());
        let _events = state.install_test_main_run("active-turn");
        let scope = CoreEventScope {
            session_id: Some(state.transcript.id.clone()),
            agent_id: Some(state.transcript.id.clone()),
            turn_id: Some("active-turn".into()),
            parent_tool_call_id: Some("trusted-call".into()),
            ..Default::default()
        };
        for field in ["session", "agent", "turn", "tool"] {
            let mut forged = scope.clone();
            match field {
                "session" => forged.session_id = Some("other-session".into()),
                "agent" => forged.agent_id = Some("other-agent".into()),
                "turn" => forged.turn_id = Some("old-turn".into()),
                "tool" => forged.parent_tool_call_id = None,
                _ => unreachable!(),
            }
            assert!(matches!(
                state.task_list_access(&forged),
                Err(TaskAccessError::InvalidToolScope(_))
            ));
        }
        assert!(matches!(
            state.task_list_access(&scope),
            Err(TaskAccessError::Manager(AgentManagerError::Unavailable))
        ));

        let manager = AgentManager::new();
        let mut candidate = AgentInstanceCandidate {
            state: test_client().await,
        };
        candidate.state.transcript = state.transcript.clone();
        candidate.state.task_list = Some(binding.clone());
        let instance = manager.insert(candidate).unwrap();
        let epoch = manager
            .lock_live()
            .unwrap()
            .member(instance.id())
            .unwrap()
            .epoch;
        state.orchestration = Some(InstanceAgentScope::new(
            Arc::downgrade(&manager.registry),
            instance.id().clone(),
            epoch,
        ));
        let tasks = state.task_list_access(&scope).unwrap();
        assert_eq!(tasks.validate().unwrap(), &binding);
        for selection in [
            None,
            Some(crate::tasks::TaskListBinding::new(&root, "other").unwrap()),
            Some(crate::tasks::TaskListBinding::new(root.join("other"), "shared").unwrap()),
        ] {
            state.task_list = selection;
            assert!(matches!(
                state.task_list_access(&scope),
                Err(TaskAccessError::BindingChanged)
            ));
        }
        state.task_list = Some(binding.clone());
        assert!(state.task_list_access(&scope).is_ok());
        for tool_call_id in [None, Some("".into())] {
            let mut invalid = scope.clone();
            invalid.parent_tool_call_id = tool_call_id;
            assert!(matches!(
                state.task_list_access(&invalid),
                Err(TaskAccessError::InvalidToolScope(_))
            ));
        }
        state.abort();
        assert!(matches!(
            state.task_list_access(&scope),
            Err(TaskAccessError::InvalidToolScope(_))
        ));
        let _next_events = state.install_test_main_run("next-turn");
        assert!(matches!(
            state.task_list_access(&scope),
            Err(TaskAccessError::InvalidToolScope(_))
        ));
        let mut next_scope = scope;
        next_scope.turn_id = Some("next-turn".into());
        assert!(state.task_list_access(&next_scope).is_ok());
        let closing = manager.lock().unwrap().close_members(|_| true);
        assert!(matches!(
            tasks.validate(),
            Err(TaskAccessError::InactiveCaller)
        ));
        assert!(matches!(
            state.task_list_access(&next_scope),
            Err(TaskAccessError::InactiveCaller)
        ));
        state.abort();
        manager.stop_members(closing).await.unwrap();
        assert!(!root.exists());
    }
}
