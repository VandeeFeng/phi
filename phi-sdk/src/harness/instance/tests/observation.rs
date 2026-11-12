use super::*;

const QUIESCENCE_WAIT_LIMIT: usize = 64;

#[tokio::test]
async fn failure_history_survives_runtime_replacement() {
    let failures = [
        crate::PhiFailure::new(
            crate::PhiFailureKind::RateLimited,
            "provider throttled",
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS.as_u16()),
        ),
        crate::PhiFailure::cancelled(crate::CancellationReason::UserRequested, "cancelled"),
    ];
    for failure in failures {
        let (instance, events) = test_running_instance().await;
        let mut observer = instance.subscribe();
        events
            .send(AgentEvent::Error(failure.clone()))
            .await
            .unwrap();
        drop(events);
        wait_for_status(&instance, AgentInstanceStatus::Failed).await;
        let view = instance.view().await.unwrap();
        assert_eq!(view.snapshot.latest_error.as_ref(), Some(&failure));
        assert!(matches!(
            &view.transcript.entries.last().unwrap().kind,
            crate::transcript::TranscriptEntryKind::Failure { turn_id, failure: stored }
                if turn_id.as_deref() == Some("active-turn") && stored == &failure
        ));
        assert!(
            view.transcript
                .effective_context()
                .unwrap()
                .entries
                .iter()
                .all(|entry| {
                    !matches!(
                        entry.kind,
                        crate::transcript::TranscriptEntryKind::Failure { .. }
                    )
                })
        );
        assert!(view.snapshot.active_turn_id.is_none());
        assert!(view.snapshot.live_run.is_none());
        let observed = std::iter::from_fn(|| observer.try_recv().ok()).collect::<Vec<_>>();
        assert!(observed.iter().any(|event| matches!(
            event,
            AgentInstanceEvent::Phi(envelope)
                if matches!(&envelope.event, PhiEvent::TurnFailed { failure: actual } if actual == &failure)
        )));
        let replacement =
            test_prepare_agent_instance_with_transcript(Some(view.transcript), false).await;
        instance
            .replace_runtime(replacement.into_runtime_candidate())
            .await
            .unwrap();
        let recovered = instance.view().await.unwrap();
        assert_eq!(recovered.snapshot.status, AgentInstanceStatus::Idle);
        assert!(recovered.snapshot.latest_error.is_none());
        assert!(matches!(
            &recovered.transcript.entries.last().unwrap().kind,
            crate::transcript::TranscriptEntryKind::Failure { failure: stored, .. } if stored == &failure
        ));
        instance.stop().await.unwrap();
    }
}

#[tokio::test]
async fn retained_snapshot_does_not_block_stop() {
    let instance = test_instance().await;
    let snapshots = instance.subscribe_snapshots();
    let (ready_tx, ready_rx) = oneshot::channel();
    let (stopped_tx, stopped_rx) = std::sync::mpsc::channel();
    let observer = std::thread::spawn(move || {
        let retained = snapshots.snapshot();
        let status = retained.status;
        ready_tx.send(()).unwrap();
        let stopped_while_retained = stopped_rx.recv_timeout(Duration::from_secs(1)).is_ok();
        assert_eq!(retained.status, status);
        stopped_while_retained
    });
    ready_rx.await.unwrap();
    instance.stop().await.unwrap();
    let _ = stopped_tx.send(());
    assert!(observer.join().unwrap(), "snapshot retention blocked Stop");
    assert_eq!(instance.snapshot().status, AgentInstanceStatus::Stopped);
}

#[tokio::test]
async fn quiescence_waits_for_preparing_descendants() {
    use crate::core::lifecycle::UserPromptSubmitOutcome;
    use crate::harness::manager::AgentManager;
    use crate::hooks::Hook;

    let manager = AgentManager::new();
    let root = manager.insert(test_client().await).unwrap();
    let candidate = test_client().await;
    let child = manager
        .lock_live()
        .unwrap()
        .register(candidate, Some(root.id()))
        .unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .no_tools(true)
        .hooks(vec![Hook::user_prompt_submit({
            let entered = entered.clone();
            let release = release.clone();
            move |_| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(UserPromptSubmitOutcome::Handled { reason: None })
                }
            }
        })])
        .build()
        .unwrap()
        .with_runtime_services(PhiRuntimeServices::new().model_catalog(test_catalog()));
    let candidate = phi.agent_instance_builder().build().await.unwrap();
    let leaf = manager
        .lock_live()
        .unwrap()
        .register(candidate, Some(child.id()))
        .unwrap();
    root.wait_for_quiescence(None).await.unwrap();
    let transcript = leaf.transcript().await.unwrap();
    let mut receipt = manager
        .for_agent(root.id())
        .unwrap()
        .send_message(leaf.id(), "prepare work")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), entered.notified())
        .await
        .unwrap();
    assert_eq!(leaf.snapshot().status, AgentInstanceStatus::Running);
    let mut drained = Box::pin(root.wait_for_quiescence(None));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), drained.as_mut())
            .await
            .is_err()
    );
    release.notify_one();
    assert!(matches!(
        receipt.wait(Duration::from_secs(1)).await.delivery,
        crate::agent_manager::AgentMessageDelivery::Rejected { .. }
    ));
    tokio::time::timeout(Duration::from_secs(1), drained)
        .await
        .unwrap()
        .unwrap();
    manager.stop(leaf.id()).await.unwrap();
    let candidate = test_prepare_agent_instance_with_transcript(Some(transcript), false).await;
    manager
        .lock_live()
        .unwrap()
        .register(candidate, Some(child.id()))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), root.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn quiescence_waits_are_bounded_cancel_safe_and_independent_of_event_consumption() {
    let (instance, events) = test_running_instance().await;
    let mut observer = instance.subscribe();
    let mut waiters = Vec::new();
    for _ in 0..QUIESCENCE_WAIT_LIMIT {
        let mut wait = Box::pin(instance.wait_for_quiescence(None));
        assert!(futures::poll!(wait.as_mut()).is_pending());
        waiters.push(wait);
    }
    instance.view().await.unwrap();
    assert!(matches!(
        instance.wait_for_quiescence(None).await,
        Err(AgentInstanceError::Busy)
    ));
    drop(waiters.pop());
    let mut final_wait = Box::pin(instance.wait_for_quiescence(Some("drained".into())));
    assert!(futures::poll!(final_wait.as_mut()).is_pending());
    instance.view().await.unwrap();
    assert_eq!(instance.snapshot().status, AgentInstanceStatus::Running);
    for index in 0..=EVENT_CAPACITY {
        instance
            .view_with_event_boundary(index.to_string())
            .await
            .unwrap();
    }
    assert!(matches!(
        observer.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    observer = observer.resubscribe();
    events
        .send(AgentEvent::RunFinished {
            response: "done".into(),
            usage: Default::default(),
        })
        .await
        .unwrap();
    drop(events);
    let view = tokio::time::timeout(Duration::from_secs(1), final_wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.snapshot.status, AgentInstanceStatus::Idle);
    assert_eq!(view.snapshot.id, *instance.id());
    assert!(view.pending_submission_ids.is_empty());
    assert!(
        std::iter::from_fn(|| observer.try_recv().ok()).any(|event| matches!(
            event, AgentInstanceEvent::ViewBoundary { id } if id == "drained"
        ))
    );
    for wait in waiters {
        assert_eq!(
            wait.await.unwrap().snapshot.status,
            AgentInstanceStatus::Idle
        );
    }
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn stop_invalidates_quiescence_wait_without_successor_identity_reuse() {
    let manager = crate::harness::manager::AgentManager::new();
    let mut candidate = test_client().await;
    let _events = candidate.install_test_main_run("old-turn");
    let instance = manager.insert(candidate).unwrap();
    let old_scope = {
        let registry = manager.lock().unwrap();
        crate::harness::manager::policy::InstanceAgentScope::new(
            registry.owner.clone(),
            instance.id().clone(),
            registry.member(instance.id()).unwrap().epoch,
        )
    };
    let mut wait = Box::pin(instance.wait_for_quiescence(None));
    assert!(futures::poll!(wait.as_mut()).is_pending());
    instance.view().await.unwrap();
    let transcript = instance.stop().await.unwrap();
    assert!(matches!(wait.await, Err(AgentInstanceError::Stopped)));
    let successor = manager
        .insert(test_prepare_agent_instance_with_transcript(Some(transcript), false).await)
        .unwrap();
    assert_eq!(instance.id(), successor.id());
    assert_ne!(instance.registration_id(), successor.registration_id());
    assert!(old_scope.has_pending_execution().is_err());
    assert!(matches!(
        instance.wait_for_quiescence(None).await,
        Err(AgentInstanceError::Stopped)
    ));
    assert_eq!(
        successor
            .wait_for_quiescence(None)
            .await
            .unwrap()
            .snapshot
            .status,
        AgentInstanceStatus::Idle
    );
    manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn explicit_stop_returns_transcript_and_closes_instance() {
    let instance = test_instance().await;
    let expected_id = instance.id().as_str().to_string();

    let transcript = instance.stop().await.unwrap();

    assert_eq!(transcript.id.as_str(), expected_id);
    assert_eq!(instance.snapshot().status, AgentInstanceStatus::Stopped);
    assert_eq!(
        serde_json::to_value(instance.transcript().await.unwrap()).unwrap(),
        serde_json::to_value(transcript).unwrap()
    );
}

#[tokio::test]
async fn publishes_phi_events_in_delivery_order() {
    let (instance, events) = test_running_instance().await;
    let mut observer = instance.subscribe();
    events
        .send(AgentEvent::AssistantDelta {
            original: "first".into(),
            display: "first".into(),
        })
        .await
        .unwrap();
    events
        .send(AgentEvent::RunFinished {
            response: "done".into(),
            usage: crate::transcript::Usage::default(),
        })
        .await
        .unwrap();
    drop(events);

    let received = tokio::time::timeout(Duration::from_secs(1), async {
        let mut received = Vec::new();
        while received.len() < 2 {
            if let AgentInstanceEvent::Phi(envelope) = observer.recv().await.unwrap() {
                received.push(envelope.event);
            }
        }
        received
    })
    .await
    .unwrap();

    assert!(matches!(
        &received[0],
        PhiEvent::AssistantDelta { original, .. } if original == "first"
    ));
    assert!(matches!(received[1], PhiEvent::TurnFinished { .. }));
    wait_for_status(&instance, AgentInstanceStatus::Idle).await;
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn late_observer_recovers_in_flight_state() {
    let (instance, events) = test_running_instance().await;
    let replacement = test_prepare_agent_instance_with_transcript(
        Some(instance.transcript().await.unwrap()),
        false,
    )
    .await;
    assert!(matches!(
        instance
            .replace_runtime(replacement.into_runtime_candidate())
            .await,
        Err(AgentInstanceError::Busy)
    ));
    let mut observer = instance.subscribe();
    let mut snapshots = instance.subscribe_snapshots();
    let mut queued_ids = Vec::new();
    for dispatch in [SubmissionDispatch::Steer, SubmissionDispatch::FollowUp] {
        let SubmissionDisposition::Queued { submission_id, .. } = instance
            .dispatch(
                crate::core::turn::TurnPrompt {
                    tool_policy: Default::default(),
                    prompt: "same prompt".into(),
                    source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                },
                dispatch,
            )
            .await
            .unwrap()
        else {
            panic!("expected queued submission")
        };
        queued_ids.push(submission_id);
    }
    assert_eq!(
        instance.view().await.unwrap().pending_submission_ids,
        queued_ids
    );
    events
        .send(AgentEvent::UserInputInjected {
            submission_id: Some(queued_ids[0].clone()),
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
        .send(AgentEvent::AssistantDelta {
            original: "partial".into(),
            display: "partial".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while snapshots
            .snapshot()
            .live_run
            .as_ref()
            .map(|run| run.assistant_text.as_str())
            != Some("partial")
        {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    events
        .send(AgentEvent::ToolCall {
            id: "tool-1".into(),
            provider_call_id: Some("tool-1".into()),
            provider_item_id: None,
            name: "read".into(),
            args: serde_json::json!({"path":"file"}),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while snapshots
            .snapshot()
            .live_run
            .as_ref()
            .and_then(|run| run.tool.as_ref())
            .is_none()
        {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    for index in 0..=EVENT_CAPACITY {
        instance
            .view_with_event_boundary(format!("overflow-{index}"))
            .await
            .unwrap();
    }
    assert!(matches!(
        observer.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(_))
    ));
    observer = observer.resubscribe();
    let view = instance
        .view_with_event_boundary("live-boundary".into())
        .await
        .unwrap();
    let mut covered_events = 0;
    loop {
        match observer.recv().await.unwrap() {
            AgentInstanceEvent::ViewBoundary { id } if id == "live-boundary" => break,
            AgentInstanceEvent::Phi(_) => covered_events += 1,
            _ => {}
        }
    }
    assert_eq!(covered_events, 0);
    events
        .send(AgentEvent::AssistantDelta {
            original: "after boundary".into(),
            display: "after boundary".into(),
        })
        .await
        .unwrap();
    let following = tokio::time::timeout(Duration::from_secs(1), observer.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(following, AgentInstanceEvent::Phi(event)
        if matches!(&event.event, PhiEvent::AssistantDelta { display, .. }
            if display == "after boundary")));
    assert_eq!(view.transcript.id.as_str(), view.snapshot.id.as_str());
    assert_eq!(view.pending_submission_ids, vec![queued_ids[1].clone()]);
    assert!(view.transcript.has_user_message());
    let snapshot = view.snapshot;
    let run = snapshot.live_run.as_ref().unwrap();
    assert_eq!(snapshot.status, AgentInstanceStatus::Running);
    assert!(run.assistant_text.is_empty());
    assert_eq!(run.tool.as_ref().unwrap().id, "tool-1");
    let mut updated = run.clone();
    updated.tool.as_mut().unwrap().args = serde_json::json!({"path": "updated"});
    assert!(!snapshot_is_current(
        &snapshot,
        snapshot.status,
        snapshot.active_turn_id.as_deref(),
        Some(&updated),
        snapshot.progress.as_ref(),
        &snapshot.pending_interactions,
        snapshot.latest_error.as_ref(),
    ));
    let transcript = instance.stop().await.unwrap();
    let mut observer = instance.subscribe();
    let view = instance
        .view_with_event_boundary("stopped-boundary".into())
        .await
        .unwrap();
    assert!(matches!(
        observer.recv().await.unwrap(),
        AgentInstanceEvent::ViewBoundary { id } if id == "stopped-boundary"
    ));
    assert_eq!(
        serde_json::to_value(view.transcript).unwrap(),
        serde_json::to_value(transcript).unwrap()
    );
    assert!(view.pending_submission_ids.is_empty());
    let stopped = view.snapshot;
    assert_eq!(stopped.status, AgentInstanceStatus::Stopped);
    assert!(stopped.active_turn_id.is_none());
    assert!(stopped.live_run.is_none());
}

#[tokio::test]
async fn retains_interaction_until_a_handle_responds() {
    let config = PermissionConfig {
        bash: Some(ToolPerm::Simple(Action::Ask)),
        ..PermissionConfig::default()
    };
    let services = PhiRuntimeServices::new()
        .permission_policy(config, None)
        .model_catalog(test_catalog())
        .interactive(true);
    let state = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .no_tools(true)
        .build()
        .unwrap()
        .with_runtime_services(services)
        .prepare_agent_instance()
        .await
        .unwrap();
    let permission = state.resources().permission().clone();
    let instance = AgentInstance::spawn(state);
    let permission_request = permission.clone();
    let task = tokio::spawn(async move {
        permission_request
            .authorize_command(&CommandPermissionRequest::new("printf", &[]))
            .await
    });
    let mut snapshots = instance.subscribe_snapshots();
    tokio::time::timeout(Duration::from_secs(1), async {
        while snapshots.snapshot().pending_interactions.is_empty() {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let request_id = snapshots.snapshot().pending_interactions[0]
        .request_id
        .clone();

    let mut observer = instance.subscribe();
    let pending = instance
        .view_with_event_boundary("permission-pending".into())
        .await
        .unwrap();
    assert_eq!(pending.snapshot.status, AgentInstanceStatus::RequiresAction);
    assert_eq!(
        pending.snapshot.pending_interactions[0].request_id,
        request_id
    );
    assert!(matches!(observer.recv().await.unwrap(),
        AgentInstanceEvent::ViewBoundary { id } if id == "permission-pending"));
    assert!(matches!(
        instance.reset_context(None).await,
        Err(AgentInstanceError::Busy)
    ));
    let unchanged = instance.view().await.unwrap();
    assert_eq!(unchanged.snapshot.revision, pending.snapshot.revision);
    assert_eq!(
        serde_json::to_value(unchanged.transcript).unwrap(),
        serde_json::to_value(pending.transcript).unwrap()
    );
    let mut wait = Box::pin(instance.wait_for_quiescence(None));
    assert!(futures::poll!(wait.as_mut()).is_pending());
    instance.view().await.unwrap();
    assert!(futures::poll!(wait.as_mut()).is_pending());
    instance
        .respond_interaction(
            request_id.clone(),
            InteractionResponse::Permission(PermissionDecision::AllowOnce),
        )
        .await
        .unwrap();

    assert!(task.await.unwrap().is_ok());
    assert!(
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .pending_interactions
            .is_empty()
    );
    let committed = instance
        .view_with_event_boundary("permission-committed".into())
        .await
        .unwrap();
    assert!(committed.snapshot.pending_interactions.is_empty());
    assert!(matches!(observer.recv().await.unwrap(),
        AgentInstanceEvent::InteractionClosed { request_id: closed } if closed == request_id));
    assert!(matches!(observer.recv().await.unwrap(),
        AgentInstanceEvent::ViewBoundary { id } if id == "permission-committed"));

    let task = tokio::spawn(async move {
        permission
            .authorize_command(&CommandPermissionRequest::new("printf", &[]))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while snapshots.snapshot().pending_interactions.is_empty() {
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    instance.stop().await.unwrap();
    assert!(task.await.unwrap().is_err());
    let mut observer = instance.subscribe();
    let stopped = instance
        .view_with_event_boundary("permission-stopped".into())
        .await
        .unwrap();
    assert_eq!(stopped.snapshot.status, AgentInstanceStatus::Stopped);
    assert!(stopped.snapshot.pending_interactions.is_empty());
    assert!(matches!(observer.recv().await.unwrap(),
        AgentInstanceEvent::ViewBoundary { id } if id == "permission-stopped"));
}

#[tokio::test]
async fn event_stream_is_bounded_and_reports_lag() {
    let instance = test_instance().await;
    let mut events = instance.subscribe();
    for _ in 0..EVENT_CAPACITY + 1 {
        let _ = instance.events.send(AgentInstanceEvent::Stopped);
    }

    assert!(matches!(
        events.recv().await,
        Err(broadcast::error::RecvError::Lagged(1))
    ));
    instance.stop().await.unwrap();
}
