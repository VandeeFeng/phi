//! Runtime commits and application Session associations/persistence.
//! SDK membership remains owned by AgentManager.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use phi_sdk::agent_instance::{AgentInstanceId, AgentInstanceStatus};
use phi_sdk::agent_manager::AgentManager;
use phi_sdk::lifecycle::SessionEndReason;

use crate::session::{Session, storage};

use super::save::{AgentSessions, SessionCheckpoints};
#[cfg(test)]
use super::session::AgentSessionSnapshot;
use super::session::{AgentSession, SessionNotices};
use super::{PreparedAgentRuntime, RuntimePreparation, RuntimeRebuildOptions};

pub(crate) enum SavedSessionLocator<'a> {
    Id(&'a str),
    IdOrPath(&'a str),
}

#[derive(Debug)]
pub(crate) struct ActiveSessionDeletion(AgentInstanceId);

impl std::fmt::Display for ActiveSessionDeletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "cannot delete an associated session `{}`; stop the Agent first",
            self.0
        )
    }
}

impl std::error::Error for ActiveSessionDeletion {}

pub(crate) struct SessionAssociations {
    manager: AgentManager,
    workflows: Arc<phi_sdk::workflow::WorkflowRuntime>,
    membership: tokio::sync::Mutex<()>,
    agent_sessions: Arc<AgentSessions>,
    checkpoints: Arc<SessionCheckpoints>,
    session_locks: RwLock<HashMap<AgentInstanceId, crate::session::storage::SessionLock>>,
    retired_notices: Mutex<Vec<SessionNotices>>,
}

struct SessionAdmission {
    session: Session,
    prepared: PreparedAgentRuntime,
    persist: bool,
    lock: Option<storage::SessionLock>,
}

pub(super) struct SessionCreation {
    associations: Arc<SessionAssociations>,
    admission: SessionAdmission,
    reason: phi_sdk::lifecycle::SessionStartReason,
}

impl SessionCreation {
    pub(super) async fn start(self) -> anyhow::Result<Arc<AgentSession>> {
        self.associations
            .insert_prepared(self.admission, self.reason, None)
            .await
    }

    pub(super) async fn start_with_publication(
        self,
        publish: impl FnOnce(&Arc<AgentSession>) -> anyhow::Result<()> + Send + 'static,
    ) -> anyhow::Result<Arc<AgentSession>> {
        tokio::spawn(async move {
            let binding = self.start().await?;
            publish(&binding)?;
            Ok(binding)
        })
        .await?
    }
}

impl SessionAdmission {
    fn reserve(&mut self) -> anyhow::Result<()> {
        ensure_master_candidate(&mut self.session, &self.prepared)?;
        if self.lock.is_none() && (self.persist || storage::session_path(&self.session).exists()) {
            self.lock = Some(storage::lock_session(&self.session)?);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct SessionAccess {
    id: AgentInstanceId,
    associations: Arc<SessionAssociations>,
}

impl SessionAccess {
    pub(crate) fn id(&self) -> &AgentInstanceId {
        &self.id
    }

    pub(crate) fn binding(&self) -> anyhow::Result<Arc<AgentSession>> {
        self.associations
            .find(&self.id)?
            .ok_or_else(|| anyhow::anyhow!("missing Session association for agent `{}`", self.id))
    }

    pub(super) fn retain_notices(
        &self,
        resources: &phi_sdk::agent_instance::AgentRuntimeResources,
    ) -> anyhow::Result<()> {
        self.associations
            .retired_notices
            .lock()
            .map_err(|_| anyhow::anyhow!("retired Session notices are unavailable"))?
            .push(SessionNotices::take(self.id.clone(), resources)?);
        Ok(())
    }

    pub(crate) fn pending_interactions(
        &self,
    ) -> anyhow::Result<Vec<phi_sdk::interaction::InteractionEnvelope>> {
        Ok(self
            .associations
            .find(&self.id)?
            .map(|binding| binding.instance().snapshot().pending_interactions)
            .unwrap_or_default())
    }

    pub(crate) fn interaction_sessions(&self) -> anyhow::Result<Vec<Arc<AgentSession>>> {
        let root = self.binding()?;
        ensure_attachable(&root)?;
        Ok(self
            .associations
            .scoped_sessions(&root)?
            .into_iter()
            .filter(|binding| ensure_attachable(binding).is_ok())
            .collect())
    }

    pub(crate) async fn checkpoint(&self) -> anyhow::Result<()> {
        self.associations.checkpoint_transcript(&self.id).await
    }

    pub(crate) async fn child(&self, instance_id: &str) -> anyhow::Result<Self> {
        let parent = self.binding()?;
        ensure_attachable(&parent)?;
        let link = parent
            .subagents()
            .await?
            .into_iter()
            .find(|link| link.child_agent_id == instance_id)
            .ok_or_else(|| anyhow::anyhow!("unknown subagent `{instance_id}`"))?;
        let access = Self {
            id: AgentInstanceId::new(&link.child_agent_id),
            associations: self.associations.clone(),
        };
        let child = access.binding()?;
        ensure_attachable(&child)?;
        anyhow::ensure!(
            child.session().await.id == link.child_session_id,
            "child Session does not match the parent child link"
        );
        Ok(access)
    }

    pub(crate) async fn send_message(
        &self,
        target: &AgentInstanceId,
        message: String,
    ) -> anyhow::Result<phi_sdk::agent_manager::AgentMessageReceipt> {
        self.child(target.as_str()).await?;
        Ok(self
            .associations
            .manager
            .for_agent(&self.id)?
            .send_message(target, message)
            .await?)
    }

    pub(crate) async fn subagent_view(
        &self,
        instance_id: &str,
    ) -> anyhow::Result<super::SubagentView> {
        let parent = self.binding()?;
        let link = parent
            .subagents()
            .await?
            .into_iter()
            .find(|link| link.child_agent_id == instance_id)
            .ok_or_else(|| anyhow::anyhow!("unknown subagent `{instance_id}`"))?;
        let (session, live_run, compact_enabled) = if let Some(child) = self
            .associations
            .find(&AgentInstanceId::new(&link.child_agent_id))?
        {
            let recovery = child.recovery().await?;
            (
                recovery.session,
                recovery.snapshot.instance.live_run,
                recovery.declarations.capabilities.compact_enabled,
            )
        } else {
            (
                storage::load_session_id(&link.child_session_id)?,
                None,
                None,
            )
        };
        session.validate_parent(&parent.session().await)?;
        let view = super::SubagentView {
            session,
            live_run,
            compact_enabled,
        };
        anyhow::ensure!(
            view.session.id == link.child_session_id,
            "child Session does not match the parent child link"
        );
        Ok(view)
    }
}

impl SessionAssociations {
    pub(crate) fn take_notices(&self) -> anyhow::Result<Vec<SessionNotices>> {
        let mut notices = self
            .retired_notices
            .lock()
            .map_err(|_| anyhow::anyhow!("retired Session notices are unavailable"))?;
        for agent in self.agent_sessions()? {
            notices.push(agent.take_notices()?);
        }
        notices.retain(|notice| {
            !notice.hook_notices.is_empty() || !notice.extension_notifications.is_empty()
        });
        Ok(std::mem::take(&mut *notices))
    }

    pub(crate) fn has_associations(&self) -> anyhow::Result<bool> {
        Ok(!self.agent_sessions()?.is_empty())
    }

    pub(crate) fn session_access(self: &Arc<Self>, id: &AgentInstanceId) -> SessionAccess {
        SessionAccess {
            id: id.clone(),
            associations: self.clone(),
        }
    }

    pub(super) async fn start_agent_session(
        self: &Arc<Self>,
        agent_session: &Arc<AgentSession>,
        reason: phi_sdk::lifecycle::SessionStartReason,
        related_session: Option<&str>,
    ) {
        self.observe(agent_session).await;
        let mut finalization = agent_session
            .instance()
            .subscribe_finalization()
            .expect("Session association requires SDK registration");
        tokio::select! {
            biased;
            _ = finalization.wait_for(|status| !matches!(status, phi_sdk::agent_manager::AgentFinalizationStatus::Open)) => {},
            _ = agent_session.session_started(reason, related_session) => {},
        }
    }
}

impl SessionAssociations {
    #[cfg(test)]
    pub(crate) async fn create(
        self: &Arc<Self>,
        preparation: &RuntimePreparation,
        session: Session,
        options: RuntimeRebuildOptions<'_>,
        reason: phi_sdk::lifecycle::SessionStartReason,
        persist: bool,
    ) -> anyhow::Result<Arc<AgentSession>> {
        self.prepare_creation(preparation, session, options, reason, persist, None)
            .await?
            .start()
            .await
    }

    pub(super) async fn prepare_creation(
        self: &Arc<Self>,
        preparation: &RuntimePreparation,
        session: Session,
        options: RuntimeRebuildOptions<'_>,
        reason: phi_sdk::lifecycle::SessionStartReason,
        persist: bool,
        lock: Option<storage::SessionLock>,
    ) -> anyhow::Result<SessionCreation> {
        ensure_master_session(&session)?;
        let prepared = preparation.prepare_initial(options).await?;
        self.prepared_creation(session, prepared, reason, persist, lock)
    }

    pub(super) fn prepared_creation(
        self: &Arc<Self>,
        session: Session,
        prepared: PreparedAgentRuntime,
        reason: phi_sdk::lifecycle::SessionStartReason,
        persist: bool,
        lock: Option<storage::SessionLock>,
    ) -> anyhow::Result<SessionCreation> {
        ensure_master_session(&session)?;
        Ok(SessionCreation {
            associations: self.clone(),
            admission: SessionAdmission {
                session,
                prepared,
                persist,
                lock,
            },
            reason,
        })
    }

    pub(crate) fn open_existing(
        &self,
        id: &AgentInstanceId,
    ) -> anyhow::Result<Option<Arc<AgentSession>>> {
        self.find(id)?
            .map(|agent_session| {
                ensure_attachable(&agent_session)?;
                Ok(agent_session)
            })
            .transpose()
    }

    pub(crate) fn get(&self, id: &AgentInstanceId) -> anyhow::Result<Arc<AgentSession>> {
        self.find(id)?
            .ok_or_else(|| anyhow::anyhow!("missing Session association for agent `{id}`"))
    }

    pub(crate) async fn stop_scoped_workflow(
        &self,
        root: &Arc<AgentSession>,
        run: phi_sdk::workflow::WorkflowRunId,
    ) -> anyhow::Result<Option<phi_sdk::workflow::WorkflowSnapshot>> {
        for binding in self.scoped_sessions(root)? {
            let Some(registration) = binding.instance().registration_id() else {
                continue;
            };
            if binding
                .workflow_views(registration)?
                .iter()
                .any(|view| view.snapshot.id() == run)
            {
                return binding.stop_workflow(registration, run).await.map(Some);
            }
        }
        Ok(None)
    }

    // Adapter input admission must already be closed; this does not fence new submissions.
    pub(crate) async fn wait_for_quiescence(
        &self,
        root: &Arc<AgentSession>,
        event_boundary: Option<String>,
    ) -> anyhow::Result<()> {
        loop {
            root.instance().wait_for_quiescence(None).await?;
            let bindings = self.scoped_sessions(root)?;
            let runs = workflow_ids(&bindings);
            for binding in bindings {
                binding.settle_workflow_deliveries().await?;
            }
            root.instance().wait_for_quiescence(None).await?;
            // Terminal feedback may launch another run, including on an ordinary child.
            if runs == workflow_ids(&self.scoped_sessions(root)?) {
                root.instance().wait_for_quiescence(event_boundary).await?;
                return Ok(());
            }
        }
    }

    fn scoped_sessions(&self, root: &Arc<AgentSession>) -> anyhow::Result<Vec<Arc<AgentSession>>> {
        anyhow::ensure!(
            self.find(root.instance().id())?
                .is_some_and(|binding| Arc::ptr_eq(&binding, root)),
            "root Session association has changed"
        );
        let candidates = self.agent_sessions()?;
        let mut bindings = vec![root.clone()];
        let mut index = 0;
        while index < bindings.len() {
            let parent = bindings[index].instance().id().clone();
            for candidate in &candidates {
                if candidate.creator_id() == Some(&parent)
                    && !bindings
                        .iter()
                        .any(|binding| Arc::ptr_eq(binding, candidate))
                {
                    bindings.push(candidate.clone());
                }
            }
            index += 1;
        }
        Ok(bindings)
    }

    #[cfg(test)]
    pub(crate) async fn list(&self) -> anyhow::Result<Vec<AgentSessionSnapshot>> {
        let agents = self.agent_sessions()?;
        let mut snapshots = Vec::with_capacity(agents.len());
        for agent in agents {
            snapshots.push(agent.snapshot().await);
        }
        Ok(snapshots)
    }
}

fn workflow_ids(
    bindings: &[Arc<AgentSession>],
) -> std::collections::HashSet<phi_sdk::workflow::WorkflowRunId> {
    bindings
        .iter()
        .flat_map(|binding| binding.workflow_run_ids())
        .collect()
}

impl SessionAssociations {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn transition_prepared(
        self: &Arc<Self>,
        current_id: &AgentInstanceId,
        session: Session,
        prepared: PreparedAgentRuntime,
        start_reason: phi_sdk::lifecycle::SessionStartReason,
        start_related_session: Option<&str>,
        end_reason: phi_sdk::lifecycle::SessionEndReason,
        end_related_session: Option<&str>,
        persist: bool,
        lock: Option<storage::SessionLock>,
    ) -> anyhow::Result<Arc<AgentSession>> {
        let _membership = self.membership.lock().await;
        let target_id = AgentInstanceId::new(session.id.to_string());
        if &target_id == current_id {
            anyhow::bail!("Session transition requires a different Session ID");
        }
        let existing = self.find(&target_id)?;
        if let Some(agent_session) = &existing {
            ensure_attachable(agent_session)?;
        }
        let mut admission = SessionAdmission {
            session,
            prepared,
            persist,
            lock,
        };
        if existing.is_none() {
            admission.reserve()?;
        }
        drop(_membership);
        let associations = self.clone();
        let current = self.get(current_id)?;
        let start_related_session = start_related_session.map(ToString::to_string);
        let end_related_session = end_related_session.map(ToString::to_string);
        tokio::spawn(async move {
            associations
                .stop_agent_session(current, end_reason, end_related_session.as_deref())
                .await?;
            if let Some(agent_session) = existing {
                ensure_attachable(&agent_session)?;
                return Ok(agent_session);
            }
            associations
                .insert_prepared(admission, start_reason, start_related_session.as_deref())
                .await
        })
        .await?
    }

    pub(crate) async fn stop(
        self: &Arc<Self>,
        id: &AgentInstanceId,
        reason: phi_sdk::lifecycle::SessionEndReason,
    ) -> anyhow::Result<Session> {
        let agent_session = self.get(id)?;
        let associations = self.clone();
        tokio::spawn(async move {
            associations
                .stop_agent_session(agent_session, reason, None)
                .await
        })
        .await?
    }

    pub(crate) async fn shutdown(
        self: &Arc<Self>,
        reason: phi_sdk::lifecycle::SessionEndReason,
    ) -> anyhow::Result<Vec<Session>> {
        let associations = self.clone();
        tokio::spawn(async move {
            associations.workflows.shutdown().await?;
            let agent_sessions = associations.agent_sessions()?;
            let mut capture_errors = Vec::new();
            for agent_session in &agent_sessions {
                agent_session.close_workflow_admission().await;
                if let Err(error) = agent_session.settle_workflow_deliveries().await {
                    capture_errors
                        .push(error.context(format!("Agent {}", agent_session.instance().id())));
                }
                agent_session.request_session_end(reason, None).await;
            }
            let stopped = associations.manager.shutdown().await;
            for agent_session in &agent_sessions {
                wait_for_finalization(agent_session).await?;
            }
            let saved = associations.reconcile_finalizations().await;
            finalization_result(
                capture_errors,
                stopped.map(|_| ()).map_err(Into::into),
                saved,
            )?;
            let mut sessions = Vec::with_capacity(agent_sessions.len());
            for agent_session in agent_sessions {
                sessions.push(agent_session.session().await);
            }
            Ok(sessions)
        })
        .await?
    }
}

impl SessionAssociations {
    async fn insert_prepared(
        self: &Arc<Self>,
        admission: SessionAdmission,
        reason: phi_sdk::lifecycle::SessionStartReason,
        related_session: Option<&str>,
    ) -> anyhow::Result<Arc<AgentSession>> {
        let associations = self.clone();
        let related_session = related_session.map(ToString::to_string);
        tokio::spawn(async move {
            let membership = associations.membership.lock().await;
            let agent_session = associations.insert_prepared_locked(admission).await?;
            drop(membership);
            associations
                .start_agent_session(&agent_session, reason, related_session.as_deref())
                .await;
            ensure_attachable(&agent_session)?;
            Ok(agent_session)
        })
        .await?
    }

    async fn insert_prepared_locked(
        self: &Arc<Self>,
        mut admission: SessionAdmission,
    ) -> anyhow::Result<Arc<AgentSession>> {
        let expected_id = AgentInstanceId::new(admission.session.id.to_string());
        anyhow::ensure!(
            self.find(&expected_id)?.is_none(),
            "Session association for agent `{expected_id}` already exists"
        );
        admission.reserve()?;
        let SessionAdmission {
            session,
            prepared,
            persist,
            lock: session_lock,
        } = admission;
        let (prepared_instance, runtime) = prepared.into_parts();
        let team_root = (runtime.team_capability
            == phi_sdk::agent_manager::TeamCapability::Enabled)
            .then(|| {
                phi_sdk::agent_manager::TeamStorageRoot::new(std::path::absolute(
                    crate::config::settings::phi_path().join("teams"),
                )?)
                .map_err(anyhow::Error::from)
            })
            .transpose()?;
        let instance = self.manager.insert(prepared_instance)?;
        if let Some(root) = team_root {
            if let Err(cause) = self.manager.initialize_team(instance.id(), root).await {
                return match self.manager.stop(&expected_id).await {
                    Ok(_) => Err(cause.into()),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "Team startup failed: {cause}; cleanup failed: {cleanup}"
                    )),
                };
            }
        }
        let agent_session = AgentSession::new(instance, session, runtime, persist);
        agent_session
            .runtime()
            .workflow_tool
            .bind(&agent_session, &self.workflows);
        if let Err(error) =
            self.register_agent_session(expected_id.clone(), agent_session.clone(), session_lock)
        {
            return match self.manager.stop(&expected_id).await {
                Ok(_) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "Session association failed: {error}; cleanup failed: {cleanup}"
                )),
            };
        }
        Ok(agent_session)
    }
}

impl SessionAssociations {
    pub(super) async fn associate_child_session(
        &self,
        creator_id: &AgentInstanceId,
        mut session: Session,
        runtime: super::AgentRuntime,
        instance: phi_sdk::agent_instance::AgentInstance,
        cancellation: &mut tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<Arc<AgentSession>> {
        anyhow::ensure!(
            instance.id().as_str() == session.id.as_str()
                && session.creator_id() == Some(creator_id)
                && std::path::Path::new(session.working_dir.as_str())
                    == instance.snapshot().working_dir,
            "child association identity mismatch"
        );
        session.bind_task_list(instance.task_list())?;
        session.set_transcript(instance.transcript().await?);
        let parent = self.find(creator_id)?.ok_or_else(|| {
            anyhow::anyhow!("missing Session association for agent `{creator_id}`")
        })?;
        session.validate_parent(&parent.session().await)?;
        let persist = parent.persists();
        let session_lock = tokio::task::spawn_blocking({
            let session = session.clone();
            move || {
                let path = storage::session_path(&session);
                let lock = (persist || path.exists())
                    .then(|| storage::lock_session(&session))
                    .transpose()?;
                if persist {
                    storage::save_session(&session)?;
                }
                Ok::<_, anyhow::Error>(lock)
            }
        })
        .await??;
        let membership = tokio::select! {
            biased;
            _ = async { let _ = cancellation.wait_for(|cancelled| *cancelled).await; } => anyhow::bail!("subagent cancelled before association"),
            membership = self.membership.lock() => membership,
        };
        let current_parent = self.find(parent.instance().id())?.ok_or_else(|| {
            anyhow::anyhow!(
                "missing Session association for agent `{}`",
                parent.instance().id()
            )
        })?;
        ensure_attachable(&current_parent)?;
        let id = instance.id().clone();
        let agent_session = AgentSession::new(instance, session, runtime, persist);
        agent_session
            .runtime()
            .workflow_tool
            .bind(&agent_session, &self.workflows);
        self.register_agent_session(id, agent_session.clone(), session_lock)?;
        drop(membership);
        Ok(agent_session)
    }
}

impl SessionAssociations {
    pub(crate) fn new(
        manager: AgentManager,
        workflows: Arc<phi_sdk::workflow::WorkflowRuntime>,
    ) -> Arc<Self> {
        let agent_sessions = Arc::new(RwLock::new(HashMap::new()));
        Arc::new(Self {
            checkpoints: SessionCheckpoints::new(),
            manager,
            workflows,
            membership: tokio::sync::Mutex::new(()),
            agent_sessions,
            session_locks: RwLock::new(HashMap::new()),
            retired_notices: Mutex::default(),
        })
    }

    async fn observe(self: &Arc<Self>, agent_session: &Arc<AgentSession>) {
        self.checkpoints
            .observe(agent_session, Arc::downgrade(self))
            .await;
    }

    async fn checkpoint_transcript(&self, id: &AgentInstanceId) -> anyhow::Result<()> {
        let agent_session = self
            .find(id)?
            .ok_or_else(|| anyhow::anyhow!("missing Session association for agent `{id}`"))?;
        let checkpoints = self.checkpoints.clone();
        tokio::spawn(async move { checkpoints.checkpoint(&agent_session).await })
            .await
            .map_err(|error| anyhow::anyhow!("Session checkpoint task failed: {error}"))?
    }

    pub(crate) fn delete_session(&self, id: &str) -> anyhow::Result<()> {
        let id = AgentInstanceId::new(id);
        if self.find(&id)?.is_some() {
            return Err(ActiveSessionDeletion(id).into());
        }
        storage::delete_session(id.as_str())
    }

    fn register_agent_session(
        &self,
        id: AgentInstanceId,
        agent_session: Arc<AgentSession>,
        session_lock: Option<crate::session::storage::SessionLock>,
    ) -> anyhow::Result<()> {
        let mut agents = self
            .agent_sessions
            .write()
            .map_err(|_| anyhow::anyhow!("Session association registry is unavailable"))?;
        let mut locks = self
            .session_locks
            .write()
            .map_err(|_| anyhow::anyhow!("session lock registry is unavailable"))?;
        anyhow::ensure!(
            !agents.contains_key(&id),
            "Session association for agent `{id}` already exists"
        );
        agents.insert(id.clone(), agent_session.clone());
        if let Some(lock) = session_lock {
            locks.insert(id, lock);
        }
        drop(locks);
        drop(agents);
        super::mcp::observe(&agent_session);
        Ok(())
    }
}

impl SessionAssociations {
    pub(super) async fn discard_child_session(
        &self,
        child: &Arc<AgentSession>,
    ) -> anyhow::Result<()> {
        child
            .request_session_end(SessionEndReason::Quit, None)
            .await;
        self.finalize_agent_session(child).await?;
        Ok(())
    }
}

impl SessionAssociations {
    async fn stop_agent_session(
        &self,
        agent_session: Arc<AgentSession>,
        reason: SessionEndReason,
        related_session: Option<&str>,
    ) -> anyhow::Result<Session> {
        agent_session.close_workflow_admission().await;
        if let Some(registration) = agent_session.instance().registration_id() {
            self.workflows.stop_owned(registration).await?;
        }
        let capture_errors = agent_session
            .settle_workflow_deliveries()
            .await
            .err()
            .into_iter()
            .collect();
        agent_session
            .request_session_end(reason, related_session)
            .await;
        if agent_session
            .instance()
            .finalization()
            .is_none_or(|result| result.cleanup_error.is_some())
        {
            let stopped = match agent_session.instance().stop().await {
                Ok(_) | Err(phi_sdk::agent_instance::AgentInstanceError::Stopped) => Ok(()),
                Err(error) => Err(error.into()),
            };
            wait_for_finalization(&agent_session).await?;
            let saved = self.reconcile_finalizations().await;
            finalization_result(capture_errors, stopped, saved)?;
        } else {
            finalization_result(capture_errors, Ok(()), self.reconcile_finalizations().await)?;
        }
        Ok(agent_session.session().await)
    }
}

impl SessionAssociations {
    fn agent_sessions(&self) -> anyhow::Result<Vec<Arc<AgentSession>>> {
        let mut agent_sessions = self
            .agent_sessions
            .read()
            .map_err(|_| anyhow::anyhow!("Session association registry is unavailable"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        agent_sessions.sort_by(|left, right| {
            left.instance()
                .id()
                .as_str()
                .cmp(right.instance().id().as_str())
        });
        Ok(agent_sessions)
    }

    async fn reconcile_finalizations(&self) -> anyhow::Result<Vec<Session>> {
        let mut sessions = Vec::new();
        let mut failures = Vec::new();
        for agent_session in self.agent_sessions()? {
            if agent_session.instance().finalization().is_none() {
                continue;
            }
            match self.finalize_agent_session(&agent_session).await {
                Ok(Some(session)) => sessions.push(session),
                Ok(None) => {}
                Err(error) => failures.push(format!("{}: {error}", agent_session.instance().id())),
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "Session finalization failed: {}",
            failures.join("; ")
        );
        Ok(sessions)
    }

    pub(super) async fn finalize_agent_session(
        &self,
        agent_session: &Arc<AgentSession>,
    ) -> anyhow::Result<Option<Session>> {
        agent_session.close_workflow_admission().await;
        agent_session.settle_workflow_deliveries().await?;
        let _checkpoint = agent_session.checkpoint_guard().await;
        let id = agent_session.instance().id().clone();
        if self
            .find(&id)?
            .is_none_or(|current| !Arc::ptr_eq(&current, agent_session))
        {
            return Ok(None);
        }
        let attempt = agent_session
            .begin_save(super::save::SaveOperation::Finalization)
            .await;
        let result = self.save_final_session(agent_session).await;
        let boundary = result
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .map(|session| agent_session.saved_boundary(session));
        agent_session
            .complete_save(
                attempt,
                super::save::SaveOperation::Finalization,
                1,
                &boundary,
            )
            .await;
        boundary?;
        let session = result?;
        {
            let mut notices = self
                .retired_notices
                .lock()
                .map_err(|_| anyhow::anyhow!("retired Session notices are unavailable"))?;
            notices.push(agent_session.take_notices()?);
            self.remove_agent_session(agent_session)?;
        }
        agent_session.stop_checkpoint_observer().await;
        Ok(Some(session))
    }

    async fn save_final_session(&self, agent_session: &AgentSession) -> anyhow::Result<Session> {
        let instance = agent_session.instance();
        let finalization = instance
            .finalization()
            .ok_or_else(|| anyhow::anyhow!("registration finalization is not ready"))?;
        anyhow::ensure!(
            Some(finalization.registration_id) == instance.registration_id()
                && finalization.instance_id == *instance.id(),
            "registration finalization identity mismatch"
        );
        anyhow::ensure!(
            finalization.cleanup_error.is_none(),
            "SDK finalization failed: {}",
            finalization.cleanup_error.unwrap_or_default()
        );
        let view = finalization
            .view
            .ok_or_else(|| anyhow::anyhow!("final canonical recovery is unavailable"))?;
        agent_session.end_and_save_session(view.transcript).await
    }

    fn remove_agent_session(&self, agent_session: &Arc<AgentSession>) -> anyhow::Result<()> {
        let id = agent_session.instance().id().clone();
        let mut agent_sessions = self
            .agent_sessions
            .write()
            .map_err(|_| anyhow::anyhow!("Session association registry is unavailable"))?;
        let mut locks = self
            .session_locks
            .write()
            .map_err(|_| anyhow::anyhow!("session lock registry is unavailable"))?;
        let released_lock = if agent_sessions
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, agent_session))
        {
            agent_sessions.remove(&id);
            locks.remove(&id)
        } else {
            None
        };
        drop(locks);
        drop(agent_sessions);
        drop(released_lock);
        Ok(())
    }

    pub(super) fn find(&self, id: &AgentInstanceId) -> anyhow::Result<Option<Arc<AgentSession>>> {
        Ok(self
            .agent_sessions
            .read()
            .map_err(|_| anyhow::anyhow!("Session association registry is unavailable"))?
            .get(id)
            .cloned())
    }
}

async fn wait_for_finalization(agent_session: &AgentSession) -> anyhow::Result<()> {
    let mut status = agent_session
        .instance()
        .subscribe_finalization()
        .expect("Session association requires SDK registration");
    status
        .wait_for(|status| {
            !matches!(
                status,
                phi_sdk::agent_manager::AgentFinalizationStatus::Closing
            )
        })
        .await
        .map_err(|_| anyhow::anyhow!("registration finalization is unavailable"))?;
    Ok(())
}

fn finalization_result(
    capture_errors: Vec<anyhow::Error>,
    stopped: anyhow::Result<()>,
    saved: anyhow::Result<Vec<Session>>,
) -> anyhow::Result<Vec<Session>> {
    let mut failures: Vec<_> = capture_errors
        .into_iter()
        .map(|error| error.context("Workflow capture failed"))
        .collect();
    if let Err(error) = stopped {
        failures.push(error.context("SDK stop failed"));
    }
    let sessions = match saved {
        Ok(sessions) => sessions,
        Err(error) => {
            failures.push(error);
            Vec::new()
        }
    };
    let diagnostic = failures
        .iter()
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    match failures.into_iter().next() {
        Some(error) => Err(error.context(diagnostic)),
        None => Ok(sessions),
    }
}

#[cfg(test)]
mod association_tests {
    use super::*;

    #[tokio::test]
    async fn saved_start_holds_lock_while_waiting_for_registration() {
        use super::super::preparation::{SessionWorkflows, tests::session_test_runtime};

        for persist in [false, true] {
            let (root, policy, runtime) = session_test_runtime();
            policy.install(&runtime.preparation);
            let workflows =
                SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone())
                    .unwrap();
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            session.transcript.reset_context(None).unwrap();
            storage::save_session(&session).unwrap();
            let path = storage::session_path(&session);
            let membership = runtime.sessions.membership.lock().await;
            let id = session.id.clone();
            let waiter =
                tokio::spawn(async move { workflows.start_saved_session(&id, persist).await });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !path.with_extension("lock").exists() {
                    assert!(!waiter.is_finished());
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(storage::lock_session_path(&storage::session_path(&session)).is_err());
            assert!(storage::delete_session(&session.id).is_err());
            drop(membership);
            let agent = waiter.await.unwrap().unwrap();
            assert_eq!(agent.instance().id().as_str(), session.id.as_str());
            assert!(storage::lock_session_path(&storage::session_path(&session)).is_err());
            runtime
                .sessions
                .shutdown(SessionEndReason::Quit)
                .await
                .unwrap();
            drop(storage::lock_session(&session).unwrap());
            storage::delete_session(&session.id).unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn locked_resume_target_preserves_current_session() {
        use super::super::preparation::{SessionWorkflows, tests::session_test_runtime};

        for persist in [false, true] {
            let (root, policy, runtime) = session_test_runtime();
            policy.install(&runtime.preparation);
            let workflows =
                SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone())
                    .unwrap();
            let mut current =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            current.working_dir = root.to_string_lossy().into();
            let agent = workflows.start_session(current, persist).await.unwrap();
            let generation = agent.runtime().generation_id().to_string();
            let mut target =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            target.working_dir = root.to_string_lossy().into();
            target
                .transcript
                .reset_context(Some("saved target".into()))
                .unwrap();
            storage::save_session(&target).unwrap();
            let lock = storage::lock_session_path(&storage::session_path(&target)).unwrap();

            let path = storage::session_path(&target);
            let journal = std::fs::read(&path).unwrap();
            std::fs::write(&path, b"incomplete concurrent checkpoint\n").unwrap();
            let errors = [
                workflows
                    .start_saved_session(&target.id, persist)
                    .await
                    .err(),
                workflows
                    .resume_session(
                        agent.instance().id(),
                        SavedSessionLocator::Id(target.id.as_str()),
                    )
                    .await
                    .err(),
                workflows
                    .resume_session(
                        agent.instance().id(),
                        SavedSessionLocator::IdOrPath(path.to_str().unwrap()),
                    )
                    .await
                    .err(),
                workflows
                    .jump_anchor(agent.instance().id(), &target.id, &target.id)
                    .await
                    .err(),
            ];
            for error in errors {
                let error = error.expect("locked saved Session must be rejected before reading");
                assert!(
                    error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| { error.kind() == std::io::ErrorKind::WouldBlock }),
                    "expected lock contention before journal parsing: {error:#}"
                );
            }
            std::fs::write(&path, journal).unwrap();
            assert_eq!(
                agent.instance().snapshot().status,
                AgentInstanceStatus::Idle
            );
            assert_eq!(agent.runtime().generation_id(), generation);
            assert!(Arc::ptr_eq(
                &runtime.sessions.get(agent.instance().id()).unwrap(),
                &agent
            ));
            assert!(
                runtime
                    .sessions
                    .get(&AgentInstanceId::new(target.id.as_str()))
                    .is_err()
            );

            drop(lock);
            let transition = workflows
                .resume_session(
                    agent.instance().id(),
                    SavedSessionLocator::Id(target.id.as_str()),
                )
                .await
                .unwrap();
            assert_eq!(transition.attachment.id().as_str(), target.id.as_str());
            runtime
                .sessions
                .shutdown(SessionEndReason::Quit)
                .await
                .unwrap();
            storage::delete_session(target.id.as_str()).unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn workflow_child_links_publish_after_readable_association() {
        use clap::Parser;
        use phi_sdk::workflow::{WorkflowLimits, WorkflowRequest, WorkflowStatus};
        use std::time::Duration;

        enum ParentJournal {
            Writable,
            Unwritable,
        }

        tokio::time::timeout(Duration::from_secs(15), async {
            for journal in [ParentJournal::Writable, ParentJournal::Unwritable] {
                let (root, _, runtime) =
                    super::super::preparation::tests::session_test_runtime();
                std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
                std::fs::write(
                    root.join(".phi/agents/probe.md"),
                    "---\nname: probe\ndescription: Probe\n---\nInspect.",
                )
                .unwrap();
                std::fs::write(
                    root.join(".phi/settings.json"),
                    r#"{"defaultProvider":"local","defaultModel":"test","memory":{"enabled":false},"extensions":["!*.lua"]}"#,
                )
                .unwrap();
                let catalog = Arc::new(
                    phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                        store_path: root.join("store.json"),
                        user_paths: vec![root.join("models.json")],
                        offline: true,
                    })
                    .unwrap(),
                );
                let cli = Arc::new(crate::cli::Cli::parse_from(["phi"]));
                runtime.preparation.enable_subagents(&runtime.sessions);
                let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
                let mut settings = settings_manager.settings();
                settings.permission = Some(serde_json::json!({"mode":"allow"}));
                settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
                    enabled: false,
                    ..Default::default()
                });
                let mut session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
                session.working_dir = root.to_string_lossy().into();
                let parent = runtime
                    .sessions
                    .create(
                        &runtime.preparation,
                        session.clone(),
                        super::super::RuntimeRebuildOptions {
                            phi: super::super::PhiBuildOptions {
                                cli: &cli,
                                provider: "local",
                                model: "test",
                                runtime_cfg: settings.runtime_config(&catalog),
                                context: phi_sdk::context::load_with_options_for(
                                    false,
                                    None,
                                    vec![root.join(".phi/agents")],
                                    &root,
                                ),
                                agent_options: crate::AgentBuildOptions::default(),
                                hooks: Vec::new(),
                            },
                            instance: {
                                let mut instance = super::super::AgentInstanceBuildOptions::from_session(&session);
                                instance.settings_manager = Some(settings_manager.clone());
                                instance.resource_settings = Some(Arc::new(settings.clone()));
                                instance
                            },
                        },
                        phi_sdk::lifecycle::SessionStartReason::New,
                        true,
                    )
                    .await
                    .unwrap();
                let membership = runtime.sessions.membership.lock().await;
                let run = parent
                    .launch_workflow(
                        runtime.workflows.clone(),
                        WorkflowRequest::parse(
                            "export const meta = {name: 'readiness', description: 'child publication'}; return await agent('inspect', {role: 'probe'});",
                            None,
                        )
                        .await
                        .unwrap(),
                        None,
                        WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(),
                        None,
                    )
                    .await
                    .unwrap();
                let parent_path = storage::session_path(&session);
                let directory = parent_path
                    .parent()
                    .unwrap()
                    .join(session.id.as_str())
                    .join("subagents");
                let child = loop {
                    assert_ne!(run.snapshot().status(), WorkflowStatus::Failed, "{:?}", run.snapshot());
                    if let Ok(entries) = std::fs::read_dir(&directory)
                        && let Some(path) = entries
                            .filter_map(Result::ok)
                            .map(|entry| entry.path())
                            .find(|path| {
                                path.extension()
                                    .is_some_and(|extension| extension == "jsonl")
                            })
                        && let Ok(child) = storage::load_session(path.to_str().unwrap())
                    {
                        break child;
                    }
                    tokio::task::yield_now().await;
                };
                let child_id = AgentInstanceId::new(child.id.as_str());
                assert!(runtime.sessions.find(&child_id).unwrap().is_none());
                assert!(
                    parent.session().await.children.is_empty(),
                    "child link published before its Session association was ready"
                );
                assert!(parent.subagents().await.unwrap().is_empty());
                if matches!(journal, ParentJournal::Unwritable) {
                    std::fs::remove_file(&parent_path).unwrap();
                    std::fs::create_dir(&parent_path).unwrap();
                }
                drop(membership);
                match journal {
                    ParentJournal::Writable => {
                        while parent.session().await.children.is_empty() {
                            tokio::task::yield_now().await;
                        }
                        let access = runtime.sessions.session_access(parent.instance().id());
                        let view = access.subagent_view(child.id.as_str()).await.unwrap();
                        assert_eq!(view.session.id, child.id);
                        assert!(access.interaction_sessions().unwrap().iter()
                            .any(|binding| binding.instance().id() == &child_id));
                        run.stop().await.unwrap();
                    }
                    ParentJournal::Unwritable => {
                        let mut observations = run.subscribe();
                        while observations.snapshot().status() != WorkflowStatus::Failed {
                            observations.changed().await.unwrap();
                        }
                        assert!(runtime.sessions.find(&child_id).unwrap().is_none());
                        assert!(runtime.manager.get(&child_id).is_err());
                        assert!(parent.session().await.children.is_empty());
                        std::fs::remove_dir(&parent_path).unwrap();
                    }
                }
                runtime.sessions.shutdown(SessionEndReason::Quit).await.unwrap();
                let saved = storage::load_session_id(session.id.as_str()).unwrap();
                match journal {
                    ParentJournal::Writable => {
                        assert_eq!(saved.children.len(), 1);
                        assert_eq!(saved.children[0].child_session_id, child.id);
                    }
                    ParentJournal::Unwritable => assert!(saved.children.is_empty()),
                }
                assert_eq!(storage::load_session_id(child.id.as_str()).unwrap().id, child.id);
                std::fs::remove_dir_all(root).unwrap();
            }
        })
        .await
        .expect("Workflow child association did not settle");
    }

    #[tokio::test]
    async fn noninteractive_sessions_disable_configured_teams_across_rebuilds() {
        use super::super::factory::{
            AgentInstanceBuildOptions, PhiBuildOptions, RuntimeRebuildOptions,
        };
        use clap::Parser;
        use phi_sdk::agent_manager::{TeamCapability, team_namespace};

        let (root, _, test_runtime) = super::super::preparation::tests::session_test_runtime();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap();
        let settings = crate::config::Settings {
            agent: crate::config::AgentConfig {
                team: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
        for arguments in [vec!["phi", "-p"], vec!["phi", "--rpc"]] {
            let cli = crate::cli::Cli::parse_from(arguments);
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            let mut instance = AgentInstanceBuildOptions::from_session(&session);
            instance.settings_manager = Some(settings_manager.clone());
            instance.resource_settings = Some(Arc::new(settings.clone()));
            let namespace = crate::config::settings::phi_path()
                .join("teams")
                .join(team_namespace(session.id.as_str()).unwrap());
            let agent = test_runtime
                .sessions
                .create(
                    &test_runtime.preparation,
                    session.clone(),
                    RuntimeRebuildOptions {
                        instance,
                        phi: PhiBuildOptions {
                            cli: &cli,
                            provider: "local",
                            model: "test",
                            runtime_cfg: settings.runtime_config(&catalog),
                            context: phi_sdk::context::load_with_options_for(
                                false,
                                None,
                                Vec::new(),
                                &root,
                            ),
                            agent_options: crate::AgentBuildOptions {
                                no_tools: true,
                                ..Default::default()
                            },
                            hooks: Vec::new(),
                        },
                    },
                    phi_sdk::lifecycle::SessionStartReason::New,
                    false,
                )
                .await
                .unwrap();
            assert_eq!(agent.runtime().team_capability, TeamCapability::Disabled);
            assert!(!namespace.exists());
            assert_eq!(
                agent.instance().task_list().unwrap().list_id(),
                session.id.as_str()
            );
            let replacement = agent
                .runtime()
                .prepare_replacement(&agent.session().await)
                .await
                .unwrap();
            assert_eq!(
                replacement.runtime.team_capability,
                TeamCapability::Disabled
            );
            test_runtime
                .sessions
                .stop(
                    agent.instance().id(),
                    phi_sdk::lifecycle::SessionEndReason::Quit,
                )
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn sequential_teams_reject_prefix_collision() {
        use super::super::factory::{
            AgentInstanceBuildOptions, PhiBuildOptions, RuntimeRebuildOptions,
        };
        use clap::Parser;

        let (root, _, runtime) = super::super::preparation::tests::session_test_runtime();
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![root.join("models.json")],
                offline: true,
            })
            .unwrap(),
        );
        let cli = crate::cli::Cli::parse_from(["phi"]);
        let settings = crate::config::Settings {
            agent: crate::config::AgentConfig {
                team: Some(true),
                ..Default::default()
            },
            ..Default::default()
        };
        let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
        let first_id = format!("{}-first", &uuid::Uuid::new_v4().to_string()[..8]);
        let other_id = format!("{}-other", &first_id[..8]);
        for id in [&first_id, &first_id, &other_id] {
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.id = id.clone().into();
            session.working_dir = root.to_string_lossy().into();
            let mut instance = AgentInstanceBuildOptions::from_session(&session);
            instance.settings_manager = Some(settings_manager.clone());
            instance.resource_settings = Some(Arc::new(settings.clone()));
            let result = runtime
                .sessions
                .create(
                    &runtime.preparation,
                    session,
                    RuntimeRebuildOptions {
                        instance,
                        phi: PhiBuildOptions {
                            cli: &cli,
                            provider: "local",
                            model: "test",
                            runtime_cfg: settings.runtime_config(&catalog),
                            context: phi_sdk::context::load_with_options_for(
                                false,
                                None,
                                Vec::new(),
                                &root,
                            ),
                            agent_options: crate::AgentBuildOptions {
                                no_tools: true,
                                ..Default::default()
                            },
                            hooks: Vec::new(),
                        },
                    },
                    phi_sdk::lifecycle::SessionStartReason::New,
                    false,
                )
                .await;
            if id == &other_id {
                assert!(
                    result.is_err(),
                    "another Session adopted retained Tasks through a shortened namespace"
                );
                break;
            }
            let agent = result.unwrap();
            let namespace = phi_sdk::agent_manager::team_namespace(id).unwrap();
            let tasks = crate::config::settings::phi_path()
                .join("tasks")
                .join(&namespace);
            std::fs::create_dir_all(&tasks).unwrap();
            std::fs::write(tasks.join("1.json"), r#"{"id":"1","subject":"Private work","description":"Keep","status":"pending","blocked_by":[]}"#).unwrap();
            assert_eq!(
                agent
                    .instance()
                    .observe_tasks()
                    .await
                    .unwrap()
                    .unwrap()
                    .tasks
                    .len(),
                1
            );
            runtime
                .sessions
                .stop(
                    agent.instance().id(),
                    phi_sdk::lifecycle::SessionEndReason::Quit,
                )
                .await
                .unwrap();
            assert!(
                !crate::config::settings::phi_path()
                    .join("teams")
                    .join(namespace)
                    .exists()
            );
        }
    }

    #[tokio::test]
    async fn team_startup_precedes_ready_access_and_preserves_existing_records() {
        use super::super::factory::{
            AgentInstanceBuildOptions, PhiBuildOptions, RuntimeRebuildOptions,
        };
        use clap::Parser;
        use phi_sdk::agent_manager::{TeamCapability, team_namespace};
        let (root, _, test_runtime) = super::super::preparation::tests::session_test_runtime();
        let catalog = Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![root.join("models.json")],
                offline: true,
            })
            .unwrap(),
        );
        let cli = crate::cli::Cli::parse_from(["phi"]);
        for capability in [TeamCapability::Disabled, TeamCapability::Enabled] {
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            let namespace = crate::config::settings::phi_path()
                .join("teams")
                .join(team_namespace(session.id.as_str()).unwrap());
            let tasks = crate::config::settings::phi_path()
                .join("tasks")
                .join(session.id.as_str());
            std::fs::create_dir_all(&tasks).unwrap();
            std::fs::write(tasks.join("unrelated"), "keep").unwrap();
            let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            for collision in [true, false] {
                if collision && capability == TeamCapability::Enabled {
                    std::fs::create_dir_all(&namespace).unwrap();
                    std::fs::write(namespace.join("foreign"), "keep").unwrap();
                }
                let settings = crate::config::Settings {
                    agent: crate::config::AgentConfig {
                        team: Some(capability == TeamCapability::Enabled),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
                let mut instance = AgentInstanceBuildOptions::from_session(&session);
                instance.settings_manager = Some(settings_manager);
                instance.resource_settings = Some(Arc::new(settings.clone()));
                let hook_namespace = namespace.clone();
                let observed = started.clone();
                let options = RuntimeRebuildOptions {
                    instance,
                    phi: PhiBuildOptions {
                        cli: &cli,
                        provider: "local",
                        model: "test",
                        runtime_cfg: settings.runtime_config(&catalog),
                        context: phi_sdk::context::load_with_options_for(
                            false,
                            None,
                            Vec::new(),
                            &root,
                        ),
                        agent_options: crate::AgentBuildOptions {
                            no_tools: true,
                            ..Default::default()
                        },
                        hooks: vec![phi_sdk::hooks::Hook::session_start(move |_| {
                            assert_eq!(
                                hook_namespace.join("config.json").is_file(),
                                capability == TeamCapability::Enabled
                            );
                            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            async { Ok(Vec::new()) }
                        })],
                    },
                };
                let result = test_runtime
                    .sessions
                    .create(
                        &test_runtime.preparation,
                        session.clone(),
                        options,
                        phi_sdk::lifecycle::SessionStartReason::New,
                        false,
                    )
                    .await;
                if collision && capability == TeamCapability::Enabled {
                    assert!(result.is_err());
                    assert!(test_runtime.manager.list().unwrap().is_empty());
                    assert!(!test_runtime.sessions.has_associations().unwrap());
                    assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 0);
                    assert_eq!(
                        std::fs::read_to_string(namespace.join("foreign")).unwrap(),
                        "keep"
                    );
                    std::fs::remove_file(namespace.join("foreign")).unwrap();
                    std::fs::remove_dir(&namespace).unwrap();
                    continue;
                }
                let agent = result.unwrap();
                let expected = if capability == TeamCapability::Enabled {
                    team_namespace(session.id.as_str()).unwrap()
                } else {
                    session.id.to_string()
                };
                assert_eq!(agent.instance().task_list().unwrap().list_id(), expected);
                assert_eq!(
                    agent.session().await.task_list_id.as_deref(),
                    Some(expected.as_str())
                );
                assert_eq!(test_runtime.manager.list().unwrap().len(), 1);
                assert_eq!(started.load(std::sync::atomic::Ordering::SeqCst), 1);
                let replacement = agent
                    .runtime()
                    .prepare_replacement(&agent.session().await)
                    .await
                    .unwrap();
                assert_eq!(replacement.runtime.team_capability, capability);
                assert_eq!(
                    replacement.instance.task_list(),
                    agent.instance().task_list()
                );
                test_runtime
                    .sessions
                    .stop(
                        agent.instance().id(),
                        phi_sdk::lifecycle::SessionEndReason::Quit,
                    )
                    .await
                    .unwrap();
                assert!(!namespace.exists());
                assert_eq!(
                    std::fs::read_to_string(tasks.join("unrelated")).unwrap(),
                    "keep"
                );
                break;
            }
            std::fs::remove_dir_all(tasks).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn persistence_owner_finalizes_without_fixture_and_releases_session_lock() {
        let (root, policy, test_runtime) = super::super::preparation::tests::session_test_runtime();
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
        let instance = binding.instance();
        let id = instance.id().clone();
        let access = test_runtime.sessions.session_access(&id);
        let path = storage::session_path(&binding.session().await);
        assert!(storage::lock_session_path(&path).is_err());
        assert!(test_runtime.sessions.delete_session(id.as_str()).is_err());
        let weak_runtime = Arc::downgrade(&test_runtime);
        drop(test_runtime);
        assert!(weak_runtime.upgrade().is_none());
        assert!(Arc::ptr_eq(&access.binding().unwrap(), &binding));
        assert!(access.pending_interactions().unwrap().is_empty());
        assert!(access.subagent_view("unknown-task").await.is_err());
        access
            .binding()
            .unwrap()
            .reset_context(Some("retained history".into()))
            .await
            .unwrap();

        access.checkpoint().await.unwrap();
        instance.stop().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while access.associations.find(&id).unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(access.binding().is_err());
        assert!(access.pending_interactions().unwrap().is_empty());
        assert!(access.checkpoint().await.is_err());
        assert!(access.subagent_view("unknown-task").await.is_err());
        let saved = storage::load_session_checkpoint(id.as_str()).unwrap();
        assert_eq!(saved.id, id.as_str());
        assert_eq!(
            serde_json::to_value(&saved.transcript).unwrap(),
            serde_json::to_value(instance.transcript().await.unwrap()).unwrap(),
        );
        drop(storage::lock_session_path(&path).unwrap());
        access.associations.delete_session(id.as_str()).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn ensure_master_candidate(
    session: &mut Session,
    prepared: &PreparedAgentRuntime,
) -> anyhow::Result<()> {
    ensure_master_session(session)?;
    anyhow::ensure!(
        prepared.instance.id() == session.id.as_str(),
        "prepared Agent Instance `{}` does not match Session `{}`",
        prepared.instance.id(),
        session.id
    );
    session.bind_task_list(prepared.instance.task_list())?;
    Ok(())
}

fn ensure_master_session(session: &Session) -> anyhow::Result<()> {
    anyhow::ensure!(
        !session.is_delegated(),
        "resuming a delegated Session requires explicit validated execution configuration; read its saved history or attach to its live Agent instead"
    );
    session.validate_storage()
}

fn ensure_attachable(agent_session: &AgentSession) -> anyhow::Result<()> {
    let handle = agent_session.instance();
    let closing = handle.subscribe_finalization().is_some_and(|status| {
        !matches!(
            *status.borrow(),
            phi_sdk::agent_manager::AgentFinalizationStatus::Open
        )
    });
    anyhow::ensure!(
        !closing && handle.snapshot().status != AgentInstanceStatus::Stopped,
        "associated agent `{}` is stopped; retry stop to finish saving its Session",
        handle.id()
    );
    Ok(())
}
