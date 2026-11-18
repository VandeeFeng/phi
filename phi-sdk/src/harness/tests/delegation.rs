use super::*;

#[tokio::test]
async fn delegated_system_prompt_hooks_reach_provider_on_initial_and_follow_up_runs() {
    use crate::core::lifecycle::BuildSystemPromptOutcome;
    use crate::hooks::Hook;

    let root = std::path::Path::new("/tmp")
        .join(format!("phi-child-system-prompt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let mut phi = fixture.phi.clone();
    for suffix in ["first-hook", "second-hook"] {
        phi.hooks
            .register_hook(Hook::build_system_prompt(move |event| async move {
                Ok(BuildSystemPromptOutcome {
                    system_prompt: format!(
                        "{}\n{suffix}:{}",
                        event.system_prompt,
                        event.cwd.as_deref().unwrap()
                    ),
                })
            }));
    }
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
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, completion));
    let started = parent
        .spawn_subagent(AgentSpawnRequest::new("probe", "initial-work"))
        .await
        .unwrap();
    let expected_suffix = format!("second-hook:{}", root.display());

    for follow_up in [false, true] {
        if follow_up {
            fixture
                .manager
                .for_agent(parent.id())
                .unwrap()
                .send_message(
                    &AgentInstanceId::new(started.instance_id().as_str()),
                    "follow-up-work",
                )
                .await
                .unwrap();
        }
        let request = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let request = received.recv().await.unwrap();
                if request["model"] == "child" {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        let system_prompt = system_text(&request);
        assert!(system_prompt.starts_with("Perform the delegated task."));
        assert!(system_prompt.contains(&format!("Current working directory: {}", root.display())));
        assert!(system_prompt.ends_with(&expected_suffix), "{system_prompt}");
        assert!(!system_prompt.contains("first-hook:"));
        let child = fixture
            .manager
            .get(&AgentInstanceId::new(started.instance_id().as_str()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), child.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
    }

    fixture.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn delegated_system_prompt_hook_wait_remains_cancellable() {
    let root = std::path::Path::new("/tmp")
        .join(format!("phi-child-prompt-cancel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let observed = entered.clone();
    let mut phi = fixture.phi.clone();
    phi.hooks
        .register_hook(crate::hooks::Hook::build_system_prompt(move |event| {
            let observed = observed.clone();
            async move {
                if event
                    .system_prompt
                    .starts_with("Perform the delegated task.")
                {
                    observed.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(crate::core::lifecycle::BuildSystemPromptOutcome {
                    system_prompt: event.system_prompt,
                })
            }
        }));
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
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, completion));
    let started = parent
        .spawn_subagent(AgentSpawnRequest::new("probe", "initial-work"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            parent.cancel_subagent(started.instance_id().clone()),
        )
        .await
        .unwrap()
        .unwrap()
    );
    let status = wait_child_completion(&parent, started.instance_id()).await;
    assert_eq!(
        status.outcome.unwrap().unwrap_err().kind,
        crate::core::event::PhiFailureKind::Cancelled
    );
    assert!(received.try_recv().is_err());

    fixture.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn delegation_mode_controls_tool_return_and_initial_feedback() {
    use crate::agent_manager::DelegationMode::{Background, Foreground};

    for mode in [Foreground, Background] {
        for outcome in ["success", "failure", "parent_abort", "child_cancel"] {
            let root =
                std::env::temp_dir().join(format!("phi-delegation-mode-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let instance =
                instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
            let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
            let release = Arc::new(tokio::sync::Notify::new());
            let held = release.clone();
            let server = tokio::spawn(mock_provider_with_reply(
                listener,
                requests,
                move |request| {
                    let held = held.clone();
                    async move {
                        if system_text(&request).contains("Perform the delegated task.") {
                            held.notified().await;
                            if outcome == "failure" {
                                return json!({"error": {"message": "child rejected", "type": "authentication_error"}});
                            }
                            return completion(&request);
                        }
                        let replied = request["messages"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|message| message["role"] == "tool");
                        let delta = if replied {
                            json!({"content": "parent-final"})
                        } else {
                            let arguments = json!({"action": "create", "role": "probe", "prompt": "delegated-task", "mode": mode});
                            json!({"tool_calls": [{"index": 0, "id": "call_agent", "type": "function",
                            "function": {"name": "Agent", "arguments": arguments.to_string()}}]})
                        };
                        json!({"id": "completion", "model": "test", "choices": [{"delta": delta, "finish_reason": if replied {"stop"} else {"tool_calls"}}]})
                    }
                },
            ));
            instance
                .submit(TurnPrompt {
                    prompt: "delegate once".into(),
                    source: UserPromptSubmitSource::Interactive,
                    tool_policy: Default::default(),
                    injected_context: Vec::new(),
                    images: Vec::new(),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while !system_text(&received.recv().await.unwrap())
                    .contains("Perform the delegated task.")
                {}
            })
            .await
            .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let statuses = instance.subagent_snapshots().await.unwrap();
                    let accepted = statuses.first().is_some_and(|snapshot| snapshot.status.admission == SubagentAdmission::Accepted);
                    let transcript = instance.transcript().await.unwrap();
                    let returned = transcript.entries.iter().any(|entry| matches!(&entry.kind,
                        crate::transcript::TranscriptEntryKind::Message { message: crate::transcript::AgentMessage::ToolResult { tool_name, .. } } if tool_name == "Agent"));
                    if accepted && (mode == Foreground || returned) {
                        assert_eq!(returned, mode == Background);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            if outcome == "parent_abort" {
                instance.abort().await.unwrap();
                let statuses = instance.subagent_snapshots().await.unwrap();
                assert!(statuses[0].status.outcome.is_none());
                assert_eq!(statuses[0].status.admission, SubagentAdmission::Accepted);
            } else if outcome == "child_cancel" {
                let statuses = instance.subagent_snapshots().await.unwrap();
                assert!(
                    instance
                        .cancel_subagent(statuses[0].status.creation.instance_id().clone())
                        .await
                        .unwrap()
                );
            }
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
                .await
                .unwrap()
                .unwrap();
            let transcript = instance.transcript().await.unwrap();
            let results = transcript
                .entries
                .iter()
                .filter_map(|entry| match &entry.kind {
                    crate::transcript::TranscriptEntryKind::Message {
                        message:
                            crate::transcript::AgentMessage::ToolResult {
                                tool_name,
                                content,
                                is_error,
                                ..
                            },
                    } if tool_name == "Agent" => Some((content, *is_error)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 1);
            let output = serde_json::to_string(results[0].0).unwrap();
            if mode == Foreground {
                assert_eq!(results[0].1, outcome != "success");
                assert!(!output.contains("async_launched"));
                match outcome {
                    "success" => assert!(output.contains("child-result")),
                    "failure" => assert!(output.contains("child rejected")),
                    _ => assert!(!output.contains("agentId")),
                }
            } else {
                assert!(!results[0].1);
                assert!(output.contains("async_launched"));
            }
            let feedback = transcript
                .entries
                .iter()
                .filter(|entry| {
                    matches!(
                        &entry.kind,
                        crate::transcript::TranscriptEntryKind::Message {
                            message: crate::transcript::AgentMessage::Agent { .. }
                        }
                    )
                })
                .count();
            assert_eq!(feedback, usize::from(mode == Background));
            let statuses = instance.subagent_snapshots().await.unwrap();
            let canonical = statuses[0].status.outcome.as_ref().unwrap();
            assert_eq!(
                canonical.is_err(),
                matches!(outcome, "failure" | "child_cancel")
            );
            assert!(
                !serde_json::to_string(&transcript)
                    .unwrap()
                    .contains("Perform the delegated task.")
            );
            instance.manager.shutdown().await.unwrap();
            server.abort();
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[tokio::test]
async fn background_launch_batch_controls_continuation() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for scenario in ["single", "multiple", "mixed", "failure"] {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-launch-batch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("payload.txt"), "read-result").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let parent_requests = Arc::new(AtomicUsize::new(0));
        let observed = parent_requests.clone();
        let (requests, _received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, requests, move |request| {
            if request["model"] == "child" {
                return completion(request);
            }
            observed.fetch_add(1, Ordering::SeqCst);
            let replied = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            if replied {
                return completion(request);
            }
            let count = if scenario == "multiple" { 2 } else { 1 };
            let mut calls = (0..count)
                .map(|index| {
                    let arguments = json!({
                        "action": "create",
                        "role": if scenario == "failure" { "absent" } else { "probe" },
                        "prompt": "delegated-task",
                        "mode": "background",
                    });
                    json!({"index":index, "id":format!("agent-{index}"), "type":"function",
                        "function":{"name":"Agent", "arguments":arguments.to_string()}})
                })
                .collect::<Vec<_>>();
            if scenario == "mixed" {
                calls.push(json!({"index":count, "id":"read", "type":"function",
                    "function":{"name":"Read", "arguments":json!({"path":"payload.txt"}).to_string()}}));
            }
            json!({"id":"completion", "model":"test", "choices":[{
                "delta":{"tool_calls":calls}, "finish_reason":"tool_calls"
            }]})
        }));
        instance
            .submit(TurnPrompt {
                prompt: "delegate work".into(),
                source: UserPromptSubmitSource::Sdk,
                tool_policy: Default::default(),
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let view =
            tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
                .await
                .unwrap()
                .unwrap();
        assert!(view.snapshot.latest_error.is_none(), "{scenario}");
        let feedback_count = match scenario {
            "multiple" => 2,
            "failure" => 0,
            _ => 1,
        };
        let continues = usize::from(matches!(scenario, "mixed" | "failure"));
        assert_eq!(
            parent_requests.load(Ordering::SeqCst),
            1 + continues + feedback_count,
            "{scenario}"
        );
        let results = view
            .transcript
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                crate::transcript::TranscriptEntryKind::Message {
                    message:
                        crate::transcript::AgentMessage::ToolResult {
                            tool_name,
                            content,
                            is_error,
                            ..
                        },
                } => Some((tool_name.as_str(), content, *is_error)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let expected_results = if matches!(scenario, "multiple" | "mixed") {
            2
        } else {
            1
        };
        assert_eq!(results.len(), expected_results, "{scenario}");
        assert_eq!(results.iter().any(|result| result.2), scenario == "failure");
        if scenario == "mixed" {
            assert!(results.iter().any(|(name, content, _)| {
                *name == "Read"
                    && serde_json::to_string(content)
                        .unwrap()
                        .contains("read-result")
            }));
        }
        instance.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn message_queued_during_creation_cannot_replace_initial_result() {
    let root = std::env::temp_dir().join(format!("phi-message-boundary-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let held = release.clone();
    let server = tokio::spawn(mock_provider_with_reply(
        listener,
        requests,
        move |request| {
            let release = held.clone();
            async move {
                let mut reply = completion(&request);
                if request["model"] == "child" {
                    let followup = request["messages"]
                        .to_string()
                        .contains("independent-follow-up");
                    if !followup {
                        release.notified().await;
                    }
                    reply["choices"][0]["delta"]["content"] = json!(if followup {
                        "follow-up-result"
                    } else {
                        "initial-result"
                    });
                }
                reply
            }
        },
    ));
    let started = instance
        .spawn_subagent(crate::agent_manager::AgentSpawnRequest::new(
            "probe",
            "initial-work",
        ))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    let child = instance
        .manager
        .get(&AgentInstanceId::new(started.instance_id().as_str()))
        .unwrap();
    let receipt = instance
        .manager
        .for_agent(instance.id())
        .unwrap()
        .send_message(child.id(), "independent-follow-up")
        .await
        .unwrap();
    assert!(!receipt.delivery().is_terminal());
    release.notify_one();
    let initial = wait_child_completion(&instance, started.instance_id()).await;
    assert_eq!(initial.outcome.unwrap().unwrap(), "initial-result");
    let view = tokio::time::timeout(Duration::from_secs(5), child.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    assert!(
        serde_json::to_string(&view.transcript)
            .unwrap()
            .contains("follow-up-result")
    );
    let parent = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    let history = serde_json::to_string(&parent.transcript).unwrap();
    assert!(history.contains("Automatic subagent result"));
    assert!(history.contains("follow-up-result"));
    assert_eq!(instance.subagent_snapshots().await.unwrap().len(), 1);
    instance.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn creation_and_submission_prepare_once_without_committing_intercepted_inputs() {
    let root = std::env::temp_dir().join(format!("phi-task-input-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_calls = calls.clone();
    let entered = Arc::new(tokio::sync::Notify::new());
    let observed = entered.clone();
    let mut phi = fixture.phi.clone();
    phi.hooks
        .register_hook(crate::hooks::Hook::user_prompt_submit(move |event| {
            let observed = observed.clone();
            let automatic_report = event
                .prompt
                .contains("Automatic subagent result from Agent ")
                || event.prompt.contains("Subagent terminal outcome:");
            if !automatic_report {
                observed_calls.lock().unwrap().push(event.prompt.clone());
            }
            async move {
                assert!(matches!(event.source, UserPromptSubmitSource::Internal));
                if automatic_report {
                    assert!(matches!(
                        event.dispatch,
                        crate::core::lifecycle::UserPromptDispatch::FollowUp
                            | crate::core::lifecycle::UserPromptDispatch::Immediate
                    ));
                    return Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                        notices: Vec::new(),
                        prompt: event.prompt,
                        injected_context: Vec::new(),
                    });
                }
                assert!(matches!(
                    event.dispatch,
                    crate::core::lifecycle::UserPromptDispatch::Immediate
                ));
                if event.prompt == "handled-input" {
                    return Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Handled {
                        reason: Some("prompt intercepted".into()),
                    });
                }
                if matches!(event.prompt.as_str(), "blocked-input" | "stop-input") {
                    observed.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                    notices: Vec::new(),
                    prompt: format!("prepared:{}", event.prompt),
                    injected_context: vec![crate::context::InjectedContext {
                        source: crate::transcript::ContextSource::Hook,
                        content: "task-preparation-context".into(),
                    }],
                })
            }
        }));
    let parent = fixture
        .manager
        .insert(phi.agent_instance_builder().build().await.unwrap())
        .unwrap();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, completion));
    let interrupted = parent
        .spawn_subagent(AgentSpawnRequest::new("probe", "blocked-input"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let interrupted_executor = fixture
        .manager
        .get(&AgentInstanceId::new(interrupted.instance_id().as_str()))
        .unwrap();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            parent.cancel_subagent(interrupted.instance_id().clone())
        )
        .await
        .unwrap()
        .unwrap()
    );
    assert_eq!(
        wait_child_completion(&parent, interrupted.instance_id())
            .await
            .admission,
        SubagentAdmission::Rejected
    );
    assert!(
        !parent
            .cancel_subagent(interrupted.instance_id().clone())
            .await
            .unwrap()
    );
    assert!(
        interrupted_executor
            .view()
            .await
            .unwrap()
            .transcript
            .entries
            .is_empty()
    );
    assert!(
        fixture
            .manager
            .spawn_agent(
                parent.id(),
                &phi,
                AgentSpawnRequest::new("probe", "handled-input")
            )
            .await
            .is_err()
    );
    let initial = fixture
        .manager
        .spawn_agent(
            parent.id(),
            &phi,
            AgentSpawnRequest::new("probe", "initial-input"),
        )
        .await
        .unwrap();
    let executor = fixture
        .manager
        .get(&AgentInstanceId::new(initial.instance_id().as_str()))
        .unwrap();
    assert!(
        wait_child_completion(&parent, initial.instance_id())
            .await
            .outcome
            .unwrap()
            .is_ok()
    );
    for prompt in ["followup-input", "handled-input", "after-handled"] {
        let disposition = executor
            .dispatch(
                TurnPrompt {
                    prompt: prompt.into(),
                    source: UserPromptSubmitSource::Internal,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: Default::default(),
                },
                SubmissionDispatch::FollowUp,
            )
            .await
            .unwrap();
        if prompt == "handled-input" {
            assert!(matches!(disposition, SubmissionDisposition::Handled { .. }));
        } else {
            assert!(matches!(disposition, SubmissionDisposition::Started { .. }));
        }
        executor.wait_for_quiescence(None).await.unwrap();
    }
    let stopping_executor = executor.clone();
    let stopping = tokio::spawn(async move {
        stopping_executor
            .dispatch(
                TurnPrompt {
                    prompt: "stop-input".into(),
                    source: UserPromptSubmitSource::Internal,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: Default::default(),
                },
                SubmissionDispatch::FollowUp,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), fixture.manager.stop(executor.id()))
        .await
        .unwrap()
        .unwrap();
    assert!(stopping.await.unwrap().is_err());
    let history = executor.view().await.unwrap().transcript;
    let users = history
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message {
                message: AgentMessage::User { content },
            } => Some(content),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(users.len(), 3);
    let serialized = serde_json::to_string(&history).unwrap();
    for prompt in ["initial-input", "followup-input", "after-handled"] {
        assert!(serialized.contains(&format!("prepared:{prompt}")));
    }
    for prompt in ["handled-input", "blocked-input", "stop-input"] {
        assert!(!serialized.contains(prompt));
    }
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        [
            "blocked-input",
            "handled-input",
            "initial-input",
            "followup-input",
            "handled-input",
            "after-handled",
            "stop-input",
        ]
    );
    assert_eq!(history.entries.iter().filter(|entry| matches!(&entry.kind,
        TranscriptEntryKind::Message { message: AgentMessage::Context { content, .. } }
            if content.iter().any(|block| matches!(block,
                crate::transcript::ContentBlock::Text { text } if text.as_str() == "task-preparation-context")))).count(), 3);
    let mut requests = Vec::new();
    while let Ok(request) = received.try_recv() {
        requests.push(request.to_string());
    }
    for prompt in ["initial-input", "followup-input", "after-handled"] {
        assert!(
            requests
                .iter()
                .any(|request| request.contains(&format!("prepared:{prompt}"))
                    && request.contains("task-preparation-context"))
        );
    }
    fixture.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn initial_result_delivery_matches_mode_without_observers() {
    use crate::agent_manager::DelegationMode;

    for (origin, mode) in ["instance", "manager", "scoped"]
        .into_iter()
        .flat_map(|origin| {
            [DelegationMode::Foreground, DelegationMode::Background].map(|mode| (origin, mode))
        })
    {
        let expected_feedback = usize::from(mode == DelegationMode::Background);
        let root =
            std::env::temp_dir().join(format!("phi-unattached-provider-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, requests, completion));
        let request = AgentSpawnRequest::new("probe", "inspect").mode(mode);
        let child = match origin {
            "manager" => instance
                .manager
                .spawn_agent(instance.id(), &instance.phi, request)
                .await
                .unwrap(),
            "scoped" => instance
                .manager
                .for_agent(instance.id())
                .unwrap()
                .spawn_agent(&instance.phi, request)
                .await
                .unwrap(),
            _ => instance.spawn_subagent(request).await.unwrap(),
        };
        tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.mode(), mode);
        let child_instance = instance.manager.get(child.instance_id()).unwrap();
        let completion = child_instance
            .complete_delegated_agent(tokio::sync::watch::channel(false).1)
            .await
            .unwrap();
        assert_eq!(completion.outcome.unwrap(), "child-result");
        let delivered = instance.transcript().await.unwrap();
        assert_eq!(
            delivered
                .entries
                .iter()
                .filter(|entry| matches!(
                    &entry.kind,
                    crate::transcript::TranscriptEntryKind::Message {
                        message: crate::transcript::AgentMessage::Agent { .. }
                    }
                ))
                .count(),
            expected_feedback
        );
        assert!(!delivered.has_user_message());
        let mut outcome_request_seen = false;
        while let Ok(request) = received.try_recv() {
            outcome_request_seen |= request.to_string().contains(child.instance_id().as_str());
        }
        assert_eq!(outcome_request_seen, mode == DelegationMode::Background);
        let snapshots = instance.subagent_snapshots().await.unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(
            instance
                .subagent_status(child.instance_id().clone())
                .await
                .unwrap()
                .unwrap(),
            snapshots[0].status
        );
        assert!(snapshots[0].status.outcome.is_some());
        let events = instance.subscribe();
        instance
            .submit(TurnPrompt {
                tool_policy: Default::default(),
                prompt: "after reattach".into(),
                source: UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        drop(events);
        tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        assert!(
            std::iter::from_fn(|| received.try_recv().ok())
                .any(|request| request.to_string().contains("after reattach"))
        );
        let final_transcript = instance.stop().await.unwrap();
        assert_eq!(
            serde_json::to_value(instance.subagent_snapshots().await.unwrap()).unwrap(),
            serde_json::to_value(&snapshots).unwrap()
        );
        assert_eq!(
            final_transcript
                .entries
                .iter()
                .filter(|entry| matches!(
                    &entry.kind,
                    crate::transcript::TranscriptEntryKind::Message {
                        message: crate::transcript::AgentMessage::Agent { .. }
                    }
                ))
                .count(),
            expected_feedback
        );
        instance.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

async fn wait_for_delegation(
    events: &mut tokio::sync::broadcast::Receiver<AgentInstanceEvent>,
) -> String {
    let mut instance_id = None;
    let mut task_finished = false;
    let mut turn_finished = false;
    while !task_finished || !turn_finished {
        match events.recv().await.unwrap() {
            AgentInstanceEvent::Subagent(SubagentActivity::Started {
                instance_id: id, ..
            }) => instance_id = Some(id),
            AgentInstanceEvent::Subagent(SubagentActivity::Finished { .. }) => task_finished = true,
            AgentInstanceEvent::Phi(envelope) => match envelope.event {
                crate::core::event::PhiEvent::TurnFinished { .. } => turn_finished = true,
                crate::core::event::PhiEvent::TurnFailed { failure } => panic!("{failure:?}"),
                _ => {}
            },
            _ => {}
        }
    }
    instance_id.unwrap()
}

#[tokio::test]
async fn directed_modes_preserve_delivery_and_turn_scope() {
    use crate::agent_manager::DelegationMode::{Background, Foreground};

    for (requested, forced_background, teammate) in [
        (None, false, false),
        (None, false, true),
        (Some(Foreground), false, false),
        (Some(Background), false, false),
        (Some(Foreground), true, false),
        (Some(Foreground), false, true),
    ] {
        let root = std::env::temp_dir().join(format!("phi-directed-mode-{}", uuid::Uuid::new_v4()));
        let worktree = root.join("prepared-worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fixture = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let mut phi = fixture.phi.clone();
        phi.cfg.agent_background = forced_background;
        let instance = fixture
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .team_capability(crate::agent_manager::TeamCapability::Enabled)
                    .task_list(
                        crate::tasks::TaskListBinding::new(root.join("tasks"), "test-list")
                            .unwrap(),
                    )
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        if teammate {
            fixture
                .manager
                .initialize_team(
                    instance.id(),
                    crate::agent_manager::TeamStorageRoot::new(root.join("teams")).unwrap(),
                )
                .await
                .unwrap();
        }
        let mut events = instance.subscribe();
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, requests, move |request| {
            let mut response = completion(request);
            if system_text(request).contains("[Directed delegation for this turn only]")
                && request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool")
            {
                response["choices"][0]["delta"] = json!({"content": "parent-summary"});
            }
            if let Some(call) = response.pointer_mut("/choices/0/delta/tool_calls/0/function") {
                let mut arguments = json!({
                    "action": "create", "role": "probe", "prompt": "delegated-task",
                });
                if let Some(mode) = requested {
                    arguments["mode"] = json!(mode);
                }
                if teammate {
                    arguments["alias"] = json!("reviewer");
                }
                if system_text(request).contains("set working_dir to that existing directory")
                    && request["messages"]
                        .to_string()
                        .contains("in prepared-worktree")
                {
                    arguments["working_dir"] = json!("prepared-worktree");
                }
                call["arguments"] = json!(arguments.to_string());
            }
            response
        }));
        instance
            .submit_directed(
                TurnPrompt {
                    tool_policy: Default::default(),
                    prompt: if requested == Some(Foreground) {
                        "inspect in prepared-worktree".into()
                    } else {
                        "inspect".into()
                    },
                    source: UserPromptSubmitSource::Interactive,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                },
                DirectedDelegation {
                    agent: "probe".into(),
                    mode: requested,
                },
            )
            .await
            .unwrap();
        let id = tokio::time::timeout(Duration::from_secs(10), wait_for_delegation(&mut events))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let status = instance
            .subagent_status(crate::agent_instance::AgentInstanceId::new(id))
            .await
            .unwrap()
            .unwrap();
        let expected = if forced_background || teammate {
            Background
        } else {
            requested.unwrap_or(Background)
        };
        assert_eq!(status.creation.mode(), expected);
        let expected_dir = if requested == Some(Foreground) {
            &worktree
        } else {
            &root
        };
        assert_eq!(
            status.creation.working_dir(),
            expected_dir.canonicalize().unwrap()
        );
        assert_eq!(
            status.outcome.as_ref().unwrap().as_ref().unwrap(),
            "child-result"
        );
        let transcript = instance.transcript().await.unwrap();
        let output = transcript
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                crate::transcript::TranscriptEntryKind::Message {
                    message:
                        crate::transcript::AgentMessage::ToolResult {
                            tool_name, content, ..
                        },
                } if tool_name == "Agent" => Some(serde_json::to_string(content).unwrap()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(output.len(), 1);
        let output = &output[0];
        if expected == Foreground {
            assert!(output.contains("child-result"));
            assert!(!output.contains("async_launched"));
            assert_eq!(
                transcript
                    .entries
                    .iter()
                    .filter(|entry| matches!(
                        &entry.kind,
                        crate::transcript::TranscriptEntryKind::Message {
                            message: crate::transcript::AgentMessage::Assistant { .. },
                        }
                    ))
                    .count(),
                2,
                "foreground delegation must resume the parent with the child report"
            );
            assert!(transcript.entries.iter().any(|entry| matches!(
                    &entry.kind,
                    crate::transcript::TranscriptEntryKind::Message {
                        message: crate::transcript::AgentMessage::Assistant { content, .. },
                    } if content.iter().any(|block| matches!(block,
                        crate::transcript::ContentBlock::Text { text } if text.as_str() == "parent-summary"
                    ))
                )));
        } else {
            assert!(output.contains("async_launched"));
            assert!(
                !serde_json::to_string(&transcript)
                    .unwrap()
                    .contains("parent-summary")
            );
        }
        assert_eq!(
            transcript
                .entries
                .iter()
                .filter(|entry| matches!(
                    &entry.kind,
                    crate::transcript::TranscriptEntryKind::Message {
                        message: crate::transcript::AgentMessage::Agent { .. }
                    }
                ))
                .count(),
            usize::from(expected == Background)
        );
        let first = received.recv().await.unwrap();
        let system = system_text(&first);
        let (_, binding) = system.split_once("with these fixed arguments: ").unwrap();
        let binding = serde_json::Deserializer::from_str(binding)
            .into_iter::<Value>()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(binding["action"], "create");
        assert_eq!(binding["role"], status.creation.definition());
        assert_eq!(binding["mode"], json!(requested.unwrap_or(Background)));
        assert_eq!(status.creation.prompt(), "delegated-task");
        let requests = std::iter::from_fn(|| received.try_recv().ok()).collect::<Vec<_>>();
        if expected == Foreground {
            let summary = requests
                .iter()
                .find(|request| {
                    system_text(request).contains("[Directed delegation for this turn only]")
                })
                .expect("foreground delegation must request a summary");
            assert!(summary["tools"].as_array().is_none_or(Vec::is_empty));
            let system = system_text(summary);
            assert!(system.contains("<available_tools>\n(none)\n</available_tools>"));
            assert!(system.contains("<tool_guidelines>\n\n</tool_guidelines>"));
        }
        assert!(requests.iter().any(|request| {
            let system = system_text(&request);
            system.contains("Perform the delegated task.")
                && !system.contains("[Directed delegation for this turn only]")
        }));
        instance
            .submit(TurnPrompt {
                prompt: "ordinary-check".into(),
                source: UserPromptSubmitSource::Interactive,
                tool_policy: Default::default(),
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let ordinary = received.recv().await.unwrap();
        assert!(ordinary.to_string().contains("ordinary-check"));
        assert!(!system_text(&ordinary).contains("[Directed delegation for this turn only]"));
        assert!(
            ordinary["tools"].as_array().is_some_and(|tools| {
                tools.iter().any(|tool| tool["function"]["name"] == "Agent")
            })
        );
        fixture.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn failed_directed_completion(request: &Value) -> Value {
    if !system_text(request).contains("[Directed delegation for this turn only]") {
        return completion(request);
    }
    let text = request.to_string();
    let working_dir = if text.contains("malformed-directory") {
        json!({"path": "invalid"})
    } else {
        json!("missing-child-directory")
    };
    let agent = if text.contains("wrong-agent") {
        "leaf"
    } else {
        "probe"
    };
    let arguments = if text.contains("mixed-arguments") {
        json!({"action": "create", "role": "probe", "prompt": "inspect", "recipient": "peer"})
    } else {
        json!({"action": "create", "role": agent, "prompt": "inspect", "working_dir": working_dir})
    };
    let mut calls = vec![json!({"index": 0, "id": "failed_agent", "type": "function",
        "function": {"name": "Agent", "arguments": arguments.to_string()}})];
    if text.contains("mixed-tools") {
        calls.push(
            json!({"index": 1, "id": "unrelated_call", "type": "function",
            "function": {"name": "Bash", "arguments": json!({"command": "exit 97"}).to_string()}}),
        );
    }
    json!({"id": "completion", "model": "test", "choices": [{"delta": {
        "tool_calls": calls
    }, "finish_reason": "tool_calls"}]})
}

#[tokio::test]
async fn directed_failure_ends_turn_without_retry_and_preserves_error() {
    for (prompt, expected) in [
        ("@probe malformed-directory", "invalid type"),
        ("@probe mixed-arguments", "unknown field `recipient`"),
        ("@probe missing-directory", "working directory"),
        ("@probe wrong-agent", "does not match"),
        ("@probe mixed-tools", "no other tools were executed"),
    ] {
        let root =
            std::env::temp_dir().join(format!("phi-directed-failure-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let mut events = instance.subscribe();
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(
            listener,
            requests,
            failed_directed_completion,
        ));
        instance
            .submit_directed(
                TurnPrompt {
                    tool_policy: Default::default(),
                    prompt: prompt.into(),
                    source: UserPromptSubmitSource::Interactive,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                },
                DirectedDelegation {
                    agent: "probe".into(),
                    mode: None,
                },
            )
            .await
            .unwrap();
        let failure = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let AgentInstanceEvent::Phi(envelope) = events.recv().await.unwrap()
                    && let crate::core::event::PhiEvent::TurnFailed { failure } = envelope.event
                {
                    break failure;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(failure.kind, crate::PhiFailureKind::Delegation);
        assert!(failure.message.contains(expected), "{}", failure.message);
        tokio::time::timeout(Duration::from_secs(10), async {
            while instance.snapshot().status != crate::agent_instance::AgentInstanceStatus::Failed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        received.recv().await.unwrap();
        assert!(
            received.try_recv().is_err(),
            "failed delegation requested another completion"
        );
        assert!(instance.subagent_snapshots().await.unwrap().is_empty());
        let transcript = serde_json::to_string(&instance.transcript().await.unwrap()).unwrap();
        assert!(transcript.contains(expected));
        assert!(!transcript.contains("the tool failed"));
        instance
            .submit(TurnPrompt {
                tool_policy: Default::default(),
                prompt: "ordinary-check".into(),
                source: UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let ordinary = tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!system_text(&ordinary).contains("[Directed delegation for this turn only]"));
        instance.stop().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn nested_completion(request: &Value) -> Value {
    let parent = system_text(request).contains("Perform the delegated task.");
    let replied = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool");
    let (delta, reason) = if parent && !replied {
        (
            json!({"tool_calls": [{"index": 0, "id": "nested_call", "type": "function",
            "function": {"name": "Agent", "arguments": json!({"action": "create", "role": "leaf", "prompt": "leaf-task"}).to_string()}}]}),
            "tool_calls",
        )
    } else {
        let text = if parent {
            assert!(request.to_string().contains("leaf-result"));
            "parent-result"
        } else if system_text(request).contains("Leaf task only.") {
            "leaf-result"
        } else {
            "master-result"
        };
        (json!({"content": text}), "stop")
    };
    json!({"id": "completion", "model": request["model"], "choices": [{"delta": delta, "finish_reason": reason}]})
}

#[tokio::test]
async fn nested_child_result_resumes_creator_before_root() {
    let root = std::env::temp_dir().join(format!("phi-nested-provider-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, nested_completion));
    let parent = instance
        .spawn_subagent(AgentSpawnRequest::new("probe", "parent-task"))
        .await
        .unwrap();
    let quiescent =
        tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        quiescent.snapshot.status,
        crate::agent_instance::AgentInstanceStatus::Idle
    );
    assert!(quiescent.pending_submission_ids.is_empty());
    let parent_instance = instance.manager.get(parent.instance_id()).unwrap();
    assert_ne!(parent_instance.id(), instance.id());
    assert!(
        instance
            .subagent_status(parent.instance_id().clone())
            .await
            .unwrap()
            .unwrap()
            .outcome
            .is_some()
    );
    let snapshots = instance.subagent_snapshots().await.unwrap();
    let parent = snapshots
        .iter()
        .find(|snapshot| snapshot.status.creation.instance_id() == parent.instance_id())
        .unwrap();
    assert_eq!(snapshots.len(), 1);
    assert_eq!(
        parent.status.creation.mode(),
        crate::agent_manager::DelegationMode::Background
    );
    let descendants = parent_instance.subagent_snapshots().await.unwrap();
    let leaf = descendants
        .iter()
        .find(|snapshot| snapshot.status.creation.definition() == "leaf")
        .unwrap();
    assert_eq!(
        parent_instance
            .subagent_status(leaf.status.creation.instance_id().clone())
            .await
            .unwrap()
            .unwrap()
            .creation,
        leaf.status.creation
    );
    assert_ne!(
        leaf.status.creation.instance_id().as_str(),
        parent_instance.id().as_str()
    );
    assert_eq!(
        leaf.status.creation.creator_id().as_str(),
        parent.status.creation.instance_id().as_str()
    );
    assert_eq!(
        parent.status.outcome.as_ref().unwrap().as_ref().unwrap(),
        "parent-result"
    );
    assert_eq!(
        leaf.status.creation.mode(),
        crate::agent_manager::DelegationMode::Foreground
    );
    assert!(leaf.status.outcome.is_some());
    let (_cancel, cancellation) = tokio::sync::watch::channel(false);
    let (completion, concurrent) = tokio::join!(
        parent_instance.complete_delegated_agent(cancellation.clone()),
        parent_instance.complete_delegated_agent(cancellation),
    );
    let completion = completion.unwrap();
    assert_eq!(concurrent.unwrap(), completion);
    assert_eq!(completion.outcome.as_ref().unwrap(), "parent-result");
    assert_eq!(
        parent_instance.snapshot().definition.as_deref(),
        Some("probe")
    );
    let canonical = parent_instance.transcript().await.unwrap();
    let parent_history = serde_json::to_string(&canonical).unwrap();
    assert!(parent_history.contains("leaf-result"));
    let master_history = serde_json::to_string(&instance.transcript().await.unwrap()).unwrap();
    assert!(!master_history.contains("leaf-result"));
    assert!(
        std::iter::from_fn(|| received.try_recv().ok()).all(|request| {
            !system_text(&request).contains("Perform the delegated task.")
                || !request.to_string().contains("Subagent terminal outcome:")
        })
    );
    parent_instance.reset_context(None).await.unwrap();
    assert!(parent_instance.transcript().await.unwrap().entries.len() > canonical.entries.len());
    assert_eq!(
        parent_instance
            .complete_delegated_agent(tokio::sync::watch::channel(false).1)
            .await
            .unwrap(),
        completion
    );
    let later = parent_instance
        .spawn_subagent(AgentSpawnRequest::new("leaf", "independent-task"))
        .await
        .unwrap();
    assert_eq!(later.creator_id().as_str(), parent_instance.id().as_str());
    assert_eq!(
        instance
            .subagent_status(parent.status.creation.instance_id().clone())
            .await
            .unwrap()
            .unwrap(),
        parent.status
    );
    instance.stop().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn cancelling_parent_preserves_child_and_settles_foreground_tool() {
    let root = std::env::temp_dir().join(format!("phi-nested-cancel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let release_leaf = Arc::new(tokio::sync::Notify::new());
    let server_release = release_leaf.clone();
    let server = tokio::spawn(mock_provider_with_reply(
        listener,
        requests,
        move |request| {
            let release = server_release.clone();
            async move {
                if system_text(&request).contains("Leaf task only.") {
                    release.notified().await;
                }
                nested_completion(&request)
            }
        },
    ));
    let parent = instance
        .spawn_subagent(AgentSpawnRequest::new("probe", "parent-task"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !system_text(&received.recv().await.unwrap()).contains("Leaf task only.") {}
    })
    .await
    .unwrap();
    let parent_instance = instance
        .manager
        .get(&crate::agent_instance::AgentInstanceId::new(
            parent.instance_id().as_str(),
        ))
        .unwrap();
    assert!(
        instance
            .cancel_subagent(parent.instance_id().clone())
            .await
            .unwrap()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = instance
                .subagent_status(parent.instance_id().clone())
                .await
                .unwrap()
                .unwrap();
            if let Some(Err(failure)) = status.outcome {
                assert_eq!(failure.kind, crate::PhiFailureKind::Cancelled);
                assert_eq!(
                    failure.cancellation_reason,
                    Some(crate::CancellationReason::UserRequested)
                );
                assert!(failure.is_user_cancelled());
                assert_eq!(status.admission, SubagentAdmission::Accepted);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let canonical = instance
        .subagent_status(parent.instance_id().clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        canonical
            .outcome
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .kind,
        crate::PhiFailureKind::Cancelled
    );
    let running = parent_instance.subagent_snapshots().await.unwrap();
    assert_eq!(running.len(), 1);
    assert!(running[0].status.outcome.is_none());
    parent_instance
        .cancel_subagent(running[0].status.creation.instance_id().clone())
        .await
        .unwrap();
    release_leaf.notify_one();
    let descendants = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshots = parent_instance.subagent_snapshots().await.unwrap();
            if snapshots
                .first()
                .is_some_and(|snapshot| snapshot.status.outcome.is_some())
            {
                break snapshots;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(descendants.len(), 1);
    assert_eq!(descendants[0].status.admission, SubagentAdmission::Accepted);
    assert_eq!(
        descendants[0]
            .status
            .outcome
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap_err()
            .kind,
        crate::PhiFailureKind::Cancelled
    );
    let leaf_instance = instance
        .manager
        .get(&crate::agent_instance::AgentInstanceId::new(
            descendants[0].status.creation.instance_id().as_str(),
        ))
        .unwrap();
    let completion = leaf_instance
        .complete_delegated_agent(tokio::sync::watch::channel(false).1)
        .await
        .unwrap();
    assert_eq!(
        completion.outcome,
        descendants[0].status.outcome.clone().unwrap()
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        parent_instance.wait_for_quiescence(None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        parent_instance.snapshot().status,
        crate::agent_instance::AgentInstanceStatus::Idle
    );
    assert_eq!(
        parent_instance.snapshot().definition.as_deref(),
        Some("probe")
    );
    let transcript = parent_instance.transcript().await.unwrap();
    let settled = parent_instance
        .complete_delegated_agent(tokio::sync::watch::channel(false).1)
        .await
        .unwrap();
    assert_eq!(
        settled.outcome.unwrap_err().kind,
        crate::PhiFailureKind::Cancelled
    );
    assert!(
        !serde_json::to_string(&transcript)
            .unwrap()
            .contains("Subagent terminal outcome:")
    );
    let results = transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            crate::transcript::TranscriptEntryKind::Message {
                message:
                    crate::transcript::AgentMessage::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                        ..
                    },
            } if tool_call_id == "nested_call" => Some((content, is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 1);
    assert!(results[0].1);
    let crate::transcript::ContentBlock::Text { text } = &results[0].0[0] else {
        panic!("expected interrupted foreground result")
    };
    assert!(text.starts_with("agent tool call interrupted; subagent is still running"));
    assert!(text.contains("Observe the existing execution"));
    assert!(!text.contains("only if the user requests"));
    instance.stop().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}
