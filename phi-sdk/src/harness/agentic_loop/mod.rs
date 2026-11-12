mod pipeline;
mod response;
mod runner;
mod runtime;
mod search;
mod tools;

#[cfg(test)]
mod tests;

pub(crate) use pipeline::EventPipeline;
pub(in crate::harness) use pipeline::{envelope_core_events, prepare_completion_feedback};
pub(in crate::harness) use runner::CompletionDecision;
pub(crate) use runner::{
    AgentInputCommit, AgentRunner, AgentSteerReservation, AgentSteerSubmission,
};
#[cfg(test)]
pub(crate) use runner::{AgentSteerMailbox, RoundControl};
pub use runtime::AgentRuntime;
pub(in crate::harness) use runtime::dispatch_runtime;
pub(crate) use search::registration as search_registration;

use crate::hooks::HookRegistry;
use rig::completion::CompletionModel;

#[derive(Clone)]
pub struct AgenticLoop<M>
where
    M: CompletionModel,
{
    runtime: AgentRuntime<M>,
    hooks: HookRegistry,
    interaction_scope: crate::core::event::CoreEventScope,
    max_turns: usize,
    tool_policy: crate::core::turn::TurnToolPolicy,
}
