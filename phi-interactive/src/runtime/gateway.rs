use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Context, ensure};
use phi_remote::{
    RemoteClient, RemoteHost, RemoteSession, RemoteSnapshot, RemoteSubmission, SavedSession,
};
use phi_sdk::agent_instance::{AgentInstanceId, AgentInstanceSnapshot};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast, mpsc, oneshot};

use super::SessionAssociations;
use super::attachment::AgentEvent;
use super::preparation::{CreateSessionRequest, SessionWorkflows};
use super::session::{
    AgentSession, AgentSessionRecovery, AgentSessionSnapshot, RuntimeDeclarations,
};
use crate::session::Session;
use phi_sdk::interaction::{InteractionEnvelope, InteractionResponse};
use phi_sdk::turn::{SubmissionDispatch, TurnPrompt};

const OBSERVATION_INTERVAL: Duration = Duration::from_millis(250);
const EMPTY_SESSION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct GatewayRecovery {
    context_window: u64,
    current_switch: String,
    declarations: RuntimeDeclarations,
    persistence_error: Option<String>,
    save_observation: super::save::SaveObservation,
    #[serde(default)]
    workspace: Option<super::WorkspaceView>,
}

impl GatewayRecovery {
    fn into_runtime_view(
        self,
        metadata: RemoteSession,
        transcript: phi_sdk::transcript::Transcript,
        snapshot: AgentInstanceSnapshot,
    ) -> super::RuntimeView {
        super::RuntimeView {
            session: session_projection(&metadata, &transcript, &self),
            snapshot: AgentSessionSnapshot {
                instance: snapshot,
                #[cfg(test)]
                runtime_generation_id: self.declarations.generation_id.clone(),
                persistence_error: self.persistence_error,
            },
            declarations: self.declarations,
        }
    }
}

type GatewaySnapshot = RemoteSnapshot<GatewayRecovery>;

struct RecoveryRequest {
    boundary: Option<String>,
    reply: oneshot::Sender<anyhow::Result<GatewaySnapshot>>,
}

struct Connection {
    snapshot: RwLock<GatewaySnapshot>,
    events: broadcast::Sender<AgentEvent>,
    recovery: mpsc::Sender<RecoveryRequest>,
    task: tokio::task::AbortHandle,
}

impl Connection {
    fn install(&self, mut next: GatewaySnapshot) -> anyhow::Result<bool> {
        let mut current = self
            .snapshot
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ensure!(
            next.session.id == current.session.id
                && next.view.snapshot.id == current.view.snapshot.id,
            "Gateway returned another Session"
        );
        if next.view.snapshot.revision < current.view.snapshot.revision {
            return Ok(false);
        }
        let changed = next.application.declarations.generation_id
            != current.application.declarations.generation_id
            || next.application.save_observation != current.application.save_observation
            || next.application.persistence_error != current.application.persistence_error
            || next.session.name != current.session.name
            || next.application.current_switch != current.application.current_switch
            || next.application.context_window != current.application.context_window
            || !next
                .interactions
                .iter()
                .map(|item| (&item.agent_id, &item.request.request_id))
                .eq(current
                    .interactions
                    .iter()
                    .map(|item| (&item.agent_id, &item.request.request_id)));
        if next.view.snapshot.active_turn_id == current.view.snapshot.active_turn_id {
            next.view.snapshot.progress = current.view.snapshot.progress.clone();
        }
        *current = next;
        Ok(changed)
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Clone)]
pub(super) struct GatewayAttachment {
    client: RemoteClient,
    id: AgentInstanceId,
    connection: Arc<Connection>,
}

impl GatewayAttachment {
    pub(super) async fn connect(client: RemoteClient, id: &str) -> anyhow::Result<Self> {
        let mut watch = client.watch(id).await?;
        let snapshot = watch.recovery::<GatewayRecovery>().await?;
        let (events, _) = broadcast::channel(256);
        let (recovery, mut requests) = mpsc::channel::<RecoveryRequest>(1);
        let (start, started) = tokio::sync::oneshot::channel::<std::sync::Weak<Connection>>();
        let observer = client.clone();
        let selected = id.to_owned();
        let task = tokio::spawn(async move {
            let Ok(connection) = started.await else {
                return;
            };
            let mut refresh = tokio::time::interval(OBSERVATION_INTERVAL);
            refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut disconnected = false;
            loop {
                let Some(current) = connection.upgrade() else {
                    break;
                };
                // Do not keep the attachment alive across network waits.
                drop(current);
                if disconnected {
                    tokio::time::sleep(OBSERVATION_INTERVAL).await;
                    let result = async {
                        let mut next_watch = observer.watch(&selected).await?;
                        let next = next_watch.recovery::<GatewayRecovery>().await?;
                        Ok::<_, anyhow::Error>((next_watch, next))
                    }
                    .await;
                    let Some(current) = connection.upgrade() else {
                        break;
                    };
                    if let Ok((next_watch, next)) = result
                        && current.install(next).is_ok()
                    {
                        watch = next_watch;
                        disconnected = false;
                        let _ = current.events.send(AgentEvent::RuntimeReplaced);
                    }
                    continue;
                }
                let result: anyhow::Result<()> = tokio::select! {
                    request = requests.recv() => {
                        let Some(request) = request else { break };
                        let recovered = watch.recovery::<GatewayRecovery>().await;
                        let Some(current) = connection.upgrade() else { break };
                        let recovered = recovered.and_then(|next| {
                            current.install(next.clone())?;
                            if let Some(id) = request.boundary {
                                let _ = current.events.send(AgentEvent::RecoveryBoundary { id });
                            }
                            Ok(next)
                        });
                        let failure = recovered.as_ref().err().map(ToString::to_string);
                        let _ = request.reply.send(recovered);
                        failure.map_or(Ok(()), |message| Err(anyhow::anyhow!(message)))
                    }
                    event = watch.next() => {
                        let Some(current) = connection.upgrade() else { break };
                        event.map(|event| {
                            let event = match event {
                                phi_remote::RemoteEvent::Phi(envelope) => AgentEvent::Phi(envelope),
                                phi_remote::RemoteEvent::Progress(progress) => {
                                    current.snapshot.write().unwrap_or_else(std::sync::PoisonError::into_inner).view.snapshot.progress = Some(progress);
                                    AgentEvent::Other
                                }
                                phi_remote::RemoteEvent::Boundary { .. } => AgentEvent::Other,
                                phi_remote::RemoteEvent::Refresh { .. } => AgentEvent::RuntimeReplaced,
                            };
                            let _ = current.events.send(event);
                        })
                    }
                    _ = refresh.tick() => {
                        let next = observer.recovery::<GatewayRecovery>(&selected).await;
                        let Some(current) = connection.upgrade() else { break };
                        next.and_then(|next| current.install(next)).map(|changed| {
                            if changed {
                                let _ = current.events.send(AgentEvent::RuntimeReplaced);
                            }
                        })
                    }
                };
                if let Err(error) = result {
                    disconnected = true;
                    if let Some(current) = connection.upgrade() {
                        let _ = current.events.send(AgentEvent::ObservationFailed {
                            message: format!("Gateway observation failed; view is stale: {error}"),
                        });
                    }
                }
            }
        });
        let connection = Arc::new(Connection {
            snapshot: RwLock::new(snapshot),
            events,
            recovery,
            task: task.abort_handle(),
        });
        start
            .send(Arc::downgrade(&connection))
            .map_err(|_| anyhow::anyhow!("Gateway observer failed to start"))?;
        Ok(Self {
            client,
            id: AgentInstanceId::new(id),
            connection,
        })
    }

    pub(super) async fn configure(
        &self,
        selection: super::preparation::RuntimeSelection,
    ) -> anyhow::Result<super::RuntimeView> {
        let selection = match selection {
            super::preparation::RuntimeSelection::Model { selection } => {
                phi_remote::RemoteSelection::Model { selection }
            }
            super::preparation::RuntimeSelection::Switch { name } => {
                phi_remote::RemoteSelection::Switch { name }
            }
            super::preparation::RuntimeSelection::ThinkingLevel { level } => {
                phi_remote::RemoteSelection::ThinkingLevel { level }
            }
        };
        self.accept_update(self.client.configure(self.id.as_str(), selection).await?)
    }

    pub(super) async fn reload(&self) -> anyhow::Result<super::RuntimeView> {
        self.accept_update(self.client.reload(self.id.as_str()).await?)
    }

    fn accept_update(
        &self,
        accepted: phi_remote::RemoteRuntimeUpdate<GatewayRecovery>,
    ) -> anyhow::Result<super::RuntimeView> {
        ensure!(
            accepted.session.id == self.id.as_str() && accepted.snapshot.id == self.id,
            "Gateway returned another Session"
        );
        Ok(accepted.application.into_runtime_view(
            accepted.session,
            accepted.transcript,
            accepted.snapshot,
        ))
    }

    pub(super) async fn catalog(&self) -> anyhow::Result<super::preparation::RuntimeCatalog> {
        let catalog = self.client.catalog(self.id.as_str()).await?;
        Ok(super::preparation::RuntimeCatalog {
            switches: catalog.switches,
            models: catalog
                .models
                .into_iter()
                .map(|choice| super::preparation::ModelChoice {
                    provider: choice.provider,
                    model: choice.model,
                    complete: choice.complete,
                })
                .collect(),
        })
    }

    pub(super) async fn new_session(&self) -> anyhow::Result<Self> {
        let session = self.client.new_session(self.id.as_str()).await?;
        Self::connect(self.client.clone(), &session.id).await
    }

    pub(super) async fn resume_session(&self, id: &str) -> anyhow::Result<Self> {
        let session = self.client.resume(id).await?;
        Self::connect(self.client.clone(), &session.id).await
    }

    pub(super) async fn saved_sessions(
        &self,
        query: &str,
    ) -> anyhow::Result<Vec<crate::session::discovery::SessionSummary>> {
        Ok(self
            .client
            .saved(query)
            .await?
            .into_iter()
            .map(|summary| crate::session::discovery::SessionSummary {
                id: summary.id.into(),
                name: summary.name.into(),
                updated_at: summary.updated_at.into(),
                first_user_prompt: summary.first_user_prompt.map(Into::into),
            })
            .collect())
    }

    pub(super) async fn dispatch(
        &self,
        prompt: phi_sdk::turn::TurnPrompt,
        dispatch: phi_sdk::turn::SubmissionDispatch,
    ) -> anyhow::Result<phi_remote::RemoteSubmission> {
        self.client.submit(self.id.as_str(), prompt, dispatch).await
    }

    pub(super) async fn abort(&self) -> anyhow::Result<()> {
        self.client.cancel(self.id.as_str()).await?;
        Ok(())
    }

    pub(super) fn id(&self) -> &AgentInstanceId {
        &self.id
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.connection.events.subscribe()
    }

    pub(super) fn snapshot(&self) -> AgentInstanceSnapshot {
        self.connection
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .view
            .snapshot
            .clone()
    }

    pub(super) fn pending_interactions(&self) -> Vec<phi_sdk::interaction::InteractionEnvelope> {
        self.connection
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .interactions
            .iter()
            .map(|interaction| interaction.request.clone())
            .collect()
    }

    pub(super) async fn respond(
        &self,
        request: &str,
        decision: phi_sdk::interaction::PermissionDecision,
    ) -> anyhow::Result<Option<String>> {
        let agent = self
            .connection
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .interactions
            .iter()
            .find(|interaction| interaction.request.request_id == request)
            .map(|interaction| interaction.agent_id.clone())
            .context("interaction is no longer pending")?;
        let receipt = self
            .client
            .permission(self.id.as_str(), &agent, request, decision)
            .await?;
        ensure!(
            receipt["status"] == "answered",
            "unexpected Gateway permission receipt"
        );
        Ok(receipt["persistenceWarning"].as_str().map(str::to_owned))
    }

    pub(super) fn session(&self) -> Session {
        let observation = self
            .connection
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session_projection(
            &observation.session,
            &observation.view.transcript,
            &observation.application,
        )
    }

    pub(super) fn workspace(&self) -> super::WorkspaceView {
        let observation = self
            .connection
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation
            .application
            .workspace
            .clone()
            .unwrap_or_else(|| super::WorkspaceView {
                working_dir: observation.session.working_dir.clone(),
                git_branch: None,
            })
    }

    pub(super) fn save_observation(&self) -> super::save::SaveObservation {
        self.connection
            .snapshot
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .application
            .save_observation
            .clone()
    }

    pub(super) async fn recovery(
        &self,
        boundary: Option<String>,
    ) -> anyhow::Result<AgentSessionRecovery> {
        let (reply, response) = oneshot::channel();
        self.connection
            .recovery
            .send(RecoveryRequest { boundary, reply })
            .await
            .context("Gateway observer stopped")?;
        let next = response.await.context("Gateway observer stopped")??;
        let accepted = next.application.into_runtime_view(
            next.session,
            next.view.transcript,
            next.view.snapshot,
        );
        Ok(AgentSessionRecovery {
            pending_submission_ids: next.view.pending_submission_ids,
            session: accepted.session,
            snapshot: accepted.snapshot,
            declarations: accepted.declarations,
        })
    }
}

fn session_projection(
    metadata: &RemoteSession,
    transcript: &phi_sdk::transcript::Transcript,
    application: &GatewayRecovery,
) -> Session {
    let mut session = crate::session::new(
        &metadata.provider,
        &metadata.model,
        application.context_window,
        &metadata.working_dir,
    );
    session.current_switch = application.current_switch.as_str().into();
    session.name = metadata.name.as_str().into();
    session.set_transcript(transcript.clone());
    session
}

struct HostedSession {
    workspace: AsyncMutex<super::workspace::WorkspaceCache>,
    binding: Arc<AgentSession>,
    first_submission: Arc<AsyncMutex<bool>>,
}

impl HostedSession {
    fn new(binding: Arc<AgentSession>) -> Self {
        Self {
            workspace: AsyncMutex::new(super::workspace::WorkspaceCache::default()),
            binding,
            first_submission: Arc::new(AsyncMutex::new(false)),
        }
    }

    async fn metadata(&self) -> RemoteSession {
        remote_session(&self.binding.session().await)
    }

    fn schedule_expiry(
        self: &Arc<Self>,
        associations: &Arc<SessionAssociations>,
        roots: &Arc<Mutex<BTreeMap<String, Arc<HostedSession>>>>,
        lifecycle: &Arc<AsyncMutex<()>>,
        timeout: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + timeout;
        let root = Arc::downgrade(self);
        let associations = Arc::downgrade(associations);
        let roots = Arc::downgrade(roots);
        let lifecycle = Arc::downgrade(lifecycle);
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let (Some(root), Some(associations), Some(roots), Some(lifecycle)) = (
                root.upgrade(),
                associations.upgrade(),
                roots.upgrade(),
                lifecycle.upgrade(),
            ) else {
                return;
            };
            let submitted = root.first_submission.lock().await;
            if *submitted {
                return;
            }
            let _lifecycle = lifecycle.lock().await;
            let current = roots.lock().ok().is_some_and(|roots| {
                roots
                    .get(root.binding.instance().id().as_str())
                    .is_some_and(|current| Arc::ptr_eq(current, &root))
            });
            if current
                && let Err(error) = GatewaySessions::close_root(&associations, &roots, &root).await
            {
                tracing::warn!(session = %root.binding.instance().id(), "empty Gateway Session cleanup failed: {error}");
            }
        });
    }
}

pub(super) struct GatewaySessions {
    associations: Arc<SessionAssociations>,
    workflows: SessionWorkflows,
    preparation: Arc<super::RuntimePreparation>,
    persist: bool,
    roots: Arc<Mutex<BTreeMap<String, Arc<HostedSession>>>>,
    lifecycle: Arc<AsyncMutex<()>>,
}

fn remote_session(session: &Session) -> RemoteSession {
    RemoteSession {
        id: session.id.to_string(),
        name: session.name.to_string(),
        provider: session.provider.to_string(),
        model: session.model.to_string(),
        working_dir: session.working_dir.as_str().into(),
    }
}

impl GatewaySessions {
    pub(super) fn new(
        associations: Arc<SessionAssociations>,
        workflows: SessionWorkflows,
        preparation: Arc<super::RuntimePreparation>,
        persist: bool,
    ) -> Self {
        Self {
            associations,
            workflows,
            preparation,
            persist,
            lifecycle: Arc::new(AsyncMutex::new(())),
            roots: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    #[cfg(test)]
    pub(super) fn with_test_binding(self, binding: Arc<AgentSession>) -> Self {
        self.roots.lock().unwrap().insert(
            binding.instance().id().to_string(),
            Arc::new(HostedSession::new(binding)),
        );
        self
    }

    fn root(&self, id: &AgentInstanceId) -> anyhow::Result<Arc<HostedSession>> {
        self.roots
            .lock()
            .map_err(|_| anyhow::anyhow!("remote Session registry poisoned"))?
            .get(id.as_str())
            .cloned()
            .context("unknown gateway Session")
    }

    async fn accept(
        &self,
        creation: super::associations::SessionCreation,
        lifecycle: OwnedMutexGuard<()>,
        expiry: Option<Duration>,
    ) -> anyhow::Result<Arc<AgentSession>> {
        let registry = self.roots.clone();
        let associations = self.associations.clone();
        let lifecycle_gate = self.lifecycle.clone();
        creation
            .start_with_publication(move |binding| {
                let _lifecycle = lifecycle;
                let mut roots = registry
                    .lock()
                    .map_err(|_| anyhow::anyhow!("remote Session registry poisoned"))?;
                let id = binding.instance().id().to_string();
                ensure!(!roots.contains_key(&id), "Gateway Session already hosted");
                let root = Arc::new(HostedSession::new(binding.clone()));
                roots.insert(id, root.clone());
                if let Some(timeout) = expiry {
                    root.schedule_expiry(&associations, &registry, &lifecycle_gate, timeout);
                }
                Ok(())
            })
            .await
    }

    fn binding(&self, id: &AgentInstanceId) -> anyhow::Result<Arc<AgentSession>> {
        Ok(self.root(id)?.binding.clone())
    }

    async fn close_root(
        associations: &Arc<SessionAssociations>,
        roots: &Mutex<BTreeMap<String, Arc<HostedSession>>>,
        root: &HostedSession,
    ) -> anyhow::Result<()> {
        associations
            .stop(
                root.binding.instance().id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await?;
        roots
            .lock()
            .map_err(|_| anyhow::anyhow!("remote Session registry poisoned"))?
            .remove(root.binding.instance().id().as_str());
        Ok(())
    }
}

fn accepted_update(
    accepted: super::RuntimeView,
    save_observation: super::save::SaveObservation,
) -> phi_remote::RemoteRuntimeUpdate<GatewayRecovery> {
    phi_remote::RemoteRuntimeUpdate {
        session: remote_session(&accepted.session),
        transcript: accepted.session.transcript,
        snapshot: accepted.snapshot.instance,
        application: GatewayRecovery {
            context_window: accepted.session.context_window,
            current_switch: accepted.session.current_switch.to_string(),
            declarations: accepted.declarations,
            persistence_error: accepted.snapshot.persistence_error,
            save_observation,
            workspace: None,
        },
    }
}

impl GatewaySessions {
    async fn recover_root(
        &self,
        id: AgentInstanceId,
        boundary: Option<String>,
    ) -> anyhow::Result<GatewaySnapshot> {
        let binding = self.binding(&id)?;
        let recovery = binding.recovery_at_event_boundary(boundary).await?;
        let workspace = self
            .root(&id)?
            .workspace
            .lock()
            .await
            .observe(std::path::Path::new(recovery.session.working_dir.as_str()))
            .await;
        let interactions = recovery
            .snapshot
            .instance
            .pending_interactions
            .iter()
            .cloned()
            .map(|request| phi_remote::RemoteInteraction {
                agent_id: id.to_string(),
                request,
            })
            .collect();
        Ok(RemoteSnapshot {
            session: remote_session(&recovery.session),
            view: phi_sdk::agent_instance::AgentInstanceView {
                pending_submission_ids: recovery.pending_submission_ids,
                transcript: recovery.session.transcript,
                snapshot: recovery.snapshot.instance,
            },
            interactions,
            application: GatewayRecovery {
                context_window: recovery.session.context_window,
                current_switch: recovery.session.current_switch.to_string(),
                declarations: recovery.declarations,
                persistence_error: recovery.snapshot.persistence_error,
                save_observation: binding.save_observation().await,
                workspace: Some(workspace),
            },
        })
    }
}

impl RemoteHost for GatewaySessions {
    type Recovery = GatewayRecovery;

    async fn recovery(&self, id: AgentInstanceId) -> anyhow::Result<GatewaySnapshot> {
        self.recover_root(id, None).await
    }

    async fn recovery_at_boundary(
        &self,
        id: AgentInstanceId,
        boundary: String,
    ) -> anyhow::Result<GatewaySnapshot> {
        self.recover_root(id, Some(boundary)).await
    }

    async fn configure(
        &self,
        id: AgentInstanceId,
        selection: phi_remote::RemoteSelection,
    ) -> anyhow::Result<phi_remote::RemoteRuntimeUpdate<GatewayRecovery>> {
        let binding = self.binding(&id)?;
        let selection = match selection {
            phi_remote::RemoteSelection::Model { selection } => {
                super::preparation::RuntimeSelection::Model { selection }
            }
            phi_remote::RemoteSelection::Switch { name } => {
                super::preparation::RuntimeSelection::Switch { name }
            }
            phi_remote::RemoteSelection::ThinkingLevel { level } => {
                super::preparation::RuntimeSelection::ThinkingLevel { level }
            }
        };
        let accepted = self
            .preparation
            .configure_runtime(&self.associations.session_access(&id), selection)
            .await?;
        Ok(accepted_update(accepted, binding.save_observation().await))
    }

    async fn reload(
        &self,
        id: AgentInstanceId,
    ) -> anyhow::Result<phi_remote::RemoteRuntimeUpdate<GatewayRecovery>> {
        let binding = self.binding(&id)?;
        let accepted = self
            .preparation
            .reload_resources(&self.associations.session_access(&id))
            .await?;
        Ok(accepted_update(accepted, binding.save_observation().await))
    }

    async fn catalog(&self, id: AgentInstanceId) -> anyhow::Result<phi_remote::RemoteCatalog> {
        self.root(&id)?;
        let catalog = self
            .preparation
            .runtime_catalog(&self.associations.session_access(&id))
            .await?;
        Ok(phi_remote::RemoteCatalog {
            switches: catalog.switches,
            models: catalog
                .models
                .into_iter()
                .map(|choice| phi_remote::RemoteModel {
                    provider: choice.provider,
                    model: choice.model,
                    complete: choice.complete,
                })
                .collect(),
        })
    }

    async fn new_session(&self, source: AgentInstanceId) -> anyhow::Result<RemoteSession> {
        let lifecycle = self.lifecycle.clone().lock_owned().await;
        self.root(&source)?;
        let creation = self
            .workflows
            .prepare_new_from(&source, self.persist)
            .await?;
        let binding = self
            .accept(creation, lifecycle, Some(EMPTY_SESSION_TIMEOUT))
            .await?;
        Ok(remote_session(&binding.session().await))
    }

    async fn sessions(&self) -> anyhow::Result<Vec<RemoteSession>> {
        let roots: Vec<_> = self
            .roots
            .lock()
            .map_err(|_| anyhow::anyhow!("remote Session registry poisoned"))?
            .values()
            .cloned()
            .collect();
        let mut sessions = Vec::with_capacity(roots.len());
        for root in roots {
            sessions.push(root.metadata().await);
        }
        Ok(sessions)
    }

    async fn create(&self, working_dir: std::path::PathBuf) -> anyhow::Result<RemoteSession> {
        anyhow::ensure!(working_dir.is_absolute(), "workingDir must be absolute");
        let lifecycle = self.lifecycle.clone().lock_owned().await;
        let creation = self
            .workflows
            .prepare_new_session_in(
                CreateSessionRequest {
                    provider: None,
                    model: None,
                    switch: None,
                    persist: self.persist,
                },
                Some(working_dir),
            )
            .await?;
        let binding = self
            .accept(creation, lifecycle, Some(EMPTY_SESSION_TIMEOUT))
            .await?;
        Ok(remote_session(&binding.session().await))
    }

    async fn saved(&self, query: String) -> anyhow::Result<Vec<SavedSession>> {
        Ok(crate::session::discovery::discover_sessions(&query)?
            .into_iter()
            .map(|session| SavedSession {
                id: session.id.to_string(),
                name: session.name.to_string(),
                updated_at: session.updated_at.to_string(),
                first_user_prompt: session.first_user_prompt.map(|prompt| prompt.to_string()),
            })
            .collect())
    }

    async fn resume(&self, id: String) -> anyhow::Result<RemoteSession> {
        let lifecycle = self.lifecycle.clone().lock_owned().await;
        if let Ok(root) = self.root(&AgentInstanceId::new(&id)) {
            return Ok(root.metadata().await);
        }
        let creation = self
            .workflows
            .prepare_saved_session(&id, self.persist)
            .await?;
        let binding = self.accept(creation, lifecycle, None).await?;
        Ok(remote_session(&binding.session().await))
    }

    async fn close(&self, id: AgentInstanceId) -> anyhow::Result<()> {
        let lifecycle = self.lifecycle.clone();
        let roots = self.roots.clone();
        let associations = self.associations.clone();
        tokio::spawn(async move {
            let _lifecycle = lifecycle.lock().await;
            let root = roots
                .lock()
                .map_err(|_| anyhow::anyhow!("remote Session registry poisoned"))?
                .get(id.as_str())
                .cloned()
                .context("unknown gateway Session")?;
            Self::close_root(&associations, &roots, &root).await
        })
        .await?
    }

    async fn submit(
        &self,
        session: AgentInstanceId,
        prompt: TurnPrompt,
        dispatch: SubmissionDispatch,
    ) -> anyhow::Result<RemoteSubmission> {
        let binding = self.binding(&session)?;
        let root = self.root(&session)?;
        let mut submitted = root.first_submission.clone().lock_owned().await;
        // Keep admission and expiry serialized even if the submitting client disconnects.
        tokio::spawn(async move {
            let submission = binding.submit(prompt, None, Some(dispatch)).await?;
            *submitted = true;
            Ok(RemoteSubmission {
                disposition: submission.disposition,
                notices: submission.notices,
            })
        })
        .await?
    }

    async fn message(
        &self,
        session: AgentInstanceId,
        target: AgentInstanceId,
        message: String,
    ) -> anyhow::Result<phi_sdk::agent_manager::AgentMessageReceipt> {
        self.root(&session)?;
        self.associations
            .session_access(&session)
            .send_message(&target, message)
            .await
    }

    async fn interactions(
        &self,
        session: AgentInstanceId,
    ) -> anyhow::Result<Vec<(AgentInstanceId, Vec<InteractionEnvelope>)>> {
        self.root(&session)?;
        Ok(self
            .associations
            .session_access(&session)
            .interaction_sessions()?
            .into_iter()
            .map(|binding| {
                let snapshot = binding.instance().snapshot();
                (snapshot.id, snapshot.pending_interactions)
            })
            .collect())
    }

    async fn respond(
        &self,
        session: AgentInstanceId,
        agent: AgentInstanceId,
        request: String,
        response: InteractionResponse,
    ) -> anyhow::Result<Option<String>> {
        self.root(&session)?;
        let binding = self
            .associations
            .session_access(&session)
            .interaction_sessions()?
            .into_iter()
            .find(|binding| binding.instance().id() == &agent)
            .ok_or(phi_remote::protocol::ErrorCode::Forbidden)?;
        Ok(binding
            .respond_interaction(request, response)
            .await?
            .persistence_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::attachment::AgentAttachment;
    use crate::runtime::preparation::{SessionWorkflows, tests::session_test_runtime};

    async fn host_session(host: GatewaySessions, session: Session) -> GatewaySessions {
        let expiry = session
            .transcript
            .entries
            .is_empty()
            .then_some(EMPTY_SESSION_TIMEOUT);
        let creation = host
            .workflows
            .prepare_session(session, host.persist, None)
            .await
            .unwrap();
        let lifecycle = host.lifecycle.clone().lock_owned().await;
        host.accept(creation, lifecycle, expiry).await.unwrap();
        host
    }

    #[tokio::test]
    async fn empty_gateway_sessions_expire_despite_polling() {
        let (directory, policy, runtime) = session_test_runtime();
        let target = directory.join("target");
        std::fs::create_dir_all(target.join(".phi")).unwrap();
        std::fs::write(
            target.join(".phi/settings.json"),
            r#"{"defaultProvider":"local","defaultModel":"test"}"#,
        )
        .unwrap();
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let unknown_id = AgentInstanceId::new("unknown");
        let host = GatewaySessions::new(
            runtime.sessions.clone(),
            workflows,
            runtime.preparation.clone(),
            false,
        );
        assert!(host.sessions().await.unwrap().is_empty());
        assert!(host.interactions(unknown_id.clone()).await.is_err());
        assert!(
            host.respond(
                unknown_id.clone(),
                unknown_id,
                "unknown".into(),
                InteractionResponse::Permission(phi_sdk::interaction::PermissionDecision::Deny),
            )
            .await
            .is_err()
        );
        assert!(runtime.manager.list().unwrap().is_empty());
        let created = host.create(target).await.unwrap();
        let created_id = AgentInstanceId::new(created.id);
        let inherited = host.new_session(created_id.clone()).await.unwrap();
        let inherited_id = AgentInstanceId::new(inherited.id);
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(299)).await;
        host.recovery(created_id.clone()).await.unwrap();
        assert_eq!(host.sessions().await.unwrap().len(), 2);
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !host.sessions().await.unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("empty Sessions should expire after five minutes");
        for id in [created_id, inherited_id] {
            assert!(host.recovery(id.clone()).await.is_err());
            assert!(runtime.manager.get(&id).is_err());
        }
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn gateway_expiry_waits_for_submission_after_waiter_loss() {
        use super::super::preparation::tests::session_test_runtime_with_hooks;
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let hook = phi_sdk::hooks::Hook::user_prompt_submit({
            let entered = entered.clone();
            let release = release.clone();
            move |_| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Handled { reason: None })
                }
            }
        });
        let (directory, policy, runtime) = session_test_runtime_with_hooks(vec![hook]);
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut seed =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        seed.working_dir = directory.to_string_lossy().as_ref().into();
        let id = AgentInstanceId::new(seed.id.as_str());
        let path = crate::session::storage::session_path(&seed);
        let host = Arc::new(
            host_session(
                GatewaySessions::new(
                    runtime.sessions.clone(),
                    workflows,
                    runtime.preparation.clone(),
                    false,
                ),
                seed,
            )
            .await,
        );
        let waiter = tokio::spawn({
            let host = host.clone();
            let id = id.clone();
            async move {
                host.submit(
                    id,
                    TurnPrompt {
                        prompt: "hello".into(),
                        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                        injected_context: Vec::new(),
                        images: Vec::new(),
                        tool_policy: Default::default(),
                    },
                    SubmissionDispatch::FollowUp,
                )
                .await
            }
        });
        entered.notified().await;
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(300)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert_eq!(host.sessions().await.unwrap().len(), 1);
        release.notify_one();
        host.recovery(id.clone()).await.unwrap();
        tokio::time::advance(Duration::from_secs(600)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        host.recovery(id.clone()).await.unwrap();
        assert_eq!(host.sessions().await.unwrap().len(), 1);
        assert!(
            !path.exists(),
            "accepted input must prevent expiry without a saved file"
        );
        host.close(id).await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn gateway_expiry_excludes_resumed_history() {
        if super::super::preparation::tests::run_isolated_test(concat!(
            module_path!(),
            "::gateway_expiry_excludes_resumed_history"
        )) {
            return;
        }
        let (directory, policy, runtime) = session_test_runtime();
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut seed =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        seed.working_dir = directory.to_string_lossy().as_ref().into();
        seed.append_user_message("retained history");
        seed.append_assistant_message(
            None,
            None,
            Vec::new(),
            "test".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "local".into(),
            Default::default(),
            "stop".into(),
        );
        let id = AgentInstanceId::new(seed.id.as_str());
        crate::session::storage::save_session(&seed).unwrap();
        let host = GatewaySessions::new(
            runtime.sessions.clone(),
            workflows,
            runtime.preparation.clone(),
            false,
        );
        host.resume(id.to_string()).await.unwrap();
        host.recovery(id.clone()).await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(600)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        host.recovery(id.clone()).await.unwrap();
        host.close(id.clone()).await.unwrap();
        host.resume(id.to_string()).await.unwrap();
        tokio::time::advance(Duration::from_secs(600)).await;
        tokio::time::sleep(Duration::from_millis(1)).await;
        host.recovery(id.clone()).await.unwrap();
        host.close(id).await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn accepted_gateway_bindings_survive_waiter_loss() {
        if super::super::preparation::tests::run_isolated_test(concat!(
            module_path!(),
            "::accepted_gateway_bindings_survive_waiter_loss"
        )) {
            return;
        }
        use super::super::preparation::tests::session_test_runtime_with_hooks;
        for operation in ["create", "resume"] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let hook = phi_sdk::hooks::Hook::session_start({
                let entered = entered.clone();
                let release = release.clone();
                move |_| {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(Vec::new())
                    }
                }
            });
            let (directory, policy, runtime) = session_test_runtime_with_hooks(vec![hook]);
            let target = directory.join("target");
            std::fs::create_dir_all(target.join(".phi")).unwrap();
            std::fs::write(
                target.join(".phi/settings.json"),
                r#"{"defaultProvider":"local","defaultModel":"test"}"#,
            )
            .unwrap();
            policy.install(&runtime.preparation);
            let workflows =
                SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone())
                    .unwrap();
            let mut seed =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            seed.working_dir = directory.to_string_lossy().as_ref().into();
            let id = AgentInstanceId::new(seed.id.as_str());
            if operation == "resume" {
                seed.append_user_message("saved history");
                seed.append_assistant_message(
                    None,
                    None,
                    Vec::new(),
                    "test".into(),
                    phi_sdk::providers::ProviderApi::OpenAiCompletions,
                    "local".into(),
                    Default::default(),
                    "stop".into(),
                );
                crate::session::storage::save_session(&seed).unwrap();
            }
            let host = Arc::new(GatewaySessions::new(
                runtime.sessions.clone(),
                workflows,
                runtime.preparation.clone(),
                false,
            ));
            let mut waiter = tokio::spawn({
                let host = host.clone();
                let id = id.clone();
                async move {
                    match operation {
                        "create" => host.create(target).await.map(|_| ()),
                        "resume" => host.resume(id.to_string()).await.map(|_| ()),
                        _ => unreachable!(),
                    }
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::select! {
                    _ = entered.notified() => {},
                    result = &mut waiter => panic!("{operation} completed before startup: {result:?}"),
                }
            }).await.expect(operation);
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            let accepted = runtime.manager.list().unwrap();
            assert_eq!(accepted.len(), 1);
            let accepted_id = accepted[0].id.clone();
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if host.root(&accepted_id).is_ok() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect(operation);
            assert!(
                host.sessions()
                    .await
                    .unwrap()
                    .iter()
                    .any(|session| session.id == accepted_id.as_str())
            );
            host.recovery(accepted_id.clone()).await.unwrap();
            assert_eq!(runtime.manager.list().unwrap().len(), 1);
            host.close(accepted_id).await.unwrap();
            runtime
                .sessions
                .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
                .await
                .unwrap();
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn gateway_selection_ownership() {
        if super::super::preparation::tests::run_isolated_test(concat!(
            module_path!(),
            "::gateway_selection_ownership"
        )) {
            return;
        }
        let (root, policy, runtime) = session_test_runtime();
        let previous_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(
            root.join("models.json"),
            serde_json::to_vec(&serde_json::json!({"providers":{"local":{
                "api":"openai-completions",
                "baseUrl":format!("http://{}/v1", provider.local_addr().unwrap()),
                "models":{"test":{"contextWindow":128000},"selected":{"contextWindow":64000}}
            }}}))
            .unwrap(),
        )
        .unwrap();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap();
        super::super::preparation::tests::install_reload_catalog(&root, &catalog);
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut seed =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        seed.working_dir = root.to_string_lossy().as_ref().into();
        seed.append_user_message("retained history");
        seed.transcript.append_assistant_message(
            None,
            None,
            Vec::new(),
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "stop".into(),
        );
        crate::session::storage::save_session(&seed).unwrap();
        let id = seed.id.to_string();
        std::fs::write(root.join("app.js"), "").unwrap();
        let settings_path = root.join(".phi/settings.json");
        std::fs::create_dir_all(settings_path.parent().unwrap()).unwrap();
        let settings = r#"{"defaultProvider":"local","defaultModel":"test","no_tools":true}"#;
        std::fs::write(&settings_path, settings).unwrap();
        let skill = root.join(".phi/skills/review/SKILL.md");
        std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
        std::fs::write(
            &skill,
            "---\nname: review\ndescription: hosted skill\n---\nserver-only skill contents",
        )
        .unwrap();
        let gateway_directory = crate::cli::web::gateway_directory();
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&gateway_directory).unwrap();
        let mut config_options = std::fs::OpenOptions::new();
        config_options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            config_options.mode(0o600);
        }
        std::io::Write::write_all(
            &mut config_options.open(gateway_directory.join("config.json")).unwrap(),
            br#"{"port":0,"publicOrigin":"https://phi.example.com","trustedProxies":["127.0.0.1"]}"#,
        )
        .unwrap();
        let (gateway, registration, endpoint) = phi_remote::deployment::start(
            &gateway_directory,
            runtime.manager.clone(),
            workflows.remote_host(false),
            root.clone(),
            Some(0),
        )
        .await
        .unwrap();
        assert_eq!(gateway.url(), "https://phi.example.com/");
        assert!(gateway.local_url().starts_with("http://127.0.0.1:"));
        let discovered = crate::cli::web::existing().await.unwrap().unwrap();
        assert_eq!(discovered.url(), gateway.url());
        let client = discovered.client().unwrap();
        assert!(
            RemoteClient::connect(gateway.local_url(), &"f".repeat(64))
                .unwrap()
                .sessions()
                .await
                .is_err()
        );
        assert!(runtime.manager.list().unwrap().is_empty());
        assert!(
            crate::cli::web::select_session(&client, Some("unknown"), &root)
                .await
                .is_err()
        );
        assert!(client.sessions().await.unwrap().is_empty());
        let client_workspace = root.join("client-workspace");
        std::fs::create_dir_all(client_workspace.join(".phi")).unwrap();
        std::fs::write(client_workspace.join(".phi/settings.json"), settings).unwrap();
        let created = crate::cli::web::select_session(&client, None, &client_workspace)
            .await
            .unwrap();
        assert_eq!(created.working_dir, client_workspace);
        client.close_session(&created.id).await.unwrap();
        assert!(client.sessions().await.unwrap().is_empty());
        client.resume(&id).await.unwrap();
        for selection in [None, Some(id.as_str())] {
            let selected =
                crate::cli::web::select_session(&client, selection, &root.join("missing"))
                    .await
                    .unwrap();
            assert_eq!(selected.id, id);
        }
        assert_eq!(client.sessions().await.unwrap().len(), 1);
        let mut watch = client.watch(&id).await.unwrap();
        let initial = watch.recovery::<GatewayRecovery>().await.unwrap();
        assert_eq!(initial.session.id, id);
        drop(watch);
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = proxy.local_addr().unwrap();
        let target = gateway
            .local_url()
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        let (disconnect, _) = broadcast::channel::<()>(1);
        let sockets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let submissions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let proxy_task = tokio::spawn({
            let disconnect = disconnect.clone();
            let sockets = sockets.clone();
            let submissions = submissions.clone();
            async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    tokio::select! {
                        accepted = proxy.accept() => {
                            let (mut downstream, _) = accepted.unwrap();
                            let target = target.clone();
                            let mut disconnect = disconnect.subscribe();
                            let sockets = sockets.clone();
                            let submissions = submissions.clone();
                            connections.spawn(async move {
                                const MAX_HEADER_BYTES: usize = 16 * 1024;
                                let mut header = Vec::new();
                                while !header.ends_with(b"\r\n\r\n") {
                                    if header.len() == MAX_HEADER_BYTES {
                                        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                                    }
                                    header.push(downstream.read_u8().await?);
                                }
                                let header = String::from_utf8(header)
                                    .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
                                let observing = header.starts_with("GET /ws ");
                                if observing {
                                    sockets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                }
                                let submitting = header.lines().next().unwrap().split_whitespace().nth(1)
                                    .is_some_and(|path| path.ends_with("/submit"));
                                if submitting {
                                    submissions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                }
                                let mut header = header.replace(&proxy_address.to_string(), &target);
                                if !observing {
                                    header = header.replace("\r\n\r\n", "\r\nConnection: close\r\n\r\n");
                                }
                                let mut upstream = tokio::net::TcpStream::connect(&target).await?;
                                upstream.write_all(header.as_bytes()).await?;
                                if submitting {
                                    let length: u64 = header.lines().find_map(|line| {
                                        line.to_ascii_lowercase().strip_prefix("content-length:")
                                            .map(|value| value.trim().parse().unwrap())
                                    }).unwrap();
                                    tokio::io::copy(&mut downstream.take(length), &mut upstream).await?;
                                    let mut response = Vec::new();
                                    upstream.take(MAX_HEADER_BYTES as u64).read_to_end(&mut response).await?;
                                    assert!(response.starts_with(b"HTTP/1.1 200"));
                                    return Ok(());
                                }
                                tokio::select! {
                                    result = tokio::io::copy_bidirectional(&mut downstream, &mut upstream) => { result?; }
                                    _ = disconnect.recv(), if observing => {}
                                }
                                Ok::<_, std::io::Error>(())
                            });
                        }
                        result = connections.join_next(), if !connections.is_empty() => {
                            let _ = result.unwrap().expect("proxy connection task panicked");
                        }
                    }
                }
            }
        });
        let endpoint_record = serde_json::to_value(&endpoint).unwrap();
        let observed_client = RemoteClient::connect(
            &format!("http://{proxy_address}/"),
            endpoint_record["localCredential"].as_str().unwrap(),
        )
        .unwrap();
        let attachment = AgentAttachment::connect_gateway(observed_client, &id)
            .await
            .unwrap();
        let accepted = attachment
            .configure_runtime(super::super::preparation::RuntimeSelection::Switch {
                name: "plan".into(),
            })
            .await
            .unwrap();
        assert_eq!(accepted.session.current_switch, "plan");
        assert_eq!(accepted.session.model, "test");
        assert!(accepted.session.switch_snapshots.is_empty());
        assert_eq!(accepted.session.entries.len(), 2);
        let observed = client.recovery::<GatewayRecovery>(&id).await.unwrap();
        assert_eq!(observed.application.current_switch, "plan");
        let binding = runtime.sessions.get(&AgentInstanceId::new(&id)).unwrap();
        assert_eq!(binding.session().await.current_switch, "plan");
        let before = serde_json::to_value(&observed.application.declarations).unwrap();
        let history = serde_json::to_value(&observed.view.transcript).unwrap();
        for selection in [
            super::super::preparation::RuntimeSelection::Switch {
                name: "missing-switch".into(),
            },
            super::super::preparation::RuntimeSelection::Model {
                selection: "model:local:missing-model".into(),
            },
        ] {
            assert!(attachment.configure_runtime(selection).await.is_err());
            let unchanged = client.recovery::<GatewayRecovery>(&id).await.unwrap();
            assert_eq!(
                serde_json::to_value(&unchanged.application.declarations).unwrap(),
                before
            );
            assert_eq!(
                serde_json::to_value(&unchanged.view.transcript).unwrap(),
                history
            );
            assert_eq!(unchanged.application.current_switch, "plan");
        }
        std::fs::write(&settings_path, "invalid JSON").unwrap();
        assert!(attachment.reload_resources().await.is_err());
        let unchanged = client.recovery::<GatewayRecovery>(&id).await.unwrap();
        assert_eq!(
            serde_json::to_value(&unchanged.application.declarations).unwrap(),
            before
        );
        assert_eq!(
            serde_json::to_value(&unchanged.view.transcript).unwrap(),
            history
        );
        std::fs::write(&settings_path, settings).unwrap();
        let mut saved =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        saved.name = "named saved Session".into();
        saved.working_dir = std::env::current_dir().unwrap().to_string_lossy().into();
        saved.append_user_message("saved prompt preview");
        saved.transcript.append_assistant_message(
            None,
            None,
            Vec::new(),
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "stop".into(),
        );
        crate::session::storage::save_session(&saved).unwrap();
        let attachment = crate::interactive::verify_gateway_commands(attachment).await;
        for _ in 0..50 {
            let mut recent = saved.clone();
            recent.transcript.id = uuid::Uuid::new_v4().to_string().into();
            recent.transcript.updated_at = "2099-01-01T00:00:00Z".into();
            crate::session::storage::save_session(&recent).unwrap();
        }
        assert!(
            !client
                .saved("")
                .await
                .unwrap()
                .iter()
                .any(|summary| summary.id == saved.id.as_str())
        );
        let hosted_choices = super::super::selection::SessionSelection::Hosted
            .choices(&attachment, saved.id.as_str())
            .await
            .unwrap();
        let local_choices =
            crate::session::discovery::discover_sessions(saved.id.as_str()).unwrap();
        assert_eq!(hosted_choices.len(), 1);
        assert_eq!(
            serde_json::to_value(hosted_choices).unwrap(),
            serde_json::to_value(local_choices).unwrap()
        );
        assert_eq!(runtime.manager.list().unwrap().len(), 2);
        assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), settings);
        assert_eq!(
            binding.instance().snapshot().status,
            phi_sdk::agent_instance::AgentInstanceStatus::Idle
        );
        let mut live_events = attachment.subscribe();
        attachment.recover_events(&mut live_events).await.unwrap();
        let (entered, started) = tokio::sync::oneshot::channel();
        let (resume, resumed) = tokio::sync::oneshot::channel();
        let provider_task = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut connection, _) = provider.accept().await.unwrap();
            let delta = serde_json::json!({"id":"live-reply","model":"test","choices":[{"index":0,"delta":{"role":"assistant","content":"native live delta\n"},"finish_reason":null}]});
            let body = format!("data: {delta}\n\n");
            connection.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{body}\r\n", body.len()).as_bytes()).await.unwrap();
            entered.send(()).unwrap();
            resumed.await.unwrap();
            let delta = serde_json::json!({"id":"live-reply","model":"test","choices":[{"index":0,"delta":{"content":"native after reconnect\n"},"finish_reason":null}]});
            let body = format!("data: {delta}\n\n");
            connection
                .write_all(format!("{:x}\r\n{body}\r\n", body.len()).as_bytes())
                .await
                .unwrap();
            std::future::pending::<()>().await;
        });
        let result = attachment
            .dispatch(
                TurnPrompt {
                    prompt: "/skill:review inspect".into(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: Default::default(),
                },
                SubmissionDispatch::FollowUp,
            )
            .await;
        assert!(
            result.is_err(),
            "the accepted submission response was intentionally lost"
        );
        tokio::time::timeout(Duration::from_secs(5), started)
            .await
            .unwrap()
            .unwrap();
        let transcript = binding.instance().transcript().await.unwrap();
        assert!(transcript.entries.iter().any(|entry| matches!(&entry.kind,
            phi_sdk::transcript::TranscriptEntryKind::Message { message: phi_sdk::transcript::AgentMessage::User { content } }
                if content.iter().any(|block| matches!(block,
                    phi_sdk::transcript::UserContentBlock::Text { text } if text.contains("server-only skill contents"))))));
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let AgentEvent::Phi(envelope) = live_events.recv().await.unwrap()
                    && matches!(envelope.event, phi_sdk::PhiEvent::AssistantDelta { ref display, .. } if display == "native live delta\n")
                {
                    break;
                }
            }
        })
        .await
        .expect("native attachment must forward deltas before completion");
        let recovered_live = attachment.recover_events(&mut live_events).await.unwrap();
        assert!(
            recovered_live
                .snapshot
                .instance
                .live_run
                .as_ref()
                .unwrap()
                .assistant_text
                .contains("native live delta")
        );
        while let Ok(event) = live_events.try_recv() {
            assert!(
                !matches!(event, AgentEvent::Phi(envelope)
                if matches!(envelope.event, phi_sdk::PhiEvent::AssistantDelta { ref display, .. }
                    if display.contains("native live delta"))),
                "covered deltas must not replay after recovery"
            );
        }
        let active_turn = binding.instance().snapshot().active_turn_id;
        disconnect.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    live_events.recv().await.unwrap(),
                    AgentEvent::ObservationFailed { .. }
                ) {
                    break;
                }
            }
            loop {
                if matches!(
                    live_events.recv().await.unwrap(),
                    AgentEvent::RuntimeReplaced
                ) && sockets.load(std::sync::atomic::Ordering::SeqCst) >= 2
                {
                    break;
                }
            }
        })
        .await
        .expect("native observation must report stale access and reconnect");
        let recovered = attachment.recover_events(&mut live_events).await.unwrap();
        assert!(
            recovered
                .snapshot
                .instance
                .live_run
                .as_ref()
                .unwrap()
                .assistant_text
                .contains("native live delta")
        );
        assert_eq!(binding.instance().snapshot().active_turn_id, active_turn);
        resume.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let AgentEvent::Phi(envelope) = live_events.recv().await.unwrap() {
                    match envelope.event {
                        phi_sdk::PhiEvent::AssistantDelta { ref display, .. }
                            if display == "native after reconnect\n" =>
                        {
                            break;
                        }
                        phi_sdk::PhiEvent::AssistantDelta { ref display, .. } => {
                            assert!(
                                !display.contains("native live delta"),
                                "covered output must not replay after reconnect"
                            );
                        }
                        _ => {}
                    }
                }
            }
        })
        .await
        .expect("recovered native watch must resume incremental delivery");
        assert_eq!(submissions.load(std::sync::atomic::Ordering::SeqCst), 1);
        let before = binding.instance().snapshot();
        assert_eq!(
            before.status,
            phi_sdk::agent_instance::AgentInstanceStatus::Running
        );
        let selection = super::super::selection::SessionSelection::Hosted;
        assert!(
            selection
                .resume_session(&attachment, "missing-session")
                .await
                .is_err()
        );
        let selected = selection.new_session(&attachment).await.unwrap();
        assert_ne!(selected.attachment.id(), attachment.id());
        assert_eq!(selected.accepted.session.model, "selected");
        assert_eq!(selected.accepted.session.current_switch, "plan");
        assert_eq!(selected.accepted.session.context_window, 64_000);
        assert!(selected.accepted.session.entries.is_empty());
        let choices = selection
            .choices(&attachment, selected.attachment.id().as_str())
            .await
            .unwrap();
        assert!(choices.is_empty());
        assert!(
            client
                .sessions()
                .await
                .unwrap()
                .iter()
                .any(|session| session.id == selected.attachment.id().as_str())
        );
        let resumed = selection
            .resume_session(&selected.attachment, &id)
            .await
            .unwrap();
        assert_eq!(resumed.attachment.id(), attachment.id());
        drop(resumed);
        drop(selected);
        drop(attachment);
        assert_eq!(runtime.manager.list().unwrap().len(), 3);
        let after = binding.instance().snapshot();
        assert_eq!(after.status, before.status);
        assert_eq!(after.active_turn_id, before.active_turn_id);
        assert_eq!(after.revision, before.revision);
        assert_eq!(std::fs::read_to_string(&settings_path).unwrap(), settings);
        binding.instance().abort().await.unwrap();
        provider_task.abort();
        proxy_task.abort();
        client.stop().await.unwrap();
        gateway.close().await.unwrap();
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        drop(registration);
        assert!(crate::cli::web::existing().await.unwrap().is_none());
        std::env::set_current_dir(previous_dir).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn gateway_creation_prepares_the_target_directory_before_selection() {
        if super::super::preparation::tests::run_isolated_test(concat!(
            module_path!(),
            "::gateway_creation_prepares_the_target_directory_before_selection"
        )) {
            return;
        }
        let (directory, policy, runtime) = session_test_runtime();
        policy.install(&runtime.preparation);
        let target = directory.join("target project");
        std::fs::create_dir_all(target.join(".phi/extensions")).unwrap();
        std::fs::write(
            target.join(".phi/extensions/target.lua"),
            "register_command('target_directory', {handler = function() end})",
        )
        .unwrap();
        std::fs::write(
            target.join(".phi/settings.json"),
            r#"{"defaultProvider":"local","defaultModel":"test","defaultThinkingLevel":"low","no_tools":true}"#,
        )
        .unwrap();
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let host = GatewaySessions::new(
            runtime.sessions.clone(),
            workflows,
            runtime.preparation.clone(),
            true,
        );
        let original_dir = std::env::current_dir().unwrap();
        let created = host.create(target.clone()).await.unwrap();
        assert_eq!(created.working_dir, target.canonicalize().unwrap());
        assert_eq!(
            (created.provider.as_str(), created.model.as_str()),
            ("local", "test")
        );
        let binding = runtime
            .sessions
            .get(&AgentInstanceId::new(&created.id))
            .unwrap();
        let accepted = binding.configuration_seed().await.unwrap();
        assert_eq!(
            accepted.source_settings.default_thinking_level.as_deref(),
            Some("low")
        );
        assert_eq!(accepted.runtime.thinking_level.as_deref(), Some("low"));
        assert!(
            accepted
                .runtime
                .extensions()
                .has_command("target_directory")
        );
        assert!(
            accepted
                .runtime
                .settings_manager()
                .unwrap()
                .matches_scope(&target)
        );
        let inherited = host
            .new_session(AgentInstanceId::new(&created.id))
            .await
            .unwrap();
        assert_eq!(inherited.working_dir, created.working_dir);
        assert_eq!(
            host.resume(created.id.clone()).await.unwrap().id,
            created.id
        );
        let invalid_project = directory.join("invalid-project");
        std::fs::create_dir_all(invalid_project.join(".phi")).unwrap();
        std::fs::write(invalid_project.join(".phi/settings.json"), "invalid JSON").unwrap();
        for invalid in [
            std::path::PathBuf::new(),
            target.join("missing"),
            target.join(".phi/settings.json"),
            invalid_project,
        ] {
            assert!(host.create(invalid).await.is_err());
        }
        assert_eq!(host.sessions().await.unwrap().len(), 2);
        let mut saved = crate::session::new("local", "test", 128_000, &target);
        saved.append_user_message("retained history");
        saved.append_assistant_message(
            None,
            None,
            Vec::new(),
            "test".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "local".into(),
            Default::default(),
            "stop".into(),
        );
        crate::session::storage::save_session(&saved).unwrap();
        let restored = host.resume(saved.id.to_string()).await.unwrap();
        assert_eq!(restored.working_dir, created.working_dir);
        let restored = runtime
            .sessions
            .get(&AgentInstanceId::new(saved.id.as_str()))
            .unwrap();
        assert!(
            restored
                .runtime()
                .extensions()
                .has_command("target_directory")
        );
        assert_eq!(
            restored
                .configuration_seed()
                .await
                .unwrap()
                .runtime
                .thinking_level
                .as_deref(),
            Some("low")
        );
        assert_eq!(std::env::current_dir().unwrap(), original_dir);
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn gateway_root_recovery_reuses_the_application_event_boundary() {
        let (directory, policy, runtime) = session_test_runtime();
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut seed =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        seed.working_dir = directory.to_string_lossy().as_ref().into();
        let host = host_session(
            GatewaySessions::new(
                runtime.sessions.clone(),
                workflows,
                runtime.preparation.clone(),
                false,
            ),
            seed,
        )
        .await;
        let id = AgentInstanceId::new(host.sessions().await.unwrap()[0].id.clone());
        let instance = runtime.manager.get(&id).unwrap();
        let mut events = instance.subscribe();
        let marker = "3".repeat(64);
        let recovered = host
            .recovery_at_boundary(id.clone(), marker.clone())
            .await
            .unwrap();
        assert_eq!(recovered.view.snapshot.id, id);
        assert_eq!(recovered.view.transcript.id.as_str(), id.as_str());
        let workspace = recovered.application.workspace.as_ref().unwrap();
        assert_eq!(workspace.working_dir, directory);
        assert_eq!(workspace.git_branch, None);
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(event, phi_sdk::agent_instance::AgentInstanceEvent::ViewBoundary { id } if id == marker)
        );
        assert_eq!(
            recovered.application.declarations.generation_id,
            runtime
                .sessions
                .get(&id)
                .unwrap()
                .snapshot()
                .await
                .runtime_generation_id
        );
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn gateway_clients_share_resume_and_explicit_close() {
        if super::super::preparation::tests::run_isolated_test(concat!(
            module_path!(),
            "::gateway_clients_share_resume_and_explicit_close"
        )) {
            return;
        }
        let (root, policy, runtime) = session_test_runtime();
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut seed =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        seed.working_dir = root.to_string_lossy().as_ref().into();
        seed.current_switch = "plan".into();
        seed.append_user_message("shared history");
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet", "--initial-branch", "host-branch"])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let id = seed.id.to_string();
        assert!(runtime.manager.list().unwrap().is_empty());
        std::fs::write(root.join("app.js"), "").unwrap();
        let host = host_session(
            GatewaySessions::new(
                runtime.sessions.clone(),
                workflows,
                runtime.preparation.clone(),
                true,
            ),
            seed,
        )
        .await;
        let (gateway, registration, endpoint) = phi_remote::deployment::start(
            &root.join("gateway"),
            runtime.manager.clone(),
            host,
            root.clone(),
            Some(0),
        )
        .await
        .unwrap();
        let client = endpoint.client().unwrap();
        assert_eq!(client.sessions().await.unwrap()[0].id, id);
        let view = client.view(&id).await.unwrap();
        assert_eq!(view["agents"].as_array().unwrap().len(), 1);
        assert!(client.watch("unknown").await.is_err());
        assert_eq!(runtime.manager.list().unwrap().len(), 1);
        let (first, second) = tokio::join!(
            AgentAttachment::connect_gateway(client.clone(), &id),
            AgentAttachment::connect_gateway(client.clone(), &id),
        );
        let attachment = first.unwrap();
        drop(second.unwrap());
        let binding = runtime.sessions.get(&AgentInstanceId::new(&id)).unwrap();
        let mut events = attachment.subscribe();
        let recovered = attachment.recover_events(&mut events).await.unwrap();
        assert_eq!(
            recovered.session.id.as_str(),
            binding.instance().id().as_str()
        );
        assert_eq!(runtime.manager.list().unwrap().len(), 1);
        let workspace = attachment.workspace(&id).await.unwrap();
        assert_eq!(workspace.working_dir, root);
        assert_eq!(workspace.git_branch.as_deref(), Some("host-branch"));
        let revision = binding.instance().snapshot().revision;
        assert!(
            std::process::Command::new("git")
                .args(["symbolic-ref", "HEAD", "refs/heads/updated-host-branch"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                let workspace = attachment.workspace(&id).await.unwrap();
                assert_eq!(workspace.working_dir, root);
                if workspace.git_branch.as_deref() == Some("updated-host-branch") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(binding.instance().snapshot().revision, revision);
        let local = binding.recovery().await.unwrap();
        assert_eq!(
            recovered.session.context_window,
            local.session.context_window
        );
        assert_eq!(
            recovered.session.current_switch,
            local.session.current_switch
        );
        assert_eq!(
            serde_json::to_value(&recovered.declarations).unwrap(),
            serde_json::to_value(&local.declarations).unwrap()
        );
        assert!(
            serde_json::to_string(&recovered.session.transcript)
                .unwrap()
                .contains("shared history")
        );
        assert!(
            AgentAttachment::connect_gateway(client.clone(), "unknown")
                .await
                .is_err()
        );
        assert_eq!(runtime.manager.list().unwrap().len(), 1);

        binding.stop_checkpoint_observer().await;
        let revision = binding.instance().snapshot().revision;
        let attempt = binding
            .begin_save(super::super::save::SaveOperation::Checkpoint)
            .await;
        binding
            .complete_save(
                attempt,
                super::super::save::SaveOperation::Checkpoint,
                1,
                &Err(anyhow::anyhow!("test save failure")),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                events.recv().await.unwrap();
                if attachment
                    .save_observation()
                    .await
                    .unwrap()
                    .completed
                    .is_some_and(|completion| completion.error.is_some())
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(binding.instance().snapshot().revision, revision);
        let recovered = attachment.recover_events(&mut events).await.unwrap();
        assert_eq!(
            recovered.snapshot.persistence_error.as_deref(),
            Some("test save failure")
        );
        let mut cursor = crate::runtime::attachment::SaveObservationCursor::default();
        assert!(matches!(
            cursor.observe(attachment.save_observation().await),
            Some(crate::runtime::attachment::SaveNotice::Warning(_))
        ));
        let attempt = binding
            .begin_save(super::super::save::SaveOperation::Checkpoint)
            .await;
        binding
            .complete_save(
                attempt,
                super::super::save::SaveOperation::Checkpoint,
                1,
                &Ok(None),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                events.recv().await.unwrap();
                if attachment
                    .save_observation()
                    .await
                    .unwrap()
                    .completed
                    .is_some_and(|completion| {
                        completion.attempt == attempt && completion.error.is_none()
                    })
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(binding.instance().snapshot().revision, revision);
        assert_eq!(
            cursor.observe(attachment.save_observation().await),
            Some(crate::runtime::attachment::SaveNotice::Recovered)
        );

        binding
            .reset_context(Some("server-side update".into()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(events.recv().await.unwrap(), AgentEvent::RuntimeReplaced) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        let recovered = attachment.recover_events(&mut events).await.unwrap();
        assert!(
            serde_json::to_string(&recovered.session.transcript)
                .unwrap()
                .contains("server-side update")
        );
        let observed = client
            .snapshot(binding.instance().id().as_str())
            .await
            .unwrap();
        assert_eq!(
            recovered.session.transcript.leaf_id,
            observed.view.transcript.leaf_id
        );
        drop(attachment);
        assert_eq!(runtime.manager.list().unwrap().len(), 1);
        assert_eq!(
            binding.instance().snapshot().status,
            phi_sdk::agent_instance::AgentInstanceStatus::Idle
        );
        assert_eq!(client.sessions().await.unwrap().len(), 1);
        client.close_session(&id).await.unwrap();
        assert!(client.sessions().await.unwrap().is_empty());
        assert!(client.snapshot(&id).await.is_err());
        let (first, second) = tokio::join!(client.resume(&id), client.resume(&id));
        assert_eq!(first.unwrap().id, id);
        assert_eq!(second.unwrap().id, id);
        assert_eq!(runtime.manager.list().unwrap().len(), 1);
        let resumed = client.snapshot(&id).await.unwrap();
        assert_eq!(
            resumed.view.transcript.leaf_id,
            observed.view.transcript.leaf_id
        );
        client.close_session(&id).await.unwrap();
        assert!(client.sessions().await.unwrap().is_empty());
        gateway.close().await.unwrap();
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        drop(registration);
        std::fs::remove_dir_all(root).unwrap();
    }
}
