//! Adapts extension operations and hook readiness to TUI tasks and notifications.
//! Built-in menus and input contexts belong to `components::input`.

use phi_sdk::extension::{ExtensionKeybindingId, RegisteredKeybinding};
use phi_tui::{InputContext, KeyBindingResolver, KeyInput};

use super::{
    components::input::{UiAction, key_bindings, parse_input_context},
    state::{PickerKind, UiState},
};

pub(crate) struct CompiledExtensionBindings {
    pub(crate) resolver: KeyBindingResolver<UiAction>,
    pub(crate) diagnostics: Vec<String>,
}

pub(crate) fn compile(bindings: &[RegisteredKeybinding]) -> CompiledExtensionBindings {
    let mut resolver = key_bindings();
    let mut diagnostics = Vec::new();
    for binding in bindings {
        let Some((context, key)) = parse_binding(binding, &mut diagnostics) else {
            continue;
        };
        if protected_key(&key) || resolver.resolve(context, &key).is_some() {
            diagnostics.push(format!(
                "extension keybinding conflict: {} in {}",
                binding.key, binding.context
            ));
            continue;
        }
        resolver = resolver.bind(context, key, UiAction::RunExtensionKeybinding(binding.id));
    }
    CompiledExtensionBindings {
        resolver,
        diagnostics,
    }
}

pub(crate) enum ExtensionTaskResult {
    Keybinding(anyhow::Result<crate::runtime::session::ExtensionKeybindingResult>),
    Command {
        name: String,
        result: anyhow::Result<Option<phi_sdk::extension::ExtensionCommandOutcome>>,
    },
    HookNotices {
        notices: Vec<phi_sdk::hooks::HookNotice>,
    },
}

pub(crate) struct ExtensionTaskQueue {
    tx: tokio::sync::mpsc::UnboundedSender<ExtensionTask>,
    results: tokio::sync::mpsc::UnboundedReceiver<ExtensionTaskResult>,
    result_tx: tokio::sync::mpsc::UnboundedSender<ExtensionTaskResult>,
}

enum ExtensionTask {
    Command {
        name: String,
        args: String,
        attachment: crate::runtime::attachment::AgentAttachment,
    },
    HookNotices {
        attachment: crate::runtime::attachment::AgentAttachment,
    },
}

impl Default for ExtensionTaskQueue {
    fn default() -> Self {
        let (result_tx, results) = tokio::sync::mpsc::unbounded_channel();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let worker_results = result_tx.clone();
        tokio::spawn(async move {
            let result_tx = worker_results;
            while let Some(task) = rx.recv().await {
                match task {
                    command @ ExtensionTask::Command { .. } => {
                        let result_tx = result_tx.clone();
                        tokio::spawn(async move {
                            run_queued_task(command, &result_tx).await;
                        });
                    }
                    readiness => run_queued_task(readiness, &result_tx).await,
                }
            }
        });
        Self {
            tx,
            results,
            result_tx,
        }
    }
}

impl ExtensionTaskQueue {
    pub(super) fn poll(&mut self, state: &mut UiState) -> bool {
        let mut changed = false;
        while let Ok(result) = self.results.try_recv() {
            apply_result(result, state);
            changed = true;
        }
        changed
    }

    pub(super) fn run_keybinding(
        &self,
        id: ExtensionKeybindingId,
        attachment: crate::runtime::attachment::AgentAttachment,
        state: &UiState,
        session: &crate::session::Session,
    ) {
        let event = keybinding_event(state, session);
        let result_tx = self.result_tx.clone();
        tokio::spawn(async move {
            let result = attachment.run_extension_keybinding(id, event).await;
            let _ = result_tx.send(ExtensionTaskResult::Keybinding(result));
        });
    }

    pub(crate) fn run_command(
        &self,
        name: String,
        args: String,
        attachment: crate::runtime::attachment::AgentAttachment,
    ) {
        let _ = self.tx.send(ExtensionTask::Command {
            name,
            args,
            attachment,
        });
    }

    pub(crate) fn ready_hook_notices(
        &self,
        attachment: crate::runtime::attachment::AgentAttachment,
    ) {
        let _ = self.tx.send(ExtensionTask::HookNotices { attachment });
    }
}

async fn run_queued_task(
    task: ExtensionTask,
    result_tx: &tokio::sync::mpsc::UnboundedSender<ExtensionTaskResult>,
) {
    match task {
        ExtensionTask::Command {
            name,
            args,
            attachment,
        } => {
            let prompt = format!("/{name} {args}");
            let result = match attachment.extension_before_command(&prompt).await {
                Ok(Some(reason)) => Ok(Some(
                    phi_sdk::extension::ExtensionCommandOutcome::Rejected(reason),
                )),
                Ok(None) => attachment.run_extension_command(name.clone(), args).await,
                Err(error) => Err(error),
            };
            let _ = attachment.extension_after_command(&prompt, true).await;
            let _ = result_tx.send(ExtensionTaskResult::Command { name, result });
        }
        ExtensionTask::HookNotices { attachment } => {
            let notices = attachment
                .ready_hook_notices()
                .await
                .unwrap_or_else(|error| {
                    vec![phi_sdk::hooks::HookNotice::Warning {
                        message: error.to_string(),
                    }]
                });
            let _ = result_tx.send(ExtensionTaskResult::HookNotices { notices });
        }
    }
}

pub(crate) fn apply_result(result: ExtensionTaskResult, state: &mut UiState) {
    match result {
        ExtensionTaskResult::HookNotices { notices } => {
            for notice in notices {
                super::agent::project_hook_notice(state, notice);
            }
        }
        ExtensionTaskResult::Keybinding(result) => {
            let result = match result {
                Ok(outcome) => {
                    for notification in outcome.notifications {
                        state.notify(phi_tui::NotifyKind::Info, notification);
                    }
                    outcome.effect_result
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                state.notify(
                    phi_tui::NotifyKind::Error,
                    format!("extension keybinding failed: {error}"),
                );
            }
        }
        ExtensionTaskResult::Command { name, result } => match result {
            Ok(Some(phi_sdk::extension::ExtensionCommandOutcome::Rejected(reason))) => state
                .notify(
                    phi_tui::NotifyKind::Warning,
                    if reason.is_empty() {
                        format!("/{name} rejected")
                    } else {
                        reason
                    },
                ),
            Ok(Some(phi_sdk::extension::ExtensionCommandOutcome::Completed(Some(
                notification,
            )))) => state.notify(phi_tui::NotifyKind::Info, notification),
            Ok(Some(phi_sdk::extension::ExtensionCommandOutcome::Completed(_))) => {}
            Ok(None) => state.notify(phi_tui::NotifyKind::Warning, format!("/{name} not handled")),
            Err(error) => state.notify(
                phi_tui::NotifyKind::Error,
                format!("/{name} failed: {error}"),
            ),
        },
    }
}

fn keybinding_event(state: &UiState, session: &crate::session::Session) -> serde_json::Value {
    let selection = state
        .picker
        .as_ref()
        .and_then(|picker| picker.selected_item());
    let exchange = state
        .picker
        .as_ref()
        .filter(|picker| matches!(picker.kind, PickerKind::Rewind))
        .and(selection)
        .and_then(|item| item.value.strip_prefix("rewind:"))
        .and_then(|entry_id| session.transcript.tool_exchange(entry_id));
    serde_json::json!({
        "selection": selection.map(|item| serde_json::json!({
            "value": item.value,
            "label": item.label,
            "description": item.description,
        })),
        "tool_exchange": exchange,
        "working_dir": session.working_dir,
    })
}

fn parse_binding(
    binding: &RegisteredKeybinding,
    diagnostics: &mut Vec<String>,
) -> Option<(InputContext, KeyInput)> {
    let Some(context) = parse_input_context(&binding.context) else {
        diagnostics.push(format!(
            "unsupported extension keybinding context: {}",
            binding.context
        ));
        return None;
    };
    let key = match binding.key.parse() {
        Ok(key) => key,
        Err(error) => {
            diagnostics.push(error);
            return None;
        }
    };
    Some((context, key))
}

fn protected_key(key: &KeyInput) -> bool {
    matches!(
        key,
        KeyInput::Enter | KeyInput::AltEnter | KeyInput::Esc | KeyInput::Ctrl('c')
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::components::input::{InputSurfaceKind, surface_context};

    fn binding(id: u64, context: &str, key: &str) -> RegisteredKeybinding {
        RegisteredKeybinding {
            id: serde_json::from_value(serde_json::json!({ "runtime": 1, "binding": id })).unwrap(),
            context: context.to_string(),
            key: key.to_string(),
        }
    }

    #[test]
    fn command_notifications_preserve_single_line_content() {
        let mut state = UiState::default();
        apply_result(
            ExtensionTaskResult::Command {
                name: "test".into(),
                result: Ok(Some(
                    phi_sdk::extension::ExtensionCommandOutcome::Completed(Some(
                        "command notification".into(),
                    )),
                )),
            },
            &mut state,
        );
        assert!(matches!(state.conversation.blocks.as_slice(), [block]
            if matches!(&block.block.body, phi_tui::BlockBody::Notify(notice)
                if notice.kind == phi_tui::NotifyKind::Info && notice.message == "command notification")));
    }

    #[test]
    fn keybinding_notifications_precede_effect_errors() {
        use phi_tui::NotifyKind;

        let mut state = UiState::default();
        apply_result(
            ExtensionTaskResult::Keybinding(Ok(
                crate::runtime::session::ExtensionKeybindingResult {
                    notifications: vec!["notification".into()],
                    effect_result: Err(anyhow::anyhow!("process denied")),
                },
            )),
            &mut state,
        );
        let notices = state
            .conversation
            .blocks
            .iter()
            .map(|block| {
                let phi_tui::BlockBody::Notify(notice) = &block.block.body else {
                    panic!("expected notification");
                };
                (notice.kind, notice.message.as_str())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            notices,
            [
                (NotifyKind::Info, "notification"),
                (
                    NotifyKind::Error,
                    "extension keybinding failed: process denied"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn hook_readiness_preserves_notices_without_replaying_lifecycle() {
        use phi_sdk::hooks::Hook;
        use phi_sdk::lifecycle::SessionEndReason;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let starts = Arc::new(AtomicUsize::new(0));
        let observed = starts.clone();
        let (root, policy, test_runtime) =
            crate::runtime::preparation::tests::session_test_runtime_with_hooks(vec![
                Hook::session_start(move |_| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    async { Ok(Vec::new()) }
                }),
            ]);
        policy.install(&test_runtime.preparation);
        let binding = {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .create_session(crate::runtime::preparation::CreateSessionRequest {
            provider: None,
            model: None,
            switch: Some("default".into()),
            persist: true,
        })
        .await
        .unwrap();
        let attachment = crate::runtime::attachment::AgentAttachment::new(
            &binding,
            test_runtime
                .sessions
                .session_access(binding.instance().id()),
            test_runtime.preparation.clone(),
        );
        let mut tasks = ExtensionTaskQueue::default();
        tasks.ready_hook_notices(attachment.clone());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), tasks.results.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, ExtensionTaskResult::HookNotices { .. }));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        test_runtime
            .sessions
            .stop(attachment.id(), SessionEndReason::Quit)
            .await
            .unwrap();
        tasks.ready_hook_notices(attachment);
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), tasks.results.recv())
            .await
            .unwrap()
            .unwrap();
        let ExtensionTaskResult::HookNotices { ref notices } = result else {
            panic!("expected hook readiness result")
        };
        assert!(matches!(
            notices.as_slice(),
            [phi_sdk::hooks::HookNotice::Warning { .. }]
        ));
        let mut state = UiState::default();
        apply_result(result, &mut state);
        let phi_tui::BlockBody::Notify(notice) =
            &state.conversation.blocks.last().unwrap().block.body
        else {
            panic!("expected readiness warning")
        };
        assert_eq!(notice.kind, phi_tui::NotifyKind::Warning);
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rewind_keybinding_captures_tool_exchange_before_history_release() {
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        session.transcript.append_assistant_message(
            None,
            None,
            vec![crate::session::ContentBlock::ToolCall {
                id: "call".into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "Bash".into(),
                args: serde_json::json!({"command": "pwd"}),
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "tool_use".into(),
        );
        session
            .transcript
            .append_tool_result("call", "canonical output", false, None);
        let entry_id = session.leaf_id.clone().unwrap();
        let expected = serde_json::to_value(session.transcript.tool_exchange(&entry_id)).unwrap();
        assert!(!expected.is_null());
        let mut state = UiState::default();
        state.open_picker(
            crate::interactive::state::MenuKind::Other,
            phi_tui::PickerState::with_kind(
                PickerKind::Rewind,
                "rewind",
                vec![phi_tui::SelectItem::new(
                    format!("rewind:{entry_id}"),
                    "tool",
                )],
            ),
        );
        let transcript = session.transcript.clone();
        crate::interactive::release_transcript_snapshot(&mut session);
        assert!(keybinding_event(&state, &session)["tool_exchange"].is_null());
        crate::interactive::materialize_transcript_snapshot(&mut session, transcript);
        let event = keybinding_event(&state, &session);
        crate::interactive::release_transcript_snapshot(&mut session);
        assert_eq!(event["tool_exchange"], expected);
        assert!(session.entries.is_empty());
    }

    #[test]
    fn compiles_picker_bindings_into_application_resolver() {
        for (kind, name) in [
            (PickerKind::Rewind, "picker.rewind"),
            (PickerKind::Login, "picker.login"),
        ] {
            let compiled = compile(&[binding(1, name, "ctrl+e")]);
            let mut state = UiState::default();
            state.runtime.set_key_bindings(compiled.resolver);
            state.open_picker(
                crate::interactive::state::MenuKind::Other,
                phi_tui::PickerState::with_kind(
                    kind,
                    "select",
                    vec![phi_tui::SelectItem::new("entry", "")],
                ),
            );
            let context = surface_context(&state, InputSurfaceKind::Picker);
            assert_eq!(context, InputContext::custom(name));
            state
                .input_surface
                .activate(&mut state.runtime, InputSurfaceKind::Picker, context);

            assert!(compiled.diagnostics.is_empty());
            assert!(matches!(
                state.runtime.dispatch_key(KeyInput::Ctrl('e')),
                phi_tui::InputOutcome::Action(UiAction::RunExtensionKeybinding(_))
            ));
        }
    }

    #[test]
    fn rejects_unknown_input_context() {
        let compiled = compile(&[binding(1, "picker.unknown", "ctrl+e")]);
        assert_eq!(
            compiled.diagnostics,
            ["unsupported extension keybinding context: picker.unknown"]
        );
    }

    #[test]
    fn rejects_native_and_protected_key_conflicts() {
        let compiled = compile(&[
            binding(1, "picker.rewind", "ctrl+o"),
            binding(2, "picker.rewind", "enter"),
        ]);

        assert_eq!(compiled.diagnostics.len(), 2);
        assert!(
            compiled
                .resolver
                .resolve(InputContext::custom("picker.rewind"), &KeyInput::Enter)
                .is_none()
        );
    }
}
