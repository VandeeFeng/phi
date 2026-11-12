use crate::extension::delegated_tool_denials;
use crate::factory::Phi;
use crate::harness::instance::execution::{AgentInstanceState, RuntimeGeneration};
use crate::tools::ToolPolicy;
use crate::transcript::Transcript;
use std::sync::Arc;

// Developer-facing instance construction policy and opaque validated runtime candidates.
// Private assembly creates isolated execution resources before registration or replacement.

pub struct AgentInstanceBuilder<'a> {
    pub(crate) phi: &'a Phi,
    pub(crate) options: AgentInstanceOptions,
    pub(crate) transcript: Option<Transcript>,
    pub(crate) delegated_child: bool,
    pub(crate) task_list: Option<crate::tasks::TaskListBinding>,
    pub(crate) team_capability: crate::agent_manager::TeamCapability,
    pub(crate) working_dir: Option<std::path::PathBuf>,
    pub(crate) subagent_preparation:
        Option<Arc<dyn crate::harness::manager::preparation::SubagentPreparation>>,
}

impl<'a> AgentInstanceBuilder<'a> {
    pub(crate) fn new(phi: &'a Phi) -> Self {
        Self {
            phi,
            options: AgentInstanceOptions::default(),
            transcript: None,
            delegated_child: false,
            task_list: None,
            team_capability: Default::default(),
            working_dir: None,
            subagent_preparation: None,
        }
    }

    pub fn subagent_preparation(
        mut self,
        preparation: Arc<dyn crate::harness::manager::preparation::SubagentPreparation>,
    ) -> Self {
        self.subagent_preparation = Some(preparation);
        self
    }

    /// Applies subagent-only memory and tool boundaries to an independent child instance.
    pub fn delegated_child(mut self) -> Self {
        self.delegated_child = true;
        self
    }

    pub fn task_list(mut self, binding: crate::tasks::TaskListBinding) -> Self {
        self.task_list = Some(binding);
        self
    }

    /// Controls future named spawning in an already initialized root Team.
    pub fn team_capability(mut self, capability: crate::agent_manager::TeamCapability) -> Self {
        self.team_capability = capability;
        self
    }

    pub fn working_dir(mut self, working_dir: std::path::PathBuf) -> Self {
        self.working_dir = Some(working_dir);
        self
    }

    pub fn transcript(mut self, transcript: Transcript) -> Self {
        self.transcript = Some(transcript);
        self
    }

    pub fn options(mut self, options: AgentInstanceOptions) -> Self {
        self.options = options;
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.options.model = Some(model.into());
        self
    }

    pub fn agent_mode(mut self, mode: crate::config::AgentMode) -> Self {
        self.options.agent_mode = Some(mode);
        self
    }

    pub fn mode_prompt_override(mut self, prompt: impl Into<String>) -> Self {
        self.options.mode_prompt_override = Some(prompt.into());
        self
    }

    pub fn tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options.tool_policy.allowed = Some(tools.into_iter().map(Into::into).collect());
        self
    }

    pub fn disallowed_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.options.tool_policy.disallowed = tools.into_iter().map(Into::into).collect();
        self
    }

    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.options.system_prompt = Some(prompt.into());
        self
    }

    fn apply_instance_policy(&mut self) {
        if self.delegated_child {
            self.options.tool_policy.disallowed =
                delegated_tool_denials(std::mem::take(&mut self.options.tool_policy.disallowed));
        }
    }

    pub fn build(
        mut self,
    ) -> impl std::future::Future<Output = Result<AgentInstanceCandidate, AgentInstanceBuildError>>
    {
        let validation = self.options.validate();
        self.apply_instance_policy();
        self.prepare(validation)
    }
}

#[cfg(test)]
mod delegated_child_tests {
    use super::*;

    #[test]
    fn rejects_empty_instance_model_with_typed_error() {
        let options = AgentInstanceOptions {
            model: Some("  ".to_string()),
            ..AgentInstanceOptions::default()
        };

        assert!(matches!(
            options.validate(),
            Err(AgentInstanceBuildError::InvalidOption { field: "model", .. })
        ));
    }

    #[test]
    fn delegated_child_denies_tape_mutation_idempotently() {
        let phi = Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .no_tools(true)
            .build()
            .unwrap();
        let mut builder = phi.agent_instance_builder().delegated_child();
        builder.options.tool_policy.disallowed = vec!["existing".into(), "tape_anchor".into()];
        builder.apply_instance_policy();
        builder.apply_instance_policy();
        assert_eq!(
            builder.options.tool_policy.disallowed,
            [
                "existing",
                "tape_anchor",
                "tape_delete",
                "tape_reset",
                "memory_sync",
            ]
        );
    }
}

impl AgentInstanceCandidate {
    /// Retains the exact resources prepared for this candidate's generation.
    pub fn resources(&self) -> crate::factory::AgentRuntimeResources {
        self.state.generation.resources.clone()
    }

    pub fn id(&self) -> &str {
        self.state.transcript.id.as_str()
    }

    pub fn task_list(&self) -> Option<&crate::tasks::TaskListBinding> {
        self.state.task_list.as_ref()
    }

    pub fn into_runtime_candidate(self) -> RuntimeCandidate {
        RuntimeCandidate {
            instance_id: self.state.transcript.id.to_string(),
            delegated_child: self.state.delegated_child,
            task_list: self.state.task_list,
            generation: self.state.generation,
            branch: None,
        }
    }
}

/// Complete validated candidate consumed when creating an Agent Instance.
pub struct AgentInstanceCandidate {
    pub(crate) state: AgentInstanceState,
}

/// Complete validated candidate for atomic runtime replacement.
pub struct RuntimeCandidate {
    pub(crate) instance_id: String,
    pub(crate) delegated_child: bool,
    pub(crate) task_list: Option<crate::tasks::TaskListBinding>,
    pub(crate) generation: RuntimeGeneration,
    pub(crate) branch: Option<BranchSelection>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BranchSelection {
    pub leaf_id: Option<String>,
}

impl RuntimeCandidate {
    pub fn select_branch(mut self, selection: BranchSelection) -> Self {
        self.branch = Some(selection);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirectedDelegation {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<crate::harness::manager::preparation::DelegationMode>,
}

#[derive(Debug, Clone, Default)]
pub struct AgentInstanceOptions {
    pub model: Option<String>,
    pub agent_mode: Option<crate::config::AgentMode>,
    pub mode_prompt_override: Option<String>,
    pub tool_policy: ToolPolicy,
    pub system_prompt: Option<String>,
}

impl AgentInstanceOptions {
    fn validate(&self) -> Result<(), AgentInstanceBuildError> {
        if self
            .model
            .as_ref()
            .is_some_and(|model| model.trim().is_empty())
        {
            return Err(AgentInstanceBuildError::InvalidOption {
                field: "model",
                reason: "cannot be empty",
            });
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AgentInstanceBuildError {
    #[error("invalid Agent Instance option `{field}`: {reason}")]
    InvalidOption {
        field: &'static str,
        reason: &'static str,
    },
    #[error("failed to configure provider `{provider}`: {source}")]
    Provider {
        provider: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("failed to resolve model catalog for provider `{provider}`: {source}")]
    ModelCatalog {
        provider: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("unknown model `{provider}/{model}`")]
    UnknownModel { provider: String, model: String },
    #[error("model `{provider}/{model}` is incompatible with the provider runtime: {source}")]
    ModelConfiguration {
        provider: String,
        model: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("failed to initialize session: {source}")]
    Session {
        #[source]
        source: anyhow::Error,
    },
    #[error("failed to initialize sandbox: {source}")]
    Sandbox {
        #[source]
        source: anyhow::Error,
    },
    #[error("failed to construct runtime: {source}")]
    Runtime {
        #[source]
        source: anyhow::Error,
    },
}
