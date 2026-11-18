use super::*;

#[tokio::test]
async fn scoped_access_preserves_ephemeral_session() {
    let (root, policy, test_runtime) = session_test_runtime();
    let session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    let generation = policy
        .configuration
        .snapshot()
        .bind_settings(std::path::Path::new(&session.working_dir))
        .unwrap();
    let agent = test_runtime
        .sessions
        .create(
            &test_runtime.preparation,
            session.clone(),
            generation.runtime_build_options(&session),
            SessionStartReason::New,
            false,
        )
        .await
        .unwrap();
    let access = test_runtime.sessions.session_access(agent.instance().id());
    assert!(Arc::ptr_eq(&agent, &access.binding().unwrap()));
    policy.install(&test_runtime.preparation);
    let attachment = crate::runtime::attachment::AgentAttachment::new(
        &agent,
        access,
        test_runtime.preparation.clone(),
    );
    assert!(
        !crate::session::storage::session_path(&session)
            .with_extension("lock")
            .exists()
    );
    assert_eq!(attachment.id(), agent.instance().id());
    let error = test_runtime
        .sessions
        .delete_session(&session.id)
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<crate::runtime::ActiveSessionDeletion>()
            .is_some()
    );
    assert_eq!(test_runtime.sessions.list().await.unwrap().len(), 1);
    test_runtime
        .sessions
        .stop(agent.instance().id(), SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn new_session_transitions_preserve_process_policy_and_lifecycle() {
    let (root, policy, test_runtime) = session_test_runtime();
    let events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let start_events = events.clone();
    let end_events = events.clone();
    let run_events = events.clone();
    let mut configuration = policy.configuration.snapshot();
    configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
        name: "review".into(),
        provider: Some("local".into()),
        model: Some("test".into()),
        mode: Some("plan".into()),
        ..Default::default()
    }]);
    configuration.hooks = vec![
        Hook::session_start(move |event| {
            start_events.lock().unwrap().push((
                "start",
                format!("{:?}", event.reason),
                event.related_session,
            ));
            async { Ok(Vec::new()) }
        }),
        Hook::session_end(move |event| {
            end_events.lock().unwrap().push((
                "end",
                format!("{:?}", event.reason),
                event.related_session,
            ));
            async { Ok(Vec::new()) }
        }),
        Hook::agent_start(move |_| {
            run_events
                .lock()
                .unwrap()
                .push(("run", String::new(), None));
            async { Ok(()) }
        }),
    ];
    *policy.configuration.generation.write().unwrap() = configuration.clone();
    policy.install(&test_runtime.preparation);
    let bystander = Box::pin(
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .create_session(crate::runtime::preparation::CreateSessionRequest {
            provider: None,
            model: None,
            switch: Some("default".to_string()),
            persist: true,
        }),
    )
    .await
    .map(|agent| {
        crate::runtime::attachment::AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        )
    })
    .unwrap();
    let working_dir = root.join("agent");
    std::fs::create_dir_all(&working_dir).unwrap();
    let mut seed = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    seed.working_dir = working_dir.to_string_lossy().into();
    seed.current_switch = "review".into();
    seed.permission_allowlist
        .push(crate::session::PermissionAllowEntry {
            tool: "Bash".into(),
            pattern: "git status:*".into(),
        });
    seed.transcript.append_user_message("old history");
    seed.transcript.append_assistant_message(
        None,
        None,
        Vec::new(),
        "test".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "local".into(),
        Default::default(),
        "stop".into(),
    );
    let current = {
        policy.install(&test_runtime.preparation);
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
    }
    .start_session(seed.clone(), true)
    .await
    .unwrap();
    let current_handle = current.instance();
    let current_id = current_handle.id();
    events.lock().unwrap().clear();
    test_runtime
        .sessions
        .session_access(current_id)
        .checkpoint()
        .await
        .unwrap();
    let saved_path = crate::session::storage::session_path(&seed);
    std::fs::remove_file(&saved_path).unwrap();
    std::fs::create_dir(&saved_path).unwrap();
    assert!(
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .new_session(current_id)
        .await
        .map(|transition| (transition.attachment, transition.accepted))
        .is_err()
    );
    assert!(events.lock().unwrap().is_empty());
    assert!(
        test_runtime
            .sessions
            .open_existing(current_id)
            .unwrap()
            .is_some()
    );
    assert_eq!(test_runtime.sessions.list().await.unwrap().len(), 2);
    std::fs::remove_dir(&saved_path).unwrap();
    let next = crate::runtime::preparation::SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap()
    .new_session(current_id)
    .await
    .map(|transition| (transition.attachment, transition.accepted))
    .unwrap()
    .1
    .session;
    assert_ne!(next.id, seed.id);
    assert_eq!(next.working_dir, seed.working_dir);
    assert_eq!(next.provider, "local");
    assert_eq!(next.model, "test");
    assert_eq!(next.current_switch, "review");
    assert!(!next.transcript.has_user_message());
    assert!(next.permission_allowlist.is_empty());
    assert!(next.creator_id().is_none());
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            (
                "end",
                "New".into(),
                Some(
                    crate::session::storage::session_path(&next)
                        .to_string_lossy()
                        .into_owned()
                )
            ),
            (
                "start",
                "New".into(),
                Some(
                    crate::session::storage::session_path(&seed)
                        .to_string_lossy()
                        .into_owned()
                )
            ),
        ]
    );
    assert!(
        test_runtime
            .sessions
            .open_existing(current_id)
            .unwrap()
            .is_none()
    );
    assert!(
        test_runtime
            .sessions
            .open_existing(bystander.id())
            .unwrap()
            .is_some()
    );
    let next_id = AgentInstanceId::new(next.id.to_string());
    assert!(matches!(
        test_runtime
            .sessions
            .get(&next_id)
            .unwrap()
            .snapshot()
            .await
            .instance
            .status,
        phi_sdk::agent_instance::AgentInstanceStatus::Idle
    ));
    test_runtime
        .sessions
        .stop(&next_id, SessionEndReason::Quit)
        .await
        .unwrap();
    crate::session::storage::delete_session(&seed.id).unwrap();
    test_runtime
        .sessions
        .stop(bystander.id(), SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn session_transition_waiters_do_not_own_committed_lifecycle() {
    use std::time::Duration;
    let (root, policy, test_runtime) = session_test_runtime();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let gate_entered = entered.clone();
    let gate_release = release.clone();
    let ends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let end_count = ends.clone();
    let mut configuration = policy.configuration.snapshot();
    configuration.hooks = vec![Hook::session_end(move |event| {
        let entered = gate_entered.clone();
        let release = gate_release.clone();
        let count = end_count.clone();
        async move {
            if matches!(
                event.reason,
                SessionEndReason::New | SessionEndReason::Resume
            ) {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                entered.notify_one();
                release.notified().await;
            }
            Ok(Vec::new())
        }
    })];
    *policy.configuration.generation.write().unwrap() = configuration;
    policy.install(&test_runtime.preparation);
    let bystander = Box::pin(
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .create_session(crate::runtime::preparation::CreateSessionRequest {
            provider: None,
            model: None,
            switch: Some("default".to_string()),
            persist: true,
        }),
    )
    .await
    .map(|agent| {
        crate::runtime::attachment::AgentAttachment::new(
            &agent,
            test_runtime.sessions.session_access(agent.instance().id()),
            test_runtime.preparation.clone(),
        )
    })
    .unwrap();
    let mut target =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    target.working_dir = root.to_string_lossy().into();
    target.transcript.append_user_message("resume target");
    target.transcript.append_assistant_message(
        None,
        None,
        Vec::new(),
        "local".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "test".into(),
        Default::default(),
        "stop".into(),
    );
    crate::session::storage::save_session(&target).unwrap();
    for resume in [false, true] {
        let current = Box::pin(
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
            .create_session(crate::runtime::preparation::CreateSessionRequest {
                provider: None,
                model: None,
                switch: Some("default".to_string()),
                persist: true,
            }),
        )
        .await
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
        let id = current.id().clone();
        let caller = test_runtime.clone();
        let current_id = id.clone();
        let target_id = target.id.clone();
        let waiter = tokio::spawn(async move {
            if resume {
                crate::runtime::preparation::SessionWorkflows::new(
                    caller.sessions.clone(),
                    caller.preparation.clone(),
                )
                .unwrap()
                .resume_session(
                    &current_id,
                    crate::runtime::SavedSessionLocator::IdOrPath(target_id.as_str()),
                )
                .await
                .map(|transition| (transition.attachment, transition.accepted))
            } else {
                crate::runtime::preparation::SessionWorkflows::new(
                    caller.sessions.clone(),
                    caller.preparation.clone(),
                )
                .unwrap()
                .new_session(&current_id)
                .await
                .map(|transition| (transition.attachment, transition.accepted))
            }
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), bystander.abort())
            .await
            .expect("pending transition blocked unrelated control")
            .unwrap();
        if resume {
            assert!(
                crate::session::storage::lock_session_path(&crate::session::storage::session_path(
                    &target
                ))
                .is_err()
            );
        }
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        if resume {
            assert!(
                crate::session::storage::lock_session_path(&crate::session::storage::session_path(
                    &target
                ))
                .is_err()
            );
        }
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let agents = test_runtime.sessions.list().await.unwrap();
                if agents.len() == 2 && !agents.iter().any(|agent| agent.instance.id == id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("accepted transition must finish after waiter disappears");
        let next = test_runtime
            .sessions
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|agent| &agent.instance.id != bystander.id())
            .unwrap();
        test_runtime
            .sessions
            .stop(&next.instance.id, SessionEndReason::Quit)
            .await
            .unwrap();
    }
    assert_eq!(ends.load(std::sync::atomic::Ordering::SeqCst), 2);
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    crate::session::storage::delete_session(&target.id).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn session_workflows_outlive_fixture_and_finalize_canonical_history() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let weak_runtime = Arc::downgrade(&test_runtime);
    let weak_sessions = Arc::downgrade(&workflows.sessions);
    drop(policy);
    drop(test_runtime);
    assert!(weak_runtime.upgrade().is_none());

    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.transcript.append_user_message("preserved history");
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
    let original_id = AgentInstanceId::new(session.id.to_string());
    let original = workflows.start_session(session, true).await.unwrap();
    let original_access = workflows.sessions.session_access(&original_id);
    let registration = original.instance().registration_id();
    assert!(
        workflows
            .resume_session(
                &original_id,
                crate::runtime::SavedSessionLocator::Id("missing")
            )
            .await
            .is_err()
    );
    assert_eq!(original.instance().registration_id(), registration);
    assert!(original_access.binding().is_ok());

    let next = workflows.new_session(&original_id).await.unwrap();
    let next_id = next.attachment.id().clone();
    assert_ne!(next_id, original_id);
    assert!(original_access.binding().is_err());
    let saved = crate::session::storage::load_session_checkpoint(original_id.as_str()).unwrap();
    assert_eq!(
        serde_json::to_value(saved.transcript).unwrap(),
        serde_json::to_value(original.instance().transcript().await.unwrap()).unwrap()
    );
    let resumed = workflows
        .resume_session(
            &next_id,
            crate::runtime::SavedSessionLocator::Id(original_id.as_str()),
        )
        .await
        .unwrap();
    assert_eq!(resumed.attachment.id(), &original_id);
    assert!(workflows.sessions.get(&next_id).is_err());
    let reopened = workflows
        .sessions
        .session_access(&original_id)
        .binding()
        .unwrap();
    assert!(Arc::ptr_eq(
        &reopened,
        &workflows.sessions.get(resumed.attachment.id()).unwrap()
    ));
    assert_ne!(reopened.instance().registration_id(), registration);
    workflows
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    assert!(original_access.binding().is_err());
    let saved = crate::session::storage::load_session_checkpoint(original_id.as_str()).unwrap();
    for session in [&saved, &next.accepted.session] {
        let lock = crate::session::storage::lock_session_path(
            &crate::session::storage::session_path(session),
        )
        .unwrap();
        drop(lock);
    }
    assert!(!crate::session::storage::session_path(&next.accepted.session).exists());
    assert_eq!(
        serde_json::to_value(saved.transcript).unwrap(),
        serde_json::to_value(reopened.instance().transcript().await.unwrap()).unwrap()
    );
    drop(original_access);
    drop(next);
    drop(resumed);
    drop(workflows);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while weak_sessions.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn scoped_workflows_outlive_fixture_and_revoke_after_finalization() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    std::fs::write(root.join(".phi/settings.json"), b"{}").unwrap();
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    session.transcript.append_user_message("preserved history");
    let target = session.transcript.leaf_id.clone().unwrap();
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
    let agent = {
        policy.install(&test_runtime.preparation);
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
    }
    .start_session(session, true)
    .await
    .unwrap();
    let access = test_runtime.sessions.session_access(agent.instance().id());
    let attachment = crate::runtime::attachment::AgentAttachment::new(
        &agent,
        access.clone(),
        test_runtime.preparation.clone(),
    );
    let before = agent.recovery().await.unwrap();
    let registration = agent.instance().registration_id();
    let weak_runtime = Arc::downgrade(&test_runtime);
    let weak_picker = Arc::downgrade(&policy.choices.picker_seed);
    let weak_configuration = Arc::downgrade(&policy.configuration);
    drop(policy);
    drop(test_runtime);
    assert!(weak_runtime.upgrade().is_none());
    assert!(weak_picker.upgrade().is_none());
    assert!(weak_configuration.upgrade().is_some());
    assert!(
        attachment
            .configure_runtime(RuntimeSelection::Model {
                selection: "model:local:missing".into(),
            })
            .await
            .is_err()
    );
    assert_eq!(
        agent.snapshot().await.runtime_generation_id,
        before.snapshot.runtime_generation_id
    );
    let configured = attachment
        .configure_runtime(RuntimeSelection::Model {
            selection: "model:local:test".into(),
        })
        .await
        .unwrap();
    assert_ne!(
        configured.declarations.generation_id,
        before.declarations.generation_id
    );
    let navigated = attachment
        .navigate_branch(BranchNavigation::Entry {
            entry_id: target.to_string(),
        })
        .await
        .unwrap();
    assert_eq!(navigated.session.leaf_id, Some(target));
    assert_eq!(agent.instance().registration_id(), registration);
    assert_ne!(
        navigated.declarations.generation_id,
        configured.declarations.generation_id
    );
    agent.instance().stop().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while access.binding().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        attachment
            .configure_runtime(RuntimeSelection::Model {
                selection: "model:local:test".into(),
            })
            .await
            .is_err()
    );
    assert!(
        attachment
            .navigate_branch(BranchNavigation::Rewind {
                entry_id: "stopped".into(),
            })
            .await
            .is_err()
    );
    let saved = crate::session::storage::load_session_checkpoint(attachment.id().as_str()).unwrap();
    let mut expected = agent.instance().transcript().await.unwrap();
    expected.leaf_id = expected.entries.last().map(|entry| entry.id.clone());
    assert_eq!(
        serde_json::to_value(saved.transcript).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    let lock = crate::session::storage::lock_session_path(&crate::session::storage::session_path(
        &navigated.session,
    ))
    .unwrap();
    drop(lock);
    drop(attachment);
    assert!(weak_configuration.upgrade().is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn resume_current_unsaved_session_is_noop() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
    for persist in [false, true] {
        let agent = {
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
            persist,
        })
        .await
        .unwrap();
        let id = agent.instance().id().clone();
        let session = agent.session().await;
        assert!(!crate::session::storage::session_path(&session).exists());
        let generation = agent.snapshot().await.runtime_generation_id;
        let (attached, accepted) = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .resume_session(
            &id,
            crate::runtime::SavedSessionLocator::IdOrPath(id.as_str()),
        )
        .await
        .map(|transition| (transition.attachment, transition.accepted))
        .unwrap();
        let resumed = accepted.session;
        assert_eq!(attached.id(), &id);
        assert_eq!(resumed.id, session.id);
        assert!(Arc::ptr_eq(
            &agent,
            &test_runtime.sessions.get(&id).unwrap()
        ));
        assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
        assert!(!crate::session::storage::session_path(&session).exists());
        test_runtime
            .sessions
            .stop(&id, SessionEndReason::Quit)
            .await
            .unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn resume_uses_current_selection_not_historical_switch() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy
        .configuration
        .generation
        .write()
        .unwrap()
        .settings
        .switches = Some(vec![crate::config::SwitchConfig {
        name: "plan".into(),
        thinking_level: Some("high".into()),
        prompt: Some("accepted plan instructions".into()),
        ..Default::default()
    }]);
    policy.install(&test_runtime.preparation);
    let workflows = crate::runtime::preparation::SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let current = Box::pin(workflows.create_session(CreateSessionRequest {
        provider: None,
        model: None,
        switch: Some("plan".into()),
        persist: true,
    }))
    .await
    .unwrap();
    let mut target = crate::session::new(
        "missing-provider",
        "retired-model",
        128_000,
        &std::env::current_dir().unwrap(),
    );
    target.current_switch = "deleted-switch".into();
    let target_dir = root.join("target");
    std::fs::create_dir_all(target_dir.join(".phi")).unwrap();
    std::fs::write(target_dir.join(".phi/settings.json"), r#"{"switches":[{"name":"plan","thinking_level":"low","prompt":"target plan instructions"}]}"#).unwrap();
    target.working_dir = target_dir.to_string_lossy().into();
    target
        .transcript
        .switch_snapshots
        .push(phi_sdk::transcript::SwitchSnapshotRecord {
            id: "deleted-switch-1".into(),
            switch: phi_sdk::transcript::SwitchSnapshot {
                name: "deleted-switch".into(),
                provider: "missing-provider".into(),
                model: "retired-model".into(),
                thinking_level: Some("low".into()),
                mode: "default".into(),
                prompt: Some("historical instructions".into()),
            },
        });
    target.transcript.append_user_message("saved history");
    target
        .transcript
        .append_entry(crate::session::TranscriptEntryKind::Message {
            message: crate::session::AgentMessage::Assistant {
                id: None,
                turn_id: None,
                content: Vec::new(),
                provider: "missing-provider".into(),
                api: None,
                model: "retired-model".into(),
                switch: Some("deleted-switch-1".into()),
                usage: Default::default(),
                stop_reason: "stop".into(),
            },
        });
    crate::session::storage::save_session(&target).unwrap();
    let history = serde_json::to_value(&target.transcript.entries).unwrap();
    let resumed = workflows
        .resume_session(
            current.instance().id(),
            crate::runtime::SavedSessionLocator::Id(&target.id),
        )
        .await
        .unwrap();
    assert_eq!(resumed.accepted.session.current_switch, "plan");
    assert_eq!(resumed.accepted.session.provider, "local");
    assert_eq!(resumed.accepted.session.model, "test");
    assert_eq!(
        serde_json::to_value(&resumed.accepted.session.transcript.entries).unwrap(),
        history
    );
    let config = test_runtime
        .sessions
        .get(resumed.attachment.id())
        .unwrap()
        .runtime()
        .runtime_config();
    assert_eq!(config.agent_mode, phi_sdk::config::AgentMode::Plan);
    assert_eq!(config.default_thinking_level.as_deref(), Some("high"));
    assert_eq!(
        config.mode_prompt_override.as_deref(),
        Some("accepted plan instructions")
    );
    assert_eq!(resumed.accepted.session.working_dir, target.working_dir);
    test_runtime
        .sessions
        .stop(resumed.attachment.id(), SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn resume_preserves_preparation_and_local_locators() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
    let mut target =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    target
        .transcript
        .append_user_message("resume saved history");
    target.transcript.append_assistant_message(
        None,
        None,
        Vec::new(),
        "local".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "test".into(),
        Default::default(),
        "stop".into(),
    );
    crate::session::storage::save_session(&target).unwrap();
    let path = crate::session::storage::session_path(&target);
    let mut invalid_sessions = Vec::new();
    let mut invalid = target.clone();
    invalid.id = uuid::Uuid::new_v4().to_string().into();
    invalid.transcript.id = invalid.id.clone();
    invalid.bind_test_parent(&target);
    crate::session::storage::save_session(&invalid).unwrap();
    invalid_sessions.push(invalid);
    let mut invalid_storage = target.clone();
    invalid_storage.id = uuid::Uuid::new_v4().to_string().into();
    invalid_storage.bind_test_parent(&target);
    invalid_storage.agent_kind = phi_sdk::agents::AgentKind::Master;
    let mut metadata = serde_json::to_value(&invalid_storage).unwrap();
    metadata["type"] = serde_json::json!("metadata");
    let invalid_path = root.join("invalid-storage.jsonl");
    let mut invalid_journal = std::fs::read(&path).unwrap();
    invalid_journal.extend_from_slice(format!("{metadata}\n").as_bytes());
    std::fs::write(&invalid_path, invalid_journal).unwrap();
    assert!(
        crate::session::storage::load_session(invalid_path.to_str().unwrap())
            .unwrap()
            .validate_storage()
            .is_err()
    );
    for locator in [target.id.as_str(), path.to_str().unwrap()] {
        let attachment = Box::pin(
            crate::runtime::preparation::SessionWorkflows::new(
                test_runtime.sessions.clone(),
                test_runtime.preparation.clone(),
            )
            .unwrap()
            .create_session(crate::runtime::preparation::CreateSessionRequest {
                provider: None,
                model: None,
                switch: Some("default".to_string()),
                persist: true,
            }),
        )
        .await
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
        let id = attachment.id();
        for invalid_target in std::iter::once("missing-session")
            .chain(invalid_sessions.iter().map(|session| session.id.as_str()))
            .chain(std::iter::once(invalid_path.to_str().unwrap()))
        {
            assert!(
                crate::runtime::preparation::SessionWorkflows::new(
                    test_runtime.sessions.clone(),
                    test_runtime.preparation.clone(),
                )
                .unwrap()
                .resume_session(
                    id,
                    crate::runtime::SavedSessionLocator::IdOrPath(invalid_target),
                )
                .await
                .map(|transition| (transition.attachment, transition.accepted))
                .is_err()
            );
            assert!(test_runtime.sessions.open_existing(id).unwrap().is_some());
        }
        for invalid in &invalid_sessions {
            drop(
                crate::session::storage::lock_session(invalid)
                    .expect("failed preparation must release the saved Session lock"),
            );
        }
        let (resumed, session) = crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .resume_session(id, crate::runtime::SavedSessionLocator::IdOrPath(locator))
        .await
        .map(|transition| (transition.attachment, transition.accepted))
        .unwrap();
        assert_eq!(resumed.id().as_str(), target.id.as_str());
        assert_eq!(
            serde_json::to_value(&session.session.transcript.entries[0]).unwrap(),
            serde_json::to_value(&target.transcript.entries[0]).unwrap(),
        );
        assert!(test_runtime.sessions.open_existing(id).unwrap().is_none());
        let target_id = AgentInstanceId::new(target.id.to_string());
        let generation = test_runtime
            .sessions
            .get(&target_id)
            .unwrap()
            .snapshot()
            .await
            .runtime_generation_id;
        crate::runtime::preparation::SessionWorkflows::new(
            test_runtime.sessions.clone(),
            test_runtime.preparation.clone(),
        )
        .unwrap()
        .resume_session(
            &target_id,
            crate::runtime::SavedSessionLocator::IdOrPath(locator),
        )
        .await
        .map(|transition| (transition.attachment, transition.accepted))
        .unwrap();
        assert_eq!(
            test_runtime
                .sessions
                .get(&target_id)
                .unwrap()
                .snapshot()
                .await
                .runtime_generation_id,
            generation
        );
        test_runtime
            .sessions
            .stop(&target_id, SessionEndReason::Quit)
            .await
            .unwrap();
    }
    assert!(test_runtime.sessions.list().await.unwrap().is_empty());
    crate::session::storage::delete_session(&target.id).unwrap();
    for session in invalid_sessions {
        crate::session::storage::delete_session(&session.id).unwrap();
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn runtime_replacement_preserves_new_child_links() {
    for persist in [false, true] {
        let (root, policy, runtime) = session_test_runtime();
        policy.install(&runtime.preparation);
        let configuration = policy.configuration.snapshot();
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        let agent = workflows.start_session(session, persist).await.unwrap();
        let candidate = agent.recovery().await.unwrap().session;
        let prepared = runtime
            .preparation
            .prepare(configuration.runtime_build_options(&candidate))
            .await
            .unwrap();
        let child: crate::session::ChildSessionLink = serde_json::from_value(serde_json::json!({
            "child_agent_id": "admitted-child", "child_session_id": "admitted-child",
            "child_working_dir": root, "parent_tool_call_id": "admitted-call",
            "mode": "background",
        }))
        .unwrap();
        agent.record_child_intent(child.clone()).await.unwrap();
        let accepted = agent
            .replace_runtime(prepared, candidate, None, true)
            .await
            .unwrap();
        assert_eq!(accepted.session.children, [child.clone()]);
        assert_eq!(agent.session().await.children, [child.clone()]);
        runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        if persist {
            let saved =
                crate::session::storage::load_session_checkpoint(agent.instance().id().as_str())
                    .unwrap();
            assert_eq!(saved.children, [child]);
            crate::session::storage::delete_session(agent.instance().id().as_str()).unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn fresh_intent_validates_selection_before_registration() {
    let (root, policy, test_runtime) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.cli = Arc::new(Cli::parse_from([
        "phi",
        "--provider",
        "missing",
        "--model",
        "missing",
    ]));
    configuration.settings.default_thinking_level = Some("medium".into());
    configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
        name: "review".into(),
        provider: Some("local".into()),
        model: Some("test".into()),
        thinking_level: Some("low".into()),
        mode: Some("plan".into()),
        prompt: Some("review prompt".into()),
        ..Default::default()
    }]);
    policy.configuration.publish_configuration(configuration);
    policy.install(&test_runtime.preparation);
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    for switch in [None, Some("review"), Some("missing")] {
        for provider in [None, Some("local")] {
            for model in [None, Some("test")] {
                let result = Box::pin(workflows.create_session(CreateSessionRequest {
                    provider: provider.map(Into::into),
                    model: model.map(Into::into),
                    switch: switch.map(Into::into),
                    persist: false,
                }))
                .await;
                let valid = switch == Some("review")
                    || (switch.is_none() && provider.is_some() && model.is_some());
                if valid {
                    let agent = result.unwrap();
                    let seed = agent.configuration_seed().await.unwrap();
                    assert_eq!(seed.session.provider, "local");
                    assert_eq!(seed.session.model, "test");
                    assert_eq!(seed.session.context_window, 128_000);
                    let (thinking, mode, prompt) = if switch.is_some() {
                        ("low", "plan", Some("review prompt"))
                    } else {
                        ("medium", "default", None)
                    };
                    assert_eq!(
                        seed.runtime
                            .runtime_config()
                            .default_thinking_level
                            .as_deref(),
                        Some(thinking)
                    );
                    assert_eq!(seed.runtime.runtime_config().agent_mode.as_str(), mode);
                    assert_eq!(
                        seed.runtime
                            .runtime_config()
                            .mode_prompt_override
                            .as_deref(),
                        prompt
                    );
                    let recorded = EffectiveSwitch::from_runtime(
                        &seed.session,
                        &seed.runtime.runtime_config(),
                    );
                    assert_eq!(recorded.provider, "local");
                    assert_eq!(recorded.model, "test");
                    test_runtime
                        .sessions
                        .stop(agent.instance().id(), SessionEndReason::Quit)
                        .await
                        .unwrap();
                } else {
                    assert!(result.is_err());
                }
                assert!(test_runtime.sessions.list().await.unwrap().is_empty());
            }
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn new_session_rejects_missing_configuration_without_stopping_current() {
    let (root, policy, test_runtime) = session_test_runtime();
    policy.install(&test_runtime.preparation);
    let configuration = policy.configuration.snapshot();
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.to_string_lossy().into();
    let mut options = configuration.runtime_build_options(&session);
    options.instance.resource_settings = None;
    let agent = test_runtime
        .sessions
        .create(
            &test_runtime.preparation,
            session.clone(),
            options,
            SessionStartReason::Startup,
            false,
        )
        .await
        .unwrap();
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let generation = agent.snapshot().await.runtime_generation_id;
    assert!(workflows.new_session(agent.instance().id()).await.is_err());
    assert_eq!(agent.snapshot().await.runtime_generation_id, generation);
    assert!(test_runtime.sessions.get(agent.instance().id()).is_ok());
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn new_session_keeps_accepted_switch_and_process_policy() {
    let (root, policy, _) = session_test_runtime();
    let mut configuration = policy.configuration.snapshot();
    configuration.cli = Arc::new(Cli::parse_from([
        "phi",
        "--model",
        "test",
        "--no-tools",
        "--max-tokens",
        "4096",
    ]));
    configuration.settings.default_thinking_level = Some("medium".into());
    configuration.settings.switches = Some(vec![crate::config::SwitchConfig {
        name: "review".into(),
        alias: Some("r".into()),
        provider: Some("alternate".into()),
        model: Some("next".into()),
        thinking_level: Some("high".into()),
        mode: Some("plan".into()),
        prompt: Some("accepted switch prompt".into()),
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
    std::fs::write(root.join("models.json"), serde_json::to_vec(&serde_json::json!({
        "providers": {
            "local": {"api":"openai-completions", "baseUrl":"http://localhost/v1", "models":{"test":{"contextWindow":128000}}},
            "alternate": {"api":"openai-completions", "baseUrl":"http://localhost/v1", "models":{"next":{"contextWindow":64000}}}
        }
    })).unwrap()).unwrap();
    let catalog = Arc::new(
        ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![root.join("models.json")],
            offline: true,
        })
        .unwrap(),
    );
    configuration.model_catalog = catalog.clone();
    let resources = RuntimeResources::new(
        phi_sdk::PhiRuntimeServices::new().model_catalog(catalog.clone()),
        Arc::new(phi_sdk::extension::ExtensionRuntime::load()),
        catalog,
        phi_sdk::sandbox::Sandbox::new(false, root.clone()).unwrap(),
    );
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
    let agent = Box::pin(workflows.start_session(session, true))
        .await
        .unwrap();
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    std::fs::write(root.join(".phi/settings.json"), serde_json::to_vec(&serde_json::json!({
        "defaultThinkingLevel":"medium",
        "switches":[{"name":"review", "provider":"local", "model":"test", "thinking_level":"low", "mode":"default", "prompt":"unaccepted switch prompt"}]
    })).unwrap()).unwrap();
    let selected = Box::pin(test_runtime.preparation.configure_runtime(
        &test_runtime.sessions.session_access(agent.instance().id()),
        RuntimeSelection::Switch { name: "r".into() },
    ))
    .await
    .unwrap();
    assert_eq!(selected.session.provider, "alternate");
    assert_eq!(selected.session.model, "next");
    assert_eq!(agent.runtime().runtime_config().no_tools, Some(true));
    assert_eq!(agent.runtime().runtime_config().max_tokens, Some(4096));
    assert_eq!(
        selected.declarations.capabilities.thinking_level.as_deref(),
        Some("high")
    );
    assert_eq!(
        agent.runtime().phi().agent_mode(),
        phi_sdk::config::AgentMode::Plan
    );
    let current = agent.session().await;
    let runtime = agent.runtime().clone();
    let prepared = runtime.prepare_replacement(&current).await.unwrap();
    assert_eq!(
        prepared
            .runtime
            .source_settings()
            .unwrap()
            .default_thinking_level
            .as_deref(),
        Some("medium")
    );
    agent
        .replace_runtime(prepared, current, None, false)
        .await
        .unwrap();
    std::fs::write(
        root.join(".phi/settings.json"),
        r#"{"defaultThinkingLevel":"low","switches":[]}"#,
    )
    .unwrap();
    let mut current_id = agent.instance().id().clone();
    for _ in 0..2 {
        let next = Box::pin(workflows.new_session(&current_id)).await.unwrap();
        assert_eq!(next.accepted.session.current_switch, "review");
        assert_eq!(next.accepted.session.provider, "alternate");
        assert_eq!(next.accepted.session.model, "next");
        assert_eq!(next.accepted.session.context_window, 64000);
        assert_eq!(
            next.accepted
                .declarations
                .capabilities
                .thinking_level
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            test_runtime
                .sessions
                .get(next.attachment.id())
                .unwrap()
                .runtime()
                .phi()
                .agent_mode(),
            phi_sdk::config::AgentMode::Plan
        );
        next.attachment.ready_hook_notices().await.unwrap();
        assert!(
            prompts
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .contains("accepted switch prompt")
        );
        current_id = next.attachment.id().clone();
    }
    assert_eq!(
        policy
            .configuration
            .snapshot()
            .settings
            .default_thinking_level
            .as_deref(),
        Some("medium")
    );
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}
