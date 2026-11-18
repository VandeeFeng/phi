use super::policy::{AgentAccess, AgentOperation, validate_delegation_capacity};
#[cfg(test)]
use super::preparation::resolve_working_dir;
use super::preparation::{
    AgentCreation, AgentSpawnRequest, DelegationMode, PreparedSubagent, SubagentAssociation,
    SubagentCompletion, prepare_sdk_child,
};
use super::registry::{AgentManager, AgentManagerError, Registry, instance_error};
use crate::factory::Phi;
use crate::harness::instance::{AgentInstance, AgentInstanceId, AgentInstanceStatus};

// Bounded candidate startup and compensation across asynchronous boundaries.

const MAX_PENDING_ADMISSIONS: usize = 1024;

struct AdmissionPermit {
    total: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    parent: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.total.fetch_sub(1, Ordering::Relaxed);
        self.parent.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct ReservedSubagent {
    admission: InitialAdmission,
    access: Option<AgentAccess>,
}

impl ReservedSubagent {
    pub(crate) fn creation(&self) -> &AgentCreation {
        &self.admission.creation
    }

    pub(crate) fn registration(&self) -> Option<super::AgentRegistrationId> {
        self.admission
            .child
            .as_ref()
            .map(|(_, registration)| super::AgentRegistrationId::new(*registration))
    }

    pub(crate) fn completion(&self) -> crate::harness::instance::child::InitialCompletion {
        self.admission.completion.clone()
    }

    pub(crate) async fn admit_sdk(
        mut self,
        phi: &Phi,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<ChildCompletion, AgentManagerError> {
        self.start_sdk(phi, cancellation).await
    }

    async fn start_sdk(
        &mut self,
        phi: &Phi,
        mut cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<ChildCompletion, AgentManagerError> {
        self.admission.cancellation = Some(cancellation.clone());
        let result = tokio::select! {
            biased;
            _ = super::preparation::wait_for_cancellation(&mut cancellation) => Err(AgentManagerError::AdmissionInterrupted),
            result = self.admission.prepare_and_start(phi, self.access.as_ref()) => result,
        };
        self.admission.finish_admission(result).await?;
        self.admission.observe_completion(cancellation)
    }

    pub(crate) async fn admit_prepared(
        mut self,
        preparation: std::sync::Arc<dyn super::preparation::SubagentPreparation>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<ChildCompletion, AgentManagerError> {
        self.start_prepared(preparation, cancellation).await
    }

    async fn start_prepared(
        &mut self,
        preparation: std::sync::Arc<dyn super::preparation::SubagentPreparation>,
        mut cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<ChildCompletion, AgentManagerError> {
        self.admission.cancellation = Some(cancellation.clone());
        let cancel = cancellation.clone();
        let result = tokio::select! {
            biased;
            _ = super::preparation::wait_for_cancellation(&mut cancellation) => Err(AgentManagerError::AdmissionInterrupted),
            result = self.admission.prepare_with_association(move |creation| preparation.prepare(creation, cancel), self.access.as_ref()) => result,
        };
        self.admission.finish_admission(result).await?;
        self.admission.observe_completion(cancellation)
    }
}

impl ReservedSubagent {
    pub(crate) fn bind_workflow(
        &mut self,
        run: crate::harness::workflow::WorkflowRunId,
    ) -> Result<(), AgentManagerError> {
        if self.admission.creation.alias().is_some()
            || !matches!(
                self.admission.creation.kind(),
                super::AgentKind::Subagent { .. }
            )
        {
            return Err(AgentManagerError::InvalidCandidate {
                reason: "Workflow workers must be ordinary unnamed subagents",
            });
        }
        self.admission.creation.bind_workflow(run);
        Ok(())
    }

    pub(crate) async fn admit_workflow(
        &mut self,
        phi: &Phi,
        preparation: Option<std::sync::Arc<dyn super::preparation::SubagentPreparation>>,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<ChildCompletion, AgentManagerError> {
        if !matches!(
            self.creation().result_routing(),
            super::preparation::AgentResultRouting::Workflow(_)
        ) || self.admission.accepted
            || self.admission.completion.is_admission_finished()
        {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        match preparation {
            Some(preparation) => self.start_prepared(preparation, cancellation).await,
            None => self.start_sdk(phi, cancellation).await,
        }
    }

    pub(crate) async fn finalize_workflow(&mut self) -> Result<(), AgentManagerError> {
        if self.admission.accepted {
            if let Some((instance, epoch)) = &self.admission.child {
                self.admission
                    .manager
                    .stop_registered(instance.clone(), *epoch)
                    .await?;
                self.admission.child = None;
                self.admission.association = None;
            }
            return Ok(());
        }
        compensate_admission(
            &self.admission.manager,
            &mut self.admission.child,
            &mut self.admission.association,
        )
        .await?;
        self.admission
            .manager
            .lock()?
            .release_alias(&self.admission.creation);
        self.admission.permit.take();
        self.admission.completion.mark_admission_finished(None);
        Ok(())
    }
}

struct InitialAdmission {
    manager: AgentManager,
    creation: AgentCreation,
    assigner_epoch: uuid::Uuid,
    child: Option<(AgentInstance, uuid::Uuid)>,
    accepted: bool,
    association: Option<std::sync::Arc<dyn SubagentAssociation>>,
    completion: crate::harness::instance::child::InitialCompletion,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    permit: Option<AdmissionPermit>,
}

pub(crate) struct ChildCompletion {
    child: AgentInstance,
    completion: crate::harness::instance::child::InitialCompletion,
    cancellation: tokio::sync::watch::Receiver<bool>,
}

impl ChildCompletion {
    pub(crate) async fn finish(mut self) -> Result<SubagentCompletion, String> {
        tokio::select! {
            biased;
            _ = super::preparation::wait_for_cancellation(&mut self.cancellation) => {
                self.completion.cancel();
                self.child.await_initial_completion(&self.completion).await
            }
            settled = self.child.await_initial_completion(&self.completion) => settled,
        }
        .map_err(|error| error.to_string())?;
        self.completion
            .get()
            .ok_or_else(|| "child completion is unavailable".to_string())
    }
}

struct AdmissionCleanupReceipt {
    completion: crate::harness::instance::child::InitialCompletion,
    permit: Option<AdmissionPermit>,
}

impl AdmissionCleanupReceipt {
    fn finish(mut self, result: Result<(), AgentManagerError>) -> Result<(), AgentManagerError> {
        self.permit.take();
        self.completion
            .mark_admission_finished(result.as_ref().err().map(ToString::to_string));
        result
    }

    async fn compensate(
        self,
        manager: &AgentManager,
        creation: &AgentCreation,
        child: &mut Option<(AgentInstance, uuid::Uuid)>,
        association: &mut Option<std::sync::Arc<dyn SubagentAssociation>>,
    ) -> Result<(), AgentManagerError> {
        let result = compensate_admission(manager, child, association)
            .await
            .and_then(|()| {
                manager.lock()?.release_alias(creation);
                Ok(())
            });
        self.finish(result)
    }
}

impl Drop for AdmissionCleanupReceipt {
    fn drop(&mut self) {
        self.permit.take();
        if !self.completion.is_admission_finished() {
            self.completion
                .mark_admission_finished(Some("startup compensation was interrupted".into()));
        }
    }
}

impl AgentManager {
    /// Trusted SDK creation. Agent-originated creation uses caller-bound AgentAccess.
    /// Both modes return creation facts; foreground callers await child completion separately.
    pub async fn spawn_agent(
        &self,
        parent: &AgentInstanceId,
        phi: &Phi,
        request: AgentSpawnRequest,
    ) -> Result<AgentCreation, AgentManagerError> {
        self.spawn_initial(parent, None, phi, request).await
    }

    pub(crate) async fn spawn_scoped(
        &self,
        access: &AgentAccess,
        phi: &Phi,
        request: AgentSpawnRequest,
    ) -> Result<AgentCreation, AgentManagerError> {
        self.spawn_initial(access.caller_id(), Some(access), phi, request)
            .await
    }

    pub(crate) async fn spawn_initial(
        &self,
        parent: &AgentInstanceId,
        access: Option<&AgentAccess>,
        phi: &Phi,
        request: AgentSpawnRequest,
    ) -> Result<AgentCreation, AgentManagerError> {
        let (directory, epoch) = self.creation_scope(parent, access)?;
        let directory =
            super::preparation::prepare_working_dir(directory, request.working_dir.clone()).await?;
        let mut admission =
            self.reserve_initial(parent, access, &request, directory, epoch, None)?;
        let result = async {
            let started = admission.prepare_and_start(phi, access).await?;
            let parent = {
                let registry = self.lock_live()?;
                admission.validate_parent(&registry)?;
                registry.member(parent)?.instance.clone()
            };
            let child = admission
                .child
                .as_ref()
                .ok_or(AgentManagerError::AdmissionInterrupted)?
                .0
                .clone();
            parent
                .observe_started_child(started.clone(), admission.completion.clone(), child)
                .await
                .map_err(|source| instance_error(parent.id(), source))?;
            Ok(started)
        }
        .await;
        admission.finish_admission(result).await
    }

    pub(crate) fn reserve_for_instance(
        &self,
        parent: &AgentInstanceId,
        access: Option<AgentAccess>,
        request: AgentSpawnRequest,
        directory: std::path::PathBuf,
        epoch: uuid::Uuid,
        tool_call_id: Option<compact_str::CompactString>,
    ) -> Result<ReservedSubagent, AgentManagerError> {
        let admission = self.reserve_initial(
            parent,
            access.as_ref(),
            &request,
            directory,
            epoch,
            tool_call_id,
        )?;
        Ok(ReservedSubagent { admission, access })
    }

    fn creation_scope(
        &self,
        parent: &AgentInstanceId,
        access: Option<&AgentAccess>,
    ) -> Result<(std::path::PathBuf, uuid::Uuid), AgentManagerError> {
        let registry = self.lock_live()?;
        validate_creation(&registry, parent, access)?;
        let parent = registry.member(parent)?;
        Ok((parent.instance.snapshot().working_dir, parent.epoch))
    }

    fn reserve_initial(
        &self,
        parent: &AgentInstanceId,
        access: Option<&AgentAccess>,
        request: &AgentSpawnRequest,
        working_dir: std::path::PathBuf,
        epoch: uuid::Uuid,
        tool_call_id: Option<compact_str::CompactString>,
    ) -> Result<InitialAdmission, AgentManagerError> {
        let mut registry = self.lock_live()?;
        validate_creation(&registry, parent, access)?;
        let member = registry.member(parent)?;
        if member.epoch != epoch {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        validate_initial_capacity(&registry, parent)?;
        let mut creation = AgentCreation::new(
            parent.clone(),
            request,
            working_dir,
            member.instance.task_list().cloned(),
        );
        let mode = if member.background_required {
            DelegationMode::Background
        } else {
            request
                .mode
                .unwrap_or_else(|| match member.snapshot().kind {
                    super::AgentKind::Master => DelegationMode::Background,
                    super::AgentKind::Subagent { .. } | super::AgentKind::Teammate { .. } => {
                        DelegationMode::Foreground
                    }
                })
        };
        creation = creation.with_mode(mode).with_tool_call(tool_call_id);
        registry.reserve_alias(&mut creation)?;
        let permit = AdmissionPermit {
            total: registry.admissions.clone(),
            parent: registry.member(parent)?.admissions.clone(),
        };
        use std::sync::atomic::Ordering;
        permit.total.fetch_add(1, Ordering::Relaxed);
        permit.parent.fetch_add(1, Ordering::Relaxed);
        Ok(InitialAdmission::new(self.clone(), creation, epoch, permit))
    }
}

fn validate_creation(
    registry: &Registry,
    parent: &AgentInstanceId,
    access: Option<&AgentAccess>,
) -> Result<(), AgentManagerError> {
    let member = registry.member(parent)?;
    if member.closing
        || matches!(
            member.instance.snapshot().status,
            AgentInstanceStatus::Stopping | AgentInstanceStatus::Stopped
        )
    {
        return Err(AgentManagerError::AdmissionInterrupted);
    }
    if let Some(access) = access {
        access.require_creation(registry, AgentOperation::CreateAgent)?;
    }
    Ok(())
}

fn validate_initial_capacity(
    registry: &Registry,
    parent: &AgentInstanceId,
) -> Result<(), AgentManagerError> {
    use std::sync::atomic::Ordering;
    let member = registry.member(parent)?;
    validate_delegation_capacity(
        member.ancestors.len(),
        member.admissions.load(Ordering::Relaxed),
    )?;
    if registry.admissions.load(Ordering::Relaxed) >= MAX_PENDING_ADMISSIONS {
        return Err(AgentManagerError::AdmissionLimit {
            limit: MAX_PENDING_ADMISSIONS,
        });
    }
    Ok(())
}

impl InitialAdmission {
    fn new(
        manager: AgentManager,
        creation: AgentCreation,
        assigner_epoch: uuid::Uuid,
        permit: AdmissionPermit,
    ) -> Self {
        Self {
            manager,
            creation,
            assigner_epoch,
            child: None,
            accepted: false,
            association: None,
            completion: Default::default(),
            cancellation: None,
            permit: Some(permit),
        }
    }

    fn observe_completion(
        &self,
        cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<ChildCompletion, AgentManagerError> {
        let child = self
            .child
            .as_ref()
            .ok_or(AgentManagerError::AdmissionInterrupted)?
            .0
            .clone();
        Ok(ChildCompletion {
            child,
            completion: self.completion.clone(),
            cancellation,
        })
    }

    fn validate_parent(&self, registry: &Registry) -> Result<(), AgentManagerError> {
        if self.completion.is_cancelled() {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        let parent = registry.member(self.creation.creator_id())?;
        validate_admission_member(parent, self.assigner_epoch)?;
        Ok(())
    }

    async fn prepare_and_start(
        &mut self,
        phi: &Phi,
        access: Option<&AgentAccess>,
    ) -> Result<AgentCreation, AgentManagerError> {
        let mut cancellation = self.completion.cancellation();
        tokio::select! {
            biased;
            _ = super::preparation::wait_for_cancellation(&mut cancellation) => Err(AgentManagerError::AdmissionInterrupted),
            result = self.prepare_sdk(phi, access) => result,
        }
    }

    async fn prepare_sdk(
        &mut self,
        phi: &Phi,
        access: Option<&AgentAccess>,
    ) -> Result<AgentCreation, AgentManagerError> {
        let candidate = prepare_sdk_child(phi, &self.creation)
            .await
            .map_err(|source| AgentManagerError::Preparation { source })?;
        let child = self.register(candidate, access).await?;
        self.start_registered(&child).await?;
        Ok(self.creation.clone())
    }

    async fn prepare_with_association<F, Fut>(
        &mut self,
        prepare: F,
        access: Option<&AgentAccess>,
    ) -> Result<(), AgentManagerError>
    where
        F: FnOnce(AgentCreation) -> Fut,
        Fut: std::future::Future<Output = Result<PreparedSubagent, String>>,
    {
        let mut cancellation = self.completion.cancellation();
        tokio::select! {
            biased;
            _ = super::preparation::wait_for_cancellation(&mut cancellation) => Err(AgentManagerError::AdmissionInterrupted),
            result = self.prepare_associated(prepare, access) => result,
        }
    }

    async fn prepare_associated<F, Fut>(
        &mut self,
        prepare: F,
        access: Option<&AgentAccess>,
    ) -> Result<(), AgentManagerError>
    where
        F: FnOnce(AgentCreation) -> Fut,
        Fut: std::future::Future<Output = Result<PreparedSubagent, String>>,
    {
        let prepared = prepare(self.creation.clone())
            .await
            .map_err(|message| AgentManagerError::Association { message })?;
        self.association = Some(prepared.association.clone());
        let child = self.register(prepared.candidate, access).await?;
        prepared
            .association
            .associate(child.clone(), self.creation.clone())
            .await
            .map_err(|message| AgentManagerError::Association { message })?;
        self.start_registered(&child).await
    }

    async fn start_registered(&mut self, child: &AgentInstance) -> Result<(), AgentManagerError> {
        let files = {
            let registry = self.manager.lock_live()?;
            self.validate_parent(&registry)?;
            let member = registry.member(child.id())?;
            registry.validate_team_start(&self.creation, member.epoch)?;
            member
                .team
                .as_ref()
                .map(|team| registry.team(team).map(|state| state.files.clone()))
                .transpose()?
        };
        if let Some(files) = files {
            let epoch = self.child.as_ref().expect("registered child").1;
            files
                .add_member(self.manager.clone(), self.creation.clone(), epoch)
                .await?;
        }
        if self.completion.is_cancelled() {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        child
            .start_created_agent(
                &self.creation,
                self.creation.parent_tool_call_id().map(Into::into),
                self.completion.clone(),
            )
            .await
            .map_err(|source| instance_error(child.id(), source))?;
        self.accept()
    }

    async fn register(
        &mut self,
        candidate: crate::AgentInstanceCandidate,
        access: Option<&AgentAccess>,
    ) -> Result<AgentInstance, AgentManagerError> {
        super::preparation::revalidate_directory(self.creation.working_dir()).await?;
        let mut registry = self.manager.lock_live()?;
        self.validate_parent(&registry)?;
        validate_creation(&registry, self.creation.creator_id(), access)?;
        let child = registry.register_creation(candidate, &self.creation)?;
        let epoch = registry.member(child.id())?.epoch;
        self.child = Some((child.clone(), epoch));
        self.completion.bind_executor(epoch);
        Ok(child)
    }

    fn accept(&mut self) -> Result<(), AgentManagerError> {
        let cancellation = self.cancellation.as_ref().map(|receiver| receiver.borrow());
        if cancellation.as_ref().is_some_and(|cancelled| **cancelled) {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        let mut registry = self.manager.lock_live()?;
        self.validate_parent(&registry)?;
        let (child, epoch) = self
            .child
            .as_ref()
            .ok_or(AgentManagerError::AdmissionInterrupted)?;
        let member = registry.member(child.id())?;
        validate_admission_member(member, *epoch)?;
        registry.publish_creation(&self.creation, *epoch)?;
        self.completion.accept();
        self.accepted = true;
        Ok(())
    }

    async fn finish_admission<T>(
        &mut self,
        result: Result<T, AgentManagerError>,
    ) -> Result<T, AgentManagerError> {
        let Err(cause) = result else {
            self.permit.take();
            self.completion.mark_admission_finished(None);
            return result;
        };
        let cleanup = AdmissionCleanupReceipt {
            completion: self.completion.clone(),
            permit: self.permit.take(),
        }
        .compensate(
            &self.manager,
            &self.creation,
            &mut self.child,
            &mut self.association,
        )
        .await;
        match cleanup {
            Ok(()) => Err(cause),
            Err(cleanup) => Err(AgentManagerError::Compensation {
                cause: Box::new(cause),
                cleanup: Box::new(cleanup),
            }),
        }
    }
}

fn validate_admission_member(
    member: &super::registry::Member,
    epoch: uuid::Uuid,
) -> Result<(), AgentManagerError> {
    if member.epoch != epoch
        || member.closing
        || matches!(
            member.instance.snapshot().status,
            AgentInstanceStatus::Stopping | AgentInstanceStatus::Stopped
        )
    {
        return Err(AgentManagerError::AdmissionInterrupted);
    }
    Ok(())
}

impl Drop for InitialAdmission {
    fn drop(&mut self) {
        if self.accepted {
            self.permit.take();
            if !self.completion.is_admission_finished() {
                self.completion.mark_admission_finished(None);
            }
            return;
        }
        let mut child = self.child.take();
        let mut association = self.association.take();
        let receipt = AdmissionCleanupReceipt {
            completion: self.completion.clone(),
            permit: self.permit.take(),
        };
        if child.is_none() && association.is_none() {
            let _ = receipt.finish(self.manager.lock().map(|mut registry| {
                registry.release_alias(&self.creation);
            }));
            return;
        }
        let manager = self.manager.clone();
        let creation = self.creation.clone();
        tokio::spawn(async move {
            let _ = receipt
                .compensate(&manager, &creation, &mut child, &mut association)
                .await;
        });
    }
}

async fn compensate_admission(
    manager: &AgentManager,
    child: &mut Option<(AgentInstance, uuid::Uuid)>,
    association: &mut Option<std::sync::Arc<dyn SubagentAssociation>>,
) -> Result<(), AgentManagerError> {
    let stopped = match child.as_ref() {
        Some((instance, epoch)) => manager
            .stop_registered(instance.clone(), *epoch)
            .await
            .map(|_| ()),
        None => Ok(()),
    };
    if stopped.is_ok() {
        *child = None;
    }
    let associated = match association.as_ref() {
        Some(association) => association
            .rollback()
            .await
            .map_err(|message| AgentManagerError::Association { message }),
        None => Ok(()),
    };
    if associated.is_ok() {
        *association = None;
    }
    stopped.and(associated)
}

#[cfg(test)]
mod tests {
    use super::super::preparation::AgentSpawnRequest;
    use super::super::registry::{AgentManager, AgentManagerError};
    use super::{AgentInstance, AgentInstanceStatus};
    use crate::agent::custom::AgentDefinition;
    use crate::harness::instance::child::SubagentAdmission;

    use crate::factory::Phi;
    use crate::factory::PhiRuntimeServices;
    use crate::transcript::{AgentMessage, TranscriptEntryKind};

    #[tokio::test]
    async fn delegation_mode_defaults_and_overrides_apply_at_admission() {
        use super::super::preparation::DelegationMode::{Background, Foreground};

        for forced_background in [false, true] {
            let mut phi = test_phi();
            phi.cfg.agent_background = forced_background;
            let manager = AgentManager::new();
            let storage = std::path::PathBuf::from("/tmp")
                .join(format!("phi-mode-admission-{}", uuid::Uuid::new_v4()));
            let parent = manager
                .insert(
                    phi.agent_instance_builder()
                        .task_list(crate::tasks::TaskListBinding::new(&storage, "shared").unwrap())
                        .build()
                        .await
                        .unwrap(),
                )
                .unwrap();
            let child_creation = super::AgentCreation::new(
                parent.id().clone(),
                &AgentSpawnRequest::new("reviewer", "inspect"),
                parent.snapshot().working_dir,
                parent.task_list().cloned(),
            );
            let candidate = super::prepare_sdk_child(&phi, &child_creation)
                .await
                .unwrap();
            let child = manager
                .lock_live()
                .unwrap()
                .register_creation(candidate, &child_creation)
                .unwrap();
            for caller in [&parent, &child] {
                for requested in [None, Some(Foreground), Some(Background)] {
                    let mut request = AgentSpawnRequest::new("reviewer", "inspect");
                    request.mode = requested;
                    let mut admission = reserve(&manager, caller, &request);
                    let expected = if forced_background {
                        Background
                    } else {
                        requested.unwrap_or(if caller.id() == parent.id() {
                            Background
                        } else {
                            Foreground
                        })
                    };
                    assert_eq!(admission.creation.mode(), expected);
                    let _: Result<(), _> = admission
                        .finish_admission(Err(AgentManagerError::AdmissionInterrupted))
                        .await;
                }
            }
            manager
                .initialize_team(
                    parent.id(),
                    super::super::team::TeamStorageRoot::new(storage.join("teams")).unwrap(),
                )
                .await
                .unwrap();
            for requested in [None, Some(Foreground), Some(Background)] {
                let mut request = AgentSpawnRequest::new("reviewer", "team work")
                    .alias(super::super::AgentAlias::new("reviewer").unwrap());
                request.mode = requested;
                let mut admission = reserve(&manager, &parent, &request);
                assert!(matches!(
                    admission.creation.kind(),
                    super::super::AgentKind::Teammate { .. }
                ));
                assert_eq!(admission.creation.mode(), Background);
                let _: Result<(), _> = admission
                    .finish_admission(Err(AgentManagerError::AdmissionInterrupted))
                    .await;
            }
            manager.shutdown().await.unwrap();
            std::fs::remove_dir_all(storage).unwrap();
        }
    }

    #[tokio::test]
    async fn accepted_background_policy_replacement_does_not_mutate_children() {
        use super::super::preparation::DelegationMode::{Background, Foreground};

        let phi = test_phi();
        let manager = AgentManager::new();
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let creation = super::AgentCreation::new(
            parent.id().clone(),
            &AgentSpawnRequest::new("reviewer", "inspect").mode(Foreground),
            parent.snapshot().working_dir,
            None,
        );
        let candidate = super::prepare_sdk_child(&phi, &creation).await.unwrap();
        let child = manager
            .lock_live()
            .unwrap()
            .register_creation(candidate, &creation)
            .unwrap();
        let mut changed = phi.clone();
        changed.cfg.agent_background = true;
        let replacement = changed
            .agent_instance_builder()
            .transcript(parent.transcript().await.unwrap())
            .build()
            .await
            .unwrap();
        parent
            .replace_runtime(replacement.into_runtime_candidate())
            .await
            .unwrap();
        let rejected = phi.agent_instance_builder().build().await.unwrap();
        assert!(
            parent
                .replace_runtime(rejected.into_runtime_candidate())
                .await
                .is_err()
        );
        for (caller, expected) in [(&parent, Background), (&child, Foreground)] {
            let mut admission = reserve(
                &manager,
                caller,
                &AgentSpawnRequest::new("reviewer", "new work").mode(Foreground),
            );
            assert_eq!(admission.creation.mode(), expected);
            let _: Result<(), _> = admission
                .finish_admission(Err(AgentManagerError::AdmissionInterrupted))
                .await;
        }
        assert_eq!(creation.mode(), Foreground);
        let automatic = phi
            .agent_instance_builder()
            .transcript(parent.transcript().await.unwrap())
            .build()
            .await
            .unwrap();
        parent
            .replace_runtime(automatic.into_runtime_candidate())
            .await
            .unwrap();
        let mut admission = reserve(
            &manager,
            &parent,
            &AgentSpawnRequest::new("reviewer", "automatic again").mode(Foreground),
        );
        assert_eq!(admission.creation.mode(), Foreground);
        let _: Result<(), _> = admission
            .finish_admission(Err(AgentManagerError::AdmissionInterrupted))
            .await;
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn creation_and_messages_need_no_checklist_binding() {
        let (prompts, mut observed) = tokio::sync::mpsc::unbounded_channel();
        let mut phi = test_phi();
        phi.hooks
            .register_hook(crate::hooks::Hook::user_prompt_submit(move |event| {
                prompts.send(event.prompt.clone()).unwrap();
                async move {
                    Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                        notices: Vec::new(),
                        prompt: event.prompt,
                        injected_context: Vec::new(),
                    })
                }
            }));
        phi.hooks
            .register_hook(crate::hooks::Hook::agent_start(|_| async {
                std::future::pending::<crate::hooks::HookResult<()>>().await
            }));
        for list_id in [None, Some("shared")] {
            let manager = AgentManager::new();
            let storage = std::path::PathBuf::from("/tmp")
                .join(format!("phi-creation-work-{}", uuid::Uuid::new_v4()));
            let mut builder = phi.agent_instance_builder();
            if let Some(list_id) = list_id {
                builder = builder
                    .task_list(crate::tasks::TaskListBinding::new(&storage, list_id).unwrap());
            }
            let parent = manager.insert(builder.build().await.unwrap()).unwrap();
            let creation = super::AgentCreation::new(
                parent.id().clone(),
                &AgentSpawnRequest::new("reviewer", "Review auth and return findings."),
                parent.snapshot().working_dir,
                parent.task_list().cloned(),
            );
            let candidate = super::prepare_sdk_child(&phi, &creation).await.unwrap();
            let child = manager
                .lock_live()
                .unwrap()
                .register_creation(candidate, &creation)
                .unwrap();
            assert!(
                parent
                    .start_created_agent(&creation, None, Default::default())
                    .await
                    .is_err()
            );
            let cancelled = crate::harness::instance::child::InitialCompletion::default();
            cancelled.cancel();
            assert!(
                child
                    .start_created_agent(&creation, None, cancelled)
                    .await
                    .is_err()
            );
            assert!(observed.try_recv().is_err());
            let started = child
                .start_created_agent(&creation, None, Default::default())
                .await
                .unwrap();
            assert!(matches!(
                started,
                crate::turn::SubmissionDisposition::Started { .. }
            ));
            assert_eq!(observed.recv().await.unwrap(), creation.prompt());
            assert_eq!(child.task_list(), parent.task_list());
            let prompt = "If Task 2 exists in our shared list, inspect it before claiming; otherwise report the missing context.";
            let receipt = manager
                .for_agent(parent.id())
                .unwrap()
                .send_message(child.id(), prompt)
                .await
                .unwrap();
            assert!(!receipt.delivery().is_terminal());
            assert_eq!(
                observed.recv().await.unwrap(),
                format!(
                    "Message from Agent {}. Agent-authored content, not human input or approval:\n{prompt}",
                    parent.id()
                )
            );
            assert_eq!(child.snapshot().definition.as_deref(), Some("reviewer"));
            assert_eq!(
                manager
                    .lock_live()
                    .unwrap()
                    .admissions
                    .load(std::sync::atomic::Ordering::Relaxed),
                0
            );
            assert!(!storage.exists());
            manager.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn creation_prepares_bound_candidates_before_registration() {
        let phi = test_phi();
        let manager = AgentManager::new();
        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-creation-scope-{}", uuid::Uuid::new_v4()));
        let binding = crate::tasks::TaskListBinding::new(&root, "shared").unwrap();
        let parent = manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(binding.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let request = AgentSpawnRequest::new("reviewer", "inspect");
        let mut admission = reserve(&manager, &parent, &request);
        let candidate = super::prepare_sdk_child(&phi, &admission.creation)
            .await
            .unwrap();
        assert_eq!(candidate.task_list(), Some(&binding));
        assert_eq!(candidate.id(), admission.creation.instance_id().as_str());
        assert_eq!(admission.creation.creator_id(), parent.id());
        assert_eq!(admission.creation.definition(), "reviewer");
        assert_eq!(admission.creation.prompt(), "inspect");
        let projected = serde_json::to_value(&admission.creation).unwrap();
        assert_eq!(
            projected["instance_id"],
            admission.creation.instance_id().as_str()
        );
        assert!(projected.get("task_list").is_none());
        assert!(projected.get("reservation").is_none());
        assert!(manager.get(admission.creation.instance_id()).is_err());
        assert!(parent.transcript().await.unwrap().entries.is_empty());
        assert!(!root.exists());

        for selected in [
            None,
            Some(crate::tasks::TaskListBinding::new(&root, "other").unwrap()),
            Some(crate::tasks::TaskListBinding::new(root.join("other"), "shared").unwrap()),
        ] {
            let mut transcript = crate::transcript::Transcript::new();
            transcript.id = admission.creation.instance_id().as_str().into();
            let mut builder = phi
                .agent_instance_builder()
                .delegated_child()
                .working_dir(admission.creation.working_dir().to_path_buf())
                .transcript(transcript);
            if let Some(selected) = selected {
                builder = builder.task_list(selected);
            }
            assert!(matches!(
                admission
                    .register(builder.build().await.unwrap(), None)
                    .await,
                Err(AgentManagerError::InvalidCandidate { .. })
            ));
        }

        let inherited = binding.clone();
        let creator = parent.id().clone();
        let result = admission
            .prepare_with_association(
                move |creation| async move {
                    assert_eq!(creation.creator_id(), &creator);
                    assert_eq!(creation.task_list(), Some(&inherited));
                    assert_eq!(creation.definition(), "reviewer");
                    Err("preparation rejected".into())
                },
                None,
            )
            .await;
        assert!(matches!(result, Err(AgentManagerError::Association { .. })));
        assert!(manager.get(admission.creation.instance_id()).is_err());
        assert!(!root.exists());
        drop(admission);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn instance_spawn_reserves_in_manager_and_runs_in_an_independent_sdk_actor() {
        let mut phi = test_phi();
        phi.hooks
            .register_hook(crate::hooks::Hook::agent_start(|_| async {
                std::future::pending::<crate::hooks::HookResult<()>>().await
            }));
        let manager = AgentManager::new();
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let started = parent
            .spawn_subagent(crate::agent_manager::AgentSpawnRequest::new(
                "reviewer", "inspect",
            ))
            .await
            .unwrap();
        let canonical = parent
            .subagent_status(started.instance_id().clone())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(canonical.creation, started);
        assert_eq!(
            canonical.creation.creator_id().as_str(),
            parent.id().as_str()
        );
        assert_eq!(canonical.creation.parent_tool_call_id(), None);
        let child = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Ok(child) =
                    manager.get(&crate::harness::instance::handle::AgentInstanceId::new(
                        started.instance_id().as_str(),
                    ))
                {
                    break child;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_ne!(parent.id(), child.id());
        assert_eq!(
            manager
                .for_agent(parent.id())
                .unwrap()
                .list()
                .unwrap()
                .into_iter()
                .find(|member| member.instance.id == *child.id())
                .unwrap()
                .team,
            None
        );
        assert_eq!(
            child
                .transcript()
                .await
                .unwrap()
                .effective_context()
                .unwrap()
                .entries
                .len(),
            1
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while parent
                .subagent_status(started.instance_id().clone())
                .await
                .unwrap()
                .unwrap()
                .admission
                != SubagentAdmission::Accepted
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let observation = parent.subagent_snapshots().await.unwrap().remove(0);
        assert_eq!(
            observation.executor_registration_id,
            child.registration_id()
        );
        assert_eq!(observation.initial_turn_id, child.snapshot().active_turn_id);
        assert!(observation.initial_turn_id.is_some());
        let independent = manager
            .spawn_agent(
                child.id(),
                &phi,
                AgentSpawnRequest::new("reviewer", "independent"),
            )
            .await
            .unwrap();
        assert_eq!(independent.creator_id().as_str(), child.id().as_str());
        manager.interrupt(parent.id()).await.unwrap();
        assert_ne!(
            child.snapshot().status,
            crate::harness::instance::handle::AgentInstanceStatus::Stopped
        );
        assert_eq!(
            parent
                .subagent_status(started.instance_id().clone())
                .await
                .unwrap()
                .unwrap()
                .admission,
            SubagentAdmission::Accepted
        );
        assert_eq!(
            manager
                .for_agent(parent.id())
                .unwrap()
                .list()
                .unwrap()
                .len(),
            3
        );
        parent
            .cancel_subagent(started.instance_id().clone())
            .await
            .unwrap();
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn accepted_admission_survives_waiter_loss_and_releases_permits() {
        let mut phi = test_phi();
        phi.hooks
            .register_hook(crate::hooks::Hook::agent_start(|_| async {
                std::future::pending::<crate::hooks::HookResult<()>>().await
            }));
        let manager = AgentManager::new();
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let mut admission = reserve(
            &manager,
            &parent,
            &AgentSpawnRequest::new("reviewer", "inspect"),
        );
        let completion = admission.completion.clone();
        let started = admission.prepare_and_start(&phi, None).await.unwrap();
        let child = manager
            .get(&crate::agent_instance::AgentInstanceId::new(
                started.instance_id().as_str(),
            ))
            .unwrap();
        assert_eq!(completion.admission(), SubagentAdmission::Accepted);
        drop(admission);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                completion.wait_admission_finished()
            )
            .await
            .unwrap()
            .is_none()
        );
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .admissions
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_ne!(
            child.snapshot().status,
            crate::agent_instance::AgentInstanceStatus::Stopped
        );
        assert!(completion.cancel());
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            completion.wait_complete(),
        )
        .await
        .unwrap();
        assert_eq!(
            completion.get().unwrap().outcome.unwrap_err().kind,
            crate::PhiFailureKind::Cancelled
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn early_failure_preserves_acknowledged_startup_and_private_pending_completion() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const FAILURE_STATUS: &str = "HTTP/1.1 400 Bad Request";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body =
                r#"{"error":{"message":"early task failure","type":"invalid_request_error"}}"#;
            stream.write_all(format!("{FAILURE_STATUS}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        });
        let catalog = crate::models::ModelCatalog::for_test_model(crate::models::ModelDescriptor {
            provider: "local".into(),
            id: "test".into(),
            name: "Test".into(),
            api: crate::provider::ProviderApi::OpenAiCompletions,
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::models::ModelInput::Text],
            context_window: 128_000,
            max_output_tokens: 16_384,
            cost: Default::default(),
        })
        .unwrap();
        let phi = Phi::builder()
            .provider("local")
            .model("test")
            .no_tools(true)
            .runtime_config(crate::config::RuntimeConfig {
                custom_providers: std::collections::HashMap::from([(
                    "local".into(),
                    crate::config::CustomProviderConfig {
                        api: crate::provider::ProviderApi::OpenAiCompletions,
                        base_url: Some(format!("http://{address}/v1")),
                        api_key: None,
                    },
                )]),
                ..Default::default()
            })
            .agent(AgentDefinition::new(
                "reviewer",
                "Review",
                "Review the task",
            ))
            .build()
            .unwrap()
            .with_runtime_services(
                PhiRuntimeServices::new().model_catalog(std::sync::Arc::new(catalog)),
            );
        let manager = AgentManager::new();
        let root = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let request = AgentSpawnRequest::new("reviewer", "early failure");
        let mut admission = reserve(&manager, &root, &request);
        let candidate = super::prepare_sdk_child(&phi, &admission.creation)
            .await
            .unwrap();
        let child = admission.register(candidate, None).await.unwrap();
        child
            .start_created_agent(&admission.creation, None, admission.completion.clone())
            .await
            .unwrap();
        let completion = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            child.complete_delegated_agent(tokio::sync::watch::channel(false).1),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(completion.outcome.is_err());
        server.await.unwrap();
        assert_eq!(admission.completion.admission(), SubagentAdmission::Pending);
        assert!(!admission.completion.is_cancelled());
        admission.accept().unwrap();
        admission.finish_admission(Ok(())).await.unwrap();
        assert_eq!(
            admission.completion.admission(),
            SubagentAdmission::Accepted
        );
        assert_eq!(admission.completion.get(), Some(completion));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn child_cancellation_preserves_instances_and_unrelated_creation_descendants() {
        let mut phi = test_phi();
        phi.hooks
            .register_hook(crate::hooks::Hook::agent_start(|_| async {
                std::future::pending::<crate::hooks::HookResult<()>>().await
            }));
        {
            let manager = AgentManager::new();
            let root = manager
                .insert(phi.agent_instance_builder().build().await.unwrap())
                .unwrap();
            let task = manager
                .spawn_agent(
                    root.id(),
                    &phi,
                    AgentSpawnRequest {
                        ..AgentSpawnRequest::new("reviewer", "parent")
                    },
                )
                .await
                .unwrap();
            let parent = manager
                .get(&crate::harness::instance::handle::AgentInstanceId::new(
                    task.instance_id().as_str(),
                ))
                .unwrap();
            let epoch = manager
                .lock_live()
                .unwrap()
                .member(parent.id())
                .unwrap()
                .epoch;
            let reserved = manager
                .reserve_for_instance(
                    parent.id(),
                    None,
                    AgentSpawnRequest::new("reviewer", "direct child"),
                    parent.snapshot().working_dir,
                    epoch,
                    None,
                )
                .unwrap();
            let child_task = reserved.creation().clone();
            let child_completion = reserved.completion();
            let _run = reserved
                .admit_sdk(&phi, tokio::sync::watch::channel(false).1)
                .await
                .unwrap();
            let child = manager
                .get(&crate::harness::instance::handle::AgentInstanceId::new(
                    child_task.instance_id().as_str(),
                ))
                .unwrap();
            let independent = manager
                .spawn_agent(
                    parent.id(),
                    &phi,
                    AgentSpawnRequest::new("reviewer", "independent"),
                )
                .await
                .unwrap();
            let independent_child = manager
                .get(&crate::harness::instance::handle::AgentInstanceId::new(
                    independent.instance_id().as_str(),
                ))
                .unwrap();
            let cancellation = async {
                root.cancel_subagent(task.instance_id().clone())
                    .await
                    .unwrap()
            };
            tokio::pin!(cancellation);
            assert!(
                std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
                    cancellation.as_mut(),
                    cx
                )))
                .await
                .is_pending()
            );
            assert!(cancellation.await);
            let completion = parent
                .complete_delegated_agent(tokio::sync::watch::channel(false).1)
                .await
                .unwrap();
            assert_eq!(
                completion.outcome.unwrap_err().kind,
                crate::PhiFailureKind::Cancelled
            );
            assert!(child_completion.get().is_none());
            assert_eq!(manager.list().unwrap().len(), 4);
            for instance in [&parent, &child] {
                assert_ne!(
                    instance.snapshot().status,
                    crate::harness::instance::handle::AgentInstanceStatus::Stopped
                );
                assert_ne!(
                    instance.snapshot().status,
                    crate::harness::instance::handle::AgentInstanceStatus::Stopping
                );
            }
            assert!(
                parent
                    .subagent_status(independent.instance_id().clone())
                    .await
                    .unwrap()
                    .unwrap()
                    .outcome
                    .is_none()
            );
            assert_eq!(
                independent_child.snapshot().status,
                crate::harness::instance::handle::AgentInstanceStatus::Running
            );
            assert!(
                !root
                    .cancel_subagent(task.instance_id().clone())
                    .await
                    .unwrap()
            );
            assert!(
                !root
                    .cancel_subagent(child_task.instance_id().clone())
                    .await
                    .unwrap()
            );
            assert!(
                !parent
                    .cancel_subagent(task.instance_id().clone())
                    .await
                    .unwrap()
            );
            let outsider = manager
                .insert(phi.agent_instance_builder().build().await.unwrap())
                .unwrap();
            assert!(
                !outsider
                    .cancel_subagent(independent.instance_id().clone())
                    .await
                    .unwrap()
            );
            manager.shutdown().await.unwrap();
        }
    }

    #[test]
    fn unresolved_directory_requests_remain_responsive_and_cancel_without_admission() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let phi = test_phi();
            for (tool_origin, abandon) in [(false, false), (false, true), (true, false), (true, true)] {
                let manager = AgentManager::new();
                let mut candidate = phi
                    .agent_instance_builder()
                    .build()
                    .await
                    .unwrap();
                let sender = candidate.install_test_agent_channel();
                let _events =
                    tool_origin.then(|| candidate.install_test_main_run("directory-turn"));
                let parent = manager.insert(candidate).unwrap();
                let replacement = phi
                    .agent_instance_builder()
                    .transcript(parent.transcript().await.unwrap())
                    .build()
                    .await
                    .unwrap();
                let (release, blocked) = std::sync::mpsc::channel::<()>();
                let mut release = Some(release);
                let (entered, ready) = tokio::sync::oneshot::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    let _ = entered.send(());
                    let _ = blocked.recv();
                });
                ready.await.unwrap();
                let (reply, tool_result) = tokio::sync::oneshot::channel();
                let direct = if tool_origin {
                    sender
                        .send(crate::agent::tools::AgentToolRequest {
                            args: crate::agent::tools::AgentArgs::Create(AgentSpawnRequest::new(
                                "reviewer", "inspect",
                            )),
                            scope: crate::core::event::CoreEventScope {
                                session_id: Some(parent.id().as_str().into()),
                                turn_id: Some("directory-turn".into()),
                                parent_tool_call_id: Some("pending-directory".into()),
                                ..Default::default()
                            },
                            reply,
                        })
                        .unwrap();
                    None
                } else {
                    drop(reply);
                    let parent = parent.clone();
                    Some(tokio::spawn(async move {
                        parent
                            .spawn_subagent(crate::agent_manager::AgentSpawnRequest::new(
                                "reviewer", "inspect",
                            ))
                            .await
                    }))
                };
                tokio::task::yield_now().await;
                // Allow the actor to consume the tool request through its ordinary pump.
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                tokio::time::timeout(std::time::Duration::from_secs(1), parent.view())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(manager.lock_live().unwrap().admissions.load(std::sync::atomic::Ordering::Relaxed), 0);
                assert!(matches!(
                    parent
                        .replace_runtime(replacement.into_runtime_candidate())
                        .await,
                    Err(crate::harness::instance::handle::AgentInstanceError::Busy)
                ));
                if tool_origin {
                    if abandon {
                        let mut events = parent.subscribe();
                        _events
                            .as_ref()
                            .unwrap()
                            .send(crate::core::event::AgentEvent::RunFinished {
                                response: "turn ended before directory preparation".into(),
                                usage: Default::default(),
                            })
                            .await
                            .unwrap();
                        drop(_events);
                        tokio::time::timeout(std::time::Duration::from_secs(1), async {
                            loop {
                                if matches!(
                                    events.recv().await.unwrap(),
                                    crate::agent_instance::AgentInstanceEvent::Phi(envelope)
                                        if matches!(envelope.event, crate::PhiEvent::TurnFinished { .. })
                                ) {
                                    break;
                                }
                            }
                        })
                        .await
                        .unwrap();
                        drop(release.take());
                        let error = tokio::time::timeout(
                            std::time::Duration::from_secs(1),
                            tool_result,
                        )
                        .await
                        .unwrap()
                        .unwrap()
                        .unwrap_err();
                        assert!(error.contains("admission was interrupted"));
                        parent.stop().await.unwrap();
                    } else {
                        parent.stop().await.unwrap();
                        assert!(tool_result.await.unwrap().is_err());
                    }
                } else if abandon {
                    let direct = direct.unwrap();
                    direct.abort();
                    assert!(direct.await.unwrap_err().is_cancelled());
                    tokio::time::timeout(std::time::Duration::from_secs(1), async {
                        while parent.snapshot().status != crate::harness::instance::handle::AgentInstanceStatus::Idle {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    parent.stop().await.unwrap();
                } else {
                    parent.abort().await.unwrap();
                    assert!(direct.unwrap().await.unwrap().is_err());
                    parent.stop().await.unwrap();
                }
                drop(release);
                blocker.await.unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                assert_eq!(manager.lock_live().unwrap().admissions.load(std::sync::atomic::Ordering::Relaxed), 0);
                assert_eq!(manager.list().unwrap().len(), 0);
                manager.shutdown().await.unwrap();
            }
        });
    }

    struct TestAssociation {
        pending: bool,
        fail_rollback: bool,
        rollback_panic: bool,
        rollback_pending: bool,
        rollback_release: tokio::sync::Notify,
        panic: bool,
        entered: tokio::sync::Notify,
        rolled_back: tokio::sync::Notify,
        child: std::sync::Mutex<Option<crate::harness::instance::handle::AgentInstance>>,
    }

    impl super::SubagentAssociation for TestAssociation {
        fn associate(
            &self,
            child: crate::harness::instance::handle::AgentInstance,
            _creation: crate::agents::AgentCreation,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
        {
            Box::pin(async move {
                assert_eq!(
                    child.snapshot().status,
                    crate::harness::instance::handle::AgentInstanceStatus::Idle
                );
                *self.child.lock().unwrap() = Some(child);
                self.entered.notify_one();
                assert!(!self.panic, "association callback panicked");
                if self.pending {
                    std::future::pending().await
                } else {
                    Err("association rejected".into())
                }
            })
        }

        fn rollback(
            &self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
        {
            Box::pin(async move {
                assert_eq!(
                    self.child
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .snapshot()
                        .status,
                    crate::harness::instance::handle::AgentInstanceStatus::Stopped
                );
                self.rolled_back.notify_one();
                if self.rollback_pending {
                    self.rollback_release.notified().await;
                }
                assert!(!self.rollback_panic, "association rollback panicked");
                if self.fail_rollback {
                    Err("association cleanup failed".into())
                } else {
                    Ok(())
                }
            })
        }
    }

    #[tokio::test]
    async fn association_finishes_before_execution_and_sdk_observes_completion() {
        struct Association {
            manager: AgentManager,
            namespace: std::path::PathBuf,
        }

        impl super::SubagentAssociation for Association {
            fn associate(
                &self,
                child: AgentInstance,
                creation: super::AgentCreation,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
            {
                Box::pin(async move {
                    assert_eq!(child.id(), creation.instance_id());
                    assert_eq!(child.snapshot().status, AgentInstanceStatus::Idle);
                    assert!(child.transcript().await.unwrap().entries.is_empty());
                    assert!(!self.namespace.join("inboxes/security.json").exists());
                    assert!(
                        !self
                            .manager
                            .for_agent(creation.creator_id())
                            .unwrap()
                            .list()
                            .unwrap()
                            .iter()
                            .any(|member| member.instance.id == *child.id())
                    );
                    Ok(())
                })
            }

            fn rollback(
                &self,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>>
            {
                Box::pin(async { panic!("accepted association must not roll back") })
            }
        }

        let mut phi = test_phi();
        phi.hooks
            .register_hook(crate::hooks::Hook::agent_start(|_| async {
                std::future::pending::<crate::hooks::HookResult<()>>().await
            }));
        let directory =
            std::env::temp_dir().join(format!("phi-member-association-{}", uuid::Uuid::new_v4()));
        let manager = AgentManager::new();
        let parent = manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(
                        crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared")
                            .unwrap(),
                    )
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        manager
            .initialize_team(
                parent.id(),
                super::super::team::TeamStorageRoot::new(directory.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let namespace = directory
            .join("teams")
            .join(super::super::team::team_namespace(parent.id().as_str()).unwrap());
        let mut admission = reserve(
            &manager,
            &parent,
            &AgentSpawnRequest::new("reviewer", "inspect")
                .alias(super::super::AgentAlias::new("security").unwrap()),
        );
        let association = std::sync::Arc::new(Association {
            manager: manager.clone(),
            namespace,
        });
        let child_phi = phi.clone();
        let result = admission
            .prepare_with_association(
                move |creation| async move {
                    let candidate = super::prepare_sdk_child(&child_phi, &creation)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(super::PreparedSubagent::new(candidate, association))
                },
                None,
            )
            .await;
        admission.finish_admission(result).await.unwrap();
        let child = manager.get(admission.creation.instance_id()).unwrap();
        assert_eq!(child.snapshot().status, AgentInstanceStatus::Running);
        let (cancel, cancellation) = tokio::sync::watch::channel(false);
        let observed = admission.observe_completion(cancellation).unwrap();
        cancel.send_replace(true);
        let completion = tokio::time::timeout(std::time::Duration::from_secs(2), observed.finish())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Some(completion.clone()), admission.completion.get());
        assert_eq!(
            completion.outcome.unwrap_err().kind,
            crate::PhiFailureKind::Cancelled
        );
        assert!(manager.get(child.id()).is_ok());
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn failed_cancelled_and_abandoned_associations_compensate_before_acceptance() {
        let phi = test_phi();
        let manager = AgentManager::new();
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        for mode in 0..9 {
            let association = std::sync::Arc::new(TestAssociation {
                pending: matches!(mode, 1 | 2 | 5 | 6 | 7 | 8),
                fail_rollback: matches!(mode, 3 | 7),
                rollback_panic: mode == 8,
                rollback_pending: mode == 6,
                rollback_release: Default::default(),
                panic: mode == 4,
                entered: Default::default(),
                rolled_back: Default::default(),
                child: Default::default(),
            });
            let request = AgentSpawnRequest::new("reviewer", "inspect");
            let mut admission = reserve(&manager, &parent, &request);
            let completion = admission.completion.clone();
            let (cancel, mut cancellation) = tokio::sync::watch::channel(false);
            admission.cancellation = Some(cancellation.clone());
            let worker_phi = phi.clone();
            let staged = association.clone();
            let worker = tokio::spawn(async move {
                let prepare = move |creation| async move {
                    let candidate = super::prepare_sdk_child(&worker_phi, &creation)
                        .await
                        .map_err(|error| error.to_string())?;
                    Ok(super::PreparedSubagent::new(candidate, staged))
                };
                let result = tokio::select! {
                    biased;
                    _ = super::super::preparation::wait_for_cancellation(&mut cancellation) => Err(AgentManagerError::AdmissionInterrupted),
                    result = admission.prepare_with_association(prepare, None) => result,
                };
                admission.finish_admission(result).await
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                association.entered.notified(),
            )
            .await
            .unwrap();
            if mode == 1 {
                cancel.send_replace(true);
            }
            if mode == 5 {
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        assert!(completion.cancel());
                        assert!(completion.wait_admission_finished().await.is_none());
                        true
                    })
                    .await
                    .unwrap()
                );
                assert_eq!(manager.list().unwrap().len(), 1);
            }
            if matches!(mode, 7 | 8) {
                completion.cancel();
                assert!(
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        completion.wait_admission_finished()
                    )
                    .await
                    .unwrap()
                    .is_some()
                );
            }
            if mode == 6 {
                let cancelling = completion.clone();
                let cancellation = tokio::spawn(async move {
                    assert!(cancelling.cancel());
                    cancelling.wait_admission_finished().await
                });
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    association.rolled_back.notified(),
                )
                .await
                .unwrap();
                assert!(!cancellation.is_finished());
                assert_eq!(
                    manager
                        .lock_live()
                        .unwrap()
                        .admissions
                        .load(std::sync::atomic::Ordering::Relaxed),
                    1
                );
                assert_eq!(completion.admission(), SubagentAdmission::Pending);
                assert!(!cancellation.is_finished());
                association.rollback_release.notify_one();
                assert!(cancellation.await.unwrap().is_none());
            }
            if mode == 2 {
                worker.abort();
            }
            let result = worker.await;
            if mode == 2 {
                assert!(result.err().unwrap().is_cancelled());
            } else if matches!(mode, 4 | 8) {
                assert!(result.err().unwrap().is_panic());
            } else if matches!(mode, 3 | 7) {
                assert!(matches!(
                    result.unwrap(),
                    Err(AgentManagerError::Compensation { .. })
                ));
            } else {
                assert!(result.unwrap().is_err());
            }
            if mode != 6 {
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    association.rolled_back.notified(),
                )
                .await
                .unwrap();
                completion.wait_admission_finished().await;
                assert_eq!(completion.admission(), SubagentAdmission::Rejected);
            }
            assert_eq!(manager.list().unwrap().len(), 1);
            assert_eq!(
                manager
                    .lock_live()
                    .unwrap()
                    .admissions
                    .load(std::sync::atomic::Ordering::Relaxed),
                0
            );
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn teammate_files_precede_execution_and_alias_publication() {
        use super::super::AgentAlias;
        use super::super::team::{TeamCapability, TeamStorageRoot, team_namespace};

        enum StartupEnd {
            Accept,
            Disable,
            Abandon,
            RecordFailure,
        }

        for ending in [
            StartupEnd::Accept,
            StartupEnd::Disable,
            StartupEnd::Abandon,
            StartupEnd::RecordFailure,
        ] {
            let directory =
                std::env::temp_dir().join(format!("phi-member-start-{}", uuid::Uuid::new_v4()));
            let mut phi = test_phi();
            let entered = std::sync::Arc::new(tokio::sync::Notify::new());
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            let observed = entered.clone();
            let resume = release.clone();
            phi.hooks
                .register_hook(crate::hooks::Hook::user_prompt_submit(move |event| {
                    let entered = observed.clone();
                    let release = resume.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                            notices: Vec::new(),
                            prompt: event.prompt,
                            injected_context: Vec::new(),
                        })
                    }
                }));
            phi.hooks
                .register_hook(crate::hooks::Hook::agent_start(|_| async {
                    std::future::pending::<crate::hooks::HookResult<()>>().await
                }));
            let manager = AgentManager::new();
            let lead = manager
                .insert(
                    phi.agent_instance_builder()
                        .task_list(
                            crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared")
                                .unwrap(),
                        )
                        .build()
                        .await
                        .unwrap(),
                )
                .unwrap();
            let team = manager
                .initialize_team(
                    lead.id(),
                    TeamStorageRoot::new(directory.join("teams")).unwrap(),
                )
                .await
                .unwrap();
            let namespace = directory
                .join("teams")
                .join(team_namespace(lead.id().as_str()).unwrap());
            let mut accepted = 0;
            for alias in ["security", "performance"] {
                let request = AgentSpawnRequest::new("reviewer", "Inspect the change.")
                    .alias(AgentAlias::new(alias).unwrap());
                let mut admission = reserve(&manager, &lead, &request);
                let creation = admission.creation.clone();
                let candidate = super::prepare_sdk_child(&phi, &creation).await.unwrap();
                let child = admission.register(candidate, None).await.unwrap();
                let completion = admission.completion.clone();
                assert!(child.transcript().await.unwrap().entries.is_empty());
                assert!(!namespace.join(format!("inboxes/{alias}.json")).exists());
                let config = namespace.join("config.json");
                let original = std::fs::read(&config).unwrap();
                if matches!(ending, StartupEnd::RecordFailure) {
                    std::fs::write(&config, b"broken JSON").unwrap();
                }
                let running = child.clone();
                let worker = tokio::spawn(async move {
                    let result = admission.start_registered(&running).await;
                    admission.finish_admission(result).await
                });
                if matches!(ending, StartupEnd::RecordFailure) {
                    assert!(matches!(
                        worker.await.unwrap(),
                        Err(AgentManagerError::Compensation { .. })
                    ));
                    assert!(
                        tokio::time::timeout(std::time::Duration::ZERO, entered.notified())
                            .await
                            .is_err()
                    );
                    assert_eq!(child.snapshot().status, AgentInstanceStatus::Stopped);
                    assert!(manager.lock().unwrap().member(child.id()).unwrap().closing);
                    std::fs::write(&config, original).unwrap();
                    manager.stop(child.id()).await.unwrap();
                    assert!(!namespace.join(format!("inboxes/{alias}.json")).exists());
                    let replacement = reserve(&manager, &lead, &request);
                    drop(replacement);
                    break;
                }
                tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
                    .await
                    .unwrap();
                let record: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(namespace.join("config.json")).unwrap())
                        .unwrap();
                assert_eq!(record["members"].as_array().unwrap().len(), accepted + 2);
                let inbox: serde_json::Value = serde_json::from_slice(
                    &std::fs::read(namespace.join(format!("inboxes/{alias}.json"))).unwrap(),
                )
                .unwrap();
                assert_eq!(inbox["recipient"], child.id().as_str());
                let access = manager.for_agent(lead.id()).unwrap();
                assert!(
                    !access
                        .list()
                        .unwrap()
                        .iter()
                        .any(|member| member.instance.id == *child.id())
                );
                for selector in [
                    child.id().clone(),
                    crate::agent_instance::AgentInstanceId::new(alias),
                ] {
                    assert!(access.send_message(&selector, "premature").await.is_err());
                    assert!(
                        manager
                            .send_message(lead.id(), &selector, "premature")
                            .await
                            .is_err()
                    );
                }
                let child_access = manager.for_agent(child.id()).unwrap();
                assert!(child_access.list().unwrap().is_empty());
                assert!(child_access.task_list().is_err());
                match ending {
                    StartupEnd::Accept => {
                        release.notify_one();
                        worker.await.unwrap().unwrap();
                        assert!(
                            access
                                .list()
                                .unwrap()
                                .iter()
                                .any(|member| member.instance.id == *child.id())
                        );
                        assert!(child_access.task_list().is_ok());
                        accepted += 1;
                    }
                    StartupEnd::Disable => {
                        manager
                            .set_team_capability(&team, TeamCapability::Disabled)
                            .unwrap();
                        release.notify_one();
                        assert!(worker.await.unwrap().is_err());
                    }
                    StartupEnd::RecordFailure => {
                        unreachable!("file failure returned before prompt preparation")
                    }
                    StartupEnd::Abandon => {
                        worker.abort();
                        assert!(worker.await.unwrap_err().is_cancelled());
                        tokio::time::timeout(
                            std::time::Duration::from_secs(2),
                            completion.wait_admission_finished(),
                        )
                        .await
                        .unwrap();
                    }
                }
                if !matches!(ending, StartupEnd::Accept) {
                    assert_eq!(child.snapshot().status, AgentInstanceStatus::Stopped);
                    assert!(manager.get(child.id()).is_err());
                    assert!(!namespace.join(format!("inboxes/{alias}.json")).exists());
                    let record: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(namespace.join("config.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(record["members"].as_array().unwrap().len(), 1);
                    manager
                        .set_team_capability(&team, TeamCapability::Enabled)
                        .unwrap();
                    let replacement = reserve(&manager, &lead, &request);
                    drop(replacement);
                    break;
                }
            }
            manager.shutdown().await.unwrap();
            assert!(!namespace.exists());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn team_messages_record_before_admission_and_survive_sender_waiter_loss() {
        use super::super::AgentAlias;
        use super::super::message::AgentMessageDelivery;
        use super::super::team::{TeamStorageRoot, TeamStore, team_namespace};
        use std::future::Future;

        enum RecordingEnd {
            Publish,
            StopRecipient,
        }
        for ending in [RecordingEnd::Publish, RecordingEnd::StopRecipient] {
            let directory = std::env::temp_dir()
                .join(format!("phi-message-recording-{}", uuid::Uuid::new_v4()));
            let mut phi = test_phi();
            let entered = std::sync::Arc::new(tokio::sync::Notify::new());
            let observed = entered.clone();
            phi.hooks
                .register_hook(crate::hooks::Hook::user_prompt_submit(move |event| {
                    let entered = observed.clone();
                    async move {
                        if event.prompt.ends_with("first recorded message")
                            || event.prompt.ends_with("second recorded message")
                        {
                            entered.notify_one();
                            return std::future::pending().await;
                        }
                        Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                            notices: Vec::new(),
                            prompt: event.prompt,
                            injected_context: Vec::new(),
                        })
                    }
                }));
            phi.hooks
                .register_hook(crate::hooks::Hook::agent_start(|_| async {
                    std::future::pending::<crate::hooks::HookResult<()>>().await
                }));
            let manager = AgentManager::new();
            let lead = manager
                .insert(
                    phi.agent_instance_builder()
                        .task_list(
                            crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared")
                                .unwrap(),
                        )
                        .build()
                        .await
                        .unwrap(),
                )
                .unwrap();
            manager
                .initialize_team(
                    lead.id(),
                    TeamStorageRoot::new(directory.join("teams")).unwrap(),
                )
                .await
                .unwrap();
            let request = AgentSpawnRequest::new("reviewer", "Inspect the change.")
                .alias(AgentAlias::new("security").unwrap());
            let creation = manager
                .spawn_agent(lead.id(), &phi, request.clone())
                .await
                .unwrap();
            let child = manager.get(creation.instance_id()).unwrap();
            let namespace = directory
                .join("teams")
                .join(team_namespace(lead.id().as_str()).unwrap());
            let inbox = namespace.join("inboxes/security.json");
            let original = std::fs::read(&inbox).unwrap();
            std::fs::write(&inbox, b"broken JSON").unwrap();
            let access = manager.for_agent(lead.id()).unwrap();
            let selector = crate::agent_instance::AgentInstanceId::new("security");
            assert!(matches!(
                access.send_message(&selector, "must not activate").await,
                Err(AgentManagerError::TeamStorage { .. })
            ));
            assert!(
                tokio::time::timeout(std::time::Duration::ZERO, entered.notified())
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read(&inbox).unwrap(), b"broken JSON");
            std::fs::write(&inbox, original).unwrap();
            let epoch = manager.lock().unwrap().member(child.id()).unwrap().epoch;
            let store = TeamStore::lock(&directory.join("teams")).unwrap();
            {
                let sending = access.send_message(&selector, "first recorded message");
                tokio::pin!(sending);
                assert!(
                    std::future::poll_fn(|context| std::task::Poll::Ready(
                        sending.as_mut().poll(context)
                    ))
                    .await
                    .is_pending()
                );
            }
            {
                let mut registry = manager.lock().unwrap();
                assert!(registry.messages.has_pending_messages(epoch));
                assert!(registry.messages.pending(epoch).is_empty());
            }
            assert!(
                tokio::time::timeout(std::time::Duration::ZERO, entered.notified())
                    .await
                    .is_err()
            );
            match ending {
                RecordingEnd::Publish => {
                    drop(store);
                    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
                        .await
                        .unwrap();
                    let first_record: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&inbox).unwrap()).unwrap();
                    let first_id: super::super::message::AgentMessageId =
                        serde_json::from_value(first_record["messages"][0]["id"].clone()).unwrap();
                    let first = manager.message_updates(&first_id).unwrap().unwrap();
                    assert_eq!(first.snapshot().delivery, AgentMessageDelivery::Admitted);
                    let second = access
                        .send_message(&selector, "second recorded message")
                        .await
                        .unwrap();
                    let stored = std::fs::read(&inbox).unwrap();
                    let record: serde_json::Value = serde_json::from_slice(&stored).unwrap();
                    assert_eq!(record["recipient"], child.id().as_str());
                    assert_eq!(record["messages"].as_array().unwrap().len(), 2);
                    assert_eq!(record["messages"][0]["content"], "first recorded message");
                    assert_eq!(record["messages"][1]["id"], second.message().id.as_str());
                    assert_eq!(second.message().recipient, *child.id());
                    assert_eq!(first.snapshot().message_id, first_id);
                    assert_eq!(std::fs::read(&inbox).unwrap(), stored);
                }
                RecordingEnd::StopRecipient => {
                    let captured = manager.clone();
                    let id = child.id().clone();
                    let stopping = tokio::spawn(async move { captured.stop(&id).await });
                    tokio::time::timeout(std::time::Duration::from_secs(1), async {
                        while child.snapshot().status != AgentInstanceStatus::Stopped {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    drop(store);
                    stopping.await.unwrap().unwrap();
                    let replacement = manager.spawn_agent(lead.id(), &phi, request).await.unwrap();
                    assert_ne!(replacement.instance_id(), child.id());
                    let record: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(&inbox).unwrap()).unwrap();
                    assert_eq!(record["recipient"], replacement.instance_id().as_str());
                    assert!(record["messages"].as_array().unwrap().is_empty());
                    assert!(
                        tokio::time::timeout(std::time::Duration::ZERO, entered.notified())
                            .await
                            .is_err()
                    );
                }
            }
            manager.shutdown().await.unwrap();
            assert!(!namespace.exists());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[tokio::test]
    async fn model_send_returns_acceptance_without_waiting_for_recipient() {
        let phi = test_phi();
        let manager = AgentManager::new();
        let mut candidate = phi.agent_instance_builder().build().await.unwrap();
        let sender = candidate.install_test_agent_channel();
        let _source_events = candidate.install_test_main_run("message-turn");
        let source = manager.insert(candidate).unwrap();
        let mut candidate = phi.agent_instance_builder().build().await.unwrap();
        let _target_events = candidate.install_test_main_run("held-turn");
        let target = manager.insert(candidate).unwrap();
        manager
            .grant_communication(source.id(), target.id())
            .unwrap();
        let (reply, response) = tokio::sync::oneshot::channel();
        sender
            .send(crate::agent::tools::AgentToolRequest {
                args: crate::agent::tools::AgentArgs::Manage(
                    crate::agent::tools::AgentManagementArgs::SendMessage {
                        recipient: target.id().clone(),
                        content: "continue work".into(),
                    },
                ),
                scope: crate::core::event::CoreEventScope {
                    session_id: Some(source.id().as_str().into()),
                    turn_id: Some("message-turn".into()),
                    parent_tool_call_id: Some("message-call".into()),
                    ..Default::default()
                },
                reply,
            })
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), response)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            crate::agent::tools::AgentToolOutput::MessageSent(recipient) if &recipient == target.id()
        ));
        assert!(target.transcript().await.unwrap().entries.is_empty());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn model_message_recording_keeps_actor_responsive_and_rejects_reused_sender() {
        use super::super::team::{TeamStorageRoot, TeamStore, team_namespace};
        let directory =
            std::env::temp_dir().join(format!("phi-model-message-{}", uuid::Uuid::new_v4()));
        let phi = test_phi();
        let manager = AgentManager::new();
        let mut candidate = phi.agent_instance_builder().build().await.unwrap();
        let sender = candidate.install_test_agent_channel();
        let _events = candidate.install_test_main_run("message-turn");
        let source = manager.insert(candidate).unwrap();
        let target = manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(
                        crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared")
                            .unwrap(),
                    )
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        manager
            .initialize_team(
                target.id(),
                TeamStorageRoot::new(directory.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        manager
            .grant_communication(source.id(), target.id())
            .unwrap();
        let epoch = manager.lock().unwrap().member(target.id()).unwrap().epoch;
        let store = TeamStore::lock(&directory.join("teams")).unwrap();
        let (reply, response) = tokio::sync::oneshot::channel();
        sender
            .send(crate::agent::tools::AgentToolRequest {
                args: crate::agent::tools::AgentArgs::Manage(
                    crate::agent::tools::AgentManagementArgs::SendMessage {
                        recipient: target.id().clone(),
                        content: "must not reach successor scope".into(),
                    },
                ),
                scope: crate::core::event::CoreEventScope {
                    session_id: Some(source.id().as_str().into()),
                    turn_id: Some("message-turn".into()),
                    parent_tool_call_id: Some("message-call".into()),
                    ..Default::default()
                },
                reply,
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !manager.lock().unwrap().messages.has_pending_messages(epoch) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(manager.lock().unwrap().messages.pending(epoch).is_empty());
        tokio::time::timeout(std::time::Duration::from_secs(1), source.view())
            .await
            .unwrap()
            .unwrap();
        let transcript =
            tokio::time::timeout(std::time::Duration::from_secs(1), manager.stop(source.id()))
                .await
                .unwrap()
                .unwrap();
        let replacement = manager
            .insert(
                phi.agent_instance_builder()
                    .transcript(transcript)
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        assert_eq!(replacement.id(), source.id());
        drop(store);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(2), response)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        let namespace = directory
            .join("teams")
            .join(team_namespace(target.id().as_str()).unwrap());
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(namespace.join("inboxes/lead.json")).unwrap())
                .unwrap();
        assert!(record["messages"].as_array().unwrap().is_empty());
        assert!(target.transcript().await.unwrap().entries.is_empty());
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn named_startup_reservations_compensate_and_cannot_cross_root_registrations() {
        use super::super::{AgentAlias, AgentKind};
        let phi = test_phi();
        let manager = AgentManager::new();
        let binding =
            crate::tasks::TaskListBinding::new("/tmp/phi-named-startup", "shared").unwrap();
        let lead = manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(binding.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let team = manager
            .initialize_team(
                lead.id(),
                super::super::team::TeamStorageRoot::new(
                    std::env::temp_dir().join(format!("phi-named-team-{}", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        let epoch = manager
            .lock_live()
            .unwrap()
            .member(lead.id())
            .unwrap()
            .epoch;
        let request = AgentSpawnRequest::new("reviewer", "Review security.")
            .alias(AgentAlias::new("security").unwrap());
        let directory = lead.snapshot().working_dir;
        let admission = manager
            .reserve_initial(lead.id(), None, &request, directory.clone(), epoch, None)
            .unwrap();
        assert!(matches!(
            admission.creation.kind(),
            AgentKind::Teammate { .. }
        ));
        assert!(matches!(
            manager.reserve_initial(lead.id(), None, &request, directory.clone(), epoch, None),
            Err(AgentManagerError::AliasInUse { .. })
        ));
        drop(admission);
        assert!(
            manager
                .lock_live()
                .unwrap()
                .team(&team)
                .unwrap()
                .reservations
                .is_empty()
        );
        let admission = manager
            .reserve_initial(lead.id(), None, &request, directory, epoch, None)
            .unwrap();
        manager
            .set_team_capability(&team, super::super::team::TeamCapability::Disabled)
            .unwrap();
        let candidate = super::prepare_sdk_child(&phi, &admission.creation)
            .await
            .unwrap();
        assert!(matches!(
            manager
                .lock_live()
                .unwrap()
                .register_creation(candidate, &admission.creation),
            Err(AgentManagerError::AdmissionInterrupted)
        ));
        drop(admission);
        manager
            .set_team_capability(&team, super::super::team::TeamCapability::Enabled)
            .unwrap();
        for _ in 0..2 {
            let failed = AgentSpawnRequest::new("missing", "No such definition.")
                .alias(AgentAlias::new("security").unwrap());
            assert!(manager.spawn_agent(lead.id(), &phi, failed).await.is_err());
            let registry = manager.lock_live().unwrap();
            assert!(registry.team(&team).unwrap().reservations.is_empty());
            assert_eq!(registry.team(&team).unwrap().config.members().len(), 1);
        }
        let outsider = manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(binding)
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let request = AgentSpawnRequest::new("missing", "No team context.")
            .alias(AgentAlias::new("security").unwrap());
        let outsider_epoch = manager
            .lock_live()
            .unwrap()
            .member(outsider.id())
            .unwrap()
            .epoch;
        let ordinary = manager
            .reserve_initial(
                outsider.id(),
                None,
                &request,
                outsider.snapshot().working_dir,
                outsider_epoch,
                None,
            )
            .unwrap();
        assert!(matches!(
            ordinary.creation.kind(),
            AgentKind::Subagent { .. }
        ));
        assert!(
            manager
                .lock_live()
                .unwrap()
                .team(&team)
                .unwrap()
                .reservations
                .is_empty()
        );
        drop(ordinary);
        manager.shutdown().await.unwrap();
    }

    fn test_phi() -> Phi {
        let descriptor = crate::models::ModelDescriptor {
            provider: "openai-codex".into(),
            id: "gpt-5.6-sol".into(),
            name: "Test".into(),
            api: crate::provider::ProviderApi::OpenAiCodexResponses,
            reasoning: true,
            thinking_level_map: None,
            input: vec![crate::models::ModelInput::Text],
            context_window: 272_000,
            max_output_tokens: 128_000,
            cost: Default::default(),
        };
        let catalog =
            std::sync::Arc::new(crate::models::ModelCatalog::for_test_model(descriptor).unwrap());
        Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .no_tools(true)
            .agent(AgentDefinition::new(
                "reviewer",
                "Review",
                "Review the task",
            ))
            .build()
            .unwrap()
            .with_runtime_services(PhiRuntimeServices::new().model_catalog(catalog))
    }

    #[tokio::test]
    async fn sdk_spawn_uses_independent_identity_history_and_explicit_team() {
        let directory =
            std::env::temp_dir().join(format!("phi-sdk-child-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(directory.join("child")).unwrap();
        let phi = test_phi();
        let manager = AgentManager::new();
        let parent = manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(directory.clone())
                    .task_list(
                        crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared")
                            .unwrap(),
                    )
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let access = manager.for_agent(parent.id()).unwrap();
        let team = manager
            .initialize_team(
                parent.id(),
                super::super::team::TeamStorageRoot::new(directory.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let request = AgentSpawnRequest::new("reviewer", "inspect")
            .alias(super::super::AgentAlias::new("reviewer").unwrap());
        let started = access.spawn_agent(&phi, request).await.unwrap();
        let child = manager
            .get(&crate::harness::instance::AgentInstanceId::new(
                started.instance_id().as_str(),
            ))
            .unwrap();
        assert_ne!(child.id(), parent.id());
        assert_eq!(started.creator_id().as_str(), parent.id().as_str());
        assert_eq!(started.working_dir(), directory.canonicalize().unwrap());
        assert_eq!(
            parent
                .subagent_status(started.instance_id().clone())
                .await
                .unwrap()
                .unwrap()
                .admission,
            SubagentAdmission::Accepted
        );
        let members = access.list().unwrap();
        assert_eq!(
            members
                .iter()
                .find(|member| member.instance.id == *child.id())
                .unwrap()
                .team,
            Some(team)
        );
        child.abort().await.unwrap();
        let transcript = child.transcript().await.unwrap();
        assert_eq!(
            transcript
                .entries
                .iter()
                .filter(|entry| matches!(
                    &entry.kind,
                    TranscriptEntryKind::Message {
                        message: AgentMessage::User { .. }
                    }
                ))
                .count(),
            1
        );
        assert!(!parent.transcript().await.unwrap().has_user_message());
        let outsider = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        assert!(matches!(
            manager
                .for_agent(outsider.id())
                .unwrap()
                .transcript(child.id())
                .await,
            Err(AgentManagerError::Denied { .. })
        ));
        manager.stop(outsider.id()).await.unwrap();
        let mut selected = AgentSpawnRequest::new("reviewer", "second");
        selected.working_dir = Some("child".into());
        let omitted = manager
            .spawn_agent(parent.id(), &phi, selected)
            .await
            .unwrap();
        let omitted_child = manager
            .get(&crate::harness::instance::AgentInstanceId::new(
                omitted.instance_id().as_str(),
            ))
            .unwrap();
        assert_eq!(
            omitted_child.snapshot().working_dir,
            directory.join("child").canonicalize().unwrap()
        );
        assert_eq!(
            access
                .list()
                .unwrap()
                .into_iter()
                .find(|member| member.instance.id == *omitted_child.id())
                .unwrap()
                .team,
            None
        );
        for selected in [std::path::PathBuf::from("."), directory.clone()] {
            let mut request = AgentSpawnRequest::new("reviewer", "same directory");
            request.working_dir = Some(selected);
            let task = manager
                .spawn_agent(parent.id(), &phi, request)
                .await
                .unwrap();
            assert_eq!(task.working_dir(), directory.canonicalize().unwrap());
        }
        assert!(!directory.join(".git").exists());
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn admission_failure_leaves_no_child_or_reservation() {
        let phi = test_phi();
        let manager = AgentManager::new();
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        assert!(
            manager
                .spawn_agent(
                    parent.id(),
                    &phi,
                    AgentSpawnRequest::new("missing", "invalid")
                )
                .await
                .is_err()
        );
        let mut invalid_model = phi.clone();
        invalid_model.context.agent_catalog = crate::agent::custom::AgentCatalog::new(vec![
            AgentDefinition::new("reviewer", "Review", "Review the task").model("not-a-model"),
        ]);
        assert!(
            manager
                .spawn_agent(
                    parent.id(),
                    &invalid_model,
                    AgentSpawnRequest::new("reviewer", "invalid model")
                )
                .await
                .is_err()
        );
        assert_eq!(manager.list().unwrap().len(), 1);
        let unavailable = phi.clone().with_runtime_services(PhiRuntimeServices::new());
        for _ in 0..=super::super::policy::MAX_CHILD_WORKERS_PER_PARENT {
            assert!(matches!(
                manager
                    .spawn_agent(
                        parent.id(),
                        &unavailable,
                        AgentSpawnRequest::new("reviewer", "invalid")
                    )
                    .await,
                Err(AgentManagerError::Preparation { .. })
            ));
        }
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .admissions
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(manager.list().unwrap().len(), 1);
        manager.shutdown().await.unwrap();
    }

    fn reserve(
        manager: &AgentManager,
        parent: &crate::harness::instance::AgentInstance,
        request: &AgentSpawnRequest,
    ) -> super::InitialAdmission {
        let (directory, epoch) = manager.creation_scope(parent.id(), None).unwrap();
        let directory =
            super::resolve_working_dir(&directory, request.working_dir.as_deref()).unwrap();
        manager
            .reserve_initial(parent.id(), None, request, directory, epoch, None)
            .unwrap()
    }

    #[tokio::test]
    async fn idle_children_do_not_charge_creation_capacity() {
        let mut phi = test_phi();
        phi.hooks
            .register_hook(crate::hooks::Hook::agent_start(|_| async {
                std::future::pending::<crate::hooks::HookResult<()>>().await
            }));
        let manager = AgentManager::new();
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let request = AgentSpawnRequest::new("reviewer", "inspect");
        for _ in 0..super::super::policy::MAX_CHILD_WORKERS_PER_PARENT {
            let started = manager
                .spawn_agent(parent.id(), &phi, request.clone())
                .await
                .unwrap();
            parent
                .cancel_subagent(started.instance_id().clone())
                .await
                .unwrap();
            assert!(manager.get(started.instance_id()).is_ok());
        }
        let mut admissions = vec![reserve(&manager, &parent, &request)];
        assert!(
            admissions[0]
                .completion
                .executor_registration_id()
                .is_none()
        );
        for _ in 1..super::super::policy::MAX_CHILD_WORKERS_PER_PARENT {
            admissions.push(reserve(&manager, &parent, &request));
        }
        assert!(matches!(
            manager
                .spawn_agent(parent.id(), &phi, request.clone())
                .await,
            Err(AgentManagerError::Subagent(
                crate::harness::instance::execution::PhiSubagentError::ChildLimit
            ))
        ));
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .member(parent.id())
                .unwrap()
                .admissions
                .load(std::sync::atomic::Ordering::Relaxed),
            super::super::policy::MAX_CHILD_WORKERS_PER_PARENT
        );
        drop(admissions.pop());
        admissions.push(reserve(&manager, &parent, &request));
        drop(admissions);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn aggregate_admission_capacity_rejects_without_allocation_and_reclaims_permits() {
        let phi = test_phi();
        let capacity = super::MAX_PENDING_ADMISSIONS;
        let manager = AgentManager::new();
        let request = AgentSpawnRequest::new("reviewer", "inspect");
        let mut reservations = Vec::new();
        while reservations.len() < capacity {
            let parent = manager
                .insert(phi.agent_instance_builder().build().await.unwrap())
                .unwrap();
            for _ in 0..super::super::policy::MAX_CHILD_WORKERS_PER_PARENT {
                reservations.push(reserve(&manager, &parent, &request));
            }
        }
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let members = manager.list().unwrap().len();
        assert!(matches!(
            manager
                .spawn_agent(parent.id(), &phi, request.clone())
                .await,
            Err(AgentManagerError::AdmissionLimit { limit }) if limit == capacity
        ));
        assert!(matches!(
            manager
                .for_agent(parent.id())
                .unwrap()
                .spawn_agent(&phi, request.clone())
                .await,
            Err(AgentManagerError::AdmissionLimit { .. })
        ));
        assert_eq!(manager.list().unwrap().len(), members);
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .admissions
                .load(std::sync::atomic::Ordering::Relaxed),
            capacity
        );
        drop(reservations.pop());
        let admission = reserve(&manager, &parent, &request);
        assert_eq!(admission.completion.admission(), SubagentAdmission::Pending);
        drop(admission);
        drop(reservations);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn depth_abandonment_and_identity_reuse_reject_startup() {
        let phi = test_phi();
        let manager = AgentManager::new();
        let parent = manager
            .insert(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let access = manager.for_agent(parent.id()).unwrap();
        let request = AgentSpawnRequest::new("reviewer", "inspect");
        let child = access
            .register_child(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        let grandchild = manager
            .for_agent(&child.instance.id)
            .unwrap()
            .register_child(phi.agent_instance_builder().build().await.unwrap())
            .unwrap();
        assert!(matches!(
            manager
                .spawn_agent(&grandchild.instance.id, &phi, request.clone())
                .await,
            Err(AgentManagerError::Subagent(
                crate::harness::instance::execution::PhiSubagentError::DepthLimit
            ))
        ));
        manager.stop(&grandchild.instance.id).await.unwrap();
        manager.stop(&child.instance.id).await.unwrap();
        let mut admission = reserve(&manager, &parent, &request);
        let completion = admission.completion.clone();
        let candidate = super::prepare_sdk_child(&phi, &admission.creation)
            .await
            .unwrap();
        let child = admission.register(candidate, None).await.unwrap();
        drop(admission);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut snapshots = child.subscribe_snapshots();
            while snapshots.snapshot().status
                != crate::harness::instance::AgentInstanceStatus::Stopped
            {
                snapshots.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        completion.wait_admission_finished().await;
        assert_eq!(completion.admission(), SubagentAdmission::Rejected);
        assert_eq!(manager.list().unwrap().len(), 1);

        let mut admission = reserve(&manager, &parent, &request);
        let transcript = manager.stop(parent.id()).await.unwrap();
        let replacement = manager
            .insert(
                phi.agent_instance_builder()
                    .transcript(transcript)
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        assert!(matches!(
            admission.prepare_and_start(&phi, None).await,
            Err(AgentManagerError::AdmissionInterrupted)
        ));
        assert!(matches!(
            access.spawn_agent(&phi, request).await,
            Err(AgentManagerError::Denied { .. })
        ));
        assert_eq!(manager.list().unwrap().len(), 1);
        assert_eq!(manager.list().unwrap()[0].id, *replacement.id());
        drop(admission);
        manager.shutdown().await.unwrap();
    }
}
