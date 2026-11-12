use super::*;

#[tokio::test]
async fn prompt_preparation_preserves_generation_and_raw_tape_evidence() {
    use phi_sdk::hooks::Hook;
    use phi_sdk::lifecycle::{
        BuildSystemPromptOutcome, UserPromptExpansionOutcome, UserPromptSubmitOutcome,
        UserPromptSubmitSource,
    };
    use phi_sdk::transcript::{AgentMessage, TranscriptEntryKind};
    use phi_sdk::turn::{SubmissionDisposition, TurnPrompt};

    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let system_prompts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hooks = vec![
        Hook::build_system_prompt({
            let system_prompts = system_prompts.clone();
            move |event| {
                system_prompts
                    .lock()
                    .unwrap()
                    .push(event.system_prompt.clone());
                async move {
                    Ok(BuildSystemPromptOutcome {
                        system_prompt: event.system_prompt,
                    })
                }
            }
        }),
        Hook::user_prompt_submit({
            let observed = observed.clone();
            move |event| {
                observed
                    .lock()
                    .unwrap()
                    .push((event.original_prompt, event.prompt.clone()));
                async move {
                    if event.prompt == "/skill:review blocked" {
                        return Ok(UserPromptSubmitOutcome::Handled {
                            reason: Some("submission blocked".into()),
                        });
                    }
                    Ok(UserPromptSubmitOutcome::Continue {
                        notices: Vec::new(),
                        prompt: event.prompt,
                        injected_context: Vec::new(),
                    })
                }
            }
        }),
        Hook::user_prompt_expansion(|event| async move {
            if event.command_args == "blocked-expansion" {
                return Ok(UserPromptExpansionOutcome::Block {
                    reason: "expansion blocked".into(),
                });
            }
            Ok(UserPromptExpansionOutcome::Allow)
        }),
    ];
    let (root, _, runtime) = super::preparation::tests::session_test_runtime();
    let skill = root.join(".phi/skills/review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: frozen skill metadata\n---\nremember old skill body",
    )
    .unwrap();
    let hidden = root.join(".phi/skills/manual/SKILL.md");
    std::fs::create_dir_all(hidden.parent().unwrap()).unwrap();
    std::fs::write(&hidden, "---\nname: manual\ndescription: hidden skill metadata\ndisable-model-invocation: true\n---\nmanual body").unwrap();
    let template = root.join(".phi/prompts/inspect.md");
    std::fs::create_dir_all(template.parent().unwrap()).unwrap();
    std::fs::write(
        &template,
        "---\nallowed-tools: [Read]\ndisallowed-tools: [Edit]\n---\nremember template body",
    )
    .unwrap();
    std::fs::write(
        template.with_file_name("hide.md"),
        "keyword-free template body",
    )
    .unwrap();
    let mut settings = Settings::default();
    settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
        tape: Some(phi_sdk::extension::memory::TapeConfig {
            enabled: Some(true),
            only_git: Some(false),
            anchor: Some(phi_sdk::extension::memory::TapeAnchorConfig {
                keywords: Some(phi_sdk::extension::memory::TapeKeywordConfig {
                    global: vec!["remember".into()],
                    project: Vec::new(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
        memory_dir: Some(phi_sdk::extension::memory::MemoryDirSettings {
            local_path: Some(root.join("memory").display().to_string()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let catalog = Arc::new(
        phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap(),
    );
    let policy = TestConfiguration::new(
        Arc::new(Cli::parse_from(["phi"])),
        settings,
        crate::AgentBuildOptions {
            no_tools: true,
            ..Default::default()
        },
        hooks,
        catalog,
    );
    policy.install(&runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let agent = crate::runtime::preparation::SessionWorkflows::new(
        runtime.sessions.clone(),
        runtime.preparation.clone(),
    )
    .unwrap()
    .start_session(session.clone(), false)
    .await
    .unwrap();
    agent.ready_hook_notices().await;
    let prompts = system_prompts.lock().unwrap();
    let system_prompt = prompts.last().unwrap();
    assert!(!system_prompt.contains("<available_skills>"));
    assert!(!system_prompt.contains("hidden skill metadata"));
    drop(prompts);

    let preview = agent.expand_prompt("/inspect").unwrap();
    assert!(
        preview
            .expanded_prompt
            .unwrap()
            .contains("remember template body")
    );
    assert_eq!(
        preview.tool_policy.allowances,
        [("Read".into(), "**".into())]
    );
    assert!(observed.lock().unwrap().is_empty());
    assert!(
        agent
            .instance()
            .transcript()
            .await
            .unwrap()
            .entries
            .is_empty()
    );
    std::fs::write(&template, "changed template body").unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: changed metadata\n---\nremember latest skill body",
    )
    .unwrap();
    assert!(
        agent
            .expand_prompt("/inspect")
            .unwrap()
            .expanded_prompt
            .unwrap()
            .contains("remember template body")
    );
    assert!(
        agent
            .expand_prompt("/skill:review")
            .unwrap()
            .expanded_prompt
            .unwrap()
            .contains("latest skill body")
    );

    let inputs = [
        (
            UserPromptSubmitSource::Interactive,
            "/inspect",
            "remember template body",
            false,
        ),
        (
            UserPromptSubmitSource::Print,
            "/inspect",
            "remember template body",
            false,
        ),
        (
            UserPromptSubmitSource::Rpc,
            "/inspect",
            "remember template body",
            false,
        ),
        (
            UserPromptSubmitSource::Sdk,
            "/inspect",
            "remember template body",
            false,
        ),
        (
            UserPromptSubmitSource::Internal,
            "/inspect",
            "/inspect",
            false,
        ),
        (
            UserPromptSubmitSource::Extension,
            "/inspect",
            "/inspect",
            false,
        ),
        (
            UserPromptSubmitSource::Interactive,
            "/skill:review",
            "latest skill body",
            false,
        ),
        (
            UserPromptSubmitSource::Interactive,
            "/hide remember explicit user evidence",
            "keyword-free template body",
            true,
        ),
    ];
    for (source, original, expected, keyword_context) in inputs {
        let before = agent.instance().transcript().await.unwrap().entries.len();
        let receipt = agent
            .submit(
                TurnPrompt {
                    prompt: original.into(),
                    source,
                    injected_context: Vec::new(),
                    images: Vec::new(),
                    tool_policy: Default::default(),
                },
                None,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            receipt.disposition,
            SubmissionDisposition::Started { .. }
        ));
        let transcript = agent.instance().transcript().await.unwrap();
        assert_eq!(
            transcript.entries[before..].iter().any(|entry| matches!(
                &entry.kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::Context { source, .. }
                } if *source == phi_sdk::extension::memory::MemoryContextKind::KeywordIntro.source()
            )),
            keyword_context,
            "{original}"
        );
        let observed = observed.lock().unwrap();
        let (raw, transformed) = observed.last().unwrap();
        assert_eq!(raw, original);
        assert_eq!(transformed, original);
        assert!(transcript.entries[before..].iter().any(|entry| matches!(
            &entry.kind,
            TranscriptEntryKind::Message { message: AgentMessage::User { content } }
                if content.iter().any(|block| matches!(block,
                    phi_sdk::transcript::UserContentBlock::Text { text } if text.contains(expected)))
        )), "missing prepared input: {expected}");
        drop(observed);
        agent.instance().abort().await.unwrap();
    }
    std::fs::remove_file(&skill).unwrap();
    let blocked = agent
        .submit(
            TurnPrompt {
                prompt: "/skill:review blocked".into(),
                source: UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(blocked.disposition, SubmissionDisposition::Handled { reason: Some(ref reason) } if reason == "submission blocked")
    );
    let blocked = agent
        .submit(
            TurnPrompt {
                prompt: "/skill:review blocked-expansion".into(),
                source: UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert!(
        matches!(blocked.disposition, SubmissionDisposition::Handled { reason: Some(ref reason) } if reason == "expansion blocked")
    );
    let before = agent.instance().transcript().await.unwrap().entries.len();
    let receipt = agent
        .submit(
            TurnPrompt {
                prompt: "/skill:review".into(),
                source: UserPromptSubmitSource::Interactive,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        receipt.disposition,
        SubmissionDisposition::Handled { reason: Some(_) }
    ));
    assert_eq!(
        agent.instance().transcript().await.unwrap().entries.len(),
        before
    );

    let mut child =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    child.working_dir = session.working_dir.clone();
    child.bind_test_parent(&session);
    let parent_runtime = agent.runtime().clone();
    let _prepared_child = parent_runtime
        .prepare_child(
            &child,
            crate::runtime::subagents::ChildRuntimePolicy {
                sessions: Arc::downgrade(&runtime.sessions),
            },
            None,
        )
        .await
        .unwrap();
    assert!(
        !system_prompts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("<available_skills>")
    );
    runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn prepared_session_retains_model_discovery_services() {
    let (root, policy, runtime) = super::preparation::tests::session_test_runtime();
    policy.install(&runtime.preparation);
    let agent = super::preparation::SessionWorkflows::new(
        runtime.sessions.clone(),
        runtime.preparation.clone(),
    )
    .unwrap()
    .create_session(super::preparation::CreateSessionRequest {
        provider: None,
        model: None,
        switch: None,
        persist: false,
    })
    .await
    .unwrap();

    let error = agent.discover_models().await.err().unwrap();
    assert_eq!(error.to_string(), "model discovery disabled by PHI_OFFLINE");

    runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn scoped_resources_reject_missing_child_and_bind_its_sandbox() {
    let root = std::env::temp_dir().join(format!("phi-child-runtime-{}", uuid::Uuid::new_v4()));
    let child = root.join("child");
    std::fs::create_dir_all(&child).unwrap();
    let cli = Cli::parse_from(["phi"]);
    let settings_manager = crate::config::SettingsManager::load_for(&child).unwrap();
    let settings = settings_manager.settings();
    let catalog = Arc::new(phi_sdk::models::ModelCatalog::empty());
    let resources = AgentRuntimeFactory::resources_for(
        &cli,
        &settings,
        &settings_manager,
        catalog.clone(),
        &child,
    )
    .unwrap();
    let command = resources.sandbox.wrap_program("sh", &[]);
    let args = command
        .as_std()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|arg| arg == child.to_str().unwrap()));
    assert!(
        AgentRuntimeFactory::resources_for(
            &cli,
            &settings,
            &settings_manager,
            catalog,
            &root.join("missing")
        )
        .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn saved_session_operations_share_process_policy_without_starting_agents() {
    let root = std::env::temp_dir().join(format!("phi-saved-sessions-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let catalog = Arc::new(phi_sdk::models::ModelCatalog::empty());
    let resources = RuntimeResources::new(
        PhiRuntimeServices::new(),
        Arc::new(ExtensionRuntime::load()),
        catalog.clone(),
        Sandbox::new(false, root.clone()).unwrap(),
    );
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    let mut session =
        crate::session::new("unused", "unused", 1_000, &std::env::current_dir().unwrap());
    session.transcript.append_user_message("saved history");
    session.transcript.append_assistant_message(
        None,
        None,
        Vec::new(),
        "unused".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "unused".into(),
        Default::default(),
        "stop".into(),
    );
    crate::session::storage::save_session(&session).unwrap();
    let path = crate::session::storage::session_path(&session);
    for query in ["", session.id.as_str()] {
        let expected =
            serde_json::to_value(crate::session::discovery::discover_sessions(query).unwrap())
                .unwrap();
        assert!(
            expected
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["id"] == session.id.as_str())
        );
    }
    assert_eq!(
        crate::session::storage::load_session(&session.id)
            .unwrap()
            .id,
        session.id
    );
    assert_eq!(
        crate::session::storage::load_session(path.to_str().unwrap())
            .unwrap()
            .id,
        session.id
    );
    assert!(crate::session::storage::load_session_id(path.to_str().unwrap()).is_err());
    assert!(crate::session::storage::load_session("missing-saved-session").is_err());
    assert!(test_runtime.sessions.list().await.unwrap().is_empty());

    let lock = crate::session::storage::lock_session_path(&path).unwrap();
    assert!(test_runtime.sessions.delete_session(&session.id).is_err());
    assert!(path.exists());
    drop(lock);
    assert!(path.exists());
    test_runtime.sessions.delete_session(&session.id).unwrap();
    assert!(!path.exists());
    assert!(test_runtime.sessions.list().await.unwrap().is_empty());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn extension_commands_submit_and_wait_through_direct_attachment() {
    use phi_sdk::extension::ExtensionCommandOutcome;
    use phi_sdk::hooks::Hook;
    use phi_sdk::lifecycle::{UserPromptSubmitOutcome, UserPromptSubmitSource};
    let root = std::env::temp_dir().join(format!("phi-command-submit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let model_path = root.join("models.json");
    std::fs::write(&model_path, serde_json::json!({ "providers": { "local": {
        "api": "openai-completions", "baseUrl": format!("http://{}/v1", listener.local_addr().unwrap()),
        "models": { "test": { "contextWindow": 128000 } }
    } } }).to_string()).unwrap();
    let extension = root.join("commands.lua");
    std::fs::write(
        &extension,
        r#"
register_command("send", {
  handler = function(args, ctx)
    local options
    if args == "queued" then options = { deliver_as = "follow_up" } end
    local receipt = ctx.agent.send_user_message(args, options)
    if args == "queued" then
      assert(receipt.status == "queued" and receipt.submission_id and receipt.dispatch == "follow_up")
    elseif args == "busy" or args == "blocked" then
      assert(receipt.status == "handled" and receipt.reason)
    else
      assert(receipt.status == "started" and receipt.turn_id)
    end
  end,
})
register_command("wait", {
  handler = function(args, ctx)
    ctx.wait_for_idle()
    ctx.agent.send_user_message(args)
  end,
})
"#,
    )
    .unwrap();
    let catalog = Arc::new(
        phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![model_path],
            offline: true,
        })
        .unwrap(),
    );
    let resources = RuntimeResources::new(
        PhiRuntimeServices::new().model_catalog(catalog.clone()),
        Arc::new(ExtensionRuntime::load()),
        catalog.clone(),
        Sandbox::new(false, root.clone()).unwrap(),
    )
    .extension_paths(vec![extension]);
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    let cli = Cli::parse_from(["phi"]);
    let mut settings = Settings::default();
    settings.memory = Some(phi_sdk::extension::memory::MemoryConfig {
        enabled: false,
        ..Default::default()
    });
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = seen.clone();
    let hook = Hook::user_prompt_submit(move |event| {
        let seen = captured.clone();
        async move {
            assert!(matches!(event.source, UserPromptSubmitSource::Extension));
            seen.lock().unwrap().push(event.prompt.clone());
            if event.prompt == "blocked" {
                return Ok(UserPromptSubmitOutcome::Handled {
                    reason: Some("blocked by policy".into()),
                });
            }
            Ok(UserPromptSubmitOutcome::Continue {
                notices: Vec::new(),
                prompt: format!("prepared {}", event.prompt),
                injected_context: Vec::new(),
            })
        }
    });
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
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
                    context: phi_sdk::context::load(true),
                    agent_options: crate::AgentBuildOptions {
                        no_tools: true,
                        ..Default::default()
                    },
                    hooks: vec![hook],
                },
                instance: AgentInstanceBuildOptions::from_session(&session),
            },
            phi_sdk::lifecycle::SessionStartReason::New,
            false,
        )
        .await
        .unwrap();
    assert_eq!(
        agent_session
            .run_extension_command("send", "review main")
            .await
            .unwrap(),
        Some(ExtensionCommandOutcome::Completed(None))
    );
    assert!(
        agent_session
            .run_extension_command("send", "busy")
            .await
            .is_err()
    );
    assert_eq!(*seen.lock().unwrap(), ["review main"]);
    agent_session
        .run_extension_command("send", "queued")
        .await
        .unwrap();
    assert_eq!(
        agent_session
            .instance()
            .view()
            .await
            .unwrap()
            .pending_submission_ids
            .len(),
        1
    );
    assert_eq!(
        agent_session
            .instance()
            .transcript()
            .await
            .unwrap()
            .current_branch()
            .iter()
            .filter_map(|entry| match &entry.kind {
                phi_sdk::transcript::TranscriptEntryKind::Message {
                    message: phi_sdk::transcript::AgentMessage::User { content },
                } => content.iter().find_map(|block| match block {
                    phi_sdk::transcript::UserContentBlock::Text { text } => Some(text.to_string()),
                    _ => None,
                }),
                _ => None,
            })
            .collect::<Vec<_>>(),
        ["prepared review main"]
    );
    agent_session.instance().abort().await.unwrap();
    assert_eq!(
        agent_session
            .run_extension_command("send", "blocked")
            .await
            .unwrap(),
        Some(ExtensionCommandOutcome::Rejected(
            "blocked by policy".into()
        ))
    );
    agent_session
        .run_extension_command("send", "running")
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
    let waiting = attachment.clone();
    let mut command = tokio::spawn(async move {
        waiting
            .run_extension_command("wait".into(), "after cancel".into())
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(25), &mut command)
            .await
            .is_err()
    );
    attachment.abort().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), command)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Some(ExtensionCommandOutcome::Completed(None))
    );
    assert!(
        agent_session
            .instance()
            .transcript()
            .await
            .unwrap()
            .current_branch()
            .iter()
            .any(|entry| matches!(&entry.kind,
                phi_sdk::transcript::TranscriptEntryKind::Message {
                    message: phi_sdk::transcript::AgentMessage::User { content },
                } if content.iter().any(|block| matches!(block,
                    phi_sdk::transcript::UserContentBlock::Text { text } if text == &"prepared after cancel"))))
    );
    test_runtime
        .sessions
        .stop(agent_session.instance().id(), SessionEndReason::Quit)
        .await
        .unwrap();
    assert!(
        agent_session
            .run_extension_command("send", "after stop")
            .await
            .is_err()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn prompt_resources_follow_their_committed_generation() {
    use crate::runtime::preparation::tests::{install_reload_catalog, run_isolated_test};
    if run_isolated_test(concat!(
        module_path!(),
        "::prompt_resources_follow_their_committed_generation"
    )) {
        return;
    }
    let root = std::env::temp_dir().join(format!("phi-host-prompts-{}", uuid::Uuid::new_v4()));
    for name in ["a", "b"] {
        let prompts = root.join(name).join(".phi/prompts");
        std::fs::create_dir_all(&prompts).unwrap();
        std::fs::write(prompts.join("inspect.md"), format!("{name} $ARGUMENTS")).unwrap();
    }
    let invalid_agent = root.join("a/.phi/agents/broken.md");
    std::fs::create_dir_all(invalid_agent.parent().unwrap()).unwrap();
    std::fs::write(&invalid_agent, "---\nname: [invalid\n---\nbroken").unwrap();
    let invalid_prompt = root.join("a/.phi/prompts/reload.md");
    std::fs::write(&invalid_prompt, "conflicting command").unwrap();
    let skill = root.join("a/.phi/skills/review/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: review\ndescription: review\n---\nhost skill",
    )
    .unwrap();
    let models = root.join("models.json");
    std::fs::write(&models, br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://127.0.0.1:1/v1","models":{"test":{"contextWindow":128000}}}}}"#).unwrap();
    let catalog = Arc::new(
        phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![models],
            offline: true,
        })
        .unwrap(),
    );
    let resources = RuntimeResources::new_for(
        PhiRuntimeServices::new().model_catalog(catalog.clone()),
        Arc::new(ExtensionRuntime::load()),
        catalog.clone(),
        Sandbox::new(false, root.clone()).unwrap(),
        &root,
    );
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    let cli = Arc::new(Cli::parse_from(["phi"]));
    let settings = Settings::default();
    let policy = TestConfiguration::new(
        cli,
        settings,
        crate::AgentBuildOptions {
            no_tools: true,
            ..Default::default()
        },
        Vec::new(),
        catalog.clone(),
    );
    let mut agents = Vec::new();
    for name in ["a", "b"] {
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.join(name).to_string_lossy().into();
        agents.push(
            {
                policy.install(&test_runtime.preparation);
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
            }
            .start_session(session, false)
            .await
            .unwrap(),
        );
    }
    install_reload_catalog(&root, &catalog);
    std::fs::remove_file(&invalid_prompt).unwrap();
    std::fs::remove_file(&invalid_agent).unwrap();
    for (agent, expected) in agents.iter().zip(["a", "b"]) {
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
        let recovery = Box::pin(attachment.recovery()).await.unwrap();
        assert_eq!(
            recovery.snapshot.runtime_generation_id,
            recovery.declarations.generation_id
        );
        let capabilities = recovery.declarations.capabilities;
        assert_eq!(capabilities.compact_enabled, Some(true));
        assert_eq!(
            capabilities
                .agent_diagnostics
                .iter()
                .any(|message| message.contains(invalid_agent.to_str().unwrap())),
            expected == "a"
        );
        let has_invalid = capabilities
            .prompt_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == invalid_prompt);
        assert_eq!(has_invalid, expected == "a");
        let commands = recovery.declarations.commands;
        assert_eq!(
            commands
                .iter()
                .any(|command| command.name == "skill:review"),
            expected == "a"
        );
        let direct = test_runtime
            .sessions
            .get(agent.instance().id())
            .map(|agent| {
                crate::runtime::attachment::AgentAttachment::new(
                    &agent,
                    test_runtime.sessions.session_access(agent.instance().id()),
                    test_runtime.preparation.clone(),
                )
            })
            .unwrap()
            .expand_prompt("/inspect files")
            .unwrap();
        assert_eq!(
            direct.expanded_prompt.as_deref(),
            Some(format!("<command name=\"inspect\">\n{expected} files\n</command>").as_str())
        );
    }
    let attachment = test_runtime
        .sessions
        .get(agents[0].instance().id())
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    assert!(
        attachment
            .expand_prompt("/skill:review")
            .unwrap()
            .expanded_prompt
            .unwrap()
            .contains("host skill")
    );
    std::fs::remove_file(skill).unwrap();
    assert!(attachment.expand_prompt("/skill:review").is_err());
    std::fs::write(root.join("a/.phi/prompts/inspect.md"), "updated $ARGUMENTS").unwrap();
    assert!(
        agents[0]
            .expand_prompt("/inspect files")
            .unwrap()
            .expanded_prompt
            .unwrap()
            .contains("a files")
    );
    std::fs::write(
        root.join("a/.phi/settings.json"),
        r#"{"compact_enabled":false}"#,
    )
    .unwrap();
    assert_eq!(agents[0].capabilities().compact_enabled, Some(true));
    let accepted = test_runtime
        .preparation
        .clone()
        .reload_resources(
            &test_runtime
                .sessions
                .session_access(agents[0].instance().id()),
        )
        .await
        .unwrap();
    assert_eq!(
        accepted.declarations.capabilities.compact_enabled,
        Some(false)
    );
    assert!(
        accepted
            .declarations
            .capabilities
            .agent_diagnostics
            .iter()
            .all(|message| !message.contains(invalid_agent.to_str().unwrap()))
    );
    assert_eq!(
        accepted.declarations.generation_id,
        accepted.snapshot.runtime_generation_id
    );
    for (agent, expected) in agents.iter().zip(["updated", "b"]) {
        assert_eq!(agent.capabilities().compact_enabled, Some(expected == "b"));
        assert!(
            !agent
                .capabilities()
                .agent_diagnostics
                .iter()
                .any(|message| message.contains(invalid_agent.to_str().unwrap()))
        );
        assert!(
            !agent
                .capabilities()
                .prompt_diagnostics
                .iter()
                .any(|diagnostic| diagnostic.path == invalid_prompt)
        );
        assert_eq!(
            agent
                .expand_prompt("/inspect files")
                .unwrap()
                .expanded_prompt
                .as_deref(),
            Some(format!("<command name=\"inspect\">\n{expected} files\n</command>").as_str())
        );
    }
    let committed = agents[0].recovery().await.unwrap();
    std::fs::write(&invalid_agent, "---\nname: [invalid\n---\nbroken").unwrap();
    std::fs::write(
        root.join("a/.phi/settings.json"),
        r#"{"compact_enabled":true,"permission":{"bash":"invalid"}}"#,
    )
    .unwrap();
    assert!(
        test_runtime
            .preparation
            .clone()
            .reload_resources(
                &test_runtime
                    .sessions
                    .session_access(agents[0].instance().id()),
            )
            .await
            .is_err()
    );
    let unchanged = agents[0].recovery().await.unwrap();
    assert_eq!(
        unchanged.declarations.generation_id,
        committed.declarations.generation_id
    );
    assert_eq!(
        serde_json::to_value(unchanged.declarations.capabilities).unwrap(),
        serde_json::to_value(committed.declarations.capabilities).unwrap()
    );
    assert!(
        (test_runtime
            .sessions
            .get(&AgentInstanceId::new("missing"))
            .map(|agent| crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )))
        .is_err()
    );
    let attachment = test_runtime
        .sessions
        .get(agents[0].instance().id())
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    let mut events = attachment.subscribe();
    let recovered = attachment.recover_events(&mut events).await.unwrap();
    assert!(
        async {
            test_runtime
                .preparation
                .configure_runtime(
                    &test_runtime.sessions.session_access(attachment.id()),
                    preparation::RuntimeSelection::Model {
                        selection: "model:local:missing".into(),
                    },
                )
                .await
        }
        .await
        .is_err()
    );
    assert_eq!(
        attachment
            .recovery()
            .await
            .unwrap()
            .declarations
            .generation_id,
        recovered.declarations.generation_id
    );
    assert!(
        (test_runtime
            .sessions
            .get(&AgentInstanceId::new("missing"))
            .map(|agent| crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )))
        .is_err()
    );
    drop(attachment);
    let attachment = test_runtime
        .sessions
        .get(agents[0].instance().id())
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    assert_eq!(
        attachment
            .recovery()
            .await
            .unwrap()
            .declarations
            .generation_id,
        unchanged.declarations.generation_id
    );
    assert_eq!(
        test_runtime.sessions.list().await.unwrap().len(),
        agents.len()
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
