use crate::agent::custom::AgentDefinition;
use crate::agent_instance::AgentInstance;
use crate::config::RuntimeConfig;
use crate::factory::{Phi, PhiRuntimeServices};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub(in crate::harness) async fn read_request(stream: &mut TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        assert_ne!(stream.read(&mut buffer).await.unwrap(), 0);
        bytes.extend_from_slice(&buffer);
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

pub(in crate::harness) fn system_text(request: &Value) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "system")
        .map(|message| match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(in crate::harness) fn completion(request: &Value) -> Value {
    let directed = system_text(request).contains("[Directed delegation for this turn only]");
    let replied = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool");
    let delta = if directed && !replied {
        let arguments = json!({"action": "create", "role": "probe", "prompt": "delegated-task"});
        json!({"tool_calls": [{"index": 0, "id": "call_agent", "type": "function",
            "function": {"name": "Agent", "arguments": arguments.to_string()}}]})
    } else {
        json!({"content": "child-result"})
    };
    let reason = if directed && !replied {
        "tool_calls"
    } else {
        "stop"
    };
    json!({"id": "completion", "model": "test", "choices": [{"delta": delta, "finish_reason": reason}]})
}

pub(in crate::harness) async fn mock_provider(
    listener: TcpListener,
    requests: tokio::sync::mpsc::UnboundedSender<Value>,
    respond: impl Fn(&Value) -> Value + Send + 'static,
) {
    mock_provider_with_reply(listener, requests, move |request| {
        std::future::ready(respond(&request))
    })
    .await;
}

pub(in crate::harness) async fn mock_provider_with_reply<F: std::future::Future<Output = Value>>(
    listener: TcpListener,
    requests: tokio::sync::mpsc::UnboundedSender<Value>,
    respond: impl Fn(Value) -> F,
) {
    loop {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        requests.send(request.clone()).unwrap();
        let chunk = respond(request).await;
        let (status, content_type, body) = if chunk.get("error").is_some() {
            ("401 Unauthorized", "application/json", chunk.to_string())
        } else if chunk["choices"][0].get("message").is_some() {
            ("200 OK", "application/json", chunk.to_string())
        } else {
            (
                "200 OK",
                "text/event-stream",
                format!("data: {chunk}\n\ndata: [DONE]\n\n"),
            )
        };
        let _ = stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
    }
}

pub(in crate::harness) struct RegisteredInstance {
    pub(in crate::harness) manager: crate::harness::manager::AgentManager,
    pub(in crate::harness) instance: AgentInstance,
    pub(in crate::harness) phi: Phi,
}

impl std::ops::Deref for RegisteredInstance {
    type Target = AgentInstance;

    fn deref(&self) -> &Self::Target {
        &self.instance
    }
}

pub(in crate::harness) async fn instance_for_provider(
    root: &std::path::Path,
    address: std::net::SocketAddr,
    delegated_child: bool,
) -> RegisteredInstance {
    let path = root.join("models.json");
    std::fs::write(
        &path,
        json!({"providers": {"local": {"api": "openai-completions",
        "baseUrl": format!("http://{address}/v1"), "models": {"test": {"contextWindow": 128000}, "child": {"contextWindow": 128000}}}}})
        .to_string(),
    )
    .unwrap();
    let catalog = Arc::new(
        crate::models::ModelCatalog::load(crate::models::ModelCatalogOptions {
            store_path: root.join("store.json"),
            user_paths: vec![path],
            offline: true,
        })
        .unwrap(),
    );
    let phi = Phi::builder()
        .provider("local")
        .model("test")
        .context(crate::context::load(true))
        .runtime_config(RuntimeConfig {
            custom_providers: catalog.custom_providers().into_iter().collect(),
            extensions: crate::extension::ExtensionConfig {
                memory: Some(crate::extension::memory::MemoryConfig {
                    memory_dir: Some(crate::extension::memory::MemoryDirSettings {
                        local_path: Some(root.join("memory").display().to_string()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        })
        .agent(
            AgentDefinition::new("probe", "inspect", "Perform the delegated task.")
                .model("child")
                .tools(["Agent"])
                .max_turns(6),
        )
        .agent(AgentDefinition::new("leaf", "inspect", "Leaf task only.").tools(["Read"]))
        .build()
        .unwrap()
        .with_runtime_services(PhiRuntimeServices::new().model_catalog(catalog));
    let mut builder = phi
        .agent_instance_builder()
        .task_list(crate::tasks::TaskListBinding::new(root.join("tasks"), "test-list").unwrap())
        .working_dir(root.to_path_buf());
    if delegated_child {
        builder = builder.delegated_child();
    }
    let manager = crate::harness::manager::AgentManager::new();
    let candidate = builder.build().await.unwrap();
    let instance = manager.insert(candidate).unwrap();
    RegisteredInstance {
        manager,
        instance,
        phi,
    }
}
