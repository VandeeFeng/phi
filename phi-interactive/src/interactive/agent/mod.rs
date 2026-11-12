//! Main/child Agent event projection, prompt interaction and active-view navigation.
//! Attachments supply canonical observations; this module never executes or persists Agents.

mod agent;
mod input;
mod prompt;
mod subagent;

pub(super) use agent::drain_agent_events;
#[cfg(test)]
pub(super) use input::handle_subagent_key;
pub(super) use input::{ActiveSubagentAction, handle_active_subagent_input};
pub(super) use prompt::{
    directed_agent_invocation, project_hook_notice, project_session_notices, submit_prompt,
};
pub(crate) use subagent::SubagentProjection;
