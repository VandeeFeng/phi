use super::*;
use phi_sdk::hooks::Hook;
use phi_sdk::lifecycle::SessionStartReason;
use phi_sdk::workflow::{WorkflowLimits, WorkflowRequest, WorkflowRun, WorkflowStatus};

#[derive(Clone, Copy)]
enum FinalizationScope {
    Session,
    AbandonedSession,
    AbandonedControl,
    Process,
    AbandonedProcess,
    PersistenceRetry,
    ProcessPersistenceRetry,
    AbandonedLaunch,
    Completed,
    CreatorClosed,
}

#[tokio::test]
async fn model_workflow_launch_uses_governance_and_session_capture() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::time::timeout(Duration::from_secs(30), async {
        for scenario in ["default", "disabled", "configured", "allowed", "denied", "removed", "no_tools", "config_no_tools", "invalid", "unsupported", "null", "print", "rpc", "worker", "slash"] {
            let root = std::path::PathBuf::from("/tmp")
                .join(format!("phi-workflow-tool-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            if scenario == "worker" {
                std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
                std::fs::write(root.join(".phi/agents/probe.md"), "---\nname: probe\ndescription: Probe\n---\nworker-only-context").unwrap();
                std::fs::write(root.join(".phi/settings.json"), r#"{"defaultProvider":"local","defaultModel":"test","permission":{"mode":"allow"},"extensions":["!*.lua"],"memory":{"enabled":false}}"#).unwrap();
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let models = root.join("models.json");
            std::fs::write(&models, serde_json::json!({"providers":{"local":{
                "api":"openai-completions", "baseUrl":format!("http://{}/v1", listener.local_addr().unwrap()),
                "models":{"test":{"contextWindow":128000}}
            }}}).to_string()).unwrap();
            let source = if scenario == "invalid" {
                "export const meta = {name: 'bad', description: 'invalid'}; import('x');"
            } else if scenario == "worker" {
                "export const meta = {name: 'worker', description: 'worker history'}; phase('inspect'); log('inspection started'); return await agent('private-worker-input', {role: 'probe', label: 'one'});"
            } else {
                "export const meta = {name: 'model', description: 'captured launch'}; return {missing: args === undefined, value: args};"
            };
            let mut arguments = serde_json::json!({"source": source});
            if scenario == "null" {
                arguments["args"] = serde_json::Value::Null;
            } else if scenario == "unsupported" {
                arguments["max_concurrent_workers"] = serde_json::json!(1000);
            }
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (worker_started, worker_ready) = tokio::sync::oneshot::channel();
            let (worker_release, worker_continue) = tokio::sync::oneshot::channel();
            let provider = tokio::spawn({
                let requests = requests.clone();
                async move {
                    let mut first = true;
                    let mut worker_started = Some(worker_started);
                    let mut worker_continue = Some(worker_continue);
                    loop {
                        let (mut connection, _) = listener.accept().await.unwrap();
                        let mut bytes = Vec::new();
                        let request = loop {
                            let mut buffer = [0; 4096];
                            assert_ne!(connection.read(&mut buffer).await.unwrap(), 0);
                            bytes.extend_from_slice(&buffer);
                            if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                                let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
                                let length: usize = headers.lines().find_map(|line| line.strip_prefix("content-length: ")).unwrap().parse().unwrap();
                                if bytes.len() >= start + 4 + length {
                                    break serde_json::from_slice::<serde_json::Value>(&bytes[start + 4..start + 4 + length]).unwrap();
                                }
                            }
                        };
                        let worker = request["messages"].to_string().contains("worker-only-context");
                        requests.lock().unwrap().push(request);
                        if worker {
                            worker_started.take().unwrap().send(()).unwrap();
                            worker_continue.take().unwrap().await.unwrap();
                        }
                        let delta = if first {
                            first = false;
                            serde_json::json!({"tool_calls":[{"index":0,"id":"workflow-launch","type":"function",
                                "function":{"name":"Workflow","arguments":arguments.to_string()}}]})
                        } else {
                            serde_json::json!({"content":if worker {"private-worker-output"} else {"done"}})
                        };
                        let reply = serde_json::json!({"id":"completion","model":"test","choices":[{
                            "delta":delta,"finish_reason":if delta.get("tool_calls").is_some() {"tool_calls"} else {"stop"}
                        }]});
                        let body = format!("data: {reply}\n\ndata: [DONE]\n\n");
                        connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    }
                }
            });
            let catalog = Arc::new(phi_sdk::models::ModelCatalog::load(phi_sdk::models::ModelCatalogOptions {
                store_path: root.join("store.json"), user_paths: vec![models], offline: true,
            }).unwrap());
            let permission = phi_sdk::permission::PermissionConfig::try_from_value(serde_json::json!({
                "mode":"allow", "tools":{"Workflow":if scenario == "denied" {"deny"} else {"allow"}}
            })).unwrap();
            let resources = RuntimeResources::new_for(
                PhiRuntimeServices::new().model_catalog(catalog.clone()).permission_policy(permission, Some(root.clone())),
                Arc::new(ExtensionRuntime::load_with_paths(Vec::new())), catalog.clone(),
                Sandbox::new(false, root.clone()).unwrap(), &root,
            ).extension_paths(Vec::new());
            let factory = AgentRuntimeFactory::new(resources);
            let runtime = TestRuntime::new(factory.clone());
            let mut cli = if matches!(scenario, "default" | "disabled" | "configured" | "config_no_tools" | "slash") {
                Cli::parse_from(["phi"])
            } else {
                Cli::parse_from(["phi", "--workflow"])
            };
            cli.no_tools = scenario == "no_tools";
            cli.print = scenario == "print";
            cli.rpc = scenario == "rpc";
            runtime.preparation.enable_subagents(&runtime.sessions);
            let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
            let mut configuration = settings_manager.settings();
            configuration.memory = Some(phi_sdk::extension::memory::MemoryConfig { enabled: false, ..Default::default() });
            let mut session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            configuration.agent.workflow = match scenario {
                "default" => None,
                "configured" | "config_no_tools" | "slash" => Some(true),
                _ => Some(false),
            };
            configuration.no_tools = Some(scenario == "config_no_tools");
            configuration.permission = Some(serde_json::json!({
                "mode": "allow", "tools": {"Workflow": if scenario == "denied" { "deny" } else { "allow" }}
            }));
            configuration.extensions = Some(vec!["!*.lua".into()]);
            let build_options = |configuration: &Settings| RuntimeRebuildOptions {
                phi: PhiBuildOptions {
                    cli: &cli, provider: "local", model: "test", runtime_cfg: configuration.runtime_config(&catalog),
                    context: phi_sdk::context::load_with_options_for(false, None, vec![root.join(".phi/agents")], &root),
                    agent_options: crate::agent_options_from_cli(&cli, configuration), hooks: Vec::new(),
                },
                instance: {
                    let mut instance = AgentInstanceBuildOptions::from_session(&session);
                    instance.settings_manager = Some(settings_manager.clone());
                    instance.resource_settings = Some(Arc::new(configuration.clone()));
                    instance
                },
            };
            let binding = runtime.sessions.create(&runtime.preparation, session.clone(), build_options(&configuration), SessionStartReason::New, true).await.unwrap();
            let replacement_seed = binding.session().await;
            if scenario == "allowed" {
                let previous_generation = binding.runtime().generation_id().to_string();
                let prepared = factory.prepare(build_options(&configuration)).await.unwrap();
                binding.replace_runtime(prepared, binding.session().await, None, false).await.unwrap();
                assert_ne!(binding.runtime().generation_id(), previous_generation);
                assert!(binding.launch_workflow(
                    runtime.workflows.clone(), WorkflowRequest::parse(source, None).await.unwrap(),
                    None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), Some(&previous_generation),
                ).await.err().unwrap().to_string().contains("generation"));
                assert!(binding.session().await.workflow_run_ids.is_empty());
            }
            assert_eq!(binding.commands().iter().any(|command| command.name == "workflow"), configuration.agent.workflow == Some(true), "{scenario}");
            let instance = binding.instance();
            for input in ["/workflow", "/workflow  \n", "/workflow inspect"] {
                let unavailable = configuration.agent.workflow != Some(true) || matches!(scenario, "no_tools" | "config_no_tools" | "removed");
                if !input.trim_end().eq("/workflow") && !unavailable {
                    continue;
                }
                let error = binding.submit(phi_sdk::turn::TurnPrompt {
                    prompt: input.into(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                    tool_policy: phi_sdk::turn::TurnToolPolicy {
                        removals: if scenario == "removed" { vec!["Workflow".into()] } else { Vec::new() },
                        ..Default::default()
                    },
                    injected_context: Vec::new(), images: Vec::new(),
                }, None, None).await.err().expect("invalid Workflow input must fail before admission");
                assert!(error.to_string().contains(if input.trim_end() == "/workflow" { "Usage: /workflow <task>" } else { "Workflow is unavailable" }), "{scenario}: {error}");
                assert!(requests.lock().unwrap().is_empty());
            }
            binding.submit(phi_sdk::turn::TurnPrompt {
                prompt: if scenario == "slash" { "/workflow launch the workflow" } else { "launch the workflow" }.into(),
                source: phi_sdk::lifecycle::UserPromptSubmitSource::Sdk,
                tool_policy: phi_sdk::turn::TurnToolPolicy {
                    removals: if scenario == "removed" { vec!["Workflow".into()] } else { Vec::new() },
                    ..Default::default()
                },
                injected_context: Vec::new(), images: Vec::new(),
            }, None, None).await.unwrap();
            if scenario == "worker" {
                worker_ready.await.unwrap();
                let children = binding.subagents().await.unwrap();
                assert_eq!(children.len(), 1);
                assert_eq!(children[0].outcome, crate::session::SubagentOutcome::Running);
                worker_release.send(()).unwrap();
            }
            instance.wait_for_quiescence(None).await.unwrap();
            let accepted = matches!(scenario, "allowed" | "configured" | "null" | "worker" | "print" | "rpc" | "slash");
            if scenario == "configured" {
                binding.settle_workflow_deliveries().await.unwrap();
                instance.wait_for_quiescence(None).await.unwrap();
                assert_eq!(requests.lock().unwrap().len(), 2, "launch receipt must not request a model continuation");
            }
            let runs = runtime.workflows.runs_owned(instance.registration_id().unwrap()).unwrap();
            assert_eq!(runs.len(), usize::from(accepted), "{scenario}");
            let advertised = requests.lock().unwrap()[0]["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "Workflow"));
            assert_eq!(advertised, !matches!(scenario, "default" | "disabled" | "removed" | "no_tools" | "config_no_tools"), "{scenario}");
            let guided = requests.lock().unwrap()[0]["messages"].as_array().unwrap().iter()
                .filter(|message| message["role"] == "system")
                .any(|message| message["content"].to_string().contains("Use Workflow for complex, parallel or multistage tasks"));
            assert_eq!(guided, advertised, "{scenario}");
            let cli_context = requests.lock().unwrap()[0]["messages"].to_string().contains("workflow_intro");
            assert_eq!(cli_context, (cli.workflow || scenario == "slash") && advertised, "{scenario}");
            if scenario == "slash" {
                let request = &requests.lock().unwrap()[0];
                let messages = request["messages"].to_string();
                assert!(!messages.contains("/workflow"));
                assert!(messages.contains("Workflow has been explicitly enabled."));
                assert!(!messages.contains("enabled via --workflow"));
            }
            let transcript = instance.transcript().await.unwrap();
            assert_eq!(transcript.current_branch().iter().filter(|entry| matches!(entry.kind,
                crate::session::TranscriptEntryKind::Message { message: crate::session::AgentMessage::User { .. } }
            )).count(), 1, "Workflow context must not create another input");
            assert_eq!(transcript.current_branch().iter().filter(|entry| matches!(&entry.kind,
                crate::session::TranscriptEntryKind::Message { message: crate::session::AgentMessage::Context { source, .. } }
                    if source.as_str() == "workflow_intro"
            )).count(), usize::from((cli.workflow || scenario == "slash") && advertised), "{scenario}");
            if let Some(run) = runs.first() {
                let mut observations = run.subscribe();
                while !matches!(observations.snapshot().status(), WorkflowStatus::Completed | WorkflowStatus::Failed) {
                    observations.changed().await.unwrap();
                }
                assert_eq!(run.snapshot().status(), WorkflowStatus::Completed);
                let storage = crate::session::storage::WorkflowStorage::new(&session, run.id()).unwrap();
                let record: serde_json::Value = serde_json::from_slice(&std::fs::read(storage.path()).unwrap()).unwrap();
                assert_eq!(record["source"], source);
                assert_eq!(record.as_object().unwrap().contains_key("args"), scenario == "null");
                assert_eq!(binding.session().await.workflow_run_ids, [run.id()]);
            } else {
                assert!(binding.session().await.workflow_run_ids.is_empty());
            }
            if scenario == "slash" {
                binding.settle_workflow_deliveries().await.unwrap();
                instance.wait_for_quiescence(None).await.unwrap();
                binding.submit(phi_sdk::turn::TurnPrompt {
                    prompt: "ordinary follow-up".into(),
                    source: phi_sdk::lifecycle::UserPromptSubmitSource::Interactive,
                    tool_policy: Default::default(), injected_context: Vec::new(), images: Vec::new(),
                }, None, None).await.unwrap();
                instance.wait_for_quiescence(None).await.unwrap();
                let transcript = instance.transcript().await.unwrap();
                assert_eq!(transcript.current_branch().iter().filter(|entry| matches!(&entry.kind,
                    crate::session::TranscriptEntryKind::Message { message: crate::session::AgentMessage::Context { source, .. } }
                        if source.as_str() == "workflow_intro"
                )).count(), 1, "ordinary follow-up must not inject another intro");
                assert!(requests.lock().unwrap().last().unwrap()["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "Workflow"));
            }
            if matches!(scenario, "configured" | "allowed") {
                binding.settle_workflow_deliveries().await.unwrap();
                instance.wait_for_quiescence(None).await.unwrap();
                for configured in [Some(false), None, Some(true)] {
                    let mut changed = configuration.clone();
                    changed.agent.workflow = configured;
                    let prepared = factory.prepare(build_options(&changed)).await.unwrap();
                    binding.replace_runtime(prepared, replacement_seed.clone(), None, false).await.unwrap();
                    assert_eq!(binding.session().await.workflow_run_ids, runs.iter().map(|run| run.id()).collect::<Vec<_>>());
                    binding.submit(phi_sdk::turn::TurnPrompt {
                        prompt: "check accepted tool exposure".into(),
                        source: phi_sdk::lifecycle::UserPromptSubmitSource::Sdk,
                        tool_policy: Default::default(), injected_context: Vec::new(), images: Vec::new(),
                    }, None, None).await.unwrap();
                    instance.wait_for_quiescence(None).await.unwrap();
                    let requests = requests.lock().unwrap();
                    let request = requests.last().unwrap();
                    let advertised = request["tools"].as_array().is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == "Workflow"));
                    let guided = request["messages"].as_array().unwrap().iter()
                        .filter(|message| message["role"] == "system")
                        .any(|message| message["content"].to_string().contains("Use Workflow for complex, parallel or multistage tasks"));
                    assert_eq!(binding.commands().iter().any(|command| command.name == "workflow"), configured == Some(true));
                    assert_eq!(advertised, cli.workflow || configured.unwrap_or(false));
                    assert_eq!(guided, advertised);
                    assert_eq!(request["messages"].to_string().contains("workflow_intro"), cli.workflow && advertised);
                }
            }
            if scenario == "worker" {
                let requests = requests.lock().unwrap();
                let request = requests.iter().find(|request| request["messages"].to_string().contains("worker-only-context")).unwrap();
                assert!(request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == "Workflow"));
                assert!(!request["messages"].to_string().contains("workflow_intro"));
                drop(requests);
                binding.settle_workflow_deliveries().await.unwrap();
                instance.wait_for_quiescence(None).await.unwrap();
                let attachment = crate::runtime::attachment::AgentAttachment::new(
                    &binding, runtime.sessions.session_access(instance.id()), runtime.preparation.clone(),
                );
                let registration = instance.registration_id().unwrap();
                let before = serde_json::to_string(&instance.view().await.unwrap().transcript).unwrap();
                let views = attachment.workflow_views(registration).unwrap();
                assert_eq!(views.len(), 1);
                assert_eq!(views[0].name, "worker");
                assert_eq!(views[0].capture, crate::runtime::session::WorkflowCapture::Complete);
                let worker = views[0].snapshot.invocations()[0].worker().unwrap();
                let children = attachment.subagents().await.unwrap();
                assert_eq!(children[0].child_agent_id, worker.instance_id().as_str());
                assert_eq!(children[0].outcome, crate::session::SubagentOutcome::Finished {
                    result: "private-worker-output".into(), is_error: false,
                });
                assert!(attachment.workflow_views(worker.registration().unwrap()).is_err());
                let child = attachment.subagent_view(worker.instance_id().as_str()).await.unwrap();
                let history = serde_json::to_string(&child.session.transcript).unwrap();
                assert!(history.contains("private-worker-input"));
                assert!(history.contains("private-worker-output"));
                assert_eq!(child.session.id.as_str(), worker.instance_id().as_str());
                assert_eq!(serde_json::to_string(&instance.view().await.unwrap().transcript).unwrap(), before);
            }
            runtime.sessions.shutdown(SessionEndReason::Quit).await.unwrap();
            let saved = crate::session::storage::load_session(crate::session::storage::session_path(&session).to_str().unwrap()).unwrap();
            let history = serde_json::to_string(&saved.transcript).unwrap();
            assert_eq!(history.matches("phi_sdk.workflow_feedback").count(), usize::from(accepted), "{scenario}");
            if let Some(run) = runs.first() {
                assert!(history.contains("run_id"));
                let storage = crate::session::storage::WorkflowStorage::new(&session, run.id()).unwrap();
                let record: serde_json::Value = serde_json::from_slice(&std::fs::read(storage.path()).unwrap()).unwrap();
                assert_eq!(record["version"], 2);
                if scenario == "worker" {
                    assert_eq!(record["state"]["value"], "private-worker-output");
                    let invocations = record["state"]["invocations"].as_array().unwrap();
                    assert_eq!(invocations.len(), 1);
                    let invocation = &invocations[0];
                    assert_eq!(invocation["id"], 0);
                    assert_eq!(invocation["role"], "probe");
                    assert_eq!(invocation["phase"], "inspect");
                    assert_eq!(invocation["label"], "one");
                    assert_eq!(invocation["status"], "succeeded");
                    assert_eq!(invocation["value"], "private-worker-output");
                    assert_eq!(invocation["worker_instance_id"], invocation["worker_session"]["child_agent_id"]);
                    let link = &saved.children[0];
                    assert_eq!(link.workflow_run_id, Some(run.id()));
                    assert_eq!(link.parent_tool_call_id.as_deref(), Some("workflow-launch"));
                    assert_eq!(crate::session::SubagentChildLink::from(link).outcome, crate::session::SubagentOutcome::Finished {
                        result: "private-worker-output".into(), is_error: false,
                    });
                    assert_eq!(link.child_agent_id, invocation["worker_instance_id"].as_str().unwrap());
                    let child_path = crate::session::storage::session_path(&session).parent().unwrap().join(session.id.as_str()).join("subagents").join(format!("agent-{}.jsonl", link.child_session_id));
                    let child = crate::session::storage::load_session(child_path.to_str().unwrap()).unwrap();
                    let child_history = serde_json::to_string(&child.transcript).unwrap();
                    assert!(child_history.contains("private-worker-input"));
                    assert!(child_history.contains("private-worker-output"));
                    let saved_path = crate::session::storage::session_path(&session);
                    for (pointer, replacement) in [
                        ("/creator_id", serde_json::json!("another-session")),
                        ("/run_id", serde_json::json!(uuid::Uuid::new_v4().to_string())),
                        ("/state/invocations/0/worker_instance_id", serde_json::json!("another-worker")),
                        ("/state/invocations/0/worker_session/child_session_id", serde_json::json!("another-child-session")),
                        ("/state/invocations/0/worker_session/child_agent_id", serde_json::json!("another-worker")),
                        ("/state/status", serde_json::json!("prepared")),
                        ("/version", serde_json::json!(0)),
                    ] {
                        let mut unrelated = record.clone();
                        *unrelated.pointer_mut(pointer).unwrap() = replacement;
                        storage.save(&unrelated).unwrap();
                        let loaded = crate::session::storage::load_session(saved_path.to_str().unwrap()).unwrap();
                        assert_eq!(loaded.children[0].parent_tool_call_id, None, "{pointer}");
                        assert_eq!(crate::session::SubagentChildLink::from(&loaded.children[0]).outcome, crate::session::SubagentOutcome::Interrupted, "{pointer}");
                    }
                    std::fs::remove_file(storage.path()).unwrap();
                    let loaded = crate::session::storage::load_session(saved_path.to_str().unwrap()).unwrap();
                    assert_eq!(loaded.children[0].parent_tool_call_id, None);
                    assert_eq!(crate::session::SubagentChildLink::from(&loaded.children[0]).outcome, crate::session::SubagentOutcome::Interrupted);
                    storage.save(&record).unwrap();
                    let restored = crate::session::storage::load_session_id(session.id.as_str()).unwrap();
                    assert_eq!(restored.children[0].parent_tool_call_id.as_deref(), Some("workflow-launch"));
                    assert_eq!(crate::session::SubagentChildLink::from(&restored.children[0]).outcome, crate::session::SubagentOutcome::Finished {
                        result: "private-worker-output".into(), is_error: false,
                    });
                    crate::session::storage::save_session(&restored).unwrap();
                    let explicit = crate::session::storage::load_session_id(session.id.as_str()).unwrap();
                    assert_eq!(crate::session::SubagentChildLink::from(&explicit.children[0]).outcome, crate::session::SubagentChildLink::from(&restored.children[0]).outcome);
                    assert!(!serde_json::to_string(&restored.children).unwrap().contains("private-worker-output"));
                    let resumed_runtime = TestRuntime::new(factory.clone());
                    let resumed = resumed_runtime.sessions.create(
                        &resumed_runtime.preparation,
                        restored.clone(),
                        RuntimeRebuildOptions {
                            instance: AgentInstanceBuildOptions::from_session(&restored),
                            ..build_options(&configuration)
                        },
                        SessionStartReason::Resume,
                        true,
                    ).await.unwrap();
                    assert_eq!(resumed.subagents().await.unwrap()[0].outcome, crate::session::SubagentChildLink::from(&restored.children[0]).outcome);
                    assert!(resumed.activity_agents().await.unwrap().is_empty());
                    let registration = resumed.instance().registration_id().unwrap();
                    assert!(resumed.workflow_views(registration).unwrap().is_empty());
                    assert_eq!(record["state"]["phase"], "inspect");
                    assert_eq!(record["state"]["logs"], serde_json::json!(runs[0].snapshot().logs()));
                    assert!(!runs[0].snapshot().logs().is_empty());
                    let resumed_access = resumed_runtime.sessions.session_access(resumed.instance().id());
                    assert_eq!(resumed_access.subagent_view(link.child_agent_id.as_str()).await.unwrap().session.id, link.child_session_id);
                    assert_eq!(resumed_runtime.manager.discover().unwrap().len(), 1);
                    resumed_runtime.sessions.shutdown(SessionEndReason::Quit).await.unwrap();
                    let mut unrelated = restored.clone();
                    unrelated.children[0].parent_tool_call_id = Some("another-call".into());
                    crate::session::storage::save_session(&unrelated).unwrap();
                    let loaded = crate::session::storage::load_session_id(session.id.as_str()).unwrap();
                    assert_eq!(loaded.children[0].parent_tool_call_id.as_deref(), Some("another-call"));
                    assert_eq!(crate::session::SubagentChildLink::from(&loaded.children[0]).outcome, crate::session::SubagentOutcome::Interrupted);
                    crate::session::storage::save_session(&restored).unwrap();
                    std::fs::write(storage.path(), "invalid record").unwrap();
                    let loaded = crate::session::storage::load_session_id(session.id.as_str()).unwrap();
                    assert_eq!(loaded.children[0].parent_tool_call_id.as_deref(), Some("workflow-launch"));
                    assert_eq!(crate::session::SubagentChildLink::from(&loaded.children[0]).outcome, crate::session::SubagentOutcome::Interrupted);
                    assert_eq!(runtime.manager.discover().unwrap().len(), 0);
                } else {
                assert!(record["state"]["invocations"].as_array().unwrap().is_empty());
                assert_eq!(record["state"]["value"], if scenario == "null" {
                    serde_json::json!({"missing": false, "value": null})
                } else {
                    serde_json::json!({"missing": true})
                });
                }
            }
            provider.abort();
            let _ = provider.await;
            std::fs::remove_dir_all(root).unwrap();
        }
    }).await.expect("model Workflow launch timed out");
}

#[tokio::test]
async fn workflow_settles_before_session_end_for_stop_and_shutdown() {
    tokio::time::timeout(Duration::from_secs(30), async {
        for scope in [
            FinalizationScope::Session,
            FinalizationScope::AbandonedSession,
            FinalizationScope::AbandonedControl,
            FinalizationScope::Process,
            FinalizationScope::AbandonedProcess,
            FinalizationScope::PersistenceRetry,
            FinalizationScope::ProcessPersistenceRetry,
            FinalizationScope::AbandonedLaunch,
            FinalizationScope::Completed,
            FinalizationScope::CreatorClosed,
        ] {
            let root = std::path::PathBuf::from("/tmp")
                .join(format!("phi-workflow-finalization-{}", uuid::Uuid::new_v4()));
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
            let runtime = TestRuntime::new(AgentRuntimeFactory::new(resources));
            let expected_status = if matches!(scope, FinalizationScope::Completed) { WorkflowStatus::Completed } else { WorkflowStatus::Stopped };
            let observed_run = Arc::new(std::sync::Mutex::new(None::<WorkflowRun>));
            let end_statuses = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded_path = Arc::new(std::sync::Mutex::new(None::<std::path::PathBuf>));
            let hook = Hook::session_end({
                let observed_run = observed_run.clone();
                let end_statuses = end_statuses.clone();
                let recorded_path = recorded_path.clone();
                move |_| {
                    let status = observed_run
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .snapshot()
                        .status();
                    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(recorded_path.lock().unwrap().as_ref().unwrap()).unwrap()).unwrap();
                    assert_eq!(record["state"]["status"], "settled");
                    assert!(matches!(record["state"]["feedback"].as_str(), Some("admitted" | "creator_closed")));
                    assert_eq!(record["state"]["execution_status"], serde_json::to_value(expected_status).unwrap());
                    if expected_status == WorkflowStatus::Completed {
                        assert_eq!(record["state"]["value"], serde_json::json!({"output": null}));
                    }
                    end_statuses.lock().unwrap().push(status);
                    async { Ok(Vec::new()) }
                }
            });
            let cli = Cli::parse_from(["phi"]);
            let mut configuration = crate::config::SettingsManager::load_for(&root).unwrap().settings();
            configuration.memory = Some(phi_sdk::extension::memory::MemoryConfig {
                enabled: false,
                ..Default::default()
            });
            let mut session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
            session.working_dir = root.to_string_lossy().into();
            let binding = runtime
                .sessions
                .create(
                    &runtime.preparation,
                    session.clone(),
                    RuntimeRebuildOptions {
                        phi: PhiBuildOptions {
                            cli: &cli,
                            provider: "local",
                            model: "test",
                            runtime_cfg: configuration.runtime_config(&catalog),
                            context: phi_sdk::context::load(true),
                            agent_options: crate::AgentBuildOptions::default(),
                            hooks: vec![hook],
                        },
                        instance: AgentInstanceBuildOptions::from_session(&session),
                    },
                    SessionStartReason::New,
                    true,
                )
                .await
                .unwrap();
            let peer = if matches!(scope, FinalizationScope::ProcessPersistenceRetry) {
                let mut peer_session = crate::session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
                peer_session.working_dir = session.working_dir.clone();
                Some(runtime.sessions.create(
                    &runtime.preparation,
                    peer_session.clone(),
                    RuntimeRebuildOptions {
                        phi: PhiBuildOptions {
                            cli: &cli,
                            provider: "local",
                            model: "test",
                            runtime_cfg: configuration.runtime_config(&catalog),
                            context: phi_sdk::context::load(true),
                            agent_options: crate::AgentBuildOptions::default(),
                            hooks: Vec::new(),
                        },
                        instance: AgentInstanceBuildOptions::from_session(&peer_session),
                    },
                    SessionStartReason::New,
                    true,
                ).await.unwrap())
            } else {
                None
            };
            let source = if matches!(scope, FinalizationScope::Completed) {
                "export const meta = {name: 'done', description: 'completed script'}; return {output: args};"
            } else {
                "export const meta = {name: 'busy', description: 'cancellable script'}; while (true) {}"
            };
            let request = WorkflowRequest::parse(source, Some(serde_json::Value::Null)).await.unwrap();
            let directory = crate::session::storage::session_path(&session).parent().unwrap().join(session.id.as_str()).join("workflows");
            std::fs::create_dir_all(directory.parent().unwrap()).unwrap();
            std::fs::write(&directory, b"block snapshot writes").unwrap();
            assert!(binding.launch_workflow(runtime.workflows.clone(), request.clone(), None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), None).await.is_err());
            assert!(runtime.workflows.runs_owned(binding.instance().registration_id().unwrap()).unwrap().is_empty());
            binding.instance().reset_context(None).await.unwrap();
            std::fs::remove_file(&directory).unwrap();
            let run = if matches!(scope, FinalizationScope::AbandonedLaunch) {
                let mut launch = Box::pin(binding.launch_workflow(runtime.workflows.clone(), request, None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), None));
                std::future::poll_fn(|context| {
                    assert!(std::future::Future::poll(launch.as_mut(), context).is_pending());
                    std::task::Poll::Ready(())
                }).await;
                drop(launch);
                loop {
                    if let Some(run) = runtime.workflows.runs_owned(binding.instance().registration_id().unwrap()).unwrap().pop() {
                        break run;
                    }
                    tokio::task::yield_now().await;
                }
            } else {
                binding.launch_workflow(runtime.workflows.clone(), request, None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), None).await.unwrap()
            };
            let storage = crate::session::storage::WorkflowStorage::new(&session, run.id()).unwrap();
            *recorded_path.lock().unwrap() = Some(storage.path().to_owned());
            let record: serde_json::Value = serde_json::from_slice(&std::fs::read(storage.path()).unwrap()).unwrap();
            assert_eq!(record["source"], source);
            assert_eq!(record["args"], serde_json::Value::Null);
            assert!(record.as_object().unwrap().contains_key("args"));
            if !matches!(scope, FinalizationScope::Completed) {
                assert_eq!(record["state"]["status"], "prepared");
            }
            let saved = crate::session::storage::load_session(crate::session::storage::session_path(&session).to_str().unwrap()).unwrap();
            assert_eq!(saved.workflow_run_ids, [run.id()]);
            *observed_run.lock().unwrap() = Some(run.clone());
            match scope {
                FinalizationScope::Session | FinalizationScope::AbandonedLaunch | FinalizationScope::Completed => {
                    if matches!(scope, FinalizationScope::Completed) {
                        let mut observations = run.subscribe();
                        while observations.snapshot().status() != WorkflowStatus::Completed {
                            observations.changed().await.unwrap();
                        }
                    }
                    runtime
                        .sessions
                        .stop(binding.instance().id(), SessionEndReason::New)
                        .await
                        .unwrap();
                }
                FinalizationScope::AbandonedSession => {
                    let id = binding.instance().id().clone();
                    let mut stop = Box::pin(runtime.sessions.stop(&id, SessionEndReason::Quit));
                    std::future::poll_fn(|context| {
                        assert!(std::future::Future::poll(stop.as_mut(), context).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    drop(stop);
                    while runtime.sessions.has_associations().unwrap() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                FinalizationScope::AbandonedControl => {
                    let registration = binding.instance().registration_id().unwrap();
                    let mut stop = Box::pin(binding.stop_workflow(registration, run.id()));
                    std::future::poll_fn(|context| {
                        assert!(std::future::Future::poll(stop.as_mut(), context).is_pending());
                        std::task::Poll::Ready(())
                    }).await;
                    drop(stop);
                    let mut observations = run.subscribe();
                    while observations.snapshot().status() != WorkflowStatus::Stopped {
                        observations.changed().await.unwrap();
                    }
                    runtime.sessions.stop(binding.instance().id(), SessionEndReason::Quit).await.unwrap();
                }
                FinalizationScope::CreatorClosed => {
                    runtime.manager.shutdown().await.unwrap();
                    runtime.sessions.shutdown(SessionEndReason::Quit).await.unwrap();
                }
                FinalizationScope::Process => {
                    runtime.sessions.shutdown(SessionEndReason::Quit).await.unwrap();
                }
                FinalizationScope::PersistenceRetry | FinalizationScope::ProcessPersistenceRetry => {
                    std::fs::remove_file(storage.path()).unwrap();
                    std::fs::create_dir(storage.path()).unwrap();
                    let result = if matches!(scope, FinalizationScope::ProcessPersistenceRetry) {
                        runtime.sessions.shutdown(SessionEndReason::Quit).await.map(|_| ())
                    } else {
                        runtime.sessions.stop(binding.instance().id(), SessionEndReason::Quit).await.map(|_| ())
                    };
                    assert!(result.unwrap_err().root_cause().is::<std::io::Error>());
                    assert_eq!(binding.instance().snapshot().status, AgentInstanceStatus::Stopped);
                    if let Some(peer) = &peer {
                        assert_eq!(peer.instance().snapshot().status, AgentInstanceStatus::Stopped);
                        assert!(runtime.sessions.get(peer.instance().id()).is_err());
                    }
                    assert!(runtime.sessions.has_associations().unwrap());
                    assert!(end_statuses.lock().unwrap().is_empty());
                    assert_eq!(run.snapshot().status(), WorkflowStatus::Stopped);
                    let views = binding.workflow_views(binding.instance().registration_id().unwrap()).unwrap();
                    assert!(matches!(views[0].capture, crate::runtime::session::WorkflowCapture::Failed(_)));
                    std::fs::remove_dir(storage.path()).unwrap();
                    let changed = binding.workflow_changed();
                    let (first, second) = tokio::join!(binding.settle_workflow_deliveries(), binding.settle_workflow_deliveries());
                    first.unwrap();
                    second.unwrap();
                    tokio::time::timeout(Duration::from_secs(1), changed).await.unwrap();
                    let views = binding.workflow_views(binding.instance().registration_id().unwrap()).unwrap();
                    assert_eq!(views[0].capture, crate::runtime::session::WorkflowCapture::Complete);
                    runtime.sessions.shutdown(SessionEndReason::Quit).await.unwrap();
                    let saved_record = storage.path().with_extension("saved");
                    std::fs::rename(storage.path(), &saved_record).unwrap();
                    std::fs::create_dir(storage.path()).unwrap();
                    binding.settle_workflow_deliveries().await.unwrap();
                    std::fs::remove_dir(storage.path()).unwrap();
                    std::fs::rename(saved_record, storage.path()).unwrap();
                }
                FinalizationScope::AbandonedProcess => {
                    let mut shutdown = Box::pin(runtime.sessions.shutdown(SessionEndReason::Quit));
                    std::future::poll_fn(|context| {
                        assert!(std::future::Future::poll(shutdown.as_mut(), context).is_pending());
                        std::task::Poll::Ready(())
                    })
                    .await;
                    drop(shutdown);
                    while runtime.sessions.has_associations().unwrap() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            }
            assert_eq!(*end_statuses.lock().unwrap(), [expected_status]);
            assert_eq!(run.snapshot().status(), expected_status);
            assert!(binding.launch_workflow(runtime.workflows.clone(), WorkflowRequest::parse(source, None).await.unwrap(), None, WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(), None).await.is_err());
            assert!(runtime.manager.discover().unwrap().is_empty());
            assert!(!runtime.sessions.has_associations().unwrap());
            let record: serde_json::Value = serde_json::from_slice(&std::fs::read(storage.path()).unwrap()).unwrap();
            let admitted = record["state"]["feedback"] == "admitted";
            if !matches!(scope, FinalizationScope::CreatorClosed | FinalizationScope::PersistenceRetry | FinalizationScope::ProcessPersistenceRetry) {
                assert!(admitted);
            }
            let saved = crate::session::storage::load_session(crate::session::storage::session_path(&session).to_str().unwrap()).unwrap();
            let history = serde_json::to_string(&saved.transcript).unwrap();
            assert_eq!(history.matches("Workflow terminal report").count(), usize::from(admitted));
            assert_eq!(history.matches("phi_sdk.workflow_feedback").count(), usize::from(admitted));
            if admitted {
                run.deliver_to(&binding.instance()).await.unwrap();
            }
            runtime.workflows.shutdown().await.unwrap();
            std::fs::remove_dir_all(root).unwrap();
        }
    })
    .await
    .expect("Workflow and Session finalization did not settle");
}
