use super::registry::{AgentManager, AgentManagerError, Member, Registry, instance_error};
use crate::harness::instance::execution::PhiSubagentError;
use crate::harness::instance::{AgentInstance, AgentInstanceId};
pub(crate) use crate::harness::manager::{AgentAccess, AgentOperation};
use crate::transcript::Transcript;

// Registration-bound authority and target scope; role guidance and messages cannot grant access.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTargetScope {
    Descendant,
    DirectRelative,
    Peer,
    ReviewTarget,
    AuthorizedCommunication,
    Unrelated,
}

pub(crate) fn permits(operation: AgentOperation, scope: AgentTargetScope) -> bool {
    match operation {
        AgentOperation::CreateAgent | AgentOperation::CreateTeam => true,
        AgentOperation::Observe | AgentOperation::Message => matches!(
            scope,
            AgentTargetScope::Descendant
                | AgentTargetScope::DirectRelative
                | AgentTargetScope::Peer
                | AgentTargetScope::AuthorizedCommunication
        ),
        AgentOperation::ReadHistory => matches!(
            scope,
            AgentTargetScope::Descendant | AgentTargetScope::ReviewTarget
        ),
        AgentOperation::Stop | AgentOperation::Interrupt => scope == AgentTargetScope::Descendant,
    }
}

#[derive(Clone)]
pub(crate) struct InstanceAgentScope {
    owner: std::sync::Weak<std::sync::Mutex<Registry>>,
    caller: AgentInstanceId,
    epoch: uuid::Uuid,
    pub(crate) finalization: tokio::sync::watch::Sender<super::control::AgentFinalizationStatus>,
}

impl InstanceAgentScope {
    pub(crate) fn new(
        owner: std::sync::Weak<std::sync::Mutex<Registry>>,
        caller: AgentInstanceId,
        epoch: uuid::Uuid,
    ) -> Self {
        Self {
            owner,
            caller,
            epoch,
            finalization: tokio::sync::watch::channel(
                super::control::AgentFinalizationStatus::Open,
            )
            .0,
        }
    }

    pub(crate) fn registration_id(&self) -> super::control::AgentRegistrationId {
        super::control::AgentRegistrationId::new(self.epoch)
    }

    pub(crate) async fn publish_idle_report(
        &self,
        content: String,
    ) -> Result<(), AgentManagerError> {
        AgentManager::from_scope(&self.owner)?
            .publish_idle_report(&self.caller, self.epoch, content)
            .await
    }

    pub(crate) async fn stop(
        &self,
        instance: AgentInstance,
    ) -> Result<Transcript, AgentManagerError> {
        match AgentManager::from_scope(&self.owner) {
            Ok(manager) => manager.stop_registered(instance, self.epoch).await,
            Err(AgentManagerError::Unavailable) => {
                instance.close_registration();
                tokio::spawn(async move {
                    let result = instance
                        .stop_local()
                        .await
                        .map_err(|source| instance_error(instance.id(), source));
                    instance.finish_registration(result.as_ref().err().map(ToString::to_string));
                    result
                })
                .await
                .map_err(|error| AgentManagerError::StopCoordination {
                    message: error.to_string(),
                })?
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn access(&self) -> Result<AgentAccess, AgentManagerError> {
        let manager = AgentManager::from_scope(&self.owner)?;
        Ok(AgentAccess::new(manager, self.caller.clone(), self.epoch))
    }

    pub(crate) fn has_pending_execution(&self) -> Result<bool, AgentManagerError> {
        let manager = AgentManager::from_scope(&self.owner)?;
        let registry = manager.lock_live()?;
        let member = registry.member(&self.caller)?;
        if member.epoch != self.epoch || member.closing {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        for candidate in registry.members.values() {
            let descendant = candidate.ancestors.contains(&self.epoch);
            if candidate.epoch != self.epoch && !descendant {
                continue;
            }
            if candidate
                .admissions
                .load(std::sync::atomic::Ordering::Acquire)
                != 0
                || registry.messages.has_pending_messages(candidate.epoch)
                || (descendant
                    && candidate
                        .instance
                        .execution_pending()
                        .map_err(|source| instance_error(candidate.instance.id(), source))?)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub(crate) fn pending_messages(
        &self,
    ) -> Result<Vec<std::sync::Arc<super::message::MessageRoute>>, AgentManagerError> {
        let manager = AgentManager::from_scope(&self.owner)?;
        let mut registry = manager.lock_live()?;
        let member = registry.member(&self.caller)?;
        if member.epoch != self.epoch || member.closing {
            return Ok(Vec::new());
        }
        Ok(registry.messages.pending(self.epoch))
    }

    pub(crate) fn reserve(
        &self,
        request: super::preparation::AgentSpawnRequest,
        working_dir: std::path::PathBuf,
        tool_call_id: Option<compact_str::CompactString>,
        agent_origin: bool,
    ) -> Result<super::coordinator::ReservedSubagent, AgentManagerError> {
        let manager = AgentManager::from_scope(&self.owner)?;
        let access = AgentAccess::new(manager.clone(), self.caller.clone(), self.epoch);
        {
            let registry = manager.lock_live()?;
            access.caller(&registry, AgentOperation::CreateAgent)?;
        }
        manager.reserve_for_instance(
            &self.caller,
            agent_origin.then_some(access),
            request,
            working_dir,
            self.epoch,
            tool_call_id,
        )
    }
}

impl AgentAccess {
    pub(crate) fn new(manager: AgentManager, caller: AgentInstanceId, epoch: uuid::Uuid) -> Self {
        Self {
            manager,
            caller,
            epoch,
        }
    }

    pub(crate) fn caller<'a>(
        &self,
        registry: &'a Registry,
        operation: AgentOperation,
    ) -> Result<&'a Member, AgentManagerError> {
        let caller = registry.member(&self.caller)?;
        if caller.epoch != self.epoch {
            return Err(denied(&self.caller, operation, self.caller.as_str()));
        }
        Ok(caller)
    }

    pub(crate) fn require_creation(
        &self,
        registry: &Registry,
        operation: AgentOperation,
    ) -> Result<(), AgentManagerError> {
        let caller = self.caller(registry, operation)?;
        if caller.closing || !registry.is_published(caller) {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        if permits(operation, AgentTargetScope::Unrelated) {
            return Ok(());
        }
        Err(denied(&self.caller, operation, self.caller.as_str()))
    }

    pub(crate) fn require_message(
        &self,
        registry: &Registry,
        recipient: &AgentInstanceId,
    ) -> Result<(), AgentManagerError> {
        self.control_target(registry, recipient, AgentOperation::Message)
            .map(|_| ())
    }

    pub(crate) fn caller_id(&self) -> &AgentInstanceId {
        &self.caller
    }

    pub(crate) fn allows(
        &self,
        registry: &Registry,
        caller: &Member,
        target: &Member,
        operation: AgentOperation,
    ) -> bool {
        if !registry.is_published(caller) || !registry.is_published(target) {
            return false;
        }
        if target.epoch == self.epoch
            && matches!(
                operation,
                AgentOperation::Observe | AgentOperation::ReadHistory
            )
        {
            return true;
        }
        permits(operation, registry.scope(caller, target, operation))
    }

    pub(crate) fn control_target(
        &self,
        registry: &Registry,
        target: &AgentInstanceId,
        operation: AgentOperation,
    ) -> Result<(AgentInstance, uuid::Uuid), AgentManagerError> {
        let caller = self.caller(registry, operation)?;
        let member = registry.resolve_target(caller, target)?;
        if caller.closing || !self.allows(registry, caller, member, operation) {
            return Err(denied(&self.caller, operation, target.as_str()));
        }
        Ok((member.instance.clone(), member.epoch))
    }

    pub(crate) fn history_reader(
        &self,
        registry: &Registry,
        target: &AgentInstanceId,
    ) -> Result<AgentInstance, AgentManagerError> {
        let caller = self.caller(registry, AgentOperation::ReadHistory)?;
        let member = registry.resolve_target(caller, target)?;
        if !self.allows(registry, caller, member, AgentOperation::ReadHistory) {
            return Err(denied(
                &self.caller,
                AgentOperation::ReadHistory,
                target.as_str(),
            ));
        }
        Ok(member.instance.clone())
    }
}

pub(crate) fn denied(
    caller: &AgentInstanceId,
    operation: AgentOperation,
    target: &str,
) -> AgentManagerError {
    AgentManagerError::Denied {
        caller: caller.to_string(),
        operation,
        target: target.to_string(),
    }
}

pub(crate) const MAX_CHILD_WORKERS_PER_PARENT: usize = 4;

pub(crate) const MAX_DELEGATION_DEPTH: usize = 2;

pub(crate) fn validate_delegation_capacity(
    depth: usize,
    pending_admissions: usize,
) -> Result<(), PhiSubagentError> {
    if depth >= MAX_DELEGATION_DEPTH {
        return Err(PhiSubagentError::DepthLimit);
    }
    if pending_admissions >= MAX_CHILD_WORKERS_PER_PARENT {
        return Err(PhiSubagentError::ChildLimit);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPES: [AgentTargetScope; 6] = [
        AgentTargetScope::Descendant,
        AgentTargetScope::DirectRelative,
        AgentTargetScope::Peer,
        AgentTargetScope::ReviewTarget,
        AgentTargetScope::AuthorizedCommunication,
        AgentTargetScope::Unrelated,
    ];

    fn assert_scopes(operation: AgentOperation, allowed: &[AgentTargetScope]) {
        for scope in SCOPES {
            assert_eq!(
                permits(operation, scope),
                allowed.contains(&scope),
                "{operation:?} on {scope:?}"
            );
        }
    }

    #[test]
    fn management_requires_target_authority() {
        for operation in [
            AgentOperation::Stop,
            AgentOperation::Interrupt,
            AgentOperation::ReadHistory,
        ] {
            let allowed = if operation == AgentOperation::ReadHistory {
                &[AgentTargetScope::Descendant, AgentTargetScope::ReviewTarget][..]
            } else {
                &[AgentTargetScope::Descendant][..]
            };
            assert_scopes(operation, allowed);
        }
    }

    #[test]
    fn collaboration_has_no_history_or_control_grants() {
        let collaboration = [
            AgentTargetScope::Descendant,
            AgentTargetScope::DirectRelative,
            AgentTargetScope::Peer,
            AgentTargetScope::AuthorizedCommunication,
        ];
        for operation in [AgentOperation::Observe, AgentOperation::Message] {
            assert_scopes(operation, &collaboration);
        }
    }

    #[test]
    fn creation_has_no_responsibility_role_gate() {
        for operation in [AgentOperation::CreateAgent, AgentOperation::CreateTeam] {
            assert_scopes(operation, &SCOPES);
        }
    }
}
