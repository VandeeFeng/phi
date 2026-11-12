//! Application runtime preparation, Session associations and canonical persistence.
//! Execution and delegation remain SDK-owned; modes select concrete capabilities.

mod associations;
mod factory;
mod gateway;
mod mcp;
pub(crate) mod preparation;
pub(crate) mod save;
pub(crate) mod selection;
pub(crate) mod session;
mod subagents;
#[cfg(test)]
mod tests;
mod workspace;

#[cfg(test)]
pub(crate) use associations::ActiveSessionDeletion;
pub(crate) use associations::{SavedSessionLocator, SessionAccess, SessionAssociations};
pub(crate) use factory::{
    AgentInstanceBuildOptions, AgentRuntime, AgentRuntimeFactory, PhiBuildOptions,
    PreparedAgentRuntime, RuntimeRebuildOptions, RuntimeResources,
};
pub(crate) use mcp::{McpReconnectResult, McpServerView};
#[cfg(test)]
pub(crate) use preparation::tests::TestConfiguration;
#[cfg(test)]
pub(crate) use session::AgentSessionSnapshot;
pub(crate) use session::{AgentSessionRecovery, RuntimeDeclarations, RuntimeView};
pub(crate) use subagents::SubagentView;
#[cfg(test)]
pub(crate) use tests::TestRuntime;
pub(crate) use workspace::WorkspaceView;

pub(crate) mod attachment;

use crate::session::Session;
use std::sync::{Arc, OnceLock, RwLock};
use subagents::ChildRuntimePolicy;

pub(crate) struct RuntimePreparation {
    configuration: OnceLock<Arc<self::preparation::RuntimeConfiguration>>,
    runtime_factory: RwLock<Arc<AgentRuntimeFactory>>,
    child_policy: RwLock<Option<ChildRuntimePolicy>>,
}

impl RuntimePreparation {
    pub(crate) fn enable_subagents(&self, sessions: &Arc<SessionAssociations>) {
        self.set_child_policy(ChildRuntimePolicy {
            sessions: Arc::downgrade(sessions),
        });
    }

    pub(crate) fn new(factory: AgentRuntimeFactory) -> Arc<Self> {
        Arc::new(Self {
            configuration: OnceLock::new(),
            runtime_factory: RwLock::new(Arc::new(factory)),
            child_policy: RwLock::new(None),
        })
    }

    fn initialize_configuration(
        &self,
        configuration: Arc<self::preparation::RuntimeConfiguration>,
    ) -> &Arc<self::preparation::RuntimeConfiguration> {
        self.configuration.get_or_init(|| configuration)
    }

    fn configuration(&self) -> anyhow::Result<&Arc<self::preparation::RuntimeConfiguration>> {
        self.configuration
            .get()
            .ok_or_else(|| anyhow::anyhow!("runtime preparation is not initialized"))
    }

    fn set_child_policy(&self, policy: ChildRuntimePolicy) {
        *self
            .child_policy
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(policy);
    }

    fn with_child_preparation<'a>(
        &self,
        mut options: RuntimeRebuildOptions<'a>,
    ) -> RuntimeRebuildOptions<'a> {
        let policy = self
            .child_policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(policy) = policy.as_ref() {
            options.instance.subagent_preparation = Some(Arc::new(policy.clone()));
        }
        options
    }

    async fn prepare_initial(
        &self,
        options: RuntimeRebuildOptions<'_>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let factory = self
            .runtime_factory
            .read()
            .map_err(|_| anyhow::anyhow!("runtime factory is unavailable"))?
            .clone();
        factory
            .prepare_initial(self.with_child_preparation(options))
            .await
    }

    async fn prepare(
        &self,
        options: RuntimeRebuildOptions<'_>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let factory = self
            .runtime_factory
            .read()
            .map_err(|_| anyhow::anyhow!("runtime factory is unavailable"))?
            .clone();
        factory.prepare(self.with_child_preparation(options)).await
    }

    async fn reload(
        &self,
        access: &SessionAccess,
        session: Session,
        prepared: PreparedAgentRuntime,
        expected_generation: Option<&str>,
    ) -> anyhow::Result<self::session::RuntimeView> {
        let factory = AgentRuntimeFactory::new(prepared.runtime.resources.clone());
        let agent_session = access.binding()?;
        let previous_resources = agent_session.runtime().sdk_resources();
        let snapshot = agent_session
            .replace_runtime_if_generation(prepared, session, None, true, expected_generation)
            .await?;
        previous_resources
            .session_ended(phi_sdk::lifecycle::SessionEndReason::Reload, None)
            .await;
        let resources = agent_session.runtime().sdk_resources();
        resources
            .session_started(phi_sdk::lifecycle::SessionStartReason::Reload, None)
            .await;
        *self
            .runtime_factory
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(factory);
        access.retain_notices(&previous_resources)?;
        Ok(snapshot)
    }
}

#[cfg(test)]
impl RuntimePreparation {
    fn runtime_factory(&self) -> anyhow::Result<Arc<AgentRuntimeFactory>> {
        Ok(self
            .runtime_factory
            .read()
            .map_err(|_| anyhow::anyhow!("runtime factory is unavailable"))?
            .clone())
    }
}
