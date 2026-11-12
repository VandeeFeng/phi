//! Private serialized execution; SDK handles provide access without sharing ownership.

pub(crate) mod child;

pub(crate) mod actor;
pub(crate) mod execution;

pub(crate) mod bash;
pub(crate) mod builder;
pub(crate) mod handle;
pub(crate) mod interaction;
mod progress;

#[cfg(test)]
mod tests;
pub use handle::{
    AgentInstance, AgentInstanceError, AgentInstanceId, AgentInstanceSnapshot, AgentInstanceStatus,
    AgentInstanceView,
};
pub use progress::{ExecutionProgress, ExecutionStatus, ExecutionTool};
