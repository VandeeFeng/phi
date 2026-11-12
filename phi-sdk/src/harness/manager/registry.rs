use super::policy::{AgentOperation, AgentTargetScope, denied};
use super::preparation::AgentCreation;
use super::team::{
    AgentTeam, AgentTeamMember, AliasPublication, AliasRegistration, AliasReservation, LiveTeam,
    TeamCapability, TeamFiles, TeamStorageRoot,
};
use super::{AgentAlias, AgentKind, LEAD_ALIAS};
use crate::harness::instance::execution::AgentInstanceCandidate;
use crate::harness::instance::{
    AgentInstance, AgentInstanceError, AgentInstanceId, AgentInstanceStatus,
};
pub(crate) use crate::harness::manager::{
    AgentManager, AgentManagerError, AgentMemberSnapshot, AgentTeamId,
};
#[cfg(test)]
use crate::transcript::Transcript;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

// Instance membership, fixed Teams and captured registration epochs; never owns mutable execution.

const MAX_AGENT_INSTANCES: usize = 1024;
const MAX_AGENT_TEAMS: usize = 1024;
const MAX_TEAM_MEMBERS: usize = 1024;

pub(crate) struct Member {
    pub(crate) instance: AgentInstance,
    pub(crate) epoch: uuid::Uuid,
    pub(crate) closing: bool,
    pub(crate) team: Option<AgentTeamId>,
    pub(crate) alias: Option<AgentAlias>,
    kind: AgentKind,
    pub(crate) background_required: bool,
    pub(crate) ancestors: Vec<uuid::Uuid>,
    pub(crate) admissions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(super) exit: Option<std::sync::Arc<tokio::sync::Mutex<super::control::TeammateExit>>>,
}

impl Member {
    fn new(
        instance: AgentInstance,
        epoch: uuid::Uuid,
        creator: Option<&AgentInstanceId>,
        ancestors: Vec<uuid::Uuid>,
    ) -> Self {
        Self {
            instance,
            epoch,
            closing: false,
            team: None,
            alias: None,
            kind: creator.map_or(AgentKind::Master, |creator| AgentKind::Subagent {
                creator: creator.clone(),
            }),
            background_required: false,
            ancestors,
            admissions: Default::default(),
            exit: None,
        }
    }

    pub(crate) fn snapshot(&self) -> AgentMemberSnapshot {
        AgentMemberSnapshot {
            instance: self.instance.snapshot(),
            team: self.team.clone(),
            created_by: self.kind.creator().cloned(),
            kind: self.kind.clone(),
            alias: self.alias.clone(),
        }
    }
}

struct OrdinaryAlias {
    instance_id: AgentInstanceId,
    token: uuid::Uuid,
    epoch: Option<uuid::Uuid>,
    ready: bool,
}

#[derive(Default)]
pub(crate) struct Registry {
    pub(crate) owner: std::sync::Weak<Mutex<Registry>>,
    pub(crate) members: HashMap<AgentInstanceId, Member>,
    pub(crate) shutting_down: bool,
    pub(crate) admissions: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) messages: super::message::MessageMailbox,
    pub(crate) teams: HashMap<AgentTeamId, LiveTeam>,
    ordinary_aliases: HashMap<(uuid::Uuid, AgentAlias), OrdinaryAlias>,
    pub(crate) communication: HashSet<(uuid::Uuid, uuid::Uuid)>,
    pub(crate) reviews: HashSet<(uuid::Uuid, uuid::Uuid)>,
}

impl Registry {
    pub(super) fn with_owner(owner: std::sync::Weak<Mutex<Registry>>) -> Self {
        Self {
            owner,
            ..Default::default()
        }
    }

    pub(crate) fn member(&self, id: &AgentInstanceId) -> Result<&Member, AgentManagerError> {
        self.members
            .get(id)
            .ok_or_else(|| AgentManagerError::Unknown { id: id.to_string() })
    }

    pub(crate) fn team(&self, id: &AgentTeamId) -> Result<&LiveTeam, AgentManagerError> {
        self.teams
            .get(id)
            .ok_or_else(|| AgentManagerError::UnknownTeam { id: id.0.clone() })
    }

    pub(super) fn reserve_team(
        &mut self,
        lead: &AgentInstanceId,
        root: TeamStorageRoot,
    ) -> Result<
        (
            AgentTeam,
            std::sync::Arc<TeamFiles>,
            tokio::sync::OwnedMutexGuard<()>,
        ),
        AgentManagerError,
    > {
        if self.shutting_down {
            return Err(AgentManagerError::ShuttingDown);
        }
        let member = self.member(lead)?;
        if member.closing
            || member.kind != AgentKind::Master
            || member.team.is_some()
            || self
                .teams
                .values()
                .any(|team| team.lead_epoch == member.epoch)
        {
            return Err(denied(lead, AgentOperation::CreateTeam, lead.as_str()));
        }
        let binding =
            member
                .instance
                .task_list()
                .cloned()
                .ok_or(AgentManagerError::InvalidCandidate {
                    reason: "Team initialization requires a resolved task list binding",
                })?;
        let alias = AgentAlias::new(LEAD_ALIAS).expect("reserved lead alias");
        let epoch = member.epoch;
        let id = AgentTeamId(uuid::Uuid::new_v4().to_string());
        let config = AgentTeam::new(
            id.clone(),
            AgentTeamMember::new(
                lead.clone(),
                alias.clone(),
                member.instance.snapshot().definition.as_deref(),
                member.instance.snapshot().working_dir,
            ),
            binding,
            Vec::new(),
        )
        .map_err(|error| AgentManagerError::TeamDefinition {
            message: error.to_string(),
        })?;
        if self
            .teams
            .values()
            .any(|team| team.config.namespace() == config.namespace())
        {
            return Err(AgentManagerError::TeamNamespaceConflict {
                namespace: config.namespace().into(),
            });
        }
        if self.teams.len() >= MAX_AGENT_TEAMS {
            return Err(AgentManagerError::TeamLimit {
                limit: MAX_AGENT_TEAMS,
            });
        }
        let (files, lane) = TeamFiles::reserve(root);
        self.teams.insert(
            id.clone(),
            LiveTeam {
                config: config.clone(),
                lead_epoch: epoch,
                capability: TeamCapability::Enabled,
                registrations: HashMap::from([(
                    alias.clone(),
                    AliasRegistration {
                        epoch,
                        publication: AliasPublication::Ready,
                    },
                )]),
                reservations: HashMap::new(),
                files: files.clone(),
                ready: false,
            },
        );
        Ok((config, files, lane))
    }

    pub(crate) fn reserve_alias(
        &mut self,
        creation: &mut AgentCreation,
    ) -> Result<(), AgentManagerError> {
        let Some(alias) = creation.alias().cloned() else {
            return Ok(());
        };
        let parent = self.member(creation.creator_id())?;
        if self.shutting_down || parent.closing {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        if self
            .teams
            .values()
            .any(|team| team.lead_epoch == parent.epoch && !team.ready)
        {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        let team_id = parent.team.clone();
        let team = team_id.as_ref().map(|id| self.team(id)).transpose()?;
        if team.is_none_or(|team| team.capability == TeamCapability::Disabled) {
            let key = (parent.epoch, alias.clone());
            if self.ordinary_aliases.contains_key(&key)
                || team_id.as_ref().is_some_and(|id| {
                    self.teams
                        .get(id)
                        .is_some_and(|team| team.registrations.contains_key(&alias))
                })
            {
                return Err(AgentManagerError::AliasInUse {
                    alias: alias.as_str().into(),
                });
            }
            let token = uuid::Uuid::new_v4();
            self.ordinary_aliases.insert(
                key,
                OrdinaryAlias {
                    instance_id: creation.instance_id().clone(),
                    token,
                    epoch: None,
                    ready: false,
                },
            );
            creation.bind_alias_reservation(token);
            return Ok(());
        }
        let id = team_id.expect("enabled Team has an id");
        let team = self.team(&id)?;
        if team.lead_epoch != parent.epoch || team.config.lead() != creation.creator_id() {
            return Err(denied(
                creation.creator_id(),
                AgentOperation::CreateAgent,
                id.as_str(),
            ));
        }
        if team.registrations.contains_key(&alias) || team.reservations.contains_key(&alias) {
            return Err(AgentManagerError::AliasInUse {
                alias: alias.as_str().into(),
            });
        }
        if team.registrations.len() + team.reservations.len() >= MAX_TEAM_MEMBERS {
            return Err(AgentManagerError::TeamMemberLimit {
                limit: MAX_TEAM_MEMBERS,
            });
        }
        let token = uuid::Uuid::new_v4();
        self.teams
            .get_mut(&id)
            .expect("validated Team")
            .reservations
            .insert(
                alias,
                AliasReservation {
                    instance_id: creation.instance_id().clone(),
                    token,
                },
            );
        creation.bind_teammate(id, token);
        Ok(())
    }

    pub(crate) fn release_alias(&mut self, creation: &AgentCreation) {
        if matches!(creation.kind(), AgentKind::Subagent { .. }) {
            self.ordinary_aliases.retain(|_, entry| {
                !(entry.instance_id == *creation.instance_id()
                    && Some(entry.token) == creation.reservation()
                    && entry.epoch.is_none())
            });
            return;
        }
        let AgentKind::Teammate { team, .. } = creation.kind() else {
            return;
        };
        let Some(team) = self.teams.get_mut(team) else {
            return;
        };
        let Some(alias) = creation.alias() else {
            return;
        };
        if team.reservations.get(alias).is_some_and(|reservation| {
            reservation.instance_id == *creation.instance_id()
                && Some(reservation.token) == creation.reservation()
        }) {
            team.reservations.remove(alias);
        }
    }

    pub(crate) fn resolve_target(
        &self,
        caller: &Member,
        selector: &AgentInstanceId,
    ) -> Result<&Member, AgentManagerError> {
        if let Some(member) = self.members.get(selector) {
            return Ok(member);
        }
        let alias = AgentAlias::new(selector.as_str()).map_err(|_| AgentManagerError::Unknown {
            id: selector.to_string(),
        })?;
        self.resolve_alias(caller, &alias)
    }

    pub(crate) fn resolve_alias(
        &self,
        caller: &Member,
        alias: &AgentAlias,
    ) -> Result<&Member, AgentManagerError> {
        if let Some(team_id) = &caller.team {
            let team = self.team(team_id)?;
            if let Some(registration) = team
                .registrations
                .get(alias)
                .filter(|registration| registration.publication == AliasPublication::Ready)
            {
                return self
                    .members
                    .values()
                    .find(|member| member.epoch == registration.epoch && !member.closing)
                    .ok_or_else(|| AgentManagerError::UnknownAlias {
                        alias: alias.as_str().into(),
                    });
            }
        }
        let registration = self
            .ordinary_aliases
            .get(&(caller.epoch, alias.clone()))
            .or_else(|| {
                caller.ancestors.last().and_then(|parent_epoch| {
                    self.ordinary_aliases.get(&(*parent_epoch, alias.clone()))
                })
            })
            .filter(|registration| registration.ready)
            .ok_or_else(|| AgentManagerError::UnknownAlias {
                alias: alias.as_str().into(),
            })?;
        self.members
            .get(&registration.instance_id)
            .filter(|member| member.epoch == registration.epoch.unwrap() && !member.closing)
            .ok_or_else(|| AgentManagerError::UnknownAlias {
                alias: alias.as_str().into(),
            })
    }

    pub(crate) fn register_creation(
        &mut self,
        candidate: AgentInstanceCandidate,
        creation: &AgentCreation,
    ) -> Result<AgentInstance, AgentManagerError> {
        candidate
            .validate_creation(creation)
            .map_err(|reason| AgentManagerError::InvalidCandidate { reason })?;
        if let AgentKind::Subagent { creator } = creation.kind()
            && let Some(alias) = creation.alias()
        {
            let parent = self.member(creator)?;
            let registration = self.ordinary_aliases.get(&(parent.epoch, alias.clone()));
            if registration.is_none_or(|registration| {
                registration.instance_id != *creation.instance_id()
                    || Some(registration.token) != creation.reservation()
                    || registration.epoch.is_some()
            }) {
                return Err(AgentManagerError::AdmissionInterrupted);
            }
        }
        if let AgentKind::Teammate { team, creator } = creation.kind() {
            let parent = self.member(creator)?;
            let state = self.team(team)?;
            let reservation = creation
                .alias()
                .and_then(|alias| state.reservations.get(alias));
            if state.capability != TeamCapability::Enabled
                || state.lead_epoch != parent.epoch
                || state.config.lead() != creator
                || reservation.is_none_or(|reservation| {
                    reservation.instance_id != *creation.instance_id()
                        || Some(reservation.token) != creation.reservation()
                })
                || candidate.task_list() != Some(state.config.task_list())
            {
                return Err(AgentManagerError::AdmissionInterrupted);
            }
        }
        let instance = self.register(candidate, Some(creation.creator_id()))?;
        let member = self
            .members
            .get_mut(instance.id())
            .expect("registered child");
        member.kind = creation.kind().clone();
        member.alias = creation.alias().cloned();
        let epoch = member.epoch;
        if matches!(creation.kind(), AgentKind::Subagent { .. })
            && let Some(alias) = creation.alias()
        {
            let parent_epoch = *member.ancestors.last().expect("child has a parent");
            self.ordinary_aliases
                .get_mut(&(parent_epoch, alias.clone()))
                .expect("validated ordinary alias")
                .epoch = Some(epoch);
        }
        if let AgentKind::Teammate { team, .. } = creation.kind() {
            member.team = Some(team.clone());
            let epoch = member.epoch;
            let alias = creation.alias().expect("reserved teammate alias").clone();
            let state = self.teams.get_mut(team).expect("validated Team");
            state.reservations.remove(&alias);
            state.registrations.insert(
                alias.clone(),
                AliasRegistration {
                    epoch,
                    publication: AliasPublication::Starting,
                },
            );
            state.config.add_member(AgentTeamMember::new(
                instance.id().clone(),
                alias,
                Some(creation.definition()),
                creation.working_dir().to_owned(),
            ));
        }
        Ok(instance)
    }

    pub(super) fn is_published(&self, member: &Member) -> bool {
        let Some(id) = &member.team else {
            return true;
        };
        member
            .alias
            .as_ref()
            .and_then(|alias| self.teams.get(id)?.registrations.get(alias))
            .is_some_and(|registration| {
                registration.epoch == member.epoch
                    && registration.publication == AliasPublication::Ready
            })
    }

    pub(super) fn validate_team_start(
        &self,
        creation: &AgentCreation,
        epoch: uuid::Uuid,
    ) -> Result<(), AgentManagerError> {
        let AgentKind::Teammate { team, creator } = creation.kind() else {
            return Ok(());
        };
        let parent = self.member(creator)?;
        let member = self.member(creation.instance_id())?;
        let state = self.team(team)?;
        let registration = creation
            .alias()
            .and_then(|alias| state.registrations.get(alias));
        if self.shutting_down
            || parent.closing
            || member.closing
            || member.epoch != epoch
            || state.lead_epoch != parent.epoch
            || !state.ready
            || state.capability != TeamCapability::Enabled
            || member.team.as_ref() != Some(team)
            || member.kind != *creation.kind()
            || registration.is_none_or(|registration| registration.epoch != epoch)
        {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        Ok(())
    }

    pub(super) fn publish_creation(
        &mut self,
        creation: &AgentCreation,
        epoch: uuid::Uuid,
    ) -> Result<(), AgentManagerError> {
        self.validate_team_start(creation, epoch)?;
        if let AgentKind::Subagent { .. } = creation.kind()
            && let Some(alias) = creation.alias()
        {
            let member = self.member(creation.instance_id())?;
            let parent_epoch = *member.ancestors.last().expect("child has a parent");
            let epoch = member.epoch;
            let registration = self
                .ordinary_aliases
                .get_mut(&(parent_epoch, alias.clone()))
                .ok_or(AgentManagerError::AdmissionInterrupted)?;
            if registration.epoch != Some(epoch)
                || Some(registration.token) != creation.reservation()
            {
                return Err(AgentManagerError::AdmissionInterrupted);
            }
            registration.ready = true;
        }
        if let AgentKind::Teammate { team, .. } = creation.kind() {
            self.teams
                .get_mut(team)
                .expect("validated Team")
                .registrations
                .get_mut(creation.alias().expect("teammate alias"))
                .expect("validated registration")
                .publication = AliasPublication::Ready;
        }
        Ok(())
    }

    pub(crate) fn remove_membership(&mut self, instance: &AgentInstanceId, epoch: uuid::Uuid) {
        self.ordinary_aliases
            .retain(|(parent_epoch, _), registration| {
                *parent_epoch != epoch
                    && !(registration.instance_id == *instance && registration.epoch == Some(epoch))
            });
        for team in self.teams.values_mut() {
            if let Some(alias) = team
                .registrations
                .iter()
                .find_map(|(alias, registered)| (registered.epoch == epoch).then(|| alias.clone()))
            {
                team.registrations.remove(&alias);
                team.config.remove_member(instance);
            }
        }
    }

    pub(crate) fn register(
        &mut self,
        mut candidate: AgentInstanceCandidate,
        parent: Option<&AgentInstanceId>,
    ) -> Result<AgentInstance, AgentManagerError> {
        if self.shutting_down {
            return Err(AgentManagerError::ShuttingDown);
        }
        let id = AgentInstanceId::from_transcript(candidate.transcript());
        if self.members.contains_key(&id) {
            return Err(AgentManagerError::Duplicate { id: id.to_string() });
        }
        if self.members.len() >= MAX_AGENT_INSTANCES {
            return Err(AgentManagerError::InstanceLimit {
                limit: MAX_AGENT_INSTANCES,
            });
        }
        let ancestors = self.ancestors(parent)?;
        if let Some(parent) = parent {
            let inherited = self.member(parent)?.instance.task_list();
            if candidate
                .state
                .task_list
                .as_ref()
                .is_some_and(|binding| Some(binding) != inherited)
            {
                return Err(AgentManagerError::InvalidCandidate {
                    reason: "ordinary child task list must match its creator's binding",
                });
            }
            candidate.state.task_list = inherited.cloned();
        }
        let epoch = uuid::Uuid::new_v4();
        candidate.bind_orchestration(super::policy::InstanceAgentScope::new(
            self.owner.clone(),
            id.clone(),
            epoch,
        ));
        let background_required = candidate.state.background_required();
        let instance = AgentInstance::spawn(candidate);
        let mut member = Member::new(instance.clone(), epoch, parent, ancestors);
        member.background_required = background_required;
        self.members.insert(id, member);
        Ok(instance)
    }

    pub(crate) fn prune_stopped(&mut self) {
        let mut removed = Vec::new();
        self.members.retain(|id, member| {
            if !member.closing
                && member.team.is_none()
                && member.instance.snapshot().status == AgentInstanceStatus::Stopped
            {
                self.messages.reject(member.epoch);
                removed.push((id.clone(), member.epoch));
                return false;
            }
            true
        });
        for (id, epoch) in removed {
            self.remove_membership(&id, epoch);
        }
        let epochs = self
            .members
            .values()
            .map(|member| member.epoch)
            .collect::<HashSet<_>>();
        self.teams.retain(|_, team| {
            !team.ready
                || epochs.contains(&team.lead_epoch)
                || !team.registrations.is_empty()
                || !team.reservations.is_empty()
        });
        self.communication
            .retain(|(caller, target)| epochs.contains(caller) && epochs.contains(target));
        self.reviews
            .retain(|(caller, target)| epochs.contains(caller) && epochs.contains(target));
    }

    fn ancestors(
        &self,
        parent: Option<&AgentInstanceId>,
    ) -> Result<Vec<uuid::Uuid>, AgentManagerError> {
        let Some(parent) = parent else {
            return Ok(Vec::new());
        };
        let parent = self.member(parent)?;
        if parent.closing {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        let mut ancestors = parent.ancestors.clone();
        ancestors.push(parent.epoch);
        Ok(ancestors)
    }

    pub(crate) fn scope(
        &self,
        caller: &Member,
        target: &Member,
        operation: AgentOperation,
    ) -> AgentTargetScope {
        let pair = (caller.epoch, target.epoch);
        if operation == AgentOperation::ReadHistory && self.reviews.contains(&pair) {
            return AgentTargetScope::ReviewTarget;
        }
        if matches!(operation, AgentOperation::Observe | AgentOperation::Message)
            && self.communication.contains(&pair)
        {
            return AgentTargetScope::AuthorizedCommunication;
        }
        if caller.team.is_some() && target.team.is_some() && caller.team != target.team {
            return AgentTargetScope::Unrelated;
        }
        self.relationship_scope(caller, target)
    }

    fn relationship_scope(&self, caller: &Member, target: &Member) -> AgentTargetScope {
        if target.ancestors.contains(&caller.epoch) {
            return AgentTargetScope::Descendant;
        }
        if caller.ancestors.last() == Some(&target.epoch) {
            return AgentTargetScope::DirectRelative;
        }
        if caller.team.is_some() && caller.team == target.team {
            return AgentTargetScope::Peer;
        }
        AgentTargetScope::Unrelated
    }
}

impl AgentManager {
    pub(crate) fn from_scope(
        owner: &std::sync::Weak<Mutex<Registry>>,
    ) -> Result<Self, AgentManagerError> {
        Ok(Self {
            registry: owner.upgrade().ok_or(AgentManagerError::Unavailable)?,
        })
    }

    pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, Registry>, AgentManagerError> {
        self.registry
            .lock()
            .map_err(|_| AgentManagerError::Unavailable)
    }

    pub(crate) fn lock_live(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Registry>, AgentManagerError> {
        let mut registry = self.lock()?;
        registry.prune_stopped();
        Ok(registry)
    }
}

pub(crate) fn instance_error(
    id: &AgentInstanceId,
    source: AgentInstanceError,
) -> AgentManagerError {
    AgentManagerError::Instance {
        id: id.to_string(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::factory::Phi;
    use crate::factory::PhiRuntimeServices;
    use crate::models::{ModelCatalog, ModelCost, ModelDescriptor, ModelInput};
    use crate::provider::ProviderApi;

    fn creation(
        manager: &AgentManager,
        parent: &AgentInstanceId,
        alias: Option<&str>,
    ) -> AgentCreation {
        let registry = manager.lock_live().unwrap();
        let member = registry.member(parent).unwrap();
        let mut request =
            super::super::preparation::AgentSpawnRequest::new("code-reviewer", "Review changes.");
        request.alias = alias.map(|alias| AgentAlias::new(alias).unwrap());
        AgentCreation::new(
            parent.clone(),
            &request,
            member.instance.snapshot().working_dir,
            member.instance.task_list().cloned(),
        )
    }

    async fn admit_creation(manager: &AgentManager, creation: &AgentCreation) -> AgentInstance {
        let mut transcript = Transcript::new();
        transcript.id = creation.instance_id().as_str().into();
        let mut candidate =
            test_bound_client(Some(transcript), creation.task_list().cloned()).await;
        candidate.state.delegated_child = true;
        let child = manager
            .lock_live()
            .unwrap()
            .register_creation(candidate, creation)
            .unwrap();
        let (epoch, files) = {
            let registry = manager.lock_live().unwrap();
            let member = registry.member(child.id()).unwrap();
            (
                member.epoch,
                member
                    .team
                    .as_ref()
                    .map(|team| registry.team(team).unwrap().files.clone()),
            )
        };
        if let Some(files) = files {
            files
                .add_member(manager.clone(), creation.clone(), epoch)
                .await
                .unwrap();
        }
        manager
            .lock_live()
            .unwrap()
            .publish_creation(creation, epoch)
            .unwrap();
        child
    }

    fn team_storage() -> TeamStorageRoot {
        TeamStorageRoot::new(
            std::env::temp_dir().join(format!("phi-team-registry-{}", uuid::Uuid::new_v4())),
        )
        .unwrap()
    }

    async fn team_lead(manager: &AgentManager) -> (AgentInstance, AgentTeamId) {
        let binding =
            crate::tasks::TaskListBinding::new("/tmp/phi-team-registry", "shared").unwrap();
        let lead = manager
            .insert(test_bound_client(None, Some(binding)).await)
            .unwrap();
        let team = manager
            .initialize_team(lead.id(), team_storage())
            .await
            .unwrap();
        (lead, team)
    }

    #[tokio::test]
    async fn team_initialization_and_named_spawn_are_distinct_from_ordinary_children() {
        let manager = AgentManager::new();
        let (lead, team) = team_lead(&manager).await;
        assert_eq!(manager.list().unwrap().len(), 1);
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .team(&team)
                .unwrap()
                .config
                .members()
                .len(),
            1
        );
        assert!(
            manager
                .initialize_team(lead.id(), team_storage())
                .await
                .is_err()
        );
        let mut instances = Vec::new();
        for alias in ["security", "performance"] {
            let mut request = creation(&manager, lead.id(), Some(alias));
            manager
                .lock_live()
                .unwrap()
                .reserve_alias(&mut request)
                .unwrap();
            instances.push(admit_creation(&manager, &request).await);
        }
        let ordinary = creation(&manager, lead.id(), None);
        let child = admit_creation(&manager, &ordinary).await;
        let discovery = manager.discover().unwrap();
        for (instance, alias) in instances.iter().zip(["security", "performance"]) {
            let member = discovery
                .iter()
                .find(|member| member.id == *instance.id())
                .unwrap();
            assert_eq!(member.alias.as_ref().unwrap().as_str(), alias);
            assert_eq!(member.team.as_ref(), Some(&team));
            assert!(
                matches!(&member.kind, AgentKind::Teammate { creator, .. } if creator == lead.id())
            );
            let encoded = serde_json::to_value(member).unwrap();
            assert_eq!(encoded["kind"]["kind"], "teammate");
            assert!(encoded.get("origin").is_none());
            let decoded: super::super::AgentDiscovery = serde_json::from_value(encoded).unwrap();
            assert_eq!(decoded.kind, member.kind);
            assert_eq!(decoded.created_by, member.created_by);
            assert_eq!(instance.task_list(), lead.task_list());
            assert_eq!(
                manager
                    .for_agent(instance.id())
                    .unwrap()
                    .task_list()
                    .unwrap()
                    .validate()
                    .unwrap(),
                lead.task_list().unwrap()
            );
            assert!(instance.transcript().await.unwrap().entries.is_empty());
        }
        let member = discovery
            .iter()
            .find(|member| member.id == *child.id())
            .unwrap();
        assert_eq!(member.team, None);
        assert_eq!(child.task_list(), lead.task_list());
        assert!(matches!(member.kind, AgentKind::Subagent { .. }));
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .admissions
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        manager.shutdown().await.unwrap();
        assert!(manager.lock_live().unwrap().teams.is_empty());
    }

    #[tokio::test]
    async fn alias_reservations_are_atomic_and_stale_compensation_cannot_release_a_successor() {
        let manager = AgentManager::new();
        let (lead, team) = team_lead(&manager).await;
        let requests = [
            creation(&manager, lead.id(), Some("security")),
            creation(&manager, lead.id(), Some("security")),
        ];
        let outcomes = std::thread::scope(|scope| {
            let handles = requests.map(|mut request| {
                let manager = manager.clone();
                scope.spawn(move || {
                    manager
                        .lock_live()
                        .unwrap()
                        .reserve_alias(&mut request)
                        .map(|_| request)
                })
            });
            handles.map(|handle| handle.join().unwrap())
        });
        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        let stale = outcomes.into_iter().find_map(Result::ok).unwrap();
        manager.lock_live().unwrap().release_alias(&stale);
        let mut successor = creation(&manager, lead.id(), Some("security"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut successor)
            .unwrap();
        manager.lock_live().unwrap().release_alias(&stale);
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .team(&team)
                .unwrap()
                .reservations
                .len(),
            1
        );
        let child = admit_creation(&manager, &successor).await;
        child.close_registration();
        manager
            .lock_live()
            .unwrap()
            .members
            .get_mut(child.id())
            .unwrap()
            .closing = true;
        let mut conflicting = creation(&manager, lead.id(), Some("security"));
        assert!(matches!(
            manager.lock_live().unwrap().reserve_alias(&mut conflicting),
            Err(AgentManagerError::AliasInUse { .. })
        ));
        manager.stop(child.id()).await.unwrap();
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut conflicting)
            .unwrap();
        manager.lock_live().unwrap().release_alias(&conflicting);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn ordinary_aliases_resolve_within_creation_family_only() {
        let manager = AgentManager::new();
        let lead = manager.insert(test_client(None).await).unwrap();
        let unrelated = manager.insert(test_client(None).await).unwrap();
        let mut first = creation(&manager, lead.id(), Some("probe"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut first)
            .unwrap();
        let mut duplicate = creation(&manager, lead.id(), Some("probe"));
        assert!(matches!(
            manager.lock_live().unwrap().reserve_alias(&mut duplicate),
            Err(AgentManagerError::AliasInUse { .. })
        ));
        let selector = AgentInstanceId::new("probe");
        assert!(
            manager
                .for_agent(lead.id())
                .unwrap()
                .control_target(
                    &manager.lock_live().unwrap(),
                    &selector,
                    AgentOperation::Observe
                )
                .is_err()
        );
        let mut transcript = Transcript::new();
        transcript.id = first.instance_id().as_str().into();
        let mut candidate = test_bound_client(Some(transcript), first.task_list().cloned()).await;
        candidate.state.delegated_child = true;
        let child = manager
            .lock_live()
            .unwrap()
            .register_creation(candidate, &first)
            .unwrap();
        let epoch = manager
            .lock_live()
            .unwrap()
            .member(child.id())
            .unwrap()
            .epoch;
        assert!(
            manager
                .for_agent(lead.id())
                .unwrap()
                .control_target(
                    &manager.lock_live().unwrap(),
                    &selector,
                    AgentOperation::Observe
                )
                .is_err()
        );
        manager
            .lock_live()
            .unwrap()
            .publish_creation(&first, epoch)
            .unwrap();
        assert!(matches!(
            manager.lock_live().unwrap().reserve_alias(&mut duplicate),
            Err(AgentManagerError::AliasInUse { .. })
        ));
        let sibling = admit_creation(&manager, &creation(&manager, lead.id(), None)).await;
        let mut other_name = creation(&manager, unrelated.id(), Some("probe"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut other_name)
            .unwrap();
        let other_child = admit_creation(&manager, &other_name).await;
        let mut nested_name = creation(&manager, sibling.id(), Some("nested"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut nested_name)
            .unwrap();
        let nested = admit_creation(&manager, &nested_name).await;
        let lead_access = manager.for_agent(lead.id()).unwrap();
        let sibling_access = manager.for_agent(sibling.id()).unwrap();
        assert_eq!(
            sibling_access
                .control_target(
                    &manager.lock_live().unwrap(),
                    &AgentInstanceId::new("nested"),
                    AgentOperation::Observe
                )
                .unwrap()
                .0
                .id(),
            nested.id()
        );
        assert_eq!(
            lead_access
                .control_target(
                    &manager.lock_live().unwrap(),
                    &selector,
                    AgentOperation::Observe
                )
                .unwrap()
                .0
                .id(),
            child.id()
        );
        assert_eq!(
            lead_access
                .send_message(&selector, "review findings")
                .await
                .unwrap()
                .message()
                .recipient,
            *child.id()
        );
        assert!(
            sibling_access
                .send_message(&selector, "no sibling grant")
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .for_agent(unrelated.id())
                .unwrap()
                .control_target(
                    &manager.lock_live().unwrap(),
                    &selector,
                    AgentOperation::Observe
                )
                .unwrap()
                .0
                .id(),
            other_child.id()
        );
        assert!(
            manager
                .for_agent(unrelated.id())
                .unwrap()
                .send_message(child.id(), "unrelated")
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .discover()
                .unwrap()
                .iter()
                .find(|member| member.id == *child.id())
                .unwrap()
                .team,
            None
        );
        manager.stop(child.id()).await.unwrap();
        assert!(
            lead_access
                .control_target(
                    &manager.lock_live().unwrap(),
                    &selector,
                    AgentOperation::Observe
                )
                .is_err()
        );
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut duplicate)
            .unwrap();
        manager.lock_live().unwrap().release_alias(&duplicate);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn capability_gates_future_spawns_without_granting_membership_or_nested_teams() {
        let manager = AgentManager::new();
        let uninitialized = manager.insert(test_client(None).await).unwrap();
        let mut ordinary = creation(&manager, uninitialized.id(), Some("named"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut ordinary)
            .unwrap();
        assert!(matches!(ordinary.kind(), AgentKind::Subagent { .. }));
        let (lead, team) = team_lead(&manager).await;
        let mut pending = creation(&manager, lead.id(), Some("pending"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut pending)
            .unwrap();
        manager
            .set_team_capability(&team, TeamCapability::Disabled)
            .unwrap();
        let mut fallback = creation(&manager, lead.id(), Some("pending"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut fallback)
            .unwrap();
        assert!(matches!(fallback.kind(), AgentKind::Subagent { .. }));
        let child = admit_creation(&manager, &fallback).await;
        assert_eq!(
            manager
                .for_agent(child.id())
                .unwrap()
                .list()
                .unwrap()
                .iter()
                .find(|member| member.instance.id == *child.id())
                .unwrap()
                .team,
            None
        );
        assert!(
            manager
                .initialize_team(child.id(), team_storage())
                .await
                .is_err()
        );
        manager.lock_live().unwrap().release_alias(&pending);
        manager
            .set_team_capability(&team, TeamCapability::Enabled)
            .unwrap();
        let mut teammate = creation(&manager, lead.id(), Some("reviewer"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut teammate)
            .unwrap();
        let worker = admit_creation(&manager, &teammate).await;
        let mut forbidden = creation(&manager, worker.id(), Some("nested"));
        assert!(matches!(
            manager.lock_live().unwrap().reserve_alias(&mut forbidden),
            Err(AgentManagerError::Denied { .. })
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn alias_messages_capture_the_registration_and_membership_grants_no_peer_control() {
        let manager = AgentManager::new();
        let (lead, _) = team_lead(&manager).await;
        let mut workers = Vec::new();
        for alias in ["security", "performance"] {
            let mut request = creation(&manager, lead.id(), Some(alias));
            manager
                .lock_live()
                .unwrap()
                .reserve_alias(&mut request)
                .unwrap();
            workers.push(admit_creation(&manager, &request).await);
        }
        let access = manager.for_agent(workers[0].id()).unwrap();
        assert!(
            access
                .view(&AgentInstanceId::new("performance"))
                .await
                .is_err()
        );
        assert!(
            access
                .stop(&AgentInstanceId::new("performance"))
                .await
                .is_err()
        );
        let receipt = access
            .send_message(&AgentInstanceId::new("performance"), "review findings")
            .await
            .unwrap();
        assert_eq!(receipt.message().recipient, *workers[1].id());
        manager.stop(workers[1].id()).await.unwrap();
        let mut replacement = creation(&manager, lead.id(), Some("performance"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut replacement)
            .unwrap();
        let replacement = admit_creation(&manager, &replacement).await;
        assert_ne!(replacement.id(), workers[1].id());
        assert!(replacement.transcript().await.unwrap().entries.is_empty());
        assert_eq!(receipt.message().recipient, *workers[1].id());
        let history = manager.stop(workers[0].id()).await.unwrap();
        let reused = manager.insert(test_client(Some(history)).await).unwrap();
        assert!(
            access
                .send_message(&AgentInstanceId::new("performance"), "stale sender")
                .await
                .is_err()
        );
        let restored = manager.for_agent(reused.id()).unwrap();
        assert!(
            restored
                .send_message(
                    &AgentInstanceId::new("performance"),
                    "saved identity is not membership"
                )
                .await
                .is_err()
        );
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .member(reused.id())
                .unwrap()
                .alias,
            None
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn failed_member_finalization_keeps_its_alias_reserved_until_retry() {
        let manager = AgentManager::new();
        let (lead, _) = team_lead(&manager).await;
        let mut request = creation(&manager, lead.id(), Some("security"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut request)
            .unwrap();
        let child = admit_creation(&manager, &request).await;
        manager
            .lock_live()
            .unwrap()
            .members
            .get_mut(child.id())
            .unwrap()
            .instance = child.without_owner();
        assert!(manager.stop(child.id()).await.is_err());
        let mut replacement = creation(&manager, lead.id(), Some("security"));
        assert!(matches!(
            manager.lock_live().unwrap().reserve_alias(&mut replacement),
            Err(AgentManagerError::AliasInUse { .. })
        ));
        child.stop_local().await.unwrap();
        child.stop().await.unwrap();
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut replacement)
            .unwrap();
        manager.lock_live().unwrap().release_alias(&replacement);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn member_writes_keep_the_lane_after_cancellation_and_close_before_effects() {
        use std::future::Future;
        let directory =
            std::env::temp_dir().join(format!("phi-member-write-race-{}", uuid::Uuid::new_v4()));
        let manager = AgentManager::new();
        let binding =
            crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared").unwrap();
        let lead = manager
            .insert(test_bound_client(None, Some(binding)).await)
            .unwrap();
        let id = manager
            .initialize_team(
                lead.id(),
                TeamStorageRoot::new(directory.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let mut request = creation(&manager, lead.id(), Some("security"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut request)
            .unwrap();
        let mut transcript = Transcript::new();
        transcript.id = request.instance_id().as_str().into();
        let mut candidate = test_bound_client(Some(transcript), request.task_list().cloned()).await;
        candidate.state.delegated_child = true;
        let child = manager
            .lock_live()
            .unwrap()
            .register_creation(candidate, &request)
            .unwrap();
        let (epoch, files) = {
            let registry = manager.lock().unwrap();
            (
                registry.member(child.id()).unwrap().epoch,
                registry.team(&id).unwrap().files.clone(),
            )
        };
        let store = super::super::team::TeamStore::lock(&directory.join("teams")).unwrap();
        {
            let write = files.add_member(manager.clone(), request.clone(), epoch);
            tokio::pin!(write);
            assert!(
                std::future::poll_fn(|context| std::task::Poll::Ready(
                    write.as_mut().poll(context)
                ))
                .await
                .is_pending()
            );
        }
        let lane = files.lock();
        tokio::pin!(lane);
        assert!(
            std::future::poll_fn(|context| std::task::Poll::Ready(lane.as_mut().poll(context)))
                .await
                .is_pending()
        );
        let captured = manager.clone();
        let target = child.id().clone();
        let stopping = tokio::spawn(async move { captured.stop(&target).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while child.snapshot().status != AgentInstanceStatus::Stopped {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        stopping.abort();
        assert!(stopping.await.unwrap_err().is_cancelled());
        drop(store);
        drop(lane.await);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while manager.lock().unwrap().members.contains_key(child.id()) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let namespace = directory
            .join("teams")
            .join(super::super::team::team_namespace(lead.id().as_str()).unwrap());
        assert!(!namespace.join("inboxes/security.json").exists());
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(namespace.join("config.json")).unwrap()).unwrap();
        assert_eq!(record["members"].as_array().unwrap().len(), 1);
        let mut replacement = creation(&manager, lead.id(), Some("security"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut replacement)
            .unwrap();
        manager.lock_live().unwrap().release_alias(&replacement);
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn member_cleanup_file_failure_keeps_the_alias_until_retry() {
        let directory =
            std::env::temp_dir().join(format!("phi-member-cleanup-{}", uuid::Uuid::new_v4()));
        let manager = AgentManager::new();
        let lead = manager
            .insert(
                test_bound_client(
                    None,
                    Some(
                        crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared")
                            .unwrap(),
                    ),
                )
                .await,
            )
            .unwrap();
        manager
            .initialize_team(
                lead.id(),
                TeamStorageRoot::new(directory.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let mut request = creation(&manager, lead.id(), Some("security"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut request)
            .unwrap();
        let child = admit_creation(&manager, &request).await;
        let namespace = directory
            .join("teams")
            .join(super::super::team::team_namespace(lead.id().as_str()).unwrap());
        let inbox = namespace.join("inboxes/security.json");
        let original = std::fs::read(&inbox).unwrap();
        let lead_inbox = namespace.join("inboxes/lead.json");
        let lead_original = std::fs::read(&lead_inbox).unwrap();
        std::fs::write(&lead_inbox, b"broken JSON").unwrap();
        assert!(matches!(
            manager.stop(child.id()).await,
            Err(AgentManagerError::TeamStorage { .. })
        ));
        assert!(
            manager
                .send_message(child.id(), lead.id(), "closing sender")
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(&inbox).unwrap(), original);
        std::fs::write(&lead_inbox, lead_original).unwrap();
        std::fs::write(&inbox, b"broken JSON").unwrap();
        assert!(matches!(
            manager.stop(child.id()).await,
            Err(AgentManagerError::TeamStorage { .. })
        ));
        let notices: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&lead_inbox).unwrap()).unwrap();
        assert_eq!(notices["messages"].as_array().unwrap().len(), 1);
        assert_eq!(notices["messages"][0]["sender"], child.id().as_str());
        assert_eq!(child.snapshot().status, AgentInstanceStatus::Stopped);
        let mut replacement = creation(&manager, lead.id(), Some("security"));
        assert!(matches!(
            manager.lock_live().unwrap().reserve_alias(&mut replacement),
            Err(AgentManagerError::AliasInUse { .. })
        ));
        assert!(manager.lock().unwrap().member(child.id()).unwrap().closing);
        assert!(namespace.join("inboxes/lead.json").is_file());
        std::fs::write(&inbox, original).unwrap();
        let (first, second) = tokio::join!(manager.stop(child.id()), manager.stop(child.id()));
        first.unwrap();
        second.unwrap();
        assert!(!inbox.exists());
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&lead_inbox).unwrap())
                .unwrap(),
            notices
        );
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(namespace.join("config.json")).unwrap()).unwrap();
        assert_eq!(record["members"].as_array().unwrap().len(), 1);
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut replacement)
            .unwrap();
        manager.lock_live().unwrap().release_alias(&replacement);
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn captured_exit_notice_never_activates_a_reused_lead_registration() {
        let directory = std::path::Path::new("/tmp")
            .join(format!("phi-exit-lead-reuse-{}", uuid::Uuid::new_v4()));
        let manager = AgentManager::new();
        let binding =
            crate::tasks::TaskListBinding::new(directory.join("tasks"), "shared").unwrap();
        let lead = manager
            .insert(test_bound_client(None, Some(binding.clone())).await)
            .unwrap();
        let storage = TeamStorageRoot::new(directory.join("teams")).unwrap();
        manager
            .initialize_team(lead.id(), storage.clone())
            .await
            .unwrap();
        let mut request = creation(&manager, lead.id(), Some("security"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut request)
            .unwrap();
        let child = admit_creation(&manager, &request).await;
        let target = {
            let mut registry = manager.lock().unwrap();
            let epoch = registry.member(child.id()).unwrap().epoch;
            registry.close_members(|member| member.epoch == epoch);
            super::super::message::MessageTarget::teammate_exit(&registry, child.id(), epoch)
                .unwrap()
                .unwrap()
        };
        let notice = target.envelope("captured old exit".into());
        lead.close_registration();
        manager
            .publish_exit_notice(&target, notice.clone())
            .await
            .unwrap();
        let namespace = directory
            .join("teams")
            .join(super::super::team::team_namespace(lead.id().as_str()).unwrap());
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(namespace.join("inboxes/lead.json")).unwrap())
                .unwrap();
        assert!(record["messages"].as_array().unwrap().is_empty());
        let history = manager.stop(lead.id()).await.unwrap();
        let replacement = manager
            .insert(test_bound_client(Some(history), Some(binding)).await)
            .unwrap();
        assert_eq!(replacement.id(), lead.id());
        manager
            .initialize_team(replacement.id(), storage)
            .await
            .unwrap();
        manager.publish_exit_notice(&target, notice).await.unwrap();
        let namespace = directory
            .join("teams")
            .join(super::super::team::team_namespace(lead.id().as_str()).unwrap());
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(namespace.join("inboxes/lead.json")).unwrap())
                .unwrap();
        assert!(record["messages"].as_array().unwrap().is_empty());
        assert!(replacement.transcript().await.unwrap().entries.is_empty());
        manager.shutdown().await.unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn stopped_members_release_aliases_and_team_config() {
        let manager = AgentManager::new();
        let (lead, team_id) = team_lead(&manager).await;
        for alias in ["security", "performance"] {
            let mut request = creation(&manager, lead.id(), Some(alias));
            manager
                .lock_live()
                .unwrap()
                .reserve_alias(&mut request)
                .unwrap();
            let child = admit_creation(&manager, &request).await;
            if alias == "security" {
                manager.stop(child.id()).await.unwrap();
            } else {
                child.stop_local().await.unwrap();
                assert!(
                    manager
                        .lock_live()
                        .unwrap()
                        .members
                        .contains_key(child.id())
                );
                manager.stop(child.id()).await.unwrap();
            }
            let mut replacement = creation(&manager, lead.id(), Some(alias));
            let mut registry = manager.lock_live().unwrap();
            let team = registry.team(&team_id).unwrap();
            assert_eq!(team.config.members().len(), 1);
            assert_eq!(team.config.members()[0].instance_id(), lead.id());
            assert_eq!(team.registrations.len(), 1);
            assert!(!registry.members.contains_key(child.id()));
            registry.reserve_alias(&mut replacement).unwrap();
            registry.release_alias(&replacement);
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn pending_and_registered_names_share_one_bounded_team_capacity() {
        let manager = AgentManager::new();
        let (lead, team) = team_lead(&manager).await;
        let mut pending = Vec::new();
        for index in 1..MAX_TEAM_MEMBERS {
            let mut request = creation(&manager, lead.id(), Some(&format!("worker-{index}")));
            manager
                .lock_live()
                .unwrap()
                .reserve_alias(&mut request)
                .unwrap();
            pending.push(request);
        }
        let mut extra = creation(&manager, lead.id(), Some("extra"));
        assert!(matches!(
            manager.lock_live().unwrap().reserve_alias(&mut extra),
            Err(AgentManagerError::TeamMemberLimit {
                limit: MAX_TEAM_MEMBERS
            })
        ));
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .team(&team)
                .unwrap()
                .reservations
                .len(),
            MAX_TEAM_MEMBERS - 1
        );
        manager
            .lock_live()
            .unwrap()
            .release_alias(&pending.pop().unwrap());
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut extra)
            .unwrap();
        for request in pending.iter().chain(std::iter::once(&extra)) {
            manager.lock_live().unwrap().release_alias(request);
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn namespace_collisions_and_missing_bindings_fail_before_membership_changes() {
        let manager = AgentManager::new();
        let unbound = manager.insert(test_client(None).await).unwrap();
        assert!(
            manager
                .initialize_team(unbound.id(), team_storage())
                .await
                .is_err()
        );
        let binding =
            crate::tasks::TaskListBinding::new("/tmp/phi-team-registry", "shared").unwrap();
        let mut roots = Vec::new();
        for id in ["12345678-first", "12345678-second"] {
            let mut transcript = Transcript::new();
            transcript.id = id.into();
            roots.push(
                manager
                    .insert(test_bound_client(Some(transcript), Some(binding.clone())).await)
                    .unwrap(),
            );
        }
        let storage = team_storage();
        manager
            .initialize_team(roots[0].id(), storage.clone())
            .await
            .unwrap();
        assert!(matches!(
            manager
                .initialize_team(roots[1].id(), storage.clone())
                .await,
            Err(AgentManagerError::TeamNamespaceConflict { .. })
        ));
        assert_eq!(
            manager
                .lock_live()
                .unwrap()
                .member(roots[1].id())
                .unwrap()
                .team,
            None
        );
        manager.stop(roots[0].id()).await.unwrap();
        assert!(matches!(
            manager.initialize_team(roots[1].id(), storage).await,
            Err(AgentManagerError::TeamNamespaceConflict { .. })
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn team_files_publish_after_io_and_cleanup_retries_without_touching_tasks() {
        let directory =
            std::env::temp_dir().join(format!("phi-team-lifecycle-{}", uuid::Uuid::new_v4()));
        let storage = TeamStorageRoot::new(directory.join("teams")).unwrap();
        let tasks = directory.join("tasks");
        std::fs::create_dir_all(&tasks).unwrap();
        std::fs::write(tasks.join("unrelated"), "keep").unwrap();
        let manager = AgentManager::new();
        let lead = manager
            .insert(
                test_bound_client(
                    None,
                    Some(crate::tasks::TaskListBinding::new(&tasks, "selected").unwrap()),
                )
                .await,
            )
            .unwrap();
        std::fs::write(directory.join("teams"), "occupied").unwrap();
        assert!(matches!(
            manager.initialize_team(lead.id(), storage.clone()).await,
            Err(AgentManagerError::TeamStorage { .. })
        ));
        assert!(manager.lock().unwrap().teams.is_empty());
        assert!(
            manager
                .lock()
                .unwrap()
                .member(lead.id())
                .unwrap()
                .team
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(directory.join("teams")).unwrap(),
            "occupied"
        );
        std::fs::remove_file(directory.join("teams")).unwrap();
        let team = manager
            .initialize_team(lead.id(), storage.clone())
            .await
            .unwrap();
        let namespace = directory
            .join("teams")
            .join(super::super::team::team_namespace(lead.id().as_str()).unwrap());
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(namespace.join("config.json")).unwrap()).unwrap();
        assert_eq!(config["lead_id"], lead.id().as_str());
        assert_eq!(config["task_list_id"], "selected");
        assert_eq!(config["members"].as_array().unwrap().len(), 1);
        assert!(namespace.join("inboxes/lead.json").is_file());
        assert_eq!(manager.list().unwrap().len(), 1);

        let other = AgentManager::new();
        let mut transcript = Transcript::new();
        transcript.id = format!("{}-collision", &lead.id().as_str()[..8]).into();
        let rival = other
            .insert(test_bound_client(Some(transcript), lead.task_list().cloned()).await)
            .unwrap();
        assert!(matches!(
            other.initialize_team(rival.id(), storage).await,
            Err(AgentManagerError::TeamNamespaceConflict { .. })
        ));
        assert!(other.lock().unwrap().teams.is_empty());
        other.shutdown().await.unwrap();

        let mut request = creation(&manager, lead.id(), Some("reviewer"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut request)
            .unwrap();
        let child = admit_creation(&manager, &request).await;
        std::fs::write(namespace.join("foreign"), "keep").unwrap();
        assert!(matches!(
            manager.stop(lead.id()).await,
            Err(AgentManagerError::TeamStorage { .. })
        ));
        {
            let registry = manager.lock().unwrap();
            assert!(registry.member(lead.id()).unwrap().closing);
            assert!(registry.team(&team).unwrap().ready);
            assert!(!registry.members.contains_key(child.id()));
        }
        assert_eq!(child.snapshot().status, AgentInstanceStatus::Stopped);
        assert!(namespace.join("config.json").exists());
        std::fs::remove_file(namespace.join("foreign")).unwrap();
        let final_history = manager.stop(lead.id()).await.unwrap();
        assert_eq!(final_history.id, lead.id().as_str());
        assert!(!namespace.exists());
        assert!(directory.join("teams/.lock").is_file());
        assert_eq!(
            std::fs::read_to_string(tasks.join("unrelated")).unwrap(),
            "keep"
        );
        assert!(manager.lock().unwrap().teams.is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn team_initialization_reservation_survives_waiter_loss_and_stop_races() {
        for stop_during_io in [false, true] {
            let directory = std::env::temp_dir()
                .join(format!("phi-team-startup-race-{}", uuid::Uuid::new_v4()));
            let store_root = directory.join("teams");
            let store = super::super::team::TeamStore::lock(&store_root).unwrap();
            let manager = AgentManager::new();
            let binding =
                crate::tasks::TaskListBinding::new(directory.join("tasks"), "selected").unwrap();
            let lead = manager
                .insert(test_bound_client(None, Some(binding)).await)
                .unwrap();
            let captured = manager.clone();
            let id = lead.id().clone();
            let root = TeamStorageRoot::new(store_root.clone()).unwrap();
            let initializer =
                tokio::spawn(async move { captured.initialize_team(&id, root).await });
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if !manager.lock().unwrap().teams.is_empty() {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            {
                let registry = manager.lock().unwrap();
                assert!(registry.member(lead.id()).unwrap().team.is_none());
                assert!(!registry.teams.values().next().unwrap().ready);
            }
            let mut request = creation(&manager, lead.id(), Some("reviewer"));
            assert!(matches!(
                manager.lock_live().unwrap().reserve_alias(&mut request),
                Err(AgentManagerError::AdmissionInterrupted)
            ));
            let stopper = if stop_during_io {
                let captured = manager.clone();
                let id = lead.id().clone();
                let stopper = tokio::spawn(async move { captured.stop(&id).await });
                tokio::time::timeout(std::time::Duration::from_secs(1), async {
                    loop {
                        if manager.lock().unwrap().member(lead.id()).unwrap().closing {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(!stopper.is_finished());
                Some(stopper)
            } else {
                initializer.abort();
                None
            };
            if let Some(stopper) = &stopper {
                stopper.abort();
            }
            drop(store);
            if let Some(stopper) = stopper {
                assert!(stopper.await.unwrap_err().is_cancelled());
                assert!(matches!(
                    initializer.await.unwrap(),
                    Err(AgentManagerError::AdmissionInterrupted)
                ));
            } else {
                assert!(initializer.await.unwrap_err().is_cancelled());
            }
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                loop {
                    let registry = manager.lock().unwrap();
                    let finalized = !stop_during_io || !registry.members.contains_key(lead.id());
                    if registry.teams.is_empty() && finalized {
                        break;
                    }
                    drop(registry);
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            manager.shutdown().await.unwrap();
            assert!(manager.lock().unwrap().teams.is_empty());
            let namespace =
                store_root.join(super::super::team::team_namespace(lead.id().as_str()).unwrap());
            assert!(!namespace.exists());
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    async fn test_client(transcript: Option<Transcript>) -> AgentInstanceCandidate {
        test_bound_client(transcript, None).await
    }

    async fn test_bound_client(
        transcript: Option<Transcript>,
        binding: Option<crate::tasks::TaskListBinding>,
    ) -> AgentInstanceCandidate {
        test_policy_client(transcript, binding, TeamCapability::Disabled).await
    }

    async fn test_policy_client(
        transcript: Option<Transcript>,
        binding: Option<crate::tasks::TaskListBinding>,
        capability: TeamCapability,
    ) -> AgentInstanceCandidate {
        let descriptor = ModelDescriptor {
            provider: "openai-codex".to_string(),
            id: "gpt-5.6-sol".to_string(),
            name: "GPT-5.6-Sol".to_string(),
            api: ProviderApi::OpenAiCodexResponses,
            reasoning: true,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            context_window: 272_000,
            max_output_tokens: 128_000,
            cost: ModelCost::default(),
        };
        let catalog = Arc::new(ModelCatalog::for_test_model(descriptor).unwrap());
        let phi = Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .no_tools(true)
            .build()
            .unwrap()
            .with_runtime_services(PhiRuntimeServices::new().model_catalog(catalog));
        let mut builder = phi.agent_instance_builder().team_capability(capability);
        if let Some(transcript) = transcript {
            builder = builder.transcript(transcript);
        }
        if let Some(binding) = binding {
            builder = builder.task_list(binding);
        }
        builder.build().await.unwrap()
    }

    #[tokio::test]
    async fn accepted_runtime_capability_only_changes_initialized_lead_spawn_routing() {
        let manager = AgentManager::new();
        let (lead, team) = team_lead(&manager).await;
        let binding = lead.task_list().cloned();
        let mut pending = creation(&manager, lead.id(), Some("reviewer"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut pending)
            .unwrap();
        let child = admit_creation(&manager, &pending).await;
        let original = lead.transcript().await.unwrap();
        let invalid =
            test_policy_client(Some(original.clone()), None, TeamCapability::Disabled).await;
        assert!(matches!(
            lead.replace_runtime(invalid.into_runtime_candidate()).await,
            Err(AgentInstanceError::ReplacementTaskList)
        ));
        assert_eq!(
            manager.lock().unwrap().team(&team).unwrap().capability,
            TeamCapability::Enabled
        );
        for capability in [TeamCapability::Disabled, TeamCapability::Enabled] {
            let replacement =
                test_policy_client(Some(original.clone()), binding.clone(), capability).await;
            lead.replace_runtime(replacement.into_runtime_candidate())
                .await
                .unwrap();
            let registry = manager.lock().unwrap();
            let state = registry.team(&team).unwrap();
            assert_eq!(state.capability, capability);
            assert_eq!(state.config.members().len(), 2);
            assert_eq!(state.config.task_list(), binding.as_ref().unwrap());
            assert!(!registry.member(child.id()).unwrap().closing);
            drop(registry);
            let mut request = creation(&manager, lead.id(), Some("next"));
            manager
                .lock_live()
                .unwrap()
                .reserve_alias(&mut request)
                .unwrap();
            assert_eq!(
                matches!(request.kind(), AgentKind::Teammate { .. }),
                capability == TeamCapability::Enabled
            );
            manager.lock().unwrap().release_alias(&request);
        }
        let ordinary = manager
            .for_agent(lead.id())
            .unwrap()
            .register_child(test_client(None).await)
            .unwrap();
        let ordinary = manager.get(&ordinary.instance.id).unwrap();
        let candidate = test_policy_client(
            Some(ordinary.transcript().await.unwrap()),
            binding.clone(),
            TeamCapability::Disabled,
        )
        .await;
        ordinary
            .replace_runtime(candidate.into_runtime_candidate())
            .await
            .unwrap();
        assert_eq!(
            manager.lock().unwrap().team(&team).unwrap().capability,
            TeamCapability::Enabled
        );
        let uninitialized = manager
            .insert(test_bound_client(None, binding.clone()).await)
            .unwrap();
        let candidate = test_policy_client(
            Some(uninitialized.transcript().await.unwrap()),
            binding,
            TeamCapability::Enabled,
        )
        .await;
        uninitialized
            .replace_runtime(candidate.into_runtime_candidate())
            .await
            .unwrap();
        let mut request = creation(&manager, uninitialized.id(), Some("ordinary"));
        manager
            .lock_live()
            .unwrap()
            .reserve_alias(&mut request)
            .unwrap();
        assert!(matches!(request.kind(), AgentKind::Subagent { .. }));
        assert_eq!(manager.lock().unwrap().teams.len(), 1);
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_list_inheritance_is_independent_of_work_and_authority() {
        let manager = AgentManager::new();
        let binding = crate::tasks::TaskListBinding::new("/tmp/phi-task-scopes", "shared").unwrap();
        let parent = manager
            .insert(test_bound_client(None, Some(binding.clone())).await)
            .unwrap();
        let unrelated = manager
            .insert(test_bound_client(None, Some(binding.clone())).await)
            .unwrap();
        let access = manager.for_agent(parent.id()).unwrap();
        let child = access.register_child(test_client(None).await).unwrap();
        let instance = manager.get(&child.instance.id).unwrap();
        assert_eq!(instance.task_list(), Some(&binding));
        for caller in [parent.id(), instance.id(), unrelated.id()] {
            let tasks = manager.for_agent(caller).unwrap().task_list().unwrap();
            assert_eq!(tasks.validate().unwrap(), &binding);
        }
        assert_eq!(manager.discover().unwrap().len(), 3);
        assert_eq!(child.created_by.as_ref(), Some(parent.id()));
        assert_eq!(instance.snapshot().status, AgentInstanceStatus::Idle);
        assert!(instance.transcript().await.unwrap().entries.is_empty());
        assert!(matches!(
            access.stop(unrelated.id()).await,
            Err(AgentManagerError::Denied { .. })
        ));
        assert!(matches!(
            access.view(unrelated.id()).await,
            Err(AgentManagerError::Denied { .. })
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_access_requires_binding() {
        use crate::harness::task::TaskAccessError;

        let manager = AgentManager::new();
        let unbound = manager.insert(test_client(None).await).unwrap();
        let bound = manager
            .insert(
                test_bound_client(
                    None,
                    Some(
                        crate::tasks::TaskListBinding::new("/tmp/phi-task-access", "shared")
                            .unwrap(),
                    ),
                )
                .await,
            )
            .unwrap();
        manager
            .grant_communication(unbound.id(), bound.id())
            .unwrap();
        manager.grant_review(unbound.id(), bound.id()).unwrap();
        let access = manager.for_agent(unbound.id()).unwrap();
        assert!(access.view(bound.id()).await.is_ok());
        assert!(matches!(access.task_list(), Err(TaskAccessError::NotBound)));
        assert!(unbound.transcript().await.unwrap().entries.is_empty());
        assert!(bound.transcript().await.unwrap().entries.is_empty());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn task_access_rejects_id_reuse() {
        use crate::harness::task::TaskAccessError;

        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-task-access-{}", uuid::Uuid::new_v4()));
        let binding = crate::tasks::TaskListBinding::new(&root, "shared").unwrap();
        for storage in [&root, &root.join("other")] {
            for list in ["shared", "other"] {
                let manager = AgentManager::new();
                let transcript = Transcript::new();
                let original = manager
                    .insert(
                        test_bound_client(Some(transcript.clone()), Some(binding.clone())).await,
                    )
                    .unwrap();
                let caller = manager.for_agent(original.id()).unwrap();
                let tasks = caller.task_list().unwrap();
                let cloned_tasks = tasks.clone();
                assert_eq!(tasks.validate().unwrap(), &binding);
                original.close_registration();
                assert!(matches!(
                    caller.task_list(),
                    Err(TaskAccessError::InactiveCaller)
                ));
                for access in [&tasks, &cloned_tasks] {
                    assert!(matches!(
                        access.validate(),
                        Err(TaskAccessError::InactiveCaller)
                    ));
                }
                let closing = manager.lock().unwrap().close_members(|_| true);
                assert!(matches!(
                    caller.task_list(),
                    Err(TaskAccessError::InactiveCaller)
                ));
                for access in [&tasks, &cloned_tasks] {
                    assert!(matches!(
                        access.validate(),
                        Err(TaskAccessError::InactiveCaller)
                    ));
                }
                manager.stop_members(closing).await.unwrap();
                assert!(tasks.validate().is_err());

                let selected = crate::tasks::TaskListBinding::new(storage, list).unwrap();
                let replacement = manager
                    .insert(test_bound_client(Some(transcript), Some(selected.clone())).await)
                    .unwrap();
                assert_eq!(replacement.id(), original.id());
                assert!(matches!(
                    caller.task_list(),
                    Err(TaskAccessError::InactiveCaller)
                ));
                for access in [&tasks, &cloned_tasks] {
                    assert!(matches!(
                        access.validate(),
                        Err(TaskAccessError::InactiveCaller)
                    ));
                }
                assert_eq!(
                    manager
                        .for_agent(replacement.id())
                        .unwrap()
                        .task_list()
                        .unwrap()
                        .validate()
                        .unwrap(),
                    &selected
                );
                assert_eq!(manager.discover().unwrap().len(), 1);
                assert!(replacement.transcript().await.unwrap().entries.is_empty());
                manager.shutdown().await.unwrap();
            }
        }
        assert!(!root.exists());
    }

    #[tokio::test]
    async fn conflicting_child_task_scope_is_rejected_before_actor_registration() {
        let manager = AgentManager::new();
        let binding = crate::tasks::TaskListBinding::new("/tmp/phi-task-scopes", "shared").unwrap();
        let parent = manager
            .insert(test_bound_client(None, Some(binding)).await)
            .unwrap();
        let access = manager.for_agent(parent.id()).unwrap();
        for (root, list) in [
            ("/tmp/phi-task-scopes", "other"),
            ("/tmp/phi-other-scopes", "shared"),
        ] {
            let candidate = test_bound_client(
                None,
                Some(crate::tasks::TaskListBinding::new(root, list).unwrap()),
            )
            .await;
            let child_id = AgentInstanceId::new(candidate.id());
            assert!(matches!(
                access.register_child(candidate),
                Err(AgentManagerError::InvalidCandidate { .. })
            ));
            assert!(matches!(
                manager.get(&child_id),
                Err(AgentManagerError::Unknown { .. })
            ));
        }
        let unbound = manager.insert(test_client(None).await).unwrap();
        let candidate = test_bound_client(
            None,
            Some(crate::tasks::TaskListBinding::new("/tmp/phi-task-scopes", "shared").unwrap()),
        )
        .await;
        assert!(matches!(
            manager
                .for_agent(unbound.id())
                .unwrap()
                .register_child(candidate),
            Err(AgentManagerError::InvalidCandidate { .. })
        ));
        assert!(parent.transcript().await.unwrap().entries.is_empty());
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn runtime_replacement_preserves_task_list_binding() {
        let manager = AgentManager::new();
        let binding = crate::tasks::TaskListBinding::new("/tmp/phi-task-scopes", "shared").unwrap();
        let instance = manager
            .insert(test_bound_client(None, Some(binding.clone())).await)
            .unwrap();
        let transcript = instance.transcript().await.unwrap();
        let replacement = test_bound_client(Some(transcript.clone()), Some(binding.clone())).await;
        instance
            .replace_runtime(replacement.into_runtime_candidate())
            .await
            .unwrap();
        let before = instance.snapshot().revision;
        for selection in [
            None,
            Some(crate::tasks::TaskListBinding::new("/tmp/phi-task-scopes", "other").unwrap()),
            Some(crate::tasks::TaskListBinding::new("/tmp/phi-other-scopes", "shared").unwrap()),
        ] {
            let replacement = test_bound_client(Some(transcript.clone()), selection).await;
            assert!(matches!(
                instance
                    .replace_runtime(replacement.into_runtime_candidate())
                    .await,
                Err(AgentInstanceError::ReplacementTaskList)
            ));
            assert_eq!(instance.task_list(), Some(&binding));
            assert_eq!(instance.snapshot().revision, before);
            assert!(instance.transcript().await.unwrap().entries.is_empty());
        }
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn master_access_is_relationship_scoped_and_registration_bound() {
        let manager = AgentManager::new();
        let ordinary = manager.insert(test_client(None).await).unwrap();
        let ordinary_access = manager.for_agent(ordinary.id()).unwrap();
        assert!(ordinary_access.list().is_ok());
        let master = manager.insert(test_client(None).await).unwrap();
        let access = manager.for_agent(master.id()).unwrap();
        for operation in [AgentOperation::Stop, AgentOperation::Interrupt] {
            let registry = manager.lock_live().unwrap();
            assert!(matches!(
                access.control_target(&registry, master.id(), operation),
                Err(AgentManagerError::Denied { .. })
            ));
        }
        let child = access.register_child(test_client(None).await).unwrap();
        assert!(matches!(
            manager
                .initialize_team(&child.instance.id, team_storage())
                .await,
            Err(AgentManagerError::Denied { .. })
        ));
        access.interrupt(&child.instance.id).await.unwrap();
        access.view(&child.instance.id).await.unwrap();
        assert!(matches!(
            access.stop(ordinary.id()).await,
            Err(AgentManagerError::Denied { .. })
        ));
        assert!(matches!(
            access.view(ordinary.id()).await,
            Err(AgentManagerError::Denied { .. })
        ));
        access.stop(&child.instance.id).await.unwrap();
        let history = manager.stop(master.id()).await.unwrap();
        let reused = manager.insert(test_client(Some(history)).await).unwrap();
        assert!(matches!(
            access.list(),
            Err(AgentManagerError::Denied { .. })
        ));
        assert!(matches!(
            manager
                .for_agent(reused.id())
                .unwrap()
                .stop(ordinary.id())
                .await,
            Err(AgentManagerError::Denied { .. })
        ));
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn review_and_communication_grants_remain_separate() {
        let manager = AgentManager::new();
        let reviewer = manager.insert(test_client(None).await).unwrap();
        let target = manager.insert(test_client(None).await).unwrap();
        let access = manager.for_agent(reviewer.id()).unwrap();
        assert_eq!(access.list().unwrap().len(), 1);
        assert!(access.transcript(target.id()).await.is_err());
        assert!(matches!(
            access.send_message(target.id(), "unrelated").await,
            Err(AgentManagerError::Denied {
                operation: AgentOperation::Message,
                ..
            })
        ));
        manager
            .grant_communication(reviewer.id(), target.id())
            .unwrap();
        assert_eq!(access.list().unwrap().len(), 2);
        assert!(access.transcript(target.id()).await.is_err());
        assert!(
            access
                .send_message(target.id(), "authorized communication")
                .await
                .is_ok()
        );
        assert!(
            manager
                .for_agent(target.id())
                .unwrap()
                .send_message(reviewer.id(), "no reverse grant")
                .await
                .is_err()
        );
        manager.grant_review(reviewer.id(), target.id()).unwrap();
        let view = access.view(target.id()).await.unwrap();
        assert_eq!(view.transcript.id.as_str(), view.snapshot.id.as_str());
        let transcript = manager.stop(target.id()).await.unwrap();
        let replacement = manager.insert(test_client(Some(transcript)).await).unwrap();
        assert!(matches!(
            access.view(replacement.id()).await,
            Err(AgentManagerError::Denied {
                operation: AgentOperation::ReadHistory,
                ..
            })
        ));
        assert_eq!(access.list().unwrap().len(), 1);
        assert!(
            access
                .send_message(replacement.id(), "grant expired")
                .await
                .is_err()
        );
        manager.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn subtree_stop_is_shared_by_manager_access_and_instance_handles() {
        let manager = AgentManager::new();
        let root = manager.insert(test_client(None).await).unwrap();
        let access = manager.for_agent(root.id()).unwrap();
        let child = access.register_child(test_client(None).await).unwrap();
        let grandchild = manager
            .for_agent(&child.instance.id)
            .unwrap()
            .register_child(test_client(None).await)
            .unwrap();
        let child_handle = manager.get(&child.instance.id).unwrap();
        let grandchild_handle = manager.get(&grandchild.instance.id).unwrap();
        let peer = manager.insert(test_client(None).await).unwrap();
        let mut finalization = grandchild_handle.subscribe_finalization().unwrap();
        assert!(matches!(
            manager.insert(test_client_with_id(child.instance.id.as_str()).await),
            Err(AgentManagerError::Duplicate { .. })
        ));
        let transcript = access.stop(&child.instance.id).await.unwrap();
        assert_eq!(
            *finalization.borrow_and_update(),
            crate::harness::manager::AgentFinalizationStatus::Stopped
        );
        assert_eq!(transcript.id.as_str(), child.instance.id.as_str());
        assert_eq!(child_handle.snapshot().status, AgentInstanceStatus::Stopped);
        assert_eq!(
            grandchild_handle.snapshot().status,
            AgentInstanceStatus::Stopped
        );
        assert_eq!(manager.list().unwrap().len(), 2);
        assert!(matches!(
            manager.get(&child.instance.id),
            Err(AgentManagerError::Unknown { .. })
        ));
        assert_eq!(peer.snapshot().status, AgentInstanceStatus::Idle);
        for instance in [&child_handle, &grandchild_handle] {
            let finalization = instance.finalization().unwrap();
            assert_eq!(
                Some(finalization.registration_id),
                instance.registration_id()
            );
            assert!(finalization.cleanup_error.is_none());
            let view = finalization.view.unwrap();
            assert_eq!(view.snapshot.status, AgentInstanceStatus::Stopped);
            assert_eq!(view.transcript.id.as_str(), instance.id().as_str());
        }
        assert!(peer.finalization().is_none());
        assert!(matches!(
            access.interrupt(peer.id()).await,
            Err(AgentManagerError::Denied {
                operation: AgentOperation::Interrupt,
                ..
            })
        ));
        for through_handle in [false, true] {
            let child = access.register_child(test_client(None).await).unwrap();
            let grandchild = manager
                .for_agent(&child.instance.id)
                .unwrap()
                .register_child(test_client(None).await)
                .unwrap();
            let grandchild = manager.get(&grandchild.instance.id).unwrap();
            access.interrupt(&child.instance.id).await.unwrap();
            assert_eq!(grandchild.snapshot().status, AgentInstanceStatus::Idle);
            assert_eq!(
                manager.get(&child.instance.id).unwrap().snapshot().status,
                AgentInstanceStatus::Idle
            );
            if through_handle {
                manager
                    .get(&child.instance.id)
                    .unwrap()
                    .stop()
                    .await
                    .unwrap();
            } else {
                manager.stop(&child.instance.id).await.unwrap();
            }
            assert_eq!(grandchild.snapshot().status, AgentInstanceStatus::Stopped);
            assert!(grandchild.finalization().unwrap().cleanup_error.is_none());
            assert_eq!(manager.list().unwrap().len(), 2);
        }
        assert!(matches!(
            access.stop(peer.id()).await,
            Err(AgentManagerError::Denied { .. })
        ));
        manager.shutdown().await.unwrap();
        for instance in [&root, &peer] {
            assert!(instance.finalization().unwrap().cleanup_error.is_none());
            assert!(instance.finalization().unwrap().view.is_some());
        }
    }

    #[tokio::test]
    async fn abandoned_stop_and_shutdown_finish_the_captured_subtree() {
        for shutdown in [false, true] {
            let manager = AgentManager::new();
            let root = manager.insert(test_client(None).await).unwrap();
            let child = manager
                .for_agent(root.id())
                .unwrap()
                .register_child(test_client(None).await)
                .unwrap();
            let child = manager.get(&child.instance.id).unwrap();
            let caller = manager.clone();
            let id = root.id().clone();
            let mut stop = Box::pin(async move {
                if shutdown {
                    caller.shutdown().await.map(|_| ())
                } else {
                    caller.stop(&id).await.map(|_| ())
                }
            });
            assert!(futures::poll!(stop.as_mut()).is_pending());
            assert_eq!(root.snapshot().status, AgentInstanceStatus::Idle);
            assert!(matches!(
                root.submit(crate::core::turn::TurnPrompt {
                    tool_policy: Default::default(),
                    prompt: "must not commit after closure".into(),
                    source: crate::core::lifecycle::UserPromptSubmitSource::Interactive,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                })
                .await,
                Err(AgentInstanceError::Stopped)
            ));
            drop(stop);
            if shutdown {
                assert!(matches!(
                    manager.insert(test_client(None).await),
                    Err(AgentManagerError::ShuttingDown)
                ));
            }
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if [&root, &child]
                        .iter()
                        .all(|instance| instance.finalization().is_some())
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("abandoned control request left captured finalization unfinished");
            assert!(manager.list().unwrap().is_empty());
            for instance in [&root, &child] {
                let finalization = instance.finalization().unwrap();
                assert!(finalization.cleanup_error.is_none());
                assert!(!finalization.view.unwrap().transcript.has_user_message());
            }
        }
    }

    #[tokio::test]
    async fn failed_stop_retains_registration_evidence_and_does_not_skip_peers() {
        let manager = AgentManager::new();
        let root = manager.insert(test_client(None).await).unwrap();
        let access = manager.for_agent(root.id()).unwrap();
        let failed = access.register_child(test_client(None).await).unwrap();
        let failed = manager.get(&failed.instance.id).unwrap();
        let peer = access.register_child(test_client(None).await).unwrap();
        let peer = manager.get(&peer.instance.id).unwrap();
        {
            let mut registry = manager.lock().unwrap();
            let member = registry.members.get_mut(failed.id()).unwrap();
            member.instance = failed.without_owner();
        }
        assert!(manager.stop(root.id()).await.is_err());
        for instance in [&root, &peer] {
            assert!(instance.finalization().unwrap().cleanup_error.is_none());
            assert!(instance.finalization().unwrap().view.is_some());
        }
        let finalization = failed.finalization().unwrap();
        assert!(finalization.cleanup_error.is_some());
        assert!(finalization.view.is_none());
        assert!(manager.lock().unwrap().member(failed.id()).unwrap().closing);
        assert_eq!(manager.list().unwrap().len(), 1);
        assert!(matches!(
            manager.insert(test_client_with_id(failed.id().as_str()).await),
            Err(AgentManagerError::Duplicate { .. })
        ));
        failed.stop_local().await.unwrap();
        failed.stop().await.unwrap();
        assert!(failed.finalization().unwrap().cleanup_error.is_none());
        assert!(failed.finalization().unwrap().view.is_some());
        assert!(manager.list().unwrap().is_empty());
    }

    async fn test_client_with_id(id: &str) -> AgentInstanceCandidate {
        let mut transcript = Transcript::new();
        transcript.id = id.into();
        test_client(Some(transcript)).await
    }

    #[tokio::test]
    async fn removes_instances_stopped_through_a_handle() {
        let manager = AgentManager::new();
        let instance = manager.insert(test_client(None).await).unwrap();
        let id = instance.id().clone();

        instance.stop().await.unwrap();

        assert!(manager.list().unwrap().is_empty());
        assert!(matches!(
            manager.get(&id),
            Err(AgentManagerError::Unknown { .. })
        ));
        let finalization = instance.finalization().unwrap();
        let replacement = manager
            .insert(test_client_with_id(id.as_str()).await)
            .unwrap();
        assert_ne!(
            Some(finalization.registration_id),
            replacement.registration_id()
        );
        assert!(replacement.finalization().is_none());
        assert_eq!(
            instance.finalization().unwrap().registration_id,
            finalization.registration_id
        );
        drop(manager);
        let mut stopping = Box::pin(replacement.stop());
        assert!(futures::poll!(stopping.as_mut()).is_pending());
        drop(stopping);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while replacement.finalization().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let finalization = replacement.finalization().unwrap();
        assert_eq!(
            finalization.view.unwrap().transcript.id.as_str(),
            id.as_str()
        );
        assert!(finalization.cleanup_error.is_none());
    }
}
