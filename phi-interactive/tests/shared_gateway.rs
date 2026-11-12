#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use phi_remote::RemoteClient;
use phi_sdk::agent_instance::AgentInstanceStatus;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(20);

struct Process {
    root: PathBuf,
    child: Option<Child>,
}

impl Process {
    fn new(provider_port: u16) -> Self {
        let root = PathBuf::from("/tmp").join(format!("phi-shared-{}", uuid::Uuid::new_v4()));
        for directory in ["config", "workspace/.phi", "client-workspace/.phi"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
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
                name.to_owned(),
                json!({
                    "api": api, "baseUrl": format!("http://127.0.0.1:{provider_port}/v1"),
                    "models": {"test": {"contextWindow": 128000}},
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
        std::fs::write(
            root.join("config/models.json"),
            json!({"providers": providers}).to_string(),
        )
        .unwrap();
        std::fs::write(root.join("config/settings.json"), json!({
            "memory": {"enabled": false},
            "sandbox": {"enabled": false}, "extensions": ["!*.lua"], "permission": {"mode": "deny"},
        }).to_string()).unwrap();
        std::fs::write(
            root.join("workspace/.phi/settings.json"),
            "invalid project settings",
        )
        .unwrap();
        std::fs::write(
            root.join("client-workspace/.phi/settings.json"),
            r#"{"defaultProvider":"local","defaultModel":"test"}"#,
        )
        .unwrap();
        Self { root, child: None }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_phi"));
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.root)
            .env("PHI_CONFIG_DIR", self.root.join("config"))
            .env("PHI_AUTH_DIR", self.root.join("auth"))
            .env("PHI_SESSION_DIR", self.root.join("history"))
            .env("PHI_GATEWAY_DIR", self.root.join("gateway"))
            .env("PHI_OFFLINE", "1")
            .current_dir(self.root.join("workspace"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    async fn start(&mut self) -> RemoteClient {
        self.child = Some(
            self.command()
                .args(["gateway", "--port", "0"])
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        loop {
            if let Ok(bytes) = std::fs::read(self.root.join("gateway/endpoint.json"))
                && let Ok(endpoint) = serde_json::from_slice::<Value>(&bytes)
            {
                let client = RemoteClient::connect(
                    endpoint["localEndpoint"].as_str().unwrap(),
                    endpoint["localCredential"].as_str().unwrap(),
                )
                .unwrap();
                if client.sessions().await.is_ok() {
                    return client;
                }
            }
            if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                let output = self.child.take().unwrap().wait_with_output().unwrap();
                panic!(
                    "Gateway exited: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            assert!(Instant::now() < deadline, "Gateway did not become ready");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn close(&mut self, client: &RemoteClient) {
        client.stop().await.unwrap();
        let deadline = Instant::now() + PROCESS_TIMEOUT;
        while self.child.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "Gateway did not finalize");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let output = self.child.take().unwrap().wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!self.root.join("gateway/endpoint.json").exists());
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn clients_require_a_running_gateway_without_preparing_runtime() {
    let process = Process::new(0);
    for entry in ["--web", "--gateway"] {
        let output = process
            .command()
            .args([entry, "--no-context-files"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("start phi gateway"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!process.root.join("gateway").exists());
        assert!(!process.root.join("history").exists());
    }
}

#[tokio::test]
async fn clients_reuse_gateway_and_disconnect_without_cancelling_execution() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (received, request) = tokio::sync::oneshot::channel();
    let (finish, released) = tokio::sync::oneshot::channel();
    let port = listener.local_addr().unwrap().port();
    let provider = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let length = socket.read(&mut buffer).await.unwrap();
            assert_ne!(length, 0);
            bytes.extend_from_slice(&buffer[..length]);
        }
        let header_end = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
            + 4;
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while bytes.len() < header_end + content_length {
            let length = socket.read(&mut buffer).await.unwrap();
            assert_ne!(length, 0);
            bytes.extend_from_slice(&buffer[..length]);
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        received.send(()).unwrap();
        released.await.unwrap();
        socket.write_all(b"data: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"completed\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"test\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").await.unwrap();
    });
    let mut process = Process::new(port);
    let operator = process.start().await;
    assert!(operator.sessions().await.unwrap().is_empty());
    assert!(
        operator
            .create(std::path::Path::new("relative"))
            .await
            .is_err()
    );
    let client = operator.clone();
    let first = client
        .create(&process.root.join("client-workspace"))
        .await
        .unwrap();
    assert_eq!(first.working_dir, process.root.join("client-workspace"));
    let watch = client.watch(&first.id).await.unwrap();
    client
        .message(&first.id, "continue after disconnect")
        .await
        .unwrap();
    tokio::time::timeout(PROCESS_TIMEOUT, request)
        .await
        .unwrap()
        .unwrap();
    let snapshot = operator.snapshot(&first.id).await.unwrap();
    assert_eq!(snapshot.view.snapshot.status, AgentInstanceStatus::Running);
    drop(watch);
    drop(client);
    let duplicate = tokio::process::Command::from(process.command())
        .args(["gateway", "--port", "0"])
        .output()
        .await
        .unwrap();
    assert!(
        duplicate.status.success(),
        "{}",
        String::from_utf8_lossy(&duplicate.stderr)
    );
    assert!(String::from_utf8_lossy(&duplicate.stdout).contains("Phi web:"));
    assert!(
        process
            .child
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    assert_eq!(
        operator
            .snapshot(&first.id)
            .await
            .unwrap()
            .view
            .snapshot
            .status,
        AgentInstanceStatus::Running
    );
    finish.send(()).unwrap();
    provider.await.unwrap();
    tokio::time::timeout(PROCESS_TIMEOUT, async {
        loop {
            let snapshot = operator.snapshot(&first.id).await.unwrap();
            if snapshot.view.snapshot.status != AgentInstanceStatus::Running {
                assert_eq!(
                    snapshot.view.snapshot.status,
                    AgentInstanceStatus::Idle,
                    "{:?}",
                    snapshot.view.snapshot
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    process.close(&operator).await;
    let restarted = process.start().await;
    assert_eq!(restarted.resume(&first.id).await.unwrap().id, first.id);
    process.close(&restarted).await;
}
