use super::*;
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::harness::instance::execution::PhiTurnError;
use crate::harness::tests::support::{instance_for_provider, read_request};
use crate::transcript::{AgentMessage, TranscriptEntryKind};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn prompt_preparation_keeps_controls_and_execution_responsive() {
    use crate::core::lifecycle::PromptContextOutcome;
    use crate::hooks::Hook;

    for dispatch in [
        None,
        Some(SubmissionDispatch::Steer),
        Some(SubmissionDispatch::FollowUp),
    ] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let hook = Hook::prompt_context({
            let entered = entered.clone();
            let release = release.clone();
            move |_| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(PromptContextOutcome::default())
                }
            }
        });
        let phi = Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .no_tools(true)
            .hook(hook)
            .build()
            .unwrap()
            .with_runtime_services(PhiRuntimeServices::new().model_catalog(test_catalog()));
        let mut candidate = phi.agent_instance_builder().build().await.unwrap();
        let events = dispatch.map(|_| candidate.install_test_main_run("active-turn"));
        let instance = AgentInstance::spawn(candidate);
        let submitting = tokio::spawn({
            let instance = instance.clone();
            async move {
                let prompt = TurnPrompt {
                    prompt: "must not commit after abort".into(),
                    source: UserPromptSubmitSource::Sdk,
                    images: Vec::new(),
                    injected_context: Vec::new(),
                    tool_policy: Default::default(),
                };
                match dispatch {
                    Some(dispatch) => instance.dispatch(prompt, dispatch).await,
                    None => instance.submit(prompt).await,
                }
            }
        });
        let timeout = Duration::from_secs(1);
        tokio::time::timeout(timeout, entered.notified())
            .await
            .unwrap();
        let mut observer = instance.subscribe();
        if let Some(events) = &events {
            events
                .send(AgentEvent::AssistantStarted {
                    id: "partial".into(),
                })
                .await
                .unwrap();
            events
                .send(AgentEvent::AssistantDelta {
                    original: "still progressing".into(),
                    display: "still progressing".into(),
                })
                .await
                .unwrap();
        }
        let controls = tokio::time::timeout(timeout, async {
            let view = instance.view().await.unwrap();
            assert!(matches!(
                instance.reset_context(None).await,
                Err(AgentInstanceError::Busy)
            ));
            assert!(view.pending_submission_ids.is_empty());
            if events.is_some() {
                tokio::time::timeout(timeout, async {
                    loop {
                        if let AgentInstanceEvent::Phi(envelope) = observer.recv().await.unwrap()
                            && matches!(envelope.event, PhiEvent::AssistantDelta { .. })
                        {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            }
            instance.abort().await.unwrap();
        })
        .await;
        release.notify_one();
        let result = submitting.await.unwrap();
        let transcript = instance.stop().await.unwrap();
        controls.expect("prompt preparation blocked actor controls or execution");
        assert!(matches!(
            result,
            Err(AgentInstanceError::Turn(PhiTurnError::Cancelled))
        ));
        assert!(
            !serde_json::to_string(&transcript)
                .unwrap()
                .contains("must not commit after abort")
        );
    }
}

#[tokio::test]
async fn preparation_preserves_input_order_and_stop_cancels_waits() {
    use crate::core::lifecycle::UserPromptSubmitOutcome;
    use crate::hooks::Hook;

    let (entered, mut preparing) = mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .no_tools(true)
        .hook(Hook::user_prompt_submit({
            let release = release.clone();
            move |event| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.send(event.prompt).unwrap();
                    release.notified().await;
                    Ok(UserPromptSubmitOutcome::Handled { reason: None })
                }
            }
        }))
        .build()
        .unwrap()
        .with_runtime_services(PhiRuntimeServices::new().model_catalog(test_catalog()));
    let instance = AgentInstance::spawn(phi.agent_instance_builder().build().await.unwrap());
    let timeout = Duration::from_secs(1);
    let mut waiters = Vec::new();
    for prompt in ["first", "second"] {
        let mut waiter = Box::pin(instance.submit(TurnPrompt {
            prompt: prompt.into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        }));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        waiters.push(waiter);
        tokio::time::timeout(timeout, instance.view())
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(preparing.recv().await.unwrap(), "first");
    assert!(preparing.try_recv().is_err());
    drop(waiters.remove(0));
    release.notify_one();
    assert_eq!(
        tokio::time::timeout(timeout, preparing.recv())
            .await
            .unwrap()
            .unwrap(),
        "second"
    );
    let transcript = tokio::time::timeout(timeout, instance.stop())
        .await
        .unwrap()
        .unwrap();
    release.notify_one();
    assert!(matches!(
        waiters.remove(0).await,
        Err(AgentInstanceError::Turn(PhiTurnError::Cancelled))
    ));
    assert!(!transcript.has_user_message());
    assert!(preparing.try_recv().is_err());
}

#[tokio::test]
async fn abort_publishes_partial_message_completion_before_cancellation() {
    let (instance, events) = test_running_instance().await;
    let mut observer = instance.subscribe();
    for event in [
        AgentEvent::AssistantStarted {
            id: "partial".into(),
        },
        AgentEvent::Reasoning("partial thinking".into()),
        AgentEvent::AssistantDelta {
            original: "partial output".into(),
            display: "partial output".into(),
        },
    ] {
        events.send(event).await.unwrap();
    }
    instance.abort().await.unwrap();
    let mut received = Vec::new();
    while let Ok(event) = observer.try_recv() {
        let cancelled = matches!(event, AgentInstanceEvent::RunCancelled { .. });
        received.push(event);
        if cancelled {
            break;
        }
    }
    assert!(matches!(received.as_slice(), [
        AgentInstanceEvent::Phi(start),
        AgentInstanceEvent::Phi(reasoning),
        AgentInstanceEvent::Phi(delta),
        AgentInstanceEvent::Phi(reasoning_end),
        AgentInstanceEvent::Phi(message_end),
        AgentInstanceEvent::Progress(message_progress),
        AgentInstanceEvent::Progress(terminal_progress),
        AgentInstanceEvent::RunCancelled { turn_id: Some(turn_id), reason: crate::CancellationReason::Interrupted },
    ] if turn_id == "active-turn" && matches!(&start.event, PhiEvent::AssistantStarted { id } if id == "partial")
        && matches!(&reasoning.event, PhiEvent::ReasoningDelta { text } if text == "partial thinking")
        && matches!(&delta.event, PhiEvent::AssistantDelta { original, .. } if original == "partial output")
        && matches!(&reasoning_end.event, PhiEvent::ReasoningFinished { .. })
        && matches!(&message_end.event, PhiEvent::AssistantFinished { id, response, .. } if id == "partial" && response == "partial output")
        && message_progress.status == crate::agent_instance::ExecutionStatus::Running
        && terminal_progress.status == crate::agent_instance::ExecutionStatus::Cancelled
        && terminal_progress.cancellation_reason == Some(crate::CancellationReason::Interrupted)
        && terminal_progress.usage.output_tokens == 0
    ));
    let transcript = instance.transcript().await.unwrap();
    assert_eq!(transcript.entries.len(), 1);
    assert_eq!(instance.snapshot().status, AgentInstanceStatus::Idle);
    assert!(
        events
            .send(AgentEvent::RunFinished {
                response: "late".into(),
                usage: crate::transcript::Usage::default(),
            })
            .await
            .is_err()
    );
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn abort_discards_only_uncommitted_submission_ids() {
    let (instance, events) = test_running_instance().await;
    let mut observer = instance.subscribe();
    let mut accepted = Vec::new();
    for dispatch in [
        SubmissionDispatch::Steer,
        SubmissionDispatch::Steer,
        SubmissionDispatch::FollowUp,
    ] {
        let SubmissionDisposition::Queued { submission_id, .. } = instance
            .dispatch(
                TurnPrompt {
                    prompt: "same prompt".into(),
                    source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                    tool_policy: Default::default(),
                    injected_context: Vec::new(),
                    images: Vec::new(),
                },
                dispatch,
            )
            .await
            .unwrap()
        else {
            panic!("submission was not queued")
        };
        accepted.push(submission_id);
    }
    events
        .send(AgentEvent::UserInputInjected {
            submission_id: Some(accepted[0].clone()),
            prompt: "same prompt".into(),
            source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
            contexts: Vec::new(),
            content: vec![crate::transcript::UserContentBlock::Text {
                text: "same prompt".into(),
            }],
        })
        .await
        .unwrap();
    events
        .send(AgentEvent::SubmissionDiscarded {
            submission_id: accepted[1].clone(),
        })
        .await
        .unwrap();
    instance.abort().await.unwrap();
    instance.abort().await.unwrap();
    let mut committed = Vec::new();
    let mut discarded = Vec::new();
    let mut cancellations = Vec::new();
    while let Ok(event) = observer.try_recv() {
        if let AgentInstanceEvent::RunCancelled { turn_id, .. } = &event {
            cancellations.push(turn_id.clone());
        }
        if let AgentInstanceEvent::Phi(envelope) = event {
            match envelope.event {
                PhiEvent::UserInputInjected {
                    submission_id: Some(id),
                    ..
                } => committed.push(id),
                PhiEvent::SubmissionDiscarded {
                    submission_id,
                    reason,
                } => {
                    let expected = if submission_id == accepted[1] {
                        crate::core::turn::SubmissionDiscardReason::DispatchFailed
                    } else {
                        assert_eq!(envelope.turn_id, None);
                        crate::core::turn::SubmissionDiscardReason::Cancelled
                    };
                    assert_eq!(reason, expected);
                    discarded.push(submission_id);
                }
                _ => {}
            }
        }
    }
    let view = instance.view().await.unwrap();
    instance.stop().await.unwrap();
    assert_eq!(cancellations, vec![Some("active-turn".into()), None]);
    assert_eq!(committed, accepted[..1]);
    assert_eq!(discarded, accepted[1..]);
    assert!(view.pending_submission_ids.is_empty());
    assert_eq!(view.transcript.entries.len(), 1);
}

#[tokio::test]
async fn stream_preserves_terminal_event_before_late_interrupt() {
    for terminal in [
        AgentEvent::RunFinished {
            response: "done".into(),
            usage: Default::default(),
        },
        AgentEvent::Error(crate::PhiFailure::new(
            crate::PhiFailureKind::Internal,
            "provider failed",
            None,
        )),
    ] {
        let (instance, events) = test_running_instance().await;
        let mut stream = crate::harness::instance::handle::PhiStream::new(
            instance.clone(),
            instance.subscribe(),
            crate::harness::manager::AgentManager::new(),
        );
        let successful = matches!(terminal, AgentEvent::RunFinished { .. });
        events.send(terminal).await.unwrap();
        drop(events);
        let envelope = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let envelope = stream.next().await.unwrap().unwrap();
                if matches!(
                    envelope.event,
                    PhiEvent::TurnFinished { .. } | PhiEvent::TurnFailed { .. }
                ) {
                    break envelope;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(envelope.turn_id.as_deref(), Some("active-turn"));
        match envelope.event {
            PhiEvent::TurnFinished { response, .. } => {
                assert!(successful);
                assert_eq!(response, "done");
            }
            PhiEvent::TurnFailed { failure } => {
                assert!(!successful);
                assert_eq!(failure.message, "provider failed");
            }
            _ => unreachable!(),
        }
        stream.interrupt().await.unwrap();
        assert!(stream.next().await.unwrap().is_none());
        instance.stop().await.unwrap();
    }
}

#[tokio::test]
async fn stream_interrupt_terminates_after_partial_commit() {
    let (instance, events) = test_running_instance().await;
    let mut stream = crate::harness::instance::handle::PhiStream::new(
        instance.clone(),
        instance.subscribe(),
        crate::harness::manager::AgentManager::new(),
    );
    for event in [
        AgentEvent::AssistantStarted {
            id: "partial".into(),
        },
        AgentEvent::AssistantDelta {
            original: "partial output".into(),
            display: "partial output".into(),
        },
    ] {
        events.send(event).await.unwrap();
    }
    stream.interrupt().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        let mut completed_messages = 0;
        loop {
            match stream.next().await {
                Ok(Some(envelope)) => match envelope.event {
                    PhiEvent::AssistantFinished { response, .. } => {
                        assert_eq!(response, "partial output");
                        assert_eq!(envelope.turn_id.as_deref(), Some("active-turn"));
                        completed_messages += 1;
                    }
                    PhiEvent::TurnFinished { .. } => panic!("cancellation became success"),
                    _ => {}
                },
                Err(error) => break (completed_messages, error),
                Ok(None) => panic!("cancellation lost its error"),
            }
        }
    })
    .await;
    let transcript = instance.stop().await.unwrap();
    let (completed_messages, error) = result.expect("cancelled stream did not terminate");
    assert!(matches!(error, PhiStreamError::Cancelled));
    assert_eq!(completed_messages, 1);
    assert_eq!(transcript.entries.len(), 1);
    assert!(stream.next().await.unwrap().is_none());
}

#[tokio::test]
async fn interrupt_preserves_unterminated_stream_text_without_retry() {
    const HTTP_OK: &str = "HTTP/1.1 200 OK";
    let root =
        std::path::Path::new("/tmp").join(format!("phi-stream-interrupt-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let chunk = json!({"id":"partial","model":"test","choices":[{"delta":{"content":"unfinished line"},"finish_reason":null}]});
        socket.write_all(format!("{HTTP_OK}\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\n").as_bytes()).await.unwrap();
        std::future::pending::<()>().await;
    });
    let base = instance_for_provider(&root, address, false).await;
    let mut events = base.instance.subscribe();
    base.instance
        .submit(TurnPrompt {
            prompt: "Respond.".into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(events.recv().await.unwrap(), AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, crate::PhiEvent::AssistantDelta { ref original, .. } if original == "unfinished line")) { break; }
        }
    }).await.unwrap();
    tokio::time::timeout(
        Duration::from_millis(500),
        base.manager.interrupt(base.instance.id()),
    )
    .await
    .unwrap()
    .unwrap();
    let view = base.instance.view().await.unwrap();
    assert!(view.transcript.entries.iter().any(|entry| matches!(&entry.kind, TranscriptEntryKind::Message { message:AgentMessage::Assistant { content, stop_reason, .. } } if stop_reason == "aborted" && matches!(content.as_slice(), [crate::transcript::ContentBlock::Text { text }] if text == &"unfinished line"))));
    let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert!(
        observed
            .iter()
            .any(|event| matches!(event, AgentInstanceEvent::RunCancelled { .. }))
    );
    assert!(!observed.iter().any(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, crate::PhiEvent::TurnFinished { .. } | crate::PhiEvent::TurnFailed { .. }))));
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}
