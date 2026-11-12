use super::*;

#[tokio::test]
async fn anchor_jump_preserves_two_commit_boundary() {
    for change_settings_after_resume in [false, true] {
        let (root, policy, test_runtime) = session_test_runtime();
        let target_dir = root.join("target");
        std::fs::create_dir_all(target_dir.join(".phi")).unwrap();
        let boundaries = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = boundaries.clone();
        let settings = target_dir.join(".phi/settings.json");
        std::fs::write(&settings, br#"{"permission":{"bash":"ask"}}"#).unwrap();
        let mut configuration = policy.configuration.snapshot();
        configuration.settings.permission = Some(serde_json::json!({"bash":"ask"}));
        configuration.settings.no_tools = Some(false);
        configuration.hooks.push(Hook::session_start(move |event| {
            if matches!(event.reason, SessionStartReason::Resume) {
                observed.lock().unwrap().push(event.related_session.clone());
                if change_settings_after_resume {
                    std::fs::write(&settings, br#"{"permission":{"bash":"invalid"}}"#).unwrap();
                }
            }
            async { Ok(Vec::new()) }
        }));
        policy.configuration.publish_configuration(configuration);
        policy.install(&test_runtime.preparation);
        let mut current =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        current.working_dir = root.to_string_lossy().into();
        let outgoing = Box::pin(
            {
                policy.install(&test_runtime.preparation);
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
            }
            .start_session(current, false),
        )
        .await
        .unwrap();
        let current_id = outgoing.instance().id().clone();
        let before = outgoing.snapshot().await.runtime_generation_id;
        let mut target =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        target.working_dir = target_dir.to_string_lossy().into();
        target.transcript.append_user_message("anchor source");
        let anchor = target.leaf_id.clone().unwrap();
        target.transcript.append_assistant_message(
            None,
            None,
            vec![crate::session::ContentBlock::Text {
                text: "anchor destination".into(),
            }],
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "stop".into(),
        );
        let destination = target.leaf_id.clone().unwrap();
        target.transcript.append_user_message("later history");
        let saved_leaf = target.leaf_id.clone();
        target
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: "bash".into(),
                pattern: "printf anchor-grant".into(),
            });
        crate::session::storage::save_session(&target).unwrap();
        for (target_id, entry) in [
            ("missing", anchor.as_str()),
            (target.id.as_str(), "missing"),
        ] {
            assert!(
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
                .jump_anchor(&current_id, target_id, entry)
                .await
                .is_err()
            );
            assert_eq!(outgoing.snapshot().await.runtime_generation_id, before);
            assert!(test_runtime.sessions.get(&current_id).is_ok());
        }
        let crate::runtime::preparation::AcceptedTransition {
            attachment,
            accepted:
                crate::runtime::RuntimeView {
                    session: accepted,
                    snapshot:
                        crate::runtime::AgentSessionSnapshot {
                            persistence_error, ..
                        },
                    declarations,
                },
            navigation_error,
        } = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .jump_anchor(&current_id, &target.id, &anchor)
        .await
        .unwrap();
        assert_eq!(
            declarations.generation_id,
            test_runtime
                .sessions
                .get(attachment.id())
                .unwrap()
                .snapshot()
                .await
                .runtime_generation_id
        );
        assert_eq!(attachment.id().as_str(), target.id.as_str());
        assert_eq!(accepted.working_dir, target.working_dir);
        assert_eq!(accepted.permission_allowlist, target.permission_allowlist);
        assert!(
            crate::runtime::session::tests::session_command_allowed(
                &test_runtime.sessions.get(attachment.id()).unwrap(),
                "anchor-grant",
            )
            .await
        );
        assert!(navigation_error.is_none());
        assert!(persistence_error.is_none());
        assert_eq!(accepted.leaf_id, Some(destination));
        assert!(test_runtime.sessions.get(&current_id).is_err());
        assert_eq!(*boundaries.lock().unwrap(), vec![None]);
        let durable = crate::session::storage::load_session_id(&target.id).unwrap();
        assert_eq!(durable.leaf_id, saved_leaf);
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn anchor_jump_handles_current_live_and_rejected_saved_targets() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
    let mut target =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    target.working_dir = root.to_string_lossy().into();
    target.transcript.append_assistant_message(
        None,
        None,
        vec![crate::session::ContentBlock::Text {
            text: "live target".into(),
        }],
        "local".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "test".into(),
        Default::default(),
        "stop".into(),
    );
    let destination = target.leaf_id.clone();
    target.transcript.append_user_message("later");
    let live = Box::pin(
        {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(target.clone(), true),
    )
    .await
    .unwrap();
    let target_id = live.instance().id().clone();
    let registration = live.instance().registration_id();
    let factory = test_runtime.preparation.runtime_factory().unwrap();
    for target_name in ["", target.id.as_str()] {
        let crate::runtime::preparation::AcceptedTransition {
            attachment,
            accepted:
                crate::runtime::RuntimeView {
                    session: selected,
                    snapshot:
                        crate::runtime::AgentSessionSnapshot {
                            persistence_error: warning,
                            ..
                        },
                    ..
                },
            navigation_error,
        } = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .jump_anchor(&target_id, target_name, target.id.as_str())
        .await
        .unwrap();
        assert_eq!(attachment.id(), &target_id);
        assert_eq!(selected.leaf_id, destination);
        assert!(navigation_error.is_none() && warning.is_none());
        assert_eq!(live.instance().registration_id(), registration);
    }
    let mut current =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    current.working_dir = root.to_string_lossy().into();
    let outgoing = Box::pin(
        {
            policy.install(&test_runtime.preparation);
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
        }
        .start_session(current, false),
    )
    .await
    .unwrap();
    let id = outgoing.instance().id().clone();
    let before = outgoing.snapshot().await.runtime_generation_id;
    let mut delegated = target.clone();
    delegated.transcript.id = uuid::Uuid::new_v4().to_string().into();
    delegated.bind_test_parent(&target);
    crate::session::storage::save_session(&delegated).unwrap();
    assert!(
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .jump_anchor(&id, &delegated.id, &delegated.id)
        .await
        .is_err()
    );
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    let settings = root.join(".phi/settings.json");
    std::fs::write(&settings, r#"{"permission":{"bash":"invalid"}}"#).unwrap();
    assert!(
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .jump_anchor(&id, &target.id, &target.id)
        .await
        .is_err()
    );
    assert_eq!(outgoing.snapshot().await.runtime_generation_id, before);
    assert!(test_runtime.sessions.get(&id).is_ok());
    assert!(Arc::ptr_eq(
        &factory,
        &test_runtime.preparation.runtime_factory().unwrap()
    ));
    std::fs::remove_file(settings).unwrap();
    let crate::runtime::preparation::AcceptedTransition {
        attachment,
        accepted:
            crate::runtime::RuntimeView {
                session: selected,
                snapshot:
                    crate::runtime::AgentSessionSnapshot {
                        persistence_error: warning,
                        ..
                    },
                ..
            },
        navigation_error,
    } = crate::runtime::preparation::SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap()
    .jump_anchor(&id, &target.id, &target.id)
    .await
    .unwrap();
    assert_eq!(attachment.id(), &target_id);
    assert_eq!(selected.leaf_id, destination);
    assert!(navigation_error.is_none() && warning.is_none());
    assert_eq!(live.instance().registration_id(), registration);
    assert!(test_runtime.sessions.get(&id).is_err());
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
async fn branch_navigation_stays_transient_through_checkpoint_and_stop() {
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.settings.permission = Some(serde_json::json!({"bash":"ask"}));
    configuration.settings.no_tools = Some(false);
    configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
        name: "plan".into(),
        mode: Some("plan".into()),
        prompt: Some("branch plan instructions".into()),
        ..Default::default()
    }]);
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
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    let settings_path = root.join(".phi/settings.json");
    std::fs::write(
        &settings_path,
        serde_json::to_vec(&serde_json::json!({
            "permission": configuration.settings.permission, "switches": configuration.settings.switches,
        }))
        .unwrap(),
    )
    .unwrap();
    policy
        .configuration
        .publish_configuration(configuration.clone());
    policy.install(&test_runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.transcript.append_user_message("root draft");
    let root_user = session.leaf_id.clone().unwrap();
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
    let first = session.leaf_id.clone().unwrap();
    session.transcript.append_user_message("restore this draft");
    let user = session.leaf_id.clone().unwrap();
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
    session.current_switch = "plan".into();
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
    agent.stop_checkpoint_observer().await;
    let access = test_runtime.sessions.session_access(&id);
    access.checkpoint().await.unwrap();
    let initial = agent.recovery().await.unwrap();
    let path = crate::session::storage::session_path(&initial.session);
    let saved = std::fs::read(&path).unwrap();
    let factory = test_runtime.preparation.runtime_factory().unwrap();
    assert!(
        Box::pin(async {
            test_runtime
                .preparation
                .navigate_branch(
                    &test_runtime.sessions.session_access(&id),
                    BranchNavigation::Rewind {
                        entry_id: "missing".into(),
                    },
                )
                .await
        })
        .await
        .is_err()
    );
    assert_eq!(
        agent.snapshot().await.runtime_generation_id,
        initial.snapshot.runtime_generation_id
    );
    let crate::runtime::RuntimeView {
        session: rewound,
        snapshot:
            crate::runtime::AgentSessionSnapshot {
                persistence_error: warning,
                ..
            },
        declarations,
    } = Box::pin(async {
        test_runtime
            .preparation
            .navigate_branch(
                &test_runtime.sessions.session_access(&id),
                BranchNavigation::Rewind {
                    entry_id: user.to_string(),
                },
            )
            .await
    })
    .await
    .unwrap();
    assert!(warning.is_none());
    assert!(std::fs::read(&path).unwrap() == saved);
    assert_eq!(
        declarations.generation_id,
        agent.snapshot().await.runtime_generation_id
    );
    assert_eq!(
        rewound.leaf_id,
        rewound
            .entries
            .iter()
            .find(|entry| entry.id == user)
            .unwrap()
            .parent_id
    );
    assert_eq!(rewound.current_switch, "plan");
    agent.ready_hook_notices().await;
    assert!(
        prompts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("branch plan instructions")
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let result = test_runtime
        .preparation
        .navigate_branch(
            &test_runtime.sessions.session_access(&id),
            BranchNavigation::Entry {
                entry_id: first.to_string(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.session.leaf_id.as_deref(), Some(first.as_str()));
    assert_eq!(result.session.current_switch, "plan");
    assert!(result.snapshot.persistence_error.is_none());
    std::fs::remove_dir(&path).unwrap();
    test_runtime
        .sessions
        .session_access(&id)
        .checkpoint()
        .await
        .unwrap();
    let durable = crate::session::storage::load_session_id(id.as_str()).unwrap();
    assert_eq!(durable.leaf_id, initial.session.leaf_id);
    let saved_boundary = agent
        .save_observation()
        .await
        .completed
        .unwrap()
        .saved_boundary
        .unwrap();
    assert_eq!(
        saved_boundary.leaf_id.as_deref(),
        durable.leaf_id.as_deref()
    );
    let selected = agent.recovery().await.unwrap();
    assert_eq!(
        selected.session.entries.len(),
        initial.session.entries.len()
    );
    assert_eq!(
        selected.session.permission_allowlist,
        initial.session.permission_allowlist
    );
    assert_eq!(agent.instance().registration_id(), registration);
    assert!(crate::runtime::session::tests::session_command_allowed(&agent, "own-grant").await);
    agent.ready_hook_notices().await;
    assert!(
        prompts
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("branch plan instructions")
    );
    configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
        name: "default".into(),
        model: Some("missing".into()),
        ..Default::default()
    }]);
    std::fs::write(
        &settings_path,
        serde_json::to_vec(&serde_json::json!({
            "permission": {"bash":"ask"}, "switches": configuration.settings.switches,
        }))
        .unwrap(),
    )
    .unwrap();
    let crate::runtime::RuntimeView {
        session: selected, ..
    } = Box::pin(async {
        test_runtime
            .preparation
            .navigate_branch(
                &test_runtime.sessions.session_access(&id),
                BranchNavigation::Rewind {
                    entry_id: root_user.to_string(),
                },
            )
            .await
    })
    .await
    .unwrap();
    assert!(selected.leaf_id.is_none());
    assert_eq!(
        serde_json::to_value(&selected.entries).unwrap(),
        serde_json::to_value(&initial.session.entries).unwrap()
    );
    agent
        .allow_session_tools(&[("bash".into(), "printf after-navigation".into())])
        .await
        .unwrap();
    let durable = crate::session::storage::load_session_id(id.as_str()).unwrap();
    assert_eq!(durable.leaf_id, initial.session.leaf_id);
    assert!(Arc::ptr_eq(
        &factory,
        &test_runtime.preparation.runtime_factory().unwrap()
    ));
    test_runtime
        .sessions
        .stop(&id, SessionEndReason::Quit)
        .await
        .unwrap();
    let reopened = crate::session::storage::load_session_id(id.as_str()).unwrap();
    assert_eq!(reopened.leaf_id, initial.session.leaf_id);
    assert_eq!(
        serde_json::to_value(&reopened.entries).unwrap(),
        serde_json::to_value(&initial.session.entries).unwrap()
    );
    std::fs::write(&settings_path, br#"{"permission":{"bash":"ask"}}"#).unwrap();
    let resumed = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap()
    .start_saved_session(id.as_str(), false)
    .await
    .unwrap();
    assert_eq!(
        resumed.recovery().await.unwrap().session.leaf_id,
        initial.session.leaf_id
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
