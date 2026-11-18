//! Scoped Agent operations and owned observations, independent of client workflows.

use std::sync::Arc;

use futures_util::FutureExt;
use phi_sdk::agent_instance::{
    AgentInstanceError, AgentInstanceEvent, AgentInstanceId, AgentInstanceSnapshot,
    AgentSnapshotSubscription, BashExecution, BashMode, BashStatus,
};
use phi_sdk::agent_manager::DelegationMode;
use phi_sdk::interaction::{InteractionEnvelope, InteractionResponse};
#[cfg(test)]
use phi_sdk::lifecycle::SessionEndReason;
use phi_sdk::transcript::Transcript;
use phi_sdk::turn::{DirectedDelegation, SubmissionDispatch, TurnPrompt};
use tokio::sync::broadcast;

use crate::resources::AgentDefinitionView;
#[cfg(test)]
use crate::resources::prompts::PromptExpandResult;
use crate::runtime::save::SaveObservation;
use crate::runtime::session::SessionInteractionAnswer as InteractionAnswer;
use crate::runtime::session::SessionSubmission;
use crate::runtime::{RuntimePreparation, SessionAccess};
use crate::session::Session;

use crate::runtime::preparation::McpServerToolSelection;
use crate::runtime::session::AgentSession;
use crate::runtime::{McpReconnectResult, McpServerView};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SaveNotice {
    Warning(String),
    Recovered,
}

#[derive(Default)]
pub(crate) struct SaveObservationCursor {
    association_id: Option<String>,
    failure: Option<String>,
    completed_attempt: u64,
    read_error: Option<String>,
}

impl SaveObservationCursor {
    pub(crate) fn observe(
        &mut self,
        result: anyhow::Result<SaveObservation>,
    ) -> Option<SaveNotice> {
        match result {
            Ok(observation) => {
                self.read_error = None;
                self.observe_completion(observation)
            }
            Err(error) => {
                self.unavailable(format!("Failed to observe session save status: {error}"))
            }
        }
    }

    fn unavailable(&mut self, message: String) -> Option<SaveNotice> {
        if self.read_error.as_ref() == Some(&message) {
            return None;
        }
        self.read_error = Some(message.clone());
        Some(SaveNotice::Warning(message))
    }

    fn observe_completion(&mut self, observation: SaveObservation) -> Option<SaveNotice> {
        if self.association_id.as_ref() != Some(&observation.association_id) {
            self.association_id = Some(observation.association_id);
            self.failure = None;
            self.completed_attempt = 0;
        }
        let completed = observation.finalization.or(observation.completed)?;
        if completed.attempt < self.completed_attempt {
            return None;
        }
        self.completed_attempt = completed.attempt;
        if let Some(error) = completed.error {
            if self.failure.as_ref() == Some(&error) {
                return None;
            }
            self.failure = Some(error.clone());
            return Some(SaveNotice::Warning(format!(
                "Failed to save session: {error}"
            )));
        }
        if observation.pending.is_none() && self.failure.take().is_some() {
            return Some(SaveNotice::Recovered);
        }
        None
    }
}

#[derive(Debug, Clone)]
pub(crate) enum AgentEvent {
    Phi(phi_sdk::PhiEventEnvelope),
    RunCancelled,
    InteractionRequested(InteractionEnvelope),
    InteractionClosed { request_id: String },
    PumpFailed,
    ObservationFailed { message: String },
    RuntimeReplaced,
    RecoveryBoundary { id: String },
    Other,
    Lagged(u64),
    Stopped,
}

pub(crate) struct AgentActivities {
    pub(crate) agents: Vec<crate::session::SubagentChildLink>,
    pub(crate) bash_executions: Vec<(AgentInstanceId, BashExecution)>,
}

pub(crate) struct AgentChanges {
    events: Option<broadcast::Receiver<AgentEvent>>,
    snapshots: Option<AgentSnapshotSubscription>,
    output: Option<phi_sdk::agent_instance::BashOutputSubscription>,
}

impl AgentChanges {
    pub(crate) fn changed(&mut self) -> bool {
        let mut changed = false;
        if let Some(events) = &mut self.events {
            loop {
                match events.try_recv() {
                    Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => changed = true,
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Closed) => {
                        self.events = None;
                        changed = true;
                        break;
                    }
                }
            }
        }
        if let Some(snapshots) = &mut self.snapshots {
            match snapshots.changed().now_or_never() {
                Some(Ok(_)) => changed = true,
                Some(Err(_)) => {
                    self.snapshots = None;
                    changed = true;
                }
                None => {}
            }
        }
        if let Some(output) = &mut self.output {
            match output.changed().now_or_never() {
                Some(Ok(())) => changed = true,
                Some(Err(_)) => self.output = None,
                None => {}
            }
        }
        changed
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AgentCatalog {
    agents: Vec<AgentDefinitionView>,
}

impl AgentCatalog {
    pub(crate) fn new(agents: Vec<AgentDefinitionView>) -> Self {
        Self { agents }
    }

    pub(crate) fn all(&self) -> &[AgentDefinitionView] {
        &self.agents
    }

    pub(crate) fn get(&self, name: &str) -> Option<&AgentDefinitionView> {
        self.agents.iter().find(|agent| agent.name == name)
    }
}

pub(crate) use crate::runtime::session::AgentCapabilities;

#[derive(Clone)]
pub(crate) struct AgentAttachment {
    backend: AttachmentBackend,
}

#[derive(Clone)]
enum AttachmentBackend {
    Embedded(LocalAttachment),
    Gateway(super::gateway::GatewayAttachment),
}

#[derive(Clone)]
struct LocalAttachment {
    session: SessionAccess,
    preparation: Arc<RuntimePreparation>,
    snapshots: AgentSnapshotSubscription,
    workspace: Arc<tokio::sync::Mutex<super::workspace::WorkspaceCache>>,
}

fn local_subscription(session: &SessionAccess) -> broadcast::Receiver<AgentEvent> {
    let (events, receiver) = broadcast::channel(256);
    let Ok(binding) = session.binding() else {
        return receiver;
    };
    let mut source = binding.instance().subscribe();
    tokio::spawn(async move {
        loop {
            let received = tokio::select! {
                _ = events.closed() => break,
                event = source.recv() => event,
            };
            let event = match received {
                Ok(event) => project_event(event),
                Err(broadcast::error::RecvError::Lagged(count)) => AgentEvent::Lagged(count),
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if events.send(event).is_err() {
                break;
            }
        }
    });
    receiver
}

fn project_event(event: AgentInstanceEvent) -> AgentEvent {
    match event {
        AgentInstanceEvent::Phi(event) => AgentEvent::Phi(event),
        AgentInstanceEvent::RunCancelled { .. } => AgentEvent::RunCancelled,
        AgentInstanceEvent::InteractionRequested(request) => {
            AgentEvent::InteractionRequested(request)
        }
        AgentInstanceEvent::InteractionClosed { request_id } => {
            AgentEvent::InteractionClosed { request_id }
        }
        AgentInstanceEvent::PumpFailed { .. } => AgentEvent::PumpFailed,
        AgentInstanceEvent::RuntimeReplaced => AgentEvent::RuntimeReplaced,
        AgentInstanceEvent::ViewBoundary { id } => AgentEvent::RecoveryBoundary { id },
        AgentInstanceEvent::Stopped => AgentEvent::Stopped,
        _ => AgentEvent::Other,
    }
}

fn operation_error(source: anyhow::Error) -> AgentInstanceError {
    AgentInstanceError::Operation { source }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::save::{SaveCompletion, SaveOperation};

    fn failed() -> SaveObservation {
        let mut observation = SaveObservation {
            association_id: uuid::Uuid::new_v4().to_string(),
            persistent: true,
            attempt: 1,
            pending: None,
            completed: None,
            finalization: None,
        };
        observation.completed = Some(SaveCompletion {
            attempt: 1,
            operation: SaveOperation::Checkpoint,
            attempts: 2,
            saved_boundary: None,
            error: Some("disk unavailable".into()),
        });
        observation.attempt = 1;
        observation
    }

    #[test]
    fn late_failure_is_deduplicated_until_matching_recovery_or_new_association() {
        let mut cursor = SaveObservationCursor::default();
        let mut observation = failed();
        assert!(matches!(
            cursor.observe(Ok(observation.clone())),
            Some(SaveNotice::Warning(_))
        ));
        assert!(matches!(
            cursor.observe(Err(anyhow::anyhow!("disconnected"))),
            Some(SaveNotice::Warning(_))
        ));
        assert_eq!(cursor.observe(Err(anyhow::anyhow!("disconnected"))), None);
        assert_eq!(cursor.observe(Ok(observation.clone())), None);
        observation.pending = Some(SaveOperation::Checkpoint);
        observation.attempt += 1;
        assert_eq!(cursor.observe(Ok(observation.clone())), None);
        observation.pending = None;
        observation.completed.as_mut().unwrap().attempt = observation.attempt;
        observation.completed.as_mut().unwrap().error = None;
        assert_eq!(
            cursor.observe(Ok(observation.clone())),
            Some(SaveNotice::Recovered)
        );
        assert_eq!(cursor.observe(Ok(observation.clone())), None);
        let stale_success = observation.clone();
        observation.attempt += 1;
        observation.completed.as_mut().unwrap().attempt = observation.attempt;
        observation.completed.as_mut().unwrap().error = Some("disk unavailable".into());
        assert!(matches!(
            cursor.observe(Ok(observation.clone())),
            Some(SaveNotice::Warning(_))
        ));
        assert_eq!(cursor.observe(Ok(stale_success)), None);
        let replacement = failed();
        assert!(matches!(
            cursor.observe(Ok(replacement)),
            Some(SaveNotice::Warning(_))
        ));
    }

    #[test]
    fn checkpoint_success_does_not_clear_finalization_failure() {
        let mut observation = failed();
        let mut finalization = observation.completed.clone().unwrap();
        finalization.operation = SaveOperation::Finalization;
        observation.finalization = Some(finalization);
        let mut cursor = SaveObservationCursor::default();
        assert!(matches!(
            cursor.observe(Ok(observation.clone())),
            Some(SaveNotice::Warning(_))
        ));
        observation.completed.as_mut().unwrap().error = None;
        assert_eq!(cursor.observe(Ok(observation.clone())), None);
        observation.finalization.as_mut().unwrap().error = None;
        assert_eq!(cursor.observe(Ok(observation)), Some(SaveNotice::Recovered));
    }
}

async fn wait_event_boundary(
    events: &mut broadcast::Receiver<AgentEvent>,
    id: &str,
) -> anyhow::Result<()> {
    loop {
        if matches!(events.recv().await?, AgentEvent::RecoveryBoundary { id: boundary }
            if boundary == id)
        {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

    #[tokio::test]
    async fn direct_access_observes_without_owning_execution_or_retaining_membership() {
        use phi_sdk::agent_instance::AgentInstanceStatus;

        let (root, policy, test_runtime) =
            crate::runtime::preparation::tests::session_test_runtime();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let binding = Box::pin(
            {
                policy.install(&test_runtime.preparation);
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
            }
            .start_session(session, true),
        )
        .await
        .unwrap();
        let attachment = AgentAttachment::new(
            &binding,
            test_runtime
                .sessions
                .session_access(binding.instance().id()),
            test_runtime.preparation.clone(),
        );
        let mut events = attachment.subscribe();
        let recovery = attachment.recover_events(&mut events).await.unwrap();
        assert_eq!(recovery.session.id.as_str(), attachment.id().as_str());
        assert_eq!(
            recovery.declarations.generation_id,
            recovery.snapshot.runtime_generation_id,
        );
        drop(events);
        drop(attachment.subscribe_changes());
        drop(attachment);
        assert_eq!(
            binding.instance().snapshot().status,
            AgentInstanceStatus::Idle
        );
        assert!(test_runtime.sessions.get(binding.instance().id()).is_ok());
        let attachment = AgentAttachment::new(
            &binding,
            test_runtime
                .sessions
                .session_access(binding.instance().id()),
            test_runtime.preparation.clone(),
        );
        let stopped = test_runtime
            .sessions
            .stop(binding.instance().id(), SessionEndReason::Quit)
            .await
            .unwrap();
        assert_eq!(stopped.id.as_str(), attachment.id().as_str());
        assert!(attachment.recovery().await.is_err());
        assert!(attachment.transcript().await.is_err());
        assert!(attachment.abort().await.is_err());
        assert!(attachment.subscribe_mcp().is_err());
        assert!(attachment.pending_interactions().await.unwrap().is_empty());
        assert!(matches!(
            attachment.subscribe().try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
        assert_eq!(attachment.snapshot().status, AgentInstanceStatus::Stopped);
        drop(attachment);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn boundary_discards_only_covered_events_and_lag_requires_a_new_read() {
        let (source, mut events) = broadcast::channel(2);
        source.send(AgentEvent::Other).unwrap();
        source
            .send(AgentEvent::RecoveryBoundary {
                id: "covered".into(),
            })
            .unwrap();
        wait_event_boundary(&mut events, "covered").await.unwrap();
        source.send(AgentEvent::Other).unwrap();
        assert!(matches!(events.try_recv().unwrap(), AgentEvent::Other));

        source
            .send(AgentEvent::RecoveryBoundary { id: "lost".into() })
            .unwrap();
        source.send(AgentEvent::Other).unwrap();
        source.send(AgentEvent::Other).unwrap();
        assert!(wait_event_boundary(&mut events, "lost").await.is_err());
        events = events.resubscribe();
        source
            .send(AgentEvent::RecoveryBoundary { id: "fresh".into() })
            .unwrap();
        source.send(AgentEvent::Other).unwrap();
        wait_event_boundary(&mut events, "fresh").await.unwrap();
        assert!(matches!(events.try_recv().unwrap(), AgentEvent::Other));
    }
}

impl AgentAttachment {
    pub(crate) fn new(
        binding: &AgentSession,
        session: SessionAccess,
        preparation: Arc<RuntimePreparation>,
    ) -> Self {
        let instance = binding.instance();
        Self {
            backend: AttachmentBackend::Embedded(LocalAttachment {
                session,
                preparation,
                snapshots: instance.subscribe_snapshots(),
                workspace: Arc::default(),
            }),
        }
    }

    pub(crate) async fn connect_gateway(
        client: phi_remote::RemoteClient,
        id: &str,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            backend: AttachmentBackend::Gateway(
                super::gateway::GatewayAttachment::connect(client, id).await?,
            ),
        })
    }

    fn gateway(&self) -> Option<&super::gateway::GatewayAttachment> {
        match &self.backend {
            AttachmentBackend::Gateway(gateway) => Some(gateway),
            _ => None,
        }
    }

    fn local(&self) -> anyhow::Result<&LocalAttachment> {
        match &self.backend {
            AttachmentBackend::Embedded(local) => Ok(local),
            AttachmentBackend::Gateway(_) => {
                anyhow::bail!("this operation is available only in embedded local mode")
            }
        }
    }

    fn session_binding(&self) -> anyhow::Result<Arc<AgentSession>> {
        self.local()?.session.binding()
    }

    pub(crate) fn subscribe_mcp(
        &self,
    ) -> anyhow::Result<Option<broadcast::Receiver<phi_sdk::extension::mcp::McpRuntimeEvent>>> {
        Ok(self.session_binding()?.subscribe_mcp())
    }

    pub(crate) async fn cancel_mcp_connect(&self, server: &str) -> anyhow::Result<()> {
        self.session_binding()?.cancel_mcp_connect(server).await
    }

    pub(crate) async fn reconnect_mcp(&self, server: &str) -> anyhow::Result<McpReconnectResult> {
        let outcome = self.session_binding()?.reconnect_mcp(server).await?;
        Ok(match outcome {
            Ok(tool_count) => McpReconnectResult::Refreshed { tool_count },
            Err(seconds) => McpReconnectResult::RetryAfter { seconds },
        })
    }

    pub(crate) async fn save_mcp_tools(
        &self,
        servers: Vec<McpServerToolSelection>,
    ) -> anyhow::Result<()> {
        let local = self.local()?;
        local
            .preparation
            .save_mcp_tools(&local.session, servers)
            .await
    }

    pub(crate) async fn apply_switch_config(
        &self,
        switches: Option<Vec<crate::config::SwitchConfig>>,
    ) -> anyhow::Result<crate::runtime::RuntimeView> {
        let local = self.local()?;
        local
            .preparation
            .apply_switch_config(&local.session, switches)
            .await
    }

    pub(crate) async fn configure_runtime(
        &self,
        selection: crate::runtime::preparation::RuntimeSelection,
    ) -> anyhow::Result<crate::runtime::RuntimeView> {
        if let Some(gateway) = self.gateway() {
            return gateway.configure(selection).await;
        }
        let local = self.local()?;
        local
            .preparation
            .configure_runtime(&local.session, selection)
            .await
    }

    pub(crate) async fn navigate_branch(
        &self,
        navigation: crate::runtime::preparation::BranchNavigation,
    ) -> anyhow::Result<crate::runtime::RuntimeView> {
        let local = self.local()?;
        local
            .preparation
            .navigate_branch(&local.session, navigation)
            .await
    }

    pub(crate) async fn runtime_catalog(
        &self,
    ) -> anyhow::Result<super::preparation::RuntimeCatalog> {
        if let Some(gateway) = self.gateway() {
            return gateway.catalog().await;
        }
        let local = self.local()?;
        local.preparation.runtime_catalog(&local.session).await
    }

    pub(crate) async fn new_hosted_session(&self) -> anyhow::Result<Self> {
        let gateway = self
            .gateway()
            .ok_or_else(|| anyhow::anyhow!("hosted Session access requires Gateway"))?;
        Ok(Self {
            backend: AttachmentBackend::Gateway(gateway.new_session().await?),
        })
    }

    pub(crate) async fn resume_hosted_session(&self, id: &str) -> anyhow::Result<Self> {
        let gateway = self
            .gateway()
            .ok_or_else(|| anyhow::anyhow!("hosted Session access requires Gateway"))?;
        Ok(Self {
            backend: AttachmentBackend::Gateway(gateway.resume_session(id).await?),
        })
    }

    pub(crate) async fn saved_session_choices(
        &self,
        query: &str,
    ) -> anyhow::Result<Vec<crate::session::discovery::SessionSummary>> {
        let gateway = self
            .gateway()
            .ok_or_else(|| anyhow::anyhow!("hosted Session access requires Gateway"))?;
        gateway.saved_sessions(query).await
    }

    pub(crate) async fn reload_resources(&self) -> anyhow::Result<crate::runtime::RuntimeView> {
        if let Some(gateway) = self.gateway() {
            return gateway.reload().await;
        }
        let local = self.local()?;
        local.preparation.reload_resources(&local.session).await
    }

    pub(crate) async fn mcp_servers(&self) -> anyhow::Result<Vec<McpServerView>> {
        Ok(self
            .session_binding()?
            .mcp_servers()
            .await
            .into_iter()
            .map(McpServerView::from)
            .collect())
    }

    pub(crate) async fn mcp_tool_names(
        &self,
        tool_names: Vec<String>,
    ) -> anyhow::Result<std::collections::HashMap<String, String>> {
        Ok(self.session_binding()?.mcp_tool_names(&tool_names).await)
    }

    #[cfg(test)]
    pub(crate) fn expand_prompt(&self, display: &str) -> anyhow::Result<PromptExpandResult> {
        self.session_binding()?.expand_prompt(display)
    }

    pub(crate) fn background_required(&self) -> anyhow::Result<bool> {
        Ok(self.session_binding()?.background_required())
    }

    pub(crate) fn id(&self) -> &AgentInstanceId {
        match &self.backend {
            AttachmentBackend::Embedded(local) => local.session.id(),
            AttachmentBackend::Gateway(gateway) => gateway.id(),
        }
    }

    pub(crate) fn snapshot(&self) -> AgentInstanceSnapshot {
        match &self.backend {
            AttachmentBackend::Embedded(local) => local.snapshots.snapshot(),
            AttachmentBackend::Gateway(gateway) => gateway.snapshot(),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        match &self.backend {
            AttachmentBackend::Embedded(local) => local_subscription(&local.session),
            AttachmentBackend::Gateway(gateway) => gateway.subscribe(),
        }
    }

    pub(crate) fn registration_id(
        &self,
    ) -> anyhow::Result<Option<phi_sdk::agent_manager::AgentRegistrationId>> {
        Ok(self.session_binding()?.instance().registration_id())
    }

    pub(crate) fn subscribe_changes(&self) -> AgentChanges {
        AgentChanges {
            events: Some(self.subscribe()),
            snapshots: self.local().ok().map(|local| local.snapshots.clone()),
            output: self
                .session_binding()
                .ok()
                .map(|owner| owner.instance().subscribe_bash_output()),
        }
    }

    pub(crate) fn subscribe_snapshot_changes(&self) -> AgentChanges {
        AgentChanges {
            events: self.gateway().map(|gateway| gateway.subscribe()),
            snapshots: self.local().ok().map(|local| local.snapshots.clone()),
            output: None,
        }
    }

    pub(crate) async fn subscribe_tasks(
        &self,
    ) -> anyhow::Result<Option<phi_sdk::tasks::TaskSubscription>> {
        let owner = self.session_binding()?;
        let subscription = owner.instance().subscribe_tasks().await?;
        anyhow::ensure!(
            Arc::ptr_eq(&owner, &self.session_binding()?),
            "Session association changed during Task subscription"
        );
        Ok(subscription)
    }

    pub(crate) async fn observe_tasks(
        &self,
    ) -> anyhow::Result<Option<phi_sdk::tasks::TaskListView>> {
        let owner = self.session_binding()?;
        let view = owner.instance().observe_tasks().await?;
        anyhow::ensure!(
            Arc::ptr_eq(&owner, &self.session_binding()?),
            "Session association changed during Task observation"
        );
        Ok(view)
    }

    pub(crate) async fn ready_hook_notices(
        &self,
    ) -> anyhow::Result<Vec<phi_sdk::hooks::HookNotice>> {
        Ok(self.session_binding()?.ready_hook_notices().await)
    }

    #[cfg(test)]
    pub(crate) async fn submit(
        &self,
        prompt: TurnPrompt,
    ) -> Result<SessionSubmission, AgentInstanceError> {
        self.session_binding()
            .map_err(operation_error)?
            .submit(prompt, None, None)
            .await
            .map_err(operation_error)
    }

    pub(crate) async fn submit_directed(
        &self,
        prompt: TurnPrompt,
        constraint: DirectedDelegation,
    ) -> Result<SessionSubmission, AgentInstanceError> {
        self.session_binding()
            .map_err(operation_error)?
            .submit(prompt, Some(constraint), None)
            .await
            .map_err(operation_error)
    }

    pub(crate) async fn dispatch(
        &self,
        prompt: TurnPrompt,
        mode: SubmissionDispatch,
    ) -> Result<SessionSubmission, AgentInstanceError> {
        if let Some(gateway) = self.gateway() {
            let submission = gateway
                .dispatch(prompt, mode)
                .await
                .map_err(operation_error)?;
            return Ok(SessionSubmission {
                disposition: submission.disposition,
                notices: submission.notices,
            });
        }
        self.session_binding()
            .map_err(operation_error)?
            .submit(prompt, None, Some(mode))
            .await
            .map_err(operation_error)
    }

    pub(crate) async fn activities(&self) -> anyhow::Result<AgentActivities> {
        let owner = self.session_binding()?;
        let children = owner.activity_agents().await?;
        let mut instances = vec![owner.instance()];
        for child in children
            .iter()
            .filter(|child| child.mode == Some(DelegationMode::Foreground))
        {
            if let Ok(access) = self.local()?.session.child(&child.child_agent_id).await
                && let Ok(binding) = access.binding()
            {
                instances.push(binding.instance());
            }
        }
        let executions = instances
            .into_iter()
            .flat_map(|instance| {
                let owner = instance.id().clone();
                instance
                    .snapshot()
                    .bash_executions
                    .into_iter()
                    .filter(|execution| {
                        execution.mode == BashMode::Background
                            || execution.status == BashStatus::Running
                    })
                    .map(move |execution| (owner.clone(), execution))
            })
            .collect();
        anyhow::ensure!(
            Arc::ptr_eq(&owner, &self.session_binding()?),
            "Session association changed during activity observation"
        );
        Ok(AgentActivities {
            agents: children,
            bash_executions: executions,
        })
    }

    pub(crate) async fn background_bash(&self, id: &str) -> Result<(), AgentInstanceError> {
        self.session_binding()
            .map_err(operation_error)?
            .instance()
            .background_bash(id)
            .await?;
        Ok(())
    }

    pub(crate) async fn stop_bash(&self, id: &str) -> Result<(), AgentInstanceError> {
        self.session_binding()
            .map_err(operation_error)?
            .instance()
            .stop_bash(id)
            .await
    }

    pub(crate) async fn read_bash_output(&self, id: &str) -> Result<String, AgentInstanceError> {
        self.session_binding()
            .map_err(operation_error)?
            .instance()
            .read_bash_output(id)
            .await
    }

    pub(crate) async fn abort(&self) -> Result<(), AgentInstanceError> {
        if let Some(gateway) = self.gateway() {
            gateway.abort().await.map_err(operation_error)?;
            return Ok(());
        }
        self.session_binding()
            .map_err(operation_error)?
            .instance()
            .abort()
            .await
    }

    pub(crate) async fn pending_interactions(&self) -> anyhow::Result<Vec<InteractionEnvelope>> {
        if let Some(gateway) = self.gateway() {
            return Ok(gateway.pending_interactions());
        }
        self.local()?.session.pending_interactions()
    }

    pub(crate) async fn respond_interaction(
        &self,
        request_id: impl Into<String>,
        response: InteractionResponse,
    ) -> Result<InteractionAnswer, AgentInstanceError> {
        if let Some(gateway) = self.gateway() {
            let decision = match response {
                InteractionResponse::Permission(decision) => decision,
                InteractionResponse::McpElicitation(
                    phi_sdk::extension::mcp::McpElicitationResponse::Cancel
                    | phi_sdk::extension::mcp::McpElicitationResponse::Decline,
                ) => phi_sdk::interaction::PermissionDecision::Deny,
                _ => {
                    return Err(operation_error(anyhow::anyhow!(
                        "this interaction is unavailable through Gateway"
                    )));
                }
            };
            let persistence_error = gateway
                .respond(&request_id.into(), decision)
                .await
                .map_err(operation_error)?;
            return Ok(InteractionAnswer {
                #[cfg(test)]
                status: "answered".into(),
                allow_always: None,
                persistence_error,
            });
        }
        self.session_binding()
            .map_err(operation_error)?
            .respond_interaction(request_id.into(), response)
            .await
            .map_err(operation_error)
    }

    pub(crate) async fn save_observation(&self) -> anyhow::Result<SaveObservation> {
        if let Some(gateway) = self.gateway() {
            return Ok(gateway.save_observation());
        }
        Ok(self.session_binding()?.save_observation().await)
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "semantic client access has no TUI action")
    )]
    pub(crate) async fn reset_context(
        &self,
        handoff: Option<String>,
    ) -> Result<crate::runtime::AgentSessionRecovery, AgentInstanceError> {
        self.session_binding()
            .map_err(operation_error)?
            .reset_context(handoff)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn checkpoint(&self) -> anyhow::Result<()> {
        self.local()?.session.checkpoint().await
    }

    pub(crate) async fn workspace(
        &self,
        session_id: &str,
    ) -> anyhow::Result<crate::runtime::WorkspaceView> {
        let working_dir = if session_id == self.id().as_str() {
            if let Some(gateway) = self.gateway() {
                return Ok(gateway.workspace());
            }
            self.snapshot().working_dir
        } else {
            let link = self
                .subagents()
                .await?
                .into_iter()
                .find(|link| link.child_session_id == session_id)
                .ok_or_else(|| anyhow::anyhow!("unknown child Session `{session_id}`"))?;
            let view = self.subagent_view(&link.child_agent_id).await?;
            std::path::PathBuf::from(view.session.working_dir.as_str())
        };
        Ok(self
            .local()?
            .workspace
            .lock()
            .await
            .observe(&working_dir)
            .await)
    }

    pub(crate) async fn session(&self) -> anyhow::Result<Session> {
        if let Some(gateway) = self.gateway() {
            return Ok(gateway.session());
        }
        Ok(self.session_binding()?.session().await)
    }

    #[cfg(test)]
    pub(crate) async fn recovery(&self) -> anyhow::Result<crate::runtime::AgentSessionRecovery> {
        self.read_recovery(None).await
    }

    pub(crate) async fn recover_events(
        &self,
        events: &mut broadcast::Receiver<AgentEvent>,
    ) -> anyhow::Result<crate::runtime::AgentSessionRecovery> {
        const RECOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        const GATEWAY_RECOVERY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
        let recovery_timeout = if self.gateway().is_some() {
            GATEWAY_RECOVERY_TIMEOUT
        } else {
            RECOVERY_TIMEOUT
        };
        // Reset only before the read; its ordered marker protects all subsequent events.
        *events = events.resubscribe();
        let id = uuid::Uuid::new_v4().to_string();
        tokio::time::timeout(recovery_timeout, async {
            let recovery = self.read_recovery(Some(id.clone())).await?;
            wait_event_boundary(events, &id).await?;
            Ok(recovery)
        })
        .await
        .map_err(|_| anyhow::anyhow!("agent recovery event boundary timed out"))?
    }

    async fn read_recovery(
        &self,
        event_boundary: Option<String>,
    ) -> anyhow::Result<crate::runtime::AgentSessionRecovery> {
        if let Some(gateway) = self.gateway() {
            return gateway.recovery(event_boundary).await;
        }
        self.session_binding()?
            .recovery_at_event_boundary(event_boundary)
            .await
    }

    pub(crate) async fn transcript(&self) -> Result<Transcript, AgentInstanceError> {
        if let Some(gateway) = self.gateway() {
            return Ok(gateway.session().transcript);
        }
        self.session_binding()
            .map_err(operation_error)?
            .instance()
            .transcript()
            .await
    }

    pub(crate) async fn supported_agents(&self) -> Result<AgentCatalog, AgentInstanceError> {
        let agents = self
            .session_binding()
            .map_err(operation_error)?
            .instance()
            .supported_agents()
            .await?
            .all()
            .iter()
            .map(|agent| AgentDefinitionView {
                name: agent.name().to_string(),
                description: agent.description().to_string(),
            })
            .collect();
        Ok(AgentCatalog::new(agents))
    }

    pub(crate) async fn subagents(&self) -> anyhow::Result<Vec<crate::session::SubagentChildLink>> {
        self.session_binding()?.subagents().await
    }

    pub(crate) fn workflow_changed(&self) -> anyhow::Result<tokio::sync::futures::OwnedNotified> {
        Ok(self.session_binding()?.workflow_changed())
    }

    pub(crate) fn workflow_views(
        &self,
        registration: phi_sdk::agent_manager::AgentRegistrationId,
    ) -> anyhow::Result<Vec<crate::runtime::session::WorkflowView>> {
        self.session_binding()?.workflow_views(registration)
    }

    pub(crate) async fn child_attachment(&self, instance_id: &str) -> anyhow::Result<Self> {
        let local = self.local()?;
        let access = local.session.child(instance_id).await?;
        let binding = access.binding()?;
        Ok(Self::new(&binding, access, local.preparation.clone()))
    }

    pub(crate) async fn subagent_view(
        &self,
        instance_id: &str,
    ) -> anyhow::Result<crate::runtime::SubagentView> {
        self.local()?.session.subagent_view(instance_id).await
    }
}

impl AgentAttachment {
    pub(crate) async fn cancel_subagent(
        &self,
        instance_id: AgentInstanceId,
    ) -> Result<bool, AgentInstanceError> {
        self.session_binding()
            .map_err(|_| AgentInstanceError::Unavailable)?
            .instance()
            .cancel_subagent(instance_id)
            .await
    }

    pub(crate) async fn run_extension_keybinding(
        &self,
        id: phi_sdk::extension::ExtensionKeybindingId,
        event: serde_json::Value,
    ) -> anyhow::Result<super::session::ExtensionKeybindingResult> {
        self.session_binding()?
            .run_extension_keybinding(id, event)
            .await
    }

    pub(crate) async fn run_extension_command(
        &self,
        name: String,
        args: String,
    ) -> anyhow::Result<Option<phi_sdk::extension::ExtensionCommandOutcome>> {
        self.session_binding()?
            .run_extension_command(&name, &args)
            .await
    }

    pub(crate) async fn extension_before_command(
        &self,
        command: &str,
    ) -> anyhow::Result<Option<String>> {
        Ok(self
            .session_binding()?
            .extension_before_command(command)
            .await)
    }

    pub(crate) async fn extension_after_command(
        &self,
        command: &str,
        handled: bool,
    ) -> anyhow::Result<()> {
        self.session_binding()?
            .extension_after_command(command, handled)
            .await;
        Ok(())
    }

    pub(crate) async fn extension_notify(&self, level: &str, message: &str) -> anyhow::Result<()> {
        self.session_binding()?
            .extension_notify(level, message)
            .await;
        Ok(())
    }

    pub(crate) async fn extension_ui_rendered(&self) -> anyhow::Result<()> {
        self.session_binding()?.extension_ui_rendered().await;
        Ok(())
    }

    pub(crate) async fn discover_models(
        &self,
    ) -> anyhow::Result<phi_sdk::models::ModelDiscoveryResult> {
        self.session_binding()?.discover_models().await
    }
}
