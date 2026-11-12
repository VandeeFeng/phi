use super::*;

async fn wait_for_child_outcome(
    agent_session: &AgentSession,
    instance_id: &str,
) -> crate::session::SubagentChildLink {
    let id: phi_sdk::agent_instance::AgentInstanceId =
        serde_json::from_value(instance_id.into()).unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let finished = agent_session
                .instance()
                .subagent_status(id.clone())
                .await
                .unwrap()
                .is_some_and(|status| status.outcome.is_some());
            if finished
                && matches!(
                    agent_session.instance().snapshot().status,
                    AgentInstanceStatus::Idle | AgentInstanceStatus::Failed
                )
                && let Some(link) =
                    agent_session
                        .subagents()
                        .await
                        .unwrap()
                        .into_iter()
                        .find(|link| {
                            link.child_agent_id == instance_id
                                && link.outcome != crate::session::SubagentOutcome::Running
                        })
            {
                return link;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "child {instance_id} did not finish: {:?}",
            agent_session.instance().snapshot()
        )
    })
}

#[tokio::test]
async fn directory_delegation_persists_child_and_parent_failure() {
    let root = std::env::temp_dir().join(format!("phi-delegation-{}", uuid::Uuid::new_v4()));
    let agent_dir = root.join(".phi/agents");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(
        agent_dir.join("reviewer.md"),
        "---\nname: reviewer\ndescription: Review\n---\nReview code.",
    )
    .unwrap();
    let memory_dir = root.join("memory");
    let memory_remote = root.join("memory-remote");
    for args in [
        vec!["init", "--bare", "memory-remote"],
        vec!["init", "memory"],
        vec![
            "-C",
            "memory",
            "remote",
            "add",
            "origin",
            memory_remote.to_str().unwrap(),
        ],
    ] {
        assert!(
            std::process::Command::new("git")
                .current_dir(&root)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let memory = phi_sdk::extension::memory::MemoryConfig {
        memory_dir: Some(phi_sdk::extension::memory::MemoryDirSettings {
            local_path: Some(memory_dir.to_string_lossy().into()),
            repo_url: Some(memory_remote.to_string_lossy().into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut child_memory = memory.clone();
    child_memory.hooks = Some(phi_sdk::extension::memory::HookConfig {
        user_prompt_submit: vec![phi_sdk::extension::memory::HookAction::Pull],
        session_end: vec![phi_sdk::extension::memory::HookAction::Pull],
        ..Default::default()
    });
    std::fs::write(
        root.join(".phi/settings.json"),
        serde_json::json!({
            "defaultProvider": "local", "defaultModel": "test",
            "switches": [{"name": "child-default", "provider": "local", "model": "test"}],
            "memory": child_memory,
        })
        .to_string(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let observed = entered.clone();
    let resume = release.clone();
    let successor_entered = Arc::new(tokio::sync::Notify::new());
    let successor_release = Arc::new(tokio::sync::Notify::new());
    let successor_observed = successor_entered.clone();
    let successor_resume = successor_release.clone();
    let provider = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut connection, _) = listener.accept().await.unwrap();
        assert_ne!(connection.read(&mut [0; 4096]).await.unwrap(), 0);
        observed.notify_one();
        resume.notified().await;
        drop(connection);
        loop {
            let (mut connection, _) = listener.accept().await.unwrap();
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
                        break;
                    }
                }
            }
            if String::from_utf8_lossy(&bytes).contains("ordinary successor") {
                successor_observed.notify_one();
                successor_resume.notified().await;
                break;
            }
        }
    });
    let model_path = root.join("models.json");
    std::fs::write(
        &model_path,
        serde_json::json!({"providers":{"local":{"api":"openai-completions","baseUrl":format!("http://{address}/v1"),"models":{"test":{"contextWindow":128000}}}}}).to_string(),
    )
    .unwrap();
    let worktree = root.with_extension("child");
    std::fs::create_dir_all(worktree.join(".phi/agents")).unwrap();
    std::fs::copy(
        agent_dir.join("reviewer.md"),
        worktree.join(".phi/agents/reviewer.md"),
    )
    .unwrap();
    std::fs::copy(
        root.join(".phi/settings.json"),
        worktree.join(".phi/settings.json"),
    )
    .unwrap();
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
    settings.memory = Some(memory);
    let services = PhiRuntimeServices::new()
        .model_catalog(catalog.clone())
        .permission_policy(
            phi_sdk::permission::PermissionConfig {
                mode: phi_sdk::permission::PermissionMode::Allow,
                ..Default::default()
            },
            None,
        );
    let resources = RuntimeResources::new_for(
        services,
        Arc::new(ExtensionRuntime::load()),
        catalog.clone(),
        Sandbox::new(false, root.clone()).unwrap(),
        &root,
    );
    let test_runtime = Arc::new(TestRuntime::new(AgentRuntimeFactory::new(resources)));
    test_runtime
        .preparation
        .enable_subagents(&test_runtime.sessions);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.current_switch = "child-default".into();
    session.working_dir = root.to_string_lossy().into();
    let mut instance_options = AgentInstanceBuildOptions::from_session(&session);
    instance_options.settings_manager = Some(settings_manager.clone());
    instance_options.resource_settings = Some(Arc::new(settings.clone()));
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
                        vec![agent_dir],
                        &root,
                    ),
                    agent_options: crate::AgentBuildOptions::default(),
                    hooks: Vec::new(),
                },
                instance: instance_options,
            },
            phi_sdk::lifecycle::SessionStartReason::Startup,
            true,
        )
        .await
        .unwrap();
    agent_session.ready_hook_notices().await;
    let fetch_head = memory_dir.join(".git/FETCH_HEAD");
    assert!(fetch_head.exists());
    std::fs::remove_file(&fetch_head).unwrap();
    let team = test_runtime
        .manager
        .initialize_team(
            agent_session.instance().id(),
            phi_sdk::agent_manager::TeamStorageRoot::new(root.join("teams")).unwrap(),
        )
        .await
        .unwrap();
    let started = agent_session
        .instance()
        .spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest {
            alias: Some(phi_sdk::agent_manager::AgentAlias::new("reviewer").unwrap()),
            ..phi_sdk::agent_manager::AgentSpawnRequest::new("reviewer", "inspect")
                .working_dir(&worktree)
        })
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    assert!(
        !fetch_head.exists(),
        "child repeated automatic Memory synchronization"
    );
    let child_instance = test_runtime
        .manager
        .get(&AgentInstanceId::new(started.instance_id().as_str()))
        .unwrap();
    let member = test_runtime
        .manager
        .for_agent(child_instance.id())
        .unwrap()
        .list()
        .unwrap()
        .into_iter()
        .find(|member| member.instance.id == *child_instance.id())
        .unwrap();
    assert_eq!(member.team, Some(team));
    assert_ne!(child_instance.id(), agent_session.instance().id());
    assert!(matches!(
        test_runtime
            .manager
            .for_agent(child_instance.id())
            .unwrap()
            .interrupt(agent_session.instance().id())
            .await,
        Err(phi_sdk::agent_manager::AgentManagerError::Denied { .. })
    ));
    assert!(
        agent_session
            .instance()
            .subagent_status(started.instance_id().clone())
            .await
            .unwrap()
            .is_some()
    );
    {
        let child_attachment = test_runtime
            .sessions
            .get(child_instance.id())
            .map(|agent| {
                crate::runtime::attachment::AgentAttachment::new(
                    &agent,
                    test_runtime.sessions.session_access(agent.instance().id()),
                    test_runtime.preparation.clone(),
                )
            })
            .unwrap();
        let mut child_events = child_attachment.subscribe();
        let recovery = child_attachment
            .recover_events(&mut child_events)
            .await
            .unwrap();
        assert_eq!(
            recovery.session.id.as_str(),
            recovery.snapshot.instance.id.as_str()
        );
        assert_eq!(
            recovery.snapshot.instance.status,
            AgentInstanceStatus::Running
        );
        assert!(recovery.snapshot.instance.active_turn_id.is_some());
        assert_eq!(
            recovery.session.transcript.leaf_id,
            child_instance.view().await.unwrap().transcript.leaf_id
        );
        let view = test_runtime
            .sessions
            .session_access(agent_session.instance().id())
            .subagent_view(started.instance_id().as_str())
            .await
            .unwrap();
        assert_eq!(
            view.session.transcript.leaf_id,
            recovery.session.transcript.leaf_id
        );
        assert_eq!(view.live_run, recovery.snapshot.instance.live_run);
        let local = test_runtime
            .sessions
            .get(child_instance.id())
            .map(|agent| {
                crate::runtime::attachment::AgentAttachment::new(
                    &agent,
                    test_runtime.sessions.session_access(agent.instance().id()),
                    test_runtime.preparation.clone(),
                )
            })
            .unwrap();
        let mut local_events = local.subscribe();
        let local_recovery = local.recover_events(&mut local_events).await.unwrap();
        child_instance
            .view_with_event_boundary("next-boundary".into())
            .await
            .unwrap();
        for events in [&mut local_events, &mut child_events] {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if matches!(events.recv().await.unwrap(),
                        crate::runtime::attachment::AgentEvent::RecoveryBoundary { id } if id == "next-boundary")
                    {
                        break;
                    }
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(
            local_recovery.session.transcript.leaf_id,
            recovery.session.transcript.leaf_id
        );
        assert_eq!(
            local_recovery.snapshot.instance.active_turn_id,
            recovery.snapshot.instance.active_turn_id
        );
        let phi = agent_session.runtime().phi().clone();
        let unassociated = test_runtime
            .manager
            .insert(
                phi.with_runtime_services(PhiRuntimeServices::new().model_catalog(catalog.clone()))
                    .agent_instance_builder()
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        assert!(
            test_runtime
                .sessions
                .get(unassociated.id())
                .map(|agent| {
                    crate::runtime::attachment::AgentAttachment::new(
                        &agent,
                        test_runtime.sessions.session_access(agent.instance().id()),
                        test_runtime.preparation.clone(),
                    )
                })
                .is_err()
        );
        test_runtime.manager.stop(unassociated.id()).await.unwrap();
    }
    assert_eq!(
        child_instance
            .transcript()
            .await
            .unwrap()
            .entries
            .iter()
            .filter(|entry| matches!(
                &entry.kind,
                crate::session::TranscriptEntryKind::Message {
                    message: phi_sdk::transcript::AgentMessage::User { .. }
                }
            ))
            .count(),
        1
    );
    assert!(
        agent_session
            .instance()
            .subagent_snapshots()
            .await
            .unwrap()
            .iter()
            .all(|snapshot| snapshot.status.outcome.is_none())
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = agent_session
                .instance()
                .subagent_status(started.instance_id().clone())
                .await
                .unwrap()
                .unwrap();
            if status.outcome.is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    test_runtime
        .manager
        .for_agent(agent_session.instance().id())
        .unwrap()
        .send_message(child_instance.id(), "ordinary successor")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), successor_entered.notified())
        .await
        .unwrap();
    assert!(
        !fetch_head.exists(),
        "follow-up repeated automatic Memory synchronization"
    );
    let successor_history = child_instance.view().await.unwrap();
    assert_eq!(
        successor_history.snapshot.status,
        AgentInstanceStatus::Running
    );
    let completion = tokio::time::timeout(
        Duration::from_secs(1),
        test_runtime
            .sessions
            .session_access(agent_session.instance().id())
            .child_completion(started.instance_id()),
    )
    .await
    .expect("completion observation followed the executor's successor")
    .unwrap();
    assert!(completion.outcome.is_err());
    let assigner_id = agent_session.instance().id().clone();
    let local_attachment = test_runtime
        .sessions
        .get(&assigner_id)
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    let assigner_access = test_runtime.sessions.session_access(&assigner_id);
    let (local_save, concurrent_save) =
        tokio::join!(local_attachment.checkpoint(), assigner_access.checkpoint());
    local_save.unwrap();
    concurrent_save.unwrap();
    test_runtime
        .sessions
        .session_access(child_instance.id())
        .checkpoint()
        .await
        .unwrap();
    let executor_saved =
        crate::session::storage::load_session_id(started.instance_id().as_str()).unwrap();
    assert_eq!(
        executor_saved.transcript.leaf_id,
        successor_history.transcript.leaf_id
    );
    assert_eq!(
        serde_json::to_value(&executor_saved.transcript.entries).unwrap(),
        serde_json::to_value(&successor_history.transcript.entries).unwrap()
    );
    assert_eq!(
        child_instance.snapshot().status,
        AgentInstanceStatus::Running
    );
    successor_release.notify_one();
    provider.await.unwrap();
    let link = wait_for_child_outcome(&agent_session, started.instance_id().as_str()).await;
    child_instance.wait_for_quiescence(None).await.unwrap();
    assert_eq!(
        link.admission,
        phi_sdk::agents::SubagentAdmission::Accepted,
        "{:?}",
        link.outcome
    );
    assert_eq!(link.child_working_dir, worktree.to_string_lossy());
    assert_eq!(
        agent_session
            .instance()
            .subagent_status(started.instance_id().clone())
            .await
            .unwrap()
            .unwrap()
            .admission,
        phi_sdk::agents::SubagentAdmission::Accepted
    );
    assert!(
        agent_session
            .instance()
            .subagent_status(started.instance_id().clone())
            .await
            .unwrap()
            .unwrap()
            .outcome
            .as_ref()
            .unwrap()
            .is_err()
    );
    assert!(matches!(
        link.outcome,
        crate::session::SubagentOutcome::Finished { is_error: true, .. }
    ));
    let child = crate::session::storage::load_session_id(started.instance_id().as_str()).unwrap();
    assert_eq!(
        child.creator_id().unwrap().as_str(),
        started.creator_id().as_str()
    );
    assert!(child.permission_allowlist.is_empty());
    assert_eq!(
        child
            .entries
            .iter()
            .filter(|entry| matches!(
                &entry.kind,
                crate::session::TranscriptEntryKind::Message {
                    message: phi_sdk::transcript::AgentMessage::User { .. }
                }
            ))
            .count(),
        1
    );
    assert_eq!(child.provider.as_str(), "local");
    let view = test_runtime
        .sessions
        .session_access(agent_session.instance().id())
        .subagent_view(started.instance_id().as_str())
        .await
        .unwrap();
    assert_eq!(view.session.id, child.id);
    let retained = test_runtime.sessions.get(child_instance.id()).unwrap();
    assert!(!matches!(
        retained.instance().snapshot().status,
        AgentInstanceStatus::Stopping | AgentInstanceStatus::Stopped
    ));
    assert_eq!(retained.session().await.agent_kind, child.agent_kind);
    assert_eq!(
        crate::session::storage::session_path(&retained.session().await),
        crate::session::storage::session_path(&child)
    );
    let attached = test_runtime
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
    let mut receipt = test_runtime
        .manager
        .send_message(
            agent_session.instance().id(),
            child_instance.id(),
            "ordinary local question",
        )
        .await
        .unwrap();
    let message = receipt.wait(Duration::from_secs(1)).await;
    assert!(!message.timed_out);
    assert!(matches!(
        message.delivery,
        phi_sdk::agent_manager::AgentMessageDelivery::Committed { .. }
    ));
    assert_eq!(message.message.sender, *agent_session.instance().id());
    assert!(
        serde_json::to_value(&message.message)
            .unwrap()
            .get("instance_id")
            .is_none()
    );
    let child_path = crate::session::storage::session_path(&child);
    let backup_path = root.join("child-checkpoint-backup.jsonl");
    std::fs::copy(&child_path, &backup_path).unwrap();
    std::fs::remove_file(&child_path).unwrap();
    std::fs::create_dir(&child_path).unwrap();
    test_runtime
        .manager
        .for_agent(agent_session.instance().id())
        .unwrap()
        .send_message(child_instance.id(), "inspect again")
        .await
        .unwrap();
    child_instance.wait_for_quiescence(None).await.unwrap();
    assert!(
        test_runtime
            .sessions
            .session_access(retained.instance().id())
            .checkpoint()
            .await
            .is_err()
    );
    assert!(retained.snapshot().await.persistence_error.is_some());
    let observed = retained.recovery().await.unwrap();
    let view = test_runtime
        .sessions
        .session_access(agent_session.instance().id())
        .subagent_view(started.instance_id().as_str())
        .await
        .unwrap();
    assert_eq!(
        view.session.transcript.leaf_id,
        observed.session.transcript.leaf_id
    );
    assert!(child_path.is_dir());
    attached.checkpoint().await.unwrap();
    assert!(agent_session.snapshot().await.persistence_error.is_none());
    assert!(!matches!(
        retained.instance().snapshot().status,
        AgentInstanceStatus::Stopping | AgentInstanceStatus::Stopped
    ));
    std::fs::remove_dir(&child_path).unwrap();
    std::fs::copy(&backup_path, &child_path).unwrap();
    std::fs::remove_file(&backup_path).unwrap();
    let assigner_path = crate::session::storage::session_path(&agent_session.session().await);
    let assigner_backup = root.join("assigner-checkpoint-backup.jsonl");
    std::fs::copy(&assigner_path, &assigner_backup).unwrap();
    std::fs::remove_file(&assigner_path).unwrap();
    std::fs::create_dir(&assigner_path).unwrap();
    test_runtime
        .sessions
        .session_access(retained.instance().id())
        .checkpoint()
        .await
        .unwrap();
    assert!(attached.checkpoint().await.is_err());
    assert!(agent_session.snapshot().await.persistence_error.is_some());
    let own_failure = attached.save_observation().await.unwrap();
    assert!(own_failure.completed.as_ref().unwrap().error.is_some());
    let late_observer = test_runtime
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
    let observed_failure = late_observer.save_observation().await.unwrap();
    assert_eq!(observed_failure.association_id, own_failure.association_id);
    assert!(observed_failure.completed.as_ref().unwrap().error.is_some());
    assert_eq!(
        crate::session::storage::load_session(child_path.to_str().unwrap())
            .unwrap()
            .leaf_id,
        observed.session.transcript.leaf_id
    );
    std::fs::remove_dir(&assigner_path).unwrap();
    std::fs::copy(&assigner_backup, &assigner_path).unwrap();
    std::fs::remove_file(&assigner_backup).unwrap();
    test_runtime
        .sessions
        .session_access(agent_session.instance().id())
        .checkpoint()
        .await
        .unwrap();
    assert!(agent_session.snapshot().await.persistence_error.is_none());
    assert!(retained.snapshot().await.persistence_error.is_none());
    let observed_retry = attached.save_observation().await.unwrap();
    assert_eq!(observed_retry.association_id, own_failure.association_id);
    assert!(observed_retry.completed.as_ref().unwrap().error.is_none());
    assert!(
        observed_retry
            .completed
            .as_ref()
            .unwrap()
            .saved_boundary
            .is_some()
    );
    {
        let late_retry = test_runtime
            .sessions
            .get(agent_session.instance().id())
            .map(|agent| {
                crate::runtime::attachment::AgentAttachment::new(
                    &agent,
                    test_runtime.sessions.session_access(agent.instance().id()),
                    test_runtime.preparation.clone(),
                )
            })
            .unwrap()
            .save_observation()
            .await
            .unwrap();
        assert_eq!(late_retry.association_id, observed_retry.association_id);
        assert!(late_retry.completed.as_ref().unwrap().error.is_none());
        assert_eq!(
            late_retry.completed.as_ref().unwrap().saved_boundary,
            observed_retry.completed.as_ref().unwrap().saved_boundary
        );
    }
    assert!(test_runtime.manager.get(child_instance.id()).is_ok());
    assert_eq!(retained.session().await.agent_kind, child.agent_kind);
    let persisted_child = crate::session::storage::load_session_id(child.id.as_str()).unwrap();
    assert_eq!(persisted_child.agent_kind, child.agent_kind);
    assert_eq!(persisted_child.created_at, child.created_at);
    let followup_view = test_runtime
        .sessions
        .session_access(agent_session.instance().id())
        .subagent_view(started.instance_id().as_str())
        .await
        .unwrap();
    assert_eq!(followup_view.session.id, child.id);
    assert_eq!(followup_view.session.agent_kind, child.agent_kind);
    assert!(followup_view.compact_enabled.is_some());
    assert!(
        serde_json::to_string(&followup_view.session.transcript)
            .unwrap()
            .contains("inspect again")
    );
    assert!(
        test_runtime
            .sessions
            .session_access(agent_session.instance().id())
            .subagent_view("unknown-instance")
            .await
            .is_err()
    );
    assert!(
        test_runtime
            .sessions
            .session_access(retained.instance().id())
            .subagent_view(started.instance_id().as_str())
            .await
            .is_err()
    );
    {
        let observed_view = attached
            .subagent_view(started.instance_id().as_str())
            .await
            .unwrap();
        assert_eq!(observed_view.compact_enabled, followup_view.compact_enabled);
        assert_eq!(
            observed_view.session.transcript.leaf_id,
            followup_view.session.transcript.leaf_id
        );
    }
    assert_eq!(
        retained
            .instance()
            .transcript()
            .await
            .unwrap()
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.kind,
                    crate::session::TranscriptEntryKind::Message {
                        message: phi_sdk::transcript::AgentMessage::User { .. }
                    }
                )
            })
            .count(),
        1
    );
    assert_eq!(
        retained
            .instance()
            .transcript()
            .await
            .unwrap()
            .entries
            .iter()
            .filter(|entry| {
                matches!(
                    &entry.kind,
                    crate::session::TranscriptEntryKind::Message {
                        message: phi_sdk::transcript::AgentMessage::Agent { .. }
                    }
                )
            })
            .count(),
        3
    );
    Box::pin(crate::interactive::verify_subagent_view(
        &attached,
        started.instance_id().as_str(),
        false,
    ))
    .await;
    drop(attached);

    let parent_path = crate::session::storage::session_path(&session);
    let child_path = crate::session::storage::session_path(&child);
    assert!(child.sidechain.is_none());
    assert_eq!(child.agent_kind, *started.kind());
    assert_eq!(
        child.alias.as_ref().map(|alias| alias.as_str()),
        Some("reviewer")
    );
    assert_eq!(
        child_path.parent().unwrap(),
        phi_sdk::workspace::session_scope_dir(&child.working_dir)
    );
    std::fs::copy(
        root.join(".phi/agents/reviewer.md"),
        worktree.join(".phi/agents/reviewer.md"),
    )
    .unwrap();
    let leader_task = agent_session
        .instance()
        .spawn_subagent(phi_sdk::agent_manager::AgentSpawnRequest {
            ..phi_sdk::agent_manager::AgentSpawnRequest::new(
                "reviewer",
                "organize nested inspection",
            )
            .working_dir(&worktree)
        })
        .await
        .unwrap();
    let leader_link =
        wait_for_child_outcome(&agent_session, leader_task.instance_id().as_str()).await;
    assert_eq!(
        leader_link.admission,
        phi_sdk::agents::SubagentAdmission::Accepted,
        "{:?}",
        leader_link.outcome
    );
    let leader = test_runtime
        .sessions
        .get(&AgentInstanceId::new(leader_task.instance_id().as_str()))
        .unwrap();
    let leader_path = crate::session::storage::session_path(&leader.session().await);
    assert_eq!(
        leader_path,
        parent_path
            .parent()
            .unwrap()
            .join(session.id.as_str())
            .join("subagents")
            .join(format!("agent-{}.jsonl", leader.instance().id().as_str()))
    );
    let nested = leader
        .instance()
        .spawn_subagent(
            phi_sdk::agent_manager::AgentSpawnRequest::new("reviewer", "nested inspect")
                .working_dir(&worktree),
        )
        .await
        .unwrap();
    let nested_link = wait_for_child_outcome(&leader, nested.instance_id().as_str()).await;
    assert_eq!(
        nested_link.admission,
        phi_sdk::agents::SubagentAdmission::Accepted,
        "{:?}",
        nested_link.outcome
    );
    let grandchild = test_runtime
        .sessions
        .get(&AgentInstanceId::new(nested.instance_id().as_str()))
        .unwrap();
    let grandchild_path = crate::session::storage::session_path(&grandchild.session().await);
    assert_eq!(
        grandchild_path,
        leader_path
            .parent()
            .unwrap()
            .join(leader.instance().id().as_str())
            .join("subagents")
            .join(format!(
                "agent-{}.jsonl",
                grandchild.instance().id().as_str()
            ))
    );
    let leader_id = leader.instance().id().clone();
    let assigner_access = test_runtime.sessions.session_access(&assigner_id);
    let leader_access = test_runtime.sessions.session_access(&leader_id);
    let (parent_save, leader_save) =
        tokio::join!(assigner_access.checkpoint(), leader_access.checkpoint());
    parent_save.unwrap();
    leader_save.unwrap();
    assert_eq!(test_runtime.sessions.list().await.unwrap().len(), 4);
    let stopped_attachment = test_runtime
        .sessions
        .get(child_instance.id())
        .map(|agent| {
            crate::runtime::attachment::AgentAttachment::new(
                &agent,
                test_runtime.sessions.session_access(agent.instance().id()),
                test_runtime.preparation.clone(),
            )
        })
        .unwrap();
    test_runtime
        .sessions
        .stop(child_instance.id(), SessionEndReason::Quit)
        .await
        .unwrap();
    assert!(
        test_runtime
            .sessions
            .find(child_instance.id())
            .unwrap()
            .is_none()
    );
    test_runtime
        .sessions
        .session_access(&assigner_id)
        .checkpoint()
        .await
        .unwrap();
    test_runtime
        .sessions
        .session_access(&leader_id)
        .checkpoint()
        .await
        .unwrap();
    let historical = crate::session::storage::load_session_id(child.id.as_str()).unwrap();
    let historical_view = test_runtime
        .sessions
        .session_access(&assigner_id)
        .subagent_view(started.instance_id().as_str())
        .await
        .unwrap();
    assert_eq!(historical_view.session.id, child.id);
    assert!(historical_view.live_run.is_none());
    let historical_lock = crate::session::storage::lock_session_path(&child_path).unwrap();
    test_runtime
        .sessions
        .session_access(&assigner_id)
        .checkpoint()
        .await
        .unwrap();
    drop(historical_lock);
    std::fs::remove_file(&child_path).unwrap();
    test_runtime
        .sessions
        .session_access(&assigner_id)
        .checkpoint()
        .await
        .unwrap();
    crate::session::storage::save_session(&historical).unwrap();
    test_runtime
        .manager
        .for_agent(&leader_id)
        .unwrap()
        .send_message(grandchild.instance().id(), "final review before shutdown")
        .await
        .unwrap();
    grandchild
        .instance()
        .wait_for_quiescence(None)
        .await
        .unwrap();
    let final_history = grandchild.instance().view().await.unwrap().transcript;
    let final_backup = root.join("final-checkpoint-backup.jsonl");
    std::fs::copy(&grandchild_path, &final_backup).unwrap();
    std::fs::remove_file(&grandchild_path).unwrap();
    std::fs::create_dir(&grandchild_path).unwrap();
    assert!(
        test_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .is_err()
    );
    assert!(
        test_runtime
            .sessions
            .find(grandchild.instance().id())
            .unwrap()
            .is_some()
    );
    assert!(crate::session::storage::lock_session_path(&grandchild_path).is_err());
    std::fs::remove_dir(&grandchild_path).unwrap();
    std::fs::copy(&final_backup, &grandchild_path).unwrap();
    test_runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(
            crate::session::storage::load_session_id(grandchild.instance().id().as_str())
                .unwrap()
                .transcript
                .entries
        )
        .unwrap(),
        serde_json::to_value(final_history.entries).unwrap(),
    );
    {
        assert!(stopped_attachment.recovery().await.is_err());
        let recovery = child_instance.view().await.unwrap();
        assert_eq!(recovery.snapshot.status, AgentInstanceStatus::Stopped);
        assert!(recovery.snapshot.active_turn_id.is_none());
        assert!(recovery.snapshot.live_run.is_none());
        assert!(recovery.snapshot.pending_interactions.is_empty());
        assert_eq!(
            crate::session::storage::load_session_id(child_instance.id().as_str())
                .unwrap()
                .transcript
                .leaf_id,
            recovery.transcript.leaf_id
        );
        assert!(test_runtime.sessions.list().await.unwrap().is_empty());
        let lock = crate::session::storage::lock_session_path(&child_path).unwrap();
        drop(lock);
    }
    assert!(test_runtime.sessions.list().await.unwrap().is_empty());
    let stopped_checkpoint =
        crate::session::storage::load_session_checkpoint(child.id.as_str()).unwrap();
    assert_eq!(
        stopped_checkpoint.transcript.leaf_id,
        child_instance.view().await.unwrap().transcript.leaf_id
    );
    assert_eq!(
        grandchild.instance().snapshot().status,
        AgentInstanceStatus::Stopped
    );
    assert_eq!(
        crate::session::storage::load_session(grandchild_path.to_str().unwrap())
            .unwrap()
            .transcript
            .leaf_id,
        grandchild
            .instance()
            .finalization()
            .unwrap()
            .view
            .unwrap()
            .transcript
            .leaf_id
    );
    assert_eq!(
        leader.instance().snapshot().status,
        AgentInstanceStatus::Stopped
    );
    for path in [&grandchild_path, &leader_path] {
        drop(crate::session::storage::lock_session_path(path).unwrap());
    }
    let persisted = crate::session::storage::load_session(parent_path.to_str().unwrap()).unwrap();
    assert_eq!(
        persisted.children,
        [&link, &leader_link]
            .into_iter()
            .map(crate::session::ChildSessionLink::from)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        crate::session::storage::load_session(child_path.to_str().unwrap())
            .unwrap()
            .agent_kind,
        child.agent_kind
    );

    for persist in [true, false] {
        let volatile_runtime = Arc::new(TestRuntime::new(
            test_runtime
                .preparation
                .runtime_factory()
                .unwrap()
                .as_ref()
                .clone(),
        ));
        volatile_runtime
            .preparation
            .enable_subagents(&volatile_runtime.sessions);
        let mut volatile_session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        volatile_session.current_switch = "child-default".into();
        volatile_session.working_dir = root.to_string_lossy().into();
        let volatile = volatile_runtime
            .sessions
            .create(
                &volatile_runtime.preparation,
                volatile_session.clone(),
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
                        let mut instance =
                            AgentInstanceBuildOptions::from_session(&volatile_session);
                        instance.settings_manager = Some(settings_manager.clone());
                        instance.resource_settings = Some(Arc::new(settings.clone()));
                        instance
                    },
                },
                phi_sdk::lifecycle::SessionStartReason::New,
                persist,
            )
            .await
            .unwrap();
        let first = volatile
            .instance()
            .spawn_subagent(
                phi_sdk::agent_manager::AgentSpawnRequest::new("reviewer", "volatile inspect")
                    .working_dir(&worktree),
            )
            .await
            .unwrap();
        wait_for_child_outcome(&volatile, first.instance_id().as_str()).await;
        let volatile_executor = volatile_runtime
            .sessions
            .get(&AgentInstanceId::new(first.instance_id().as_str()))
            .unwrap();
        let volatile_child_path =
            crate::session::storage::session_path(&volatile_executor.session().await);
        let volatile_parent_path = crate::session::storage::session_path(&volatile_session);
        volatile_runtime
            .sessions
            .session_access(volatile.instance().id())
            .checkpoint()
            .await
            .unwrap();
        let observation = volatile.save_observation().await;
        assert_eq!(observation.persistent, persist);
        let done = observation.completed.unwrap();
        assert!(done.error.is_none());
        assert_eq!(done.saved_boundary.is_some(), persist);
        let recent = volatile.instance().subagent_snapshots().await.unwrap();
        assert_eq!(recent.len(), 1);
        assert!(
            recent
                .iter()
                .all(|snapshot| snapshot.status.outcome.is_some())
        );
        let child_view = volatile_runtime
            .sessions
            .session_access(volatile.instance().id())
            .subagent_view(first.instance_id().as_str())
            .await
            .unwrap();
        assert_eq!(child_view.session.id, first.instance_id().as_str());
        assert_eq!(volatile_parent_path.exists(), persist);
        assert_eq!(volatile_child_path.exists(), persist);
        volatile_runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        assert!(volatile_runtime.sessions.list().await.unwrap().is_empty());
        let history = if persist {
            crate::session::storage::load_session(volatile_child_path.to_str().unwrap())
                .unwrap()
                .transcript
        } else {
            volatile_executor
                .instance()
                .view()
                .await
                .unwrap()
                .transcript
        };
        assert!(history.has_user_message());
        let stopped = volatile.instance().subagent_snapshots().await.unwrap();
        assert_eq!(stopped.len(), recent.len());
        assert!(
            stopped
                .iter()
                .all(|snapshot| snapshot.status.outcome.is_some())
        );
        assert_eq!(volatile_parent_path.exists(), persist);
        assert_eq!(volatile_child_path.exists(), persist);
    }
    assert!(
        !fetch_head.exists(),
        "child lifecycle repeated automatic Memory synchronization"
    );
    assert!(worktree.is_dir());
    assert!(!root.join(".git").exists());
    assert!(!worktree.join(".git").exists());
    std::fs::remove_dir_all(&worktree).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    std::fs::remove_file(parent_path).unwrap();
    assert!(child_path.exists());
    std::fs::remove_file(child_path).unwrap();
    std::fs::remove_file(grandchild_path).unwrap();
    std::fs::remove_file(leader_path).unwrap();
}
