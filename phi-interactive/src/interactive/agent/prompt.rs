use crate::interactive::state::{self, UiState};
use crate::interactive::sync_transcript_metadata;
use crate::runtime::attachment::{AgentAttachment, AgentCatalog};
use crate::session::Session;
use phi_sdk::extension::memory::MemoryContextKind;
use phi_sdk::hooks::HookNotice;
use phi_sdk::transcript::ContextSource;
use phi_sdk::turn::{DirectedDelegation, SubmissionDisposition, TurnPrompt};
use phi_tui::NotifyKind;

pub(in crate::interactive) fn project_hook_notice(state: &mut UiState, notice: HookNotice) {
    let (kind, message) = hook_notice_message(notice);
    state.notify(kind, message);
}

pub(in crate::interactive) fn project_session_notices(
    state: &mut UiState,
    notices: Vec<crate::runtime::session::SessionNotices>,
    root_id: &phi_sdk::agent_instance::AgentInstanceId,
) -> bool {
    let mut changed = false;
    for notices in notices {
        let prefix = if notices.agent_id == *root_id {
            String::new()
        } else {
            format!("[agent {}] ", notices.agent_id)
        };
        for notice in notices.hook_notices {
            let (kind, message) = hook_notice_message(notice);
            state.notify(kind, format!("{prefix}{message}"));
            changed = true;
        }
        for message in notices.extension_notifications {
            state.notify(NotifyKind::Info, format!("{prefix}{message}"));
            changed = true;
        }
    }
    changed
}

fn hook_notice_message(notice: HookNotice) -> (NotifyKind, String) {
    match notice {
        HookNotice::ContextInjected {
            source,
            item_count,
            requested,
        } => {
            let message = match MemoryContextKind::from_source(&source) {
                Some(kind @ (MemoryContextKind::Static | MemoryContextKind::Graph)) => {
                    let label = match kind {
                        MemoryContextKind::Graph => "Tape",
                        _ => "Memory",
                    };
                    let prefix = if requested { "/memory-inject · " } else { "" };
                    format!("{prefix}{label} context injected: {item_count} files (message-append)")
                }
                _ => match source {
                    ContextSource::Extension(key) => format!(
                        "Context injected: {}/{} ({item_count} items)",
                        key.name(),
                        key.kind()
                    ),
                    _ => format!("Context injected: {} ({item_count} items)", source.as_str()),
                },
            };
            (NotifyKind::Info, message)
        }
        HookNotice::Warning { message } => (NotifyKind::Warning, message),
        HookNotice::Info { message } => (NotifyKind::Info, message),
        HookNotice::Cancelled { event } => (NotifyKind::Info, format!("{event} hook cancelled")),
    }
}

pub(in crate::interactive) async fn submit_prompt(
    turn_prompt: TurnPrompt,
    display_prompt: String,
    editor_prompt: String,
    state: &mut UiState,
    session: &mut Session,
    client: &AgentAttachment,
    dispatch: phi_sdk::turn::SubmissionDispatch,
    directed: Option<DirectedDelegation>,
) {
    let foreground_overridden = state.features.agent_mentions
        && directed.as_ref().is_some_and(|constraint| {
            constraint.mode == Some(phi_sdk::agent_manager::DelegationMode::Foreground)
        })
        && client.background_required().unwrap_or(false);
    let observed_workflows = if state.features.workflow_observation {
        state.workflows.observed_runs(client)
    } else {
        Default::default()
    };
    let submission = match directed {
        Some(constraint) => client.submit_directed(turn_prompt, constraint).await,
        None => client.dispatch(turn_prompt, dispatch).await,
    };
    let submission = match submission {
        Ok(submission) => submission,
        Err(error) => {
            state.notify(
                NotifyKind::Error,
                format!("failed to submit prompt: {error}"),
            );
            return;
        }
    };
    for notice in submission.notices {
        project_hook_notice(state, notice);
    }
    let (accepted_prompt, queued) = match submission.disposition {
        SubmissionDisposition::Started { prompt, .. } => (prompt, None),
        SubmissionDisposition::Queued {
            submission_id,
            dispatch,
            prompt,
        } => (prompt, Some((submission_id, dispatch))),
        SubmissionDisposition::Handled { reason } => {
            if let Some(reason) = reason {
                state.notify(NotifyKind::Info, reason);
            }
            return;
        }
    };
    if state.features.workflow_observation {
        state.workflows.dismiss_settled(client, &observed_workflows);
    }
    if foreground_overridden {
        state.notify(
            NotifyKind::Warning,
            "-f overridden: agent.background: true requires background execution",
        );
    }
    if let Ok(transcript) = client.transcript().await {
        sync_transcript_metadata(session, &transcript);
    }
    if let Some((submission_id, dispatch)) = queued {
        state
            .queued_submissions
            .push_back(state::QueuedSubmissionView {
                submission_id,
                prompt: accepted_prompt,
                display: display_prompt,
                editor_prompt,
                dispatch,
            });
        return;
    }
    state.conversation.reset_live_assistant_display();
    state.conversation.reset_live_reasoning();
}

pub(in crate::interactive) fn directed_agent_invocation(
    input: &str,
    agents: &AgentCatalog,
) -> Option<Result<DirectedDelegation, String>> {
    let rest = input.strip_prefix('@')?;
    if rest.starts_with('@') {
        return Some(Err("@@ is not supported; use @agent prompt".into()));
    }
    let Some((name, prompt)) = parse_agent_invocation_parts(rest) else {
        return Some(Err("@agent requires a name and a prompt".into()));
    };
    agents.get(name)?;
    if prompt.is_empty() || prompt.starts_with("--workspace ") {
        return Some(Err(format!(
            "@{name} requires a prompt without --workspace JSON"
        )));
    }
    let flag_prompt = rest[name.len()..].trim_start_matches(' ');
    let mode = match flag_prompt.split_once(' ') {
        Some(("-f", task)) if !task.trim().is_empty() => {
            Some(phi_sdk::agent_manager::DelegationMode::Foreground)
        }
        Some(("-b", task)) if !task.trim().is_empty() => {
            Some(phi_sdk::agent_manager::DelegationMode::Background)
        }
        _ if matches!(flag_prompt.trim(), "-f" | "-b") => {
            return Some(Err(format!("@{name} requires a task after the mode flag")));
        }
        _ => None,
    };
    Some(Ok(DirectedDelegation {
        agent: name.to_string(),
        mode,
    }))
}

fn parse_agent_invocation_parts(rest: &str) -> Option<(&str, &str)> {
    let (name, prompt) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    (!name.is_empty()).then_some((name, prompt.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_notices_preserve_kind_and_identify_children() {
        use crate::runtime::session::SessionNotices;
        use phi_sdk::agent_instance::AgentInstanceId;

        let root = AgentInstanceId::new("root");
        let mut state = UiState::default();
        assert!(project_session_notices(
            &mut state,
            vec![
                SessionNotices {
                    agent_id: root.clone(),
                    hook_notices: vec![HookNotice::Warning {
                        message: "root report".into()
                    }],
                    extension_notifications: vec!["root notification".into()],
                },
                SessionNotices {
                    agent_id: AgentInstanceId::new("child"),
                    hook_notices: vec![HookNotice::Warning {
                        message: "child report".into()
                    }],
                    extension_notifications: vec!["child notification".into()],
                },
            ],
            &root,
        ));
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
                (NotifyKind::Warning, "root report"),
                (NotifyKind::Info, "root notification"),
                (NotifyKind::Warning, "[agent child] child report"),
                (NotifyKind::Info, "[agent child] child notification"),
            ]
        );
        assert!(!project_session_notices(&mut state, Vec::new(), &root));
    }

    #[test]
    fn extension_context_notice_preserves_owner_and_key() {
        let (kind, message) = hook_notice_message(HookNotice::ContextInjected {
            source: ContextSource::Extension(phi_sdk::transcript::ContextKey::new(
                "project-index",
                "overview",
            )),
            item_count: 3,
            requested: false,
        });
        assert_eq!(kind, NotifyKind::Info);
        assert_eq!(
            message,
            "Context injected: project-index/overview (3 items)"
        );
    }

    #[test]
    fn context_only_injection_shows_info_notice_without_empty_user_block() {
        for (manual, tape_enabled, label, prefix) in [
            (false, false, "Memory", ""),
            (false, true, "Tape", ""),
            (true, false, "Memory", "/memory-inject · "),
            (true, true, "Tape", "/memory-inject · "),
        ] {
            let mut state = UiState::default();
            project_hook_notice(
                &mut state,
                HookNotice::ContextInjected {
                    source: if tape_enabled {
                        MemoryContextKind::Graph
                    } else {
                        MemoryContextKind::Static
                    }
                    .source(),
                    item_count: 2,
                    requested: manual,
                },
            );
            state::push_user_message(&mut state, "");
            assert_eq!(state.conversation.blocks.len(), 1);
            let phi_tui::BlockBody::Notify(notice) = &state.conversation.blocks[0].block.body
            else {
                panic!("injection must produce a notification");
            };
            assert_eq!(notice.kind, NotifyKind::Info);
            assert_eq!(
                notice.message,
                format!("{prefix}{label} context injected: 2 files (message-append)")
            );
        }
    }

    #[tokio::test]
    async fn foreground_override_warns_only_after_accepted_submission() {
        use crate::runtime::{AgentInstanceBuildOptions, PhiBuildOptions, RuntimeRebuildOptions};
        use clap::Parser;
        use phi_sdk::agent_manager::DelegationMode::{Background, Foreground};

        let (root, _, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        let agents_dir = root.join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: review\n---\nReview the task.",
        )
        .unwrap();
        let catalog = phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap();
        let cli = crate::cli::Cli::parse_from(["phi"]);
        for forced_background in [false, true] {
            let mut config = crate::config::Settings::default();
            config.agent.background = Some(forced_background);
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            let binding = runtime
                .sessions
                .create(
                    &runtime.preparation,
                    session.clone(),
                    RuntimeRebuildOptions {
                        phi: PhiBuildOptions {
                            cli: &cli,
                            provider: "local",
                            model: "test",
                            runtime_cfg: config.runtime_config(&catalog),
                            context: phi_sdk::context::load_with_options_for(
                                false,
                                None,
                                vec![agents_dir.clone()],
                                &root,
                            ),
                            agent_options: Default::default(),
                            hooks: Vec::new(),
                        },
                        instance: AgentInstanceBuildOptions::from_session(&session),
                    },
                    phi_sdk::lifecycle::SessionStartReason::New,
                    false,
                )
                .await
                .unwrap();
            let attachment = AgentAttachment::new(
                &binding,
                runtime.sessions.session_access(binding.instance().id()),
                runtime.preparation.clone(),
            );
            assert_eq!(attachment.background_required().unwrap(), forced_background);
            for (agent, mode) in [
                ("reviewer", Some(Foreground)),
                ("reviewer", Some(Background)),
                ("reviewer", None),
                ("missing", Some(Foreground)),
            ] {
                let mut state = UiState::default();
                state.features.agent_mentions = true;
                submit_prompt(
                    TurnPrompt {
                        prompt: "inspect".into(),
                        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                        injected_context: Vec::new(),
                        images: Vec::new(),
                        tool_policy: Default::default(),
                    },
                    "inspect".into(),
                    "inspect".into(),
                    &mut state,
                    &mut session,
                    &attachment,
                    phi_sdk::turn::SubmissionDispatch::Steer,
                    Some(DirectedDelegation {
                        agent: agent.into(),
                        mode,
                    }),
                )
                .await;
                let warning = state.conversation.blocks.iter().any(|block| {
                    matches!(&block.block.body,
                        phi_tui::BlockBody::Notify(notice) if notice.kind == NotifyKind::Warning
                            && notice.message.contains("-f overridden")
                    )
                });
                assert_eq!(
                    warning,
                    agent == "reviewer" && mode == Some(Foreground) && forced_background
                );
                binding.instance().abort().await.unwrap();
            }
        }
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directed_flags_only_parse_after_agent_name() {
        use phi_sdk::agent_manager::DelegationMode::{Background, Foreground};

        let agents = AgentCatalog::new(vec![crate::resources::AgentDefinitionView {
            name: "reviewer".into(),
            description: String::new(),
        }]);
        for (input, expected) in [
            ("@reviewer inspect", None),
            ("@reviewer -f inspect background code", Some(Foreground)),
            ("@reviewer -b inspect foreground code", Some(Background)),
            ("@reviewer inspect -f", None),
            ("@reviewer inspect -b", None),
            ("@reviewer -foo inspect", None),
            ("@reviewer -finspect", None),
            ("@reviewer\t-f inspect", None),
            ("@reviewer -f\tinspect", None),
            ("@reviewer -f inspect -b", Some(Foreground)),
        ] {
            let directed = directed_agent_invocation(input, &agents).unwrap().unwrap();
            assert_eq!(directed.mode, expected, "{input}");
        }
        for input in ["@reviewer -f", "@reviewer -b ", "@reviewer -f   "] {
            assert!(directed_agent_invocation(input, &agents).unwrap().is_err());
        }
    }

    #[test]
    fn directed_shortcut_binds_only_name_and_rejects_removed_syntax() {
        let agents = AgentCatalog::new(vec![crate::resources::AgentDefinitionView {
            name: "reviewer".into(),
            description: String::new(),
        }]);
        let directed = directed_agent_invocation("@reviewer inspect", &agents)
            .unwrap()
            .unwrap();
        assert_eq!(directed.agent, "reviewer");
        assert!(
            directed_agent_invocation("@@reviewer inspect", &agents)
                .unwrap()
                .is_err()
        );
        assert!(
            directed_agent_invocation("@reviewer", &agents)
                .unwrap()
                .is_err()
        );
        assert!(directed_agent_invocation("@", &agents).unwrap().is_err());
        assert!(
            directed_agent_invocation("@reviewer --workspace {} -- inspect", &agents)
                .unwrap()
                .is_err()
        );
        assert!(directed_agent_invocation("inspect", &agents).is_none());
        assert!(directed_agent_invocation("@unknown inspect", &agents).is_none());
        assert!(directed_agent_invocation("inspect @reviewer", &agents).is_none());
    }
}
