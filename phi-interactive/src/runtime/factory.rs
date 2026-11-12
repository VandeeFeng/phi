//! Complete runtime candidate construction and isolated resource generations.

use std::sync::Arc;

use phi_sdk::config::RuntimeConfig;
use phi_sdk::context::ContextFiles;
use phi_sdk::extension::{ExtensionRuntime, ExtensionSources};
use phi_sdk::permission::PermissionConfig;
use phi_sdk::sandbox::Sandbox;
use phi_sdk::transcript::Transcript;
use phi_sdk::{AgentInstanceCandidate, Phi, PhiRuntimeServices};

use crate::cli::Cli;
use crate::config::Settings;
use crate::interactive::tool_display_formatter;
use crate::session::Session;

use super::subagents::ChildRuntimePolicy;

pub(crate) struct AgentInstanceBuildOptions {
    pub(crate) transcript: Transcript,
    working_dir: std::path::PathBuf,
    delegated_child: bool,
    team_capability: phi_sdk::agent_manager::TeamCapability,
    task_list_id: Option<String>,
    task_list: Option<Option<phi_sdk::tasks::TaskListBinding>>,
    permission_allowlist: Vec<(String, String)>,
    pub(super) resource_settings: Option<Arc<Settings>>,
    pub(super) settings_manager: Option<crate::config::SettingsManager>,
    pub(super) resource_policy: Option<Arc<crate::resources::ResourcePolicy>>,
    pub(super) subagent_preparation: Option<Arc<dyn phi_sdk::agents::SubagentPreparation>>,
}

impl AgentInstanceBuildOptions {
    pub(crate) fn from_session(session: &Session) -> Self {
        Self::with_transcript(session, session.transcript.clone())
    }

    pub(crate) fn with_transcript(session: &Session, transcript: Transcript) -> Self {
        Self {
            transcript,
            working_dir: session.working_dir.as_str().into(),
            delegated_child: session.is_delegated(),
            team_capability: Default::default(),
            task_list_id: session.task_list_id.as_ref().map(ToString::to_string),
            task_list: None,
            permission_allowlist: Self::permission_allowlist(session),
            resource_settings: None,
            settings_manager: None,
            resource_policy: None,
            subagent_preparation: None,
        }
    }

    pub(super) fn for_child(
        session: &Session,
        task_list: Option<phi_sdk::tasks::TaskListBinding>,
    ) -> Self {
        Self {
            delegated_child: true,
            task_list: Some(task_list),
            ..Self::from_session(session)
        }
    }

    fn resolve_task_list(
        &self,
        capability: phi_sdk::agent_manager::TeamCapability,
    ) -> anyhow::Result<Option<phi_sdk::tasks::TaskListBinding>> {
        if let Some(binding) = &self.task_list {
            anyhow::ensure!(
                self.task_list_id.as_deref().is_none_or(|id| binding
                    .as_ref()
                    .is_some_and(|binding| binding.list_id() == id)),
                "Session conflicts with explicit task list binding"
            );
            return Ok(binding.clone());
        }
        if self.delegated_child && self.task_list_id.is_none() {
            return Ok(None);
        }
        let list_id = match &self.task_list_id {
            Some(id) => id.clone(),
            None if !self.delegated_child
                && capability == phi_sdk::agent_manager::TeamCapability::Enabled =>
            {
                phi_sdk::agent_manager::team_namespace(self.transcript.id.as_str())?
            }
            None => self.transcript.id.to_string(),
        };
        let root = std::path::absolute(crate::config::settings::phi_path().join("tasks"))?;
        Ok(Some(phi_sdk::tasks::TaskListBinding::new(root, list_id)?))
    }

    fn permission_allowlist(session: &Session) -> Vec<(String, String)> {
        session
            .permission_allowlist
            .iter()
            .map(|entry| (entry.tool.clone(), entry.pattern.clone()))
            .collect()
    }
}

pub(crate) struct PhiBuildOptions<'a> {
    pub(crate) cli: &'a Cli,
    pub(crate) provider: &'a str,
    pub(crate) model: &'a str,
    pub(crate) runtime_cfg: RuntimeConfig,
    pub(crate) context: ContextFiles,
    pub(crate) agent_options: crate::AgentBuildOptions,
    pub(crate) hooks: Vec<phi_sdk::hooks::Hook>,
}

pub(crate) struct RuntimeRebuildOptions<'a> {
    pub(crate) phi: PhiBuildOptions<'a>,
    pub(crate) instance: AgentInstanceBuildOptions,
}

#[derive(Clone, Copy)]
pub(super) enum ExtensionLoadMode {
    Initial,
    Strict,
}

#[derive(Clone)]
pub(crate) struct RuntimeResources {
    sdk: PhiRuntimeServices,
    pub(super) extensions: Arc<ExtensionRuntime>,
    extension_paths: Vec<std::path::PathBuf>,
    extension_sources: ExtensionSources,
    resource_policy: Arc<crate::resources::ResourcePolicy>,
    extension_load_failed: bool,
    model_catalog: Arc<phi_sdk::models::ModelCatalog>,
    pub(super) sandbox: Sandbox,
    mcp_manager: Option<phi_sdk::extension::mcp::McpClientManager>,
}

pub(crate) struct PreparedAgentRuntime {
    pub(super) instance: AgentInstanceCandidate,
    pub(super) runtime: AgentRuntime,
}

#[derive(Clone)]
pub(crate) struct AgentRuntime {
    generation_id: String,
    pub(super) workflow_tool: super::session::WorkflowTool,
    workflow_requested: bool,
    workflow_enabled: bool,
    pub(super) thinking_level: Option<String>,
    pub(super) supported_thinking_levels: Vec<phi_sdk::models::ThinkingLevel>,
    prompts: Arc<crate::resources::prompts::PromptResources>,
    memory: phi_sdk::extension::memory::MemoryConfig,
    pub(super) agent_diagnostics: Vec<String>,
    pub(super) compact_enabled: bool,
    background_required: bool,
    pub(super) team_capability: phi_sdk::agent_manager::TeamCapability,
    phi: Phi,
    sdk_resources: phi_sdk::agent_instance::AgentRuntimeResources,
    mcp_intent: Arc<McpRebuildIntent>,
    source_settings: Option<Arc<Settings>>,
    settings_manager: Option<crate::config::SettingsManager>,
    pub(super) mcp_contracts: Option<phi_sdk::extension::mcp::McpToolContracts>,
    pub(super) resources: RuntimeResources,
}

#[derive(Clone)]
struct McpRebuildIntent {
    cli: Arc<Cli>,
    runtime_cfg: RuntimeConfig,
    context: ContextFiles,
    workflow: bool,
    hooks: Vec<phi_sdk::hooks::Hook>,
    subagent_preparation: Option<Arc<dyn phi_sdk::agents::SubagentPreparation>>,
}

impl PreparedAgentRuntime {
    pub(crate) fn into_parts(self) -> (AgentInstanceCandidate, AgentRuntime) {
        (self.instance, self.runtime)
    }
}

impl AgentRuntime {
    pub(super) fn settings_manager(&self) -> anyhow::Result<&crate::config::SettingsManager> {
        self.settings_manager
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Agent runtime has no settings manager"))
    }

    pub(super) fn source_settings(&self) -> Option<Arc<Settings>> {
        self.source_settings.clone()
    }

    pub(super) fn resource_policy(&self) -> Arc<crate::resources::ResourcePolicy> {
        self.resources.resource_policy.clone()
    }

    pub(super) fn model_catalog(&self) -> Arc<phi_sdk::models::ModelCatalog> {
        self.resources.model_catalog.clone()
    }

    pub(super) fn runtime_config(&self) -> RuntimeConfig {
        self.mcp_intent.runtime_cfg.clone()
    }

    pub(crate) fn expand_prompt(
        &self,
        display: &str,
    ) -> anyhow::Result<crate::resources::prompts::PromptExpandResult> {
        self.prompts.expand(display).map_err(anyhow::Error::msg)
    }

    pub(crate) fn commands(&self) -> Vec<crate::commands::SlashCommand> {
        let settings = Settings {
            extensions: Some(
                self.mcp_intent
                    .runtime_cfg
                    .extensions
                    .disabled
                    .iter()
                    .map(|extension| format!("-{}", extension.path()))
                    .collect(),
            ),
            memory: Some(self.memory_config()),
            switches: self
                .source_settings
                .as_ref()
                .and_then(|settings| settings.switches.clone()),
            mcp_servers: self
                .source_settings
                .as_ref()
                .and_then(|settings| settings.mcp_servers.clone()),
            agent: self
                .source_settings
                .as_ref()
                .map(|settings| settings.agent.clone())
                .unwrap_or_default(),
            ..Default::default()
        };
        let extensions = self.extensions();
        crate::commands::CommandRegistry::new(extensions.adapter_declarations().commands())
            .declarations(self.prompts.commands(), &settings)
    }

    pub(crate) fn prompt_diagnostics(&self) -> Vec<crate::resources::ResourceDiagnostic> {
        self.prompts.diagnostics()
    }

    pub(crate) fn memory_config(&self) -> phi_sdk::extension::memory::MemoryConfig {
        self.memory.clone()
    }

    pub(super) fn workflow_requested(&self) -> bool {
        self.workflow_requested
    }

    pub(super) fn workflow_enabled(&self) -> bool {
        self.workflow_enabled
    }

    pub(crate) fn background_required(&self) -> bool {
        self.background_required
    }

    pub(crate) fn generation_id(&self) -> &str {
        &self.generation_id
    }

    pub(crate) fn phi(&self) -> &Phi {
        &self.phi
    }

    pub(super) fn sdk_resources(&self) -> phi_sdk::agent_instance::AgentRuntimeResources {
        self.sdk_resources.clone()
    }

    pub(crate) fn extensions(&self) -> Arc<ExtensionRuntime> {
        self.resources.extensions.clone()
    }

    pub(crate) fn sandbox(&self) -> Sandbox {
        self.resources.sandbox.clone()
    }

    #[cfg(test)]
    pub(crate) async fn probe_effect_permission(
        &self,
        effect: &phi_sdk::extension::ExtensionEffect,
    ) -> anyhow::Result<()> {
        use futures_util::FutureExt;
        self.extensions()
            .authorize_effect(effect)
            .now_or_never()
            .unwrap_or_else(|| Err(anyhow::anyhow!("permission requires interaction")))
    }

    pub(crate) fn mcp_manager(&self) -> Option<phi_sdk::extension::mcp::McpClientManager> {
        self.resources.mcp_manager.clone()
    }

    pub(super) async fn prepare_replacement(
        &self,
        session: &Session,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let mut instance = AgentInstanceBuildOptions::from_session(session);
        instance.team_capability = self.team_capability;
        instance.resource_settings = self.source_settings.clone();
        let resources = self
            .resources
            .isolated_session(&instance.permission_allowlist);
        self.prepare_retained(session, instance, resources).await
    }

    pub(super) async fn prepare_selection(
        &self,
        session: &Session,
        runtime_config: RuntimeConfig,
        catalog: Arc<phi_sdk::models::ModelCatalog>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let mut runtime = self.clone();
        Arc::make_mut(&mut runtime.mcp_intent).runtime_cfg = runtime_config;
        runtime.resources.model_catalog = catalog;
        runtime.prepare_replacement(session).await
    }

    pub(super) async fn prepare_child(
        &self,
        session: &Session,
        policy: ChildRuntimePolicy,
        task_list: Option<phi_sdk::tasks::TaskListBinding>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let settings = self
            .source_settings
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Agent runtime has no source configuration"))?;
        let working_dir = std::path::Path::new(session.working_dir.as_str());
        let sandbox =
            crate::security::execution_sandbox(&self.mcp_intent.cli, settings, working_dir)?;
        let services =
            AgentRuntimeFactory::permission_services(&self.mcp_intent.cli, settings, working_dir)?
                .interactive(!self.mcp_intent.cli.print)
                .model_catalog(self.resources.model_catalog.clone());
        let resources = RuntimeResources {
            sdk: services.sandbox(sandbox.clone()),
            sandbox,
            ..self.resources.clone()
        }
        .mcp_manager(self.resources.mcp_manager.clone());
        let mut instance = AgentInstanceBuildOptions::for_child(session, task_list);
        instance.resource_settings = self.source_settings.clone();
        instance.subagent_preparation = Some(Arc::new(policy));
        let resources = resources.isolated_session(&instance.permission_allowlist);
        self.prepare_retained(session, instance, resources).await
    }

    async fn prepare_retained(
        &self,
        session: &Session,
        mut instance: AgentInstanceBuildOptions,
        resources: RuntimeResources,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        instance.settings_manager = self.settings_manager.clone();
        instance.resource_policy = Some(resources.resource_policy.clone());
        let intent = self.mcp_intent.as_ref().clone();
        if instance.subagent_preparation.is_none() {
            instance.subagent_preparation = intent.subagent_preparation;
        }
        Box::pin(AgentRuntimeFactory::prepare_with_prompts(
            RuntimeRebuildOptions {
                phi: PhiBuildOptions {
                    cli: &intent.cli,
                    provider: &session.provider,
                    model: &session.model,
                    runtime_cfg: intent.runtime_cfg,
                    context: intent.context,
                    agent_options: crate::AgentBuildOptions {
                        workflow: intent.workflow,
                        ..Default::default()
                    },
                    hooks: intent.hooks,
                },
                instance,
            },
            resources,
            Some(self.prompts.clone()),
        ))
        .await
    }

    #[cfg(test)]
    pub(crate) fn allow_session_tools(&self, tools: &[(String, String)]) {
        for (tool, pattern) in tools {
            self.sdk_resources.allow_session_tool(tool.clone(), pattern);
        }
    }
}

impl RuntimeResources {
    #[cfg(test)]
    pub(crate) fn new(
        sdk: PhiRuntimeServices,
        extensions: Arc<ExtensionRuntime>,
        model_catalog: Arc<phi_sdk::models::ModelCatalog>,
        sandbox: Sandbox,
    ) -> Self {
        let working_dir = std::env::current_dir().unwrap_or_default();
        Self::new_for(sdk, extensions, model_catalog, sandbox, &working_dir)
    }

    #[cfg(test)]
    pub(crate) fn new_for(
        sdk: PhiRuntimeServices,
        extensions: Arc<ExtensionRuntime>,
        model_catalog: Arc<phi_sdk::models::ModelCatalog>,
        sandbox: Sandbox,
        working_dir: &std::path::Path,
    ) -> Self {
        let policy = Arc::new(
            crate::resources::ResourcePolicy::from_settings(
                working_dir,
                Default::default(),
                Default::default(),
            )
            .unwrap(),
        );
        let extension_paths = policy.extension_paths();
        Self::with_policy(
            sdk,
            extensions,
            model_catalog,
            sandbox,
            policy,
            extension_paths,
        )
    }

    pub(crate) fn with_policy(
        sdk: PhiRuntimeServices,
        extensions: Arc<ExtensionRuntime>,
        model_catalog: Arc<phi_sdk::models::ModelCatalog>,
        sandbox: Sandbox,
        resource_policy: Arc<crate::resources::ResourcePolicy>,
        extension_paths: Vec<std::path::PathBuf>,
    ) -> Self {
        let extension_sources = extensions.sources();
        Self {
            sdk: sdk.sandbox(sandbox.clone()),
            extensions,
            extension_paths,
            extension_sources,
            resource_policy,
            extension_load_failed: false,
            model_catalog,
            sandbox,
            mcp_manager: None,
        }
    }

    fn capture_extensions(mut self) -> anyhow::Result<Self> {
        self.extension_sources = ExtensionSources::capture(self.extension_paths.clone())?;
        self.extension_load_failed = false;
        Ok(self)
    }

    pub(crate) fn extension_load_failed(mut self, failed: bool) -> Self {
        self.extension_load_failed = failed;
        self
    }

    #[cfg(test)]
    pub(super) fn extension_paths(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.extension_sources = ExtensionSources::capture(paths.clone()).unwrap();
        self.extension_paths = paths;
        self
    }

    pub(crate) fn mcp_manager(
        mut self,
        manager: Option<phi_sdk::extension::mcp::McpClientManager>,
    ) -> Self {
        let manager = manager.map(|manager| manager.with_sandbox(self.sandbox.clone()));
        if let Some(manager) = manager.clone() {
            self.sdk = self.sdk.mcp_manager(manager);
        }
        self.mcp_manager = manager;
        self
    }

    pub(super) fn isolated_session(&self, allowlist: &[(String, String)]) -> Self {
        let sdk = self.sdk.isolated_session();
        sdk.sync_session_permissions(allowlist);
        Self {
            sdk,
            ..self.clone()
        }
    }
}

#[derive(Clone)]
pub(crate) struct AgentRuntimeFactory {
    defaults: RuntimeResources,
}

impl AgentRuntimeFactory {
    pub(crate) fn validate_hook_trust(
        working_dir: &std::path::Path,
        trust: Option<crate::config::HookTrust>,
        cli: &Cli,
        settings: Option<&Settings>,
        extensions: &ExtensionRuntime,
    ) -> anyhow::Result<()> {
        if !cli.print
            && trust.is_none()
            && (settings
                .and_then(|settings| settings.hooks.as_ref())
                .is_some_and(|hooks| !hooks.is_empty())
                || extensions.has_process_hooks())
        {
            anyhow::bail!(
                "workspace Hook trust is missing; start phi interactively in {} first",
                working_dir.display()
            );
        }
        Ok(())
    }

    pub(crate) fn new(defaults: RuntimeResources) -> Self {
        Self { defaults }
    }

    pub(crate) fn extension_runtime_services(
        runtime_services: &PhiRuntimeServices,
        sandbox: Sandbox,
    ) -> phi_sdk::extension::ExtensionRuntimeServices {
        let sandboxed = sandbox.is_enabled();
        let runner = phi_sdk::extension::LocalExtensionProcessRunner::new(sandbox);
        runtime_services.extension_runtime_services(
            Arc::new(runner),
            phi_sdk::extension::ExtensionProcessPolicy::default()
                .trust_root(crate::config::settings::phi_path()),
            sandboxed,
        )
    }

    #[cfg(test)]
    pub(crate) fn resources_for(
        cli: &Cli,
        settings: &Settings,
        settings_manager: &crate::config::SettingsManager,
        catalog: Arc<phi_sdk::models::ModelCatalog>,
        working_dir: &std::path::Path,
    ) -> anyhow::Result<RuntimeResources> {
        Ok(
            Self::resource_defaults_for(cli, settings, settings_manager, catalog, working_dir)?
                .capture_extensions()?
                .isolated_session(&[]),
        )
    }

    #[cfg(test)]
    pub(super) fn resource_defaults_for(
        cli: &Cli,
        settings: &Settings,
        settings_manager: &crate::config::SettingsManager,
        catalog: Arc<phi_sdk::models::ModelCatalog>,
        working_dir: &std::path::Path,
    ) -> anyhow::Result<RuntimeResources> {
        let policy = settings_manager.resource_policy_for(working_dir)?;
        Self::resource_defaults_with_policy(cli, settings, catalog, working_dir, policy)
    }

    pub(super) fn resource_defaults_with_policy(
        cli: &Cli,
        settings: &Settings,
        catalog: Arc<phi_sdk::models::ModelCatalog>,
        working_dir: &std::path::Path,
        policy: Arc<crate::resources::ResourcePolicy>,
    ) -> anyhow::Result<RuntimeResources> {
        let sandbox = crate::security::execution_sandbox(cli, settings, working_dir)?;
        let services = Self::permission_services(cli, settings, working_dir)?
            .interactive(!cli.print)
            .model_catalog(catalog.clone());
        let extension_paths = policy.extension_paths();
        Ok(RuntimeResources::with_policy(
            services,
            Arc::new(ExtensionRuntime::load()),
            catalog,
            sandbox,
            policy,
            extension_paths,
        ))
    }

    pub(crate) fn mcp_manager(
        cli: &Cli,
        settings: &Settings,
        sandbox: Sandbox,
    ) -> Option<phi_sdk::extension::mcp::McpClientManager> {
        if !settings.mcp_enabled() {
            return None;
        }
        let servers = settings.mcp_servers.as_ref()?;
        Some(
            phi_sdk::extension::mcp::McpClientManager::load_with_settings(
                servers,
                settings.mcp_settings.clone().unwrap_or_default(),
            )
            .with_execution_environment(sandbox, crate::security::mcp_transport_policy(cli)),
        )
    }

    pub(crate) fn permission_services(
        cli: &Cli,
        settings: &Settings,
        working_dir: &std::path::Path,
    ) -> anyhow::Result<PhiRuntimeServices> {
        let mut permission_config = settings
            .permission
            .clone()
            .map(PermissionConfig::try_from_value)
            .transpose()?
            .unwrap_or_default();
        permission_config.mode = crate::resolve_mode(cli, &permission_config);
        Ok(PhiRuntimeServices::new()
            .permission_policy(permission_config, Some(working_dir.to_path_buf()))
            .interactive(true))
    }

    pub(crate) async fn prepare(
        &self,
        options: RuntimeRebuildOptions<'_>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        self.prepare_with_mode(options, ExtensionLoadMode::Strict)
            .await
    }

    pub(super) async fn prepare_with_resources(
        options: RuntimeRebuildOptions<'_>,
        resources: RuntimeResources,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let resources = resources
            .capture_extensions()?
            .isolated_session(&options.instance.permission_allowlist);
        Box::pin(Self::prepare_with_prompts(options, resources, None)).await
    }

    pub(super) async fn prepare_initial(
        &self,
        options: RuntimeRebuildOptions<'_>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        self.prepare_with_mode(options, ExtensionLoadMode::Initial)
            .await
    }

    async fn prepare_with_mode(
        &self,
        options: RuntimeRebuildOptions<'_>,
        mode: ExtensionLoadMode,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let resources = self.session_resources(&options, mode)?;
        Box::pin(Self::prepare_with_prompts(options, resources, None)).await
    }

    fn session_resources(
        &self,
        options: &RuntimeRebuildOptions<'_>,
        mode: ExtensionLoadMode,
    ) -> anyhow::Result<RuntimeResources> {
        let defaults = match &options.instance.resource_settings {
            Some(settings) => {
                let policy = match &options.instance.resource_policy {
                    Some(policy) => policy.clone(),
                    None => options
                        .instance
                        .settings_manager
                        .as_ref()
                        .ok_or_else(|| {
                            anyhow::anyhow!("resource loading requires a settings manager")
                        })?
                        .resource_policy_for(&options.instance.working_dir)?,
                };
                Self::resource_defaults_with_policy(
                    options.phi.cli,
                    settings,
                    self.defaults.model_catalog.clone(),
                    &options.instance.working_dir,
                    policy,
                )?
            }
            None => self.defaults.clone(),
        };
        let defaults = if matches!(mode, ExtensionLoadMode::Initial)
            && defaults.extension_paths == self.defaults.extension_paths
        {
            RuntimeResources {
                extension_sources: self.defaults.extension_sources.clone(),
                extension_load_failed: self.defaults.extension_load_failed,
                ..defaults
            }
        } else {
            defaults.capture_extensions()?
        };
        let resources = defaults.isolated_session(&options.instance.permission_allowlist);
        let resources = {
            let manager = self.defaults.mcp_manager.clone().map(|manager| {
                manager.with_execution_environment(
                    resources.sandbox.clone(),
                    crate::security::mcp_transport_policy(options.phi.cli),
                )
            });
            resources.mcp_manager(manager)
        };
        Ok(resources)
    }

    async fn prepare_with_prompts(
        mut options: RuntimeRebuildOptions<'_>,
        resources: RuntimeResources,
        frozen_prompts: Option<Arc<crate::resources::prompts::PromptResources>>,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let mut resources = resources;
        resources.model_catalog = Arc::new(resources.model_catalog.fork());
        resources.sdk = resources.sdk.model_catalog(resources.model_catalog.clone());
        let mut runtime_config = options.phi.runtime_cfg;
        let no_tools =
            options.phi.agent_options.no_tools || runtime_config.no_tools.unwrap_or(false);
        runtime_config.no_tools = Some(no_tools);
        runtime_config.max_tokens = options
            .phi
            .agent_options
            .max_tokens
            .or(runtime_config.max_tokens);
        runtime_config.temperature = options
            .phi
            .agent_options
            .temperature
            .or(runtime_config.temperature);
        if let Some(codemode) = &options.phi.agent_options.codemode {
            runtime_config.extensions.codemode = codemode.clone();
        }
        runtime_config.extensions = runtime_config.extensions.resolve();
        options.phi.runtime_cfg = runtime_config;
        if !options
            .phi
            .runtime_cfg
            .extensions
            .is_enabled(phi_sdk::extension::BuiltinExtension::Mcp)
        {
            resources.mcp_manager = None;
        }
        let mcp_intent = Arc::new(McpRebuildIntent {
            cli: Arc::new(options.phi.cli.clone()),
            runtime_cfg: options.phi.runtime_cfg.clone(),
            context: options.phi.context.clone(),
            workflow: options.phi.agent_options.workflow,
            hooks: options.phi.hooks.clone(),
            subagent_preparation: options.instance.subagent_preparation.clone(),
        });
        let mcp_contracts = if no_tools {
            None
        } else if let Some(manager) = resources.mcp_manager.clone() {
            let (snapshot, contracts) = manager.snapshot_tool_contracts().await;
            resources.mcp_manager = Some(manager);
            resources.sdk = resources.sdk.mcp_manager(snapshot);
            Some(contracts)
        } else {
            None
        };
        let memory = options
            .phi
            .runtime_cfg
            .extensions
            .memory
            .clone()
            .unwrap_or_default();
        let validate_tool_rule = |_rule: &phi_sdk::tools::ToolRule| {
            if let Some(contracts) = &mcp_contracts {
                return contracts.validate_tool_rule(_rule);
            }
            Ok(())
        };
        options
            .phi
            .context
            .validate_agent_tool_declarations(&validate_tool_rule)?;
        let prompts = Arc::new(match frozen_prompts {
            Some(prompts) => prompts.revalidated(&validate_tool_rule),
            None => crate::resources::prompts::PromptResources::load_with_validation(
                &resources.resource_policy,
                &phi_sdk::extension::builtin_skills(&options.phi.runtime_cfg),
                &validate_tool_rule,
            ),
        });
        let mut prompt_hooks = Vec::new();
        if !options.instance.delegated_child {
            prompt_hooks.extend(prompts.system_prompt_hook());
        }
        prompt_hooks.extend(options.phi.hooks);
        let background_startup = !options.phi.cli.print && options.phi.cli.command.is_none();
        let hooks = phi_sdk::extension::memory::MemoryHooks::new(
            phi_sdk::extension::memory::settings_from_config(&memory),
        );
        let hooks = if background_startup {
            hooks.background_startup()
        } else {
            hooks
        };
        let hooks = if options.instance.delegated_child {
            hooks.prompt_only()
        } else {
            hooks
        };
        let agent_diagnostics = options
            .phi
            .context
            .agent_diagnostics()
            .iter()
            .map(|diagnostic| {
                crate::resources::format_diagnostic(
                    "Agent conflicts",
                    &diagnostic.path,
                    &diagnostic.message,
                )
            })
            .collect();
        let compact_enabled = options.phi.runtime_cfg.resolve_compact_enabled();
        let background_required = options.phi.runtime_cfg.agent_background;
        let thinking_level = options.phi.runtime_cfg.default_thinking_level.clone();
        let supported_thinking_levels = resources
            .model_catalog
            .get(&options.phi.provider, &options.phi.model)
            .map(|descriptor| descriptor.supported_thinking_levels())
            .unwrap_or_default();
        let source_settings = options.instance.resource_settings.clone();
        let settings_manager = options.instance.settings_manager.clone();
        let hook_trust = settings_manager
            .as_ref()
            .map(|manager| manager.hook_workspace_trust(&options.instance.working_dir))
            .transpose()?
            .flatten();
        let hook_policy = match hook_trust {
            Some(crate::config::HookTrust::Disabled) => {
                phi_sdk::extension::ExtensionHookPolicy::Disabled
            }
            _ => phi_sdk::extension::ExtensionHookPolicy::Enabled,
        };
        // Follow Claude Code's interactive-only teammate policy; named children stay subagents.
        // https://code.claude.com/docs/en/agent-teams#enable-agent-teams
        let team_capability =
            if options.phi.cli.print || options.phi.cli.rpc || options.phi.cli.command.is_some() {
                phi_sdk::agent_manager::TeamCapability::Disabled
            } else {
                options
                    .instance
                    .resource_settings
                    .as_ref()
                    .and_then(|settings| settings.agent.team)
                    .map(|enabled| {
                        if enabled {
                            phi_sdk::agent_manager::TeamCapability::Enabled
                        } else {
                            phi_sdk::agent_manager::TeamCapability::Disabled
                        }
                    })
                    .unwrap_or(options.instance.team_capability)
            };
        let generation_id = uuid::Uuid::new_v4().to_string();
        let workflow_tool = super::session::WorkflowTool::default();
        let workflow_enabled =
            options.phi.agent_options.workflow && !no_tools && options.phi.cli.command.is_none();
        let workflow_requested = workflow_enabled && options.phi.cli.workflow;
        let tools = workflow_enabled.then(|| workflow_tool.declaration(generation_id.clone()));
        let mut builder = Phi::builder()
            .provider(options.phi.provider.to_string())
            .model(options.phi.model.to_string())
            .runtime_config(options.phi.runtime_cfg)
            .context(options.phi.context)
            .hooks(prompt_hooks)
            .prompt_expander(prompts.clone())
            .command_hooks(
                source_settings
                    .as_ref()
                    .and_then(|settings| settings.hooks.as_ref())
                    .filter(|_| hook_trust != Some(crate::config::HookTrust::Disabled))
                    .map(|hooks| hooks.commands.clone())
                    .unwrap_or_default(),
            )
            .tools(tools)
            .extension_sources(
                resources.extension_sources.clone(),
                Self::extension_runtime_services(&resources.sdk, resources.sandbox.clone())
                    .hook_policy(hook_policy),
            )
            .memory_extension(hooks)
            .tool_display_formatter(tool_display_formatter);
        if let Some(api_key) = &options.phi.cli.api_key {
            builder = builder.api_key(api_key.clone());
        }
        let phi = builder
            .build()?
            .with_runtime_services(resources.sdk.clone());
        let working_dir = options.instance.working_dir.clone();
        let instance = Self::prepare_instance(&phi, options.instance, team_capability).await?;
        let sdk_resources = instance.resources();
        resources.extensions = sdk_resources
            .extensions()
            .ok_or_else(|| anyhow::anyhow!("prepared extension runtime is missing"))?;
        Self::validate_hook_trust(
            &working_dir,
            hook_trust,
            &options.phi.cli,
            source_settings.as_deref(),
            &resources.extensions,
        )?;
        let runtime = AgentRuntime {
            generation_id,
            workflow_tool,
            workflow_requested,
            workflow_enabled,
            phi,
            sdk_resources,
            resources,
            memory,
            prompts,
            thinking_level,
            supported_thinking_levels,
            agent_diagnostics,
            compact_enabled,
            background_required,
            team_capability,
            mcp_intent,
            source_settings,
            settings_manager,
            mcp_contracts,
        };
        Ok(PreparedAgentRuntime { instance, runtime })
    }

    async fn prepare_instance(
        phi: &Phi,
        options: AgentInstanceBuildOptions,
        team_capability: phi_sdk::agent_manager::TeamCapability,
    ) -> anyhow::Result<AgentInstanceCandidate> {
        let task_list = options.resolve_task_list(team_capability)?;
        let mut builder = phi
            .agent_instance_builder()
            .team_capability(team_capability)
            .working_dir(options.working_dir)
            .transcript(options.transcript);
        if let Some(binding) = task_list {
            builder = builder.task_list(binding);
        }
        if options.delegated_child {
            builder = builder.delegated_child();
        }
        if let Some(preparation) = options.subagent_preparation {
            builder = builder.subagent_preparation(preparation);
        }
        Ok(builder.build().await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::agent_manager::TeamCapability::{Disabled, Enabled};

    #[test]
    fn task_list_selection_preserves_saved_explicit_and_inherited_scope() {
        let session =
            crate::session::new("unused", "unused", 1_000, &std::env::current_dir().unwrap());
        for capability in [Disabled, Enabled] {
            let mut options = AgentInstanceBuildOptions::from_session(&session);
            let expected = if capability == Enabled {
                phi_sdk::agent_manager::team_namespace(session.id.as_str()).unwrap()
            } else {
                session.id.to_string()
            };
            assert_eq!(
                options
                    .resolve_task_list(capability)
                    .unwrap()
                    .unwrap()
                    .list_id(),
                expected
            );
            options.task_list_id = Some("saved".into());
            assert_eq!(
                options
                    .resolve_task_list(capability)
                    .unwrap()
                    .unwrap()
                    .list_id(),
                "saved"
            );
            let binding =
                phi_sdk::tasks::TaskListBinding::new("/tmp/phi-explicit-tasks", "explicit")
                    .unwrap();
            options.task_list = Some(Some(binding.clone()));
            assert!(options.resolve_task_list(capability).is_err());
            options.task_list_id = None;
            assert_eq!(
                options.resolve_task_list(capability).unwrap(),
                Some(binding.clone())
            );
            options.task_list = Some(None);
            assert!(options.resolve_task_list(capability).unwrap().is_none());
            let child = AgentInstanceBuildOptions::for_child(&session, Some(binding.clone()));
            assert_eq!(child.resolve_task_list(capability).unwrap(), Some(binding));
            let child = AgentInstanceBuildOptions::for_child(&session, None);
            assert!(child.resolve_task_list(capability).unwrap().is_none());
        }
    }
}
