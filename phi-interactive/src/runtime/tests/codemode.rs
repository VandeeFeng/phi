use super::*;

#[tokio::test]
async fn codemode_reload_accepts_valid_config_and_preserves_generation_on_error() {
    if run_isolated_test(concat!(
        module_path!(),
        "::codemode_reload_accepts_valid_config_and_preserves_generation_on_error"
    )) {
        return;
    }
    let (root, policy, test_runtime) = session_test_runtime();
    install_reload_catalog(&root, &policy.configuration.snapshot().model_catalog);
    policy
        .configuration
        .generation
        .write()
        .unwrap()
        .settings
        .no_tools = Some(false);
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
    let access = test_runtime.sessions.session_access(agent.instance().id());
    std::fs::create_dir_all(root.join(".phi")).unwrap();
    let settings = root.join(".phi/settings.json");
    let initial = agent.snapshot().await.runtime_generation_id;
    assert!(!agent.runtime().runtime_config().extensions.codemode.enabled);
    std::fs::write(&settings, r#"{"codemode":{}}"#).unwrap();
    Box::pin(test_runtime.preparation.reload_resources(&access))
        .await
        .unwrap();
    assert!(!agent.runtime().runtime_config().extensions.codemode.enabled);
    std::fs::write(
        &settings,
        r#"{"codemode":{"inlineBudget":0,"cache":false}}"#,
    )
    .unwrap();
    Box::pin(test_runtime.preparation.reload_resources(&access))
        .await
        .unwrap();
    let enabled = agent.snapshot().await.runtime_generation_id;
    assert_ne!(initial, enabled);
    assert!(agent.runtime().runtime_config().extensions.codemode.enabled);
    let config = policy.configuration.snapshot();
    assert!(config.settings.codemode.as_ref().unwrap().enabled);
    assert_eq!(config.settings.codemode.as_ref().unwrap().inline_budget, 0);
    assert_eq!(
        config.settings.codemode.as_ref().unwrap().cache,
        phi_sdk::config::CodemodeCache::Enabled(false)
    );
    assert!(
        crate::agent_options_from_cli(&config.cli, &config.settings)
            .codemode
            .is_none()
    );
    let factory = test_runtime.preparation.runtime_factory().unwrap();
    let transcript = agent.instance().transcript().await.unwrap();
    for invalid in [
        r#"{"codemode":{"mode":"on"}}"#,
        r#"{"codemode":{"enabled":"true"}}"#,
        r#"{"codemode":{"enabled":true,"unknown":1}}"#,
        r#"{"codemode":{"enabled":true,"inline_budget":0}}"#,
        r#"{"codemode":{"enabled":true,"inlineBudget":-1}}"#,
        r#"{"codemode":{"cache":null}}"#,
        r#"{"codemode":{"cache":0}}"#,
        r#"{"codemode":{"cache":""}}"#,
    ] {
        std::fs::write(&settings, invalid).unwrap();
        assert!(
            Box::pin(test_runtime.preparation.reload_resources(&access))
                .await
                .is_err()
        );
        assert_eq!(agent.snapshot().await.runtime_generation_id, enabled);
        assert!(Arc::ptr_eq(
            &factory,
            &test_runtime.preparation.runtime_factory().unwrap()
        ));
        assert!(
            policy
                .configuration
                .snapshot()
                .settings
                .codemode
                .as_ref()
                .unwrap()
                .enabled
        );
        assert_eq!(
            agent.instance().transcript().await.unwrap().leaf_id,
            transcript.leaf_id
        );
    }
    std::fs::write(
        &settings,
        r#"{"extensions":["-builtin:codemode"],"codemode":{"enabled":true}}"#,
    )
    .unwrap();
    Box::pin(test_runtime.preparation.reload_resources(&access))
        .await
        .unwrap();
    assert!(
        policy
            .configuration
            .snapshot()
            .settings
            .codemode
            .as_ref()
            .unwrap()
            .enabled
    );
    assert!(!agent.runtime().runtime_config().extensions.codemode.enabled);
    let excluded = agent.snapshot().await.runtime_generation_id;
    std::fs::write(
        &settings,
        r#"{"extensions":[],"codemode":{"enabled":true}}"#,
    )
    .unwrap();
    Box::pin(test_runtime.preparation.reload_resources(&access))
        .await
        .unwrap();
    assert!(agent.runtime().runtime_config().extensions.codemode.enabled);
    assert_ne!(agent.snapshot().await.runtime_generation_id, excluded);
    std::fs::write(
        &settings,
        r#"{"codemode":{"enabled":false},"defaultThinkingLevel":"medium","switches":[{"name":"default","thinking_level":"high"}]}"#,
    ).unwrap();
    Box::pin(test_runtime.preparation.reload_resources(&access))
        .await
        .unwrap();
    assert_ne!(agent.snapshot().await.runtime_generation_id, enabled);
    assert!(
        !policy
            .configuration
            .snapshot()
            .settings
            .codemode
            .as_ref()
            .unwrap()
            .enabled
    );
    let workflows = SessionWorkflows::new(
        test_runtime.sessions.clone(),
        test_runtime.preparation.clone(),
    )
    .unwrap();
    let next = Box::pin(workflows.new_session(agent.instance().id()))
        .await
        .unwrap();
    assert_eq!(
        next.accepted
            .declarations
            .capabilities
            .thinking_level
            .as_deref(),
        Some("high")
    );
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
