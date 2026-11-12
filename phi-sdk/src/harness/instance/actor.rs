use super::execution::{
    AgentInstanceState, RuntimeCandidate, SubmissionDispatch, SubmissionDisposition,
    SubmissionIntent, SubmissionPreparation,
};
use super::progress::{ExecutionStatus, RunProgress, observe_progress};
use crate::core::event::{PhiEvent, PhiEventEnvelope};
use crate::core::interaction::{InteractionAnswer, InteractionEnvelope, InteractionResponse};
use crate::core::turn::TurnPrompt;
use crate::harness::instance::child::{SubagentSnapshot, SubagentStatus};
use crate::harness::instance::execution::TeammateDecision;
use crate::harness::instance::execution::{PhiSubagentError, PhiTurnError};
use crate::harness::instance::handle::AgentInstanceError;
use crate::harness::instance::handle::{
    AgentInstanceEvent, AgentInstanceId, AgentInstanceSnapshot, AgentInstanceStatus,
    AgentInstanceView, LiveRunSnapshot, PhiStreamError, snapshot_is_current,
};
use crate::harness::manager::preparation::{AgentCreation, AgentSpawnRequest};
use crate::harness::task::TeammateTasks;
use crate::tools::ToolDisplayRegistry;
use crate::transcript::Transcript;
use futures::FutureExt;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

// Serializes commands, execution progression, interactions and initial completion.

const PUMP_INTERVAL: Duration = Duration::from_millis(20);
const CHILD_CANCELLATION_MESSAGE: &str = "subagent cancelled";
const QUIESCENCE_WAIT_LIMIT: usize = 64;
const MAX_PENDING_INPUTS: usize = 64;

const MAX_TEAMMATE_CONTINUATIONS: usize = 8;
const IDLE_WORK_POLL_INTERVAL: Duration = Duration::from_secs(1);

type BoundaryResult = anyhow::Result<(TeammateTasks, TeammateDecision)>;

#[derive(Default)]
struct TeammateState {
    pending: bool,
    next_poll: Option<tokio::time::Instant>,
    last_turn: Option<String>,
    automatic_turn: Option<String>,
    continuations: usize,
    worker: Option<tokio::task::JoinHandle<BoundaryResult>>,
    stopping: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    report: Option<String>,
    reporting: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl TeammateState {
    fn started(&mut self, turn_id: &str) {
        if self.last_turn.as_deref() == Some(turn_id) {
            return;
        }
        self.cancel_check();
        if self.automatic_turn.as_deref() != Some(turn_id) {
            self.continuations = 0;
        }
        self.last_turn = Some(turn_id.to_owned());
    }

    fn submitted(&mut self, result: &Result<SubmissionDisposition, PhiTurnError>) {
        match result {
            Ok(SubmissionDisposition::Started { turn_id, .. }) => self.started(turn_id),
            Ok(SubmissionDisposition::Queued { .. }) => {
                self.cancel_check();
                self.continuations = 0;
            }
            _ => {}
        }
    }

    fn finished(&mut self, report: Option<String>) {
        self.pending = true;
        self.automatic_turn = None;
        self.report = report;
    }

    fn failed(&mut self, report: Option<String>) {
        self.suspend();
        self.report = report;
    }

    fn cancel_check(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
        self.pending = false;
        self.next_poll = None;
    }

    fn suspend(&mut self) {
        self.cancel_check();
        self.automatic_turn = None;
    }

    fn busy(&self) -> bool {
        self.pending
            || self.worker.is_some()
            || self.stopping.is_some()
            || self.report.is_some()
            || self.reporting.is_some()
    }
}

impl Drop for TeammateState {
    fn drop(&mut self) {
        self.cancel_check();
        // Accepted Manager closure owns its lifetime independently of this actor.
    }
}

struct QuiescenceWaiter {
    event_boundary: Option<String>,
    reply: oneshot::Sender<Result<AgentInstanceView, AgentInstanceError>>,
}

pub(crate) enum AgentInstanceCommand {
    Submit {
        prompt: TurnPrompt,
        reply: oneshot::Sender<Result<SubmissionDisposition, PhiTurnError>>,
    },
    SubmitDirected {
        prompt: TurnPrompt,
        constraint: super::execution::DirectedDelegation,
        reply: oneshot::Sender<Result<SubmissionDisposition, PhiTurnError>>,
    },
    Dispatch {
        prompt: TurnPrompt,
        dispatch: SubmissionDispatch,
        reply: oneshot::Sender<Result<SubmissionDisposition, PhiTurnError>>,
    },
    RespondInteraction {
        request_id: String,
        response: InteractionResponse,
        reply: oneshot::Sender<anyhow::Result<InteractionAnswer>>,
    },
    Abort {
        reply: oneshot::Sender<()>,
    },
    BackgroundBash {
        execution_id: String,
        reply: oneshot::Sender<Result<super::bash::BashExecution, AgentInstanceError>>,
    },
    StopBash {
        execution_id: String,
        reply: oneshot::Sender<Result<(), AgentInstanceError>>,
    },
    View {
        event_boundary: Option<String>,
        reply: oneshot::Sender<AgentInstanceView>,
    },
    WaitForQuiescence {
        event_boundary: Option<String>,
        reply: oneshot::Sender<Result<AgentInstanceView, AgentInstanceError>>,
    },
    ResetContext {
        handoff: Option<String>,
        reply: oneshot::Sender<Result<AgentInstanceView, AgentInstanceError>>,
    },
    EditContext {
        target_id: String,
        replacement: Option<crate::transcript::ContextReplacement>,
        reply: oneshot::Sender<Result<AgentInstanceView, AgentInstanceError>>,
    },
    ToolDisplay {
        reply: oneshot::Sender<ToolDisplayRegistry>,
    },
    SupportedAgents {
        reply: oneshot::Sender<crate::agent::custom::AgentCatalog>,
    },
    StartDelegatedAgent {
        name: String,
        prompt: String,
        parent_agent_id: String,
        parent_tool_call_id: Option<compact_str::CompactString>,
        completion: Option<crate::harness::instance::child::InitialCompletion>,
        mode: crate::agent_manager::DelegationMode,
        reply: oneshot::Sender<Result<SubmissionDisposition, PhiSubagentError>>,
    },
    AwaitDelegatedCompletion {
        cancel: bool,
        reply: oneshot::Sender<Result<crate::agents::SubagentCompletion, AgentInstanceError>>,
    },
    CaptureWorkflowScope {
        default_role: Option<String>,
        limits: crate::harness::workflow::WorkflowLimits,
        reply: oneshot::Sender<
            Result<super::execution::WorkflowScope, crate::harness::workflow::WorkflowError>,
        >,
    },
    DeliverWorkflowReport {
        report: crate::harness::workflow::WorkflowReport,
        reply: oneshot::Sender<Result<(), AgentInstanceError>>,
    },
    SpawnSubagent {
        request: AgentSpawnRequest,
        reply: oneshot::Sender<Result<AgentCreation, PhiSubagentError>>,
    },
    ObserveStartedChild {
        creation: crate::agents::AgentCreation,
        completion: crate::harness::instance::child::InitialCompletion,
        child: super::handle::AgentInstance,
        reply: oneshot::Sender<Result<(), PhiSubagentError>>,
    },
    SubagentSnapshots {
        reply: oneshot::Sender<Vec<SubagentSnapshot>>,
    },
    SubagentStatus {
        instance_id: AgentInstanceId,
        reply: oneshot::Sender<Option<SubagentStatus>>,
    },
    CancelSubagent {
        instance_id: AgentInstanceId,
        reply: oneshot::Sender<crate::harness::manager::control::ChildCancellation>,
    },
    ReplaceRuntime {
        candidate: Box<RuntimeCandidate>,
        reply: oneshot::Sender<Result<Transcript, AgentInstanceError>>,
    },
    Stop {
        reply: oneshot::Sender<Transcript>,
    },
}

enum InputPreparation {
    Waiting {
        prompt: TurnPrompt,
        intent: SubmissionIntent,
    },
    Preparing(SubmissionPreparation),
}

struct PendingInput {
    preparation: InputPreparation,
    reply: oneshot::Sender<Result<SubmissionDisposition, PhiTurnError>>,
}

pub(crate) struct AgentInstanceActor {
    state: AgentInstanceState,
    commands: mpsc::Receiver<AgentInstanceCommand>,
    pending_inputs: VecDeque<PendingInput>,
    snapshot_tx: watch::Sender<AgentInstanceSnapshot>,
    event_tx: broadcast::Sender<AgentInstanceEvent>,
    pub(crate) final_view: Arc<std::sync::OnceLock<AgentInstanceView>>,
    completion_waiters:
        Vec<oneshot::Sender<Result<crate::agents::SubagentCompletion, AgentInstanceError>>>,
    completion: crate::harness::instance::child::InitialCompletion,
    pub(crate) final_subagents: Arc<std::sync::OnceLock<Vec<SubagentSnapshot>>>,
    pending_interactions: BTreeMap<String, InteractionEnvelope>,
    active_turn_id: Option<String>,
    live_run: Option<LiveRunSnapshot>,
    progress: Option<RunProgress>,
    latest_error: Option<crate::PhiFailure>,
    completion_failure: Option<crate::PhiFailure>,
    quiescence_waiters: Vec<QuiescenceWaiter>,
    pub(crate) execution_pending: Arc<AtomicBool>,
    teammate: TeammateState,
    history_entry_count: usize,
    revision: u64,
}

impl AgentInstanceActor {
    pub(crate) fn new(
        state: AgentInstanceState,
        commands: mpsc::Receiver<AgentInstanceCommand>,
        snapshot_tx: watch::Sender<AgentInstanceSnapshot>,
        event_tx: broadcast::Sender<AgentInstanceEvent>,
    ) -> Self {
        let progress = state.active_turn_id().map(RunProgress::new);
        let history_entry_count = state.transcript().entries.len();
        Self {
            state,
            commands,
            pending_inputs: VecDeque::new(),
            snapshot_tx,
            event_tx,
            final_view: Arc::new(std::sync::OnceLock::new()),
            completion_waiters: Vec::new(),
            completion: Default::default(),
            final_subagents: Arc::new(std::sync::OnceLock::new()),
            pending_interactions: BTreeMap::new(),
            active_turn_id: None,
            live_run: None,
            progress,
            latest_error: None,
            completion_failure: None,
            quiescence_waiters: Vec::new(),
            execution_pending: Arc::new(AtomicBool::new(true)),
            teammate: Default::default(),
            history_entry_count,
            revision: 0,
        }
    }

    pub(crate) async fn run(mut self) {
        let mut pump = tokio::time::interval(PUMP_INTERVAL);
        pump.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        self.stop_without_reply().await;
                        break;
                    };
                    // Admission can precede a public snapshot change.
                    self.execution_pending.store(true, Ordering::Release);
                    let continue_running = self.handle_command(command).await;
                    self.publish_subagent_activity();
                    self.publish_delegated_completion();
                    self.complete_quiescence_waits();
                    if !continue_running {
                        break;
                    }
                }
                _ = pump.tick() => self.pump().await,
            }
        }
    }

    async fn handle_command(&mut self, command: AgentInstanceCommand) -> bool {
        self.observe_initial_cancellation();
        match command {
            AgentInstanceCommand::Submit { reply, .. }
            | AgentInstanceCommand::SubmitDirected { reply, .. }
            | AgentInstanceCommand::Dispatch { reply, .. }
                if self.completion_failure.is_some() && !self.completion.is_complete() =>
            {
                let _ = reply.send(Err(PhiTurnError::Dispatch {
                    source: anyhow::anyhow!("initial Agent completion is pending"),
                }));
            }
            AgentInstanceCommand::SpawnSubagent { reply, .. }
                if self.completion_failure.is_some() && !self.completion.is_complete() =>
            {
                let _ = reply.send(Err(PhiSubagentError::ParentTurnRunning));
            }
            AgentInstanceCommand::Submit { prompt, reply } => {
                self.enqueue_input(prompt, SubmissionIntent::Immediate, reply);
            }
            AgentInstanceCommand::SubmitDirected {
                prompt,
                constraint,
                reply,
            } => {
                self.enqueue_input(prompt, SubmissionIntent::Directed(constraint), reply);
            }
            AgentInstanceCommand::Dispatch {
                prompt,
                dispatch,
                reply,
            } => self.enqueue_input(prompt, SubmissionIntent::Dispatch(dispatch), reply),
            AgentInstanceCommand::RespondInteraction {
                request_id,
                response,
                reply,
            } => self.respond_interaction(request_id, response, reply).await,
            AgentInstanceCommand::Abort { reply } => self.abort(reply),
            AgentInstanceCommand::BackgroundBash {
                execution_id,
                reply,
            } => {
                let result = self.state.background_bash(&execution_id);
                let _ = reply.send(result);
                self.publish_snapshot(None);
            }
            AgentInstanceCommand::StopBash {
                execution_id,
                reply,
            } => {
                let result = self.state.stop_bash(&execution_id);
                self.publish_snapshot(None);
                let _ = reply.send(result);
            }
            AgentInstanceCommand::View {
                event_boundary,
                reply,
            } => {
                self.publish_snapshot(None);
                let view = self.view(self.snapshot_tx.borrow().clone());
                if let Some(id) = event_boundary {
                    let _ = self.event_tx.send(AgentInstanceEvent::ViewBoundary { id });
                }
                let _ = reply.send(view);
            }
            AgentInstanceCommand::WaitForQuiescence {
                event_boundary,
                reply,
            } => {
                self.quiescence_waiters
                    .retain(|waiter| !waiter.reply.is_closed());
                if self.quiescence_waiters.len() == QUIESCENCE_WAIT_LIMIT {
                    let _ = reply.send(Err(AgentInstanceError::Busy));
                } else {
                    self.quiescence_waiters.push(QuiescenceWaiter {
                        event_boundary,
                        reply,
                    });
                }
            }
            AgentInstanceCommand::ResetContext { handoff, reply } => {
                let result = self.reset_context(handoff);
                let _ = reply.send(result);
            }
            AgentInstanceCommand::EditContext {
                target_id,
                replacement,
                reply,
            } => {
                let result = self.edit_context(&target_id, replacement);
                let _ = reply.send(result);
            }
            AgentInstanceCommand::ToolDisplay { reply } => {
                let _ = reply.send(self.state.tool_display().clone());
            }
            AgentInstanceCommand::SupportedAgents { reply } => {
                let _ = reply.send(self.state.supported_agents().clone());
            }
            AgentInstanceCommand::StartDelegatedAgent {
                name,
                prompt,
                parent_agent_id,
                parent_tool_call_id,
                completion,
                mode,
                reply,
            } => {
                let completion = completion.unwrap_or_else(|| self.completion.clone());
                let result = if completion.is_cancelled() {
                    Err(PhiSubagentError::PromptCancelled)
                } else {
                    self.state
                        .start_delegated_agent(
                            &name,
                            &prompt,
                            &parent_agent_id,
                            parent_tool_call_id,
                            &completion,
                        )
                        .await
                };
                if let Ok(start) = &result {
                    self.state.set_bash_owner_mode(mode);
                    if let SubmissionDisposition::Started { turn_id, .. } = start {
                        completion.bind_turn(turn_id.clone());
                    }
                    self.completion = completion;
                    self.observe_turn_start(&Ok(start.clone()));
                }
                let _ = reply.send(result);
            }
            AgentInstanceCommand::AwaitDelegatedCompletion { cancel, reply } => {
                if let Some(completion) = self.completion.get() {
                    let _ = reply.send(Ok(completion));
                } else if self.state.is_delegated_agent() {
                    if cancel {
                        self.fail_initial_turn(crate::PhiFailure::cancelled(
                            self.completion.cancellation_reason(),
                            CHILD_CANCELLATION_MESSAGE,
                        ));
                    }
                    self.completion_waiters.push(reply);
                } else {
                    let _ = reply.send(Err(AgentInstanceError::Operation {
                        source: anyhow::anyhow!("no delegated Agent has started"),
                    }));
                }
            }
            AgentInstanceCommand::CaptureWorkflowScope {
                default_role,
                limits,
                reply,
            } => {
                if !reply.is_closed() {
                    let _ = reply.send(self.state.capture_workflow_scope(default_role, limits));
                }
            }
            AgentInstanceCommand::DeliverWorkflowReport { report, reply } => {
                let _ = reply.send(self.state.admit_workflow_report(report));
            }
            AgentInstanceCommand::SpawnSubagent { request, reply } => {
                self.state.spawn_subagent(request, reply);
            }
            AgentInstanceCommand::ObserveStartedChild {
                creation,
                completion,
                child,
                reply,
            } => {
                let _ = reply.send(
                    self.state
                        .observe_started_child(creation, completion, child),
                );
            }
            AgentInstanceCommand::SubagentSnapshots { reply } => {
                let _ = reply.send(self.state.subagent_snapshots());
            }
            AgentInstanceCommand::SubagentStatus { instance_id, reply } => {
                let _ = reply.send(self.state.subagent_status(&instance_id));
            }
            AgentInstanceCommand::CancelSubagent { instance_id, reply } => {
                let _ = reply.send(
                    self.state
                        .cancel_subagent(&instance_id, crate::CancellationReason::UserRequested),
                );
            }
            AgentInstanceCommand::ReplaceRuntime { candidate, reply } => {
                let _ = reply.send(self.replace_runtime(*candidate));
            }
            AgentInstanceCommand::Stop { reply } => return self.stop(reply).await,
        }
        self.publish_snapshot(None);
        true
    }

    fn enqueue_input(
        &mut self,
        prompt: TurnPrompt,
        intent: SubmissionIntent,
        reply: oneshot::Sender<Result<SubmissionDisposition, PhiTurnError>>,
    ) {
        if self.pending_inputs.len() >= MAX_PENDING_INPUTS {
            let _ = reply.send(Err(PhiTurnError::QueueFull {
                limit: MAX_PENDING_INPUTS,
            }));
            return;
        }
        self.pending_inputs.push_back(PendingInput {
            preparation: InputPreparation::Waiting { prompt, intent },
            reply,
        });
        self.progress_submission();
    }

    fn progress_submission(&mut self) {
        while let Some(input) = self.pending_inputs.pop_front() {
            let mut preparation = match input.preparation {
                InputPreparation::Waiting { prompt, intent } => {
                    match self.state.prepare_input(prompt, intent) {
                        Ok(preparation) => preparation,
                        Err(error) => {
                            let _ = input.reply.send(Err(error));
                            continue;
                        }
                    }
                }
                InputPreparation::Preparing(preparation) => preparation,
            };
            let Some(result) = preparation.ready().now_or_never() else {
                self.pending_inputs.push_front(PendingInput {
                    preparation: InputPreparation::Preparing(preparation),
                    reply: input.reply,
                });
                return;
            };
            let result = self.state.finish_submission(preparation, result);
            self.observe_turn_start(&result);
            let _ = input.reply.send(result);
        }
    }

    fn cancel_preparations(&mut self) {
        self.state.preparing_submission = false;
        for input in self.pending_inputs.drain(..) {
            let _ = input.reply.send(Err(PhiTurnError::Cancelled));
        }
    }

    fn observe_turn_start(&mut self, result: &Result<SubmissionDisposition, PhiTurnError>) {
        self.teammate.submitted(result);
        if let Ok(SubmissionDisposition::Started { turn_id, .. }) = result {
            self.active_turn_id = Some(turn_id.clone());
            self.progress = Some(RunProgress::new(turn_id.clone()));
            self.live_run = Some(LiveRunSnapshot::default());
            self.latest_error = None;
        }
    }

    async fn respond_interaction(
        &mut self,
        request_id: String,
        response: InteractionResponse,
        reply: oneshot::Sender<anyhow::Result<InteractionAnswer>>,
    ) {
        let result = self.state.respond_interaction(&request_id, response).await;
        if result.is_ok() {
            self.close_interaction(&request_id);
        }
        let _ = reply.send(result);
    }

    fn abort(&mut self, reply: oneshot::Sender<()>) {
        self.cancel_preparations();
        let turn_id = self.state.active_turn_id();
        self.observe_initial_cancellation();
        let events = self.state.abort();
        self.publish_phi_events(events);
        if self.state.is_delegated_agent() && !self.completion.is_complete() {
            self.completion_failure.get_or_insert_with(|| {
                crate::PhiFailure::cancelled(
                    crate::CancellationReason::Interrupted,
                    "subagent execution interrupted",
                )
            });
        }
        self.teammate.suspend();
        self.prune_interactions();
        self.finish_progress(
            ExecutionStatus::Cancelled,
            Some(crate::CancellationReason::Interrupted),
        );
        self.clear_active_turn();
        self.latest_error = None;
        let _ = self.event_tx.send(AgentInstanceEvent::RunCancelled {
            turn_id,
            reason: crate::CancellationReason::Interrupted,
        });
        let _ = reply.send(());
    }

    fn reset_context(
        &mut self,
        handoff: Option<String>,
    ) -> Result<AgentInstanceView, AgentInstanceError> {
        self.require_context_idle()?;
        let id = self
            .state
            .reset_context(handoff)
            .map_err(|source| AgentInstanceError::Operation { source })?;
        self.revision = self.revision.saturating_add(1);
        let snapshot = self.capture_snapshot(self.status());
        self.snapshot_tx.send_replace(snapshot.clone());
        let _ = self.event_tx.send(AgentInstanceEvent::ContextReset {
            entry_id: id.to_string(),
        });
        Ok(self.view(snapshot))
    }

    fn require_context_idle(&self) -> Result<(), AgentInstanceError> {
        if !self.pending_inputs.is_empty()
            || self.teammate.busy()
            || self.state.has_workflows()
            || self.state.needs_progress()
            || !self.state.pending_submission_ids().is_empty()
            || self.state.has_pending_interactions()
            || !self.pending_interactions.is_empty()
            || (self.state.is_delegated_agent() && !self.completion.is_complete())
        {
            return Err(AgentInstanceError::Busy);
        }
        Ok(())
    }

    fn edit_context(
        &mut self,
        target_id: &str,
        replacement: Option<crate::transcript::ContextReplacement>,
    ) -> Result<AgentInstanceView, AgentInstanceError> {
        self.require_context_idle()?;
        self.state
            .edit_context(target_id, replacement)
            .map_err(|source| AgentInstanceError::Operation { source })?;
        self.revision = self.revision.saturating_add(1);
        let snapshot = self.capture_snapshot(self.status());
        self.snapshot_tx.send_replace(snapshot.clone());
        self.publish_snapshot(None);
        Ok(self.view(snapshot))
    }

    fn replace_runtime(
        &mut self,
        candidate: RuntimeCandidate,
    ) -> Result<Transcript, AgentInstanceError> {
        let actual = AgentInstanceId::new(&candidate.instance_id);
        let expected = self.snapshot_tx.borrow().id.clone();
        if actual != expected {
            return Err(AgentInstanceError::ReplacementIdentity {
                expected: expected.to_string(),
                actual: actual.to_string(),
            });
        }
        if candidate.delegated_child != self.state.delegated_child {
            return Err(AgentInstanceError::Operation {
                source: anyhow::anyhow!("replacement cannot change the instance delegation origin"),
            });
        }
        if candidate.task_list != self.state.task_list {
            return Err(AgentInstanceError::ReplacementTaskList);
        }
        if !self.pending_inputs.is_empty()
            || self.state.has_workflows()
            || self.state.needs_progress()
            || !self.state.pending_submission_ids().is_empty()
            || !self.pending_interactions.is_empty()
            || (self.state.is_delegated_agent() && !self.completion.is_complete())
        {
            return Err(AgentInstanceError::Busy);
        }
        let transcript = if let Some(branch) = candidate.branch {
            let mut transcript = self.state.transcript().clone();
            transcript
                .select_branch(branch.leaf_id.as_deref())
                .map_err(|source| AgentInstanceError::Operation { source })?;
            Some(transcript)
        } else {
            None
        };
        self.state.replace_runtime(candidate.generation)?;
        self.teammate.suspend();
        if let Some(transcript) = transcript {
            self.state.transcript = transcript;
        }
        for waiter in self.quiescence_waiters.drain(..) {
            let _ = waiter.reply.send(Err(AgentInstanceError::RuntimeReplaced));
        }
        self.clear_active_turn();
        self.latest_error = None;
        self.publish_snapshot(None);
        let _ = self.event_tx.send(AgentInstanceEvent::RuntimeReplaced);
        Ok(self.state.transcript().clone())
    }

    async fn stop(&mut self, reply: oneshot::Sender<Transcript>) -> bool {
        self.publish_snapshot(Some(AgentInstanceStatus::Stopping));
        let _ = reply.send(self.shutdown().await);
        false
    }

    async fn stop_without_reply(&mut self) {
        self.shutdown().await;
    }

    async fn shutdown(&mut self) -> Transcript {
        self.cancel_preparations();
        self.observe_initial_cancellation();
        self.teammate.suspend();
        for waiter in self.quiescence_waiters.drain(..) {
            let _ = waiter.reply.send(Err(AgentInstanceError::Stopped));
        }
        self.publish_snapshot(Some(AgentInstanceStatus::Stopping));
        let events = self.state.shutdown();
        self.state.shutdown_bash().await;
        self.publish_phi_events(events);
        self.finish_progress(
            ExecutionStatus::Cancelled,
            Some(crate::CancellationReason::AgentStopped),
        );
        self.state.deny_pending_interactions();
        self.close_all_interactions();
        while self.state.has_child_workers() {
            self.state.progress_child_workers();
            tokio::time::sleep(PUMP_INTERVAL).await;
        }
        self.state.discard_subagent_outcomes();
        self.publish_subagent_activity();
        self.clear_active_turn();
        if !self.completion.is_complete() && self.state.is_delegated_agent() {
            self.completion_failure.get_or_insert_with(|| {
                crate::PhiFailure::cancelled(
                    crate::CancellationReason::AgentStopped,
                    "delegated Agent stopped",
                )
            });
            self.publish_delegated_completion();
        }
        let transcript = self.state.transcript().clone();
        let _ = self.final_subagents.set(self.state.subagent_snapshots());
        self.revision = self.revision.saturating_add(1);
        let snapshot = self.capture_snapshot(AgentInstanceStatus::Stopped);
        let _ = self.final_view.set(self.view(snapshot.clone()));
        self.snapshot_tx.send_replace(snapshot);
        let _ = self.event_tx.send(AgentInstanceEvent::Stopped);
        transcript
    }

    async fn pump(&mut self) {
        self.execution_pending.store(true, Ordering::Release);
        self.observe_initial_cancellation();
        self.progress_submission();
        self.state.poll_bash();
        if self.completion_failure.is_some() && !self.completion.is_complete() {
            self.state.progress_child_workers();
            self.state.discard_subagent_outcomes();
        } else {
            if let Err(error) = self.state.admit_manager_messages() {
                self.record_pump_failure(error);
            }
            if self.state.needs_progress() {
                match self.state.drain_phi_events().await {
                    Ok((events, failure)) => {
                        self.publish_phi_events(events);
                        if let Some(error) = failure {
                            self.record_pump_failure(error);
                        }
                    }
                    Err(error) => self.record_pump_failure(error),
                }
            }
        }
        self.publish_subagent_activity();
        self.publish_delegated_completion();
        self.progress_teammate().await;
        self.collect_interactions().await;
        self.prune_interactions();
        self.publish_snapshot(None);
        self.complete_quiescence_waits();
    }

    fn capture_delegated_completion(&mut self) {
        self.observe_initial_cancellation();
        if self.state.bash_cleanup_pending() {
            return;
        }
        if self.completion.is_complete() {
            return;
        }
        let Some(outcome) = self
            .completion_failure
            .clone()
            .map(Err)
            .or_else(|| self.state.delegated_completion())
        else {
            return;
        };
        if self.snapshot_tx.borrow().status != AgentInstanceStatus::Stopping {
            self.publish_snapshot(None);
        }
        if self.completion.publish(outcome).is_err() {
            self.observe_initial_cancellation();
        }
    }

    fn publish_delegated_completion(&mut self) {
        self.completion_waiters.retain(|reply| !reply.is_closed());
        self.capture_delegated_completion();
        if !self.completion_waiters.is_empty()
            && let Some(completion) = self.completion.get()
        {
            for reply in self.completion_waiters.drain(..) {
                let _ = reply.send(Ok(completion.clone()));
            }
        }
    }

    fn observe_initial_cancellation(&mut self) {
        if self.completion.is_cancelled()
            && !self.completion.is_complete()
            && self.completion_failure.is_none()
        {
            self.fail_initial_turn(crate::PhiFailure::cancelled(
                self.completion.cancellation_reason(),
                CHILD_CANCELLATION_MESSAGE,
            ));
        }
    }

    fn fail_initial_turn(&mut self, failure: crate::PhiFailure) {
        self.cancel_preparations();
        if failure.is_user_cancelled() {
            self.state.suspend_automatic_turns();
        }
        self.teammate.suspend();
        let status = ExecutionStatus::from_failure(&failure);
        let reason = failure.cancellation_reason;
        self.completion_failure.get_or_insert(failure);
        let events = self.state.abort();
        self.publish_phi_events(events);
        self.state.deny_pending_interactions();
        self.close_all_interactions();
        self.finish_progress(status, reason);
        self.clear_active_turn();
    }

    fn observe_phi_event(&mut self, envelope: &PhiEventEnvelope) {
        LiveRunSnapshot::observe(&mut self.live_run, &envelope.event);
        match &envelope.event {
            PhiEvent::TurnStarted { .. } => {
                if let Some(turn_id) = &envelope.turn_id {
                    self.teammate.started(turn_id);
                }
                self.active_turn_id = envelope.turn_id.clone();
                self.live_run = Some(LiveRunSnapshot::default());
                self.latest_error = None;
            }
            PhiEvent::TurnFinished { response, .. } => {
                let report = self.completion.is_complete().then(|| response.clone());
                self.clear_active_turn();
                self.teammate.finished(report);
            }
            PhiEvent::TurnFailed { failure } => {
                let report = self
                    .completion
                    .is_complete()
                    .then(|| format!("Agent turn failed: {}", failure.message));
                self.teammate.failed(report);
                self.clear_active_turn();
                self.latest_error = Some(failure.clone());
                if self.state.is_delegated_agent() && !self.completion.is_complete() {
                    self.fail_initial_turn(failure.clone());
                }
            }
            _ => {}
        }
    }

    fn clear_active_turn(&mut self) {
        self.active_turn_id = None;
        self.live_run = None;
    }

    fn record_pump_failure(&mut self, error: PhiStreamError) {
        self.teammate.suspend();
        let message = error.to_string();
        let failure =
            crate::PhiFailure::new(crate::PhiFailureKind::Internal, message.clone(), None);
        if self.latest_error.as_ref() == Some(&failure) {
            return;
        }
        self.state
            .record_failure(self.active_turn_id.as_deref(), &failure);
        self.finish_progress(ExecutionStatus::Failed, None);
        if self.state.is_delegated_agent() && !self.completion.is_complete() {
            self.fail_initial_turn(failure.clone());
        }
        self.latest_error = Some(failure);
        let _ = self.event_tx.send(AgentInstanceEvent::HistoryChanged);
        let _ = self
            .event_tx
            .send(AgentInstanceEvent::PumpFailed { message });
    }
}

impl AgentInstanceActor {
    fn complete_quiescence_waits(&mut self) {
        for waiter in std::mem::take(&mut self.quiescence_waiters) {
            if waiter.reply.is_closed() {
                continue;
            }
            match self.quiescence_view() {
                Ok(Some(view)) => {
                    if let Some(id) = waiter.event_boundary {
                        let _ = self.event_tx.send(AgentInstanceEvent::ViewBoundary { id });
                    }
                    let _ = waiter.reply.send(Ok(view));
                }
                Ok(None) => self.quiescence_waiters.push(waiter),
                Err(error) => {
                    let _ = waiter.reply.send(Err(error));
                }
            }
        }
    }

    fn local_execution_pending(&self) -> bool {
        !self.pending_inputs.is_empty()
            || self.teammate.busy()
            || self.state.needs_progress()
            || !self.state.pending_submission_ids().is_empty()
            || self.state.has_pending_interactions()
            || !self.pending_interactions.is_empty()
            || !self.commands.is_empty()
            || (self.state.is_delegated_agent() && !self.completion.is_complete())
    }

    fn quiescence_view(&mut self) -> Result<Option<AgentInstanceView>, AgentInstanceError> {
        if self.local_execution_pending() {
            return Ok(None);
        }
        if self
            .state
            .has_pending_execution()
            .map_err(|source| AgentInstanceError::Operation {
                source: source.into(),
            })?
        {
            return Ok(None);
        }
        self.publish_snapshot(None);
        Ok(Some(self.view(self.snapshot_tx.borrow().clone())))
    }

    fn publish_subagent_activity(&mut self) {
        for activity in self.state.drain_subagent_activity() {
            let _ = self.event_tx.send(AgentInstanceEvent::Subagent(activity));
        }
    }

    fn publish_phi_events(&mut self, events: Vec<PhiEventEnvelope>) {
        for envelope in events {
            let progress = observe_progress(&mut self.progress, &envelope);
            self.observe_phi_event(&envelope);
            let _ = self.event_tx.send(AgentInstanceEvent::Phi(envelope));
            if let Some(progress) = progress {
                let _ = self.event_tx.send(AgentInstanceEvent::Progress(progress));
            }
        }
    }

    fn finish_progress(
        &mut self,
        status: ExecutionStatus,
        reason: Option<crate::CancellationReason>,
    ) {
        if let Some(run) = &mut self.progress
            && run.finish(status)
        {
            run.observed.cancellation_reason = reason;
            let _ = self
                .event_tx
                .send(AgentInstanceEvent::Progress(run.observed.clone()));
        }
    }

    async fn collect_interactions(&mut self) {
        for interaction in self.state.drain_interactions().await {
            self.pending_interactions
                .insert(interaction.request_id.clone(), interaction.clone());
            let _ = self
                .event_tx
                .send(AgentInstanceEvent::InteractionRequested(interaction));
        }
    }

    fn prune_interactions(&mut self) {
        let request_ids = self
            .pending_interactions
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for request_id in request_ids {
            if !self.state.is_interaction_pending(&request_id) {
                self.close_interaction(&request_id);
            }
        }
    }

    fn close_interaction(&mut self, request_id: &str) {
        if self.pending_interactions.remove(request_id).is_some() {
            let _ = self.event_tx.send(AgentInstanceEvent::InteractionClosed {
                request_id: request_id.to_string(),
            });
        }
    }

    fn close_all_interactions(&mut self) {
        let request_ids = self
            .pending_interactions
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for request_id in request_ids {
            self.close_interaction(&request_id);
        }
    }

    fn publish_snapshot(&mut self, status: Option<AgentInstanceStatus>) {
        if self.history_entry_count != self.state.transcript().entries.len() {
            self.history_entry_count = self.state.transcript().entries.len();
            let _ = self.event_tx.send(AgentInstanceEvent::HistoryChanged);
        }
        let status = status.unwrap_or_else(|| self.status());
        self.execution_pending.store(
            match status {
                AgentInstanceStatus::Stopped => false,
                AgentInstanceStatus::Stopping => true,
                _ => self.local_execution_pending(),
            },
            Ordering::Release,
        );
        let pending_interactions = self
            .pending_interactions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let current = self.snapshot_tx.borrow();
        if current.definition.as_deref() == self.state.definition_name()
            && current.working_dir == self.state.working_dir()
            && current.bash_executions == self.state.bash_executions()
            && snapshot_is_current(
                &current,
                status,
                self.active_turn_id.as_deref(),
                self.live_run.as_ref(),
                self.progress.as_ref().map(|run| &run.observed),
                &pending_interactions,
                self.latest_error.as_ref(),
            )
        {
            return;
        }
        drop(current);
        self.revision = self.revision.saturating_add(1);
        self.snapshot_tx.send_replace(self.capture_snapshot(status));
    }

    fn capture_snapshot(&self, status: AgentInstanceStatus) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: self.snapshot_tx.borrow().id.clone(),
            definition: self.state.definition_name().map(str::to_owned),
            working_dir: self.state.working_dir().to_path_buf(),
            status,
            active_turn_id: self.active_turn_id.clone(),
            live_run: self.live_run.clone(),
            progress: self.progress.as_ref().map(|run| run.observed.clone()),
            bash_executions: self.state.bash_executions(),
            pending_interactions: self.pending_interactions.values().cloned().collect(),
            latest_error: self.latest_error.clone(),
            revision: self.revision,
        }
    }

    fn view(&self, mut snapshot: AgentInstanceSnapshot) -> AgentInstanceView {
        snapshot.progress = self.progress.as_ref().map(RunProgress::snapshot);
        AgentInstanceView {
            pending_submission_ids: self.state.pending_submission_ids(),
            transcript: self.state.transcript().clone(),
            snapshot,
        }
    }

    fn status(&self) -> AgentInstanceStatus {
        if !self.pending_interactions.is_empty() {
            AgentInstanceStatus::RequiresAction
        } else if !self.pending_inputs.is_empty()
            || self.state.needs_progress()
            || self.teammate.busy()
        {
            AgentInstanceStatus::Running
        } else if self.latest_error.is_some() {
            AgentInstanceStatus::Failed
        } else {
            AgentInstanceStatus::Idle
        }
    }
}

impl AgentInstanceActor {
    async fn progress_teammate(&mut self) {
        if self.state.take_teammate_stop() {
            self.teammate.suspend();
            match self.state.stop_teammate() {
                Ok(worker) => self.teammate.stopping = Some(worker),
                Err(error) => self.teammate_failure(error),
            }
        }
        if self
            .teammate
            .reporting
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            let result = self
                .teammate
                .reporting
                .take()
                .expect("checked report worker")
                .await;
            if let Err(error) = result
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            {
                self.teammate_failure(error);
            }
        }
        if let Some(stopping) = &self.teammate.stopping {
            if stopping.is_finished() {
                let result = self
                    .teammate
                    .stopping
                    .take()
                    .expect("checked stop worker")
                    .await;
                if let Err(error) = result
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                {
                    self.teammate_failure(error);
                }
            }
            return;
        }
        if !self.pending_inputs.is_empty()
            || self.state.needs_progress()
            || !self.state.pending_submission_ids().is_empty()
            || self.state.has_pending_interactions()
            || !self.pending_interactions.is_empty()
            || (self.state.is_delegated_agent() && !self.completion.is_complete())
        {
            return;
        }
        match self.state.has_pending_execution() {
            Ok(false) => {}
            Ok(true) => {
                if let Some(worker) = self.teammate.worker.take() {
                    worker.abort();
                    self.teammate.pending = true;
                }
                return;
            }
            Err(_) => {
                self.teammate.suspend();
                return;
            }
        }
        if !self.teammate.pending && self.teammate.worker.is_none() {
            self.report_idle();
        }
        if let Some(worker) = &self.teammate.worker {
            if !worker.is_finished() {
                return;
            }
            let result = self
                .teammate
                .worker
                .take()
                .expect("checked boundary worker")
                .await;
            match result
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            {
                Ok((tasks, decision)) => {
                    if tasks.validate().is_err() {
                        return;
                    }
                    match decision {
                        decision @ (TeammateDecision::Idle | TeammateDecision::Waiting) => {
                            self.report_idle();
                            if matches!(decision, TeammateDecision::Waiting) {
                                self.teammate.next_poll =
                                    Some(tokio::time::Instant::now() + IDLE_WORK_POLL_INTERVAL);
                            }
                        }
                        TeammateDecision::Continue(prompt) => {
                            if self.teammate.continuations >= MAX_TEAMMATE_CONTINUATIONS {
                                self.teammate_failure(anyhow::anyhow!(
                                    "teammate automatic continuation limit reached"
                                ));
                                return;
                            }
                            match self.state.start_teammate_continuation(prompt) {
                                Ok(start) => {
                                    self.teammate.continuations += 1;
                                    if let SubmissionDisposition::Started { turn_id, .. } = &start {
                                        self.teammate.automatic_turn = Some(turn_id.clone());
                                    }
                                    self.observe_turn_start(&Ok(start));
                                }
                                Err(error) => self.teammate_failure(error.into()),
                            }
                        }
                    }
                }
                Err(error) => self.teammate_failure(error),
            }
        } else if self.teammate.pending
            || self
                .teammate
                .next_poll
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            match self.state.teammate_check(
                MAX_TEAMMATE_CONTINUATIONS.saturating_sub(self.teammate.continuations),
            ) {
                Ok(Some(check)) => {
                    self.teammate.pending = false;
                    self.teammate.next_poll = None;
                    self.teammate.worker = Some(tokio::spawn(check.run()));
                }
                Ok(None) => {
                    self.teammate.cancel_check();
                    self.report_idle();
                }
                // Initial execution can end before Manager publishes Ready membership.
                Err(error)
                    if matches!(
                        error.downcast_ref::<crate::harness::task::TaskAccessError>(),
                        Some(crate::harness::task::TaskAccessError::InactiveCaller)
                    ) => {}
                Err(error) => self.teammate_failure(error),
            }
        }
    }

    fn report_idle(&mut self) {
        if self.teammate.reporting.is_some() {
            return;
        }
        if let Some(report) = self.teammate.report.take() {
            match self.state.publish_idle_report(report) {
                Ok(worker) => self.teammate.reporting = Some(worker),
                Err(error) => self.teammate_failure(error),
            }
        }
    }

    fn teammate_failure(&mut self, error: anyhow::Error) {
        self.record_pump_failure(PhiStreamError::Operation { source: error });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::event::AgentEvent;
    use crate::factory::{Phi, PhiRuntimeServices};
    use crate::harness::instance::tests::{test_catalog, test_client};

    #[tokio::test]
    async fn delegated_termination_retains_source() {
        #[derive(Clone, Copy)]
        enum Termination {
            PumpFailure,
            Interrupt,
            UserCancel,
            Stop,
            UserCancelThenStop,
            UserCancelThenInterrupt,
        }
        for termination in [
            Termination::PumpFailure,
            Termination::Interrupt,
            Termination::UserCancel,
            Termination::Stop,
            Termination::UserCancelThenStop,
            Termination::UserCancelThenInterrupt,
        ] {
            let phi = Phi::builder()
                .provider("openai-codex")
                .model("gpt-5.6-sol")
                .no_tools(true)
                .agent(crate::agents::AgentDefinition::new(
                    "probe", "inspect", "Inspect.",
                ))
                .build()
                .unwrap()
                .with_runtime_services(PhiRuntimeServices::new().model_catalog(test_catalog()));
            let candidate = phi
                .agent_instance_builder()
                .delegated_child()
                .build()
                .await
                .unwrap();
            let id = AgentInstanceId::new(candidate.id());
            let replacement = phi
                .agent_instance_builder()
                .transcript(candidate.transcript().clone())
                .delegated_child()
                .build()
                .await
                .unwrap()
                .into_runtime_candidate();
            let state = candidate.into_state();
            let snapshot = AgentInstanceSnapshot {
                id,
                definition: None,
                working_dir: state.working_dir().to_path_buf(),
                status: AgentInstanceStatus::Idle,
                active_turn_id: None,
                live_run: None,
                progress: None,
                bash_executions: Vec::new(),
                pending_interactions: Vec::new(),
                latest_error: None,
                revision: 0,
            };
            let (_commands, command_rx) = mpsc::channel(1);
            let (snapshots, _) = watch::channel(snapshot);
            let (events, _) = broadcast::channel(1);
            let mut actor = AgentInstanceActor::new(state, command_rx, snapshots, events);
            let started = actor
                .state
                .start_delegated_agent("probe", "task", "parent", None, &Default::default())
                .await
                .unwrap();
            actor.observe_turn_start(&Ok(started));
            let (reply, response) = oneshot::channel();
            actor.completion_waiters.push(reply);
            let expected_reason = match termination {
                Termination::PumpFailure => {
                    actor.record_pump_failure(PhiStreamError::Operation {
                        source: anyhow::anyhow!("pump failed"),
                    });
                    let failure = actor.latest_error.as_ref().unwrap();
                    assert_eq!(failure.kind, crate::PhiFailureKind::Internal);
                    assert!(failure.message.contains("pump failed"));
                    assert!(actor.state.transcript().entries.iter().any(|entry| matches!(
                        &entry.kind,
                        crate::transcript::TranscriptEntryKind::Failure { failure: stored, .. }
                            if stored == failure
                    )));
                    None
                }
                Termination::Interrupt => {
                    let (reply, _) = oneshot::channel();
                    actor.abort(reply);
                    Some(crate::CancellationReason::Interrupted)
                }
                Termination::UserCancel
                | Termination::UserCancelThenStop
                | Termination::UserCancelThenInterrupt => {
                    actor
                        .completion
                        .cancel_with_reason(crate::CancellationReason::UserRequested);
                    if matches!(termination, Termination::UserCancelThenStop) {
                        actor.shutdown().await;
                    } else if matches!(termination, Termination::UserCancelThenInterrupt) {
                        let (reply, _) = oneshot::channel();
                        actor.abort(reply);
                    } else {
                        actor.observe_initial_cancellation();
                    }
                    Some(crate::CancellationReason::UserRequested)
                }
                Termination::Stop => {
                    actor.shutdown().await;
                    Some(crate::CancellationReason::AgentStopped)
                }
            };
            if !matches!(
                termination,
                Termination::Stop | Termination::UserCancelThenStop
            ) {
                assert!(matches!(
                    actor.replace_runtime(replacement),
                    Err(AgentInstanceError::Busy)
                ));
                assert!(matches!(
                    actor.reset_context(None),
                    Err(AgentInstanceError::Busy)
                ));
            }
            actor.publish_snapshot(None);
            assert_eq!(
                actor.snapshot_tx.borrow().definition.as_deref(),
                Some("probe")
            );
            actor.publish_delegated_completion();
            let failure = response.await.unwrap().unwrap().outcome.unwrap_err();
            assert_eq!(failure.cancellation_reason, expected_reason);
            assert_eq!(
                failure.is_user_cancelled(),
                matches!(
                    termination,
                    Termination::UserCancel
                        | Termination::UserCancelThenStop
                        | Termination::UserCancelThenInterrupt
                )
            );
            if expected_reason.is_none() {
                assert_eq!(failure.kind, crate::PhiFailureKind::Internal);
                assert!(failure.message.contains("pump failed"));
            } else {
                assert_eq!(failure.kind, crate::PhiFailureKind::Cancelled);
            }
            assert_eq!(
                actor
                    .progress
                    .as_ref()
                    .unwrap()
                    .observed
                    .cancellation_reason,
                expected_reason
            );
            actor.shutdown().await;
        }
    }

    #[tokio::test]
    async fn abort_retires_lifecycle_work_from_buffered_terminal_events() {
        let mut candidate = test_client().await;
        let events = candidate.install_test_main_run("finished-before-abort");
        let id = AgentInstanceId::new(candidate.id());
        let state = candidate.into_state();
        let snapshot = AgentInstanceSnapshot {
            id,
            definition: None,
            working_dir: state.working_dir().to_path_buf(),
            status: AgentInstanceStatus::Running,
            active_turn_id: None,
            live_run: None,
            progress: None,
            bash_executions: Vec::new(),
            pending_interactions: Vec::new(),
            latest_error: None,
            revision: 0,
        };
        let (_commands, command_rx) = mpsc::channel(1);
        let (snapshots, _) = watch::channel(snapshot);
        let (published, _) = broadcast::channel(16);
        let mut actor = AgentInstanceActor::new(state, command_rx, snapshots, published);
        events
            .send(AgentEvent::RunFinished {
                response: "finished".into(),
                usage: Default::default(),
            })
            .await
            .unwrap();
        let (reply, response) = oneshot::channel();
        actor.abort(reply);
        response.await.unwrap();
        assert!(actor.reset_context(None).is_ok());
        assert_eq!(actor.status(), AgentInstanceStatus::Idle);
    }
}
