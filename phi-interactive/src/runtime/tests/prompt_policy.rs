use super::*;

#[tokio::test]
async fn permission_candidates_restore_own_grants_without_parent_inheritance() {
    use phi_sdk::extension::ExtensionEffect;
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.settings.permission = Some(serde_json::json!({"bash":"ask"}));
    configuration.settings.no_tools = Some(false);
    let mut parent =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    parent.working_dir = root.to_string_lossy().into();
    parent
        .permission_allowlist
        .push(crate::session::PermissionAllowEntry {
            tool: "bash".into(),
            pattern: "printf parent-grant".into(),
        });
    let prepared = test_runtime
        .preparation
        .prepare(configuration.runtime_build_options(&parent))
        .await
        .unwrap();
    let parent_effect = ExtensionEffect::SpawnProcess {
        extension: "grant-probe".into(),
        source: crate::config::settings::phi_path()
            .join("grant-probe.lua")
            .to_string_lossy()
            .into(),
        context: "Command".into(),
        program: "printf".into(),
        args: vec!["parent-grant".into()],
        sandboxed: true,
    };
    assert!(
        prepared
            .runtime
            .probe_effect_permission(&parent_effect)
            .await
            .is_ok()
    );
    let mut child =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    child.working_dir = parent.working_dir.clone();
    child.bind_test_parent(&parent);
    child
        .permission_allowlist
        .push(crate::session::PermissionAllowEntry {
            tool: "bash".into(),
            pattern: "printf child-grant".into(),
        });
    let child_runtime = prepared
        .runtime
        .prepare_child(
            &child,
            crate::runtime::subagents::ChildRuntimePolicy {
                sessions: Arc::downgrade(&test_runtime.sessions),
            },
            None,
        )
        .await
        .unwrap();
    assert!(
        child_runtime
            .runtime
            .probe_effect_permission(&parent_effect)
            .await
            .is_err()
    );
    let mut child_effect = parent_effect.clone();
    let ExtensionEffect::SpawnProcess { args, .. } = &mut child_effect;
    *args = vec!["child-grant".into()];
    assert!(
        child_runtime
            .runtime
            .probe_effect_permission(&child_effect)
            .await
            .is_ok()
    );
    assert!(
        prepared
            .runtime
            .probe_effect_permission(&child_effect)
            .await
            .is_err()
    );
    assert!(
        prepared
            .runtime
            .probe_effect_permission(&parent_effect)
            .await
            .is_ok()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn rejected_or_handled_prompt_policy_never_grants_or_saves_permissions() {
    let hooks = vec![Hook::user_prompt_submit(|_| async {
        Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Handled { reason: None })
    })];
    let (root, policy, test_runtime) = session_test_runtime_with_hooks(hooks);
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.transcript.append_user_message("saved history");
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
    let before = serde_json::to_value(agent.instance().transcript().await.unwrap()).unwrap();
    let attachment = test_runtime
        .sessions
        .get(agent.instance().id())
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    let path = crate::session::storage::session_path(&agent.session().await);
    test_runtime
        .sessions
        .session_access(agent.instance().id())
        .checkpoint()
        .await
        .unwrap();
    let saved = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let mut prompt = phi_sdk::turn::TurnPrompt {
        prompt: "configured prompt".into(),
        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
        injected_context: Vec::new(),
        images: Vec::new(),
        tool_policy: phi_sdk::turn::TurnToolPolicy {
            allowances: vec![("Read".into(), "**".into())],
            removals: vec!["Edit".into()],
        },
    };
    assert!(matches!(
        attachment.submit(prompt.clone()).await.unwrap().disposition,
        phi_sdk::turn::SubmissionDisposition::Handled { .. }
    ));
    prompt.tool_policy.removals.push("MCP".into());
    assert!(attachment.submit(prompt.clone()).await.is_err());
    assert_eq!(
        serde_json::to_value(agent.instance().transcript().await.unwrap()).unwrap(),
        before
    );
    assert!(agent.session().await.permission_allowlist.is_empty());
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, saved).unwrap();
    attachment.checkpoint().await.unwrap();
    assert!(
        crate::session::storage::load_session(path.to_str().unwrap())
            .unwrap()
            .permission_allowlist
            .is_empty()
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn prompt_policy_survives_waiter_loss_and_expires_before_followup() {
    use phi_sdk::turn::{SubmissionDisposition, TurnPrompt};
    use tokio::io::AsyncWriteExt;
    let (root, policy, test_runtime) = session_test_runtime();
    let skill_path = root.join(".phi/skills/scoped/SKILL.md");
    std::fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
    std::fs::write(
        &skill_path,
        "---\nname: scoped\ndescription: scoped evidence\n---\nscoped skill evidence",
    )
    .unwrap();
    std::fs::create_dir_all(root.join(".phi/prompts")).unwrap();
    let template = "---\nallowed-tools: [Read]\ndisallowed-tools: [Edit]\n---\nRead the scoped Skill: $ARGUMENTS";
    std::fs::write(root.join(".phi/prompts/inspect.md"), template).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_path = root.join("models.json");
    std::fs::write(&model_path, serde_json::json!({"providers":{"local":{
        "api":"openai-completions","baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
        "models":{"test":{"contextWindow":128000}}
    }}}).to_string()).unwrap();
    let catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("catalog.json"),
            user_paths: vec![model_path],
            offline: true,
        })
        .unwrap(),
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut configuration = policy.configuration.snapshot();
    configuration.hooks = vec![Hook::user_prompt_submit({
        let entered = entered.clone();
        let release = release.clone();
        move |event| {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                if event.original_prompt.starts_with("/inspect") {
                    assert_eq!(event.prompt, event.original_prompt);
                }
                if event.prompt.contains("cancel admission") {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Continue {
                    notices: Vec::new(),
                    prompt: event.prompt,
                    injected_context: Vec::new(),
                })
            }
        }
    })];
    configuration.model_catalog = catalog.clone();
    configuration.settings.permission = Some(serde_json::json!({"read":"ask"}));
    configuration.settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
        enabled: false,
        ..Default::default()
    });
    configuration.settings.no_tools = Some(false);
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    let (observed, mut requests) = tokio::sync::mpsc::channel(16);
    let continue_provider = Arc::new(tokio::sync::Notify::new());
    let provider_release = continue_provider.clone();
    let provider = tokio::spawn(async move {
        for call in 0..6 {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_prompt_policy_request(&mut connection).await;
            observed.send(request).await.unwrap();
            if call == 2 {
                provider_release.notified().await;
            }
            let (delta, finish) = if call % 2 == 0 {
                (
                    serde_json::json!({"tool_calls":[{"index":0,"id":format!("read_{call}"),"type":"function",
                    "function":{"name":"Read","arguments":serde_json::json!({"path":skill_path}).to_string()}}]}),
                    "tool_calls",
                )
            } else {
                (serde_json::json!({"content":"done"}), "stop")
            };
            let reply = serde_json::json!({"id":"completion","model":"test","choices":[{"delta":delta,"finish_reason":finish}]});
            let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
            connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    for index in 0..2 {
        let wait_for_admission = index == 0;
        let display = if !wait_for_admission {
            "/inspect scoped Skill"
        } else {
            "/inspect cancel admission scoped Skill"
        };
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let agent_session = Box::pin(
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
        let attachment = test_runtime
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
        let agent = test_runtime.sessions.get(attachment.id()).unwrap();
        assert!(agent.session().await.permission_allowlist.is_empty());
        let prompt = TurnPrompt {
            prompt: display.into(),
            tool_policy: Default::default(),
            injected_context: Vec::new(),
            images: Vec::new(),
            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
        };
        let path = crate::session::storage::session_path(&agent.session().await);
        std::fs::create_dir_all(&path).unwrap();
        if !wait_for_admission {
            assert!(matches!(
                attachment.submit(prompt).await.unwrap().disposition,
                SubmissionDisposition::Started { .. }
            ));
        } else {
            let submitting = attachment.clone();
            let waiter = tokio::spawn(async move { submitting.submit(prompt).await });
            tokio::time::timeout(std::time::Duration::from_secs(5), entered.notified())
                .await
                .unwrap();
            waiter.abort();
            assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
            release.notify_one();
        }
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        let messages = first["messages"].to_string();
        assert_eq!(messages.matches("<available_skills>").count(), 1);
        assert!(messages.contains("scoped evidence"));
        assert!(messages.contains("Read the scoped Skill:"));
        let definitions = first["tools"].as_array().unwrap();
        assert!(
            definitions
                .iter()
                .any(|tool| tool["function"]["name"] == "Read")
        );
        assert!(
            !definitions
                .iter()
                .any(|tool| tool["function"]["name"] == "Edit")
        );
        if index == 1 {
            use phi_sdk::turn::SubmissionDispatch;
            let steer = TurnPrompt {
                prompt: "remove Write at the next safe point".into(),
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: phi_sdk::turn::TurnToolPolicy {
                    allowances: vec![("Bash".into(), "**".into())],
                    removals: vec!["Write".into()],
                },
            };
            assert!(matches!(
                attachment
                    .dispatch(steer, SubmissionDispatch::Steer)
                    .await
                    .unwrap()
                    .disposition,
                SubmissionDisposition::Queued { .. }
            ));
            let skill = attachment.expand_prompt("/skill:scoped").unwrap();
            assert_eq!(skill.tool_policy, Default::default());
            assert!(matches!(
                attachment
                    .dispatch(
                        TurnPrompt {
                            prompt: "/skill:scoped".into(),
                            tool_policy: Default::default(),
                            injected_context: Vec::new(),
                            images: Vec::new(),
                            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                        },
                        SubmissionDispatch::FollowUp
                    )
                    .await
                    .unwrap()
                    .disposition,
                SubmissionDisposition::Queued { .. }
            ));
            continue_provider.notify_one();
        }
        let second = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
            .await
            .unwrap()
            .unwrap();
        if index == 1 {
            assert!(
                !second["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == "Edit"
                        || tool["function"]["name"] == "Write")
            );
        }
        let output = second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|message| message["role"] == "tool")
            .unwrap();
        assert!(
            output["content"]
                .as_str()
                .unwrap()
                .contains("scoped skill evidence"),
            "{output}"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while agent.instance().snapshot().status
                == phi_sdk::agent_instance::AgentInstanceStatus::Running
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(agent.session().await.permission_allowlist.is_empty());
        std::fs::remove_dir(&path).unwrap();
        attachment.checkpoint().await.unwrap();
        assert!(
            crate::session::storage::load_session(path.to_str().unwrap())
                .unwrap()
                .permission_allowlist
                .is_empty()
        );
        if index == 1 {
            let first = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(
                first["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|tool| tool["function"]["name"] == "Edit")
            );
            let pending = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(pending) = attachment.pending_interactions().await.unwrap().first()
                    {
                        break pending.clone();
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            attachment
                .respond_interaction(
                    &pending.request_id,
                    phi_sdk::interaction::InteractionResponse::Permission(
                        phi_sdk::interaction::PermissionDecision::Deny,
                    ),
                )
                .await
                .unwrap();
            let second = tokio::time::timeout(std::time::Duration::from_secs(5), requests.recv())
                .await
                .unwrap()
                .unwrap();
            let output = second["messages"]
                .as_array()
                .unwrap()
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")
                .unwrap();
            assert!(
                !output["content"]
                    .as_str()
                    .unwrap()
                    .contains("scoped skill evidence"),
                "{output}"
            );
            assert!(agent.session().await.permission_allowlist.is_empty());
        }
        test_runtime
            .sessions
            .stop(agent.instance().id(), SessionEndReason::Quit)
            .await
            .unwrap();
    }
    provider.await.unwrap();
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn settings_hooks_share_lifecycle_gates_without_lua() {
    use tokio::io::AsyncWriteExt;
    let (root, policy, test_runtime) = session_test_runtime();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_path = root.join("models.json");
    std::fs::write(&model_path, serde_json::json!({"providers":{"local":{
        "api":"openai-completions","baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
        "models":{"test":{"contextWindow":128000}}
    }}}).to_string()).unwrap();
    let mut configuration = policy.configuration.snapshot();
    configuration.model_catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("catalog.json"),
            user_paths: vec![model_path],
            offline: true,
        })
        .unwrap(),
    );
    configuration.cli = Arc::new(Cli::parse_from(["phi", "--yolo"]));
    configuration.settings.no_tools = Some(false);
    configuration.settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
        enabled: false,
        ..Default::default()
    });
    configuration.settings.hooks = Some(
        serde_json::from_value(serde_json::json!({
            "trust": [root.canonicalize().unwrap()],
            "SessionStart": [{"hooks":[{"type":"command","command":"cat > hook-start.json"}]}],
            "UserPromptSubmit": [{"hooks":[{"type":"command","command":"printf extra-hook-context"}]}],
            "Stop": [{"hooks":[{"type":"command","command":"if [ -f hook-continued ]; then exit 0; fi; touch hook-continued; echo review-again >&2; exit 2"}]}],
            "Notification": [{"matcher":"idle_prompt","hooks":[{"type":"command","command":"cat > hook-idle.json"}]}],
            "PreToolUse": [{"matcher":{"regex":"^Write$"},"hooks":[
                {"type":"command","command":"echo write-blocked-by-settings >&2; exit 2"}
            ]}],
            "PostToolUse": [{"matcher":{"regex":"^Read$"},"hooks":[
                {"type":"command","command":"cat > hook-event.json"}
            ]}]
        }))
        .unwrap(),
    );
    configuration.settings_manager = crate::config::SettingsManager::from_settings(
        &root,
        configuration.settings.clone(),
        Settings::default(),
    )
    .unwrap();
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    std::fs::write(root.join("source.txt"), "source content").unwrap();
    let provider = tokio::spawn(async move {
        for call in 0..3 {
            let (mut connection, _) = listener.accept().await.unwrap();
            let request = read_prompt_policy_request(&mut connection).await;
            assert!(
                request["messages"]
                    .to_string()
                    .contains("extra-hook-context")
            );
            if call == 2 {
                assert!(request["messages"].to_string().contains("review-again"));
            }
            let (delta, finish) = if call == 0 {
                (
                    serde_json::json!({"tool_calls":[
                        {"index":0,"id":"write","type":"function","function":{
                            "name":"Write","arguments":serde_json::json!({"path":"blocked.txt","content":"must not exist"}).to_string()
                        }},
                        {"index":1,"id":"read","type":"function","function":{
                            "name":"Read","arguments":serde_json::json!({"path":"source.txt"}).to_string()
                        }}
                    ]}),
                    "tool_calls",
                )
            } else {
                assert!(
                    request["messages"]
                        .to_string()
                        .contains("write-blocked-by-settings")
                );
                (serde_json::json!({"content":"done"}), "stop")
            };
            let reply = serde_json::json!({"id":"completion","model":"test",
                "choices":[{"delta":delta,"finish_reason":finish}]});
            let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
            connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
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
    agent
        .instance()
        .submit(phi_sdk::turn::TurnPrompt {
            prompt: "try both tools".into(),
            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
            injected_context: Vec::new(),
            images: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        provider.await.unwrap();
        agent.instance().wait_for_quiescence(None).await.unwrap();
    })
    .await
    .unwrap();
    assert!(!root.join("blocked.txt").exists());
    let event: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("hook-event.json")).unwrap()).unwrap();
    assert_eq!(event["hook_event_name"], "PostToolUse");
    assert_eq!(event["tool_name"], "Read");
    let started: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("hook-start.json")).unwrap()).unwrap();
    assert_eq!(started["hook_event_name"], "SessionStart");
    assert!(root.join("hook-continued").exists());
    assert!(
        event["tool_response"]
            .to_string()
            .contains("source content")
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    let notification: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("hook-idle.json")).unwrap()).unwrap();
    assert_eq!(notification["notification_type"], "idle_prompt");
    std::fs::remove_dir_all(root).unwrap();
}

async fn read_prompt_policy_request(connection: &mut tokio::net::TcpStream) -> serde_json::Value {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = connection.read(&mut buffer).await.unwrap();
        assert_ne!(count, 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .unwrap()
                .parse()
                .unwrap();
            if bytes.len() >= start + 4 + length {
                return serde_json::from_slice(&bytes[start + 4..start + 4 + length]).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn rewind_branches_keep_history_and_current_switch_independent() {
    if run_isolated_test(concat!(
        module_path!(),
        "::rewind_branches_keep_history_and_current_switch_independent"
    )) {
        return;
    }
    use tokio::io::AsyncWriteExt;
    let (root, policy, _) = session_test_runtime();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_path = root.join("models.json");
    std::fs::write(&model_path, serde_json::json!({"providers":{"local":{
        "api":"openai-completions","baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
        "models":{"test":{"contextWindow":128000},"selected":{"contextWindow":64000}}
    }}}).to_string()).unwrap();
    let catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("catalog.json"),
            user_paths: vec![model_path],
            offline: true,
        })
        .unwrap(),
    );
    install_reload_catalog(&root, &catalog);
    let mut configuration = policy.configuration.snapshot();
    configuration.model_catalog = catalog.clone();
    configuration.settings.switches = Some(vec![
        crate::config::SwitchConfig {
            name: "default".into(),
            model: Some("test".into()),
            thinking_level: Some("low".into()),
            ..Default::default()
        },
        crate::config::SwitchConfig {
            name: "plan".into(),
            model: Some("test".into()),
            thinking_level: Some("high".into()),
            mode: Some("plan".into()),
            prompt: Some("accepted plan instructions".into()),
            ..Default::default()
        },
    ]);
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    std::fs::write(
        root.join(".phi/settings.json"),
        serde_json::to_vec(&serde_json::json!({"defaultProvider": "local", "switches": configuration.settings.switches}))
            .unwrap(),
    )
    .unwrap();
    let resources = AgentRuntimeFactory::resources_for(
        &configuration.cli,
        &configuration.settings,
        &configuration.settings_manager,
        catalog,
        &root,
    )
    .unwrap();
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.transcript.append_user_message("historical branch");
    session.transcript.append_assistant_message(
        None,
        None,
        vec![phi_sdk::transcript::ContentBlock::Text {
            text: "shared answer".into(),
        }],
        "local".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "test".into(),
        Default::default(),
        "stop".into(),
    );
    let historical_leaf = session.leaf_id.clone().unwrap();
    session
        .transcript
        .append_user_message("original branch request");
    let rewind_target = session.leaf_id.clone().unwrap();
    session
        .transcript
        .switch_snapshots
        .push(phi_sdk::transcript::SwitchSnapshotRecord {
            id: "default-1".into(),
            switch: phi_sdk::transcript::SwitchSnapshot {
                name: "default".into(),
                provider: "local".into(),
                model: "test".into(),
                thinking_level: Some("low".into()),
                mode: "default".into(),
                prompt: Some("historical default instructions".into()),
            },
        });
    session
        .transcript
        .append_entry(crate::session::TranscriptEntryKind::Message {
            message: crate::session::AgentMessage::Assistant {
                id: None,
                turn_id: None,
                content: vec![phi_sdk::transcript::ContentBlock::Text {
                    text: "original branch answer".into(),
                }],
                provider: "local".into(),
                api: None,
                model: "test".into(),
                switch: Some("default-1".into()),
                usage: Default::default(),
                stop_reason: "stop".into(),
            },
        });
    let original_leaf = session.leaf_id.clone().unwrap();
    let agent = workflows.start_session(session, true).await.unwrap();
    let id = agent.instance().id().clone();
    let access = test_runtime.sessions.session_access(&id);
    test_runtime
        .preparation
        .configure_runtime(
            &access,
            RuntimeSelection::Switch {
                name: "plan".into(),
            },
        )
        .await
        .unwrap();
    test_runtime
        .preparation
        .configure_runtime(
            &access,
            RuntimeSelection::Model {
                selection: "model:local:selected".into(),
            },
        )
        .await
        .unwrap();
    let before = agent.recovery().await.unwrap().session;
    test_runtime
        .preparation
        .navigate_branch(
            &access,
            BranchNavigation::Rewind {
                entry_id: rewind_target.to_string(),
            },
        )
        .await
        .unwrap();
    let selected = agent.recovery().await.unwrap().session;
    assert_eq!(before.leaf_id.as_ref(), Some(&original_leaf));
    assert_eq!(selected.leaf_id.as_ref(), Some(&historical_leaf));
    assert_eq!(selected.current_switch, "plan");
    assert_eq!(agent.runtime().thinking_level.as_deref(), Some("high"));
    assert_eq!(agent.runtime().runtime_config().agent_mode.as_str(), "plan");
    assert_eq!(
        serde_json::to_value(&selected.entries).unwrap(),
        serde_json::to_value(&before.entries).unwrap()
    );
    let provider = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let request = read_prompt_policy_request(&mut connection).await;
        let reply = serde_json::json!({"id":"completion","model":"selected",
            "choices":[{"delta":{"content":"continued answer"},"finish_reason":"stop"}]});
        let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
        connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        request
    });
    let attachment = crate::runtime::attachment::AgentAttachment::new(
        &agent,
        access.clone(),
        test_runtime.preparation.clone(),
    );
    attachment
        .submit(phi_sdk::turn::TurnPrompt {
            prompt: "continue here".into(),
            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
            injected_context: Vec::new(),
            images: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    let request = tokio::time::timeout(std::time::Duration::from_secs(5), provider)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(request["model"], "selected");
    let messages = request["messages"].to_string();
    assert!(messages.contains("historical branch"));
    assert!(messages.contains("shared answer"));
    assert!(messages.contains("continue here"));
    assert!(!messages.contains("original branch request"));
    assert!(!messages.contains("original branch answer"));
    let view = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.instance().wait_for_quiescence(None),
    )
    .await
    .unwrap()
    .unwrap();
    let continuation = &view.transcript.entries[before.entries.len()..];
    assert_eq!(continuation.len(), 2);
    assert_eq!(continuation[0].parent_id.as_ref(), Some(&historical_leaf));
    let crate::session::TranscriptEntryKind::Message {
        message:
            crate::session::AgentMessage::Assistant {
                switch: Some(snapshot),
                ..
            },
    } = &continuation[1].kind
    else {
        panic!("continuation must record its effective Switch");
    };
    assert_eq!(snapshot, "plan-1");
    assert_eq!(view.transcript.switch_snapshots.len(), 2);
    let snapshot = &view
        .transcript
        .switch_snapshots
        .iter()
        .find(|record| &record.id == snapshot)
        .unwrap()
        .switch;
    assert_eq!(snapshot.name, "plan");
    assert_eq!(snapshot.provider, "local");
    assert_eq!(snapshot.model, "selected");
    assert_eq!(snapshot.thinking_level.as_deref(), Some("high"));
    assert_eq!(snapshot.mode, "plan");
    assert_eq!(
        snapshot.prompt.as_deref(),
        Some("accepted plan instructions")
    );
    assert_eq!(
        serde_json::to_value(&view.transcript.entries[..before.entries.len()]).unwrap(),
        serde_json::to_value(&before.entries).unwrap(),
    );
    let continued_leaf = view.transcript.leaf_id.clone().unwrap();
    let original = test_runtime
        .preparation
        .navigate_branch(
            &access,
            BranchNavigation::Entry {
                entry_id: original_leaf.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(original.session.model, "selected");
    assert_eq!(original.session.current_switch, "plan");
    assert!(
        original
            .session
            .current_branch()
            .iter()
            .any(|entry| entry.id == original_leaf)
    );
    assert!(
        !original
            .session
            .current_branch()
            .iter()
            .any(|entry| entry.id == continued_leaf)
    );
    test_runtime
        .preparation
        .configure_runtime(
            &access,
            RuntimeSelection::Switch {
                name: "default".into(),
            },
        )
        .await
        .unwrap();
    let continued = test_runtime
        .preparation
        .navigate_branch(
            &access,
            BranchNavigation::Entry {
                entry_id: continued_leaf.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(continued.session.model, "test");
    assert_eq!(continued.session.current_switch, "default");
    assert_eq!(agent.runtime().thinking_level.as_deref(), Some("low"));
    assert!(
        continued
            .session
            .current_branch()
            .iter()
            .any(|entry| entry.id == continued_leaf)
    );
    assert!(
        !continued
            .session
            .current_branch()
            .iter()
            .any(|entry| entry.id == original_leaf)
    );
    assert_eq!(
        serde_json::to_value(&continued.session.entries).unwrap(),
        serde_json::to_value(&view.transcript.entries).unwrap(),
    );
    test_runtime
        .sessions
        .stop(&id, SessionEndReason::Quit)
        .await
        .unwrap();
    let saved = crate::session::storage::load_session_id(id.as_str()).unwrap();
    assert_eq!(saved.leaf_id, view.transcript.leaf_id);
    assert_eq!(
        serde_json::to_value(&saved.entries).unwrap(),
        serde_json::to_value(&view.transcript.entries).unwrap()
    );
    let resumed = workflows
        .start_saved_session(id.as_str(), true)
        .await
        .unwrap();
    assert_eq!(resumed.session().await.model, "test");
    assert_eq!(resumed.session().await.current_switch, "default");
    assert_eq!(resumed.runtime().thinking_level.as_deref(), Some("low"));
    assert_eq!(
        resumed.runtime().runtime_config().agent_mode.as_str(),
        "default"
    );
    test_runtime
        .preparation
        .reload_resources(&test_runtime.sessions.session_access(&id))
        .await
        .unwrap();
    assert_eq!(resumed.session().await.model, "test");
    assert_eq!(resumed.session().await.current_switch, "default");
    assert_eq!(resumed.runtime().thinking_level.as_deref(), Some("low"));
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn session_grants_survive_replacement_branch_reload_and_saved_resume() {
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.settings.permission = Some(serde_json::json!({"bash":"ask"}));
    configuration.settings.no_tools = Some(false);
    *policy.configuration.generation.write().unwrap() = configuration.clone();
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.transcript.append_user_message("first branch");
    let first_leaf = session.transcript.leaf_id.clone();
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
    session.transcript.append_user_message("later branch");
    let durable_leaf = session.leaf_id.clone();
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
    let id = agent.instance().id().clone();
    let registration = agent.instance().registration_id();
    agent
        .allow_session_tools(&[("bash".into(), "printf own-grant".into())])
        .await
        .unwrap();
    let stale = agent.session().await;
    let prepared = Box::pin(
        test_runtime
            .preparation
            .prepare(configuration.runtime_build_options(&stale)),
    )
    .await
    .unwrap();
    agent
        .allow_session_tools(&[("bash".into(), "printf late-grant".into())])
        .await
        .unwrap();
    let generation = agent.snapshot().await.runtime_generation_id;
    assert!(
        test_runtime
            .sessions
            .get(&id)
            .unwrap()
            .replace_runtime(prepared, stale, None, true)
            .await
            .map(|accepted| accepted.snapshot)
            .is_err()
    );
    assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
    let grants = agent.session().await.permission_allowlist;
    assert_eq!(grants.len(), 2);
    for branch in [
        None,
        Some(phi_sdk::agent_instance::BranchSelection {
            leaf_id: first_leaf.as_ref().map(ToString::to_string),
        }),
    ] {
        let current = agent.session().await;
        let prepared = test_runtime
            .preparation
            .prepare(configuration.runtime_build_options(&current))
            .await
            .unwrap();
        test_runtime
            .sessions
            .get(&id)
            .unwrap()
            .replace_runtime(prepared, current, branch, true)
            .await
            .map(|accepted| accepted.snapshot)
            .unwrap();
        assert_eq!(agent.session().await.permission_allowlist, grants);
        assert_eq!(agent.instance().registration_id(), registration);
        assert!(crate::runtime::session::tests::session_command_allowed(&agent, "own-grant").await);
    }
    assert_eq!(
        agent.instance().transcript().await.unwrap().leaf_id,
        first_leaf
    );
    let current = agent.session().await;
    let resources = AgentRuntimeFactory::resources_for(
        &configuration.cli,
        &configuration.settings,
        &configuration.settings_manager,
        configuration.model_catalog.clone(),
        &root,
    )
    .unwrap();
    let prepared = Box::pin(crate::runtime::RuntimePreparation::prepare_reload(
        &test_runtime.preparation.clone(),
        &configuration,
        &current,
        &EffectiveSwitch::for_session_switch(&current, &configuration.settings),
        resources,
    ))
    .await
    .unwrap();
    Box::pin(test_runtime.preparation.reload(
        &test_runtime.sessions.session_access(&id),
        current,
        prepared,
        None,
    ))
    .await
    .unwrap();
    assert_eq!(agent.session().await.permission_allowlist, grants);
    assert!(crate::runtime::session::tests::session_command_allowed(&agent, "late-grant").await);
    let fresh = Box::pin(
        {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .create_session(CreateSessionRequest {
            provider: Some("local".into()),
            model: Some("test".into()),
            persist: false,
            switch: None,
        }),
    )
    .await
    .unwrap();
    assert!(fresh.session().await.permission_allowlist.is_empty());
    assert!(!crate::runtime::session::tests::session_command_allowed(&fresh, "own-grant").await);
    let stopped = test_runtime
        .sessions
        .stop(&id, SessionEndReason::Quit)
        .await
        .unwrap();
    assert_eq!(stopped.permission_allowlist, grants);
    for startup in [false, true] {
        let resumed = if startup {
            SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
            .start_saved_session(id.as_str(), true)
            .await
            .unwrap()
            .session()
            .await
        } else {
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
            .resume_session(
                fresh.instance().id(),
                crate::runtime::SavedSessionLocator::IdOrPath(id.as_str()),
            )
            .await
            .map(|transition| (transition.attachment, transition.accepted))
            .unwrap()
            .1
            .session
        };
        let restored = test_runtime.sessions.get(&id).unwrap();
        assert_ne!(restored.instance().registration_id(), registration);
        assert_eq!(resumed.permission_allowlist, grants);
        assert_eq!(resumed.transcript.leaf_id, durable_leaf);
        for command in ["own-grant", "late-grant"] {
            assert!(
                crate::runtime::session::tests::session_command_allowed(&restored, command).await
            );
        }
        test_runtime
            .sessions
            .stop(&id, SessionEndReason::Quit)
            .await
            .unwrap();
    }
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
