use crate::agent::custom::AgentCatalog;
use crate::core::event::PhiFailure;
use crate::factory::Phi;
use crate::harness::instance::child::InitialCompletion;
use crate::harness::manager::AgentManagerError;
use crate::harness::manager::coordinator::{ChildCompletion, ReservedSubagent};
use crate::harness::manager::preparation::{AgentCreation, SubagentPreparation};
use futures::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

pub(super) const MAX_OBSERVATIONS: usize = 1000;
const MAX_RETAINED_RUNS: usize = 64;
const MAX_CONTROL_WAITERS: usize = 64;
const MAX_WORKFLOW_INVOCATIONS: usize = 1000;
const MAX_WORKFLOW_BYTES: usize = 16 * 1024 * 1024;
const SCRIPT_ERROR_PREFIX: &str = "workflow script failed: ";
const DIAGNOSTIC_TRUNCATION: &str = "[truncated]";

type WorkflowResult<T> = Result<T, Arc<WorkflowError>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct WorkflowRunId(uuid::Uuid);

impl WorkflowRunId {
    pub(crate) fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl std::fmt::Display for WorkflowRunId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::str::FromStr for WorkflowRunId {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse().map(Self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InvocationId(pub(super) usize);

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("invalid workflow request: {0}")]
    InvalidRequest(String),
    #[error("workflow sandbox could not be prepared: {0}")]
    Engine(#[from] phi_codemode::SandboxBuildError),
    #[error("{SCRIPT_ERROR_PREFIX}{}", .0.message)]
    Script(phi_codemode::CodemodeError),
    #[error("workflow invocation requires a role or a configured default role")]
    MissingRole,
    #[error("unknown workflow Agent role `{0}`")]
    UnknownRole(String),
    #[error("workflow Agent capability is unavailable")]
    Unavailable,
    #[error("workflow has stopped accepting work")]
    Stopped,
    #[error("workflow pending invocation limit reached")]
    PendingLimit,
    #[error("workflow total invocation limit reached")]
    InvocationLimit,
    #[error("workflow result byte limit reached")]
    ResultLimit,
    #[error("workflow output limit reached")]
    OutputLimit,
    #[error("workflow invocation byte limit reached")]
    InvocationBytesLimit,
    #[error("workflow execution coordination failed")]
    Coordination,
    #[error("workflow worker failed: {}", .0.message)]
    Worker(PhiFailure),
    #[error("workflow owner is unavailable: {0}")]
    Owner(#[from] crate::harness::instance::AgentInstanceError),
    #[error("workflow Agent permission denied: {0}")]
    Permission(#[source] anyhow::Error),
    #[error("workflow Agent admission failed: {0}")]
    Admission(#[source] AgentManagerError),
    #[error("worker cleanup failed: {0}")]
    Cleanup(#[source] AgentManagerError),
}

#[derive(Debug, Clone, Copy)]
pub struct WorkflowLimits {
    max_concurrent_workers: usize,
    max_pending_invocations: usize,
    max_total_invocations: usize,
    max_result_bytes: usize,
    max_output_bytes: usize,
}

impl WorkflowLimits {
    pub(super) fn concurrency(&self) -> usize {
        self.max_concurrent_workers
    }
    pub(super) fn total_invocations(&self) -> usize {
        self.max_total_invocations
    }
    pub(super) fn output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    pub fn new(
        max_concurrent_workers: usize,
        max_pending_invocations: usize,
        max_total_invocations: usize,
        max_result_bytes: usize,
        max_output_bytes: usize,
    ) -> Result<Self, WorkflowError> {
        if [
            max_concurrent_workers,
            max_pending_invocations,
            max_total_invocations,
            max_result_bytes,
            max_output_bytes,
        ]
        .contains(&0)
            || max_concurrent_workers > max_total_invocations
            || max_pending_invocations > max_total_invocations
            || max_total_invocations > MAX_WORKFLOW_INVOCATIONS
            || max_result_bytes > MAX_WORKFLOW_BYTES
            || max_output_bytes > MAX_WORKFLOW_BYTES
        {
            return Err(WorkflowError::InvalidRequest(
                "limits must be positive, concurrency/pending cannot exceed total invocations, and invocation/byte ceilings must be respected"
                    .into(),
            ));
        }
        Ok(Self {
            max_concurrent_workers,
            max_pending_invocations,
            max_total_invocations,
            max_result_bytes,
            max_output_bytes,
        })
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowAgentOptions {
    role: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentInvocation {
    id: InvocationId,
    role: String,
    prompt: String,
    label: Option<String>,
    phase: Option<String>,
}

impl AgentInvocation {
    pub(crate) fn resolve(
        id: InvocationId,
        prompt: String,
        options: WorkflowAgentOptions,
        phase: Option<String>,
        definitions: &AgentCatalog,
        default_role: Option<&str>,
    ) -> Result<Self, WorkflowError> {
        if prompt.trim().is_empty() {
            return Err(WorkflowError::InvalidRequest(
                "Agent prompt is empty".into(),
            ));
        }
        let role = options
            .role
            .or_else(|| default_role.map(str::to_owned))
            .ok_or(WorkflowError::MissingRole)?;
        if definitions.get(&role).is_none() {
            return Err(WorkflowError::UnknownRole(role));
        }
        Ok(Self {
            id,
            role,
            prompt,
            label: options.label,
            phase,
        })
    }

    fn retained_bytes(&self) -> usize {
        [
            Some(self.role.as_str()),
            Some(self.prompt.as_str()),
            self.label.as_deref(),
            self.phase.as_deref(),
        ]
        .into_iter()
        .flatten()
        .fold(0usize, |bytes, text| bytes.saturating_add(text.len()))
    }

    pub(crate) fn request(&self) -> crate::harness::manager::preparation::AgentSpawnRequest {
        crate::harness::manager::preparation::AgentSpawnRequest::new(
            self.role.clone(),
            self.prompt.clone(),
        )
    }
}

// Retain the reservation after failed startup: its compensation may need another stop attempt.
pub(crate) struct WorkflowAdmission {
    reserved: ReservedSubagent,
    completion: InitialCompletion,
    observation: Option<ChildCompletion>,
}

impl WorkflowAdmission {
    pub(crate) fn new(reserved: ReservedSubagent) -> Self {
        Self {
            completion: reserved.completion(),
            reserved,
            observation: None,
        }
    }

    pub(crate) fn creation(&self) -> &AgentCreation {
        self.reserved.creation()
    }

    fn worker(&self) -> WorkflowWorkerSnapshot {
        WorkflowWorkerSnapshot {
            instance_id: self.creation().instance_id().clone(),
            registration: self.reserved.registration(),
        }
    }

    pub(crate) async fn admit_agent(
        &mut self,
        phi: &Phi,
        preparation: Option<Arc<dyn SubagentPreparation>>,
        cancellation: watch::Receiver<bool>,
    ) -> Result<(), WorkflowError> {
        self.observation = Some(
            self.reserved
                .admit_workflow(phi, preparation, cancellation)
                .await
                .map_err(WorkflowError::Admission)?,
        );
        Ok(())
    }

    pub(crate) async fn await_completion(&mut self) -> Result<String, PhiFailure> {
        let Some(observation) = self.observation.take() else {
            return Err(PhiFailure::new(
                crate::PhiFailureKind::Internal,
                "workflow worker has not been admitted",
                None,
            ));
        };
        let observed = observation.finish().await;
        self.completion
            .get()
            .map(|completion| completion.outcome)
            .unwrap_or_else(|| {
                Err(PhiFailure::new(
                    crate::PhiFailureKind::Internal,
                    observed
                        .err()
                        .unwrap_or_else(|| "workflow worker completion is unavailable".into()),
                    None,
                ))
            })
    }

    pub(crate) async fn finalize_worker(&mut self) -> Result<(), WorkflowError> {
        self.reserved
            .finalize_workflow()
            .await
            .map_err(WorkflowError::Cleanup)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Running,
    Finishing,
    Stopping,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone)]
pub(super) enum ScriptOutcome {
    Returned(Option<Value>),
    Failed(Arc<WorkflowError>),
    Cancelled,
}

fn bound_script_diagnostic(
    error: &phi_codemode::CodemodeError,
    budget: usize,
) -> (phi_codemode::CodemodeError, usize) {
    let bytes = error
        .message
        .len()
        .saturating_add(error.name.as_ref().map_or(0, String::len))
        .saturating_add(error.stack.as_ref().map_or(0, String::len));
    if bytes <= budget {
        return (error.clone(), bytes);
    }
    let marker = if DIAGNOSTIC_TRUNCATION.len() <= budget {
        DIAGNOSTIC_TRUNCATION
    } else {
        ""
    };
    let mut remaining = budget - marker.len();
    let mut retain = |text: &str| {
        let end = text.floor_char_boundary(remaining);
        remaining -= end;
        text[..end].to_owned()
    };
    let mut message = retain(&error.message);
    let name = error
        .name
        .as_deref()
        .map(&mut retain)
        .filter(|text| !text.is_empty());
    let stack = error
        .stack
        .as_deref()
        .map(&mut retain)
        .filter(|text| !text.is_empty());
    message.push_str(marker);
    (
        phi_codemode::CodemodeError {
            kind: error.kind,
            name,
            message,
            stack,
        },
        budget - remaining,
    )
}

#[derive(Debug, Clone)]
struct WorkflowOutcome {
    status: WorkflowStatus,
    script: ScriptOutcome,
    failure: Option<Arc<WorkflowError>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowInvocationStatus {
    Queued,
    Preparing,
    Running,
    Finalizing,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowWorkerSnapshot {
    instance_id: crate::agent_instance::AgentInstanceId,
    registration: Option<crate::agent_manager::AgentRegistrationId>,
}

impl WorkflowWorkerSnapshot {
    /// Reserved identity; it does not prove that a worker was registered or persisted.
    pub fn instance_id(&self) -> &crate::agent_instance::AgentInstanceId {
        &self.instance_id
    }
    pub fn registration(&self) -> Option<crate::agent_manager::AgentRegistrationId> {
        self.registration
    }
}

#[derive(Debug, Clone)]
pub struct WorkflowInvocationSnapshot {
    invocation: AgentInvocation,
    status: WorkflowInvocationStatus,
    worker: Option<WorkflowWorkerSnapshot>,
    outcome: Option<WorkflowResult<String>>,
}

impl WorkflowInvocationSnapshot {
    /// Zero-based accepted request order within this run, not completion order.
    pub fn id(&self) -> usize {
        self.invocation.id.0
    }
    pub fn role(&self) -> &str {
        &self.invocation.role
    }
    pub fn prompt(&self) -> &str {
        &self.invocation.prompt
    }
    pub fn label(&self) -> Option<&str> {
        self.invocation.label.as_deref()
    }
    pub fn phase(&self) -> Option<&str> {
        self.invocation.phase.as_deref()
    }
    pub fn status(&self) -> WorkflowInvocationStatus {
        self.status
    }
    pub fn worker(&self) -> Option<&WorkflowWorkerSnapshot> {
        self.worker.as_ref()
    }
    pub fn value(&self) -> Option<&str> {
        self.outcome.as_ref()?.as_ref().ok().map(String::as_str)
    }
    pub fn error(&self) -> Option<&WorkflowError> {
        self.outcome.as_ref()?.as_ref().err().map(Arc::as_ref)
    }
}

#[derive(Debug, Clone)]
pub struct WorkflowSnapshot {
    id: WorkflowRunId,
    creator: crate::harness::instance::AgentInstanceId,
    registration: crate::harness::manager::AgentRegistrationId,
    status: WorkflowStatus,
    active: usize,
    pending: usize,
    completed: usize,
    invocations: Vec<WorkflowInvocationSnapshot>,
    cleanup_errors: Vec<String>,
    phase: Option<String>,
    logs: Vec<String>,
    outcome: Option<WorkflowOutcome>,
}

impl WorkflowSnapshot {
    pub fn id(&self) -> WorkflowRunId {
        self.id
    }
    pub fn creator(&self) -> &crate::agent_instance::AgentInstanceId {
        &self.creator
    }
    pub fn registration(&self) -> crate::agent_manager::AgentRegistrationId {
        self.registration
    }
    pub fn status(&self) -> WorkflowStatus {
        self.status
    }
    pub fn active(&self) -> usize {
        self.active
    }
    pub fn pending(&self) -> usize {
        self.pending
    }
    pub fn completed(&self) -> usize {
        self.completed
    }
    pub fn invocations(&self) -> &[WorkflowInvocationSnapshot] {
        &self.invocations
    }
    pub fn phase(&self) -> Option<&str> {
        self.phase.as_deref()
    }
    pub fn logs(&self) -> &[String] {
        &self.logs
    }
    pub fn cleanup_errors(&self) -> &[String] {
        &self.cleanup_errors
    }
    pub fn error(&self) -> Option<&WorkflowError> {
        let outcome = self.outcome.as_ref()?;
        outcome
            .failure
            .as_deref()
            .or_else(|| match &outcome.script {
                ScriptOutcome::Failed(error) => Some(error.as_ref()),
                _ => None,
            })
    }
    pub fn value(&self) -> Option<&Value> {
        match &self.outcome.as_ref()?.script {
            ScriptOutcome::Returned(value) => value.as_ref(),
            _ => None,
        }
    }
}

#[derive(serde::Serialize)]
pub(crate) struct WorkflowReport {
    run_id: WorkflowRunId,
    #[serde(skip)]
    creator: crate::agent_instance::AgentInstanceId,
    #[serde(skip)]
    registration: crate::agent_manager::AgentRegistrationId,
    name: String,
    status: WorkflowStatus,
    value: Option<Value>,
    error: Option<String>,
    cleanup_errors: Vec<String>,
}

impl WorkflowReport {
    pub(crate) fn id(&self) -> WorkflowRunId {
        self.run_id
    }
    pub(crate) fn creator(&self) -> &crate::agent_instance::AgentInstanceId {
        &self.creator
    }
    pub(crate) fn registration(&self) -> crate::agent_manager::AgentRegistrationId {
        self.registration
    }
    pub(crate) fn stopped(&self) -> bool {
        self.status == WorkflowStatus::Stopped
    }
    pub(crate) fn payload(&self) -> String {
        serde_json::to_string(self).expect("serializable Workflow report")
    }
}

#[derive(Clone)]
pub struct WorkflowRun {
    id: WorkflowRunId,
    request: Arc<super::script::WorkflowRequest>,
    commands: mpsc::Sender<WorkflowCommand>,
    observations: watch::Receiver<WorkflowSnapshot>,
}

impl WorkflowRun {
    pub fn id(&self) -> WorkflowRunId {
        self.id
    }
    /// Immutable launch input retained independently of the launch waiter and script driver.
    pub fn request(&self) -> &super::script::WorkflowRequest {
        &self.request
    }
    pub fn snapshot(&self) -> WorkflowSnapshot {
        self.observations.borrow().clone()
    }
    pub fn subscribe(&self) -> WorkflowSubscription {
        WorkflowSubscription {
            observations: self.observations.clone(),
        }
    }
    /// Admit one terminal report to the captured creator; retries do not duplicate feedback.
    /// Admission can queue behind active work and is distinct from model execution or persistence.
    pub async fn deliver_to(
        &self,
        creator: &crate::agent_instance::AgentInstance,
    ) -> WorkflowResult<()> {
        let snapshot = self.snapshot();
        if snapshot.outcome.is_none() {
            return Err(Arc::new(WorkflowError::InvalidRequest(
                "terminal feedback requires a settled run".into(),
            )));
        }
        let report = WorkflowReport {
            run_id: self.id,
            creator: snapshot.creator.clone(),
            registration: snapshot.registration,
            name: self.request.name().to_owned(),
            status: snapshot.status,
            value: snapshot.value().cloned(),
            error: snapshot.error().map(ToString::to_string),
            cleanup_errors: snapshot.cleanup_errors.clone(),
        };
        creator
            .deliver_workflow_report(report)
            .await
            .map_err(|error| Arc::new(WorkflowError::Owner(error)))
    }

    /// Stop admission and settle all accepted workers; cleanup failure remains retryable.
    pub async fn stop(&self) -> WorkflowResult<WorkflowSnapshot> {
        let snapshot = self.snapshot();
        if snapshot.outcome.is_some() {
            return Ok(snapshot);
        }
        let (reply, response) = oneshot::channel();
        if self
            .commands
            .send(WorkflowCommand::Stop(reply))
            .await
            .is_ok()
            && let Ok(result) = response.await
        {
            return result;
        }
        let snapshot = self.snapshot();
        if snapshot.outcome.is_some() {
            Ok(snapshot)
        } else {
            Err(Arc::new(WorkflowError::Coordination))
        }
    }
}

pub struct WorkflowSubscription {
    observations: watch::Receiver<WorkflowSnapshot>,
}

impl WorkflowSubscription {
    pub fn snapshot(&self) -> WorkflowSnapshot {
        self.observations.borrow().clone()
    }
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.observations.changed().await
    }
}

#[derive(Clone)]
pub(super) struct WorkflowScriptHost {
    commands: mpsc::Sender<WorkflowCommand>,
    cancellation: CancellationToken,
}

impl WorkflowScriptHost {
    pub(super) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(super) async fn observe(&self, observation: WorkflowObservation) -> WorkflowResult<()> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(WorkflowCommand::Observe { observation, reply })
            .await
            .map_err(|_| Arc::new(WorkflowError::Stopped))?;
        response
            .await
            .map_err(|_| Arc::new(WorkflowError::Coordination))?
    }

    pub(super) async fn request_agent(
        &self,
        prompt: String,
        options: WorkflowAgentOptions,
        phase: Option<String>,
    ) -> WorkflowResult<String> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(WorkflowCommand::Agent {
                prompt,
                options,
                phase,
                reply,
            })
            .await
            .map_err(|_| Arc::new(WorkflowError::Stopped))?;
        response
            .await
            .map_err(|_| Arc::new(WorkflowError::Coordination))?
    }
}

#[derive(Default)]
pub struct WorkflowRuntime {
    runs: Mutex<RunRegistry>,
}

#[derive(Default)]
struct RunRegistry {
    closed: bool,
    runs: HashMap<WorkflowRunId, WorkflowRun>,
}

impl WorkflowRuntime {
    // The script adapter owns the completion sender; losing it is a supervised script failure.
    pub(super) fn start_run(
        &self,
        scope: crate::harness::instance::execution::WorkflowScope,
        request: super::script::WorkflowRequest,
        id: WorkflowRunId,
    ) -> WorkflowResult<(
        WorkflowRun,
        WorkflowScriptHost,
        oneshot::Sender<ScriptOutcome>,
    )> {
        let mut registry = self
            .runs
            .lock()
            .map_err(|_| Arc::new(WorkflowError::Coordination))?;
        if registry.closed
            || !matches!(
                *scope.subscribe_closure().borrow(),
                crate::harness::manager::AgentFinalizationStatus::Open
            )
        {
            return Err(Arc::new(WorkflowError::Stopped));
        }
        if registry.runs.len() >= MAX_RETAINED_RUNS {
            registry
                .runs
                .retain(|_, run| run.snapshot().outcome.is_none());
        }
        if registry.runs.len() >= MAX_RETAINED_RUNS {
            return Err(Arc::new(WorkflowError::PendingLimit));
        }
        let limits = scope.limits();
        let (commands, receiver) = mpsc::channel(MAX_CONTROL_WAITERS);
        let (finished, script_result) = oneshot::channel();
        let cancellation = CancellationToken::new();
        let (worker_cancellation, _) = watch::channel(false);
        let state = WorkflowRunState {
            id,
            scope: Arc::new(scope),
            state: RunState::Running,
            invocations: Vec::new(),
            pending: VecDeque::new(),
            active: FuturesUnordered::new(),
            worker_cancellation,
            script_cancellation: cancellation.clone(),
            waiters: Vec::new(),
            result_bytes: 0,
            output_bytes: 0,
            observation_count: 0,
            current_phase: None,
            logs: Vec::new(),
            invocation_bytes: 0,
            cleanup_errors: Vec::new(),
            failure: None,
        };
        let (observations, snapshot) = watch::channel(state.snapshot());
        let run = WorkflowRun {
            id,
            request: Arc::new(request),
            commands: commands.clone(),
            observations: snapshot,
        };
        registry.runs.insert(id, run.clone());
        let (progress, progress_updates) = mpsc::channel(limits.max_concurrent_workers);
        tokio::spawn(state.supervise(
            receiver,
            script_result,
            observations,
            progress,
            progress_updates,
        ));
        Ok((
            run,
            WorkflowScriptHost {
                commands,
                cancellation,
            },
            finished,
        ))
    }

    /// Retained observation/control handles, scoped to the creator registration.
    /// Settled entries may be evicted; discovery does not acknowledge terminal delivery.
    pub fn runs_owned(
        &self,
        registration: crate::agent_manager::AgentRegistrationId,
    ) -> WorkflowResult<Vec<WorkflowRun>> {
        let runs = self
            .runs
            .lock()
            .map_err(|_| Arc::new(WorkflowError::Coordination))?
            .runs
            .values()
            .filter(|run| run.snapshot().registration == registration)
            .cloned()
            .collect();
        Ok(runs)
    }

    /// Stop retained runs for this registration; this does not revoke future launches.
    pub async fn stop_owned(
        &self,
        registration: crate::agent_manager::AgentRegistrationId,
    ) -> WorkflowResult<()> {
        stop_runs(self.runs_owned(registration)?).await
    }

    /// Close launch admission and stop every run before finalizing creator Sessions.
    pub async fn shutdown(&self) -> WorkflowResult<()> {
        let runs = {
            let mut registry = self
                .runs
                .lock()
                .map_err(|_| Arc::new(WorkflowError::Coordination))?;
            registry.closed = true;
            registry.runs.values().cloned().collect::<Vec<_>>()
        };
        stop_runs(runs).await
    }
}

async fn stop_runs(runs: Vec<WorkflowRun>) -> WorkflowResult<()> {
    tokio::spawn(async move {
        let mut failure = None;
        for result in futures::future::join_all(runs.iter().map(WorkflowRun::stop)).await {
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    })
    .await
    .map_err(|_| Arc::new(WorkflowError::Coordination))?
}

pub(super) enum WorkflowObservation {
    Phase(String),
    Log(String),
}

enum WorkflowCommand {
    Observe {
        observation: WorkflowObservation,
        reply: oneshot::Sender<WorkflowResult<()>>,
    },
    Agent {
        prompt: String,
        options: WorkflowAgentOptions,
        phase: Option<String>,
        reply: oneshot::Sender<WorkflowResult<String>>,
    },
    Stop(oneshot::Sender<WorkflowResult<WorkflowSnapshot>>),
}

enum RunState {
    Running,
    Finishing(Option<ScriptOutcome>),
    Stopping(Option<ScriptOutcome>),
    Settled(WorkflowOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvocationState {
    Queued,
    Preparing,
    Running,
    Finalizing,
    Succeeded,
    Failed,
    Cancelled,
}

impl InvocationState {
    fn status(self) -> WorkflowInvocationStatus {
        match self {
            Self::Queued => WorkflowInvocationStatus::Queued,
            Self::Preparing => WorkflowInvocationStatus::Preparing,
            Self::Running => WorkflowInvocationStatus::Running,
            Self::Finalizing => WorkflowInvocationStatus::Finalizing,
            Self::Succeeded => WorkflowInvocationStatus::Succeeded,
            Self::Failed => WorkflowInvocationStatus::Failed,
            Self::Cancelled => WorkflowInvocationStatus::Cancelled,
        }
    }
}

struct InvocationRecord {
    invocation: AgentInvocation,
    state: InvocationState,
    worker: Option<WorkflowWorkerSnapshot>,
    reply: Option<oneshot::Sender<WorkflowResult<String>>>,
    outcome: Option<WorkflowResult<String>>,
    cleanup: Option<WorkflowAdmission>,
}

struct WorkerFinished {
    id: InvocationId,
    worker: Option<WorkflowWorkerSnapshot>,
    outcome: WorkflowResult<String>,
    cleanup: Option<WorkflowAdmission>,
    error: Option<Arc<WorkflowError>>,
}

struct WorkerProgress {
    id: InvocationId,
    state: InvocationState,
    worker: WorkflowWorkerSnapshot,
}

struct WorkflowRunState {
    id: WorkflowRunId,
    scope: Arc<crate::harness::instance::execution::WorkflowScope>,
    state: RunState,
    invocations: Vec<InvocationRecord>,
    pending: VecDeque<InvocationId>,
    active: FuturesUnordered<BoxFuture<'static, WorkerFinished>>,
    worker_cancellation: watch::Sender<bool>,
    script_cancellation: CancellationToken,
    waiters: Vec<oneshot::Sender<WorkflowResult<WorkflowSnapshot>>>,
    result_bytes: usize,
    output_bytes: usize,
    observation_count: usize,
    current_phase: Option<String>,
    logs: Vec<String>,
    invocation_bytes: usize,
    cleanup_errors: Vec<String>,
    failure: Option<Arc<WorkflowError>>,
}

impl WorkflowRunState {
    fn snapshot(&self) -> WorkflowSnapshot {
        let status = match &self.state {
            RunState::Running => WorkflowStatus::Running,
            RunState::Finishing(_) => WorkflowStatus::Finishing,
            RunState::Stopping(_) => WorkflowStatus::Stopping,
            RunState::Settled(outcome) => outcome.status,
        };
        WorkflowSnapshot {
            id: self.id,
            creator: self.scope.creator().clone(),
            registration: self.scope.registration(),
            status,
            active: self
                .invocations
                .iter()
                .filter(|record| {
                    matches!(
                        record.state,
                        InvocationState::Preparing
                            | InvocationState::Running
                            | InvocationState::Finalizing
                    )
                })
                .count(),
            pending: self.pending.len(),
            completed: self
                .invocations
                .iter()
                .filter(|record| {
                    matches!(
                        record.state,
                        InvocationState::Succeeded
                            | InvocationState::Failed
                            | InvocationState::Cancelled
                    )
                })
                .count(),
            invocations: self
                .invocations
                .iter()
                .map(|record| WorkflowInvocationSnapshot {
                    invocation: record.invocation.clone(),
                    status: record.state.status(),
                    worker: record.worker.clone(),
                    outcome: record.outcome.clone(),
                })
                .collect(),
            cleanup_errors: self.cleanup_errors.clone(),
            phase: self.current_phase.clone(),
            logs: self.logs.clone(),
            outcome: match &self.state {
                RunState::Settled(outcome) => Some(outcome.clone()),
                _ => None,
            },
        }
    }

    fn observe(&mut self, observation: WorkflowObservation) -> WorkflowResult<()> {
        if !matches!(self.state, RunState::Running) {
            return Err(Arc::new(WorkflowError::Stopped));
        }
        let text = match &observation {
            WorkflowObservation::Phase(text) | WorkflowObservation::Log(text) => text,
        };
        if self.observation_count >= MAX_OBSERVATIONS
            || text.len()
                > self
                    .scope
                    .limits()
                    .max_output_bytes
                    .saturating_sub(self.output_bytes)
        {
            let error = Arc::new(WorkflowError::OutputLimit);
            self.failure.get_or_insert(error.clone());
            self.state = RunState::Finishing(None);
            self.cancel_work();
            return Err(error);
        }
        self.output_bytes += text.len();
        self.observation_count += 1;
        match observation {
            WorkflowObservation::Phase(title) => self.current_phase = Some(title),
            WorkflowObservation::Log(text) => self.logs.push(text),
        }
        Ok(())
    }

    fn cancel_work(&mut self) {
        self.script_cancellation.cancel();
        self.worker_cancellation.send_replace(true);
        for id in self.pending.drain(..) {
            let record = &mut self.invocations[id.0];
            let error = Arc::new(WorkflowError::Stopped);
            record.state = InvocationState::Cancelled;
            record.outcome = Some(Err(error.clone()));
            if let Some(reply) = record.reply.take() {
                let _ = reply.send(Err(error));
            }
        }
    }

    fn request_stop(&mut self) {
        self.state = match std::mem::replace(&mut self.state, RunState::Stopping(None)) {
            RunState::Finishing(outcome) | RunState::Stopping(outcome) => {
                RunState::Stopping(outcome)
            }
            state @ RunState::Settled(_) => state,
            RunState::Running => RunState::Stopping(None),
        };
        self.cancel_work();
        for record in &mut self.invocations {
            if let Some(admission) = record.cleanup.take() {
                let id = record.invocation.id;
                let outcome = record
                    .outcome
                    .clone()
                    .expect("cleanup retains the execution outcome");
                self.active.push(Box::pin(async move {
                    finalize_invocation(id, outcome, admission).await
                }));
            }
        }
    }

    fn script_finished(&mut self, mut outcome: ScriptOutcome) {
        if let ScriptOutcome::Returned(Some(value)) = &outcome {
            let bytes = value.to_string().len();
            if bytes
                > self
                    .scope
                    .limits()
                    .max_result_bytes
                    .saturating_sub(self.result_bytes)
            {
                let error = Arc::new(WorkflowError::ResultLimit);
                self.failure.get_or_insert(error.clone());
                outcome = ScriptOutcome::Failed(error);
            } else {
                self.result_bytes += bytes;
            }
        }
        if let ScriptOutcome::Failed(error) = &mut outcome
            && let WorkflowError::Script(diagnostic) = error.as_ref()
        {
            let remaining = self
                .scope
                .limits()
                .max_output_bytes
                .saturating_sub(self.output_bytes);
            let (diagnostic, bytes) = bound_script_diagnostic(
                diagnostic,
                remaining.saturating_sub(SCRIPT_ERROR_PREFIX.len()),
            );
            self.output_bytes += remaining.min(SCRIPT_ERROR_PREFIX.len() + bytes);
            *error = Arc::new(WorkflowError::Script(diagnostic));
        }
        self.state = match self.state {
            RunState::Stopping(_) => RunState::Stopping(Some(outcome)),
            _ => RunState::Finishing(Some(outcome)),
        };
        self.cancel_work();
    }

    fn request_agent(
        &mut self,
        prompt: String,
        options: WorkflowAgentOptions,
        phase: Option<String>,
        reply: oneshot::Sender<WorkflowResult<String>>,
    ) {
        if reply.is_closed() {
            return;
        }
        let limits = self.scope.limits();
        let result = if !matches!(self.state, RunState::Running) {
            Err(WorkflowError::Stopped)
        } else if self.invocations.len() >= limits.max_total_invocations {
            Err(WorkflowError::InvocationLimit)
        } else if self.pending.len() >= limits.max_pending_invocations {
            Err(WorkflowError::PendingLimit)
        } else {
            self.scope.resolve_invocation(
                InvocationId(self.invocations.len()),
                prompt,
                options,
                phase,
            )
        };
        match result {
            Ok(invocation) => {
                let bytes = invocation.retained_bytes();
                if bytes > MAX_WORKFLOW_BYTES.saturating_sub(self.invocation_bytes) {
                    let _ = reply.send(Err(Arc::new(WorkflowError::InvocationBytesLimit)));
                    return;
                }
                self.invocation_bytes += bytes;
                self.pending.push_back(invocation.id);
                self.invocations.push(InvocationRecord {
                    invocation,
                    state: InvocationState::Queued,
                    worker: None,
                    reply: Some(reply),
                    outcome: None,
                    cleanup: None,
                });
            }
            Err(error) => {
                let _ = reply.send(Err(Arc::new(error)));
            }
        }
    }

    fn admit_pending(&mut self, progress: &mpsc::Sender<WorkerProgress>) {
        if !matches!(self.state, RunState::Running) {
            return;
        }
        while self.active.len() < self.scope.limits().max_concurrent_workers {
            let Some(id) = self.pending.pop_front() else {
                break;
            };
            let record = &mut self.invocations[id.0];
            record.state = InvocationState::Preparing;
            self.active.push(Box::pin(run_invocation(
                self.id,
                self.scope.clone(),
                record.invocation.clone(),
                self.worker_cancellation.subscribe(),
                progress.clone(),
            )));
        }
    }

    fn worker_finished(&mut self, mut finished: WorkerFinished) {
        if self.invocations[finished.id.0].outcome.is_none()
            && let Ok(text) = &finished.outcome
        {
            let bytes = text.len();
            if bytes
                > self
                    .scope
                    .limits()
                    .max_result_bytes
                    .saturating_sub(self.result_bytes)
            {
                let error = Arc::new(WorkflowError::ResultLimit);
                finished.outcome = Err(error.clone());
                self.failure.get_or_insert(error);
                if matches!(self.state, RunState::Running) {
                    self.state = RunState::Finishing(None);
                }
                self.cancel_work();
            } else {
                self.result_bytes += bytes;
            }
        }
        let record = &mut self.invocations[finished.id.0];
        if let Some(worker) = finished.worker {
            record.worker = Some(worker);
        }
        if let Some(error) = finished.error {
            self.failure.get_or_insert(error.clone());
            if self.cleanup_errors.len() < MAX_CONTROL_WAITERS {
                let remaining = self
                    .scope
                    .limits()
                    .max_output_bytes
                    .saturating_sub(self.output_bytes);
                let mut diagnostic = error.to_string();
                let mut end = diagnostic.len().min(remaining);
                while !diagnostic.is_char_boundary(end) {
                    end -= 1;
                }
                diagnostic.truncate(end);
                self.output_bytes += diagnostic.len();
                self.cleanup_errors.push(diagnostic);
            }
            record.cleanup = finished.cleanup;
            record.state = InvocationState::Finalizing;
            record.outcome = Some(finished.outcome);
            if let Some(reply) = record.reply.take() {
                let _ = reply.send(Err(error.clone()));
            }
            for waiter in self.waiters.drain(..) {
                let _ = waiter.send(Err(error.clone()));
            }
            if matches!(self.state, RunState::Running) {
                self.state = RunState::Finishing(None);
            }
            self.cancel_work();
        } else {
            record.state = match &finished.outcome {
                Ok(_) => InvocationState::Succeeded,
                Err(error)
                    if matches!(error.as_ref(), WorkflowError::Stopped)
                        || matches!(error.as_ref(), WorkflowError::Worker(failure) if failure.kind == crate::PhiFailureKind::Cancelled) =>
                {
                    InvocationState::Cancelled
                }
                Err(_) => InvocationState::Failed,
            };
            record.outcome = Some(finished.outcome.clone());
            if let Some(reply) = record.reply.take() {
                let _ = reply.send(finished.outcome);
            }
        }
    }

    fn finish_run(&mut self) -> bool {
        if !self.active.is_empty()
            || self
                .invocations
                .iter()
                .any(|record| record.cleanup.is_some())
        {
            return false;
        }
        let state = std::mem::replace(&mut self.state, RunState::Finishing(None));
        let (script, stopped) = match state {
            RunState::Finishing(Some(script)) => (script, false),
            RunState::Stopping(Some(script)) => (script, true),
            state => {
                self.state = state;
                return false;
            }
        };
        let status = if stopped {
            WorkflowStatus::Stopped
        } else if self.failure.is_some()
            || matches!(script, ScriptOutcome::Failed(_) | ScriptOutcome::Cancelled)
        {
            WorkflowStatus::Failed
        } else {
            WorkflowStatus::Completed
        };
        self.state = RunState::Settled(WorkflowOutcome {
            status,
            script,
            failure: self.failure.clone(),
        });
        true
    }

    async fn supervise(
        mut self,
        mut commands: mpsc::Receiver<WorkflowCommand>,
        mut script_result: oneshot::Receiver<ScriptOutcome>,
        observations: watch::Sender<WorkflowSnapshot>,
        progress: mpsc::Sender<WorkerProgress>,
        mut progress_updates: mpsc::Receiver<WorkerProgress>,
    ) {
        let mut closure = self.scope.subscribe_closure();
        let mut commands_open = true;
        loop {
            if !matches!(
                *closure.borrow(),
                crate::harness::manager::AgentFinalizationStatus::Open
            ) && matches!(self.state, RunState::Running | RunState::Finishing(_))
            {
                self.request_stop();
            }
            self.admit_pending(&progress);
            if self.finish_run() {
                let snapshot = self.snapshot();
                observations.send_replace(snapshot.clone());
                for waiter in self.waiters.drain(..) {
                    let _ = waiter.send(Ok(snapshot.clone()));
                }
                break;
            }
            observations.send_replace(self.snapshot());
            let owner_open = matches!(
                *closure.borrow(),
                crate::harness::manager::AgentFinalizationStatus::Open
            );
            tokio::select! {
                command = commands.recv(), if commands_open => match command {
                    Some(WorkflowCommand::Agent { prompt, options, phase, reply }) => self.request_agent(prompt, options, phase, reply),
                    Some(WorkflowCommand::Observe { observation, reply }) => { let _ = reply.send(self.observe(observation)); },
                    Some(WorkflowCommand::Stop(reply)) => {
                        self.waiters.retain(|waiter| !waiter.is_closed());
                        if self.waiters.len() >= MAX_CONTROL_WAITERS {
                            let _ = reply.send(Err(Arc::new(WorkflowError::PendingLimit)));
                        } else {
                            self.waiters.push(reply);
                            self.request_stop();
                        }
                    }
                    None => { commands_open = false; self.request_stop(); },
                },
                result = &mut script_result, if matches!(self.state, RunState::Running | RunState::Finishing(None) | RunState::Stopping(None)) => {
                    let outcome = result.unwrap_or_else(|_| {
                        if matches!(self.state, RunState::Stopping(_)) { ScriptOutcome::Cancelled }
                        else { ScriptOutcome::Failed(Arc::new(WorkflowError::Coordination)) }
                    });
                    self.script_finished(outcome);
                },
                finished = self.active.next(), if !self.active.is_empty() => {
                    if let Some(finished) = finished { self.worker_finished(finished); }
                },
                Some(update) = progress_updates.recv() => {
                    let record = &mut self.invocations[update.id.0];
                    if record.outcome.is_none() {
                        record.state = update.state;
                        record.worker = Some(update.worker);
                    }
                },
                _ = closure.changed(), if owner_open => {},
            }
        }
    }
}

async fn finalize_invocation(
    id: InvocationId,
    outcome: WorkflowResult<String>,
    mut admission: WorkflowAdmission,
) -> WorkerFinished {
    let worker = Some(admission.worker());
    let finalized = std::panic::AssertUnwindSafe(admission.finalize_worker())
        .catch_unwind()
        .await
        .unwrap_or(Err(WorkflowError::Coordination));
    let outcome = admission
        .completion
        .get()
        .map(|completion| {
            completion
                .outcome
                .map_err(|failure| Arc::new(WorkflowError::Worker(failure)))
        })
        .unwrap_or(outcome);
    match finalized {
        Ok(()) => WorkerFinished {
            id,
            worker,
            outcome,
            cleanup: None,
            error: None,
        },
        Err(error) => WorkerFinished {
            id,
            worker,
            outcome,
            cleanup: Some(admission),
            error: Some(Arc::new(error)),
        },
    }
}

async fn run_invocation(
    run: WorkflowRunId,
    scope: Arc<crate::harness::instance::execution::WorkflowScope>,
    invocation: AgentInvocation,
    mut cancellation: watch::Receiver<bool>,
    progress: mpsc::Sender<WorkerProgress>,
) -> WorkerFinished {
    let id = invocation.id;
    let reserved = std::panic::AssertUnwindSafe(async {
        tokio::select! {
            biased;
            _ = crate::harness::manager::preparation::wait_for_cancellation(&mut cancellation) => Err(WorkflowError::Stopped),
            reserved = scope.reserve_agent(run, &invocation) => reserved,
        }
    }).catch_unwind().await.unwrap_or(Err(WorkflowError::Coordination));
    let mut admission = match reserved {
        Ok(admission) => admission,
        Err(error) => {
            return WorkerFinished {
                id,
                worker: None,
                outcome: Err(Arc::new(error)),
                cleanup: None,
                error: None,
            };
        }
    };
    let _ = progress
        .send(WorkerProgress {
            id,
            state: InvocationState::Preparing,
            worker: admission.worker(),
        })
        .await;
    let outcome = std::panic::AssertUnwindSafe(async {
        scope.admit_agent(&mut admission, cancellation.clone()).await.map_err(|error| {
            if *cancellation.borrow()
                && matches!(error, WorkflowError::Admission(AgentManagerError::AdmissionInterrupted)) {
                Arc::new(WorkflowError::Stopped)
            } else { Arc::new(error) }
        })?;
        let _ = progress.send(WorkerProgress { id, state: InvocationState::Running, worker: admission.worker() }).await;
        tokio::select! {
            biased;
            _ = crate::harness::manager::preparation::wait_for_cancellation(&mut cancellation) => Err(Arc::new(WorkflowError::Stopped)),
            result = admission.await_completion() => result.map_err(|failure| Arc::new(WorkflowError::Worker(failure))),
        }
    }).catch_unwind().await.unwrap_or_else(|_| Err(Arc::new(WorkflowError::Coordination)));
    if let Err(error) = &outcome
        && matches!(
            error.as_ref(),
            WorkflowError::Admission(AgentManagerError::Compensation { .. })
        )
    {
        let error = error.clone();
        return WorkerFinished {
            id,
            worker: Some(admission.worker()),
            outcome,
            cleanup: Some(admission),
            error: Some(error),
        };
    }
    let _ = progress
        .send(WorkerProgress {
            id,
            state: InvocationState::Finalizing,
            worker: admission.worker(),
        })
        .await;
    finalize_invocation(id, outcome, admission).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_reject_zero_and_inconsistent_capacity() {
        for empty in 0..5 {
            let mut limits = [1; 5];
            limits[empty] = 0;
            assert!(
                WorkflowLimits::new(limits[0], limits[1], limits[2], limits[3], limits[4]).is_err()
            );
        }
        assert!(WorkflowLimits::new(1, 1, usize::MAX, 1, 1).is_err());
        assert!(WorkflowLimits::new(1, 1, 1, usize::MAX, 1).is_err());
        assert!(WorkflowLimits::new(1, 1, 1, 1, usize::MAX).is_err());
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            failure_persistence: None,
            ..Default::default()
        })]
        #[test]
        fn script_diagnostics_stay_bounded(
            message in ".{0,128}",
            name in proptest::option::of(".{0,64}"),
            stack in proptest::option::of(".{0,256}"),
            budget in 0usize..1024,
        ) {
            let original = phi_codemode::CodemodeError {
                kind: phi_codemode::ErrorKind::Script,
                name,
                message,
                stack,
            };
            let original_bytes = original.message.len()
                + original.name.as_ref().map_or(0, String::len)
                + original.stack.as_ref().map_or(0, String::len);
            let (bounded, bytes) = bound_script_diagnostic(&original, budget);
            proptest::prop_assert!(bytes <= budget);
            proptest::prop_assert_eq!(bytes, bounded.message.len()
                + bounded.name.as_ref().map_or(0, String::len)
                + bounded.stack.as_ref().map_or(0, String::len));
            proptest::prop_assert_eq!(bounded.kind, original.kind);
            if original_bytes <= budget {
                proptest::prop_assert_eq!(bounded, original);
            } else if budget >= DIAGNOSTIC_TRUNCATION.len() {
                proptest::prop_assert!(bounded.message.ends_with(DIAGNOSTIC_TRUNCATION));
            }
        }

        #[test]
        fn generated_limits_respect_capacity_bounds(
            concurrent in 0usize..1500,
            pending in 0usize..1500,
            total in 0usize..1500,
        ) {
            proptest::prop_assert_eq!(
                WorkflowLimits::new(concurrent, pending, total, 1, 1).is_ok(),
                concurrent > 0 && pending > 0 && total > 0
                    && concurrent <= total && pending <= total && total <= MAX_WORKFLOW_INVOCATIONS,
            );
        }
    }

    #[test]
    fn role_resolution_never_selects_an_arbitrary_definition() {
        let definitions = AgentCatalog::new(vec![crate::agent::custom::AgentDefinition::new(
            "reviewer",
            "Review",
            "Review the supplied work.",
        )]);
        let resolve = |options, default_role| {
            AgentInvocation::resolve(
                InvocationId(0),
                "inspect".into(),
                options,
                Some("review".into()),
                &definitions,
                default_role,
            )
        };
        assert!(matches!(
            resolve(WorkflowAgentOptions::default(), None),
            Err(WorkflowError::MissingRole)
        ));
        assert!(matches!(
            resolve(WorkflowAgentOptions::default(), Some("absent")),
            Err(WorkflowError::UnknownRole(_))
        ));
        let invocation = resolve(WorkflowAgentOptions::default(), Some("reviewer")).unwrap();
        assert_eq!(invocation.id, InvocationId(0));
        assert_eq!(invocation.role, "reviewer");
        assert_eq!(invocation.phase.as_deref(), Some("review"));
        for field in [
            "agentType",
            "model",
            "schema",
            "isolation",
            "stallMs",
            "alias",
            "mode",
        ] {
            assert!(
                serde_json::from_value::<WorkflowAgentOptions>(serde_json::json!({field: "value"}))
                    .is_err()
            );
        }
        let options = serde_json::from_value(
            serde_json::json!({"role": "reviewer", "label": "not-an-alias"}),
        )
        .unwrap();
        let invocation = resolve(options, None).unwrap();
        assert_eq!(invocation.label.as_deref(), Some("not-an-alias"));
        assert!(invocation.request().alias.is_none());
        assert!(
            AgentInvocation::resolve(
                InvocationId(1),
                " \n ".into(),
                WorkflowAgentOptions::default(),
                None,
                &definitions,
                Some("reviewer")
            )
            .is_err()
        );
    }
}
