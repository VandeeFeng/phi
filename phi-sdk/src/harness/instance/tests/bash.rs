use super::*;
use crate::agent_instance::{BashMode, BashStatus};
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::harness::tests::support::{instance_for_provider, mock_provider};
use serde_json::json;
use tokio::net::TcpListener;

fn bash_lifecycle(transcript: &Transcript, id: &str) -> Vec<BashStatus> {
    transcript
        .current_branch()
        .iter()
        .filter_map(|entry| {
            let crate::transcript::TranscriptEntryKind::Event { name, data } = &entry.kind else {
                return None;
            };
            if name != "phi_sdk.bash_execution" || data["id"] != id {
                return None;
            }
            Some(serde_json::from_value(data["status"].clone()).unwrap())
        })
        .collect()
}

#[tokio::test]
async fn restored_bash_history_is_passive_and_idempotent() {
    use crate::harness::instance::bash::BashExecution;
    use crate::transcript::{AgentMessage, TranscriptEntryKind};

    for (status, lifecycle_recorded) in [
        (BashStatus::Running, false),
        (BashStatus::Running, true),
        (BashStatus::Completed, true),
        (BashStatus::Failed, true),
        (BashStatus::Stopped, true),
    ] {
        for delivered in [false, true] {
            let mut transcript = Transcript::new();
            transcript.append_user_message("run background work");
            let execution = BashExecution {
                id: "old-execution".into(),
                turn_id: "old-turn".into(),
                tool_call_id: "old-call".into(),
                command: "printf result".into(),
                mode: BashMode::Background,
                status,
                output_path: "/tmp/phi-recovery-output.log".into(),
                exit_code: (status == BashStatus::Completed).then_some(0),
                error: None,
            };
            transcript.append_entry(TranscriptEntryKind::Message {
                message: AgentMessage::ToolResult {
                    tool_call_id: execution.tool_call_id.clone().into(),
                    provider_call_id: None,
                    provider_item_id: None,
                    tool_name: "Bash".into(),
                    content: vec![crate::transcript::ContentBlock::Text {
                        text: json!({ "status": "background_started", "execution_id": execution.id,
                            "output_path": execution.output_path })
                        .to_string()
                        .into(),
                    }],
                    is_error: false,
                    details: None,
                },
            });
            if lifecycle_recorded {
                transcript.append_entry(TranscriptEntryKind::Event {
                    name: "phi_sdk.bash_execution".into(),
                    data: serde_json::to_value(&execution).unwrap(),
                });
            }
            if delivered {
                transcript.append_entry(TranscriptEntryKind::Event {
                    name: "phi_sdk.bash_feedback".into(),
                    data: json!({"execution_id": execution.id}),
                });
            }
            let original = serde_json::to_value(&transcript.entries).unwrap();
            let original_count = transcript.entries.len();
            let instance = AgentInstance::spawn(
                test_prepare_agent_instance_with_transcript(Some(transcript), false).await,
            );
            let view = instance.wait_for_quiescence(None).await.unwrap();
            assert_eq!(view.snapshot.status, AgentInstanceStatus::Idle);
            assert!(view.snapshot.bash_executions.is_empty());
            assert!(view.pending_submission_ids.is_empty());
            assert_eq!(
                serde_json::to_value(&view.transcript.entries[..original_count]).unwrap(),
                original
            );
            let feedback = view.transcript.entries[original_count..]
                .iter()
                .filter_map(|entry| match &entry.kind {
                    TranscriptEntryKind::Message {
                        message: AgentMessage::Agent { content },
                    } => Some(serde_json::to_string(content).unwrap()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(feedback.len(), usize::from(!delivered));
            if !delivered {
                assert!(feedback[0].contains("not human input or approval"));
                assert!(feedback[0].contains("old-execution"));
                assert!(feedback[0].contains("Check the output and side effects"));
                assert_eq!(
                    feedback[0].contains("terminal outcome is unknown"),
                    status == BashStatus::Running
                );
            }
            instance.stop().await.unwrap();
            let restored = AgentInstance::spawn(
                test_prepare_agent_instance_with_transcript(
                    Some(
                        serde_json::from_value(serde_json::to_value(&view.transcript).unwrap())
                            .unwrap(),
                    ),
                    false,
                )
                .await,
            );
            assert_eq!(
                serde_json::to_value(restored.transcript().await.unwrap()).unwrap(),
                serde_json::to_value(view.transcript).unwrap()
            );
            restored.stop().await.unwrap();
        }
    }
}

#[tokio::test]
async fn restored_bash_history_respects_selected_branch_and_reset() {
    use crate::transcript::TranscriptEntryKind;
    for boundary in ["unselected", "sibling_feedback", "compaction", "reset"] {
        let mut transcript = Transcript::new();
        transcript.append_user_message("original work");
        let root = transcript.leaf_id.clone().unwrap();
        let execution = transcript.append_entry(TranscriptEntryKind::Event {
            name: "phi_sdk.bash_execution".into(),
            data: json!({ "id": "old-execution", "turn_id": "old-turn", "tool_call_id": "old-call",
                "command": "printf result", "mode": "background", "status": "running",
                "output_path": "/tmp/phi-recovery-output.log", "exit_code": null, "error": null }),
        });
        match boundary {
            "unselected" => {
                transcript.select_branch(Some(&root)).unwrap();
                transcript.append_user_message("different work");
            }
            "sibling_feedback" => {
                transcript.append_entry(TranscriptEntryKind::Event {
                    name: "phi_sdk.bash_feedback".into(),
                    data: json!({ "execution_id": "old-execution" }),
                });
                transcript.select_branch(Some(&execution)).unwrap();
                transcript.append_user_message("different branch");
            }
            "compaction" => {
                transcript.append_user_message("later work");
                let head = transcript.leaf_id.clone().unwrap();
                transcript.compress("previous work".into(), &head).unwrap();
            }
            "reset" => {
                transcript.reset_context(None).unwrap();
            }
            _ => unreachable!(),
        }
        let original = transcript.entries.len();
        let instance = AgentInstance::spawn(
            test_prepare_agent_instance_with_transcript(Some(transcript), false).await,
        );
        let transcript = instance.transcript().await.unwrap();
        assert_eq!(
            transcript.entries.len() > original,
            matches!(boundary, "sibling_feedback" | "compaction"),
            "{boundary}"
        );
        instance.stop().await.unwrap();
    }
}

#[tokio::test]
async fn foreground_bash_json_output_is_not_a_launch_receipt() {
    use crate::core::event::{ToolOutputDetails, ToolResultDetails};
    use crate::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind};
    let mut transcript = Transcript::new();
    transcript.append_entry(TranscriptEntryKind::Message {
        message: AgentMessage::ToolResult {
            tool_call_id: "foreground-call".into(), provider_call_id: None, provider_item_id: None,
            tool_name: "Bash".into(), is_error: false,
            content: vec![ContentBlock::Text { text: json!({ "status": "background_started",
                "execution_id": "printed-not-launched", "output_path": "/tmp/not-a-real-execution" }).to_string().into() }],
            details: Some(ToolResultDetails { diff: None, output: Some(ToolOutputDetails {
                total_bytes: 100, total_lines: 1, truncated: false, full_output_path: None, exit_code: 0,
            }), ..Default::default() }),
        },
    });
    let original = serde_json::to_value(&transcript).unwrap();
    let instance = AgentInstance::spawn(
        test_prepare_agent_instance_with_transcript(Some(transcript), false).await,
    );
    assert_eq!(
        serde_json::to_value(instance.transcript().await.unwrap()).unwrap(),
        original
    );
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn bash_lifetime_follows_resolved_subagent_mode() {
    use crate::agent::custom::{AgentCatalog, AgentDefinition};
    use crate::agent_manager::{AgentSpawnRequest, DelegationMode};
    for mode in [DelegationMode::Foreground, DelegationMode::Background] {
        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-child-bash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let mut phi = fixture.phi.clone();
        phi.context.agent_catalog = AgentCatalog::new(vec![
            AgentDefinition::new("shell", "run shell", "Run a command.")
                .model("child")
                .tools(["Bash"]),
        ]);
        let parent = fixture
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let (requests, _received) = mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, requests, |request| {
            let tool_called = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            let (delta, reason) = if request["model"] == "child" && !tool_called {
                (
                    json!({"tool_calls": [{"index": 0, "id": "background-bash", "type": "function", "function": {
                        "name": "Bash", "arguments": json!({"command": "printf ready; deadline=$((SECONDS + 5)); while ! test -f release; do (( SECONDS < deadline )) || exit 124; sleep 0.05; done", "run_in_background": true}).to_string()
                    }}]}),
                    "tool_calls",
                )
            } else {
                (json!({"content": "finished response"}), "stop")
            };
            json!({"id": "child-bash", "model": "test", "choices": [{"delta": delta, "finish_reason": reason}]})
        }));
        let creation = parent
            .spawn_subagent(AgentSpawnRequest::new("shell", "start shell").mode(mode))
            .await
            .unwrap();
        let child = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(child) = fixture.manager.get(creation.instance_id()) {
                    break child;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        let (_cancellation, cancelled) = tokio::sync::watch::channel(false);
        let completion = tokio::time::timeout(
            Duration::from_secs(5),
            child.complete_delegated_agent(cancelled),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(completion.outcome.is_ok(), "{:?}", completion.outcome);
        let execution = child.snapshot().bash_executions.first().unwrap().clone();
        assert_eq!(
            execution.status,
            if mode == DelegationMode::Foreground {
                BashStatus::Stopped
            } else {
                BashStatus::Running
            }
        );
        tokio::time::timeout(Duration::from_secs(5), child.stop_bash(&execution.id))
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), child.stop_bash(&execution.id))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            child.snapshot().bash_executions[0].status,
            BashStatus::Stopped
        );
        tokio::time::timeout(Duration::from_secs(5), fixture.manager.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            child.snapshot().bash_executions[0].status,
            BashStatus::Stopped
        );
        server.abort();
        std::fs::remove_file(execution.output_path).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn bash_handoff_commits_receipt_then_internal_completion() {
    let root =
        std::path::PathBuf::from("/tmp").join(format!("phi-managed-bash-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (requests, mut received) = mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, |request| {
        let tool_called = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool");
        let feedback = request.to_string().contains("Bash execution outcome.");
        let (delta, reason) = if !tool_called {
            (
                json!({"tool_calls": [{"index": 0, "id": "slow-bash", "type": "function", "function": {
                    "name": "Bash", "arguments": json!({"command": "printf ready; deadline=$((SECONDS + 5)); while ! test -f release; do (( SECONDS < deadline )) || exit 124; sleep 0.05; done; printf done"}).to_string()
                }}]}),
                "tool_calls",
            )
        } else {
            (
                json!({"content": if feedback { "handled completion" } else { "continuing independently" }}),
                "stop",
            )
        };
        json!({"id": "bash-test", "model": "test", "choices": [{"delta": delta, "finish_reason": reason}]})
    }));
    instance
        .submit(TurnPrompt {
            tool_policy: Default::default(),
            prompt: "run the command".into(),
            source: UserPromptSubmitSource::Sdk,
            injected_context: Vec::new(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    let execution = tokio::time::timeout(Duration::from_secs(5), async {
        let mut snapshots = instance.subscribe_snapshots();
        loop {
            let snapshot = snapshots.snapshot();
            if let Some(execution) = snapshot.bash_executions.first() {
                break execution.clone();
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(execution.mode, BashMode::Foreground);
    assert_eq!(execution.status, BashStatus::Running);
    let background = instance.background_bash(&execution.id).await.unwrap();
    assert_eq!(background.id, execution.id);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !received
            .recv()
            .await
            .unwrap()
            .to_string()
            .contains("background_started")
        {}
    })
    .await
    .unwrap();
    let view = instance.view().await.unwrap();
    let receipt_count = view.transcript.entries.iter().filter(|entry| {
        matches!(&entry.kind, crate::transcript::TranscriptEntryKind::Message { message: crate::transcript::AgentMessage::ToolResult { content, .. } }
            if serde_json::to_string(content).unwrap().contains("background_started"))
    }).count();
    assert_eq!(receipt_count, 1);
    assert_eq!(
        bash_lifecycle(&view.transcript, &execution.id),
        vec![BashStatus::Running]
    );
    let interrupted_history = view.transcript.clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        while instance.read_bash_output(&execution.id).await.unwrap() != "ready" {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            instance.wait_for_quiescence(None)
        )
        .await
        .is_err()
    );
    std::fs::write(root.join("release"), "").unwrap();
    let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        view.snapshot.bash_executions[0].status,
        BashStatus::Completed
    );
    assert_eq!(view.snapshot.bash_executions[0].exit_code, Some(0));
    assert_eq!(
        instance.read_bash_output(&execution.id).await.unwrap(),
        "readydone"
    );
    let history = serde_json::to_string(&view.transcript).unwrap();
    assert!(history.contains("Automated harness notification, not human input or approval"));
    assert!(history.contains("handled completion"));
    assert_eq!(
        bash_lifecycle(&view.transcript, &execution.id),
        vec![BashStatus::Running, BashStatus::Completed]
    );
    assert_eq!(history.matches("phi_sdk.bash_feedback").count(), 1);
    while received.try_recv().is_ok() {}
    let restored = AgentInstance::spawn(
        instance
            .phi
            .agent_instance_builder()
            .working_dir(root.clone())
            .transcript(interrupted_history)
            .build()
            .await
            .unwrap(),
    );
    assert_eq!(restored.snapshot().status, AgentInstanceStatus::Idle);
    assert!(restored.snapshot().bash_executions.is_empty());
    assert!(received.try_recv().is_err());
    restored
        .submit(TurnPrompt {
            prompt: "continue after resume".into(),
            source: UserPromptSubmitSource::Sdk,
            injected_context: Vec::new(),
            images: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), restored.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    let request = received.try_recv().unwrap().to_string();
    assert!(request.contains("terminal outcome is unknown"));
    assert!(request.contains("not human input or approval"));
    assert!(received.try_recv().is_err());
    assert!(restored.snapshot().bash_executions.is_empty());
    restored.stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), instance.manager.shutdown())
        .await
        .unwrap()
        .unwrap();
    server.abort();
    std::fs::remove_file(execution.output_path).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn bash_cancellation_is_passive() {
    for termination in ["user_cancelled", "unexpected_exit", "agent_stopped"] {
        let root = std::path::PathBuf::from("/tmp")
            .join(format!("phi-bash-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (requests, mut received) = mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, requests, |request| {
            let tool_called = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            let (delta, reason) = if !tool_called {
                (
                    json!({"tool_calls": [{"index": 0, "id": "recoverable-bash", "type": "function", "function": {
                        "name": "Bash", "arguments": json!({"command": "printf ready; deadline=$((SECONDS + 5)); while ! test -f release; do (( SECONDS < deadline )) || exit 124; sleep 0.05; done; kill -TERM $$", "run_in_background": true}).to_string()
                    }}]}),
                    "tool_calls",
                )
            } else {
                (json!({"content": "notification handled"}), "stop")
            };
            json!({"id": "bash-recovery", "model": "test", "choices": [{"delta": delta, "finish_reason": reason}]})
        }));
        instance
            .submit(TurnPrompt {
                tool_policy: Default::default(),
                prompt: "run the command".into(),
                source: UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let execution = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(execution) = instance.snapshot().bash_executions.first().cloned()
                    && instance.read_bash_output(&execution.id).await.unwrap() == "ready"
                {
                    break execution;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        if termination == "user_cancelled" {
            tokio::time::timeout(Duration::from_secs(5), instance.stop_bash(&execution.id))
                .await
                .unwrap()
                .unwrap();
        } else if termination == "unexpected_exit" {
            std::fs::write(root.join("release"), "").unwrap();
        }
        let view = if termination == "agent_stopped" {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !serde_json::to_string(&instance.transcript().await.unwrap())
                    .unwrap()
                    .contains("notification handled")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            instance.stop().await.unwrap();
            instance.view().await.unwrap()
        } else {
            tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
                .await
                .unwrap()
                .unwrap()
        };
        let history = serde_json::to_string(&view.transcript).unwrap();
        let mut request_count = 0;
        while received.try_recv().is_ok() {
            request_count += 1;
        }
        if termination == "agent_stopped" {
            assert_eq!(request_count, 2);
            assert_eq!(
                bash_lifecycle(&view.transcript, &execution.id),
                vec![BashStatus::Running, BashStatus::Stopped]
            );
            let restored = AgentInstance::spawn(
                test_prepare_agent_instance_with_transcript(Some(view.transcript.clone()), false)
                    .await,
            );
            let restored_view = restored.wait_for_quiescence(None).await.unwrap();
            assert_eq!(restored_view.snapshot.status, AgentInstanceStatus::Idle);
            assert!(restored_view.snapshot.bash_executions.is_empty());
            let recovered = serde_json::to_string(&restored_view.transcript).unwrap();
            assert!(recovered.contains("previous Agent runtime"));
            assert!(recovered.contains("Its terminal state was recorded"));
            assert!(!recovered.contains("terminal outcome is unknown"));
            restored.stop().await.unwrap();
        } else if termination == "user_cancelled" {
            assert_eq!(view.snapshot.bash_executions[0].status, BashStatus::Stopped);
            assert!(history.contains("user_cancelled"));
            assert!(history.contains("Do not automatically retry or recreate"));
            assert!(history.contains("does not by itself withdraw the overall user goal"));
            assert!(history.contains("not human input or approval"));
            assert!(!history.contains("Recover the interrupted work"));
            assert_eq!(
                request_count, 2,
                "user cancellation must not wake the model"
            );
            instance
                .submit(TurnPrompt {
                    prompt: "continue with other work".into(),
                    source: UserPromptSubmitSource::Sdk,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: Default::default(),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
                .await
                .unwrap()
                .unwrap();
            let request = received.try_recv().unwrap().to_string();
            assert!(request.contains("user_cancelled"));
            assert!(request.contains("Do not automatically retry or recreate"));
            assert!(received.try_recv().is_err());
            assert_eq!(instance.snapshot().bash_executions.len(), 1);
        } else {
            assert_eq!(view.snapshot.bash_executions[0].status, BashStatus::Failed);
            assert!(history.contains("Recover the interrupted work"));
            assert!(!history.contains("Do not automatically retry or recreate"));
            assert_eq!(
                request_count, 3,
                "unexpected failure must still notify the model"
            );
        }
        tokio::time::timeout(Duration::from_secs(5), instance.manager.shutdown())
            .await
            .unwrap()
            .unwrap();
        server.abort();
        std::fs::remove_file(execution.output_path).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
