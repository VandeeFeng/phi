use std::collections::HashSet;
use std::process::Stdio;
use std::sync::{Arc, RwLock};

use phi_sdk::agent_instance::{
    AgentInstance, AgentInstanceError, AgentInstanceSnapshot, AgentInstanceStatus,
};
use phi_sdk::agents::SubagentAdmission;
use phi_sdk::extension::{
    ExtensionCommandOutcome, ExtensionEffect, ExtensionKeybindingId, ExtensionRuntime,
};
use phi_sdk::hooks::HookNotice;
use phi_sdk::lifecycle::{SessionEndReason, SessionStartReason};
use phi_sdk::sandbox::Sandbox;
use tokio::sync::{Mutex, Notify};

use super::save::{SaveCompletion, SaveObservation, SaveOperation, SavedBoundary};
use super::{AgentRuntime, PreparedAgentRuntime};
use crate::session::{ChildSessionLink, PermissionAllowEntry, Session, SubagentChildLink};

use crate::session::SubagentOutcome;
use crate::session::storage::WorkflowStorage;
use crate::session::workflow::{
    WorkflowFeedback, WorkflowInvocation, WorkflowRecord, WorkflowState,
};
use futures_util::FutureExt;
use phi_sdk::agent_instance::AgentInstanceId;
use phi_sdk::agent_manager::AgentRegistrationId;
use phi_sdk::workflow::{
    WorkflowLimits, WorkflowRequest, WorkflowRun, WorkflowRunId, WorkflowRuntime, WorkflowSnapshot,
    WorkflowStatus,
};

pub(crate) struct SessionSubmission {
    pub(crate) disposition: phi_sdk::turn::SubmissionDisposition,
    pub(crate) notices: Vec<HookNotice>,
}

pub(crate) struct SessionNotices {
    pub(crate) agent_id: AgentInstanceId,
    pub(crate) hook_notices: Vec<HookNotice>,
    pub(crate) extension_notifications: Vec<String>,
}

impl SessionNotices {
    pub(super) fn take(
        agent_id: AgentInstanceId,
        resources: &phi_sdk::agent_instance::AgentRuntimeResources,
    ) -> anyhow::Result<Self> {
        let extension_notifications = resources
            .extensions()
            .map(|extensions| extensions.take_notifications())
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            agent_id,
            hook_notices: resources.take_hook_notices(),
            extension_notifications,
        })
    }
}

pub(crate) struct ExtensionKeybindingResult {
    pub(crate) notifications: Vec<String>,
    pub(crate) effect_result: anyhow::Result<()>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentSessionSnapshot {
    pub(crate) instance: AgentInstanceSnapshot,
    #[cfg(test)]
    pub(crate) runtime_generation_id: String,
    pub(crate) persistence_error: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct AgentSessionRecovery {
    pub(crate) pending_submission_ids: Vec<phi_sdk::turn::SubmissionId>,
    pub(crate) session: Session,
    pub(crate) snapshot: AgentSessionSnapshot,
    pub(crate) declarations: RuntimeDeclarations,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RuntimeDeclarations {
    pub(crate) generation_id: String,
    pub(crate) capabilities: AgentCapabilities,
    pub(crate) commands: Vec<crate::commands::SlashCommand>,
}

#[derive(Debug)]
pub(crate) struct RuntimeView {
    pub(crate) snapshot: AgentSessionSnapshot,
    pub(crate) session: Session,
    pub(crate) declarations: RuntimeDeclarations,
}

pub(crate) struct SessionInteractionAnswer {
    #[cfg(test)]
    pub(crate) status: String,
    pub(crate) allow_always: Option<(String, String)>,
    pub(crate) persistence_error: Option<String>,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) agent_diagnostics: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) compact_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) prompt_diagnostics: Vec<crate::resources::ResourceDiagnostic>,
    pub(crate) extension_commands: Vec<phi_sdk::extension::RegisteredCommand>,
    pub(crate) extension_keybindings: Vec<phi_sdk::extension::RegisteredKeybinding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) thinking_level: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) supported_thinking_levels: Vec<phi_sdk::models::ThinkingLevel>,
}

pub(super) struct RuntimeConfigurationSeed {
    pub(super) session: Session,
    pub(super) source_settings: Arc<crate::config::Settings>,
    pub(super) runtime: AgentRuntime,
}

pub(crate) struct AgentSession {
    instance: AgentInstance,
    creator_id: Option<phi_sdk::agent_instance::AgentInstanceId>,
    session: Arc<Mutex<SessionState>>,
    historical_children: HashSet<String>,
    runtime: RwLock<AgentRuntime>,
    transition: Arc<Mutex<()>>,
    workflow_admission: Arc<Mutex<WorkflowAdmission>>,
    workflow_deliveries: std::sync::Mutex<Vec<WorkflowDelivery>>,
    workflow_changed: Arc<Notify>,
    // Serializes Session grant application/accounting with replacement/finalization.
    // This is not a durability guarantee. Keep separate from transition so lifecycle
    // hooks can receive replies; when both are needed, acquire transition first.
    permission_gate: Arc<Mutex<()>>,
    persist: bool,
    checkpoint_observer: Mutex<Option<tokio::task::JoinHandle<()>>>,
    checkpoint_requested: Arc<Notify>,
    checkpoint_gate: Mutex<()>,
    pub(super) runtime_changed: Arc<Notify>,
    pub(super) mcp_events: tokio::sync::broadcast::Sender<phi_sdk::extension::mcp::McpRuntimeEvent>,
}

struct SessionState {
    session: Session,
    session_ended: bool,
    finalized: bool,
    end_context: Option<phi_sdk::lifecycle::SessionEndEvent>,
    persistence_error: Option<String>,
    save_observation: SaveObservation,
}

impl AgentSession {
    pub(crate) fn new(
        instance: AgentInstance,
        session: Session,
        runtime: AgentRuntime,
        persist: bool,
    ) -> Arc<Self> {
        let historical_children = session
            .children
            .iter()
            .map(|child| child.child_agent_id.clone())
            .collect();
        Arc::new(Self {
            instance,
            historical_children,
            creator_id: session.creator_id().cloned(),
            session: Arc::new(Mutex::new(SessionState {
                session,
                session_ended: false,
                finalized: false,
                end_context: None,
                persistence_error: None,
                save_observation: SaveObservation::new(persist),
            })),
            runtime: RwLock::new(runtime),
            transition: Arc::new(Mutex::new(())),
            workflow_admission: Arc::new(Mutex::new(WorkflowAdmission::Open)),
            workflow_deliveries: std::sync::Mutex::new(Vec::new()),
            workflow_changed: Arc::new(Notify::new()),
            permission_gate: Arc::new(Mutex::new(())),
            persist,
            checkpoint_observer: Mutex::new(None),
            checkpoint_requested: Arc::new(Notify::new()),
            checkpoint_gate: Mutex::new(()),
            runtime_changed: Arc::new(Notify::new()),
            mcp_events: tokio::sync::broadcast::channel(256).0,
        })
    }

    pub(crate) async fn submit(
        self: &Arc<Self>,
        mut prompt: phi_sdk::turn::TurnPrompt,
        directed: Option<phi_sdk::turn::DirectedDelegation>,
        dispatch: Option<phi_sdk::turn::SubmissionDispatch>,
    ) -> anyhow::Result<SessionSubmission> {
        let transition = self.transition.clone().lock_owned().await;
        self.inject_workflow_context(&mut prompt)?;
        let resources = self.runtime().sdk_resources();
        resources.ready().await;
        let instance = self.instance.clone();
        // Admission can outlive its waiter; serialize transitions until it finishes.
        tokio::spawn(async move {
            let _transition = transition;
            let disposition = match (directed, dispatch) {
                (Some(constraint), _) => instance.submit_directed(prompt, constraint).await,
                (None, Some(mode)) => instance.dispatch(prompt, mode).await,
                (None, None) => instance.submit(prompt).await,
            }?;
            Ok(SessionSubmission {
                disposition,
                notices: resources.take_hook_notices(),
            })
        })
        .await?
    }

    pub(crate) async fn reset_context(
        self: &Arc<Self>,
        handoff: Option<String>,
    ) -> Result<AgentSessionRecovery, AgentInstanceError> {
        let transition = self.transition.clone().lock_owned().await;
        self.ensure_session_open()
            .await
            .map_err(|source| AgentInstanceError::Operation { source })?;
        let agent_session = self.clone();
        tokio::spawn(async move {
            let _transition = transition;
            let view = agent_session.instance.reset_context(handoff).await?;
            let session = {
                let mut state = agent_session.session.lock().await;
                state.session.set_transcript(view.transcript);
                state.session.clone()
            };
            let snapshot = agent_session.application_snapshot(view.snapshot).await;
            let declarations = agent_session.runtime_declarations();
            agent_session.request_checkpoint();
            Ok(AgentSessionRecovery {
                pending_submission_ids: view.pending_submission_ids,
                session,
                snapshot,
                declarations,
            })
        })
        .await
        .map_err(|error| AgentInstanceError::Operation {
            source: error.into(),
        })?
    }

    pub(crate) fn take_notices(&self) -> anyhow::Result<SessionNotices> {
        SessionNotices::take(self.instance.id().clone(), &self.runtime().sdk_resources())
    }

    pub(crate) async fn ready_hook_notices(&self) -> Vec<HookNotice> {
        let _transition = self.transition.lock().await;
        let resources = self.runtime().sdk_resources();
        resources.ready().await;
        resources.take_hook_notices()
    }

    pub(crate) fn commands(&self) -> Vec<crate::commands::SlashCommand> {
        self.runtime().commands()
    }

    pub(crate) fn background_required(&self) -> bool {
        self.runtime().background_required()
    }

    pub(crate) fn capabilities(&self) -> AgentCapabilities {
        let runtime = self.runtime();
        let extensions = runtime.extensions();
        let declarations = extensions.adapter_declarations();
        AgentCapabilities {
            agent_diagnostics: runtime.agent_diagnostics.clone(),
            compact_enabled: Some(runtime.compact_enabled),
            prompt_diagnostics: runtime.prompt_diagnostics(),
            extension_commands: declarations.commands().to_vec(),
            extension_keybindings: declarations.keybindings().to_vec(),
            thinking_level: runtime.thinking_level.clone(),
            supported_thinking_levels: runtime.supported_thinking_levels.clone(),
        }
    }

    pub(crate) fn expand_prompt(
        &self,
        display: &str,
    ) -> anyhow::Result<crate::resources::prompts::PromptExpandResult> {
        self.runtime().expand_prompt(display)
    }

    pub(crate) fn creator_id(&self) -> Option<&phi_sdk::agent_instance::AgentInstanceId> {
        self.creator_id.as_ref()
    }

    pub(crate) fn instance(&self) -> AgentInstance {
        self.instance.clone()
    }

    pub(crate) fn persists(&self) -> bool {
        self.persist
    }

    pub(crate) fn subscribe_mcp(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<phi_sdk::extension::mcp::McpRuntimeEvent>> {
        Some(self.mcp_events.subscribe())
    }

    pub(super) fn mcp_manager(&self) -> Option<phi_sdk::extension::mcp::McpClientManager> {
        self.runtime().mcp_manager()
    }

    async fn mcp_server_manager(
        &self,
        server: &str,
    ) -> anyhow::Result<phi_sdk::extension::mcp::McpClientManager> {
        let manager = self
            .mcp_manager()
            .ok_or_else(|| anyhow::anyhow!("MCP is not enabled for this Agent"))?;
        if !manager
            .server_snapshots()
            .await
            .iter()
            .any(|entry| entry.name == server)
        {
            anyhow::bail!("unknown MCP server: {server}");
        }
        Ok(manager)
    }

    async fn prepare_mcp_reconnect(
        &self,
        server: &str,
    ) -> anyhow::Result<Result<phi_sdk::extension::mcp::McpClientManager, u64>> {
        let manager = self.mcp_server_manager(server).await?;
        if let Some(seconds) = manager.failure_retry_after_secs(server).await {
            return Ok(Err(seconds));
        }
        manager.clear_cancelled_connect(server).await;
        Ok(Ok(manager))
    }

    pub(crate) async fn reconnect_mcp(
        self: &Arc<Self>,
        server: &str,
    ) -> anyhow::Result<Result<usize, u64>> {
        match self.prepare_mcp_reconnect(server).await? {
            Ok(manager) => {
                let server = server.to_string();
                tokio::spawn(async move { Ok(Ok(manager.refresh_metadata(&server).await?.len())) })
                    .await?
            }
            Err(seconds) => Ok(Err(seconds)),
        }
    }

    pub(crate) async fn cancel_mcp_connect(&self, server: &str) -> anyhow::Result<()> {
        self.mcp_server_manager(server)
            .await?
            .cancel_connect(server)
            .await;
        Ok(())
    }

    pub(crate) async fn mcp_servers(&self) -> Vec<phi_sdk::extension::mcp::McpServerSnapshot> {
        let manager = self.runtime().mcp_manager();
        match manager {
            Some(manager) => manager.server_snapshots().await,
            None => Vec::new(),
        }
    }

    pub(crate) async fn mcp_tool_names(
        &self,
        tool_names: &[String],
    ) -> std::collections::HashMap<String, String> {
        let Some(manager) = self.mcp_manager() else {
            return Default::default();
        };
        let mut names = manager.mcp_tool_display_names().await;
        for name in tool_names {
            if let Some(display) = manager.infer_mcp_tool_display_name(name) {
                names.entry(name.clone()).or_insert(display);
            }
        }
        names
    }

    pub(crate) async fn run_extension_keybinding(
        &self,
        id: ExtensionKeybindingId,
        event: serde_json::Value,
    ) -> anyhow::Result<ExtensionKeybindingResult> {
        let (extensions, sandbox) = {
            let runtime = self.runtime();
            (runtime.extensions(), runtime.sandbox())
        };
        let outcome = extensions.run_keybinding(id, &event)?;
        let effect_result =
            apply_extension_effects(extensions.as_ref(), outcome.effects, &sandbox).await;
        Ok(ExtensionKeybindingResult {
            notifications: outcome.notifications,
            effect_result,
        })
    }

    pub(crate) async fn run_extension_command(
        self: &Arc<Self>,
        name: &str,
        args: &str,
    ) -> anyhow::Result<Option<ExtensionCommandOutcome>> {
        let (extensions, generation) = {
            let runtime = self.runtime();
            (runtime.extensions(), runtime.generation_id().to_string())
        };
        let agent = Arc::new(CommandPromptSubmitter {
            agent_session: Arc::downgrade(self),
            generation,
        });
        let mut snapshots = self.instance.subscribe_snapshots();
        let (idle_tx, idle_rx) =
            tokio::sync::watch::channel(instance_is_idle(snapshots.snapshot().status));
        let _idle_signal = CommandIdleSignal(tokio::spawn(async move {
            while let Ok(snapshot) = snapshots.changed().await {
                idle_tx.send_replace(instance_is_idle(snapshot.status));
            }
        }));
        extensions
            .run_command_with_agent(name, args, idle_rx, agent)
            .await
    }

    pub(crate) async fn extension_before_command(&self, command: &str) -> Option<String> {
        let extensions = self.runtime().extensions();
        extensions.before_command(command).await
    }

    pub(crate) async fn extension_after_command(&self, command: &str, handled: bool) {
        let extensions = self.runtime().extensions();
        extensions.after_command(command, handled).await;
    }

    pub(crate) async fn extension_session_tree_changed(
        &self,
        reason: phi_sdk::lifecycle::SessionTreeReason,
        target_id: &str,
        old_leaf_id: Option<&str>,
        new_leaf_id: Option<&str>,
        editor_text: bool,
    ) {
        let extensions = self.runtime().extensions();
        extensions
            .session_tree_changed(reason, target_id, old_leaf_id, new_leaf_id, editor_text)
            .await;
    }

    pub(crate) async fn extension_notify(&self, level: &str, message: &str) {
        self.runtime()
            .sdk_resources()
            .notify(phi_sdk::lifecycle::NotificationEvent {
                kind: phi_sdk::lifecycle::NotificationKind::Message,
                level: level.into(),
                message: message.into(),
                tool_name: None,
            });
    }

    pub(crate) async fn extension_ui_rendered(&self) {
        let extensions = self.runtime().extensions();
        extensions.ui_rendered().await;
    }

    pub(crate) async fn session_started(
        &self,
        reason: SessionStartReason,
        related_session: Option<&str>,
    ) {
        let _transition = self.transition.lock().await;
        let resources = self.runtime().sdk_resources();
        resources.session_started(reason, related_session).await;
    }

    pub(crate) async fn discover_models(
        &self,
    ) -> anyhow::Result<phi_sdk::models::ModelDiscoveryResult> {
        let phi = self.runtime().phi().clone();
        phi.discover_models().await
    }

    pub(crate) async fn recovery(&self) -> anyhow::Result<AgentSessionRecovery> {
        self.recovery_at_event_boundary(None).await
    }

    pub(crate) async fn recovery_at_event_boundary(
        &self,
        event_boundary: Option<String>,
    ) -> anyhow::Result<AgentSessionRecovery> {
        let _transition = self.transition.lock().await;
        let view = match event_boundary {
            Some(id) => self.instance.view_with_event_boundary(id).await?,
            None => self.instance.view().await?,
        };
        let mut session = self.session().await;
        session.set_transcript(view.transcript);
        let snapshot = self.application_snapshot(view.snapshot).await;
        Ok(AgentSessionRecovery {
            pending_submission_ids: view.pending_submission_ids,
            session,
            snapshot,
            declarations: self.runtime_declarations(),
        })
    }

    fn runtime_declarations(&self) -> RuntimeDeclarations {
        RuntimeDeclarations {
            generation_id: self.runtime().generation_id().to_string(),
            capabilities: self.capabilities(),
            commands: self.commands(),
        }
    }

    pub(super) async fn runtime_seed(&self) -> (Session, AgentRuntime) {
        let _transition = self.transition.lock().await;
        (self.session().await, self.runtime().clone())
    }

    pub(super) async fn configuration_seed(&self) -> anyhow::Result<RuntimeConfigurationSeed> {
        let _transition = self.transition.lock().await;
        let transcript = self.instance.transcript().await?;
        let mut session = self.session().await;
        session.set_transcript(transcript);
        let runtime = self.runtime().clone();
        let source_settings = runtime
            .source_settings()
            .ok_or_else(|| anyhow::anyhow!("Agent runtime has no source configuration"))?;
        Ok(RuntimeConfigurationSeed {
            session,
            source_settings,
            runtime,
        })
    }

    pub(crate) async fn current_runtime_view(&self) -> RuntimeView {
        let _transition = self.transition.lock().await;
        self.runtime_view_locked().await
    }

    async fn runtime_view_locked(&self) -> RuntimeView {
        RuntimeView {
            snapshot: self.snapshot().await,
            session: self.session().await,
            declarations: self.runtime_declarations(),
        }
    }

    pub(crate) async fn snapshot(&self) -> AgentSessionSnapshot {
        self.application_snapshot(self.instance.snapshot()).await
    }

    async fn application_snapshot(&self, instance: AgentInstanceSnapshot) -> AgentSessionSnapshot {
        let session = self.session.lock().await;
        AgentSessionSnapshot {
            instance,
            #[cfg(test)]
            runtime_generation_id: self.runtime().generation_id().to_string(),
            persistence_error: session.persistence_error.clone(),
        }
    }

    pub(crate) async fn replace_runtime(
        &self,
        prepared: PreparedAgentRuntime,
        session: Session,
        branch: Option<phi_sdk::agent_instance::BranchSelection>,
        checkpoint_after_replace: bool,
    ) -> anyhow::Result<RuntimeView> {
        Box::pin(self.replace_runtime_if_generation(
            prepared,
            session,
            branch,
            checkpoint_after_replace,
            None,
        ))
        .await
    }

    pub(super) async fn replace_runtime_if_generation(
        &self,
        prepared: PreparedAgentRuntime,
        mut session: Session,
        branch: Option<phi_sdk::agent_instance::BranchSelection>,
        checkpoint_after_replace: bool,
        expected_generation: Option<&str>,
    ) -> anyhow::Result<RuntimeView> {
        let _transition = self.transition.lock().await;
        if let Some(expected) = expected_generation {
            anyhow::ensure!(
                self.runtime().generation_id() == expected,
                "MCP runtime generation has been replaced"
            );
        }
        let _permissions = self.permission_gate.lock().await;
        if session.id.as_str() != self.instance.id().as_str() {
            anyhow::bail!("replacement Session ID does not match AgentInstance");
        }
        {
            let current = self.session.lock().await;
            anyhow::ensure!(
                session.working_dir == current.session.working_dir
                    && session.permission_allowlist == current.session.permission_allowlist
                    && session.agent_kind == current.session.agent_kind
                    && session.definition == current.session.definition
                    && session.alias == current.session.alias
                    && session.sidechain == current.session.sidechain
                    && session.task_list_id == current.session.task_list_id,
                "replacement cannot change the associated Agent directory, grants, creation identity, delegation origin or storage binding"
            );
            session.children.clone_from(&current.session.children);
            session
                .workflow_run_ids
                .clone_from(&current.session.workflow_run_ids);
        }
        let (prepared_instance, runtime) = prepared.into_parts();
        let mut candidate = prepared_instance.into_runtime_candidate();
        if let Some(selection) = branch {
            candidate = candidate.select_branch(selection);
        }
        runtime.workflow_tool.inherit(&self.runtime().workflow_tool);
        let transcript = self.instance.replace_runtime(candidate).await?;
        session.set_transcript(transcript);
        *self
            .runtime
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = runtime;
        self.session.lock().await.session = session;
        self.runtime_changed.notify_one();
        if checkpoint_after_replace {
            let persistence_error = self
                .save_session()
                .await
                .err()
                .map(|error| error.to_string());
            self.session.lock().await.persistence_error = persistence_error;
        }
        Ok(self.runtime_view_locked().await)
    }

    pub(crate) async fn session(&self) -> Session {
        self.session.lock().await.session.clone()
    }

    pub(crate) async fn record_child_intent(
        &self,
        mut link: ChildSessionLink,
    ) -> anyhow::Result<()> {
        if let Some(snapshot) = self
            .instance
            .subagent_snapshots()
            .await?
            .into_iter()
            .find(|snapshot| snapshot.status.creation.instance_id().as_str() == link.child_agent_id)
        {
            link.parent_tool_call_id = snapshot.status.parent_tool_call_id;
        }
        let _transition = self.transition.lock().await;
        self.ensure_session_open().await?;
        let transcript = self.instance.transcript().await?;
        let mut state = self.session.lock().await;
        anyhow::ensure!(
            link.child_session_id != state.session.id
                && !state
                    .session
                    .children
                    .iter()
                    .any(|child| child.child_session_id == link.child_session_id
                        || child.child_agent_id == link.child_agent_id),
            "child must have a distinct Session and creation identity"
        );
        let mut candidate = state.session.clone();
        candidate.set_transcript(transcript);
        candidate.children.push(link);
        if self.persist {
            crate::session::storage::save_session(&candidate)?;
        }
        state.session = candidate;
        Ok(())
    }

    pub(crate) async fn respond_interaction(
        self: &Arc<Self>,
        request_id: String,
        response: phi_sdk::interaction::InteractionResponse,
    ) -> anyhow::Result<SessionInteractionAnswer> {
        let permissions = match &response {
            phi_sdk::interaction::InteractionResponse::Permission(
                phi_sdk::interaction::PermissionDecision::AllowAlways(_),
            ) => Some(self.permission_gate.clone().lock_owned().await),
            _ => None,
        };
        self.ensure_session_open().await?;
        let agent_session = self.clone();
        tokio::spawn(async move {
            let _permissions = permissions;
            agent_session
                .complete_interaction(request_id, response)
                .await
        })
        .await?
    }

    async fn complete_interaction(
        &self,
        request_id: String,
        response: phi_sdk::interaction::InteractionResponse,
    ) -> anyhow::Result<SessionInteractionAnswer> {
        let answer = self
            .instance
            .respond_interaction(request_id, response)
            .await?;
        let persistence_error = match &answer.allow_always {
            Some(grant) => self
                .persist_permission_allows(std::slice::from_ref(grant))
                .await
                .err()
                .map(|error| error.to_string()),
            None => None,
        };
        Ok(SessionInteractionAnswer {
            #[cfg(test)]
            status: answer.status,
            allow_always: answer.allow_always,
            persistence_error,
        })
    }

    async fn persist_permission_allows(&self, tools: &[(String, String)]) -> anyhow::Result<()> {
        let mut state = self.session.lock().await;
        for (tool, pattern) in tools {
            let grant = PermissionAllowEntry {
                tool: tool.clone(),
                pattern: pattern.clone(),
            };
            if !state.session.permission_allowlist.contains(&grant) {
                state.session.permission_allowlist.push(grant);
            }
        }
        drop(state);
        let result = self.save_session().await;
        self.set_persistence_error(result.as_ref().err().map(ToString::to_string))
            .await;
        self.request_checkpoint();
        result
    }

    #[cfg(test)]
    pub(crate) async fn allow_session_tools(
        &self,
        tools: &[(String, String)],
    ) -> anyhow::Result<()> {
        let _transition = self.transition.lock().await;
        self.ensure_session_open().await?;
        let _permissions = self.permission_gate.lock().await;
        self.runtime().allow_session_tools(tools);
        self.persist_permission_allows(tools).await
    }

    pub(super) fn checkpoint_wakeup(&self) -> Arc<Notify> {
        self.checkpoint_requested.clone()
    }

    pub(super) async fn install_checkpoint_observer(&self, task: tokio::task::JoinHandle<()>) {
        *self.checkpoint_observer.lock().await = Some(task);
    }

    pub(super) async fn checkpoint_guard(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.checkpoint_gate.lock().await
    }

    pub(crate) async fn save_observation(&self) -> SaveObservation {
        self.session.lock().await.save_observation.clone()
    }

    pub(super) async fn begin_save(&self, operation: SaveOperation) -> u64 {
        self.session.lock().await.save_observation.begin(operation)
    }

    pub(super) async fn complete_save(
        &self,
        attempt: u64,
        operation: SaveOperation,
        attempts: u8,
        result: &anyhow::Result<Option<SavedBoundary>>,
    ) {
        let mut state = self.session.lock().await;
        let error = result.as_ref().err().map(ToString::to_string);
        if state.save_observation.complete(SaveCompletion {
            attempt,
            operation,
            attempts,
            saved_boundary: result.as_ref().ok().cloned().flatten(),
            error: error.clone(),
        }) {
            state.persistence_error = error;
        }
    }

    pub(super) async fn set_persistence_error(&self, error: Option<String>) {
        self.session.lock().await.persistence_error = error;
    }

    pub(super) async fn request_session_end(
        &self,
        reason: SessionEndReason,
        related_session: Option<&str>,
    ) {
        self.session
            .lock()
            .await
            .end_context
            .get_or_insert(phi_sdk::lifecycle::SessionEndEvent {
                reason,
                related_session: related_session.map(str::to_owned),
            });
    }

    pub(super) async fn stop_checkpoint_observer(&self) {
        if let Some(task) = self.checkpoint_observer.lock().await.take() {
            task.abort();
        }
    }

    pub(crate) async fn end_and_save_session(
        &self,
        transcript: phi_sdk::transcript::Transcript,
    ) -> anyhow::Result<Session> {
        let _transition = self.transition.lock().await;
        let _permissions = self.permission_gate.lock().await;
        {
            let mut session = self.session.lock().await;
            if session.finalized {
                return Ok(session.session.clone());
            }
            if !session.session_ended {
                session.session_ended = true;
                let context =
                    session
                        .end_context
                        .clone()
                        .unwrap_or(phi_sdk::lifecycle::SessionEndEvent {
                            reason: SessionEndReason::Quit,
                            related_session: None,
                        });
                drop(session);
                let resources = self.runtime().sdk_resources();
                resources
                    .session_ended(context.reason, context.related_session.as_deref())
                    .await;
            }
        }
        let mut session = self.session.lock().await;
        session.session.set_transcript(transcript);
        if self.persist {
            if let Err(error) = crate::session::storage::save_session(&session.session) {
                session.persistence_error = Some(error.to_string());
                return Err(error);
            }
        }
        session.persistence_error = None;
        session.finalized = true;
        Ok(session.session.clone())
    }

    pub(crate) async fn activity_agents(&self) -> anyhow::Result<Vec<SubagentChildLink>> {
        let links = self.subagents().await?;
        let state = self.session.lock().await;
        Ok(activity_links(
            &state.session,
            links,
            &self.historical_children,
        ))
    }

    pub(crate) async fn subagents(&self) -> anyhow::Result<Vec<SubagentChildLink>> {
        let snapshots = self.instance.subagent_snapshots().await?;
        let state = self.session.lock().await;
        let mut links = state
            .session
            .children
            .iter()
            .map(SubagentChildLink::from)
            .collect();
        for snapshot in &snapshots {
            apply_child_status(&mut links, state.session.id.as_str(), &snapshot.status);
        }
        drop(state);
        self.apply_workflow_children(&mut links);
        Ok(links)
    }

    pub(super) async fn checkpoint_history(&self) -> anyhow::Result<Option<Session>> {
        let _transition = self.transition.lock().await;
        let view = self.instance.view().await?;
        let mut state = self.session.lock().await;
        if state.finalized {
            return Ok((self.persist
                && crate::session::storage::session_path(&state.session).exists())
            .then(|| state.session.clone()));
        }
        let mut candidate = state.session.clone();
        candidate.set_transcript(view.transcript);
        if self.persist {
            if let Err(error) = crate::session::storage::save_session(&candidate) {
                state.persistence_error = Some(error.to_string());
                return Err(error);
            }
        }
        state.persistence_error = None;
        state.session = candidate;
        Ok(
            (self.persist && crate::session::storage::session_path(&state.session).exists())
                .then(|| state.session.clone()),
        )
    }

    pub(crate) fn request_checkpoint(&self) {
        self.checkpoint_requested.notify_one();
    }

    pub(super) fn saved_boundary(&self, session: &Session) -> Option<SavedBoundary> {
        (self.persist && crate::session::storage::session_path(session).exists())
            .then(|| SavedBoundary::from_session(session))
    }

    async fn ensure_session_open(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.session.lock().await.session_ended,
            "Session association is closed"
        );
        Ok(())
    }

    async fn save_session(&self) -> anyhow::Result<()> {
        if self.persist {
            let session = self.session.lock().await;
            crate::session::storage::save_session(&session.session)?;
        }
        Ok(())
    }

    pub(super) fn runtime(&self) -> std::sync::RwLockReadGuard<'_, AgentRuntime> {
        self.runtime
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn activity_links(
    session: &Session,
    links: Vec<SubagentChildLink>,
    historical_children: &HashSet<String>,
) -> Vec<SubagentChildLink> {
    let current_agents: HashSet<_> = session
        .children
        .iter()
        .filter(|child| {
            !historical_children.contains(&child.child_agent_id) && child.workflow_run_id.is_none()
        })
        .map(|child| child.child_agent_id.as_str())
        .collect();
    links
        .into_iter()
        .filter(|child| current_agents.contains(child.child_agent_id.as_str()))
        .collect()
}

fn apply_child_status(
    links: &mut Vec<SubagentChildLink>,
    parent_id: &str,
    status: &phi_sdk::agents::SubagentStatus,
) {
    if status.creation.creator_id().as_str() != parent_id {
        return;
    }
    if let Some(link) = links
        .iter_mut()
        .find(|link| link.child_agent_id == status.creation.instance_id().as_str())
    {
        *link = status.into();
    } else if status.admission == SubagentAdmission::Accepted {
        links.push(status.into());
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn activities_keep_current_agents_without_historical_children() {
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        let run_id: phi_sdk::workflow::WorkflowRunId =
            uuid::Uuid::new_v4().to_string().parse().unwrap();
        for (id, owner) in [
            ("historical", None),
            ("ordinary", None),
            ("worker", Some(run_id)),
        ] {
            let mut child: ChildSessionLink = serde_json::from_value(serde_json::json!({
                "child_agent_id": id,
                "child_session_id": id,
                "child_working_dir": "/tmp",
            }))
            .unwrap();
            child.workflow_run_id = owner;
            session.children.push(child);
        }
        let historical_children = HashSet::from(["historical".to_owned()]);
        let mut links: Vec<_> = session
            .children
            .iter()
            .map(SubagentChildLink::from)
            .collect();
        links[1].outcome = crate::session::SubagentOutcome::Finished {
            result: "done".into(),
            is_error: false,
        };
        let activities = activity_links(&session, links, &historical_children);
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].child_agent_id, "ordinary");
        assert_eq!(
            activities[0].outcome,
            crate::session::SubagentOutcome::Finished {
                result: "done".into(),
                is_error: false,
            }
        );
    }

    pub(crate) async fn session_command_allowed(agent: &AgentSession, argument: &str) -> bool {
        let effect = phi_sdk::extension::ExtensionEffect::SpawnProcess {
            extension: "grant-probe".into(),
            source: crate::config::settings::phi_path()
                .join("grant-probe.lua")
                .to_string_lossy()
                .into(),
            context: "Command".into(),
            program: "printf".into(),
            args: vec![argument.into()],
            sandboxed: true,
        };
        agent
            .runtime()
            .probe_effect_permission(&effect)
            .await
            .is_ok()
    }

    pub(crate) enum PermissionReply {
        Attachment,
        Session,
    }

    pub(crate) async fn assert_competing_permission_answers(
        local: &crate::runtime::attachment::AgentAttachment,
        remote: &crate::runtime::attachment::AgentAttachment,
        allow_once: bool,
    ) {
        use phi_sdk::interaction::{InteractionRequest, InteractionResponse, PermissionDecision};
        let pending = local.pending_interactions().await.unwrap().remove(0);
        let permission = match &pending.request {
            InteractionRequest::Permission(permission) => permission,
            _ => panic!("expected permission request"),
        };
        let before = local.session().await.unwrap().permission_allowlist;
        let local_pattern = format!("{} alternative", permission.input);
        let local_decision = if allow_once {
            PermissionDecision::AllowOnce
        } else {
            PermissionDecision::AllowAlways(local_pattern.clone())
        };
        let (direct, wire) = tokio::join!(
            local.respond_interaction(
                &pending.request_id,
                InteractionResponse::Permission(local_decision)
            ),
            remote.respond_interaction(
                &pending.request_id,
                InteractionResponse::Permission(PermissionDecision::AllowAlways(
                    permission.input.clone()
                ))
            ),
        );
        assert_ne!(direct.is_ok(), wire.is_ok(), "exactly one reply must win");
        let winner = direct.or(wire).unwrap();
        let mut expected = before;
        if let Some((tool, pattern)) = winner.allow_always {
            assert_eq!(tool, permission.tool);
            assert!(pattern == permission.input || (!allow_once && pattern == local_pattern));
            expected.push(PermissionAllowEntry { tool, pattern });
        } else {
            assert!(allow_once);
            assert_eq!(winner.status, "permission allowed once");
        }
        assert!(winner.persistence_error.is_none());
        assert_eq!(
            local.session().await.unwrap().permission_allowlist,
            expected
        );
        assert_eq!(
            remote
                .recovery()
                .await
                .unwrap()
                .session
                .permission_allowlist,
            expected
        );
        local.checkpoint().await.unwrap();
        let session = local.session().await.unwrap();
        let path = crate::session::storage::session_path(&session);
        assert_eq!(
            crate::session::storage::load_session(path.to_str().unwrap())
                .unwrap()
                .permission_allowlist,
            expected
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !local
                .pending_interactions()
                .await
                .unwrap()
                .iter()
                .any(|next| next.request_id != pending.request_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("winning permission did not progress SDK execution");
    }

    pub(crate) async fn assert_applied_permission_survives_waiter_loss(
        test_runtime: Arc<crate::runtime::TestRuntime>,
        agent: Arc<AgentSession>,
        reply: PermissionReply,
        stop_after_application: bool,
    ) {
        use phi_sdk::interaction::{InteractionResponse, PermissionDecision};
        let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(pending) = agent.instance().snapshot().pending_interactions.first() {
                    break pending.clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("SDK did not request permission");
        let permission = match &pending.request {
            phi_sdk::interaction::InteractionRequest::Permission(permission) => permission,
            _ => panic!("expected permission request"),
        };
        let grant = PermissionAllowEntry {
            tool: permission.tool.clone(),
            pattern: permission.input.clone(),
        };
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        );
        let first = agent.session.lock().await;
        let waiter = tokio::spawn({
            let request_id = pending.request_id.clone();
            let pattern = grant.pattern.clone();
            let agent = agent.clone();
            async move {
                let response =
                    InteractionResponse::Permission(PermissionDecision::AllowAlways(pattern));
                match reply {
                    PermissionReply::Session => {
                        agent.respond_interaction(request_id, response).await
                    }
                    PermissionReply::Attachment => attachment
                        .respond_interaction(request_id, response)
                        .await
                        .map_err(Into::into),
                }
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while agent.permission_gate.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("answer did not reach Session admission");
        // FIFO places this guard after admission but before grant accounting.
        let accounting = agent.session.lock();
        tokio::pin!(accounting);
        tokio::select! {
            biased;
            _ = &mut accounting => panic!("Session gate was not held"),
            _ = std::future::ready(()) => {}
        }
        drop(first);
        let accounting = accounting.await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !agent
                .instance()
                .snapshot()
                .pending_interactions
                .iter()
                .any(|request| request.request_id != pending.request_id)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("SDK did not execute the authorized tool and request the next permission");
        assert!(!accounting.session.permission_allowlist.contains(&grant));
        let path = crate::session::storage::session_path(&accounting.session);
        assert!(!waiter.is_finished(), "answer completed before accounting");
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        if stop_after_application {
            tokio::time::timeout(std::time::Duration::from_secs(5), agent.instance().stop())
                .await
                .expect("SDK stop waited for Session grant accounting")
                .unwrap();
            assert!(
                test_runtime.sessions.get(agent.instance().id()).is_ok(),
                "association finalized before accounting"
            );
        }
        drop(accounting);
        let _completed = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent.permission_gate.lock(),
        )
        .await
        .expect("retained answer did not complete");
        let saved = crate::session::storage::load_session(path.to_str().unwrap()).unwrap();
        assert_eq!(
            saved
                .permission_allowlist
                .iter()
                .filter(|entry| **entry == grant)
                .count(),
            1
        );
        assert_eq!(
            agent.session().await.permission_allowlist,
            saved.permission_allowlist
        );
        assert!(agent.snapshot().await.persistence_error.is_none());
    }

    async fn memory_runtime(
        root: &std::path::Path,
        hooks: Vec<phi_sdk::hooks::Hook>,
        memory_enabled: bool,
    ) -> (
        Arc<crate::runtime::TestRuntime>,
        Arc<AgentSession>,
        crate::runtime::TestConfiguration,
    ) {
        use crate::runtime::{
            AgentInstanceBuildOptions, AgentRuntimeFactory, PhiBuildOptions, RuntimeRebuildOptions,
            RuntimeResources,
        };
        use clap::Parser;
        let working_dir = root.join("workspace");
        let memory_dir = root.join("memory/workspace/core/project");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(
            memory_dir.join("context.md"),
            "---\ndescription: context\n---\ncontext",
        )
        .unwrap();
        let models = root.join("models.json");
        std::fs::write(&models, br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://127.0.0.1:1/v1","models":{"test":{"contextWindow":128000}}}}}"#).unwrap();
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![models],
                offline: true,
            })
            .unwrap(),
        );
        let resources = RuntimeResources::new_for(
            phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()),
            Arc::new(ExtensionRuntime::load_with_paths(Vec::new())),
            catalog.clone(),
            Sandbox::new(false, working_dir.clone()).unwrap(),
            &working_dir,
        );
        let test_runtime = Arc::new(crate::runtime::TestRuntime::new(AgentRuntimeFactory::new(
            resources,
        )));
        let cli = Arc::new(crate::cli::Cli::parse_from(["phi"]));
        let mut settings = crate::config::Settings::default();
        settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
            enabled: memory_enabled,
            injection: Some(phi_sdk::extension::memory::MemoryInjectionMode::MessageAppend),
            memory_dir: Some(phi_sdk::extension::memory::MemoryDirSettings {
                local_path: Some(root.join("memory").display().to_string()),
                ..Default::default()
            }),
            ..Default::default()
        });
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = working_dir.display().to_string().into();
        let agent = Box::pin(test_runtime.sessions.create(
            &test_runtime.preparation,
            session.clone(),
            RuntimeRebuildOptions {
                phi: PhiBuildOptions {
                    cli: &cli,
                    provider: "local",
                    model: "test",
                    runtime_cfg: settings.runtime_config(&catalog),
                    context: phi_sdk::context::load(true),
                    agent_options: Default::default(),
                    hooks,
                },
                instance: AgentInstanceBuildOptions::from_session(&session),
            },
            SessionStartReason::New,
            false,
        ))
        .await
        .unwrap();
        agent.ready_hook_notices().await;
        let policy = crate::runtime::TestConfiguration::new(
            cli,
            settings,
            Default::default(),
            Vec::new(),
            catalog,
        );
        (test_runtime, agent, policy)
    }

    #[tokio::test]
    async fn hook_warnings_are_available_without_another_submission() {
        use crate::runtime::preparation::tests::session_test_runtime_with_hooks;
        use phi_sdk::hooks::{Hook, HookError};
        use std::time::Duration;

        let release = Arc::new(Notify::new());
        let hook = Hook::provider_context({
            let release = release.clone();
            move |_| {
                let release = release.clone();
                async move {
                    release.notified().await;
                    Err(HookError::new("context observer unavailable"))
                }
            }
        });
        let (root, policy, test_runtime) = session_test_runtime_with_hooks(vec![hook]);
        policy.install(&test_runtime.preparation);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.display().to_string().into();
        let agent = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .start_session(session, false)
        .await
        .unwrap();
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        );
        attachment.ready_hook_notices().await.unwrap();
        let submission = attachment
            .submit(phi_sdk::turn::TurnPrompt {
                prompt: "run".into(),
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        assert!(submission.notices.is_empty());
        release.notify_one();
        let notices = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let notices = test_runtime
                    .sessions
                    .take_notices()
                    .unwrap()
                    .into_iter()
                    .flat_map(|notices| notices.hook_notices)
                    .collect::<Vec<_>>();
                if !notices.is_empty() {
                    break notices;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            matches!(notices.as_slice(), [HookNotice::Warning { message }]
            if message.contains("context observer unavailable"))
        );
        assert!(test_runtime.sessions.take_notices().unwrap().is_empty());
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn context_reset_preserves_session_and_rejects_busy() {
        use crate::runtime::preparation::tests::session_test_runtime_with_hooks;
        use phi_sdk::agent_instance::AgentInstanceError;
        use phi_sdk::hooks::Hook;
        use phi_sdk::lifecycle::ProviderContextOutcome;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;
        let boundaries = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let hooks = vec![
            Hook::session_start({
                let boundaries = boundaries.clone();
                move |_| {
                    boundaries.fetch_add(1, Ordering::Relaxed);
                    async { Ok(Vec::new()) }
                }
            }),
            Hook::session_end({
                let boundaries = boundaries.clone();
                move |_| {
                    boundaries.fetch_add(1, Ordering::Relaxed);
                    async { Ok(Vec::new()) }
                }
            }),
            Hook::provider_context({
                let entered = entered.clone();
                let release = release.clone();
                let requests = requests.clone();
                move |_| {
                    let entered = entered.clone();
                    let release = release.clone();
                    requests.fetch_add(1, Ordering::Relaxed);
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(ProviderContextOutcome::default())
                    }
                }
            }),
        ];
        let (root, policy, test_runtime) = session_test_runtime_with_hooks(hooks);
        policy.install(&test_runtime.preparation);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.display().to_string().into();
        session.transcript.append_user_message("old context");
        session.transcript.total_tokens = 17;
        session.transcript.total_cost = 0.5;
        session.permission_allowlist.push(PermissionAllowEntry {
            tool: "Read".into(),
            pattern: "**".into(),
        });
        let agent = {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(session, true)
        .await
        .unwrap();
        let original = agent.recovery().await.unwrap();
        let boundary_count = boundaries.load(Ordering::Relaxed);
        {
            let attachment = crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            );
            for handoff in [None, Some("handoff")] {
                let before = attachment.recovery().await.unwrap();
                let reset = attachment
                    .reset_context(handoff.map(str::to_owned))
                    .await
                    .unwrap();
                assert_eq!(reset.session.id, before.session.id);
                assert_eq!(reset.session.created_at, before.session.created_at);
                assert_eq!(reset.session.working_dir, before.session.working_dir);
                assert_eq!(
                    reset.session.permission_allowlist,
                    before.session.permission_allowlist
                );
                assert_eq!(reset.session.current_switch, before.session.current_switch);
                assert_eq!(reset.session.total_tokens, before.session.total_tokens);
                assert_eq!(reset.session.total_cost, before.session.total_cost);
                assert_eq!(reset.session.provider, before.session.provider);
                assert_eq!(reset.session.model, before.session.model);
                assert_eq!(
                    reset.snapshot.runtime_generation_id,
                    original.snapshot.runtime_generation_id
                );
                assert_eq!(
                    serde_json::to_value(&reset.declarations).unwrap(),
                    serde_json::to_value(&original.declarations).unwrap()
                );
                assert_eq!(reset.snapshot.instance.status, AgentInstanceStatus::Idle);
                assert!(reset.snapshot.instance.revision > before.snapshot.instance.revision);
                assert_eq!(
                    serde_json::to_value(&reset.session.entries[..before.session.entries.len()])
                        .unwrap(),
                    serde_json::to_value(&before.session.entries).unwrap()
                );
                let marker = reset.session.entries.last().unwrap();
                assert_eq!(marker.parent_id, before.session.leaf_id);
                assert_eq!(marker.head.as_ref(), Some(&marker.id));
                assert_eq!(
                    reset
                        .session
                        .transcript
                        .effective_context()
                        .unwrap()
                        .preamble_text(),
                    handoff
                );
                assert_eq!(boundaries.load(Ordering::Relaxed), boundary_count);
                assert_eq!(requests.load(Ordering::Relaxed), 0);
                test_runtime
                    .sessions
                    .session_access(attachment.id())
                    .checkpoint()
                    .await
                    .unwrap();
                let saved = crate::session::storage::load_session(
                    crate::session::storage::session_path(&reset.session)
                        .to_str()
                        .unwrap(),
                )
                .unwrap();
                assert_eq!(
                    serde_json::to_value(saved.transcript).unwrap(),
                    serde_json::to_value(reset.session.transcript).unwrap()
                );
            }
        }
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        );
        attachment
            .submit(phi_sdk::turn::TurnPrompt {
                prompt: "active request".into(),
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Sdk,
                tool_policy: Default::default(),
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        let before = attachment.recovery().await.unwrap();
        {
            assert!(matches!(
                attachment.reset_context(None).await,
                Err(AgentInstanceError::Busy)
            ));
            let after = attachment.recovery().await.unwrap();
            assert_eq!(
                serde_json::to_value(after.session.transcript).unwrap(),
                serde_json::to_value(&before.session.transcript).unwrap()
            );
            assert_eq!(after.pending_submission_ids, before.pending_submission_ids);
        }
        attachment.abort().await.unwrap();
        attachment.reset_context(None).await.unwrap();
        assert_eq!(requests.load(Ordering::Relaxed), 1);
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn context_reset_retains_checkpoint_after_waiter_loss() {
        use crate::runtime::preparation::tests::session_test_runtime;
        use phi_sdk::agent_instance::AgentInstanceEvent;
        use std::time::Duration;
        for persist in [false, true] {
            for handoff in [None, Some("handoff")] {
                let (root, policy, test_runtime) = session_test_runtime();
                let mut session = crate::session::new(
                    "local",
                    "test",
                    128_000,
                    &std::env::current_dir().unwrap(),
                );
                session.working_dir = root.display().to_string().into();
                let agent = {
                    policy.install(&test_runtime.preparation);
                    crate::runtime::preparation::SessionWorkflows::new(
                        test_runtime.sessions.clone(),
                        test_runtime.preparation.clone(),
                    )
                    .unwrap()
                }
                .start_session(session, persist)
                .await
                .unwrap();
                let mut events = agent.instance().subscribe();
                let checkpoint_attempt = agent.save_observation().await.attempt;
                let mut waiter = Box::pin(agent.reset_context(handoff.map(str::to_owned)));
                assert!(futures_util::poll!(waiter.as_mut()).is_pending());
                drop(waiter);
                let marker = tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if let AgentInstanceEvent::ContextReset { entry_id } =
                            events.recv().await.unwrap()
                        {
                            break entry_id;
                        }
                    }
                })
                .await
                .unwrap();
                tokio::time::timeout(Duration::from_secs(5), async {
                    let _completed = agent.transition.lock().await;
                })
                .await
                .unwrap();
                let session = agent.session().await;
                assert_eq!(session.leaf_id.as_deref(), Some(marker.as_str()));
                assert_eq!(session.entries.len(), 1);
                assert_eq!(
                    session
                        .transcript
                        .effective_context()
                        .unwrap()
                        .preamble_text(),
                    handoff
                );
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let observation = agent.save_observation().await;
                        if observation.pending.is_none()
                            && let Some(completion) = observation.completed
                            && completion.attempt > checkpoint_attempt
                            && completion.error.is_none()
                            && (!persist
                                || completion.saved_boundary.as_ref().is_some_and(|boundary| {
                                    boundary.leaf_id.as_deref() == Some(marker.as_str())
                                }))
                        {
                            assert_eq!(completion.saved_boundary.is_some(), persist);
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let path = crate::session::storage::session_path(&session);
                assert_eq!(path.exists(), persist);
                if persist {
                    let saved =
                        crate::session::storage::load_session(path.to_str().unwrap()).unwrap();
                    assert_eq!(
                        serde_json::to_value(saved.transcript).unwrap(),
                        serde_json::to_value(session.transcript).unwrap()
                    );
                }
                test_runtime
                    .sessions
                    .shutdown(SessionEndReason::Quit)
                    .await
                    .unwrap();
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn context_reset_does_not_rollback_after_checkpoint_failure() {
        use crate::runtime::preparation::tests::session_test_runtime;
        use std::time::Duration;
        let (root, policy, test_runtime) = session_test_runtime();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.display().to_string().into();
        let agent = {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(session, true)
        .await
        .unwrap();
        let id = agent.instance().id().clone();
        test_runtime
            .sessions
            .session_access(&id)
            .checkpoint()
            .await
            .unwrap();
        let checkpoint = agent.checkpoint_guard().await;
        let checkpoint_attempt = agent.save_observation().await.attempt;
        let path = crate::session::storage::session_path(&agent.session().await);
        std::fs::create_dir_all(&path).unwrap();
        let reset = test_runtime
            .sessions
            .session_access(&id)
            .binding()
            .unwrap()
            .reset_context(Some("handoff".into()))
            .await
            .unwrap();
        let expected = serde_json::to_value(&reset.session.transcript).unwrap();
        drop(checkpoint);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let observation = agent.save_observation().await;
                if observation.pending.is_none()
                    && observation.completed.as_ref().is_some_and(|completion| {
                        completion.attempt > checkpoint_attempt && completion.error.is_some()
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let failed = agent.recovery().await.unwrap();
        assert!(failed.snapshot.persistence_error.is_some());
        assert_eq!(
            serde_json::to_value(&failed.session.transcript).unwrap(),
            expected
        );
        assert_eq!(
            failed.snapshot.runtime_generation_id,
            reset.snapshot.runtime_generation_id
        );
        std::fs::remove_dir(&path).unwrap();
        test_runtime
            .sessions
            .session_access(&id)
            .checkpoint()
            .await
            .unwrap();
        let saved = crate::session::storage::load_session(path.to_str().unwrap()).unwrap();
        assert_eq!(serde_json::to_value(saved.transcript).unwrap(), expected);
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn checkpoint_observation_survives_waiter_loss_without_claiming_nonpersistent_durability()
    {
        let root = std::env::temp_dir().join(format!("phi-save-waiter-{}", uuid::Uuid::new_v4()));
        let (test_runtime, agent, _) = memory_runtime(&root, Vec::new(), false).await;
        agent.stop_checkpoint_observer().await;
        test_runtime
            .sessions
            .session_access(agent.instance().id())
            .checkpoint()
            .await
            .unwrap();
        let transition = agent.transition.lock().await;
        let saving_runtime = test_runtime.clone();
        let id = agent.instance().id().clone();
        let waiter = tokio::spawn(async move {
            saving_runtime
                .sessions
                .session_access(&id)
                .checkpoint()
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while agent.save_observation().await.pending.is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let pending = agent.save_observation().await;
        assert!(!pending.persistent);
        assert!(pending.completed.is_some());
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(transition);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while agent.save_observation().await.pending.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let completed = agent.save_observation().await.completed.unwrap();
        assert_eq!(completed.attempt, pending.attempt);
        assert!(completed.error.is_none());
        assert!(completed.saved_boundary.is_none());
        assert!(!crate::session::storage::session_path(&agent.session().await).exists());
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn explicit_memory_injection_rejects_unavailable_context_without_admission() {
        for scenario in ["disabled", "empty", "invalid"] {
            let root =
                std::env::temp_dir().join(format!("phi-memory-reject-{}", uuid::Uuid::new_v4()));
            let (test_runtime, agent, _) =
                Box::pin(memory_runtime(&root, Vec::new(), scenario != "disabled")).await;
            let file = root.join("memory/workspace/core/project/context.md");
            match scenario {
                "empty" => std::fs::remove_file(&file).unwrap(),
                "invalid" => std::fs::write(&file, [0xff]).unwrap(),
                _ => {}
            }
            let before =
                serde_json::to_value(agent.instance().transcript().await.unwrap()).unwrap();
            for command in ["/memory-inject", "/memory-inject inspect"] {
                let error = agent
                    .submit(
                        phi_sdk::turn::TurnPrompt {
                            tool_policy: Default::default(),
                            prompt: command.into(),
                            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                            injected_context: Vec::new(),
                            images: Vec::new(),
                        },
                        None,
                        None,
                    )
                    .await
                    .err()
                    .expect("explicit injection must reject unavailable context");
                let expected = match scenario {
                    "disabled" => "Memory is disabled",
                    "empty" => "No Memory/Tape context available",
                    _ => "Memory context failed",
                };
                assert!(error.to_string().contains(expected), "{error:#}");
                assert_eq!(
                    serde_json::to_value(agent.instance().transcript().await.unwrap()).unwrap(),
                    before
                );
            }
            test_runtime
                .sessions
                .shutdown(SessionEndReason::Quit)
                .await
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn non_grant_replies_bypass_permission_and_lifecycle_gates() {
        let root = std::env::temp_dir().join(format!(
            "phi-permission-transition-{}",
            uuid::Uuid::new_v4()
        ));
        let (test_runtime, agent, _) = Box::pin(memory_runtime(&root, Vec::new(), true)).await;
        let transition = agent.transition.lock().await;
        let permissions = agent.permission_gate.lock().await;
        let responses = vec![
            phi_sdk::interaction::InteractionResponse::Permission(
                phi_sdk::interaction::PermissionDecision::AllowOnce,
            ),
            phi_sdk::interaction::InteractionResponse::Permission(
                phi_sdk::interaction::PermissionDecision::Deny,
            ),
            phi_sdk::interaction::InteractionResponse::McpElicitation(
                phi_sdk::extension::mcp::McpElicitationResponse::Decline,
            ),
        ];
        for response in responses {
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                agent.respond_interaction("missing".into(), response),
            )
            .await
            .expect("non-grant reply waited for an application gate");
            assert!(result.is_err());
        }
        assert!(agent.session().await.permission_allowlist.is_empty());
        drop(permissions);
        drop(transition);
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn anchor_jump_survives_response_waiter_loss_after_transition_admission() {
        use crate::runtime::preparation::tests::session_test_runtime_with_hooks;
        use phi_sdk::agent_instance::AgentInstanceId;
        let reached = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered = reached.clone();
        let resume = release.clone();
        let hooks = vec![phi_sdk::hooks::Hook::session_end(move |event| {
            let entered = entered.clone();
            let resume = resume.clone();
            async move {
                if matches!(event.reason, SessionEndReason::Resume) {
                    assert!(event.related_session.is_none());
                    entered.notify_one();
                    resume.notified().await;
                }
                Ok(Vec::new())
            }
        })];
        let (root, policy, test_runtime) = session_test_runtime_with_hooks(hooks);
        policy.install(&test_runtime.preparation);
        let mut current =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        current.working_dir = root.to_string_lossy().into();
        let outgoing = Box::pin(
            {
                policy.install(&test_runtime.preparation);
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
            }
            .start_session(current, false),
        )
        .await
        .unwrap();
        let current_id = outgoing.instance().id().clone();
        let mut target =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        target.working_dir = root.to_string_lossy().into();
        target.transcript.append_user_message("anchor");
        let anchor = target.leaf_id.clone().unwrap();
        target.transcript.append_assistant_message(
            None,
            None,
            vec![crate::session::ContentBlock::Text {
                text: "destination".into(),
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "stop".into(),
        );
        let destination = target.leaf_id.clone();
        target.transcript.append_user_message("later");
        let saved_leaf = target.leaf_id.clone();
        crate::session::storage::save_session(&target).unwrap();
        let target_id = AgentInstanceId::new(target.id.as_str());
        let workflows = {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        };
        let outgoing_id = current_id.clone();
        let incoming_id = target_id.clone();
        let waiter = tokio::spawn(async move {
            workflows
                .jump_anchor(&outgoing_id, incoming_id.as_str(), &anchor)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), reached.notified())
            .await
            .unwrap();
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        release.notify_one();
        let incoming = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(agent) = test_runtime.sessions.get(&target_id)
                    && agent.recovery().await.unwrap().session.leaf_id == destination
                {
                    break agent;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            incoming.recovery().await.unwrap().session.leaf_id,
            destination
        );
        assert!(test_runtime.sessions.get(&current_id).is_err());
        assert_eq!(
            crate::session::storage::load_session_id(target_id.as_str())
                .unwrap()
                .leaf_id,
            saved_leaf
        );
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn branch_navigation_survives_response_waiter_loss() {
        use crate::runtime::preparation::{BranchNavigation, tests::session_test_runtime};
        let (root, policy, test_runtime) = session_test_runtime();
        policy.install(&test_runtime.preparation);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        session.transcript.append_user_message("restore draft");
        let target = session.leaf_id.clone().unwrap();
        session.transcript.append_assistant_message(
            None,
            None,
            Vec::new(),
            "test".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "local".into(),
            Default::default(),
            "stop".into(),
        );
        let agent = Box::pin(
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
        let id = agent.instance().id().clone();
        let before = agent.snapshot().await.runtime_generation_id;
        let permissions = agent.permission_gate.lock().await;
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(&id),
            test_runtime.preparation.clone(),
        );
        let waiter = tokio::spawn(async move {
            attachment
                .navigate_branch(BranchNavigation::Rewind {
                    entry_id: target.to_string(),
                })
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while agent.transition.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("navigation did not reach Session commit");
        assert_eq!(agent.snapshot().await.runtime_generation_id, before);
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        tokio::task::yield_now().await;
        drop(permissions);
        let selected = tokio::time::timeout(std::time::Duration::from_secs(5), agent.recovery())
            .await
            .unwrap()
            .unwrap();
        assert!(selected.session.leaf_id.is_none());
        assert_ne!(selected.snapshot.runtime_generation_id, before);
        let saved = crate::session::storage::load_session(
            crate::session::storage::session_path(&selected.session)
                .to_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            saved.leaf_id,
            selected
                .session
                .entries
                .last()
                .map(|entry| entry.id.clone())
        );
        assert_eq!(saved.entries.len(), selected.session.entries.len());
        test_runtime
            .sessions
            .stop(&id, SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn permission_waiter_cancels_before_admission() {
        use phi_sdk::interaction::{InteractionResponse, PermissionDecision};
        let root =
            std::env::temp_dir().join(format!("phi-permission-control-{}", uuid::Uuid::new_v4()));
        let (test_runtime, agent, _) = Box::pin(memory_runtime(&root, Vec::new(), true)).await;
        let permissions = agent.permission_gate.lock().await;
        let waiter = tokio::spawn({
            let agent = agent.clone();
            async move {
                agent
                    .respond_interaction(
                        "missing".into(),
                        InteractionResponse::Permission(PermissionDecision::AllowAlways(
                            "gate-probe".into(),
                        )),
                    )
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), agent.instance().abort())
            .await
            .unwrap()
            .unwrap();
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        drop(permissions);
        assert!(agent.session().await.permission_allowlist.is_empty());
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancelled_preparation_never_admits_input() {
        let root =
            std::env::temp_dir().join(format!("phi-readiness-controls-{}", uuid::Uuid::new_v4()));
        let release = Arc::new(Notify::new());
        let hook = phi_sdk::hooks::Hook::session_start_with_mode(
            phi_sdk::hooks::SessionStartMode::BeforeFirstPrompt,
            {
                let release = release.clone();
                move |event| {
                    let release = release.clone();
                    async move {
                        if matches!(event.reason, SessionStartReason::Reload) {
                            release.notified().await;
                        }
                        Ok(Vec::new())
                    }
                }
            },
        );
        let (test_runtime, agent, _) = Box::pin(memory_runtime(&root, vec![hook], true)).await;
        for dispatch in [None, Some(phi_sdk::turn::SubmissionDispatch::FollowUp)] {
            agent
                .session_started(SessionStartReason::Reload, None)
                .await;
            let waiter = tokio::spawn({
                let agent = agent.clone();
                async move {
                    agent
                        .submit(
                            phi_sdk::turn::TurnPrompt {
                                prompt: "waiting".into(),
                                source: phi_sdk::lifecycle::UserPromptSubmitSource::Rpc,
                                injected_context: Vec::new(),
                                images: Vec::new(),
                                tool_policy: Default::default(),
                            },
                            None,
                            dispatch,
                        )
                        .await
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while agent.transition.try_lock().is_ok() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("submission did not reach readiness");
            tokio::time::timeout(std::time::Duration::from_secs(1), agent.instance().abort())
                .await
                .unwrap()
                .unwrap();
            waiter.abort();
            assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
            release.notify_one();
            agent.ready_hook_notices().await;
            assert!(
                !agent
                    .instance()
                    .transcript()
                    .await
                    .unwrap()
                    .has_user_message()
            );
        }
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancelled_submission_waiter_does_not_lose_admitted_memory_accounting() {
        for dispatch in [None, Some(phi_sdk::turn::SubmissionDispatch::FollowUp)] {
            Box::pin(assert_cancelled_admission_accounting(dispatch)).await;
        }
    }

    async fn assert_cancelled_admission_accounting(
        dispatch: Option<phi_sdk::turn::SubmissionDispatch>,
    ) {
        let root =
            std::env::temp_dir().join(format!("phi-admission-cancel-{}", uuid::Uuid::new_v4()));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let hooks = vec![phi_sdk::hooks::Hook::user_prompt_submit({
            let entered = entered.clone();
            let release = release.clone();
            move |event| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    if event.prompt == "blocked admission" {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Continue {
                        notices: Vec::new(),
                        prompt: event.prompt,
                        injected_context: Vec::new(),
                    })
                }
            }
        })];
        let (test_runtime, agent, _policy) = Box::pin(memory_runtime(&root, hooks, true)).await;
        let prompt = phi_sdk::turn::TurnPrompt {
            tool_policy: phi_sdk::turn::TurnToolPolicy {
                allowances: vec![("Read".into(), "**".into())],
                removals: vec!["Edit".into()],
            },
            prompt: "blocked admission".into(),
            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
            injected_context: Vec::new(),
            images: Vec::new(),
        };
        let waiter = tokio::spawn({
            let agent = agent.clone();
            let prompt = prompt.clone();
            async move { agent.submit(prompt, None, dispatch).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        release.notify_one();
        agent.ready_hook_notices().await;
        assert!(
            agent
                .instance()
                .transcript()
                .await
                .unwrap()
                .entries
                .iter()
                .any(|entry| matches!(
                    &entry.kind,
                    phi_sdk::transcript::TranscriptEntryKind::Message {
                        message: phi_sdk::transcript::AgentMessage::Context { source, .. }
                    } if phi_sdk::extension::memory::MemoryContextKind::from_source(source).is_some()
                )),
            "accepted input lost its Memory contribution"
        );
        assert!(agent.session().await.permission_allowlist.is_empty());
        agent.instance().abort().await.unwrap();
        let receipt = agent
            .submit(
                phi_sdk::turn::TurnPrompt {
                    prompt: "next".into(),
                    tool_policy: Default::default(),
                    ..prompt
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert!(
            receipt
                .notices
                .iter()
                .all(|notice| !matches!(notice, HookNotice::ContextInjected { .. }))
        );
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

struct CommandIdleSignal(tokio::task::JoinHandle<()>);

impl Drop for CommandIdleSignal {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct CommandPromptSubmitter {
    agent_session: std::sync::Weak<AgentSession>,
    generation: String,
}

impl phi_sdk::extension::ExtensionPromptSubmitter for CommandPromptSubmitter {
    fn submit(
        &self,
        prompt: String,
        dispatch: Option<phi_sdk::turn::SubmissionDispatch>,
    ) -> phi_sdk::extension::ExtensionSubmissionFuture {
        let agent_session = self.agent_session.upgrade();
        let generation = self.generation.clone();
        Box::pin(async move {
            let agent_session =
                agent_session.ok_or_else(|| anyhow::anyhow!("Agent is unavailable"))?;
            let _transition = agent_session.transition.lock().await;
            agent_session.ensure_session_open().await?;
            anyhow::ensure!(
                agent_session.runtime().generation_id() == generation,
                "command runtime has been replaced"
            );
            let prompt = phi_sdk::turn::TurnPrompt {
                tool_policy: Default::default(),
                prompt,
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Extension,
                injected_context: Vec::new(),
                images: Vec::new(),
            };
            Ok(match dispatch {
                Some(dispatch) => agent_session.instance.dispatch(prompt, dispatch).await?,
                None => agent_session.instance.submit(prompt).await?,
            })
        })
    }
}

fn instance_is_idle(status: AgentInstanceStatus) -> bool {
    matches!(
        status,
        AgentInstanceStatus::Idle | AgentInstanceStatus::Failed
    )
}

async fn apply_extension_effects(
    extensions: &ExtensionRuntime,
    effects: Vec<ExtensionEffect>,
    sandbox: &Sandbox,
) -> anyhow::Result<()> {
    for effect in effects {
        extensions.authorize_effect(&effect).await?;
        let ExtensionEffect::SpawnProcess {
            extension,
            source,
            program,
            args,
            ..
        } = effect;
        let mut child = sandbox
            .wrap_program(&program, &args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| {
                anyhow::anyhow!(
                    "extension '{extension}' ({source}) failed to spawn '{program}': {error}"
                )
            })?;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
    Ok(())
}

const MAX_SESSION_WORKFLOWS: usize = 64;
const MODEL_WORKFLOW_WORKERS: usize = 4;
const MODEL_WORKFLOW_PENDING: usize = 32;
const MODEL_WORKFLOW_INVOCATIONS: usize = 128;
const MODEL_WORKFLOW_BYTES: usize = 1024 * 1024;
const WORKFLOW_TOOL_NAME: &str = "Workflow";
const WORKFLOW_INTRO: &str = concat!(
    "Workflow has been explicitly enabled. ",
    "Prefer Workflow for complex, parallel or multistage tasks; handle simple tasks directly. ",
    "Tool restrictions and permission checks still apply."
);

#[derive(Clone, Default)]
pub(in crate::runtime) struct WorkflowTool {
    owner: Arc<std::sync::OnceLock<WorkflowToolOwner>>,
}

#[derive(Clone)]
struct WorkflowToolOwner {
    session: std::sync::Weak<AgentSession>,
    runtime: std::sync::Weak<WorkflowRuntime>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowToolArgs {
    source: String,
    #[serde(default, deserialize_with = "capture_workflow_args")]
    args: Option<serde_json::Value>,
}

fn capture_workflow_args<'de, D>(deserializer: D) -> Result<Option<serde_json::Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}

impl WorkflowTool {
    pub(in crate::runtime) fn bind(
        &self,
        session: &Arc<AgentSession>,
        runtime: &Arc<WorkflowRuntime>,
    ) {
        let _ = self.owner.set(WorkflowToolOwner {
            session: Arc::downgrade(session),
            runtime: Arc::downgrade(runtime),
        });
    }

    pub(in crate::runtime) fn inherit(&self, previous: &Self) {
        if let Some(owner) = previous.owner.get() {
            let _ = self.owner.set(owner.clone());
        }
    }

    pub(in crate::runtime) fn declaration(
        &self,
        generation: String,
    ) -> phi_sdk::tools::ToolDeclaration {
        let binding = self.clone();
        let tool = phi_sdk::tools::PortableDynamicTool::new(
            WORKFLOW_TOOL_NAME,
            concat!(
                "Launch a bounded JavaScript workflow in the background. The first statement must be ",
                "export const meta = { name, description, phases? } with literal values. ",
                "Use await agent(prompt, { role, label? }), parallel(deferredFunctions), ",
                "pipeline(items, ...stages), phase(title), log(value), and return a final report. ",
                "Every agent call requires an existing role. Only role and label are supported. ",
                "args is captured JSON input, or undefined when omitted. No files, shell, network, ",
                "imports or ordinary tools are available to the script. Workers use governed tools. ",
                "Limits: 4 concurrent workers, 32 pending calls, 128 total calls, ",
                "1 MiB cumulative worker results and 1 MiB output. ",
                "The script has a shared 300-second deadline, including worker and permission waits. ",
                "Returns a run_id receipt, not execution success. One terminal report arrives later; ",
                "intermediate results remain private to the workflow. Do not poll or sleep to wait."
            ),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "Literal metadata followed by an async JavaScript body" },
                    "args": { "description": "Optional captured JSON input" }
                },
                "required": ["source"],
                "additionalProperties": false
            }),
            move |input| {
                let binding = binding.clone();
                let generation = generation.clone();
                Box::pin(async move {
                    binding.launch(input, &generation).await.map_err(|error| {
                        phi_sdk::tools::ToolExecutionError::provider(error.to_string())
                    })
                })
            },
        );
        phi_sdk::tools::ToolDeclaration::dynamic(tool)
            .sequential()
            .terminate_on_success()
            .prompt_snippet("Launch bounded background JavaScript orchestration with isolated Agent results")
            .prompt_guidelines(["Use Workflow for complex, parallel or multistage tasks; handle simple tasks directly. A launch receipt is not a completed result."])
    }

    async fn launch(
        &self,
        input: serde_json::Value,
        generation: &str,
    ) -> anyhow::Result<phi_sdk::tools::ToolOutput> {
        let args: WorkflowToolArgs = serde_json::from_value(input)?;
        let owner = self
            .owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("Workflow Session is not associated"))?;
        let session = owner
            .session
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Workflow Session is closed"))?;
        let runtime = owner
            .runtime
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("Workflow runtime is closed"))?;
        let request = WorkflowRequest::parse(&args.source, args.args).await?;
        let limits = WorkflowLimits::new(
            MODEL_WORKFLOW_WORKERS,
            MODEL_WORKFLOW_PENDING,
            MODEL_WORKFLOW_INVOCATIONS,
            MODEL_WORKFLOW_BYTES,
            MODEL_WORKFLOW_BYTES,
        )?;
        let run = session
            .launch_workflow(runtime, request, None, limits, Some(generation))
            .await?;
        Ok(phi_sdk::tools::ToolOutput::text(
            serde_json::json!({
                "run_id": run.id(),
                "status": "accepted"
            })
            .to_string(),
        ))
    }
}

pub(super) enum WorkflowAdmission {
    Open,
    Closed,
}

#[derive(Clone)]
pub(super) struct WorkflowDelivery {
    run: WorkflowRun,
    storage: Option<WorkflowStorage>,
    completed: tokio::sync::watch::Sender<WorkflowCapture>,
    retry: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorkflowView {
    pub(crate) name: String,
    pub(crate) snapshot: WorkflowObservation,
    pub(crate) capture: WorkflowCapture,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowCapture {
    Pending,
    Complete,
    Failed(String),
}

fn save_terminal(
    run: &WorkflowRun,
    storage: Option<&WorkflowStorage>,
    feedback: WorkflowFeedback,
    children: &[crate::session::ChildSessionLink],
) -> anyhow::Result<()> {
    let snapshot = run.snapshot();
    anyhow::ensure!(settled(snapshot.status()), "Workflow has not settled");
    if let Some(storage) = storage {
        storage.save(&WorkflowRecord::new(
            run.id(),
            snapshot.creator().as_str(),
            run.request(),
            WorkflowState::Settled {
                execution_status: snapshot.status(),
                value: snapshot.value().cloned(),
                error: snapshot.error().map(ToString::to_string),
                cleanup_errors: snapshot.cleanup_errors().to_vec(),
                phase: snapshot.phase().map(str::to_owned),
                logs: snapshot.logs().to_vec(),
                invocations: snapshot
                    .invocations()
                    .iter()
                    .map(|invocation| {
                        let worker = invocation
                            .worker()
                            .map(|worker| worker.instance_id().as_str());
                        WorkflowInvocation {
                            id: invocation.id(),
                            role: invocation.role().to_owned(),
                            prompt: invocation.prompt().to_owned(),
                            label: invocation.label().map(ToOwned::to_owned),
                            phase: invocation.phase().map(ToOwned::to_owned),
                            status: invocation.status(),
                            worker_instance_id: worker.map(ToOwned::to_owned),
                            worker_session: children
                                .iter()
                                .find(|link| {
                                    Some(link.child_agent_id.as_str()) == worker
                                        && link.child_session_id.as_str() == link.child_agent_id
                                })
                                .cloned(),
                            value: invocation.value().map(ToOwned::to_owned),
                            error: invocation.error().map(ToString::to_string),
                        }
                    })
                    .collect(),
                feedback,
            },
        ))?;
    }
    Ok(())
}

async fn deliver_terminal(
    run: &WorkflowRun,
    storage: Option<&WorkflowStorage>,
    session: &AgentSession,
) -> anyhow::Result<()> {
    let children = session.session().await.children;
    let creator = &session.instance;
    save_terminal(run, storage, WorkflowFeedback::Pending, &children)?;
    let feedback = match run.deliver_to(creator).await {
        Ok(()) => WorkflowFeedback::Admitted,
        Err(error)
            if creator.snapshot().status == AgentInstanceStatus::Stopped
                && matches!(
                    error.as_ref(),
                    phi_sdk::workflow::WorkflowError::Owner(
                        AgentInstanceError::Stopped | AgentInstanceError::Unavailable
                    )
                ) =>
        {
            WorkflowFeedback::CreatorClosed
        }
        Err(error) => return Err(error.into()),
    };
    save_terminal(run, storage, feedback, &children)
}

fn settled(status: WorkflowStatus) -> bool {
    matches!(
        status,
        WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Stopped
    )
}

impl AgentSession {
    pub(super) fn inject_workflow_context(
        &self,
        prompt: &mut phi_sdk::turn::TurnPrompt,
    ) -> anyhow::Result<()> {
        let task = crate::commands::workflow_prompt(&prompt.prompt);
        let runtime = self.runtime();
        let removed = prompt
            .tool_policy
            .removals
            .iter()
            .any(|tool| tool == WORKFLOW_TOOL_NAME);
        if let Some(task) = task {
            anyhow::ensure!(!task.is_empty(), "Usage: /workflow <task>");
            anyhow::ensure!(
                runtime
                    .source_settings()
                    .is_some_and(|config| config.agent.workflow == Some(true))
                    && runtime.workflow_enabled()
                    && !removed
                    && self.creator_id.is_none(),
                "Workflow is unavailable; /workflow requires agent.workflow: true in settings and an enabled Workflow tool"
            );
            prompt.prompt = task.to_string();
        } else if self.creator_id.is_some() || !runtime.workflow_requested() || removed {
            return Ok(());
        }
        prompt
            .injected_context
            .push(phi_sdk::turn::InjectedContext {
                source: phi_sdk::transcript::ContextSource::WorkflowIntro,
                content: WORKFLOW_INTRO.to_string(),
            });
        Ok(())
    }

    pub(super) fn apply_workflow_children(&self, children: &mut [SubagentChildLink]) {
        let deliveries = self
            .workflow_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for delivery in deliveries.iter() {
            let snapshot = delivery.run.snapshot();
            if snapshot.creator() != self.instance.id()
                || Some(snapshot.registration()) != self.instance.registration_id()
            {
                continue;
            }
            for invocation in snapshot.invocations() {
                let Some(worker) = invocation.worker() else {
                    continue;
                };
                let Some(child) = children.iter_mut().find(|child| {
                    child.child_agent_id == worker.instance_id().as_str()
                        && child.child_session_id.as_str() == worker.instance_id().as_str()
                }) else {
                    continue;
                };
                child.outcome = match (invocation.value(), invocation.error()) {
                    (Some(value), _) => SubagentOutcome::Finished {
                        result: value.to_owned(),
                        is_error: false,
                    },
                    (_, Some(phi_sdk::workflow::WorkflowError::Worker(failure))) => {
                        SubagentOutcome::from(&Err(failure.clone()))
                    }
                    (_, Some(phi_sdk::workflow::WorkflowError::Stopped)) => {
                        SubagentOutcome::Cancelled
                    }
                    (_, Some(error)) => SubagentOutcome::Finished {
                        result: error.to_string(),
                        is_error: true,
                    },
                    _ => SubagentOutcome::Running,
                };
            }
        }
    }

    pub(in crate::runtime) fn workflow_run_ids(&self) -> Vec<WorkflowRunId> {
        self.workflow_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|delivery| delivery.run.id())
            .collect()
    }

    fn ensure_workflow_owner(&self, registration: AgentRegistrationId) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.instance.registration_id() == Some(registration),
            "Workflow creator registration has changed"
        );
        Ok(())
    }

    pub(crate) fn workflow_changed(&self) -> tokio::sync::futures::OwnedNotified {
        self.workflow_changed.clone().notified_owned()
    }

    pub(crate) fn workflow_views(
        &self,
        registration: AgentRegistrationId,
    ) -> anyhow::Result<Vec<WorkflowView>> {
        self.ensure_workflow_owner(registration)?;
        let deliveries = self
            .workflow_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(deliveries
            .iter()
            .filter_map(|delivery| {
                let snapshot = delivery.run.snapshot();
                if snapshot.registration() != registration
                    || snapshot.creator() != self.instance.id()
                {
                    return None;
                }
                let capture = delivery.completed.borrow().clone();
                Some(WorkflowView {
                    name: delivery.run.request().name().to_owned(),
                    snapshot: snapshot.into(),
                    capture,
                })
            })
            .collect())
    }

    fn owned_workflow(
        &self,
        registration: AgentRegistrationId,
        id: WorkflowRunId,
    ) -> anyhow::Result<WorkflowRun> {
        self.ensure_workflow_owner(registration)?;
        self.workflow_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|delivery| {
                let snapshot = delivery.run.snapshot();
                snapshot.id() == id
                    && snapshot.registration() == registration
                    && snapshot.creator() == self.instance.id()
            })
            .map(|delivery| delivery.run.clone())
            .ok_or_else(|| anyhow::anyhow!("unknown Workflow `{id}` for this Session registration"))
    }

    pub(crate) async fn stop_workflow(
        &self,
        registration: AgentRegistrationId,
        id: WorkflowRunId,
    ) -> anyhow::Result<WorkflowSnapshot> {
        let run = self.owned_workflow(registration, id)?;
        // Accepted cleanup outlives its response waiter.
        tokio::spawn(async move { run.stop().await })
            .await?
            .map_err(Into::into)
    }

    pub(crate) async fn launch_workflow(
        self: &Arc<Self>,
        runtime: Arc<WorkflowRuntime>,
        request: WorkflowRequest,
        default_role: Option<String>,
        limits: WorkflowLimits,
        expected_generation: Option<&str>,
    ) -> anyhow::Result<WorkflowRun> {
        let admission = self.workflow_admission.clone().lock_owned().await;
        anyhow::ensure!(
            matches!(*admission, WorkflowAdmission::Open),
            "Workflow admission is closed"
        );
        let transition = self.transition.clone().lock_owned().await;
        if let Some(expected) = expected_generation {
            anyhow::ensure!(
                self.runtime().generation_id() == expected,
                "Workflow runtime generation has been replaced"
            );
        }
        let agent_session = self.clone();
        tokio::spawn(async move {
            let _admission = admission;
            let _transition = transition;
            agent_session.ensure_session_open().await?;
            let session = agent_session.session().await;
            anyhow::ensure!(
                session.workflow_run_ids.len() < MAX_SESSION_WORKFLOWS,
                "Session Workflow retention limit reached"
            );
            let launch = runtime
                .prepare(request, &agent_session.instance, default_role, limits)
                .await?;
            let _permissions = agent_session.permission_gate.lock().await;
            let view = agent_session.instance.view().await?;
            let storage = agent_session
                .persist
                .then(|| WorkflowStorage::new(&session, launch.id()))
                .transpose()?;
            if let Some(storage) = &storage {
                storage.save(&WorkflowRecord::new(
                    launch.id(),
                    agent_session.instance.id().as_str(),
                    launch.request(),
                    WorkflowState::Prepared,
                ))?;
            }
            let mut candidate = agent_session.session().await;
            candidate.workflow_run_ids.push(launch.id());
            candidate.set_transcript(view.transcript);
            if agent_session.persist {
                if let Err(error) = crate::session::storage::save_session(&candidate) {
                    if let Some(storage) = &storage {
                        storage.save(&WorkflowRecord::new(
                            launch.id(),
                            agent_session.instance.id().as_str(),
                            launch.request(),
                            WorkflowState::LaunchFailed {
                                error: error.to_string(),
                            },
                        ))?;
                    }
                    let mut state = agent_session.session.lock().await;
                    state.session = candidate;
                    state.persistence_error = Some(error.to_string());
                    return Err(error);
                }
            }
            agent_session.session.lock().await.session = candidate;
            let id = launch.id();
            let request = launch.request().clone();
            let result = launch.start(&runtime, {
                let agent_session = agent_session.clone();
                let storage = storage.clone();
                move |run| {
                    let (completed, _) = tokio::sync::watch::channel(WorkflowCapture::Pending);
                    agent_session
                        .workflow_deliveries
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(WorkflowDelivery {
                            run: run.clone(),
                            storage: storage.clone(),
                            completed: completed.clone(),
                            retry: Arc::new(tokio::sync::Mutex::new(())),
                        });
                    agent_session.workflow_changed.notify_waiters();
                    tokio::spawn(async move {
                        let mut subscription = run.subscribe();
                        let result = std::panic::AssertUnwindSafe(async {
                            loop {
                                agent_session.workflow_changed.notify_waiters();
                                if settled(subscription.snapshot().status()) {
                                    break deliver_terminal(&run, storage.as_ref(), &agent_session)
                                        .await;
                                }
                                if let Err(error) = subscription.changed().await {
                                    break Err(anyhow::anyhow!(
                                        "Workflow observation closed before settlement: {error}"
                                    ));
                                }
                            }
                        })
                        .catch_unwind()
                        .await
                        .unwrap_or_else(|_| {
                            Err(anyhow::anyhow!("Workflow terminal capture was abandoned"))
                        });
                        if result.is_ok() {
                            agent_session.request_checkpoint();
                        }
                        if let Err(error) = &result {
                            agent_session
                                .set_persistence_error(Some(error.to_string()))
                                .await;
                        }
                        completed.send_replace(match result {
                            Ok(()) => WorkflowCapture::Complete,
                            Err(error) => WorkflowCapture::Failed(error.to_string()),
                        });
                        agent_session.workflow_changed.notify_waiters();
                    });
                }
            });
            match result {
                Ok(run) => Ok(run),
                Err(error) => {
                    if let Some(storage) = &storage {
                        storage.save(&WorkflowRecord::new(
                            id,
                            agent_session.instance.id().as_str(),
                            &request,
                            WorkflowState::LaunchFailed {
                                error: error.to_string(),
                            },
                        ))?;
                    }
                    Err(error.into())
                }
            }
        })
        .await?
    }

    pub(in crate::runtime) async fn close_workflow_admission(&self) {
        *self.workflow_admission.lock().await = WorkflowAdmission::Closed;
    }

    pub(in crate::runtime) async fn settle_workflow_deliveries(&self) -> anyhow::Result<()> {
        let deliveries = self
            .workflow_deliveries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        for delivery in deliveries {
            let mut completed = delivery.completed.subscribe();
            while matches!(*completed.borrow(), WorkflowCapture::Pending) {
                completed.changed().await.map_err(|error| {
                    anyhow::anyhow!("Workflow terminal capture was abandoned: {error}")
                })?;
            }
            let _retry = delivery.retry.lock().await;
            if matches!(*completed.borrow(), WorkflowCapture::Failed(_)) {
                let result = deliver_terminal(&delivery.run, delivery.storage.as_ref(), self).await;
                delivery.completed.send_replace(match &result {
                    Ok(()) => WorkflowCapture::Complete,
                    Err(error) => WorkflowCapture::Failed(error.to_string()),
                });
                self.workflow_changed.notify_waiters();
                result?;
                self.request_checkpoint();
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct WorkflowObservation {
    id: WorkflowRunId,
    status: WorkflowStatus,
    active: usize,
    pending: usize,
    completed: usize,
    invocations: Vec<InvocationObservation>,
    cleanup_errors: Vec<String>,
    phase: Option<String>,
    error: Option<String>,
    value: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InvocationObservation {
    worker: Option<WorkerObservation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerObservation {
    instance_id: AgentInstanceId,
    registration: Option<AgentRegistrationId>,
}

impl WorkerObservation {
    pub(crate) fn registration(&self) -> Option<AgentRegistrationId> {
        self.registration
    }
    pub(crate) fn instance_id(&self) -> &AgentInstanceId {
        &self.instance_id
    }
}

impl InvocationObservation {
    pub(crate) fn worker(&self) -> Option<&WorkerObservation> {
        self.worker.as_ref()
    }
}

impl WorkflowObservation {
    pub(crate) fn value(&self) -> Option<&serde_json::Value> {
        self.value.as_ref()
    }
    pub(crate) fn id(&self) -> WorkflowRunId {
        self.id
    }
    pub(crate) fn status(&self) -> WorkflowStatus {
        self.status
    }
    pub(crate) fn active(&self) -> usize {
        self.active
    }
    pub(crate) fn pending(&self) -> usize {
        self.pending
    }
    pub(crate) fn completed(&self) -> usize {
        self.completed
    }
    pub(crate) fn invocations(&self) -> &[InvocationObservation] {
        &self.invocations
    }
    pub(crate) fn phase(&self) -> Option<&str> {
        self.phase.as_deref()
    }
    pub(crate) fn cleanup_errors(&self) -> &[String] {
        &self.cleanup_errors
    }
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

impl From<phi_sdk::workflow::WorkflowSnapshot> for WorkflowObservation {
    fn from(snapshot: phi_sdk::workflow::WorkflowSnapshot) -> Self {
        Self {
            id: snapshot.id(),
            status: snapshot.status(),
            active: snapshot.active(),
            pending: snapshot.pending(),
            completed: snapshot.completed(),
            invocations: snapshot
                .invocations()
                .iter()
                .map(|item| InvocationObservation {
                    worker: item.worker().map(|worker| WorkerObservation {
                        instance_id: worker.instance_id().clone(),
                        registration: worker.registration(),
                    }),
                })
                .collect(),
            cleanup_errors: snapshot.cleanup_errors().to_vec(),
            phase: snapshot.phase().map(str::to_owned),
            error: snapshot.error().map(ToString::to_string),
            value: snapshot.value().cloned(),
        }
    }
}

#[cfg(test)]
mod workflow_tests {
    use super::*;

    #[tokio::test]
    async fn conflicting_history_keeps_ordinary_child_history() {
        let (root, policy, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&runtime.preparation);
        let sessions = crate::runtime::preparation::SessionWorkflows::new(
            runtime.sessions.clone(),
            runtime.preparation.clone(),
        )
        .unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let child: crate::session::ChildSessionLink = serde_json::from_value(serde_json::json!({
            "child_agent_id": "ordinary-child", "child_session_id": "ordinary-child",
            "child_working_dir": root, "parent_tool_call_id": "ordinary-call",
        }))
        .unwrap();
        session.children.push(child.clone());
        let id: WorkflowRunId = uuid::Uuid::new_v4().to_string().parse().unwrap();
        session.workflow_run_ids.push(id);
        let mut saved_worker = child;
        saved_worker.parent_tool_call_id = Some("workflow-call".into());
        WorkflowStorage::new(&session, id).unwrap().save(&serde_json::json!({
            "version": 2, "run_id": id, "creator_id": session.id, "name": "saved workflow",
            "state": {"status": "settled", "execution_status": "completed", "cleanup_errors": [],
                "invocations": [{"id": 0, "role": "probe", "prompt": "inspect", "status": "succeeded",
                    "worker_instance_id": "ordinary-child", "worker_session": saved_worker, "value": "result"}]},
        })).unwrap();
        let binding = sessions.start_session(session, true).await.unwrap();
        assert!(binding.activity_agents().await.unwrap().is_empty());
        assert_eq!(
            binding.subagents().await.unwrap()[0].child_agent_id,
            "ordinary-child"
        );
        assert!(
            binding
                .workflow_views(binding.instance().registration_id().unwrap())
                .unwrap()
                .is_empty()
        );
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn recorded_worker_origin_survives_invalid_history() {
        let (root, policy, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&runtime.preparation);
        let sessions = crate::runtime::preparation::SessionWorkflows::new(
            runtime.sessions.clone(),
            runtime.preparation.clone(),
        )
        .unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let id: WorkflowRunId = uuid::Uuid::new_v4().to_string().parse().unwrap();
        let binding = sessions.start_session(session, false).await.unwrap();
        for (agent, owner) in [("worker", Some(id)), ("ordinary", None)] {
            let mut link: crate::session::ChildSessionLink =
                serde_json::from_value(serde_json::json!({
                    "child_agent_id": agent,
                    "child_session_id": agent,
                    "child_working_dir": root,
                }))
                .unwrap();
            link.workflow_run_id = owner;
            binding.record_child_intent(link).await.unwrap();
        }
        let stored = serde_json::to_value(binding.session().await).unwrap();
        assert_eq!(stored["children"][0]["workflow_run_id"], id.to_string());
        let activities = binding.activity_agents().await.unwrap();
        assert_eq!(activities.len(), 1);
        assert_eq!(activities[0].child_agent_id, "ordinary");
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
