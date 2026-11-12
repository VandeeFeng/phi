use super::team;
use crate::harness::instance::builder::AgentInstanceCandidate;
use crate::harness::instance::{
    AgentInstance, AgentInstanceError, AgentInstanceId, AgentInstanceSnapshot, AgentInstanceStatus,
    AgentInstanceView,
};
use crate::harness::manager::policy::denied;
use crate::harness::manager::registry::{Member, Registry, instance_error};
use crate::transcript::Transcript;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

// Developer-facing Agent API for trusted SDK consumers and registration-bound callers.
// Private coordination preserves authority, admission and reliable-result protocols.

const MAX_ALIAS_BYTES: usize = 64;
pub(super) const LEAD_ALIAS: &str = "lead";

/// An instance name, independent of its Agent definition and management authority.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AgentAlias(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "agent alias must be 1-{MAX_ALIAS_BYTES} ASCII letters, digits, hyphens or underscores, starting with a letter or digit"
)]
pub struct InvalidAgentAlias;

impl AgentAlias {
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidAgentAlias> {
        Self::try_from(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for AgentAlias {
    type Error = InvalidAgentAlias;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() > MAX_ALIAS_BYTES
            || !value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(InvalidAgentAlias);
        }
        Ok(Self(value))
    }
}

impl From<AgentAlias> for String {
    fn from(alias: AgentAlias) -> Self {
        alias.0
    }
}

/// Instance kind and associations fixed at creation, not responsibilities or authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AgentKind {
    Master,
    Subagent {
        creator: AgentInstanceId,
    },
    Teammate {
        creator: AgentInstanceId,
        team: AgentTeamId,
    },
}

impl AgentKind {
    pub fn creator(&self) -> Option<&AgentInstanceId> {
        match self {
            Self::Master => None,
            Self::Subagent { creator } | Self::Teammate { creator, .. } => Some(creator),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct AgentTeamId(pub(crate) String);

impl AgentTeamId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub struct AgentMemberSnapshot {
    pub instance: AgentInstanceSnapshot,
    pub team: Option<AgentTeamId>,
    pub created_by: Option<AgentInstanceId>,
    pub kind: AgentKind,
    pub alias: Option<AgentAlias>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AgentDiscovery {
    pub id: AgentInstanceId,
    pub definition: Option<String>,
    pub working_dir: std::path::PathBuf,
    pub status: AgentInstanceStatus,
    pub team: Option<AgentTeamId>,
    pub created_by: Option<AgentInstanceId>,
    pub kind: AgentKind,
    pub alias: Option<AgentAlias>,
}

impl From<AgentMemberSnapshot> for AgentDiscovery {
    fn from(member: AgentMemberSnapshot) -> Self {
        Self {
            id: member.instance.id,
            definition: member.instance.definition,
            working_dir: member.instance.working_dir,
            status: member.instance.status,
            team: member.team,
            created_by: member.created_by,
            kind: member.kind,
            alias: member.alias,
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentManagerError {
    #[error("agent instance `{id}` already exists")]
    Duplicate { id: String },
    #[error("unknown agent instance `{id}`")]
    Unknown { id: String },
    #[error("unknown agent team `{id}`")]
    UnknownTeam { id: String },
    #[error("invalid Team definition: {message}")]
    TeamDefinition { message: String },
    #[error("Team namespace `{namespace}` belongs to another lead registration")]
    TeamNamespaceConflict { namespace: String },
    #[error("Team storage operation failed: {message}")]
    TeamStorage { message: String },
    #[error("Agent alias `{alias}` is reserved or in use")]
    AliasInUse { alias: String },
    #[error("unknown or unavailable Agent alias `{alias}`")]
    UnknownAlias { alias: String },
    #[error("agent team limit ({limit}) reached")]
    TeamLimit { limit: usize },
    #[error("Team member and startup reservation limit ({limit}) reached")]
    TeamMemberLimit { limit: usize },
    #[error("pending message limit ({limit}) reached")]
    MessageLimit { limit: usize },
    #[error("pending message payload budget ({limit} bytes) exhausted")]
    MessageBytesLimit { limit: usize },
    #[error("message `{id}` receipt is unknown, evicted or outside this registration")]
    MessageUnavailable { id: String },
    #[error("wait timeout exceeds {limit} milliseconds")]
    InvalidWaitTimeout { limit: u64 },
    #[error("pending Agent admission limit ({limit}) reached")]
    AdmissionLimit { limit: usize },
    #[error("live Agent instance limit ({limit}) reached")]
    InstanceLimit { limit: usize },
    #[error("agent `{caller}` is not authorized for {operation:?} on `{target}`")]
    Denied {
        caller: String,
        operation: AgentOperation,
        target: String,
    },
    #[error("agent instance `{id}` operation failed: {source}")]
    Instance {
        id: String,
        #[source]
        source: AgentInstanceError,
    },
    #[error(transparent)]
    Subagent(#[from] crate::harness::instance::execution::PhiSubagentError),
    #[error("child candidate preparation failed: {source}")]
    Preparation {
        #[source]
        source: crate::harness::instance::builder::AgentInstanceBuildError,
    },
    #[error("invalid child candidate: {reason}")]
    InvalidCandidate { reason: &'static str },
    #[error("Agent admission was interrupted")]
    AdmissionInterrupted,
    #[error("child application association failed: {message}")]
    Association { message: String },
    #[error("Agent admission rejected: {cause}; compensation failed: {cleanup}")]
    Compensation {
        #[source]
        cause: Box<AgentManagerError>,
        cleanup: Box<AgentManagerError>,
    },
    #[error("agent manager is shutting down")]
    ShuttingDown,
    #[error("stop coordination failed: {message}")]
    StopCoordination { message: String },
    #[error("agent manager registry is unavailable")]
    Unavailable,
}

/// Trusted SDK consumer entry point. Agent-originated requests use `for_agent` instead.
#[derive(Clone)]
pub struct AgentManager {
    pub(crate) registry: Arc<Mutex<Registry>>,
}

impl Default for AgentManager {
    fn default() -> Self {
        Self {
            registry: Arc::new_cyclic(|owner| Mutex::new(Registry::with_owner(owner.clone()))),
        }
    }
}

impl AgentManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &self,
        candidate: AgentInstanceCandidate,
    ) -> Result<AgentInstance, AgentManagerError> {
        self.lock_live()?.register(candidate, None)
    }

    /// Changes future named-spawn routing without changing existing membership or list bindings.
    pub fn set_team_capability(
        &self,
        id: &AgentTeamId,
        capability: team::TeamCapability,
    ) -> Result<(), AgentManagerError> {
        let mut registry = self.lock_live()?;
        let lead = registry.team(id)?;
        let member = registry.member(lead.config.lead())?;
        if !lead.ready || member.closing || member.epoch != lead.lead_epoch {
            return Err(AgentManagerError::AdmissionInterrupted);
        }
        registry
            .teams
            .get_mut(id)
            .expect("validated Team")
            .capability = capability;
        Ok(())
    }

    pub fn grant_communication(
        &self,
        caller: &AgentInstanceId,
        target: &AgentInstanceId,
    ) -> Result<(), AgentManagerError> {
        let mut registry = self.lock_live()?;
        let pair = (
            registry.member(caller)?.epoch,
            registry.member(target)?.epoch,
        );
        registry.communication.insert(pair);
        Ok(())
    }

    pub fn grant_review(
        &self,
        caller: &AgentInstanceId,
        target: &AgentInstanceId,
    ) -> Result<(), AgentManagerError> {
        let mut registry = self.lock_live()?;
        let caller_member = registry.member(caller)?;
        let pair = (caller_member.epoch, registry.member(target)?.epoch);
        registry.reviews.insert(pair);
        Ok(())
    }

    pub fn for_agent(&self, caller: &AgentInstanceId) -> Result<AgentAccess, AgentManagerError> {
        let epoch = self.lock_live()?.member(caller)?.epoch;
        Ok(AgentAccess::new(self.clone(), caller.clone(), epoch))
    }

    pub fn get(&self, id: &AgentInstanceId) -> Result<AgentInstance, AgentManagerError> {
        Ok(self.lock_live()?.member(id)?.instance.clone())
    }

    pub fn discover(&self) -> Result<Vec<AgentDiscovery>, AgentManagerError> {
        let mut members = self
            .lock_live()?
            .members
            .values()
            .map(|member| AgentDiscovery::from(member.snapshot()))
            .collect::<Vec<_>>();
        members.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(members)
    }

    pub fn list(&self) -> Result<Vec<AgentInstanceSnapshot>, AgentManagerError> {
        let mut snapshots = self
            .lock_live()?
            .members
            .values()
            .map(|member| member.instance.snapshot())
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(snapshots)
    }

    pub async fn interrupt(&self, id: &AgentInstanceId) -> Result<(), AgentManagerError> {
        self.get(id)?
            .abort()
            .await
            .map_err(|source| instance_error(id, source))
    }

    pub async fn stop(&self, id: &AgentInstanceId) -> Result<Transcript, AgentManagerError> {
        let (instance, epoch) = {
            let registry = self.lock_live()?;
            let member = registry.member(id)?;
            (member.instance.clone(), member.epoch)
        };
        self.stop_registered(instance, epoch).await
    }

    pub async fn shutdown(&self) -> Result<Vec<Transcript>, AgentManagerError> {
        let members = {
            let mut registry = self.lock_live()?;
            registry.shutting_down = true;
            registry.close_members(|_| true)
        };
        self.stop_members(members).await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentOperation {
    CreateAgent,
    CreateTeam,
    Stop,
    Interrupt,
    Observe,
    Message,
    ReadHistory,
}

/// Caller-bound access. Binding and grant mutation belong to trusted SDK consumers code.
#[derive(Clone)]
pub struct AgentAccess {
    pub(crate) manager: AgentManager,
    pub(crate) caller: AgentInstanceId,
    pub(crate) epoch: uuid::Uuid,
}

impl AgentAccess {
    pub async fn spawn_agent(
        &self,
        phi: &crate::factory::Phi,
        request: crate::harness::manager::preparation::AgentSpawnRequest,
    ) -> Result<crate::harness::manager::preparation::AgentCreation, AgentManagerError> {
        self.manager.spawn_scoped(self, phi, request).await
    }

    /// Sends follow-up work or coordination; acceptance is not recipient completion.
    pub async fn send_message(
        &self,
        recipient: &AgentInstanceId,
        content: impl Into<String>,
    ) -> Result<crate::harness::manager::message::AgentMessageReceipt, AgentManagerError> {
        self.manager
            .queue_message(&self.caller, recipient, content.into(), Some(self))
            .await
    }

    pub fn message_updates(
        &self,
        id: &crate::harness::manager::message::AgentMessageId,
    ) -> Result<Option<crate::harness::manager::message::AgentMessageUpdates>, AgentManagerError>
    {
        let mut registry = self.manager.lock_live()?;
        self.caller(&registry, AgentOperation::Observe)?;
        let updates = registry.messages.capture(id);
        if updates
            .as_ref()
            .is_some_and(|updates| !updates.belongs_to(AgentRegistrationId::new(self.epoch)))
        {
            return Err(denied(&self.caller, AgentOperation::Observe, id.as_str()));
        }
        Ok(updates)
    }

    pub fn list(&self) -> Result<Vec<AgentMemberSnapshot>, AgentManagerError> {
        let registry = self.manager.lock_live()?;
        let caller = self.caller(&registry, AgentOperation::Observe)?;
        let mut snapshots = registry
            .members
            .values()
            .filter(|target| self.allows(&registry, caller, target, AgentOperation::Observe))
            .map(Member::snapshot)
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| left.instance.id.as_str().cmp(right.instance.id.as_str()));
        Ok(snapshots)
    }

    pub async fn interrupt(&self, target: &AgentInstanceId) -> Result<(), AgentManagerError> {
        let instance = {
            let registry = self.manager.lock_live()?;
            self.control_target(&registry, target, AgentOperation::Interrupt)?
                .0
        };
        instance
            .abort()
            .await
            .map_err(|source| instance_error(target, source))
    }

    pub async fn stop(&self, target: &AgentInstanceId) -> Result<Transcript, AgentManagerError> {
        let (instance, epoch) = {
            let registry = self.manager.lock_live()?;
            self.control_target(&registry, target, AgentOperation::Stop)?
        };
        self.manager.stop_registered(instance, epoch).await
    }

    pub async fn view(
        &self,
        target: &AgentInstanceId,
    ) -> Result<AgentInstanceView, AgentManagerError> {
        let instance = {
            let registry = self.manager.lock_live()?;
            self.history_reader(&registry, target)?
        };
        instance
            .view()
            .await
            .map_err(|source| instance_error(target, source))
    }

    pub async fn transcript(
        &self,
        target: &AgentInstanceId,
    ) -> Result<Transcript, AgentManagerError> {
        Ok(self.view(target).await?.transcript)
    }

    /// Registers an idle child; creation admission and startup remain separate.
    pub fn register_child(
        &self,
        candidate: AgentInstanceCandidate,
    ) -> Result<AgentMemberSnapshot, AgentManagerError> {
        let mut registry = self.manager.lock_live()?;
        self.require_creation(&registry, AgentOperation::CreateAgent)?;
        let child = registry.register(candidate, Some(&self.caller))?;
        Ok(registry.member(child.id())?.snapshot())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AgentRegistrationId(uuid::Uuid);

impl AgentRegistrationId {
    pub(crate) fn new(epoch: uuid::Uuid) -> Self {
        Self(epoch)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AgentFinalizationStatus {
    Open,
    Closing,
    Stopped,
    Failed { message: String },
}

#[derive(Debug, Clone)]
pub struct AgentFinalization {
    pub instance_id: AgentInstanceId,
    pub registration_id: AgentRegistrationId,
    pub view: Option<AgentInstanceView>,
    pub cleanup_error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_validate_at_construction_and_deserialization() {
        for value in [
            "",
            ".",
            "..",
            "../other",
            "a/b",
            "a\\b",
            "_hidden",
            "two words",
            "审查",
        ] {
            assert!(AgentAlias::new(value).is_err(), "{value}");
            assert!(serde_json::from_value::<AgentAlias>(serde_json::json!(value)).is_err());
        }
        for length in 1..=MAX_ALIAS_BYTES {
            let alias = AgentAlias::new("a".repeat(length)).unwrap();
            let encoded = serde_json::to_string(&alias).unwrap();
            assert_eq!(serde_json::from_str::<AgentAlias>(&encoded).unwrap(), alias);
        }
        assert!(AgentAlias::new("a".repeat(MAX_ALIAS_BYTES + 1)).is_err());
        assert_eq!(
            AgentAlias::new("security-review_2").unwrap().as_str(),
            "security-review_2"
        );
    }

    #[test]
    fn kind_preserves_creator_independently_of_team_membership() {
        let creator = AgentInstanceId::new("parent");
        assert_eq!(AgentKind::Master.creator(), None);
        assert_eq!(
            serde_json::to_value(AgentKind::Master).unwrap(),
            serde_json::json!({ "kind": "master" })
        );
        for kind in [
            AgentKind::Subagent {
                creator: creator.clone(),
            },
            AgentKind::Teammate {
                creator: creator.clone(),
                team: serde_json::from_value(serde_json::json!("team")).unwrap(),
            },
        ] {
            assert_eq!(kind.creator(), Some(&creator));
            let encoded = serde_json::to_value(&kind).unwrap();
            assert!(encoded.get("role").is_none());
            assert!(encoded.get("alias").is_none());
            assert_eq!(serde_json::from_value::<AgentKind>(encoded).unwrap(), kind);
        }
    }
}
