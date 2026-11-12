use crate::interactive::state::{self, UiState};
use crate::interactive::{release_transcript_snapshot, sync_transcript_metadata};
use crate::runtime::attachment::{AgentAttachment, AgentEvent};
use crate::session::Session;
use phi_sdk::PhiEvent;
use phi_tui::NotifyKind;

pub(in crate::interactive) async fn drain_agent_events(
    client: &AgentAttachment,
    events: &mut tokio::sync::broadcast::Receiver<AgentEvent>,
    state: &mut UiState,
    session: &mut Session,
) -> bool {
    let mut changed = false;
    let mut sync_metadata = false;
    let mut recover_transcript = state.agent_recovery_pending;
    loop {
        match events.try_recv() {
            Ok(AgentEvent::Phi(envelope)) if is_main_event(&envelope, client.id()) => {
                if matches!(envelope.event, PhiEvent::TurnFinished { .. }) {
                    sync_metadata = true;
                }
                changed |= apply_phi_event(envelope.event, state);
            }
            Ok(AgentEvent::RuntimeReplaced | AgentEvent::PumpFailed) => {
                recover_transcript = true;
                changed = true;
            }
            Ok(AgentEvent::ObservationFailed { message }) => {
                state.notify(NotifyKind::Warning, message);
                changed = true;
            }
            Ok(AgentEvent::InteractionRequested(request)) => {
                let _ = request.request_id;
                changed = true;
            }
            Ok(AgentEvent::InteractionClosed { request_id }) => {
                let _ = request_id;
                changed = true;
            }
            Ok(AgentEvent::RunCancelled) => {
                // Abort can commit tool results without ToolCallFinished events.
                match client.transcript().await {
                    Ok(transcript) => state.conversation.reconcile_tool_results(&transcript),
                    Err(_) => recover_transcript = true,
                }
                state.project_run_cancelled();
                state.conversation.notify_operation_canceled();
                sync_metadata = true;
                changed = true;
            }
            Ok(AgentEvent::Lagged(count)) => {
                state.notify(
                    NotifyKind::Warning,
                    format!("agent event stream lagged by {count}; recovering from transcript"),
                );
                recover_transcript = true;
                changed = true;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                state.notify(
                    NotifyKind::Warning,
                    format!("agent event stream lagged by {count}; recovering from transcript"),
                );
                recover_transcript = true;
                changed = true;
            }
        }
    }
    if recover_transcript || state.agent_recovery_pending {
        state.agent_recovery_pending = true;
        match client.recover_events(events).await {
            Ok(recovery) => {
                state.agent_recovery_pending = false;
                state.reconcile_pending_submissions(&recovery.pending_submission_ids);
                *session = recovery.session;
                state.recover_history(session);
                state.recovered_declarations = Some(recovery.declarations);
                state
                    .conversation
                    .restore_live_run(&recovery.snapshot.instance);
                release_transcript_snapshot(session);
                return true;
            }
            Err(error) => {
                state.notify(
                    NotifyKind::Warning,
                    format!("failed to recover agent projection: {error}"),
                );
                return true;
            }
        }
    }
    if sync_metadata && let Ok(transcript) = client.transcript().await {
        sync_transcript_metadata(session, &transcript);
    }
    changed
}

fn is_main_event(
    envelope: &phi_sdk::PhiEventEnvelope,
    instance: &phi_sdk::agent_instance::AgentInstanceId,
) -> bool {
    envelope.session_id == instance.as_str()
        && envelope
            .agent_id
            .as_deref()
            .is_none_or(|agent| agent == instance.as_str())
}

fn project_committed_submission(
    state: &mut UiState,
    submission_id: Option<&str>,
    prompt: &str,
    source: &str,
) {
    let Some(index) = state
        .queued_submissions
        .iter()
        .position(|input| Some(input.submission_id.as_str()) == submission_id)
    else {
        if source != phi_sdk::lifecycle::UserPromptSubmitSource::Internal.as_str() {
            state::push_user_message(state, prompt);
        }
        return;
    };
    if let Some(input) = state.queued_submissions.remove(index) {
        state::push_user_message(state, &input.prompt);
    }
}

fn apply_phi_event(event: PhiEvent, state: &mut UiState) -> bool {
    match &event {
        PhiEvent::SubmissionDiscarded {
            submission_id,
            reason: phi_sdk::turn::SubmissionDiscardReason::DispatchFailed,
        } => state
            .queued_submissions
            .retain(|input| &input.submission_id != submission_id),
        PhiEvent::TurnStarted {
            submission_id,
            prompt,
            source,
        }
        | PhiEvent::UserInputInjected {
            submission_id,
            prompt,
            source,
        } => {
            project_committed_submission(state, submission_id.as_deref(), prompt, source);
        }
        PhiEvent::TurnFailed { .. } => state.agent_recovery_pending = true,
        _ => {}
    }
    state.conversation.project_event(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::agent_instance::{
        AgentInstanceId, AgentInstanceSnapshot, AgentInstanceStatus, LiveRunSnapshot,
    };
    use phi_tui::BlockRole;
    #[test]
    fn external_inputs_project_on_commit_without_a_local_queue() {
        use phi_sdk::lifecycle::UserPromptSubmitSource;

        for source in [
            UserPromptSubmitSource::Interactive,
            UserPromptSubmitSource::Extension,
            UserPromptSubmitSource::Sdk,
            UserPromptSubmitSource::Rpc,
            UserPromptSubmitSource::Print,
        ] {
            let mut state = UiState::default();
            for event in [
                PhiEvent::TurnStarted {
                    submission_id: None,
                    prompt: "same input".into(),
                    source: source.as_str().into(),
                },
                PhiEvent::UserInputInjected {
                    submission_id: Some("external-steer".into()),
                    prompt: "same input".into(),
                    source: source.as_str().into(),
                },
            ] {
                apply_phi_event(event, &mut state);
            }
            assert_eq!(state.conversation.blocks.len(), 2, "{}", source.as_str());
            for block in &state.conversation.blocks {
                assert!(matches!(
                    &block.block.body,
                    phi_tui::BlockBody::Text(text) if text == "same input"
                ));
            }
        }
        let mut state = UiState::default();
        apply_phi_event(
            PhiEvent::TurnStarted {
                submission_id: None,
                prompt: "private coordination".into(),
                source: UserPromptSubmitSource::Internal.as_str().into(),
            },
            &mut state,
        );
        assert!(state.conversation.blocks.is_empty());
    }

    #[test]
    fn parent_projection_accepts_only_its_own_reasoning_and_assistant_events() {
        let instance = AgentInstanceId::new("main");
        for (session, agent, expected) in [
            ("main", None, true),
            ("main", Some("main"), true),
            ("main", Some("child"), false),
            ("child", Some("child"), false),
        ] {
            for event in [
                PhiEvent::ReasoningDelta {
                    text: "private reasoning".into(),
                },
                PhiEvent::AssistantDelta {
                    original: "child result".into(),
                    display: "child result".into(),
                },
            ] {
                let envelope = phi_sdk::PhiEventEnvelope {
                    id: "event".into(),
                    emitted_at: chrono::Utc::now(),
                    session_id: session.into(),
                    turn_id: Some("turn".into()),
                    message_id: None,
                    agent_id: agent.map(Into::into),
                    parent_agent_id: None,
                    parent_tool_call_id: None,
                    event,
                };
                assert_eq!(is_main_event(&envelope, &instance), expected);
            }
        }
    }

    #[tokio::test]
    async fn accepted_runtime_event_requests_capability_refresh_after_coherent_recovery() {
        let (root, policy, test_runtime) =
            crate::runtime::preparation::tests::session_test_runtime();
        policy.install(&test_runtime.preparation);
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().as_ref().into();
        let agent = Box::pin(
            {
                policy.install(&test_runtime.preparation);
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
            }
            .start_session(session.clone(), false),
        )
        .await
        .unwrap();
        let attachment = AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        );
        let mut events = attachment.subscribe();
        attachment
            .configure_runtime(crate::runtime::preparation::RuntimeSelection::Model {
                selection: "model:local:test".into(),
            })
            .await
            .unwrap();
        let mut state = UiState::default();
        assert!(drain_agent_events(&attachment, &mut events, &mut state, &mut session,).await);
        assert!(!state.agent_recovery_pending);
        state.editor.set_text("retained draft");
        let declarations = state.recovered_declarations.take().unwrap();
        assert_eq!(
            declarations.generation_id,
            agent.snapshot().await.runtime_generation_id
        );
        let mut capabilities = crate::runtime::attachment::AgentCapabilities::default();
        crate::interactive::controller::install_runtime_declarations(
            &mut capabilities,
            &mut state,
            declarations,
        );
        assert!(!state.slash_commands.is_empty());
        assert_eq!(state.editor.text(), "retained draft");
        test_runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn failed_turns_recover_and_reopen_as_history_without_local_notices() {
        use clap::Parser;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const AUTHENTICATION_FAILURE_STATUS: &str = "HTTP/1.1 401 Unauthorized";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut start = [0; 1];
                    socket.read_exact(&mut start).await.unwrap();
                    let body = r#"{"error":{"message":"credentials rejected","type":"authentication_error"}}"#;
                    let response = format!(
                        "{AUTHENTICATION_FAILURE_STATUS}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                    let _ = tokio::io::copy(&mut socket, &mut tokio::io::sink()).await;
                });
            }
        });
        let (root, _, runtime) = crate::runtime::preparation::tests::session_test_runtime();
        let models = root.join("models.json");
        std::fs::write(
            &models,
            serde_json::json!({"providers":{"local":{
                "api":"openai-completions", "baseUrl":format!("http://{address}/v1"),
                "models":{"test":{"contextWindow":128000}}
            }}})
            .to_string(),
        )
        .unwrap();
        let catalog = std::sync::Arc::new(
            phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("failure-model-store.json"),
                user_paths: vec![models],
                offline: true,
            })
            .unwrap(),
        );
        let policy = crate::runtime::preparation::tests::TestConfiguration::new(
            std::sync::Arc::new(crate::cli::Cli::parse_from(["phi"])),
            crate::config::Settings {
                default_provider: Some("local".into()),
                default_model: Some("test".into()),
                ..Default::default()
            },
            crate::AgentBuildOptions {
                no_tools: true,
                ..Default::default()
            },
            Vec::new(),
            catalog,
        );
        policy.install(&runtime.preparation);
        let workflows = crate::runtime::preparation::SessionWorkflows::new(
            runtime.sessions.clone(),
            runtime.preparation.clone(),
        )
        .unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().as_ref().into();
        let agent = workflows
            .start_session(session.clone(), true)
            .await
            .unwrap();
        let attachment = AgentAttachment::new(
            &agent,
            runtime.sessions.session_access(agent.instance().id()),
            runtime.preparation.clone(),
        );
        let mut events = attachment.subscribe();
        let mut state = UiState::default();
        for expected_failures in 1..=2 {
            attachment
                .submit(phi_sdk::turn::TurnPrompt {
                    prompt: "inspect".into(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: Default::default(),
                })
                .await
                .unwrap();
            let view = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                agent.instance().wait_for_quiescence(None),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(view.snapshot.status, AgentInstanceStatus::Failed);
            assert_eq!(
                view.snapshot.latest_error.as_ref().unwrap().kind,
                phi_sdk::PhiFailureKind::Authentication
            );
            assert_eq!(
                view.transcript
                    .entries
                    .iter()
                    .filter(|entry| matches!(
                        entry.kind,
                        phi_sdk::transcript::TranscriptEntryKind::Failure { .. }
                    ))
                    .count(),
                expected_failures
            );
            assert!(drain_agent_events(&attachment, &mut events, &mut state, &mut session).await);
            assert_eq!(
                state
                    .conversation
                    .blocks
                    .iter()
                    .filter(|block| block.role == BlockRole::Error)
                    .count(),
                expected_failures
            );
            assert!(
                state
                    .conversation
                    .blocks
                    .iter()
                    .all(|block| !block.local_notification)
            );
        }
        runtime
            .sessions
            .session_access(agent.instance().id())
            .checkpoint()
            .await
            .unwrap();
        let saved = crate::session::storage::load_session_id(&session.id).unwrap();
        let saved_entries = serde_json::to_value(&saved.transcript.entries).unwrap();
        runtime
            .sessions
            .stop(
                agent.instance().id(),
                phi_sdk::lifecycle::SessionEndReason::Quit,
            )
            .await
            .unwrap();
        let reopened = workflows
            .start_saved_session(&session.id, true)
            .await
            .unwrap();
        let recovery = reopened.recovery().await.unwrap();
        assert_eq!(recovery.snapshot.instance.status, AgentInstanceStatus::Idle);
        assert!(recovery.snapshot.instance.latest_error.is_none());
        assert_eq!(
            serde_json::to_value(&recovery.session.transcript.entries).unwrap(),
            saved_entries
        );
        let mut fresh = UiState::default();
        fresh.rebuild_history(&recovery.session);
        fresh
            .conversation
            .restore_live_run(&recovery.snapshot.instance);
        assert_eq!(
            fresh
                .conversation
                .blocks
                .iter()
                .filter(|block| block.role == BlockRole::Error)
                .count(),
            2
        );
        assert!(
            fresh
                .conversation
                .blocks
                .iter()
                .all(|block| !block.local_notification)
        );
        server.abort();
        runtime
            .sessions
            .shutdown(phi_sdk::lifecycle::SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_replaces_stale_history_and_live_content_without_resetting_input() {
        let mut session =
            crate::session::new("test", "model", 1_000, &std::env::current_dir().unwrap());
        session.transcript.append_user_message("committed prompt");
        session.transcript.append_assistant_message(
            None,
            None,
            vec![crate::session::ContentBlock::ToolCall {
                id: "tool-1".into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "read".into(),
                args: serde_json::json!({}),
            }],
            "test".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "model".into(),
            Default::default(),
            "tool_use".into(),
        );
        let mut state = UiState::default();
        state.editor.set_text("unfinished draft".to_string());
        state.active_menu = Some(crate::interactive::state::MenuKind::Other);
        state.conversation.blocks_expanded = Some(true);
        state.conversation.project_main_run_started();
        state.conversation.append_live_assistant("stale partial");
        let stale_id = state.conversation.blocks[0].id;
        let snapshot = AgentInstanceSnapshot {
            id: AgentInstanceId::new(session.id.as_str()),
            definition: None,
            working_dir: Default::default(),
            status: AgentInstanceStatus::Running,
            active_turn_id: Some("turn".into()),
            live_run: Some(LiveRunSnapshot {
                assistant_text: "canonical partial".into(),
                tool: Some(phi_sdk::agent_instance::LiveToolSnapshot {
                    id: "tool-1".into(),
                    name: "read".into(),
                    args: serde_json::json!({}),
                    display: None,
                }),
                ..LiveRunSnapshot::default()
            }),
            progress: None,
            bash_executions: Vec::new(),
            pending_interactions: Vec::new(),
            latest_error: None,
            revision: 2,
        };
        state.recover_history(&session);
        let tool_block = state.conversation.blocks[1].id;
        state.conversation.restore_live_run(&snapshot);
        assert_eq!(state.conversation.blocks.len(), 3);
        assert_eq!(state.conversation.blocks[1].id, tool_block);
        assert!(
            state
                .conversation
                .project_tool_call_finished("tool-1", "done", false, None)
        );
        assert!(matches!(
            &state.conversation.blocks[1].block.body,
            phi_tui::BlockBody::Output(text) if text == "done"
        ));
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .all(|block| block.id > stale_id)
        );
        assert_eq!(state.editor.text(), "unfinished draft");
        assert_eq!(
            state.active_menu,
            Some(crate::interactive::state::MenuKind::Other)
        );
        assert_eq!(state.conversation.blocks_expanded, Some(true));
        assert!(state.conversation.run_active_projection());
        assert!(state.conversation.live_assistant_block().is_some());
        assert!(matches!(
            &state.conversation.blocks[2].block.body,
            phi_tui::BlockBody::Text(text) if text == "canonical partial"
        ));

        state.recover_history(&session);
        let stopped = AgentInstanceSnapshot {
            status: AgentInstanceStatus::Stopped,
            active_turn_id: None,
            live_run: None,
            ..snapshot
        };
        state.conversation.restore_live_run(&stopped);
        assert_eq!(state.conversation.blocks.len(), 2);
        assert!(!state.conversation.run_active_projection());
        assert!(state.conversation.live_assistant_block().is_none());
        assert!(state.conversation.live_run.is_none());
    }

    #[test]
    fn structured_message_completion_preserves_streamed_text_and_image_order() {
        let mut state = UiState::default();
        let source = phi_sdk::transcript::ImageSource::Url {
            url: "https://example.com/generated.png".into(),
            mime_type: None,
        };
        state.conversation.project_main_run_started();
        for event in [
            PhiEvent::AssistantDelta {
                original: "before".into(),
                display: "before".into(),
            },
            PhiEvent::AssistantImage {
                source: source.clone(),
            },
            PhiEvent::AssistantDelta {
                original: "after".into(),
                display: "after".into(),
            },
        ] {
            apply_phi_event(event, &mut state);
        }
        let ids = state
            .conversation
            .blocks
            .iter()
            .map(|block| block.id)
            .collect::<Vec<_>>();
        apply_phi_event(
            PhiEvent::AssistantFinished {
                id: "partial".into(),
                content: vec![
                    crate::session::ContentBlock::Text {
                        text: "before".into(),
                    },
                    crate::session::ContentBlock::Image { source },
                    crate::session::ContentBlock::Text {
                        text: "after".into(),
                    },
                ],
                response: "beforeafter".into(),
                display: "beforeafter".into(),
                usage: Default::default(),
                has_tool_calls: false,
            },
            &mut state,
        );
        state.project_run_cancelled();
        assert!(
            matches!(state.conversation.blocks.as_slice(), [before, image, after]
                if matches!(&before.block.body, phi_tui::BlockBody::Text(text) if text == "before")
                && matches!(&image.block.body, phi_tui::BlockBody::Image(_))
                && matches!(&after.block.body, phi_tui::BlockBody::Text(text) if text == "after")
            )
        );
        assert_eq!(
            state
                .conversation
                .blocks
                .iter()
                .map(|block| block.id)
                .collect::<Vec<_>>(),
            ids
        );
        assert!(state.conversation.live_assistant_block().is_none());
    }

    #[test]
    fn cancellation_finishes_live_reasoning_and_only_unfinished_tools() {
        let mut state = UiState::default();
        state.conversation.project_main_run_started();
        let finished =
            state
                .conversation
                .project_tool_call_started("done", "read", "{}", Default::default());
        state
            .conversation
            .project_tool_call_finished("done", "result", false, None);
        let bash =
            state
                .conversation
                .project_tool_call_started("bash", "Bash", "{}", Default::default());
        let read =
            state
                .conversation
                .project_tool_call_started("read", "read", "{}", Default::default());
        state.conversation.set_live_reasoning("partial reasoning");

        state.project_run_cancelled();

        assert!(state.conversation.live_run.is_none());
        assert!(!state.conversation.run_active_projection());
        assert!(
            state.conversation.tool_block_meta[&bash]
                .tool_timer
                .is_some_and(|timer| timer.finished_at.is_some())
        );
        assert!(
            !state
                .conversation
                .advance_clocks(std::time::Instant::now() + std::time::Duration::from_secs(60))
        );
        for (id, status) in [
            (finished, phi_tui::BlockStatus::Success),
            (bash, phi_tui::BlockStatus::Error),
            (read, phi_tui::BlockStatus::Error),
        ] {
            assert_eq!(
                state.conversation.blocks[state.conversation.block_positions[&id]]
                    .block
                    .status,
                status
            );
        }
        assert_eq!(
            state.conversation.blocks.last().unwrap().block.status,
            phi_tui::BlockStatus::Success
        );
    }

    #[test]
    fn event_recovery_preserves_local_notifications_without_duplicates() {
        let mut session =
            crate::session::new("test", "model", 1_000, &std::env::current_dir().unwrap());
        session.transcript.append_user_message("committed prompt");
        let mut state = UiState::default();
        state.notify(NotifyKind::Info, "previous notice");
        state
            .conversation
            .push_text_block(BlockRole::User, "committed prompt");
        state.conversation.project_main_run_started();
        state.conversation.append_live_assistant("stale partial");
        state.conversation.reset_live_assistant();
        state.project_run_cancelled();
        state.conversation.notify_operation_canceled();
        let notice_ids = [
            state.conversation.blocks[0].id,
            state.conversation.blocks.last().unwrap().id,
        ];

        for _ in 0..2 {
            state.recover_history(&session);
            assert_eq!(state.conversation.blocks.len(), 3);
            assert_eq!(state.conversation.blocks[0].id, notice_ids[0]);
            assert_eq!(state.conversation.blocks[2].id, notice_ids[1]);
            assert!(matches!(
                &state.conversation.blocks[0].block.body,
                phi_tui::BlockBody::Notify(notice) if notice.message == "previous notice"
            ));
            assert!(matches!(
                &state.conversation.blocks[2].block.body,
                phi_tui::BlockBody::Styled(lines) if lines[0].text == "Operation canceled"
            ));
            for (position, block) in state.conversation.blocks.iter().enumerate() {
                assert_eq!(
                    state.conversation.block_positions.get(&block.id),
                    Some(&position)
                );
            }
            assert!(!state.conversation.run_active_projection());
        }

        let empty_session =
            crate::session::new("test", "model", 1_000, &std::env::current_dir().unwrap());
        state.recover_history(&empty_session);
        assert_eq!(state.conversation.blocks.len(), 2);
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .all(|block| block.local_notification)
        );
        assert_eq!(state.conversation.blocks[0].id, notice_ids[0]);
        assert_eq!(state.conversation.blocks[1].id, notice_ids[1]);

        state.rebuild_history(&session);
        assert_eq!(state.conversation.blocks.len(), 1);
        assert!(
            state
                .conversation
                .blocks
                .iter()
                .all(|block| !notice_ids.contains(&block.id))
        );
    }

    #[test]
    fn queue_recovery_uses_identity_and_preserves_pending_editor_payloads() {
        let mut state = UiState::default();
        for id in ["consumed", "pending", "discarded"] {
            state
                .queued_submissions
                .push_back(state::QueuedSubmissionView {
                    submission_id: id.into(),
                    prompt: "identical accepted text".into(),
                    display: "paste placeholder".into(),
                    editor_prompt: "original expanded submission".into(),
                    dispatch: phi_sdk::turn::SubmissionDispatch::Steer,
                });
        }
        state.reconcile_pending_submissions(&["pending".into()]);
        let pending = &state.queued_submissions[0];
        assert_eq!(state.queued_submissions.len(), 1);
        assert_eq!(pending.submission_id, "pending");
        assert_eq!(pending.display, "paste placeholder");
        assert_eq!(pending.editor_prompt, "original expanded submission");
        project_committed_submission(
            &mut state,
            Some("external"),
            "external input",
            "interactive",
        );
        assert_eq!(state.queued_submissions.len(), 1);
        project_committed_submission(&mut state, Some("pending"), "ignored", "interactive");
        assert!(state.queued_submissions.is_empty());
        assert_eq!(state.conversation.blocks.len(), 2);
        for (block, expected) in state
            .conversation
            .blocks
            .iter()
            .zip(["external input", "identical accepted text"])
        {
            assert!(
                matches!(&block.block.body, phi_tui::BlockBody::Text(text) if text == expected)
            );
        }
    }
}
