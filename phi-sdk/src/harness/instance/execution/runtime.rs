use super::*;
use crate::agent::tools::task::TaskToolRequest;
use crate::core::interaction::{PermissionReceiver, PermissionSender};
use crate::factory::Phi;
use crate::harness::instance::builder::{AgentInstanceBuildError, AgentInstanceOptions};
use crate::harness::workflow::{
    AgentInvocation, InvocationId, WorkflowAdmission, WorkflowAgentOptions, WorkflowError,
    WorkflowLimits, WorkflowRunId,
};
use crate::provider;

const MAX_WORKFLOW_SCOPES: usize = 16;

pub(crate) struct WorkflowScope {
    creator: AgentInstanceId,
    registration: crate::harness::manager::AgentRegistrationId,
    _generation_lease: Arc<()>,
    definitions: AgentCatalog,
    default_role: Option<String>,
    working_dir: std::path::PathBuf,
    agent_access: crate::harness::manager::policy::InstanceAgentScope,
    permission: crate::permission::runtime::PermissionRuntime,
    child_factory: Phi,
    preparation: Option<Arc<dyn SubagentPreparation>>,
    limits: WorkflowLimits,
}

impl WorkflowScope {
    pub(crate) fn creator(&self) -> &AgentInstanceId {
        &self.creator
    }

    pub(crate) fn registration(&self) -> crate::harness::manager::AgentRegistrationId {
        self.registration
    }

    pub(crate) fn limits(&self) -> WorkflowLimits {
        self.limits
    }

    pub(crate) fn subscribe_closure(
        &self,
    ) -> tokio::sync::watch::Receiver<crate::harness::manager::AgentFinalizationStatus> {
        self.agent_access.finalization.subscribe()
    }

    pub(crate) fn resolve_invocation(
        &self,
        id: InvocationId,
        prompt: String,
        options: WorkflowAgentOptions,
        phase: Option<String>,
    ) -> Result<AgentInvocation, WorkflowError> {
        AgentInvocation::resolve(
            id,
            prompt,
            options,
            phase,
            &self.definitions,
            self.default_role.as_deref(),
        )
    }

    pub(crate) async fn reserve_agent(
        &self,
        run: WorkflowRunId,
        invocation: &AgentInvocation,
    ) -> Result<WorkflowAdmission, WorkflowError> {
        let request = invocation.request();
        let input = serde_json::to_string(&request)
            .map_err(|error| WorkflowError::InvalidRequest(error.to_string()))?;
        let interaction_scope = CoreEventScope {
            session_id: Some(self.creator.as_str().into()),
            agent_id: Some(self.creator.as_str().into()),
            ..Default::default()
        };
        crate::core::interaction::with_interaction_scope(
            interaction_scope,
            self.permission
                .authorize_tool("Agent", &input, None, Some("workflow script".into())),
        )
        .await
        .map_err(WorkflowError::Permission)?;
        let directory = crate::harness::manager::preparation::prepare_working_dir(
            self.working_dir.clone(),
            None,
        )
        .await
        .map_err(|error| WorkflowError::Admission(error.into()))?;
        let mut reserved = self
            .agent_access
            .reserve(request, directory, None, true)
            .map_err(WorkflowError::Admission)?;
        reserved
            .bind_workflow(run)
            .map_err(WorkflowError::Admission)?;
        Ok(WorkflowAdmission::new(reserved))
    }

    pub(crate) async fn admit_agent(
        &self,
        admission: &mut WorkflowAdmission,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<(), WorkflowError> {
        admission
            .admit_agent(&self.child_factory, self.preparation.clone(), cancellation)
            .await
    }
}

struct AgentInstanceSession {
    transcript: Transcript,
    services: RuntimeSessionServices,
}

#[derive(Clone)]
pub(super) struct RuntimeSessionServices {
    pub(super) working_dir: std::path::PathBuf,
    pub(super) memory: crate::extension::memory::MemorySession,
}

fn create_session(
    transcript: Transcript,
    cfg: &RuntimeConfig,
    working_dir: &std::path::Path,
) -> anyhow::Result<AgentInstanceSession> {
    transcript.effective_context()?;
    transcript.validate_switch_snapshots()?;
    let services = RuntimeSessionServices {
        working_dir: working_dir.to_path_buf(),
        memory: crate::extension::memory::MemorySession::create(
            cfg,
            transcript.id.clone(),
            working_dir,
        )?,
    };
    Ok(AgentInstanceSession {
        transcript,
        services,
    })
}

impl AgentInstanceState {
    pub(crate) fn has_workflows(&self) -> bool {
        self.workflow_scopes
            .iter()
            .any(|scope| scope.strong_count() != 0)
    }

    pub(crate) fn capture_workflow_scope(
        &mut self,
        default_role: Option<String>,
        limits: WorkflowLimits,
    ) -> Result<WorkflowScope, WorkflowError> {
        if !self.generation.runtime.is_tool_available("Agent") {
            return Err(WorkflowError::Unavailable);
        }
        if let Some(role) = &default_role
            && self.generation.agents.get(role).is_none()
        {
            return Err(WorkflowError::UnknownRole(role.clone()));
        }
        let scope = self
            .orchestration
            .as_ref()
            .ok_or(WorkflowError::Unavailable)?;
        let access = scope.access().map_err(WorkflowError::Admission)?;
        {
            let registry = access
                .manager
                .lock_live()
                .map_err(WorkflowError::Admission)?;
            access
                .require_creation(
                    &registry,
                    crate::harness::manager::AgentOperation::CreateAgent,
                )
                .map_err(WorkflowError::Admission)?;
        }
        self.workflow_scopes
            .retain(|scope| scope.strong_count() != 0);
        if self.workflow_scopes.len() >= MAX_WORKFLOW_SCOPES {
            return Err(WorkflowError::InvalidRequest(
                "creator Workflow scope limit reached".into(),
            ));
        }
        let generation = Arc::new(());
        self.workflow_scopes.push(Arc::downgrade(&generation));
        Ok(WorkflowScope {
            creator: AgentInstanceId::from_transcript(&self.transcript),
            registration: scope.registration_id(),
            _generation_lease: generation,
            definitions: self.generation.agents.clone(),
            default_role,
            working_dir: self.working_dir().to_path_buf(),
            agent_access: scope.clone(),
            permission: self.generation.resources.permission().clone(),
            child_factory: self.generation.child_factory.clone(),
            preparation: self.generation.subagent_preparation.clone(),
            limits,
        })
    }

    pub(crate) fn background_required(&self) -> bool {
        self.generation.cfg.agent_background
    }

    pub(crate) fn supported_agents(&self) -> &AgentCatalog {
        &self.generation.agents
    }

    pub(crate) fn working_dir(&self) -> &std::path::Path {
        &self.generation.services.working_dir
    }

    pub(crate) fn replace_runtime(
        &mut self,
        generation: RuntimeGeneration,
    ) -> Result<(), AgentInstanceError> {
        if self.has_workflows() {
            return Err(AgentInstanceError::Busy);
        }
        if let Some(scope) = &self.orchestration {
            let access = scope
                .access()
                .map_err(|source| AgentInstanceError::Operation {
                    source: source.into(),
                })?;
            let mut registry =
                access
                    .manager
                    .lock()
                    .map_err(|source| AgentInstanceError::Operation {
                        source: source.into(),
                    })?;
            let member = access
                .caller(&registry, crate::agent_manager::AgentOperation::Observe)
                .map_err(|source| AgentInstanceError::Operation {
                    source: source.into(),
                })?;
            if registry.shutting_down
                || member.closing
                || !matches!(
                    *scope.finalization.borrow(),
                    crate::agent_manager::AgentFinalizationStatus::Open
                )
            {
                return Err(AgentInstanceError::Stopped);
            }
            let team_id = member.team.clone();
            if let Some(team) = team_id.and_then(|id| registry.teams.get_mut(&id))
                && team.lead_epoch == access.epoch
                && team.config.lead() == &access.caller
            {
                team.capability = generation.team_capability;
            }
            registry
                .members
                .get_mut(&access.caller)
                .expect("validated caller")
                .background_required = generation.cfg.agent_background;
        }
        generation
            .resources
            .permission()
            .inherit_session_grants(self.generation.resources.permission());
        generation.hooks.continue_bindings(&self.generation.hooks);
        self.pending_internal_prompt = None;
        self.generation = generation;
        Ok(())
    }

    pub(crate) async fn poll_interaction(
        &mut self,
    ) -> Option<crate::core::interaction::InteractionEnvelope> {
        let hooks = self.generation.hooks.clone();
        self.generation.interactions.poll(&hooks).await
    }

    pub(crate) async fn drain_interactions(
        &mut self,
    ) -> Vec<crate::core::interaction::InteractionEnvelope> {
        let mut interactions = Vec::new();
        while let Some(request) = self.poll_interaction().await {
            interactions.push(request);
        }
        interactions
    }

    pub(crate) fn has_pending_interactions(&self) -> bool {
        self.generation.interactions.has_pending()
    }

    pub(crate) fn is_interaction_pending(&mut self, request_id: &str) -> bool {
        self.generation.interactions.is_pending(request_id)
    }

    pub(crate) async fn respond_interaction(
        &mut self,
        request_id: &str,
        response: crate::core::interaction::InteractionResponse,
    ) -> anyhow::Result<crate::core::interaction::InteractionAnswer> {
        let hooks = self.generation.hooks.clone();
        self.generation
            .interactions
            .respond(request_id, response, &hooks)
            .await
    }

    pub(crate) fn deny_pending_interactions(&mut self) {
        self.generation.interactions.deny_all();
    }

    pub(crate) fn tool_display(&self) -> &ToolDisplayRegistry {
        self.generation.tool_display.as_ref()
    }
}

// Private provider, sandbox, interaction and execution-resource assembly for SDK candidates.

impl<'a> AgentInstanceBuilder<'a> {
    pub(crate) async fn prepare(
        self,
        validation: Result<(), AgentInstanceBuildError>,
    ) -> Result<AgentInstanceCandidate, AgentInstanceBuildError> {
        validation?;
        let transcript = self.transcript.unwrap_or_else(Transcript::new);
        let working_dir = match self.working_dir {
            Some(path) => path,
            None => std::env::current_dir().map_err(|source| AgentInstanceBuildError::Runtime {
                source: source.into(),
            })?,
        };
        // Keep candidate-local factory state off the nested preparation future's stack.
        let phi = Box::new(self.phi.clone());
        let phi = {
            let mut phi = phi;
            if !phi.options.no_tools
                && let Some(manager) = phi.runtime_services.mcp_manager_ref()
            {
                let (snapshot, contracts) = manager.snapshot_tool_contracts().await;
                phi.context
                    .validate_agent_tool_declarations(&|rule| contracts.validate_tool_rule(rule))
                    .map_err(|source| AgentInstanceBuildError::Runtime {
                        source: source.into(),
                    })?;
                phi.runtime_services = phi.runtime_services.clone().mcp_manager(snapshot);
            }
            phi
        };
        let client = create_client(&phi).map_err(|source| AgentInstanceBuildError::Provider {
            provider: phi.provider.clone(),
            source,
        })?;
        let mut runtime_cfg = resolve_runtime_config(&phi, &self.options);
        let session = create_session(transcript, &runtime_cfg, &working_dir)
            .map_err(|source| AgentInstanceBuildError::Session { source })?;
        let (permission_tx, interaction_rx) = permission_interaction_channel(&phi);
        let (elicitation_tx, elicitation_rx) = mcp_elicitation_channel(&phi);
        let resources = phi
            .prepare_resources(&working_dir, permission_tx)
            .map_err(|source| AgentInstanceBuildError::Runtime { source })?;
        let (runtime, descriptor, catalog, agent_requests, task_requests, bash_requests) =
            build_runtime(
                &phi,
                &resources,
                &client,
                &self.options,
                &runtime_cfg,
                &session.services,
                elicitation_tx,
            )
            .await?;
        let (child_worker_sender, child_worker_messages) = tokio::sync::mpsc::unbounded_channel();
        runtime_cfg.context_window = runtime_cfg
            .context_window
            .or(Some(descriptor.context_window));
        Ok(AgentInstanceCandidate {
            state: AgentInstanceState {
                delegated_child: self.delegated_child,
                teammate_stop_requested: false,
                automatic_turns: AutomaticTurnPolicy::Enabled,
                task_list: self.task_list,
                orchestration: None,
                workflow_scopes: Vec::new(),
                workflow_feedback: Default::default(),
                pending_creations: std::collections::VecDeque::new(),
                transcript: session.transcript,
                pending_internal_prompt: None,
                pending_message_prompt: None,
                preparing_submission: false,
                directed_delegation: None,
                delegated_agent: None,
                generation: RuntimeGeneration {
                    team_capability: self.team_capability,
                    client,
                    runtime,
                    descriptor,
                    catalog,
                    agent_options: phi.options.clone(),
                    cfg: runtime_cfg,
                    services: session.services,
                    hooks: resources.hooks.clone(),
                    tool_display: resources.tool_display.clone(),
                    resources,
                    agents: phi.context.agent_catalog.clone(),
                    interactions: crate::harness::instance::interaction::InteractionBroker::new(
                        interaction_rx,
                        elicitation_rx,
                    ),
                    agent_requests,
                    task_requests,
                    bash_requests,
                    subagent_preparation: self.subagent_preparation,
                    child_factory: *phi,
                },
                main_run: None,
                bash_executions: Default::default(),
                bash_owner_mode: DelegationMode::Background,
                child_workers: Default::default(),
                child_worker_messages,
                child_worker_sender,
                child_observations: Default::default(),
                completed_child_ids: Default::default(),
                admitted_subagent_outcomes: Default::default(),
                agent_tool_replies: Default::default(),
                cancelled_admission_tools: Default::default(),
                observer_subagent_activity: Default::default(),
                queued_steer_submissions: Default::default(),
                follow_up_submissions: Default::default(),
                pending_submissions: Default::default(),
            },
        })
    }
}

fn resolve_runtime_config(
    phi: &Phi,
    options: &AgentInstanceOptions,
) -> crate::config::RuntimeConfig {
    let mut cfg = phi.cfg.clone();
    if let Some(mode) = options.agent_mode {
        if mode != cfg.agent_mode && options.mode_prompt_override.is_none() {
            cfg.mode_prompt_override = None;
        }
        cfg.agent_mode = mode;
    }
    if let Some(prompt) = &options.mode_prompt_override {
        cfg.mode_prompt_override = Some(prompt.clone());
    }
    cfg
}

fn create_client(phi: &Phi) -> anyhow::Result<AnyClient> {
    provider::create_client(
        &phi.provider,
        phi.api_key.as_deref(),
        &phi.cfg.custom_providers,
    )
}

fn permission_interaction_channel(
    phi: &Phi,
) -> (Option<PermissionSender>, Option<PermissionReceiver>) {
    if !phi.runtime_services.interactive_enabled() {
        return (None, None);
    }
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    (Some(sender), Some(receiver))
}

fn mcp_elicitation_channel(
    phi: &Phi,
) -> (
    Option<crate::core::interaction::McpElicitationSender>,
    Option<crate::core::interaction::McpElicitationReceiver>,
) {
    if !phi.runtime_services.interactive_enabled()
        || phi.runtime_services.mcp_manager_ref().is_none()
    {
        return (None, None);
    }
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    (Some(sender), Some(receiver))
}

async fn build_runtime(
    phi: &Phi,
    resources: &crate::factory::AgentRuntimeResources,
    client: &AnyClient,
    query_options: &AgentInstanceOptions,
    runtime_cfg: &crate::config::RuntimeConfig,
    session: &RuntimeSessionServices,
    elicitation_tx: Option<crate::core::interaction::McpElicitationSender>,
) -> Result<
    (
        AnyRuntime,
        crate::models::ModelDescriptor,
        Arc<crate::models::ModelCatalog>,
        Option<tokio::sync::mpsc::UnboundedReceiver<AgentToolRequest>>,
        Option<tokio::sync::mpsc::UnboundedReceiver<TaskToolRequest>>,
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::harness::instance::bash::BashRequest>>,
    ),
    AgentInstanceBuildError,
> {
    let model = query_options.model.as_deref().unwrap_or(&phi.model);
    let catalog = phi
        .runtime_services
        .resolve_model_catalog(&phi.provider)
        .map_err(|source| AgentInstanceBuildError::ModelCatalog {
            provider: phi.provider.clone(),
            source,
        })?;
    let descriptor =
        catalog
            .get(&phi.provider, model)
            .ok_or_else(|| AgentInstanceBuildError::UnknownModel {
                provider: phi.provider.clone(),
                model: model.to_string(),
            })?;
    let resolved_model = client.resolve_model(&descriptor).map_err(|source| {
        AgentInstanceBuildError::ModelConfiguration {
            provider: phi.provider.clone(),
            model: model.to_string(),
            source,
        }
    })?;
    let runtime_descriptor = descriptor.clone();
    let sandbox = phi
        .runtime_services
        .resolve_sandbox(&session.working_dir)
        .map_err(|source| AgentInstanceBuildError::Sandbox { source })?;
    let mut options = phi.options.clone();
    let (agent_sender, agent_requests) = tokio::sync::mpsc::unbounded_channel();
    let (task_sender, task_requests) = tokio::sync::mpsc::unbounded_channel();
    options.agent_sender = Some(agent_sender);
    options.task_sender = Some(task_sender);
    let (bash_sender, bash_requests) = tokio::sync::mpsc::unbounded_channel();
    options.bash_sender = Some(bash_sender);
    options.memory = session.memory.clone();
    let mcp_manager = phi
        .runtime_services
        .mcp_manager_ref()
        .cloned()
        .map(|manager| {
            manager
                .with_sandbox(sandbox.clone())
                .with_interaction_sender(elicitation_tx)
        });
    let mut runtime = provider::build_runtime(
        resolved_model,
        &options,
        runtime_cfg,
        &phi.context,
        &session.working_dir,
        Some(resources.permission().clone()),
        sandbox,
        resources.extensions().as_ref(),
        &phi.tool_declarations,
        mcp_manager.as_ref(),
    )
    .await
    .map_err(|source| AgentInstanceBuildError::Runtime { source })?;

    if let Some(system_prompt) = &query_options.system_prompt {
        runtime = runtime.append_system_prompt(system_prompt);
    }
    if query_options.tool_policy.is_restricted() {
        runtime = runtime.tool_policy(
            query_options.tool_policy.allowed.clone(),
            query_options.tool_policy.disallowed.clone(),
        );
    }
    runtime = runtime.defer_system_prompt(&resources.hooks, &session.working_dir);
    Ok((
        runtime,
        runtime_descriptor,
        catalog,
        Some(agent_requests),
        Some(task_requests),
        Some(bash_requests),
    ))
}

impl AgentInstanceCandidate {
    pub(crate) fn bind_orchestration(
        &mut self,
        scope: crate::harness::manager::policy::InstanceAgentScope,
    ) {
        self.state.orchestration = Some(scope);
    }

    pub(crate) fn orchestration(
        &self,
    ) -> Option<crate::harness::manager::policy::InstanceAgentScope> {
        self.state.orchestration.clone()
    }

    pub(crate) fn transcript(&self) -> &Transcript {
        &self.state.transcript
    }

    pub(crate) fn validate_creation(&self, creation: &AgentCreation) -> Result<(), &'static str> {
        if self.id() != creation.instance_id().as_str() {
            return Err("identity does not match the reserved child");
        }
        if self.task_list() != creation.task_list() {
            return Err("child candidate does not match the inherited task list");
        }
        if self.state.working_dir() != creation.working_dir()
            || !self.state.delegated_child
            || self.state.needs_progress()
            || !self.state.transcript.entries.is_empty()
        {
            return Err("creation requires an idle, empty, directory-matching child candidate");
        }
        Ok(())
    }

    pub(crate) fn into_state(mut self) -> AgentInstanceState {
        self.state.recover_bash_history();
        self.state
    }

    #[cfg(test)]
    pub(crate) fn install_test_agent_channel(&mut self) -> crate::agent::tools::AgentToolSender {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        self.state.generation.agent_requests = Some(receiver);
        sender
    }

    #[cfg(test)]
    pub(crate) fn install_test_main_run(
        &mut self,
        turn_id: &str,
    ) -> tokio::sync::mpsc::Sender<crate::core::event::AgentEvent> {
        self.state.install_test_main_run(turn_id)
    }
}
