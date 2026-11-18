use crate::agent::custom::{AgentCatalogError, AgentDefinitionOrigin, AgentValidationReason};
use crate::agent::options::AgentOptions;
use crate::core::event::{PhiFailure, ToolDisplay};
use crate::core::lifecycle::*;
use crate::harness::instance::builder::{AgentInstanceBuildError, AgentInstanceOptions};
use crate::harness::instance::execution;
use crate::harness::instance::execution::PhiTurnError;
use crate::harness::instance::handle::PhiStreamError;
use crate::hooks::{HookRegistry, *};
use crate::models::ModelCatalog;
use crate::permission::PermissionConfig;
use crate::permission::checker::PermissionChecker;
use crate::permission::runtime::PermissionRuntime;
use crate::sandbox::Sandbox;
use crate::tools::{ToolDeclaration, ToolDisplayFormatter, ToolDisplayRegistry};
use crate::transcript::Transcript;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct ExtensionPreparation {
    sources: crate::extension::ExtensionSources,
    services: crate::extension::ExtensionRuntimeServices,
    display_slot: Arc<dyn ToolDisplayFormatter>,
}

pub struct PhiBuilder {
    provider: Option<String>,
    model: Option<String>,
    api_key: Option<String>,
    no_tools: bool,
    codemode: Option<crate::config::CodemodeConfig>,
    max_tokens: Option<u64>,
    temperature: Option<f64>,
    thinking_level: Option<String>,
    agent_mode: Option<crate::config::AgentMode>,
    mode_prompt_override: Option<String>,
    openai_reasoning: Option<crate::config::OpenAiReasoningConfig>,
    no_context_files: Option<bool>,
    global_agents_path: Option<PathBuf>,
    custom_agent_roots: Vec<PathBuf>,
    runtime_config: Option<crate::config::RuntimeConfig>,
    context: Option<crate::context::ContextFiles>,
    tool_declarations: Vec<ToolDeclaration>,
    hooks: Vec<Hook>,
    prompt_expander: Option<Arc<dyn crate::context::PromptExpander>>,
    command_hooks: crate::hooks::CommandHooks,
    hook_timeouts: HookTimeouts,
    agent_definitions: Vec<crate::agent::custom::AgentDefinition>,
    extensions: Option<Arc<crate::extension::ExtensionRuntime>>,
    extension_preparation: Option<ExtensionPreparation>,
    builtins: crate::extension::BuiltinExtensions,
    tool_display: ToolDisplayRegistry,
    runtime_services: PhiRuntimeServices,
}

#[derive(Clone)]
pub struct Phi {
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) api_key: Option<String>,
    pub(crate) cfg: crate::config::RuntimeConfig,
    pub(crate) context: crate::context::ContextFiles,
    pub(crate) hooks: HookRegistry,
    pub(crate) prompt_expander: Option<Arc<dyn crate::context::PromptExpander>>,
    command_hooks: crate::hooks::CommandHooks,
    pub(crate) options: AgentOptions,
    pub(crate) extensions: Option<Arc<crate::extension::ExtensionRuntime>>,
    extension_preparation: Option<ExtensionPreparation>,
    builtins: crate::extension::BuiltinExtensions,
    pub(crate) tool_declarations: Vec<ToolDeclaration>,
    pub(crate) tool_display: Arc<ToolDisplayRegistry>,
    pub(crate) runtime_services: PhiRuntimeServices,
}

impl Phi {
    pub fn builder() -> PhiBuilder {
        PhiBuilder::default()
    }

    pub fn messages(
        &self,
        options: crate::messages::MessagesOptions,
    ) -> Result<crate::messages::Messages, crate::messages::MessageError> {
        crate::messages::Messages::prepare(self, options)
    }

    pub async fn prepare_agent_instance(
        &self,
    ) -> Result<execution::AgentInstanceCandidate, AgentInstanceBuildError> {
        self.agent_instance_builder().build().await
    }

    pub async fn discover_models(&self) -> anyhow::Result<crate::models::ModelDiscoveryResult> {
        let catalog = self
            .runtime_services
            .resolve_model_catalog(&self.provider)?;
        if catalog.offline() {
            anyhow::bail!("model discovery disabled by PHI_OFFLINE");
        }
        let client = crate::provider::create_client(
            &self.provider,
            self.api_key.as_deref(),
            &self.cfg.custom_providers,
        )?;
        client.discover_models(&catalog).await
    }

    pub async fn prepare_agent_instance_with_options(
        &self,
        options: AgentInstanceOptions,
    ) -> Result<execution::AgentInstanceCandidate, AgentInstanceBuildError> {
        self.agent_instance_builder().options(options).build().await
    }

    pub async fn prepare_agent_instance_with_transcript(
        &self,
        transcript: Transcript,
    ) -> Result<execution::AgentInstanceCandidate, AgentInstanceBuildError> {
        self.agent_instance_builder()
            .transcript(transcript)
            .build()
            .await
    }

    pub fn agent_instance_builder(&self) -> execution::AgentInstanceBuilder<'_> {
        execution::AgentInstanceBuilder::new(self)
    }

    pub fn with_runtime_services(mut self, mut services: PhiRuntimeServices) -> Self {
        services.enforce_extensions(&self.cfg);
        self.runtime_services = services;
        self
    }

    pub fn format_tool_display(
        &self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<Option<ToolDisplay>> {
        self.tool_display.format_tool_display(name, args)
    }

    pub fn tool_display(&self) -> &ToolDisplayRegistry {
        self.tool_display.as_ref()
    }

    pub fn agents(&self) -> &crate::agent::custom::AgentCatalog {
        &self.context.agent_catalog
    }

    pub fn agent_mode(&self) -> crate::config::AgentMode {
        self.cfg.agent_mode
    }

    /// Notifies factory hooks. Prepared generations use `AgentRuntimeResources`.
    pub async fn session_started(&self, reason: SessionStartReason, related_session: Option<&str>) {
        crate::hooks::emit_session_start(&self.hooks, reason, related_session).await;
    }

    /// Notifies factory hooks. Prepared generations use `AgentRuntimeResources`.
    pub async fn session_ended(&self, reason: SessionEndReason, related_session: Option<&str>) {
        crate::hooks::emit_session_end(&self.hooks, reason, related_session).await;
    }
}

impl Default for PhiBuilder {
    fn default() -> Self {
        Self {
            provider: None,
            model: None,
            api_key: None,
            no_tools: false,
            codemode: None,
            max_tokens: None,
            temperature: None,
            thinking_level: None,
            agent_mode: None,
            mode_prompt_override: None,
            openai_reasoning: None,
            no_context_files: None,
            global_agents_path: None,
            custom_agent_roots: Vec::new(),
            runtime_config: None,
            context: None,
            tool_declarations: Vec::new(),
            hooks: Vec::new(),
            prompt_expander: None,
            command_hooks: crate::hooks::CommandHooks::default(),
            hook_timeouts: HookTimeouts::default(),
            agent_definitions: Vec::new(),
            extensions: None,
            extension_preparation: None,
            builtins: crate::extension::BuiltinExtensions::default(),
            tool_display: ToolDisplayRegistry::default(),
            runtime_services: PhiRuntimeServices::default(),
        }
    }
}

impl PhiBuilder {
    pub fn provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    pub fn codemode(mut self, config: crate::config::CodemodeConfig) -> Self {
        self.codemode = Some(config);
        self
    }

    pub fn no_tools(mut self, no_tools: bool) -> Self {
        self.no_tools = no_tools;
        self
    }

    pub fn max_tokens(mut self, max_tokens: u64) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    pub fn temperature(mut self, temperature: f64) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn thinking_level(mut self, thinking_level: impl Into<String>) -> Self {
        self.thinking_level = Some(thinking_level.into());
        self
    }

    pub fn agent_mode(mut self, mode: crate::config::AgentMode) -> Self {
        self.agent_mode = Some(mode);
        self
    }

    pub fn mode_prompt_override(mut self, prompt: impl Into<String>) -> Self {
        self.mode_prompt_override = Some(prompt.into());
        self
    }

    pub fn openai_reasoning(mut self, reasoning: crate::config::OpenAiReasoningConfig) -> Self {
        self.openai_reasoning = Some(reasoning);
        self
    }

    pub fn no_context_files(mut self, no_context_files: bool) -> Self {
        self.no_context_files = Some(no_context_files);
        self
    }

    pub fn global_agents_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.global_agents_path = Some(path.into());
        self
    }

    pub fn custom_agent_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.custom_agent_roots.push(path.into());
        self
    }

    pub fn custom_agent_roots(
        mut self,
        paths: impl IntoIterator<Item = impl Into<PathBuf>>,
    ) -> Self {
        self.custom_agent_roots
            .extend(paths.into_iter().map(Into::into));
        self
    }

    pub fn runtime_config(mut self, cfg: crate::config::RuntimeConfig) -> Self {
        self.runtime_config = Some(cfg);
        self
    }

    pub fn context(mut self, context: crate::context::ContextFiles) -> Self {
        self.context = Some(context);
        self
    }

    /// Sets callback deadlines and the independent SessionEnd budget.
    pub fn hook_timeouts(mut self, timeouts: HookTimeouts) -> Self {
        self.hook_timeouts = timeouts;
        self
    }

    /// Appends one typed hook.
    pub fn hook(mut self, hook: Hook) -> Self {
        self.hooks.push(hook);
        self
    }

    /// Appends typed hooks in iterator order.
    pub fn hooks(mut self, hooks: impl IntoIterator<Item = Hook>) -> Self {
        self.hooks.extend(hooks);
        self
    }

    /// Supplies application-owned command expansion after lifecycle gates.
    pub fn prompt_expander(mut self, expander: Arc<dyn crate::context::PromptExpander>) -> Self {
        self.prompt_expander = Some(expander);
        self
    }

    pub fn command_hooks(mut self, hooks: crate::hooks::CommandHooks) -> Self {
        self.command_hooks = hooks;
        self
    }

    pub fn agent(mut self, definition: crate::agent::custom::AgentDefinition) -> Self {
        self.agent_definitions.push(definition);
        self
    }

    pub fn agents(
        mut self,
        definitions: impl IntoIterator<Item = crate::agent::custom::AgentDefinition>,
    ) -> Self {
        self.agent_definitions.extend(definitions);
        self
    }

    pub fn memory_extension(mut self, hooks: crate::extension::memory::MemoryHooks) -> Self {
        self.builtins = self.builtins.with_memory(hooks);
        self
    }

    pub fn extension_runtime(
        mut self,
        extensions: Arc<crate::extension::ExtensionRuntime>,
    ) -> Self {
        if let Some(previous) = self.extensions.take() {
            self.tool_display
                .remove_formatter(&(previous as Arc<dyn ToolDisplayFormatter>));
        }
        if let Some(previous) = self.extension_preparation.take() {
            self.tool_display.remove_formatter(&previous.display_slot);
        }
        self.tool_display.push(extensions.clone());
        self.extensions = Some(extensions);
        self
    }

    /// Defers Lua execution until the isolated generation has its permission runtime.
    pub fn extension_sources(
        mut self,
        sources: crate::extension::ExtensionSources,
        services: crate::extension::ExtensionRuntimeServices,
    ) -> Self {
        if let Some(previous) = self.extensions.take() {
            self.tool_display
                .remove_formatter(&(previous as Arc<dyn ToolDisplayFormatter>));
        }
        if let Some(previous) = self.extension_preparation.take() {
            self.tool_display.remove_formatter(&previous.display_slot);
        }
        let display_slot: Arc<dyn ToolDisplayFormatter> = Arc::new(|_: &str, _: &str| Ok(None));
        self.tool_display.push(display_slot.clone());
        self.extension_preparation = Some(ExtensionPreparation {
            sources,
            services,
            display_slot,
        });
        self
    }

    pub fn tool(mut self, declaration: ToolDeclaration) -> Self {
        self.tool_declarations.push(declaration);
        self
    }

    pub fn tools(mut self, declarations: impl IntoIterator<Item = ToolDeclaration>) -> Self {
        self.tool_declarations.extend(declarations);
        self
    }

    pub fn tool_display_formatter(
        mut self,
        formatter: impl ToolDisplayFormatter + 'static,
    ) -> Self {
        self.tool_display.push(Arc::new(formatter));
        self
    }

    pub fn tool_display_registry(mut self, registry: ToolDisplayRegistry) -> Self {
        self.tool_display = registry;
        self
    }

    pub fn runtime_services(mut self, services: PhiRuntimeServices) -> Self {
        self.runtime_services = services;
        self
    }

    pub fn build(self) -> Result<Phi, PhiBuildError> {
        let mut cfg = self.runtime_config.unwrap_or_default();
        if let Some(codemode) = self.codemode {
            cfg.extensions.codemode = codemode;
        }
        if let Some(reasoning) = self.openai_reasoning {
            cfg.openai_reasoning = Some(reasoning);
        }
        if let Some(mode) = self.agent_mode {
            cfg.agent_mode = mode;
        }
        if let Some(prompt) = self.mode_prompt_override {
            cfg.mode_prompt_override = Some(prompt);
        }
        cfg.extensions = cfg.extensions.resolve();
        let builtins = self.builtins.with_disabled(&cfg.extensions.disabled);
        let mut runtime_services = self.runtime_services;
        runtime_services.enforce_extensions(&cfg);
        let provider = self
            .provider
            .filter(|value| !value.trim().is_empty())
            .ok_or(PhiBuildError::MissingProvider)?;
        let model = self
            .model
            .filter(|value| !value.trim().is_empty())
            .ok_or(PhiBuildError::MissingModel)?;
        let no_context_files = self
            .no_context_files
            .unwrap_or_else(|| cfg.no_context_files.unwrap_or(false));
        let mut context = self.context.unwrap_or_else(|| {
            crate::context::load_with_options(
                no_context_files,
                self.global_agents_path,
                self.custom_agent_roots,
            )
        });
        context.agent_catalog = context
            .agent_catalog
            .with_definitions(self.agent_definitions)?;
        let mut hooks = HookRegistry::with_timeouts(self.hook_timeouts);
        for hook in self.hooks {
            hooks.register_hook(hook);
        }
        for hook in builtins.hooks() {
            hooks.register_hook(hook);
        }
        if let Some(extensions) = &self.extensions {
            extensions.register_hooks(&mut hooks);
        }
        hooks.validate_registrations()?;
        let options = AgentOptions {
            no_tools: self.no_tools || cfg.no_tools.unwrap_or(false),
            max_tokens: self.max_tokens.or(cfg.max_tokens),
            temperature: self.temperature.or(cfg.temperature),
            thinking_level: self
                .thinking_level
                .or_else(|| cfg.default_thinking_level.clone()),
            agent_sender: None,
            task_sender: None,
            bash_sender: None,
            memory: Default::default(),
        };

        let mut tool_display = self.tool_display;
        if self.extensions.is_none() && self.extension_preparation.is_none() {
            tool_display.push(Arc::new(builtins.clone()));
        }
        Ok(Phi {
            provider,
            model,
            api_key: self.api_key,
            context,
            hooks,
            prompt_expander: self.prompt_expander,
            command_hooks: self.command_hooks,
            options,
            extensions: self.extensions,
            extension_preparation: self.extension_preparation,
            builtins,
            tool_declarations: self.tool_declarations,
            tool_display: Arc::new(tool_display),
            runtime_services,
            cfg,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{AgentDefinition, AgentDefinitionOrigin, AgentValidationReason};

    fn build_with_agent(definition: AgentDefinition) -> Result<Phi, PhiBuildError> {
        Phi::builder()
            .provider("openai")
            .model("test-model")
            .agent(definition)
            .build()
    }

    #[test]
    fn builder_returns_typed_required_field_errors() {
        assert!(matches!(
            Phi::builder().model("test-model").build(),
            Err(PhiBuildError::MissingProvider)
        ));
        assert!(matches!(
            Phi::builder().provider("openai").build(),
            Err(PhiBuildError::MissingModel)
        ));
    }

    #[test]
    fn builder_returns_structured_agent_validation_reasons() {
        let cases = [
            (
                AgentDefinition::new("Code Reviewer", "Review code", "Review code."),
                "Code Reviewer",
                AgentValidationReason::InvalidName,
            ),
            (
                AgentDefinition::new("reviewer", " ", "Review code."),
                "reviewer",
                AgentValidationReason::EmptyDescription,
            ),
            (
                AgentDefinition::new("reviewer", "Review code", " "),
                "reviewer",
                AgentValidationReason::EmptyPrompt,
            ),
            (
                AgentDefinition::new("reviewer", "Review code", "Review code.").model(" "),
                "reviewer",
                AgentValidationReason::EmptyModel,
            ),
            (
                AgentDefinition::new("reviewer", "Review code", "Review code.").max_turns(0),
                "reviewer",
                AgentValidationReason::ZeroMaxTurns,
            ),
            (
                AgentDefinition::new("reviewer", "Review code", "Review code.")
                    .tools(["read"])
                    .disallowed_tools(["Read"]),
                "reviewer",
                AgentValidationReason::ConflictingToolPolicy,
            ),
        ];

        for (definition, expected_name, expected_reason) in cases {
            assert!(matches!(
                build_with_agent(definition),
                Err(PhiBuildError::InvalidAgent { ref name, reason })
                    if name == expected_name && reason == expected_reason
            ));
        }
    }

    #[test]
    fn builder_rejects_duplicate_agent_definitions_with_origins() {
        let definition = || AgentDefinition::new("reviewer", "Review code", "Review code.");
        let result = Phi::builder()
            .provider("openai")
            .model("test-model")
            .agents([definition(), definition()])
            .build();

        assert!(matches!(
            result,
            Err(PhiBuildError::DuplicateAgent {
                ref name,
                existing: AgentDefinitionOrigin::Rust,
                duplicate: AgentDefinitionOrigin::Rust,
            }) if name == "reviewer"
        ));
    }

    #[test]
    fn builder_reports_file_and_rust_duplicate_origins() {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-agent-origin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("reviewer.md");
        std::fs::write(
            &path,
            "---\nname: reviewer\ndescription: File reviewer\n---\nReview code.",
        )
        .unwrap();

        let result = Phi::builder()
            .provider("openai")
            .model("test-model")
            .custom_agent_root(&root)
            .agent(AgentDefinition::new(
                "reviewer",
                "Rust reviewer",
                "Review code.",
            ))
            .build();
        std::fs::remove_dir_all(root).unwrap();

        assert!(matches!(
            result,
            Err(PhiBuildError::DuplicateAgent {
                ref name,
                existing: AgentDefinitionOrigin::File(ref existing),
                duplicate: AgentDefinitionOrigin::Rust,
            }) if name == "reviewer" && existing == &path
        ));
    }

    #[test]
    fn builder_defers_environment_dependent_agent_references() {
        let result = build_with_agent(
            AgentDefinition::new("reviewer", "Review code", "Review code.")
                .model("not-resolved-during-build")
                .tools(["not-registered-during-build"]),
        );

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn public_hooks_observe_session_boundaries() {
        use crate::Phi;
        use crate::hooks::Hook;
        use crate::lifecycle::{SessionEndReason, SessionStartReason};

        let starts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let start_count = starts.clone();
        let end_count = ends.clone();
        let phi = Phi::builder()
            .provider("openai")
            .model("test-model")
            .hook(Hook::session_start(move |event| {
                let starts = start_count.clone();
                async move {
                    assert!(matches!(event.reason, SessionStartReason::Resume));
                    starts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(Vec::new())
                }
            }))
            .hook(Hook::session_end(move |event| {
                let ends = end_count.clone();
                async move {
                    assert!(matches!(event.reason, SessionEndReason::Quit));
                    ends.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(Vec::new())
                }
            }))
            .build()
            .unwrap();

        phi.session_started(SessionStartReason::Resume, None).await;
        phi.session_ended(SessionEndReason::Quit, None).await;

        assert_eq!(starts.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(ends.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hook_timeouts_reach_factory_and_prepared_generations() {
        let timeouts = HookTimeouts {
            hook_timeout: std::time::Duration::from_secs(1),
            session_end_timeout: std::time::Duration::from_secs(4),
        };
        let phi = Phi::builder()
            .provider("openai")
            .model("test")
            .context(crate::context::load(true))
            .hook_timeouts(timeouts)
            .hook(Hook::session_start(|_| std::future::pending()))
            .hook(
                Hook::session_end(|_| async {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    Ok(vec![HookNotice::Info {
                        message: "ended".into(),
                    }])
                })
                .with_timeout(std::time::Duration::from_secs(3)),
            )
            .build()
            .unwrap();
        let resources = phi
            .prepare_resources(std::path::Path::new("/tmp"), None)
            .unwrap();
        for hooks in [&phi.hooks, &resources.hooks] {
            let started = tokio::time::Instant::now();
            emit_session_start(hooks, SessionStartReason::Startup, None).await;
            assert_eq!(started.elapsed(), timeouts.hook_timeout);
            assert!(matches!(
                hooks.take_notices().as_slice(),
                [HookNotice::Warning { message }] if message.contains("timed out")
            ));
            emit_session_end(hooks, SessionEndReason::Quit, None).await;
            assert_eq!(
                hooks.take_notices(),
                [HookNotice::Info {
                    message: "ended".into(),
                }]
            );
        }
    }

    #[tokio::test]
    async fn builtin_exclusions_block_overrides_hooks_and_service_rebinding() {
        use crate::extension::{BuiltinExtension, mcp::McpClientManager};
        let manager = McpClientManager::load_with_settings(&Default::default(), Default::default());
        let services = PhiRuntimeServices::new().mcp_manager(manager);
        let phi = Phi::builder()
            .provider("openai")
            .model("test")
            .context(crate::context::load(true))
            .runtime_config(crate::config::RuntimeConfig {
                extensions: crate::extension::ExtensionConfig {
                    disabled: BuiltinExtension::ALL.into_iter().collect(),
                    memory: Some(Default::default()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .codemode(crate::config::CodemodeConfig {
                enabled: true,
                ..Default::default()
            })
            .memory_extension(
                crate::extension::memory::MemoryHooks::new(
                    crate::extension::memory::settings_from_config(&Default::default()),
                )
                .prompt_only(),
            )
            .runtime_services(services.clone())
            .build()
            .unwrap();
        assert!(!phi.cfg.extensions.codemode.enabled);
        assert!(!phi.cfg.extensions.memory.as_ref().unwrap().enabled);
        assert!(phi.runtime_services.mcp_manager_ref().is_none());
        let phi = phi.with_runtime_services(services);
        assert!(phi.runtime_services.mcp_manager_ref().is_none());
        let resources = phi
            .prepare_resources(std::path::Path::new("/tmp"), None)
            .unwrap();
        let result = emit_user_prompt_submit(
            &resources.hooks,
            UserPromptSubmitEvent {
                delegated_child: false,
                original_prompt: "/memory-inject inspect".into(),
                prompt: "/memory-inject inspect".into(),
                source: UserPromptSubmitSource::Sdk,
                dispatch: UserPromptDispatch::Immediate,
            },
        )
        .await;
        assert!(
            matches!(result, UserPromptSubmitAction::Continue { prompt, .. }
            if prompt == "/memory-inject inspect")
        );
    }

    #[test]
    fn builtin_memory_display_precedes_application_fallback() {
        let extensions =
            Arc::new(crate::extension::ExtensionRuntime::try_load_with_paths(Vec::new()).unwrap());
        let phi = Phi::builder()
            .provider("openai")
            .model("test")
            .context(crate::context::load(true))
            .extension_runtime(extensions)
            .tool_display_formatter(|_: &str, _: &str| {
                Ok(Some(ToolDisplay {
                    title: Some("fallback".into()),
                    ..Default::default()
                }))
            })
            .build()
            .unwrap();
        let resources = phi
            .prepare_resources(std::path::Path::new("/tmp"), None)
            .unwrap();
        for display in [&phi.tool_display, &resources.tool_display] {
            assert_eq!(
                display
                    .format_tool_display("tape_anchor", "{}")
                    .unwrap()
                    .unwrap()
                    .title
                    .as_deref(),
                Some("Tape anchor")
            );
            assert_eq!(
                display
                    .format_tool_display("unknown", "{}")
                    .unwrap()
                    .unwrap()
                    .title
                    .as_deref(),
                Some("fallback")
            );
        }
    }

    #[tokio::test]
    async fn memory_extension_hooks_survive_generation_preparation_once() {
        let config = crate::extension::memory::MemoryConfig {
            enabled: false,
            ..Default::default()
        };
        let hooks = crate::extension::memory::MemoryHooks::new(
            crate::extension::memory::settings_from_config(&config),
        )
        .prompt_only();
        for extensions in [
            None,
            Some(Arc::new(
                crate::extension::ExtensionRuntime::try_load_with_paths(Vec::new()).unwrap(),
            )),
        ] {
            let mut builder = Phi::builder()
                .provider("openai")
                .model("test")
                .context(crate::context::load(true))
                .memory_extension(hooks.clone())
                .memory_extension(hooks.clone());
            if let Some(extensions) = extensions {
                builder = builder.extension_runtime(extensions);
            }
            let phi = builder.build().unwrap();
            for _ in 0..2 {
                let resources = phi
                    .prepare_resources(std::path::Path::new("/tmp"), None)
                    .unwrap();
                let result = emit_user_prompt_submit(
                    &resources.hooks,
                    UserPromptSubmitEvent {
                        delegated_child: false,
                        original_prompt: "/memory-inject /memory-inject inspect".into(),
                        prompt: "/memory-inject /memory-inject inspect".into(),
                        source: UserPromptSubmitSource::Sdk,
                        dispatch: UserPromptDispatch::Immediate,
                    },
                )
                .await;
                assert!(
                    matches!(result, UserPromptSubmitAction::Continue { prompt, .. }
                    if prompt == "/memory-inject inspect")
                );
            }
        }
    }

    #[tokio::test]
    async fn captured_extensions_prepare_isolated_surfaces_from_frozen_sources() {
        use crate::extension::{ExtensionRuntimeServices, ExtensionSources};
        let path = std::path::Path::new("/tmp").join(format!(
            "phi-captured-extension-{}.lua",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "invalid lua source").unwrap();
        let invalid = ExtensionSources::capture(vec![path.clone()]).unwrap();
        let phi = Phi::builder()
            .provider("openai")
            .model("test")
            .context(crate::context::load(true))
            .extension_sources(invalid, ExtensionRuntimeServices::new())
            .build()
            .unwrap();
        assert!(
            phi.prepare_resources(std::path::Path::new("/tmp"), None)
                .is_err()
        );
        std::fs::write(
            &path,
            r#"
local count = 0
on("SessionStart", function() count = count + 1 end)
register_command("captured", {handler = function() end})
register_tool_display("counter", function() return "captured display" end)
register_tool("counter", {
  description = "counter",
  handler = function() return { count = count } end,
})
"#,
        )
        .unwrap();
        let sources = ExtensionSources::capture(vec![path.clone()]).unwrap();
        std::fs::remove_file(path).unwrap();
        let phi = Phi::builder()
            .provider("openai")
            .model("test")
            .context(crate::context::load(true))
            .extension_sources(sources, ExtensionRuntimeServices::new())
            .tool_display_formatter(|_: &str, _: &str| Ok(Some(ToolDisplay::title("fallback"))))
            .build()
            .unwrap();
        let first = phi
            .prepare_resources(std::path::Path::new("/tmp"), None)
            .unwrap();
        let second = phi
            .prepare_resources(std::path::Path::new("/tmp"), None)
            .unwrap();
        first.session_started(SessionStartReason::New, None).await;
        for (resources, count) in [(&first, 1), (&second, 0)] {
            let runtime = resources.extensions().unwrap();
            assert!(runtime.has_command("captured"));
            let output: serde_json::Value =
                serde_json::from_str(&runtime.call_tool("counter", "{}".into()).unwrap()).unwrap();
            assert_eq!(output["count"], count);
            for (tool, title) in [
                ("counter", "captured display"),
                ("tape_anchor", "Tape anchor"),
                ("unknown", "fallback"),
            ] {
                assert_eq!(
                    resources
                        .tool_display
                        .format_tool_display(tool, "{}")
                        .unwrap()
                        .unwrap()
                        .title
                        .as_deref(),
                    Some(title)
                );
            }
        }
    }

    #[tokio::test]
    async fn extension_runtime_registers_sdk_surfaces_once() {
        let path = std::env::temp_dir().join(format!(
            "phi-sdk-extension-runtime-{}.lua",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            r#"
local count = 0
on("SessionStart", function()
  count = count + 1
  return { active = true }
end)
register_tool_display("counter", function() return "shared" end)
register_tool("counter", {
  description = "counter",
  handler = function() return { count = count } end,
})
"#,
        )
        .unwrap();
        let extensions = Arc::new(
            crate::extension::ExtensionRuntime::try_load_with_paths(vec![path.clone()]).unwrap(),
        );
        let phi = Phi::builder()
            .provider("openai")
            .model("test")
            .context(crate::context::load(true))
            .extension_runtime(extensions.clone())
            .build()
            .unwrap();

        phi.session_started(crate::core::lifecycle::SessionStartReason::Startup, None)
            .await;
        phi.session_started(crate::core::lifecycle::SessionStartReason::Reload, None)
            .await;

        let _ = std::fs::remove_file(path);
        let output = extensions.call_tool("counter", "{}".to_string()).unwrap();
        let output: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(output["count"], 2);
        assert_eq!(
            phi.format_tool_display("counter", "{}")
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("shared")
        );
        assert_eq!(phi.extensions.as_ref().unwrap().tool_specs().len(), 1);
    }
}

/// Generation-scoped resources shared by execution and application boundaries.
#[derive(Clone)]
pub struct AgentRuntimeResources {
    permission: PermissionRuntime,
    extensions: Option<Arc<crate::extension::ExtensionRuntime>>,
    pub(crate) hooks: HookRegistry,
    pub(crate) tool_display: Arc<ToolDisplayRegistry>,
}

impl AgentRuntimeResources {
    pub async fn ready(&self) {
        self.hooks.ready().await
    }

    /// Returns a transient count of in-flight hook invocations for this generation.
    pub fn running_hook_count(&self) -> usize {
        self.hooks.running_hook_count()
    }

    pub fn take_hook_notices(&self) -> Vec<HookNotice> {
        self.hooks.take_notices()
    }

    /// Cancels owned background hooks and prevents new background work in this generation.
    /// SessionEnd callbacks remain available for bounded cleanup.
    pub async fn cancel_background_hooks(&self) {
        self.hooks.cancel_background_hooks().await;
    }

    pub fn notify(&self, event: NotificationEvent) {
        self.hooks.schedule_notification(event);
    }

    pub fn extensions(&self) -> Option<Arc<crate::extension::ExtensionRuntime>> {
        self.extensions.clone()
    }

    pub fn allow_session_tool(&self, tool: impl Into<String>, pattern: &str) {
        self.permission.add_session_allowlist(tool, pattern);
    }

    pub async fn session_started(&self, reason: SessionStartReason, related_session: Option<&str>) {
        crate::hooks::emit_session_start(&self.hooks, reason, related_session).await;
    }

    pub async fn session_ended(&self, reason: SessionEndReason, related_session: Option<&str>) {
        crate::hooks::emit_session_end(&self.hooks, reason, related_session).await;
    }

    pub(crate) fn permission(&self) -> &PermissionRuntime {
        &self.permission
    }
}

impl Phi {
    pub(crate) fn prepare_resources(
        &self,
        working_dir: &std::path::Path,
        sender: Option<crate::core::interaction::PermissionSender>,
    ) -> anyhow::Result<AgentRuntimeResources> {
        let permission = self
            .runtime_services
            .prepare_permission(Some(working_dir))
            .with_interaction_sender(sender);
        let mut hooks = self.hooks.for_generation();
        if !self.command_hooks.is_empty() {
            self.command_hooks.register(
                &mut hooks,
                self.runtime_services.resolve_sandbox(working_dir)?,
            )?;
        }
        let mut tool_display = self.tool_display.as_ref().clone();
        let prepared_extension: Option<(_, Arc<dyn ToolDisplayFormatter>)> =
            if let Some(preparation) = &self.extension_preparation {
                Some((
                    preparation
                        .sources
                        .load(preparation.services.clone().permission(permission.clone()))?,
                    preparation.display_slot.clone(),
                ))
            } else if let Some(previous) = &self.extensions {
                Some((
                    previous.prepare_generation(permission.clone())?,
                    previous.clone(),
                ))
            } else {
                None
            };
        let extensions = prepared_extension
            .map(|(mut runtime, formatter)| -> anyhow::Result<_> {
                runtime.set_builtins(self.builtins.clone(), &self.cfg)?;
                let runtime = Arc::new(runtime);
                runtime.register_hooks(&mut hooks);
                tool_display.replace_formatter(&formatter, runtime.clone());
                Ok(runtime)
            })
            .transpose()?;
        hooks.validate_registrations()?;
        Ok(AgentRuntimeResources {
            permission,
            extensions,
            hooks,
            tool_display: Arc::new(tool_display),
        })
    }
}

#[derive(Clone)]
struct PermissionPolicy {
    config: PermissionConfig,
    working_dir: Option<std::path::PathBuf>,
}

impl PermissionPolicy {
    fn runtime(&self, interactive: bool) -> PermissionRuntime {
        let checker = PermissionChecker::new(&self.config, self.working_dir.clone());
        PermissionRuntime::new(Arc::new(Mutex::new(checker))).interactive(interactive)
    }
}

/// Reusable construction policy and initial grants, never live instance permissions.
#[derive(Clone)]
pub struct PhiRuntimeServices {
    permission_seeds: Arc<Mutex<Vec<(String, String)>>>,
    permission_policy: PermissionPolicy,
    sandbox: Option<Sandbox>,
    interactive: bool,
    model_catalog: Option<Arc<ModelCatalog>>,
    mcp_manager: Option<crate::extension::mcp::McpClientManager>,
}

impl Default for PhiRuntimeServices {
    fn default() -> Self {
        let policy = PermissionPolicy {
            config: PermissionConfig {
                mode: crate::permission::PermissionMode::Allow,
                ..Default::default()
            },
            working_dir: None,
        };
        Self {
            permission_seeds: Arc::default(),
            permission_policy: policy,
            sandbox: None,
            interactive: false,
            model_catalog: None,
            mcp_manager: None,
        }
    }
}

impl PhiRuntimeServices {
    fn enforce_extensions(&mut self, config: &crate::config::RuntimeConfig) {
        if !config
            .extensions
            .is_enabled(crate::extension::BuiltinExtension::Mcp)
        {
            self.mcp_manager = None;
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn permission_policy(
        mut self,
        config: PermissionConfig,
        working_dir: Option<std::path::PathBuf>,
    ) -> Self {
        let policy = PermissionPolicy {
            config,
            working_dir,
        };
        self.permission_seeds = Arc::default();
        self.permission_policy = policy;
        self
    }

    /// Keeps shared infrastructure and policy, but clears initial Session grants.
    pub fn isolated_session(&self) -> Self {
        let mut services = self.clone();
        services.permission_seeds = Arc::default();
        services
    }

    /// Adds initial grants for future candidates only; active generations are unchanged.
    pub fn sync_session_permissions(&self, allowlist: &[(String, String)]) {
        let mut seeds = self.permission_seeds.lock().unwrap();
        for grant in allowlist {
            if !seeds.contains(grant) {
                seeds.push(grant.clone());
            }
        }
    }

    pub fn allow_session_tool(&self, tool: impl Into<String>, pattern: &str) {
        self.sync_session_permissions(&[(tool.into(), pattern.to_string())]);
    }

    pub fn extension_runtime_services(
        &self,
        process_runner: Arc<dyn crate::extension::ExtensionProcessRunner>,
        process_policy: crate::extension::ExtensionProcessPolicy,
        sandbox_effects: bool,
    ) -> crate::extension::ExtensionRuntimeServices {
        crate::extension::ExtensionRuntimeServices::new()
            .process_runner(process_runner)
            .process_policy(process_policy)
            .sandbox_effects(sandbox_effects)
            .permission(self.prepare_permission(None).interactive(false))
    }

    pub fn sandbox(mut self, sandbox: Sandbox) -> Self {
        self.sandbox = Some(sandbox);
        self
    }

    pub fn model_catalog(mut self, catalog: Arc<ModelCatalog>) -> Self {
        self.model_catalog = Some(catalog);
        self
    }

    pub(crate) fn resolve_model_catalog(
        &self,
        provider: &str,
    ) -> anyhow::Result<Arc<ModelCatalog>> {
        if let Some(catalog) = &self.model_catalog {
            return Ok(catalog.clone());
        }
        anyhow::bail!(
            "model catalog runtime service is required for provider {provider}; inject one with PhiRuntimeServices::model_catalog(...)"
        )
    }

    pub fn interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    pub fn mcp_manager(mut self, manager: crate::extension::mcp::McpClientManager) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    pub(crate) fn prepare_permission(
        &self,
        working_dir: Option<&std::path::Path>,
    ) -> PermissionRuntime {
        let mut policy = self.permission_policy.clone();
        if let Some(working_dir) = working_dir {
            policy.working_dir = Some(working_dir.to_path_buf());
        }
        let permission = policy.runtime(self.interactive);
        permission.load_session_allowlist(&self.permission_seeds.lock().unwrap());
        permission
    }

    pub(crate) fn resolve_sandbox(&self, working_dir: &std::path::Path) -> anyhow::Result<Sandbox> {
        if let Some(sandbox) = &self.sandbox {
            return Ok(sandbox.clone());
        }
        Sandbox::new(true, working_dir)
    }

    pub(crate) fn interactive_enabled(&self) -> bool {
        self.interactive
    }

    pub(crate) fn mcp_manager_ref(&self) -> Option<&crate::extension::mcp::McpClientManager> {
        self.mcp_manager.as_ref()
    }
}

#[cfg(test)]
mod service_tests {
    use super::PhiRuntimeServices;
    use crate::permission::{Action, PermissionConfig, ToolPerm};

    #[tokio::test]
    async fn default_services_construct_an_explicit_open_permission_policy() {
        let services = PhiRuntimeServices::new();
        let permission = services.prepare_permission(None);
        assert!(
            permission
                .authorize_tool("custom", "input", None, None)
                .await
                .is_ok()
        );
        assert!(
            services
                .isolated_session()
                .prepare_permission(None)
                .authorize_tool("custom", "input", None, None)
                .await
                .is_ok()
        );
    }

    #[test]
    fn defaults_to_a_sandbox() {
        let working_dir = std::env::temp_dir();
        let sandbox = PhiRuntimeServices::new()
            .resolve_sandbox(&working_dir)
            .unwrap();
        assert!(sandbox.is_enabled());
        let command = sandbox.wrap_program("sh", &[]);
        assert!(
            command
                .as_std()
                .get_args()
                .any(|arg| arg == working_dir.as_os_str())
        );
    }

    #[tokio::test]
    async fn isolated_sessions_do_not_share_permission_allowlists() {
        let services = PhiRuntimeServices::new().permission_policy(
            PermissionConfig {
                read: Some(ToolPerm::Simple(Action::Ask)),
                ..PermissionConfig::default()
            },
            None,
        );
        services.allow_session_tool("read", "shared.txt");
        let isolated = services.isolated_session();

        assert!(
            services
                .prepare_permission(None)
                .authorize_tool("read", "shared.txt", None, None)
                .await
                .is_ok()
        );
        assert!(
            isolated
                .prepare_permission(None)
                .authorize_tool("read", "shared.txt", None, None)
                .await
                .is_err()
        );
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PhiBuildError {
    #[error("invalid hook registration: {0}")]
    InvalidHook(#[from] HookError),
    #[error("provider is required")]
    MissingProvider,
    #[error("model is required")]
    MissingModel,
    #[error("invalid agent `{name}`: {reason}")]
    InvalidAgent {
        name: String,
        reason: AgentValidationReason,
    },
    #[error("duplicate agent `{name}`: {existing} conflicts with {duplicate}")]
    DuplicateAgent {
        name: String,
        existing: AgentDefinitionOrigin,
        duplicate: AgentDefinitionOrigin,
    },
}
impl From<AgentCatalogError> for PhiBuildError {
    fn from(error: AgentCatalogError) -> Self {
        match error {
            AgentCatalogError::InvalidAgent { name, reason } => Self::InvalidAgent { name, reason },
            AgentCatalogError::DuplicateAgent {
                name,
                existing,
                duplicate,
            } => Self::DuplicateAgent {
                name,
                existing,
                duplicate,
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PhiQueryError {
    #[error(transparent)]
    InstanceBuild(#[from] AgentInstanceBuildError),
    #[error(transparent)]
    Turn(#[from] PhiTurnError),
    #[error("query was not started: {reason}")]
    NotStarted { reason: String },
    #[error("query execution failed: {0}")]
    Execution(#[source] PhiFailure),
    #[error(transparent)]
    Stream(#[from] PhiStreamError),
}
