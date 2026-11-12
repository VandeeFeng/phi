use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};

use super::{
    HookCondition, HookControl, HookHandler, HookHandlerFuture, HookMatcher, HookOptions,
    HookOutput, HookRegistry, HookResponse, HookResult,
};
use crate::core::lifecycle::{
    HookAction, LifecycleEvent, LifecycleEventKind, PermissionDecision, PostToolUseResult,
    PreToolUseResult,
};
use crate::sandbox::Sandbox;

const DEFAULT_TIMEOUT_SECONDS: u64 = 2;
const MAX_TIMEOUT_SECONDS: u64 = 30;
const BLOCK_EXIT_CODE: i32 = 2;

/// Settings declarations are compiled into ordinary lifecycle registrations.
#[derive(Debug, Clone, Default)]
pub struct CommandHooks {
    session_start: Vec<HookGroup>,
    user_prompt_submit: Vec<HookGroup>,
    user_prompt_expansion: Vec<HookGroup>,
    pre_tool_use: Vec<HookGroup>,
    post_tool_use: Vec<HookGroup>,
    stop: Vec<HookGroup>,
    notification: Vec<HookGroup>,
}

impl<'de> Deserialize<'de> for CommandHooks {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut groups =
            std::collections::BTreeMap::<String, Vec<HookGroup>>::deserialize(deserializer)?;
        let declarations = Self {
            session_start: groups.remove("SessionStart").unwrap_or_default(),
            user_prompt_submit: groups.remove("UserPromptSubmit").unwrap_or_default(),
            user_prompt_expansion: groups.remove("UserPromptExpansion").unwrap_or_default(),
            pre_tool_use: groups.remove("PreToolUse").unwrap_or_default(),
            post_tool_use: groups.remove("PostToolUse").unwrap_or_default(),
            stop: groups.remove("Stop").unwrap_or_default(),
            notification: groups.remove("Notification").unwrap_or_default(),
        };
        if !groups.is_empty() {
            return Err(serde::de::Error::custom("unsupported command hook event"));
        }
        declarations.validate().map_err(serde::de::Error::custom)?;
        Ok(declarations)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookGroup {
    #[serde(default)]
    matcher: Option<HookMatcher>,
    hooks: Vec<CommandHook>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum CommandHook {
    Command {
        #[serde(deserialize_with = "deserialize_command")]
        command: String,
        #[serde(default = "default_timeout", deserialize_with = "deserialize_timeout")]
        timeout: u64,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        once: bool,
        #[serde(default, rename = "if")]
        condition: Option<HookCondition>,
    },
}

impl CommandHook {
    fn control(&self, event: &str, matcher: Option<HookMatcher>) -> HookResult<HookControl> {
        let Self::Command {
            id,
            once,
            condition,
            ..
        } = self;
        HookControl::from_options(
            "settings:global",
            event,
            HookOptions {
                id: id.clone(),
                once: *once,
                condition: condition.clone(),
                timeout: None,
            },
            matcher,
        )
    }
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
}

fn deserialize_command<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<String, D::Error> {
    let command = String::deserialize(deserializer)?;
    if command.trim().is_empty() {
        return Err(serde::de::Error::custom("hook command must not be empty"));
    }
    Ok(command)
}

fn deserialize_timeout<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let timeout = u64::deserialize(deserializer)?;
    if !(1..=MAX_TIMEOUT_SECONDS).contains(&timeout) {
        return Err(serde::de::Error::custom(
            "hook timeout must be 1..=30 seconds",
        ));
    }
    Ok(timeout)
}

impl CommandHooks {
    fn validate(&self) -> HookResult<()> {
        let mut identities = std::collections::BTreeSet::new();
        for (event, groups) in self.groups() {
            for group in groups {
                HookControl::from_options(
                    "settings:global",
                    event.as_str(),
                    HookOptions::default(),
                    group.matcher.clone(),
                )?;
                for hook in &group.hooks {
                    let control = hook.control(event.as_str(), group.matcher.clone())?;
                    if let Some(identity) = control.identity()
                        && !identities.insert(identity)
                    {
                        return Err(super::HookError::new("duplicate command hook identity"));
                    }
                }
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.groups()
            .into_iter()
            .all(|(_, groups)| groups.iter().all(|group| group.hooks.is_empty()))
    }

    fn groups(&self) -> [(LifecycleEventKind, &[HookGroup]); 7] {
        [
            (LifecycleEventKind::SessionStart, &self.session_start),
            (
                LifecycleEventKind::UserPromptSubmit,
                &self.user_prompt_submit,
            ),
            (
                LifecycleEventKind::UserPromptExpansion,
                &self.user_prompt_expansion,
            ),
            (LifecycleEventKind::PreToolUse, &self.pre_tool_use),
            (LifecycleEventKind::PostToolUse, &self.post_tool_use),
            (LifecycleEventKind::Stop, &self.stop),
            (LifecycleEventKind::Notification, &self.notification),
        ]
    }

    pub(crate) fn register(&self, bus: &mut HookRegistry, sandbox: Sandbox) -> HookResult<()> {
        self.validate()?;
        for (event, groups) in self.groups() {
            for (group_index, group) in groups.iter().enumerate() {
                for (handler_index, hook) in group.hooks.iter().enumerate() {
                    let CommandHook::Command {
                        command, timeout, ..
                    } = hook;
                    bus.register_handler(Arc::new(CommandHookRuntime {
                        event: event.clone(),
                        control: hook
                            .control(event.as_str(), group.matcher.clone())?
                            .with_default_id(
                                "settings:global",
                                &format!("{}:{group_index}", event.as_str()),
                                handler_index,
                            ),
                        command: command.clone(),
                        timeout: Duration::from_secs(*timeout),
                        sandbox: sandbox.unrestricted(),
                    }));
                }
            }
        }
        Ok(())
    }
}

struct CommandHookRuntime {
    event: LifecycleEventKind,
    control: HookControl,
    command: String,
    timeout: Duration,
    sandbox: Sandbox,
}

impl CommandHookRuntime {
    async fn run(&self, event: &LifecycleEvent) -> anyhow::Result<Option<HookResponse>> {
        if event.kind != self.event {
            return Ok(None);
        }
        if !self.control.start_adapter(event) {
            return Ok(None);
        }
        let mut input = event.payload.clone();
        input["hook_event_name"] = event.kind.as_str().into();
        let input = serde_json::to_string(&input)?;
        let args = vec!["-c".to_string(), self.command.clone()];
        let output =
            crate::utils::process::run(&self.sandbox, "bash", &args, Some(&input), self.timeout)
                .await?;
        if !output.success {
            let reason = if output.stderr.trim().is_empty() {
                format!("command hook exited with {:?}", output.exit_code)
            } else {
                output.stderr.trim().to_string()
            };
            if output.exit_code == Some(BLOCK_EXIT_CODE)
                && matches!(
                    event.kind,
                    LifecycleEventKind::PreToolUse
                        | LifecycleEventKind::UserPromptSubmit
                        | LifecycleEventKind::UserPromptExpansion
                        | LifecycleEventKind::Stop
                )
            {
                return Ok(Some(HookResponse {
                    block: true,
                    reason: Some(reason),
                    ..Default::default()
                }));
            }
            anyhow::bail!("{} command hook failed: {reason}", event.kind.as_str());
        }
        let text = output.stdout.trim();
        let payload = if text.starts_with('{') {
            anyhow::ensure!(!output.truncated, "command hook JSON output was truncated");
            let mut value: Value = serde_json::from_str(text)?;
            let fields = value
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("command hook output must be an object"))?;
            if let Some(specific) = fields.remove("hookSpecificOutput") {
                let specific = specific
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("hookSpecificOutput must be an object"))?;
                for (key, value) in specific {
                    fields.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
            value
        } else if matches!(
            event.kind,
            LifecycleEventKind::UserPromptSubmit | LifecycleEventKind::UserPromptExpansion
        ) && !text.is_empty()
        {
            json!({"additionalContext": text})
        } else {
            json!({})
        };
        let block = payload.get("decision").and_then(Value::as_str) == Some("block")
            || payload.get("block").and_then(Value::as_bool) == Some(true)
            || payload
                .get("permissionDecision")
                .or_else(|| payload.get("permission_decision"))
                .and_then(Value::as_str)
                == Some("deny");
        let reason = payload
            .get("reason")
            .or_else(|| payload.get("permissionDecisionReason"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut result = HookResponse {
            block,
            reason,
            ..Default::default()
        };
        let contexts = match payload
            .get("additionalContext")
            .or_else(|| payload.get("additional_context"))
        {
            Some(Value::String(text)) => vec![text.clone()],
            Some(_) => anyhow::bail!("additionalContext must be a string"),
            None => Vec::new(),
        };
        result.output = match event.kind {
            LifecycleEventKind::PreToolUse => Some(HookOutput::Action(HookAction::PreToolUse(
                PreToolUseResult {
                    permission_decision: block.then_some(PermissionDecision::Deny),
                    reason: result.reason.clone(),
                    updated_input: payload
                        .get("updatedInput")
                        .or_else(|| payload.get("updated_input"))
                        .cloned(),
                    additional_context: contexts,
                },
            ))),
            LifecycleEventKind::PostToolUse => Some(HookOutput::Action(HookAction::PostToolUse(
                PostToolUseResult {
                    updated_output: payload
                        .get("updated_output")
                        .or_else(|| payload.get("updatedToolOutput"))
                        .cloned(),
                    additional_context: contexts,
                    ..Default::default()
                },
            ))),
            LifecycleEventKind::UserPromptSubmit => {
                let mut payload = payload;
                let mut injected = match payload.get("injected_context") {
                    Some(value) => serde_json::from_value::<Vec<crate::context::InjectedContext>>(
                        value.clone(),
                    )?,
                    None => Vec::new(),
                };
                injected.extend(contexts.into_iter().map(|content| {
                    crate::context::InjectedContext {
                        source: crate::transcript::ContextSource::Hook,
                        content,
                    }
                }));
                payload["injected_context"] = serde_json::to_value(injected)?;
                Some(HookOutput::Payload(payload))
            }
            _ => Some(HookOutput::Payload(payload)),
        };
        Ok(Some(result))
    }
}

impl HookHandler for CommandHookRuntime {
    fn control(&self) -> Option<&HookControl> {
        Some(&self.control)
    }
    fn timeout(&self, _default: Duration) -> Option<Duration> {
        // The process runner owns the command deadline.
        None
    }

    fn is_generation_bound(&self) -> bool {
        true
    }

    fn has_agent_turn_end(&self) -> bool {
        self.event == LifecycleEventKind::AgentTurnEnd
    }

    fn has_task_created(&self) -> bool {
        false
    }

    fn has_task_completed(&self) -> bool {
        false
    }

    fn handle<'a>(&'a self, event: &'a LifecycleEvent) -> HookHandlerFuture<'a> {
        Box::pin(self.run(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::lifecycle::{PermissionDecision, ToolResultEvent, ToolUseEvent};
    use crate::core::lifecycle::{
        UserPromptDispatch, UserPromptSubmitAction, UserPromptSubmitEvent, UserPromptSubmitSource,
    };
    use crate::hooks;
    use crate::permission::{PermissionConfig, PermissionMode};
    use crate::{Phi, PhiRuntimeServices};
    use proptest::prelude::*;
    use serde_json::json;
    use std::path::Path;

    fn factory(hooks: serde_json::Value, permission: PermissionConfig) -> Phi {
        Phi::builder()
            .provider("openai")
            .model("test")
            .context(crate::context::load(true))
            .command_hooks(serde_json::from_value(hooks).unwrap())
            .build()
            .unwrap()
            .with_runtime_services(
                PhiRuntimeServices::new()
                    .permission_policy(permission, Some("/tmp".into()))
                    .interactive(false)
                    .sandbox(Sandbox::new(false, "/tmp").unwrap()),
            )
    }

    fn tool_event(name: &str) -> ToolUseEvent {
        ToolUseEvent {
            tool_name: name.into(),
            tool_use_id: "call-test".into(),
            tool_input: json!({"file_path": "untrusted ' ; $(exit 2)\npath"}),
        }
    }

    #[test]
    fn command_hook_configuration_is_strict() {
        for value in [
            json!({"StopFailure": []}),
            json!({"PreToolUse": [{"matcher": {"regex": "["}, "hooks": []}]}),
            json!({"PreToolUse": [{"hooks": [{"type": "prompt", "command": "true"}]}]}),
            json!({"PreToolUse": [{"hooks": [{"type": "command", "command": " "}]}]}),
            json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "true", "async": true}]}]}),
            json!({"UserPromptSubmit": [{"matcher": "unsupported", "hooks": []}]}),
            json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "true", "if": {"field": "source", "equals": "sdk"}}]}]}),
            json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "true", "if": {"field": "tool_name", "regex": "["}}]}]}),
            json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "true", "execution": "background_observation"}]}]}),
            json!({"SessionStart": [{"hooks": [{"type": "command", "command": "true", "start_mode": "before_first_prompt"}]}]}),
            json!({"PreToolUse": [{"hooks": [
                {"type": "command", "command": "true", "once": true, "id": "duplicate"},
                {"type": "command", "command": "true", "once": true, "id": "duplicate"}
            ]}]}),
        ] {
            assert!(serde_json::from_value::<CommandHooks>(value).is_err());
        }
        for matcher in [json!(""), json!("*"), json!({"regex": "Edit|Write"})] {
            let hooks: CommandHooks = serde_json::from_value(json!({
                "PreToolUse": [{"matcher": matcher, "hooks": [{"type": "command", "command": "true"}]}]
            })).unwrap();
            assert!(!hooks.is_empty());
        }
    }

    #[tokio::test]
    async fn command_once_consumes_invalid_output_and_process_failure() {
        let directory =
            Path::new("/tmp").join(format!("phi-command-once-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let attempts = directory.join("attempts");
        let phi = factory(
            json!({"PreToolUse": [{"matcher": "Write", "hooks": [{
                "type": "command", "once": true,
                "if": {"field": "tool_input.file_path", "regex": "untrusted"},
                "command": format!("printf 'attempt\\n' >> {}; printf '{{'", attempts.display())
            }]}]}),
            PermissionConfig {
                mode: PermissionMode::Allow,
                ..Default::default()
            },
        );
        let original = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
        assert!(
            hooks::emit_pre_tool_use(&original.hooks, tool_event("WriteMore"))
                .await
                .permission_decision
                .is_none()
        );
        assert!(!attempts.exists());
        assert_eq!(
            hooks::emit_pre_tool_use(&original.hooks, tool_event("Write"))
                .await
                .permission_decision,
            Some(PermissionDecision::Deny)
        );
        let next = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
        next.hooks.continue_bindings(&original.hooks);
        assert!(
            hooks::emit_pre_tool_use(&next.hooks, tool_event("Write"))
                .await
                .permission_decision
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(&attempts).unwrap().lines().count(),
            1
        );
        let independent = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
        hooks::emit_pre_tool_use(&independent.hooks, tool_event("Write")).await;
        assert_eq!(
            std::fs::read_to_string(&attempts).unwrap().lines().count(),
            2
        );
        let failed = factory(
            json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "exit 1", "once": true, "id": "failed"}]}]}),
            PermissionConfig::default(),
        );
        let resources = failed.prepare_resources(Path::new("/tmp"), None).unwrap();
        assert_eq!(
            hooks::emit_pre_tool_use(&resources.hooks, tool_event("Write"))
                .await
                .permission_decision,
            Some(PermissionDecision::Deny)
        );
        assert!(
            hooks::emit_pre_tool_use(&resources.hooks, tool_event("Write"))
                .await
                .permission_decision
                .is_none()
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    proptest! {
        #[test]
        fn command_timeout_is_always_bounded(timeout in prop_oneof![0_u64..=40, any::<u64>()]) {
            let result = serde_json::from_value::<CommandHooks>(json!({
                "PreToolUse": [{"hooks": [{"type": "command", "command": "true", "timeout": timeout}]}]
            }));
            prop_assert_eq!(result.is_ok(), (1..=MAX_TIMEOUT_SECONDS).contains(&timeout));
        }
    }

    #[tokio::test]
    async fn command_hooks_match_deliver_json_and_preserve_results() {
        let root = Path::new("/tmp").join(format!("phi-hooks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let input = root.join("input.json");
        let after = root.join("after.json");
        let phi = factory(
            json!({
                "PreToolUse": [{"matcher": {"regex": "^Write$"}, "hooks": [
                    {"type": "command", "command": format!("cat > {}", input.display())},
                    {"type": "command", "command": "echo blocked-by-hook >&2; exit 2"},
                    {"type": "command", "command": "echo must-not-run >&2; exit 1"}
                ]}],
                "PostToolUse": [{"matcher": "Write", "hooks": [
                    {"type": "command", "command": format!("cat > {}; exit 2", after.display())}
                ]}]
            }),
            PermissionConfig {
                mode: PermissionMode::Allow,
                ..Default::default()
            },
        );
        let resources = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
        let skipped = hooks::emit_pre_tool_use(&resources.hooks, tool_event("Read")).await;
        assert!(skipped.permission_decision.is_none());
        assert!(!input.exists());
        let blocked = hooks::emit_pre_tool_use(&resources.hooks, tool_event("Write")).await;
        assert_eq!(blocked.permission_decision, Some(PermissionDecision::Deny));
        let reason = blocked.reason.as_deref().unwrap();
        assert!(reason.contains("blocked-by-hook"));
        assert!(reason.contains("must-not-run"));
        let received: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
        assert_eq!(received["hook_event_name"], "PreToolUse");
        assert_eq!(received["tool_input"], tool_event("Write").tool_input);
        assert_eq!(received["tool_use_id"], "call-test");
        let result = hooks::emit_post_tool_use(
            &resources.hooks,
            ToolResultEvent {
                tool_name: "Write".into(),
                tool_use_id: "call-test".into(),
                tool_input: tool_event("Write").tool_input,
                tool_output: json!({"success": true}),
                duration_ms: Some(1),
            },
        )
        .await;
        assert!(!result.block);
        assert!(result.updated_output.is_none());
        let received: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&after).unwrap()).unwrap();
        assert_eq!(received["hook_event_name"], "PostToolUse");
        assert_eq!(received["tool_response"], json!({"success": true}));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn command_hooks_run_outside_the_tool_sandbox() {
        let root = Path::new("/tmp").join(format!("phi-hook-host-{}", uuid::Uuid::new_v4()));
        let working_dir = root.join("workspace");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::write(root.join("host-input"), "host data").unwrap();
        let phi = factory(
            json!({"PreToolUse": [{"hooks": [{
                "type": "command", "command": "cat ../host-input > hook-copy"
            }]}]}),
            PermissionConfig::default(),
        )
        .with_runtime_services(
            PhiRuntimeServices::new()
                .sandbox(Sandbox::new(true, &working_dir).unwrap())
                .interactive(false),
        );
        let resources = phi.prepare_resources(&working_dir, None).unwrap();
        let result = hooks::emit_pre_tool_use(&resources.hooks, tool_event("Write")).await;
        assert!(result.permission_decision.is_none(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(working_dir.join("hook-copy")).unwrap(),
            "host data"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn command_hooks_do_not_require_agent_permission_grants() {
        for mode in [PermissionMode::Ask, PermissionMode::Deny] {
            let phi = factory(
                json!({"PreToolUse": [{"hooks": [{"type": "command", "command": "true"}]}]}),
                PermissionConfig {
                    mode,
                    ..Default::default()
                },
            );
            let resources = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
            let result = hooks::emit_pre_tool_use(&resources.hooks, tool_event("Write")).await;
            assert!(result.permission_decision.is_none());
            assert!(
                resources
                    .permission()
                    .authorize_command(&crate::permission::command::CommandPermissionRequest::new(
                        "bash",
                        &["-c".into(), "true".into()]
                    ))
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn command_timeout_replaces_default_and_each_command_has_its_own_budget() {
        let mut phi = factory(
            json!({"PreToolUse": [{"hooks": [
                {"type":"command", "command":"sleep 0.03", "timeout":1},
                {"type":"command", "command":"sleep 0.03", "timeout":1}
            ]}]}),
            PermissionConfig {
                mode: PermissionMode::Allow,
                ..Default::default()
            },
        );
        phi.hooks.timeouts.hook_timeout = Duration::from_millis(5);
        let resources = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
        let result = hooks::emit_pre_tool_use(&resources.hooks, tool_event("Write")).await;
        assert!(result.permission_decision.is_none());
    }

    #[tokio::test]
    async fn hook_specific_output_preserves_top_level_rejection() {
        let phi = factory(
            json!({"UserPromptSubmit": [{"hooks": [{"type":"command", "command":
                "printf '%s' '{\"decision\":\"block\",\"reason\":\"policy\",\"hookSpecificOutput\":{\"additionalContext\":\"context\"}}'"
            }]}]}),
            PermissionConfig {
                mode: PermissionMode::Allow,
                ..Default::default()
            },
        );
        let resources = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
        let result = hooks::emit_user_prompt_submit(
            &resources.hooks,
            UserPromptSubmitEvent {
                original_prompt: "original".into(),
                prompt: "original".into(),
                source: UserPromptSubmitSource::Sdk,
                dispatch: UserPromptDispatch::Immediate,
                delegated_child: false,
            },
        )
        .await;
        assert!(
            matches!(result, UserPromptSubmitAction::Handled { reason:Some(reason) } if reason == "policy")
        );
    }

    #[tokio::test]
    async fn command_prompt_output_is_an_atomic_transform_and_context_contribution() {
        let phi = factory(
            json!({"UserPromptSubmit": [{"hooks": [{"type":"command", "command":
                "printf '%s' '{\"prompt\":\"rewritten\",\"additionalContext\":\"context\"}'"
            }]}]}),
            PermissionConfig {
                mode: PermissionMode::Allow,
                ..Default::default()
            },
        );
        let resources = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
        let result = hooks::emit_user_prompt_submit(
            &resources.hooks,
            UserPromptSubmitEvent {
                original_prompt: "original".into(),
                prompt: "original".into(),
                source: UserPromptSubmitSource::Sdk,
                dispatch: UserPromptDispatch::Immediate,
                delegated_child: false,
            },
        )
        .await;
        let UserPromptSubmitAction::Continue {
            prompt,
            injected_context,
            ..
        } = result
        else {
            panic!("prompt rejected");
        };
        assert_eq!(prompt, "rewritten");
        assert_eq!(injected_context.len(), 1);
        assert_eq!(injected_context[0].content, "context");
    }

    #[tokio::test]
    async fn failed_command_hooks_fail_closed() {
        for command in ["exit 1", "kill -TERM $$", "sleep 2"] {
            let phi = factory(
                json!({"PreToolUse": [{"hooks": [
                    {"type": "command", "command": command, "timeout": 1}
                ]}]}),
                PermissionConfig {
                    mode: PermissionMode::Allow,
                    ..Default::default()
                },
            );
            let resources = phi.prepare_resources(Path::new("/tmp"), None).unwrap();
            let result = hooks::emit_pre_tool_use(&resources.hooks, tool_event("Write")).await;
            assert_eq!(result.permission_decision, Some(PermissionDecision::Deny));
        }
    }
}
