use super::*;

#[tokio::test]
async fn task_tools_use_registry_policy_and_preserve_unfinished_work() {
    #[derive(Clone, Copy, Debug)]
    enum Policy {
        Allowed,
        Removed,
        NoTools,
        Excluded,
        Denied,
        Preapproved,
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    for policy in [
        Policy::Allowed,
        Policy::Removed,
        Policy::NoTools,
        Policy::Excluded,
        Policy::Denied,
        Policy::Preapproved,
    ] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-task-provider-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let mut phi = base.phi.clone();
        phi.options.no_tools = matches!(policy, Policy::NoTools);
        let permission = match policy {
            Policy::Denied => json!({"mode": "allow", "tools": {"Task": "deny"}}),
            Policy::Preapproved => json!({"mode": "ask"}),
            _ => json!({"mode": "allow"}),
        };
        phi = phi.with_runtime_services(base.phi.runtime_services.clone().permission_policy(
            crate::permission::PermissionConfig::try_from_value(permission).unwrap(),
            Some(root.clone()),
        ));
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(base.task_list().unwrap().clone())
                    .working_dir(root.clone())
                    .options(crate::AgentInstanceOptions {
                        tool_policy: crate::tools::ToolPolicy {
                            allowed: None,
                            disallowed: if matches!(policy, Policy::Excluded) {
                                vec!["Task".into()]
                            } else {
                                Vec::new()
                            },
                        },
                        ..Default::default()
                    })
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let id = instance.id().clone();
        let round = Arc::new(AtomicUsize::new(0));
        let count = round.clone();
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, requests, move |_| {
            let index = count.fetch_add(1, Ordering::SeqCst);
            let call = match index {
                0 => Some((
                    "Task",
                    json!({"action": "create", "subject": "Review security", "description": "Inspect authorization", "active_form": "Reviewing security"}),
                )),
                1 if matches!(policy, Policy::Allowed | Policy::Preapproved) => Some((
                    "Task",
                    json!({"action": "update", "task_id": "1", "owner": id, "status": "in_progress"}),
                )),
                2 if matches!(policy, Policy::Allowed | Policy::Preapproved) => {
                    Some(("Task", json!({"action": "get", "task_id": "1"})))
                }
                3 if matches!(policy, Policy::Allowed | Policy::Preapproved) => {
                    Some(("Task", json!({"action": "list"})))
                }
                _ => None,
            };
            let delta = match &call {
                Some((name, input)) => {
                    json!({"tool_calls": [{"index": 0, "id": format!("task_{index}"), "type": "function", "function": {"name": name, "arguments": input.to_string()}}]})
                }
                None => json!({"content": "Work remains in progress"}),
            };
            json!({"id": "completion", "model": "test", "choices": [{"delta": delta, "finish_reason": if call.is_some() {"tool_calls"} else {"stop"}}]})
        }));
        instance
            .submit(TurnPrompt {
                prompt: "Record and inspect work without implicit completion".into(),
                tool_policy: crate::core::turn::TurnToolPolicy {
                    allowances: if matches!(policy, Policy::Preapproved | Policy::Denied) {
                        ["Task"]
                            .into_iter()
                            .map(|name| (name.into(), "**".into()))
                            .collect()
                    } else {
                        Vec::new()
                    },
                    removals: if matches!(policy, Policy::Removed) {
                        vec!["Task".into()]
                    } else {
                        Vec::new()
                    },
                },
                source: UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while instance.snapshot().status == crate::agent_instance::AgentInstanceStatus::Running
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let first = received.recv().await.unwrap();
        let names = first["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["function"]["name"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(
            names.contains(&"Task"),
            matches!(
                policy,
                Policy::Allowed | Policy::Denied | Policy::Preapproved
            ),
            "{policy:?}"
        );
        for legacy_name in ["TaskCreate", "TaskGet", "TaskList", "TaskUpdate"] {
            assert!(!names.contains(&legacy_name), "{policy:?}: {legacy_name}");
        }
        if matches!(policy, Policy::Allowed | Policy::Preapproved) {
            assert_eq!(names.iter().filter(|name| **name == "Task").count(), 1);
        }
        let transcript = instance.transcript().await.unwrap();
        let results = transcript
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                TranscriptEntryKind::Message {
                    message:
                        AgentMessage::ToolResult {
                            tool_name,
                            is_error,
                            content,
                            ..
                        },
                } => Some((tool_name.as_str(), *is_error, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let path = root.join("tasks/test-list/1.json");
        if matches!(policy, Policy::Allowed | Policy::Preapproved) {
            assert_eq!(
                results.iter().map(|(name, _, _)| *name).collect::<Vec<_>>(),
                ["Task", "Task", "Task", "Task"]
            );
            assert!(
                results.iter().all(|(_, error, _)| !error),
                "{policy:?}: {results:?}"
            );
            let record: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(record["status"], "in_progress");
            assert_eq!(record["owner"]["instance_id"], instance.id().as_str());
        } else {
            assert_eq!(results.len(), 1, "{policy:?}");
            assert!(results[0].1, "{policy:?}");
            assert!(!root.join("tasks").exists(), "{policy:?}");
        }
        assert_eq!(base.manager.list().unwrap().len(), 2);
        base.manager.shutdown().await.unwrap();
        if path.exists() {
            let record: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
            assert_eq!(record["status"], "in_progress");
        }
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn task_tool_interrupt_keeps_actor_responsive_and_rejects_stale_effects() {
    use crate::core::lifecycle::TaskGateOutcome;
    use crate::hooks::Hook;
    use std::sync::atomic::{AtomicUsize, Ordering};
    for boundary in ["permission", "hook"] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-task-interrupt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let mut phi = base.phi.clone();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        if boundary == "hook" {
            phi.hooks.register_hook(Hook::task_created({
                let entered = entered.clone();
                let release = release.clone();
                move |_| {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        Ok(TaskGateOutcome::Allow)
                    }
                }
            }));
        }
        phi = phi.with_runtime_services(
            base.phi
                .runtime_services
                .clone()
                .permission_policy(
                    crate::permission::PermissionConfig::try_from_value(
                        json!({"mode": if boundary == "permission" {"ask"} else {"allow"}}),
                    )
                    .unwrap(),
                    Some(root.clone()),
                )
                .interactive(true),
        );
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .task_list(base.task_list().unwrap().clone())
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let round = Arc::new(AtomicUsize::new(0));
        let count = round.clone();
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, requests, move |_| {
            let first = count.fetch_add(1, Ordering::SeqCst) == 0;
            let delta = if first {
                json!({"tool_calls": [{"index": 0, "id": "blocked_task", "type": "function", "function": {"name": "Task", "arguments": json!({"action": "create", "subject": "Cancelled work", "description": "Must not publish"}).to_string()}}]})
            } else {
                json!({"content": "Replacement is responsive"})
            };
            json!({"id": "completion", "model": "test", "choices": [{"delta": delta, "finish_reason": if first {"tool_calls"} else {"stop"}}]})
        }));
        instance
            .submit(TurnPrompt {
                prompt: "Record work".into(),
                source: UserPromptSubmitSource::Interactive,
                tool_policy: Default::default(),
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let interaction = tokio::time::timeout(Duration::from_secs(5), async {
            if boundary == "hook" {
                entered.notified().await;
                None
            } else {
                loop {
                    if let Some(interaction) = instance.snapshot().pending_interactions.first() {
                        break Some(interaction.clone());
                    }
                    tokio::task::yield_now().await;
                }
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), instance.abort())
            .await
            .unwrap()
            .unwrap();
        if let Some(interaction) = interaction {
            assert_eq!(
                interaction.scope.parent_tool_call_id.as_deref(),
                Some("blocked_task")
            );
            assert!(
                instance
                    .respond_interaction(
                        interaction.request_id,
                        crate::core::interaction::InteractionResponse::Permission(
                            crate::core::interaction::PermissionDecision::AllowOnce
                        )
                    )
                    .await
                    .is_err()
            );
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !instance.snapshot().pending_interactions.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let transcript = instance.transcript().await.unwrap();
        let replacement = phi
            .agent_instance_builder()
            .transcript(transcript)
            .task_list(base.task_list().unwrap().clone())
            .working_dir(root.clone())
            .build()
            .await
            .unwrap()
            .into_runtime_candidate();
        tokio::time::timeout(
            Duration::from_secs(5),
            instance.replace_runtime(replacement),
        )
        .await
        .unwrap()
        .unwrap();
        release.notify_one();
        instance
            .submit(TurnPrompt {
                prompt: "Continue without old work".into(),
                source: UserPromptSubmitSource::Interactive,
                tool_policy: Default::default(),
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while instance.snapshot().status == crate::agent_instance::AgentInstanceStatus::Running
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(received.try_recv().is_ok());
        assert_eq!(round.load(Ordering::SeqCst), 2, "{boundary}");
        assert!(!root.join("tasks/test-list/1.json").exists(), "{boundary}");
        assert!(instance.snapshot().pending_interactions.is_empty());
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}
