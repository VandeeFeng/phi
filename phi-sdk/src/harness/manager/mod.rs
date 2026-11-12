mod api;
pub(crate) mod control;
pub(crate) mod coordinator;
pub(crate) mod message;
pub(crate) mod policy;
pub(crate) mod preparation;
pub(crate) mod registry;
pub(crate) mod team;

use api::LEAD_ALIAS;
pub use api::{
    AgentAccess, AgentAlias, AgentDiscovery, AgentFinalization, AgentFinalizationStatus, AgentKind,
    AgentManager, AgentManagerError, AgentMemberSnapshot, AgentOperation, AgentRegistrationId,
    AgentTeamId, InvalidAgentAlias,
};
