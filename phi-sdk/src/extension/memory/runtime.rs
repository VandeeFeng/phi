use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use compact_str::CompactString;

use super::MemoryContextKind;
use super::service::MemoryService;
use super::tape::gate::{
    KeywordAnchorCandidate, KeywordAnchorInstruction, TapeMutationState, clear_keyword_anchor,
    detect_unseen_keyword_context, set_keyword_anchor,
};
use super::tools::collect_memory_tools;
use crate::config::RuntimeConfig;
use crate::core::lifecycle::{PromptContextOutcome, UserPromptSubmitSource};
use crate::core::turn::{InjectedContext, TurnPrompt};
use crate::extension::{BuiltinExtension, ExtensionToolContext};
use crate::hooks::{Hook, HookRegistry};
use crate::tools::{ToolMetadata, ToolPromptMetadata, ToolRegistration};

const SUBAGENT_DENIED_TOOLS: &[&str] = &["tape_anchor", "tape_delete", "tape_reset", "memory_sync"];

pub(in crate::extension) fn delegated_tool_denials() -> &'static [&'static str] {
    SUBAGENT_DENIED_TOOLS
}

const MEMORY_COMMANDS: &[(&str, &str, Option<(&str, &[&str])>)] = &[
    (
        "memory-check",
        "check memory repository status and folder structure",
        Some(("[--global] [max-lines]", &["--global"])),
    ),
    (
        "memory-inject",
        "inject memory context for an optional prompt",
        Some(("[prompt]", &[])),
    ),
    ("memory-refresh", "refresh memory context from files", None),
    (
        "memory-review",
        "show tape anchor summary and recent anchors",
        Some(("[limit: 1–100, default: 50]", &[])),
    ),
];

pub(in crate::extension) fn command_names() -> Vec<&'static str> {
    MEMORY_COMMANDS.iter().map(|(name, _, _)| *name).collect()
}

pub(in crate::extension) fn adapter_commands(
    config: Option<&super::MemoryConfig>,
) -> Vec<super::super::RegisteredCommand> {
    let Some(config) = config.filter(|config| config.enabled) else {
        return Vec::new();
    };
    MEMORY_COMMANDS
        .iter()
        .copied()
        .filter(|(name, _, _)| *name != "memory-review" || config.tape_enabled())
        .map(
            |(name, description, arguments)| super::super::RegisteredCommand {
                name: name.into(),
                description: Some(description.into()),
                source: BuiltinExtension::Memory.path().into(),
                target: if name == "memory-inject" {
                    super::super::CommandTarget::Prompt
                } else {
                    super::super::CommandTarget::Application {
                        action: name.into(),
                    }
                },
                arguments: arguments
                    .map(|(usage, values)| crate::extension::CommandArguments::new(usage, values)),
            },
        )
        .collect()
}

pub(in crate::extension) fn skill_declarations(
    config: Option<&super::MemoryConfig>,
) -> Vec<super::super::BuiltinSkill> {
    let enabled = config.is_some_and(|config| config.enabled && config.skill_enabled());
    let tape_enabled = enabled && config.is_some_and(super::MemoryConfig::tape_enabled);
    [
        ("memory-digest", tape_enabled),
        ("memory-import", enabled),
        ("memory-init", enabled),
        ("memory-write", enabled),
        ("planning-with-files", enabled),
    ]
    .into_iter()
    .map(|(name, enabled)| super::super::BuiltinSkill { name, enabled })
    .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryAgentKind {
    Primary,
    Delegated,
}

#[derive(Clone, Default)]
pub(crate) struct MemorySession {
    session_id: Option<CompactString>,
    service: Option<Arc<MemoryService>>,
    state: Arc<Mutex<TapeMutationState>>,
}

pub(crate) struct MemorySubmission {
    state: Arc<Mutex<TapeMutationState>>,
    candidate: Option<KeywordAnchorCandidate>,
}

#[derive(Default)]
pub(crate) struct MemoryDispatch {
    state: Option<Arc<Mutex<TapeMutationState>>>,
    instruction: Option<KeywordAnchorInstruction>,
}

impl MemorySession {
    pub(crate) fn create(
        config: &RuntimeConfig,
        session_id: CompactString,
        working_dir: &Path,
    ) -> anyhow::Result<Self> {
        let memory = config.extensions.memory.clone().unwrap_or_default();
        let service = if config.extensions.is_enabled(BuiltinExtension::Memory) {
            Some(Arc::new(
                MemoryService::create(&memory, session_id.clone(), working_dir.to_path_buf())
                    .map_err(|error| {
                        anyhow::anyhow!("failed to initialize memory service: {error}")
                    })?,
            ))
        } else {
            None
        };
        Ok(Self {
            session_id: Some(session_id),
            service,
            ..Default::default()
        })
    }

    pub(crate) fn collect_tools(
        &self,
        context: &ExtensionToolContext<'_>,
    ) -> Vec<ToolRegistration> {
        let Some(config) = context
            .config
            .extensions
            .memory
            .as_ref()
            .filter(|_| !context.options.no_tools)
        else {
            return Vec::new();
        };
        let metadata = if config.tape_enabled() {
            HashMap::from([(
                "tape_anchor".to_string(),
                ToolMetadata::sequential(ToolPromptMetadata::default().with_guidelines([
                    "To inspect code changes between Tape anchors, resolve their meta.git.commitOid values with memory_search, use Bash to run git diff, and use tape_read separately for conversation evidence.",
                ])),
            )])
        } else {
            HashMap::new()
        };
        collect_memory_tools(
            config,
            self.session_id.clone(),
            Some(self.state.clone()),
            self.service.clone(),
            context.permission.clone(),
            &metadata,
        )
    }

    pub(crate) fn prepare_submission(
        &self,
        config: &RuntimeConfig,
        working_dir: &Path,
        agent_kind: MemoryAgentKind,
        prompt: &mut TurnPrompt,
        hooks: &mut HookRegistry,
    ) -> anyhow::Result<MemorySubmission> {
        let candidate = if agent_kind == MemoryAgentKind::Delegated
            || !matches!(
                prompt.source,
                UserPromptSubmitSource::Interactive
                    | UserPromptSubmitSource::Print
                    | UserPromptSubmitSource::Rpc
                    | UserPromptSubmitSource::Sdk
            ) {
            prompt
                .injected_context
                .retain(|context| context.source != MemoryContextKind::KeywordIntro.source());
            None
        } else {
            self.keyword_candidate(config, working_dir, &prompt.prompt)?
        };
        if let Some(candidate) = &candidate {
            let message = candidate.instruction.message.clone();
            hooks.register_hook(Hook::prompt_context(move |_| {
                let message = message.clone();
                async move {
                    Ok(PromptContextOutcome {
                        contexts: vec![InjectedContext {
                            source: MemoryContextKind::KeywordIntro.source(),
                            content: message,
                        }],
                        ..Default::default()
                    })
                }
            }));
        }
        Ok(MemorySubmission {
            state: self.state.clone(),
            candidate,
        })
    }

    pub(crate) fn end_turn(&self) {
        if let Ok(mut state) = self.state.lock() {
            clear_keyword_anchor(&mut state);
        }
    }

    fn keyword_candidate(
        &self,
        config: &RuntimeConfig,
        working_dir: &Path,
        prompt: &str,
    ) -> anyhow::Result<Option<KeywordAnchorCandidate>> {
        if !config.extensions.is_enabled(BuiltinExtension::Memory) {
            return Ok(None);
        }
        let Some(memory) = config.extensions.memory.as_ref() else {
            return Ok(None);
        };
        let settings = super::settings_from_config(memory);
        if !super::tape_gate_enabled(&settings, working_dir) {
            return Ok(None);
        }
        let keywords = settings
            .tape
            .as_ref()
            .and_then(|tape| tape.anchor.as_ref())
            .and_then(|anchor| anchor.keywords.as_ref());
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("tape anchor gate is unavailable"))?;
        Ok(detect_unseen_keyword_context(&state, prompt, keywords))
    }
}

impl MemorySubmission {
    pub(crate) fn dispatch(&self) -> MemoryDispatch {
        MemoryDispatch {
            state: Some(self.state.clone()),
            instruction: self
                .candidate
                .as_ref()
                .map(|candidate| candidate.instruction.clone()),
        }
    }

    pub(crate) fn accept(self) {
        if let Some(candidate) = self.candidate {
            // Admission has committed; bookkeeping must not turn it into an error.
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            candidate.accept(&mut state);
        }
    }
}

impl MemoryDispatch {
    pub(crate) fn activate(self) -> anyhow::Result<()> {
        let Some(state) = self.state else {
            return Ok(());
        };
        let mut state = state
            .lock()
            .map_err(|_| anyhow::anyhow!("tape anchor gate is unavailable"))?;
        clear_keyword_anchor(&mut state);
        if let Some(instruction) = self.instruction {
            set_keyword_anchor(&mut state, instruction);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::{
        MemoryConfig, MemoryDirSettings, TapeAnchorConfig, TapeConfig, TapeKeywordConfig,
    };
    use proptest::prelude::*;

    fn keyword_config() -> RuntimeConfig {
        RuntimeConfig {
            extensions: crate::extension::ExtensionConfig {
                memory: Some(MemoryConfig {
                    tape: Some(TapeConfig {
                        enabled: Some(true),
                        only_git: Some(false),
                        anchor: Some(TapeAnchorConfig {
                            keywords: Some(TapeKeywordConfig {
                                global: vec!["remember".into()],
                                project: Vec::new(),
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn keyword_prompt() -> TurnPrompt {
        TurnPrompt {
            prompt: "remember this decision".into(),
            source: UserPromptSubmitSource::Sdk,
            injected_context: Vec::new(),
            images: Vec::new(),
            tool_policy: Default::default(),
        }
    }

    #[test]
    fn service_uses_explicit_scope_without_materialization() {
        let root = Path::new("/tmp").join(format!("phi-memory-runtime-{}", uuid::Uuid::new_v4()));
        let child = root.join("child-worktree");
        std::fs::create_dir_all(&child).unwrap();
        let mut config = RuntimeConfig {
            extensions: crate::extension::ExtensionConfig {
                memory: Some(MemoryConfig {
                    memory_dir: Some(MemoryDirSettings {
                        local_path: Some(root.join("memory").display().to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let runtime = MemorySession::create(&config, "child".into(), &child).unwrap();
        assert_eq!(
            runtime.service.unwrap().memory_project_dir(),
            root.join("memory/child-worktree")
        );
        assert!(!root.join("memory").exists());
        config.extensions.disabled.insert(BuiltinExtension::Memory);
        let runtime = MemorySession::create(&config, "child".into(), &child).unwrap();
        assert!(runtime.service.is_none());
        assert!(!root.join("memory").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    proptest! {
        #[test]
        fn consent_changes_only_at_admission_and_dispatch(
            admitted in any::<bool>(),
            dispatched in any::<bool>(),
        ) {
            let runtime = MemorySession::default();
            let config = keyword_config();
            let submission = runtime.prepare_submission(
                &config, Path::new("/tmp"), MemoryAgentKind::Primary,
                &mut keyword_prompt(), &mut HookRegistry::new(),
            ).unwrap();
            prop_assert!(submission.candidate.is_some());
            let dispatch = submission.dispatch();
            prop_assert!(runtime.state.lock().unwrap().pending_keyword_anchor.is_none());
            prop_assert!(runtime.state.lock().unwrap().seen_keyword_contexts.is_empty());
            if admitted {
                submission.accept();
            } else {
                drop(submission);
            }
            prop_assert!(runtime.state.lock().unwrap().pending_keyword_anchor.is_none());
            prop_assert_eq!(!runtime.state.lock().unwrap().seen_keyword_contexts.is_empty(), admitted);
            if admitted && dispatched {
                dispatch.activate().unwrap();
            }
            prop_assert_eq!(runtime.state.lock().unwrap().pending_keyword_anchor.is_some(), admitted && dispatched);
            runtime.end_turn();
            prop_assert!(runtime.state.lock().unwrap().pending_keyword_anchor.is_none());
            prop_assert_eq!(runtime.keyword_candidate(&config, Path::new("/tmp"), "remember this decision").unwrap().is_some(), !admitted);
        }

        #[test]
        fn consent_requires_enabled_memory_and_original_human_input(
            source in prop::sample::select(vec![
                UserPromptSubmitSource::Interactive, UserPromptSubmitSource::Print,
                UserPromptSubmitSource::Rpc, UserPromptSubmitSource::Sdk,
                UserPromptSubmitSource::Internal, UserPromptSubmitSource::Extension,
            ]),
            agent_kind in prop_oneof![Just(MemoryAgentKind::Primary), Just(MemoryAgentKind::Delegated)],
            memory_enabled in any::<bool>(),
            tape_enabled in any::<bool>(),
            disabled in any::<bool>(),
        ) {
            let runtime = MemorySession::default();
            let mut config = keyword_config();
            let memory = config.extensions.memory.as_mut().unwrap();
            memory.enabled = memory_enabled;
            memory.tape.as_mut().unwrap().enabled = Some(tape_enabled);
            if disabled {
                config.extensions.disabled.insert(BuiltinExtension::Memory);
            }
            let mut prompt = keyword_prompt();
            prompt.source = source;
            let submission = runtime.prepare_submission(
                &config, Path::new("/tmp"), agent_kind,
                &mut prompt, &mut HookRegistry::new(),
            ).unwrap();
            let human_input = !matches!(source, UserPromptSubmitSource::Internal | UserPromptSubmitSource::Extension);
            prop_assert_eq!(submission.candidate.is_some(),
                memory_enabled && tape_enabled && !disabled && human_input && agent_kind == MemoryAgentKind::Primary);
            prop_assert!(runtime.state.lock().unwrap().pending_keyword_anchor.is_none());
        }
    }

    #[test]
    fn a_new_dispatch_retires_previous_keyword_evidence() {
        let runtime = MemorySession::default();
        let config = keyword_config();
        let submission = runtime
            .prepare_submission(
                &config,
                Path::new("/tmp"),
                MemoryAgentKind::Primary,
                &mut keyword_prompt(),
                &mut HookRegistry::new(),
            )
            .unwrap();
        let dispatch = submission.dispatch();
        submission.accept();
        dispatch.activate().unwrap();
        assert!(
            runtime
                .state
                .lock()
                .unwrap()
                .pending_keyword_anchor
                .is_some()
        );
        let mut prompt = keyword_prompt();
        prompt.prompt = "ordinary next input".into();
        let submission = runtime
            .prepare_submission(
                &config,
                Path::new("/tmp"),
                MemoryAgentKind::Primary,
                &mut prompt,
                &mut HookRegistry::new(),
            )
            .unwrap();
        submission.dispatch().activate().unwrap();
        assert!(
            runtime
                .state
                .lock()
                .unwrap()
                .pending_keyword_anchor
                .is_none()
        );
    }
}
