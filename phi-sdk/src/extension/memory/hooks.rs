use serde::{Deserialize, Serialize};

use super::memory_core;
use super::memory_git::{LocalMemoryGit, MemoryGit};
use super::types::MemorySettings;
use crate::core::lifecycle::{
    PromptContextEvent, PromptContextOutcome, SessionStartReason, UserPromptSubmitOutcome,
};
use crate::hooks::{Hook, HookError, HookNotice};

const MEMORY_INJECT_COMMAND: &str = "/memory-inject";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookTrigger {
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
}

impl HookTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionStart => "sessionStart",
            Self::SessionEnd => "sessionEnd",
            Self::UserPromptSubmit => "userPromptSubmit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HookAction {
    Pull,
    Push,
}

impl HookAction {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Pull => "pull",
            Self::Push => "push",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HookConfig {
    #[serde(default = "default_session_start_hooks")]
    pub session_start: Vec<HookAction>,
    #[serde(default)]
    pub session_end: Vec<HookAction>,
    #[serde(default)]
    pub user_prompt_submit: Vec<HookAction>,
}

impl Default for HookConfig {
    fn default() -> Self {
        Self {
            session_start: default_session_start_hooks(),
            session_end: Vec::new(),
            user_prompt_submit: Vec::new(),
        }
    }
}

fn get_hook_actions(settings: &MemorySettings, trigger: HookTrigger) -> Vec<HookAction> {
    let hooks = settings.hooks.clone().unwrap_or_default();
    match trigger {
        HookTrigger::SessionStart => hooks.session_start,
        HookTrigger::SessionEnd => hooks.session_end,
        HookTrigger::UserPromptSubmit => hooks.user_prompt_submit,
    }
}

fn default_session_start_hooks() -> Vec<HookAction> {
    vec![HookAction::Pull]
}

#[derive(Debug, Clone)]
pub struct MemoryHooks {
    settings: MemorySettings,
    background_startup: bool,
    prompt_only: bool,
}

impl MemoryHooks {
    pub fn new(settings: MemorySettings) -> Self {
        Self {
            settings,
            background_startup: false,
            prompt_only: false,
        }
    }

    pub fn background_startup(mut self) -> Self {
        self.background_startup = true;
        self
    }

    pub fn prompt_only(mut self) -> Self {
        self.prompt_only = true;
        self
    }

    pub fn hooks(&self) -> Vec<Hook> {
        if self.prompt_only {
            return self.prompt_hooks();
        }
        let hooks = self.clone();
        let startup = move |event: crate::core::lifecycle::SessionStartEvent| {
            let hooks = hooks.clone();
            async move {
                Ok(
                    if !matches!(
                        event.reason,
                        SessionStartReason::New | SessionStartReason::Resume
                    ) {
                        hooks.run_trigger(HookTrigger::SessionStart).await
                    } else {
                        Vec::new()
                    },
                )
            }
        };
        let start = if self.background_startup {
            Hook::session_start_with_mode(
                crate::hooks::SessionStartMode::BeforeFirstPrompt,
                startup,
            )
        } else {
            Hook::session_start(startup)
        };
        let hooks = self.clone();
        let end = Hook::session_end(move |_| {
            let hooks = hooks.clone();
            async move { Ok(hooks.run_trigger(HookTrigger::SessionEnd).await) }
        });
        let mut registrations = vec![start, end];
        registrations.extend(self.prompt_hooks());
        registrations
    }

    pub fn prompt_hooks(&self) -> Vec<Hook> {
        let hooks = self.clone();
        let transform = Hook::user_prompt_submit(move |event| {
            let hooks = hooks.clone();
            async move {
                let notices = if event.delegated_child {
                    Vec::new()
                } else {
                    hooks.run_trigger(HookTrigger::UserPromptSubmit).await
                };
                Ok(UserPromptSubmitOutcome::Continue {
                    prompt: if memory_inject_prompt(&event.original_prompt).is_some() {
                        memory_inject_prompt(&event.prompt).unwrap_or(&event.prompt)
                    } else {
                        &event.prompt
                    }
                    .to_string(),
                    injected_context: Vec::new(),
                    notices,
                })
            }
        });
        let hooks = self.clone();
        let automatic_enabled =
            self.settings.enabled.unwrap_or(true) && self.settings.injection.is_some();
        let automatic = Hook::prompt_context_with_recovery(
            move |event| {
                let hooks = hooks.clone();
                async move { hooks.prepare_context(event).await }
            },
            |_, error| {
                Ok(PromptContextOutcome {
                    notices: vec![HookNotice::Warning {
                        message: format!("Memory context failed: {error}"),
                    }],
                    ..Default::default()
                })
            },
        )
        .with_once(true)
        .with_id("builtin:memory:automatic-context")
        .when(move |event: &PromptContextEvent| {
            automatic_enabled
                && !event.delegated_child
                && memory_inject_prompt(&event.original_prompt).is_none()
        });
        let hooks = self.clone();
        let explicit = Hook::prompt_context(move |event| {
            let hooks = hooks.clone();
            async move { hooks.prepare_context(event).await }
        })
        .when(|event: &PromptContextEvent| memory_inject_prompt(&event.original_prompt).is_some());
        vec![transform, automatic, explicit]
    }

    async fn prepare_context(
        &self,
        event: PromptContextEvent,
    ) -> Result<PromptContextOutcome, HookError> {
        let requested = memory_inject_prompt(&event.original_prompt).is_some();
        let mut outcome = PromptContextOutcome::default();
        if !self.settings.enabled.unwrap_or(true) {
            return if requested {
                Err(HookError::new(
                    "Memory is disabled; /memory-inject requires Memory to be enabled",
                ))
            } else {
                Ok(outcome)
            };
        }
        if event.delegated_child {
            return if requested {
                Err(HookError::new("No Memory/Tape context available to inject"))
            } else {
                Ok(outcome)
            };
        }
        if !requested && self.settings.injection.is_none() {
            return Ok(outcome);
        }
        let settings = self.settings.clone();
        let result = tokio::task::spawn_blocking(move || {
            if memory_core::tape_gate_enabled(&settings, &event.working_dir) {
                let mut prepared = memory_core::precompute_tape_context(
                    &settings,
                    &event.working_dir,
                    &event.transcript,
                )?;
                let warnings = prepared
                    .as_mut()
                    .map(|context| context.take_warnings())
                    .unwrap_or_default();
                let context = memory_core::build_tape_injection_with_prompt(
                    &settings,
                    &event.working_dir,
                    &event.transcript,
                    (!event.prompt.is_empty()).then_some(event.prompt.as_str()),
                    prepared.as_ref(),
                )?;
                Ok((context, warnings))
            } else {
                memory_core::build_memory_injection(&settings, &event.working_dir)
                    .map(|context| (context, Vec::new()))
            }
        })
        .await
        .map_err(|error| anyhow::anyhow!("memory preparation task failed: {error}"))
        .and_then(|result| result);
        let result = result.map(|(context, warnings)| {
            outcome.notices.extend(
                warnings
                    .into_iter()
                    .map(|message| HookNotice::Warning { message }),
            );
            context
        });
        let injection = match result {
            Ok(Some(injection)) => {
                if requested {
                    injection.with_retrieval_hint()
                } else {
                    injection
                }
            }
            Ok(None) if requested => {
                return Err(HookError::new("No Memory/Tape context available to inject"));
            }
            Ok(None) => return Ok(outcome),
            Err(error) if requested => {
                return Err(HookError::new(format!("Memory context failed: {error:#}")));
            }
            Err(error) => {
                outcome.notices.push(HookNotice::Warning {
                    message: format!("Memory context failed: {error}"),
                });
                return Ok(outcome);
            }
        };
        let item_count = injection.file_count();
        outcome.contexts = injection.into_injected_contexts();
        if let Some(context) = outcome
            .contexts
            .iter()
            .find(|context| !context.content.is_empty())
        {
            outcome.notices.push(HookNotice::ContextInjected {
                source: context.source.clone(),
                item_count,
                requested,
            });
        }
        Ok(outcome)
    }

    async fn run_trigger(&self, trigger: HookTrigger) -> Vec<HookNotice> {
        if !self.settings.enabled.unwrap_or(true) {
            return Vec::new();
        }
        let cancellation = tokio_util::sync::CancellationToken::new();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        let mut notices = Vec::new();
        for action in get_hook_actions(&self.settings, trigger) {
            let git = LocalMemoryGit::new(
                memory_core::get_memory_repo_dir(&self.settings),
                self.settings
                    .memory_dir
                    .as_ref()
                    .and_then(|dir| dir.repo_url.clone())
                    .or_else(|| self.settings.repo_url.clone()),
            )
            .with_cancellation(cancellation.clone());
            let operation = action.clone();
            let result = tokio::task::spawn_blocking(move || match operation {
                HookAction::Pull => git.pull(),
                HookAction::Push => git.push(),
            })
            .await
            .map_err(|error| anyhow::anyhow!("memory hook task failed: {error}"))
            .and_then(|result| result);
            match result {
                Ok(result) if result.message.is_empty() || (result.success && !result.updated) => {}
                Ok(result) => {
                    let message = format!(
                        "{} ({}/{})",
                        result.message,
                        trigger.as_str(),
                        action.as_str()
                    );
                    notices.push(if result.success {
                        HookNotice::Info { message }
                    } else {
                        HookNotice::Warning { message }
                    });
                }
                Err(error) => notices.push(HookNotice::Warning {
                    message: format!(
                        "memory hook failed ({}/{}): {error}",
                        trigger.as_str(),
                        action.as_str()
                    ),
                }),
            }
        }
        notices
    }
}

pub fn memory_inject_prompt(input: &str) -> Option<&str> {
    let rest = input.strip_prefix(MEMORY_INJECT_COMMAND)?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    Some(rest.trim())
}

#[cfg(test)]
mod tests {
    use super::super::MemoryContextKind;
    use super::*;
    use crate::hooks::HookRegistry;

    #[tokio::test]
    async fn generated_memory_command_is_not_consumed() {
        use crate::core::lifecycle::{
            UserPromptDispatch, UserPromptSubmitAction, UserPromptSubmitEvent,
            UserPromptSubmitSource,
        };

        for (original, rewrite, expected) in [
            (
                "/template",
                Some("/memory-inject generated"),
                "/memory-inject generated",
            ),
            ("/memory-inject inspect", None, "inspect"),
        ] {
            let mut hooks = HookRegistry::new();
            if let Some(rewrite) = rewrite {
                hooks.register_hook(Hook::user_prompt_submit(move |_| async move {
                    Ok(UserPromptSubmitOutcome::Continue {
                        notices: Vec::new(),
                        prompt: rewrite.into(),
                        injected_context: Vec::new(),
                    })
                }));
            }
            for hook in MemoryHooks::new(memory_core::settings_from_config(&Default::default()))
                .prompt_hooks()
            {
                hooks.register_hook(hook);
            }
            let result = crate::hooks::emit_user_prompt_submit(
                &hooks,
                UserPromptSubmitEvent {
                    delegated_child: false,
                    original_prompt: original.into(),
                    prompt: original.into(),
                    source: UserPromptSubmitSource::Sdk,
                    dispatch: UserPromptDispatch::Immediate,
                },
            )
            .await;
            assert!(
                matches!(result, UserPromptSubmitAction::Continue { prompt, .. } if prompt == expected),
                "{original}"
            );
        }
    }

    fn context_event(working_dir: std::path::PathBuf) -> PromptContextEvent {
        PromptContextEvent {
            original_prompt: "nebula".into(),
            prompt: "quartz".into(),
            source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
            dispatch: crate::core::lifecycle::UserPromptDispatch::Immediate,
            transcript: std::sync::Arc::new(crate::transcript::Transcript::new()),
            working_dir,
            delegated_child: false,
        }
    }

    #[tokio::test]
    async fn automatic_injection_requires_a_mode_but_manual_injection_does_not() {
        use super::super::{MemoryConfig, MemoryDirSettings, MemoryInjectionMode, TapeConfig};

        let root = std::path::Path::new("/tmp")
            .join(format!("phi-injection-mode-{}", uuid::Uuid::new_v4()));
        let working_dir = root.join("workspace");
        let memory_dir = root.join("memory/workspace/core/project");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(
            memory_dir.join("context.md"),
            "---\ndescription: quartz\n---\nquartz notes",
        )
        .unwrap();
        for tape_enabled in [false, true] {
            for injection in [None, Some(MemoryInjectionMode::MessageAppend)] {
                let settings = memory_core::settings_from_config(&MemoryConfig {
                    injection,
                    memory_dir: Some(MemoryDirSettings {
                        local_path: Some(root.join("memory").display().to_string()),
                        ..Default::default()
                    }),
                    tape: Some(TapeConfig {
                        enabled: Some(tape_enabled),
                        only_git: Some(false),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
                let hooks = MemoryHooks::new(settings);
                for requested in [false, true] {
                    let expected = requested || injection.is_some();
                    let mut event = context_event(working_dir.clone());
                    if requested {
                        event.original_prompt = MEMORY_INJECT_COMMAND.into();
                    }
                    let outcome = hooks.prepare_context(event).await.unwrap();
                    assert_eq!(
                        !outcome.contexts.is_empty(),
                        expected,
                        "tape={tape_enabled}, injection={injection:?}"
                    );
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn automatic_attempt_is_consumed_without_admission() {
        use super::super::{MemoryConfig, MemoryDirSettings, MemoryInjectionMode};

        for initially_empty in [false, true] {
            let root = std::path::Path::new("/tmp")
                .join(format!("phi-memory-once-{}", uuid::Uuid::new_v4()));
            let working_dir = root.join("workspace");
            let memory_dir = root.join("memory/workspace/core/project");
            std::fs::create_dir_all(&working_dir).unwrap();
            std::fs::create_dir_all(&memory_dir).unwrap();
            let content = "---\ndescription: quartz\n---\nquartz notes";
            if !initially_empty {
                std::fs::write(memory_dir.join("context.md"), content).unwrap();
            }
            let settings = memory_core::settings_from_config(&MemoryConfig {
                injection: Some(MemoryInjectionMode::MessageAppend),
                memory_dir: Some(MemoryDirSettings {
                    local_path: Some(root.join("memory").display().to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            });
            let mut registry = HookRegistry::new();
            for hook in MemoryHooks::new(settings).prompt_hooks() {
                registry.register_hook(hook);
            }
            let mut transcript = crate::transcript::Transcript::new();
            let first = registry
                .prepare_context(context_event(working_dir.clone()))
                .await
                .unwrap();
            assert_eq!(first.contexts.is_empty(), initially_empty);
            std::fs::write(memory_dir.join("context.md"), content).unwrap();
            transcript.reset_context(None).unwrap();
            let mut event = context_event(working_dir);
            event.transcript = std::sync::Arc::new(transcript.clone());
            let next = registry.prepare_context(event.clone()).await.unwrap();
            assert!(next.contexts.is_empty());
            event.original_prompt = MEMORY_INJECT_COMMAND.into();
            for _ in 0..2 {
                let explicit = registry.prepare_context(event.clone()).await.unwrap();
                assert!(!explicit.contexts.is_empty());
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn tape_hook_selects_transformed_input_with_graph_metadata() {
        let root =
            std::env::temp_dir().join(format!("phi-hook-selection-{}", uuid::Uuid::new_v4()));
        let working_dir = root.join("workspace");
        let memory_dir = root.join("memory/workspace/core/project");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&memory_dir).unwrap();
        for topic in ["nebula", "quartz"] {
            std::fs::write(
                memory_dir.join(format!("{topic}.md")),
                format!("---\ndescription: {topic}\ntags: [{topic}]\n---\n{topic} notes"),
            )
            .unwrap();
        }
        let settings = memory_core::settings_from_config(&super::super::MemoryConfig {
            memory_dir: Some(super::super::MemoryDirSettings {
                local_path: Some(root.join("memory").display().to_string()),
                ..Default::default()
            }),
            injection: Some(super::super::MemoryInjectionMode::MessageAppend),
            hooks: Some(HookConfig {
                session_start: Vec::new(),
                ..Default::default()
            }),
            tape: Some(super::super::TapeConfig {
                enabled: Some(true),
                only_git: Some(false),
                context: Some(super::super::TapeContextConfig {
                    file_limit: Some(1),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        let outcome = MemoryHooks::new(settings)
            .prepare_context(context_event(working_dir))
            .await
            .unwrap();
        assert_eq!(outcome.contexts.len(), 1);
        assert!(outcome.contexts[0].content.contains("quartz.md"));
        assert!(!outcome.contexts[0].content.contains("nebula.md"));
        assert_eq!(
            outcome.contexts[0].source,
            MemoryContextKind::Graph.source()
        );
        assert!(outcome.notices.iter().any(|notice| matches!(
            notice,
            HookNotice::ContextInjected {
                source,
                item_count: 1,
                requested: false,
            } if *source == MemoryContextKind::Graph.source()
        )));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn prompt_sync_finishes_before_context_is_read() {
        let root = std::env::temp_dir().join(format!("phi-hook-sync-{}", uuid::Uuid::new_v4()));
        let remote = root.join("remote");
        let working_dir = root.join("workspace");
        let memory_dir = remote.join("workspace/core/project");
        std::fs::create_dir_all(&working_dir).unwrap();
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(
            memory_dir.join("synced.md"),
            "---\ndescription: synchronized context\n---\nnotes",
        )
        .unwrap();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Phi",
                "-c",
                "user.email=phi@example.invalid",
                "commit",
                "-m",
                "seed context",
            ],
        ] {
            let output = std::process::Command::new("git")
                .current_dir(&remote)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let settings = memory_core::settings_from_config(&super::super::MemoryConfig {
            memory_dir: Some(super::super::MemoryDirSettings {
                local_path: Some(root.join("memory").display().to_string()),
                repo_url: Some(remote.display().to_string()),
                ..Default::default()
            }),
            injection: Some(super::super::MemoryInjectionMode::MessageAppend),
            hooks: Some(HookConfig {
                session_start: Vec::new(),
                session_end: Vec::new(),
                user_prompt_submit: vec![HookAction::Pull],
            }),
            ..Default::default()
        });
        let mut registry = crate::hooks::HookRegistry::new();
        for hook in MemoryHooks::new(settings).prompt_hooks().into_iter().rev() {
            registry.register_hook(hook);
        }
        async fn submit(registry: &HookRegistry) {
            crate::hooks::emit_user_prompt_submit(
                registry,
                crate::core::lifecycle::UserPromptSubmitEvent {
                    original_prompt: "input".into(),
                    prompt: "input".into(),
                    source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                    dispatch: crate::core::lifecycle::UserPromptDispatch::Immediate,
                    delegated_child: false,
                },
            )
            .await;
        }
        submit(&registry).await;
        let prepared = registry
            .prepare_context(context_event(working_dir.clone()))
            .await
            .unwrap();
        assert_eq!(prepared.contexts.len(), 1);
        assert!(
            prepared.contexts[0]
                .content
                .contains("synchronized context")
        );
        registry.record_notices(prepared.notices);
        assert!(
            registry
                .take_notices()
                .iter()
                .any(|notice| matches!(notice, HookNotice::Info { .. }))
        );
        let updated = "---\ndescription: updated context\n---\nnew notes";
        std::fs::write(memory_dir.join("synced.md"), updated).unwrap();
        let output = std::process::Command::new("git")
            .current_dir(&remote)
            .args([
                "-c",
                "user.name=Phi",
                "-c",
                "user.email=phi@example.invalid",
                "commit",
                "-am",
                "update context",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        submit(&registry).await;
        let prepared = registry
            .prepare_context(context_event(working_dir))
            .await
            .unwrap();
        assert!(prepared.contexts.is_empty());
        registry.record_notices(prepared.notices);
        assert!(
            !registry
                .take_notices()
                .iter()
                .any(|notice| matches!(notice, HookNotice::ContextInjected { .. }))
        );
        assert_eq!(
            std::fs::read_to_string(root.join("memory/workspace/core/project/synced.md")).unwrap(),
            updated
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn session_end_budget_stops_memory_git_before_runtime_exit() {
        const CHILD_ROOT: &str = "PHI_TEST_MEMORY_HOOK_EXIT_ROOT";
        const TEST_NAME: &str = "extension::memory::hooks::tests::session_end_budget_stops_memory_git_before_runtime_exit";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = std::path::PathBuf::from(root);
            let settings = memory_core::settings_from_config(&super::super::MemoryConfig {
                memory_dir: Some(super::super::MemoryDirSettings {
                    local_path: Some(root.display().to_string()),
                    repo_url: Some(root.display().to_string()),
                    ..Default::default()
                }),
                hooks: Some(HookConfig {
                    session_start: Vec::new(),
                    session_end: vec![HookAction::Push],
                    user_prompt_submit: Vec::new(),
                }),
                ..Default::default()
            });
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut registry = crate::hooks::HookRegistry::new();
                for hook in MemoryHooks::new(settings).hooks() {
                    registry.register_hook(hook);
                }
                crate::hooks::emit_session_end(
                    &registry,
                    crate::lifecycle::SessionEndReason::Quit,
                    None,
                )
                .await;
                assert!(root.join(".git/hook-started").exists());
                assert!(registry.take_notices().iter().any(|notice| matches!(
                    notice,
                    HookNotice::Warning { message } if message == "SessionEnd timed out"
                )));
            });
            drop(runtime);
            return;
        }

        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("phi-memory-hook-exit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["config", "user.name", "Phi"],
            vec!["config", "user.email", "phi@example.invalid"],
        ] {
            let output = std::process::Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        std::fs::write(root.join("note.md"), "pending memory change").unwrap();
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::write(
            &hook,
            "#!/bin/sh\ntouch .git/hook-started\nsleep 30 &\nwait\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_ROOT, &root);
        let result =
            crate::utils::process::run_sync(&mut child, std::time::Duration::from_secs(5), None);
        std::fs::remove_dir_all(root).unwrap();
        let output = result.expect("Memory Git blocked process exit past the SessionEnd budget");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn memory_inject_parses_optional_prompt_without_matching_other_commands() {
        assert_eq!(memory_inject_prompt("/memory-inject"), Some(""));
        assert_eq!(memory_inject_prompt("/memory-inject  \n"), Some(""));
        assert_eq!(
            memory_inject_prompt("/memory-inject\tinspect code"),
            Some("inspect code")
        );
        assert_eq!(memory_inject_prompt("/memory-inject-extra inspect"), None);
        assert_eq!(memory_inject_prompt("inspect code"), None);
    }

    #[test]
    fn hook_configuration_rejects_unknown_actions_and_retired_events() {
        for value in [
            serde_json::json!({ "userPromptSubmit": ["unsupported"] }),
            serde_json::json!({ "beforeAgentStart": ["pull"] }),
        ] {
            assert!(serde_json::from_value::<HookConfig>(value).is_err());
        }
        let config: HookConfig = serde_json::from_value(serde_json::json!({
            "userPromptSubmit": ["pull", "push"]
        }))
        .unwrap();
        assert_eq!(
            config.user_prompt_submit,
            [HookAction::Pull, HookAction::Push]
        );
    }
}
