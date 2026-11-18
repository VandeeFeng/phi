use super::*;
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::harness::tests::support::{
    instance_for_provider, mock_provider, mock_provider_with_reply, system_text,
};
use serde_json::{Value, json};
use tokio::net::TcpListener;

#[tokio::test]
async fn system_prompt_waits_for_session_start_readiness() {
    use crate::hooks::{Hook, SessionStartMode};
    use crate::lifecycle::{BuildSystemPromptOutcome, SessionStartReason};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    for mode in [
        SessionStartMode::Blocking,
        SessionStartMode::BeforeFirstPrompt,
    ] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-startup-prompt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, mut received) = mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(
            listener,
            requests,
            |_| json!({"id":"completion", "model":"test", "choices":[{"delta":{"content":"done"}, "finish_reason":"stop"}]}),
        ));
        let base = instance_for_provider(&root, address, false).await;
        let initialized = Arc::new(AtomicBool::new(false));
        let builds = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let mut phi = base.phi.clone();
        phi.hooks
            .register_hook(Hook::session_start_with_mode(mode, {
                let initialized = initialized.clone();
                let release = release.clone();
                move |_| {
                    let initialized = initialized.clone();
                    let release = release.clone();
                    async move {
                        if mode == SessionStartMode::BeforeFirstPrompt {
                            release.notified().await;
                        }
                        initialized.store(true, Ordering::SeqCst);
                        Ok(Vec::new())
                    }
                }
            }));
        phi.hooks.register_hook(Hook::build_system_prompt({
            let initialized = initialized.clone();
            let builds = builds.clone();
            move |event| {
                builds.fetch_add(1, Ordering::SeqCst);
                let initialized = initialized.load(Ordering::SeqCst);
                async move {
                    Ok(BuildSystemPromptOutcome {
                        system_prompt: format!(
                            "{}\nstartup-ready={initialized}",
                            event.system_prompt
                        ),
                    })
                }
            }
        }));
        let candidate = phi
            .agent_instance_builder()
            .working_dir(root.clone())
            .build()
            .await
            .unwrap();
        let resources = candidate.resources();
        let instance = base.manager.insert(candidate).unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        resources
            .session_started(SessionStartReason::Startup, None)
            .await;
        assert_eq!(builds.load(Ordering::SeqCst), 0);
        if mode == SessionStartMode::BeforeFirstPrompt {
            let mut readiness = Box::pin(resources.ready());
            assert!(futures::poll!(readiness.as_mut()).is_pending());
            assert_eq!(builds.load(Ordering::SeqCst), 0);
            drop(readiness);
            release.notify_one();
        }
        resources.ready().await;
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        instance
            .submit(TurnPrompt {
                prompt: "input".into(),
                source: UserPromptSubmitSource::Sdk,
                tool_policy: Default::default(),
                images: Vec::new(),
                injected_context: Vec::new(),
            })
            .await
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(system_text(&request).contains("startup-ready=true"));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        instance.stop().await.unwrap();
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn system_prompt_once_waits_for_execution_and_survives_replacement() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root =
        std::path::Path::new("/tmp").join(format!("phi-system-hook-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, mut received) = mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(
        listener,
        requests,
        |_| json!({"id":"completion", "model":"test", "choices":[{"delta":{"content":"done"}, "finish_reason":"stop"}]}),
    ));
    let base = instance_for_provider(&root, address, false).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let mut phi = base.phi.clone();
    phi.hooks.register_hook(
        crate::hooks::Hook::build_system_prompt({
            let calls = calls.clone();
            move |event| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(crate::core::lifecycle::BuildSystemPromptOutcome {
                        system_prompt: format!("{}\\nhook-marker", event.system_prompt),
                    })
                }
            }
        })
        .with_once(true),
    );
    let candidate = phi
        .agent_instance_builder()
        .working_dir(root.clone())
        .build()
        .await
        .unwrap();
    let discarded = phi
        .agent_instance_builder()
        .working_dir(root.clone())
        .build()
        .await
        .unwrap();
    drop(discarded);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let instance = base.manager.insert(candidate).unwrap();
    let input = || TurnPrompt {
        prompt: "input".into(),
        source: UserPromptSubmitSource::Sdk,
        tool_policy: Default::default(),
        images: Vec::new(),
        injected_context: Vec::new(),
    };
    for step in ["first", "same-generation", "replacement"] {
        if step == "replacement" {
            let unrelated = phi
                .agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap();
            assert!(
                instance
                    .replace_runtime(unrelated.into_runtime_candidate())
                    .await
                    .is_err()
            );
            let next = phi
                .agent_instance_builder()
                .working_dir(root.clone())
                .transcript(instance.transcript().await.unwrap())
                .build()
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            instance
                .replace_runtime(next.into_runtime_candidate())
                .await
                .unwrap();
        }
        instance.submit(input()).await.unwrap();
        let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            system_text(&request).contains("hook-marker"),
            step != "replacement"
        );
        tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    let independent = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    independent.submit(input()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        independent.wait_for_quiescence(None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    instance.stop().await.unwrap();
    independent.stop().await.unwrap();
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn slow_hook_preserves_queue_order() {
    use crate::core::lifecycle::PromptContextOutcome;
    use crate::hooks::Hook;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for dispatch in [SubmissionDispatch::FollowUp, SubmissionDispatch::Steer] {
        let root = std::env::temp_dir().join(format!("phi-hook-order-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, mut received) = mpsc::unbounded_channel();
        let provider_release = Arc::new(tokio::sync::Notify::new());
        let server = tokio::spawn(mock_provider_with_reply(listener, requests, {
            let release = provider_release.clone();
            let rounds = AtomicUsize::new(0);
            move |_| {
                let first = rounds.fetch_add(1, Ordering::SeqCst) == 0;
                let release = release.clone();
                async move {
                    if first {
                        release.notified().await;
                    }
                    json!({"id": "completion", "model": "test", "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}]})
                }
            }
        }));
        let base = instance_for_provider(&root, address, false).await;
        let entered = Arc::new(tokio::sync::Notify::new());
        let hook_release = Arc::new(tokio::sync::Notify::new());
        let mut phi = base.phi.clone();
        phi.hooks.register_hook(Hook::prompt_context({
            let entered = entered.clone();
            let release = hook_release.clone();
            move |event| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    if event.prompt == "second" {
                        entered.notify_one();
                        release.notified().await;
                    }
                    Ok(PromptContextOutcome::default())
                }
            }
        }));
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let input = |text: &str| TurnPrompt {
            prompt: text.into(),
            source: UserPromptSubmitSource::Sdk,
            tool_policy: Default::default(),
            images: Vec::new(),
            injected_context: Vec::new(),
        };
        let timeout = Duration::from_secs(5);
        instance.submit(input("initial")).await.unwrap();
        tokio::time::timeout(timeout, received.recv())
            .await
            .unwrap()
            .unwrap();
        instance
            .dispatch(input("first"), SubmissionDispatch::FollowUp)
            .await
            .unwrap();
        let second = tokio::spawn({
            let instance = instance.clone();
            let prompt = input("second");
            async move { instance.dispatch(prompt, dispatch).await }
        });
        tokio::time::timeout(timeout, entered.notified())
            .await
            .unwrap();
        let mut events = instance.subscribe();
        provider_release.notify_one();
        tokio::time::timeout(timeout, async {
            loop {
                if let AgentInstanceEvent::Phi(envelope) = events.recv().await.unwrap()
                    && matches!(envelope.event, PhiEvent::TurnFinished { .. })
                {
                    break;
                }
            }
        })
        .await
        .unwrap();
        hook_release.notify_one();
        let disposition = tokio::time::timeout(timeout, second)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            matches!(disposition, SubmissionDisposition::Queued { dispatch: actual, .. } if actual == dispatch)
        );
        let order = match dispatch {
            SubmissionDispatch::FollowUp => ["first", "second"],
            SubmissionDispatch::Steer => ["second", "first"],
        };
        for expected in order {
            let request = tokio::time::timeout(timeout, received.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                request["messages"].as_array().unwrap().last().unwrap()["content"],
                expected
            );
        }
        let view = tokio::time::timeout(timeout, instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        assert!(view.pending_submission_ids.is_empty());
        let users = view
            .transcript
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                crate::transcript::TranscriptEntryKind::Message {
                    message: crate::transcript::AgentMessage::User { content },
                } => match content.first() {
                    Some(crate::transcript::UserContentBlock::Text { text }) => {
                        Some(text.to_string())
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(users, ["initial", order[0], order[1]]);
        instance.stop().await.unwrap();
        base.stop().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn outer_turn_policy_survives_inner_rounds_and_retires_before_follow_up() {
    use crate::core::turn::TurnToolPolicy;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = std::env::temp_dir().join(format!("phi-outer-policy-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("scope.txt"), "scoped-read-success").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let paused = release.clone();
    let round = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(mock_provider_with_reply(listener, requests, move |_| {
        let index = round.fetch_add(1, Ordering::SeqCst);
        let paused = paused.clone();
        async move {
            if index == 0 {
                paused.notified().await;
            }
            let delta = match index {
                0 | 2 | 4 => {
                    json!({"tool_calls": [{"index": 0, "id": format!("read_{index}"), "type": "function",
                    "function": {"name": "Read", "arguments": json!({"path": "scope.txt"}).to_string()}}]})
                }
                1 => json!({"tool_calls": [{"index": 0, "id": "removed_bash", "type": "function",
                    "function": {"name": "Bash", "arguments": json!({"command": "printf bypass"}).to_string()}}]}),
                _ => json!({"content": "done"}),
            };
            json!({"id": "completion", "model": "test", "choices": [{"delta": delta,
                "finish_reason": if index == 0 || index == 1 || index == 2 || index == 4 { "tool_calls" } else { "stop" }}]})
        }
    }));
    let base = instance_for_provider(&root, address, false).await;
    let phi = base.phi.clone().with_runtime_services(
        base.phi.runtime_services.clone().permission_policy(
            crate::permission::PermissionConfig::default(),
            Some(root.clone()),
        ),
    );
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let input = |prompt: &str, policy| TurnPrompt {
        prompt: prompt.into(),
        source: UserPromptSubmitSource::Sdk,
        injected_context: Vec::new(),
        images: Vec::new(),
        tool_policy: policy,
    };
    instance
        .submit(input(
            "initial",
            TurnToolPolicy {
                allowances: vec![("Read".into(), "**".into())],
                removals: vec!["Write".into()],
            },
        ))
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_request_tool(&first, "Read", true);
    assert_request_tool(&first, "Write", false);
    instance
        .dispatch(
            input(
                "steer",
                TurnToolPolicy {
                    allowances: vec![("Bash".into(), "**".into())],
                    removals: vec!["Bash".into()],
                },
            ),
            SubmissionDispatch::Steer,
        )
        .await
        .unwrap();
    instance
        .dispatch(
            input(
                "follow-up",
                TurnToolPolicy {
                    removals: vec!["Edit".into()],
                    ..Default::default()
                },
            ),
            SubmissionDispatch::FollowUp,
        )
        .await
        .unwrap();
    release.notify_one();
    for index in 1..=5 {
        let request = tokio::time::timeout(Duration::from_secs(10), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_request_tool(&request, "Bash", index >= 4);
        assert_request_tool(&request, "Write", index >= 4);
        assert_request_tool(&request, "Edit", index < 4);
        if matches!(index, 1 | 3) {
            let read_id = format!("read_{}", index - 1);
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["tool_call_id"] == read_id
                            && message["content"]
                                .as_str()
                                .unwrap_or_default()
                                .contains("scoped-read-success")
                    })
            );
        }
        if index == 2 {
            assert!(request.to_string().contains("not allowed for this agent"));
        }
        if index == 5 {
            assert!(
                request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| {
                        message["tool_call_id"] == "read_4"
                            && message["content"] == "the tool failed"
                    })
            );
        }
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        while instance.snapshot().status != crate::agent_instance::AgentInstanceStatus::Idle {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    instance
        .submit(input("later", TurnToolPolicy::default()))
        .await
        .unwrap();
    let later = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    for name in ["Bash", "Write", "Edit", "Read"] {
        assert_request_tool(&later, name, true);
    }
    instance.stop().await.unwrap();
    base.stop().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn failed_outer_turn_hands_unconsumed_steer_to_a_fresh_policy_scope() {
    use crate::core::turn::TurnToolPolicy;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = std::env::temp_dir().join(format!("phi-policy-handoff-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("scope.txt"), "must-not-inherit-approval").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let paused = release.clone();
    let round = Arc::new(AtomicUsize::new(0));
    let server = tokio::spawn(mock_provider_with_reply(listener, requests, move |_| {
        let index = round.fetch_add(1, Ordering::SeqCst);
        let paused = paused.clone();
        async move {
            if index == 0 {
                paused.notified().await;
                return json!({"error": {"message": "fixture failure", "type": "authentication_error"}});
            }
            let delta = if index == 1 {
                json!({"tool_calls": [{"index": 0, "id": "handoff_read", "type": "function",
                    "function": {"name": "Read", "arguments": json!({"path": "scope.txt"}).to_string()}}]})
            } else {
                json!({"content": "done"})
            };
            json!({"id": "completion", "model": "test", "choices": [{"delta": delta,
                "finish_reason": if index == 1 { "tool_calls" } else { "stop" }}]})
        }
    }));
    let base = instance_for_provider(&root, address, false).await;
    let phi = base.phi.clone().with_runtime_services(
        base.phi.runtime_services.clone().permission_policy(
            crate::permission::PermissionConfig::default(),
            Some(root.clone()),
        ),
    );
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let input = |prompt: &str, policy| TurnPrompt {
        prompt: prompt.into(),
        source: UserPromptSubmitSource::Sdk,
        injected_context: Vec::new(),
        images: Vec::new(),
        tool_policy: policy,
    };
    instance
        .submit(input(
            "failing",
            TurnToolPolicy {
                allowances: vec![("Read".into(), "**".into())],
                removals: vec!["Write".into()],
            },
        ))
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_request_tool(&first, "Write", false);
    instance
        .dispatch(
            input(
                "pending steer",
                TurnToolPolicy {
                    removals: vec!["Edit".into()],
                    ..Default::default()
                },
            ),
            SubmissionDispatch::Steer,
        )
        .await
        .unwrap();
    release.notify_one();
    let handoff = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_request_tool(&handoff, "Write", true);
    assert_request_tool(&handoff, "Edit", false);
    let result = tokio::time::timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(
        result["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| {
                message["tool_call_id"] == "handoff_read" && message["content"] == "the tool failed"
            })
    );
    instance.stop().await.unwrap();
    base.stop().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn tape_finalization_and_memory_queries_use_instance_scope() {
    use crate::extension::memory::{MemoryService, TapeConfig};
    use crate::transcript::{AgentMessage, TranscriptEntryKind};

    let root =
        std::path::PathBuf::from("/tmp").join(format!("phi-tape-flow-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let mut phi = base.phi.clone().with_runtime_services(
        base.phi.runtime_services.clone().permission_policy(
            PermissionConfig {
                mode: crate::permission::PermissionMode::Allow,
                ..Default::default()
            },
            Some(root.clone()),
        ),
    );
    phi.cfg.extensions.memory.as_mut().unwrap().tape = Some(TapeConfig {
        only_git: Some(false),
        ..Default::default()
    });
    let memory = MemoryService::create(
        phi.cfg.extensions.memory.as_ref().unwrap(),
        "observer",
        &root,
    )
    .unwrap();
    let core = memory.memory_project_dir().join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("decision.md"),
        "---\ndescription: needle decision\n---\nneedle context",
    )
    .unwrap();
    let instance = base
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
    let server = tokio::spawn(mock_provider(listener, requests, |request| {
        let round = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .count();
        let call = match round {
            0 => Some(("tape_anchor", json!({"name": ""}))),
            1 => Some(("tape_anchor", json!({"name": "task/checkpoint"}))),
            2 => Some((
                "memory_search",
                json!({"mode": "tape", "query": "task/checkpoint", "includeRelations": true, "include_orphans": true}),
            )),
            3 => Some((
                "memory_search",
                json!({"mode": "default", "query": "needle"}),
            )),
            _ => None,
        };
        let delta = match call {
            Some((name, arguments)) => {
                json!({"tool_calls": [{"index": 0, "id": format!("call_{round}"), "type": "function", "function": {"name": name, "arguments": arguments.to_string()}}]})
            }
            None => json!({"content": "done"}),
        };
        json!({"id": "completion", "model": "test", "choices": [{"delta": delta, "finish_reason": if round < 4 { "tool_calls" } else { "stop" }}]})
    }));
    instance
        .submit(TurnPrompt {
            prompt: "record checkpoint".into(),
            source: UserPromptSubmitSource::Sdk,
            injected_context: Vec::new(),
            images: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(10), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.snapshot.status, AgentInstanceStatus::Idle);
    let first = received.recv().await.unwrap();
    assert_request_tool(&first, "tape_anchor", true);
    received.recv().await.unwrap();
    received.recv().await.unwrap();
    let request = received.recv().await.unwrap();
    let search = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "call_2")
        .unwrap();
    let result: Value = serde_json::from_str(search["content"].as_str().unwrap()).unwrap();
    assert_eq!(result["anchors"].as_array().unwrap().len(), 1);
    let anchor = &result["anchors"][0];
    assert_eq!(anchor["name"], "task/checkpoint");
    assert!(!result["graph"]["nodes"].as_array().unwrap().is_empty());
    let history = instance.transcript().await.unwrap();
    let entry = history.entries.iter().find(|entry| matches!(&entry.kind,
        TranscriptEntryKind::Message { message: AgentMessage::ToolResult { tool_call_id, is_error: false, .. } }
            if tool_call_id == "call_1")).unwrap();
    assert_eq!(anchor["sessionId"], history.id.as_str());
    assert_eq!(anchor["sessionEntryId"], entry.id.as_str());
    assert!(history.entries.iter().any(|entry| matches!(&entry.kind,
        TranscriptEntryKind::Message { message: AgentMessage::ToolResult { tool_call_id, is_error: true, .. } }
            if tool_call_id == "call_0")));
    let request = received.recv().await.unwrap();
    let search = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["tool_call_id"] == "call_3")
        .unwrap();
    let output = search["content"].as_str().unwrap();
    assert!(
        output.contains(&core.join("decision.md").display().to_string()),
        "{output}"
    );
    assert_eq!(memory.reconcile().unwrap().nodes.len(), 2);
    instance.stop().await.unwrap();
    base.stop().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

fn assert_request_tool(request: &Value, name: &str, available: bool) {
    assert_eq!(
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == name),
        available
    );
    let system = system_text(request);
    let summary = system
        .split_once("<available_tools>")
        .unwrap()
        .1
        .split_once("</available_tools>")
        .unwrap()
        .0;
    assert_eq!(
        summary
            .lines()
            .any(|line| line.starts_with(&format!("- {name}:"))),
        available
    );
}

fn failing_completion(request: &Value) -> Value {
    if request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool")
    {
        return json!({"error": {"message": "test authentication failed", "type": "authentication_error"}});
    }
    json!({"id": "completion", "model": request["model"], "choices": [{"delta": {
        "tool_calls": [{"index": 0, "id": "failed_call", "type": "function", "function": {
            "name": "Agent", "arguments": json!({"action": "create", "role": "missing", "prompt": "task"}).to_string()
        }}]}, "finish_reason": "tool_calls"}]})
}

#[tokio::test]
async fn delegated_failure_retains_canonical_tool_history() {
    let root = std::env::temp_dir().join(format!("phi-delegated-failure-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), true).await;
    let (requests, _received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, failing_completion));
    instance
        .start_delegated_agent("probe", "task", "external-parent")
        .await
        .unwrap();
    let (_cancel, cancellation) = tokio::sync::watch::channel(false);
    let completion = tokio::time::timeout(
        Duration::from_secs(10),
        instance.complete_delegated_agent(cancellation),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(completion.outcome.is_err());
    let history = instance.transcript().await.unwrap();
    assert_eq!(history.id, instance.id().as_str());
    let repeated = instance
        .complete_delegated_agent(tokio::sync::watch::channel(true).1)
        .await
        .unwrap();
    assert_eq!(repeated, completion);
    assert!(history.entries.iter().any(|entry| matches!(&entry.kind,
        crate::transcript::TranscriptEntryKind::Message {
            message: crate::transcript::AgentMessage::ToolResult { tool_call_id, is_error: true, .. }
        } if tool_call_id == "failed_call")));
    assert_eq!(
        instance.snapshot().status,
        crate::agent_instance::AgentInstanceStatus::Failed
    );
    assert_eq!(instance.snapshot().definition.as_deref(), Some("probe"));
    instance.stop().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}
