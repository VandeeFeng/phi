use std::io::{BufRead, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(12);
const HTTP_STATUS_OK: &str = "200 OK";
const HTTP_STATUS_UNAUTHORIZED: &str = "401 Unauthorized";
const INPUT_FRAME_LIMIT: usize = 1024 * 1024;

enum OutputMode {
    Read,
    Closed,
    Unread,
}
enum ProcessSelection {
    Launch,
    Initialize,
}

enum Tools {
    Disabled,
    Enabled,
}

struct RpcProcess {
    child: Child,
    root: PathBuf,
    output: mpsc::Receiver<Result<Value, String>>,
}

impl RpcProcess {
    fn start(output_mode: OutputMode) -> Self {
        Self::with_provider(output_mode, "http://127.0.0.1:1/v1", Tools::Disabled)
    }

    fn with_provider(output_mode: OutputMode, base_url: &str, tools: Tools) -> Self {
        let root = PathBuf::from("/tmp").join(format!("phi-rpc-{}", uuid::Uuid::new_v4()));
        Self::in_directory(root, output_mode, base_url, tools)
    }

    fn in_directory(root: PathBuf, output_mode: OutputMode, base_url: &str, tools: Tools) -> Self {
        Self::spawn(root, output_mode, base_url, tools, ProcessSelection::Launch)
    }

    fn spawn(
        root: PathBuf,
        output_mode: OutputMode,
        base_url: &str,
        tools: Tools,
        selection: ProcessSelection,
    ) -> Self {
        let config = root.join("config");
        let project = root.join("project");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(project.join(".phi/agents")).unwrap();
        for (name, model, tools) in [
            ("probe", "child", "[Agent, Bash]"),
            ("leaf", "nested", "[Bash]"),
        ] {
            std::fs::write(project.join(format!(".phi/agents/{name}.md")), format!("---\nname: {name}\ndescription: {name}\nmodel: {model}\ntools: {tools}\n---\nPerform the task.")).unwrap();
        }
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
                json!({"api":api,"baseUrl":base_url,"models":{"test":{"contextWindow":128000},"child":{"contextWindow":128000},"nested":{"contextWindow":128000}}}),
            )
        })
        .collect::<serde_json::Map<_, _>>();
        std::fs::write(
            config.join("models.json"),
            serde_json::to_vec(&json!({"providers":providers})).unwrap(),
        )
        .unwrap();
        std::fs::write(config.join("settings.json"),br#"{"memory":{"enabled":false},"sandbox":{"enabled":false},"permission":{"bash":"ask","tools":{"agent":"allow"}}}"#).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_phi"));
        command.args(["--rpc", "--no-context-files"]);
        if matches!(selection, ProcessSelection::Launch) {
            command.args(["--provider", "local", "--model", "test"]);
        }
        if matches!(tools, Tools::Disabled) {
            command.arg("--no-tools");
        }
        let mut child = command
            .current_dir(project)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .env("PHI_CONFIG_DIR", config)
            .env("PHI_SESSION_DIR", root.join("sessions"))
            .env("PHI_AUTH_DIR", root.join("auth"))
            .env("PHI_OFFLINE", "1")
            .env("XDG_CONFIG_HOME", root.join("xdg-config"))
            .env("XDG_CACHE_HOME", root.join("xdg-cache"))
            .env("XDG_DATA_HOME", root.join("xdg-data"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = mpsc::channel();
        match output_mode {
            OutputMode::Read => {
                std::thread::spawn(move || {
                    for line in std::io::BufReader::new(stdout).lines() {
                        let message = line.map_err(|error| error.to_string()).and_then(|line| {
                            serde_json::from_str(&line).map_err(|error| error.to_string())
                        });
                        if sender.send(message).is_err() {
                            break;
                        }
                    }
                });
            }
            OutputMode::Closed => drop(stdout),
            OutputMode::Unread => child.stdout = Some(stdout),
        }
        Self {
            child,
            root,
            output,
        }
    }

    fn send(&mut self, message: Value) {
        let input = self.child.stdin.as_mut().unwrap();
        serde_json::to_writer(&mut *input, &message).unwrap();
        input.write_all(b"\n").unwrap();
        input.flush().unwrap();
    }
    fn control(&mut self, id: &str, request: Value) {
        self.send(json!({"type":"control","control_id":id,"request":request}));
    }
    fn initialize(&mut self) -> String {
        self.control(
            "init",
            json!({"kind":"initialize","version":2,"startup":{"kind":"new","persistence":"saved"}}),
        );
        let reply = self.reply("init");
        assert_eq!(reply["result"]["status"], "ok", "{reply}");
        reply["result"]["value"]["session_id"]
            .as_str()
            .unwrap()
            .to_string()
    }
    fn input(&mut self, id: &str, dispatch: &str, text: &str) {
        self.send(json!({"type":"input","input_id":id,"dispatch":dispatch,"prompt":{"text":text}}));
    }
    fn receive(&self) -> Value {
        self.output
            .recv_timeout(PROCESS_TIMEOUT)
            .expect("RPC output deadline")
            .expect("stdout must contain only JSON frames")
    }
    fn until(&self, kind: &str) -> Value {
        loop {
            let frame = self.receive();
            if frame["type"] == kind {
                return frame;
            }
        }
    }
    fn reply(&self, id: &str) -> Value {
        loop {
            let frame = self.receive();
            if frame["type"] == "control_result" && frame["control_id"] == id {
                return frame;
            }
        }
    }
    fn finish(&mut self) -> (std::process::ExitStatus, String) {
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "RPC process did not exit");
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut stderr = String::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        (status, stderr)
    }
    fn close(&mut self, mode: &str) -> Value {
        self.control("close", json!({"kind":"close","mode":mode}));
        self.until("closed")
    }
}
impl Drop for RpcProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            if let Some(mut stderr) = self.child.stderr.take() {
                let mut message = String::new();
                let _ = stderr.read_to_string(&mut message);
                eprintln!("{message}");
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

enum ProviderReply {
    Chunk(Value),
    Failure,
}

struct Provider {
    address: std::net::SocketAddr,
    requests: mpsc::Receiver<(Value, mpsc::Sender<ProviderReply>)>,
    replies: std::cell::RefCell<Option<mpsc::Sender<ProviderReply>>>,
    stopping: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Provider {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (requests_sender, requests) = mpsc::channel();
        let stopping = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = stopping.clone();
        let thread = std::thread::spawn(move || {
            for connection in listener.incoming() {
                let Ok(mut connection) = connection else {
                    break;
                };
                if stopped.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                let requests_sender = requests_sender.clone();
                std::thread::spawn(move || {
                    let mut bytes = Vec::new();
                    let (start, count) = loop {
                        let mut buffer = [0; 4096];
                        let Ok(count) = connection.read(&mut buffer) else {
                            return;
                        };
                        if count == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..count]);
                        if let Some(start) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&bytes[..start]).to_lowercase();
                            let count: usize = headers
                                .lines()
                                .find_map(|line| line.strip_prefix("content-length: "))
                                .unwrap()
                                .parse()
                                .unwrap();
                            if bytes.len() >= start + 4 + count {
                                break (start + 4, count);
                            }
                        }
                    };
                    let request: Value =
                        serde_json::from_slice(&bytes[start..start + count]).unwrap();
                    let (replies, receiver) = mpsc::channel();
                    if requests_sender.send((request, replies)).is_err() {
                        return;
                    }
                    let Ok(reply) = receiver.recv() else {
                        return;
                    };
                    let (status, content_type, body) = match reply {
                    ProviderReply::Chunk(reply) => (
                        HTTP_STATUS_OK,
                        "text/event-stream",
                        format!("data: {reply}\n\ndata: [DONE]\n\n"),
                    ),
                    ProviderReply::Failure => (
                        HTTP_STATUS_UNAUTHORIZED,
                        "application/json",
                        json!({"error":{"message":"test failure","type":"authentication_error"}})
                            .to_string(),
                    ),
                };
                    let _ = connection.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes());
                });
            }
        });
        Self {
            address,
            requests,
            replies: Default::default(),
            stopping,
            thread: Some(thread),
        }
    }
    fn url(&self) -> String {
        format!("http://{}/v1", self.address)
    }
    fn request(&self) -> Value {
        let (request, reply) = self
            .requests
            .recv_timeout(PROCESS_TIMEOUT)
            .expect("provider request deadline");
        assert!(
            self.replies.borrow_mut().replace(reply).is_none(),
            "previous provider request needs a reply"
        );
        request
    }
    fn response(&self, text: &str) {
        self.reply(json!({"id":"completion","model":"test","choices":[{"delta":{"content":text},"finish_reason":"stop"}]}));
    }
    fn reply(&self, value: Value) {
        self.replies
            .borrow_mut()
            .take()
            .unwrap()
            .send(ProviderReply::Chunk(value))
            .unwrap();
    }
    fn failure(&self) {
        self.replies
            .borrow_mut()
            .take()
            .unwrap()
            .send(ProviderReply::Failure)
            .unwrap();
    }
    fn delegate(&self, agent: &str) {
        self.reply(json!({"id":"completion","model":"test","choices":[{"delta":{"tool_calls":[{"index":0,"id":"delegate-call","type":"function","function":{"name":"Agent","arguments":json!({"role":agent,"prompt":"run task"}).to_string()}}]},"finish_reason":"tool_calls"}]}));
    }
    fn permission(&self) {
        self.reply(json!({"id":"completion","model":"test","choices":[{"delta":{"tool_calls":[{"index":0,"id":uuid::Uuid::new_v4().to_string(),"type":"function","function":{"name":"Bash","arguments":"{\"command\":\"printf rpc-permission\"}"}}]},"finish_reason":"tool_calls"}]}));
    }
    fn tool(&self, name: &str, arguments: Value) {
        self.reply(json!({"id":"completion","model":"test","choices":[{"delta":{"tool_calls":[{"index":0,"id":uuid::Uuid::new_v4().to_string(),"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":"tool_calls"}]}));
    }
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.replies.borrow_mut().take();
        self.stopping
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = std::net::TcpStream::connect(self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn explicit_notifications_and_hook_reports_are_not_failures() {
    let provider = Provider::start();
    let root = PathBuf::from("/tmp").join(format!("phi-rpc-notices-{}", uuid::Uuid::new_v4()));
    let extensions = root.join("config/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::write(
        extensions.join("notify.lua"),
        r#"
on("AgentStart", function(_, ctx) ctx.ui.notify("explicit notification") end)
on("SessionEnd", function(_, ctx)
  ctx.ui.notify("finalization notification")
  error("finalization observer failed")
end)
on("PromptContext", function()
  return { contexts = {}, notices = {{ kind = "info", message = "hook informational report" }} }
end)
"#,
    )
    .unwrap();
    let mut process =
        RpcProcess::in_directory(root, OutputMode::Read, &provider.url(), Tools::Disabled);
    process.initialize();
    process.input("one", "follow_up", "run");
    provider.request();
    provider.response("done");
    drop(process.child.stdin.take());
    let mut notices = Vec::new();
    loop {
        let frame = process.receive();
        if frame["type"] == "notice" {
            notices.push(frame.clone());
        }
        if frame["type"] == "closed" {
            assert_eq!(frame["status"], "completed", "{frame}");
            break;
        }
    }
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice["code"] == "extension_notification"
                && notice["message"] == "explicit notification")
            .count(),
        1,
        "{notices:?}"
    );
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice["code"] == "extension_notification"
                && notice["message"] == "finalization notification")
            .count(),
        1,
        "{notices:?}"
    );
    assert_eq!(
        notices
            .iter()
            .filter(|notice| notice["code"] == "hook_notice"
                && notice["message"]
                    .as_str()
                    .unwrap()
                    .contains("finalization observer failed"))
            .count(),
        1,
        "{notices:?}"
    );
    let hook = notices
        .iter()
        .find(|notice| {
            notice["code"] == "hook_notice"
                && notice["message"]
                    .as_str()
                    .unwrap()
                    .contains("hook informational report")
        })
        .expect("hook report");
    let payload: Value = serde_json::from_str(hook["message"].as_str().unwrap()).unwrap();
    assert_eq!(payload["kind"], "info");
    assert_eq!(payload["message"], "hook informational report");
    assert!(
        !notices
            .iter()
            .any(|notice| notice["code"] == "operation_failed")
    );
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
    assert!(!stderr.contains("explicit notification"));
}

#[test]
fn framing_recovers_without_legacy_aliases_and_uninitialized_eof_closes() {
    let mut process = RpcProcess::start(OutputMode::Read);
    process.send(json!({"id":1,"method":"initialize"}));
    assert_eq!(process.until("notice")["code"], "invalid_message");
    for operation in [
        "subscribe",
        "session/create",
        "session/recovery",
        "task/wait",
        "agent/discover",
        "mcp_save",
        "mcp_status",
        "mcp_reconnect",
        "mcp_cancel",
    ] {
        process.control("retired", json!({"kind":operation}));
        assert_eq!(process.until("notice")["code"], "invalid_message");
    }
    process
        .child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&vec![b'x'; INPUT_FRAME_LIMIT + 1])
        .unwrap();
    process.child.stdin.as_mut().unwrap().write_all(b"\n \r\n{\"type\":\"control\",\"control_id\":\"tail\",\"request\":{\"kind\":\"runtime_info\"}}").unwrap();
    drop(process.child.stdin.take());
    assert_eq!(process.until("notice")["code"], "invalid_message");
    assert_eq!(
        process.reply("tail")["result"]["error"]["code"],
        "not_ready"
    );
    assert_eq!(process.until("closed")["status"], "completed");
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn initialization_validates_intent_and_never_admits_a_second_root() {
    let mut process = RpcProcess::start(OutputMode::Read);
    process.control(
        "version",
        json!({"kind":"initialize","version":1,"startup":{"kind":"new","persistence":"saved"}}),
    );
    assert_eq!(
        process.reply("version")["result"]["error"]["code"],
        "unsupported_version"
    );
    process.control("bad",json!({"kind":"initialize","version":2,"startup":{"kind":"new","persistence":"saved","provider":"missing"}}));
    assert_eq!(process.reply("bad")["result"]["status"], "error");
    let id = process.initialize();
    process.control(
        "second",
        json!({"kind":"initialize","version":2,"startup":{"kind":"new","persistence":"saved"}}),
    );
    assert_eq!(
        process.reply("second")["result"]["error"]["code"],
        "invalid_message"
    );
    process.control("info", json!({"kind":"runtime_info"}));
    let info = process.reply("info");
    assert!(info["result"]["value"]["runtime"]["generation_id"].is_string());
    assert!(!id.is_empty());
    let closed = process.close("drain");
    assert_eq!(closed["status"], "completed");
    assert!(process.finish().0.success());
}

fn background_bash(process: &mut RpcProcess, provider: &Provider) -> PathBuf {
    process.input("bash", "immediate", "run a background command");
    process.until("admission");
    provider.request();
    let command = format!(
        "printf ready; deadline=$((SECONDS + {})); while ! test -f release; do (( SECONDS < deadline )) || exit 124; sleep 0.05; done; printf done > completed",
        PROCESS_TIMEOUT.as_secs(),
    );
    provider.tool("Bash", json!({"command":command, "run_in_background":true}));
    let question = process.until("question");
    process.send(json!({"type":"answer","question_id":question["question_id"],"response":{"kind":"permission","decision":"allow_once"}}));
    process.until("question_closed");
    let request = provider.request();
    let receipt: Value = serde_json::from_str(
        request["messages"].as_array().unwrap().last().unwrap()["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(receipt["status"], "background_started");
    provider.response("command launched");
    process.until("outcome");
    PathBuf::from(receipt["output_path"].as_str().unwrap())
}

#[test]
fn eof_waits_for_background_bash_and_its_completion_turn() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    process.initialize();
    let output = background_bash(&mut process, &provider);
    drop(process.child.stdin.take());
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        if let Ok(Ok(frame)) = process
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            assert_ne!(
                frame["type"], "closed",
                "EOF dropped a live Bash execution: {frame}"
            );
        }
    }
    std::fs::write(process.root.join("project/release"), "").unwrap();
    let request = provider.request();
    let messages = request["messages"].to_string();
    assert!(messages.contains("not human input or approval"));
    assert!(messages.contains("Bash execution outcome"));
    assert!(messages.contains("completed"));
    provider.response("background result handled");
    assert_eq!(
        process.until("outcome")["result"]["response"],
        "background result handled"
    );
    assert_eq!(process.until("closed")["status"], "completed");
    assert!(process.root.join("project/completed").exists());
    assert!(process.finish().0.success());
    std::fs::remove_file(output).unwrap();
}

#[test]
fn cancel_close_stops_background_bash_without_a_completion_turn() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    process.initialize();
    let output = background_bash(&mut process, &provider);
    assert_eq!(process.close("cancel")["status"], "cancelled");
    assert!(!process.root.join("project/completed").exists());
    assert!(process.finish().0.success());
    std::fs::remove_file(output).unwrap();
}

#[test]
fn eof_drains_repeated_inputs_and_reports_outcomes_not_receipts() {
    let provider = Provider::start();
    let response = |text: &str| {
        json!({"id":"reused-response","model":"test",
            "choices":[{"delta":{"content":text},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":5,"completion_tokens":2,"total_tokens":7}})
    };
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    let root_id = process.initialize();
    process.input("first", "immediate", "first");
    let first = process.until("admission");
    assert_eq!(first["disposition"]["kind"], "started");
    provider.request();
    process.input("queued", "follow_up", "second");
    let queued = process.until("admission");
    assert_eq!(queued["disposition"]["kind"], "queued");
    drop(process.child.stdin.take());
    provider.reply(response("first result"));
    provider.request();
    provider.reply(response("second result"));
    let mut outcomes = Vec::new();
    let mut progress = Vec::new();
    let mut running_turns = Vec::new();
    loop {
        let frame = process.receive();
        if frame["type"] == "event" || frame["type"] == "outcome" {
            assert_eq!(frame["agent_id"], root_id, "{frame}");
        }
        if frame["type"] == "outcome" {
            outcomes.push(frame);
        } else if frame["type"] == "event" && frame["payload"]["kind"] == "progress" {
            assert!(frame.get("task_id").is_none());
            let update = &frame["payload"]["progress"];
            assert_eq!(update["tool_uses"], 0);
            if update["status"] == "running" {
                running_turns.push(update["turn_id"].clone());
            } else if update["status"] == "completed" {
                progress.push(update.clone());
            }
        } else if frame["type"] == "closed" {
            assert_eq!(frame["status"], "completed");
            break;
        }
    }
    assert_eq!(outcomes.len(), 2);
    assert_ne!(outcomes[0]["turn_id"], outcomes[1]["turn_id"]);
    assert_eq!(outcomes[1]["result"]["response"], "second result");
    assert_eq!(progress.len(), 2);
    for (terminal, outcome) in progress.iter().zip(&outcomes) {
        assert_eq!(terminal["turn_id"], outcome["turn_id"]);
        assert!(running_turns.contains(&terminal["turn_id"]));
        assert_eq!(terminal["usage"]["input_tokens"], 5);
        assert_eq!(terminal["usage"]["output_tokens"], 2);
    }
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn interrupt_discards_queued_input_and_allows_another_turn() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    process.initialize();
    process.input("first", "immediate", "first");
    let first = process.until("admission");
    provider.request();
    process.input("queued", "follow_up", "must be discarded");
    assert_eq!(process.until("admission")["disposition"]["kind"], "queued");
    process.control("interrupt", json!({"kind":"interrupt"}));
    let mut cancelled = None;
    let mut ack = false;
    while cancelled.is_none() || !ack {
        let frame = process.receive();
        if frame["type"] == "outcome" {
            cancelled = Some(frame);
        } else if frame["control_id"] == "interrupt" {
            ack = true;
        }
    }
    let cancelled = cancelled.unwrap();
    assert_eq!(cancelled["turn_id"], first["disposition"]["turn_id"]);
    assert_eq!(cancelled["result"]["status"], "cancelled");
    assert_eq!(cancelled["result"]["cancellation_reason"], "interrupted");
    provider.response("cancelled request");
    process.input("next", "immediate", "next");
    assert_eq!(process.until("admission")["disposition"]["kind"], "started");
    provider.request();
    provider.response("after interrupt");
    assert_eq!(
        process.until("outcome")["result"]["response"],
        "after interrupt"
    );
    assert_eq!(process.close("drain")["status"], "completed");
    assert!(process.finish().0.success());
}

#[test]
fn eof_with_unanswerable_permission_fails_and_finalizes() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    process.initialize();
    process.input("tool", "immediate", "run command");
    process.until("admission");
    provider.request();
    provider.permission();
    assert_eq!(process.until("question")["request"]["kind"], "permission");
    drop(process.child.stdin.take());
    let closed = process.until("closed");
    assert_eq!(closed["status"], "failed");
    assert_eq!(closed["reason"], "input_unavailable");
    let (status, stderr) = process.finish();
    assert!(!status.success(), "{stderr}");
}

#[test]
fn drain_keeps_answers_live_and_rejects_wrong_or_stale_answers() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    process.initialize();
    process.input("tool", "immediate", "run command");
    process.until("admission");
    provider.request();
    provider.permission();
    let question = process.until("question");
    let id = question["question_id"].as_str().unwrap();
    process.control("drain", json!({"kind":"close","mode":"drain"}));
    assert_eq!(process.reply("drain")["result"]["status"], "ok");
    process.input("late", "immediate", "late");
    assert_eq!(
        process.until("admission")["disposition"]["error"]["code"],
        "closing"
    );
    process.send(json!({"type":"answer","question_id":id,"response":{"kind":"permission","decision":"allow_always"}}));
    assert_eq!(process.until("question_closed")["status"], "rejected");
    process.send(json!({"type":"answer","question_id":id,"response":{"kind":"permission","decision":"allow_always","pattern":"printf rpc-permission"}}));
    assert_eq!(process.until("question_closed")["status"], "answered");
    provider.request();
    process.send(json!({"type":"answer","question_id":id,"response":{"kind":"permission","decision":"allow_once"}}));
    assert_eq!(process.until("question_closed")["status"], "rejected");
    provider.response("authorized result");
    assert_eq!(process.until("closed")["status"], "completed");
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn received_answer_is_settled_before_eof_drain() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    process.initialize();
    process.input("tool", "immediate", "run command");
    process.until("admission");
    provider.request();
    provider.permission();
    let question = process.until("question");
    process.send(json!({"type":"answer","question_id":question["question_id"],"response":{"kind":"permission","decision":"allow_always","pattern":"printf rpc-permission"}}));
    drop(process.child.stdin.take());
    provider.request();
    provider.response("answer survived EOF");
    let closed = process.until("closed");
    assert_eq!(closed["status"], "completed", "{closed}");
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn close_drain_can_escalate_to_cancel() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    process.initialize();
    process.input("first", "immediate", "first");
    process.until("admission");
    provider.request();
    process.control("drain", json!({"kind":"close","mode":"drain"}));
    assert_eq!(process.reply("drain")["result"]["status"], "ok");
    let closed = process.close("cancel");
    assert_eq!(closed["status"], "cancelled", "{closed}");
    assert_eq!(closed["reason"], "close_cancel");
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn root_failure_remains_controllable_but_makes_close_fail() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    process.initialize();
    process.input("failed", "immediate", "fails");
    process.until("admission");
    provider.request();
    provider.failure();
    let outcome = process.until("outcome");
    assert_eq!(outcome["result"]["status"], "failed");
    assert!(outcome["result"]["failure"]["kind"].is_string());
    process.input("next", "immediate", "later saved input");
    process.until("admission");
    provider.request();
    provider.response("later saved response");
    process.until("outcome");
    let closed = process.close("cancel");
    assert_eq!(closed["reason"], "execution_failed");
    assert!(!process.finish().0.success());
    assert!(
        saved_paths(&process.root.join("sessions"))
            .iter()
            .any(|path| std::fs::read_to_string(path)
                .unwrap()
                .contains("later saved response"))
    );
}

#[test]
fn broken_stdout_exits_while_stdin_is_open() {
    let mut process = RpcProcess::start(OutputMode::Closed);
    process.send(json!({"id":1,"method":"initialize"}));
    let (status, stderr) = process.finish();
    assert!(!status.success(), "{stderr}");
}

#[test]
fn one_blocked_physical_write_is_supervised_without_eof() {
    let provider = Provider::start();
    let mut process =
        RpcProcess::with_provider(OutputMode::Unread, &provider.url(), Tools::Disabled);
    process.control(
        "init",
        json!({"kind":"initialize","version":2,"startup":{"kind":"new","persistence":"saved"}}),
    );
    let mut stdout = std::io::BufReader::new(process.child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    let ready: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(ready["result"]["status"], "ok", "{ready}");
    process.input("large", "immediate", "large response");
    provider.request();
    provider.response(&"x".repeat(128 * 1024));
    let (status, stderr) = process.finish();
    assert!(!status.success(), "{stderr}");
    drop(stdout);
}

fn saved_paths(directory: &std::path::Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            paths.extend(saved_paths(&path));
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            paths.push(path);
        }
    }
    paths
}

#[test]
fn saved_id_resume_preserves_canonical_history_and_rejects_path_overrides() {
    let provider = Provider::start();
    let mut first = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    let id = first.initialize();
    first.input("first", "immediate", "saved prompt");
    first.until("admission");
    provider.request();
    provider.response("saved response");
    first.until("outcome");
    first.close("drain");
    assert!(first.finish().0.success());
    let paths = saved_paths(&first.root.join("sessions"));
    assert_eq!(paths.len(), 1);
    let mut resumed = RpcProcess::in_directory(
        first.root.clone(),
        OutputMode::Read,
        &provider.url(),
        Tools::Disabled,
    );
    for invalid in [
        paths[0].to_string_lossy().to_string(),
        id[..id.len() - 1].to_string(),
    ] {
        resumed.control("invalid", json!({"kind":"initialize","version":2,"startup":{"kind":"resume","persistence":"saved","session_id":invalid}}));
        assert_eq!(
            resumed.reply("invalid")["result"]["error"]["code"],
            "startup_failed"
        );
    }
    resumed.control("resume", json!({"kind":"initialize","version":2,"startup":{"kind":"resume","persistence":"saved","session_id":id}}));
    let reply = resumed.reply("resume");
    assert_eq!(reply["result"]["value"]["session_id"], id, "{reply}");
    resumed.input("next", "immediate", "continued prompt");
    resumed.until("admission");
    let request = provider.request();
    assert!(request["messages"].to_string().contains("saved response"));
    provider.response("continued response");
    resumed.until("outcome");
    resumed.close("drain");
    assert!(resumed.finish().0.success());
    let history = std::fs::read_to_string(&paths[0]).unwrap();
    assert!(history.contains("continued response"));
    assert_eq!(saved_paths(&first.root.join("sessions")).len(), 1);
}

#[test]
fn finalization_save_error_is_not_a_successful_close() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    process.initialize();
    process.input("first", "immediate", "save failure");
    process.until("admission");
    provider.request();
    provider.response("canonical response");
    process.until("outcome");
    let directory = process.root.join("sessions");
    std::fs::rename(&directory, process.root.join("saved-backup")).unwrap();
    std::fs::write(directory, "not a directory").unwrap();
    let closed = process.close("drain");
    assert_eq!(closed["status"], "failed", "{closed}");
    assert_eq!(closed["reason"], "finalization_failed");
    assert!(!process.finish().0.success());
}

fn directed_child(process: &mut RpcProcess, provider: &Provider) -> String {
    let root_id = process.initialize();
    process.send(json!({"type":"input","input_id":"delegate","dispatch":"immediate","directed":{"agent":"probe"},"prompt":{"text":"inspect"}}));
    assert_eq!(process.until("admission")["disposition"]["kind"], "started");
    assert_eq!(provider.request()["model"], "test");
    provider.delegate("probe");
    loop {
        let request = provider.request();
        if request["model"] == "child" {
            return root_id;
        }
        assert_eq!(request["model"], "test");
        provider.response("parent initial result");
    }
}

#[test]
fn eof_waits_for_directed_child_then_the_final_parent_turn() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    let root_id = directed_child(&mut process, &provider);
    drop(process.child.stdin.take());
    provider.response("child result");
    loop {
        let parent = provider.request();
        assert_eq!(parent["model"], "test");
        assert!(process.child.try_wait().unwrap().is_none());
        if parent["messages"]
            .to_string()
            .contains("Subagent terminal outcome:")
        {
            provider.response("final parent result");
            break;
        }
        provider.response("parent initial result");
    }
    let mut root_response = None;
    let mut child_outcome = false;
    loop {
        let frame = process.receive();
        if frame["type"] == "outcome" {
            if frame["agent_id"] == root_id {
                root_response = Some(frame["result"]["response"].clone());
            } else {
                child_outcome = true;
                assert!(frame["task_id"].is_string());
                assert_eq!(frame["task_id"], frame["turn_id"]);
            }
        } else if frame["type"] == "closed" {
            assert_eq!(frame["status"], "completed", "{frame}");
            break;
        }
    }
    assert_eq!(root_response, Some(json!("final parent result")));
    assert!(child_outcome);
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
    let paths = saved_paths(&process.root.join("sessions"));
    assert_eq!(paths.len(), 2);
    assert!(paths.iter().any(|path| {
        std::fs::read_to_string(path)
            .unwrap()
            .contains("final parent result")
    }));
}

fn settled_child(process: &mut RpcProcess, provider: &Provider) -> (String, String) {
    let root_id = directed_child(process, provider);
    provider.permission();
    let question = process.until("question");
    assert_ne!(question["agent_id"], root_id);
    let child_id = question["agent_id"].as_str().unwrap().to_owned();
    process.send(json!({"type":"answer","question_id":question["question_id"],"response":{"kind":"permission","decision":"allow_once"}}));
    process.until("question_closed");
    loop {
        let request = provider.request();
        if request["model"] == "child" {
            provider.response("child first result");
            continue;
        }
        assert_eq!(request["model"], "test");
        let final_parent = request["messages"]
            .to_string()
            .contains("Subagent terminal outcome:");
        provider.response(if final_parent {
            "root fully settled"
        } else {
            "root initial"
        });
        if final_parent {
            break;
        }
    }
    let mut child_finished = false;
    let mut parent_finished = false;
    while !child_finished || !parent_finished {
        let frame = process.receive();
        if frame["type"] == "outcome" {
            if frame["agent_id"] == child_id {
                child_finished = true;
            }
            if frame["agent_id"] == root_id && frame["result"]["response"] == "root fully settled" {
                parent_finished = true;
            }
        }
    }
    std::thread::sleep(Duration::from_millis(200));
    (root_id, child_id)
}

#[test]
fn retained_child_permissions_remain_reachable() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    let (_, child_id) = settled_child(&mut process, &provider);
    process.input("reuse", "immediate", "invoke retained child");
    assert_eq!(provider.request()["model"], "test");
    provider.tool(
        "Agent",
        json!({"action":"message","recipient":child_id,"content":"second invocation"}),
    );
    loop {
        let request = provider.request();
        if request["model"] == "child" {
            provider.permission();
            break;
        }
        provider.response("invocation accepted");
    }
    let question = process.until("question");
    assert_eq!(question["agent_id"], child_id);
    assert!(
        question.get("task_id").is_none(),
        "later work inherited a stale initial-turn binding: {question}"
    );
    process.send(json!({"type":"answer","question_id":question["question_id"],"response":{"kind":"permission","decision":"allow_once"}}));
    process.until("question_closed");
    loop {
        let request = provider.request();
        provider.response("invocation finished");
        if request["model"] == "child" {
            break;
        }
    }
    let mut child_outcome = false;
    let mut child_progress = false;
    while !child_outcome || !child_progress {
        let frame = process.receive();
        if frame["agent_id"] != child_id {
            continue;
        }
        if frame["type"] == "event" && frame["payload"]["kind"] == "progress" {
            let progress = &frame["payload"]["progress"];
            assert!(
                frame.get("task_id").is_none(),
                "later progress inherited initial-turn identity: {frame}"
            );
            assert_eq!(progress["turn_id"], question["turn_id"]);
            assert_eq!(progress["tool_uses"], 1);
            child_progress |= progress["status"] == "completed";
        } else if frame["type"] == "outcome" {
            assert!(
                frame.get("task_id").is_none(),
                "later outcome inherited initial-turn identity: {frame}"
            );
            child_outcome = true;
        }
    }
    let request = provider.request();
    assert_eq!(request["model"], "test");
    assert!(
        request["messages"]
            .to_string()
            .contains("invocation finished")
    );
    provider.response("follow-up result received");
    assert_eq!(process.close("drain")["status"], "completed");
    assert!(process.finish().0.success());
}

#[test]
fn retained_child_question_after_eof_is_fatal() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    let (_, child_id) = settled_child(&mut process, &provider);
    process.input("reuse", "immediate", "invoke retained child");
    provider.request();
    provider.tool(
        "Agent",
        json!({"action":"message","recipient":child_id,"content":"second invocation"}),
    );
    loop {
        let request = provider.request();
        if request["model"] == "child" {
            drop(process.child.stdin.take());
            provider.permission();
            break;
        }
        provider.response("invocation accepted");
    }
    let closed = process.until("closed");
    assert_eq!(closed["status"], "failed", "{closed}");
    assert_eq!(closed["reason"], "input_unavailable", "{closed}");
    assert!(!process.finish().0.success());
}

#[test]
fn drain_waits_for_retained_invocation() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    let (root_id, child_id) = settled_child(&mut process, &provider);
    process.input("reuse", "immediate", "invoke retained child");
    process.until("admission");
    provider.request();
    provider.tool(
        "Agent",
        json!({"action":"message","recipient":child_id,"content":"second invocation"}),
    );
    let mut held_child = None;
    let mut parent_replied = false;
    while held_child.is_none() || !parent_replied {
        let request = provider.request();
        if request["model"] == "child" {
            held_child = provider.replies.borrow_mut().take();
        } else {
            provider.response("second admission accepted");
            parent_replied = true;
        }
    }
    loop {
        let frame = process.receive();
        if frame["type"] == "outcome"
            && frame["agent_id"] == root_id
            && frame["result"]["response"] == "second admission accepted"
        {
            break;
        }
    }
    process.control("drain", json!({"kind":"close","mode":"drain"}));
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        if let Ok(Ok(frame)) = process
            .output
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            assert_ne!(
                frame["type"], "closed",
                "drain finalized admitted work before provider reply: {frame}"
            );
        }
    }
    held_child.unwrap().send(ProviderReply::Chunk(json!({"id":"completion","model":"child","choices":[{"delta":{"content":"second child finished"},"finish_reason":"stop"}]}))).unwrap();
    let request = provider.request();
    assert_eq!(request["model"], "test");
    assert!(
        request["messages"]
            .to_string()
            .contains("second child finished")
    );
    provider.response("follow-up result received");
    assert_eq!(process.until("closed")["status"], "completed");
    assert!(process.finish().0.success());
}

#[test]
fn a_child_question_discovered_after_eof_is_fatal() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    directed_child(&mut process, &provider);
    drop(process.child.stdin.take());
    provider.permission();
    let closed = process.until("closed");
    assert_eq!(closed["status"], "failed", "{closed}");
    assert_eq!(closed["reason"], "input_unavailable");
    assert!(!process.finish().0.success());
}

#[test]
fn nested_question_targets_are_root_scoped_and_task_stop_remains_live() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    let root_id = directed_child(&mut process, &provider);
    provider.delegate("leaf");
    loop {
        let request = provider.request();
        if request["model"] == "nested" {
            provider.permission();
            break;
        }
        provider.response("waiting for leaf");
    }
    let question = process.until("question");
    assert_ne!(question["agent_id"], root_id);
    let task_id = question["task_id"].as_str().unwrap();
    assert_eq!(question["task_id"], question["turn_id"]);
    assert_ne!(question["task_id"], question["agent_id"]);
    process.control(
        "instance",
        json!({"kind":"task_stop","task_id":question["agent_id"]}),
    );
    assert_eq!(
        process.reply("instance")["result"]["error"]["code"],
        "invalid_target"
    );
    process.control("wrong", json!({"kind":"task_stop","task_id":"unrelated"}));
    assert_eq!(
        process.reply("wrong")["result"]["error"]["code"],
        "invalid_target"
    );
    process.control("stop", json!({"kind":"task_stop","task_id":task_id}));
    let stopped = process.reply("stop");
    assert_eq!(stopped["result"]["status"], "ok", "{stopped}");
    let closed = process.close("cancel");
    assert_eq!(closed["status"], "cancelled", "{closed}");
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
}

#[test]
fn task_stop_rejects_checklist_ids_and_preserves_task_files() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    let root_id = process.initialize();
    process.send(
        json!({"type":"input","input_id":"checklist","dispatch":"immediate",
        "prompt":{"text":"record unfinished work","tool_policy":{"allowances":[["Task","*"]]}}}),
    );
    assert_eq!(process.until("admission")["disposition"]["kind"], "started");
    let request = provider.request();
    assert!(
        request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "Task")
    );
    provider.tool("Task", json!({"action":"create","subject":"Unfinished review","description":"Independent from background execution"}));
    let request = provider.request();
    let result = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .unwrap();
    let result: Value = serde_json::from_str(result["content"].as_str().unwrap()).unwrap();
    let checklist_id = result["task"]["id"].as_str().unwrap();
    assert_eq!(checklist_id, "1");
    provider.tool(
        "Task",
        json!({"action":"update","task_id":checklist_id,"status":"in_progress","owner":root_id}),
    );
    provider.request();
    provider.response("checklist recorded");
    let mut observed_tools = std::collections::HashSet::new();
    loop {
        let frame = process.receive();
        if frame["type"] == "event" {
            assert!(frame.get("task_id").is_none(), "{frame}");
            for name in ["Task"] {
                if frame["payload"].to_string().contains(name) {
                    observed_tools.insert(name);
                }
            }
        } else if frame["type"] == "outcome" {
            assert_eq!(frame["agent_id"], root_id);
            assert!(frame.get("task_id").is_none(), "{frame}");
            assert_ne!(frame["turn_id"], checklist_id);
            assert_eq!(frame["result"]["response"], "checklist recorded");
            break;
        }
    }
    assert_eq!(observed_tools.len(), 1);
    let task_path = process
        .root
        .join("config/tasks")
        .join(&root_id)
        .join(format!("{checklist_id}.json"));
    let task_bytes = std::fs::read(&task_path).unwrap();
    let task: Value = serde_json::from_slice(&task_bytes).unwrap();
    assert_eq!(task["status"], "in_progress");
    assert_eq!(task["owner"]["instance_id"], root_id);

    process.send(
        json!({"type":"input","input_id":"delegate","dispatch":"immediate",
        "directed":{"agent":"probe"},"prompt":{"text":"inspect without changing the checklist"}}),
    );
    process.until("admission");
    provider.request();
    provider.delegate("probe");
    loop {
        let request = provider.request();
        if request["model"] == "child" {
            provider.permission();
            break;
        }
        provider.response("parent initial result");
    }
    let question = process.until("question");
    let running_id = question["task_id"].as_str().unwrap();
    assert_eq!(question["task_id"], question["turn_id"]);
    for invalid in [checklist_id, question["agent_id"].as_str().unwrap()] {
        assert_ne!(running_id, invalid);
        process.control("invalid", json!({"kind":"task_stop","task_id":invalid}));
        assert_eq!(
            process.reply("invalid")["result"]["error"]["code"],
            "invalid_target"
        );
        assert_eq!(std::fs::read(&task_path).unwrap(), task_bytes);
    }
    process.control("stop", json!({"kind":"task_stop","task_id":running_id}));
    assert_eq!(process.reply("stop")["result"]["status"], "ok");
    process.control("stale", json!({"kind":"task_stop","task_id":running_id}));
    assert_eq!(
        process.reply("stale")["result"]["error"]["code"],
        "invalid_target"
    );
    assert_eq!(std::fs::read(&task_path).unwrap(), task_bytes);
    let closed = process.close("cancel");
    assert_eq!(closed["status"], "cancelled", "{closed}");
    let (status, stderr) = process.finish();
    assert!(status.success(), "{stderr}");
    assert_eq!(std::fs::read(&task_path).unwrap(), task_bytes);
    assert_eq!(saved_paths(&process.root.join("sessions")).len(), 2);
}

#[test]
fn permission_accounting_survives_failed_save_and_concurrent_runtime_selection() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Enabled);
    process.initialize();
    process.input("warmup", "immediate", "warmup");
    process.until("admission");
    provider.request();
    provider.response("saved before grant");
    process.until("outcome");
    process.input("tool", "immediate", "run command");
    process.until("admission");
    provider.request();
    provider.permission();
    let question = process.until("question");
    process.control(
        "select",
        json!({"kind":"configure","selection":{"kind":"model","selection":"local/test"}}),
    );
    let sessions = process.root.join("sessions");
    let backup = process.root.join("backup");
    std::fs::rename(&sessions, &backup).unwrap();
    std::fs::write(&sessions, "not a directory").unwrap();
    process.send(json!({"type":"answer","question_id":question["question_id"],"response":{"kind":"permission","decision":"allow_always","pattern":"printf rpc-permission"}}));
    let mut answered = false;
    let mut selected = false;
    while !answered || !selected {
        let frame = process.receive();
        if frame["type"] == "question_closed" {
            assert_eq!(frame["status"], "answered", "{frame}");
            assert!(frame["persistence_warning"].is_string(), "{frame}");
            answered = true;
        } else if frame["control_id"] == "select" {
            assert_eq!(frame["result"]["status"], "error");
            selected = true;
        }
    }
    provider.request();
    provider.permission();
    provider.request();
    provider.response("permission was applied");
    loop {
        let frame = process.receive();
        assert_ne!(
            frame["type"], "question",
            "applied grant must not roll back after a save failure"
        );
        if frame["type"] == "outcome" {
            assert_eq!(frame["result"]["status"], "completed");
            break;
        }
    }
    let closed = process.close("drain");
    assert_eq!(closed["reason"], "finalization_failed", "{closed}");
    assert!(!process.finish().0.success());
}

#[test]
fn steers_keep_sdk_submission_identity_without_inventing_extra_turns() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    process.initialize();
    process.input("first", "immediate", "first");
    let first = process.until("admission");
    provider.request();
    let mut submissions = std::collections::HashSet::new();
    for id in ["steer-one", "steer-two"] {
        process.input(id, "steer", id);
        let queued = process.until("admission");
        assert_eq!(queued["disposition"]["kind"], "queued");
        assert_eq!(queued["disposition"]["dispatch"], "steer");
        submissions.insert(
            queued["disposition"]["submission_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    process.input("busy", "immediate", "busy");
    assert_eq!(
        process.until("admission")["disposition"]["kind"],
        "rejected"
    );
    drop(process.child.stdin.take());
    provider.response("initial response");
    provider.request();
    provider.response("first steer response");
    provider.request();
    provider.response("steered response");
    let mut outcomes = Vec::new();
    let mut injected = std::collections::HashSet::new();
    loop {
        let frame = process.receive();
        if frame["type"] == "outcome" {
            outcomes.push(frame);
        } else if frame["type"] == "event" {
            let envelope = &frame["payload"]["envelope"];
            if envelope["event"]["type"] == "user_input_injected" {
                injected.insert(
                    envelope["event"]["submission_id"]
                        .as_str()
                        .unwrap()
                        .to_string(),
                );
                assert_eq!(envelope["turn_id"], first["disposition"]["turn_id"]);
            }
        } else if frame["type"] == "closed" {
            assert_eq!(frame["status"], "completed", "{frame}");
            break;
        }
    }
    assert_eq!(injected, submissions);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0]["turn_id"], first["disposition"]["turn_id"]);
    assert!(process.finish().0.success());
}

#[test]
fn initialization_can_select_runtime_without_launch_defaults() {
    let root = PathBuf::from("/tmp").join(format!("phi-rpc-no-defaults-{}", uuid::Uuid::new_v4()));
    let mut process = RpcProcess::spawn(
        root,
        OutputMode::Read,
        "http://127.0.0.1:1/v1",
        Tools::Disabled,
        ProcessSelection::Initialize,
    );
    process.control("init",json!({"kind":"initialize","version":2,"startup":{"kind":"new","persistence":"ephemeral","provider":"local","model":"test"}}));
    let initialized = process.reply("init");
    assert_eq!(initialized["result"]["status"], "ok", "{initialized}");
    assert_eq!(process.close("drain")["status"], "completed");
    assert!(process.finish().0.success());
}

#[test]
fn fast_turns_correlate_receipts_and_outcomes_without_assuming_arrival_order() {
    let provider = Provider::start();
    let mut process = RpcProcess::with_provider(OutputMode::Read, &provider.url(), Tools::Disabled);
    let root = process.initialize();
    const FAST_TURNS: usize = 12;
    let mut turns = std::collections::HashSet::new();
    for index in 0..FAST_TURNS {
        let id = format!("input-{index}");
        let text = format!("response-{index}");
        process.input(&id, "immediate", "fast");
        provider.request();
        provider.response(&text);
        let mut admission = None;
        let mut outcome = None;
        while admission.is_none() || outcome.is_none() {
            let frame = process.receive();
            if frame["type"] == "admission" {
                assert_eq!(frame["input_id"], id);
                admission = Some(frame);
            } else if frame["type"] == "outcome" {
                assert_eq!(frame["agent_id"], root);
                assert_eq!(frame["result"]["response"], text);
                outcome = Some(frame);
            } else if frame["type"] == "event" {
                assert_ne!(
                    frame["payload"]["envelope"]["event"]["type"],
                    "turn_finished"
                );
            }
        }
        let admission = admission.unwrap();
        let outcome = outcome.unwrap();
        assert_eq!(admission["disposition"]["turn_id"], outcome["turn_id"]);
        assert!(turns.insert(outcome["turn_id"].as_str().unwrap().to_string()));
    }
    process.control("idle", json!({"kind":"interrupt"}));
    loop {
        let frame = process.receive();
        assert_ne!(frame["type"], "outcome");
        if frame["control_id"] == "idle" {
            break;
        }
    }
    assert_eq!(process.close("drain")["status"], "completed");
    assert!(process.finish().0.success());
}

#[test]
fn prompt_controls_return_input_payloads_not_terminal_command_declarations() {
    let mut process = RpcProcess::start(OutputMode::Read);
    process.initialize();
    let prompts = process.root.join("project/.phi/prompts");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(
        prompts.join("inspect.md"),
        "---\ndescription: inspect\n---\nInspect $ARGUMENTS",
    )
    .unwrap();
    process.control("reload", json!({"kind":"reload"}));
    assert_eq!(process.reply("reload")["result"]["status"], "ok");
    process.control("info", json!({"kind":"runtime_info"}));
    let info = process.reply("info");
    assert!(
        info["result"]["value"]["prompts"]
            .as_array()
            .unwrap()
            .contains(&json!("inspect"))
    );
    assert!(info["result"]["value"].get("commands").is_none());
    process.control(
        "expand",
        json!({"kind":"expand_prompt","text":"/inspect workspace"}),
    );
    let expanded = process.reply("expand");
    assert_eq!(
        expanded["result"]["value"]["text"],
        "<command name=\"inspect\">\nInspect workspace\n</command>"
    );
    assert!(expanded["result"]["value"]["tool_policy"].is_object());
    process.control(
        "literal",
        json!({"kind":"expand_prompt","text":"literal text"}),
    );
    assert_eq!(
        process.reply("literal")["result"]["value"]["text"],
        "literal text"
    );
    process.close("drain");
    assert!(process.finish().0.success());
}
