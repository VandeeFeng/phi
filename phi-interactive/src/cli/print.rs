use std::sync::Arc;

use phi_sdk::PhiEvent;
#[cfg(test)]
use phi_sdk::lifecycle::SessionEndReason;
use phi_sdk::lifecycle::UserPromptSubmitSource;
use phi_sdk::turn::{SubmissionDisposition, TurnPrompt};

use phi_sdk::agent_instance::AgentInstanceEvent;

use crate::runtime::session::AgentSession;
use crate::session;

pub(crate) async fn run_print(
    workflows: crate::runtime::preparation::SessionWorkflows,
    sessions: Arc<crate::runtime::SessionAssociations>,
    session: &session::Session,
    message: String,
    persist: bool,
    lock: Option<session::storage::SessionLock>,
) -> anyhow::Result<String> {
    let agent = workflows
        .start_with_lock(session.clone(), persist, lock)
        .await?;
    query_text(&agent, &sessions, message).await
}

async fn query_text(
    agent: &Arc<AgentSession>,
    sessions: &crate::runtime::SessionAssociations,
    prompt: String,
) -> anyhow::Result<String> {
    let instance = agent.instance();
    let mut events = instance.subscribe();
    let submission = agent
        .submit(
            TurnPrompt {
                tool_policy: Default::default(),
                prompt,
                source: UserPromptSubmitSource::Print,
                injected_context: Vec::new(),
                images: Vec::new(),
            },
            None,
            None,
        )
        .await?;
    for notice in submission.notices {
        if let phi_sdk::hooks::HookNotice::Warning { message } = notice {
            eprintln!("[agent {}] {message}", instance.id());
        }
    }
    if !matches!(
        submission.disposition,
        SubmissionDisposition::Started { .. }
    ) {
        anyhow::bail!("prompt did not start");
    }
    let boundary = uuid::Uuid::new_v4().to_string();
    let mut quiescence = Box::pin(sessions.wait_for_quiescence(agent, Some(boundary.clone())));
    let mut quiescent = false;
    let mut output = None;
    loop {
        tokio::select! {
            result = &mut quiescence, if !quiescent => {
                result?;
                quiescent = true;
            }
            event = events.recv() => {
                let event = event?;
                report_notices(sessions)?;
                match event {
                    AgentInstanceEvent::Phi(envelope) => match envelope.event {
                        PhiEvent::TurnFinished { response, .. } => output = Some(response),
                        PhiEvent::TurnFailed { failure } => anyhow::bail!(failure.message),
                        _ => {}
                    },
                    AgentInstanceEvent::ViewBoundary { id } if id == boundary => {
                        return output.ok_or_else(|| anyhow::anyhow!("print execution ended without a response"));
                    }
                    AgentInstanceEvent::RunCancelled { .. } => anyhow::bail!("print execution cancelled"),
                    AgentInstanceEvent::PumpFailed { message } => anyhow::bail!(message),
                    AgentInstanceEvent::Stopped => anyhow::bail!("agent instance stopped"),
                    _ => {}
                }
            }
        }
    }
}

pub(crate) fn finish_print(
    sessions: &crate::runtime::SessionAssociations,
    mut result: anyhow::Result<Option<String>>,
) -> anyhow::Result<()> {
    if let Err(notice_error) = report_notices(sessions) {
        result = Err(match result {
            Ok(_) => notice_error,
            Err(error) => error.context(format!("Notice delivery also failed: {notice_error:#}")),
        });
    }
    if let Some(output) = result? {
        println!("{output}");
    }
    Ok(())
}

fn report_notices(sessions: &crate::runtime::SessionAssociations) -> anyhow::Result<()> {
    for notices in sessions.take_notices()? {
        for notice in notices.hook_notices {
            if let phi_sdk::hooks::HookNotice::Warning { message } = notice {
                eprintln!("[agent {}] {message}", notices.agent_id);
            }
        }
        for message in notices.extension_notifications {
            eprintln!("[agent {}] {message}", notices.agent_id);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use crate::runtime::{AgentRuntimeFactory, RuntimeResources};
    use clap::Parser;
    use phi_sdk::agent_instance::{AgentInstanceEvent, AgentInstanceStatus};
    use phi_sdk::extension::ExtensionRuntime;
    use phi_sdk::models::{ModelCatalog, ModelCatalogOptions};
    use phi_sdk::sandbox::Sandbox;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;

    const HTTP_STATUS_OK: &str = "200 OK";

    #[tokio::test]
    async fn print_waits_for_descendant_followup() {
        assert_print_completion("Agent").await;
    }

    #[tokio::test]
    async fn print_waits_for_workflow_report() {
        assert_print_completion("Workflow").await;
    }

    async fn assert_print_completion(tool: &str) {
        let root =
            std::env::temp_dir().join(format!("phi-print-completion-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".phi/agents")).unwrap();
        std::fs::write(
            root.join(".phi/settings.json"),
            json!({"defaultProvider": "local", "defaultModel": "child", "permission": {"bash": "ask"}, "extensions": ["!*.lua"], "memory": {"enabled": false}}).to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join(".phi/agents/probe.md"),
            "---\nname: probe\ndescription: inspect\nmodel: child\n---\nPerform the delegated task.",
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let release = Arc::new(Notify::new());
        let child_started = Arc::new(Notify::new());
        let resume = release.clone();
        let entered = child_started.clone();
        let expected_runs = if tool == "Workflow" { 2 } else { 0 };
        let tool = tool.to_owned();
        let command = format!("touch {}", root.join("permission-probe").display());
        let provider = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut connection, _) = listener.accept().await.unwrap();
                let resume = resume.clone();
                let entered = entered.clone();
                let tool = tool.clone();
                let command = command.clone();
                connections.spawn(async move {
                    let mut bytes = Vec::new();
                    let request: Value = loop {
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
                                break serde_json::from_slice(&bytes[start + 4..start + 4 + length]).unwrap();
                            }
                        }
                    };
                    let messages = request["messages"].as_array().unwrap();
                    let child = request["model"] == "child";
                    let reports = messages.iter().filter(|message| message["content"].to_string().contains("Workflow terminal report")).count();
                    let followup = messages.iter().any(|message| message["content"].to_string().contains("Subagent terminal outcome:"));
                    let tool_result = messages.iter().any(|message| message["role"] == "tool");
                    let successor = messages.iter().any(|message| message["tool_call_id"] == "successor");
                    let (delta, reason) = if child {
                        if !tool_result {
                            entered.notify_one();
                            resume.notified().await;
                        }
                        if tool == "Workflow" && !tool_result {
                            (json!({"tool_calls": [{"index": 0, "id": "permission-probe", "type": "function",
                                "function": {"name": "Bash", "arguments": json!({"command": command}).to_string()}}]}), "tool_calls")
                        } else {
                            (json!({"content": "child-result"}), "stop")
                        }
                    } else if reports == 1 && !successor {
                        (json!({"tool_calls": [{"index": 0, "id": "successor", "type": "function",
                            "function": {"name": "Workflow", "arguments": json!({"source": "export const meta = {name: 'successor', description: 'report-triggered run'}; return 'successor-result';"}).to_string()}}]}), "tool_calls")
                    } else if followup || reports == 2 {
                        (json!({"content": "final-parent-result"}), "stop")
                    } else if tool_result {
                        (json!({"content": "parent-first-result"}), "stop")
                    } else {
                        let arguments = if tool == "Workflow" {
                            json!({"source": "export const meta = {name: 'print', description: 'print lifetime'}; return await agent('inspect', {role: 'probe'});"})
                        } else {
                            json!({"role": "probe", "prompt": "inspect"})
                        };
                        (json!({"tool_calls": [{"index": 0, "id": "delegate", "type": "function",
                            "function": {"name": tool, "arguments": arguments.to_string()}}]}), "tool_calls")
                    };
                    let chunk = json!({"id": "completion", "model": request["model"],
                        "choices": [{"delta": delta, "finish_reason": reason}]});
                    let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                    let response = format!("HTTP/1.1 {HTTP_STATUS_OK}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = connection.write_all(response.as_bytes()).await;
                });
            }
        });
        let models = root.join("models.json");
        std::fs::write(
            &models,
            json!({"providers": {"local": {"api": "openai-completions",
            "baseUrl": format!("http://{address}/v1"), "models": {
                "test": {"contextWindow": 128000}, "child": {"contextWindow": 128000}
            }}}})
            .to_string(),
        )
        .unwrap();
        let catalog = Arc::new(
            ModelCatalog::load(ModelCatalogOptions {
                store_path: root.join("store.json"),
                user_paths: vec![models],
                offline: true,
            })
            .unwrap(),
        );
        let cli = Cli::parse_from(["phi", "--print", "--workflow"]);
        let settings_manager = crate::config::SettingsManager::load_for(&root).unwrap();
        let mut settings = settings_manager.settings();
        settings.permission = Some(serde_json::json!({"mode":"allow", "bash":"ask"}));
        let services = phi_sdk::PhiRuntimeServices::new()
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
            Arc::new(ExtensionRuntime::load_with_paths(Vec::new())),
            catalog.clone(),
            Sandbox::new(false, root.clone()).unwrap(),
            &root,
        );
        let sessions = crate::runtime::SessionAssociations::new(
            phi_sdk::agent_manager::AgentManager::new(),
            Arc::new(phi_sdk::workflow::WorkflowRuntime::default()),
        );
        let preparation =
            crate::runtime::RuntimePreparation::new(AgentRuntimeFactory::new(resources));
        preparation.enable_subagents(&sessions);
        let mut session = session::new("local", "test", 128_000, &std::env::current_dir().unwrap());
        session.working_dir = root.to_string_lossy().into();
        preparation.initialize(
            &root,
            Arc::new(cli),
            settings,
            settings_manager,
            Vec::new(),
            catalog,
        );
        let agent_session = crate::runtime::preparation::SessionWorkflows::new(
            sessions.clone(),
            preparation.clone(),
        )
        .unwrap()
        .start_session(session, true)
        .await
        .unwrap();
        let mut events = agent_session.instance().subscribe();
        let mut query = Box::pin(query_text(
            &agent_session,
            &sessions,
            "delegate inspection".into(),
        ));
        let first = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                tokio::select! {
                    response = &mut query => panic!("print returned before child completion: {response:?}"),
                    event = events.recv() => match event.unwrap() {
                        AgentInstanceEvent::Phi(envelope) => match envelope.event {
                            PhiEvent::TurnFinished { response, .. } => break response,
                            PhiEvent::TurnFailed { failure } => panic!("{failure:?}"),
                            _ => {}
                        },
                        AgentInstanceEvent::PumpFailed { message } => panic!("{message}"),
                        _ => {}
                    }
                }
            }
        })
        .await
        .unwrap();
        assert!(first.is_empty());
        tokio::time::timeout(Duration::from_secs(10), child_started.notified())
            .await
            .unwrap();
        if expected_runs == 0 {
            assert_eq!(
                agent_session.instance().snapshot().status,
                AgentInstanceStatus::Running
            );
        }
        assert!(futures_util::poll!(query.as_mut()).is_pending());
        release.notify_one();
        let final_response = tokio::time::timeout(Duration::from_secs(10), query)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            agent_session.session().await.workflow_run_ids.len(),
            expected_runs
        );
        let history =
            serde_json::to_string(&agent_session.instance().transcript().await.unwrap()).unwrap();
        assert_eq!(
            history.matches("phi_sdk.workflow_feedback").count(),
            expected_runs
        );
        if expected_runs != 0 {
            let worker_id = agent_session.session().await.children[0]
                .child_agent_id
                .clone();
            let worker = sessions
                .session_access(agent_session.instance().id())
                .subagent_view(&worker_id)
                .await
                .unwrap();
            let history = serde_json::to_string(&worker.session.transcript).unwrap();
            assert!(history.contains("\"is_error\":true"));
            assert!(!root.join("permission-probe").exists());
        }
        sessions.shutdown(SessionEndReason::Quit).await.unwrap();
        provider.abort();
        std::fs::remove_dir_all(root).unwrap();
        assert_eq!(final_response, "final-parent-result");
    }
}
