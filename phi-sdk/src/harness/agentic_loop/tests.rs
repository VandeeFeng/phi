use super::EventPipeline;
use crate::config::RuntimeConfig;
use crate::core::event::CoreEventScope;
use crate::core::lifecycle::CompletionOutcome;
use crate::core::lifecycle::UserPromptSubmitSource;
use crate::core::turn::{TurnPrompt, TurnToolPolicy};
use crate::harness::instance::handle::AgentInstanceEvent;
use crate::harness::tests::support::{
    completion, instance_for_provider, mock_provider, mock_provider_with_reply, read_request,
};
use crate::hooks::Hook;
use crate::transcript::{AgentMessage, Transcript, TranscriptEntryKind};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn reported_zero_output_is_not_replaced_with_a_text_estimate() {
    for reported_output in [Some(0), Some(7), None] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-usage-presence-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (sender, _requests) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, sender, move |request| {
            let mut response = completion(request);
            let mut usage =
                json!({"prompt_tokens": 11, "total_tokens": 11 + reported_output.unwrap_or(0)});
            if let Some(output) = reported_output {
                usage["completion_tokens"] = json!(output);
            }
            response["usage"] = usage;
            response
        }));
        let transcript = submit_prompt(&base.instance, "usage").await;
        let usage = transcript
            .entries
            .iter()
            .find_map(|entry| match &entry.kind {
                TranscriptEntryKind::Message {
                    message: AgentMessage::Assistant { usage, .. },
                } => Some(usage),
                _ => None,
            })
            .unwrap();
        let expected =
            reported_output.unwrap_or_else(|| Transcript::estimate_tokens("child-result"));
        assert_eq!(
            usage.output, expected,
            "reported output: {reported_output:?}"
        );
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn turn_end_context_edits_change_only_the_next_request_and_commit_atomically() {
    use crate::core::lifecycle::{AgentTurnEndOutcome, ContextEditDraft};
    use crate::transcript::{ContextContent, ContextReplacement};
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum EditMode {
        Replace,
        Omit,
        Invalid,
        Error,
    }
    for mode in [
        EditMode::Replace,
        EditMode::Omit,
        EditMode::Invalid,
        EditMode::Error,
    ] {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-context-edit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(mock_provider(listener, sender, move |request| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                json!({"id":"tool-response", "model":"test", "choices":[{"delta":{"tool_calls":[{
                    "index":0,"id":"read-call","type":"function", "function":{"name":"Read", "arguments":json!({"path":"missing-source-file"}).to_string()}
                }]}, "finish_reason":"tool_calls"}]})
            } else {
                completion(request)
            }
        }));
        let mut phi = base.phi.clone();
        phi.hooks
            .register_hook(Hook::agent_turn_end(move |event| async move {
                if event.turn.index != 0 {
                    return Ok(AgentTurnEndOutcome::default());
                }
                if mode == EditMode::Error {
                    return Err(crate::hooks::HookError::new("observer failed"));
                }
                let target = event
                    .transcript
                    .entries
                    .iter()
                    .find(|entry| {
                        matches!(
                            entry.kind,
                            TranscriptEntryKind::Message {
                                message: AgentMessage::ToolResult { .. }
                            }
                        )
                    })
                    .unwrap();
                let replacement = (mode != EditMode::Omit).then(|| ContextReplacement {
                    content: ContextContent::Text("short replacement".into()),
                });
                let mut edits = vec![ContextEditDraft {
                    target_id: target.id.to_string(),
                    replacement,
                }];
                if mode == EditMode::Invalid {
                    edits.push(ContextEditDraft {
                        target_id: "missing".into(),
                        replacement: None,
                    });
                }
                Ok(AgentTurnEndOutcome {
                    context_edits: edits,
                })
            }));
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let transcript = submit_prompt(&instance, "read the file").await;
        let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(requests.len(), 2);
        let edit_count = transcript
            .entries
            .iter()
            .filter(|entry| matches!(entry.kind, TranscriptEntryKind::ContextEdit { .. }))
            .count();
        let expected = match mode {
            EditMode::Replace => Some("short replacement"),
            EditMode::Omit => Some("No result provided"),
            EditMode::Invalid | EditMode::Error => None,
        };
        assert_eq!(edit_count, usize::from(expected.is_some()));
        let original = transcript
            .entries
            .iter()
            .find(|entry| {
                matches!(
                    entry.kind,
                    TranscriptEntryKind::Message {
                        message: AgentMessage::ToolResult { .. }
                    }
                )
            })
            .unwrap();
        assert!(
            !serde_json::to_string(original)
                .unwrap()
                .contains("short replacement")
        );
        let tool_result = requests[1]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool")
            .unwrap();
        assert_eq!(tool_result["tool_call_id"], "read-call");
        let content = tool_result["content"].to_string();
        if let Some(expected) = expected {
            assert!(content.contains(expected));
        } else {
            assert!(!content.contains("short replacement"));
            assert!(!content.contains("No result provided"));
        }
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[derive(Clone)]
struct DeferredProbe(Arc<AtomicUsize>);

impl rig::tool::PortableTool for DeferredProbe {
    const NAME: &'static str = "deferred_probe";
    type Args = serde_json::Value;
    type Output = String;
    type Error = std::convert::Infallible;

    fn description(&self) -> String {
        "Retrieve the deferred catalog probe".into()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({"type":"object","properties":{"needle":{"type":"string","description":"schemaonlykeyword"}}})
    }

    async fn call(&self, _input: Self::Args) -> Result<String, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok("probe-result".into())
    }
}

fn search_flow_response(
    request: &serde_json::Value,
    name: &str,
    namespace: Option<&str>,
) -> serde_json::Value {
    let messages = request["messages"].as_array().unwrap();
    let last_user = messages
        .iter()
        .rposition(|message| message["role"] == "user")
        .unwrap();
    let tool_count = messages[last_user + 1..]
        .iter()
        .filter(|message| message["role"] == "tool")
        .count();
    let prompt = messages[last_user]["content"].to_string();
    let calls = if prompt.contains("search-and-load") && tool_count == 0 {
        vec![
            (
                "tool_search",
                json!({"query":"schemaonlykeyword", "limit":1,"namespace":namespace}),
            ),
            (name, json!({})),
        ]
    } else if (prompt.contains("search-and-load") && tool_count == 2)
        || (prompt.contains("call-loaded") && tool_count == 0)
    {
        vec![(name, json!({}))]
    } else {
        return completion(request);
    };
    json!({"id":"search-flow","model":"test","choices":[{"delta":{"tool_calls":calls.iter().enumerate().map(|(index,(name,args))| json!({
        "index":index,"id":format!("call_{tool_count}_{index}"),"type":"function",
        "function":{"name":name,"arguments":args.to_string()}
    })).collect::<Vec<_>>()},"finish_reason":"tool_calls"}]})
}

async fn submit_prompt(
    instance: &crate::agent_instance::AgentInstance,
    prompt: &str,
) -> Transcript {
    instance
        .submit(TurnPrompt {
            prompt: prompt.into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(30), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    assert!(
        view.snapshot.latest_error.is_none(),
        "{:?}",
        view.snapshot.latest_error
    );
    view.transcript
}

fn declares(request: &serde_json::Value, name: &str) -> bool {
    request["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
}

#[tokio::test]
async fn tool_search_loads_without_codemode_and_restores_only_the_selected_branch() {
    use crate::tools::{ToolDeclaration, ToolExposure};
    let root =
        std::path::Path::new("/tmp").join(format!("phi-tool-search-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        search_flow_response(request, "deferred_probe", None)
    }));
    let effects = Arc::new(AtomicUsize::new(0));
    let mut phi = base.phi.clone();
    phi.cfg.extensions.codemode.enabled = false;
    phi.tool_declarations.push(
        ToolDeclaration::new(DeferredProbe(effects.clone()))
            .prompt_snippet("loaded-probe-snippet")
            .prompt_guidelines(["loaded-probe-guideline"])
            .exposure(ToolExposure::Deferred),
    );
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let transcript = submit_prompt(&instance, "search-and-load").await;
    let initial = requests.try_recv().unwrap();
    let loaded = requests.try_recv().unwrap();
    let executed = requests.try_recv().unwrap();
    assert!(declares(&initial, "ToolSearch"));
    assert!(!declares(&initial, "Codemode"));
    assert!(!declares(&initial, "deferred_probe"));
    assert!(declares(&loaded, "deferred_probe"));
    assert!(declares(&executed, "deferred_probe"));
    assert!(loaded["messages"].to_string().contains("is deferred"));
    assert!(
        loaded["messages"]
            .to_string()
            .contains("loaded-probe-guideline")
    );
    assert!(executed["messages"].to_string().contains("probe-result"));
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    assert_eq!(
        super::search::read_loaded_tools(&transcript),
        std::collections::BTreeSet::from(["deferred_probe".into()])
    );
    let restored: Transcript =
        serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
    let first_user = restored
        .entries
        .iter()
        .find(|entry| {
            matches!(
                &entry.kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::User { .. }
                }
            )
        })
        .unwrap()
        .id
        .clone();
    for scenario in [
        "restore",
        "rewind",
        "hidden",
        "excluded",
        "no-tools",
        "permission-denied",
    ] {
        let manager = crate::harness::manager::AgentManager::new();
        let mut history = restored.clone();
        let mut candidate_phi = phi.clone();
        if scenario == "rewind" {
            history.select_branch(Some(&first_user)).unwrap();
        }
        if scenario == "hidden" {
            candidate_phi.tool_declarations[0] = candidate_phi.tool_declarations[0]
                .clone()
                .exposure(ToolExposure::Hidden);
        }
        if scenario == "no-tools" {
            candidate_phi.options.no_tools = true;
        }
        if scenario == "permission-denied" {
            candidate_phi.runtime_services = candidate_phi.runtime_services.permission_policy(
                crate::permission::PermissionConfig::try_from_value(json!({"mode":"deny"}))
                    .unwrap(),
                Some(root.clone()),
            );
        }
        let mut builder = candidate_phi
            .agent_instance_builder()
            .working_dir(root.clone())
            .transcript(history);
        if scenario == "excluded" {
            builder = builder.tools(["ToolSearch"]);
        }
        let instance = manager.insert(builder.build().await.unwrap()).unwrap();
        let before = effects.load(Ordering::SeqCst);
        let history = submit_prompt(
            &instance,
            if scenario == "restore" || scenario == "permission-denied" {
                "call-loaded"
            } else {
                "inspect-only"
            },
        )
        .await;
        let request = requests.try_recv().unwrap();
        assert_eq!(
            declares(&request, "deferred_probe"),
            matches!(scenario, "restore" | "permission-denied"),
            "{scenario}"
        );
        if matches!(scenario, "restore" | "permission-denied") {
            let _ = requests.try_recv().unwrap();
            assert_eq!(
                effects.load(Ordering::SeqCst) - before,
                usize::from(scenario == "restore")
            );
        }
        if scenario == "rewind" {
            assert!(super::search::read_loaded_tools(&history).is_empty());
        }
        manager.shutdown().await.unwrap();
    }
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn tool_search_loads_cold_mcp_and_rehydrates_deferred_contracts_on_resume() {
    use crate::extension::mcp::McpClientManager;
    let root =
        std::path::Path::new("/tmp").join(format!("phi-tool-search-mcp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let script = root.join("server.py");
    let effects = root.join("effects");
    std::fs::write(&script, r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if 'id' not in request:
        continue
    method = request['method']
    if method == 'initialize':
        result = {'protocolVersion':'2024-11-05','capabilities':{'tools':{}},'serverInfo':{'name':'search-fixture','version':'1'}}
    elif method == 'tools/list':
        result = {'tools':[{'name':name,'description':'deferred fixture','inputSchema':{'type':'object','properties':{'needle':{'type':'string','description':'schemaonlykeyword'}}}} for name in ['probe','blocked']]}
    elif method in ['resources/list','prompts/list']:
        result = {method.split('/')[0]:[]}
    elif method == 'tools/call':
        with open(sys.argv[1], 'a') as target:
            target.write(request['params']['name'] + '\n')
        result = {'content':[{'type':'text','text':'probe-result'}]}
    else:
        print(json.dumps({'jsonrpc':'2.0','id':request['id'],'error':{'code':-32601,'message':'method not found'}}), flush=True)
        continue
    print(json.dumps({'jsonrpc':'2.0','id':request['id'],'result':result}), flush=True)
"#).unwrap();
    let configs = serde_json::from_value(json!({"fixture":{
        "command":"python3","args":[script,effects],"exposure":"deferred","toolExposure":{"blocked":"hidden"}
    }})).unwrap();
    let mcp = McpClientManager::load_with_settings(&configs, Default::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        search_flow_response(request, "fixture_probe", Some("fixture"))
    }));
    let mut phi = base.phi.clone();
    phi.cfg.extensions.codemode.enabled = false;
    phi.runtime_services = phi.runtime_services.mcp_manager(mcp.clone());
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    assert!(!mcp.server_snapshots().await[0].status.connected);
    let history = submit_prompt(&instance, "search-and-load").await;
    let initial = requests.try_recv().unwrap();
    let loaded = requests.try_recv().unwrap();
    let executed = requests.try_recv().unwrap();
    assert!(!declares(&initial, "fixture_probe"));
    assert!(!declares(&initial, "Codemode"));
    assert!(declares(&loaded, "fixture_probe"));
    assert!(executed["messages"].to_string().contains("probe-result"));
    for request in [&initial, &loaded, &executed] {
        assert!(!declares(request, "fixture_blocked"));
        assert!(!declares(request, "MCP"));
    }
    assert_eq!(std::fs::read_to_string(&effects).unwrap(), "probe\n");
    base.manager.shutdown().await.unwrap();
    mcp.shutdown().await;
    let resumed_mcp = McpClientManager::load_with_settings(&configs, Default::default());
    phi.runtime_services = phi.runtime_services.mcp_manager(resumed_mcp.clone());
    let manager = crate::harness::manager::AgentManager::new();
    let restored: Transcript =
        serde_json::from_value(serde_json::to_value(history).unwrap()).unwrap();
    let instance = manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .transcript(restored)
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    submit_prompt(&instance, "call-loaded").await;
    assert!(declares(&requests.try_recv().unwrap(), "fixture_probe"));
    let _ = requests.try_recv().unwrap();
    assert_eq!(std::fs::read_to_string(&effects).unwrap(), "probe\nprobe\n");
    manager.shutdown().await.unwrap();
    resumed_mcp.shutdown().await;
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn request_compaction_uses_selected_model_and_commits_only_complete_summaries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider = crate::config::CustomProviderConfig {
        api: crate::provider::ProviderApi::OpenAiCompletions,
        base_url: Some(format!("http://{}/v1", listener.local_addr().unwrap())),
        api_key: None,
    };
    let client = crate::provider::create_client(
        "local",
        Some("test-key"),
        &std::collections::HashMap::from([("local".into(), provider)]),
    )
    .unwrap();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, requests, |request| {
        let model = request["model"].as_str().unwrap();
        let (finish, text) = match model {
            "length" => ("length", "partial summary"),
            "empty" => ("stop", " "),
            _ => ("stop", "complete summary"),
        };
        json!({"id":"summary", "object":"chat.completion", "created":0, "model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":finish}],
            "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}})
    }));
    for model in ["length", "empty", "selected-model"] {
        let mut transcript = crate::transcript::Transcript::new();
        transcript.append_user_message(&format!("{}end-of-old-input", "界".repeat(3_000)));
        transcript.append_user_message("retained input");
        transcript.total_tokens = 1_000_000;
        let before = transcript.entries.len();
        let (_events, event_rx) = tokio::sync::mpsc::channel(2);
        let (control, round_requests) = crate::harness::agentic_loop::RoundControl::channel();
        let context = tokio::spawn(async move {
            control
                .context(
                    crate::context::CompactionTrigger::Threshold,
                    crate::context::HistoryTarget {
                        provider: "test".into(),
                        api: None,
                        model: "test".into(),
                    },
                )
                .await
        });
        let runner = crate::harness::agentic_loop::AgentRunner {
            tool_policy: None,
            round_requests: Some(round_requests),
            event_rx,
            steering: Arc::new(crate::harness::agentic_loop::AgentSteerMailbox::new(
                crate::core::turn::QueuedSubmissionDrainMode::OneAtATime,
            )),
            task: tokio::spawn(async {}),
        };
        let mut pipeline = EventPipeline::new(runner, CoreEventScope::default()).with_compaction(
            client.clone(),
            crate::context::HistoryTarget {
                provider: "local".into(),
                api: Some(crate::provider::ProviderApi::OpenAiCompletions),
                model: model.into(),
            },
            RuntimeConfig {
                switch: Some("review".into()),
                context_window: Some(1),
                reserve_tokens: Some(0),
                keep_recent_tokens: Some(1),
                ..Default::default()
            },
            Default::default(),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !context.is_finished() {
                pipeline.progress_round(
                    &mut transcript,
                    &crate::hooks::HookRegistry::new(),
                    |_| Some(Box::pin(async { Ok(super::CompletionDecision::Finish) })),
                );
                pipeline.drain(&mut transcript).await;
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let context = context.await.unwrap();
        let request = received.recv().await.unwrap();
        assert_eq!(request["model"], model);
        assert!(request["messages"].to_string().contains("end-of-old-input"));
        let successful = model == "selected-model";
        assert_eq!(transcript.entries.len(), before + usize::from(successful));
        assert_eq!(context.is_ok(), successful);
        if let Ok(context) = context {
            assert!(
                serde_json::to_string(&context.messages)
                    .unwrap()
                    .contains("complete summary")
            );
        }
        let history =
            serde_json::to_string(&crate::context::convert_history(&transcript).unwrap()).unwrap();
        assert_eq!(history.contains("complete summary"), successful);
        assert_eq!(history.contains("end-of-old-input"), !successful);
        assert!(history.contains("retained input"));
        assert_eq!(transcript.needs_compaction(64, 0), !successful);
        assert_eq!(transcript.total_tokens, 1_000_000);
    }
    server.abort();
}

#[tokio::test]
async fn canonical_compaction_and_request_retries_never_replay_tools() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    const HTTP_OK: &str = "HTTP/1.1 200 OK";
    const HTTP_UNAVAILABLE: &str = "HTTP/1.1 503 Service Unavailable";
    for failures in [3, 4] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-canonical-round-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("payload.txt"), "tool-payload".repeat(4_000)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let retries = Arc::new(AtomicUsize::new(0));
        let counted = retries.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                sender.send(request.clone()).unwrap();
                let summarizing = request["stream"] != true;
                let tool_result = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|message| message["role"] == "tool");
                let unavailable = !summarizing
                    && tool_result
                    && counted.fetch_add(1, Ordering::SeqCst) < failures;
                let (content_type, body) = if unavailable {
                    (
                        "application/json",
                        json!({"error": {"message": "temporarily unavailable"}}).to_string(),
                    )
                } else if summarizing {
                    ("application/json", json!({"id":"summary", "object":"chat.completion", "created":0, "model":"test", "choices":[{"index":0,"message":{"role":"assistant","content":"canonical compacted prefix"},"finish_reason":"stop"}]}).to_string())
                } else {
                    let delta = if tool_result {
                        json!({"content":"finished"})
                    } else {
                        json!({"tool_calls":[{"index":0,"id":"read-once","type":"function","function":{"name":"Read","arguments":json!({"path":"payload.txt"}).to_string()}}]})
                    };
                    let chunk = json!({"id":"response","model":"test","choices":[{"delta":delta,"finish_reason":if tool_result {"stop"} else {"tool_calls"}}]});
                    (
                        "text/event-stream",
                        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                    )
                };
                let status = if unavailable {
                    HTTP_UNAVAILABLE
                } else {
                    HTTP_OK
                };
                let response = format!(
                    "{status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let base = instance_for_provider(&root, address, false).await;
        let mut phi = base.phi.clone().with_runtime_services(
            base.phi.runtime_services.clone().permission_policy(
                crate::permission::PermissionConfig::default(),
                Some(root.clone()),
            ),
        );
        phi.cfg.context_window = Some(32_000);
        phi.cfg.reserve_tokens = Some(0);
        phi.options.max_tokens = Some(128);
        phi.cfg.keep_recent_tokens = Some(1);
        let calls = Arc::new(AtomicUsize::new(0));
        phi.hooks.register_hook(crate::hooks::Hook::pre_tool_use({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(crate::core::lifecycle::PreToolUseOutcome::Continue) }
            }
        }));
        let mut transcript = Transcript::new();
        transcript.append_user_message(&"old-prefix".repeat(10_000));
        transcript.append_assistant_message(
            None,
            None,
            vec![crate::transcript::ContentBlock::Text {
                text: "old answer".into(),
            }],
            "local".into(),
            crate::provider::ProviderApi::OpenAiCompletions,
            "test".into(),
            Default::default(),
            "stop".into(),
        );
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .transcript(transcript)
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let mut events = instance.subscribe();
        instance
            .submit(TurnPrompt {
                prompt: "Read payload.txt and report.".into(),
                source: UserPromptSubmitSource::Sdk,
                images: Vec::new(),
                injected_context: Vec::new(),
                tool_policy: TurnToolPolicy {
                    allowances: vec![("Read".into(), "**".into())],
                    ..Default::default()
                },
            })
            .await
            .unwrap();
        let view =
            tokio::time::timeout(Duration::from_secs(30), instance.wait_for_quiescence(None))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        let finished = observed.iter().any(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, crate::PhiEvent::TurnFinished { compacted: true, .. })));
        assert_eq!(finished, failures == 3);
        let delays = observed
            .iter()
            .filter_map(|event| match event {
                AgentInstanceEvent::Phi(envelope) => match &envelope.event {
                    crate::PhiEvent::ProviderRecovery { recovery }
                        if recovery.reason == crate::RecoveryReason::ProviderRetry =>
                    {
                        Some(recovery.delay_ms)
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(delays, [2000, 4000, 8000]);
        assert_eq!(view.snapshot.latest_error.is_some(), failures == 4);
        let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(requests.len(), 6);
        assert!(
            !requests[0]
                .to_string()
                .contains("canonical compacted prefix")
        );
        assert_ne!(requests[1]["stream"], true);
        for request in &requests[2..] {
            let messages = request["messages"].to_string();
            assert!(messages.contains("canonical compacted prefix"));
            assert!(messages.contains("tool-payload"));
            assert!(!messages.contains("old-prefix"));
        }
        assert!(
            serde_json::to_string(&view.transcript)
                .unwrap()
                .contains("old-prefix")
        );
        assert_eq!(
            view.transcript
                .entries
                .iter()
                .filter(|entry| matches!(entry.kind, TranscriptEntryKind::Compaction { .. }))
                .count(),
            1
        );
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn server_retry_delay_remains_interruptible() {
    let root =
        std::path::Path::new("/tmp").join(format!("phi-retry-delay-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body = r#"{"error":{"message":"overloaded"}}"#;
        socket.write_all(format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nRetry-After-Ms: 309000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len(),
        ).as_bytes()).await.unwrap();
    });
    let mut events = base.subscribe();
    base.submit(TurnPrompt {
        prompt: "retry".into(),
        source: UserPromptSubmitSource::Sdk,
        images: Vec::new(),
        injected_context: Vec::new(),
        tool_policy: Default::default(),
    })
    .await
    .unwrap();
    let recovery = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let AgentInstanceEvent::Phi(envelope) = events.recv().await.unwrap()
                && let crate::PhiEvent::ProviderRecovery { recovery } = envelope.event
            {
                break recovery;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(recovery.delay_ms, 309_000);
    assert_eq!(recovery.attempt, 1);
    assert_eq!(recovery.max_attempts, 3);
    tokio::time::timeout(Duration::from_secs(5), async {
        base.abort().await.unwrap();
        base.wait_for_quiescence(None).await.unwrap();
        base.manager.shutdown().await.unwrap();
    })
    .await
    .unwrap();
    server.await.unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn stream_retry_requests_committed_partial_content() {
    const HTTP_OK: &str = "HTTP/1.1 200 OK";
    let root =
        std::path::Path::new("/tmp").join(format!("phi-stream-retry-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (requests, mut received) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        requests.send(read_request(&mut socket).await).unwrap();
        let chunk = json!({"id":"partial","model":"test","choices":[{"delta":{"content":"retained fragment"},"finish_reason":null}]});
        let body = format!("data: {chunk}\n\n");
        socket.write_all(format!("{HTTP_OK}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + 1).as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
        drop(socket);
        mock_provider_with_reply(listener, requests, |_| async {
            json!({"id":"response","model":"test","choices":[{"delta":{"content":"completed continuation"},"finish_reason":"stop"}]})
        }).await;
    });
    let base = instance_for_provider(&root, address, false).await;
    let mut events = base.instance.subscribe();
    base.instance
        .submit(TurnPrompt {
            prompt: "Respond.".into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    let view = tokio::time::timeout(
        Duration::from_secs(5),
        base.instance.wait_for_quiescence(None),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        view.snapshot.latest_error.is_none(),
        "{:?}",
        view.snapshot.latest_error
    );
    let requests = std::iter::from_fn(|| received.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]["messages"]
            .to_string()
            .contains("retained fragment")
    );
    let assistants = view
        .transcript
        .entries
        .iter()
        .filter_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message {
                message: message @ AgentMessage::Assistant { .. },
            } => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(assistants.len(), 2);
    for (message, (expected_stop, expected_text)) in assistants.iter().zip([
        ("error", "retained fragment"),
        ("stop", "completed continuation"),
    ]) {
        let AgentMessage::Assistant {
            id,
            provider,
            api,
            model,
            content,
            stop_reason,
            ..
        } = message
        else {
            unreachable!()
        };
        assert!(
            id.is_none(),
            "chat streaming provides no provider message ID"
        );
        assert_eq!(provider, "local");
        assert_eq!(*api, Some(crate::provider::ProviderApi::OpenAiCompletions));
        assert_eq!(model, "test");
        assert_eq!(stop_reason, expected_stop);
        assert!(
            matches!(content.as_slice(), [crate::transcript::ContentBlock::Text { text }] if text == &expected_text)
        );
    }
    let mut started = Vec::new();
    let mut finished = Vec::new();
    let mut completed_turns = 0;
    while let Ok(event) = events.try_recv() {
        if let AgentInstanceEvent::Phi(envelope) = event {
            match envelope.event {
                crate::PhiEvent::AssistantStarted { id } => started.push(id),
                crate::PhiEvent::AssistantFinished { id, .. } => finished.push(id),
                crate::PhiEvent::TurnFinished { .. } => completed_turns += 1,
                crate::PhiEvent::TurnFailed { .. } => {
                    panic!("recovered stream must not fail the turn")
                }
                _ => {}
            }
        }
    }
    assert_eq!(started.len(), 2);
    assert_ne!(started[0], started[1]);
    assert_eq!(finished, started);
    assert_eq!(completed_turns, 1);
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn overflow_compacts_canonical_context_once_before_retry() {
    for (repeated, enabled) in [(false, true), (true, true), (false, false)] {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-overflow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                sender.send(request.clone()).unwrap();
                let summarizing = request["stream"] != true;
                let recovered = request["messages"]
                    .to_string()
                    .contains("recovered summary");
                let (status, content_type, body) = if summarizing {
                    ("200 OK", "application/json", json!({"id":"summary", "object":"chat.completion", "created":0, "model":"test", "choices":[{"index":0,"message":{"role":"assistant","content":"recovered summary"},"finish_reason":"stop"}]}).to_string())
                } else if !recovered || repeated {
                    ("400 Bad Request", "application/json", json!({"error":{"code":"context_length_exceeded", "message":"Your input exceeds the context window of this model"}}).to_string())
                } else {
                    let chunk = json!({"id":"answer", "model":"test", "choices":[{"delta":{"content":"finished"},"finish_reason":"stop"}]});
                    (
                        "200 OK",
                        "text/event-stream",
                        format!("data: {chunk}\n\ndata: [DONE]\n\n"),
                    )
                };
                socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let mut phi = base.phi.clone();
        phi.cfg.context_window = Some(128_000);
        phi.cfg.compact_enabled = Some(enabled);
        phi.cfg.keep_recent_tokens = Some(1);
        let mut transcript = Transcript::new();
        transcript.append_user_message("old input below the configured threshold");
        transcript.append_user_message("retained input");
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .transcript(transcript)
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let mut events = instance.subscribe();
        instance
            .submit(TurnPrompt {
                prompt: "finish the task".into(),
                source: UserPromptSubmitSource::Sdk,
                images: Vec::new(),
                injected_context: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        let recoveries = observed
            .iter()
            .filter_map(|event| match event {
                AgentInstanceEvent::Phi(envelope) => match &envelope.event {
                    crate::PhiEvent::ProviderRecovery { recovery } => Some(recovery),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(recoveries.len(), 1);
        assert_eq!(recoveries[0].reason, crate::RecoveryReason::ContextOverflow);
        assert_eq!(recoveries[0].attempt, 1);
        assert_eq!(recoveries[0].model, "test");
        let successful = enabled && !repeated;
        assert_eq!(view.snapshot.latest_error.is_none(), successful);
        assert_eq!(observed.iter().filter(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, crate::PhiEvent::TurnFinished { .. }))).count(), usize::from(successful));
        let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(requests.len(), if enabled { 3 } else { 1 });
        if enabled {
            assert_ne!(requests[1]["stream"], true);
            assert!(
                requests[2]["messages"]
                    .to_string()
                    .contains("recovered summary")
            );
            assert!(!requests[2]["messages"].to_string().contains("old input"));
        }
        assert!(
            serde_json::to_string(&view.transcript)
                .unwrap()
                .contains("old input")
        );
        assert_eq!(
            view.transcript
                .entries
                .iter()
                .filter(|entry| matches!(entry.kind, TranscriptEntryKind::Compaction { .. }))
                .count(),
            usize::from(enabled)
        );
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn request_budget_rechecks_frozen_context_after_canonical_compaction() {
    for (enabled, oversized_summary) in [(true, false), (true, true), (false, false)] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-request-budget-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, sender, move |request| {
            if request["stream"] == true {
                completion(request)
            } else {
                let summary = if oversized_summary {
                    "summary".repeat(30_000)
                } else {
                    "budgeted summary".into()
                };
                json!({"id":"summary", "object":"chat.completion", "created":0, "model":"test",
                    "choices":[{"index":0,"message":{"role":"assistant","content":summary},"finish_reason":"stop"}]})
            }
        }));
        let mut phi = base.phi.clone();
        phi.cfg.context_window = Some(32_000);
        phi.cfg.reserve_tokens = Some(128);
        phi.options.max_tokens = Some(128);
        phi.cfg.keep_recent_tokens = Some(4);
        phi.cfg.compact_enabled = Some(enabled);
        phi.cfg.no_tools = Some(true);
        let hook_calls = Arc::new(AtomicUsize::new(0));
        phi.hooks
            .register_hook(crate::hooks::Hook::provider_context({
                let hook_calls = hook_calls.clone();
                move |_| {
                    hook_calls.fetch_add(1, Ordering::SeqCst);
                    async {
                        Ok(crate::core::lifecycle::ProviderContextOutcome {
                            additions: vec![format!(
                                "dynamic-budget-marker:{}",
                                "d".repeat(80_000)
                            )],
                        })
                    }
                }
            }));
        let mut transcript = Transcript::new();
        transcript.append_user_message(&format!("old-budget-marker:{}", "h".repeat(80_000)));
        transcript.append_user_message("retained input");
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .transcript(transcript)
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let mut events = instance.subscribe();
        instance
            .submit(TurnPrompt {
                prompt: "finish".into(),
                source: UserPromptSubmitSource::Sdk,
                images: Vec::new(),
                injected_context: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        let successful = enabled && !oversized_summary;
        assert_eq!(
            view.snapshot.latest_error.is_none(),
            successful,
            "{:?}; {} requests",
            view.snapshot.latest_error,
            requests.len()
        );
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["stream"] != true)
                .count(),
            usize::from(enabled)
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request["stream"] == true)
                .count(),
            usize::from(successful)
        );
        let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(observed.iter().filter(|event| matches!(event,
            AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, crate::PhiEvent::TurnFinished { .. })
        )).count(), usize::from(successful));
        assert_eq!(
            view.transcript
                .entries
                .iter()
                .filter(|entry| matches!(entry.kind, TranscriptEntryKind::Compaction { .. }))
                .count(),
            usize::from(enabled)
        );
        assert!(
            serde_json::to_string(&view.transcript)
                .unwrap()
                .contains("old-budget-marker")
        );
        if successful {
            let request = requests.last().unwrap();
            let messages = request["messages"].to_string();
            assert!(messages.contains("budgeted summary"));
            assert!(messages.contains("dynamic-budget-marker"));
            assert!(messages.contains("retained input"));
            assert!(!messages.contains("old-budget-marker"));
        }
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn oversized_fixed_context_fails_without_sending_a_model_request() {
    let root =
        std::path::Path::new("/tmp").join(format!("phi-fixed-budget-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, completion));
    let mut phi = base.phi.clone();
    phi.cfg.context_window = Some(32_000);
    phi.cfg.reserve_tokens = Some(128);
    phi.options.max_tokens = Some(128);
    phi.cfg.no_tools = Some(true);
    phi.hooks
        .register_hook(crate::hooks::Hook::provider_context(|_| async {
            Ok(crate::core::lifecycle::ProviderContextOutcome {
                additions: vec!["x".repeat(150_000)],
            })
        }));
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    instance
        .submit(TurnPrompt {
            prompt: "finish".into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    assert!(view.snapshot.latest_error.is_some());
    assert!(requests.try_recv().is_err());
    assert!(
        !view
            .transcript
            .entries
            .iter()
            .any(|entry| matches!(entry.kind, TranscriptEntryKind::Compaction { .. }))
    );
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn completion_gate_commits_feedback_before_one_final_outcome() {
    for mode in [
        "continue_once",
        "fail",
        "always_continue",
        "cancel",
        "invalid",
        "invalid_policy",
        "error",
    ] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-completion-gate-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(
            listener,
            sender,
            |_| json!({"id":"response","model":"test","choices":[{"delta":{"content":"candidate answer"},"finish_reason":"stop"}]}),
        ));
        let mut phi = base.phi.clone();
        if mode == "invalid_policy" {
            phi.hooks
                .register_hook(Hook::user_prompt_submit(|event| async move {
                    Ok(
                        crate::core::lifecycle::UserPromptSubmitOutcome::ContinueWithPolicy {
                            notices: Vec::new(),
                            prompt: event.prompt,
                            injected_context: Vec::new(),
                            tool_policy: TurnToolPolicy {
                                removals: if matches!(
                                    event.source,
                                    UserPromptSubmitSource::Internal
                                ) {
                                    vec!["MCP".into()]
                                } else {
                                    Vec::new()
                                },
                                ..Default::default()
                            },
                        },
                    )
                }));
        }
        phi.hooks
            .register_hook(Hook::prompt_context(|event| async move {
                Ok(crate::core::lifecycle::PromptContextOutcome {
                    contexts: if event.prompt == "complete" {
                        Vec::new()
                    } else {
                        vec![crate::context::InjectedContext {
                            source: crate::transcript::ContextSource::Hook,
                            content: format!("completion-context:{}", event.prompt),
                        }]
                    },
                    ..Default::default()
                })
            }));
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        phi.hooks.register_hook(Hook::stop({
            let calls = calls.clone();
            let entered = entered.clone();
            move |event| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(event.continuations, index);
                assert_eq!(event.response, "candidate answer");
                assert!(
                    serde_json::to_string(&event.transcript)
                        .unwrap()
                        .contains("candidate answer")
                );
                let entered = entered.clone();
                async move {
                    entered.notify_one();
                    match mode {
                        "cancel" => std::future::pending().await,
                        "invalid" => Ok(CompletionOutcome::Continue {
                            feedback: " ".into(),
                        }),
                        "error" => Err("acceptance unavailable".into()),
                        "fail" => Ok(CompletionOutcome::Fail {
                            reason: "acceptance rejected".into(),
                        }),
                        "continue_once" if index > 0 => Ok(CompletionOutcome::Allow),
                        _ => Ok(CompletionOutcome::Continue {
                            feedback: "tests still fail; fix them".into(),
                        }),
                    }
                }
            }
        }));
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let mut events = instance.subscribe();
        instance
            .submit(TurnPrompt {
                prompt: "complete".into(),
                source: UserPromptSubmitSource::Sdk,
                images: Vec::new(),
                injected_context: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        if mode == "cancel" {
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            base.manager.interrupt(instance.id()).await.unwrap();
        }
        let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        if mode != "cancel" {
            assert_eq!(
                view.snapshot.latest_error.is_none(),
                mode == "continue_once",
                "{mode}"
            );
        }
        if mode == "invalid_policy" {
            assert!(
                view.snapshot
                    .latest_error
                    .as_ref()
                    .unwrap()
                    .message
                    .contains("MCP tools")
            );
            assert!(
                !serde_json::to_string(&view.transcript)
                    .unwrap()
                    .contains("tests still fail; fix them")
            );
        }
        let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(observed.iter().filter(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, crate::PhiEvent::TurnFinished { .. }))).count(), usize::from(mode == "continue_once"));
        let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(
            requests.len(),
            match mode {
                "continue_once" => 2,
                "always_continue" => 9,
                _ => 1,
            }
        );
        if matches!(mode, "continue_once" | "always_continue") {
            let history = requests[1]["messages"].to_string();
            assert!(history.contains("tests still fail; fix them"));
            assert!(history.contains("completion-context:tests still fail; fix them"));
            let last = requests.last().unwrap()["messages"].to_string();
            assert_eq!(
                last.matches("completion-context:").count(),
                requests.len() - 1
            );
        }
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn incomplete_responses_never_execute_tools_or_publish_success() {
    for (reason, tool_call) in [
        ("length", true),
        ("length", false),
        ("content_filter", true),
        ("future_reason", false),
    ] {
        let root =
            std::path::Path::new("/tmp").join(format!("phi-termination-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, sender, move |request| {
            let resumed = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool");
            let delta = if resumed {
                json!({"content":"safe completion"})
            } else if tool_call {
                json!({"tool_calls":[{"index":0,"id":"unsafe-call","type":"function","function":{"name":"Read","arguments":json!({"path":"missing-file"}).to_string()}}]})
            } else {
                json!({"content":"incomplete answer"})
            };
            json!({"id":"response","model":"test","choices":[{"delta":delta,"finish_reason":if resumed { "stop" } else { reason }}]})
        }));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut phi = base.phi.clone();
        phi.hooks.register_hook(crate::hooks::Hook::pre_tool_use({
            let calls = calls.clone();
            move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(crate::core::lifecycle::PreToolUseOutcome::Continue) }
            }
        }));
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        let mut events = instance.subscribe();
        instance
            .submit(TurnPrompt {
                prompt: "complete".into(),
                source: UserPromptSubmitSource::Sdk,
                images: Vec::new(),
                injected_context: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{reason}");
        let continued = reason == "length" && tool_call;
        assert_eq!(view.snapshot.latest_error.is_none(), continued, "{reason}");
        let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(observed.iter().filter(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, crate::PhiEvent::TurnFinished { .. }))).count(), usize::from(continued));
        let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(requests.len(), if continued { 2 } else { 1 });
        if continued {
            assert!(requests[1]["messages"].to_string().contains("truncated"));
        }
        if tool_call {
            assert!(view.transcript.entries.iter().any(|entry| matches!(&entry.kind, TranscriptEntryKind::Message { message:AgentMessage::ToolResult { tool_call_id, is_error:true, .. } } if tool_call_id == "unsafe-call")));
        }
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}
#[tokio::test]
async fn bash_spills_output_before_canonical_commit_and_provider_request() {
    const PREVIEW_BYTE_LIMIT: usize = 50 * 1024;
    const PREVIEW_LINE_LIMIT: usize = 2_000;

    let root =
        std::path::Path::new("/tmp").join(format!("phi-bash-output-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        let replied = request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool");
        let delta = if replied {
            json!({"content":"finished"})
        } else {
            json!({"tool_calls":[{"index":0,"id":"large-bash","type":"function","function":{"name":"Bash","arguments":json!({"command":"printf '%060000d\\n' 0; for ((index=0; index<2100; index++)); do printf 'stderr\\n' >&2; done"}).to_string()}}]})
        };
        json!({"id":"response","model":"test","choices":[{"delta":delta,"finish_reason":if replied {"stop"} else {"tool_calls"}}]})
    }));
    let phi = base.phi.clone().with_runtime_services(
        base.phi.runtime_services.clone().permission_policy(
            crate::permission::PermissionConfig::default(),
            Some(root.clone()),
        ),
    );
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let mut events = instance.subscribe();
    instance
        .submit(TurnPrompt {
            prompt: "run the command".into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: TurnToolPolicy {
                allowances: vec![("Bash".into(), "**".into())],
                ..Default::default()
            },
        })
        .await
        .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(5), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    assert!(view.snapshot.latest_error.is_none());
    let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    let details = observed
        .iter()
        .find_map(|event| match event {
            AgentInstanceEvent::Phi(envelope) => match &envelope.event {
                crate::PhiEvent::ToolCallFinished {
                    details: Some(details),
                    ..
                } => details.output.as_ref(),
                _ => None,
            },
            _ => None,
        })
        .unwrap();
    assert!(details.truncated);
    assert_eq!(details.total_bytes, 74_701);
    assert_eq!(details.total_lines, 2_101);
    assert_eq!(details.exit_code, 0);
    let path = details.full_output_path.as_ref().unwrap();
    let full_output = std::fs::read(path).unwrap();
    assert_eq!(full_output.len(), details.total_bytes as usize);
    assert_eq!(
        full_output.iter().filter(|byte| **byte == b'0').count(),
        60_000
    );
    assert_eq!(
        full_output.iter().filter(|byte| **byte == b'\n').count(),
        2_101
    );
    let request = std::iter::from_fn(|| requests.try_recv().ok())
        .last()
        .unwrap();
    let tool_result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let preview = tool_result["content"].as_str().unwrap();
    let marker = format!("[Output truncated; full output: {}]\n", path.display());
    let body = preview.strip_prefix(&marker).expect("missing output path");
    assert!(body.len() <= PREVIEW_BYTE_LIMIT);
    assert!(body.lines().count() <= PREVIEW_LINE_LIMIT);
    assert!(full_output.ends_with(body.as_bytes()));
    let content = view
        .transcript
        .entries
        .iter()
        .find_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message {
                message:
                    AgentMessage::ToolResult {
                        tool_call_id,
                        content,
                        ..
                    },
            } if tool_call_id == "large-bash" => Some(content),
            _ => None,
        })
        .unwrap();
    let [crate::transcript::ContentBlock::Text { text }] = content.as_slice() else {
        panic!("expected text tool result");
    };
    assert_eq!(text.as_str(), preview);
    std::fs::remove_file(path).unwrap();
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

fn codemode_response(request: &serde_json::Value, code: &str) -> serde_json::Value {
    let messages = request["messages"].as_array().unwrap();
    let latest_user = messages
        .iter()
        .rposition(|message| message["role"] == "user")
        .unwrap();
    if messages[latest_user..]
        .iter()
        .any(|message| message["role"] == "tool")
    {
        completion(request)
    } else {
        json!({"id":"script", "model":"test", "choices":[{"delta":{"tool_calls":[{
            "index":0,"id":"script-call","type":"function","function":{"name":"Codemode","arguments":json!({"code":code}).to_string()}
        }]},"finish_reason":"tool_calls"}]})
    }
}

#[tokio::test]
async fn codemode_images_allow_continuation_and_replay() {
    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";
    for input in [vec!["text", "image"], vec!["text"]] {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-codemode-images-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let base = instance_for_provider(&root, address, false).await;
        let path = root.join("images-models.json");
        std::fs::write(&path, json!({"providers":{"local":{"api":"openai-completions",
            "baseUrl":format!("http://{address}/v1"),"models":{"test":{"contextWindow":128000,"input":input}}}}}).to_string()).unwrap();
        let catalog = crate::models::ModelCatalog::load(crate::models::ModelCatalogOptions {
            store_path: root.join("image-store.json"),
            user_paths: vec![path],
            offline: true,
        })
        .unwrap();
        let mut phi = base.phi.clone().with_runtime_services(
            base.phi
                .runtime_services
                .clone()
                .model_catalog(Arc::new(catalog)),
        );
        phi.cfg.extensions.codemode.enabled = true;
        let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(mock_provider(listener, sender, |request| {
            if request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["role"] == "tool")
            {
                completion(request)
            } else {
                codemode_response(
                    request,
                    &format!(
                        "text('before'); image('data:image/png;base64,{PNG}'); text('after');"
                    ),
                )
            }
        }));
        let instance = base
            .manager
            .insert(
                phi.agent_instance_builder()
                    .working_dir(root.clone())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        for prompt in ["show image", "replay image"] {
            instance
                .submit(TurnPrompt {
                    prompt: prompt.into(),
                    source: UserPromptSubmitSource::Sdk,
                    images: vec![],
                    injected_context: vec![],
                    tool_policy: Default::default(),
                })
                .await
                .unwrap();
            let view =
                tokio::time::timeout(Duration::from_secs(30), instance.wait_for_quiescence(None))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(
                view.snapshot.latest_error.is_none(),
                "{:?}",
                view.snapshot.latest_error
            );
        }
        let requests = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            let messages = request["messages"].as_array().unwrap();
            let tool = messages
                .iter()
                .position(|message| message["role"] == "tool")
                .unwrap();
            assert!(messages[tool]["content"].is_string());
            let attached_image = messages.iter().any(|message| {
                message["role"] == "user"
                    && message["content"].as_array().is_some_and(|content| {
                        content.iter().any(|block| {
                            block["type"] == "image_url"
                                && block["image_url"]["url"]
                                    .as_str()
                                    .is_some_and(|url| url.contains(PNG))
                        })
                    })
            });
            assert_eq!(attached_image, input.contains(&"image"));
            if input.contains(&"image") {
                assert_eq!(messages[tool + 1]["role"], "user");
                let content = messages[tool + 1]["content"].as_array().unwrap();
                let image = content
                    .iter()
                    .position(|block| block["type"] == "image_url")
                    .unwrap();
                assert!(content[..image].iter().any(|block| {
                    block["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("before"))
                }));
                assert!(content[image + 1..].iter().any(|block| {
                    block["text"]
                        .as_str()
                        .is_some_and(|text| text.contains("after"))
                }));
            }
        }
        let transcript = instance.view().await.unwrap().transcript;
        assert!(serde_json::to_string(&transcript).unwrap().contains(PNG));
        for entry in &transcript.entries {
            if let TranscriptEntryKind::Message {
                message: AgentMessage::ToolResult { content, .. },
            } = &entry.kind
            {
                for block in content {
                    if let crate::transcript::ContentBlock::Text { text } = block {
                        for line in text.lines() {
                            if let Some(path) = line
                                .strip_prefix("[Image saved to ")
                                .and_then(|line| line.strip_suffix(']'))
                            {
                                std::fs::remove_file(path).unwrap();
                            }
                        }
                    }
                }
            }
        }
        base.manager.shutdown().await.unwrap();
        server.abort();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn codemode_declarations_require_opt_in_and_respect_no_tools() {
    let root =
        std::path::Path::new("/tmp").join(format!("phi-codemode-opt-in-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, completion));
    for enabled in [false, true] {
        for no_tools in [false, true] {
            let mut phi = base.phi.clone();
            phi.cfg.extensions.codemode.enabled = enabled;
            phi.options.no_tools = no_tools;
            let instance = base
                .manager
                .insert(
                    phi.agent_instance_builder()
                        .working_dir(root.clone())
                        .build()
                        .await
                        .unwrap(),
                )
                .unwrap();
            instance
                .submit(TurnPrompt {
                    prompt: "check declarations".into(),
                    source: UserPromptSubmitSource::Sdk,
                    images: Vec::new(),
                    injected_context: Vec::new(),
                    tool_policy: Default::default(),
                })
                .await
                .unwrap();
            let view =
                tokio::time::timeout(Duration::from_secs(30), instance.wait_for_quiescence(None))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(view.snapshot.latest_error.is_none());
            let request = requests.try_recv().unwrap();
            let declared = request["tools"].as_array().is_some_and(|tools| {
                tools
                    .iter()
                    .any(|tool| tool["function"]["name"] == "Codemode")
            });
            assert_eq!(declared, enabled && !no_tools);
            if no_tools {
                assert!(request["tools"].as_array().is_none_or(Vec::is_empty));
            }
        }
    }
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn codemode_keeps_nested_data_out_of_history_and_restores_branch_store() {
    let root =
        std::path::Path::new("/tmp").join(format!("phi-codemode-flow-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("payload.txt"),
        "raw-tool-payload-must-not-reach-model",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, mut requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        let code = if request["messages"].to_string().contains("restore-cursor") {
            "return load('cursor');"
        } else {
            "const results = await Promise.all([tools.Read({path:'payload.txt'}), tools.Read({path:'payload.txt'})]); store('cursor', {count:results.length}); return 'filtered';"
        };
        let mut response = codemode_response(request, code);
        if let Some(function) = response["choices"][0]["delta"]["tool_calls"]
            .get_mut(0)
            .and_then(|call| call.get_mut("function"))
        {
            function["name"] = json!("codemode");
        }
        response
    }));
    let mut phi = base.phi.clone();
    phi.cfg.extensions.codemode.enabled = true;
    phi.cfg.extensions.codemode.cache = crate::config::CodemodeCache::Directory(root.join("cache"));
    let pre_calls = Arc::new(AtomicUsize::new(0));
    let release_reads = Arc::new(tokio::sync::Semaphore::new(0));
    phi.hooks.register_hook(Hook::pre_tool_use({
        let calls = pre_calls.clone();
        let release_reads = release_reads.clone();
        move |event| {
            let release_reads = release_reads.clone();
            let is_read = event.tool_name == "Read";
            if is_read {
                calls.fetch_add(1, Ordering::SeqCst);
            }
            async move {
                if is_read {
                    release_reads.acquire().await.unwrap().forget();
                }
                Ok(crate::core::lifecycle::PreToolUseOutcome::Continue)
            }
        }
    }));
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let mut events = instance.subscribe();
    instance
        .submit(TurnPrompt {
            prompt: "filter-files".into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    let mut observed_nested = 0;
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut parent = None;
        while observed_nested < 2 {
            if let AgentInstanceEvent::Phi(envelope) = events.recv().await.unwrap() {
                match envelope.event {
                    crate::PhiEvent::ToolCallStarted { id, .. } => parent = Some(id),
                    crate::PhiEvent::NestedToolCall { call } => {
                        assert!(matches!(
                            call.status,
                            crate::tools::NestedCallStatus::Running
                        ));
                        assert_eq!(Some(&call.parent_tool_call_id), parent.as_ref());
                        observed_nested += 1;
                    }
                    crate::PhiEvent::ToolCallFinished { .. } => {
                        panic!("script finished before nested starts")
                    }
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("nested starts must be published while tools are blocked");
    release_reads.add_permits(2);
    tokio::time::timeout(Duration::from_secs(30), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    let first = instance.transcript().await.unwrap();
    assert!(root.join("cache/modules").is_dir());
    assert_eq!(pre_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        crate::extension::codemode::read_store(&first).get("cursor"),
        Some(&json!({"count":2}))
    );
    let audit = first.entries.iter().filter(|entry| matches!(&entry.kind, TranscriptEntryKind::Event { name, .. } if name == "nested_tool_call")).count();
    assert_eq!(audit, 2);
    let parent = first
        .entries
        .iter()
        .find_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message {
                message: AgentMessage::ToolResult { details, .. },
            } => details.as_ref(),
            _ => None,
        })
        .unwrap();
    assert_eq!(parent.nested_calls.as_ref().unwrap().calls.len(), 2);
    let nested_events = std::iter::from_fn(|| events.try_recv().ok()).filter(|event| matches!(event,
        AgentInstanceEvent::Phi(envelope) if matches!(&envelope.event, crate::PhiEvent::NestedToolCall { .. }))).count();
    assert_eq!(nested_events + observed_nested, 4);
    let sent = std::iter::from_fn(|| requests.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(sent.len(), 2);
    assert!(
        sent[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "Codemode")
    );
    assert!(!sent[1]["messages"].to_string().contains("raw-tool-payload"));
    assert!(sent[1]["messages"].to_string().contains("filtered"));
    let restored: Transcript =
        serde_json::from_value(serde_json::to_value(&first).unwrap()).unwrap();
    let restored_manager = crate::harness::manager::AgentManager::new();
    let restored = restored_manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .transcript(restored)
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    restored
        .submit(TurnPrompt {
            prompt: "restore-cursor".into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), restored.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    let history = restored.transcript().await.unwrap();
    assert!(history.entries.iter().any(|entry| matches!(&entry.kind,
        TranscriptEntryKind::Message { message: AgentMessage::ToolResult { content, .. } } if serde_json::to_string(content).unwrap().contains("count"))));
    restored_manager.shutdown().await.unwrap();
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn codemode_file_calls_overlap_and_await_dependent_reads() {
    let root =
        std::path::Path::new("/tmp").join(format!("phi-codemode-files-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    for name in ["shared.txt", "left.txt", "right.txt", "dependent.txt"] {
        std::fs::write(root.join(name), "original").unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, _requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        codemode_response(
            request,
            r#"
            await Promise.all([tools.Read({path:'shared.txt'}), tools.Read({path:'shared.txt'})]);
            await Promise.all([tools.Read({path:'left.txt'}), tools.Read({path:'right.txt'})]);
            await Promise.all([tools.Write({path:'left.txt',content:'written'}), tools.Write({path:'right.txt',content:'written'})]);
            await Promise.all([tools.Edit({path:'left.txt',old_text:'written',new_text:'edited'}), tools.Write({path:'right.txt',content:'rewritten'})]);
            await Promise.all([tools.Edit({path:'left.txt',old_text:'edited',new_text:'done'}), tools.Edit({path:'right.txt',old_text:'rewritten',new_text:'done'})]);
            await tools.Edit({path:'dependent.txt',old_text:'original',new_text:'awaited-result'});
            return await tools.Read({path:'dependent.txt'});
        "#,
        )
    }));
    let mut phi = base.phi.clone();
    phi.cfg.extensions.codemode.enabled = true;
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let overlaps = Arc::new(AtomicUsize::new(0));
    phi.hooks.register_hook(Hook::pre_tool_use({
        let overlaps = overlaps.clone();
        move |event| {
            let barrier = barrier.clone();
            let overlaps = overlaps.clone();
            async move {
                if matches!(event.tool_name.as_str(), "Read" | "Write" | "Edit")
                    && event.tool_input["path"] != "dependent.txt"
                {
                    if tokio::time::timeout(Duration::from_secs(2), barrier.wait())
                        .await
                        .is_err()
                    {
                        return Ok(crate::core::lifecycle::PreToolUseOutcome::Deny {
                            reason: "independent file calls did not overlap".into(),
                        });
                    }
                    overlaps.fetch_add(1, Ordering::SeqCst);
                }
                Ok(crate::core::lifecycle::PreToolUseOutcome::Continue)
            }
        }
    }));
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let transcript = submit_prompt(&instance, "file-concurrency").await;
    let (output, is_error) = codemode_result(&transcript);
    assert!(!is_error, "{output}");
    assert!(output.contains("awaited-result"), "{output}");
    assert_eq!(overlaps.load(Ordering::SeqCst), 10);
    for name in ["left.txt", "right.txt"] {
        assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), "done");
    }
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn codemode_script_batches_preserve_store_updates() {
    let root = std::path::Path::new("/tmp")
        .join(format!("phi-codemode-store-batch-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("state.txt"), "state").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, _requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        let mut response = codemode_response(
            request,
            "const next = (load('count') ?? 0) + 1; await tools.Read({path:'state.txt'}); store('count', next); return next;",
        );
        if let Some(calls) = response["choices"][0]["delta"]["tool_calls"].as_array_mut() {
            let mut second = calls[0].clone();
            second["index"] = json!(1);
            second["id"] = json!("second-script-call");
            calls.push(second);
        }
        response
    }));
    let mut phi = base.phi.clone();
    phi.cfg.extensions.codemode.enabled = true;
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    let transcript = submit_prompt(&instance, "store-batch").await;
    let (output, is_error) = codemode_result(&transcript);
    assert!(!is_error, "{output}");
    assert_eq!(
        crate::extension::codemode::read_store(&transcript)["count"],
        2
    );
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

fn codemode_result(transcript: &Transcript) -> (&str, bool) {
    transcript
        .entries
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            TranscriptEntryKind::Message {
                message:
                    AgentMessage::ToolResult {
                        tool_name,
                        content,
                        is_error,
                        ..
                    },
            } if tool_name == "Codemode" => {
                let [crate::transcript::ContentBlock::Text { text }] = content.as_slice() else {
                    panic!("expected script text");
                };
                Some((text.as_str(), *is_error))
            }
            _ => None,
        })
        .expect("missing Codemode result")
}

#[tokio::test]
async fn codemode_inherits_permissions_removals_selection_and_plan_mode() {
    let root = std::path::Path::new("/tmp")
        .join(format!("phi-codemode-authority-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("payload.txt"), "allowed-data").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, _requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        codemode_response(
            request,
            "const listed = ALL_TOOLS.some(t => t.name === 'Read'); let read = false; try { await tools.Read({path:'payload.txt'}); read = true; } catch {} return {listed, read, write:'Write' in tools, edit:'Edit' in tools, recursive:'Codemode' in tools};",
        )
    }));
    for scenario in [
        "preapproval",
        "deny",
        "removal",
        "selection",
        "plan",
        "live-removal",
        "hook-deny",
    ] {
        let mut phi = base.phi.clone();
        phi.cfg.extensions.codemode.enabled = true;
        let permissions = if scenario == "preapproval" {
            json!({"mode":"ask"})
        } else if scenario == "deny" {
            json!({"mode":"allow","read":"deny"})
        } else {
            json!({"mode":"allow"})
        };
        phi.runtime_services = phi.runtime_services.clone().permission_policy(
            crate::permission::PermissionConfig::try_from_value(permissions).unwrap(),
            Some(root.clone()),
        );
        if scenario == "plan" {
            phi.cfg.agent_mode = crate::config::AgentMode::Plan;
        }
        if scenario == "live-removal" || scenario == "hook-deny" {
            phi.hooks
                .register_hook(Hook::pre_tool_use(move |event| async move {
                    if event.tool_name == "Read" {
                        if scenario == "hook-deny" {
                            return Ok(crate::core::lifecycle::PreToolUseOutcome::Deny {
                                reason: "blocked by hook".into(),
                            });
                        }
                        crate::permission::execution_scope::OuterTurnPolicy::merge_active(
                            &TurnToolPolicy {
                                removals: vec!["Read".into()],
                                ..Default::default()
                            },
                        );
                    }
                    Ok(crate::core::lifecycle::PreToolUseOutcome::Continue)
                }));
        }
        let mut builder = phi.agent_instance_builder().working_dir(root.clone());
        if scenario == "selection" {
            builder = builder.tools(["Codemode"]);
        }
        let instance = base.manager.insert(builder.build().await.unwrap()).unwrap();
        instance
            .submit(TurnPrompt {
                prompt: scenario.into(),
                source: UserPromptSubmitSource::Sdk,
                images: Vec::new(),
                injected_context: Vec::new(),
                tool_policy: TurnToolPolicy {
                    allowances: vec![
                        ("Codemode".into(), "**".into()),
                        ("Read".into(), "**".into()),
                    ],
                    removals: if scenario == "removal" {
                        vec!["Read".into()]
                    } else {
                        Vec::new()
                    },
                },
            })
            .await
            .unwrap();
        let view =
            tokio::time::timeout(Duration::from_secs(30), instance.wait_for_quiescence(None))
                .await
                .unwrap()
                .unwrap();
        let (output, is_error) = codemode_result(&view.transcript);
        assert!(!is_error, "{scenario}: {output}");
        let result: serde_json::Value =
            serde_json::from_str(output.split_once("Output:\n").unwrap().1).unwrap();
        assert_eq!(
            result["read"],
            scenario == "preapproval" || scenario == "plan",
            "{scenario}: {output}"
        );
        assert_eq!(
            result["listed"],
            scenario != "removal" && scenario != "selection",
            "{scenario}: {output}"
        );
        assert_eq!(
            result["write"],
            scenario != "plan" && scenario != "selection",
            "{scenario}"
        );
        assert_eq!(result["edit"], result["write"], "{scenario}");
        assert_eq!(result["recursive"], false);
    }
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn codemode_keeps_original_bash_evidence_and_structured_results() {
    let root =
        std::path::Path::new("/tmp").join(format!("phi-codemode-bash-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = instance_for_provider(&root, listener.local_addr().unwrap(), false).await;
    let (sender, _requests) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(mock_provider(listener, sender, |request| {
        codemode_response(
            request,
            "let blocked = false; try { await tools.Bash({command:'printf original'}); } catch { blocked = true; } const result = await tools.Bash({command:'printf structured; exit 7'}); return {blocked, exit:result.exit_code, output:result.output};",
        )
    }));
    let mut phi = base.phi.clone();
    phi.cfg.extensions.codemode.enabled = true;
    phi.runtime_services = phi.runtime_services.permission_policy(
        crate::permission::PermissionConfig::try_from_value(
            json!({"mode":"allow","bash":{"printf original":"deny"}}),
        )
        .unwrap(),
        Some(root.clone()),
    );
    phi.hooks
        .register_hook(Hook::pre_tool_use(|event| async move {
            if event.tool_name == "Bash" && event.tool_input["command"] == "printf original" {
                Ok(crate::core::lifecycle::PreToolUseOutcome::Rewrite {
                    tool_input: json!({"command":"printf rewritten"}),
                })
            } else {
                Ok(crate::core::lifecycle::PreToolUseOutcome::Continue)
            }
        }));
    let instance = base
        .manager
        .insert(
            phi.agent_instance_builder()
                .working_dir(root.clone())
                .build()
                .await
                .unwrap(),
        )
        .unwrap();
    instance
        .submit(TurnPrompt {
            prompt: "bash".into(),
            source: UserPromptSubmitSource::Sdk,
            images: Vec::new(),
            injected_context: Vec::new(),
            tool_policy: Default::default(),
        })
        .await
        .unwrap();
    let view = tokio::time::timeout(Duration::from_secs(30), instance.wait_for_quiescence(None))
        .await
        .unwrap()
        .unwrap();
    let (output, is_error) = codemode_result(&view.transcript);
    assert!(!is_error, "{output}");
    let result: serde_json::Value =
        serde_json::from_str(output.split_once("Output:\n").unwrap().1).unwrap();
    assert_eq!(
        result,
        json!({"blocked":true,"exit":7,"output":"structured"})
    );
    base.manager.shutdown().await.unwrap();
    server.abort();
    std::fs::remove_dir_all(root).unwrap();
}
