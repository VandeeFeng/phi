use super::*;

#[tokio::test]
async fn temporary_thinking_and_model_selection_record_actual_switch_snapshots() {
    use phi_sdk::models::ThinkingLevel;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (root, policy, _) = session_test_runtime();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_path = root.join("models.json");
    std::fs::write(&model_path, serde_json::json!({"providers":{"local":{
        "api":"openai-completions","baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
        "models":{
            "test":{"contextWindow":128000,"reasoning":true},
            "other":{"contextWindow":128000,"reasoning":true}
        }
    }}}).to_string()).unwrap();
    let mut configuration = policy.configuration.snapshot();
    configuration.model_catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("selection-catalog.json"),
            user_paths: vec![model_path],
            offline: true,
        })
        .unwrap(),
    );
    configuration.settings.default_thinking_level = Some("low".into());
    configuration.settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
        enabled: false,
        ..Default::default()
    });
    let catalog = configuration.model_catalog.clone();
    let resources = RuntimeResources::new(
        phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()),
        Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
        catalog,
        phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap(),
    );
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let agent = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap()
    .start_session(session, true)
    .await
    .unwrap();
    let access = test_runtime.sessions.session_access(agent.instance().id());
    let attachment = crate::runtime::attachment::AgentAttachment::new(
        &agent,
        access.clone(),
        test_runtime.preparation.clone(),
    );
    let initial_settings = agent.runtime().source_settings().unwrap();
    let (send_request, mut requests) = tokio::sync::mpsc::channel(1);
    let provider = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let request = loop {
                let mut buffer = [0; 4096];
                let count = connection.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&buffer[..count]);
                let Some(header_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                let body = &bytes[header_end + 4..];
                if body.len() >= length {
                    break serde_json::from_slice::<serde_json::Value>(&body[..length]).unwrap();
                }
            };
            let (release, wait) = tokio::sync::oneshot::channel();
            send_request.send((request.clone(), release)).await.unwrap();
            wait.await.unwrap();
            let reply = serde_json::json!({"id":"completion","model":request["model"],
                "choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]});
            let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
            connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    for (index, model, level) in [
        (0, "test", ThinkingLevel::Low),
        (1, "other", ThinkingLevel::High),
        (2, "test", ThinkingLevel::Low),
    ] {
        let before = agent.session().await;
        if index != 0 {
            attachment
                .configure_runtime(RuntimeSelection::Model {
                    selection: format!("model:local:{model}"),
                })
                .await
                .unwrap();
            attachment
                .configure_runtime(RuntimeSelection::ThinkingLevel { level })
                .await
                .unwrap();
        }
        let selected = agent.recovery().await.unwrap();
        assert_eq!(selected.session.current_switch, "default");
        assert_eq!(selected.session.model, model);
        assert_eq!(
            selected.declarations.capabilities.thinking_level.as_deref(),
            Some(level.as_str())
        );
        assert!(
            selected
                .declarations
                .capabilities
                .supported_thinking_levels
                .contains(&level)
        );
        assert_eq!(selected.session.entries.len(), before.entries.len());
        assert_eq!(
            selected.session.switch_snapshots.len(),
            before.switch_snapshots.len()
        );
        let generation = selected.snapshot.runtime_generation_id;
        attachment
            .configure_runtime(RuntimeSelection::ThinkingLevel { level })
            .await
            .unwrap();
        assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
        let settings = agent.runtime().source_settings().unwrap();
        assert!(Arc::ptr_eq(&settings, &initial_settings));
        assert_eq!(settings.default_thinking_level.as_deref(), Some("low"));
        attachment
            .submit(phi_sdk::turn::TurnPrompt {
                prompt: format!("request {index}"),
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        let (request, release) =
            tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(request["model"], model);
        assert!(
            attachment
                .configure_runtime(RuntimeSelection::ThinkingLevel {
                    level: ThinkingLevel::Medium
                })
                .await
                .is_err()
        );
        assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
        release.send(()).unwrap();
        let view = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            agent.instance().wait_for_quiescence(None),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(view.transcript.entries.len(), before.entries.len() + 2);
        assert_eq!(
            view.transcript.switch_snapshots.len(),
            if index == 0 { 1 } else { 2 }
        );
        let crate::session::TranscriptEntryKind::Message {
            message:
                crate::session::AgentMessage::Assistant {
                    switch: Some(snapshot_id),
                    ..
                },
        } = &view.transcript.entries.last().unwrap().kind
        else {
            panic!("assistant must reference the actual execution snapshot");
        };
        let record = view
            .transcript
            .switch_snapshots
            .iter()
            .find(|record| &record.id == snapshot_id)
            .unwrap();
        assert_eq!(record.switch.name, "default");
        assert_eq!(record.switch.model, model);
        assert_eq!(
            record.switch.thinking_level.as_deref(),
            Some(level.as_str())
        );
        assert_eq!(
            snapshot_id.as_str(),
            if index == 1 { "default-2" } else { "default-1" }
        );
    }
    provider.await.unwrap();
    let clamped = attachment
        .configure_runtime(RuntimeSelection::ThinkingLevel {
            level: ThinkingLevel::Max,
        })
        .await
        .unwrap();
    assert_eq!(
        clamped.declarations.capabilities.thinking_level.as_deref(),
        Some("high")
    );
    let generation = clamped.snapshot.runtime_generation_id;
    attachment
        .configure_runtime(RuntimeSelection::ThinkingLevel {
            level: ThinkingLevel::Xhigh,
        })
        .await
        .unwrap();
    assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
    assert_eq!(agent.session().await.switch_snapshots.len(), 2);
    let path = crate::session::storage::session_path(&agent.session().await);
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    let saved = crate::session::storage::load_session(&path.to_string_lossy()).unwrap();
    assert_eq!(saved.switch_snapshots.len(), 2);
    saved.transcript.validate_switch_snapshots().unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn semantic_runtime_selection_preserves_identity_grants_and_branch_settings() {
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.settings.permission = Some(serde_json::json!({"bash":"ask"}));
    configuration.settings.no_tools = Some(false);
    let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = prompts.clone();
    configuration.hooks = vec![Hook::build_system_prompt(move |event| {
        captured.lock().unwrap().push(event.system_prompt.clone());
        async move {
            Ok(phi_sdk::lifecycle::BuildSystemPromptOutcome {
                system_prompt: event.system_prompt,
            })
        }
    })];
    configuration.settings.switches = Some(vec![
        crate::config::SwitchConfig {
            name: "plan".into(),
            alias: Some("p".into()),
            provider: Some("local".into()),
            model: Some("test".into()),
            thinking_level: Some("high".into()),
            mode: Some("plan".into()),
            prompt: Some("custom plan instructions".into()),
        },
        crate::config::SwitchConfig {
            name: "default".into(),
            prompt: Some("loaded default instructions".into()),
            ..Default::default()
        },
        crate::config::SwitchConfig {
            name: "invalid".into(),
            model: Some("missing".into()),
            ..Default::default()
        },
    ]);
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    std::fs::write(
        root.join(".phi/settings.json"),
        serde_json::to_vec(&serde_json::json!({
            "permission": {"bash":"ask"}, "switches": configuration.settings.switches,
        }))
        .unwrap(),
    )
    .unwrap();
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.transcript.append_user_message("preserved history");
    session.transcript.append_assistant_message(
        None,
        None,
        Vec::new(),
        "test".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "local".into(),
        Default::default(),
        "stop".into(),
    );
    let agent = Box::pin(
        {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(session, true),
    )
    .await
    .unwrap();
    agent
        .allow_session_tools(&[("bash".into(), "printf own-grant".into())])
        .await
        .unwrap();
    let id = agent.instance().id().clone();
    let registration = agent.instance().registration_id();
    let initial = agent.recovery().await.unwrap();
    let default_factory = test_runtime.preparation.runtime_factory().unwrap();
    for selection in [
        RuntimeSelection::Switch {
            name: "missing".into(),
        },
        RuntimeSelection::Switch {
            name: "invalid".into(),
        },
        RuntimeSelection::Model {
            selection: "model:local:missing".into(),
        },
    ] {
        assert!(
            Box::pin(async {
                test_runtime
                    .preparation
                    .configure_runtime(&test_runtime.sessions.session_access(&id), selection)
                    .await
            })
            .await
            .is_err()
        );
        assert_eq!(
            agent.snapshot().await.runtime_generation_id,
            initial.snapshot.runtime_generation_id
        );
        assert_eq!(
            agent.session().await.transcript.entries.len(),
            initial.session.transcript.entries.len()
        );
    }
    let accepted = Box::pin(async {
        test_runtime
            .preparation
            .configure_runtime(
                &test_runtime.sessions.session_access(&id),
                RuntimeSelection::Switch { name: "p".into() },
            )
            .await
    })
    .await
    .unwrap();
    assert_eq!(
        accepted.declarations.generation_id,
        accepted.snapshot.runtime_generation_id
    );
    assert_eq!(
        accepted.declarations.capabilities.thinking_level.as_deref(),
        Some("high")
    );
    let selected = accepted.session;
    assert!(accepted.snapshot.persistence_error.is_none());
    assert_eq!(selected.current_switch, "plan");
    assert_eq!(
        selected.permission_allowlist,
        initial.session.permission_allowlist
    );
    assert_eq!(agent.instance().registration_id(), registration);
    assert!(crate::runtime::session::tests::session_command_allowed(&agent, "own-grant").await);
    assert_eq!(
        selected.transcript.entries.len(),
        initial.session.transcript.entries.len()
    );
    let crate::runtime::RuntimeView {
        session: selected, ..
    } = Box::pin(async {
        test_runtime
            .preparation
            .configure_runtime(
                &test_runtime.sessions.session_access(&id),
                RuntimeSelection::Model {
                    selection: "model:local:test".into(),
                },
            )
            .await
    })
    .await
    .unwrap();
    assert_eq!(selected.current_switch, "plan");
    assert_eq!(
        selected.transcript.entries.len(),
        initial.session.transcript.entries.len()
    );
    assert_eq!(agent.runtime().runtime_config().agent_mode.as_str(), "plan");
    agent.ready_hook_notices().await;
    assert!(
        prompts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("custom plan instructions")
    );
    std::fs::write(root.join(".phi/settings.json"), br#"{"permission":{"bash":"ask"},"switches":[{"name":"default","prompt":"saved default instructions"}]}"#).unwrap();
    let path = crate::session::storage::session_path(&selected);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let selected_view = test_runtime
        .preparation
        .configure_runtime(
            &test_runtime.sessions.session_access(&id),
            RuntimeSelection::Switch {
                name: "default".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(selected_view.session.current_switch, "default");
    assert!(selected_view.snapshot.persistence_error.is_some());
    let accepted = agent.session().await;
    assert_eq!(accepted.current_switch, "default");
    assert_eq!(
        accepted.permission_allowlist,
        initial.session.permission_allowlist
    );
    assert_eq!(accepted.entries.len(), selected.entries.len());
    agent.ready_hook_notices().await;
    assert!(
        !prompts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("custom plan instructions")
    );
    assert!(
        prompts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("loaded default instructions")
    );
    std::fs::remove_dir(&path).unwrap();
    test_runtime
        .sessions
        .session_access(&id)
        .checkpoint()
        .await
        .unwrap();
    let saved = crate::session::storage::load_session(path.to_str().unwrap()).unwrap();
    assert_eq!(saved.current_switch, accepted.current_switch);
    assert_eq!(saved.leaf_id, accepted.leaf_id);
    assert!(Arc::ptr_eq(
        &default_factory,
        &test_runtime.preparation.runtime_factory().unwrap()
    ));
    test_runtime
        .sessions
        .stop(&id, SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn accepted_reload_result_survives_a_newer_generation_before_response() {
    use std::time::Duration;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let hook = Hook::session_end({
        let entered = entered.clone();
        let release = release.clone();
        move |event| {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                if matches!(event.reason, SessionEndReason::Reload) {
                    entered.notify_one();
                    release.notified().await;
                    return Ok(vec![phi_sdk::hooks::HookNotice::Info {
                        message: "previous generation ended".into(),
                    }]);
                }
                Ok(Vec::new())
            }
        }
    });
    let (root, policy, test_runtime) = session_test_runtime_with_hooks(vec![hook]);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let agent = Box::pin(
        {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(session, false),
    )
    .await
    .unwrap();
    let id = agent.instance().id().clone();
    let configuration = policy.configuration.snapshot();
    let current = agent.session().await;
    let mut options = configuration.runtime_build_options(&current);
    options.phi.runtime_cfg.default_thinking_level = Some("high".into());
    let prepared = test_runtime.preparation.prepare(options).await.unwrap();
    let waiter = tokio::spawn({
        let test_runtime = test_runtime.clone();
        let id = id.clone();
        async move {
            test_runtime
                .preparation
                .reload(
                    &test_runtime.sessions.session_access(&id),
                    current,
                    prepared,
                    None,
                )
                .await
                .unwrap()
        }
    });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let accepted_generation = agent.snapshot().await.runtime_generation_id;
    let mut newer_session = agent.session().await;
    newer_session.current_switch = "newer".into();
    let mut options = configuration.runtime_build_options(&newer_session);
    options.phi.runtime_cfg.default_thinking_level = Some("low".into());
    let prepared = test_runtime.preparation.prepare(options).await.unwrap();
    let newer = agent
        .replace_runtime(prepared, newer_session, None, false)
        .await
        .unwrap();
    assert_ne!(newer.snapshot.runtime_generation_id, accepted_generation);
    release.notify_one();
    let accepted = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .unwrap()
        .unwrap();
    let notices = test_runtime.sessions.take_notices().unwrap();
    assert!(matches!(notices.as_slice(), [notice]
        if notice.agent_id == id
            && matches!(notice.hook_notices.as_slice(), [phi_sdk::hooks::HookNotice::Info { message }]
                if message == "previous generation ended")));
    assert!(test_runtime.sessions.take_notices().unwrap().is_empty());
    assert_eq!(accepted.snapshot.runtime_generation_id, accepted_generation);
    assert_eq!(accepted.declarations.generation_id, accepted_generation);
    assert_eq!(
        accepted.declarations.capabilities.thinking_level.as_deref(),
        Some("high")
    );
    assert_ne!(accepted.session.current_switch, "newer");
    let recovery = agent.recovery().await.unwrap();
    assert_eq!(
        recovery.snapshot.runtime_generation_id,
        newer.snapshot.runtime_generation_id
    );
    assert_eq!(
        recovery.declarations.generation_id,
        newer.snapshot.runtime_generation_id
    );
    assert_eq!(
        recovery.declarations.capabilities.thinking_level.as_deref(),
        Some("low")
    );
    assert_eq!(recovery.session.current_switch, "newer");
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn selections_keep_loaded_switch_definitions() {
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
        name: "review".into(),
        alias: Some("accepted".into()),
        thinking_level: Some("low".into()),
        mode: Some("default".into()),
        prompt: Some("accepted prompt".into()),
        ..Default::default()
    }]);
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.current_switch = "review".into();
    session.working_dir = root.to_string_lossy().into();
    let agent = Box::pin(workflows.start_session(session, true))
        .await
        .unwrap();
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    std::fs::write(root.join(".phi/settings.json"), serde_json::to_vec(&serde_json::json!({
        "switches":[{"name":"review","alias":"unloaded","thinking_level":"high","mode":"plan","prompt":"new prompt"}]
    })).unwrap()).unwrap();
    let access = test_runtime.sessions.session_access(agent.instance().id());
    let model = Box::pin(test_runtime.preparation.configure_runtime(
        &access,
        RuntimeSelection::Model {
            selection: "test".into(),
        },
    ))
    .await
    .unwrap();
    let assert_loaded_arguments = |declarations: &crate::runtime::RuntimeDeclarations| {
        let arguments = declarations
            .commands
            .iter()
            .find(|command| command.name == "switch")
            .unwrap()
            .arguments
            .as_ref()
            .unwrap();
        let review = arguments
            .values
            .iter()
            .find(|argument| argument.value == "review")
            .unwrap();
        assert_eq!(review.alias.as_deref(), Some("accepted"));
    };
    assert_loaded_arguments(&model.declarations);
    assert_eq!(
        model.declarations.capabilities.thinking_level.as_deref(),
        Some("low")
    );
    assert_eq!(
        agent.runtime().phi().agent_mode(),
        phi_sdk::config::AgentMode::Default
    );
    let fresh = Box::pin(workflows.new_session(agent.instance().id()))
        .await
        .unwrap();
    assert_loaded_arguments(&fresh.accepted.declarations);
    assert_eq!(
        fresh
            .accepted
            .declarations
            .capabilities
            .thinking_level
            .as_deref(),
        Some("low")
    );
    let agent = test_runtime.sessions.get(fresh.attachment.id()).unwrap();
    assert_eq!(
        agent
            .runtime()
            .runtime_config()
            .mode_prompt_override
            .as_deref(),
        Some("accepted prompt")
    );
    let access = test_runtime.sessions.session_access(agent.instance().id());
    let selected = Box::pin(test_runtime.preparation.configure_runtime(
        &access,
        RuntimeSelection::Switch {
            name: "review".into(),
        },
    ))
    .await
    .unwrap();
    assert_loaded_arguments(&selected.declarations);
    assert_eq!(
        selected.declarations.capabilities.thinking_level.as_deref(),
        Some("low")
    );
    assert_eq!(
        agent.runtime().phi().agent_mode(),
        phi_sdk::config::AgentMode::Default
    );
    assert_eq!(
        agent
            .runtime()
            .runtime_config()
            .mode_prompt_override
            .as_deref(),
        Some("accepted prompt")
    );
    let next = Box::pin(workflows.new_session(agent.instance().id()))
        .await
        .unwrap();
    assert_loaded_arguments(&next.accepted.declarations);
    assert_eq!(
        next.accepted
            .declarations
            .capabilities
            .thinking_level
            .as_deref(),
        Some("low")
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
