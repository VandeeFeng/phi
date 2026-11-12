use super::builder::{AgentInstanceCandidate, AgentInstanceOptions, RuntimeCandidate};
use crate::core::event::{PhiEvent, PhiEventEnvelope};
use crate::core::interaction::{InteractionAnswer, InteractionEnvelope, InteractionResponse};
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::core::turn::TurnPrompt;
use crate::factory::{Phi, PhiQueryError};
use crate::harness::instance::actor::{AgentInstanceActor, AgentInstanceCommand};
use crate::harness::instance::child::{SubagentActivity, SubagentSnapshot, SubagentStatus};
use crate::harness::instance::execution;
use crate::harness::instance::execution::{PhiSubagentError, PhiTurnError};
use crate::harness::manager::AgentManager;
use crate::harness::manager::preparation::{AgentCreation, AgentSpawnRequest};
use crate::tools::ToolDisplayRegistry;
use crate::transcript::Transcript;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, oneshot, watch};

// Scoped instance operations and views; commands serialize through the private actor.

const COMMAND_CAPACITY: usize = 64;
pub(crate) const EVENT_CAPACITY: usize = 256;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentInstanceError {
    #[error("agent instance is stopped")]
    Stopped,
    #[error("agent instance owner is unavailable")]
    Unavailable,
    #[error("agent instance is busy")]
    Busy,
    #[error("agent runtime was replaced while waiting")]
    RuntimeReplaced,
    #[error("replacement transcript ID `{actual}` does not match instance `{expected}`")]
    ReplacementIdentity { expected: String, actual: String },
    #[error("replacement cannot change the instance task list binding")]
    ReplacementTaskList,
    #[error(transparent)]
    Turn(#[from] PhiTurnError),
    #[error(transparent)]
    Subagent(#[from] PhiSubagentError),
    #[error("agent instance operation failed: {source}")]
    Operation {
        #[source]
        source: anyhow::Error,
    },
    #[error("agent interaction failed: {source}")]
    Interaction {
        #[source]
        source: anyhow::Error,
    },
}

#[derive(Clone)]
pub struct AgentInstance {
    id: AgentInstanceId,
    task_list: Option<crate::tasks::TaskListBinding>,
    orchestration: Option<crate::harness::manager::policy::InstanceAgentScope>,
    commands: mpsc::Sender<AgentInstanceCommand>,
    snapshots: AgentSnapshotSubscription,
    bash_output: super::bash::BashOutputSubscription,
    execution_pending: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) events: broadcast::Sender<AgentInstanceEvent>,
    final_view: Arc<std::sync::OnceLock<AgentInstanceView>>,
    final_subagents: Arc<std::sync::OnceLock<Vec<SubagentSnapshot>>>,
}

impl AgentInstance {
    pub(crate) fn spawn(candidate: AgentInstanceCandidate) -> Self {
        let id = AgentInstanceId::from_transcript(candidate.transcript());
        let orchestration = candidate.orchestration();
        let state = candidate.into_state();
        let task_list = state.task_list.clone();
        let bash_output = state.subscribe_bash_output();
        let snapshot = AgentInstanceSnapshot {
            id: id.clone(),
            definition: state.definition_name().map(str::to_owned),
            working_dir: state.working_dir().to_path_buf(),
            status: if state.needs_progress() {
                AgentInstanceStatus::Running
            } else {
                AgentInstanceStatus::Idle
            },
            active_turn_id: None,
            live_run: None,
            progress: None,
            bash_executions: Vec::new(),
            pending_interactions: Vec::new(),
            latest_error: None,
            revision: 0,
        };
        let (command_tx, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (snapshot_tx, snapshot_rx) = watch::channel(snapshot);
        let (event_tx, _) = broadcast::channel(EVENT_CAPACITY);
        let actor = AgentInstanceActor::new(state, command_rx, snapshot_tx, event_tx.clone());
        let final_subagents = actor.final_subagents.clone();
        let final_view = actor.final_view.clone();
        let execution_pending = actor.execution_pending.clone();
        tokio::spawn(actor.run());
        Self {
            id,
            task_list,
            orchestration,
            commands: command_tx,
            snapshots: AgentSnapshotSubscription::new(snapshot_rx),
            bash_output,
            execution_pending,
            events: event_tx,
            final_view,
            final_subagents,
        }
    }

    pub(crate) fn execution_pending(&self) -> Result<bool, AgentInstanceError> {
        if self.commands.is_closed() && self.snapshot().status != AgentInstanceStatus::Stopped {
            return Err(AgentInstanceError::Stopped);
        }
        Ok(self
            .execution_pending
            .load(std::sync::atomic::Ordering::Acquire))
    }

    pub fn id(&self) -> &AgentInstanceId {
        &self.id
    }

    pub fn task_list(&self) -> Option<&crate::tasks::TaskListBinding> {
        self.task_list.as_ref()
    }

    /// Observe canonical work records without invoking tools or advancing execution.
    pub async fn observe_tasks(
        &self,
    ) -> Result<Option<crate::tasks::TaskListView>, AgentInstanceError> {
        if self.task_list.is_none() {
            return Ok(None);
        }
        self.task_access()?
            .observe()
            .await
            .map(Some)
            .map_err(|source| AgentInstanceError::Operation {
                source: anyhow::Error::new(source),
            })
    }

    /// Identifies this registration, not a later instance reusing the same Transcript ID.
    pub fn registration_id(&self) -> Option<crate::harness::manager::AgentRegistrationId> {
        self.orchestration
            .as_ref()
            .map(|scope| scope.registration_id())
    }

    /// Subscribe before observing Tasks so concurrent file changes are retained.
    pub async fn subscribe_tasks(
        &self,
    ) -> Result<Option<crate::tasks::TaskSubscription>, AgentInstanceError> {
        if self.task_list.is_none() {
            return Ok(None);
        }
        self.task_access()?
            .subscribe()
            .await
            .map(Some)
            .map_err(|source| AgentInstanceError::Operation { source })
    }

    fn task_access(&self) -> Result<crate::harness::task::TaskListAccess, AgentInstanceError> {
        self.orchestration
            .as_ref()
            .ok_or(AgentInstanceError::Unavailable)?
            .access()
            .map_err(anyhow::Error::new)
            .and_then(|access| Ok(access.task_list()?))
            .map_err(|source| AgentInstanceError::Operation { source })
    }

    /// History-free wakeup; final history is read from the retained instance capability.
    pub fn subscribe_finalization(
        &self,
    ) -> Option<watch::Receiver<crate::harness::manager::AgentFinalizationStatus>> {
        Some(self.orchestration.as_ref()?.finalization.subscribe())
    }

    /// Latest completed stop attempt. Failed cleanup may have no final history; Open/Closing is None.
    pub fn finalization(&self) -> Option<crate::harness::manager::AgentFinalization> {
        use crate::harness::manager::AgentFinalization;
        use crate::harness::manager::AgentFinalizationStatus;
        let scope = self.orchestration.as_ref()?;
        let cleanup_error = match scope.finalization.borrow().clone() {
            AgentFinalizationStatus::Stopped => None,
            AgentFinalizationStatus::Failed { message } => Some(message),
            AgentFinalizationStatus::Open | AgentFinalizationStatus::Closing => return None,
        };
        Some(AgentFinalization {
            instance_id: self.id.clone(),
            registration_id: scope.registration_id(),
            view: self.final_view.get().cloned(),
            cleanup_error,
        })
    }

    pub(crate) fn close_registration(&self) {
        use crate::harness::manager::AgentFinalizationStatus;
        if let Some(scope) = &self.orchestration {
            scope.finalization.send_if_modified(|status| {
                if matches!(status, AgentFinalizationStatus::Open) {
                    *status = AgentFinalizationStatus::Closing;
                    return true;
                }
                false
            });
        }
    }

    pub(crate) fn finish_registration(&self, error: Option<String>) {
        use crate::harness::manager::AgentFinalizationStatus;
        if let Some(scope) = &self.orchestration {
            scope.finalization.send_if_modified(|status| {
                if matches!(status, AgentFinalizationStatus::Stopped) {
                    return false;
                }
                *status = match error {
                    Some(message) => AgentFinalizationStatus::Failed { message },
                    None => AgentFinalizationStatus::Stopped,
                };
                true
            });
        }
    }

    #[cfg(test)]
    pub(crate) fn without_owner(&self) -> Self {
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        drop(receiver);
        Self {
            commands,
            ..self.clone()
        }
    }

    pub(crate) fn final_transcript(&self) -> Option<Transcript> {
        self.final_view.get().map(|view| view.transcript.clone())
    }

    pub fn snapshot(&self) -> AgentInstanceSnapshot {
        self.snapshots.snapshot()
    }

    /// Output invalidations do not change canonical execution status.
    pub fn subscribe_bash_output(&self) -> super::bash::BashOutputSubscription {
        self.bash_output.clone()
    }

    pub fn subscribe_snapshots(&self) -> AgentSnapshotSubscription {
        self.snapshots.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentInstanceEvent> {
        self.events.subscribe()
    }

    pub async fn submit(
        &self,
        prompt: TurnPrompt,
    ) -> Result<SubmissionDisposition, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::Submit { prompt, reply })
            .await?
            .map_err(Into::into)
    }

    pub async fn submit_directed(
        &self,
        prompt: TurnPrompt,
        constraint: super::builder::DirectedDelegation,
    ) -> Result<SubmissionDisposition, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::SubmitDirected {
            prompt,
            constraint,
            reply,
        })
        .await?
        .map_err(Into::into)
    }

    pub async fn dispatch(
        &self,
        prompt: TurnPrompt,
        dispatch: SubmissionDispatch,
    ) -> Result<SubmissionDisposition, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::Dispatch {
            prompt,
            dispatch,
            reply,
        })
        .await?
        .map_err(Into::into)
    }

    pub async fn respond_interaction(
        &self,
        request_id: impl Into<String>,
        response: InteractionResponse,
    ) -> Result<InteractionAnswer, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::RespondInteraction {
            request_id: request_id.into(),
            response,
            reply,
        })
        .await?
        .map_err(|source| AgentInstanceError::Interaction { source })
    }

    pub async fn read_bash_output(&self, execution_id: &str) -> Result<String, AgentInstanceError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        const OUTPUT_PREVIEW_BYTES: u64 = 50 * 1024;
        let execution = self
            .snapshot()
            .bash_executions
            .into_iter()
            .find(|execution| execution.id == execution_id)
            .ok_or_else(|| AgentInstanceError::Operation {
                source: anyhow::anyhow!("Bash execution is unavailable"),
            })?;
        let read = async {
            let mut file = tokio::fs::File::open(execution.output_path).await?;
            let length = file.metadata().await?.len();
            let offset = length.saturating_sub(OUTPUT_PREVIEW_BYTES);
            file.seek(std::io::SeekFrom::Start(offset)).await?;
            let mut bytes = Vec::new();
            file.take(OUTPUT_PREVIEW_BYTES)
                .read_to_end(&mut bytes)
                .await?;
            let start = if offset == 0 {
                0
            } else {
                bytes
                    .iter()
                    .position(|byte| byte & 0xc0 != 0x80)
                    .unwrap_or(bytes.len())
            };
            Ok::<_, std::io::Error>(String::from_utf8_lossy(&bytes[start..]).into_owned())
        };
        read.await.map_err(|source| AgentInstanceError::Operation {
            source: source.into(),
        })
    }

    pub async fn background_bash(
        &self,
        execution_id: impl Into<String>,
    ) -> Result<super::bash::BashExecution, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::BackgroundBash {
            execution_id: execution_id.into(),
            reply,
        })
        .await?
    }

    pub async fn stop_bash(
        &self,
        execution_id: impl Into<String>,
    ) -> Result<(), AgentInstanceError> {
        let execution_id = execution_id.into();
        let mut snapshots = self.subscribe_snapshots();
        self.request(|reply| AgentInstanceCommand::StopBash {
            execution_id: execution_id.clone(),
            reply,
        })
        .await??;
        loop {
            if snapshots
                .snapshot()
                .bash_executions
                .iter()
                .find(|execution| execution.id == execution_id)
                .is_none_or(|execution| execution.status != super::bash::BashStatus::Running)
            {
                return Ok(());
            }
            snapshots
                .changed()
                .await
                .map_err(|_| AgentInstanceError::Stopped)?;
        }
    }

    pub async fn abort(&self) -> Result<(), AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::Abort { reply })
            .await
    }

    pub async fn reset_context(
        &self,
        handoff: Option<String>,
    ) -> Result<AgentInstanceView, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::ResetContext { handoff, reply })
            .await?
    }

    /// Revise future model context while retaining raw history and accounting.
    /// Rejected while execution or pending admission can still advance the Transcript.
    pub async fn edit_context(
        &self,
        target_id: String,
        replacement: Option<crate::transcript::ContextReplacement>,
    ) -> Result<AgentInstanceView, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::EditContext {
            target_id,
            replacement,
            reply,
        })
        .await?
    }

    pub async fn transcript(&self) -> Result<Transcript, AgentInstanceError> {
        Ok(self.view().await?.transcript)
    }

    pub async fn view(&self) -> Result<AgentInstanceView, AgentInstanceError> {
        self.read_view(None).await
    }

    /// Observes local work and registration-scoped child/message activity, not success.
    /// Later admission may follow the captured actor/Manager boundary. This never closes input
    /// or stops execution; dropping the wait has no execution effect. Runtime replacement or
    /// stop invalidates a pending wait. At most 64 concurrent waits are admitted (otherwise Busy).
    /// If supplied, the event boundary is published before returning the coherent view.
    pub async fn wait_for_quiescence(
        &self,
        event_boundary: Option<String>,
    ) -> Result<AgentInstanceView, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::WaitForQuiescence {
            event_boundary,
            reply,
        })
        .await?
    }

    /// Subscribe first, use a fresh correlation ID, then discard through its boundary marker.
    /// A lagged or missing marker requires another view, never blind queue clearing.
    pub async fn view_with_event_boundary(
        &self,
        id: String,
    ) -> Result<AgentInstanceView, AgentInstanceError> {
        self.read_view(Some(id)).await
    }

    async fn read_view(
        &self,
        event_boundary: Option<String>,
    ) -> Result<AgentInstanceView, AgentInstanceError> {
        if let Some(view) = self.final_view.get() {
            return Ok(self.final_view_boundary(view, event_boundary));
        }
        match self
            .request(|reply| AgentInstanceCommand::View {
                event_boundary: event_boundary.clone(),
                reply,
            })
            .await
        {
            Ok(view) => Ok(view),
            Err(error) => self
                .final_view
                .get()
                .map(|view| self.final_view_boundary(view, event_boundary))
                .ok_or(error),
        }
    }

    fn final_view_boundary(
        &self,
        view: &AgentInstanceView,
        event_boundary: Option<String>,
    ) -> AgentInstanceView {
        if let Some(id) = event_boundary {
            let _ = self.events.send(AgentInstanceEvent::ViewBoundary { id });
        }
        view.clone()
    }

    pub async fn tool_display(&self) -> Result<ToolDisplayRegistry, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::ToolDisplay { reply })
            .await
    }

    pub async fn supported_agents(
        &self,
    ) -> Result<crate::agent::custom::AgentCatalog, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::SupportedAgents { reply })
            .await
    }

    pub async fn start_delegated_agent(
        &self,
        name: impl Into<String>,
        prompt: impl Into<String>,
        parent_agent_id: impl Into<String>,
    ) -> Result<SubmissionDisposition, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::StartDelegatedAgent {
            name: name.into(),
            prompt: prompt.into(),
            parent_agent_id: parent_agent_id.into(),
            parent_tool_call_id: None,
            mode: crate::agent_manager::DelegationMode::Foreground,
            completion: None,
            reply,
        })
        .await?
        .map_err(Into::into)
    }

    pub(crate) async fn start_created_agent(
        &self,
        creation: &crate::harness::manager::preparation::AgentCreation,
        parent_tool_call_id: Option<compact_str::CompactString>,
        completion: crate::harness::instance::child::InitialCompletion,
    ) -> Result<SubmissionDisposition, AgentInstanceError> {
        if creation.instance_id() != self.id() {
            return Err(AgentInstanceError::Operation {
                source: anyhow::anyhow!("creation targets another instance"),
            });
        }
        self.request(|reply| AgentInstanceCommand::StartDelegatedAgent {
            name: creation.definition().into(),
            prompt: creation.prompt().into(),
            parent_agent_id: creation.creator_id().as_str().into(),
            parent_tool_call_id,
            mode: creation.mode(),
            completion: Some(completion),
            reply,
        })
        .await?
        .map_err(Into::into)
    }

    pub(crate) async fn await_initial_completion(
        &self,
        completion: &crate::harness::instance::child::InitialCompletion,
    ) -> Result<(), AgentInstanceError> {
        let mut snapshots = self.subscribe_snapshots();
        loop {
            if snapshots.snapshot().status == AgentInstanceStatus::Stopped
                && !completion.is_complete()
            {
                return Err(AgentInstanceError::Stopped);
            }
            tokio::select! {
                biased;
                _ = completion.wait_complete() => return Ok(()),
                changed = snapshots.changed() => {
                    if changed.is_err() { return Err(AgentInstanceError::Unavailable); }
                }
            }
        }
    }

    /// Returns immutable completion facts without stopping or cloning the retained history.
    pub async fn complete_delegated_agent(
        &self,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<crate::agents::SubagentCompletion, AgentInstanceError> {
        tokio::select! {
            biased;
            _ = crate::harness::manager::preparation::wait_for_cancellation(&mut cancellation) => {
                self.request(|reply| AgentInstanceCommand::AwaitDelegatedCompletion { cancel: true, reply }).await?
            }
            result = self.request(|reply| AgentInstanceCommand::AwaitDelegatedCompletion { cancel: false, reply }) => result?,
        }
    }

    /// Returns creation facts in either mode; use the child completion API to wait.
    /// Foreground mode suppresses automatic initial feedback to this parent.
    pub async fn spawn_subagent(
        &self,
        request: AgentSpawnRequest,
    ) -> Result<AgentCreation, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::SpawnSubagent { request, reply })
            .await?
            .map_err(Into::into)
    }

    pub(crate) async fn capture_workflow_scope(
        &self,
        default_role: Option<String>,
        limits: crate::harness::workflow::WorkflowLimits,
    ) -> Result<execution::WorkflowScope, crate::harness::workflow::WorkflowError> {
        self.request(|reply| AgentInstanceCommand::CaptureWorkflowScope {
            default_role,
            limits,
            reply,
        })
        .await?
    }

    pub(crate) async fn deliver_workflow_report(
        &self,
        report: crate::harness::workflow::WorkflowReport,
    ) -> Result<(), AgentInstanceError> {
        if report.creator() != self.id() || Some(report.registration()) != self.registration_id() {
            return Err(AgentInstanceError::Operation {
                source: anyhow::anyhow!("Workflow report creator registration mismatch"),
            });
        }
        let id = report.id();
        let result = self
            .request(|reply| AgentInstanceCommand::DeliverWorkflowReport { report, reply })
            .await
            .and_then(|result| result);
        if result.is_err()
            && let Some(view) = self.final_view.get()
            && execution::workflow_report_delivered(&view.transcript, id)
        {
            return Ok(());
        }
        result
    }

    pub(crate) async fn observe_started_child(
        &self,
        creation: crate::agents::AgentCreation,
        completion: crate::harness::instance::child::InitialCompletion,
        child: AgentInstance,
    ) -> Result<(), AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::ObserveStartedChild {
            creation,
            completion,
            child,
            reply,
        })
        .await?
        .map_err(Into::into)
    }

    pub async fn subagent_snapshots(&self) -> Result<Vec<SubagentSnapshot>, AgentInstanceError> {
        if let Some(snapshots) = self.final_subagents.get() {
            return Ok(snapshots.clone());
        }
        match self
            .request(|reply| AgentInstanceCommand::SubagentSnapshots { reply })
            .await
        {
            Ok(snapshots) => Ok(snapshots),
            Err(error) => self.final_subagents.get().cloned().ok_or(error),
        }
    }

    pub async fn subagent_status(
        &self,
        instance_id: AgentInstanceId,
    ) -> Result<Option<SubagentStatus>, AgentInstanceError> {
        if let Some(snapshots) = self.final_subagents.get() {
            return Ok(snapshots
                .iter()
                .find(|snapshot| snapshot.status.creation.instance_id() == &instance_id)
                .map(|snapshot| snapshot.status.clone()));
        }
        self.request(|reply| AgentInstanceCommand::SubagentStatus { instance_id, reply })
            .await
    }

    pub async fn cancel_subagent(
        &self,
        instance_id: AgentInstanceId,
    ) -> Result<bool, AgentInstanceError> {
        if self.snapshot().status == AgentInstanceStatus::Stopped {
            return Err(AgentInstanceError::Stopped);
        }
        self.request(|reply| AgentInstanceCommand::CancelSubagent { instance_id, reply })
            .await?
            .finish()
            .await
            .map_err(|source| AgentInstanceError::Operation {
                source: source.into(),
            })
    }

    pub async fn replace_runtime(
        &self,
        candidate: RuntimeCandidate,
    ) -> Result<Transcript, AgentInstanceError> {
        self.request(|reply| AgentInstanceCommand::ReplaceRuntime {
            candidate: Box::new(candidate),
            reply,
        })
        .await?
    }

    pub async fn stop(&self) -> Result<Transcript, AgentInstanceError> {
        let failed_cleanup = self.orchestration.as_ref().is_some_and(|scope| {
            matches!(
                *scope.finalization.borrow(),
                crate::harness::manager::AgentFinalizationStatus::Failed { .. }
            )
        });
        if self.snapshot().status == AgentInstanceStatus::Stopped && !failed_cleanup {
            return Err(AgentInstanceError::Stopped);
        }
        if let Some(scope) = &self.orchestration {
            return scope.stop(self.clone()).await.map_err(|source| {
                AgentInstanceError::Operation {
                    source: source.into(),
                }
            });
        }
        self.stop_local().await
    }

    pub(crate) async fn stop_local(&self) -> Result<Transcript, AgentInstanceError> {
        if self.snapshot().status == AgentInstanceStatus::Stopped {
            return Err(AgentInstanceError::Stopped);
        }
        self.request(|reply| AgentInstanceCommand::Stop { reply })
            .await
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<T>) -> AgentInstanceCommand,
    ) -> Result<T, AgentInstanceError> {
        if self.snapshot().status == AgentInstanceStatus::Stopped {
            return Err(AgentInstanceError::Stopped);
        }
        let (reply, response) = oneshot::channel();
        let command = command(reply);
        let closed = self.orchestration.as_ref().is_some_and(|scope| {
            !matches!(
                *scope.finalization.borrow(),
                crate::harness::manager::AgentFinalizationStatus::Open
            )
        });
        if closed
            && matches!(
                command,
                AgentInstanceCommand::Submit { .. }
                    | AgentInstanceCommand::SubmitDirected { .. }
                    | AgentInstanceCommand::Dispatch { .. }
                    | AgentInstanceCommand::RespondInteraction { .. }
                    | AgentInstanceCommand::StartDelegatedAgent { .. }
                    | AgentInstanceCommand::SpawnSubagent { .. }
                    | AgentInstanceCommand::CaptureWorkflowScope { .. }
                    | AgentInstanceCommand::BackgroundBash { .. }
                    | AgentInstanceCommand::ObserveStartedChild { .. }
                    | AgentInstanceCommand::ReplaceRuntime { .. }
                    | AgentInstanceCommand::ResetContext { .. }
                    | AgentInstanceCommand::EditContext { .. }
                    | AgentInstanceCommand::WaitForQuiescence { .. }
            )
        {
            return Err(AgentInstanceError::Stopped);
        }
        self.commands
            .send(command)
            .await
            .map_err(|_| AgentInstanceError::Unavailable)?;
        response.await.map_err(|_| AgentInstanceError::Unavailable)
    }
}

/// Controls submission dispatch when an agent run is already active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionDispatch {
    /// Dispatch before the active run's next provider request.
    Steer,
    /// Dispatch as a new turn after the active run and earlier queued work settle.
    FollowUp,
}

impl SubmissionDispatch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "follow_up",
        }
    }
}

/// How a submission was handled, not its eventual execution outcome.
/// `Started` identifies a new outer turn; queued Steer joins the active turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SubmissionDisposition {
    Started {
        turn_id: String,
        prompt: String,
    },
    Queued {
        submission_id: crate::core::turn::SubmissionId,
        dispatch: SubmissionDispatch,
        prompt: String,
    },
    Handled {
        reason: Option<String>,
    },
}

// Instance identity and execution projections; views pair history and state at one actor boundary.

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentInstanceId(String);

impl AgentInstanceId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn from_transcript(transcript: &Transcript) -> Self {
        Self::new(transcript.id.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for AgentInstanceId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for AgentInstanceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<String> for AgentInstanceId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for AgentInstanceId {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentInstanceStatus {
    Idle,
    Running,
    RequiresAction,
    Failed,
    Stopping,
    Stopped,
}

const LIVE_TEXT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveToolSnapshot {
    pub id: String,
    pub name: String,
    pub args: serde_json::Value,
    pub display: Option<crate::core::event::ToolDisplay>,
}

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LiveRunSnapshot {
    pub assistant_text: String,
    pub reasoning_text: String,
    pub tool: Option<LiveToolSnapshot>,
}

impl LiveRunSnapshot {
    pub(crate) fn observe(live: &mut Option<Self>, event: &PhiEvent) {
        match event {
            PhiEvent::TurnStarted { .. } => *live = Some(Self::default()),
            PhiEvent::AssistantDelta { display, .. } => {
                let run = live.get_or_insert_with(Self::default);
                run.assistant_text.push_str(display);
                keep_live_tail(&mut run.assistant_text);
            }
            PhiEvent::ReasoningDelta { text } => {
                if let Some(run) = live {
                    run.reasoning_text = text.clone();
                    keep_live_tail(&mut run.reasoning_text);
                }
            }
            PhiEvent::ToolCallStarted {
                id,
                name,
                args,
                display,
                ..
            } => {
                let run = live.get_or_insert_with(Self::default);
                run.assistant_text.clear();
                run.reasoning_text.clear();
                run.tool = Some(LiveToolSnapshot {
                    id: id.clone(),
                    name: name.clone(),
                    args: args.clone(),
                    display: display.clone(),
                });
            }
            PhiEvent::ToolCallFinished { id, .. } => {
                if let Some(run) = live
                    && run.tool.as_ref().is_some_and(|tool| tool.id == *id)
                {
                    run.tool = None;
                }
            }
            PhiEvent::AssistantFinished { .. } => {
                if let Some(run) = live {
                    run.assistant_text.clear();
                }
            }
            PhiEvent::ReasoningFinished { .. } => {
                if let Some(run) = live {
                    run.reasoning_text.clear();
                }
            }
            PhiEvent::TurnFinished { .. } | PhiEvent::TurnFailed { .. } => *live = None,
            _ => {}
        }
    }
}

fn keep_live_tail(text: &mut String) {
    if text.len() <= LIVE_TEXT_LIMIT {
        return;
    }
    let mut start = text.len() - LIVE_TEXT_LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text.drain(..start);
}

#[derive(Debug, Clone)]
pub struct AgentInstanceSnapshot {
    pub id: AgentInstanceId,
    pub definition: Option<String>,
    pub working_dir: std::path::PathBuf,
    pub status: AgentInstanceStatus,
    pub active_turn_id: Option<String>,
    pub live_run: Option<LiveRunSnapshot>,
    /// Latest outer turn, retained after completion until another turn starts.
    pub progress: Option<super::ExecutionProgress>,
    pub bash_executions: Vec<super::bash::BashExecution>,
    pub pending_interactions: Vec<InteractionEnvelope>,
    /// Current failure, cleared on turn start, abort or runtime replacement.
    pub latest_error: Option<crate::PhiFailure>,
    pub revision: u64,
}

/// Latest-value observation without consumer-held publisher locks.
#[derive(Clone)]
pub struct AgentSnapshotSubscription {
    receiver: watch::Receiver<AgentInstanceSnapshot>,
}

impl AgentSnapshotSubscription {
    fn new(receiver: watch::Receiver<AgentInstanceSnapshot>) -> Self {
        Self { receiver }
    }

    /// Copies the current value without marking it seen.
    pub fn snapshot(&self) -> AgentInstanceSnapshot {
        self.receiver.borrow().clone()
    }

    /// Returns an owned latest value and marks it seen; updates may coalesce.
    pub async fn changed(&mut self) -> Result<AgentInstanceSnapshot, watch::error::RecvError> {
        self.receiver.changed().await?;
        Ok(self.receiver.borrow_and_update().clone())
    }
}

/// History and execution captured together at one actor serialization point.
#[derive(Debug, Clone)]
pub struct AgentInstanceView {
    /// Accepted submissions not yet committed or explicitly discarded.
    pub pending_submission_ids: Vec<crate::core::turn::SubmissionId>,
    pub transcript: Transcript,
    pub snapshot: AgentInstanceSnapshot,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AgentInstanceEvent {
    Phi(PhiEventEnvelope),
    Subagent(SubagentActivity),
    Progress(super::ExecutionProgress),
    RunCancelled {
        /// Aborted outer turn, or None when cancellation found no active turn.
        turn_id: Option<String>,
        reason: crate::CancellationReason,
    },
    InteractionRequested(InteractionEnvelope),
    InteractionClosed {
        request_id: String,
    },
    PumpFailed {
        message: String,
    },
    RuntimeReplaced,
    /// Canonical history changed; this is not a persistence acknowledgement or a turn.
    HistoryChanged,
    ContextReset {
        entry_id: String,
    },
    /// Events before this marker are covered by the correlated view.
    ViewBoundary {
        id: String,
    },
    Stopped,
}

pub(crate) fn snapshot_is_current(
    current: &AgentInstanceSnapshot,
    status: AgentInstanceStatus,
    active_turn_id: Option<&str>,
    live_run: Option<&LiveRunSnapshot>,
    progress: Option<&super::ExecutionProgress>,
    pending_interactions: &[InteractionEnvelope],
    latest_error: Option<&crate::PhiFailure>,
) -> bool {
    current.status == status
        && current.active_turn_id.as_deref() == active_turn_id
        && current.live_run.as_ref() == live_run
        && current.progress.as_ref() == progress
        && current.latest_error.as_ref() == latest_error
        && current.pending_interactions.len() == pending_interactions.len()
        && current
            .pending_interactions
            .iter()
            .zip(pending_interactions)
            .all(|(left, right)| left.request_id == right.request_id)
}

/// Owned stream for a one-shot `Phi::query(...)` execution.
pub struct PhiStream {
    instance: AgentInstance,
    _manager: crate::harness::manager::AgentManager,
    events: broadcast::Receiver<AgentInstanceEvent>,
    finished: bool,
}

impl PhiStream {
    pub(super) fn new(
        instance: AgentInstance,
        events: broadcast::Receiver<AgentInstanceEvent>,
        manager: crate::harness::manager::AgentManager,
    ) -> Self {
        Self {
            instance,
            _manager: manager,
            events,
            finished: false,
        }
    }

    pub async fn next(&mut self) -> Result<Option<PhiEventEnvelope>, PhiStreamError> {
        if self.finished {
            return Ok(None);
        }
        loop {
            match self.events.recv().await {
                Ok(AgentInstanceEvent::Phi(envelope)) => {
                    self.finished = matches!(
                        envelope.event,
                        PhiEvent::TurnFinished { .. } | PhiEvent::TurnFailed { .. }
                    );
                    return Ok(Some(envelope));
                }
                Ok(AgentInstanceEvent::RunCancelled { .. }) => {
                    self.finished = true;
                    return Err(PhiStreamError::Cancelled);
                }
                Ok(AgentInstanceEvent::PumpFailed { message }) => {
                    self.finished = true;
                    return Err(PhiStreamError::Operation {
                        source: anyhow::anyhow!(message),
                    });
                }
                Ok(AgentInstanceEvent::Stopped) => {
                    self.finished = true;
                    return Ok(None);
                }
                Ok(_) => {}
                Err(error) => {
                    self.finished = true;
                    return Err(PhiStreamError::Operation {
                        source: anyhow::Error::new(error),
                    });
                }
            }
        }
    }

    pub async fn next_event(&mut self) -> Result<Option<PhiEvent>, PhiStreamError> {
        Ok(self.next().await?.map(|envelope| envelope.event))
    }

    pub async fn interrupt(&self) -> Result<(), PhiStreamError> {
        self.instance
            .abort()
            .await
            .map_err(|source| PhiStreamError::Operation {
                source: anyhow::Error::new(source),
            })
    }

    pub async fn close(&self) -> Result<(), PhiStreamError> {
        self.interrupt().await
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PhiStreamError {
    #[error("query cancelled")]
    Cancelled,
    #[error(transparent)]
    Turn(#[from] PhiTurnError),
    #[error(transparent)]
    Subagent(#[from] PhiSubagentError),
    #[error("stream operation failed: {source}")]
    Operation {
        #[source]
        source: anyhow::Error,
    },
}
pub struct Query<'a> {
    phi: &'a Phi,
    prompt: String,
    options: AgentInstanceOptions,
}

impl<'a> Query<'a> {
    fn new(phi: &'a Phi, prompt: impl Into<String>) -> Self {
        Self {
            phi,
            prompt: prompt.into(),
            options: AgentInstanceOptions::default(),
        }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.options.model = Some(model.into());
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options.tool_policy.allowed = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn available_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options.tool_policy.allowed = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn disallowed_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options.tool_policy.disallowed = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.options.system_prompt = Some(prompt.into());
        self
    }

    pub fn agent_mode(mut self, mode: crate::config::AgentMode) -> Self {
        self.options.agent_mode = Some(mode);
        self
    }

    pub fn mode_prompt_override(mut self, prompt: impl Into<String>) -> Self {
        self.options.mode_prompt_override = Some(prompt.into());
        self
    }

    pub async fn stream(self) -> Result<PhiStream, PhiQueryError> {
        let prepared = self
            .phi
            .prepare_agent_instance_with_options(self.options)
            .await?;
        let manager = AgentManager::new();
        let instance = manager.insert(prepared).map_err(|source| {
            PhiQueryError::Stream(PhiStreamError::Operation {
                source: anyhow::Error::new(source),
            })
        })?;
        let events = instance.subscribe();
        match instance
            .submit(crate::core::turn::TurnPrompt {
                tool_policy: Default::default(),
                prompt: self.prompt,
                source: UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .map_err(|source| {
                PhiQueryError::Stream(PhiStreamError::Operation {
                    source: anyhow::Error::new(source),
                })
            })? {
            execution::SubmissionDisposition::Started { .. } => {
                Ok(PhiStream::new(instance, events, manager))
            }
            execution::SubmissionDisposition::Queued { .. } => Err(PhiQueryError::NotStarted {
                reason: "prompt was queued unexpectedly".to_string(),
            }),
            execution::SubmissionDisposition::Handled { reason } => {
                Err(PhiQueryError::NotStarted {
                    reason: reason.unwrap_or_else(|| "prompt was handled".to_string()),
                })
            }
        }
    }

    pub async fn collect_text(self) -> Result<String, PhiQueryError> {
        let mut stream = self.stream().await?;
        while let Some(event) = stream.next_event().await? {
            match event {
                PhiEvent::TurnFinished { response, .. } => return Ok(response),
                PhiEvent::TurnFailed { failure } => return Err(PhiQueryError::Execution(failure)),
                _ => {}
            }
        }
        Err(PhiQueryError::NotStarted {
            reason: "query ended without a final response".to_string(),
        })
    }
}

impl Phi {
    pub fn query(&self, prompt: impl Into<String>) -> Query<'_> {
        Query::new(self, prompt)
    }

    pub async fn query_text(&self, prompt: impl Into<String>) -> Result<String, PhiQueryError> {
        self.query(prompt).collect_text().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(revision: u64) -> AgentInstanceSnapshot {
        AgentInstanceSnapshot {
            id: AgentInstanceId::new("observed"),
            definition: None,
            working_dir: std::path::PathBuf::new(),
            status: AgentInstanceStatus::Idle,
            active_turn_id: None,
            live_run: None,
            progress: None,
            bash_executions: Vec::new(),
            pending_interactions: Vec::new(),
            latest_error: None,
            revision,
        }
    }

    #[tokio::test]
    async fn owned_snapshots_preserve_latest_value_clone_and_closure_semantics() {
        for updates in [1, 2, 16, 128] {
            let (publisher, receiver) = watch::channel(snapshot(0));
            let mut subscription = AgentSnapshotSubscription::new(receiver);
            let retained = subscription.snapshot();
            assert!(futures::poll!(Box::pin(subscription.changed())).is_pending());
            let mut peer = subscription.clone();
            for revision in 1..=updates {
                publisher.send_replace(snapshot(revision));
                assert_eq!(retained.revision, 0);
            }
            assert_eq!(subscription.snapshot().revision, updates);
            assert_eq!(subscription.changed().await.unwrap().revision, updates);
            assert!(futures::poll!(Box::pin(subscription.changed())).is_pending());
            assert_eq!(peer.changed().await.unwrap().revision, updates);
            let mut inherited = peer.clone();
            assert!(futures::poll!(Box::pin(inherited.changed())).is_pending());
            drop(peer);
            publisher.send_replace(snapshot(updates + 1));
            drop(publisher);
            assert_eq!(subscription.changed().await.unwrap().revision, updates + 1);
            assert_eq!(inherited.changed().await.unwrap().revision, updates + 1);
            assert!(subscription.changed().await.is_err());
            assert!(inherited.changed().await.is_err());
            assert_eq!(subscription.snapshot().revision, updates + 1);
            assert_eq!(retained.revision, 0);
        }
    }
}
