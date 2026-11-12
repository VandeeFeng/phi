use super::*;
use crate::core::event::{PhiFailure, PhiFailureKind};
use crate::core::interaction::{InteractionResponse, PermissionDecision};
use crate::factory::{Phi, PhiRuntimeServices};
use crate::harness::instance::builder::AgentInstanceBuildError;
use crate::permission::command::CommandPermissionRequest;
use crate::permission::{Action, PermissionConfig, ToolPerm};
use crate::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind, UserContentBlock};
use std::sync::Arc;

impl AgentInstanceState {
    pub(super) async fn submit_input(
        &mut self,
        prompt: TurnPrompt,
        intent: SubmissionIntent,
    ) -> Result<SubmissionDisposition, PhiTurnError> {
        let mut preparation = self.prepare_input(prompt, intent)?;
        let result = preparation.ready().await;
        self.finish_submission(preparation, result)
    }
}

pub(super) async fn test_client() -> AgentInstanceState {
    test_client_with_agent_options(true).await
}

fn accepted_child(state: &mut AgentInstanceState, prompt: &str) -> AgentCreation {
    let creation = AgentCreation::new(
        AgentInstanceId::new(state.transcript.id.as_str()),
        &AgentSpawnRequest::new("reviewer", prompt),
        state.working_dir().to_path_buf(),
        None,
    );
    state.child_observations.insert(
        creation.instance_id().clone(),
        ChildObservation {
            status: SubagentStatus {
                creation: creation.clone(),
                parent_tool_call_id: None,
                admission: SubagentAdmission::Accepted,
                outcome: None,
            },
            completion: Default::default(),
        },
    );
    creation
}

fn complete_child(
    state: &mut AgentInstanceState,
    child: &AgentCreation,
    outcome: Result<String, PhiFailure>,
) {
    let completion = state
        .child_observations
        .get(child.instance_id())
        .unwrap()
        .completion
        .clone();
    let _ = completion.publish(outcome);
    state.record_subagent_result(ChildResult {
        creation: child.clone(),
        outcome: completion.get().unwrap().outcome,
    });
}

fn agent_messages(state: &AgentInstanceState) -> Vec<String> {
    state
        .transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message {
                message: AgentMessage::Agent { content },
            } => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn once_survives_discard_reset_compaction_and_reload() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let mut state = test_client().await;
    state.generation.hooks = HookRegistry::new();
    state.generation.hooks.register_hook(
        crate::hooks::Hook::prompt_context({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async {
                    Ok(crate::core::lifecycle::PromptContextOutcome {
                        contexts: vec![InjectedContext {
                            source: ContextSource::Hook,
                            content: "context".into(),
                        }],
                        ..Default::default()
                    })
                }
            }
        })
        .with_once(true),
    );
    let independent = state.generation.hooks.for_generation();
    for step in ["initial", "next", "reset", "compaction", "replacement"] {
        if step == "reset" {
            state.reset_context(None).unwrap();
        } else if step == "compaction" {
            state.transcript.append_user_message("older input");
            state.transcript.append_user_message("retained input");
            let head = state.transcript.leaf_id.clone().unwrap();
            state.transcript.compress("summary".into(), &head).unwrap();
        } else if step == "replacement" {
            let mut candidate = test_client().await;
            candidate.generation.hooks = state.generation.hooks.for_generation();
            state.replace_runtime(candidate.generation).unwrap();
        }
        let prepared = state
            .prompt_preparation()
            .prepare(
                turn_prompt("input"),
                crate::core::lifecycle::UserPromptDispatch::Immediate,
            )
            .await
            .unwrap();
        let CoreTurnPreparation::Ready(prompt) = prepared else {
            panic!("input must be prepared");
        };
        assert_eq!(prompt.contexts.len(), usize::from(step == "initial"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Discard contributions instead of admitting them.
    }
    let mut separate = test_client().await;
    separate.generation.hooks = independent;
    separate
        .prompt_preparation()
        .prepare(
            turn_prompt("input"),
            crate::core::lifecycle::UserPromptDispatch::Immediate,
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn internal_outcome_start_is_atomic() {
    let mut state = test_client().await;
    state
        .generation
        .hooks
        .register_hook(crate::hooks::Hook::prompt_context(|_| async {
            Ok(crate::core::lifecycle::PromptContextOutcome {
                contexts: vec![InjectedContext {
                    source: ContextSource::Hook,
                    content: "internal-outcome-context".into(),
                }],
                ..Default::default()
            })
        }));
    let completion = crate::harness::instance::child::InitialCompletion::default();
    completion.publish(Ok("initial result".into())).unwrap();
    state.delegated_agent = Some(DelegatedAgent {
        definition: AgentDefinition::new("reviewer", "review code", "Review.").model("missing"),
        scope: CoreEventScope {
            session_id: Some(state.transcript.id.clone()),
            turn_id: Some("previous-turn".into()),
            parent_tool_call_id: Some("initial-call".into()),
            ..Default::default()
        },
        initial_completion: completion,
        outcome: Some(Ok("initial result".into())),
    });
    state.set_bash_owner_mode(DelegationMode::Foreground);
    let child = accepted_child(&mut state, "inspect");
    complete_child(&mut state, &child, Ok("child result".into()));
    let history = crate::context::convert_history(&state.transcript).unwrap();
    assert!(state.start_next_subagent_outcome().is_err());
    assert!(state.admitted_subagent_outcomes.is_empty());
    state.start_next_subagent_outcome().unwrap();
    assert_eq!(
        crate::context::convert_history(&state.transcript).unwrap(),
        history
    );
    assert!(state.transcript.entries.iter().any(|entry| matches!(
        &entry.kind,
        TranscriptEntryKind::Event { name, data }
            if name == INTERNAL_FEEDBACK_FAILURE_EVENT && data["prompt"].as_str().unwrap().contains("child result")
    )));
    assert!(state.can_start_main());
    assert_eq!(state.bash_owner_mode, DelegationMode::Foreground);
    let agent = state.delegated_agent.as_ref().unwrap();
    assert_eq!(agent.scope.turn_id.as_deref(), Some("previous-turn"));
    assert_eq!(agent.outcome, Some(Ok("initial result".into())));

    state.delegated_agent.as_mut().unwrap().definition =
        AgentDefinition::new("reviewer", "review code", "Review.");
    let next_child = accepted_child(&mut state, "next task");
    complete_child(&mut state, &next_child, Ok("next result".into()));
    state.start_next_subagent_outcome().unwrap();
    assert!(state.admitted_subagent_outcomes.is_empty());
    assert_eq!(agent_messages(&state).len(), 1);
    assert_eq!(state.bash_owner_mode, DelegationMode::Background);
    let agent = state.delegated_agent.as_ref().unwrap();
    assert_eq!(
        agent.scope.turn_id.as_deref(),
        state.active_turn_id().as_deref()
    );
    assert_ne!(agent.scope.turn_id.as_deref(), Some("previous-turn"));
    assert!(agent.scope.parent_tool_call_id.is_none());
    assert!(agent.outcome.is_none());
    assert!(
        serde_json::to_string(&crate::context::convert_history(&state.transcript).unwrap())
            .unwrap()
            .contains("internal-outcome-context")
    );
    state.abort();
}

#[tokio::test]
async fn pending_admission_abort_preserves_termination_source() {
    use crate::CancellationReason;

    for reason in [
        None,
        Some(CancellationReason::Interrupted),
        Some(CancellationReason::UserRequested),
        Some(CancellationReason::AgentStopped),
    ] {
        let mut state = test_client().await;
        state.transcript.append_assistant_message(
            None,
            None,
            vec![ContentBlock::ToolCall {
                id: "pending-call".into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "Agent".into(),
                args: serde_json::json!({"role": "reviewer", "prompt": "inspect"}),
            }],
            "test".into(),
            crate::provider::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "tool_call".into(),
        );
        let child = accepted_child(&mut state, "pending admission");
        let observed = state
            .child_observations
            .get_mut(child.instance_id())
            .unwrap();
        observed.status.creation = child.with_tool_call(Some("pending-call".into()));
        observed.status.admission = SubagentAdmission::Pending;
        if let Some(reason) = reason {
            observed.completion.cancel_with_reason(reason);
        }
        state.abort();
        let history = serde_json::to_string(&state.transcript).unwrap();
        assert!(history.contains("admission was interrupted"));
        assert_eq!(
            history.contains("Do not automatically retry or recreate"),
            reason == Some(CancellationReason::UserRequested)
        );
        assert_eq!(
            history.contains("safely resume or retry"),
            matches!(reason, None | Some(CancellationReason::Interrupted))
        );
        let completion = &state.child_observations.values().next().unwrap().completion;
        assert!(completion.is_cancelled());
        assert_eq!(
            completion.cancellation_reason(),
            reason.unwrap_or(CancellationReason::Interrupted)
        );
    }
}

#[tokio::test]
async fn shutdown_propagates_stop_to_children() {
    let mut running = test_client().await;
    let child = accepted_child(&mut running, "running child");
    running.child_workers.insert(
        child.instance_id().clone(),
        ChildWorker {
            creation: child.clone(),
            handle: tokio::spawn(std::future::pending()),
            cancellation: tokio::sync::watch::channel(false).0,
        },
    );
    running.shutdown();
    let completion = &running.child_observations[child.instance_id()].completion;
    assert!(completion.is_cancelled());
    assert_eq!(
        completion.cancellation_reason(),
        crate::CancellationReason::AgentStopped
    );
    running
        .child_workers
        .remove(child.instance_id())
        .unwrap()
        .handle
        .abort();
}

#[tokio::test]
async fn child_continuations_enforce_capacity_and_shutdown() {
    let mut state = test_client().await;
    for _ in 0..MAX_QUEUED_SUBMISSIONS {
        let child = accepted_child(&mut state, "inspect");
        complete_child(&mut state, &child, Ok("done".into()));
    }
    assert_eq!(
        state.admitted_subagent_outcomes.len(),
        MAX_QUEUED_SUBMISSIONS
    );
    assert_eq!(
        state.subagent_snapshots().len(),
        super::delegation::RECENT_CHILD_LIMIT
    );
    let child = accepted_child(&mut state, "capacity probe");
    let instance = crate::harness::instance::AgentInstance::spawn(AgentInstanceCandidate {
        state: test_client().await,
    });
    assert!(matches!(
        state.observe_started_child(child, Default::default(), instance.clone()),
        Err(PhiSubagentError::ChildLimit)
    ));
    assert_eq!(
        state.admitted_subagent_outcomes.len(),
        MAX_QUEUED_SUBMISSIONS
    );
    state.abort();
    assert_eq!(
        state.admitted_subagent_outcomes.len(),
        MAX_QUEUED_SUBMISSIONS
    );
    state.shutdown();
    assert!(state.admitted_subagent_outcomes.is_empty());
    instance.stop().await.unwrap();
}

#[tokio::test]
async fn outcomes_wait_for_parent_and_accepted_user_input_and_deliver_once() {
    let mut state = test_client().await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    state
        .generation
        .hooks
        .register_hook(crate::hooks::Hook::user_prompt_submit(move |event| {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                    notices: Vec::new(),
                    prompt: event.prompt,
                    injected_context: Vec::new(),
                })
            }
        }));
    let children = (0..3)
        .map(|index| accepted_child(&mut state, &format!("task-{index}")))
        .collect::<Vec<_>>();
    let parent = state.install_test_main_run("parent-turn");
    complete_child(&mut state, &children[0], Ok("final-answer".into()));
    complete_child(
        &mut state,
        &children[1],
        Err(PhiFailure::new(
            PhiFailureKind::Internal,
            "child-failed",
            None,
        )),
    );
    complete_child(
        &mut state,
        &children[2],
        Err(PhiFailure::cancelled(
            crate::CancellationReason::Interrupted,
            "interrupted",
        )),
    );
    let accepted = state
        .submit_input(
            turn_prompt("accepted-user"),
            SubmissionIntent::Dispatch(SubmissionDispatch::FollowUp),
        )
        .await
        .unwrap();
    assert!(matches!(accepted, SubmissionDisposition::Queued { .. }));
    state.drain_events().await.unwrap();
    assert!(agent_messages(&state).is_empty());
    assert_eq!(state.admitted_subagent_outcomes.len(), 3);
    for child in &children {
        complete_child(&mut state, child, Ok("must-not-replace".into()));
    }
    assert_eq!(state.admitted_subagent_outcomes.len(), 3);
    drop(parent);
    state.drain_events().await.unwrap();
    assert!(agent_messages(&state).is_empty());
    assert!(
        state
            .transcript
            .current_branch()
            .iter()
            .any(|entry| matches!(
                &entry.kind,
                crate::transcript::TranscriptEntryKind::Message {
                    message: crate::transcript::AgentMessage::User { content }
                } if content.iter().any(|block| matches!(block,
                    crate::transcript::UserContentBlock::Text { text } if text == &"accepted-user"))
            ))
    );
    for _ in &children {
        state.abort_main();
        state.dispatch_next_queued_turn(&mut Vec::new()).unwrap();
    }
    let messages = agent_messages(&state);
    assert_eq!(messages.len(), 3);
    for (message, child) in messages.iter().zip(&children) {
        let (_, result) = message.rsplit_once('\n').unwrap();
        let result: serde_json::Value = serde_json::from_str(result).unwrap();
        assert_eq!(result["instance_id"], child.instance_id().as_str());
        assert_eq!(result["role"], child.definition());
        assert_eq!(result["prompt"], child.prompt());
        assert!(!message.contains("taskId"));
    }
    assert!(messages[0].contains("final-answer"));
    assert!(messages[1].contains("child-failed"));
    assert!(messages[2].contains("interrupted"));
    assert!(messages[2].contains("safely resume or retry"));
    assert!(!messages[0].contains(CANCELLED_EXECUTION_GUIDANCE));
    assert!(!messages[1].contains(CANCELLED_EXECUTION_GUIDANCE));
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        children.len() + 1
    );
    complete_child(&mut state, &children[0], Ok("duplicate-answer".into()));
    assert!(state.admitted_subagent_outcomes.is_empty());
    assert_eq!(agent_messages(&state).len(), 3);
    let persisted = serde_json::to_string(&state.transcript).unwrap();
    assert!(!persisted.contains("private-child-history"));
    assert!(!persisted.contains("duplicate-answer"));
    let restored: Transcript = serde_json::from_str(&persisted).unwrap();
    assert!(!restored.entries.iter().any(|entry| matches!(&entry.kind,
        TranscriptEntryKind::Event { name, .. } if name.starts_with("phi_sdk.subagent_outcome_")
    )));
    state.abort();
}

#[tokio::test]
async fn suspended_agent_records_background_results_without_waking() {
    for outcome in [
        Ok("finished background work".into()),
        Err(PhiFailure::new(
            PhiFailureKind::Internal,
            "failed background work",
            None,
        )),
    ] {
        let mut state = test_client().await;
        let child = accepted_child(&mut state, "background work");
        state.suspend_automatic_turns();
        complete_child(&mut state, &child, outcome);
        state.dispatch_next_queued_turn(&mut Vec::new()).unwrap();
        assert!(state.can_start_main());
        assert!(state.admitted_subagent_outcomes.is_empty());
        let context =
            serde_json::to_string(&crate::context::convert_history(&state.transcript).unwrap())
                .unwrap();
        assert!(context.contains("background work"));
        assert!(context.contains("not human input or approval"));
    }
}

#[tokio::test]
async fn child_control_cancellation_is_passive() {
    for reason in [
        crate::CancellationReason::UserRequested,
        crate::CancellationReason::AgentStopped,
    ] {
        let mut state = test_client().await;
        let child = accepted_child(&mut state, "cancelled background work");
        let parent = state.install_test_main_run("parent-turn");
        complete_child(
            &mut state,
            &child,
            Err(PhiFailure::cancelled(reason, "cancelled")),
        );
        assert_eq!(state.active_turn_id().as_deref(), Some("parent-turn"));
        assert!(state.admitted_subagent_outcomes.is_empty());
        let expected_records = usize::from(reason == crate::CancellationReason::UserRequested);
        assert_eq!(agent_messages(&state).len(), expected_records);
        complete_child(&mut state, &child, Ok("must-not-replace".into()));
        assert_eq!(agent_messages(&state).len(), expected_records);
        assert_eq!(
            state.subagent_snapshots()[0]
                .status
                .outcome
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err()
                .cancellation_reason,
            Some(reason)
        );
        state.abort_main();
        drop(parent);
        state.dispatch_next_queued_turn(&mut Vec::new()).unwrap();
        assert!(state.can_start_main());
        if reason == crate::CancellationReason::UserRequested {
            let history = serde_json::to_string(&state.transcript).unwrap();
            let restored: Transcript = serde_json::from_str(&history).unwrap();
            let provider_context = crate::context::convert_history(&restored).unwrap();
            let context = serde_json::to_string(&provider_context).unwrap();
            assert!(context.contains("user_requested"));
            assert!(context.contains("Do not automatically retry or recreate"));
            assert!(context.contains("does not by itself withdraw the overall user goal"));
        }
    }
}

#[tokio::test]
async fn abort_distinguishes_foreground_wait_from_child_outcome() {
    use crate::harness::manager::preparation::DelegationMode;

    for mode in [DelegationMode::Foreground, DelegationMode::Background] {
        for outcome in [
            None,
            Some(Ok("child finished".to_string())),
            Some(Err(PhiFailure::cancelled(
                crate::CancellationReason::UserRequested,
                "subagent cancelled",
            ))),
            Some(Err(PhiFailure::cancelled(
                crate::CancellationReason::Interrupted,
                "subagent interrupted",
            ))),
            Some(Err(PhiFailure::cancelled(
                crate::CancellationReason::AgentStopped,
                "Agent stopped",
            ))),
            Some(Err(PhiFailure::new(
                PhiFailureKind::Cancelled,
                "legacy cancellation",
                None,
            ))),
            Some(Err(PhiFailure::new(
                PhiFailureKind::Internal,
                "unexpected child failure",
                None,
            ))),
        ] {
            let mut state = test_client().await;
            state.transcript.append_assistant_message(
                None,
                None,
                vec![ContentBlock::ToolCall {
                    id: "agent-call".into(),
                    provider_call_id: None,
                    provider_item_id: None,
                    name: "Agent".into(),
                    args: serde_json::json!({"role": "reviewer", "prompt": "inspect"}),
                }],
                "test".into(),
                crate::provider::ProviderApi::OpenAiCompletions,
                "test".into(),
                Default::default(),
                "tool_call".into(),
            );
            let child = AgentCreation::new(
                AgentInstanceId::new(state.transcript.id.as_str()),
                &AgentSpawnRequest::new("reviewer", "inspect").mode(mode),
                std::env::temp_dir(),
                None,
            )
            .with_tool_call(Some("agent-call".into()));
            state.child_observations.insert(
                child.instance_id().clone(),
                ChildObservation {
                    status: SubagentStatus {
                        creation: child.clone(),
                        parent_tool_call_id: Some("agent-call".into()),
                        admission: SubagentAdmission::Accepted,
                        outcome: outcome.clone(),
                    },
                    completion: Default::default(),
                },
            );
            state.abort();
            let results = state
                .transcript
                .entries
                .iter()
                .filter_map(|entry| match &entry.kind {
                    TranscriptEntryKind::Message {
                        message:
                            AgentMessage::ToolResult {
                                tool_call_id,
                                content,
                                is_error,
                                ..
                            },
                    } if tool_call_id == "agent-call" => Some((content, *is_error)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(results.len(), 1);
            let ContentBlock::Text { text } = &results[0].0[0] else {
                panic!("expected tool result text");
            };
            if mode == DelegationMode::Background {
                assert!(!results[0].1);
                assert!(text.contains("async_launched"));
            } else {
                match &outcome {
                    None => {
                        assert!(results[0].1);
                        assert!(
                            text.starts_with(
                                "agent tool call interrupted; subagent is still running"
                            )
                        );
                        assert!(text.contains("Observe the existing execution"));
                        assert!(!text.contains("only if the user requests"));
                    }
                    Some(Ok(_)) => assert!(!results[0].1),
                    Some(Err(failure)) => {
                        assert!(results[0].1);
                        assert!(text.starts_with(&failure.message));
                        assert_eq!(
                            text.contains("Do not automatically retry or recreate"),
                            failure.is_user_cancelled()
                        );
                        if failure.kind == PhiFailureKind::Cancelled {
                            assert_eq!(
                                text.contains("safely resume or retry"),
                                matches!(
                                    failure.cancellation_reason,
                                    None | Some(crate::CancellationReason::Interrupted)
                                )
                            );
                        }
                    }
                }
            }
            assert_eq!(
                state
                    .child_observations
                    .values()
                    .next()
                    .unwrap()
                    .status
                    .outcome,
                outcome
            );
            let settled = serde_json::to_value(&state.transcript.entries).unwrap();
            if outcome.is_none() {
                complete_child(
                    &mut state,
                    &child,
                    Err(PhiFailure::new(
                        PhiFailureKind::Cancelled,
                        "cancelled",
                        None,
                    )),
                );
            }
            state.abort();
            assert_eq!(
                serde_json::to_value(&state.transcript.entries).unwrap(),
                settled
            );
            assert_eq!(
                state
                    .subagent_status(child.instance_id())
                    .unwrap()
                    .admission,
                SubagentAdmission::Accepted
            );
        }
    }
}

fn test_model_catalog() -> Arc<crate::models::ModelCatalog> {
    let model = crate::models::ModelDescriptor {
        provider: "openai-codex".to_string(),
        id: "gpt-5.6-sol".to_string(),
        name: "GPT-5.6-Sol".to_string(),
        api: crate::provider::ProviderApi::OpenAiCodexResponses,
        reasoning: true,
        thinking_level_map: None,
        input: vec![crate::models::ModelInput::Text],
        context_window: 272_000,
        max_output_tokens: 128_000,
        cost: crate::models::ModelCost::default(),
    };
    Arc::new(crate::models::ModelCatalog::for_test_model(model).unwrap())
}

pub(super) fn test_services() -> PhiRuntimeServices {
    PhiRuntimeServices::new().model_catalog(test_model_catalog())
}

pub(super) fn turn_prompt(prompt: &str) -> TurnPrompt {
    TurnPrompt {
        tool_policy: Default::default(),
        prompt: prompt.to_string(),
        source: UserPromptSubmitSource::Sdk,
        injected_context: Vec::new(),
        images: Vec::new(),
    }
}

fn install_main_run(
    client: &mut AgentInstanceState,
    steering: Arc<crate::harness::agentic_loop::AgentSteerMailbox>,
) -> tokio::sync::mpsc::Sender<crate::core::event::AgentEvent> {
    let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
    client.main_run = Some(EventPipeline::new(
        crate::harness::agentic_loop::AgentRunner {
            tool_policy: None,
            round_requests: None,
            event_rx,
            steering,
            task: tokio::spawn(async {}),
        },
        CoreEventScope::default(),
    ));
    event_tx
}

async fn test_client_with_agent_options(no_tools: bool) -> AgentInstanceState {
    let agent = AgentDefinition::new("reviewer", "review code", "review carefully").max_turns(1);
    let mut context = crate::context::load(true);
    context.agent_catalog = AgentCatalog::new(vec![agent]);
    Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .context(context)
        .no_tools(no_tools)
        .build()
        .unwrap()
        .with_runtime_services(test_services())
        .prepare_agent_instance()
        .await
        .unwrap()
        .into_state()
}

#[tokio::test]
async fn instance_build_reports_unknown_model_structurally() {
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .no_tools(true)
        .build()
        .unwrap();

    let result = phi
        .with_runtime_services(test_services())
        .agent_instance_builder()
        .model("missing-model")
        .build()
        .await;
    let Err(error) = result else {
        panic!("unknown model unexpectedly prepared an Agent Instance");
    };

    assert!(matches!(
        error,
        AgentInstanceBuildError::UnknownModel { provider, model }
            if provider == "openai-codex" && model == "missing-model"
    ));
}

#[tokio::test]
async fn reused_factory_preserves_independent_permission_routing() {
    let services = test_services()
        .permission_policy(
            PermissionConfig {
                bash: Some(ToolPerm::Simple(Action::Ask)),
                ..Default::default()
            },
            None,
        )
        .interactive(true);
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .context(crate::context::load(true))
        .no_tools(true)
        .runtime_services(services.clone())
        .build()
        .unwrap();
    let mut first = phi.prepare_agent_instance().await.unwrap().into_state();
    let permission = first.generation.resources.permission().clone();
    let mut second = phi.prepare_agent_instance().await.unwrap().into_state();
    let other_permission = second.generation.resources.permission().clone();
    let discarded = phi.prepare_agent_instance().await.unwrap();
    drop(discarded);
    assert!(
        phi.agent_instance_builder()
            .model("missing-model")
            .build()
            .await
            .is_err()
    );

    let other_task = tokio::spawn(async move {
        other_permission
            .authorize_command(&CommandPermissionRequest::new("printf", &["second".into()]))
            .await
    });
    let task = tokio::spawn(async move {
        permission
            .authorize_command(&CommandPermissionRequest::new("printf", &["first".into()]))
            .await
    });
    let request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(request) = first.poll_interaction().await {
                return request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(&request.request,
        crate::core::interaction::InteractionRequest::Permission(permission)
        if permission.input.contains("first")));
    first
        .respond_interaction(
            &request.request_id,
            InteractionResponse::Permission(PermissionDecision::AllowAlways("printf first".into())),
        )
        .await
        .unwrap();
    let other_request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(request) = second.poll_interaction().await {
                return request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(matches!(&other_request.request,
        crate::core::interaction::InteractionRequest::Permission(permission)
        if permission.input.contains("second")));
    assert!(
        second
            .respond_interaction(
                &request.request_id,
                InteractionResponse::Permission(PermissionDecision::AllowOnce)
            )
            .await
            .is_err()
    );
    second
        .respond_interaction(
            &other_request.request_id,
            InteractionResponse::Permission(PermissionDecision::Deny),
        )
        .await
        .unwrap();
    assert!(task.await.unwrap().is_ok());
    assert!(other_task.await.unwrap().is_err());
    assert!(
        second
            .generation
            .resources
            .permission()
            .clone()
            .interactive(false)
            .authorize_bash("printf first", None)
            .await
            .is_err()
    );
    drop(first);
    drop(second);
    assert!(phi.prepare_agent_instance().await.is_ok());
}

#[tokio::test]
async fn reused_factory_isolates_live_grants_from_other_candidates_and_seeds() {
    for interactive in [false, true] {
        let services = test_services()
            .permission_policy(
                PermissionConfig {
                    read: Some(ToolPerm::Simple(Action::Ask)),
                    ..Default::default()
                },
                None,
            )
            .interactive(interactive);
        services.allow_session_tool("read", "seed.txt");
        let phi = Phi::builder()
            .provider("openai-codex")
            .model("gpt-5.6-sol")
            .context(crate::context::load(true))
            .no_tools(true)
            .runtime_services(services.clone())
            .build()
            .unwrap();
        let first = phi.prepare_agent_instance().await.unwrap().into_state();
        let permission = first
            .generation
            .resources
            .permission()
            .clone()
            .interactive(false);
        permission.add_session_allowlist("read", "live.txt");
        assert!(
            permission
                .authorize_tool("read", "live.txt", None, None)
                .await
                .is_ok()
        );
        let second = phi.prepare_agent_instance().await.unwrap().into_state();
        let other = second
            .generation
            .resources
            .permission()
            .clone()
            .interactive(false);
        assert!(
            other
                .authorize_tool("read", "seed.txt", None, None)
                .await
                .is_ok()
        );
        assert!(
            other
                .authorize_tool("read", "live.txt", None, None)
                .await
                .is_err()
        );
        assert!(
            services
                .prepare_permission(None)
                .interactive(false)
                .authorize_tool("read", "live.txt", None, None)
                .await
                .is_err()
        );
        services.allow_session_tool("read", "future.txt");
        for active in [&permission, &other] {
            assert!(
                active
                    .authorize_tool("read", "future.txt", None, None)
                    .await
                    .is_err()
            );
        }
        let future = phi.prepare_agent_instance().await.unwrap();
        assert!(
            future
                .resources()
                .permission()
                .clone()
                .interactive(false)
                .authorize_tool("read", "future.txt", None, None)
                .await
                .is_ok()
        );
    }
}

#[tokio::test]
async fn extension_bindings_use_only_their_candidate_permissions() {
    use crate::extension::{
        ExtensionCommandOutcome, ExtensionProcessPolicy, ExtensionRuntime,
        LocalExtensionProcessRunner,
    };
    let root =
        std::env::temp_dir().join(format!("phi-permission-bindings-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("probe.lua");
    std::fs::write(
        &path,
        r#"
register_extension({ name = "probe", capabilities = { process = {
  Command = { "printf" }, SessionStart = { "printf" },
} } })
local starts = 0
on("SessionStart", function(_event, ctx)
  ctx.process.run("printf", { "seed" })
  starts = starts + 1
end)
register_command("probe", { handler = function(args, ctx)
  ctx.process.run("printf", { args })
end })
register_tool("starts", { description = "starts", handler = function() return starts end })
"#,
    )
    .unwrap();
    let sandbox = crate::sandbox::Sandbox::new(false, &root).unwrap();
    let services = test_services()
        .permission_policy(
            PermissionConfig {
                bash: Some(ToolPerm::Simple(Action::Ask)),
                ..Default::default()
            },
            Some(root.clone()),
        )
        .interactive(true);
    services.allow_session_tool("bash", "printf seed");
    let extensions = Arc::new(
        ExtensionRuntime::try_load_with_paths_and_services(
            vec![path.clone()],
            services.extension_runtime_services(
                Arc::new(LocalExtensionProcessRunner::new(sandbox.clone())),
                ExtensionProcessPolicy::default().trust_root(&root),
                false,
            ),
        )
        .unwrap(),
    );
    let phi = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .context(crate::context::load(true))
        .no_tools(true)
        .runtime_services(services.sandbox(sandbox))
        .extension_runtime(extensions.clone())
        .build()
        .unwrap();
    let first = phi
        .agent_instance_builder()
        .working_dir(root.clone())
        .build()
        .await
        .unwrap();
    let mut second = phi
        .agent_instance_builder()
        .working_dir(root.clone())
        .build()
        .await
        .unwrap()
        .into_state();
    let resources = first.resources();
    resources.allow_session_tool("bash", "printf own");
    let bound = resources.extensions().unwrap();
    assert!(!Arc::ptr_eq(&bound, &extensions));
    std::fs::remove_file(path).unwrap();
    let frozen = phi
        .agent_instance_builder()
        .working_dir(root.clone())
        .build()
        .await
        .unwrap();
    resources
        .session_started(crate::core::lifecycle::SessionStartReason::Startup, None)
        .await;
    assert_eq!(bound.call_tool("starts", "{}".into()).unwrap(), "1");
    assert_eq!(extensions.call_tool("starts", "{}".into()).unwrap(), "0");
    assert_eq!(
        second
            .generation
            .resources
            .extensions()
            .unwrap()
            .call_tool("starts", "{}".into())
            .unwrap(),
        "0"
    );
    assert_eq!(
        frozen
            .resources()
            .extensions()
            .unwrap()
            .call_tool("starts", "{}".into())
            .unwrap(),
        "0"
    );
    assert!(matches!(
        bound.run_command("probe", "own").await.unwrap(),
        Some(ExtensionCommandOutcome::Completed(_))
    ));
    assert!(extensions.run_command("probe", "own").await.is_err());
    let other = second.generation.resources.extensions().unwrap();
    let pending = tokio::spawn(async move { other.run_command("probe", "own").await });
    let request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(request) = second.poll_interaction().await {
                return request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    second
        .respond_interaction(
            &request.request_id,
            InteractionResponse::Permission(PermissionDecision::AllowAlways("printf own".into())),
        )
        .await
        .unwrap();
    assert!(pending.await.unwrap().is_ok());
    assert!(
        second
            .generation
            .resources
            .permission()
            .clone()
            .interactive(false)
            .authorize_bash("printf own", None)
            .await
            .is_ok()
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn parent_abort_preserves_child_scoped_permission_interaction() {
    let config = PermissionConfig {
        bash: Some(ToolPerm::Simple(Action::Ask)),
        ..PermissionConfig::default()
    };
    let services = PhiRuntimeServices::new()
        .permission_policy(config, None)
        .model_catalog(test_model_catalog())
        .interactive(true);
    let mut client = Phi::builder()
        .provider("openai-codex")
        .model("gpt-5.6-sol")
        .no_tools(true)
        .build()
        .unwrap()
        .with_runtime_services(services)
        .prepare_agent_instance()
        .await
        .unwrap()
        .into_state();
    let permission = client.generation.resources.permission().clone();
    let scope = CoreEventScope {
        agent_id: Some("accepted-child".into()),
        ..Default::default()
    };
    let task = tokio::spawn(crate::core::interaction::with_interaction_scope(
        scope,
        async move {
            permission
                .authorize_command(&CommandPermissionRequest::new("printf", &[]))
                .await
        },
    ));
    let request = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if let Some(request) = client.poll_interaction().await {
                return request;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    assert_eq!(request.scope.agent_id.as_deref(), Some("accepted-child"));
    let _parent = client.install_test_main_run("parent-turn");
    client.abort();
    assert!(client.is_interaction_pending(&request.request_id));
    assert!(!task.is_finished());
    client
        .respond_interaction(
            &request.request_id,
            InteractionResponse::Permission(PermissionDecision::AllowOnce),
        )
        .await
        .unwrap();
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test]
async fn directed_turn_validates_agent_and_uses_normal_parent_input() {
    let mut state = test_client().await;
    assert!(matches!(
        state
            .submit_input(
                turn_prompt("@missing inspect"),
                SubmissionIntent::Directed(DirectedDelegation {
                    agent: "missing".into(),
                    mode: None,
                }),
            )
            .await,
        Err(PhiTurnError::UnknownDirectedAgent { .. })
    ));
    assert!(matches!(
        state
            .submit_input(
                turn_prompt("@reviewer inspect"),
                SubmissionIntent::Directed(DirectedDelegation {
                    agent: "reviewer".into(),
                    mode: None,
                }),
            )
            .await,
        Err(PhiTurnError::DirectedToolUnavailable)
    ));
    let (reply, result) = tokio::sync::oneshot::channel();
    state.spawn_subagent(
        AgentSpawnRequest::new("reviewer", "inspect").working_dir(std::env::temp_dir()),
        reply,
    );
    assert!(matches!(
        result.await.unwrap(),
        Err(PhiSubagentError::PreparationUnavailable)
    ));
    assert!(!state.transcript.has_user_message());
    let mut state = test_client_with_agent_options(false).await;
    let start = state
        .submit_input(
            turn_prompt("@reviewer remember this"),
            SubmissionIntent::Directed(DirectedDelegation {
                agent: "reviewer".into(),
                mode: None,
            }),
        )
        .await
        .unwrap();
    let SubmissionDisposition::Started { turn_id, .. } = start else {
        panic!("expected directed turn");
    };
    assert_eq!(state.directed_delegation.as_ref().unwrap().turn_id, turn_id);
    assert!(state.transcript.has_user_message());
    assert!(matches!(
        state
            .submit_input(
                turn_prompt("@reviewer again"),
                SubmissionIntent::Directed(DirectedDelegation {
                    agent: "reviewer".into(),
                    mode: None,
                }),
            )
            .await,
        Err(PhiTurnError::DirectedUnavailable)
    ));
    state.abort();
    assert!(state.directed_delegation.is_none());
}

#[tokio::test]
async fn directed_turn_without_agent_call_reports_failure_on_completion_or_stream_close() {
    for terminal_event in [true, false] {
        let mut state = test_client().await;
        let turn_id = "directed-turn";
        state.directed_delegation = Some(ActiveDirectedDelegation {
            turn_id: turn_id.into(),
            constraint: DirectedDelegation {
                agent: "reviewer".into(),
                mode: None,
            },
            admitted: Arc::new(OnceLock::new()),
            attempted: false,
            instance_id: None,
        });
        let (sender, receiver) = tokio::sync::mpsc::channel(2);
        if terminal_event {
            sender
                .send(crate::core::event::AgentEvent::RunFinished {
                    response: "Please clarify the worktree".into(),
                    usage: Default::default(),
                })
                .await
                .unwrap();
        }
        drop(sender);
        state.main_run = Some(EventPipeline::new(
            crate::harness::agentic_loop::AgentRunner {
                tool_policy: None,
                round_requests: None,
                event_rx: receiver,
                steering: Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                    QueuedSubmissionDrainMode::OneAtATime,
                )),
                task: tokio::spawn(async {}),
            },
            CoreEventScope {
                session_id: Some(state.transcript.id.clone()),
                turn_id: Some(turn_id.into()),
                ..Default::default()
            },
        ));
        let (events, failure) = state.drain_events().await.unwrap();
        assert!(failure.is_none());
        assert!(events.iter().any(|event| matches!(
            &event.event,
            CoreEvent::TurnFailed { failure } if failure.kind == crate::PhiFailureKind::Delegation
                && failure.message.contains("did not delegate")
        )));
        assert!(state.directed_delegation.is_none());
        assert!(
            matches!(state.transcript.entries.last().map(|entry| &entry.kind),
                Some(crate::transcript::TranscriptEntryKind::Failure { turn_id: stored_turn, failure })
                    if stored_turn.as_deref() == Some(turn_id)
                        && failure.kind == crate::PhiFailureKind::Delegation
                        && failure.message.contains("did not delegate")
            )
        );
        assert!(state.transcript.entries.iter().any(|entry| matches!(
            &entry.kind,
            crate::transcript::TranscriptEntryKind::Event { name, data }
                if name == "phi_sdk.directed_delegation_failed"
                    && data["turn_id"] == turn_id
        )));
    }
}

#[tokio::test]
async fn directed_mode_cannot_be_changed_or_omitted_by_model() {
    use crate::harness::manager::preparation::DelegationMode::{Background, Foreground};

    for constrained in [None, Some(Foreground), Some(Background)] {
        for requested in [None, Some(Foreground), Some(Background)] {
            let mut state = test_client().await;
            let _events = state.install_test_main_run("turn-1");
            state.directed_delegation = Some(ActiveDirectedDelegation {
                turn_id: "turn-1".into(),
                constraint: DirectedDelegation {
                    agent: "reviewer".into(),
                    mode: constrained,
                },
                admitted: Arc::new(OnceLock::new()),
                attempted: false,
                instance_id: None,
            });
            let mut request = AgentSpawnRequest::new("reviewer", "inspect");
            request.mode = requested;
            let (reply, result) = tokio::sync::oneshot::channel();
            state.start_agent_tool_request(AgentToolRequest {
                args: crate::agent::tools::AgentArgs::Create(request),
                scope: CoreEventScope {
                    session_id: Some(state.transcript.id.clone()),
                    turn_id: Some("turn-1".into()),
                    parent_tool_call_id: Some("mode-call".into()),
                    ..Default::default()
                },
                reply,
            });
            let error = result.await.unwrap().unwrap_err();
            let matches = match constrained {
                Some(mode) => requested == Some(mode),
                None => requested != Some(Foreground),
            };
            assert_eq!(error.contains("does not match"), !matches);
            assert_eq!(
                state.directed_delegation.as_ref().unwrap().attempted,
                matches
            );
            state.abort();
        }
    }
}

#[tokio::test]
async fn directed_agent_tool_rejects_mismatch_and_duplicate_without_spawning() {
    let mut state = test_client().await;
    let _events = state.install_test_main_run("turn-1");
    state.directed_delegation = Some(ActiveDirectedDelegation {
        turn_id: "turn-1".into(),
        constraint: DirectedDelegation {
            agent: "reviewer".into(),
            mode: None,
        },
        admitted: Arc::new(OnceLock::new()),
        attempted: false,
        instance_id: None,
    });
    for input in [
        serde_json::json!({"role": "other", "prompt": "inspect"}),
        serde_json::json!({"action": "message", "recipient": "retained", "content": "inspect"}),
        serde_json::json!({"action": "list"}),
        serde_json::json!({"role": "reviewer", "prompt": "inspect"}),
        serde_json::json!({"role": "reviewer", "prompt": "inspect"}),
    ] {
        let (reply, result) = tokio::sync::oneshot::channel();
        state.start_agent_tool_request(AgentToolRequest {
            args: serde_json::from_value(input.clone()).unwrap(),
            scope: CoreEventScope {
                session_id: Some(state.transcript.id.clone()),
                turn_id: Some("turn-1".into()),
                parent_tool_call_id: Some("directed-call".into()),
                ..Default::default()
            },
            reply,
        });
        assert!(result.await.unwrap().is_err());
        if input["role"] != "reviewer" {
            assert!(!state.directed_delegation.as_ref().unwrap().attempted);
        }
    }
    assert!(state.child_observations.is_empty());
    assert!(state.directed_delegation.as_ref().unwrap().attempted);
    assert!(
        state
            .directed_delegation
            .as_ref()
            .unwrap()
            .admitted
            .get()
            .is_none()
    );
    let (reply, result) = tokio::sync::oneshot::channel();
    state.start_agent_tool_request(AgentToolRequest {
        args: crate::agent::tools::AgentArgs::Create(AgentSpawnRequest::new(
            "reviewer",
            "nested task",
        )),
        scope: CoreEventScope {
            session_id: Some("child-session".into()),
            turn_id: Some("child-turn".into()),
            ..Default::default()
        },
        reply,
    });
    assert!(result.await.unwrap().is_err());
    assert!(state.child_observations.is_empty());
    state.abort();
}

#[tokio::test]
async fn agent_tool_rejects_forged_stale_and_missing_tool_scope() {
    let mut state = test_client().await;
    let scope = CoreEventScope {
        session_id: Some(state.transcript.id.clone()),
        turn_id: Some("active-turn".into()),
        parent_tool_call_id: Some("trusted-call".into()),
        ..Default::default()
    };
    let _events = state.install_test_main_run("active-turn");
    for field in ["session", "agent", "turn", "tool"] {
        let mut forged = scope.clone();
        match field {
            "session" => forged.session_id = Some("other-session".into()),
            "agent" => forged.agent_id = Some("other-agent".into()),
            "turn" => forged.turn_id = Some("old-turn".into()),
            "tool" => forged.parent_tool_call_id = None,
            _ => unreachable!(),
        }
        for input in [
            serde_json::json!({"role": "reviewer", "prompt": "task"}),
            serde_json::json!({"action": "message", "recipient": "retained", "content": "task"}),
            serde_json::json!({"action": "list"}),
        ] {
            let (reply, result) = tokio::sync::oneshot::channel();
            state.start_agent_tool_request(AgentToolRequest {
                args: serde_json::from_value(input.clone()).unwrap(),
                scope: forged.clone(),
                reply,
            });
            let error = result.await.unwrap().unwrap_err();
            assert!(
                error.contains("execution scope") || error.contains("tool-call identity"),
                "{field}: {input}: {error}"
            );
        }
    }
    assert!(state.child_observations.is_empty());
    state.abort();
}

#[tokio::test]
async fn submission_identity_survives_runner_handoff_until_canonical_commit() {
    let mut client = test_client().await;
    let event_tx = client.install_test_main_run("submission-turn");
    let mut ids = Vec::new();
    for dispatch in [
        SubmissionDispatch::Steer,
        SubmissionDispatch::Steer,
        SubmissionDispatch::FollowUp,
    ] {
        let SubmissionDisposition::Queued { submission_id, .. } = client
            .submit_input(
                turn_prompt("same prompt"),
                SubmissionIntent::Dispatch(dispatch),
            )
            .await
            .unwrap()
        else {
            panic!("expected queued submission")
        };
        ids.push(submission_id);
    }
    assert_ne!(ids[0], ids[1]);
    let event = crate::core::event::PhiEvent::SubmissionDiscarded {
        submission_id: ids[1].clone(),
        reason: crate::core::turn::SubmissionDiscardReason::DispatchFailed,
    };
    let wire = serde_json::to_value(&event).unwrap();
    assert_eq!(wire["type"], "submission_discarded");
    assert_eq!(wire["submission_id"], ids[1]);
    assert!(wire.get("input_id").is_none());
    assert!(client.queued_steer_submissions.is_empty());
    assert_eq!(client.pending_submission_ids(), ids);
    event_tx
        .send(crate::core::event::AgentEvent::UserInputInjected {
            submission_id: Some(ids[0].clone()),
            prompt: "same prompt".into(),
            source: UserPromptSubmitSource::Interactive,
            contexts: Vec::new(),
            content: vec![crate::transcript::UserContentBlock::Text {
                text: "same prompt".into(),
            }],
        })
        .await
        .unwrap();
    client.commit_ready_main_events();
    assert_eq!(client.pending_submission_ids(), ids[1..].to_vec());
    event_tx
        .send(crate::core::event::AgentEvent::SubmissionDiscarded {
            submission_id: ids[1].clone(),
        })
        .await
        .unwrap();
    client.commit_ready_main_events();
    assert_eq!(client.pending_submission_ids(), vec![ids[2].clone()]);
    client.abort();
    assert!(client.pending_submission_ids().is_empty());
}

#[tokio::test]
async fn late_steer_stays_in_the_steer_queue() {
    let mut client = test_client_with_agent_options(false).await;
    let steering = Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
        QueuedSubmissionDrainMode::OneAtATime,
    ));
    assert!(steering.drain_or_close().await.unwrap().is_empty());
    let event_tx = install_main_run(&mut client, steering);

    let mut late = turn_prompt("too late to steer");
    late.tool_policy = crate::core::turn::TurnToolPolicy {
        allowances: vec![("Read".into(), "**".into())],
        removals: vec!["Read".into()],
    };
    let intent = late.tool_policy.clone();
    let queued = client
        .submit_input(late, SubmissionIntent::Dispatch(SubmissionDispatch::Steer))
        .await
        .unwrap();

    assert!(matches!(
        queued,
        SubmissionDisposition::Queued {
            dispatch: SubmissionDispatch::Steer,
            ..
        }
    ));
    assert_eq!(client.queued_steer_submissions.len(), 1);
    assert_eq!(
        client
            .queued_steer_submissions
            .front()
            .unwrap()
            .prompt
            .tool_policy,
        intent
    );
    assert!(client.generation.runtime.is_tool_available("Read"));
    assert!(client.follow_up_submissions.is_empty());
    assert_eq!(client.pending_submission_ids().len(), 1);
    client
        .queued_steer_submissions
        .front_mut()
        .unwrap()
        .before_dispatch = Box::new(|| Err(anyhow::anyhow!("dispatch failed")));
    event_tx
        .send(crate::core::event::AgentEvent::RunFinished {
            response: "previous turn completed".into(),
            usage: Default::default(),
        })
        .await
        .unwrap();
    drop(event_tx);
    let submission_id = client.pending_submission_ids()[0].clone();
    let (events, failure) = client.drain_events().await.unwrap();
    assert!(failure.is_some());
    assert!(matches!(events.as_slice(), [finished, discarded]
        if matches!(&finished.event, CoreEvent::TurnFinished { response, .. }
            if response == "previous turn completed")
        && discarded.scope.turn_id.is_none()
        && matches!(&discarded.event,
            CoreEvent::SubmissionDiscarded { submission_id: id, reason }
                if id == &submission_id
                    && *reason == crate::core::turn::SubmissionDiscardReason::DispatchFailed)
    ));
    let (events, failure) = client.drain_events().await.unwrap();
    assert!(events.is_empty());
    assert!(failure.is_none());
    assert!(client.pending_submission_ids().is_empty());
    assert!(client.generation.runtime.is_tool_available("Read"));
    client.abort();
}

#[tokio::test]
async fn submission_capacity_bounds_admission_without_committing_prompts() {
    for dispatch in [SubmissionDispatch::Steer, SubmissionDispatch::FollowUp] {
        let mut state = test_client().await;
        let _events = state.install_test_main_run("held-turn");
        let mut accepted = std::collections::HashSet::new();
        for index in 0..super::MAX_QUEUED_SUBMISSIONS {
            let submitted = state
                .submit_input(
                    turn_prompt(&format!("queued-{index}")),
                    SubmissionIntent::Dispatch(dispatch),
                )
                .await
                .unwrap();
            let SubmissionDisposition::Queued { submission_id, .. } = submitted else {
                panic!("expected queued submission");
            };
            assert!(accepted.insert(submission_id));
        }
        assert!(state.transcript.entries.is_empty());
        for _ in 0..3 {
            assert!(matches!(
                state
                    .submit_input(turn_prompt("overflow"), SubmissionIntent::Dispatch(dispatch))
                    .await,
                Err(PhiTurnError::QueueFull { limit }) if limit == super::MAX_QUEUED_SUBMISSIONS
            ));
            assert_eq!(state.pending_submission_ids().len(), accepted.len());
            assert!(state.transcript.entries.is_empty());
        }
        state.abort();
        assert!(state.pending_submission_ids().is_empty());
        let _events = state.install_test_main_run("next-turn");
        assert!(matches!(
            state
                .submit_input(turn_prompt("fresh"), SubmissionIntent::Dispatch(dispatch))
                .await
                .unwrap(),
            SubmissionDisposition::Queued { .. }
        ));
        state.abort();
    }
}

#[tokio::test]
async fn queued_history_failure_discards_each_submission_once_without_a_turn() {
    for dispatch in [SubmissionDispatch::Steer, SubmissionDispatch::FollowUp] {
        let mut state = test_client().await;
        let steering = Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
            QueuedSubmissionDrainMode::OneAtATime,
        ));
        steering.drain_or_close().await.unwrap();
        install_main_run(&mut state, steering);
        let SubmissionDisposition::Queued { submission_id, .. } = state
            .submit_input(
                turn_prompt("queued input"),
                SubmissionIntent::Dispatch(dispatch),
            )
            .await
            .unwrap()
        else {
            panic!("expected queued input")
        };
        state.main_run = None;
        state.transcript.leaf_id = Some("missing-entry".into());
        let (events, failure) = state.drain_events().await.unwrap();
        assert!(failure.is_some());
        assert!(matches!(events.as_slice(), [discarded]
            if discarded.scope.turn_id.is_none()
                && matches!(&discarded.event, CoreEvent::SubmissionDiscarded {
                    submission_id: id,
                    reason: crate::core::turn::SubmissionDiscardReason::DispatchFailed,
                } if id == &submission_id)
        ));
        assert!(state.pending_submission_ids().is_empty());
        let (events, failure) = state.drain_events().await.unwrap();
        assert!(events.is_empty());
        assert!(failure.is_none());
        assert!(state.abort().is_empty());
    }
}

#[tokio::test]
async fn terminal_failure_history_is_committed_once_during_drain_or_abort() {
    use crate::core::event::AgentEvent;
    use crate::transcript::{AgentMessage, TranscriptEntryKind};

    enum Completion {
        Drain,
        Abort,
    }
    for completion in [Completion::Drain, Completion::Abort] {
        for partial_reply in [None, Some("partial reply")] {
            let mut client = test_client().await;
            let events = client.install_test_main_run("failed-turn");
            let failure =
                PhiFailure::new(PhiFailureKind::ProviderUnavailable, "connection lost", None);
            if let Some(text) = partial_reply {
                events
                    .try_send(AgentEvent::AssistantStarted {
                        id: "partial".into(),
                    })
                    .unwrap();
                events
                    .try_send(AgentEvent::AssistantDelta {
                        original: text.into(),
                        display: text.into(),
                    })
                    .unwrap();
            }
            events.try_send(AgentEvent::Error(failure.clone())).unwrap();
            drop(events);
            let published = match completion {
                Completion::Drain => client.drain_phi_events().await.unwrap().0,
                Completion::Abort => client.abort(),
            };
            assert_eq!(
                published
                    .iter()
                    .filter(|envelope| matches!(envelope.event, crate::PhiEvent::TurnFailed { .. }))
                    .count(),
                1
            );
            let entries = &client.transcript.entries;
            assert_eq!(entries.len(), 1 + usize::from(partial_reply.is_some()));
            assert!(matches!(&entries.last().unwrap().kind,
                TranscriptEntryKind::Failure { turn_id, failure: stored }
                    if turn_id.as_deref() == Some("failed-turn") && stored == &failure
            ));
            if partial_reply.is_some() {
                assert!(matches!(&entries[0].kind, TranscriptEntryKind::Message {
                    message: AgentMessage::Assistant { stop_reason, .. }
                } if stop_reason == "error"));
            }
            let entry_count = entries.len();
            assert!(client.drain_events().await.unwrap().0.is_empty());
            assert!(client.abort().is_empty());
            assert_eq!(client.transcript.entries.len(), entry_count);
        }
    }
}

#[tokio::test]
async fn abort_preserves_generated_assistant_content_once() {
    use crate::core::event::AgentEvent;
    use crate::transcript::{AgentMessage, ContentBlock, ImageSource, TranscriptEntryKind};

    let mut client = test_client().await;
    let events = client.install_test_main_run("turn-partial");
    let image = ImageSource::Url {
        url: "https://example.com/generated.png".into(),
        mime_type: Some("image/png".into()),
    };
    for event in [
        AgentEvent::AssistantStarted {
            id: "assistant-partial".into(),
        },
        AgentEvent::Reasoning("partial reasoning".into()),
        AgentEvent::AssistantDelta {
            original: "before image".into(),
            display: "before image".into(),
        },
        AgentEvent::AssistantImage { source: image },
    ] {
        events.try_send(event).unwrap();
    }
    client.drain_events().await.unwrap();
    events
        .try_send(AgentEvent::AssistantDelta {
            original: "after image".into(),
            display: "after image".into(),
        })
        .unwrap();

    let completion = client.abort();
    assert!(client.abort().is_empty());
    assert!(matches!(completion.as_slice(), [delta, finished]
        if matches!(&delta.event, crate::PhiEvent::AssistantDelta { original, .. } if original == "after image")
        && matches!(&finished.event, crate::PhiEvent::AssistantFinished { id, content, .. }
            if id == "assistant-partial" && content.len() == 4)
    ));

    assert_eq!(client.transcript.entries.len(), 1);
    let TranscriptEntryKind::Message {
        message:
            AgentMessage::Assistant {
                id,
                turn_id,
                content,
                stop_reason,
                ..
            },
    } = &client.transcript.entries[0].kind
    else {
        panic!("expected an aborted Assistant message");
    };
    assert_eq!(id.as_deref(), None);
    assert_eq!(turn_id.as_deref(), Some("turn-partial"));
    assert_eq!(stop_reason, "aborted");
    assert!(matches!(content.as_slice(), [
        ContentBlock::Thinking { text: reasoning, .. },
        ContentBlock::Text { text: before },
        ContentBlock::Image { source: ImageSource::Url { url, .. } },
        ContentBlock::Text { text: after },
    ] if reasoning.as_str() == "partial reasoning" && before.as_str() == "before image" && url == "https://example.com/generated.png" && after.as_str() == "after image"));
}

#[tokio::test]
async fn abort_handles_empty_and_reasoning_only_messages() {
    use crate::core::event::AgentEvent;
    use crate::transcript::{AgentMessage, ContentBlock, TranscriptEntryKind};

    for reasoning in ["", "still thinking"] {
        let mut client = test_client().await;
        let events = client.install_test_main_run("turn");
        events
            .try_send(AgentEvent::AssistantStarted {
                id: "assistant".into(),
            })
            .unwrap();
        if !reasoning.is_empty() {
            events
                .try_send(AgentEvent::Reasoning(reasoning.into()))
                .unwrap();
        }
        client.abort();

        assert_eq!(
            client.transcript.entries.len(),
            usize::from(!reasoning.is_empty())
        );
        if !reasoning.is_empty() {
            assert!(matches!(&client.transcript.entries[0].kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::Assistant { content, stop_reason, .. }
                } if stop_reason == "aborted"
                    && matches!(content.as_slice(), [ContentBlock::Thinking { text, .. }]
                        if text.as_str() == reasoning)
            ));
        }
    }
}

#[tokio::test]
async fn abort_commits_queued_tool_call_before_stopping_run() {
    let mut client = test_client().await;
    let event_tx = client.install_test_main_run("turn-1");
    for event in [
        crate::core::event::AgentEvent::AssistantStarted {
            id: "assistant-1".into(),
        },
        crate::core::event::AgentEvent::ToolCall {
            id: "tool-1".into(),
            provider_call_id: Some("tool-1".into()),
            provider_item_id: None,
            name: "read".into(),
            args: serde_json::json!({"path": "README.md"}),
        },
        crate::core::event::AgentEvent::AssistantFinished {
            id: "assistant-1".into(),
            provider_message_id: None,
            turn_id: "turn-1".into(),
            provider: "provider".into(),
            api: None,
            model: "model".into(),
            content: Vec::new(),
            response: "".into(),
            display: "".into(),
            usage: crate::transcript::Usage::default(),
            stop_reason: "tool_use".into(),
            has_tool_calls: true,
        },
    ] {
        event_tx.try_send(event).unwrap();
    }
    client.abort();

    assert_eq!(client.transcript().entries.len(), 1);
    assert!(client.transcript().entries.iter().any(|entry| matches!(
        &entry.kind,
        crate::transcript::TranscriptEntryKind::Message {
            message: crate::transcript::AgentMessage::Assistant { content, .. }
        } if content.iter().any(|block| matches!(
            block,
            crate::transcript::ContentBlock::ToolCall { name, .. } if name == "read"
        ))
    )));
}

#[tokio::test]
async fn queued_dispatch_prioritizes_steer_and_runs_follow_up() {
    let mut client = test_client().await;
    install_main_run(
        &mut client,
        Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
            QueuedSubmissionDrainMode::OneAtATime,
        )),
    );

    for (prompt, dispatch) in [
        ("later", SubmissionDispatch::FollowUp),
        ("change direction", SubmissionDispatch::Steer),
    ] {
        client
            .submit_input(turn_prompt(prompt), SubmissionIntent::Dispatch(dispatch))
            .await
            .unwrap();
    }

    client.drain_events().await.unwrap();
    let (events, failure) = client.drain_events().await.unwrap();
    assert!(failure.is_none());
    assert!(matches!(
        events.first().map(|event| &event.event),
        Some(CoreEvent::TurnStarted { prompt, .. }) if prompt == "change direction"
    ));
    assert_eq!(client.follow_up_submissions.len(), 1);
    client.abort();

    let mut follow_up_client = test_client().await;
    install_main_run(
        &mut follow_up_client,
        Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
            QueuedSubmissionDrainMode::OneAtATime,
        )),
    );
    let queued = follow_up_client
        .submit_input(
            turn_prompt("afterwards"),
            SubmissionIntent::Dispatch(SubmissionDispatch::FollowUp),
        )
        .await
        .unwrap();
    assert!(matches!(queued, SubmissionDisposition::Queued { .. }));
    assert!(!follow_up_client.transcript().has_user_message());
    follow_up_client.drain_events().await.unwrap();
    assert!(follow_up_client.transcript().has_user_message());
    assert!(follow_up_client.pending_submission_ids().is_empty());
    follow_up_client.abort();
}

#[tokio::test]
async fn text_only_model_rejects_image_before_turn_mutation() {
    let mut client = test_client().await;
    client.generation.descriptor.input = vec![crate::models::ModelInput::Text];

    let mut input = turn_prompt("inspect");
    input.images.push(crate::core::turn::ImageInput::url(
        "https://example.com/image.png",
    ));
    let result = client
        .submit_input(input, SubmissionIntent::Immediate)
        .await;
    let Err(error) = result else {
        panic!("text-only model accepted image input");
    };

    assert!(matches!(
        error,
        PhiTurnError::UnsupportedImageInput { provider, model }
            if provider == "openai-codex" && model == "gpt-5.6-sol"
    ));
    assert!(!client.transcript().has_user_message());
    assert!(client.is_idle());
}

#[test]
fn agent_prompt_records_an_agent_entry() {
    let mut transcript = Transcript::new();
    transcript.append_user_message("@@reviewer inspect this");
    let turn_prompt = PreparedTurnPrompt {
        hook_notices: Vec::new(),
        submission_id: None,
        tool_policy: Default::default(),
        prompt: "internal follow-up".to_string(),
        source: UserPromptSubmitSource::Internal,
        contexts: Vec::new(),
        user_content: vec![UserContentBlock::Text {
            text: "internal follow-up".into(),
        }],
    };

    record_prepared_prompt(&mut transcript, &turn_prompt, TranscriptPromptRole::Agent);

    assert_eq!(transcript.current_branch().len(), 2);
    let crate::transcript::TranscriptEntryKind::Message { message } =
        &transcript.current_branch()[1].kind
    else {
        panic!("missing agent message");
    };
    assert!(matches!(
        message,
        crate::transcript::AgentMessage::Agent { .. }
    ));
    assert_eq!(serde_json::to_value(message).unwrap()["role"], "agent");
}

#[tokio::test]
async fn immediate_busy_input_is_rejected_without_a_hook_receipt() {
    let mut state = test_client().await;
    assert!(matches!(
        state
            .submit_input(turn_prompt("first"), SubmissionIntent::Immediate)
            .await
            .unwrap(),
        SubmissionDisposition::Started { .. }
    ));
    let before = state.transcript.entries.len();
    assert!(matches!(
        state
            .submit_input(turn_prompt("busy"), SubmissionIntent::Immediate)
            .await,
        Err(PhiTurnError::Busy)
    ));
    assert_eq!(state.transcript.entries.len(), before);
    assert!(state.follow_up_submissions.is_empty());
    state.abort();
}
