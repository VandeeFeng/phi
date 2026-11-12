use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_STATUS_OK: &str = "200 OK";
const HTTP_STATUS_UNAUTHORIZED: &str = "401 Unauthorized";

enum Completion {
    Success,
    Failure,
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(address: std::net::SocketAddr, settings: Value) -> Self {
        let directory =
            Self(PathBuf::from("/tmp").join(format!("phi-print-process-{}", uuid::Uuid::new_v4())));
        let config = directory.0.join("config");
        let agents = directory.0.join("project/.phi/agents");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(
            agents.join("probe.md"),
            "---\nname: probe\ndescription: inspect\nmodel: child\n---\nPerform the delegated task.",
        )
        .unwrap();
        std::fs::write(config.join("settings.json"), settings.to_string()).unwrap();
        let providers = [
            ("openai", "openai-responses"),
            ("anthropic", "anthropic-messages"),
            ("google", "gemini-generate-content"),
            ("deepseek", "deepseek-chat"),
            ("openai-codex", "openai-codex-responses"),
            ("openrouter", "openai-completions"),
            ("local", "openai-completions"),
        ]
        .into_iter()
        .map(|(name, api)| {
            (
                name.to_string(),
                json!({"api": api, "baseUrl": format!("http://{address}/v1"),
                    "models": {"test": {"contextWindow": 128000}, "child": {"contextWindow": 128000}}}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
        std::fs::write(
            config.join("models.json"),
            json!({"providers": providers}).to_string(),
        )
        .unwrap();
        directory
    }

    fn launch(&self) -> tokio::process::Child {
        self.command().spawn().unwrap()
    }

    fn command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_phi"));
        command
            .args([
                "--print",
                "--provider",
                "local",
                "--model",
                "test",
                "--permission-mode",
                "allow",
                "--no-context-files",
                "delegate inspection",
            ])
            .current_dir(self.0.join("project"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.0)
            .env("PHI_CONFIG_DIR", self.0.join("config"))
            .env("PHI_SESSION_DIR", self.0.join("sessions"))
            .env("PHI_AUTH_DIR", self.0.join("auth"))
            .env("PHI_OFFLINE", "1")
            .env("XDG_CONFIG_HOME", self.0.join("xdg-config"))
            .env("XDG_CACHE_HOME", self.0.join("xdg-cache"))
            .env("XDG_DATA_HOME", self.0.join("xdg-data"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn read_request(connection: &mut TcpStream) -> Value {
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
                return serde_json::from_slice(&bytes[start + 4..start + 4 + length]).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn print_keeps_sdk_diagnostics_separate_from_failure_output() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let directory = TestDirectory::new(
        listener.local_addr().unwrap(),
        json!({"memory":{"enabled":false}}),
    );
    let extensions = directory.0.join("config/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::write(
        extensions.join("notify.lua"),
        r#"on("AgentStart", function(_, ctx)
  ctx.ui.notify("explicit extension notification")
end)
on("SessionEnd", function(_, ctx)
  ctx.ui.notify("finalization notification")
  error("finalization observer failed")
end)"#,
    )
    .unwrap();
    let path = directory.0.join("diagnostics.log");
    let api_key = uuid::Uuid::new_v4().to_string();
    let child = directory
        .command()
        .args(["--api-key", &api_key, "--log-file"])
        .arg(&path)
        .env("RUST_LOG", "warn")
        .spawn()
        .unwrap();
    let provider = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        read_request(&mut connection).await;
        let body =
            json!({"error":{"message":"provider rejected request","type":"authentication_error"}})
                .to_string();
        connection.write_all(format!("HTTP/1.1 {HTTP_STATUS_UNAUTHORIZED}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let output = tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(PROCESS_TIMEOUT, provider)
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("provider rejected request"), "{stderr}");
    assert!(!stderr.contains("visible in process listings"), "{stderr}");
    assert_eq!(
        stderr.matches("explicit extension notification").count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("finalization notification").count(),
        1,
        "{stderr}"
    );
    assert_eq!(
        stderr.matches("finalization observer failed").count(),
        1,
        "{stderr}"
    );
    let diagnostics = std::fs::read_to_string(path).unwrap();
    assert!(
        diagnostics.contains("visible in process listings"),
        "{diagnostics}"
    );
    assert!(!diagnostics.contains(&api_key));
    assert!(!diagnostics.contains("explicit extension notification"));
}

#[tokio::test]
async fn restored_session_reads_target_directory_settings() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let directory = TestDirectory::new(
        listener.local_addr().unwrap(),
        json!({"defaultProvider":"local", "defaultModel":"test", "memory":{"enabled":false}}),
    );
    let source = directory.0.join("project");
    let target = directory.0.join("target");
    std::fs::create_dir_all(source.join(".phi")).unwrap();
    std::fs::create_dir_all(target.join(".phi")).unwrap();
    std::fs::write(source.join(".phi/settings.json"), "invalid JSON").unwrap();
    std::fs::write(
        target.join(".phi/settings.json"),
        r#"{"no_tools":true,"memory":{"enabled":false}}"#,
    )
    .unwrap();
    let session_id = uuid::Uuid::new_v4().to_string();
    let session_dir =
        directory
            .0
            .join("sessions")
            .join(phi_sdk::workspace::encode_working_dir_key(
                target.to_str().unwrap(),
            ));
    std::fs::create_dir_all(&session_dir).unwrap();
    let path = session_dir.join(format!("20260101T000000Z-{session_id}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{}\n",
            json!({
                "type":"metadata", "id":session_id, "name":"", "created_at":"2026-01-01T00:00:00Z",
                "updated_at":"2026-01-01T00:00:00Z", "total_tokens":0, "total_cost":0.0,
                "context_window":128000, "model":"test", "provider":"local",
                "working_dir":target, "agent_kind":{"kind":"master"}
            })
        ),
    )
    .unwrap();
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_phi"))
        .arg("--print")
        .arg("--session")
        .arg(&path)
        .arg("verify target settings")
        .current_dir(&source)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &directory.0)
        .env("PHI_CONFIG_DIR", directory.0.join("config"))
        .env("PHI_SESSION_DIR", directory.0.join("sessions"))
        .env("PHI_AUTH_DIR", directory.0.join("auth"))
        .env("PHI_OFFLINE", "1")
        .env("XDG_CONFIG_HOME", directory.0.join("xdg-config"))
        .env("XDG_CACHE_HOME", directory.0.join("xdg-cache"))
        .env("XDG_DATA_HOME", directory.0.join("xdg-data"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let served = tokio::spawn(async move {
        let (mut connection, _) = listener.accept().await.unwrap();
        let request = read_request(&mut connection).await;
        let chunk = json!({"id":"completion","model":"test","choices":[{"delta":{"content":"target-settings-loaded"},"finish_reason":"stop"}]});
        let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
        connection.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        request
    });
    let output = tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = tokio::time::timeout(PROCESS_TIMEOUT, served)
        .await
        .unwrap()
        .unwrap();
    assert!(
        request.get("tools").is_none() || request["tools"].as_array().is_some_and(Vec::is_empty)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("target-settings-loaded"));
}

#[tokio::test]
async fn print_process_waits_for_child_and_reports_final_parent_outcome() {
    for completion in [Completion::Success, Completion::Failure] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let directory = TestDirectory::new(
            listener.local_addr().unwrap(),
            json!({"defaultProvider": "local", "defaultModel": "test", "memory": {"enabled": false}}),
        );
        let extensions = directory.0.join("config/extensions");
        std::fs::create_dir_all(&extensions).unwrap();
        std::fs::write(
            extensions.join("notify.lua"),
            r#"
on("SessionStart", function(_, ctx) ctx.ui.notify("session started") end)
on("SessionEnd", function(_, ctx) ctx.ui.notify("session ended") end)
on("PromptContext", function()
  return { notices = {{ kind = "warning", message = "prompt hook report" }} }
end)
"#,
        )
        .unwrap();
        let child_started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let entered = child_started.clone();
        let resume = release.clone();
        let successful = matches!(completion, Completion::Success);
        let provider = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut connection, _) = listener.accept().await.unwrap();
                let entered = entered.clone();
                let resume = resume.clone();
                connections.spawn(async move {
                    let request = read_request(&mut connection).await;
                    let messages = request["messages"].as_array().unwrap();
                    let child = request["model"] == "child";
                    let followup = messages.iter().any(|message| {
                        message["content"].to_string().contains("Subagent terminal outcome:")
                    });
                    let tool_count = messages.iter().filter(|message| message["role"] == "tool").count();
                    let replied = tool_count >= 3;
                    let (delta, reason) = if child {
                        entered.notify_one();
                        resume.notified().await;
                        (json!({"content": "child-result"}), "stop")
                    } else if followup {
                        (json!({"content": "final-parent-result"}), "stop")
                    } else if replied {
                        panic!("unexpected startup acknowledgement request");
                    } else {
                        let (name, arguments) = match tool_count {
                            0 => ("Task", json!({"action": "create", "subject": "Unfinished inspection", "description": "Independent checklist work"})),
                            1 => ("Task", json!({"action": "update", "task_id": "1", "status": "in_progress"})),
                            _ => ("Agent", json!({"role": "probe", "prompt": "inspect"})),
                        };
                        (json!({"tool_calls": [{"index": 0, "id": format!("call-{tool_count}"), "type": "function",
                            "function": {"name": name, "arguments": arguments.to_string()}}]}), "tool_calls")
                    };
                    let (status, content_type, body) = if followup && !successful {
                        (HTTP_STATUS_UNAUTHORIZED, "application/json", json!({"error": {"message": "final-provider-failure", "type": "authentication_error"}}).to_string())
                    } else {
                        let chunk = json!({"id": "completion", "model": request["model"],
                            "choices": [{"delta": delta, "finish_reason": reason}]});
                        (HTTP_STATUS_OK, "text/event-stream", format!("data: {chunk}\n\ndata: [DONE]\n\n"))
                    };
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = connection.write_all(response.as_bytes()).await;
                });
            }
        });
        let mut child = directory.launch();
        let readiness = tokio::time::timeout(PROCESS_TIMEOUT, async {
            child_started.notified().await;
        });
        let output = tokio::select! {
            result = readiness => {
                result.expect("child did not reach the provider");
                assert!(child.try_wait().unwrap().is_none(), "print exited before child release");
                release.notify_one();
                tokio::time::timeout(PROCESS_TIMEOUT, child.wait_with_output()).await.unwrap().unwrap()
            }
            status = child.wait() => panic!("print exited during startup: {status:?}"),
        };
        provider.abort();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.success(), successful, "{stderr}");
        for message in ["session started", "session ended"] {
            assert_eq!(stderr.matches(message).count(), 2, "{stderr}");
        }
        let started = stderr
            .lines()
            .filter(|line| line.contains("session started"))
            .collect::<Vec<_>>();
        assert_ne!(
            started[0], started[1],
            "notifications must identify their Agents: {stderr}"
        );
        assert_eq!(stderr.matches("prompt hook report").count(), 3, "{stderr}");
        let lists = std::fs::read_dir(directory.0.join("config/tasks"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(lists.len(), 1);
        let task: Value =
            serde_json::from_slice(&std::fs::read(lists[0].join("1.json")).unwrap()).unwrap();
        assert_eq!(task["status"], "in_progress");
        assert!(task["owner"].is_null());
        assert!(!directory.0.join("config/teams").exists());
        if successful {
            assert_eq!(stdout.trim(), "final-parent-result");
        } else {
            assert!(
                stdout.trim().is_empty(),
                "printed an earlier success: {stdout}"
            );
            assert!(stderr.contains("final-provider-failure"), "{stderr}");
        }
    }
}

type ProviderRequest = (Value, tokio::sync::oneshot::Sender<Value>);

fn controlled_provider(
    listener: TcpListener,
) -> (
    tokio::task::JoinHandle<()>,
    tokio::sync::mpsc::Receiver<ProviderRequest>,
) {
    let (requests, incoming) = tokio::sync::mpsc::channel(16);
    let provider = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        loop {
            let (mut connection, _) = listener.accept().await.unwrap();
            let requests = requests.clone();
            connections.spawn(async move {
                let request = read_request(&mut connection).await;
                let (reply, response) = tokio::sync::oneshot::channel::<Value>();
                requests.send((request.clone(), reply)).await.unwrap();
                let Ok(delta) = response.await else { return; };
                let reason = if delta.get("tool_calls").is_some() { "tool_calls" } else { "stop" };
                let chunk = json!({"id": "completion", "model": request["model"],
                    "choices": [{"delta": delta, "finish_reason": reason}]});
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let response = format!("HTTP/1.1 {HTTP_STATUS_OK}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = connection.write_all(response.as_bytes()).await;
            });
        }
    });
    (provider, incoming)
}

#[tokio::test]
async fn print_waits_for_message_follow_up() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let directory = TestDirectory::new(
        listener.local_addr().unwrap(),
        json!({"memory":{"enabled":false}}),
    );
    let (provider, mut incoming) = controlled_provider(listener);
    let call = |arguments: Value| json!({"tool_calls":[{"index":0,"id":uuid::Uuid::new_v4().to_string(),"type":"function","function":{"name":"Agent","arguments":arguments.to_string()}}]});
    let mut invoked = false;
    let mut child_requests = 0;
    let child = directory.launch();
    let output = child.wait_with_output();
    tokio::pin!(output);
    tokio::time::timeout(PROCESS_TIMEOUT, async {
        let held_reply = loop {
            let (request, reply) = tokio::select! {
                output = &mut output => panic!("print finalized before second provider request: {output:?}"),
                request = incoming.recv() => request.unwrap(),
            };
            if request["model"] == "child" {
                child_requests += 1;
                if child_requests == 1 { reply.send(json!({"content":"initial-child-result"})).unwrap(); }
                else { break reply; }
            } else {
                let messages = request["messages"].as_array().unwrap();
                if !messages.iter().any(|message| message["role"] == "tool") {
                    reply.send(call(json!({"role":"probe","prompt":"initial inspection"}))).unwrap();
                } else if request["messages"].to_string().contains("Subagent terminal outcome:") && !invoked {
                    let marker = "Subagent terminal outcome:\n";
                    let content = messages.iter().filter_map(|message| message["content"].as_str()).find(|content| content.contains(marker)).unwrap();
                    let result: Value = serde_json::from_str(content.split_once(marker).unwrap().1).unwrap();
                    invoked = true;
                    reply.send(call(json!({"action":"message","recipient":result["instance_id"],"content":"second inspection"}))).unwrap();
                } else { reply.send(json!({"content":"parent accepted"})).unwrap(); }
            }
        };
        let deadline = tokio::time::sleep(Duration::from_millis(400));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                output = &mut output => panic!("print cancelled admitted work before provider reply: {output:?}"),
                _ = &mut deadline => break,
                request = incoming.recv() => {
                    let (request, reply) = request.unwrap();
                    assert_eq!(request["model"], "test");
                    reply.send(json!({"content":"parent accepted"})).unwrap();
                }
            }
        }
        held_reply.send(json!({"content":"second-child-result"})).unwrap();
        let mut reported = false;
        loop {
            tokio::select! {
                output = &mut output => {
                    assert!(output.unwrap().status.success());
                    assert!(reported);
                    break;
                }
                request = incoming.recv() => {
                    let (request, reply) = request.unwrap();
                    assert_eq!(request["model"], "test");
                    reported |= request["messages"].to_string().contains("second-child-result");
                    reply.send(json!({"content":"follow-up result received"})).unwrap();
                }
            }
        }
    }).await.unwrap();
    provider.abort();
}

#[tokio::test]
async fn print_named_subagents_claim_message_complete_and_unblock() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let directory = TestDirectory::new(
        listener.local_addr().unwrap(),
        json!({"memory": {"enabled": false}, "agent": {"team": true}}),
    );
    let (provider, mut incoming) = controlled_provider(listener);
    let call = |name: &str, arguments: Value| {
        json!({"tool_calls": [{"index": 0, "id": uuid::Uuid::new_v4().to_string(), "type": "function",
            "function": {"name": name, "arguments": arguments.to_string()}}]})
    };
    let read_task = |id: &str| -> Value {
        let lists = std::fs::read_dir(directory.0.join("config/tasks")).unwrap();
        let list = lists.into_iter().next().unwrap().unwrap().path();
        serde_json::from_slice(&std::fs::read(list.join(format!("{id}.json"))).unwrap()).unwrap()
    };
    let mut security_waiting: Option<tokio::sync::oneshot::Sender<Value>> = None;
    let mut performance_waiting: Option<tokio::sync::oneshot::Sender<Value>> = None;
    let mut security_completed: Option<tokio::sync::oneshot::Sender<Value>> = None;
    let mut blocked_claim = false;
    let mut finished = std::collections::HashSet::new();
    let child = directory.launch();
    let output = tokio::time::timeout(PROCESS_TIMEOUT, async {
        let output = child.wait_with_output();
        tokio::pin!(output);
        loop {
            let (request, reply) = tokio::select! {
                output = &mut output => break output.unwrap(),
                request = incoming.recv() => request.unwrap(),
            };
            let messages = request["messages"].as_array().unwrap();
            let tools = messages.iter().filter(|message| message["role"] == "tool").collect::<Vec<_>>();
            let count = tools.len();
            if request["model"] == "test" {
                for result in &tools {
                    let content: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
                    assert_ne!(content["success"], false, "{result}");
                    assert!(content.get("error").is_none(), "{result}");
                }
                let delta = match count {
                    0 => {
                        assert!(!directory.0.join("config/teams").exists());
                        call("Task", json!({"action": "create", "subject": "Security review", "description": "Review security"}))
                    }
                    1 => call("Task", json!({"action": "create", "subject": "Performance review", "description": "Review performance after security"})),
                    2 => call("Task", json!({"action": "update", "task_id": "2", "add_blocked_by": ["1"]})),
                    3 => {
                        assert!(read_task("1")["owner"].is_null());
                        assert_eq!(read_task("2")["blocked_by"], json!(["1"]));
                        let mut launches = call("Agent", json!({"role": "probe", "alias": "security", "prompt": "security-work: claim Task 1, report and complete it"}));
                        let mut performance = call("Agent", json!({"role": "probe", "alias": "performance", "prompt": "performance-work: claim Task 2 only when unblocked, report and complete it"}));
                        performance["tool_calls"][0]["index"] = json!(1);
                        launches["tool_calls"].as_array_mut().unwrap().push(performance["tool_calls"][0].clone());
                        launches
                    }
                    _ => {
                        assert!(!directory.0.join("config/teams").exists());
                        json!({"content": "subagent-result"})
                    },
                };
                reply.send(delta).unwrap();
                continue;
            }
            assert_eq!(request["model"], "child");
            let security = messages.iter().any(|message| message["role"] == "user" && message["content"].to_string().contains("security-work:"));
            let worker = if security { "security" } else { "performance" };
            for name in ["Task", "Agent"] {
                assert!(request["tools"].as_array().unwrap().iter().any(|tool| tool["function"]["name"] == name), "{worker}: missing {name}");
            }
            let last = tools.last().map(|message| message["content"].as_str().unwrap());
            if security {
                match count {
                    0 => { reply.send(call("Task", json!({"action": "get", "task_id": "1"}))).unwrap(); }
                    1 => {
                        let result: Value = serde_json::from_str(last.unwrap()).unwrap();
                        assert_eq!(result["task"]["id"], "1");
                        reply.send(call("Task", json!({"action": "update", "task_id": "1", "owner": "security", "status": "in_progress"}))).unwrap();
                    }
                    2 => {
                        assert_eq!(read_task("1")["status"], "in_progress");
                        assert!(read_task("1")["owner"]["instance_id"].is_string());
                        security_waiting = Some(reply);
                    }
                    3 => {
                        assert!(last.unwrap().contains("is not authorized for Message"), "{last:?}");
                        reply.send(call("Task", json!({"action": "update", "task_id": "1", "status": "completed"}))).unwrap();
                    }
                    4 => {
                        assert!(blocked_claim);
                        assert_eq!(read_task("1")["status"], "completed");
                        assert_eq!(read_task("2")["status"], "pending");
                        assert!(read_task("2")["owner"].is_null());
                        security_completed = Some(reply);
                        performance_waiting.take().unwrap().send(call("Task", json!({"action": "update", "task_id": "2", "owner": "performance", "status": "in_progress"}))).unwrap();
                    }
                    _ => { reply.send(json!({"content": "security retained"})).unwrap(); }
                }
            } else {
                match count {
                    0 => { reply.send(call("Task", json!({"action": "get", "task_id": "2"}))).unwrap(); }
                    1 => {
                        let result: Value = serde_json::from_str(last.unwrap()).unwrap();
                        assert_eq!(result["task"]["unresolved"], json!(["1"]));
                        reply.send(call("Task", json!({"action": "update", "task_id": "2", "owner": "performance", "status": "in_progress"}))).unwrap();
                    }
                    2 => {
                        assert!(last.unwrap().contains("prerequisites are unfinished"), "{last:?}");
                        assert_eq!(read_task("2")["status"], "pending");
                        assert!(read_task("2")["owner"].is_null());
                        blocked_claim = true;
                        performance_waiting = Some(reply);
                    }
                    3 => {
                        let first = read_task("1");
                        let second = read_task("2");
                        assert_eq!(second["status"], "in_progress");
                        assert_ne!(first["owner"], second["owner"]);
                        finished.insert("security");
                        security_completed.take().unwrap().send(json!({"content": "security-result"})).unwrap();
                        let parent_id = std::fs::read_dir(directory.0.join("config/tasks")).unwrap()
                            .next().unwrap().unwrap().file_name().to_string_lossy().into_owned();
                        reply.send(call("Agent", json!({"action": "message", "recipient": parent_id, "content": "performance-reviewed"}))).unwrap();
                    }
                    4 => {
                        let result: Value = serde_json::from_str(last.unwrap())
                            .unwrap_or_else(|error| panic!("invalid message result {last:?}: {error}"));
                        assert!(result.get("error").is_none(), "{result}");
                        reply.send(call("Task", json!({"action": "update", "task_id": "2", "status": "completed"}))).unwrap();
                    }
                    _ => {
                        assert_eq!(read_task("2")["status"], "completed");
                        finished.insert("performance");
                        reply.send(json!({"content": "performance-result"})).unwrap();
                    }
                }
            }
            if security_waiting.is_some() && performance_waiting.is_some() {
                security_waiting.take().unwrap().send(call("Agent", json!({"action": "message", "recipient": "performance", "content": "security-reviewed"}))).unwrap();
            }
        }
    }).await.expect("Named subagent tool flow did not finish");
    provider.abort();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stderr}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "subagent-result"
    );
    assert_eq!(finished.len(), 2);
    for id in ["1", "2"] {
        assert_eq!(read_task(id)["status"], "completed");
        assert!(read_task(id)["owner"]["instance_id"].is_string());
    }
    assert!(!directory.0.join("config/teams").exists());
    let scope = std::fs::read_dir(directory.0.join("sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let histories = std::fs::read_dir(&scope)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert_eq!(histories.len(), 2, "{histories:?}");
    let lead_history = histories
        .iter()
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .unwrap();
    let children = histories
        .iter()
        .find(|path| path.is_dir())
        .unwrap()
        .join("subagents");
    assert_eq!(std::fs::read_dir(&children).unwrap().count(), 2);
    for id in ["1", "2"] {
        let child_id = read_task(id)["owner"]["instance_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(children.join(format!("agent-{child_id}.jsonl")).is_file());
    }
    let performance_id = read_task("2")["owner"]["instance_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let performance_history = children.join(format!("agent-{performance_id}.jsonl"));
    assert!(
        !std::fs::read_to_string(performance_history)
            .unwrap()
            .contains("security-reviewed")
    );
    assert!(
        std::fs::read_to_string(lead_history)
            .unwrap()
            .contains("performance-reviewed")
    );
}
