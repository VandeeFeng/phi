use super::*;

#[tokio::test]
async fn peer_messages_join_active_turn_at_safe_points() {
    use crate::agent_manager::AgentMessageDelivery;
    let root =
        std::path::Path::new("/tmp").join(format!("phi-peer-steer-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let mut phi = instance.phi.clone();
    phi.hooks
        .register_hook(crate::hooks::Hook::prompt_context(|event| async move {
            Ok(crate::core::lifecycle::PromptContextOutcome {
                contexts: if event.prompt.contains("peer correction") {
                    vec![crate::context::InjectedContext {
                        source: crate::transcript::ContextSource::Hook,
                        content: "peer-context".into(),
                    }]
                } else {
                    Vec::new()
                },
                ..Default::default()
            })
        }));
    instance.instance = instance
        .manager
        .insert(phi.agent_instance_builder().build().await.unwrap())
        .unwrap();
    let sender = instance
        .manager
        .insert(instance.phi.agent_instance_builder().build().await.unwrap())
        .unwrap();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let held = release.clone();
    let server = tokio::spawn(mock_provider_with_reply(
        listener,
        requests,
        move |request| {
            let held = held.clone();
            async move {
                if !request["messages"].to_string().contains("peer correction") {
                    held.notified().await;
                }
                completion(&request)
            }
        },
    ));
    let mut events = instance.subscribe();
    let SubmissionDisposition::Started { turn_id, .. } = instance
        .submit(TurnPrompt {
            prompt: "active work".into(),
            source: UserPromptSubmitSource::Sdk,
            injected_context: Vec::new(),
            images: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap()
    else {
        panic!("expected active turn");
    };
    tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    let mut receipts = Vec::new();
    for _ in 0..2 {
        let mut receipt = instance
            .manager
            .send_message(sender.id(), instance.id(), "peer correction")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while receipt.delivery() == AgentMessageDelivery::Queued {
                receipt.changed().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(receipt.delivery(), AgentMessageDelivery::Admitted);
        receipts.push(receipt);
    }
    assert!(
        !serde_json::to_string(&instance.transcript().await.unwrap())
            .unwrap()
            .contains("peer correction")
    );
    release.notify_one();
    let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    let mut turns = Vec::new();
    let mut injections = Vec::new();
    while let Ok(event) = events.try_recv() {
        let AgentInstanceEvent::Phi(event) = event else {
            continue;
        };
        match event.event {
            crate::PhiEvent::TurnStarted { .. } => turns.push(event.turn_id.unwrap()),
            crate::PhiEvent::UserInputInjected {
                submission_id: Some(id),
                ..
            } => {
                assert_eq!(event.turn_id.as_deref(), Some(turn_id.as_str()));
                injections.push(id);
            }
            _ => {}
        }
    }
    assert_eq!(turns, [turn_id]);
    assert_eq!(
        view.transcript.entries.iter().filter(|entry| matches!(
            &entry.kind,
            TranscriptEntryKind::Message {
                message: AgentMessage::Context { source: crate::transcript::ContextSource::Hook, content, .. }
            } if matches!(content.as_slice(), [crate::transcript::ContentBlock::Text { text }] if text.as_str() == "peer-context")
        )).count(),
        receipts.len()
    );
    let request = tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        request["messages"]
            .to_string()
            .matches("peer-context")
            .count(),
        1
    );
    assert_eq!(
        injections,
        receipts
            .iter()
            .map(|receipt| receipt.message().id.as_str().to_owned())
            .collect::<Vec<_>>()
    );
    for receipt in receipts {
        let AgentMessageDelivery::Committed { entry_id } = receipt.delivery() else {
            panic!("message not committed");
        };
        let entry = view
            .transcript
            .entries
            .iter()
            .find(|entry| entry.id.as_str() == entry_id)
            .unwrap();
        assert!(matches!(
            &entry.kind,
            TranscriptEntryKind::Message {
                message: AgentMessage::Agent { .. }
            }
        ));
        assert!(
            serde_json::to_string(entry)
                .unwrap()
                .contains("not human input or approval")
        );
    }
    instance.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn peer_at_turn_limit_preserves_completion_before_follow_up() {
    use crate::core::lifecycle::CompletionOutcome;
    use crate::hooks::Hook;

    for reject_completion in [false, true] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-peer-completion-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let mut phi = instance.phi.clone();
        phi.context.agent_catalog = crate::agent::custom::AgentCatalog::new(vec![
            crate::agents::AgentDefinition::new(
                "limited",
                "Inspect",
                "Perform the delegated task.",
            )
            .model("child")
            .max_turns(1),
        ]);
        let check_started = Arc::new(tokio::sync::Notify::new());
        let release_check = Arc::new(tokio::sync::Notify::new());
        phi.hooks.register_hook(Hook::stop({
            let check_started = check_started.clone();
            let release_check = release_check.clone();
            move |event| {
                let check_started = check_started.clone();
                let release_check = release_check.clone();
                async move {
                    let history = serde_json::to_string(&event.transcript).unwrap();
                    if history.contains("held work") && !history.contains("late correction") {
                        check_started.notify_one();
                        release_check.notified().await;
                        return Ok(if reject_completion {
                            CompletionOutcome::Fail {
                                reason: "required validation failed".into(),
                            }
                        } else {
                            CompletionOutcome::Allow
                        });
                    }
                    Ok(CompletionOutcome::Allow)
                }
            }
        }));
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let release_response = Arc::new(tokio::sync::Notify::new());
        let held = release_response.clone();
        let server = tokio::spawn(mock_provider_with_reply(
            listener,
            requests,
            move |request| {
                let held = held.clone();
                async move {
                    let history = request["messages"].to_string();
                    if request["model"] == "child"
                        && history.contains("held work")
                        && !history.contains("late correction")
                    {
                        held.notified().await;
                    }
                    completion(&request)
                }
            },
        ));
        let creation = instance
            .manager
            .spawn_agent(
                instance.id(),
                &phi,
                AgentSpawnRequest::new("limited", "initial work"),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        while received.try_recv().is_ok() {}
        let child = instance.manager.get(creation.instance_id()).unwrap();
        let mut events = child.subscribe();
        let SubmissionDisposition::Started { turn_id, .. } = child
            .submit(TurnPrompt {
                prompt: "held work".into(),
                source: UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap()
        else {
            panic!("expected active turn")
        };
        tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        let mut receipt = instance
            .manager
            .send_message(instance.id(), child.id(), "late correction")
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while receipt.delivery() == crate::agent_manager::AgentMessageDelivery::Queued {
                receipt.changed().await;
            }
        })
        .await
        .unwrap();
        release_response.notify_one();
        tokio::time::timeout(Duration::from_secs(5), check_started.notified())
            .await
            .unwrap();
        let premature_completion =
            tokio::time::timeout(Duration::from_millis(100), child.wait_for_quiescence(None)).await;
        release_check.notify_one();
        let view = tokio::time::timeout(Duration::from_secs(5), child.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let mut failed = false;
        let mut completed = false;
        let mut succeeded = false;
        let mut turns = 0;
        while let Ok(event) = events.try_recv() {
            if let AgentInstanceEvent::Phi(event) = event {
                match event.event {
                    crate::PhiEvent::TurnFailed { failure }
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) =>
                    {
                        assert!(failure.message.contains("required validation failed"));
                        failed = true;
                    }
                    crate::PhiEvent::TurnFinished { .. } => {
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) {
                            completed = true;
                        } else {
                            succeeded = true;
                        }
                    }
                    crate::PhiEvent::TurnStarted { .. } => turns += 1,
                    crate::PhiEvent::UserInputInjected { .. } => {
                        panic!("exhausted turn consumed peer input")
                    }
                    _ => {}
                }
            }
        }
        instance.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
        assert!(
            premature_completion.is_err(),
            "completion check was bypassed"
        );
        assert_eq!(failed, reject_completion);
        assert_eq!(completed, !reject_completion);
        assert!(succeeded);
        assert_eq!(turns, 2);
        assert!(matches!(
            receipt.delivery(),
            crate::agent_manager::AgentMessageDelivery::Committed { .. }
        ));
        assert!(view.pending_submission_ids.is_empty());
    }
}

#[tokio::test]
async fn background_feedback_and_teammate_exit_keep_distinct_activation() {
    const HELD_WORK: &str = "held lead work";
    const QUEUED_WORK: &str = "queued lead work";
    for (released_tasks, queued_prompt) in [
        (Vec::new(), None),
        (Vec::new(), Some(QUEUED_WORK)),
        (vec!["1"], None),
    ] {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-feedback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        instance
            .manager
            .initialize_team(
                instance.id(),
                crate::agent_manager::TeamStorageRoot::new(root.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let held = release.clone();
        let server = tokio::spawn(mock_provider_with_reply(
            listener,
            requests,
            move |request| {
                let held = held.clone();
                async move {
                    let messages = request["messages"].to_string();
                    if messages.contains(HELD_WORK) && !messages.contains(QUEUED_WORK) {
                        held.notified().await;
                    }
                    completion(&request)
                }
            },
        ));
        let creation = instance
            .spawn_subagent(
                AgentSpawnRequest::new("probe", "initial work")
                    .alias(crate::agent_manager::AgentAlias::new("worker").unwrap()),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let initial_requests = std::iter::from_fn(|| received.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(initial_requests.len(), 2);
        assert!(
            initial_requests[1]["messages"]
                .to_string()
                .contains("not human input or approval")
        );
        if let Some(prompt) = queued_prompt {
            for (text, dispatch) in [
                (HELD_WORK, crate::turn::SubmissionDispatch::Steer),
                (prompt, crate::turn::SubmissionDispatch::FollowUp),
            ] {
                instance
                    .dispatch(
                        TurnPrompt {
                            prompt: text.into(),
                            source: UserPromptSubmitSource::Sdk,
                            injected_context: Vec::new(),
                            images: Vec::new(),
                            tool_policy: Default::default(),
                        },
                        dispatch,
                    )
                    .await
                    .unwrap();
            }
            tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap();
        }
        for id in &released_tasks {
            let directory = root.join("tasks/test-list");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(format!("{id}.json")),
                json!({"id": id, "subject": "Review", "description": "Finish review",
                    "status": "in_progress", "owner": {"instance_id": creation.instance_id()},
                    "blocked_by": [], "metadata": {}})
                .to_string(),
            )
            .unwrap();
        }
        let mut changes = instance.subscribe();
        instance.manager.stop(creation.instance_id()).await.unwrap();
        if queued_prompt.is_some() {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let view = instance.view().await.unwrap();
                    let history = serde_json::to_string(&view.transcript).unwrap();
                    if view.pending_submission_ids.len() == 2
                        || history.contains("Automatic teammate exit notice")
                    {
                        assert!(!history.contains("Automatic teammate exit notice"));
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            release.notify_one();
        }
        let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let history = serde_json::to_string(&view.transcript).unwrap();
        assert!(history.contains("Automatic teammate exit notice"));
        assert!(history.contains("not human input or approval"));
        assert!(
            std::iter::from_fn(|| changes.try_recv().ok())
                .any(|event| matches!(event, AgentInstanceEvent::HistoryChanged))
        );
        let exit_requests = std::iter::from_fn(|| received.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            exit_requests.len(),
            usize::from(!released_tasks.is_empty()) + usize::from(queued_prompt.is_some())
        );
        instance.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn user_cancelled_child_requires_direct_resumption_before_peer_work() {
    let root =
        std::path::Path::new("/tmp").join(format!("phi-cancelled-peer-{}", uuid::Uuid::new_v4()));
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
            let held = held.clone();
            async move {
                let history = request["messages"].to_string();
                if request["model"] == "child"
                    && history.contains("cancelled work")
                    && !history.contains("peer-work")
                    && !history.contains("user-resume")
                {
                    held.notified().await;
                }
                completion(&request)
            }
        },
    ));
    let creation = instance
        .spawn_subagent(AgentSpawnRequest::new("probe", "cancelled work"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), received.recv())
        .await
        .unwrap()
        .unwrap();
    let child = instance.manager.get(creation.instance_id()).unwrap();
    let background = child
        .spawn_subagent(
            AgentSpawnRequest::new("probe", "independent background work")
                .mode(crate::agent_manager::DelegationMode::Background),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let status = child
                .subagent_status(background.instance_id().clone())
                .await
                .unwrap()
                .unwrap();
            if status.admission == SubagentAdmission::Accepted {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let queued = instance
        .manager
        .send_message(
            instance.id(),
            child.id(),
            "peer-work queued before cancellation",
        )
        .await
        .unwrap();
    assert!(instance.cancel_subagent(child.id().clone()).await.unwrap());
    release.notify_one();
    instance.wait_for_quiescence(None).await.unwrap();
    let initial = instance.subagent_snapshots().await.unwrap()[0]
        .status
        .outcome
        .clone()
        .unwrap();
    assert!(initial.as_ref().unwrap_err().is_user_cancelled());
    let subsequent = instance
        .manager
        .send_message(instance.id(), child.id(), "peer-work must not restart it")
        .await
        .unwrap();
    for mut receipt in [queued, subsequent] {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !receipt.delivery().is_terminal() {
                receipt.changed().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            receipt.delivery(),
            crate::agent_manager::AgentMessageDelivery::Rejected { reason }
                if reason.contains("cancelled")
        ));
    }
    let view = child.wait_for_quiescence(None).await.unwrap();
    assert!(
        !serde_json::to_string(&view.transcript)
            .unwrap()
            .contains("peer-work")
    );
    let background_request = received.try_recv().unwrap();
    assert!(
        background_request["messages"]
            .to_string()
            .contains("independent background work")
    );
    assert!(received.try_recv().is_err());
    let background = instance.manager.get(background.instance_id()).unwrap();
    background
        .submit(TurnPrompt {
            prompt: "later background report".into(),
            source: UserPromptSubmitSource::Sdk,
            injected_context: Vec::new(),
            images: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    background.wait_for_quiescence(None).await.unwrap();
    let view = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let view = child.view().await.unwrap();
            if serde_json::to_string(&view.transcript)
                .unwrap()
                .contains("Automatic subagent result")
            {
                break view;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(view.snapshot.active_turn_id.is_none());
    assert!(
        received.try_recv().unwrap()["messages"]
            .to_string()
            .contains("later background report")
    );
    assert!(received.try_recv().is_err());
    for source in [
        UserPromptSubmitSource::Internal,
        UserPromptSubmitSource::Sdk,
    ] {
        let result = child
            .submit(TurnPrompt {
                prompt: "user-resume".into(),
                source,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            })
            .await;
        if matches!(source, UserPromptSubmitSource::Internal) {
            assert!(result.unwrap_err().to_string().contains("cancelled"));
        } else {
            assert!(matches!(
                result.unwrap(),
                SubmissionDisposition::Started { .. }
            ));
        }
    }
    child.wait_for_quiescence(None).await.unwrap();
    let mut receipt = instance
        .manager
        .send_message(instance.id(), child.id(), "peer-work after explicit resume")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !receipt.delivery().is_terminal() {
            receipt.changed().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(
        receipt.delivery(),
        crate::agent_manager::AgentMessageDelivery::Committed { .. }
    ));
    child.wait_for_quiescence(None).await.unwrap();
    assert_eq!(
        serde_json::to_value(
            &instance.subagent_snapshots().await.unwrap()[0]
                .status
                .outcome
        )
        .unwrap(),
        serde_json::to_value(Some(initial)).unwrap()
    );
    instance.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ordinary_submissions_preserve_instance_definition_without_new_tasks() {
    let root =
        std::env::temp_dir().join(format!("phi-instance-submission-{}", uuid::Uuid::new_v4()));
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
            let block = request["messages"]
                .to_string()
                .contains("ordinary-instance-followup")
                && !request["messages"]
                    .to_string()
                    .contains("queued-instance-followup");
            async move {
                if block {
                    release.notified().await;
                }
                completion(&request)
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
    wait_child_completion(&instance, started.instance_id()).await;
    let initial_turn = instance.subagent_snapshots().await.unwrap()[0]
        .initial_turn_id
        .clone()
        .unwrap();
    let child = instance.manager.get(started.instance_id()).unwrap();
    let submitted = child
        .dispatch(
            TurnPrompt {
                prompt: "ordinary-instance-followup".into(),
                source: UserPromptSubmitSource::Internal,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            },
            SubmissionDispatch::FollowUp,
        )
        .await
        .unwrap();
    let SubmissionDisposition::Started { turn_id, .. } = submitted else {
        panic!("expected an ordinary instance turn");
    };
    assert_ne!(turn_id, initial_turn);
    assert!(
        !instance
            .cancel_subagent(started.instance_id().clone())
            .await
            .unwrap()
    );
    assert_eq!(
        child.snapshot().active_turn_id.as_deref(),
        Some(turn_id.as_str())
    );
    assert_eq!(
        instance.subagent_snapshots().await.unwrap()[0]
            .initial_turn_id
            .as_deref(),
        Some(initial_turn.as_str())
    );
    let request = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let request = received.recv().await.unwrap();
            if request["messages"]
                .to_string()
                .contains("ordinary-instance-followup")
            {
                break request;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(request["model"], "child");
    assert!(system_text(&request).contains("Perform the delegated task."));
    let tools = request["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["function"]["name"] == "Agent"));
    assert!(!tools.iter().any(|tool| tool["function"]["name"] == "Read"));
    assert!(request["messages"].to_string().contains("initial-work"));
    let mut events = child.subscribe();
    let access = instance.manager.for_agent(instance.id()).unwrap();
    let mut queued = std::collections::HashSet::new();
    for prompt in ["queued-instance-followup", "queued-next"] {
        let receipt = access.send_message(child.id(), prompt).await.unwrap();
        assert!(!receipt.delivery().is_terminal());
        assert!(queued.insert(receipt.message().id.as_str().to_owned()));
    }
    assert!(
        !serde_json::to_string(&child.transcript().await.unwrap())
            .unwrap()
            .contains("queued-instance-followup")
    );
    release.notify_one();
    let view = tokio::time::timeout(Duration::from_secs(5), child.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    for prompt in [
        "ordinary-instance-followup",
        "queued-instance-followup",
        "queued-next",
    ] {
        assert!(
            serde_json::to_string(&view.transcript)
                .unwrap()
                .contains(prompt)
        );
    }
    for _ in 0..2 {
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let request = received.recv().await.unwrap();
                if request["messages"]
                    .to_string()
                    .contains("queued-instance-followup")
                {
                    break request;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(request["model"], "child");
        assert!(system_text(&request).contains("Perform the delegated task."));
    }
    let mut observed = std::collections::HashSet::new();
    while let Ok(event) = events.try_recv() {
        if let AgentInstanceEvent::Phi(envelope) = event
            && let crate::PhiEvent::UserInputInjected {
                submission_id: Some(id),
                source,
                ..
            } = envelope.event
        {
            assert_eq!(envelope.turn_id.as_deref(), Some(turn_id.as_str()));
            assert_eq!(source, UserPromptSubmitSource::Internal.as_str());
            assert!(observed.insert(id));
        }
    }
    assert_eq!(observed, queued);
    assert_eq!(instance.subagent_snapshots().await.unwrap().len(), 1);
    instance.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn ordinary_message_preparation_is_once_and_stop_interrupts_blocked_hooks() {
    use crate::agent_manager::AgentMessageDelivery;
    let root = std::env::temp_dir().join(format!("phi-message-input-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let observed = entered.clone();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed_calls = calls.clone();
    let mut phi = instance.phi.clone();
    phi.hooks
        .register_hook(crate::hooks::Hook::user_prompt_submit(move |event| {
            let observed = observed.clone();
            let calls = observed_calls.clone();
            async move {
                calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                assert!(matches!(event.source, UserPromptSubmitSource::Internal));
                if event.prompt.contains("blocked-message") {
                    observed.notify_one();
                    std::future::pending::<()>().await;
                }
                Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Handled {
                    reason: Some("message intercepted".into()),
                })
            }
        }));
    let recipient = instance
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let mut handled = instance
        .manager
        .send_message(instance.id(), recipient.id(), "handled-message")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(handled.delivery(), AgentMessageDelivery::Rejected { .. }) {
            handled.changed().await;
        }
    })
    .await
    .unwrap();
    assert!(!recipient.transcript().await.unwrap().has_user_message());
    let blocked = instance
        .manager
        .send_message(instance.id(), recipient.id(), "blocked-message")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert_eq!(blocked.delivery(), AgentMessageDelivery::Admitted);
    let view = tokio::time::timeout(Duration::from_secs(1), recipient.view()).await;
    tokio::time::timeout(Duration::from_secs(2), recipient.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        blocked.delivery(),
        AgentMessageDelivery::Rejected { .. }
    ));
    view.expect("message preparation blocked actor controls")
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert!(
        recipient
            .transcript()
            .await
            .unwrap()
            .entries
            .iter()
            .all(|entry| !matches!(
                &entry.kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::Agent { .. }
                }
            ))
    );
    instance.manager.shutdown().await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn message_follow_up_automatically_reports_to_creator() {
    for expected in ["child-result", "follow-up-failed"] {
        for team_directory in [None, Some("teams")] {
            let root =
                std::env::temp_dir().join(format!("phi-message-result-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let instance =
                instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
            if let Some(directory) = team_directory {
                instance
                    .manager
                    .initialize_team(
                        instance.id(),
                        crate::agent_manager::TeamStorageRoot::new(root.join(directory)).unwrap(),
                    )
                    .await
                    .unwrap();
            }
            let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
            let server = tokio::spawn(mock_provider(listener, requests, move |request| {
                if expected == "follow-up-failed"
                    && request["model"] == "child"
                    && request["messages"]
                        .to_string()
                        .contains("continue the existing work")
                {
                    json!({"error": {"message": expected}})
                } else {
                    completion(request)
                }
            }));
            let creation = instance
                .spawn_subagent(AgentSpawnRequest::new("probe", "initial work"))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), received.recv())
                .await
                .unwrap()
                .unwrap();
            let child = instance.manager.get(creation.instance_id()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
                .await
                .unwrap()
                .unwrap();
            while received.try_recv().is_ok() {}
            instance
                .manager
                .for_agent(instance.id())
                .unwrap()
                .send_message(child.id(), "continue the existing work")
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let request = received.recv().await.unwrap();
                    if request["model"] == "test"
                        && request["messages"]
                            .to_string()
                            .contains("Automatic subagent result")
                    {
                        assert!(request["messages"].to_string().contains(expected));
                        break;
                    }
                }
            })
            .await
            .unwrap();
            let view =
                tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
                    .await
                    .unwrap()
                    .unwrap();
            assert_eq!(
            view.transcript
                .entries
                .iter()
                .filter(|entry| matches!(
                    &entry.kind,
                    TranscriptEntryKind::Event { name, data }
                        if name == "phi_sdk.message_delivered" && data["kind"] == "subagent_result"
                ))
                .count(),
            1
        );
            let history = child.transcript().await.unwrap();
            let history = serde_json::to_string(&history).unwrap();
            assert!(history.contains("initial work"));
            assert!(history.contains("continue the existing work"));
            instance.manager.shutdown().await.unwrap();
            server.abort();
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[tokio::test]
async fn ordinary_messages_commit_in_order_without_tasks_and_stop_rejects_pending_delivery() {
    use crate::agent_manager::AgentMessageDelivery;
    let root = std::env::temp_dir().join(format!("phi-messages-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let instance = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Notify::new());
    let server_release = release.clone();
    let server = tokio::spawn(mock_provider_with_reply(
        listener,
        requests,
        move |request| {
            let release = server_release.clone();
            async move {
                let prompt = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "user")
                    .unwrap()["content"]
                    .to_string();
                if request["model"] == "child"
                    && (prompt.contains("initial-held") || prompt.contains("hold-stop"))
                {
                    release.notified().await;
                }
                completion(&request)
            }
        },
    ));
    let initial = instance
        .spawn_subagent(AgentSpawnRequest::new("probe", "initial-held"))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), received.recv())
        .await
        .unwrap()
        .unwrap();
    let child_id = AgentInstanceId::new(initial.instance_id().as_str());
    let child = instance.manager.get(&child_id).unwrap();
    let access = instance.manager.for_agent(instance.id()).unwrap();
    let mut first = access
        .send_message(&child_id, "same coordination")
        .await
        .unwrap();
    let mut second = access
        .send_message(&child_id, "same coordination")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while second.delivery() == AgentMessageDelivery::Queued {
            second.changed().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(first.delivery(), AgentMessageDelivery::Admitted);
    assert_eq!(second.delivery(), AgentMessageDelivery::Admitted);
    let mut unrelated = access
        .send_message(&child_id, "unrelated feedback")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while unrelated.delivery() == AgentMessageDelivery::Queued {
            unrelated.changed().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(unrelated.delivery(), AgentMessageDelivery::Admitted);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        for message in [&mut first, &mut second, &mut unrelated] {
            while !matches!(message.delivery(), AgentMessageDelivery::Committed { .. }) {
                message.changed().await;
            }
        }
    })
    .await
    .unwrap();
    let original = tokio::time::timeout(
        Duration::from_secs(5),
        child.complete_delegated_agent(tokio::sync::watch::channel(false).1),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        std::iter::from_fn(|| received.try_recv().ok()).any(|request| {
            request["model"] == "child"
                && request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "user")
                    .unwrap()["content"]
                    .to_string()
                    .contains("same coordination")
        })
    );
    let history: crate::transcript::Transcript =
        serde_json::from_str(&serde_json::to_string(&child.transcript().await.unwrap()).unwrap())
            .unwrap();
    let markers = history
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Event { name, data } if name == "phi_sdk.message_delivered" => {
                Some(data)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        markers
            .iter()
            .map(|marker| marker["messageId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            first.message().id.as_str(),
            second.message().id.as_str(),
            unrelated.message().id.as_str()
        ]
    );
    for (marker, receipt) in markers.iter().zip([&first, &second, &unrelated]) {
        let AgentMessageDelivery::Committed { entry_id } = receipt.delivery() else {
            panic!("message was not committed");
        };
        let entry = history
            .entries
            .iter()
            .find(|entry| entry.id.as_str() == entry_id)
            .unwrap();
        assert_eq!(marker["messageEntryId"], entry_id);
        assert_eq!(marker["sender"], instance.id().as_str());
        assert_eq!(marker["recipient"], child_id.as_str());
        assert!(
            serde_json::to_string(entry)
                .unwrap()
                .contains(&receipt.message().content)
        );
        assert!(marker.get("taskId").is_none());
    }
    assert_eq!(
        history
            .entries
            .iter()
            .filter(|entry| matches!(
                &entry.kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::Agent { .. }
                }
            ))
            .count(),
        3
    );
    let mut late = access
        .send_message(&child_id, "late feedback")
        .await
        .unwrap();
    let late_delivery = late.wait(Duration::from_secs(2)).await;
    assert!(matches!(
        late_delivery.delivery,
        AgentMessageDelivery::Committed { .. }
    ));
    let view = tokio::time::timeout(Duration::from_secs(5), child.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    assert!(view.snapshot.active_turn_id.is_none());
    assert!(
        serde_json::to_string(&view.transcript)
            .unwrap()
            .contains("late feedback")
    );
    let mut independent = access
        .send_message(&child_id, "idle continuation")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(
            independent.delivery(),
            AgentMessageDelivery::Committed { .. }
        ) {
            independent.changed().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        child
            .complete_delegated_agent(tokio::sync::watch::channel(false).1)
            .await
            .unwrap(),
        original
    );
    assert_eq!(instance.subagent_snapshots().await.unwrap().len(), 1);
    let mut held = access.send_message(&child_id, "hold-stop").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(held.delivery(), AgentMessageDelivery::Committed { .. }) {
            held.changed().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let request = received.recv().await.unwrap();
            if request["model"] == "child" && request.to_string().contains("hold-stop") {
                break;
            }
        }
    })
    .await
    .unwrap();
    let mut stopped = access
        .send_message(&child_id, "must not commit")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while stopped.delivery() == AgentMessageDelivery::Queued {
            stopped.changed().await;
        }
    })
    .await
    .unwrap();
    child.stop().await.unwrap();
    assert!(
        matches!(stopped.delivery(), AgentMessageDelivery::Rejected { .. }),
        "{:?}",
        stopped.delivery()
    );
    assert!(
        !serde_json::to_string(&child.transcript().await.unwrap())
            .unwrap()
            .contains("must not commit")
    );
    assert!(access.send_message(&child_id, "no revival").await.is_err());
    let final_history = child.transcript().await.unwrap();
    let reopened = instance
        .manager
        .insert(
            instance
                .phi
                .agent_instance_builder()
                .transcript(final_history.clone())
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    assert_eq!(reopened.id(), &child_id);
    assert!(
        access
            .send_message(reopened.id(), "old creation scope expired")
            .await
            .is_err()
    );
    assert_eq!(
        reopened.view().await.unwrap().transcript.entries.len(),
        final_history.entries.len()
    );
    assert!(matches!(
        stopped.delivery(),
        AgentMessageDelivery::Rejected { .. }
    ));
    instance.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}
