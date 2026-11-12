use super::*;
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::harness::tests::support::{instance_for_provider, mock_provider};
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[tokio::test]
async fn runtime_replacement_invalidates_waits_for_manager_admission() {
    use std::sync::atomic::Ordering;

    let manager = crate::harness::manager::AgentManager::new();
    let instance = manager.insert(test_client().await).unwrap();
    let transcript = instance.transcript().await.unwrap();
    let replacement = test_prepare_agent_instance_with_transcript(Some(transcript), false)
        .await
        .into_runtime_candidate();
    let admissions = manager
        .lock()
        .unwrap()
        .member(instance.id())
        .unwrap()
        .admissions
        .clone();
    admissions.fetch_add(1, Ordering::Relaxed);
    let mut wait = Box::pin(instance.wait_for_quiescence(None));
    assert!(futures::poll!(wait.as_mut()).is_pending());
    assert_eq!(
        instance.view().await.unwrap().snapshot.status,
        AgentInstanceStatus::Idle
    );
    assert!(futures::poll!(wait.as_mut()).is_pending());
    assert!(matches!(
        instance
            .replace_runtime(test_client().await.into_runtime_candidate())
            .await,
        Err(AgentInstanceError::ReplacementIdentity { .. })
    ));
    assert!(futures::poll!(wait.as_mut()).is_pending());
    instance.replace_runtime(replacement).await.unwrap();
    assert!(matches!(
        wait.await,
        Err(AgentInstanceError::RuntimeReplaced)
    ));
    let mut successor_wait = Box::pin(instance.wait_for_quiescence(None));
    assert!(futures::poll!(successor_wait.as_mut()).is_pending());
    instance.view().await.unwrap();
    assert!(futures::poll!(successor_wait.as_mut()).is_pending());
    admissions.fetch_sub(1, Ordering::Relaxed);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), successor_wait)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .status,
        AgentInstanceStatus::Idle
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_edit_is_an_idle_canonical_commit_without_runtime_replacement() {
    use crate::transcript::{ContextContent, ContextReplacement};
    let mut transcript = Transcript::new();
    transcript.append_user_message("original");
    let target = transcript.leaf_id.clone().unwrap();
    let manager = crate::harness::manager::AgentManager::new();
    let instance = manager
        .insert(test_prepare_agent_instance_with_transcript(Some(transcript), false).await)
        .unwrap();
    let before = instance.view().await.unwrap();
    let mut events = instance.subscribe();
    let edited = instance
        .edit_context(
            target.to_string(),
            Some(ContextReplacement {
                content: ContextContent::Text("replacement".into()),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        edited.transcript.entries.last().unwrap().parent_id,
        before.transcript.leaf_id
    );
    assert_eq!(edited.snapshot.id, before.snapshot.id);
    assert_eq!(edited.snapshot.working_dir, before.snapshot.working_dir);
    assert!(edited.snapshot.revision > before.snapshot.revision);
    assert!(
        std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AgentInstanceEvent::HistoryChanged))
    );
    assert_eq!(
        serde_json::to_value(&edited.transcript.entries[0]).unwrap(),
        serde_json::to_value(&before.transcript.entries[0]).unwrap()
    );
    instance.close_registration();
    assert!(matches!(
        instance.edit_context(target.to_string(), None).await,
        Err(AgentInstanceError::Stopped)
    ));
    assert_eq!(
        serde_json::to_value(instance.view().await.unwrap().transcript).unwrap(),
        serde_json::to_value(&edited.transcript).unwrap()
    );
    manager.shutdown().await.unwrap();
    let (running, _runner) = test_running_instance().await;
    let before = running.transcript().await.unwrap();
    assert!(matches!(
        running.edit_context("missing".into(), None).await,
        Err(AgentInstanceError::Busy)
    ));
    assert_eq!(
        serde_json::to_value(running.transcript().await.unwrap()).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    running.stop().await.unwrap();
}

#[tokio::test]
async fn context_reset_rejects_closed_registration_without_mutation() {
    let manager = crate::harness::manager::AgentManager::new();
    let instance = manager.insert(test_client().await).unwrap();
    let before = instance.view().await.unwrap();
    instance.close_registration();
    let result = instance.reset_context(Some("handoff".into())).await;
    let after = instance.view().await.unwrap();
    assert_eq!(
        serde_json::to_value(after.transcript).unwrap(),
        serde_json::to_value(before.transcript).unwrap()
    );
    assert_eq!(after.snapshot.revision, before.snapshot.revision);
    assert!(matches!(result, Err(AgentInstanceError::Stopped)));
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn context_reset_preserves_identity_history_accounting_and_runtime() {
    for entry_count in 0..8 {
        for handoff in [None, Some("  "), Some("handoff")] {
            let mut transcript = Transcript::new();
            for index in 0..entry_count {
                transcript.append_user_message(&format!("old {index}"));
            }
            transcript.total_tokens = 17;
            transcript.total_cost = 0.5;
            transcript.total_cache_read = 3;
            transcript.total_cache_write = 2;
            let instance = AgentInstance::spawn(
                test_prepare_agent_instance_with_transcript(Some(transcript), false).await,
            );
            let before = instance.view().await.unwrap();
            let mut events = instance.subscribe();
            let reset = instance
                .reset_context(handoff.map(str::to_owned))
                .await
                .unwrap();
            let marker = reset.transcript.entries.last().unwrap();
            assert_eq!(marker.parent_id, before.transcript.leaf_id);
            assert_eq!(marker.head.as_ref(), Some(&marker.id));
            assert_eq!(reset.transcript.leaf_id.as_ref(), Some(&marker.id));
            assert_eq!(reset.transcript.id, before.transcript.id);
            assert_eq!(reset.transcript.created_at, before.transcript.created_at);
            assert_eq!(
                reset.transcript.total_tokens,
                before.transcript.total_tokens
            );
            assert_eq!(reset.transcript.total_cost, before.transcript.total_cost);
            assert_eq!(
                reset.transcript.total_cache_read,
                before.transcript.total_cache_read
            );
            assert_eq!(
                reset.transcript.total_cache_write,
                before.transcript.total_cache_write
            );
            assert_eq!(
                serde_json::to_value(&reset.transcript.entries[..entry_count]).unwrap(),
                serde_json::to_value(&before.transcript.entries).unwrap()
            );
            let context = reset.transcript.effective_context().unwrap();
            assert!(context.entries.is_empty());
            assert_eq!(
                context.preamble_text(),
                handoff.filter(|text| !text.trim().is_empty())
            );
            assert_eq!(reset.snapshot.id, before.snapshot.id);
            assert_eq!(reset.snapshot.working_dir, before.snapshot.working_dir);
            assert_eq!(reset.snapshot.status, AgentInstanceStatus::Idle);
            assert!(reset.snapshot.revision > before.snapshot.revision);
            assert!(reset.pending_submission_ids.is_empty());
            assert!(matches!(events.try_recv().unwrap(),
                AgentInstanceEvent::ContextReset { entry_id } if entry_id == marker.id));
            assert!(matches!(
                events.try_recv().unwrap(),
                AgentInstanceEvent::HistoryChanged
            ));
            assert!(events.try_recv().is_err());
            drop(events);
            instance.reset_context(None).await.unwrap();
            let final_history = instance.stop().await.unwrap();
            assert_eq!(final_history.entries.len(), entry_count + 2);
            assert!(matches!(
                instance.reset_context(None).await,
                Err(AgentInstanceError::Stopped)
            ));
        }
    }
}

#[tokio::test]
async fn context_reset_rejects_running_and_queued_input_without_mutation() {
    let (instance, _runner) = test_running_instance().await;
    for queued in [false, true] {
        if queued {
            instance
                .dispatch(
                    TurnPrompt {
                        prompt: "accepted follow-up".into(),
                        source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                        tool_policy: Default::default(),
                        injected_context: Vec::new(),
                        images: Vec::new(),
                    },
                    SubmissionDispatch::FollowUp,
                )
                .await
                .unwrap();
        }
        let before = instance.view().await.unwrap();
        let mut events = instance.subscribe();
        assert!(matches!(
            instance.reset_context(Some("handoff".into())).await,
            Err(AgentInstanceError::Busy)
        ));
        let after = instance.view().await.unwrap();
        assert_eq!(
            serde_json::to_value(after.transcript).unwrap(),
            serde_json::to_value(before.transcript).unwrap()
        );
        assert_eq!(after.pending_submission_ids, before.pending_submission_ids);
        assert_eq!(after.snapshot.revision, before.snapshot.revision);
        assert_eq!(
            after.snapshot.active_turn_id,
            before.snapshot.active_turn_id
        );
        assert!(events.try_recv().is_err());
    }
    instance.abort().await.unwrap();
    instance.reset_context(None).await.unwrap();
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn replacement_inherits_confirmed_grants_only_at_acceptance() {
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .context(crate::context::load(true))
        .no_tools(true)
        .runtime_services(
            PhiRuntimeServices::new()
                .model_catalog(test_catalog())
                .permission_policy(
                    PermissionConfig {
                        read: Some(ToolPerm::Simple(Action::Ask)),
                        ..Default::default()
                    },
                    None,
                ),
        )
        .build()
        .unwrap();
    let original = phi.prepare_agent_instance().await.unwrap();
    let previous = original.resources();
    let instance = AgentInstance::spawn(original);
    previous.allow_session_tool("read", "confirmed.txt");
    let candidate = phi
        .agent_instance_builder()
        .transcript(instance.transcript().await.unwrap())
        .build()
        .await
        .unwrap();
    let next = candidate.resources();
    previous.allow_session_tool("read", "late.txt");
    assert!(
        next.permission()
            .authorize_tool("read", "confirmed.txt", None, None)
            .await
            .is_err()
    );
    let unrelated = phi.prepare_agent_instance().await.unwrap();
    assert!(
        instance
            .replace_runtime(unrelated.into_runtime_candidate())
            .await
            .is_err()
    );
    assert!(
        previous
            .permission()
            .authorize_tool("read", "confirmed.txt", None, None)
            .await
            .is_ok()
    );
    instance
        .replace_runtime(candidate.into_runtime_candidate())
        .await
        .unwrap();
    for pattern in ["confirmed.txt", "late.txt"] {
        assert!(
            next.permission()
                .authorize_tool("read", pattern, None, None)
                .await
                .is_ok()
        );
    }
    previous.allow_session_tool("read", "stale.txt");
    assert!(
        next.permission()
            .authorize_tool("read", "stale.txt", None, None)
            .await
            .is_err()
    );
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn replaces_an_idle_runtime_without_replacing_the_transcript() {
    let instance = test_instance().await;
    let mut replacement_transcript = instance.transcript().await.unwrap();
    replacement_transcript.append_user_message("replacement");
    let working_dir = std::env::temp_dir().canonicalize().unwrap();
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .no_tools(true)
        .build()
        .unwrap()
        .with_runtime_services(PhiRuntimeServices::new().model_catalog(test_catalog()));
    let replacement = phi
        .agent_instance_builder()
        .transcript(replacement_transcript)
        .working_dir(working_dir.clone())
        .build()
        .await
        .unwrap();
    let previous_revision = instance.snapshot().revision;
    let mut events = instance.subscribe();

    instance
        .replace_runtime(replacement.into_runtime_candidate())
        .await
        .unwrap();

    assert_eq!(instance.snapshot().working_dir, working_dir);
    assert!(!instance.transcript().await.unwrap().has_user_message());
    let view = instance.view().await.unwrap();
    assert_eq!(view.snapshot.working_dir, working_dir);
    assert!(view.snapshot.revision > previous_revision);
    assert!(matches!(
        events.recv().await.unwrap(),
        AgentInstanceEvent::RuntimeReplaced
    ));
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn runtime_replacement_validates_before_selecting_a_canonical_branch() {
    for delegated_child in [false, true] {
        let mut transcript = Transcript::new();
        transcript.append_user_message("first");
        let first = transcript.leaf_id.clone().unwrap();
        transcript.append_user_message("second");
        let instance = AgentInstance::spawn(
            test_prepare_agent_instance_with_transcript(Some(transcript), delegated_child).await,
        );
        let before = instance.view().await.unwrap();
        let mut events = instance.subscribe();
        let invalid_identity = test_client().await.into_runtime_candidate();
        let invalid_origin = test_prepare_agent_instance_with_transcript(
            Some(before.transcript.clone()),
            !delegated_child,
        )
        .await
        .into_runtime_candidate()
        .select_branch(crate::harness::instance::builder::BranchSelection {
            leaf_id: Some(first.to_string()),
        });
        let invalid_branch = test_prepare_agent_instance_with_transcript(
            Some(before.transcript.clone()),
            delegated_child,
        )
        .await
        .into_runtime_candidate()
        .select_branch(crate::harness::instance::builder::BranchSelection {
            leaf_id: Some("missing".into()),
        });
        for (candidate, boundary) in [
            (invalid_identity, "identity"),
            (invalid_origin, "origin"),
            (invalid_branch, "branch"),
        ] {
            let error = instance.replace_runtime(candidate).await.unwrap_err();
            match boundary {
                "identity" => assert!(matches!(
                    error,
                    AgentInstanceError::ReplacementIdentity { .. }
                )),
                "origin" => assert!(error.to_string().contains("delegation origin")),
                "branch" => assert!(matches!(error, AgentInstanceError::Operation { .. })),
                _ => unreachable!(),
            }
            let view = instance.view().await.unwrap();
            assert_eq!(
                serde_json::to_value(view.transcript).unwrap(),
                serde_json::to_value(&before.transcript).unwrap()
            );
            assert_eq!(view.snapshot.revision, before.snapshot.revision);
            assert!(events.try_recv().is_err());
        }
        let replacement =
            test_prepare_agent_instance_with_transcript(Some(before.transcript), delegated_child)
                .await;
        instance
            .replace_runtime(replacement.into_runtime_candidate().select_branch(
                crate::harness::instance::builder::BranchSelection {
                    leaf_id: Some(first.to_string()),
                },
            ))
            .await
            .unwrap();
        let selected = instance.transcript().await.unwrap();
        assert_eq!(selected.leaf_id.as_deref(), Some(first.as_str()));
        assert_eq!(selected.effective_context().unwrap().entries.len(), 1);
        assert_eq!(selected.entries.len(), 2);
        instance.stop().await.unwrap();
    }
}

fn workspace_completion(request: &Value) -> Value {
    let results: Vec<_> = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    let calls = [
        ("Read", json!({"path": "phi-workspace-scope.txt"})),
        (
            "Write",
            json!({"path": "phi-workspace-scope.txt", "content": "workspace-written"}),
        ),
        (
            "Edit",
            json!({"path": "phi-workspace-scope.txt", "old_text": "workspace-written", "new_text": "workspace-after"}),
        ),
        ("Grep", json!({"pattern": "workspace-after"})),
        ("Glob", json!({"pattern": "phi-workspace-scope.txt"})),
    ];
    let call = calls.get(results.len()).filter(|_| {
        !results
            .iter()
            .any(|result| result.to_string().contains("No such file or directory"))
    });
    let (delta, reason) = match call {
        Some((name, arguments)) => (
            json!({"tool_calls": [{"index": 0,
            "id": format!("workspace_call_{}", results.len()), "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]}),
            "tool_calls",
        ),
        None => (json!({"content": "workspace-result"}), "stop"),
    };
    json!({"id": "completion", "model": request["model"], "choices": [{"delta": delta, "finish_reason": reason}]})
}

#[tokio::test]
async fn filesystem_tools_use_instance_workspace_for_paths_effects_and_search_defaults() {
    let root = std::env::temp_dir().join(format!("phi-tool-workspace-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("phi-workspace-scope.txt");
    std::fs::write(&path, "workspace-before").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (requests, _received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, workspace_completion));
    instance
        .submit(TurnPrompt {
            tool_policy: Default::default(),
            prompt: "perform the workspace file operations".into(),
            source: UserPromptSubmitSource::Interactive,
            injected_context: Vec::new(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while instance.snapshot().status == crate::agent_instance::AgentInstanceStatus::Running {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let transcript = instance.stop().await.unwrap();
    server.abort();
    let final_content = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(final_content, "workspace-after");
    let results: Vec<_> = transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            crate::transcript::TranscriptEntryKind::Message {
                message:
                    crate::transcript::AgentMessage::ToolResult {
                        tool_name,
                        is_error,
                        content,
                        ..
                    },
            } => Some((
                tool_name.as_str(),
                *is_error,
                serde_json::to_string(content).unwrap(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
        ["Read", "Write", "Edit", "Grep", "Glob"]
    );
    assert!(results.iter().all(|(_, is_error, _)| !is_error));
    assert!(
        results
            .iter()
            .all(|(_, _, output)| output.contains("phi-workspace-scope.txt"))
    );
}
