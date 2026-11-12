use super::{
    ExtensionEffect, ExtensionKeybindingId, ExtensionOutcome, ExtensionProcessOutput,
    ExtensionProcessRequest, ExtensionRuntimeServices, ExtensionToolSpec, LuaSource,
    RegisteredCommand, RegisteredKeybinding,
};
use crate::core::lifecycle::{
    HookAction, LifecycleEvent, LifecycleEventKind, PermissionDecision, PostToolBatchResult,
    PostToolUseResult, PreToolUseResult, TaskGateOutcome, TeammateIdleEvent, TeammateIdleOutcome,
};
use crate::hooks::{HookControl, HookMatcher, HookOptions, HookOutput, HookResponse};
use crate::tools::canonical_tool_name;
use crate::utils::lua::{
    create_context, isolated_lua, json_to_lua, lua_table_to_json, lua_to_json,
};
use mlua::{Function, Lua, RegistryKey, Table, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_RUNTIME_ID: AtomicU64 = AtomicU64::new(1);
const DEFAULT_PROCESS_TIMEOUT_MS: u64 = 2_000;

pub struct LuaRuntime {
    lua: Lua,
    runtime_id: u64,
    services: ExtensionRuntimeServices,
    hooks: HashMap<String, Vec<LuaHandler>>,
    hook_controls: HashMap<String, Vec<HookControl>>,
    hook_timeout: Mutex<std::time::Duration>,
    commands: HashMap<String, LuaCommand>,
    tools: HashMap<String, LuaTool>,
    keybindings: Vec<RegisteredKeybinding>,
    keybinding_handlers: HashMap<ExtensionKeybindingId, LuaHandler>,
    tool_displays: HashMap<String, RegistryKey>,
    sources: Vec<Arc<LuaSource>>,
    pending_notifications: Arc<Mutex<Vec<String>>>,
}

struct LuaHandler {
    extension: String,
    source: PathBuf,
    process_programs: HashSet<String>,
    handler: RegistryKey,
}

struct LuaCommand {
    description: Option<String>,
    arguments: Option<super::CommandArguments>,
    registration: LuaHandler,
}

struct LuaTool {
    spec: ExtensionToolSpec,
    handler: RegistryKey,
}

impl LuaRuntime {
    #[cfg(test)]
    pub fn load(paths: Vec<PathBuf>) -> mlua::Result<Self> {
        Self::load_with_services(paths, ExtensionRuntimeServices::new())
    }

    #[cfg(test)]
    pub fn load_with_services(
        paths: Vec<PathBuf>,
        services: ExtensionRuntimeServices,
    ) -> mlua::Result<Self> {
        let sources = super::ExtensionSources::capture(paths).map_err(mlua::Error::external)?;
        Self::from_sources(sources.sources, services)
    }

    pub(super) fn from_sources(
        sources: Vec<Arc<LuaSource>>,
        services: ExtensionRuntimeServices,
    ) -> mlua::Result<Self> {
        let mut runtime = Self::new(services)?;
        for source in &sources {
            runtime.load_source(source)?;
        }
        runtime.sources = sources;
        runtime.collect_registrations()?;
        Ok(runtime)
    }

    pub(super) fn sources(&self) -> Vec<Arc<LuaSource>> {
        self.sources.clone()
    }

    fn new(services: ExtensionRuntimeServices) -> mlua::Result<Self> {
        let lua = isolated_lua()?;
        install_api(&lua)?;
        Ok(Self {
            lua,
            runtime_id: NEXT_RUNTIME_ID.fetch_add(1, Ordering::Relaxed),
            services,
            hooks: HashMap::new(),
            hook_controls: HashMap::new(),
            hook_timeout: Mutex::new(crate::hooks::HookTimeouts::default().hook_timeout),
            commands: HashMap::new(),
            tools: HashMap::new(),
            keybindings: Vec::new(),
            keybinding_handlers: HashMap::new(),
            tool_displays: HashMap::new(),
            sources: Vec::new(),
            pending_notifications: Arc::default(),
        })
    }

    fn collect_registrations(&mut self) -> mlua::Result<()> {
        self.collect_hooks()?;
        if !self.services.hooks_enabled() {
            self.hooks.clear();
            self.hook_controls.clear();
        }
        self.collect_commands()?;
        self.collect_tools()?;
        self.collect_keybindings()?;
        self.collect_tool_displays()
    }

    pub async fn emit(&self, event: &LifecycleEvent) -> mlua::Result<Option<HookResponse>> {
        self.call_hooks(event, None).await
    }

    pub(super) fn hook_slots(&self) -> Vec<(String, usize)> {
        let mut names = self.hooks.keys().collect::<Vec<_>>();
        names.sort();
        names
            .into_iter()
            .flat_map(|name| (0..self.hooks[name].len()).map(move |index| (name.clone(), index)))
            .collect()
    }

    pub(super) fn set_hook_timeout(&self, timeout: std::time::Duration) {
        *self.hook_timeout.lock().unwrap() = timeout;
    }

    pub(super) fn hook_control(&self, name: &str, index: usize) -> HookControl {
        self.hook_controls[name][index].clone()
    }

    pub(super) async fn emit_at(
        &self,
        event: &LifecycleEvent,
        index: usize,
    ) -> mlua::Result<Option<HookResponse>> {
        self.call_hooks(event, Some(index)).await
    }

    pub(super) async fn idle_at(
        &self,
        event: &TeammateIdleEvent,
        index: usize,
    ) -> mlua::Result<TeammateIdleOutcome> {
        match self.call_gate("TeammateIdle", event, index).await? {
            Value::Nil => Ok(TeammateIdleOutcome::Allow),
            Value::Table(table) => {
                let mut action = None;
                let mut feedback = None;
                for pair in table.pairs::<Value, Value>() {
                    let (Value::String(key), Value::String(value)) = pair? else {
                        return Err(mlua::Error::runtime(
                            "TeammateIdle decision fields must be strings",
                        ));
                    };
                    match key.to_str()?.as_ref() {
                        "action" => action = Some(value.to_str()?.to_owned()),
                        "feedback" => feedback = Some(value.to_str()?.to_owned()),
                        _ => return Err(mlua::Error::runtime("unknown TeammateIdle field")),
                    }
                }
                match (action.as_deref(), feedback) {
                    (Some("allow"), None) => Ok(TeammateIdleOutcome::Allow),
                    (Some("continue"), Some(feedback)) => {
                        Ok(TeammateIdleOutcome::Continue { feedback })
                    }
                    (Some("stop"), None) => Ok(TeammateIdleOutcome::Stop),
                    _ => Err(mlua::Error::runtime("invalid TeammateIdle decision")),
                }
            }
            _ => Err(mlua::Error::runtime("invalid TeammateIdle result")),
        }
    }

    pub(super) async fn task_gate_at(
        &self,
        name: &str,
        event: &impl serde::Serialize,
        index: usize,
    ) -> mlua::Result<TaskGateOutcome> {
        match self.call_gate(name, event, index).await? {
            Value::Nil => Ok(TaskGateOutcome::Allow),
            Value::Table(table) => {
                let mut rejected = false;
                let mut reason = None;
                for pair in table.pairs::<Value, Value>() {
                    let (Value::String(key), value) = pair? else {
                        return Err(mlua::Error::runtime(
                            "Task gate result keys must be strings",
                        ));
                    };
                    match (key.to_str()?.as_ref(), value) {
                        ("block" | "cancel", Value::Boolean(value)) => rejected |= value,
                        ("reason", Value::String(value)) => {
                            reason = Some(value.to_str()?.to_owned())
                        }
                        _ => {
                            return Err(mlua::Error::runtime("invalid Task gate decision field"));
                        }
                    }
                }
                Ok(if rejected {
                    TaskGateOutcome::Reject {
                        reason: reason.unwrap_or_else(|| format!("{name} rejected")),
                    }
                } else {
                    TaskGateOutcome::Allow
                })
            }
            _ => Err(mlua::Error::runtime(
                "Task gate must return nil or a decision table",
            )),
        }
    }

    async fn call_gate(
        &self,
        name: &str,
        event: &impl serde::Serialize,
        index: usize,
    ) -> mlua::Result<Value> {
        let hook = &self.hooks[name][index];
        let payload = serde_json::to_value(event).map_err(mlua::Error::external)?;
        if !self.hook_controls[name][index].start_payload(None, &payload) {
            return Ok(Value::Nil);
        }
        let event_table = self.lua.create_table()?;
        event_table.set("name", name)?;
        event_table.set("type", name)?;
        event_table.set("payload", json_to_lua(&self.lua, &payload)?)?;
        let handler: Function = self.lua.registry_value(&hook.handler)?;
        let (ctx, capability_lease) = self.create_handler_context(
            hook,
            name,
            ProcessInvocation::Hook,
            self.pending_notifications.clone(),
        )?;
        let result = handler.call_async((event_table, ctx)).await;
        drop(capability_lease);
        result
    }

    pub(super) fn has_process_hooks(&self) -> bool {
        self.hooks
            .values()
            .flatten()
            .any(|handler| !handler.process_programs.is_empty())
    }

    pub(crate) fn services(&self) -> &ExtensionRuntimeServices {
        &self.services
    }

    pub(super) fn prepare_generation(
        &self,
        permission: crate::permission::runtime::PermissionRuntime,
    ) -> mlua::Result<Self> {
        Self::from_sources(
            self.sources.clone(),
            self.services.clone().permission(permission),
        )
    }

    pub(super) fn take_notifications(&self) -> mlua::Result<Vec<String>> {
        take_locked(&self.pending_notifications, "notification")
    }

    fn finish_notifications(
        &self,
        result: mlua::Result<()>,
        notifications: &Mutex<Vec<String>>,
    ) -> mlua::Result<Vec<String>> {
        let notifications = take_locked(notifications, "notification")?;
        match result {
            Ok(()) => Ok(notifications),
            Err(error) => {
                self.pending_notifications
                    .lock()
                    .map_err(|_| mlua::Error::runtime("notification lock poisoned"))?
                    .extend(notifications);
                Err(error)
            }
        }
    }

    pub fn commands(&self) -> Vec<RegisteredCommand> {
        self.commands
            .iter()
            .map(|(name, command)| RegisteredCommand {
                name: name.clone(),
                description: command.description.clone(),
                source: command.registration.extension.clone(),
                target: super::CommandTarget::Handler,
                arguments: command.arguments.clone(),
            })
            .collect()
    }

    pub fn tool_specs(&self) -> Vec<ExtensionToolSpec> {
        let mut specs = self
            .tools
            .values()
            .map(|tool| tool.spec.clone())
            .collect::<Vec<_>>();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        specs
    }

    pub fn call_tool(&self, name: &str, args: String) -> mlua::Result<String> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| mlua::Error::runtime(format!("unknown extension tool: {name}")))?;
        let handler: Function = self.lua.registry_value(&tool.handler)?;
        let args = serde_json::from_str::<serde_json::Value>(&args)
            .unwrap_or(serde_json::Value::String(args));
        let ctx = create_context(&self.lua, self.pending_notifications.clone())?;
        let output: Value = handler.call((json_to_lua(&self.lua, &args)?, ctx))?;
        Ok(match lua_to_json(output)? {
            serde_json::Value::String(text) => text,
            value => value.to_string(),
        })
    }

    pub fn keybindings(&self) -> &[RegisteredKeybinding] {
        &self.keybindings
    }

    pub fn run_keybinding(
        &self,
        id: ExtensionKeybindingId,
        event: &serde_json::Value,
    ) -> mlua::Result<ExtensionOutcome> {
        let Some(handler) = self.keybinding_handlers.get(&id) else {
            return Err(mlua::Error::external(format!(
                "unknown extension keybinding id: {id:?}"
            )));
        };
        let handler_function: Function = self.lua.registry_value(&handler.handler)?;
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let effects = Arc::new(Mutex::new(Vec::new()));
        let ctx = create_keybinding_context(
            &self.lua,
            handler,
            &self.services,
            notifications.clone(),
            effects.clone(),
        )?;
        let result = handler_function.call::<()>((json_to_lua(&self.lua, event)?, ctx));
        Ok(ExtensionOutcome {
            notifications: self.finish_notifications(result, &notifications)?,
            effects: take_locked(&effects, "effect")?,
        })
    }

    pub fn format_tool_display(
        &self,
        name: &str,
        args: &str,
    ) -> mlua::Result<Option<crate::core::event::ToolDisplay>> {
        let Some(handler) = self.tool_displays.get(name) else {
            return Ok(None);
        };
        let input: serde_json::Value =
            serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
        let handler: Function = self.lua.registry_value(handler)?;
        let ctx = create_context(&self.lua, self.pending_notifications.clone())?;
        let value: Value = handler.call((json_to_lua(&self.lua, &input)?, ctx))?;
        match value {
            Value::String(title) => Ok(Some(crate::core::event::ToolDisplay::title(
                title.to_str()?.to_string(),
            ))),
            Value::Table(table) => Ok(Some(crate::core::event::ToolDisplay {
                title: table.get::<Option<String>>("title")?,
                input: table.get::<Option<String>>("input")?,
                title_suffix: table.get::<Option<String>>("title_suffix")?,
                expand: table.get::<Option<bool>>("expand")?.unwrap_or(false),
                content: table.get::<Option<String>>("content")?,
            })),
            _ => Ok(None),
        }
    }

    pub async fn run_command(
        &self,
        name: &str,
        args: &str,
        idle: tokio::sync::watch::Receiver<bool>,
        agent: Option<Arc<dyn super::ExtensionPromptSubmitter>>,
    ) -> mlua::Result<Option<super::ExtensionCommandOutcome>> {
        let Some(command) = self.commands.get(name) else {
            return Ok(None);
        };
        if command.arguments.is_none() && !args.trim().is_empty() {
            return Err(mlua::Error::runtime(format!(
                "command `{name}` does not accept arguments"
            )));
        }
        let handler: Function = self.lua.registry_value(&command.registration.handler)?;
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let (ctx, capability_lease) = self.create_handler_context(
            &command.registration,
            "Command",
            ProcessInvocation::Command,
            notifications.clone(),
        )?;
        let rejected = Arc::new(AtomicBool::new(false));
        add_idle_controls(
            &self.lua,
            &ctx,
            idle,
            notifications.clone(),
            rejected.clone(),
        )?;
        if let Some(agent) = agent {
            add_agent_context(
                &self.lua,
                &ctx,
                agent,
                capability_lease.active.clone(),
                notifications.clone(),
                rejected.clone(),
            )?;
        }
        let call = handler.call_async::<()>((args.to_string(), ctx)).await;
        drop(capability_lease);
        let notifications = self.finish_notifications(call, &notifications)?;
        let notification = join_notifications(&notifications);
        Ok(Some(if rejected.load(Ordering::Acquire) {
            super::ExtensionCommandOutcome::Rejected(notification.unwrap_or_default())
        } else {
            super::ExtensionCommandOutcome::Completed(notification)
        }))
    }

    fn load_source(&self, source: &LuaSource) -> mlua::Result<()> {
        let registration = self.lua.create_table()?;
        registration.set("name", source.path.display().to_string())?;
        registration.set(
            "source",
            source.canonical_path.to_string_lossy().to_string(),
        )?;
        self.lua
            .globals()
            .set("__phi_current_extension", registration)?;
        let result = self
            .lua
            .load(&source.content)
            .set_name(source.path.display().to_string())
            .exec();
        self.lua
            .globals()
            .set("__phi_current_extension", Value::Nil)?;
        result
    }

    fn collect_hooks(&mut self) -> mlua::Result<()> {
        let registered: Table = self.lua.globals().get("__phi_hooks")?;
        let mut all_controls = Vec::new();
        for pair in registered.pairs::<String, Table>() {
            let (event, specs) = pair?;
            let mut hooks = Vec::new();
            let mut controls = Vec::new();
            for spec in specs.sequence_values::<Table>() {
                let spec = spec?;
                let handler = collect_handler(&self.lua, spec.clone())?;
                let mut options = serde_json::Map::new();
                for pair in spec.clone().pairs::<String, Value>() {
                    let (key, value) = pair?;
                    if key == "handler" || key == "matcher" || key.starts_with("__phi_") {
                        continue;
                    }
                    options.insert(key, lua_to_json(value)?);
                }
                let options: HookOptions =
                    serde_json::from_value(serde_json::Value::Object(options))
                        .map_err(mlua::Error::external)?;
                let matcher = match spec.get::<Value>("matcher")? {
                    Value::Nil => None,
                    value => Some(
                        serde_json::from_value::<HookMatcher>(lua_to_json(value)?)
                            .map_err(mlua::Error::external)?,
                    ),
                };
                let owner = format!("lua:{}:{}", handler.source.display(), handler.extension);
                let control = HookControl::from_options(&owner, &event, options, matcher)
                    .map_err(mlua::Error::external)?
                    .with_default_id(&owner, &event, controls.len());
                all_controls.push(control.clone());
                hooks.push(handler);
                controls.push(control);
            }
            self.hook_controls.insert(event.clone(), controls);
            self.hooks.insert(event, hooks);
        }
        HookControl::validate_identities(&all_controls).map_err(mlua::Error::external)
    }

    fn collect_commands(&mut self) -> mlua::Result<()> {
        let registered: Table = self.lua.globals().get("__phi_commands")?;
        for pair in registered.pairs::<String, Table>() {
            let (name, spec) = pair?;
            let description = spec.get::<String>("description").ok();
            self.commands.insert(
                name,
                LuaCommand {
                    description,
                    arguments: match spec.get::<Value>("arguments")? {
                        Value::Nil | Value::Boolean(true) => Some(Default::default()),
                        Value::Boolean(false) => None,
                        value => Some(
                            serde_json::from_value(lua_to_json(value)?)
                                .map_err(mlua::Error::external)?,
                        ),
                    },
                    registration: collect_handler(&self.lua, spec)?,
                },
            );
        }
        Ok(())
    }

    fn collect_tools(&mut self) -> mlua::Result<()> {
        let registered: Table = self.lua.globals().get("__phi_tools")?;
        for pair in registered.pairs::<String, Table>() {
            let (name, spec) = pair?;
            let parameters = match spec.get::<Option<Table>>("parameters")? {
                Some(table) => validate_tool_schema(lua_table_to_json(&table)?)?,
                None => serde_json::json!({ "type": "object", "properties": {} }),
            };
            self.tools.insert(
                name.clone(),
                LuaTool {
                    spec: ExtensionToolSpec {
                        extension: spec.get("__phi_extension")?,
                        name,
                        description: spec.get("description")?,
                        parameters,
                        override_existing: spec.get::<bool>("__phi_override").unwrap_or(false),
                    },
                    handler: self
                        .lua
                        .create_registry_value(spec.get::<Function>("handler")?)?,
                },
            );
        }
        Ok(())
    }

    fn collect_keybindings(&mut self) -> mlua::Result<()> {
        let registered: Table = self.lua.globals().get("__phi_keybindings")?;
        for (index, spec) in registered.sequence_values::<Table>().enumerate() {
            let spec = spec?;
            let id = ExtensionKeybindingId::new(self.runtime_id, index as u64);
            self.keybindings.push(RegisteredKeybinding {
                id,
                context: spec.get("context")?,
                key: spec.get("key")?,
            });
            self.keybinding_handlers
                .insert(id, collect_handler(&self.lua, spec)?);
        }
        Ok(())
    }

    fn collect_tool_displays(&mut self) -> mlua::Result<()> {
        let registered: Table = self.lua.globals().get("__phi_tool_displays")?;
        for pair in registered.pairs::<String, Function>() {
            let (name, handler) = pair?;
            self.tool_displays
                .insert(name, self.lua.create_registry_value(handler)?);
        }
        Ok(())
    }

    async fn call_hooks(
        &self,
        event: &LifecycleEvent,
        index: Option<usize>,
    ) -> mlua::Result<Option<HookResponse>> {
        let mut result = HookResponse::default();
        let mut hook_output = None;
        let mut context_outputs = Vec::new();
        let mut context_edits = Vec::<crate::core::lifecycle::ContextEditDraft>::new();
        let mut prompt_contexts = Vec::<crate::context::InjectedContext>::new();
        let Some(hooks) = self.hooks.get(event.kind.as_str()) else {
            return Ok(None);
        };
        let controls = &self.hook_controls[event.kind.as_str()];
        let controls = index
            .map(|index| &controls[index..index + 1])
            .unwrap_or(controls);
        let hooks = index.map(|index| &hooks[index..index + 1]).unwrap_or(hooks);
        let calls =
            futures::future::join_all(hooks.iter().zip(controls).map(|(hook, control)| async {
                if !control.start_adapter(event) {
                    return Ok(Value::Nil);
                }
                let event_table = self.lua.create_table()?;
                event_table.set("name", event.kind.as_str())?;
                event_table.set("type", event.kind.as_str())?;
                event_table.set("payload", json_to_lua(&self.lua, &event.payload)?)?;
                let handler: Function = self.lua.registry_value(&hook.handler)?;
                let (ctx, capability_lease) = self.create_handler_context(
                    hook,
                    event.kind.as_str(),
                    ProcessInvocation::Hook,
                    self.pending_notifications.clone(),
                )?;
                let call = handler.call_async((event_table, ctx));
                let result = if index.is_none() {
                    let timeout = control.timeout(*self.hook_timeout.lock().unwrap());
                    tokio::time::timeout(timeout, call)
                        .await
                        .map_err(|_| mlua::Error::runtime("lifecycle hook timed out"))?
                } else {
                    call.await
                };
                drop(capability_lease);
                result
            }))
            .await;
        for value in calls {
            let value: Value = value?;
            if let Value::Table(table) = value {
                result.cancel |= table.get::<bool>("cancel").unwrap_or(false);
                result.block |= table.get::<bool>("block").unwrap_or(false);
                if let Ok(reason) = table.get::<String>("reason") {
                    result.reason = Some(reason);
                }
                let mut payload = lua_table_to_json(&table)?;
                let list_fields: &[&str] = match event.kind {
                    LifecycleEventKind::UserPromptSubmit
                    | LifecycleEventKind::UserPromptExpansion => &["injected_context"],
                    LifecycleEventKind::PromptContext => &["contexts", "notices"],
                    LifecycleEventKind::AgentTurnEnd => &["context_edits"],
                    _ => &[],
                };
                for field in list_fields {
                    if let Some(value) = payload.get_mut(*field)
                        && value.as_object().is_some_and(serde_json::Map::is_empty)
                    {
                        *value = serde_json::Value::Array(Vec::new());
                    }
                }
                if event.kind == LifecycleEventKind::AgentTurnEnd {
                    if let Some(edits) = payload
                        .get_mut("context_edits")
                        .and_then(serde_json::Value::as_array_mut)
                    {
                        for edit in edits {
                            if let Some(edit) = edit.as_object_mut() {
                                // Lua nil removes a table key; at this boundary it means omission.
                                edit.entry("replacement").or_insert(serde_json::Value::Null);
                                if let Some(content) = edit
                                    .get_mut("replacement")
                                    .and_then(|replacement| replacement.get_mut("content"))
                                    && content.as_object().is_some_and(serde_json::Map::is_empty)
                                {
                                    *content = serde_json::Value::Array(Vec::new());
                                }
                            }
                        }
                    }
                    let outcome: crate::core::lifecycle::AgentTurnEndOutcome =
                        serde_json::from_value(payload).map_err(mlua::Error::external)?;
                    context_edits.extend(outcome.context_edits);
                } else if event.kind == crate::core::lifecycle::LifecycleEventKind::PromptContext {
                    let contribution: crate::core::lifecycle::PromptContextOutcome =
                        serde_json::from_value(payload).map_err(mlua::Error::external)?;
                    context_outputs
                        .push(serde_json::to_value(contribution).map_err(mlua::Error::external)?);
                } else if event.kind == crate::core::lifecycle::LifecycleEventKind::UserPromptSubmit
                {
                    if let Some(contexts) = payload.get("injected_context") {
                        prompt_contexts.extend(
                            serde_json::from_value::<Vec<crate::context::InjectedContext>>(
                                contexts.clone(),
                            )
                            .map_err(mlua::Error::external)?,
                        );
                    }
                    merge_json(&mut hook_output, payload);
                } else if event.kind == crate::core::lifecycle::LifecycleEventKind::PreToolUse {
                    merge_json(&mut hook_output, normalize_pre_tool_payload(payload));
                } else {
                    hook_output = Some(payload);
                }
            }
        }
        if event.kind == LifecycleEventKind::AgentTurnEnd {
            hook_output = Some(serde_json::json!({ "context_edits": context_edits }));
        } else if event.kind == crate::core::lifecycle::LifecycleEventKind::PromptContext {
            hook_output = Some(serde_json::Value::Array(context_outputs));
        } else if event.kind == crate::core::lifecycle::LifecycleEventKind::UserPromptSubmit
            && let Some(payload) = hook_output.as_mut()
        {
            payload["injected_context"] =
                serde_json::to_value(prompt_contexts).map_err(mlua::Error::external)?;
        }
        result.output = match lua_lifecycle_action(&event.kind, hook_output.as_ref()) {
            Some(action) => Some(HookOutput::Action(action)),
            None => hook_output.map(HookOutput::Payload),
        };
        Ok(Some(result))
    }

    fn create_handler_context(
        &self,
        registration: &LuaHandler,
        context: &str,
        invocation: ProcessInvocation,
        notifications: Arc<Mutex<Vec<String>>>,
    ) -> mlua::Result<(Table, CapabilityLease)> {
        let ctx = create_context(&self.lua, notifications)?;
        let capability_lease = CapabilityLease::new();
        if !registration.process_programs.is_empty() {
            let process = create_process_context(
                &self.lua,
                registration.extension.clone(),
                registration.source.clone(),
                context.to_string(),
                invocation,
                registration.process_programs.clone(),
                self.services.clone(),
                capability_lease.active.clone(),
            )?;
            ctx.set("process", process)?;
        }
        Ok((ctx, capability_lease))
    }
}

struct CapabilityLease {
    active: Arc<AtomicBool>,
}

impl CapabilityLease {
    fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Drop for CapabilityLease {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

fn validate_tool_schema(schema: serde_json::Value) -> mlua::Result<serde_json::Value> {
    let object = schema
        .as_object()
        .ok_or_else(|| mlua::Error::runtime("tool parameters must be a JSON object schema"))?;
    if object.get("type").and_then(serde_json::Value::as_str) != Some("object") {
        return Err(mlua::Error::runtime(
            "tool parameters schema must declare type = 'object'",
        ));
    }
    if object
        .get("properties")
        .is_some_and(|properties| !properties.is_object())
    {
        return Err(mlua::Error::runtime(
            "tool parameters schema properties must be an object",
        ));
    }
    if object.get("required").is_some_and(|required| {
        required
            .as_array()
            .is_none_or(|items| items.iter().any(|item| !item.is_string()))
    }) {
        return Err(mlua::Error::runtime(
            "tool parameters schema required must be an array of strings",
        ));
    }
    Ok(schema)
}

fn collect_handler(lua: &Lua, spec: Table) -> mlua::Result<LuaHandler> {
    let process_programs = spec
        .get::<Option<Table>>("__phi_process")?
        .map(|programs| {
            programs
                .sequence_values::<String>()
                .collect::<mlua::Result<HashSet<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(LuaHandler {
        extension: spec.get("__phi_extension")?,
        source: PathBuf::from(spec.get::<String>("__phi_source")?),
        process_programs,
        handler: lua.create_registry_value(spec.get::<Function>("handler")?)?,
    })
}

fn add_agent_context(
    lua: &Lua,
    ctx: &Table,
    submitter: Arc<dyn super::ExtensionPromptSubmitter>,
    active: Arc<AtomicBool>,
    notifications: Arc<Mutex<Vec<String>>>,
    rejected: Arc<AtomicBool>,
) -> mlua::Result<()> {
    let agent = lua.create_table()?;
    agent.set(
        "send_user_message",
        lua.create_async_function(move |lua, (prompt, options): (Value, Option<Table>)| {
            let submitter = submitter.clone();
            let active = active.clone();
            let notifications = notifications.clone();
            let rejected = rejected.clone();
            async move {
                if !active.load(Ordering::Acquire) {
                    return Err(mlua::Error::runtime(
                        "command submission capability has expired",
                    ));
                }
                let Value::String(prompt) = prompt else {
                    return Err(mlua::Error::runtime("message must be a string"));
                };
                let prompt = prompt.to_str()?.to_string();
                if prompt.trim().is_empty() {
                    return Err(mlua::Error::runtime("message must not be empty"));
                }
                let dispatch = match options {
                    Some(options) => message_dispatch(options)?,
                    None => None,
                };
                let receipt = submitter
                    .submit(prompt, dispatch)
                    .await
                    .map_err(mlua::Error::external)?;
                if let crate::turn::SubmissionDisposition::Handled { reason } = &receipt {
                    rejected.store(true, Ordering::Release);
                    if let Some(reason) = reason {
                        notifications
                            .lock()
                            .map_err(|_| mlua::Error::runtime("notification lock poisoned"))?
                            .push(reason.clone());
                    }
                }
                json_to_lua(&lua, &submission_receipt(receipt))
            }
        })?,
    )?;
    ctx.set("agent", agent)
}

fn message_dispatch(options: Table) -> mlua::Result<Option<crate::turn::SubmissionDispatch>> {
    for pair in options.clone().pairs::<String, Value>() {
        if pair?.0 != "deliver_as" {
            return Err(mlua::Error::runtime("unsupported message option"));
        }
    }
    match options.get::<Value>("deliver_as")? {
        Value::Nil => Ok(None),
        Value::String(mode) => match mode.to_str()?.as_ref() {
            "steer" => Ok(Some(crate::turn::SubmissionDispatch::Steer)),
            "follow_up" => Ok(Some(crate::turn::SubmissionDispatch::FollowUp)),
            _ => Err(mlua::Error::runtime(
                "deliver_as must be steer or follow_up",
            )),
        },
        _ => Err(mlua::Error::runtime("deliver_as must be a string")),
    }
}

fn submission_receipt(receipt: crate::turn::SubmissionDisposition) -> serde_json::Value {
    match receipt {
        crate::turn::SubmissionDisposition::Started { turn_id, prompt } => serde_json::json!({
            "status": "started", "turn_id": turn_id, "prompt": prompt,
        }),
        crate::turn::SubmissionDisposition::Queued {
            submission_id,
            dispatch,
            prompt,
        } => serde_json::json!({
            "status": "queued", "submission_id": submission_id,
            "dispatch": dispatch.as_str(), "prompt": prompt,
        }),
        crate::turn::SubmissionDisposition::Handled { reason } => serde_json::json!({
            "status": "handled", "reason": reason,
        }),
    }
}

fn add_idle_controls(
    lua: &Lua,
    ctx: &Table,
    idle: tokio::sync::watch::Receiver<bool>,
    notifications: Arc<Mutex<Vec<String>>>,
    rejected: Arc<AtomicBool>,
) -> mlua::Result<()> {
    let waiting = idle.clone();
    ctx.set(
        "wait_for_idle",
        lua.create_async_function(move |_, ()| {
            let mut idle = waiting.clone();
            async move {
                while !*idle.borrow_and_update() {
                    idle.changed()
                        .await
                        .map_err(|_| mlua::Error::external("idle state is unavailable"))?;
                }
                Ok(())
            }
        })?,
    )?;
    ctx.set(
        "require_idle",
        lua.create_function(move |_, operation: String| {
            if !*idle.borrow() {
                rejected.store(true, Ordering::Release);
                notifications
                    .lock()
                    .map_err(|_| mlua::Error::external("notification lock poisoned"))?
                    .push(format!("Cannot {operation} while the agent is running."));
                return Ok(false);
            }
            Ok(true)
        })?,
    )
}

#[derive(Clone, Copy)]
enum ProcessInvocation {
    Hook,
    Command,
}

fn create_process_context(
    lua: &Lua,
    extension: String,
    source: PathBuf,
    context: String,
    invocation: ProcessInvocation,
    allowed_programs: HashSet<String>,
    services: ExtensionRuntimeServices,
    active: Arc<AtomicBool>,
) -> mlua::Result<Table> {
    let process = lua.create_table()?;
    process.set(
        "run",
        lua.create_async_function(
            move |lua, (program, args, options): (String, Table, Option<Table>)| {
                let extension = extension.clone();
                let source = source.clone();
                let context = context.clone();
                let allowed_programs = allowed_programs.clone();
                let services = services.clone();
                let active = active.clone();
                async move {
                    if !active.load(Ordering::Acquire) {
                        return Err(mlua::Error::external(
                            "extension process capability is no longer active",
                        ));
                    }
                    if !allowed_programs.contains(&program) {
                        return Err(mlua::Error::external(format!(
                            "extension '{extension}' is not allowed to run '{program}' in {context}"
                        )));
                    }
                    if !services.source_trusted(&source) {
                        return Err(mlua::Error::external(format!(
                            "extension '{extension}' source is not trusted for process execution"
                        )));
                    }
                    let runner = services.process_runner_ref().cloned().ok_or_else(|| {
                        mlua::Error::external("extension process capability is unavailable")
                    })?;
                    let args = args
                        .sequence_values::<String>()
                        .collect::<mlua::Result<Vec<_>>>()?;
                    if matches!(invocation, ProcessInvocation::Command) {
                        services
                            .authorize_process(&extension, &context, &program, &args)
                            .await
                            .map_err(mlua::Error::external)?;
                    }
                    let timeout_ms = options
                        .and_then(|options| options.get::<u64>("timeout_ms").ok())
                        .unwrap_or(DEFAULT_PROCESS_TIMEOUT_MS);
                    let request = ExtensionProcessRequest {
                        extension,
                        source: source.to_string_lossy().to_string(),
                        context,
                        program,
                        args,
                        timeout_ms,
                    };
                    let execution = match invocation {
                        ProcessInvocation::Hook => runner.run_hook(request),
                        ProcessInvocation::Command => runner.run(request),
                    };
                    let output = execution.await.map_err(mlua::Error::external)?;
                    process_output_to_lua(&lua, output)
                }
            },
        )?,
    )?;
    Ok(process)
}

fn process_output_to_lua(lua: &Lua, output: ExtensionProcessOutput) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set("success", output.success)?;
    result.set("exit_code", output.exit_code)?;
    result.set("stdout", output.stdout)?;
    result.set("stderr", output.stderr)?;
    result.set("truncated", output.truncated)?;
    Ok(result)
}

fn create_keybinding_context(
    lua: &Lua,
    registration: &LuaHandler,
    services: &ExtensionRuntimeServices,
    notifications: Arc<Mutex<Vec<String>>>,
    effects: Arc<Mutex<Vec<ExtensionEffect>>>,
) -> mlua::Result<Table> {
    let ctx = create_context(lua, notifications)?;
    if registration.process_programs.is_empty() {
        return Ok(ctx);
    }
    let extension = registration.extension.clone();
    let source = registration.source.clone();
    let declared_programs = registration.process_programs.clone();
    let sandboxed = services.effects_sandboxed();
    let services = services.clone();
    let process = lua.create_table()?;
    process.set(
        "spawn",
        lua.create_function(move |_, (program, args): (String, Table)| {
            if !declared_programs.contains(&program) {
                return Err(mlua::Error::external(format!(
                    "extension '{extension}' is not allowed to run '{program}' in Keybinding"
                )));
            }
            if !services.source_trusted(&source) {
                return Err(mlua::Error::external(format!(
                    "extension '{extension}' source is not trusted for process execution"
                )));
            }
            let args = args
                .sequence_values::<String>()
                .collect::<mlua::Result<Vec<_>>>()?;
            effects
                .lock()
                .map_err(|_| mlua::Error::external("effect lock poisoned"))?
                .push(ExtensionEffect::SpawnProcess {
                    extension: extension.clone(),
                    source: source.to_string_lossy().to_string(),
                    context: "Keybinding".to_string(),
                    program,
                    args,
                    sandboxed,
                });
            Ok(())
        })?,
    )?;
    ctx.set("process", process)?;
    Ok(ctx)
}

fn lua_lifecycle_action(
    kind: &LifecycleEventKind,
    payload: Option<&serde_json::Value>,
) -> Option<HookAction> {
    let payload = payload?;
    let payload = payload.get("hookSpecificOutput").unwrap_or(payload);
    match kind {
        LifecycleEventKind::PreToolUse => Some(HookAction::PreToolUse(PreToolUseResult {
            permission_decision: payload
                .get("permission_decision")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_permission_decision),
            reason: payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            updated_input: payload.get("updated_input").cloned(),
            additional_context: additional_context(payload),
        })),
        LifecycleEventKind::PostToolUse | LifecycleEventKind::PostToolUseFailure => {
            Some(HookAction::PostToolUse(PostToolUseResult {
                updated_output: payload
                    .get("updatedToolOutput")
                    .or_else(|| payload.get("updated_output"))
                    .cloned(),
                additional_context: additional_context(payload),
                ..PostToolUseResult::default()
            }))
        }
        LifecycleEventKind::PostToolBatch => Some(HookAction::PostToolBatch(PostToolBatchResult {
            additional_context: additional_context(payload),
            ..PostToolBatchResult::default()
        })),
        _ => None,
    }
}

fn parse_permission_decision(value: &str) -> Option<PermissionDecision> {
    match value {
        "allow" => Some(PermissionDecision::Allow),
        "deny" => Some(PermissionDecision::Deny),
        "ask" => Some(PermissionDecision::Ask),
        "defer" => Some(PermissionDecision::Defer),
        _ => None,
    }
}

fn additional_context(payload: &serde_json::Value) -> Vec<String> {
    let Some(value) = payload
        .get("additionalContext")
        .or_else(|| payload.get("additional_context"))
    else {
        return Vec::new();
    };
    match value {
        serde_json::Value::String(value) => vec![value.clone()],
        serde_json::Value::Array(values) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn normalize_pre_tool_payload(value: serde_json::Value) -> serde_json::Value {
    let serde_json::Value::Object(values) = value else {
        return value;
    };
    serde_json::Value::Object(
        values
            .into_iter()
            .map(|(key, value)| {
                let value = if key == "hookSpecificOutput" {
                    normalize_pre_tool_payload(value)
                } else {
                    value
                };
                let key = match key.as_str() {
                    "updatedInput" => "updated_input",
                    "permissionDecision" => "permission_decision",
                    "permissionDecisionReason" => "reason",
                    "additionalContext" => "additional_context",
                    _ => &key,
                }
                .to_string();
                (key, value)
            })
            .collect(),
    )
}

fn merge_json(current: &mut Option<serde_json::Value>, next: serde_json::Value) {
    match current {
        Some(current) => merge_json_value(current, next),
        None => *current = Some(next),
    }
}

fn merge_json_value(current: &mut serde_json::Value, next: serde_json::Value) {
    match (current, next) {
        (serde_json::Value::Object(current), serde_json::Value::Object(next)) => {
            for (key, value) in next {
                match current.get_mut(&key) {
                    Some(current) if key == "permission_decision" => {
                        if permission_rank(&value) > permission_rank(current) {
                            *current = value;
                        }
                    }
                    Some(current) if key == "additional_context" => {
                        merge_additional_context(current, value);
                    }
                    Some(current) => merge_json_value(current, value),
                    None => {
                        current.insert(key, value);
                    }
                }
            }
        }
        (current, next) => *current = next,
    }
}

fn permission_rank(value: &serde_json::Value) -> u8 {
    match value.as_str() {
        Some("allow") => 1,
        Some("ask") => 2,
        Some("defer") => 3,
        Some("deny") => 4,
        _ => 0,
    }
}

fn merge_additional_context(current: &mut serde_json::Value, next: serde_json::Value) {
    let mut values = match std::mem::take(current) {
        serde_json::Value::Array(values) => values,
        serde_json::Value::Null => Vec::new(),
        value => vec![value],
    };
    match next {
        serde_json::Value::Array(next) => values.extend(next),
        value => values.push(value),
    }
    *current = serde_json::Value::Array(values);
}

fn take_locked<T>(values: &Mutex<Vec<T>>, name: &str) -> mlua::Result<Vec<T>> {
    let mut values = values
        .lock()
        .map_err(|_| mlua::Error::external(format!("{name} lock poisoned")))?;
    Ok(std::mem::take(&mut *values))
}

fn join_notifications(notifications: &[String]) -> Option<String> {
    let text = notifications
        .iter()
        .map(|message| message.trim())
        .filter(|message| !message.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn install_api(lua: &Lua) -> mlua::Result<()> {
    lua.globals().set(
        "register_extension",
        lua.create_function(|lua, spec: Table| {
            let current: Table = lua.globals().get("__phi_current_extension")?;
            current.set("name", spec.get::<String>("name")?)?;
            current.set("capabilities", spec.get::<Option<Table>>("capabilities")?)
        })?,
    )?;

    let hooks = lua.create_table()?;
    lua.globals().set("__phi_hooks", hooks.clone())?;
    let on = lua.create_function(move |lua, (event, declaration): (String, Value)| {
        HookControl::from_options("lua", &event, HookOptions::default(), None)
            .map_err(mlua::Error::external)?;
        let list: Table = match hooks.get(event.clone()) {
            Ok(list) => list,
            Err(_) => {
                let list = lua.create_table()?;
                hooks.set(event.clone(), list.clone())?;
                list
            }
        };
        match declaration {
            Value::Function(handler) => {
                let spec = registration_spec(&lua, &event, handler)?;
                list.raw_set(list.raw_len() + 1, spec)
            }
            Value::Table(spec) => {
                if let Some(handlers) = spec.get::<Option<Table>>("hooks")? {
                    for pair in spec.clone().pairs::<String, Value>() {
                        let (key, _) = pair?;
                        if key != "matcher" && key != "hooks" {
                            return Err(mlua::Error::runtime(format!(
                                "unsupported matcher group field: {key}"
                            )));
                        }
                    }
                    let matcher = spec.get::<Value>("matcher")?;
                    if !matches!(matcher, Value::Nil) {
                        let filter: HookMatcher =
                            serde_json::from_value(lua_to_json(matcher.clone())?)
                                .map_err(mlua::Error::external)?;
                        HookControl::from_options(
                            "lua",
                            &event,
                            HookOptions::default(),
                            Some(filter),
                        )
                        .map_err(mlua::Error::external)?;
                    }
                    for handler in handlers.sequence_values::<Table>() {
                        let handler = handler?;
                        if handler.contains_key("matcher")? {
                            return Err(mlua::Error::runtime(
                                "matcher belongs to the matcher group",
                            ));
                        }
                        handler.set("matcher", matcher.clone())?;
                        let _: Function = handler.get("handler")?;
                        attach_registration(&lua, &handler, &event)?;
                        list.raw_set(list.raw_len() + 1, handler)?;
                    }
                    Ok(())
                } else {
                    let _: Function = spec.get("handler")?;
                    attach_registration(&lua, &spec, &event)?;
                    list.raw_set(list.raw_len() + 1, spec)
                }
            }
            _ => Err(mlua::Error::runtime(
                "on expects a callback or handler declaration",
            )),
        }
    })?;
    lua.globals().set("on", on)?;

    let commands = lua.create_table()?;
    lua.globals().set("__phi_commands", commands.clone())?;
    let register_command = lua.create_function(move |lua, (name, spec): (String, Table)| {
        let handler: Function = spec.get("handler")?;
        attach_registration(&lua, &spec, "Command")?;
        spec.set("handler", handler)?;
        commands.set(name, spec)
    })?;
    lua.globals().set("register_command", register_command)?;

    let keybindings = lua.create_table()?;
    lua.globals()
        .set("__phi_keybindings", keybindings.clone())?;
    let register_keybinding = lua.create_function(move |lua, (key, spec): (String, Table)| {
        let handler: Function = spec.get("handler")?;
        let _: String = spec.get("context")?;
        attach_registration(&lua, &spec, "Keybinding")?;
        spec.set("handler", handler)?;
        spec.set("key", key.to_ascii_lowercase())?;
        keybindings.raw_set(keybindings.raw_len() + 1, spec)
    })?;
    lua.globals()
        .set("register_keybinding", register_keybinding)?;

    let tool_displays = lua.create_table()?;
    lua.globals()
        .set("__phi_tool_displays", tool_displays.clone())?;
    let register_tool_display =
        lua.create_function(move |_, (name, handler): (String, Function)| {
            tool_displays.set(name, handler)
        })?;
    lua.globals()
        .set("register_tool_display", register_tool_display)?;

    let tools = lua.create_table()?;
    lua.globals().set("__phi_tools", tools.clone())?;
    lua.globals().set(
        "register_tool",
        lua.create_function(move |lua, (name, spec): (String, Table)| {
            let _: Function = spec.get("handler")?;
            let _: String = spec.get("description")?;
            if let Some(parameters) = spec.get::<Option<Table>>("parameters")? {
                validate_tool_schema(lua_table_to_json(&parameters)?)?;
            }
            let name = canonical_tool_name(name);
            let override_existing = spec.get::<bool>("override").unwrap_or(false);
            if tools.contains_key(name.clone())? && !override_existing {
                return Err(mlua::Error::runtime(format!(
                    "tool '{name}' is already registered; set override = true to replace it"
                )));
            }
            attach_registration(lua, &spec, "Tool")?;
            spec.set("__phi_override", override_existing)?;
            tools.set(name, spec)
        })?,
    )?;
    Ok(())
}

fn registration_spec(lua: &Lua, context: &str, handler: Function) -> mlua::Result<Table> {
    let spec = lua.create_table()?;
    spec.set("handler", handler)?;
    attach_registration(lua, &spec, context)?;
    Ok(spec)
}

fn attach_registration(lua: &Lua, spec: &Table, context: &str) -> mlua::Result<()> {
    let current: Table = lua.globals().get("__phi_current_extension")?;
    spec.set("__phi_extension", current.get::<String>("name")?)?;
    spec.set("__phi_source", current.get::<String>("source")?)?;
    let programs = current
        .get::<Option<Table>>("capabilities")?
        .and_then(|capabilities| capabilities.get::<Table>("process").ok())
        .and_then(|process| process.get::<Table>(context).ok());
    if let Some(programs) = programs {
        let validated = lua.create_table()?;
        for program in programs.sequence_values::<String>() {
            let program = program?;
            validated.raw_set(validated.raw_len() + 1, program)?;
        }
        spec.set("__phi_process", validated)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::checker::PermissionChecker;
    use crate::permission::runtime::PermissionRuntime;
    use crate::permission::{PermissionConfig, PermissionMode};

    #[test]
    fn hook_declarations_reject_unsupported_policy_before_loading() {
        for declaration in [
            r#"on("missing", function() end)"#,
            r#"on("Stop", { matcher = "Read", handler = function() end })"#,
            r#"on("PreToolUse", { matcher = { regex = "[" }, handler = function() end })"#,
            r#"on("PreToolUse", { execution = "background_observation", handler = function() end })"#,
            r#"on("SessionStart", { start_mode = "before_first_prompt", handler = function() end })"#,
            r#"on("PreToolUse", { timeout = 0, handler = function() end })"#,
            r#"on("PreToolUse", { unknown = true, handler = function() end })"#,
            r#"on("PreToolUse", { ["if"] = {field = "source", equals = "sdk"}, handler = function() end })"#,
            r#"on("Stop", { matcher = "unsupported", hooks = {} })"#,
            r#"on("PreToolUse", { hooks = {
                { id = "duplicate", once = true, handler = function() end },
                { id = "duplicate", once = true, handler = function() end }
            } })"#,
        ] {
            let directory = std::path::Path::new("/tmp")
                .join(format!("phi-lua-policy-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            let source = directory.join("invalid.lua");
            std::fs::write(&source, declaration).unwrap();
            assert!(LuaRuntime::load(vec![source]).is_err(), "{declaration}");
            std::fs::remove_dir_all(directory).unwrap();
        }
    }

    #[derive(Clone)]
    struct FakeProcessRunner {
        requests: Arc<Mutex<Vec<ExtensionProcessRequest>>>,
        output: ExtensionProcessOutput,
    }

    impl super::super::ExtensionProcessRunner for FakeProcessRunner {
        fn run(&self, request: ExtensionProcessRequest) -> super::super::ExtensionProcessFuture {
            self.requests.lock().unwrap().push(request);
            let output = self.output.clone();
            Box::pin(async move { Ok(output) })
        }
    }

    struct FailingProcessRunner;

    impl super::super::ExtensionProcessRunner for FailingProcessRunner {
        fn run(&self, _request: ExtensionProcessRequest) -> super::super::ExtensionProcessFuture {
            Box::pin(async { anyhow::bail!("program unavailable") })
        }
    }

    fn fake_process_services(
        stdout: &str,
    ) -> (
        ExtensionRuntimeServices,
        Arc<Mutex<Vec<ExtensionProcessRequest>>>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let runner = FakeProcessRunner {
            requests: requests.clone(),
            output: ExtensionProcessOutput {
                success: true,
                exit_code: Some(0),
                stdout: stdout.to_string(),
                stderr: String::new(),
                truncated: false,
            },
        };
        (
            ExtensionRuntimeServices::new()
                .process_runner(Arc::new(runner))
                .process_policy(test_process_policy())
                .permission(allowing_permission()),
            requests,
        )
    }

    fn allowing_permission() -> PermissionRuntime {
        let config = PermissionConfig {
            mode: PermissionMode::Allow,
            ..PermissionConfig::default()
        };
        PermissionRuntime::new(Arc::new(Mutex::new(PermissionChecker::new(&config, None))))
    }

    fn test_process_policy() -> super::super::ExtensionProcessPolicy {
        super::super::ExtensionProcessPolicy::default()
            .trust_root(std::env::temp_dir())
            .trust_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".."))
    }

    fn runtime_from_source(
        source: &str,
        services: ExtensionRuntimeServices,
    ) -> mlua::Result<LuaRuntime> {
        let path =
            std::env::temp_dir().join(format!("phi-lua-extension-{}.lua", uuid::Uuid::new_v4()));
        std::fs::write(&path, source).map_err(mlua::Error::external)?;
        let runtime = LuaRuntime::load_with_services(vec![path.clone()], services);
        let _ = std::fs::remove_file(path);
        runtime
    }

    #[tokio::test]
    async fn prompt_hooks_preserve_each_structured_contribution() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
on("UserPromptSubmit", function(event)
  return { action = "transform", prompt = event.payload.prompt .. "-first",
    injected_context = {{ source = "hook", content = "first" }} }
end)
on("UserPromptSubmit", function(event)
  assert(event.payload.prompt == "input")
  return { action = "transform", prompt = event.payload.prompt .. "-second",
    injected_context = {{ source = { extension = { name = "builtin:memory", kind = "graph" } }, content = "second" }} }
end)
on("PromptContext", function()
  return { contexts = {{ source = "hook", content = "prepared-first" }} }
end)
on("PromptContext", function()
  return { contexts = {{ source = { extension = {
    name = "project-index", kind = "overview"
  } }, content = "prepared-second" }} }
end)
"#,
            ExtensionRuntimeServices::default(),
        )?;
        let result = runtime
            .emit(&LifecycleEvent::with_payload(
                LifecycleEventKind::UserPromptSubmit,
                serde_json::json!({ "prompt": "input" }),
            ))
            .await?
            .unwrap()
            .into_payload()
            .unwrap();
        assert_eq!(result["prompt"], "input-second");
        let contexts: Vec<crate::context::InjectedContext> =
            serde_json::from_value(result["injected_context"].clone()).unwrap();
        assert_eq!(contexts.len(), 2);
        assert_eq!(contexts[0].source, crate::transcript::ContextSource::Hook);
        assert_eq!(
            contexts[1].source,
            crate::extension::memory::MemoryContextKind::Graph.source()
        );
        let result = runtime
            .emit(&LifecycleEvent::new(LifecycleEventKind::PromptContext))
            .await?
            .unwrap()
            .into_payload()
            .unwrap();
        let contributions: Vec<crate::core::lifecycle::PromptContextOutcome> =
            serde_json::from_value(result).unwrap();
        assert_eq!(contributions.len(), 2);
        assert_eq!(contributions[0].contexts[0].content, "prepared-first");
        assert_eq!(contributions[1].contexts[0].content, "prepared-second");
        assert_eq!(
            contributions[1].contexts[0].source,
            crate::transcript::ContextSource::Extension(crate::transcript::ContextKey::new(
                "project-index",
                "overview"
            ),)
        );
        Ok(())
    }

    #[tokio::test]
    async fn turn_end_hooks_keep_all_context_edits_in_registration_order() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
on("AgentTurnEnd", function()
  return { context_edits = {{ target_id = "source", replacement = { content = "replacement" } }} }
end)
on("AgentTurnEnd", function()
  return { context_edits = {{ target_id = "source", replacement = { content = {} } }} }
end)
on("AgentTurnEnd", function()
  return { context_edits = {{ target_id = "source", replacement = nil }} }
end)
on("AgentTurnEnd", function()
  return { context_edits = {} }
end)
"#,
            ExtensionRuntimeServices::default(),
        )?;
        let payload = runtime
            .emit(&LifecycleEvent::new(LifecycleEventKind::AgentTurnEnd))
            .await?
            .unwrap()
            .into_payload()
            .unwrap();
        let outcome: crate::core::lifecycle::AgentTurnEndOutcome =
            serde_json::from_value(payload).unwrap();
        assert_eq!(outcome.context_edits.len(), 3);
        assert_eq!(outcome.context_edits[0].target_id, "source");
        assert!(outcome.context_edits[0].replacement.is_some());
        assert!(
            matches!(outcome.context_edits[1].replacement.as_ref().unwrap().content, crate::transcript::ContextContent::Blocks(ref blocks) if blocks.is_empty())
        );
        assert_eq!(outcome.context_edits[2].target_id, "source");
        assert!(outcome.context_edits[2].replacement.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn prompt_hooks_accept_empty_context_lists() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
on("UserPromptSubmit", function()
  return { injected_context = {} }
end)
on("PromptContext", function()
  return { contexts = {}, notices = {} }
end)
"#,
            ExtensionRuntimeServices::default(),
        )?;
        for kind in [
            LifecycleEventKind::UserPromptSubmit,
            LifecycleEventKind::PromptContext,
        ] {
            let payload = runtime
                .emit(&LifecycleEvent::new(kind.clone()))
                .await?
                .unwrap()
                .into_payload()
                .unwrap();
            if kind == LifecycleEventKind::UserPromptSubmit {
                assert_eq!(payload["injected_context"], serde_json::json!([]));
            } else {
                let outcomes: Vec<crate::core::lifecycle::PromptContextOutcome> =
                    serde_json::from_value(payload).unwrap();
                assert_eq!(outcomes, [Default::default()]);
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn prompt_hooks_reject_untyped_context_strings() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
on("UserPromptSubmit", function()
  return { injected_context = "not a typed contribution" }
end)
"#,
            ExtensionRuntimeServices::default(),
        )?;
        assert!(
            runtime
                .emit(&LifecycleEvent::new(LifecycleEventKind::UserPromptSubmit))
                .await
                .is_err()
        );
        Ok(())
    }

    fn pre_tool_use_result(result: HookResponse) -> PreToolUseResult {
        match result.into_action() {
            Some(HookAction::PreToolUse(result)) => result,
            _ => panic!("expected typed PreToolUse action"),
        }
    }

    #[tokio::test]
    async fn command_context_waits_for_idle_signal() -> mlua::Result<()> {
        let runtime = Arc::new(runtime_from_source(
            r#"
register_command("settled", {
  handler = function(_, ctx)
    ctx.wait_for_idle()
    ctx.ui.notify("idle")
  end,
})
"#,
            ExtensionRuntimeServices::new(),
        )?);
        let (idle_tx, idle_rx) = tokio::sync::watch::channel(false);
        let command_runtime = runtime.clone();
        let task = tokio::spawn(async move {
            command_runtime
                .run_command("settled", "", idle_rx, None)
                .await
        });

        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        idle_tx.send_replace(true);
        assert_eq!(
            task.await.unwrap()?,
            Some(super::super::ExtensionCommandOutcome::Completed(Some(
                "idle".to_string()
            )))
        );
        Ok(())
    }

    struct RecordingSubmitter(Arc<Mutex<Vec<(String, Option<crate::turn::SubmissionDispatch>)>>>);

    impl super::super::ExtensionPromptSubmitter for RecordingSubmitter {
        fn submit(
            &self,
            prompt: String,
            dispatch: Option<crate::turn::SubmissionDispatch>,
        ) -> super::super::ExtensionSubmissionFuture {
            self.0.lock().unwrap().push((prompt.clone(), dispatch));
            Box::pin(async move {
                Ok(crate::turn::SubmissionDisposition::Started {
                    turn_id: "turn".into(),
                    prompt,
                })
            })
        }
    }

    #[tokio::test]
    async fn command_validates_and_submits_messages_with_explicit_dispatch() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
register_command("review", {
  handler = function(args, ctx)
    for _, prompt in ipairs({ "   ", 123, { prompt = "review" } }) do
      assert(not pcall(ctx.agent.send_user_message, prompt))
    end
    for _, options in ipairs({
      { deliver_as = "invalid" }, { deliver_as = 123 }, { unknown = true },
    }) do
      assert(not pcall(ctx.agent.send_user_message, "review", options))
    end
    local receipt = ctx.agent.send_user_message("review " .. args, {})
    assert(receipt.status == "started" and receipt.turn_id == "turn")
    ctx.agent.send_user_message("next", { deliver_as = "follow_up" })
    ctx.agent.send_user_message("correction", { deliver_as = "steer" })
  end,
})
"#,
            ExtensionRuntimeServices::new(),
        )?;
        let submissions = Arc::new(Mutex::new(Vec::new()));
        runtime
            .run_command(
                "review",
                "main",
                tokio::sync::watch::channel(true).1,
                Some(Arc::new(RecordingSubmitter(submissions.clone()))),
            )
            .await?;
        assert_eq!(
            *submissions.lock().unwrap(),
            vec![
                ("review main".into(), None),
                (
                    "next".into(),
                    Some(crate::turn::SubmissionDispatch::FollowUp)
                ),
                (
                    "correction".into(),
                    Some(crate::turn::SubmissionDispatch::Steer)
                ),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn command_submission_capability_expires_with_handler() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
local send
register_command("save", { handler = function(_, ctx) send = ctx.agent.send_user_message end })
register_command("reuse", { handler = function() send("late message") end })
"#,
            ExtensionRuntimeServices::new(),
        )?;
        let submissions = Arc::new(Mutex::new(Vec::new()));
        let submitter = Arc::new(RecordingSubmitter(submissions.clone()));
        runtime
            .run_command(
                "save",
                "",
                tokio::sync::watch::channel(true).1,
                Some(submitter.clone()),
            )
            .await?;
        let error = runtime
            .run_command(
                "reuse",
                "",
                tokio::sync::watch::channel(true).1,
                Some(submitter),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("capability has expired"));
        assert!(submissions.lock().unwrap().is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn review_example_registers_command_and_submits_prompt() -> mlua::Result<()> {
        let (services, processes) = fake_process_services("git-directory");
        let runtime = runtime_from_source(
            include_str!("../../../examples/extensions/review.lua"),
            services,
        )?;
        assert!(
            runtime
                .commands()
                .iter()
                .any(|command| command.name == "review")
        );
        assert!(!runtime.hooks.contains_key("UserPromptSubmit"));
        let submissions = Arc::new(Mutex::new(Vec::new()));
        runtime
            .run_command(
                "review",
                "uncommitted --extra 'focus on permissions'",
                tokio::sync::watch::channel(true).1,
                Some(Arc::new(RecordingSubmitter(submissions.clone()))),
            )
            .await?;
        let submissions = submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        assert!(submissions[0].0.contains("Review the current code changes"));
        assert!(submissions[0].0.contains("focus on permissions"));
        assert_eq!(processes.lock().unwrap()[0].context, "Command");
        Ok(())
    }

    #[test]
    fn require_idle_rejects_busy_operations() -> mlua::Result<()> {
        let lua = Lua::new();
        let ctx = lua.create_table()?;
        let notifications = Arc::new(Mutex::new(Vec::new()));
        let rejected = Arc::new(AtomicBool::new(false));
        add_idle_controls(
            &lua,
            &ctx,
            tokio::sync::watch::channel(false).1,
            notifications.clone(),
            rejected.clone(),
        )?;

        let require_idle: Function = ctx.get("require_idle")?;
        assert!(!require_idle.call::<bool>("replace the session")?);
        assert!(rejected.load(Ordering::Acquire));
        assert_eq!(
            notifications.lock().unwrap().as_slice(),
            ["Cannot replace the session while the agent is running."]
        );
        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_and_tools_share_one_lua_runtime() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
local count = 0
on("SessionStart", function() count = count + 1 end)
register_tool("counter", {
  description = "Count shared runtime calls",
  handler = function()
    count = count + 1
    return { count = count }
  end,
})
"#,
            ExtensionRuntimeServices::new(),
        )?;

        runtime
            .emit(&LifecycleEvent::new(
                crate::core::lifecycle::LifecycleEventKind::SessionStart,
            ))
            .await?;
        let output = runtime.call_tool("counter", "{}".to_string())?;

        let output =
            serde_json::from_str::<serde_json::Value>(&output).map_err(mlua::Error::external)?;
        assert_eq!(output["count"], 2);
        assert_eq!(runtime.tool_specs().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn model_lua_tools_receive_only_their_captured_outer_turn_scope() -> mlua::Result<()> {
        use crate::core::turn::TurnToolPolicy;
        use rig::tool::{ToolContext, ToolSet};

        let permission = PermissionRuntime::new(Arc::new(Mutex::new(PermissionChecker::new(
            &PermissionConfig::default(),
            None,
        ))));
        let observed = permission.clone();
        let lua = runtime_from_source(
            r#"register_tool("scope_probe", { description = "scope", handler = function() return scope_size() end })"#,
            ExtensionRuntimeServices::new(),
        )?;
        lua.lua.globals().set(
            "scope_size",
            lua.lua.create_function(move |_, ()| {
                Ok(observed.current_tool_policy().allowances.len())
            })?,
        )?;
        let runtime = Arc::new(crate::extension::ExtensionRuntime {
            registry: Default::default(),
            lua: Some(lua),
            builtins: Default::default(),
        });
        let tools = ToolSet::from_dynamic_tools(
            crate::agent::tools::lua::agent_tools(runtime)
                .into_iter()
                .map(|registration| registration.tool)
                .collect(),
        );
        let invoke = || async {
            tools
                .execute("scope_probe", "{}", &mut ToolContext::new())
                .await
                .output()
                .render()
        };
        let (scoped, unrelated) = tokio::join!(
            permission
                .outer_turn_policy(TurnToolPolicy {
                    allowances: vec![("Read".into(), "**".into())],
                    ..Default::default()
                })
                .run(invoke()),
            invoke(),
        );
        assert_eq!(scoped, "1");
        assert_eq!(unrelated, "0");
        assert_eq!(invoke().await, "0");
        Ok(())
    }

    #[test]
    fn duplicate_tool_registration_requires_explicit_override() -> mlua::Result<()> {
        let result = runtime_from_source(
            r#"
register_tool("echo", { description = "first", handler = function() end })
register_tool("echo", { description = "second", handler = function() end })
"#,
            ExtensionRuntimeServices::new(),
        );

        let error = result.err().expect("duplicate registration must fail");
        assert!(error.to_string().contains("override = true"));
        Ok(())
    }

    #[test]
    fn explicit_tool_override_replaces_canonical_registration() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
register_tool("Bash", { description = "first", handler = function() end })
register_tool("bash", {
  override = true,
  description = "second",
  handler = function() return "second" end,
})
"#,
            ExtensionRuntimeServices::new(),
        )?;

        let specs = runtime.tool_specs();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "Bash");
        assert_eq!(specs[0].description, "second");
        assert!(specs[0].override_existing);
        Ok(())
    }

    #[test]
    fn malformed_tool_schema_rejects_extension_candidate() -> mlua::Result<()> {
        let result = runtime_from_source(
            r#"
register_tool("broken", {
  description = "broken",
  parameters = "not a schema",
  handler = function() end,
})
"#,
            ExtensionRuntimeServices::new(),
        );

        assert!(result.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn disabled_hooks_preserve_ordinary_commands() -> mlua::Result<()> {
        use crate::core::lifecycle::LifecycleEventKind;
        let (services, requests) = fake_process_services("");
        let runtime = runtime_from_source(
            r#"
register_extension({name = "disabled", capabilities = {process = {SessionStart = {"rtk"}, Command = {"rtk"}}}})
on("SessionStart", function(_, ctx) ctx.process.run("rtk", {}) end)
on("AgentStart", function(_, ctx) ctx.ui.notify("must not run") end)
on("resources_discover", function(_, ctx) ctx.ui.notify("must not run") end)
register_command("check", {handler = function(_, ctx) ctx.process.run("rtk", {}) end})
"#,
            services.hook_policy(super::super::ExtensionHookPolicy::Disabled),
        )?;
        for runtime in [
            &runtime,
            &runtime.prepare_generation(allowing_permission())?,
        ] {
            assert!(runtime.hook_slots().is_empty());
            assert!(!runtime.has_process_hooks());
            for event in [
                LifecycleEventKind::SessionStart,
                LifecycleEventKind::AgentStart,
                LifecycleEventKind::ResourcesDiscover,
            ] {
                assert!(runtime.emit(&LifecycleEvent::new(event)).await?.is_none());
            }
            let (_, idle) = tokio::sync::watch::channel(true);
            runtime.run_command("check", "", idle, None).await?;
            assert!(runtime.take_notifications()?.is_empty());
        }
        assert_eq!(requests.lock().unwrap().len(), 2);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn hook_processes_leave_command_sandbox_intact() -> mlua::Result<()> {
        let root = PathBuf::from("/tmp").join(format!("phi-lua-host-{}", uuid::Uuid::new_v4()));
        let working_dir = root.join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::write(root.join("host-input"), "host data").unwrap();
        let protected = working_dir.join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(protected.join("config"), "original").unwrap();
        let runner = super::super::LocalExtensionProcessRunner::new(
            crate::sandbox::Sandbox::new(true, &working_dir)
                .unwrap()
                .with_write_protection(vec![protected.clone()], Vec::new())
                .unwrap(),
        );
        let runtime = runtime_from_source(
            r#"
register_extension({
  name = "boundary",
  capabilities = {process = {SessionStart = {"bash"}, Command = {"bash"}}},
})
on("SessionStart", function(_, ctx)
  local output = ctx.process.run("bash", {"-c", "cat ../host-input > hook-copy && printf hook > protected/config"})
  assert(output.success, output.stderr)
end)
register_command("check", {handler = function(_, ctx)
  local output = ctx.process.run("bash", {"-c", "cat ../host-input"})
  assert(not output.success, "ordinary command escaped the sandbox")
  output = ctx.process.run("bash", {"-c", "printf command > protected/config"})
  assert(not output.success, "ordinary command escaped write protection")
end})
"#,
            ExtensionRuntimeServices::new()
                .process_runner(Arc::new(runner))
                .process_policy(test_process_policy())
                .permission(allowing_permission()),
        )?;
        runtime
            .emit(&LifecycleEvent::new(
                crate::core::lifecycle::LifecycleEventKind::SessionStart,
            ))
            .await?;
        assert_eq!(
            std::fs::read_to_string(working_dir.join("hook-copy")).unwrap(),
            "host data"
        );
        let (_, idle) = tokio::sync::watch::channel(true);
        runtime.run_command("check", "", idle, None).await?;
        assert_eq!(
            std::fs::read_to_string(protected.join("config")).unwrap(),
            "hook"
        );
        std::fs::remove_dir_all(root).unwrap();
        Ok(())
    }

    #[tokio::test]
    async fn hook_processes_do_not_grant_command_permissions() -> mlua::Result<()> {
        for mode in [PermissionMode::Ask, PermissionMode::Deny] {
            let (services, requests) = fake_process_services("");
            let (sender, mut approvals) = tokio::sync::mpsc::channel(1);
            let permission = PermissionRuntime::new(Arc::new(Mutex::new(PermissionChecker::new(
                &PermissionConfig {
                    mode,
                    ..Default::default()
                },
                None,
            ))))
            .interactive(true)
            .with_interaction_sender(Some(sender));
            let runtime = runtime_from_source(
                r#"
register_extension({
  name = "boundary",
  capabilities = { process = { Notification = { "rtk" }, Command = { "rtk" } } },
})
on("Notification", function(_, ctx) ctx.process.run("rtk", {}) end)
register_command("check", { handler = function(_, ctx) ctx.process.run("rtk", {}) end })
"#,
                services.permission(permission.clone()),
            )?;
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                runtime.emit(&LifecycleEvent::new(
                    crate::core::lifecycle::LifecycleEventKind::Notification,
                )),
            )
            .await
            .expect("hooks cannot wait for Agent approval")?;
            assert!(approvals.try_recv().is_err());
            let (_, idle) = tokio::sync::watch::channel(true);
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                tokio::select! {
                    result = runtime.run_command("check", "", idle, None) => {
                        assert_eq!(mode, PermissionMode::Deny);
                        assert!(result.is_err());
                    }
                    approval = approvals.recv() => {
                        assert_eq!(mode, PermissionMode::Ask);
                        assert!(approval.is_some());
                    }
                }
            })
            .await
            .expect("ordinary commands must still require Agent approval");
            assert_eq!(requests.lock().unwrap().len(), 1);
        }
        Ok(())
    }

    #[tokio::test]
    async fn lifecycle_process_capability_rewrites_tool_input() -> mlua::Result<()> {
        let (services, requests) = fake_process_services("rewritten command");
        let runtime = runtime_from_source(
            r#"
register_extension({
  name = "rtk",
  capabilities = { process = { PreToolUse = { "rtk" } } },
})
on("PreToolUse", function(event, ctx)
  local output = ctx.process.run("rtk", { "rewrite", event.payload.tool_input.command })
  return { updated_input = { command = output.stdout } }
end)
"#,
            services,
        )?;
        let result = runtime
            .emit(&LifecycleEvent::with_payload(
                crate::core::lifecycle::LifecycleEventKind::PreToolUse,
                serde_json::json!({ "tool_input": { "command": "echo old" } }),
            ))
            .await?
            .unwrap();

        assert_eq!(
            pre_tool_use_result(result).updated_input.unwrap()["command"],
            "rewritten command"
        );
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].program, "rtk");
        assert_eq!(requests[0].args, ["rewrite", "echo old"]);
        assert_eq!(requests[0].context, "PreToolUse");
        Ok(())
    }

    #[tokio::test]
    async fn pre_tool_use_hooks_preserve_rewrites_and_context() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
on("PreToolUse", function()
  return { updated_input = { command = "rtk git status" } }
end)
on("PreToolUse", function()
  return { additional_context = "checked" }
end)
on("PreToolUse", function(event)
  return { updated_input = { command = event.payload.tool_input.command .. " && echo second" } }
end)
"#,
            ExtensionRuntimeServices::new(),
        )?;

        let result = runtime
            .emit(&LifecycleEvent::with_payload(
                crate::core::lifecycle::LifecycleEventKind::PreToolUse,
                serde_json::json!({ "tool_input": { "command": "git status" } }),
            ))
            .await?
            .unwrap();

        let result = pre_tool_use_result(result);
        assert_eq!(
            result.updated_input.unwrap()["command"],
            "git status && echo second"
        );
        assert_eq!(result.additional_context, ["checked"]);
        Ok(())
    }

    #[tokio::test]
    async fn pre_tool_use_preserves_strongest_permission_decision() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
on("PreToolUse", function()
  return { permissionDecision = "deny", reason = "blocked" }
end)
on("PreToolUse", function()
  return { permission_decision = "allow" }
end)
"#,
            ExtensionRuntimeServices::new(),
        )?;

        let result = runtime
            .emit(&LifecycleEvent::new(
                crate::core::lifecycle::LifecycleEventKind::PreToolUse,
            ))
            .await?
            .unwrap();

        assert_eq!(
            pre_tool_use_result(result).permission_decision,
            Some(PermissionDecision::Deny)
        );
        Ok(())
    }

    #[tokio::test]
    async fn process_capability_is_scoped_to_declared_context() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
register_extension({
  name = "scoped",
  capabilities = { process = { SessionStart = { "rtk" } } },
})
on("PreToolUse", function(_event, ctx)
  return { updated_input = { has_process = ctx.process ~= nil } }
end)
"#,
            ExtensionRuntimeServices::new(),
        )?;
        let result = runtime
            .emit(&LifecycleEvent::with_payload(
                crate::core::lifecycle::LifecycleEventKind::PreToolUse,
                serde_json::Value::Null,
            ))
            .await?
            .unwrap();

        assert_eq!(
            pre_tool_use_result(result).updated_input.unwrap()["has_process"],
            false
        );
        Ok(())
    }

    #[tokio::test]
    async fn process_capability_cannot_escape_its_handler_invocation() -> mlua::Result<()> {
        let (services, _) = fake_process_services("");
        let runtime = runtime_from_source(
            r#"
local stored_process
register_extension({
  name = "lease",
  capabilities = { process = { SessionStart = { "rtk" } } },
})
on("SessionStart", function(_event, ctx)
  stored_process = ctx.process
end)
on("PreToolUse", function()
  stored_process.run("rtk", { "rewrite", "echo old" })
end)
"#,
            services,
        )?;
        runtime
            .emit(&LifecycleEvent::new(
                crate::core::lifecycle::LifecycleEventKind::SessionStart,
            ))
            .await?;

        let error = runtime
            .emit(&LifecycleEvent::new(
                crate::core::lifecycle::LifecycleEventKind::PreToolUse,
            ))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("no longer active"));
        Ok(())
    }

    #[tokio::test]
    async fn process_capability_rejects_untrusted_source() -> mlua::Result<()> {
        let policy = super::super::ExtensionProcessPolicy::default()
            .trust_root(PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        let runtime = runtime_from_source(
            r#"
register_extension({
  name = "spoofed",
  capabilities = { process = { PreToolUse = { "rtk" } } },
})
on("PreToolUse", function(_event, ctx)
  ctx.process.run("rtk", { "rewrite", "echo old" })
end)
"#,
            ExtensionRuntimeServices::new()
                .process_runner(Arc::new(FailingProcessRunner))
                .process_policy(policy),
        )?;

        let error = runtime
            .emit(&LifecycleEvent::new(
                crate::core::lifecycle::LifecycleEventKind::PreToolUse,
            ))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("source is not trusted"));
        Ok(())
    }

    #[tokio::test]
    async fn process_capability_rejects_undeclared_programs() -> mlua::Result<()> {
        let (services, _) = fake_process_services("");
        let runtime = runtime_from_source(
            r#"
register_extension({
  name = "restricted",
  capabilities = { process = { PreToolUse = { "rtk" } } },
})
on("PreToolUse", function(_event, ctx)
  ctx.process.run("uv", { "--version" })
end)
"#,
            services,
        )?;

        let error = runtime
            .emit(&LifecycleEvent::with_payload(
                crate::core::lifecycle::LifecycleEventKind::PreToolUse,
                serde_json::Value::Null,
            ))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("not allowed to run 'uv'"));
        Ok(())
    }

    #[test]
    fn registered_keybinding_receives_generic_event() -> mlua::Result<()> {
        let path =
            std::env::temp_dir().join(format!("phi-lua-keybinding-{}.lua", std::process::id()));
        std::fs::write(
            &path,
            r#"
register_extension({
  name = "keybinding-test",
  capabilities = { process = { Keybinding = { "editor" } } },
})
register_keybinding("ctrl+e", {
  context = "picker.rewind",
  handler = function(event, ctx)
    ctx.ui.notify(event.selection.value)
    ctx.process.spawn("editor", { "file.rs" })
  end,
})
"#,
        )
        .map_err(mlua::Error::external)?;
        let source = path.canonicalize()?.to_string_lossy().to_string();
        let runtime = LuaRuntime::load_with_services(
            vec![path.clone()],
            ExtensionRuntimeServices::new()
                .process_policy(test_process_policy())
                .sandbox_effects(true),
        )?;

        let binding = &runtime.keybindings()[0];
        assert_eq!(binding.context, "picker.rewind");
        assert_eq!(binding.key, "ctrl+e");
        let outcome = runtime.run_keybinding(
            binding.id,
            &serde_json::json!({ "selection": { "value": "rewind:entry" } }),
        )?;
        assert_eq!(outcome.notifications, ["rewind:entry"]);
        assert_eq!(
            outcome.effects,
            [ExtensionEffect::SpawnProcess {
                extension: "keybinding-test".into(),
                source,
                context: "Keybinding".into(),
                program: "editor".into(),
                args: vec!["file.rs".into()],
                sandboxed: true,
            }]
        );
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn keybinding_process_effect_requires_trusted_source() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
register_extension({
  name = "ungranted-keybinding",
  capabilities = { process = { Keybinding = { "editor" } } },
})
register_keybinding("ctrl+e", {
  context = "picker.rewind",
  handler = function(_, ctx) ctx.process.spawn("editor", { "file.rs" }) end,
})
"#,
            ExtensionRuntimeServices::new(),
        )?;

        let error = runtime
            .run_keybinding(runtime.keybindings()[0].id, &serde_json::Value::Null)
            .unwrap_err();

        assert!(error.to_string().contains("source is not trusted"));
        Ok(())
    }

    #[test]
    fn runtime_excludes_ambient_system_capabilities() -> mlua::Result<()> {
        let runtime = LuaRuntime::load(Vec::new())?;
        let globals = runtime.lua.globals();

        assert!(globals.get::<Option<Table>>("io")?.is_none());
        assert!(globals.get::<Option<Table>>("os")?.is_none());
        assert!(globals.get::<Option<Table>>("package")?.is_none());
        assert!(globals.get::<Option<Function>>("require")?.is_none());
        assert!(globals.get::<Option<Function>>("dofile")?.is_none());
        assert!(globals.get::<Option<Function>>("loadfile")?.is_none());
        Ok(())
    }

    #[test]
    fn keybinding_ids_are_scoped_to_the_loaded_runtime() -> mlua::Result<()> {
        let path =
            std::env::temp_dir().join(format!("phi-lua-runtime-id-{}.lua", std::process::id()));
        std::fs::write(
            &path,
            r#"register_keybinding("ctrl+e", {
  context = "picker.rewind",
  handler = function() end,
})"#,
        )
        .map_err(mlua::Error::external)?;
        let first = LuaRuntime::load(vec![path.clone()])?;
        let second = LuaRuntime::load(vec![path.clone()])?;
        let _ = std::fs::remove_file(path);

        assert_ne!(first.keybindings()[0].id, second.keybindings()[0].id);
        assert!(
            second
                .run_keybinding(first.keybindings()[0].id, &serde_json::Value::Null)
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn command_arguments_drive_declarations_and_admission() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
register_command("inspect", {
  arguments = { usage = "<mode>", values = {{ value = "fast", alias = "f" }, { value = "full" }} },
  handler = function(args, ctx) ctx.ui.notify(args) end,
})
register_command("status", { arguments = false, handler = function(_, ctx) ctx.ui.notify("called") end })
register_command("freeform", { handler = function(args, ctx) ctx.ui.notify(args) end })
"#,
            ExtensionRuntimeServices::new(),
        )?;
        let commands: Vec<super::super::RegisteredCommand> =
            serde_json::from_value(serde_json::to_value(runtime.commands()).unwrap()).unwrap();
        let arguments = commands
            .iter()
            .find(|command| command.name == "inspect")
            .unwrap()
            .arguments
            .as_ref()
            .unwrap();
        assert_eq!(arguments.usage, "<mode>");
        assert_eq!(arguments.values[0].value, "fast");
        assert_eq!(arguments.values[0].alias.as_deref(), Some("f"));
        assert!(
            commands
                .iter()
                .find(|command| command.name == "status")
                .unwrap()
                .arguments
                .is_none()
        );
        assert_eq!(
            commands
                .iter()
                .find(|command| command.name == "freeform")
                .unwrap()
                .arguments,
            Some(Default::default())
        );
        let (_, idle) = tokio::sync::watch::channel(true);
        let error = runtime
            .run_command("status", "extra", idle.clone(), None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("does not accept arguments"));
        assert!(matches!(
            runtime.run_command("status", "", idle.clone(), None).await?,
            Some(super::super::ExtensionCommandOutcome::Completed(Some(message))) if message == "called"
        ));
        for name in ["inspect", "freeform"] {
            assert!(matches!(
                runtime.run_command(name, "unlisted argument", idle.clone(), None).await?,
                Some(super::super::ExtensionCommandOutcome::Completed(Some(message)))
                    if message == "unlisted argument"
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn explicit_notifications_are_scoped_and_drained_once() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
on("AgentStart", function(_, ctx) ctx.ui.notify("hook notification") end)
on("AgentEnd", function(_, ctx)
  ctx.ui.notify("notification before failure")
  error("observer failed")
end)
register_tool("test", {
  description = "test",
  handler = function(_, ctx)
    ctx.ui.notify("tool notification")
    return "tool output"
  end,
})
register_tool_display("test", function(_, ctx)
  ctx.ui.notify("display notification")
  return "display title"
end)
register_command("test", {
  handler = function(_, ctx) ctx.ui.notify("command notification") end,
})
register_keybinding("ctrl+e", {
  context = "picker.rewind",
  handler = function(_, ctx) ctx.ui.notify("keybinding notification") end,
})
"#,
            ExtensionRuntimeServices::new(),
        )?;
        runtime
            .emit(&LifecycleEvent::new(LifecycleEventKind::AgentStart))
            .await?;
        assert_eq!(runtime.call_tool("test", "{}".into())?, "tool output");
        assert_eq!(
            runtime
                .format_tool_display("test", "{}")?
                .unwrap()
                .title
                .as_deref(),
            Some("display title")
        );
        let (_, idle) = tokio::sync::watch::channel(true);
        assert_eq!(
            runtime.run_command("test", "", idle, None).await?,
            Some(super::super::ExtensionCommandOutcome::Completed(Some(
                "command notification".into()
            )))
        );
        let keybinding =
            runtime.run_keybinding(runtime.keybindings()[0].id, &serde_json::Value::Null)?;
        assert_eq!(keybinding.notifications, ["keybinding notification"]);
        assert_eq!(
            runtime.take_notifications()?,
            [
                "hook notification",
                "tool notification",
                "display notification"
            ]
        );
        assert!(runtime.take_notifications()?.is_empty());
        let other = runtime_from_source("", ExtensionRuntimeServices::new())?;
        assert!(
            runtime
                .emit(&LifecycleEvent::new(LifecycleEventKind::AgentEnd))
                .await
                .is_err()
        );
        assert!(other.take_notifications()?.is_empty());
        assert_eq!(
            runtime.take_notifications()?,
            ["notification before failure"]
        );
        assert!(runtime.take_notifications()?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn explicit_notifications_survive_operation_failure() -> mlua::Result<()> {
        let runtime = runtime_from_source(
            r#"
register_command("test", {
  handler = function(_, ctx)
    ctx.ui.notify("command notification before failure")
    error("command failed")
  end,
})
register_keybinding("ctrl+e", {
  context = "picker.rewind",
  handler = function(_, ctx)
    ctx.ui.notify("keybinding notification before failure")
    error("keybinding failed")
  end,
})
"#,
            ExtensionRuntimeServices::new(),
        )?;
        let (_, idle) = tokio::sync::watch::channel(true);
        assert!(runtime.run_command("test", "", idle, None).await.is_err());
        assert_eq!(
            runtime.take_notifications()?,
            ["command notification before failure"]
        );
        assert!(
            runtime
                .run_keybinding(runtime.keybindings()[0].id, &serde_json::Value::Null)
                .is_err()
        );
        assert_eq!(
            runtime.take_notifications()?,
            ["keybinding notification before failure"]
        );
        assert!(runtime.take_notifications()?.is_empty());
        Ok(())
    }

    #[test]
    fn tool_display_context_has_no_process_capability() -> mlua::Result<()> {
        let path =
            std::env::temp_dir().join(format!("phi-lua-display-purity-{}.lua", std::process::id()));
        std::fs::write(
            &path,
            r#"
register_extension({
  name = "display-purity",
  capabilities = { process = { ToolDisplay = { "rtk" } } },
})
register_tool_display("test", function(_, ctx)
  return ctx.process and "impure" or "pure"
end)
register_tool_display("structured", function(args)
  return { title = "label", input = args.command, title_suffix = " (metadata)", content = "prefix", expand = true }
end)
"#,
        )
        .map_err(mlua::Error::external)?;
        let runtime = LuaRuntime::load(vec![path.clone()]);
        let _ = std::fs::remove_file(path);
        let runtime = runtime?;
        let display = runtime.format_tool_display("test", "{}")?.unwrap();

        assert_eq!(display.title.as_deref(), Some("pure"));
        assert_eq!(display.input, None);
        let display = runtime
            .format_tool_display("structured", r#"{"command":"echo test"}"#)?
            .unwrap();
        assert_eq!(display.title.as_deref(), Some("label"));
        assert_eq!(display.input.as_deref(), Some("echo test"));
        assert_eq!(display.title_suffix.as_deref(), Some(" (metadata)"));
        assert_eq!(display.content.as_deref(), Some("prefix"));
        assert!(display.expand);
        Ok(())
    }
}
