use super::*;

#[tokio::test]
async fn directed_memory_keeps_scope_without_retrying_automatic_injection() {
    use phi_sdk::extension::memory::{MemoryConfig, MemoryDirSettings, MemoryInjectionMode};
    let root = std::env::temp_dir().join(format!("phi-remote-memory-{}", uuid::Uuid::new_v4()));
    let working_dir = root.join("host-project");
    let agent_dir = root.join("agents");
    let memory_dir = root.join("memory/host-project/core/project");
    std::fs::create_dir_all(&working_dir).unwrap();
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(
        memory_dir.join("context.md"),
        "---\ndescription: host context\ntags: [test]\n---\nhost-only-memory",
    )
    .unwrap();
    std::fs::write(
        agent_dir.join("reviewer.md"),
        "---\nname: reviewer\ndescription: review\n---\nReview the task.",
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let provider = tokio::spawn(async move {
        let mut connections = Vec::new();
        loop {
            connections.push(listener.accept().await.unwrap().0);
        }
    });
    let model_path = root.join("models.json");
    std::fs::write(
        &model_path,
        serde_json::json!({"providers":{"local":{
            "api":"openai-completions", "baseUrl":format!("http://{address}/v1"),
            "models":{"test":{"contextWindow":128000}}
        }}})
        .to_string(),
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
    let resources = RuntimeResources::new_for(
        PhiRuntimeServices::new().model_catalog(catalog.clone()),
        Arc::new(ExtensionRuntime::load()),
        catalog.clone(),
        Sandbox::new(false, working_dir.clone()).unwrap(),
        &working_dir,
    );
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    let cli = Cli::parse_from(["phi"]);
    let mut settings = Settings::default();
    settings.memory = Some(MemoryConfig {
        enabled: true,
        injection: Some(MemoryInjectionMode::MessageAppend),
        memory_dir: Some(MemoryDirSettings {
            local_path: Some(root.join("memory").to_string_lossy().into_owned()),
            ..Default::default()
        }),
        ..Default::default()
    });
    let options = |session: &crate::session::Session| RuntimeRebuildOptions {
        phi: PhiBuildOptions {
            cli: &cli,
            provider: "local",
            model: "test",
            runtime_cfg: settings.runtime_config(&catalog),
            context: phi_sdk::context::load_with_options_for(
                false,
                None,
                vec![agent_dir.clone()],
                &working_dir,
            ),
            agent_options: crate::AgentBuildOptions::default(),
            hooks: vec![phi_sdk::hooks::Hook::user_prompt_submit(
                |event| async move {
                    if event.prompt == "handled" {
                        Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Handled { reason: None })
                    } else {
                        Ok(phi_sdk::lifecycle::UserPromptSubmitOutcome::Continue {
                            notices: Vec::new(),
                            prompt: event.prompt,
                            injected_context: Vec::new(),
                        })
                    }
                },
            )],
        },
        instance: AgentInstanceBuildOptions::from_session(session),
    };
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = working_dir.to_string_lossy().into();
    let agent_session = test_runtime
        .sessions
        .create(
            &test_runtime.preparation,
            session.clone(),
            options(&session),
            phi_sdk::lifecycle::SessionStartReason::New,
            false,
        )
        .await
        .unwrap();
    let input = phi_sdk::turn::TurnPrompt {
        tool_policy: Default::default(),
        prompt: "@reviewer inspect".into(),
        source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
        injected_context: Vec::new(),
        images: Vec::new(),
    };
    let missing = phi_sdk::turn::DirectedDelegation {
        agent: "missing".into(),
        mode: None,
    };
    assert!(
        agent_session
            .submit(input.clone(), Some(missing), None)
            .await
            .is_err()
    );
    assert!(
        !agent_session
            .instance()
            .transcript()
            .await
            .unwrap()
            .has_user_message()
    );
    let handled = agent_session
        .submit(
            phi_sdk::turn::TurnPrompt {
                prompt: "handled".into(),
                ..input.clone()
            },
            None,
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        handled.disposition,
        phi_sdk::turn::SubmissionDisposition::Handled { .. }
    ));
    assert!(
        handled
            .notices
            .iter()
            .all(|notice| !matches!(notice, phi_sdk::hooks::HookNotice::ContextInjected { .. }))
    );
    let invalid_memory = memory_dir.join("invalid.md");
    std::fs::write(&invalid_memory, [0xff]).unwrap();
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
    let failed_context_prompt = phi_sdk::turn::TurnPrompt {
        prompt: "inspect".into(),
        ..input.clone()
    };
    let direct = attachment
        .dispatch(
            failed_context_prompt.clone(),
            phi_sdk::turn::SubmissionDispatch::FollowUp,
        )
        .await
        .unwrap();
    assert!(matches!(
        direct.disposition,
        phi_sdk::turn::SubmissionDisposition::Started { .. }
    ));
    assert!(direct.notices.iter().any(|notice| matches!(notice,
        phi_sdk::hooks::HookNotice::Warning { message } if message.contains("Memory context failed")
    )));
    agent_session.instance().abort().await.unwrap();
    let retried = agent_session
        .submit(failed_context_prompt, None, None)
        .await
        .unwrap();
    assert!(matches!(
        retried.disposition,
        phi_sdk::turn::SubmissionDisposition::Started { .. }
    ));
    assert!(retried.notices.is_empty());
    agent_session.instance().abort().await.unwrap();
    std::fs::remove_file(invalid_memory).unwrap();
    for prompt in ["@reviewer inspect", "@reviewer inspect again"] {
        let constraint = phi_sdk::turn::DirectedDelegation {
            agent: "reviewer".into(),
            mode: None,
        };
        let mut turn = input.clone();
        turn.prompt = prompt.into();
        let accepted = attachment.submit_directed(turn, constraint).await.unwrap();
        let injected = accepted
            .notices
            .iter()
            .filter(|notice| matches!(notice, phi_sdk::hooks::HookNotice::ContextInjected { .. }))
            .count();
        assert_eq!(injected, 0);
        assert!(matches!(
            accepted.disposition,
            phi_sdk::turn::SubmissionDisposition::Started { .. }
        ));
        agent_session.instance().abort().await.unwrap();
    }
    let transcript = agent_session.instance().transcript().await.unwrap();
    let memory_contexts = |transcript: &phi_sdk::transcript::Transcript| {
        transcript
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry.kind,
                    phi_sdk::transcript::TranscriptEntryKind::Message {
                        message: phi_sdk::transcript::AgentMessage::Context { .. }
                    }
                )
            })
            .count()
    };
    assert_eq!(memory_contexts(&transcript), 0);
    assert!(
        !serde_json::to_string(&transcript)
            .unwrap()
            .contains("If needed, use memory_search")
    );
    assert!(
        !serde_json::to_string(&transcript)
            .unwrap()
            .contains("host context")
    );
    std::fs::write(
        memory_dir.join("context.md"),
        "---\ndescription: updated host context\ntags: [test]\n---\nlatest-host-memory",
    )
    .unwrap();
    for (command, expected_prompt) in [
        ("/memory-inject inspect latest", "inspect latest"),
        ("/memory-inject", ""),
    ] {
        let mut turn = input.clone();
        turn.prompt = command.into();
        let result = agent_session.submit(turn, None, None).await.unwrap();
        assert!(result.notices.iter().any(|notice| matches!(
            notice,
            phi_sdk::hooks::HookNotice::ContextInjected {
                item_count: 1,
                requested: true,
                ..
            }
        )));
        assert!(matches!(
            result.disposition,
            phi_sdk::turn::SubmissionDisposition::Started { prompt, .. } if prompt == expected_prompt
        ));
        agent_session.instance().abort().await.unwrap();
        let transcript = agent_session.instance().transcript().await.unwrap();
        let context = transcript
            .entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                phi_sdk::transcript::TranscriptEntryKind::Message {
                    message: phi_sdk::transcript::AgentMessage::Context { content, .. },
                } => content.last().and_then(|block| match block {
                    phi_sdk::transcript::ContentBlock::Text { text } => Some(text),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap();
        assert!(context.contains("updated host context"));
        assert!(context.ends_with(
            "\nIf needed, use memory_search to look up specific content and tape_read to read relevant conversation history."
        ));
    }
    let transcript = agent_session.instance().transcript().await.unwrap();
    assert_eq!(memory_contexts(&transcript), 2);
    Box::pin(async {
        let mut queued_session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        queued_session.working_dir = session.working_dir.clone();
        let queued_agent = test_runtime
            .sessions
            .create(
                &test_runtime.preparation,
                queued_session.clone(),
                options(&queued_session),
                phi_sdk::lifecycle::SessionStartReason::New,
                false,
            )
            .await
            .unwrap();
        let context_file = memory_dir.join("context.md");
        let backup = root.join("queue-context.md");
        std::fs::rename(&context_file, &backup).unwrap();
        queued_agent.instance().submit(input.clone()).await.unwrap();
        std::fs::rename(backup, context_file).unwrap();
        let handled = queued_agent
            .submit(
                phi_sdk::turn::TurnPrompt {
                    prompt: "handled".into(),
                    ..input.clone()
                },
                None,
                Some(phi_sdk::turn::SubmissionDispatch::FollowUp),
            )
            .await
            .unwrap();
        assert!(matches!(
            handled.disposition,
            phi_sdk::turn::SubmissionDisposition::Handled { .. }
        ));
        let queued_prompt = phi_sdk::turn::TurnPrompt {
            prompt: "queued".into(),
            ..input.clone()
        };
        let (first, second) = tokio::join!(
            queued_agent.submit(
                queued_prompt.clone(),
                None,
                Some(phi_sdk::turn::SubmissionDispatch::FollowUp)
            ),
            queued_agent.submit(
                queued_prompt,
                None,
                Some(phi_sdk::turn::SubmissionDispatch::FollowUp)
            ),
        );
        let submissions = [first.unwrap(), second.unwrap()];
        assert!(submissions.iter().all(|receipt| matches!(
            receipt.disposition,
            phi_sdk::turn::SubmissionDisposition::Queued { .. }
        )));
        assert_eq!(
            submissions
                .iter()
                .flat_map(|receipt| &receipt.notices)
                .filter(|notice| matches!(
                    notice,
                    phi_sdk::hooks::HookNotice::ContextInjected { .. }
                ))
                .count(),
            0
        );
        assert_eq!(
            queued_agent
                .recovery()
                .await
                .unwrap()
                .pending_submission_ids
                .len(),
            2
        );
        queued_agent.instance().abort().await.unwrap();
        let next = queued_agent
            .submit(input.clone(), None, None)
            .await
            .unwrap();
        assert!(
            next.notices.iter().all(|notice| !matches!(
                notice,
                phi_sdk::hooks::HookNotice::ContextInjected { .. }
            ))
        );
        queued_agent.instance().abort().await.unwrap();
    })
    .await;
    Box::pin(async {
        let mut child =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        child.working_dir = session.working_dir.clone();
        child.bind_test_parent(&agent_session.session().await);
        let prepared = Box::pin(test_runtime.preparation.prepare(options(&child)))
            .await
            .unwrap();
        let (candidate, runtime) = prepared.into_parts();
        let registered = test_runtime
            .manager
            .for_agent(agent_session.instance().id())
            .unwrap()
            .register_child(candidate)
            .unwrap();
        let instance = test_runtime.manager.get(&registered.instance.id).unwrap();
        agent_session
            .record_child_intent(crate::session::ChildSessionLink {
                child_session_id: child.id.clone(),
                child_agent_id: child.id.to_string(),
                child_working_dir: child.working_dir.clone(),
                definition: None,
                alias: None,
                parent_tool_call_id: None,
                workflow_run_id: None,
                mode: phi_sdk::agent_manager::DelegationMode::Background,
                recovered_outcome: None,
            })
            .await
            .unwrap();
        let (_cancel, mut cancellation) = tokio::sync::watch::channel(false);
        let agent_session = test_runtime
            .sessions
            .associate_child_session(
                agent_session.instance().id(),
                child,
                runtime,
                instance,
                &mut cancellation,
            )
            .await
            .unwrap();
        test_runtime
            .sessions
            .start_agent_session(
                &agent_session,
                phi_sdk::lifecycle::SessionStartReason::New,
                Some(session.id.as_str()),
            )
            .await;
        for prompt in ["inspect", "/memory-inject inspect", "/memory-inject"] {
            let mut turn = input.clone();
            turn.prompt = prompt.into();
            let before =
                serde_json::to_value(agent_session.instance().transcript().await.unwrap()).unwrap();
            let result = agent_session.submit(turn, None, None).await;
            if prompt.starts_with("/memory-inject") {
                assert!(
                    result
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("No Memory/Tape context available")
                );
                assert_eq!(
                    serde_json::to_value(agent_session.instance().transcript().await.unwrap())
                        .unwrap(),
                    before
                );
            } else {
                assert!(matches!(
                    result.unwrap().disposition,
                    phi_sdk::turn::SubmissionDisposition::Started { .. }
                ));
                agent_session.instance().abort().await.unwrap();
            }
        }
        let transcript = agent_session.instance().transcript().await.unwrap();
        assert_eq!(memory_contexts(&transcript), 0);
        assert!(
            !serde_json::to_string(&transcript)
                .unwrap()
                .contains("host-only-memory")
        );
    })
    .await;
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    provider.abort();
    std::fs::remove_dir_all(root).unwrap();
}
