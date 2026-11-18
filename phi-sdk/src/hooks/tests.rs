use super::*;
use crate::context::InjectedContext;
use crate::transcript::{ContextSource, Transcript};
use std::path::PathBuf;

mod trigger_registration_tests {
    use super::*;

    fn contribution() -> PromptContextOutcome {
        PromptContextOutcome {
            contexts: vec![InjectedContext {
                source: ContextSource::Hook,
                content: "context".into(),
            }],
            notices: vec![HookNotice::Info {
                message: "handler output".into(),
            }],
        }
    }

    fn event() -> PromptContextEvent {
        PromptContextEvent {
            original_prompt: "input".into(),
            prompt: "input".into(),
            source: UserPromptSubmitSource::Sdk,
            dispatch: UserPromptDispatch::Immediate,
            transcript: Arc::new(Transcript::new()),
            working_dir: PathBuf::from("/tmp"),
            delegated_child: false,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn once_consumes_every_started_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for result in ["empty", "context", "error", "timeout", "recovery"] {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut hooks = HookRegistry::new();
            hooks.register_hook(
                Hook::prompt_context_with_recovery(
                    {
                        let calls = calls.clone();
                        move |_| {
                            calls.fetch_add(1, Ordering::SeqCst);
                            async move {
                                match result {
                                    "error" | "recovery" => Err(HookError::new("failed")),
                                    "timeout" => std::future::pending().await,
                                    "context" => Ok(contribution()),
                                    _ => Ok(PromptContextOutcome::default()),
                                }
                            }
                        }
                    },
                    move |_, error| {
                        if result == "recovery" {
                            Ok(contribution())
                        } else {
                            Err(error)
                        }
                    },
                )
                .with_once(true)
                .when(|event: &PromptContextEvent| event.prompt != "filtered"),
            );
            let mut filtered = event();
            filtered.prompt = "filtered".into();
            assert!(
                hooks
                    .prepare_context(filtered)
                    .await
                    .unwrap()
                    .contexts
                    .is_empty()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let first = hooks.prepare_context(event()).await;
            assert_eq!(first.is_err(), matches!(result, "error" | "timeout"));
            // Discard output instead of acknowledging admission.
            drop(first);
            let mut reset = event();
            Arc::make_mut(&mut reset.transcript)
                .reset_context(None)
                .unwrap();
            assert!(
                hooks
                    .prepare_context(reset)
                    .await
                    .unwrap()
                    .contexts
                    .is_empty()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn once_needs_no_identity_and_sibling_registrations_are_independent() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let hook = Hook::prompt_context({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(PromptContextOutcome::default()) }
            }
        })
        .with_once(true);
        let mut hooks = HookRegistry::new();
        hooks.register_hook(hook.clone());
        hooks.register_hook(hook);
        hooks.validate_registrations().unwrap();
        hooks.prepare_context(event()).await.unwrap();
        let next = hooks.for_generation();
        next.continue_bindings(&hooks);
        next.prepare_context(event()).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_boundaries_start_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let mut hooks = HookRegistry::new();
        hooks.register_hook(
            Hook::prompt_context({
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async {
                        tokio::task::yield_now().await;
                        Ok(contribution())
                    }
                }
            })
            .with_once(true),
        );
        let tasks = (0..32).map(|_| {
            let hooks = hooks.clone();
            tokio::spawn(
                async move { hooks.prepare_context(event()).await.unwrap().contexts.len() },
            )
        });
        let results = join_all(tasks).await;
        assert_eq!(results.into_iter().map(Result::unwrap).sum::<usize>(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_consumes_only_started_work() {
        let started = Arc::new(tokio::sync::Notify::new());
        let mut hooks = HookRegistry::new();
        hooks.register_hook(
            Hook::prompt_context({
                let started = started.clone();
                move |_| {
                    started.notify_one();
                    std::future::pending::<HookResult<PromptContextOutcome>>()
                }
            })
            .with_once(true),
        );
        drop(hooks.prepare_context(event()));
        let task = tokio::spawn({
            let hooks = hooks.clone();
            async move { hooks.prepare_context(event()).await }
        });
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(
            hooks
                .prepare_context(event())
                .await
                .unwrap()
                .contexts
                .is_empty()
        );
        let independent = hooks.for_generation();
        let task = tokio::spawn(async move { independent.prepare_context(event()).await });
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
}

mod control_policy_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn tool(name: &str, command: &str) -> ToolUseEvent {
        ToolUseEvent {
            tool_name: name.into(),
            tool_use_id: "call".into(),
            tool_input: serde_json::json!({"command": command}),
        }
    }

    fn registration(calls: Arc<AtomicUsize>, scope: &str) -> Hook {
        Hook::pre_tool_use(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                Ok(PreToolUseOutcome::Deny {
                    reason: "blocked".into(),
                })
            }
        })
        .with_once(true)
        .with_id(scope)
        .when(|event: &ToolUseEvent| event.tool_input["command"] == "run")
        .with_matcher(HookMatcher::exact("Read"))
    }

    #[tokio::test]
    async fn tool_selection_and_live_lifetimes_share_only_continuing_allowances() {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut original = HookRegistry::new();
        original.register_hook(registration(calls.clone(), "activation"));
        original.validate_registrations().unwrap();
        assert!(
            emit_pre_tool_use(&original, tool("ReadMore", "run"))
                .await
                .permission_decision
                .is_none()
        );
        assert!(
            emit_pre_tool_use(&original, tool("Read", "skip"))
                .await
                .permission_decision
                .is_none()
        );
        let next = original.for_generation();
        next.continue_bindings(&original);
        // Work retaining the old generation can start after acceptance.
        assert_eq!(
            emit_pre_tool_use(&original, tool("Read", "run"))
                .await
                .permission_decision,
            Some(PermissionDecision::Deny)
        );
        assert!(
            emit_pre_tool_use(&next, tool("Read", "run"))
                .await
                .permission_decision
                .is_none()
        );
        let independent = original.for_generation();
        emit_pre_tool_use(&independent, tool("Read", "run")).await;
        let mut fresh_scope = HookRegistry::new();
        fresh_scope.register_hook(registration(calls.clone(), "new-activation"));
        fresh_scope.continue_bindings(&next);
        emit_pre_tool_use(&fresh_scope, tool("Read", "run")).await;
        let removed = HookRegistry::new();
        removed.continue_bindings(&fresh_scope);
        let mut readded = HookRegistry::new();
        readded.register_hook(registration(calls.clone(), "new-activation"));
        readded.continue_bindings(&removed);
        emit_pre_tool_use(&readded, tool("Read", "run")).await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]
        #[test]
        fn matches_and_reloads_never_rearm_a_live_once(actions in proptest::collection::vec(0_u8..5, 1..32)) {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let mut hooks = HookRegistry::new();
            let mut lifetime = 0;
            hooks.register_hook(registration(calls.clone(), "0"));
            let mut consumed = false;
            let mut expected_calls = 0;
            for action in actions {
                match action {
                    0 => {
                        runtime.block_on(emit_pre_tool_use(&hooks, tool("Read", "run")));
                        if !consumed { expected_calls += 1; consumed = true; }
                    }
                    1 => { runtime.block_on(emit_pre_tool_use(&hooks, tool("ReadMore", "run"))); }
                    2 => { drop(hooks.for_generation()); }
                    3 => {
                        let next = hooks.for_generation();
                        next.continue_bindings(&hooks);
                        hooks = next;
                    }
                    _ => {
                        lifetime += 1;
                        let mut next = HookRegistry::new();
                        next.register_hook(registration(calls.clone(), &lifetime.to_string()));
                        next.continue_bindings(&hooks);
                        hooks = next;
                        consumed = false;
                    }
                }
                proptest::prop_assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
            }
        }
    }

    #[test]
    fn invalid_event_policies_fail_factory_validation() {
        let noop = || Hook::pre_tool_use(|_| async { Ok(PreToolUseOutcome::Continue) });
        for hooks in [
            vec![noop().when(|_: &NotificationEvent| true)],
            vec![noop().with_id("")],
            vec![noop().with_timeout(Duration::ZERO)],
            vec![
                Hook::stop(|_| async { Ok(CompletionOutcome::Allow) })
                    .with_matcher(HookMatcher::exact("Read")),
            ],
            vec![noop().with_id("duplicate"), noop().with_id("duplicate")],
        ] {
            let result = crate::Phi::builder()
                .provider("openai")
                .model("test")
                .context(crate::context::load(true))
                .hooks(hooks)
                .build();
            assert!(matches!(result, Err(crate::PhiBuildError::InvalidHook(_))));
        }
    }

    #[tokio::test]
    async fn deferred_and_background_tasks_consume_once_inside_the_task() {
        let starts = Arc::new(AtomicUsize::new(0));
        let notifications = Arc::new(AtomicUsize::new(0));
        let mut hooks = HookRegistry::new();
        hooks.register_hook(
            Hook::session_start_with_mode(SessionStartMode::BeforeFirstPrompt, {
                let starts = starts.clone();
                move |_| {
                    starts.fetch_add(1, Ordering::SeqCst);
                    async { Ok(Vec::new()) }
                }
            })
            .with_once(true)
            .with_matcher(HookMatcher::exact("reload")),
        );
        hooks.register_hook(
            Hook::notification({
                let notifications = notifications.clone();
                move |_| {
                    notifications.fetch_add(1, Ordering::SeqCst);
                    async { Ok(()) }
                }
            })
            .with_once(true),
        );
        hooks.validate_registrations().unwrap();
        emit_session_start(&hooks, SessionStartReason::New, None).await;
        hooks.ready().await;
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            emit_session_start(&hooks, SessionStartReason::Reload, None).await;
            hooks.ready().await;
        }
        for _ in 0..8 {
            notify_completion(&hooks, "done");
        }
        hooks.flush_notifications().await;
        let next = hooks.for_generation();
        next.continue_bindings(&hooks);
        notify_completion(&next, "again");
        next.flush_notifications().await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lua_groups_filter_before_frequency_and_keep_siblings_running() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-hook-control-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("control.lua");
        std::fs::write(&source, r#"
on("PreToolUse", { matcher = "Read", hooks = {
  { once = true, ["if"] = { field = "tool_input.command", equals = "run" },
    handler = function(event, ctx) ctx.ui.notify("attempt"); error("failed") end },
  { handler = function(event) assert(event.payload.tool_input.command == "run" or event.payload.tool_input.command == "skip")
    return { updated_input = { command = "sibling" } } end }
}})
on("command_before", { once = true, matcher = "inspect",
  handler = function() return { block = true, reason = "application gate" } end })
on("Notification", { once = true,
  handler = function() return { block = true, reason = "invalid gate" } end })
"#).unwrap();
        let runtime = Arc::new(
            crate::extension::ExtensionRuntime::try_load_with_paths(vec![source.clone()]).unwrap(),
        );
        let mut hooks = HookRegistry::new();
        runtime.register_hooks(&mut hooks);
        hooks.validate_registrations().unwrap();
        emit_pre_tool_use(&hooks, tool("ReadMore", "run")).await;
        emit_pre_tool_use(&hooks, tool("Read", "skip")).await;
        assert!(runtime.take_notifications().unwrap().is_empty());
        let first = emit_pre_tool_use(&hooks, tool("Read", "run")).await;
        assert_eq!(first.permission_decision, Some(PermissionDecision::Deny));
        assert_eq!(first.updated_input.unwrap()["command"], "sibling");
        let candidate = Arc::new(
            crate::extension::ExtensionRuntime::try_load_with_paths(vec![source]).unwrap(),
        );
        let mut next = HookRegistry::new();
        candidate.register_hooks(&mut next);
        assert_eq!(runtime.before_command("inspect-more").await, None);
        assert_eq!(
            runtime.before_command("inspect").await.as_deref(),
            Some("application gate")
        );
        next.continue_bindings(&hooks);
        assert_eq!(candidate.before_command("inspect").await, None);
        assert!(
            emit_pre_tool_use(&next, tool("Read", "run"))
                .await
                .permission_decision
                .is_none()
        );
        assert_eq!(runtime.take_notifications().unwrap(), ["attempt"]);
        assert!(candidate.take_notifications().unwrap().is_empty());
        notify_completion(&hooks, "done");
        hooks.flush_notifications().await;
        assert!(hooks.take_notices().iter().any(|notice| matches!(notice, HookNotice::Warning { message } if message.contains("Notification cannot control execution"))));
        notify_completion(&next, "again");
        next.flush_notifications().await;
        assert!(next.take_notices().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

mod independent_dispatch_tests {
    use super::*;
    use proptest::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DecisionHandler(PermissionDecision);

    impl HookHandler for DecisionHandler {
        fn handle<'a>(&'a self, event: &'a LifecycleEvent) -> HookHandlerFuture<'a> {
            Box::pin(async move {
                assert_eq!(
                    event.payload["tool_input"],
                    serde_json::json!({"original": true})
                );
                Ok(Some(lifecycle_action(HookAction::PreToolUse(
                    PreToolUseResult {
                        permission_decision: Some(self.0),
                        reason: Some(format!("{:?}", self.0)),
                        additional_context: vec!["checked".into()],
                        ..Default::default()
                    },
                ))))
            })
        }
    }

    proptest! {
        #[test]
        fn permission_reduction_is_order_independent(decisions in prop::collection::vec(0_u8..4, 0..12)) {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            let choices = [PermissionDecision::Allow, PermissionDecision::Ask, PermissionDecision::Defer, PermissionDecision::Deny];
            let expected = decisions.iter().max().map(|index| choices[*index as usize]);
            for reverse in [false, true] {
                let mut decisions = decisions.clone();
                if reverse { decisions.reverse(); }
                let mut hooks = HookRegistry::new();
                for index in &decisions { hooks.register_handler(Arc::new(DecisionHandler(choices[*index as usize]))); }
                let result = runtime.block_on(emit_pre_tool_use(&hooks, ToolUseEvent {
                    tool_name: "test".into(), tool_use_id: "call".into(), tool_input: serde_json::json!({"original": true}),
                }));
                prop_assert_eq!(result.permission_decision, expected);
                prop_assert_eq!(result.additional_context.len(), decisions.len());
                prop_assert_eq!(result.reason, expected.map(|decision| format!("{decision:?}")));
            }
        }
    }

    #[tokio::test]
    async fn submission_hooks_share_input_and_run_concurrently() {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut hooks = HookRegistry::new();
        for index in 0..2 {
            let barrier = barrier.clone();
            hooks.register_hook(Hook::user_prompt_submit(move |event| {
                let barrier = barrier.clone();
                async move {
                    assert_eq!(event.prompt, "original");
                    barrier.wait().await;
                    Ok(UserPromptSubmitOutcome::Continue {
                        notices: Vec::new(),
                        prompt: format!("rewrite-{index}"),
                        injected_context: Vec::new(),
                    })
                }
            }));
        }
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            emit_user_prompt_submit(
                &hooks,
                UserPromptSubmitEvent {
                    delegated_child: false,
                    original_prompt: "original".into(),
                    prompt: "original".into(),
                    source: UserPromptSubmitSource::Sdk,
                    dispatch: UserPromptDispatch::Immediate,
                },
            ),
        )
        .await
        .expect("hooks must run concurrently");
        assert!(
            matches!(result, UserPromptSubmitAction::Continue { prompt, .. } if prompt == "rewrite-1")
        );
    }

    #[tokio::test]
    async fn completion_rejects_invalid_individual_and_merged_feedback() {
        const FEEDBACK_LIMIT: usize = 16 * 1024;
        for feedback in [
            vec![String::new(), "valid".into()],
            vec![" ".into(), "valid".into()],
            vec!["x".repeat(FEEDBACK_LIMIT + 1), "valid".into()],
            vec!["x".repeat(FEEDBACK_LIMIT / 2); 2],
        ] {
            let mut hooks = HookRegistry::new();
            for feedback in feedback {
                hooks.register_hook(Hook::stop(move |_| {
                    let feedback = feedback.clone();
                    async move { Ok(CompletionOutcome::Continue { feedback }) }
                }));
            }
            assert!(
                hooks
                    .completion_check(&CompletionEvent {
                        transcript: Transcript::new(),
                        response: "candidate".into(),
                        continuations: 0,
                    })
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn completion_merges_all_checks_and_failure_wins() {
        for (last, expected) in [
            (
                CompletionOutcome::Allow,
                CompletionOutcome::Continue {
                    feedback: "first".into(),
                },
            ),
            (
                CompletionOutcome::Continue {
                    feedback: "second".into(),
                },
                CompletionOutcome::Continue {
                    feedback: "first\nsecond".into(),
                },
            ),
            (
                CompletionOutcome::Fail {
                    reason: "failed".into(),
                },
                CompletionOutcome::Fail {
                    reason: "failed".into(),
                },
            ),
        ] {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut hooks = HookRegistry::new();
            hooks.register_hook(Hook::stop(|_| async {
                Ok(CompletionOutcome::Continue {
                    feedback: "first".into(),
                })
            }));
            let observed = calls.clone();
            hooks.register_hook(Hook::stop(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                let outcome = last.clone();
                async move { Ok(outcome) }
            }));
            let outcome = hooks
                .completion_check(&CompletionEvent {
                    transcript: Transcript::new(),
                    response: "candidate".into(),
                    continuations: 0,
                })
                .await
                .unwrap();
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(outcome, expected);
        }
    }
}

mod registration_tests {
    use super::*;
    use crate::sandbox::Sandbox;

    #[tokio::test]
    async fn rust_commands_and_lua_share_original_inputs() {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-hook-order-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("hooks.lua");
        std::fs::write(
            &source,
            r#"
on("PreToolUse", function(event)
  assert(event.payload.tool_input.command == "original")
  return { updated_input = { command = "lua" }, additional_context = "lua-context" }
end)
on("PostToolUseFailure", function(event)
  assert(event.payload.is_interrupt == true)
  assert(event.payload.error == "original-error")
  return { updated_output = "lua-error" }
end)
on("UserPromptExpansion", function(event)
  assert(event.payload.command_name == "review")
  assert(event.payload.prompt == "/review main")
  return { hookSpecificOutput = { additionalContext = "lua-expansion" } }
end)
"#,
        )
        .unwrap();
        let mut bus = HookRegistry::new();
        bus.register_hook(Hook::pre_tool_use(|_| async {
            Ok(PreToolUseOutcome::Rewrite {
                tool_input: serde_json::json!({"command":"rust"}),
            })
        }));
        bus.register_hook(Hook::post_tool_failure(|event| async move {
            assert_eq!(event.error, "original-error");
            assert!(event.is_interrupt);
            Ok(PostToolFailureOutcome::Rewrite {
                error: "redacted".into(),
            })
        }));
        bus.register_hook(Hook::user_prompt_expansion(|_| async {
            Ok(UserPromptExpansionOutcome::AddContext {
                contexts: vec![InjectedContext {
                    source: ContextSource::Hook,
                    content: "rust-expansion".into(),
                }],
            })
        }));
        let config: CommandHooks = serde_json::from_value(serde_json::json!({
            "PreToolUse":[{"hooks":[{
                "type":"command", "command":"printf '%s' '{\"updatedInput\":{\"command\":\"command\"},\"additionalContext\":\"command-context\"}'"
            }]}],
            "UserPromptExpansion": [{"matcher": {"regex": "^review$"}, "hooks": [{
                "type": "command", "command": "printf '%s' '{\"hookSpecificOutput\":{\"additionalContext\":\"command-expansion\"}}'"
            }]}, {"matcher": "other", "hooks": [{"type": "command", "command": "exit 2"}]}]
        })).unwrap();
        config
            .register(&mut bus, Sandbox::new(false, &root).unwrap())
            .unwrap();
        Arc::new(crate::extension::ExtensionRuntime::try_load_with_paths(vec![source]).unwrap())
            .register_hooks(&mut bus);
        bus.register_hook(Hook::pre_tool_use(|event| async move {
            assert_eq!(event.tool_input["command"], "original");
            Ok(PreToolUseOutcome::Deny {
                reason: "final-check".into(),
            })
        }));
        bus.register_hook(Hook::pre_tool_use(|_| async {
            Ok(PreToolUseOutcome::Continue)
        }));
        let result = emit_pre_tool_use(
            &bus,
            ToolUseEvent {
                tool_name: "Bash".into(),
                tool_use_id: "call".into(),
                tool_input: serde_json::json!({"command":"original"}),
            },
        )
        .await;
        assert_eq!(result.permission_decision, Some(PermissionDecision::Deny));
        assert_eq!(result.updated_input.unwrap()["command"], "lua");
        assert_eq!(
            result.additional_context,
            ["command-context", "lua-context"]
        );
        let failure = emit_post_tool_use_failure(
            &bus,
            ToolFailureEvent {
                tool_name: "Bash".into(),
                tool_use_id: "failed".into(),
                tool_input: Value::Null,
                error: "original-error".into(),
                is_interrupt: true,
                duration_ms: None,
            },
        )
        .await;
        assert_eq!(
            failure.updated_output,
            Some(Value::String("lua-error".into()))
        );
        let expansion = emit_prompt_expansion(
            &bus,
            &UserPromptExpansionEvent {
                prompt: "/review main".into(),
                command_name: "review".into(),
                command_args: "main".into(),
                command_source: "test".into(),
            },
        )
        .await
        .unwrap();
        let UserPromptExpansionOutcome::AddContext { contexts } = expansion else {
            panic!("expected merged expansion contexts");
        };
        assert_eq!(
            contexts
                .iter()
                .map(|context| context.content.as_str())
                .collect::<Vec<_>>(),
            ["rust-expansion", "command-expansion", "lua-expansion"]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn triggered_results_keep_order() {
        let root =
            PathBuf::from("/tmp").join(format!("phi-context-order-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("context.lua");
        std::fs::write(
            &source,
            r#"
on("PromptContext", function(event)
  assert(event.payload.memory_accepted == nil)
  return { contexts = {{ source = "hook", content = "lua" }},
    notices = {{ kind = "info", message = "lua" }} }
end)
"#,
        )
        .unwrap();
        let mut hooks = HookRegistry::new();
        hooks.register_hook(
            Hook::prompt_context(|_| async move {
                Ok(PromptContextOutcome {
                    contexts: vec![InjectedContext {
                        source: ContextSource::Hook,
                        content: "rust".into(),
                    }],
                    notices: vec![HookNotice::Info {
                        message: "rust".into(),
                    }],
                })
            })
            .with_once(true),
        );
        Arc::new(crate::extension::ExtensionRuntime::try_load_with_paths(vec![source]).unwrap())
            .register_hooks(&mut hooks);
        let memory_source = crate::extension::memory::MemoryContextKind::Graph.source();
        hooks.register_hook(Hook::prompt_context({
            let memory_source = memory_source.clone();
            move |_| {
                let memory_source = memory_source.clone();
                async move {
                    Ok(PromptContextOutcome {
                        contexts: vec![InjectedContext {
                            source: memory_source.clone(),
                            content: "last".into(),
                        }],
                        notices: vec![HookNotice::ContextInjected {
                            source: memory_source,
                            item_count: 2,
                            requested: false,
                        }],
                    })
                }
            }
        }));
        let transcript = Transcript::new();
        let contributions = hooks
            .prepare_context(PromptContextEvent {
                original_prompt: "input".into(),
                prompt: "input".into(),
                source: UserPromptSubmitSource::Sdk,
                dispatch: UserPromptDispatch::Immediate,
                transcript: Arc::new(transcript.clone()),
                working_dir: root.clone(),
                delegated_child: false,
            })
            .await
            .unwrap();
        let contexts = contributions.contexts;
        assert_eq!(
            contexts
                .iter()
                .map(|context| context.content.as_str())
                .collect::<Vec<_>>(),
            ["rust", "lua", "last"]
        );
        assert_eq!(contexts[2].source, memory_source);
        assert!(hooks.take_notices().is_empty());
        hooks.record_notices(contributions.notices);
        assert_eq!(
            hooks.take_notices(),
            [
                HookNotice::Info {
                    message: "rust".into()
                },
                HookNotice::Info {
                    message: "lua".into()
                },
                HookNotice::ContextInjected {
                    source: memory_source,
                    item_count: 2,
                    requested: false,
                },
            ]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn lua_rewrites_preserve_tool_payload_keys() {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-hook-payload-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("rewrite.lua");
        std::fs::write(
            &source,
            r#"
on("PreToolUse", function(event)
  return { hookSpecificOutput = {
    updatedInput = event.payload.tool_input,
    additionalContext = "checked",
  } }
end)
"#,
        )
        .unwrap();
        let mut bus = HookRegistry::new();
        Arc::new(crate::extension::ExtensionRuntime::try_load_with_paths(vec![source]).unwrap())
            .register_hooks(&mut bus);
        let input = serde_json::json!({
            "updatedInput": {"permissionDecision": "application-data"},
            "additionalContext": "application-context",
            "items": [{"permissionDecisionReason": "application-reason"}]
        });
        let result = emit_pre_tool_use(
            &bus,
            ToolUseEvent {
                tool_name: "Custom".into(),
                tool_use_id: "call".into(),
                tool_input: input.clone(),
            },
        )
        .await;
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(result.updated_input, Some(input));
        assert_eq!(result.additional_context, ["checked"]);
    }

    #[tokio::test]
    async fn lua_stop_is_a_completion_gate_not_a_second_end_callback() {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-stop-gate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("stop.lua");
        std::fs::write(
            &source,
            r#"
local calls = 0
on("Stop", function(event)
  calls = calls + 1
  if calls == 1 then return { block = true, reason = "check again" } end
  assert(calls == 2)
end)
"#,
        )
        .unwrap();
        let mut bus = HookRegistry::new();
        Arc::new(crate::extension::ExtensionRuntime::try_load_with_paths(vec![source]).unwrap())
            .register_hooks(&mut bus);
        let mut event = CompletionEvent {
            transcript: Transcript::new(),
            response: "candidate".into(),
            continuations: 0,
        };
        assert_eq!(
            bus.completion_check(&event).await.unwrap(),
            CompletionOutcome::Continue {
                feedback: "check again".into()
            }
        );
        event.continuations = 1;
        assert_eq!(
            bus.completion_check(&event).await.unwrap(),
            CompletionOutcome::Allow
        );
        notify_completion(&bus, "accepted");
        bus.flush_notifications().await;
        assert!(bus.take_notices().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn slow_notifications_do_not_starve_session_end_hooks() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut bus = HookRegistry::new();
        bus.register_hook(Hook::notification({
            let entered = entered.clone();
            move |_| {
                let entered = entered.clone();
                async move {
                    entered.notify_one();
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    Ok(())
                }
            }
        }));
        bus.register_hook(Hook::session_end({
            let ended = ended.clone();
            move |_| {
                let ended = ended.clone();
                async move {
                    ended.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(Vec::new())
                }
            }
        }));
        notify_completion(&bus, "done");
        entered.notified().await;
        emit_session_end(&bus, SessionEndReason::Quit, None).await;
        assert!(ended.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn notifications_are_retained_observations_not_actor_waits() {
        let entered = Arc::new(tokio::sync::Notify::new());
        let finished = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut bus = HookRegistry::new();
        bus.register_hook(Hook::notification({
            let entered = entered.clone();
            let finished = finished.clone();
            move |event| {
                let entered = entered.clone();
                let finished = finished.clone();
                async move {
                    assert_eq!(event.kind, NotificationKind::PermissionPrompt);
                    entered.notify_one();
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    finished.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                }
            }
        }));
        bus.schedule_notification(NotificationEvent {
            kind: NotificationKind::PermissionPrompt,
            level: "info".into(),
            message: "approval".into(),
            tool_name: Some("Bash".into()),
        });
        entered.notified().await;
        assert!(!finished.load(std::sync::atomic::Ordering::SeqCst));
        bus.flush_notifications().await;
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    }
}

mod hooks_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn hook_timeout_overrides_defaults_without_mutating_clones() {
        let original = Hook::session_start(|_| async {
            tokio::time::sleep(std::time::Duration::from_secs(11)).await;
            Ok(Vec::new())
        })
        .with_timeout(std::time::Duration::from_secs(1));
        let extended = original
            .clone()
            .with_timeout(std::time::Duration::from_secs(20));
        for (hook, seconds) in [(original, 1), (extended, 11)] {
            let mut hooks = HookRegistry::new();
            hooks.register_hook(hook);
            let started = tokio::time::Instant::now();
            emit_session_start(&hooks, SessionStartReason::Startup, None).await;
            assert_eq!(started.elapsed(), std::time::Duration::from_secs(seconds));
            let notices = hooks.take_notices();
            if seconds == 1 {
                assert!(
                    matches!(notices.as_slice(), [HookNotice::Warning { message }]
                    if message.contains("timed out"))
                );
            } else {
                assert!(notices.is_empty());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn context_recovery_handles_errors_and_timeouts() {
        let event = PromptContextEvent {
            original_prompt: "original".into(),
            prompt: "transformed".into(),
            source: UserPromptSubmitSource::Sdk,
            dispatch: UserPromptDispatch::Immediate,
            transcript: Arc::new(Transcript::new()),
            working_dir: PathBuf::from("/tmp"),
            delegated_child: false,
        };
        for (delay, timeout) in [
            (Duration::ZERO, DEFAULT_HOOK_TIMEOUT),
            (DEFAULT_HOOK_TIMEOUT * 2, DEFAULT_HOOK_TIMEOUT),
            (Duration::from_secs(2), Duration::from_secs(1)),
        ] {
            let callback = move |_: PromptContextEvent| async move {
                tokio::time::sleep(delay).await;
                Err(HookError::new("context unavailable"))
            };
            let mut strict = HookRegistry::new();
            strict.register_hook(Hook::prompt_context(callback).with_timeout(timeout));
            let error = strict.prepare_context(event.clone()).await.unwrap_err();
            let recoveries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mut hooks = HookRegistry::new();
            hooks.register_hook(
                Hook::prompt_context_with_recovery(callback, {
                    let recoveries = recoveries.clone();
                    let expected = error.to_string();
                    move |event, error| {
                        assert_eq!(event.original_prompt, "original");
                        assert_eq!(event.prompt, "transformed");
                        assert_eq!(error.to_string(), expected);
                        recoveries.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Ok(PromptContextOutcome {
                            notices: vec![HookNotice::Warning {
                                message: error.to_string(),
                            }],
                            ..Default::default()
                        })
                    }
                })
                .with_timeout(timeout),
            );
            let started = tokio::time::Instant::now();
            assert_eq!(
                hooks.prepare_context(event.clone()).await.unwrap(),
                PromptContextOutcome {
                    notices: vec![HookNotice::Warning {
                        message: error.to_string(),
                    }],
                    ..Default::default()
                }
            );
            assert_eq!(started.elapsed(), delay.min(timeout));
            assert_eq!(recoveries.load(std::sync::atomic::Ordering::Relaxed), 1);
        }
        let mut hooks = HookRegistry::new();
        hooks.register_hook(Hook::prompt_context_with_recovery(
            |_| async { Ok(PromptContextOutcome::default()) },
            |_, _| panic!("successful context must not invoke recovery"),
        ));
        assert_eq!(
            hooks.prepare_context(event).await.unwrap(),
            PromptContextOutcome::default()
        );
    }

    #[tokio::test]
    async fn background_readiness_survives_cancelled_waits() {
        let mut hooks = HookRegistry::new();
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        hooks.register_hook(Hook::session_start_with_mode(
            SessionStartMode::BeforeFirstPrompt,
            {
                let release = release.clone();
                let completed = completed.clone();
                move |_| {
                    let release = release.clone();
                    let completed = completed.clone();
                    async move {
                        release.notified().await;
                        completed.store(true, std::sync::atomic::Ordering::SeqCst);
                        Ok(vec![HookNotice::Info {
                            message: "ready".into(),
                        }])
                    }
                }
            },
        ));
        emit_session_start(&hooks, SessionStartReason::Startup, None).await;
        let mut wait = Box::pin(hooks.ready());
        assert!(matches!(
            futures::poll!(wait.as_mut()),
            std::task::Poll::Pending
        ));
        drop(wait);
        assert!(hooks.take_notices().is_empty());
        let replacement = hooks.for_generation();
        replacement.ready().await;
        assert!(replacement.take_notices().is_empty());
        release.notify_one();
        hooks.ready().await;
        assert!(completed.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            hooks.take_notices(),
            [HookNotice::Info {
                message: "ready".into()
            }]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn background_teardown_cancels_readiness_and_notifications() {
        let startup_entered = Arc::new(tokio::sync::Notify::new());
        let notification_entered = Arc::new(tokio::sync::Notify::new());
        let startup_dropped = tokio_util::sync::CancellationToken::new();
        let notification_dropped = tokio_util::sync::CancellationToken::new();
        let mut hooks = HookRegistry::new();
        hooks.register_hook(Hook::session_start_with_mode(
            SessionStartMode::BeforeFirstPrompt,
            |_| async {
                Ok(vec![HookNotice::Info {
                    message: "completed startup".into(),
                }])
            },
        ));
        hooks.register_hook(Hook::session_start_with_mode(
            SessionStartMode::BeforeFirstPrompt,
            {
                let entered = startup_entered.clone();
                let dropped = startup_dropped.clone();
                move |_| {
                    let entered = entered.clone();
                    let guard = dropped.clone().drop_guard();
                    async move {
                        let _guard = guard;
                        entered.notify_one();
                        std::future::pending().await
                    }
                }
            },
        ));
        hooks.register_hook(Hook::notification({
            let entered = notification_entered.clone();
            let dropped = notification_dropped.clone();
            move |_| {
                let entered = entered.clone();
                let guard = dropped.clone().drop_guard();
                async move {
                    let _guard = guard;
                    entered.notify_one();
                    std::future::pending().await
                }
            }
        }));
        let hooks = Arc::new(hooks);
        emit_session_start(&hooks, SessionStartReason::Startup, None).await;
        notify_completion(&hooks, "done");
        startup_entered.notified().await;
        notification_entered.notified().await;
        let readiness = tokio::spawn({
            let hooks = hooks.clone();
            async move { hooks.ready().await }
        });
        tokio::task::yield_now().await;
        assert_eq!(hooks.running_hook_count(), 2);
        let started = tokio::time::Instant::now();
        hooks.cancel_background_hooks().await;
        readiness.await.unwrap();
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(startup_dropped.is_cancelled());
        assert!(notification_dropped.is_cancelled());
        assert_eq!(hooks.running_hook_count(), 0);
        let notices = hooks.take_notices();
        for event in ["SessionStart", "Notification"] {
            assert!(notices.contains(&HookNotice::Cancelled {
                event: event.into(),
            }));
        }
        assert!(notices.contains(&HookNotice::Info {
            message: "completed startup".into(),
        }));
        assert_eq!(notices.len(), 3);
        let cancelled = HookNotice::Cancelled {
            event: "Notification".into(),
        };
        assert_eq!(
            serde_json::to_value(cancelled).unwrap()["kind"],
            "cancelled"
        );

        notify_completion(&hooks, "after teardown");
        hooks.cancel_background_hooks().await;
        assert!(hooks.take_notices().is_empty());
        let next = hooks.for_generation();
        notify_completion(&next, "new generation");
        notification_entered.notified().await;
        assert_eq!(next.running_hook_count(), 1);
        next.cancel_background_hooks().await;
        assert_eq!(next.running_hook_count(), 0);
        assert!(hooks.take_notices().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_teardown_kills_hook_process() {
        let root =
            PathBuf::from("/tmp").join(format!("phi-hook-teardown-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("pid");
        let mut hooks = HookRegistry::new();
        hooks.register_hook(Hook::notification({
            let root = root.clone();
            let pid_file = pid_file.clone();
            move |_| {
                let root = root.clone();
                let pid_file = pid_file.clone();
                async move {
                    let sandbox = crate::sandbox::Sandbox::new(false, root).unwrap();
                    crate::utils::process::run(
                        &sandbox,
                        "bash",
                        &[
                            "-c".into(),
                            "echo $$ > \"$1\"; sleep 30".into(),
                            "--".into(),
                            pid_file.display().to_string(),
                        ],
                        None,
                        Duration::from_secs(60),
                    )
                    .await
                    .map_err(|error| HookError::new(error.to_string()))?;
                    Ok(())
                }
            }
        }));
        notify_completion(&hooks, "done");
        let process_id: i32 = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&pid_file)
                    && let Ok(pid) = pid.trim().parse()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(1), hooks.cancel_background_hooks())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while unsafe { libc::kill(process_id, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("cancelled hook process survived teardown");
        assert_eq!(
            hooks.take_notices(),
            [HookNotice::Cancelled {
                event: "Notification".into(),
            }]
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_hooks_release_only_their_generation_counts() {
        for count in 1..=3 {
            let mut hooks = HookRegistry::new();
            for _ in 0..count {
                hooks.register_hook(Hook::session_end(|_| std::future::pending()));
            }
            let mut ending = Box::pin(emit_session_end(&hooks, SessionEndReason::Quit, None));
            assert!(futures::poll!(ending.as_mut()).is_pending());
            assert_eq!(hooks.running_hook_count(), count);

            let next = hooks.for_generation();
            assert_eq!(next.running_hook_count(), 0);
            let mut next_ending = Box::pin(emit_session_end(&next, SessionEndReason::Quit, None));
            assert!(futures::poll!(next_ending.as_mut()).is_pending());
            assert_eq!(next.running_hook_count(), count);

            drop(ending);
            assert_eq!(hooks.running_hook_count(), 0);
            assert_eq!(next.running_hook_count(), count);
            drop(next_ending);
            assert_eq!(next.running_hook_count(), 0);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn session_end_does_not_wait_for_startup_readiness() {
        let mut hooks = HookRegistry::new();
        hooks.register_hook(Hook::session_start_with_mode(
            SessionStartMode::BeforeFirstPrompt,
            |_| std::future::pending::<HookResult<Vec<HookNotice>>>(),
        ));
        hooks.register_hook(Hook::session_end(|_| async {
            Ok(vec![HookNotice::Info {
                message: "ended".into(),
            }])
        }));
        emit_session_start(&hooks, SessionStartReason::Startup, None).await;

        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            emit_session_end(&hooks, SessionEndReason::Quit, None),
        )
        .await
        .expect("SessionEnd waited for startup readiness");
        assert_eq!(
            hooks.take_notices(),
            [HookNotice::Info {
                message: "ended".into(),
            }]
        );
        let mut readiness = Box::pin(hooks.ready());
        assert!(futures::poll!(readiness.as_mut()).is_pending());
    }

    #[tokio::test(start_paused = true)]
    async fn session_end_runs_hooks_concurrently_within_one_budget() {
        let mut hooks = HookRegistry::new();
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for _ in 0..2 {
            let completed = completed.clone();
            hooks.register_hook(Hook::session_end(move |_| {
                let completed = completed.clone();
                async move {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(Vec::new())
                }
            }));
        }
        let hooks = Arc::new(hooks);
        let ending = tokio::spawn({
            let hooks = hooks.clone();
            async move {
                emit_session_end(&hooks, SessionEndReason::Quit, None).await;
                hooks.take_notices()
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(hooks.running_hook_count(), 2);
        tokio::time::advance(DEFAULT_SESSION_END_TIMEOUT).await;
        let notices = tokio::time::timeout(std::time::Duration::from_millis(100), ending)
            .await
            .expect("SessionEnd exceeded the shared budget")
            .unwrap();
        assert_eq!(completed.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(hooks.running_hook_count(), 0);
        assert!(notices.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn session_end_uses_the_remaining_configured_budget() {
        for (budget, hook_timeout) in [(1, 30), (3, 30), (20, 30), (20, 2)] {
            let budget = std::time::Duration::from_secs(budget);
            let hook_timeout = std::time::Duration::from_secs(hook_timeout);
            let mut hooks = HookRegistry {
                timeouts: HookTimeouts {
                    session_end_timeout: budget,
                    ..Default::default()
                },
                ..Default::default()
            };
            hooks.register_hook(
                Hook::session_end(|_| std::future::pending()).with_timeout(hook_timeout),
            );
            hooks.register_hook(Hook::session_end(|_| async {
                Ok(vec![HookNotice::Info {
                    message: "next hook".into(),
                }])
            }));
            let started = tokio::time::Instant::now();
            emit_session_end(&hooks, SessionEndReason::Quit, None).await;
            assert_eq!(started.elapsed(), budget.min(hook_timeout));
            assert_eq!(hooks.running_hook_count(), 0);
            let notices = hooks.take_notices();
            if hook_timeout < budget {
                assert!(matches!(
                    notices.as_slice(),
                    [HookNotice::Info { message }, HookNotice::Warning { .. }]
                        if message == "next hook"
                ));
            } else {
                assert!(matches!(
                    notices.as_slice(),
                    [HookNotice::Info { message }, HookNotice::Warning { message: warning }]
                        if message == "next hook" && warning == "SessionEnd timed out"
                ));
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn configured_default_timeout_applies_to_dynamic_handlers() {
        struct PendingHandler;
        impl HookHandler for PendingHandler {
            fn handle<'a>(&'a self, _: &'a LifecycleEvent) -> HookHandlerFuture<'a> {
                Box::pin(std::future::pending())
            }

            fn teammate_idle<'a>(&'a self, _: &'a TeammateIdleEvent) -> TeammateIdleFuture<'a> {
                Box::pin(std::future::pending())
            }
        }
        for seconds in [1, 20] {
            let timeout = std::time::Duration::from_secs(seconds);
            let mut hooks = HookRegistry {
                timeouts: HookTimeouts {
                    hook_timeout: timeout,
                    ..Default::default()
                },
                ..Default::default()
            };
            hooks.register_handler(Arc::new(PendingHandler));
            let started = tokio::time::Instant::now();
            assert!(matches!(
                emit_user_prompt_submit(
                    &hooks,
                    UserPromptSubmitEvent {
                        delegated_child: false,
                        original_prompt: "input".into(),
                        prompt: "input".into(),
                        source: UserPromptSubmitSource::Sdk,
                        dispatch: UserPromptDispatch::Immediate,
                    },
                )
                .await,
                UserPromptSubmitAction::Handled { reason: Some(reason) }
                    if reason.contains("timed out")
            ));
            assert_eq!(started.elapsed(), timeout);
            let started = tokio::time::Instant::now();
            assert!(
                hooks
                    .teammate_idle(TeammateIdleEvent {
                        context: TaskLifecycleContext {
                            instance_id: "test".into(),
                            list_id: "tasks".into(),
                            team: None,
                            alias: None,
                        },
                    })
                    .await
                    .is_err()
            );
            assert_eq!(started.elapsed(), timeout);
        }
    }

    #[tokio::test]
    async fn prompt_handler_failures_block_submission() {
        struct FailedHandler;
        impl HookHandler for FailedHandler {
            fn handle<'a>(&'a self, _: &'a LifecycleEvent) -> HookHandlerFuture<'a> {
                Box::pin(async { anyhow::bail!("policy unavailable") })
            }
        }
        let mut hooks = HookRegistry::new();
        hooks.register_handler(Arc::new(FailedHandler));
        assert!(matches!(
            emit_user_prompt_submit(
                &hooks,
                UserPromptSubmitEvent {
                    delegated_child: false,
                    original_prompt: "input".into(),
                    prompt: "input".into(),
                    source: UserPromptSubmitSource::Sdk,
                    dispatch: UserPromptDispatch::Immediate,
                }
            )
            .await,
            UserPromptSubmitAction::Handled { reason: Some(_) }
        ));
        assert!(hooks.take_notices().is_empty());
        for kind in [
            LifecycleEventKind::PreToolUse,
            LifecycleEventKind::UserPromptExpansion,
        ] {
            let responses = hooks.emit(&LifecycleEvent::new(kind)).await;
            assert!(matches!(responses.as_slice(), [response] if response.block
                && response.reason.as_deref().is_some_and(|reason| reason.contains("policy unavailable"))));
            assert!(hooks.take_notices().is_empty());
        }
    }

    #[tokio::test]
    async fn startup_failures_warn_without_poisoning_readiness() {
        for mode in [
            SessionStartMode::Blocking,
            SessionStartMode::BeforeFirstPrompt,
        ] {
            let mut hooks = HookRegistry::new();
            hooks.register_hook(Hook::session_start_with_mode(mode, |_| async {
                Err(HookError::new("startup failed"))
            }));
            emit_session_start(&hooks, SessionStartReason::Startup, None).await;
            for _ in 0..2 {
                hooks.ready().await;
            }
            assert_eq!(
                hooks.take_notices(),
                [HookNotice::Warning {
                    message: "startup failed".into()
                }]
            );
        }
    }

    use crate::core::lifecycle::{
        BuildSystemPromptEvent, PermissionDecision, ToolResultEvent, ToolUseEvent,
        UserPromptDispatch, UserPromptSubmitAction, UserPromptSubmitEvent, UserPromptSubmitSource,
    };
    use crate::hooks::{
        HookRegistry, emit_agent_start, emit_agent_turn_end, emit_build_system_prompt,
        emit_context, emit_post_tool_use, emit_pre_tool_use, emit_user_prompt_submit,
    };

    fn bus(hooks: impl IntoIterator<Item = Hook>) -> HookRegistry {
        let mut bus = HookRegistry::new();
        for hook in hooks {
            bus.register_hook(hook);
        }
        bus
    }

    #[tokio::test(start_paused = true)]
    async fn completion_check_timeout_fails_closed() {
        let bus = bus([Hook::stop(|_| std::future::pending())]);
        let error = bus
            .completion_check(&CompletionEvent {
                transcript: crate::transcript::Transcript::new(),
                response: "candidate".into(),
                continuations: 0,
            })
            .await
            .unwrap_err();
        assert_eq!(error.message(), "lifecycle hook timed out");
    }

    #[tokio::test]
    async fn agent_observers_receive_typed_boundaries() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observe_run = |boundary| {
            let observed = observed.clone();
            move |event: AgentRunEvent| {
                let observed = observed.clone();
                async move {
                    assert_eq!(event.session_id.as_deref(), Some("session-1"));
                    observed.lock().unwrap().push(boundary);
                    Ok(())
                }
            }
        };
        let observe_turn = |boundary| {
            let observed = observed.clone();
            move |event: AgentTurnEvent| {
                let observed = observed.clone();
                async move {
                    assert_eq!(event.run.session_id.as_deref(), Some("session-1"));
                    assert_eq!(event.turn_id, "turn-1");
                    assert_eq!(event.index, 2);
                    observed.lock().unwrap().push(boundary);
                    Ok(())
                }
            }
        };
        let bus = bus([
            Hook::agent_start(observe_run("start")),
            Hook::agent_end(|_| async { Err(HookError::new("observer failed")) }),
            Hook::agent_end(observe_run("end")),
            Hook::agent_turn_start(observe_turn("turn start")),
            Hook::agent_turn_end({
                let observe = observe_turn("turn end");
                move |event: AgentTurnEndEvent| {
                    let observed = observe(event.turn);
                    async move {
                        observed.await?;
                        Ok(AgentTurnEndOutcome::default())
                    }
                }
            }),
        ]);
        let run = AgentRunEvent {
            session_id: Some("session-1".into()),
            ..Default::default()
        };
        let turn = AgentTurnEvent {
            run: run.clone(),
            turn_id: "turn-1".into(),
            index: 2,
        };
        emit_agent_start(&bus, run.clone()).await;
        emit_agent_turn_start(&bus, turn.clone()).await;
        emit_agent_turn_end(
            &bus,
            AgentTurnEndEvent {
                turn,
                transcript: crate::transcript::Transcript::new(),
            },
        )
        .await;
        emit_agent_end(&bus, run).await;
        assert_eq!(
            *observed.lock().unwrap(),
            ["start", "turn start", "turn end", "end"]
        );
        assert_eq!(
            bus.take_notices(),
            [HookNotice::Warning {
                message: "AgentEnd hook failed: observer failed".into(),
            }]
        );
    }

    #[tokio::test]
    async fn system_prompt_hooks_receive_original_prompt() {
        let append_prompt = |suffix: &'static str| {
            Hook::build_system_prompt(move |event| async move {
                Ok(BuildSystemPromptOutcome {
                    system_prompt: format!("{}{suffix}", event.system_prompt),
                })
            })
        };
        let bus = bus([
            append_prompt("-first"),
            Hook::build_system_prompt(|_| async { Err(HookError::new("unavailable")) }),
            append_prompt("-second"),
        ]);
        let result = emit_build_system_prompt(
            &bus,
            BuildSystemPromptEvent {
                system_prompt: "base".to_string(),
                cwd: None,
            },
        )
        .await;
        assert_eq!(result.system_prompt.as_deref(), Some("base-second"));
        assert_eq!(
            bus.take_notices(),
            [HookNotice::Warning {
                message: "BuildSystemPrompt hook failed: unavailable".into(),
            }]
        );
    }

    #[tokio::test]
    async fn provider_context_hooks_collect_additions() {
        let hook = Hook::provider_context(|event| async move {
            Ok(ProviderContextOutcome {
                additions: vec![format!("context for {}", event.turn_id)],
            })
        });
        assert_eq!(
            emit_context(&bus([hook]), "turn-1").await,
            ["context for turn-1"]
        );
    }

    #[tokio::test]
    async fn pre_tool_hook_errors_fail_closed() {
        let hook = Hook::pre_tool_use(|_| async { Err(HookError::new("unavailable")) });
        let result = emit_pre_tool_use(
            &bus([hook]),
            ToolUseEvent {
                tool_name: "test".to_string(),
                tool_use_id: "call-1".to_string(),
                tool_input: serde_json::json!({}),
            },
        )
        .await;
        assert_eq!(result.permission_decision, Some(PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn post_tool_hooks_receive_original_output() {
        let rewrite = |suffix: &'static str| {
            Hook::post_tool_use(move |event| async move {
                let output = event.tool_output.as_str().unwrap_or_default();
                Ok(PostToolUseOutcome::Rewrite {
                    tool_output: Value::String(format!("{output}{suffix}")),
                })
            })
        };
        let result = emit_post_tool_use(
            &bus([rewrite("-first"), rewrite("-second")]),
            ToolResultEvent {
                tool_name: "test".to_string(),
                tool_use_id: "call-1".to_string(),
                tool_input: serde_json::json!({}),
                tool_output: Value::String("result".to_string()),
                duration_ms: None,
            },
        )
        .await;
        assert_eq!(
            result.updated_output,
            Some(Value::String("result-second".to_string()))
        );
    }

    #[tokio::test]
    async fn user_prompt_hook_receives_typed_dispatch() {
        let hook = Hook::user_prompt_submit(|event| async move {
            assert_eq!(event.dispatch, UserPromptDispatch::FollowUp);
            assert_eq!(event.original_prompt, "prompt");
            Ok(UserPromptSubmitOutcome::ContinueWithPolicy {
                notices: Vec::new(),
                prompt: format!("{}-changed", event.prompt),
                injected_context: vec![crate::context::InjectedContext {
                    source: crate::transcript::ContextSource::Hook,
                    content: "context".into(),
                }],
                tool_policy: crate::turn::TurnToolPolicy {
                    allowances: vec![("read".into(), "**".into())],
                    removals: vec!["Edit".into()],
                },
            })
        });
        let next = Hook::user_prompt_submit(|event| async move {
            assert_eq!(event.original_prompt, "prompt");
            assert_eq!(event.prompt, "prompt");
            Ok(UserPromptSubmitOutcome::ContinueWithPolicy {
                notices: Vec::new(),
                prompt: event.prompt,
                injected_context: Vec::new(),
                tool_policy: crate::turn::TurnToolPolicy {
                    allowances: vec![("Read".into(), "**".into())],
                    removals: vec!["read".into()],
                },
            })
        });
        let result = emit_user_prompt_submit(
            &bus([hook, next]),
            UserPromptSubmitEvent {
                delegated_child: false,
                original_prompt: "prompt".to_string(),
                prompt: "prompt".to_string(),
                source: UserPromptSubmitSource::Sdk,
                dispatch: UserPromptDispatch::FollowUp,
            },
        )
        .await;
        let UserPromptSubmitAction::Continue {
            prompt,
            injected_context,
            tool_policy,
        } = result
        else {
            panic!("expected transformed input");
        };
        assert_eq!(prompt, "prompt-changed");
        assert_eq!(injected_context.len(), 1);
        assert_eq!(injected_context[0].content, "context");
        assert_eq!(tool_policy.allowances, [("Read".into(), "**".into())]);
        assert_eq!(tool_policy.removals, ["Edit", "Read"]);
    }
}

mod emit_tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::hooks::{HookHandler, HookHandlerFuture};

    struct TransformHandler {
        seen: Arc<Mutex<Vec<Value>>>,
        updated_input: Value,
        block: bool,
        cancel: bool,
    }

    impl HookHandler for TransformHandler {
        fn handle<'a>(&'a self, event: &'a LifecycleEvent) -> HookHandlerFuture<'a> {
            let seen = self.seen.clone();
            let updated_input = self.updated_input.clone();
            Box::pin(async move {
                seen.lock()
                    .unwrap()
                    .push(event.payload["tool_input"].clone());
                Ok(Some(HookResponse {
                    block: self.block,
                    cancel: self.cancel,
                    output: Some(HookOutput::Action(HookAction::PreToolUse(
                        PreToolUseResult {
                            updated_input: Some(updated_input),
                            ..PreToolUseResult::default()
                        },
                    ))),
                    ..HookResponse::default()
                }))
            })
        }
    }

    struct CaptureHandler(Arc<Mutex<Vec<Value>>>);

    impl HookHandler for CaptureHandler {
        fn handle<'a>(&'a self, event: &'a LifecycleEvent) -> HookHandlerFuture<'a> {
            self.0.lock().unwrap().push(event.payload.clone());
            Box::pin(async { Ok(None) })
        }
    }

    #[tokio::test]
    async fn user_prompt_submit_exposes_effective_dispatch() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut bus = HookRegistry::new();
        bus.register_handler(Arc::new(CaptureHandler(seen.clone())));

        emit_user_prompt_submit(
            &bus,
            UserPromptSubmitEvent {
                delegated_child: false,
                original_prompt: "next".to_string(),
                prompt: "next".to_string(),
                source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                dispatch: crate::core::lifecycle::UserPromptDispatch::FollowUp,
            },
        )
        .await;

        assert_eq!(seen.lock().unwrap()[0]["dispatch"], "follow_up");
    }

    #[tokio::test]
    async fn pre_tool_handlers_share_input_even_when_rejected() {
        for (block, cancel) in [(false, false), (true, false), (false, true), (true, true)] {
            let seen = Arc::new(Mutex::new(Vec::new()));
            let mut bus = HookRegistry::new();
            bus.register_handler(Arc::new(TransformHandler {
                seen: seen.clone(),
                updated_input: serde_json::json!({ "command": "rejected" }),
                block,
                cancel,
            }));
            bus.register_handler(Arc::new(TransformHandler {
                seen: seen.clone(),
                updated_input: serde_json::json!({ "command": "last rewrite" }),
                block: false,
                cancel: false,
            }));

            let result = emit_pre_tool_use(
                &bus,
                ToolUseEvent {
                    tool_name: "Bash".to_string(),
                    tool_use_id: "tool-1".to_string(),
                    tool_input: serde_json::json!({ "command": "git status" }),
                },
            )
            .await;

            assert_eq!(
                *seen.lock().unwrap(),
                vec![serde_json::json!({ "command": "git status" }); 2]
            );
            assert_eq!(
                result.updated_input,
                Some(serde_json::json!({ "command": "last rewrite" }))
            );
            assert_eq!(
                result.permission_decision,
                (block || cancel).then_some(PermissionDecision::Deny)
            );
        }
    }
}
