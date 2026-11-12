use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use phi_sdk::agent_instance::{AgentInstanceId, AgentInstanceStatus};
use phi_sdk::agent_manager::AgentManager;
use phi_sdk::lifecycle::SessionEndReason;

use super::session::AgentSession;

use super::*;
use crate::cli::Cli;
use crate::config::Settings;
use phi_sdk::PhiRuntimeServices;
use phi_sdk::extension::ExtensionRuntime;
use phi_sdk::sandbox::Sandbox;

pub(crate) struct TestRuntime {
    pub(crate) preparation: Arc<RuntimePreparation>,
    pub(crate) sessions: Arc<SessionAssociations>,
    pub(crate) manager: AgentManager,
    pub(crate) workflows: Arc<phi_sdk::workflow::WorkflowRuntime>,
}

impl TestRuntime {
    pub(crate) fn new(factory: AgentRuntimeFactory) -> Self {
        let manager = AgentManager::new();
        let workflows = Arc::new(phi_sdk::workflow::WorkflowRuntime::default());
        Self {
            preparation: RuntimePreparation::new(factory),
            sessions: SessionAssociations::new(manager.clone(), workflows.clone()),
            manager,
            workflows,
        }
    }
}

mod children;
mod delegation;
mod history;
mod memory;
mod permissions;
mod resources;
mod workflow;
