use super::*;
use crate::runtime::preparation::{SessionWorkflows, tests::session_test_runtime};
use phi_sdk::transcript::TranscriptEntryKind;

#[tokio::test]
async fn history_changes_checkpoint_without_a_turn() {
    let (root, policy, runtime) = session_test_runtime();
    policy.install(&runtime.preparation);
    let mut session =
        crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
    session.working_dir = root.display().to_string().into();
    session.transcript.append_assistant_message(
        None,
        Some("prior-turn".into()),
        vec![phi_sdk::transcript::ContentBlock::Text {
            text: "previous answer".into(),
        }],
        "local".into(),
        phi_sdk::providers::ProviderApi::OpenAiCompletions,
        "test".into(),
        Default::default(),
        "stop".into(),
    );
    let agent = SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone())
        .unwrap()
        .start_session(session, true)
        .await
        .unwrap();
    let wait_for_save = |attempt| {
        let agent = agent.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let observation = agent.save_observation().await;
                    if observation.pending.is_none()
                        && let Some(completion) = observation.completed
                        && completion.attempt > attempt
                        && completion.error.is_none()
                    {
                        break completion.attempt;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap()
        }
    };
    let attempt = wait_for_save(0).await;
    let view = agent
        .instance()
        .reset_context(Some("passive handoff".into()))
        .await
        .unwrap();
    wait_for_save(attempt).await;
    let session = agent.session().await;
    assert_eq!(
        serde_json::to_value(&session.transcript).unwrap(),
        serde_json::to_value(&view.transcript).unwrap()
    );
    let path = crate::session::storage::session_path(&session);
    let saved = crate::session::storage::load_session(path.to_str().unwrap()).unwrap();
    assert_eq!(
        serde_json::to_value(saved.transcript).unwrap(),
        serde_json::to_value(session.transcript).unwrap()
    );
    assert_eq!(
        agent.instance().snapshot().status,
        AgentInstanceStatus::Idle
    );
    runtime
        .sessions
        .shutdown(SessionEndReason::Quit)
        .await
        .unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn saved_bash_history_recovers_once_without_starting_work() {
    for status in ["running", "stopped"] {
        let (root, policy, runtime) = session_test_runtime();
        policy.install(&runtime.preparation);
        let workflows =
            SessionWorkflows::new(runtime.sessions.clone(), runtime.preparation.clone()).unwrap();
        let mut session =
            crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.display().to_string().into();
        session
            .transcript
            .append_user_message("run background work");
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
        session.transcript.append_entry(TranscriptEntryKind::Event {
            name: "phi_sdk.bash_execution".into(),
            data: serde_json::json!({ "id": "saved-execution", "turn_id": "old-turn", "tool_call_id": "old-call",
                "command": "printf result", "mode": "background", "status": status,
                "output_path": "/tmp/phi-recovery-output.log", "exit_code": null, "error": null }),
        });
        crate::session::storage::save_session(&session).unwrap();
        let original = serde_json::to_value(&session.transcript.entries).unwrap();
        let original_count = session.transcript.entries.len();
        let mut recovered = None;
        for _ in 0..2 {
            let agent = workflows
                .start_saved_session(&session.id, true)
                .await
                .unwrap();
            let view = tokio::time::timeout(
                Duration::from_secs(2),
                agent.instance().wait_for_quiescence(None),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(view.snapshot.status, AgentInstanceStatus::Idle);
            assert!(view.snapshot.bash_executions.is_empty());
            assert!(view.pending_submission_ids.is_empty());
            assert_eq!(
                serde_json::to_value(&view.transcript.entries[..original_count]).unwrap(),
                original
            );
            let history = serde_json::to_string(&view.transcript).unwrap();
            assert_eq!(history.matches("previous Agent runtime").count(), 1);
            assert_eq!(history.matches("phi_sdk.bash_feedback").count(), 1);
            assert_eq!(
                history.contains("terminal outcome is unknown"),
                status == "running"
            );
            runtime
                .sessions
                .session_access(agent.instance().id())
                .checkpoint()
                .await
                .unwrap();
            let saved = crate::session::storage::load_session_id(&session.id).unwrap();
            let entries = serde_json::to_value(saved.transcript.entries).unwrap();
            assert_eq!(
                entries,
                serde_json::to_value(&view.transcript.entries).unwrap()
            );
            if let Some(previous) = &recovered {
                assert_eq!(&entries, previous);
            }
            recovered = Some(entries);
            runtime
                .sessions
                .stop(agent.instance().id(), SessionEndReason::Quit)
                .await
                .unwrap();
        }
        runtime
            .sessions
            .shutdown(SessionEndReason::Quit)
            .await
            .unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
