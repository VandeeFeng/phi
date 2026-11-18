use super::*;

#[tokio::test]
async fn associated_child_reload_preserves_scope_and_rejects_unconfigured_recovery() {
    let root = std::env::temp_dir().join(format!("phi-child-reload-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let models = root.join("models.json");
    std::fs::write(&models, br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}"#).unwrap();
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
    let cli = Cli::parse_from(["phi"]);
    let settings = Settings::default();
    let options = |session: &crate::session::Session| RuntimeRebuildOptions {
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
            hooks: Vec::new(),
        },
        instance: AgentInstanceBuildOptions::from_session(session),
    };
    let mut parent_session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    parent_session.working_dir = root.to_string_lossy().into();
    parent_session.task_list_id = Some("shared-review".into());
    parent_session
        .permission_allowlist
        .push(crate::session::PermissionAllowEntry {
            tool: "bash".into(),
            pattern: "printf parent-grant".into(),
        });
    let mut invalid_options = options(&parent_session);
    invalid_options.phi.provider = "";
    assert!(
        Box::pin(test_runtime.sessions.create(
            &test_runtime.preparation,
            parent_session.clone(),
            invalid_options,
            phi_sdk::lifecycle::SessionStartReason::New,
            true,
        ))
        .await
        .is_err()
    );
    assert!(test_runtime.sessions.list().await.unwrap().is_empty());
    assert!(!crate::session::storage::session_path(&parent_session).exists());
    let parent = test_runtime
        .sessions
        .create(
            &test_runtime.preparation,
            parent_session.clone(),
            options(&parent_session),
            phi_sdk::lifecycle::SessionStartReason::New,
            true,
        )
        .await
        .unwrap();
    let access = test_runtime
        .manager
        .for_agent(parent.instance().id())
        .unwrap();
    assert_eq!(
        parent.instance().task_list().unwrap().list_id(),
        "shared-review"
    );
    {
        let mut target =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        target.working_dir = root.to_string_lossy().into();
        let prepared = Box::pin(test_runtime.preparation.prepare(options(&target)))
            .await
            .unwrap();
        target.id = uuid::Uuid::new_v4().to_string().into();
        assert!(
            test_runtime
                .sessions
                .transition_prepared(
                    parent.instance().id(),
                    target.clone(),
                    prepared,
                    phi_sdk::lifecycle::SessionStartReason::Resume,
                    None,
                    SessionEndReason::Resume,
                    None,
                    true,
                    None,
                )
                .await
                .is_err()
        );
        assert!(
            test_runtime
                .sessions
                .open_existing(parent.instance().id())
                .unwrap()
                .is_some()
        );
        assert!(
            test_runtime
                .sessions
                .find(&AgentInstanceId::new(target.id.as_str()))
                .unwrap()
                .is_none()
        );
        assert!(!crate::session::storage::session_path(&target).exists());
    }
    {
        let mut child =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        child.working_dir = root.to_string_lossy().into();
        child
            .permission_allowlist
            .push(crate::session::PermissionAllowEntry {
                tool: "bash".into(),
                pattern: "printf child-grant".into(),
            });
        child.bind_test_parent(&parent.session().await);
        let prepared = Box::pin(test_runtime.preparation.prepare(options(&child)))
            .await
            .unwrap();
        let (candidate, runtime) = prepared.into_parts();
        let registered = access.register_child(candidate).unwrap();
        let instance = test_runtime.manager.get(&registered.instance.id).unwrap();
        let (_cancel, mut cancellation) = tokio::sync::watch::channel(false);
        let agent_session = test_runtime
            .sessions
            .associate_child_session(
                parent.instance().id(),
                child.clone(),
                runtime,
                instance,
                &mut cancellation,
            )
            .await
            .unwrap();
        assert_eq!(
            agent_session.instance().task_list(),
            parent.instance().task_list()
        );
        assert_eq!(
            agent_session.session().await.task_list_id.as_deref(),
            Some("shared-review")
        );
        assert_eq!(
            crate::session::storage::load_session_id(child.id.as_str())
                .unwrap()
                .task_list_id
                .as_deref(),
            Some("shared-review")
        );
        test_runtime
            .sessions
            .start_agent_session(
                &agent_session,
                phi_sdk::lifecycle::SessionStartReason::New,
                Some(parent_session.id.as_str()),
            )
            .await;
        agent_session
            .allow_session_tools(&[("bash".into(), "printf child-late-grant".into())])
            .await
            .unwrap();
        let child_grants = agent_session.session().await.permission_allowlist;
        assert_eq!(child_grants.len(), 2);
        assert!(
            !child_grants
                .iter()
                .any(|grant| grant.pattern == "printf parent-grant")
        );
        assert_eq!(
            parent.session().await.permission_allowlist,
            parent_session.permission_allowlist
        );
        let attached = test_runtime
            .sessions
            .session_access(agent_session.instance().id())
            .binding()
            .unwrap();
        assert_eq!(
            attached.instance().registration_id(),
            agent_session.instance().registration_id()
        );
        let snapshots = test_runtime.sessions.list().await.unwrap();
        assert_eq!(snapshots.len(), 2);
        assert!(
            snapshots
                .windows(2)
                .all(|pair| pair[0].instance.id.as_str() < pair[1].instance.id.as_str())
        );
        let changed_parent = AgentInstanceId::new(uuid::Uuid::new_v4().to_string());
        for kind in [
            phi_sdk::agents::AgentKind::Master,
            phi_sdk::agents::AgentKind::Subagent {
                creator: changed_parent,
            },
            child.agent_kind.clone(),
        ] {
            let mut replacement = agent_session.session().await;
            replacement.agent_kind = kind;
            let prepared = Box::pin(test_runtime.preparation.prepare(options(&replacement)))
                .await
                .unwrap();
            let result = Box::pin(
                test_runtime.preparation.reload(
                    &test_runtime
                        .sessions
                        .session_access(agent_session.instance().id()),
                    replacement.clone(),
                    prepared,
                    None,
                ),
            )
            .await;
            if replacement.agent_kind == child.agent_kind {
                result.unwrap();
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("delegation origin")
                );
            }
            assert_eq!(agent_session.session().await.agent_kind, child.agent_kind);
            assert_eq!(
                agent_session.session().await.permission_allowlist,
                child_grants
            );
            assert_eq!(attached.session().await.permission_allowlist, child_grants);
            assert_eq!(
                parent.session().await.permission_allowlist,
                parent_session.permission_allowlist
            );
        }
        let original = agent_session.session().await;
        let mut changed_definition = original.clone();
        changed_definition.definition = Some("another-definition".into());
        let mut changed_alias = original.clone();
        changed_alias.alias = Some(phi_sdk::agent_manager::AgentAlias::new("other").unwrap());
        let mut changed_storage = original.clone();
        changed_storage.sidechain = None;
        let mut changed_list = original.clone();
        changed_list.task_list_id = Some("another-list".into());
        for replacement in [
            changed_definition,
            changed_alias,
            changed_storage,
            changed_list,
        ] {
            let prepared = Box::pin(test_runtime.preparation.prepare(options(&replacement)))
                .await
                .unwrap();
            assert!(
                agent_session
                    .replace_runtime(prepared, replacement, None, false)
                    .await
                    .is_err()
            );
            let retained = agent_session.session().await;
            assert_eq!(retained.agent_kind, original.agent_kind);
            assert_eq!(retained.definition, original.definition);
            assert_eq!(retained.alias, original.alias);
            assert_eq!(retained.sidechain, original.sidechain);
            assert_eq!(retained.task_list_id, original.task_list_id);
        }
        assert!(
            agent_session
                .instance()
                .view()
                .await
                .unwrap()
                .transcript
                .entries
                .is_empty()
        );
        let saved = crate::session::storage::load_session_id(child.id.as_str()).unwrap();
        assert_eq!(saved.agent_kind, child.agent_kind);
        assert_eq!(saved.task_list_id.as_deref(), Some("shared-review"));
        test_runtime
            .sessions
            .stop(agent_session.instance().id(), SessionEndReason::Quit)
            .await
            .unwrap();
        let parent_status = parent.instance().snapshot().status;
        let error = test_runtime
            .sessions
            .create(
                &test_runtime.preparation,
                saved.clone(),
                options(&saved),
                phi_sdk::lifecycle::SessionStartReason::Resume,
                true,
            )
            .await
            .err()
            .expect("unconfigured child recovery");
        assert!(
            error
                .to_string()
                .contains("explicit validated execution configuration")
        );
        let prepared = Box::pin(test_runtime.preparation.prepare(options(&saved)))
            .await
            .unwrap();
        assert!(
            test_runtime
                .sessions
                .transition_prepared(
                    parent.instance().id(),
                    saved,
                    prepared,
                    phi_sdk::lifecycle::SessionStartReason::Resume,
                    None,
                    SessionEndReason::Resume,
                    None,
                    true,
                    None,
                )
                .await
                .is_err()
        );
        assert!(
            test_runtime
                .sessions
                .open_existing(parent.instance().id())
                .unwrap()
                .is_some()
        );
        assert_eq!(parent.instance().snapshot().status, parent_status);
        assert!(
            test_runtime
                .sessions
                .find(agent_session.instance().id())
                .unwrap()
                .is_none()
        );
    }
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn parent_checkpoints_child_link_and_canonical_history() {
    let root = std::env::temp_dir().join(format!("phi-child-link-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let sandbox = Sandbox::new(false, root.clone()).unwrap();
    let model_path = root.join("models.json");
    std::fs::write(
        &model_path,
        br#"{"providers":{"local":{"api":"openai-completions","baseUrl":"http://localhost/v1","models":{"test":{"contextWindow":128000}}}}}"#,
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
        sandbox,
    );
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    let cli = Cli::parse_from(["phi"]);
    let settings = Settings::default();
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
                    hooks: Vec::new(),
                },
                instance: AgentInstanceBuildOptions::from_session(&session),
            },
            phi_sdk::lifecycle::SessionStartReason::New,
            true,
        )
        .await
        .unwrap();
    test_runtime
        .sessions
        .session_access(agent_session.instance().id())
        .checkpoint()
        .await
        .unwrap();
    let initial_save = agent_session.save_observation().await;
    assert!(initial_save.persistent);
    let initial_done = initial_save.completed.unwrap();
    assert!(initial_done.error.is_none());
    assert!(initial_done.saved_boundary.is_none());
    let link = crate::session::ChildSessionLink {
        child_session_id: "child".into(),
        child_agent_id: "child".into(),
        child_working_dir: root.join("child").to_string_lossy().into(),
        definition: None,
        alias: None,
        parent_tool_call_id: None,
        workflow_run_id: None,
        mode: phi_sdk::agent_manager::DelegationMode::Background,
        recovered_outcome: None,
    };
    agent_session
        .record_child_intent(link.clone())
        .await
        .unwrap();
    assert!(
        agent_session
            .record_child_intent(link.clone())
            .await
            .is_err()
    );
    let path = crate::session::storage::session_path(&session);
    assert_eq!(
        crate::session::storage::load_session(path.to_str().unwrap())
            .unwrap()
            .children,
        vec![link]
    );
    test_runtime
        .sessions
        .session_access(agent_session.instance().id())
        .checkpoint()
        .await
        .unwrap();
    let before = agent_session.save_observation().await;
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    agent_session.request_checkpoint();
    let failure = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let observation = agent_session.save_observation().await;
            if observation.attempt > before.attempt
                && observation.pending.is_none()
                && observation
                    .completed
                    .as_ref()
                    .is_some_and(|done| done.error.is_some())
            {
                break observation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(failure.completed.as_ref().unwrap().attempts, 2);
    let late = test_runtime
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
    assert_eq!(late.save_observation().await.unwrap(), failure);
    std::fs::remove_dir(&path).unwrap();
    late.checkpoint().await.unwrap();
    let retried = late.save_observation().await.unwrap();
    assert_eq!(retried.association_id, before.association_id);
    assert!(retried.completed.as_ref().unwrap().error.is_none());
    assert_eq!(retried.completed.as_ref().unwrap().attempts, 1);
    assert_eq!(
        retried
            .completed
            .unwrap()
            .saved_boundary
            .unwrap()
            .entry_count,
        0
    );

    let child_dir = root.join("child");
    std::fs::create_dir(&child_dir).unwrap();
    let mut child =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    child.id = "child-session".into();
    child.working_dir = child_dir.to_string_lossy().into();
    child.bind_test_parent(&agent_session.session().await);
    let child_path_for_hook = crate::session::storage::session_path(&child);
    let backup_path = root.join("final-save-backup.jsonl");
    let backup_for_hook = backup_path.clone();
    let end_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let ended = end_count.clone();
    let end_hook = phi_sdk::hooks::Hook::session_end(move |_| {
        let child_path = child_path_for_hook.clone();
        let backup = backup_for_hook.clone();
        let ended = ended.clone();
        async move {
            if ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                std::fs::copy(&child_path, backup).unwrap();
                std::fs::remove_file(&child_path).unwrap();
                std::fs::create_dir(&child_path).unwrap();
            }
            Ok(Vec::new())
        }
    });
    let prepared = test_runtime
        .preparation
        .prepare(RuntimeRebuildOptions {
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
                hooks: vec![end_hook],
            },
            instance: AgentInstanceBuildOptions::from_session(&child),
        })
        .await
        .unwrap();
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
    let (candidate, runtime) = prepared.into_parts();
    let instance = test_runtime.manager.insert(candidate).unwrap();
    let child_session = test_runtime
        .sessions
        .associate_child_session(
            agent_session.instance().id(),
            child.clone(),
            runtime,
            instance,
            &mut cancellation,
        )
        .await
        .unwrap();
    test_runtime
        .sessions
        .start_agent_session(
            &child_session,
            phi_sdk::lifecycle::SessionStartReason::New,
            Some(session.id.as_str()),
        )
        .await;
    assert_eq!(child_session.session().await.agent_kind, child.agent_kind);
    assert_eq!(agent_session.session().await.children.len(), 2);
    let child_path = crate::session::storage::session_path(&child_session.session().await);
    let saved_child = crate::session::storage::load_session(child_path.to_str().unwrap()).unwrap();
    assert_eq!(saved_child.agent_kind, child.agent_kind);
    assert!(saved_child.transcript.entries.is_empty());
    let empty_saved = child_session.checkpoint_history().await.unwrap().unwrap();
    assert_eq!(empty_saved.agent_kind, child.agent_kind);
    assert!(empty_saved.transcript.entries.is_empty());
    child_session
        .instance()
        .submit(phi_sdk::turn::TurnPrompt {
            tool_policy: Default::default(),
            prompt: "final child input".into(),
            source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
            injected_context: Vec::new(),
            images: Vec::new(),
        })
        .await
        .unwrap();
    let final_transcript = child_session.instance().stop().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while child_session.snapshot().await.persistence_error.is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .is_err()
    );
    let failed_final = child_session.save_observation().await;
    let done = failed_final.finalization.as_ref().unwrap();
    assert_eq!(done.operation, super::save::SaveOperation::Finalization);
    assert_eq!(done.attempts, 1);
    assert!(done.error.is_some());
    assert!(
        test_runtime
            .sessions
            .find(agent_session.instance().id())
            .unwrap()
            .is_none()
    );
    assert!(
        test_runtime
            .sessions
            .find(child_session.instance().id())
            .unwrap()
            .is_some()
    );
    assert!(crate::session::storage::lock_session_path(&child_path).is_err());
    assert!(
        test_runtime
            .sessions
            .open_existing(child_session.instance().id())
            .is_err()
    );
    std::fs::remove_dir(&child_path).unwrap();
    std::fs::copy(&backup_path, &child_path).unwrap();
    let child_id = child_session.instance().id().clone();
    test_runtime
        .sessions
        .session_access(&child_id)
        .checkpoint()
        .await
        .unwrap();
    let own_retry = child_session.save_observation().await;
    assert!(own_retry.completed.as_ref().unwrap().error.is_none());
    assert!(own_retry.finalization.as_ref().unwrap().error.is_some());
    assert!(test_runtime.sessions.find(&child_id).unwrap().is_some());
    let (stopped, reconciled) = tokio::join!(
        test_runtime
            .sessions
            .stop(&child_id, SessionEndReason::Quit),
        test_runtime.sessions.finalize_agent_session(&child_session),
    );
    let stopped = stopped.unwrap();
    reconciled.unwrap();
    assert_eq!(end_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    let finalized = child_session.save_observation().await;
    assert_eq!(finalized.association_id, failed_final.association_id);
    let done = finalized.finalization.unwrap();
    assert_eq!(done.operation, super::save::SaveOperation::Finalization);
    assert!(done.error.is_none());
    assert_eq!(
        done.saved_boundary.unwrap().leaf_id,
        final_transcript.leaf_id.as_ref().map(ToString::to_string)
    );
    assert_eq!(stopped.transcript.leaf_id, final_transcript.leaf_id);
    assert!(
        test_runtime
            .sessions
            .find(child_session.instance().id())
            .unwrap()
            .is_none()
    );
    let stopped = crate::session::storage::load_session(child_path.to_str().unwrap()).unwrap();
    let released_lock = crate::session::storage::lock_session_path(&child_path).unwrap();
    drop(released_lock);
    assert_eq!(
        serde_json::to_value(&stopped.transcript).unwrap(),
        serde_json::to_value(&final_transcript).unwrap()
    );
    assert!(test_runtime.sessions.list().await.unwrap().is_empty());
    let test_runtime = Arc::new(TestRuntime::new(
        test_runtime
            .preparation
            .runtime_factory()
            .unwrap()
            .as_ref()
            .clone(),
    ));
    let saved_master = crate::session::storage::load_session_id(session.id.as_str()).unwrap();
    let reopened = test_runtime
        .sessions
        .create(
            &test_runtime.preparation,
            saved_master.clone(),
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
                    hooks: Vec::new(),
                },
                instance: AgentInstanceBuildOptions::from_session(&saved_master),
            },
            phi_sdk::lifecycle::SessionStartReason::Resume,
            true,
        )
        .await
        .unwrap();
    assert_ne!(
        reopened.instance().registration_id(),
        agent_session.instance().registration_id()
    );
    assert!(
        test_runtime
            .sessions
            .finalize_agent_session(&agent_session)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        child_session
            .allow_session_tools(&[("stale".into(), "stale".into())])
            .await
            .is_err()
    );
    assert_eq!(
        reopened.instance().snapshot().status,
        AgentInstanceStatus::Idle
    );
    assert!(crate::session::storage::lock_session_path(&path).is_err());
    let reopened_id = reopened.instance().id().clone();
    let mut stop = Box::pin(
        test_runtime
            .sessions
            .stop(&reopened_id, SessionEndReason::Quit),
    );
    std::future::poll_fn(|context| {
        assert!(std::future::Future::poll(stop.as_mut(), context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    drop(stop);
    tokio::time::timeout(Duration::from_secs(5), async {
        while test_runtime
            .sessions
            .find(reopened.instance().id())
            .unwrap()
            .is_some()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("abandoned stop left a Session association");
    assert_eq!(end_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    for abandon_creation in [false, true] {
        let test_runtime = Arc::new(TestRuntime::new(
            test_runtime
                .preparation
                .runtime_factory()
                .unwrap()
                .as_ref()
                .clone(),
        ));
        let starting = Arc::new(tokio::sync::Notify::new());
        let start_signal = starting.clone();
        let start_hook = phi_sdk::hooks::Hook::session_start(move |_| {
            let signal = start_signal.clone();
            async move {
                signal.notify_one();
                std::future::pending::<phi_sdk::hooks::HookResult<Vec<phi_sdk::hooks::HookNotice>>>(
                )
                .await
            }
        });
        let mut pending_session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        pending_session.working_dir = root.to_string_lossy().into();
        let pending_path = crate::session::storage::session_path(&pending_session);
        let starting_runtime = test_runtime.clone();
        let starting_config = settings.runtime_config(&catalog);
        let cli = Cli::parse_from(["phi"]);
        let creation = tokio::spawn(async move {
            starting_runtime
                .sessions
                .create(
                    &starting_runtime.preparation,
                    pending_session.clone(),
                    RuntimeRebuildOptions {
                        phi: PhiBuildOptions {
                            cli: &cli,
                            provider: "local",
                            model: "test",
                            runtime_cfg: starting_config,
                            context: phi_sdk::context::load(true),
                            agent_options: crate::AgentBuildOptions {
                                no_tools: true,
                                ..Default::default()
                            },
                            hooks: vec![start_hook],
                        },
                        instance: AgentInstanceBuildOptions::from_session(&pending_session),
                    },
                    phi_sdk::lifecycle::SessionStartReason::New,
                    true,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), starting.notified())
            .await
            .unwrap();
        if abandon_creation {
            creation.abort();
        }
        let mut shutdown = Box::pin(test_runtime.sessions.shutdown(SessionEndReason::Quit));
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(shutdown.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(shutdown);
        let created = tokio::time::timeout(Duration::from_secs(5), creation)
            .await
            .expect("abandoned shutdown blocked SessionStart");
        if abandon_creation {
            assert!(created.err().unwrap().is_cancelled());
        } else {
            assert!(created.unwrap().is_err());
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !test_runtime.sessions.list().await.unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("abandoned shutdown left a Session association");
        assert!(test_runtime.sessions.list().await.unwrap().is_empty());
        let lock = crate::session::storage::lock_session_path(&pending_path).unwrap();
        drop(lock);
    }
    std::fs::remove_dir_all(root).unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(child_path).unwrap();
}
