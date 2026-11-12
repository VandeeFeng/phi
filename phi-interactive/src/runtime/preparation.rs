use std::collections::HashSet;
use std::sync::Arc;

use phi_sdk::agent_instance::AgentInstanceId;
use phi_sdk::context::ContextFiles;
use phi_sdk::hooks::Hook;
use phi_sdk::lifecycle::{SessionEndReason, SessionStartReason};
use phi_sdk::models::ModelCatalog;
use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::config::Settings;
use crate::runtime::session::{AgentSession, RuntimeView};
use crate::runtime::{
    AgentInstanceBuildOptions, AgentRuntimeFactory, PhiBuildOptions, PreparedAgentRuntime,
    RuntimePreparation, RuntimeRebuildOptions, RuntimeResources, SessionAccess,
};
use crate::session::Session;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum RuntimeSelection {
    Model {
        selection: String,
    },
    Switch {
        name: String,
    },
    ThinkingLevel {
        level: phi_sdk::models::ThinkingLevel,
    },
}

/// Same-Session branch selection; never changes Agent identity or emits SessionStart/End.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub(crate) enum BranchNavigation {
    Rewind { entry_id: String },
    Entry { entry_id: String },
}

#[derive(Debug, Clone)]
pub(crate) struct CreateSessionRequest {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub switch: Option<String>,
    pub persist: bool,
}

pub(crate) struct AcceptedTransition {
    pub(crate) attachment: crate::runtime::attachment::AgentAttachment,
    pub(crate) accepted: super::session::RuntimeView,
    pub(crate) navigation_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpServerToolSelection {
    pub name: String,
    pub tools: Vec<McpToolSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpToolSelection {
    pub name: String,
    pub exposure: phi_sdk::tools::ToolExposure,
    pub was_exposure: phi_sdk::tools::ToolExposure,
}

#[derive(Clone, PartialEq, Eq)]
struct EffectiveSwitch {
    switch: String,
    provider: String,
    model: String,
    thinking_level: Option<String>,
    mode: Option<String>,
    prompt: Option<String>,
}

pub(super) struct RuntimeConfiguration {
    startup_dir: std::path::PathBuf,
    generation: std::sync::RwLock<ConfigurationGeneration>,
    reload_lock: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Clone)]
struct ConfigurationGeneration {
    cli: Arc<Cli>,
    settings: Settings,
    settings_manager: crate::config::SettingsManager,
    resource_policy: Option<Arc<crate::resources::ResourcePolicy>>,
    hooks: Vec<Hook>,
    model_catalog: Arc<ModelCatalog>,
}

#[derive(Debug)]
pub(crate) struct ModelChoice {
    pub provider: String,
    pub model: String,
    pub complete: bool,
}

pub(crate) struct RuntimeCatalog {
    pub(crate) models: Vec<ModelChoice>,
    pub(crate) switches: Vec<String>,
}

#[derive(Clone)]
pub(crate) struct ModelChoices {
    picker_seed: Arc<crate::config::SwitchSettings>,
    model_picker_config: Arc<phi_sdk::config::RuntimeConfig>,
    configuration: Arc<RuntimeConfiguration>,
}

#[derive(Clone)]
pub(crate) struct SessionWorkflows {
    sessions: Arc<super::associations::SessionAssociations>,
    preparation: Arc<super::RuntimePreparation>,
    configuration: Arc<RuntimeConfiguration>,
}

pub(crate) fn resolve_startup(
    cli: &Cli,
    settings: &Settings,
    session: &mut Session,
    catalog: &ModelCatalog,
) -> anyhow::Result<phi_sdk::config::RuntimeConfig> {
    let runtime = EffectiveSwitch::startup(settings, cli);
    runtime.apply_validated(session, settings, catalog)?;
    Ok(runtime.runtime_config(settings, catalog))
}

impl EffectiveSwitch {
    fn from_runtime(session: &Session, runtime: &phi_sdk::config::RuntimeConfig) -> Self {
        Self {
            switch: session.current_switch.to_string(),
            provider: session.provider.to_string(),
            model: session.model.to_string(),
            thinking_level: runtime.default_thinking_level.clone(),
            mode: Some(runtime.agent_mode.as_str().into()),
            prompt: runtime.mode_prompt_override.clone(),
        }
    }

    fn startup(settings: &Settings, cli: &Cli) -> Self {
        let selected = settings
            .switch_by_name(&crate::session::default_switch())
            .unwrap_or_else(|| settings.default_switch_config());
        let mut runtime = Self::from_switch(
            selected,
            &cli.resolve_provider(settings),
            &cli.resolve_model(settings),
            settings,
        );
        if let Some(provider) = &cli.provider {
            runtime.provider = provider.clone();
        }
        if let Some(model) = &cli.model {
            runtime.model = model.clone();
        }
        runtime
    }

    fn for_session_switch(session: &Session, settings: &Settings) -> Self {
        let selected = settings
            .switch_by_name(&session.current_switch)
            .unwrap_or_else(|| settings.default_switch_config());
        Self::from_switch(selected, &session.provider, &session.model, settings)
    }

    fn from_switch(
        selected: crate::config::SwitchConfig,
        fallback_provider: &str,
        fallback_model: &str,
        settings: &Settings,
    ) -> Self {
        Self {
            switch: selected.name,
            provider: selected.provider.unwrap_or_else(|| {
                settings
                    .default_provider()
                    .unwrap_or(fallback_provider)
                    .into()
            }),
            model: selected
                .model
                .unwrap_or_else(|| settings.default_model().unwrap_or(fallback_model).into()),
            thinking_level: selected
                .thinking_level
                .or_else(|| settings.default_thinking_level.clone()),
            mode: selected.mode,
            prompt: selected.prompt,
        }
    }

    fn select(
        mut self,
        selection: RuntimeSelection,
        session: &Session,
        settings: &Settings,
    ) -> anyhow::Result<Self> {
        match selection {
            RuntimeSelection::Switch { name } => {
                let selected = settings
                    .switch_by_name_or_alias(&name)
                    .ok_or_else(|| anyhow::anyhow!("unknown switch `{name}`"))?;
                Ok(Self::from_switch(
                    selected,
                    &session.provider,
                    &session.model,
                    settings,
                ))
            }
            RuntimeSelection::Model { selection } => {
                if let Some((provider, model)) = selection
                    .strip_prefix("model:")
                    .and_then(|value| value.split_once(':'))
                {
                    self.provider = provider.into();
                    self.model = model.into();
                } else {
                    self.model = selection;
                }
                Ok(self)
            }
            RuntimeSelection::ThinkingLevel { level } => {
                self.thinking_level = Some(level.as_str().into());
                Ok(self)
            }
        }
    }

    fn runtime_config(
        &self,
        settings: &Settings,
        catalog: &ModelCatalog,
    ) -> phi_sdk::config::RuntimeConfig {
        let mut resolved = settings.runtime_config(catalog);
        self.apply_to_runtime(&mut resolved);
        resolved
    }

    fn apply_to_runtime(&self, runtime: &mut phi_sdk::config::RuntimeConfig) {
        runtime.switch = Some(self.switch.clone());
        runtime.default_thinking_level = self.thinking_level.clone();
        runtime.agent_mode = phi_sdk::config::AgentMode::from_name(self.mode.as_deref());
        runtime.mode_prompt_override = self.prompt.clone();
    }

    fn apply_to_session(&self, session: &mut Session) {
        session.current_switch = self.switch.as_str().into();
        session.provider = self.provider.as_str().into();
        session.model = self.model.as_str().into();
    }

    fn apply_validated(
        &self,
        session: &mut Session,
        settings: &Settings,
        catalog: &ModelCatalog,
    ) -> anyhow::Result<()> {
        let context_window =
            settings.resolve_model_context_window(catalog, &self.provider, &self.model)?;
        self.apply_to_session(session);
        session.context_window = context_window;
        Ok(())
    }
}

impl RuntimeConfiguration {
    fn new(
        startup_dir: &std::path::Path,
        cli: Arc<Cli>,
        settings: Settings,
        settings_manager: crate::config::SettingsManager,
        hooks: Vec<Hook>,
        model_catalog: Arc<ModelCatalog>,
    ) -> Self {
        Self {
            startup_dir: startup_dir.to_path_buf(),
            generation: std::sync::RwLock::new(ConfigurationGeneration {
                cli,
                settings,
                settings_manager,
                resource_policy: None,
                hooks,
                model_catalog,
            }),
            reload_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn snapshot(&self) -> ConfigurationGeneration {
        self.generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn publish_configuration(&self, configuration: ConfigurationGeneration) {
        *self
            .generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = configuration;
    }

    async fn prepare_resources_from(
        &self,
        settings: Settings,
        resource_policy: Arc<crate::resources::ResourcePolicy>,
        working_dir: &std::path::Path,
        settings_manager: crate::config::SettingsManager,
    ) -> anyhow::Result<(ConfigurationGeneration, RuntimeResources)> {
        let current = self.snapshot();
        let offline = crate::config::settings::offline_mode()?;
        let catalog = Arc::new(ModelCatalog::load(
            crate::config::settings::model_catalog_options(offline),
        )?);
        let resources = AgentRuntimeFactory::resource_defaults_with_policy(
            &current.cli,
            &settings,
            catalog.clone(),
            working_dir,
            resource_policy.clone(),
        )?;
        if catalog.requires_bootstrap() {
            catalog.bootstrap_default().await?;
        }
        let manager =
            AgentRuntimeFactory::mcp_manager(&current.cli, &settings, resources.sandbox.clone());
        if let Some(manager) = &manager {
            manager.start().await;
        }
        let resources = resources.mcp_manager(manager);
        let hooks = Vec::new();
        let policy = ConfigurationGeneration {
            cli: current.cli,
            settings,
            settings_manager,
            resource_policy: Some(resource_policy),
            hooks,
            model_catalog: catalog,
        };
        Ok((policy, resources))
    }
}

impl ConfigurationGeneration {
    fn bind_settings(mut self, working_dir: &std::path::Path) -> anyhow::Result<Self> {
        if !self.settings_manager.matches_scope(working_dir) {
            self.settings_manager = crate::config::SettingsManager::load_for(working_dir)?;
        }
        self.resource_policy = Some(self.settings_manager.resource_policy_for(working_dir)?);
        Ok(self)
    }

    fn load_for_session(
        mut self,
        session: &Session,
        manager: &crate::config::SettingsManager,
    ) -> anyhow::Result<Self> {
        let working_dir = std::path::Path::new(session.working_dir.as_str());
        let settings_manager = if manager.matches_scope(working_dir) {
            manager.reload()?;
            manager.clone()
        } else {
            crate::config::SettingsManager::load_for(working_dir)?
        };
        let (settings, policy) = settings_manager.candidate_for(working_dir)?;
        self.settings = settings;
        self.settings_manager = settings_manager;
        self.resource_policy = Some(policy);
        Ok(self)
    }

    async fn prepare_resources_for(
        &self,
        working_dir: &std::path::Path,
    ) -> anyhow::Result<RuntimeResources> {
        let resources = AgentRuntimeFactory::resource_defaults_with_policy(
            &self.cli,
            &self.settings,
            self.model_catalog.clone(),
            working_dir,
            self.settings_manager.resource_policy_for(working_dir)?,
        )?;
        let manager =
            AgentRuntimeFactory::mcp_manager(&self.cli, &self.settings, resources.sandbox.clone());
        if let Some(manager) = &manager {
            manager.start().await;
        }
        Ok(resources.mcp_manager(manager))
    }

    fn new_session(&self, current: &Session, runtime: &EffectiveSwitch) -> anyhow::Result<Session> {
        let mut session = crate::session::new(
            &runtime.provider,
            &runtime.model,
            current.context_window,
            std::path::Path::new(current.working_dir.as_str()),
        );
        runtime.apply_to_session(&mut session);
        Ok(session)
    }

    fn runtime_build_options<'a>(&'a self, session: &'a Session) -> RuntimeRebuildOptions<'a> {
        self.build_options_for(
            session,
            &EffectiveSwitch::for_session_switch(session, &self.settings),
        )
    }

    fn build_options_for<'a>(
        &'a self,
        session: &'a Session,
        runtime: &EffectiveSwitch,
    ) -> RuntimeRebuildOptions<'a> {
        let mut instance = AgentInstanceBuildOptions::from_session(session);
        instance.settings_manager = Some(self.settings_manager.clone());
        instance.resource_settings = Some(Arc::new(self.settings.clone()));
        instance.resource_policy = self.resource_policy.clone();
        RuntimeRebuildOptions {
            phi: PhiBuildOptions {
                cli: &self.cli,
                provider: &session.provider,
                model: &session.model,
                runtime_cfg: runtime.runtime_config(&self.settings, &self.model_catalog),
                context: self.context_for(std::path::Path::new(session.working_dir.as_str())),
                agent_options: crate::agent_options_from_cli(&self.cli, &self.settings),
                hooks: self.hooks.clone(),
            },
            instance,
        }
    }

    fn context_for(&self, working_dir: &std::path::Path) -> ContextFiles {
        phi_sdk::context::load_with_options_for(
            self.cli.resolve_no_context_files(&self.settings),
            Some(crate::config::settings::global_agents_path()),
            crate::resources::resource_roots_for(working_dir, "agents")
                .into_iter()
                .map(|(_, path)| path)
                .collect(),
            working_dir,
        )
    }
}

impl ModelChoices {
    fn new(settings: &Settings, configuration: Arc<RuntimeConfiguration>) -> Self {
        Self {
            picker_seed: Arc::new(crate::config::SwitchSettings::from(settings)),
            model_picker_config: Arc::new(phi_sdk::config::RuntimeConfig {
                custom_providers: configuration
                    .snapshot()
                    .model_catalog
                    .custom_providers()
                    .into_iter()
                    .collect(),
                ..Default::default()
            }),
            configuration,
        }
    }

    pub(crate) fn picker_seed(&self) -> crate::config::SwitchSettings {
        (*self.picker_seed).clone()
    }

    pub(crate) fn model_choices(
        &self,
        provider: &str,
        current_model: &str,
        default_model: Option<&str>,
    ) -> Vec<ModelChoice> {
        let catalog = self.configuration.snapshot().model_catalog;
        let config = &self.model_picker_config;
        let mut models = prioritized_model_ids(&catalog, provider, current_model, default_model)
            .into_iter()
            .map(|model| (provider.to_string(), model))
            .collect::<Vec<_>>();
        let providers = phi_sdk::models::available_providers(&catalog, config, provider);
        models.extend(phi_sdk::models::available_provider_model_ids(
            &catalog, config, provider,
        ));
        for provider in providers {
            models.extend(
                catalog
                    .discovered_model_ids(&provider)
                    .into_iter()
                    .map(|model| (provider.clone(), model)),
            );
        }
        unique_model_choices(&catalog, models)
    }

    pub(crate) fn switch_model_choices(
        &self,
        provider: &str,
        current_model: &str,
        default_model: Option<&str>,
    ) -> Vec<String> {
        prioritized_model_ids(
            &self.configuration.snapshot().model_catalog,
            provider,
            current_model,
            default_model,
        )
    }

    pub(crate) fn switch_provider_choices(&self, current_provider: &str) -> Vec<String> {
        phi_sdk::models::available_providers(
            &self.configuration.snapshot().model_catalog,
            &self.model_picker_config,
            current_provider,
        )
    }

    pub(crate) fn automatic_model_catalog_update_enabled(&self) -> bool {
        !self.configuration.snapshot().model_catalog.offline()
    }

    pub(crate) async fn update_model_catalog(
        &self,
        force: bool,
    ) -> anyhow::Result<phi_sdk::models::CatalogUpdateResult> {
        self.configuration
            .snapshot()
            .model_catalog
            .update_default(force)
            .await
    }
}

fn prioritized_model_ids(
    catalog: &ModelCatalog,
    provider: &str,
    current_model: &str,
    default_model: Option<&str>,
) -> Vec<String> {
    let mut models = catalog.model_ids(provider);
    if let Some(model) = default_model {
        models.insert(0, model.to_string());
    }
    if !current_model.is_empty() {
        models.insert(0, current_model.to_string());
    }
    let mut unique = Vec::new();
    for model in models {
        if !unique.contains(&model) {
            unique.push(model);
        }
    }
    unique
}

fn unique_model_choices(catalog: &ModelCatalog, models: Vec<(String, String)>) -> Vec<ModelChoice> {
    let mut choices: Vec<ModelChoice> = Vec::new();
    for (provider, model) in models {
        if choices
            .iter()
            .any(|choice| choice.provider == provider && choice.model == model)
        {
            continue;
        }
        choices.push(ModelChoice {
            complete: catalog.get(&provider, &model).is_some(),
            provider,
            model,
        });
    }
    choices
}

impl BranchNavigation {
    fn select(&self, session: &mut Session) -> anyhow::Result<bool> {
        let (entry_id, _) = self.target();
        let target = session
            .entries
            .iter()
            .find(|entry| entry.id == entry_id)
            .ok_or_else(|| anyhow::anyhow!("navigation point not found: {entry_id}"))?;
        let editor_text = matches!(self, Self::Rewind { .. })
            && matches!(
                target.kind,
                crate::session::TranscriptEntryKind::Message {
                    message: crate::session::AgentMessage::User { .. }
                }
            );
        let leaf = if editor_text {
            target.parent_id.clone()
        } else {
            Some(target.id.clone())
        };
        session.select_branch(leaf.as_deref())?;
        Ok(editor_text)
    }

    fn target(&self) -> (&str, phi_sdk::lifecycle::SessionTreeReason) {
        match self {
            Self::Rewind { entry_id } => (entry_id, phi_sdk::lifecycle::SessionTreeReason::Rewind),
            Self::Entry { entry_id } => {
                (entry_id, phi_sdk::lifecycle::SessionTreeReason::AnchorJump)
            }
        }
    }
}

impl RuntimePreparation {
    pub(crate) fn initialize(
        &self,
        startup_dir: &std::path::Path,
        cli: Arc<Cli>,
        settings: Settings,
        settings_manager: crate::config::SettingsManager,
        hooks: Vec<Hook>,
        model_catalog: Arc<ModelCatalog>,
    ) -> ModelChoices {
        let configuration = self
            .initialize_configuration(Arc::new(RuntimeConfiguration::new(
                startup_dir,
                cli,
                settings,
                settings_manager,
                hooks,
                model_catalog,
            )))
            .clone();
        let snapshot = configuration.snapshot();
        ModelChoices::new(&snapshot.settings, configuration)
    }

    async fn prepare_reload(
        &self,
        policy: &ConfigurationGeneration,
        session: &Session,
        runtime: &EffectiveSwitch,
        resources: RuntimeResources,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        AgentRuntimeFactory::prepare_with_resources(
            self.with_child_preparation(policy.build_options_for(session, runtime)),
            resources,
        )
        .await
    }

    pub(super) async fn runtime_catalog(
        &self,
        access: &SessionAccess,
    ) -> anyhow::Result<RuntimeCatalog> {
        let seed = access.binding()?.configuration_seed().await?;
        let choices = ModelChoices::new(&seed.source_settings, self.configuration()?.clone());
        let picker = choices.picker_seed();
        Ok(RuntimeCatalog {
            switches: picker.switch_names(),
            models: choices.model_choices(
                &seed.session.provider,
                &seed.session.model,
                picker.default_model(),
            ),
        })
    }

    pub(crate) async fn configure_runtime(
        &self,
        access: &SessionAccess,
        mut selection: RuntimeSelection,
    ) -> anyhow::Result<RuntimeView> {
        let agent = access.binding()?;
        let seed = agent.configuration_seed().await?;
        let catalog = match &selection {
            RuntimeSelection::Model { .. } => self.configuration()?.snapshot().model_catalog,
            RuntimeSelection::Switch { .. } | RuntimeSelection::ThinkingLevel { .. } => {
                seed.runtime.model_catalog()
            }
        };
        if let RuntimeSelection::ThinkingLevel { level } = &mut selection {
            let descriptor = catalog
                .get(&seed.session.provider, &seed.session.model)
                .ok_or_else(|| anyhow::anyhow!("current model descriptor is missing"))?;
            *level = descriptor.clamp_thinking_level(*level);
            if seed.runtime.thinking_level.as_deref() == Some(level.as_str()) {
                return Ok(agent.current_runtime_view().await);
            }
        }
        let mut session = seed.session;
        let mut runtime_config = seed.runtime.runtime_config();
        let runtime = EffectiveSwitch::from_runtime(&session, &runtime_config).select(
            selection,
            &session,
            &seed.source_settings,
        )?;
        runtime.apply_validated(&mut session, &seed.source_settings, &catalog)?;
        runtime.apply_to_runtime(&mut runtime_config);
        let prepared = seed
            .runtime
            .prepare_selection(&session, runtime_config, catalog)
            .await?;
        tokio::spawn(async move { agent.replace_runtime(prepared, session, None, true).await })
            .await?
    }

    /// Select an in-memory branch while retaining the accepted runtime environment.
    pub(crate) async fn navigate_branch(
        &self,
        access: &SessionAccess,
        navigation: BranchNavigation,
    ) -> anyhow::Result<RuntimeView> {
        let agent = access.binding()?;
        let seed = agent.configuration_seed().await?;
        let mut session = seed.session;
        let old_leaf = session.leaf_id.clone();
        let (entry_id, reason) = navigation.target();
        let editor_text = navigation.select(&mut session)?;
        let leaf = session.leaf_id.clone();
        let prepared = seed.runtime.prepare_replacement(&session).await?;
        let entry_id = entry_id.to_string();
        tokio::spawn(async move {
            let branch = phi_sdk::agent_instance::BranchSelection {
                leaf_id: leaf.as_ref().map(ToString::to_string),
            };
            let accepted = agent
                .replace_runtime(prepared, session, Some(branch), false)
                .await?;
            agent
                .extension_session_tree_changed(
                    reason,
                    &entry_id,
                    old_leaf.as_deref(),
                    accepted.session.leaf_id.as_deref(),
                    editor_text,
                )
                .await;
            Ok(accepted)
        })
        .await?
    }

    pub(crate) async fn reload_resources(
        self: &Arc<Self>,
        access: &SessionAccess,
    ) -> anyhow::Result<RuntimeView> {
        let configuration = self.configuration()?;
        let reload = configuration.reload_lock.clone().lock_owned().await;
        let agent = access.binding()?;
        let expected_generation = agent.runtime().generation_id().to_string();
        let seed = agent.configuration_seed().await?;
        let mut session = seed.session;
        let loaded = configuration
            .snapshot()
            .load_for_session(&session, seed.runtime.settings_manager()?)?;
        let resource_policy = loaded.resource_policy.clone().unwrap();
        let settings_manager = loaded.settings_manager;
        let settings = loaded.settings;
        let runtime = EffectiveSwitch::for_session_switch(&session, &settings);
        let (policy, resources) = configuration
            .prepare_resources_from(
                settings,
                resource_policy,
                std::path::Path::new(&session.working_dir),
                settings_manager,
            )
            .await?;
        runtime.apply_validated(&mut session, &policy.settings, &policy.model_catalog)?;
        let prepared = self
            .prepare_reload(&policy, &session, &runtime, resources)
            .await?;
        let preparation = self.clone();
        let configuration = configuration.clone();
        let access = access.clone();
        tokio::spawn(async move {
            let _reload = reload;
            let snapshot = preparation
                .reload(&access, session, prepared, Some(&expected_generation))
                .await?;
            configuration.publish_configuration(policy);
            Ok(snapshot)
        })
        .await?
    }

    pub(crate) async fn save_mcp_tools(
        &self,
        access: &SessionAccess,
        selections: Vec<McpServerToolSelection>,
    ) -> anyhow::Result<()> {
        let configuration = self.configuration()?;
        let policy = configuration.snapshot();
        let path = crate::config::settings::mcp_config_file_path();
        crate::security::ConfigAccess::for_cli(&policy.cli).ensure_writable(
            &path,
            crate::security::trusts_remote_files(&policy.settings),
        )?;
        let _reload = configuration.reload_lock.clone().lock_owned().await;
        let snapshot = access.binding()?.mcp_servers().await;
        let mut seen = HashSet::new();
        let mut updates = Vec::new();
        for selection in selections {
            anyhow::ensure!(
                seen.insert(selection.name.clone()),
                "duplicate MCP server selection"
            );
            let current = snapshot
                .iter()
                .find(|server| server.name == selection.name)
                .ok_or_else(|| anyhow::anyhow!("unknown MCP server"))?;
            anyhow::ensure!(
                current.metadata_source.is_some(),
                "MCP tool metadata is unavailable for `{}`; refresh before saving",
                selection.name
            );
            anyhow::ensure!(
                selection.tools.len() == current.tools.len(),
                "MCP tool catalog changed"
            );
            let mut selected_names = HashSet::new();
            for tool in &selection.tools {
                anyhow::ensure!(
                    selected_names.insert(&tool.name),
                    "duplicate MCP tool selection"
                );
                anyhow::ensure!(
                    current.tools.iter().any(|item| item.name == tool.name),
                    "MCP tool catalog changed"
                );
            }
            let tool_exposure = selection
                .tools
                .iter()
                .filter(|tool| tool.exposure != tool.was_exposure)
                .map(|tool| (tool.name.clone(), tool.exposure))
                .collect::<std::collections::HashMap<_, _>>();
            if tool_exposure.is_empty() {
                continue;
            }
            let update = crate::config::settings::McpServerToolUpdate {
                server_name: selection.name.clone(),
                tool_exposure,
            };
            updates.push(update);
        }
        anyhow::ensure!(!updates.is_empty(), "no MCP tool changes");
        crate::config::SettingsManager::update_file(&path, |document| {
            crate::config::settings::update_mcp_config(document, &updates)
        })
    }
}

impl SessionWorkflows {
    pub(crate) fn remote_host(&self, persist: bool) -> impl phi_remote::RemoteHost + use<> {
        super::gateway::GatewaySessions::new(
            self.sessions.clone(),
            self.clone(),
            self.preparation.clone(),
            persist,
        )
    }

    pub(crate) fn new(
        sessions: Arc<crate::runtime::SessionAssociations>,
        preparation: Arc<crate::runtime::RuntimePreparation>,
    ) -> anyhow::Result<Self> {
        let configuration = preparation.configuration()?.clone();
        Ok(Self {
            sessions,
            preparation,
            configuration,
        })
    }

    async fn load_saved_session(
        &self,
        locator: crate::runtime::SavedSessionLocator<'_>,
    ) -> anyhow::Result<(Session, Option<crate::session::storage::SessionLock>)> {
        use crate::session::storage::SavedSessionPath;

        let path = match locator {
            crate::runtime::SavedSessionLocator::Id(id) => SavedSessionPath::resolve_id(id)?,
            crate::runtime::SavedSessionLocator::IdOrPath(locator) => {
                SavedSessionPath::resolve(locator)?
            }
        };
        if let Some(agent) = self
            .sessions
            .open_existing(&AgentInstanceId::new(path.id()))?
        {
            let session = agent.recovery().await?.session;
            path.validate(&session)?;
            return Ok((session, None));
        }
        let (session, lock) = path.open()?;
        Ok((session, Some(lock)))
    }

    pub(crate) async fn start_saved_session(
        &self,
        id: &str,
        persist: bool,
    ) -> anyhow::Result<Arc<crate::runtime::session::AgentSession>> {
        self.prepare_saved_session(id, persist).await?.start().await
    }

    pub(super) async fn prepare_saved_session(
        &self,
        id: &str,
        persist: bool,
    ) -> anyhow::Result<super::associations::SessionCreation> {
        let (mut session, lock) =
            crate::session::storage::SavedSessionPath::resolve_id(id)?.open()?;
        let policy = self.configuration.snapshot();
        let source = policy.settings_manager.clone();
        let policy = policy.load_for_session(&session, &source)?;
        let mut runtime = EffectiveSwitch::for_session_switch(&session, &policy.settings);
        runtime.provider = policy.cli.provider.clone().unwrap_or(runtime.provider);
        runtime.model = policy.cli.model.clone().unwrap_or(runtime.model);
        runtime.apply_validated(&mut session, &policy.settings, &policy.model_catalog)?;
        let resources = policy
            .prepare_resources_for(std::path::Path::new(&session.working_dir))
            .await?;
        let options = policy.build_options_for(&session, &runtime);
        let prepared = AgentRuntimeFactory::prepare_with_resources(
            self.preparation.with_child_preparation(options),
            resources,
        )
        .await?;
        self.sessions.prepared_creation(
            session,
            prepared,
            SessionStartReason::Startup,
            persist,
            Some(lock),
        )
    }

    pub(crate) async fn create_session(
        &self,
        params: CreateSessionRequest,
    ) -> anyhow::Result<Arc<crate::runtime::session::AgentSession>> {
        self.prepare_new_session(params).await?.start().await
    }

    pub(super) async fn prepare_new_session(
        &self,
        params: CreateSessionRequest,
    ) -> anyhow::Result<super::associations::SessionCreation> {
        self.prepare_new_session_in(params, None).await
    }

    pub(super) async fn prepare_new_session_in(
        &self,
        params: CreateSessionRequest,
        working_dir: Option<std::path::PathBuf>,
    ) -> anyhow::Result<super::associations::SessionCreation> {
        let mut policy = self.configuration.snapshot();
        let target_dir = working_dir
            .map(|directory| {
                anyhow::ensure!(
                    !directory.as_os_str().is_empty(),
                    "working directory is empty"
                );
                let directory = phi_sdk::workspace::expand_home_path(directory.to_string_lossy())
                    .canonicalize()?;
                anyhow::ensure!(directory.is_dir(), "working directory is not a directory");
                Ok::<_, anyhow::Error>(directory)
            })
            .transpose()?;
        if let Some(directory) = &target_dir {
            policy = policy.bind_settings(directory)?;
            policy.settings = policy.settings_manager.settings();
        }
        let mut session = crate::session::new(
            &policy.cli.resolve_provider(&policy.settings),
            &policy.cli.resolve_model(&policy.settings),
            policy.settings.context_window.unwrap_or(128_000),
            target_dir
                .as_deref()
                .unwrap_or(&self.configuration.startup_dir),
        );
        let previous = EffectiveSwitch::for_session_switch(&session, &policy.settings);
        let mut runtime = match params.switch {
            Some(name) => previous.select(
                RuntimeSelection::Switch { name },
                &session,
                &policy.settings,
            )?,
            None => EffectiveSwitch::startup(&policy.settings, &policy.cli),
        };
        if let Some(provider) = params.provider {
            runtime.provider = provider;
        }
        if let Some(model) = params.model {
            runtime.model = model;
        }
        runtime.apply_validated(&mut session, &policy.settings, &policy.model_catalog)?;
        let policy = policy.bind_settings(std::path::Path::new(&session.working_dir))?;
        let options = policy.build_options_for(&session, &runtime);
        if let Some(directory) = &target_dir {
            let resources = policy.prepare_resources_for(directory).await?;
            let prepared = AgentRuntimeFactory::prepare_with_resources(
                self.preparation.with_child_preparation(options),
                resources,
            )
            .await?;
            return self.sessions.prepared_creation(
                session,
                prepared,
                SessionStartReason::Startup,
                params.persist,
                None,
            );
        }
        self.sessions
            .prepare_creation(
                &self.preparation,
                session.clone(),
                options,
                SessionStartReason::Startup,
                params.persist,
                None,
            )
            .await
    }

    #[cfg(test)]
    pub(crate) async fn start_session(
        &self,
        session: Session,
        persist: bool,
    ) -> anyhow::Result<Arc<crate::runtime::session::AgentSession>> {
        self.start_with_lock(session, persist, None).await
    }

    pub(crate) async fn start_with_lock(
        &self,
        session: Session,
        persist: bool,
        lock: Option<crate::session::storage::SessionLock>,
    ) -> anyhow::Result<Arc<crate::runtime::session::AgentSession>> {
        self.prepare_session(session, persist, lock)
            .await?
            .start()
            .await
    }

    pub(super) async fn prepare_session(
        &self,
        session: Session,
        persist: bool,
        lock: Option<crate::session::storage::SessionLock>,
    ) -> anyhow::Result<super::associations::SessionCreation> {
        let policy = self
            .configuration
            .snapshot()
            .bind_settings(std::path::Path::new(&session.working_dir))?;
        let options = policy.runtime_build_options(&session);
        self.sessions
            .prepare_creation(
                &self.preparation,
                session.clone(),
                options,
                SessionStartReason::Startup,
                persist,
                lock,
            )
            .await
    }

    async fn prepare_new_runtime(
        &self,
        current_id: &AgentInstanceId,
    ) -> anyhow::Result<(Session, PreparedAgentRuntime)> {
        let mut policy = self.configuration.snapshot();
        let current_agent = self.sessions.get(current_id)?;
        let seed = current_agent.configuration_seed().await?;
        let current = seed.session;
        policy.settings = (*seed.source_settings).clone();
        policy.settings_manager = seed.runtime.settings_manager()?.clone();
        policy.resource_policy = Some(seed.runtime.resource_policy());
        policy.model_catalog = seed.runtime.model_catalog();
        let accepted_config = seed.runtime.runtime_config();
        let runtime = EffectiveSwitch::from_runtime(&current, &accepted_config);
        let target = policy.new_session(&current, &runtime)?;
        let mut options = policy.build_options_for(&target, &runtime);
        options.phi.runtime_cfg.custom_providers = accepted_config.custom_providers;
        let prepared = AgentRuntimeFactory::new(seed.runtime.resources.clone())
            .prepare(self.preparation.with_child_preparation(options))
            .await?;
        Ok((target, prepared))
    }

    pub(super) async fn prepare_new_from(
        &self,
        current_id: &AgentInstanceId,
        persist: bool,
    ) -> anyhow::Result<super::associations::SessionCreation> {
        let (session, prepared) = self.prepare_new_runtime(current_id).await?;
        self.sessions
            .prepared_creation(session, prepared, SessionStartReason::New, persist, None)
    }

    pub(crate) async fn new_session(
        &self,
        current_id: &AgentInstanceId,
    ) -> anyhow::Result<AcceptedTransition> {
        let (target, prepared) = self.prepare_new_runtime(current_id).await?;
        self.sessions
            .session_access(current_id)
            .checkpoint()
            .await?;
        let current = self.sessions.get(current_id)?.session().await;
        let previous_path = crate::session::storage::session_path(&current);
        let target_path = crate::session::storage::session_path(&target);
        let agent = self
            .sessions
            .transition_prepared(
                current_id,
                target,
                prepared,
                SessionStartReason::New,
                Some(&previous_path.to_string_lossy()),
                SessionEndReason::New,
                Some(&target_path.to_string_lossy()),
                true,
                None,
            )
            .await?;
        Ok(AcceptedTransition::capture(self, agent, None).await)
    }

    async fn prepare_resume(
        &self,
        target: &mut Session,
        seed: super::session::RuntimeConfigurationSeed,
        policy: ConfigurationGeneration,
    ) -> anyhow::Result<PreparedAgentRuntime> {
        let accepted_config = seed.runtime.runtime_config();
        let runtime = EffectiveSwitch::from_runtime(&seed.session, &accepted_config);
        let mut policy = policy.load_for_session(target, seed.runtime.settings_manager()?)?;
        policy.model_catalog = seed.runtime.model_catalog();
        runtime.apply_validated(target, &policy.settings, &policy.model_catalog)?;
        let mut options = policy.build_options_for(target, &runtime);
        options.phi.runtime_cfg.custom_providers = accepted_config.custom_providers;
        self.preparation.prepare(options).await
    }

    /// Resume a saved Session under the explicit stop-current policy; selecting current is a no-op.
    /// Validation precedes stop, but failures after stop do not resurrect the outgoing Agent.
    pub(crate) async fn resume_session(
        &self,
        current_id: &AgentInstanceId,
        target: crate::runtime::SavedSessionLocator<'_>,
    ) -> anyhow::Result<AcceptedTransition> {
        let current_agent = self.sessions.get(current_id)?;
        let locator = match target {
            crate::runtime::SavedSessionLocator::Id(id)
            | crate::runtime::SavedSessionLocator::IdOrPath(id) => id,
        };
        if locator == current_id.as_str() {
            return Ok(AcceptedTransition::capture(self, current_agent, None).await);
        }
        let policy = self.configuration.snapshot();
        let current = current_agent.session().await;
        let (mut target, lock) = self.load_saved_session(target).await?;
        if target.id.as_str() == current_id.as_str() {
            return Ok(AcceptedTransition::capture(self, current_agent, None).await);
        }
        let seed = current_agent.configuration_seed().await?;
        let prepared = self.prepare_resume(&mut target, seed, policy).await?;
        let previous_path = crate::session::storage::session_path(&current);
        let target_path = crate::session::storage::session_path(&target);
        let agent = self
            .sessions
            .transition_prepared(
                current_id,
                target,
                prepared,
                SessionStartReason::Resume,
                Some(&previous_path.to_string_lossy()),
                SessionEndReason::Resume,
                Some(&target_path.to_string_lossy()),
                true,
                lock,
            )
            .await?;
        Ok(AcceptedTransition::capture(self, agent, None).await)
    }

    /// Resolve an anchor entry before switching, then navigate without an immediate checkpoint.
    /// After admission the workflow retains both steps; failure returns the accepted target Agent.
    pub(crate) async fn jump_anchor(
        &self,
        current_id: &AgentInstanceId,
        target_id: &str,
        anchor_entry_id: &str,
    ) -> anyhow::Result<AcceptedTransition> {
        let current = self.sessions.get(current_id)?;
        let same_session = target_id.is_empty() || target_id == current_id.as_str();
        let (mut session, lock) = if same_session {
            (current.recovery().await?.session, None)
        } else {
            self.load_saved_session(crate::runtime::SavedSessionLocator::Id(target_id))
                .await?
        };
        let entry_id = target_entry_for_anchor(&session, anchor_entry_id)
            .ok_or_else(|| anyhow::anyhow!("No assistant entry found for selected anchor."))?;
        if same_session {
            let accepted = self
                .preparation
                .navigate_branch(
                    &self.sessions.session_access(current_id),
                    BranchNavigation::Entry { entry_id },
                )
                .await?;
            return Ok(AcceptedTransition::new(self, &current, accepted, None));
        }
        let seed = current.configuration_seed().await?;
        let prepared = self
            .prepare_resume(&mut session, seed, self.configuration.snapshot())
            .await?;
        let workflows = self.clone();
        let current_id = current_id.clone();
        tokio::spawn(async move {
            let agent = workflows
                .sessions
                .transition_prepared(
                    &current_id,
                    session,
                    prepared,
                    SessionStartReason::Resume,
                    None,
                    SessionEndReason::Resume,
                    None,
                    true,
                    lock,
                )
                .await?;
            let navigation = workflows
                .preparation
                .navigate_branch(
                    &workflows.sessions.session_access(agent.instance().id()),
                    BranchNavigation::Entry { entry_id },
                )
                .await;
            Ok(match navigation {
                Ok(accepted) => AcceptedTransition::new(&workflows, &agent, accepted, None),
                Err(error) => {
                    AcceptedTransition::capture(&workflows, agent, Some(error.to_string())).await
                }
            })
        })
        .await?
    }
}

impl AcceptedTransition {
    async fn capture(
        workflows: &SessionWorkflows,
        agent: Arc<AgentSession>,
        navigation_error: Option<String>,
    ) -> Self {
        let accepted = agent.current_runtime_view().await;
        Self::new(workflows, &agent, accepted, navigation_error)
    }

    fn new(
        workflows: &SessionWorkflows,
        agent: &Arc<AgentSession>,
        accepted: RuntimeView,
        navigation_error: Option<String>,
    ) -> Self {
        Self {
            attachment: crate::runtime::attachment::AgentAttachment::new(
                agent,
                workflows.sessions.session_access(agent.instance().id()),
                workflows.preparation.clone(),
            ),
            accepted,
            navigation_error,
        }
    }
}

fn target_entry_for_anchor(session: &Session, anchor_entry_id: &str) -> Option<String> {
    if session
        .entries
        .iter()
        .any(|entry| entry.id == anchor_entry_id && is_openable_assistant_entry(entry))
    {
        return Some(anchor_entry_id.to_string());
    }
    let mut roots = Vec::new();
    let mut children =
        std::collections::HashMap::<&str, Vec<&crate::session::TranscriptEntry>>::new();
    for entry in &session.entries {
        if let Some(parent) = entry.parent_id.as_deref() {
            children.entry(parent).or_default().push(entry);
        } else {
            roots.push(entry);
        }
    }
    let mut queue = if anchor_entry_id == session.id.as_str() {
        roots
    } else {
        children.get(anchor_entry_id).cloned().unwrap_or_default()
    };
    let mut index = 0;
    while index < queue.len() {
        let entry = queue[index];
        index += 1;
        if is_openable_assistant_entry(entry) {
            return Some(entry.id.to_string());
        }
        if let Some(descendants) = children.get(entry.id.as_str()) {
            queue.extend(descendants);
        }
    }
    None
}

fn is_openable_assistant_entry(entry: &crate::session::TranscriptEntry) -> bool {
    let crate::session::TranscriptEntryKind::Message {
        message:
            crate::session::AgentMessage::Assistant {
                content,
                stop_reason,
                ..
            },
    } = &entry.kind
    else {
        return false;
    };
    content.iter().any(|block| {
        matches!(block, crate::session::ContentBlock::Text { text } if !text.trim().is_empty())
    }) || stop_reason == "aborted"
}

#[cfg(test)]
#[path = "tests"]
pub(crate) mod tests {
    use super::*;
    use crate::AgentBuildOptions;
    use crate::runtime::TestRuntime;

    #[derive(Clone)]
    pub(crate) struct TestConfiguration {
        choices: ModelChoices,
        memory_admin: crate::interactive::MemoryAdministration,
        mcp_auth: crate::config::McpAuthDiagnostics,
        configuration: Arc<RuntimeConfiguration>,
    }

    impl TestConfiguration {
        pub(crate) fn new(
            cli: Arc<Cli>,
            mut settings: Settings,
            agent_options: AgentBuildOptions,
            hooks: Vec<Hook>,
            model_catalog: Arc<ModelCatalog>,
        ) -> Self {
            settings.no_tools = Some(agent_options.no_tools || settings.no_tools.unwrap_or(false));
            settings.max_tokens = agent_options.max_tokens.or(settings.max_tokens);
            settings.temperature = agent_options.temperature.or(settings.temperature);
            settings.codemode = agent_options.codemode.or(settings.codemode);
            let configuration = Arc::new(RuntimeConfiguration::new(
                &std::env::current_dir().unwrap(),
                cli,
                settings.clone(),
                crate::config::SettingsManager::from_settings(
                    &std::env::current_dir().unwrap(),
                    settings.clone(),
                    Settings::default(),
                )
                .unwrap(),
                hooks,
                model_catalog,
            ));
            Self {
                choices: ModelChoices::new(&settings, configuration.clone()),
                memory_admin: crate::interactive::MemoryAdministration::new(
                    settings.effective_memory().as_ref(),
                    &std::env::current_dir().unwrap(),
                ),
                mcp_auth: crate::config::McpAuthDiagnostics::new(&settings),
                configuration,
            }
        }

        pub(crate) fn install(&self, preparation: &crate::runtime::RuntimePreparation) {
            preparation.initialize_configuration(self.configuration.clone());
        }

        pub(crate) fn model_choices_owner(&self) -> ModelChoices {
            self.choices.clone()
        }

        pub(crate) fn memory_administration(&self) -> crate::interactive::MemoryAdministration {
            self.memory_admin.clone()
        }

        pub(crate) fn mcp_auth_diagnostics(&self) -> crate::config::McpAuthDiagnostics {
            self.mcp_auth.clone()
        }
    }

    use clap::Parser;

    pub(in crate::runtime) fn run_isolated_test(test_name: &str) -> bool {
        const CHILD_FLAG: &str = "PHI_TEST_RUNTIME_SCOPE";
        let test_name = test_name.split_once("::").unwrap().1;
        if std::env::var(CHILD_FLAG).as_deref() == Ok(test_name) {
            return false;
        }
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-runtime-reload-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD_FLAG, test_name)
            .env("PHI_CONFIG_DIR", root.join("config"))
            .env("PHI_SESSION_DIR", root.join("sessions"))
            .env("PHI_OFFLINE", "1")
            .current_dir(&root)
            .output()
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    pub(in crate::runtime) fn install_reload_catalog(
        root: &std::path::Path,
        catalog: &ModelCatalog,
    ) {
        let options = crate::config::settings::model_catalog_options(true);
        std::fs::create_dir_all(options.store_path.parent().unwrap()).unwrap();
        std::fs::copy(root.join("models.json"), &options.user_paths[0]).unwrap();
        let model = catalog.get("local", "test").unwrap();
        let mut providers = serde_json::Map::new();
        let mut sources = serde_json::Map::new();
        use phi_sdk::providers::ProviderApi;
        for (source, entries) in [
            (
                "models.dev",
                vec![
                    ("anthropic", ProviderApi::AnthropicMessages),
                    ("deepseek", ProviderApi::DeepSeekChat),
                    ("google", ProviderApi::GeminiGenerateContent),
                    ("openai", ProviderApi::OpenAiResponses),
                ],
            ),
            (
                "openai-codex",
                vec![("openai-codex", ProviderApi::OpenAiCodexResponses)],
            ),
            (
                "openrouter",
                vec![("openrouter", ProviderApi::OpenAiCompletions)],
            ),
        ] {
            let partition = entries
                .into_iter()
                .map(|(provider, api)| {
                    let mut descriptor = model.clone();
                    descriptor.provider = provider.into();
                    descriptor.api = api;
                    (
                        provider.to_string(),
                        std::collections::BTreeMap::from([("test".to_string(), descriptor)]),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>();
            let hash_input = root.join("catalog-partition.json");
            std::fs::write(&hash_input, serde_json::to_vec(&partition).unwrap()).unwrap();
            let hash = std::process::Command::new("sha256sum")
                .arg(&hash_input)
                .output()
                .unwrap();
            assert!(hash.status.success());
            let digest = String::from_utf8(hash.stdout)
                .unwrap()
                .split_whitespace()
                .next()
                .unwrap()
                .to_string();
            sources.insert(source.into(), serde_json::json!({"sha256":digest}));
            for (provider, models) in partition {
                providers.insert(provider, serde_json::json!({"models":models}));
            }
            std::fs::remove_file(hash_input).unwrap();
        }
        std::fs::write(
            &options.store_path,
            serde_json::to_vec(
                &serde_json::json!({"schemaVersion":2,"sources":sources,"providers":providers}),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(!ModelCatalog::load(options).unwrap().requires_bootstrap());
    }

    pub(crate) fn session_test_runtime() -> (std::path::PathBuf, TestConfiguration, Arc<TestRuntime>)
    {
        session_test_runtime_with_hooks(Vec::new())
    }

    pub(crate) fn session_test_runtime_with_hooks(
        hooks: Vec<Hook>,
    ) -> (std::path::PathBuf, TestConfiguration, Arc<TestRuntime>) {
        let root = std::env::temp_dir().join(format!("phi-open-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let model_path = root.join("models.json");
        std::fs::write(
            &model_path,
            br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}"#,
        )
        .unwrap();
        let catalog = Arc::new(
            ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![model_path],
                offline: true,
            })
            .unwrap(),
        );
        let cli = Arc::new(Cli::parse_from(["phi"]));
        let mut settings = Settings::default();
        settings.default_provider = Some("local".into());
        settings.default_model = Some("test".into());
        let settings_manager = crate::config::SettingsManager::from_settings(
            &root,
            Settings::default(),
            Settings::default(),
        )
        .unwrap();
        let policy = TestConfiguration::new(
            cli,
            settings,
            AgentBuildOptions {
                no_tools: true,
                ..Default::default()
            },
            hooks,
            catalog.clone(),
        );
        policy
            .configuration
            .generation
            .write()
            .unwrap()
            .settings_manager = settings_manager;
        let sandbox = phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap();
        let resources = RuntimeResources::new(
            phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()),
            Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
            catalog,
            sandbox,
        );
        let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
        (root, policy, test_runtime)
    }

    #[test]
    fn picker_seed_and_switch_names_keep_startup_policy_separate_from_edits() {
        let cli = Arc::new(Cli::parse_from(["phi"]));
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "defaultProvider": "startup-provider", "defaultModel": "startup-model",
            "defaultThinkingLevel": "high", "switches": [{"name": "startup-switch", "mode": "plan"}],
            "memory": {"enabled": false}
        }))
        .unwrap();
        let policy = TestConfiguration::new(
            cli.clone(),
            settings.clone(),
            crate::agent_options_from_cli(&cli, &settings),
            Vec::new(),
            Arc::new(ModelCatalog::empty()),
        );
        let mut draft = policy.choices.picker_seed();
        draft.default_model = Some("generated".into());
        draft
            .edit_switch("startup-switch", |_, switch| {
                switch.alias = Some("edited".into())
            })
            .unwrap();
        assert_eq!(
            draft.switch_by_name("plan").unwrap().model.as_deref(),
            Some("generated")
        );
        assert_eq!(
            policy.choices.picker_seed().default_model(),
            Some("startup-model")
        );
        assert!(
            policy
                .choices
                .picker_seed()
                .switch_by_name("startup-switch")
                .unwrap()
                .alias
                .is_none()
        );
        let mut generation = policy.configuration.snapshot();
        generation.settings.default_model = Some("reload-model".into());
        generation.settings.memory = Some(
            serde_json::from_value(serde_json::json!({
                "enabled": true, "tape": {"enabled": true, "onlyGit": false}
            }))
            .unwrap(),
        );
        generation.settings.switches = Some(vec![crate::config::SwitchConfig {
            name: "reload-switch".into(),
            ..Default::default()
        }]);
        *policy.configuration.generation.write().unwrap() = generation;
        assert!(
            matches!(&policy.memory_admin.check(&crate::commands::MemoryCheckOptions {
            scope: crate::commands::MemoryScope::Project,
            max_tree_lines: 25,
        })[..],
            [crate::interactive::MemoryNotice::Warning { message }]
            if message == "memory is disabled in settings")
        );
        assert!(policy.memory_admin.review(50).unwrap().is_none());
        assert_eq!(
            policy.choices.picker_seed().switch_names(),
            ["default", "plan", "startup-switch"]
        );
        assert_eq!(
            policy.choices.picker_seed().default_model(),
            Some("startup-model")
        );
        assert_eq!(
            draft
                .switch_by_name("startup-switch")
                .unwrap()
                .alias
                .as_deref(),
            Some("edited")
        );
    }

    #[tokio::test]
    async fn startup_directory_scopes_default_creation() {
        let process_dir = std::env::current_dir().unwrap();
        let (startup_dir, policy, runtime) = session_test_runtime();
        let configuration = policy.configuration.snapshot();
        runtime.preparation.initialize(
            &startup_dir,
            configuration.cli,
            configuration.settings,
            configuration.settings_manager,
            configuration.hooks,
            configuration.model_catalog,
        );
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let binding = workflows
            .create_session(CreateSessionRequest {
                provider: None,
                model: None,
                switch: None,
                persist: false,
            })
            .await
            .unwrap();
        assert_eq!(
            binding.session().await.working_dir.as_str(),
            startup_dir.to_str().unwrap()
        );
        assert_eq!(binding.instance().snapshot().working_dir, startup_dir);
        assert_eq!(std::env::current_dir().unwrap(), process_dir);
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(startup_dir).unwrap();
    }

    #[tokio::test]
    async fn model_catalog_maintenance_is_independent_without_publishing_generation() {
        let (root, policy, test_runtime) = session_test_runtime();
        assert!(
            SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone()
            )
            .is_err()
        );
        let startup = policy.configuration.snapshot();
        let models = test_runtime.preparation.clone().initialize(
            &policy.configuration.startup_dir,
            startup.cli.clone(),
            startup.settings.clone(),
            startup.settings_manager.clone(),
            startup.hooks.clone(),
            startup.model_catalog.clone(),
        );
        SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap();
        let repeated = test_runtime.preparation.clone().initialize(
            &policy.configuration.startup_dir,
            startup.cli,
            Settings::default(),
            startup.settings_manager,
            Vec::new(),
            Arc::new(ModelCatalog::empty()),
        );
        assert_eq!(repeated.picker_seed().default_model(), Some("test"));
        drop(repeated);
        let before = models.configuration.snapshot();
        assert!(!models.automatic_model_catalog_update_enabled());
        for force in [false, true] {
            let error = models.update_model_catalog(force).await.unwrap_err();
            assert!(error.to_string().contains("disabled by PHI_OFFLINE"));
        }
        let after = models.configuration.snapshot();
        assert!(Arc::ptr_eq(&before.model_catalog, &after.model_catalog));
        assert!(Arc::ptr_eq(&before.cli, &after.cli));
        assert_eq!(after.model_catalog.model_ids("local"), vec!["test"]);
        assert!(!root.join("store.json").exists());
        assert!(test_runtime.sessions.list().await.unwrap().is_empty());
        assert_eq!(models.switch_model_choices("local", "", None), ["test"]);
        let mut generation = after;
        generation.model_catalog = Arc::new(ModelCatalog::empty());
        models.configuration.publish_configuration(generation);
        assert!(models.switch_model_choices("local", "", None).is_empty());
        let weak_runtime = Arc::downgrade(&test_runtime);
        drop(test_runtime);
        drop(policy);
        assert!(weak_runtime.upgrade().is_none());
        assert_eq!(models.picker_seed().default_model(), Some("test"));
        assert!(models.switch_model_choices("local", "", None).is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn startup_uses_current_default_without_reinterpreting_history() {
        let (root, policy, _) = session_test_runtime();
        let mut configuration = policy.configuration.snapshot();
        configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
            name: "default".into(),
            thinking_level: Some("low".into()),
            prompt: Some("current instructions".into()),
            ..Default::default()
        }]);
        let mut session = crate::session::new(
            "retired-provider",
            "retired-model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.current_switch = "deleted-switch".into();
        session
            .transcript
            .switch_snapshots
            .push(phi_sdk::transcript::SwitchSnapshotRecord {
                id: "deleted-switch-1".into(),
                switch: phi_sdk::transcript::SwitchSnapshot {
                    name: "deleted-switch".into(),
                    provider: "retired-provider".into(),
                    model: "retired-model".into(),
                    thinking_level: Some("high".into()),
                    mode: "plan".into(),
                    prompt: Some("historical instructions".into()),
                },
            });
        session.transcript.append_user_message("historical request");
        session
            .transcript
            .append_entry(crate::session::TranscriptEntryKind::Message {
                message: crate::session::AgentMessage::Assistant {
                    id: None,
                    turn_id: None,
                    content: Vec::new(),
                    provider: "retired-provider".into(),
                    api: None,
                    model: "retired-model".into(),
                    switch: Some("deleted-switch-1".into()),
                    usage: Default::default(),
                    stop_reason: "stop".into(),
                },
            });
        let history = serde_json::to_value(&session.transcript).unwrap();
        for cli in [
            Cli::parse_from(["phi"]),
            Cli::parse_from(["phi", "--provider", "local", "--model", "test"]),
        ] {
            let runtime = resolve_startup(
                &cli,
                &configuration.settings,
                &mut session,
                &configuration.model_catalog,
            )
            .unwrap();
            assert_eq!(session.current_switch, "default");
            assert_eq!(session.provider, "local");
            assert_eq!(session.model, "test");
            assert_eq!(runtime.default_thinking_level.as_deref(), Some("low"));
            assert_eq!(
                runtime.mode_prompt_override.as_deref(),
                Some("current instructions")
            );
            assert_eq!(serde_json::to_value(&session.transcript).unwrap(), history);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn anchor_target_resolves_assistant_descendants_and_root() {
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.transcript.append_user_message("anchor");
        let user = session.leaf_id.clone().unwrap();
        session.transcript.append_assistant_message(
            None,
            None,
            Vec::new(),
            "test".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "local".into(),
            Default::default(),
            "stop".into(),
        );
        let empty = session.leaf_id.clone().unwrap();
        session.transcript.append_assistant_message(
            None,
            None,
            Vec::new(),
            "test".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "local".into(),
            Default::default(),
            "aborted".into(),
        );
        let aborted = session.leaf_id.clone().unwrap();
        session.select_branch(Some(user.as_str())).unwrap();
        session.transcript.append_assistant_message(
            None,
            None,
            vec![crate::session::ContentBlock::Text {
                text: "usable".into(),
            }],
            "test".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "local".into(),
            Default::default(),
            "stop".into(),
        );
        let sibling = session.leaf_id.clone().unwrap();
        assert_eq!(
            target_entry_for_anchor(&session, user.as_str()),
            Some(sibling.to_string())
        );
        assert_eq!(
            target_entry_for_anchor(&session, session.id.as_str()),
            Some(sibling.to_string())
        );
        assert_eq!(
            target_entry_for_anchor(&session, empty.as_str()),
            Some(aborted.to_string())
        );
        assert_eq!(
            target_entry_for_anchor(&session, aborted.as_str()),
            Some(aborted.to_string())
        );
        assert_eq!(target_entry_for_anchor(&session, "missing"), None);
    }

    mod configuration {
        use super::*;

        #[test]
        fn startup_selection_preserves_raw_defaults() {
            let (root, policy, _) = session_test_runtime();
            let mut configuration = policy.configuration.snapshot();
            configuration.settings.default_thinking_level = Some("medium".into());
            configuration.settings.default_model = Some("not-selected".into());
            configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
                name: "default".into(),
                thinking_level: Some("low".into()),
                ..Default::default()
            }]);
            let cli = Cli::parse_from(["phi", "--model", "test"]);
            let mut session = crate::session::new(
                "local",
                "not-selected",
                128_000,
                &std::env::current_dir().unwrap(),
            );
            let resolved = resolve_startup(
                &cli,
                &configuration.settings,
                &mut session,
                &configuration.model_catalog,
            )
            .unwrap();
            assert_eq!(session.model, "test");
            assert_eq!(resolved.default_thinking_level.as_deref(), Some("low"));
            assert_eq!(
                configuration.settings.default_model.as_deref(),
                Some("not-selected")
            );
            assert_eq!(
                configuration.settings.default_thinking_level.as_deref(),
                Some("medium")
            );
            let runtime = EffectiveSwitch::from_runtime(&session, &resolved);
            let next = configuration.new_session(&session, &runtime).unwrap();
            assert_eq!(next.model, "test");
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn startup_selection_does_not_append_history() {
            let (root, policy, _) = session_test_runtime();
            let mut configuration = policy.configuration.snapshot();
            configuration.settings.default_thinking_level = Some("low".into());
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            resolve_startup(
                &configuration.cli,
                &configuration.settings,
                &mut session,
                &configuration.model_catalog,
            )
            .unwrap();
            assert!(session.entries.is_empty());
            configuration.settings.default_model = Some("test".into());
            configuration.settings.default_thinking_level = Some("high".into());
            let resumed = resolve_startup(
                &configuration.cli,
                &configuration.settings,
                &mut session,
                &configuration.model_catalog,
            )
            .unwrap();
            assert_eq!(session.model, "test");
            assert_eq!(resumed.default_thinking_level.as_deref(), Some("high"));
            assert!(session.entries.is_empty());
            std::fs::remove_dir_all(root).unwrap();
        }

        #[tokio::test]
        async fn created_session_keeps_selection_out_of_history() {
            let (root, policy, test_runtime) = session_test_runtime();
            policy.install(&test_runtime.preparation);
            let agent = SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
            .create_session(CreateSessionRequest {
                provider: None,
                model: None,
                switch: None,
                persist: false,
            })
            .await
            .unwrap();
            let session = agent.recovery().await.unwrap().session;
            assert!(session.entries.is_empty());
            assert_eq!(session.current_switch, "default");
            assert_eq!(session.provider, "local");
            assert_eq!(session.model, "test");
            test_runtime
                .sessions
                .shutdown(SessionEndReason::Quit)
                .await
                .unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn unspecified_switch_thinking_uses_unmodified_global_default() {
            let (root, policy, _) = session_test_runtime();
            let mut configuration = policy.configuration.snapshot();
            configuration.settings.default_thinking_level = Some("medium".into());
            for mode in [None, Some("plan".to_string())] {
                configuration.settings.switches = Some(vec![
                    crate::config::SwitchConfig {
                        name: "default".into(),
                        thinking_level: Some("low".into()),
                        ..Default::default()
                    },
                    crate::config::SwitchConfig {
                        name: "review".into(),
                        mode,
                        ..Default::default()
                    },
                ]);
                let mut session = crate::session::new(
                    "local",
                    "test",
                    128_000,
                    &std::env::current_dir().unwrap(),
                );
                let startup = resolve_startup(
                    &configuration.cli,
                    &configuration.settings,
                    &mut session,
                    &configuration.model_catalog,
                )
                .unwrap();
                assert_eq!(startup.default_thinking_level.as_deref(), Some("low"));
                let selected =
                    EffectiveSwitch::for_session_switch(&session, &configuration.settings)
                        .select(
                            RuntimeSelection::Switch {
                                name: "review".into(),
                            },
                            &session,
                            &configuration.settings,
                        )
                        .unwrap();
                selected.apply_to_session(&mut session);
                let options = configuration.build_options_for(&session, &selected);
                assert_eq!(
                    options.phi.runtime_cfg.default_thinking_level.as_deref(),
                    Some("medium")
                );
                assert_eq!(
                    configuration.settings.default_thinking_level.as_deref(),
                    Some("medium")
                );
            }
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn construction_resolves_settings_without_startup_option_copies() {
            let (root, policy, _) = session_test_runtime();
            let mut configuration = policy.configuration.snapshot();
            configuration.settings.no_tools = Some(false);
            configuration.settings.max_tokens = Some(2048);
            configuration.settings.temperature = Some(0.4);
            configuration.settings.default_thinking_level = Some("medium".into());
            let session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            let options = configuration.runtime_build_options(&session);
            assert!(!options.phi.agent_options.no_tools);
            assert_eq!(options.phi.agent_options.max_tokens, Some(2048));
            assert_eq!(options.phi.runtime_cfg.temperature, Some(0.4));
            assert_eq!(
                options.phi.runtime_cfg.default_thinking_level.as_deref(),
                Some("medium")
            );
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn process_cli_constraints_survive_runtime_resolution() {
            let (root, policy, _) = session_test_runtime();
            let mut configuration = policy.configuration.snapshot();
            configuration.cli = Arc::new(Cli::parse_from([
                "phi",
                "--no-tools",
                "--max-tokens",
                "4096",
                "--temperature",
                "0.2",
                "--codemode",
            ]));
            configuration.settings.no_tools = Some(false);
            configuration.settings.max_tokens = Some(2048);
            let session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            let options = configuration.runtime_build_options(&session);
            assert!(options.phi.agent_options.no_tools);
            assert_eq!(options.phi.agent_options.max_tokens, Some(4096));
            assert_eq!(options.phi.agent_options.temperature, Some(0.2));
            assert!(options.phi.agent_options.codemode.unwrap().enabled);
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn selection_validation_is_atomic() {
            let (root, policy, _) = session_test_runtime();
            let mut configuration = policy.configuration.snapshot();
            for context_window in [None, Some(64_000)] {
                configuration.settings.context_window = context_window;
                for provider in ["local", "missing"] {
                    for model in ["test", "missing"] {
                        let mut session = crate::session::new(
                            "local",
                            "test",
                            128_000,
                            &std::env::current_dir().unwrap(),
                        );
                        let original = serde_json::to_value(&session).unwrap();
                        let mut runtime =
                            EffectiveSwitch::for_session_switch(&session, &configuration.settings);
                        runtime.switch = "selected".into();
                        runtime.provider = provider.into();
                        runtime.model = model.into();
                        let result = runtime.apply_validated(
                            &mut session,
                            &configuration.settings,
                            &configuration.model_catalog,
                        );
                        if provider == "local" && model == "test" {
                            result.unwrap();
                            assert_eq!(session.current_switch, "selected");
                            assert_eq!(session.context_window, context_window.unwrap_or(128_000));
                        } else {
                            assert!(result.is_err());
                            assert_eq!(serde_json::to_value(&session).unwrap(), original);
                        }
                    }
                }
            }
            std::fs::remove_dir_all(root).unwrap();
        }

        #[test]
        fn startup_validates_current_default_before_mutation() {
            let (root, policy, _) = session_test_runtime();
            let mut configuration = policy.configuration.snapshot();
            configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
                name: "default".into(),
                provider: Some("missing".into()),
                model: Some("test".into()),
                ..Default::default()
            }]);
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.current_switch = "review".into();
            let original = serde_json::to_value(&session).unwrap();
            assert!(
                resolve_startup(
                    &configuration.cli,
                    &configuration.settings,
                    &mut session,
                    &configuration.model_catalog
                )
                .is_err()
            );
            assert_eq!(serde_json::to_value(&session).unwrap(), original);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    mod transitions;

    mod navigation;

    mod runtime_selection;

    mod runtime_resources;

    mod prompt_policy;

    mod codemode;
}
