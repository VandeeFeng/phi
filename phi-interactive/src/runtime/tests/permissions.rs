use super::*;

#[tokio::test]
async fn child_permissions_survive_view_changes_and_clear_on_stop() {
    use phi_sdk::interaction::{InteractionResponse, PermissionDecision};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if super::preparation::tests::run_isolated_test(concat!(
        module_path!(),
        "::child_permissions_survive_view_changes_and_clear_on_stop"
    )) {
        return;
    }
    let root = std::env::temp_dir().join(format!("phi-interaction-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
    std::fs::write(
        root.join(".phi/agents/probe.md"),
        "---\nname: probe\ndescription: Probe\ntools: [Bash]\n---\nRun the requested command.",
    )
    .unwrap();
    std::fs::write(
        root.join(".phi/settings.json"),
        r#"{"defaultProvider":"local","defaultModel":"test","permission":{"bash":"ask"},"extensions":["!*.lua"],"memory":{"enabled":false}}"#,
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_path = root.join("models.json");
    std::fs::write(&model_path, serde_json::json!({"providers":{"local":{
        "api":"openai-completions","baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
        "models":{"test":{"contextWindow":128000}}
    }}}).to_string()).unwrap();
    let (release, released) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        let mut released = Some(released);
        let mut call = 0;
        loop {
            let (mut connection, _) = listener.accept().await.unwrap();
            call += 1;
            let mut bytes = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                assert_ne!(connection.read(&mut buffer).await.unwrap(), 0);
                bytes.extend_from_slice(&buffer);
                if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length: "))
                        .unwrap()
                        .parse()
                        .unwrap();
                    if bytes.len() >= start + 4 + length {
                        break;
                    }
                }
            }
            if let Some(released) = released.take() {
                released.await.unwrap();
            }
            let reply = serde_json::json!({"id":"completion","model":"test","choices":[{
                "delta":{"tool_calls":[{"index":0,"id":uuid::Uuid::new_v4().to_string(),"type":"function",
                    "function":{"name":"Bash","arguments":serde_json::json!({"command":format!("printf permission-probe{call}")}).to_string()}}]},
                "finish_reason":"tool_calls"
            }]});
            let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
            connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let catalog = Arc::new(
        phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("catalog.json"),
            user_paths: vec![model_path],
            offline: true,
        })
        .unwrap(),
    );
    let cli = Cli::parse_from(["phi"]);
    let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
    let mut settings = settings_manager.settings();
    settings.permission = Some(serde_json::json!({"bash":"ask"}));
    let resources = RuntimeResources::new_for(
        PhiRuntimeServices::new()
            .model_catalog(catalog.clone())
            .permission_policy(
                phi_sdk::permission::PermissionConfig::from_value(
                    settings.permission.clone().unwrap(),
                )
                .unwrap(),
                None,
            )
            .interactive(true),
        Arc::new(ExtensionRuntime::load_with_paths(Vec::new())),
        catalog.clone(),
        Sandbox::new(false, root.clone()).unwrap(),
        &root,
    )
    .extension_paths(Vec::new());
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    test_runtime
        .preparation
        .enable_subagents(&test_runtime.sessions);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.current_switch = "permission-test".into();
    let agent_session = test_runtime
        .sessions
        .create(
            &test_runtime.preparation,
            session.clone(),
            RuntimeRebuildOptions {
                phi: PhiBuildOptions {
                    cli: &cli,
                    provider: "local",
                    model: "test",
                    runtime_cfg: settings.runtime_config(&catalog),
                    context: phi_sdk::context::load_with_options_for(
                        false,
                        None,
                        vec![root.join(".phi/agents")],
                        &root,
                    ),
                    agent_options: crate::AgentBuildOptions::default(),
                    hooks: Vec::new(),
                },
                instance: {
                    let mut instance = AgentInstanceBuildOptions::from_session(&session);
                    instance.settings_manager = Some(settings_manager.clone());
                    instance.resource_settings = Some(Arc::new(settings.clone()));
                    instance
                },
            },
            phi_sdk::lifecycle::SessionStartReason::New,
            true,
        )
        .await
        .unwrap();
    let policy = TestConfiguration::new(
        Arc::new(cli),
        settings,
        crate::AgentBuildOptions::default(),
        Vec::new(),
        catalog,
    );
    policy.install(&test_runtime.preparation);
    let remote_workflows = super::preparation::SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    std::fs::write(root.join("app.js"), "").unwrap();
    let host = super::gateway::GatewaySessions::new(
        test_runtime.sessions.clone(),
        remote_workflows,
        test_runtime.preparation.clone(),
        true,
    )
    .with_test_binding(agent_session.clone());
    let (transport, registration, endpoint) = phi_remote::deployment::start(
        &root.join("gateway"),
        test_runtime.manager.clone(),
        host,
        root.clone(),
        Some(0),
    )
    .await
    .unwrap();
    let remote = crate::runtime::attachment::AgentAttachment::connect_gateway(
        endpoint.client().unwrap(),
        agent_session.instance().id().as_str(),
    )
    .await
    .unwrap();
    assert!(remote.pending_interactions().await.unwrap().is_empty());
    let started = agent_session
        .instance()
        .spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest::new(
            "probe",
            "run printf permission-probe",
        ))
        .await
        .unwrap();
    let child_id = AgentInstanceId::new(started.instance_id().as_str());
    let child = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(child) = test_runtime.sessions.get(&child_id) {
                break child;
            }
            let status = agent_session
                .instance()
                .subagent_status(started.instance_id().clone())
                .await
                .unwrap()
                .unwrap();
            assert_ne!(
                status.admission,
                phi_sdk::agents::SubagentAdmission::Rejected,
                "{status:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child association timed out");
    let root_revision = agent_session.instance().snapshot().revision;
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while child.instance().snapshot().pending_interactions.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let local = test_runtime
        .sessions
        .get(&child_id)
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    let attached = test_runtime
        .sessions
        .get(&child_id)
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    let pending = attached.pending_interactions().await.unwrap();
    assert_eq!(pending.len(), 1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !remote
            .pending_interactions()
            .await
            .unwrap()
            .iter()
            .any(|request| request.request_id == pending[0].request_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native root must refresh a child-only permission");
    assert_eq!(agent_session.instance().snapshot().revision, root_revision);
    let workflows = super::preparation::SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let gateway = super::gateway::GatewaySessions::new(
        test_runtime.sessions.clone(),
        workflows,
        test_runtime.preparation.clone(),
        true,
    )
    .with_test_binding(agent_session.clone());
    use phi_remote::RemoteHost;
    let root_id = agent_session.instance().id().clone();
    let scope = gateway.interactions(root_id.clone()).await.unwrap();
    let (_, questions) = scope.iter().find(|(id, _)| id == &child_id).unwrap();
    assert_eq!(questions[0].request_id, pending[0].request_id);
    assert!(scope.iter().any(|(id, _)| id == &root_id));
    let foreign = gateway.create(root.clone()).await.unwrap();
    assert!(
        gateway
            .interactions(AgentInstanceId::new(&foreign.id))
            .await
            .unwrap()
            .iter()
            .all(|(id, _)| id != &child_id)
    );
    let denied = gateway
        .respond(
            AgentInstanceId::new(&foreign.id),
            child_id.clone(),
            pending[0].request_id.clone(),
            InteractionResponse::Permission(PermissionDecision::AllowOnce),
        )
        .await
        .unwrap_err();
    assert_eq!(
        denied.downcast_ref::<phi_remote::protocol::ErrorCode>(),
        Some(&phi_remote::protocol::ErrorCode::Forbidden)
    );
    assert_eq!(
        child.instance().snapshot().pending_interactions[0].request_id,
        pending[0].request_id
    );
    assert!(
        gateway
            .message(
                AgentInstanceId::new(&foreign.id),
                child_id.clone(),
                "do not deliver".into(),
            )
            .await
            .is_err()
    );
    assert!(
        gateway
            .message(
                root_id.clone(),
                root_id.clone(),
                "do not bypass submission".into()
            )
            .await
            .is_err()
    );
    gateway
        .close(AgentInstanceId::new(&foreign.id))
        .await
        .unwrap();
    let readonly = attached.recovery().await.unwrap();
    assert_eq!(
        readonly.snapshot.instance.status,
        AgentInstanceStatus::RequiresAction
    );
    assert_eq!(
        readonly.snapshot.instance.pending_interactions[0].request_id,
        pending[0].request_id
    );
    assert_eq!(
        pending[0].request_id,
        local.pending_interactions().await.unwrap()[0].request_id
    );
    let main = test_runtime
        .sessions
        .get(agent_session.instance().id())
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    assert!(
        main.respond_interaction(
            &pending[0].request_id,
            InteractionResponse::Permission(PermissionDecision::AllowOnce)
        )
        .await
        .is_err()
    );
    crate::interactive::verify_subagent_interaction_scope(&main, &pending[0]).await;
    drop(attached);
    assert_eq!(
        local.pending_interactions().await.unwrap()[0].request_id,
        pending[0].request_id
    );
    let attached = test_runtime
        .sessions
        .get(&child_id)
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    let mut events = attached.subscribe();
    let recovered = attached.recover_events(&mut events).await.unwrap();
    assert_eq!(
        recovered.snapshot.instance.pending_interactions[0].request_id,
        pending[0].request_id
    );
    let answer = attached
        .respond_interaction(
            &pending[0].request_id,
            InteractionResponse::Permission(PermissionDecision::Deny),
        )
        .await
        .unwrap();
    assert_eq!(answer.status, "permission denied");
    assert!(answer.allow_always.is_none());
    assert!(
        attached
            .respond_interaction(
                &pending[0].request_id,
                InteractionResponse::Permission(PermissionDecision::AllowOnce)
            )
            .await
            .is_err()
    );
    let second = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let next = local.pending_interactions().await.unwrap();
            if let Some(request) = next
                .into_iter()
                .find(|request| request.request_id != pending[0].request_id)
            {
                break request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let tool = match &second.request {
        phi_sdk::interaction::InteractionRequest::Permission(permission) => permission.tool.clone(),
        _ => panic!("expected a permission request"),
    };
    let pattern = "printf permission-probe2".to_string();
    test_runtime
        .sessions
        .session_access(&child_id)
        .checkpoint()
        .await
        .unwrap();
    let child_path = crate::session::storage::session_path(&local.session().await.unwrap());
    std::fs::remove_file(&child_path).unwrap();
    std::fs::create_dir(&child_path).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !remote
            .pending_interactions()
            .await
            .unwrap()
            .iter()
            .any(|request| request.request_id == second.request_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        remote
            .pending_interactions()
            .await
            .unwrap()
            .iter()
            .all(|request| request.request_id != pending[0].request_id)
    );
    let root_revision = agent_session.instance().snapshot().revision;
    let answer = remote
        .respond_interaction(
            &second.request_id,
            InteractionResponse::Permission(PermissionDecision::AllowAlways(pattern.clone())),
        )
        .await
        .unwrap();
    assert!(answer.persistence_error.is_some());
    assert_eq!(agent_session.instance().snapshot().revision, root_revision);
    assert!(
        gateway
            .respond(
                root_id.clone(),
                child_id.clone(),
                second.request_id.clone(),
                InteractionResponse::Permission(PermissionDecision::AllowOnce),
            )
            .await
            .is_err()
    );
    assert!(child.snapshot().await.persistence_error.is_some());
    let grant = crate::session::PermissionAllowEntry {
        tool: tool.clone(),
        pattern: pattern.clone(),
    };
    for attachment in [&local, &attached] {
        let recovery = attachment.recovery().await.unwrap();
        assert!(recovery.snapshot.persistence_error.is_some());
        assert_eq!(recovery.session.permission_allowlist, vec![grant.clone()]);
    }
    std::fs::remove_dir(&child_path).unwrap();
    attached.checkpoint().await.unwrap();
    assert_eq!(
        local.recovery().await.unwrap().session.permission_allowlist,
        vec![grant.clone()]
    );
    assert_eq!(
        crate::session::storage::load_session(child_path.to_str().unwrap())
            .unwrap()
            .permission_allowlist,
        vec![grant]
    );
    assert!(
        local
            .respond_interaction(
                &second.request_id,
                InteractionResponse::Permission(PermissionDecision::AllowAlways(
                    "duplicate".into()
                ))
            )
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !local
            .pending_interactions()
            .await
            .unwrap()
            .iter()
            .any(|request| request.request_id != second.request_id)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    for reply in [
        super::session::tests::PermissionReply::Session,
        super::session::tests::PermissionReply::Attachment,
    ] {
        super::session::tests::assert_applied_permission_survives_waiter_loss(
            test_runtime.clone(),
            child.clone(),
            reply,
            false,
        )
        .await;
        assert!(
            agent_session
                .session()
                .await
                .permission_allowlist
                .is_empty()
        );
    }
    let question = local.pending_interactions().await.unwrap().remove(0);
    let phi_sdk::interaction::InteractionRequest::Permission(permission) = &question.request else {
        panic!("expected permission request");
    };
    let response = || {
        InteractionResponse::Permission(PermissionDecision::AllowAlways(permission.input.clone()))
    };
    let (local_answer, remote_answer) = tokio::join!(
        local.respond_interaction(&question.request_id, response()),
        gateway.respond(
            root_id.clone(),
            child_id.clone(),
            question.request_id.clone(),
            response()
        ),
    );
    assert_ne!(local_answer.is_ok(), remote_answer.is_ok());
    assert!(
        child
            .session()
            .await
            .permission_allowlist
            .iter()
            .any(|grant| grant.tool == permission.tool && grant.pattern == permission.input)
    );
    assert!(
        agent_session
            .session()
            .await
            .permission_allowlist
            .is_empty()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while !child
            .instance()
            .snapshot()
            .pending_interactions
            .iter()
            .any(|pending| pending.request_id != question.request_id)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for allow_once in [false, true] {
        super::session::tests::assert_competing_permission_answers(&local, &attached, allow_once)
            .await;
        assert!(
            agent_session
                .session()
                .await
                .permission_allowlist
                .is_empty()
        );
    }
    let before_queue = local.recovery().await.unwrap();
    let mut queue_events = [local.subscribe(), attached.subscribe()];
    let queued_prompt = || phi_sdk::turn::TurnPrompt {
        tool_policy: Default::default(),
        prompt: "identical queued prompt".into(),
        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
        injected_context: Vec::new(),
        images: Vec::new(),
    };
    let attachments = [&local, &attached];
    let steer_count = attachments.len();
    let mut queued_ids = Vec::new();
    for dispatch in [
        phi_sdk::turn::SubmissionDispatch::Steer,
        phi_sdk::turn::SubmissionDispatch::FollowUp,
    ] {
        for attachment in attachments {
            let receipt = attachment
                .dispatch(queued_prompt(), dispatch)
                .await
                .unwrap();
            assert!(receipt.notices.is_empty());
            let phi_sdk::turn::SubmissionDisposition::Queued {
                submission_id,
                prompt,
                dispatch: accepted_dispatch,
                ..
            } = receipt.disposition
            else {
                panic!("expected queued receipt")
            };
            assert_eq!(prompt, "identical queued prompt");
            assert_eq!(accepted_dispatch, dispatch);
            assert!(!queued_ids.contains(&submission_id));
            queued_ids.push(submission_id);
        }
    }
    for attachment in attachments {
        let result = attachment.submit(queued_prompt()).await;
        let Err(phi_sdk::agent_instance::AgentInstanceError::Operation { source }) = result else {
            panic!("busy input must fail without queueing");
        };
        assert!(matches!(
            source.downcast_ref::<phi_sdk::agent_instance::AgentInstanceError>(),
            Some(phi_sdk::agent_instance::AgentInstanceError::Turn(
                phi_sdk::turn::PhiTurnError::Busy
            ))
        ));
    }
    assert_eq!(
        local.recovery().await.unwrap().pending_submission_ids,
        queued_ids
    );
    assert_eq!(
        attached
            .recover_events(&mut events)
            .await
            .unwrap()
            .pending_submission_ids,
        queued_ids
    );
    assert_eq!(
        serde_json::to_value(local.recovery().await.unwrap().session.transcript).unwrap(),
        serde_json::to_value(before_queue.session.transcript).unwrap()
    );
    let mut expected_events = Vec::new();
    for (consumed, attachment) in attachments.into_iter().enumerate() {
        let pending = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pending) = attachment.pending_interactions().await.unwrap().pop() {
                    break pending;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("next permission timed out");
        attachment
            .respond_interaction(
                &pending.request_id,
                InteractionResponse::Permission(PermissionDecision::Deny),
            )
            .await
            .unwrap();
        expected_events.push(("closed".to_string(), pending.request_id));
        expected_events.push(("injected".to_string(), queued_ids[consumed].to_string()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while local.recovery().await.unwrap().pending_submission_ids
                != queued_ids[consumed + 1..]
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Steer consumption timed out");
    }
    let recovery = attached.recover_events(&mut events).await.unwrap();
    assert_eq!(recovery.pending_submission_ids, queued_ids[steer_count..]);
    for stream in &mut queue_events {
        let observed = tokio::time::timeout(Duration::from_secs(5), async {
            let mut observed = Vec::new();
            while observed.len() < expected_events.len() {
                match stream.recv().await.unwrap() {
                    crate::runtime::attachment::AgentEvent::InteractionClosed { request_id }
                        if expected_events
                            .contains(&("closed".to_string(), request_id.clone())) =>
                    {
                        observed.push(("closed".to_string(), request_id));
                    }
                    crate::runtime::attachment::AgentEvent::Phi(frame) => {
                        assert_eq!(frame.session_id, child_id.as_str());
                        if let phi_sdk::PhiEvent::UserInputInjected {
                            submission_id: Some(id),
                            prompt,
                            ..
                        } = frame.event
                        {
                            assert_eq!(prompt, "identical queued prompt");
                            observed.push(("injected".to_string(), id.to_string()));
                        }
                    }
                    crate::runtime::attachment::AgentEvent::Lagged(count) => {
                        panic!("queue events lagged: {count}")
                    }
                    _ => {}
                }
            }
            observed
        })
        .await
        .expect("queued commit events timed out");
        assert_eq!(observed, expected_events);
    }
    assert_eq!(recovery.session.transcript.current_branch().iter().filter(|entry| matches!(
        &entry.kind,
        phi_sdk::transcript::TranscriptEntryKind::Message {
            message: phi_sdk::transcript::AgentMessage::User { content, .. }
        } if content.iter().any(|block| matches!(block,
            phi_sdk::transcript::UserContentBlock::Text { text } if text.as_str() == "identical queued prompt"))
    )).count(), steer_count);
    tokio::time::timeout(Duration::from_secs(5), async {
        while attached.pending_interactions().await.unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("post-Steer permission timed out");
    attached.checkpoint().await.unwrap();
    let persisted = attached.recovery().await.unwrap();
    let saved_queue = crate::session::storage::load_session(child_path.to_str().unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(saved_queue.transcript).unwrap(),
        serde_json::to_value(persisted.session.transcript).unwrap()
    );
    let message = gateway
        .message(root_id.clone(), child_id.clone(), "coordinate".into())
        .await
        .unwrap();
    assert_eq!(message.message().sender, root_id);
    assert_eq!(message.message().recipient, child_id);
    super::session::tests::assert_applied_permission_survives_waiter_loss(
        test_runtime.clone(),
        child.clone(),
        super::session::tests::PermissionReply::Attachment,
        true,
    )
    .await;
    let old_registration = child.instance().registration_id();
    let old_grants = child.session().await.permission_allowlist;
    assert!(!old_grants.is_empty());
    tokio::time::timeout(
        Duration::from_secs(10),
        test_runtime
            .sessions
            .stop(agent_session.instance().id(), SessionEndReason::Quit),
    )
    .await
    .expect("subtree stop with a pending permission timed out")
    .unwrap();
    assert!(attached.pending_interactions().await.unwrap().is_empty());
    assert!(local.pending_interactions().await.unwrap().is_empty());
    assert!(attached.recovery().await.is_err());
    assert!(local.recovery().await.is_err());
    let stopped = child.instance().view().await.unwrap();
    assert!(stopped.pending_submission_ids.is_empty());
    let saved = crate::session::storage::load_session(child_path.to_str().unwrap()).unwrap();
    assert_eq!(saved.permission_allowlist, old_grants);
    assert_eq!(
        serde_json::to_value(saved.transcript).unwrap(),
        serde_json::to_value(stopped.transcript).unwrap()
    );
    assert!(test_runtime.sessions.find(&child_id).unwrap().is_none());
    assert!(test_runtime.sessions.list().await.unwrap().is_empty());
    assert!(
        attached
            .respond_interaction(
                &pending[0].request_id,
                InteractionResponse::Permission(PermissionDecision::AllowOnce)
            )
            .await
            .is_err()
    );
    let mut reused_session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    reused_session.id = child_id.as_str().into();
    reused_session.working_dir = root.to_string_lossy().into();
    let reused = Box::pin(
        {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(reused_session, false),
    )
    .await
    .unwrap();
    assert_ne!(reused.instance().registration_id(), old_registration);
    assert!(
        child
            .respond_interaction(
                "stale".into(),
                InteractionResponse::Permission(PermissionDecision::AllowAlways("stale".into()))
            )
            .await
            .is_err()
    );
    assert!(
        child
            .allow_session_tools(&[("bash".into(), "stale".into())])
            .await
            .is_err()
    );
    assert!(
        test_runtime
            .sessions
            .finalize_agent_session(&child)
            .await
            .unwrap()
            .is_none()
    );
    assert!(Arc::ptr_eq(
        &test_runtime.sessions.get(&child_id).unwrap(),
        &reused
    ));
    assert!(reused.session().await.permission_allowlist.is_empty());
    assert_eq!(child.session().await.permission_allowlist, old_grants);
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    provider.abort();
    drop(remote);
    endpoint.client().unwrap().stop().await.unwrap();
    transport.close().await.unwrap();
    drop(registration);
    std::fs::remove_dir_all(root).unwrap();
}
