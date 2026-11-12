//! Agent execution and orchestration infrastructure.

pub(crate) mod agentic_loop;
pub(crate) mod instance;
pub(crate) mod manager;
pub(crate) mod task;
pub(crate) mod workflow;

#[cfg(test)]
mod tests;
