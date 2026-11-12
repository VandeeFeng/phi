//! Harness-owned background orchestration; workers retain independent execution.

mod runtime;
mod script;

pub(crate) use runtime::{
    AgentInvocation, InvocationId, WorkflowAdmission, WorkflowAgentOptions, WorkflowReport,
};
pub use runtime::{
    WorkflowError, WorkflowInvocationSnapshot, WorkflowInvocationStatus, WorkflowLimits,
    WorkflowRun, WorkflowRunId, WorkflowRuntime, WorkflowSnapshot, WorkflowStatus,
    WorkflowSubscription, WorkflowWorkerSnapshot,
};
pub use script::{WorkflowLaunch, WorkflowRequest};

#[cfg(test)]
mod tests;
