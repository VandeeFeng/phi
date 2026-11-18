use super::*;

#[tokio::test(start_paused = true)]
async fn print_teardown_cancels_background_hooks_but_runs_session_end() {
    use phi_sdk::hooks::HookNotice;
    use phi_sdk::lifecycle::{NotificationEvent, NotificationKind};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::Notify;

    for print in [true, false] {
        for failed in [false, true] {
            let entered = Arc::new(Notify::new());
            let completed = Arc::new(AtomicBool::new(false));
            let hooks = vec![
                Hook::notification({
                    let entered = entered.clone();
                    let completed = completed.clone();
                    move |_| {
                        let entered = entered.clone();
                        let completed = completed.clone();
                        async move {
                            entered.notify_one();
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            completed.store(true, Ordering::SeqCst);
                            Ok(())
                        }
                    }
                }),
                Hook::session_end(|_| async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Ok(vec![HookNotice::Info {
                        message: "session ended".into(),
                    }])
                }),
            ];
            let (root, policy, runtime) = session_test_runtime_with_hooks(hooks);
            let mut configuration = policy.configuration.snapshot();
            configuration.cli = Arc::new(if print {
                Cli::parse_from(["phi", "-p"])
            } else {
                Cli::parse_from(["phi"])
            });
            let session = crate::session::new("local", "test", 128_000, &root);
            let agent = runtime
                .sessions
                .create(
                    &runtime.preparation,
                    session.clone(),
                    configuration.runtime_build_options(&session),
                    SessionStartReason::Startup,
                    false,
                )
                .await
                .unwrap();
            let resources = agent.runtime().sdk_resources();
            resources.notify(NotificationEvent {
                kind: NotificationKind::Message,
                level: "info".into(),
                message: "background work".into(),
                tool_name: None,
            });
            entered.notified().await;
            let started = tokio::time::Instant::now();
            let result = crate::finalize_sessions(
                &runtime.sessions,
                if failed {
                    Err(anyhow::anyhow!("execution failed"))
                } else {
                    Ok(Some("answer".into()))
                },
            )
            .await;
            assert_eq!(result.is_err(), failed);
            assert_eq!(completed.load(Ordering::SeqCst), !print);
            assert_eq!(resources.running_hook_count(), 0);
            assert_eq!(
                started.elapsed(),
                if print {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs(1)
                }
            );
            let notices: Vec<_> = runtime
                .sessions
                .take_notices()
                .unwrap()
                .into_iter()
                .flat_map(|notices| notices.hook_notices)
                .collect();
            assert!(notices.contains(&HookNotice::Info {
                message: "session ended".into(),
            }));
            assert!(!notices.iter().any(|notice| matches!(
                notice,
                HookNotice::Warning { message } if message.contains("timed out")
            )));
            let cancelled = notices
                .iter()
                .filter(|notice| matches!(notice, HookNotice::Cancelled { event } if event == "Notification"))
                .count();
            assert_eq!(cancelled, usize::from(print));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
}

#[tokio::test]
async fn unloaded_builtin_mcp_does_not_inherit_a_shared_pool() {
    let (root, policy, _) = session_test_runtime();
    {
        let mut current = policy.configuration.generation.write().unwrap();
        current.settings.extensions = Some(vec!["-builtin:mcp".into()]);
        current.settings.mcp = Some(true);
        current.settings.mcp_servers = serde_json::from_value(serde_json::json!({
            "fixture": {"command":"must-not-start", "lifecycle":"eager"}
        }))
        .unwrap();
    }
    let current = policy.configuration.snapshot();
    let manager = phi_sdk::extension::mcp::McpClientManager::load_with_settings(
        &Default::default(),
        Default::default(),
    );
    let resources = AgentRuntimeFactory::resources_for(
        &current.cli,
        &current.settings,
        &current.settings_manager,
        current.model_catalog.clone(),
        &root,
    )
    .unwrap();
    assert!(
        AgentRuntimeFactory::mcp_manager(
            &current.cli,
            &current.settings,
            resources.sandbox.clone()
        )
        .is_none()
    );
    let resources = resources.mcp_manager(Some(manager));
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let agent = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap()
    .start_session(session, false)
    .await
    .unwrap();
    assert!(agent.runtime().mcp_manager().is_none());
    assert!(agent.runtime().mcp_contracts.is_none());
    let commands = agent.runtime().commands();
    assert!(
        !commands
            .iter()
            .any(|command| command.name.starts_with("mcp"))
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn protected_url_mcp_is_blocked_before_connection() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let (root, policy, _) = session_test_runtime();
    {
        let mut current = policy.configuration.generation.write().unwrap();
        current.cli = Arc::new(Cli::parse_from(["phi", "--yolo", "--rpc"]));
        current.settings.mcp_servers = Some(serde_json::from_value(serde_json::json!({
            "external": {"url": format!("http://{}", listener.local_addr().unwrap()), "lifecycle": "eager"}
        })).unwrap());
    }
    let current = policy.configuration.snapshot();
    let resources = AgentRuntimeFactory::resources_for(
        &current.cli,
        &current.settings,
        &current.settings_manager,
        current.model_catalog.clone(),
        &root,
    )
    .unwrap();
    let manager = AgentRuntimeFactory::mcp_manager(
        &current.cli,
        &current.settings,
        resources.sandbox.clone(),
    );
    if let Some(manager) = &manager {
        manager.start().await;
    }
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(
        resources.mcp_manager(manager),
    )));
    policy.install(&test_runtime.preparation);
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
    let attachment = test_runtime
        .sessions
        .get(&id)
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    assert_eq!(attachment.mcp_servers().await.unwrap()[0].status, "blocked");
    assert!(
        attachment
            .reconnect_mcp("external")
            .await
            .unwrap_err()
            .to_string()
            .contains("URL MCP is blocked")
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn mcp_save_enforces_process_write_policy() {
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.cli = Arc::new(Cli::parse_from(["phi", "--yolo", "--rpc"]));
    *policy.configuration.generation.write().unwrap() = configuration;
    policy.install(&test_runtime.preparation);
    let id = AgentInstanceId::new("missing-agent");
    let factory = test_runtime.preparation.runtime_factory().unwrap();
    let error = test_runtime
        .preparation
        .clone()
        .save_mcp_tools(&test_runtime.sessions.session_access(&id), Vec::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("read-only"), "{error}");
    let _agent = {
        policy.install(&test_runtime.preparation);
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
    }
    .create_session(CreateSessionRequest {
        provider: None,
        model: None,
        switch: None,
        persist: false,
    })
    .await
    .unwrap();
    policy
        .configuration
        .generation
        .write()
        .unwrap()
        .settings
        .trust_remote = true;
    let error = test_runtime
        .preparation
        .clone()
        .save_mcp_tools(&test_runtime.sessions.session_access(&id), Vec::new())
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("read-only"), "{error}");
    assert!(Arc::ptr_eq(
        &factory,
        &test_runtime.preparation.runtime_factory().unwrap()
    ));
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn reload_rejects_invalid_target_config_without_changing_generations() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
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
    agent
        .allow_session_tools(&[("bash".into(), "printf own-grant".into())])
        .await
        .unwrap();
    let before = agent.session().await;
    let generation = agent.snapshot().await.runtime_generation_id;
    let factory = test_runtime.preparation.runtime_factory().unwrap();
    let configuration = policy.configuration.snapshot().model_catalog;
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    std::fs::write(
        root.join(".phi/settings.json"),
        r#"{"permission":{"bash":"invalid"}}"#,
    )
    .unwrap();
    let error = Box::pin(async {
        test_runtime
            .preparation
            .reload_resources(&test_runtime.sessions.session_access(agent.instance().id()))
            .await
    })
    .await
    .unwrap_err();
    assert!(error.to_string().contains("permission.bash"), "{error}");
    assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
    assert!(Arc::ptr_eq(
        &factory,
        &test_runtime.preparation.runtime_factory().unwrap()
    ));
    assert!(Arc::ptr_eq(
        &configuration,
        &policy.configuration.snapshot().model_catalog
    ));
    let after = agent.session().await;
    assert_eq!(after.permission_allowlist, before.permission_allowlist);
    assert_eq!(after.model, before.model);
    assert_eq!(after.transcript.leaf_id, before.transcript.leaf_id);
    test_runtime
        .sessions
        .stop(agent.instance().id(), SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn startup_admission_owns_preparation_and_preserves_loaded_grants() {
    for print in [false, true] {
        let (root, policy, test_runtime) = session_test_runtime();
        let starts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = starts.clone();
        let mut configuration = policy.configuration.snapshot();
        if print {
            configuration.cli = Arc::new(Cli::parse_from(["phi", "--print"]));
        }
        configuration.settings.no_tools = Some(false);
        configuration.settings.permission = Some(serde_json::json!({"bash":"ask"}));
        configuration.hooks.push(Hook::session_start(move |event| {
            observed
                .lock()
                .unwrap()
                .push(matches!(event.reason, SessionStartReason::Startup));
            async { Ok(Vec::new()) }
        }));
        *policy.configuration.generation.write().unwrap() = configuration.clone();
        policy.install(&test_runtime.preparation);
        for loaded in [false, true] {
            let mut session =
                crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            if loaded {
                session.transcript.append_user_message("loaded input");
                session
                    .permission_allowlist
                    .push(crate::session::PermissionAllowEntry {
                        tool: "bash".into(),
                        pattern: "printf startup-grant".into(),
                    });
            }
            let agent = Box::pin(
                {
                    policy.install(&test_runtime.preparation);
                    crate::runtime::preparation::SessionWorkflows::new(
                        test_runtime.sessions.clone(),
                        test_runtime.preparation.clone(),
                    )
                    .unwrap()
                }
                .start_session(session.clone(), false),
            )
            .await
            .unwrap();
            assert_eq!(
                agent.session().await.permission_allowlist,
                session.permission_allowlist
            );
            assert!(
                Box::pin(
                    {
                        policy.install(&test_runtime.preparation);
                        crate::runtime::preparation::SessionWorkflows::new(
                            test_runtime.sessions.clone(),
                            test_runtime.preparation.clone(),
                        )
                        .unwrap()
                    }
                    .start_session(session.clone(), false)
                )
                .await
                .is_err()
            );
            assert!(Arc::ptr_eq(
                &agent,
                &test_runtime.sessions.get(agent.instance().id()).unwrap()
            ));
            assert_eq!(
                agent.instance().transcript().await.unwrap().leaf_id,
                session.transcript.leaf_id
            );
            assert_eq!(
                crate::runtime::session::tests::session_command_allowed(&agent, "startup-grant")
                    .await,
                loaded
            );
            test_runtime
                .sessions
                .stop(agent.instance().id(), SessionEndReason::Quit)
                .await
                .unwrap();
        }
        assert_eq!(*starts.lock().unwrap(), vec![true; 2]);
        let mut delegated =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        delegated.working_dir = root.to_string_lossy().into();
        delegated.bind_test_parent(&crate::session::new(
            "local",
            "test",
            128_000,
            &std::env::current_dir().unwrap(),
        ));
        assert!(
            Box::pin(
                {
                    policy.install(&test_runtime.preparation);
                    crate::runtime::preparation::SessionWorkflows::new(
                        test_runtime.sessions.clone(),
                        test_runtime.preparation.clone(),
                    )
                    .unwrap()
                }
                .start_session(delegated, false)
            )
            .await
            .is_err()
        );
        configuration.settings.permission = Some(serde_json::json!({"bash":"invalid"}));
        *policy.configuration.generation.write().unwrap() = configuration;
        let mut failed =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        failed.working_dir = root.to_string_lossy().into();
        assert!(
            Box::pin(
                {
                    policy.install(&test_runtime.preparation);
                    crate::runtime::preparation::SessionWorkflows::new(
                        test_runtime.sessions.clone(),
                        test_runtime.preparation.clone(),
                    )
                    .unwrap()
                }
                .start_session(failed, true)
            )
            .await
            .is_err()
        );
        assert!(test_runtime.sessions.list().await.unwrap().is_empty());
        assert_eq!(starts.lock().unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn candidate_skills_freeze_metadata_but_read_live_bodies() {
    let (root, policy, test_runtime) = session_test_runtime();
    let skill = root.join(".phi/skills/review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: stale-skill-description\n---\nreview",
    )
    .unwrap();
    let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = prompts.clone();
    let mut configuration = policy.configuration.snapshot();
    configuration.hooks = vec![Hook::build_system_prompt(move |event| {
        captured.lock().unwrap().push(event.system_prompt.clone());
        async move {
            Ok(phi_sdk::lifecycle::BuildSystemPromptOutcome {
                system_prompt: event.system_prompt,
            })
        }
    })];
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let options = configuration.runtime_build_options(&session);
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: accepted-skill-description\n---\nreview",
    )
    .unwrap();
    let prepared = test_runtime.preparation.prepare(options).await.unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: unaccepted-skill-description\n---\nupdated review",
    )
    .unwrap();
    let rebuilt = prepared
        .runtime
        .prepare_replacement(&session)
        .await
        .unwrap();
    prepared.runtime.sdk_resources().ready().await;
    rebuilt.runtime.sdk_resources().ready().await;
    let prompts = prompts.lock().unwrap();
    assert_eq!(prompts.len(), 2);
    for (runtime, prompt) in [&prepared.runtime, &rebuilt.runtime]
        .into_iter()
        .zip(prompts.iter())
    {
        let commands = runtime.commands();
        let command = commands
            .iter()
            .find(|command| command.name == "skill:review")
            .unwrap();
        assert_eq!(command.description, "accepted-skill-description");
        assert!(!prompt.contains("<available_skills>"));
        assert!(
            runtime
                .expand_prompt("/skill:review")
                .unwrap()
                .expanded_prompt
                .unwrap()
                .contains("updated review")
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn runtime_changes_respect_refresh_boundaries() {
    if run_isolated_test(concat!(
        module_path!(),
        "::runtime_changes_respect_refresh_boundaries"
    )) {
        return;
    }
    let (root, policy, _) = session_test_runtime();
    std::fs::write(root.join("models.json"), br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000},"selected":{"contextWindow":64000}}}}}"#).unwrap();
    let catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap(),
    );
    install_reload_catalog(&root, &catalog);
    let target = root.join("target");
    std::fs::create_dir_all(target.join(".phi")).unwrap();
    let path = target.join(".phi/settings.json");
    std::fs::write(&path, r#"{"defaultModel":"test","temperature":0.2}"#).unwrap();
    let extension = target.join(".phi/extensions/accepted.lua");
    let skill = target.join(".phi/skills/review/SKILL.md");
    let prompt = target.join(".phi/prompts/review.md");
    for resource in [&extension, &skill, &prompt] {
        std::fs::create_dir_all(resource.parent().unwrap()).unwrap();
    }
    std::fs::write(
        &extension,
        "register_command('accepted', {handler = function() end})",
    )
    .unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: accepted skill\n---\naccepted skill body",
    )
    .unwrap();
    std::fs::write(&prompt, "accepted prompt").unwrap();
    std::fs::write(target.join("AGENTS.md"), "accepted instructions").unwrap();
    let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = prompts.clone();
    let mut configuration = policy.configuration.snapshot();
    let resources = AgentRuntimeFactory::resources_for(
        &configuration.cli,
        &configuration.settings,
        &configuration.settings_manager,
        catalog.clone(),
        &root,
    )
    .unwrap();
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    configuration.model_catalog = catalog;
    configuration.settings.temperature = Some(0.2);
    configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
        name: "review".into(),
        alias: Some("r".into()),
        model: Some("selected".into()),
        thinking_level: Some("high".into()),
        mode: Some("plan".into()),
        prompt: Some("loaded review instructions".into()),
        ..Default::default()
    }]);
    configuration
        .hooks
        .push(Hook::build_system_prompt(move |event| {
            captured.lock().unwrap().push(event.system_prompt.clone());
            async move {
                Ok(phi_sdk::lifecycle::BuildSystemPromptOutcome {
                    system_prompt: event.system_prompt,
                })
            }
        }));
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = target.to_string_lossy().into();
    session.transcript.append_user_message("rewind target");
    let user_id = session.leaf_id.clone().unwrap();
    session.transcript.append_assistant_message(
        None,
        None,
        Vec::new(),
        "local".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "test".into(),
        Default::default(),
        "stop".into(),
    );
    let entry_id = session.leaf_id.clone().unwrap();
    let agent = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap()
    .start_session(session, false)
    .await
    .unwrap();
    let settings_manager = agent.runtime().settings_manager().unwrap().clone();
    let accepted_extensions = agent.runtime().extensions();
    for (settings_content, selection, expected_model) in [
        (
            r#"{"defaultModel":"test","temperature":0.8,"skills":["!**"]}"#,
            RuntimeSelection::Model {
                selection: "model:local:selected".into(),
            },
            "selected",
        ),
        (
            "invalid JSON",
            RuntimeSelection::Switch { name: "r".into() },
            "selected",
        ),
        (
            "invalid JSON",
            RuntimeSelection::Switch {
                name: "default".into(),
            },
            "test",
        ),
    ] {
        std::fs::write(&path, settings_content).unwrap();
        if matches!(selection, RuntimeSelection::Model { .. }) {
            settings_manager.reload().unwrap();
        }
        std::fs::write(&extension, "invalid Lua source").unwrap();
        std::fs::write(
            &skill,
            "---\nname: review\ndescription: unaccepted skill\n---\nchanged skill",
        )
        .unwrap();
        std::fs::write(&prompt, "unaccepted prompt").unwrap();
        std::fs::write(target.join("AGENTS.md"), "unaccepted instructions").unwrap();
        let accepted = test_runtime
            .preparation
            .configure_runtime(
                &test_runtime.sessions.session_access(agent.instance().id()),
                selection,
            )
            .await
            .unwrap();
        assert_eq!(accepted.session.model, expected_model);
        for navigation in [
            BranchNavigation::Entry {
                entry_id: entry_id.to_string(),
            },
            BranchNavigation::Rewind {
                entry_id: user_id.to_string(),
            },
        ] {
            let navigated = test_runtime
                .preparation
                .navigate_branch(
                    &test_runtime.sessions.session_access(agent.instance().id()),
                    navigation,
                )
                .await
                .unwrap();
            assert_eq!(navigated.session.model, expected_model);
            assert_eq!(
                navigated.session.context_window,
                accepted.session.context_window
            );
        }
        assert_eq!(
            accepted.session.context_window,
            if expected_model == "selected" {
                64_000
            } else {
                128_000
            }
        );
        let runtime = agent.runtime();
        if accepted.session.current_switch == "review" {
            assert_eq!(runtime.thinking_level.as_deref(), Some("high"));
            assert_eq!(runtime.runtime_config().agent_mode.as_str(), "plan");
            assert_eq!(
                runtime.runtime_config().mode_prompt_override.as_deref(),
                Some("loaded review instructions")
            );
        } else {
            assert_eq!(runtime.runtime_config().agent_mode.as_str(), "default");
            assert!(runtime.runtime_config().mode_prompt_override.is_none());
        }
        assert_eq!(runtime.runtime_config().temperature, Some(0.2));
        assert!(runtime.extensions().has_command("accepted"));
        assert!(!Arc::ptr_eq(&accepted_extensions, &runtime.extensions()));
        assert_eq!(
            runtime
                .expand_prompt("/review")
                .unwrap()
                .expanded_prompt
                .as_deref(),
            Some("<command name=\"review\">\naccepted prompt\n</command>")
        );
        assert!(runtime.commands().iter().any(
            |command| command.name == "skill:review" && command.description == "accepted skill"
        ));
        agent.ready_hook_notices().await;
        let prompts = prompts.lock().unwrap();
        let system_prompt = prompts.last().unwrap();
        assert!(system_prompt.contains("accepted instructions"));
        assert!(!system_prompt.contains("unaccepted instructions"));
        assert!(!system_prompt.contains("<available_skills>"));
    }
    assert_eq!(settings_manager.settings().temperature, Some(0.8));
    test_runtime
        .preparation
        .configure_runtime(
            &test_runtime.sessions.session_access(agent.instance().id()),
            RuntimeSelection::Model {
                selection: "model:local:selected".into(),
            },
        )
        .await
        .unwrap();
    let mut saved = agent.recovery().await.unwrap().session;
    saved.transcript.id = uuid::Uuid::new_v4().to_string().into();
    crate::session::storage::save_session(&saved).unwrap();
    std::fs::write(&path, r#"{"defaultProvider":"local","defaultModel":"test","temperature":0.75,"switches":[{"name":"default","model":"test","thinking_level":"high"}]}"#).unwrap();
    std::fs::write(
        &extension,
        "register_command('reloaded', {handler = function() end})",
    )
    .unwrap();
    test_runtime
        .preparation
        .reload_resources(&test_runtime.sessions.session_access(agent.instance().id()))
        .await
        .unwrap();
    assert_eq!(agent.session().await.model, "test");
    assert_eq!(agent.runtime().thinking_level.as_deref(), Some("high"));
    assert_eq!(agent.runtime().runtime_config().temperature, Some(0.75));
    assert!(agent.runtime().extensions().has_command("reloaded"));
    assert!(!agent.runtime().extensions().has_command("accepted"));
    assert_eq!(
        agent
            .runtime()
            .expand_prompt("/review")
            .unwrap()
            .expanded_prompt
            .as_deref(),
        Some("<command name=\"review\">\nunaccepted prompt\n</command>")
    );
    assert_eq!(
        agent
            .runtime()
            .settings_manager()
            .unwrap()
            .settings()
            .temperature,
        Some(0.75)
    );
    assert_eq!(agent.runtime().runtime_config().temperature, Some(0.75));
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let reopened = workflows
        .start_saved_session(&saved.id, false)
        .await
        .unwrap();
    assert_eq!(reopened.session().await.model, "test");
    assert_eq!(reopened.session().await.current_switch, "default");
    assert_eq!(reopened.runtime().runtime_config().temperature, Some(0.75));
    assert!(reopened.runtime().extensions().has_command("reloaded"));
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn new_session_keeps_target_scope_resources() {
    let (root, policy, test_runtime) = session_test_runtime();
    let target = root.join("target");
    let prompt = target.join(".phi/prompts/target.md");
    std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
    std::fs::write(&prompt, "target prompt").unwrap();
    let mut generation = policy.configuration.snapshot();
    generation.resource_policy = Some(
        generation
            .settings_manager
            .resource_policy_for(&root)
            .unwrap(),
    );
    policy.configuration.publish_configuration(generation);
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = target.to_string_lossy().into();
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let agent = workflows.start_session(session, false).await.unwrap();
    assert!(
        agent
            .commands()
            .iter()
            .any(|command| command.name == "target")
    );
    assert!(!Arc::ptr_eq(
        &agent.runtime().model_catalog(),
        &policy.configuration.snapshot().model_catalog,
    ));
    let settings_manager = agent.runtime().settings_manager().unwrap().clone();
    std::fs::write(
        target.join(".phi/settings.json"),
        r#"{"temperature":0.9,"prompts":["!**"]}"#,
    )
    .unwrap();
    settings_manager.reload().unwrap();
    let accepted_providers = agent.runtime().runtime_config().custom_providers;
    assert_eq!(
        accepted_providers["local"].base_url.as_deref(),
        Some("http://localhost/v1")
    );
    assert!(
        phi_sdk::config::RuntimeConfig::from(agent.runtime().source_settings().unwrap().as_ref(),)
            .custom_providers
            .is_empty()
    );
    let models = root.join("replacement-models.json");
    std::fs::write(
        &models,
        r#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v2","models":{"test":{"contextWindow":64000}}}}}"#,
    ).unwrap();
    let mut replacement = policy.configuration.snapshot();
    replacement.model_catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("replacement-store.json"),
            user_paths: vec![models],
            offline: true,
        })
        .unwrap(),
    );
    policy.configuration.publish_configuration(replacement);
    let accepted = workflows.new_session(agent.instance().id()).await.unwrap();
    let next = test_runtime.sessions.get(accepted.attachment.id()).unwrap();
    assert_eq!(
        next.session().await.context_window,
        agent.session().await.context_window
    );
    assert_eq!(
        next.runtime().runtime_config().custom_providers["local"].base_url,
        accepted_providers["local"].base_url
    );
    assert_eq!(
        next.runtime()
            .settings_manager()
            .unwrap()
            .settings()
            .temperature,
        Some(0.9)
    );
    assert_eq!(
        next.runtime().runtime_config().temperature,
        agent.runtime().runtime_config().temperature
    );
    assert!(
        next.commands()
            .iter()
            .any(|command| command.name == "target")
    );
    let mut replacement = policy.configuration.snapshot();
    replacement.model_catalog = Arc::new(ModelCatalog::empty());
    let resources = AgentRuntimeFactory::resources_for(
        &replacement.cli,
        &replacement.settings,
        &replacement.settings_manager,
        replacement.model_catalog.clone(),
        &root,
    )
    .unwrap();
    *test_runtime.preparation.runtime_factory.write().unwrap() =
        Arc::new(AgentRuntimeFactory::new(resources));
    policy.configuration.publish_configuration(replacement);
    let accepted = workflows.new_session(next.instance().id()).await.unwrap();
    let final_agent = test_runtime.sessions.get(accepted.attachment.id()).unwrap();
    assert_eq!(
        final_agent.session().await.context_window,
        agent.session().await.context_window
    );
    assert_eq!(
        final_agent.runtime().runtime_config().custom_providers["local"].base_url,
        accepted_providers["local"].base_url
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn model_selection_uses_latest_catalog_not_retained_snapshot() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let agent = workflows.start_session(session, false).await.unwrap();
    let retained = agent.runtime().model_catalog();
    std::fs::write(root.join("models.json"), r#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"selected":{"contextWindow":64000}}}}}"#).unwrap();
    let mut generation = policy.configuration.snapshot();
    generation.model_catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap(),
    );
    policy.configuration.publish_configuration(generation);
    assert!(retained.get("local", "selected").is_none());
    let accepted = test_runtime
        .preparation
        .configure_runtime(
            &test_runtime.sessions.session_access(agent.instance().id()),
            RuntimeSelection::Model {
                selection: "model:local:selected".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(accepted.session.model, "selected");
    assert_eq!(accepted.session.context_window, 64_000);
    assert!(
        agent
            .runtime()
            .model_catalog()
            .get("local", "selected")
            .is_some()
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn reload_candidate_retains_prepared_resource_policy() {
    let (root, policy, test_runtime) = session_test_runtime();
    let prompt = root.join(".phi/prompts/prepared.md");
    std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
    std::fs::write(&prompt, "prepared prompt").unwrap();
    let configuration = policy.configuration.snapshot();
    let settings_manager = &configuration.settings_manager;
    let resources = AgentRuntimeFactory::resource_defaults_for(
        &configuration.cli,
        &configuration.settings,
        &configuration.settings_manager,
        configuration.model_catalog.clone(),
        &root,
    )
    .unwrap();
    std::fs::write(root.join(".phi/settings.json"), r#"{"prompts":["!**"]}"#).unwrap();
    settings_manager.reload().unwrap();
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let runtime = EffectiveSwitch::for_session_switch(&session, &configuration.settings);
    let prepared = test_runtime
        .preparation
        .prepare_reload(&configuration, &session, &runtime, resources)
        .await
        .unwrap();
    assert!(
        prepared
            .runtime
            .commands()
            .iter()
            .any(|command| command.name == "prepared")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn candidates_query_shared_settings_without_implicit_reload() {
    let (root, policy, test_runtime) = session_test_runtime();
    let skill = root.join(".phi/skills/snapshot/SKILL.md");
    let prompt = root.join(".phi/prompts/snapshot.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::create_dir_all(prompt.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: snapshot\ndescription: captured skill\n---\nbody",
    )
    .unwrap();
    std::fs::write(&prompt, "captured prompt").unwrap();
    let mut configuration = policy.configuration.snapshot();
    let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
    configuration.settings_manager = settings_manager.clone();
    configuration.settings = settings_manager.settings();
    std::fs::write(
        root.join(".phi/settings.json"),
        r#"{"skills":["!**"],"prompts":["!**"]}"#,
    )
    .unwrap();
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let prepared = test_runtime
        .preparation
        .prepare(configuration.runtime_build_options(&session))
        .await
        .unwrap();
    let commands = prepared.runtime.commands();
    assert!(
        commands
            .iter()
            .any(|command| command.name == "skill:snapshot")
    );
    assert!(commands.iter().any(|command| command.name == "snapshot"));
    settings_manager.reload().unwrap();
    configuration.settings = settings_manager.settings();
    let refreshed = test_runtime
        .preparation
        .prepare(configuration.runtime_build_options(&session))
        .await
        .unwrap();
    assert!(
        !refreshed
            .runtime
            .commands()
            .iter()
            .any(|command| command.name == "snapshot" || command.name == "skill:snapshot")
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn candidate_config_retains_effective_cli_extension_selection() {
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.cli = Arc::new(Cli::parse_from([
        "phi",
        "--codemode",
        "--max-tokens",
        "4096",
        "--temperature",
        "0.7",
    ]));
    configuration.settings.codemode = Some(Default::default());
    configuration.settings.max_tokens = Some(2048);
    configuration.settings.temperature = Some(0.2);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    for disabled in [false, true] {
        configuration.settings.extensions = Some(if disabled {
            vec!["-builtin:codemode".into()]
        } else {
            Vec::new()
        });
        let prepared = test_runtime
            .preparation
            .prepare(configuration.runtime_build_options(&session))
            .await
            .unwrap();
        let rebuilt = prepared
            .runtime
            .prepare_replacement(&session)
            .await
            .unwrap();
        for runtime in [&prepared.runtime, &rebuilt.runtime] {
            let config = runtime.runtime_config();
            assert_eq!(config.extensions.codemode.enabled, !disabled);
            assert_eq!(config.max_tokens, Some(4096));
            assert_eq!(config.temperature, Some(0.7));
        }
        assert!(!configuration.settings.codemode.as_ref().unwrap().enabled);
        assert_eq!(configuration.settings.max_tokens, Some(2048));
        assert_eq!(configuration.settings.temperature, Some(0.2));
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn workspace_hook_trust_gates_startup_and_rebuilds() {
    let (root, policy, test_runtime) = session_test_runtime();
    let extensions = root.join(".phi/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    let extension_path = extensions.join("startup.lua");
    let extension_source = r#"
register_extension({name = "startup", capabilities = {process = {SessionStart = {"bash"}}}})
on("SessionStart", function(_, ctx) ctx.process.run("bash", {"-c", "touch lua-started"}) end)
on("resources_discover", function(event, ctx) ctx.ui.notify(event.payload.cwd) end)
register_command("available", {handler = function() end})
"#;
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    for (arguments, rule, source, hooks_enabled) in ["phi", "phi -p", "phi --rpc", "phi --web"]
        .into_iter()
        .flat_map(|arguments| [None, Some('+'), Some('-')].map(|rule| (arguments, rule)))
        .flat_map(|(arguments, rule)| {
            ["settings", "lua", "both", "none"].map(|source| (arguments, rule, source))
        })
        .flat_map(|(arguments, rule, source)| {
            [None, Some(false), Some(true)].map(|enabled| (arguments, rule, source, enabled))
        })
        .filter(|(_, rule, source, enabled)| match enabled {
            None => true,
            Some(false) => *source == "both",
            Some(true) => *source == "both" && *rule == Some('+'),
        })
    {
        let has_settings_hooks = matches!(source, "settings" | "both");
        let has_lua_hooks = matches!(source, "lua" | "both");
        if has_lua_hooks {
            std::fs::write(&extension_path, extension_source).unwrap();
        } else if extension_path.exists() {
            std::fs::remove_file(&extension_path).unwrap();
        }
        let mut configuration = policy.configuration.snapshot();
        configuration.cli = Arc::new(Cli::parse_from(arguments.split_whitespace()));
        configuration.settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
            enabled: false,
            ..Default::default()
        });
        let mut hooks = serde_json::json!({});
        if let Some(enabled) = hooks_enabled {
            hooks["enabled"] = enabled.into();
        }
        if has_settings_hooks {
            hooks["SessionStart"] = serde_json::json!([{"hooks": [{"type": "command", "command": "touch command-started"}]}]);
        }
        if let Some(prefix) = rule {
            hooks["trust"] = serde_json::json!([format!(
                "{prefix}{}",
                root.canonicalize().unwrap().display()
            )]);
        }
        configuration.settings.hooks = Some(serde_json::from_value(hooks).unwrap());
        configuration.settings_manager = crate::config::SettingsManager::from_settings(
            &root,
            configuration.settings.clone(),
            Settings::default(),
        )
        .unwrap();
        let result = test_runtime
            .preparation
            .prepare(configuration.runtime_build_options(&session))
            .await;
        for marker in ["command-started", "lua-started"] {
            assert!(!root.join(marker).exists(), "{arguments}, {rule:?}");
        }
        if hooks_enabled != Some(false)
            && rule.is_none()
            && !configuration.cli.print
            && (has_settings_hooks || has_lua_hooks)
        {
            assert!(
                matches!(result, Err(error) if error.to_string().contains("workspace Hook trust is missing"))
            );
            continue;
        }
        let prepared = result.unwrap();
        let rebuilt = prepared
            .runtime
            .prepare_replacement(&session)
            .await
            .unwrap();
        let enabled = hooks_enabled != Some(false) && rule != Some('-');
        for runtime in [&prepared.runtime, &rebuilt.runtime] {
            let extensions = runtime.extensions();
            assert_eq!(extensions.has_process_hooks(), enabled && has_lua_hooks);
            assert_eq!(extensions.has_command("available"), has_lua_hooks);
            extensions
                .discover_resources_for(phi_sdk::lifecycle::ResourcesDiscoverReason::Startup, &root)
                .await;
            let expected = if enabled && has_lua_hooks {
                vec![root.display().to_string()]
            } else {
                Vec::new()
            };
            assert_eq!(extensions.take_notifications().unwrap(), expected);
            runtime
                .sdk_resources()
                .session_started(phi_sdk::lifecycle::SessionStartReason::Startup, None)
                .await;
            runtime.sdk_resources().ready().await;
            let notices = runtime.sdk_resources().take_hook_notices();
            if enabled && has_lua_hooks {
                assert_eq!(notices.len(), 1, "{notices:?}");
                assert!(
                    matches!(&notices[0], phi_sdk::hooks::HookNotice::Warning { message }
                    if message.contains("source is not trusted for process execution"))
                );
            } else {
                assert!(notices.is_empty(), "{notices:?}");
            }
            assert!(!root.join("lua-started").exists());
            let marker = root.join("command-started");
            assert_eq!(
                marker.exists(),
                enabled && has_settings_hooks,
                "{arguments}, {rule:?}, {source}"
            );
            if enabled && has_settings_hooks {
                std::fs::remove_file(marker).unwrap();
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn session_candidates_resolve_directory_bound_resources() {
    let (root, policy, test_runtime) = session_test_runtime();
    let working_dir = root.join("target");
    std::fs::create_dir_all(&working_dir).unwrap();
    std::fs::write(working_dir.join("AGENTS.md"), "target-only instructions").unwrap();
    {
        let extensions = working_dir.join(".phi/extensions");
        std::fs::create_dir_all(&extensions).unwrap();
        std::fs::write(
            extensions.join("target.lua"),
            "register_command('target-only', {handler = function() end})",
        )
        .unwrap();
    }
    let prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = prompts.clone();
    let mut configuration = policy.configuration.snapshot();
    configuration.settings.no_tools = Some(false);
    configuration.hooks = vec![Hook::build_system_prompt(move |event| {
        captured.lock().unwrap().push(event.system_prompt.clone());
        async move {
            Ok(phi_sdk::lifecycle::BuildSystemPromptOutcome {
                system_prompt: event.system_prompt,
            })
        }
    })];
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = working_dir.to_string_lossy().into();
    let configuration = configuration.bind_settings(&working_dir).unwrap();
    let prepared = test_runtime
        .preparation
        .prepare(configuration.runtime_build_options(&session))
        .await
        .unwrap();
    prepared.runtime.sdk_resources().ready().await;
    let sandbox = &prepared.runtime.resources.sandbox;
    assert!(sandbox.is_enabled());
    let command = sandbox.wrap_program("sh", &[]);
    assert!(
        command
            .as_std()
            .get_args()
            .any(|arg| arg == working_dir.as_os_str())
    );
    assert!(
        prompts
            .lock()
            .unwrap()
            .iter()
            .any(|prompt| prompt.contains("target-only instructions"))
    );
    {
        assert!(prepared.runtime.extensions().has_command("target-only"));
        std::fs::write(
            working_dir.join(".phi/extensions/invalid.lua"),
            "this is not valid lua",
        )
        .unwrap();
        let resources = RuntimeResources::new_for(
            phi_sdk::PhiRuntimeServices::new(),
            Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
            configuration.model_catalog.clone(),
            phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap(),
            &working_dir,
        )
        .extension_load_failed(true);
        let default_extensions = resources.extensions.clone();
        let factory = AgentRuntimeFactory::new(resources);
        let initial = factory
            .prepare_initial(configuration.runtime_build_options(&session))
            .await
            .unwrap();
        let second = factory
            .prepare_initial(configuration.runtime_build_options(&session))
            .await
            .unwrap();
        assert!(!initial.runtime.extensions().has_command("target-only"));
        assert!(initial.runtime.resources.sandbox.is_enabled());
        assert!(!Arc::ptr_eq(
            &initial.runtime.extensions(),
            &default_extensions
        ));
        assert!(!Arc::ptr_eq(
            &initial.runtime.extensions(),
            &second.runtime.extensions()
        ));
        assert!(
            factory
                .prepare(configuration.runtime_build_options(&session))
                .await
                .is_err()
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}
