use crate::core::event::ToolDisplay;
use crate::core::lifecycle::{
    LifecycleEvent, LifecycleEventKind, ResourcesDiscoverReason, SessionTreeReason,
    TaskCompletedEvent, TaskCreatedEvent, TaskGateOutcome, TeammateIdleEvent, TeammateIdleOutcome,
};
use crate::hooks::{
    HookControl, HookError, HookHandler, HookHandlerFuture, HookResponse, TaskGateFuture,
    TeammateIdleFuture,
};
use crate::tools::{ToolAuthority, ToolDisplayFormatter, ToolMetadata, ToolRegistration};
pub use process::{
    ExtensionHookPolicy, ExtensionProcessFuture, ExtensionProcessOutput, ExtensionProcessPolicy,
    ExtensionProcessRequest, ExtensionProcessRunner, ExtensionRuntimeServices,
    LocalExtensionProcessRunner,
};
use registry::ExtensionRegistry;
pub use registry::{CommandArgument, CommandArguments, CommandTarget, RegisteredCommand};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct ExtensionToolSpec {
    pub(crate) extension: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) parameters: serde_json::Value,
    pub(crate) override_existing: bool,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub(crate) struct ExtensionId(Arc<str>);

impl ExtensionId {
    pub(crate) fn lua(id: &str) -> Self {
        Self(format!("lua:{id}").into())
    }

    pub(crate) fn mcp() -> Self {
        Self("mcp".into())
    }
}

impl AsRef<str> for ExtensionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

pub(crate) struct ExtensionToolRegistration {
    pub(crate) owner: ExtensionId,
    pub(crate) namespace: Option<String>,
    pub(crate) tool: rig::tool::DynamicTool,
    pub(crate) metadata: ToolMetadata,
    pub(crate) capability: String,
    pub(crate) authority: Arc<dyn ToolAuthority>,
    pub(crate) override_existing: bool,
}

impl ExtensionToolRegistration {
    pub(crate) fn with_metadata(mut self, metadata: ToolMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub(crate) fn delegated(
        owner: ExtensionId,
        tool: rig::tool::DynamicTool,
        capability: String,
        authority: Arc<dyn ToolAuthority>,
    ) -> Self {
        Self {
            owner,
            namespace: None,
            tool,
            metadata: ToolMetadata::default(),
            capability,
            authority,
            override_existing: false,
        }
    }
}

mod builtin;
mod config;
pub(crate) use builtin::delegated_tool_denials;
pub use builtin::{BuiltinExtension, BuiltinSkill, builtin_command_names, builtin_skills};
pub(crate) use builtin::{BuiltinExtensions, ExtensionToolContext};
pub use config::ExtensionConfig;
pub(crate) mod codemode;
mod lua;
pub mod mcp;
pub mod memory;
mod process;
mod registry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ExtensionKeybindingId {
    runtime: u64,
    binding: u64,
}

impl ExtensionKeybindingId {
    pub(crate) fn new(runtime: u64, binding: u64) -> Self {
        Self { runtime, binding }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RegisteredKeybinding {
    pub id: ExtensionKeybindingId,
    pub context: String,
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtensionEffect {
    SpawnProcess {
        extension: String,
        source: String,
        context: String,
        program: String,
        args: Vec<String>,
        sandboxed: bool,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionOutcome {
    pub notifications: Vec<String>,
    pub effects: Vec<ExtensionEffect>,
}

pub type ExtensionSubmissionFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = anyhow::Result<crate::turn::SubmissionDisposition>> + Send,
    >,
>;

/// Submission capability bound to one instance and runtime generation.
pub trait ExtensionPromptSubmitter: Send + Sync {
    fn submit(
        &self,
        prompt: String,
        dispatch: Option<crate::turn::SubmissionDispatch>,
    ) -> ExtensionSubmissionFuture;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "status", rename_all = "snake_case")]
pub enum ExtensionCommandOutcome {
    Completed(Option<String>),
    Rejected(String),
}

pub struct AdapterDeclarations<'a> {
    runtime: &'a ExtensionRuntime,
}

impl AdapterDeclarations<'_> {
    pub fn commands(&self) -> &[RegisteredCommand] {
        self.runtime.commands()
    }

    pub fn keybindings(&self) -> &[RegisteredKeybinding] {
        self.runtime.keybindings()
    }

    pub fn skills(&self) -> &[BuiltinSkill] {
        self.runtime.registry.skills()
    }
}

struct LuaSource {
    path: PathBuf,
    canonical_path: PathBuf,
    content: String,
}

/// Immutable extension content shared across isolated runtime generations.
#[derive(Clone, Default)]
pub struct ExtensionSources {
    sources: Vec<Arc<LuaSource>>,
}

impl ExtensionSources {
    pub fn capture(paths: Vec<PathBuf>) -> anyhow::Result<Self> {
        let mut sources = Vec::new();
        for path in paths {
            let files = if path.is_dir() {
                let mut files = std::fs::read_dir(&path)?
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|extension| extension == "lua"))
                    .collect::<Vec<_>>();
                files.sort();
                files
            } else if path.is_file() {
                vec![path]
            } else {
                Vec::new()
            };
            for path in files {
                sources.push(Arc::new(LuaSource {
                    content: std::fs::read_to_string(&path)?,
                    canonical_path: path.canonicalize()?,
                    path,
                }));
            }
        }
        Ok(Self { sources })
    }

    pub fn load(&self, services: ExtensionRuntimeServices) -> anyhow::Result<ExtensionRuntime> {
        let lua = lua::LuaRuntime::from_sources(self.sources.clone(), services)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(ExtensionRuntime::from_lua(Some(lua)))
    }
}

pub struct ExtensionRuntime {
    registry: ExtensionRegistry,
    lua: Option<lua::LuaRuntime>,
    builtins: builtin::BuiltinExtensions,
}

impl ExtensionRuntime {
    pub fn has_process_hooks(&self) -> bool {
        self.lua
            .as_ref()
            .is_some_and(lua::LuaRuntime::has_process_hooks)
    }

    pub fn load() -> Self {
        Self::load_with_paths(Vec::new())
    }

    pub fn load_with_paths(paths: Vec<PathBuf>) -> Self {
        match Self::try_load_with_paths(paths) {
            Ok(runtime) => runtime,
            Err(error) => {
                tracing::warn!(%error, "extension load failed");
                Self::empty()
            }
        }
    }

    pub fn try_load_with_paths(paths: Vec<PathBuf>) -> anyhow::Result<Self> {
        Self::try_load_with_paths_and_services(paths, ExtensionRuntimeServices::new())
    }

    pub fn try_load_with_paths_and_services(
        paths: Vec<PathBuf>,
        services: ExtensionRuntimeServices,
    ) -> anyhow::Result<Self> {
        ExtensionSources::capture(paths)?.load(services)
    }

    /// Drains explicit notifications from this runtime's hooks, tools and display handlers.
    pub fn take_notifications(&self) -> anyhow::Result<Vec<String>> {
        match &self.lua {
            Some(lua) => lua
                .take_notifications()
                .map_err(|error| anyhow::anyhow!(error.to_string())),
            None => Ok(Vec::new()),
        }
    }

    pub fn sources(&self) -> ExtensionSources {
        ExtensionSources {
            sources: self
                .lua
                .as_ref()
                .map(|lua| lua.sources())
                .unwrap_or_default(),
        }
    }

    fn empty() -> Self {
        Self::from_lua(None)
    }

    fn from_lua(lua: Option<lua::LuaRuntime>) -> Self {
        let mut registry = ExtensionRegistry::default();
        if let Some(lua) = &lua {
            registry.extend_commands(lua.commands());
        }
        Self {
            registry,
            lua,
            builtins: builtin::BuiltinExtensions::default(),
        }
    }

    pub(crate) fn tool_specs(&self) -> Vec<ExtensionToolSpec> {
        self.lua
            .as_ref()
            .map(lua::LuaRuntime::tool_specs)
            .unwrap_or_default()
    }

    pub(crate) fn call_tool(&self, name: &str, args: String) -> anyhow::Result<String> {
        let lua = self
            .lua
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("extension tool runtime is unavailable"))?;
        lua.call_tool(name, args)
            .map_err(|error| anyhow::anyhow!(error.to_string()))
    }

    pub(crate) fn set_builtins(
        &mut self,
        builtins: BuiltinExtensions,
        config: &crate::config::RuntimeConfig,
    ) -> anyhow::Result<()> {
        self.registry.set_builtin_declarations(config)?;
        self.builtins = builtins;
        Ok(())
    }

    pub(crate) fn prepare_generation(
        &self,
        permission: crate::permission::runtime::PermissionRuntime,
    ) -> anyhow::Result<Self> {
        let lua = self
            .lua
            .as_ref()
            .map(|lua| lua.prepare_generation(permission))
            .transpose()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(Self::from_lua(lua))
    }

    pub fn adapter_declarations(&self) -> AdapterDeclarations<'_> {
        AdapterDeclarations { runtime: self }
    }

    fn commands(&self) -> &[RegisteredCommand] {
        self.registry.commands()
    }

    pub fn has_command(&self, name: &str) -> bool {
        self.commands().iter().any(|command| command.name == name)
    }

    fn keybindings(&self) -> &[RegisteredKeybinding] {
        if let Some(lua) = &self.lua {
            return lua.keybindings();
        }
        &[]
    }

    pub fn run_keybinding(
        &self,
        id: ExtensionKeybindingId,
        event: &serde_json::Value,
    ) -> anyhow::Result<ExtensionOutcome> {
        if let Some(lua) = &self.lua {
            return lua
                .run_keybinding(id, event)
                .map_err(|err| anyhow::anyhow!(err.to_string()));
        }
        Ok(ExtensionOutcome::default())
    }

    pub async fn authorize_effect(&self, effect: &ExtensionEffect) -> anyhow::Result<()> {
        if let Some(lua) = &self.lua {
            let ExtensionEffect::SpawnProcess {
                extension,
                source,
                context,
                program,
                args,
                ..
            } = effect;
            if !lua.services().source_trusted(std::path::Path::new(source)) {
                anyhow::bail!(
                    "extension '{extension}' source is not trusted for process execution"
                );
            }
            return lua
                .services()
                .authorize_process(extension, context, program, args)
                .await;
        }
        Ok(())
    }

    pub async fn run_command(
        &self,
        name: &str,
        args: &str,
    ) -> anyhow::Result<Option<ExtensionCommandOutcome>> {
        let (_idle_tx, idle_rx) = tokio::sync::watch::channel(true);
        self.run_command_with_idle(name, args, idle_rx).await
    }

    pub async fn run_command_with_idle(
        &self,
        name: &str,
        args: &str,
        idle: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<Option<ExtensionCommandOutcome>> {
        self.run_command_context(name, args, idle, None).await
    }

    pub async fn run_command_with_agent(
        &self,
        name: &str,
        args: &str,
        idle: tokio::sync::watch::Receiver<bool>,
        agent: Arc<dyn ExtensionPromptSubmitter>,
    ) -> anyhow::Result<Option<ExtensionCommandOutcome>> {
        self.run_command_context(name, args, idle, Some(agent))
            .await
    }

    async fn run_command_context(
        &self,
        name: &str,
        args: &str,
        idle: tokio::sync::watch::Receiver<bool>,
        agent: Option<Arc<dyn ExtensionPromptSubmitter>>,
    ) -> anyhow::Result<Option<ExtensionCommandOutcome>> {
        if let Some(lua) = &self.lua
            && self
                .commands()
                .iter()
                .any(|command| command.name == name && command.target == CommandTarget::Handler)
        {
            return lua
                .run_command(name, args, idle, agent)
                .await
                .map_err(|err| anyhow::anyhow!(err.to_string()));
        }
        Ok(None)
    }

    pub async fn discover_resources(&self, reason: ResourcesDiscoverReason) {
        let working_dir = std::env::current_dir().ok();
        self.discover_resources_at(reason, working_dir.as_deref())
            .await;
    }

    pub async fn discover_resources_for(
        &self,
        reason: ResourcesDiscoverReason,
        working_dir: &std::path::Path,
    ) {
        self.discover_resources_at(reason, Some(working_dir)).await;
    }

    async fn discover_resources_at(
        &self,
        reason: ResourcesDiscoverReason,
        working_dir: Option<&std::path::Path>,
    ) {
        self.emit_lifecycle_event(LifecycleEvent::with_payload(
            LifecycleEventKind::ResourcesDiscover,
            serde_json::json!({
                "cwd": working_dir.map(|path| path.display().to_string()),
                "reason": reason.as_str(),
            }),
        ))
        .await;
    }

    pub async fn session_tree_changed(
        &self,
        reason: SessionTreeReason,
        target_id: &str,
        old_leaf_id: Option<&str>,
        new_leaf_id: Option<&str>,
        editor_text: bool,
    ) {
        self.emit_lifecycle_event(LifecycleEvent::with_payload(
            LifecycleEventKind::SessionTree,
            serde_json::json!({
                "reason": reason.as_str(), "target_id": target_id,
                "old_leaf_id": old_leaf_id, "new_leaf_id": new_leaf_id,
                "editor_text": editor_text,
            }),
        ))
        .await;
    }

    pub async fn before_command(&self, command: &str) -> Option<String> {
        self.emit_lifecycle_event(LifecycleEvent::with_payload(
            LifecycleEventKind::CommandBefore,
            serde_json::json!({ "command": command }),
        ))
        .await
        .filter(HookResponse::should_stop)
        .and_then(|result| result.reason)
    }

    pub async fn after_command(&self, command: &str, handled: bool) {
        self.emit_lifecycle_event(LifecycleEvent::with_payload(
            LifecycleEventKind::CommandAfter,
            serde_json::json!({ "command": command, "handled": handled }),
        ))
        .await;
    }

    pub async fn ui_rendered(&self) {
        self.emit_lifecycle_event(LifecycleEvent::new(LifecycleEventKind::UiRender))
            .await;
    }

    async fn emit_lifecycle_event(&self, event: LifecycleEvent) -> Option<HookResponse> {
        let Some(lua) = &self.lua else {
            return None;
        };
        match lua.emit(&event).await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(kind = event.kind.as_str(), %error, "extension lifecycle failed");
                None
            }
        }
    }
}

pub(crate) async fn collect_tool_registrations(
    runtime: Option<&Arc<ExtensionRuntime>>,
    context: ExtensionToolContext<'_>,
) -> Vec<ToolRegistration> {
    let defaults = builtin::BuiltinExtensions::default();
    let builtins = runtime
        .map(|runtime| &runtime.builtins)
        .unwrap_or(&defaults);
    let mut tools = builtins.tools(context).await;
    if let Some(runtime) = runtime {
        tools.extend(
            crate::agent::tools::lua::agent_tools(runtime.clone())
                .into_iter()
                .map(Into::into),
        );
    }
    tools
}

impl ToolAuthority for ExtensionRuntime {
    fn is_available(&self, capability: &str) -> bool {
        let Some((extension, tool_name)) = capability.split_once('\0') else {
            return false;
        };
        self.tool_specs()
            .iter()
            .any(|spec| spec.extension == extension && spec.name == tool_name)
    }

    fn ensure_available(&self, capability: &str) -> Result<(), String> {
        self.is_available(capability)
            .then_some(())
            .ok_or_else(|| "extension tool capability is unavailable".to_string())
    }
}

impl ToolDisplayFormatter for ExtensionRuntime {
    fn format_tool_display(&self, name: &str, args: &str) -> anyhow::Result<Option<ToolDisplay>> {
        if let Some(lua) = &self.lua
            && let Some(display) = lua
                .format_tool_display(name, args)
                .map_err(|err| anyhow::anyhow!(err.to_string()))?
        {
            return Ok(Some(display));
        }
        self.builtins.format_tool_display(name, args)
    }
}

impl ExtensionRuntime {
    pub(crate) fn register_hooks(self: &Arc<Self>, bus: &mut crate::hooks::HookRegistry) {
        if let Some(lua) = &self.lua {
            lua.set_hook_timeout(bus.callback_timeout());
            for (name, index) in lua.hook_slots() {
                bus.register_handler(Arc::new(ExtensionHook {
                    runtime: self.clone(),
                    control: lua.hook_control(&name, index),
                    name,
                    index,
                }));
            }
        }
    }
}

struct ExtensionHook {
    runtime: Arc<ExtensionRuntime>,
    control: HookControl,
    name: String,
    index: usize,
}

impl HookHandler for ExtensionHook {
    fn control(&self) -> Option<&HookControl> {
        Some(&self.control)
    }
    fn timeout(&self, default: std::time::Duration) -> Option<std::time::Duration> {
        Some(self.control.timeout(default))
    }
    fn is_generation_bound(&self) -> bool {
        true
    }
    fn teammate_idle<'a>(&'a self, event: &'a TeammateIdleEvent) -> TeammateIdleFuture<'a> {
        Box::pin(async move {
            if self.name == "TeammateIdle"
                && let Some(lua) = &self.runtime.lua
            {
                return lua
                    .idle_at(event, self.index)
                    .await
                    .map_err(|error| HookError::new(error.to_string()));
            }
            Ok(TeammateIdleOutcome::Allow)
        })
    }

    fn has_agent_turn_end(&self) -> bool {
        self.name == "AgentTurnEnd"
    }

    fn has_task_created(&self) -> bool {
        self.name == "TaskCreated"
    }

    fn has_task_completed(&self) -> bool {
        self.name == "TaskCompleted"
    }

    fn task_created<'a>(&'a self, event: &'a TaskCreatedEvent) -> TaskGateFuture<'a> {
        Box::pin(async move {
            if self.has_task_created()
                && let Some(lua) = &self.runtime.lua
            {
                return lua
                    .task_gate_at("TaskCreated", event, self.index)
                    .await
                    .map_err(|error| HookError::new(error.to_string()));
            }
            Ok(TaskGateOutcome::Allow)
        })
    }

    fn task_completed<'a>(&'a self, event: &'a TaskCompletedEvent) -> TaskGateFuture<'a> {
        Box::pin(async move {
            if self.has_task_completed()
                && let Some(lua) = &self.runtime.lua
            {
                return lua
                    .task_gate_at("TaskCompleted", event, self.index)
                    .await
                    .map_err(|error| HookError::new(error.to_string()));
            }
            Ok(TaskGateOutcome::Allow)
        })
    }

    fn handle<'a>(&'a self, event: &'a LifecycleEvent) -> HookHandlerFuture<'a> {
        Box::pin(async move {
            if event.kind.as_str() == self.name
                && let Some(lua) = &self.runtime.lua
            {
                return lua
                    .emit_at(event, self.index)
                    .await
                    .map_err(|err| anyhow::anyhow!(err.to_string()));
            }
            Ok(None)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::runtime::PermissionRuntime;
    use crate::permission::{PermissionConfig, PermissionMode};

    #[tokio::test]
    async fn lifecycle_process_capability_controls_tool_input() {
        let path = std::env::temp_dir().join(format!(
            "phi-tool-control-extension-{}.lua",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(
            &path,
            r#"
register_extension({
  name = "tool-control",
  capabilities = { process = { PreToolUse = { "printf" } } },
})
on("PreToolUse", function(_event, ctx)
  local output = ctx.process.run("printf", { "rewritten" })
  return { updated_input = { command = output.stdout } }
end)
"#,
        )
        .unwrap();
        let services = ExtensionRuntimeServices::new()
            .process_runner(Arc::new(LocalExtensionProcessRunner::new(
                crate::sandbox::Sandbox::new(false, std::env::temp_dir()).unwrap(),
            )))
            .process_policy(ExtensionProcessPolicy::default().trust_root(std::env::temp_dir()))
            .permission(PermissionRuntime::new(Arc::new(Mutex::new(
                PermissionChecker::new(
                    &PermissionConfig {
                        mode: PermissionMode::Allow,
                        ..PermissionConfig::default()
                    },
                    None,
                ),
            ))));
        let runtime = Arc::new(
            ExtensionRuntime::try_load_with_paths_and_services(vec![path.clone()], services)
                .unwrap(),
        );
        let mut hooks = crate::hooks::HookRegistry::new();
        runtime.register_hooks(&mut hooks);

        let result = crate::hooks::emit_pre_tool_use(
            &hooks,
            crate::core::lifecycle::ToolUseEvent {
                tool_name: "Bash".to_string(),
                tool_use_id: "call".to_string(),
                tool_input: serde_json::json!({ "command": "original" }),
            },
        )
        .await;

        let _ = std::fs::remove_file(path);
        assert_eq!(result.updated_input.unwrap()["command"], "rewritten");
    }

    #[tokio::test]
    async fn builtin_declarations_share_adapter_surface_without_lua_dispatch() {
        let mut runtime = ExtensionRuntime::load();
        let config = crate::config::RuntimeConfig {
            extensions: ExtensionConfig {
                memory: Some(memory::MemoryConfig::default()),
                ..Default::default()
            },
            ..Default::default()
        };
        runtime
            .set_builtins(BuiltinExtensions::default(), &config)
            .unwrap();
        let declarations = runtime.adapter_declarations();
        assert!(declarations.commands().iter().any(|command| {
            command.name == "memory-inject" && command.target == CommandTarget::Prompt
        }));
        assert_eq!(declarations.skills(), builtin_skills(&config));
        assert!(runtime.has_command("memory-check"));
        assert_eq!(runtime.run_command("memory-check", "").await.unwrap(), None);
    }

    #[test]
    fn builtin_command_collision_rejects_candidate_declarations() {
        let mut runtime = ExtensionRuntime::load();
        runtime.registry.extend_commands(vec![RegisteredCommand {
            name: "memory-inject".into(),
            description: None,
            source: "custom".into(),
            target: CommandTarget::Handler,
            arguments: Some(Default::default()),
        }]);
        let config = crate::config::RuntimeConfig {
            extensions: ExtensionConfig {
                memory: Some(memory::MemoryConfig::default()),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut disabled = config.clone();
        disabled
            .extensions
            .disabled
            .insert(BuiltinExtension::Memory);
        for config in [crate::config::RuntimeConfig::default(), disabled, config] {
            let error = runtime
                .set_builtins(BuiltinExtensions::default(), &config)
                .unwrap_err();
            assert!(error.to_string().contains("memory-inject"));
            assert_eq!(runtime.adapter_declarations().commands().len(), 1);
            assert!(runtime.adapter_declarations().skills().is_empty());
        }
    }

    #[test]
    fn fallible_load_reports_invalid_extension_source() {
        let path = std::env::temp_dir().join(format!(
            "phi-invalid-extension-{}.lua",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "this is not valid lua").unwrap();

        let result = ExtensionRuntime::try_load_with_paths(vec![path.clone()]);

        let _ = std::fs::remove_file(path);
        assert!(result.is_err());
    }
}
