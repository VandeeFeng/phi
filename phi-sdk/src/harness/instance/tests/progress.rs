use super::*;
use crate::agent_instance::ExecutionStatus;
use crate::harness::tests::support::{instance_for_provider, mock_provider};
use serde_json::json;
use tokio::net::TcpListener;

#[tokio::test]
async fn progress_recovers_usage_parallel_tools_and_terminal_state() {
    let (instance, events) = test_running_instance().await;
    let mut updates = instance.subscribe();
    let usage = crate::transcript::Usage {
        input: 10,
        output: 3,
        total_tokens: 13,
        cache_read: 4,
        cache_write: 2,
        cost: 0.5,
    };
    for id in ["first", "second"] {
        events
            .send(AgentEvent::ToolCall {
                id: id.into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "Read".into(),
                args: serde_json::json!({}),
            })
            .await
            .unwrap();
    }
    events
        .send(AgentEvent::AssistantFinished {
            id: "response".into(),
            provider_message_id: None,
            turn_id: "active-turn".into(),
            provider: "test".into(),
            api: None,
            model: "test".into(),
            content: Vec::new(),
            response: "".into(),
            display: "".into(),
            usage: usage.clone(),
            stop_reason: "tool_use".into(),
            has_tool_calls: true,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let AgentInstanceEvent::Progress(progress) = updates.recv().await.unwrap()
                && progress.usage.output_tokens == 3
            {
                assert_eq!(progress.turn_id, "active-turn");
                assert_eq!(progress.tool_uses, 2);
                assert_eq!(progress.current_tools.len(), 2);
                break;
            }
        }
    })
    .await
    .unwrap();
    let view = instance
        .view_with_event_boundary("progress".into())
        .await
        .unwrap();
    let progress = view.snapshot.progress.unwrap();
    assert_eq!(progress.status, ExecutionStatus::Running);
    assert_eq!(progress.usage, usage.clone().into());
    assert_eq!(progress.last_tool.as_ref().unwrap().id, "second");
    events
        .send(AgentEvent::ToolResult {
            id: "first".into(),
            entry_id: Transcript::new_entry_id(),
            output: "read failed".into(),
            is_error: true,
            details: None,
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let AgentInstanceEvent::Progress(progress) = updates.recv().await.unwrap()
                && progress.current_tools.len() == 1
            {
                assert_eq!(progress.current_tools[0].id, "second");
                assert_eq!(progress.last_tool.as_ref().unwrap().id, "second");
                assert_eq!(progress.tool_uses, 2);
                break;
            }
        }
    })
    .await
    .unwrap();
    events
        .send(AgentEvent::RunFinished {
            response: "done".into(),
            usage: usage.clone(),
        })
        .await
        .unwrap();
    drop(events);
    let view = tokio::time::timeout(Duration::from_secs(2), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    let progress = view.snapshot.progress.unwrap();
    assert_eq!(progress.status, ExecutionStatus::Completed);
    assert_eq!(progress.usage, usage.into());
    assert_eq!(progress.tool_uses, 2);
    assert!(progress.current_tools.is_empty());
    assert!(view.snapshot.live_run.is_none());
    instance.stop().await.unwrap();
    assert_eq!(instance.snapshot().progress.unwrap(), progress);
}

#[tokio::test]
async fn successive_turns_reset_confirmed_usage_even_with_reused_response_ids() {
    let root = std::env::temp_dir().join(format!("phi-progress-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("input.txt"), "progress fixture").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (requests, _received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, |request| {
        let tool_finished =
            request["messages"].as_array().unwrap().last().unwrap()["role"] == "tool";
        let (delta, reason) = if tool_finished {
            (json!({"content": "done"}), "stop")
        } else {
            (
                json!({"tool_calls": [{"index": 0, "id": "read-input", "type": "function",
                "function": {"name": "Read", "arguments": "{\"path\":\"input.txt\"}"}}]}),
                "tool_calls",
            )
        };
        json!({
            "id": "reused-response", "model": "test",
            "choices": [{"delta": delta, "finish_reason": reason}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
        })
    }));
    let mut previous_turn = None;
    for prompt in ["first", "next"] {
        let admitted = instance
            .submit(TurnPrompt {
                tool_policy: Default::default(),
                prompt: prompt.into(),
                source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
            })
            .await
            .unwrap();
        let SubmissionDisposition::Started { turn_id, .. } = admitted else {
            panic!("expected a fresh turn");
        };
        let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let progress = view.snapshot.progress.unwrap();
        assert_eq!(progress.turn_id, turn_id);
        assert_ne!(previous_turn.as_ref(), Some(&turn_id));
        assert_eq!(progress.status, ExecutionStatus::Completed);
        assert_eq!(progress.usage.input_tokens, 10);
        assert_eq!(progress.usage.output_tokens, 4);
        assert_eq!(progress.tool_uses, 1);
        previous_turn = Some(turn_id);
    }
    instance.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn duration_samples_and_freezes() {
    const SAMPLE_INTERVAL: Duration = Duration::from_millis(20);

    for (status, terminal_event) in [
        (
            ExecutionStatus::Completed,
            Some(AgentEvent::RunFinished {
                response: "done".into(),
                usage: Default::default(),
            }),
        ),
        (
            ExecutionStatus::Failed,
            Some(AgentEvent::Error(crate::PhiFailure::new(
                crate::PhiFailureKind::Internal,
                "test failure",
                None,
            ))),
        ),
        (ExecutionStatus::Cancelled, None),
    ] {
        let (instance, events) = test_running_instance().await;
        let initial = instance.view().await.unwrap().snapshot.progress.unwrap();
        assert_eq!(initial.status, ExecutionStatus::Running);

        tokio::time::sleep(SAMPLE_INTERVAL).await;
        let sampled = instance.view().await.unwrap().snapshot.progress.unwrap();
        assert_eq!(sampled.status, ExecutionStatus::Running);
        assert_eq!(sampled.turn_id, initial.turn_id);
        assert_eq!(sampled.started_at, initial.started_at);
        assert!(sampled.duration_ms > initial.duration_ms);

        events
            .send(AgentEvent::ToolCall {
                id: "running".into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "Bash".into(),
                args: serde_json::json!({}),
            })
            .await
            .unwrap();
        if let Some(event) = terminal_event {
            events.send(event).await.unwrap();
        } else {
            instance.abort().await.unwrap();
        }
        drop(events);
        let view = tokio::time::timeout(Duration::from_secs(2), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let terminal = view.snapshot.progress.unwrap();
        assert_eq!(terminal.status, status);
        assert_eq!(terminal.turn_id, sampled.turn_id);
        assert_eq!(terminal.started_at, sampled.started_at);
        assert!(terminal.duration_ms >= sampled.duration_ms);
        assert_eq!(terminal.tool_uses, 1);
        assert!(terminal.current_tools.is_empty());
        assert_eq!(terminal.last_tool.as_ref().unwrap().id, "running");
        assert_eq!(terminal.usage, crate::PhiUsage::default());

        tokio::time::sleep(SAMPLE_INTERVAL).await;
        assert_eq!(
            instance.view().await.unwrap().snapshot.progress.unwrap(),
            terminal
        );
        assert_eq!(instance.snapshot().progress.unwrap(), terminal);
        instance.stop().await.unwrap();
        assert_eq!(
            instance.view().await.unwrap().snapshot.progress.unwrap(),
            terminal
        );
    }
}
