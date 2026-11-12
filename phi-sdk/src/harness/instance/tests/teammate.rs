use crate::agent::custom::AgentDefinition;
use crate::core::event::PhiEvent;
use crate::core::lifecycle::{TaskGateOutcome, TeammateIdleOutcome, UserPromptSubmitSource};
use crate::core::turn::TurnPrompt;
use crate::factory::{Phi, PhiRuntimeServices};
use crate::harness::instance::AgentInstance;
use crate::harness::instance::handle::{
    AgentInstanceError, AgentInstanceEvent, AgentInstanceStatus, AgentInstanceView,
    SubmissionDispatch,
};
use crate::harness::manager::AgentManager;
use crate::harness::manager::preparation::AgentSpawnRequest;
use crate::hooks::Hook;
use crate::tasks::TaskListBinding;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tokio::sync::{mpsc, watch};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TEAMMATE_CONTINUATIONS: usize = 8;
const IDLE_WORK_POLL_INTERVAL: Duration = Duration::from_secs(1);
const HTTP_OK: &str = "HTTP/1.1 200 OK";

#[derive(Clone, Copy)]
enum TaskPolicy {
    Unavailable,
    Allowed,
    Denied,
    Ask,
    Removed,
}

struct TeamFixture {
    root: std::path::PathBuf,
    manager: AgentManager,
    lead: AgentInstance,
    member: AgentInstance,
    phi: Phi,
    requests: mpsc::UnboundedReceiver<Value>,
    release: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for TeamFixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> Value {
    let mut bytes = Vec::new();
    loop {
        let mut buffer = [0; 4096];
        let count = stream.read(&mut buffer).await.unwrap();
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

impl TeamFixture {
    async fn new(hooks: Vec<Hook>) -> Self {
        Self::with_policy(hooks, TaskPolicy::Unavailable).await
    }

    async fn with_policy(hooks: Vec<Hook>, policy: TaskPolicy) -> Self {
        let root = std::path::Path::new("/tmp")
            .join(format!("phi-teammate-actor-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let first_reply = release.clone();
        let server = tokio::spawn(async move {
            let mut count = 0;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_request(&mut stream).await;
                sender.send(request).unwrap();
                count += 1;
                if count == 1 {
                    first_reply.notified().await;
                }
                let chunk = json!({"id": "reply", "model": "test", "choices": [{
                    "delta": {"content": format!("review-result-{count}")}, "finish_reason": "stop"
                }]});
                let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                let response = format!(
                    "{HTTP_OK}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            }
        });
        let catalog = crate::models::ModelCatalog::for_test_model(crate::models::ModelDescriptor {
            provider: "local".into(),
            id: "test".into(),
            name: "Test".into(),
            api: crate::provider::ProviderApi::OpenAiCompletions,
            reasoning: false,
            thinking_level_map: None,
            input: vec![crate::models::ModelInput::Text],
            context_window: 128_000,
            max_output_tokens: 16_384,
            cost: Default::default(),
        })
        .unwrap();
        let phi = Phi::builder()
            .provider("local")
            .model("test")
            .no_tools(matches!(policy, TaskPolicy::Unavailable))
            .runtime_config(crate::config::RuntimeConfig {
                custom_providers: std::collections::HashMap::from([(
                    "local".into(),
                    crate::config::CustomProviderConfig {
                        api: crate::provider::ProviderApi::OpenAiCompletions,
                        base_url: Some(format!("http://{address}/v1")),
                        api_key: None,
                    },
                )]),
                ..Default::default()
            })
            .agent(
                AgentDefinition::new(
                    "reviewer",
                    "Review",
                    "Reviewer specialization: inspect the assigned work.",
                )
                .disallowed_tools(if matches!(policy, TaskPolicy::Removed) {
                    vec!["Task"]
                } else {
                    Vec::new()
                }),
            )
            .hooks(hooks)
            .build()
            .unwrap()
            .with_runtime_services(
                PhiRuntimeServices::new()
                    .model_catalog(Arc::new(catalog))
                    .interactive(matches!(policy, TaskPolicy::Ask))
                    .permission_policy(
                        crate::permission::PermissionConfig {
                            mode: crate::permission::PermissionMode::Allow,
                            tools: std::collections::HashMap::from([(
                                "Task".into(),
                                crate::permission::ToolPerm::Simple(match policy {
                                    TaskPolicy::Denied => crate::permission::Action::Deny,
                                    TaskPolicy::Ask => crate::permission::Action::Ask,
                                    _ => crate::permission::Action::Allow,
                                }),
                            )]),
                            ..Default::default()
                        },
                        Some(root.clone()),
                    ),
            );
        let manager = AgentManager::new();
        let mut lead_phi = phi.clone();
        lead_phi.hooks.register_hook(Hook::agent_start(|_| async {
            std::future::pending::<crate::hooks::HookResult<()>>().await
        }));
        let lead = manager
            .insert(
                lead_phi
                    .agent_instance_builder()
                    .working_dir(root.clone())
                    .task_list(TaskListBinding::new(root.join("tasks"), "idle-list").unwrap())
                    .build()
                    .await
                    .unwrap(),
            )
            .unwrap();
        manager
            .initialize_team(
                lead.id(),
                crate::agent_manager::TeamStorageRoot::new(root.join("teams")).unwrap(),
            )
            .await
            .unwrap();
        let mut request = AgentSpawnRequest::new("reviewer", "Review the assigned work.");
        request.alias = Some(crate::agent_manager::AgentAlias::new("security").unwrap());
        let creation = manager.spawn_agent(lead.id(), &phi, request).await.unwrap();
        let member = manager.get(creation.instance_id()).unwrap();
        Self {
            root,
            manager,
            lead,
            member,
            phi,
            requests,
            release,
            server,
        }
    }

    async fn request(&mut self) -> Value {
        tokio::time::timeout(TEST_TIMEOUT, self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn settled(&self) -> AgentInstanceView {
        tokio::time::timeout(TEST_TIMEOUT, self.member.wait_for_quiescence(None))
            .await
            .unwrap()
            .unwrap()
    }

    fn pending_task(&self) -> std::path::PathBuf {
        let path = self.owned_task();
        let mut task: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        task["status"] = json!("pending");
        task.as_object_mut().unwrap().remove("owner");
        std::fs::write(&path, task.to_string()).unwrap();
        path
    }

    fn owned_task(&self) -> std::path::PathBuf {
        let directory = self.root.join("tasks/idle-list");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("1.json");
        std::fs::write(
            &path,
            json!({
                "id": "1", "subject": "Review", "description": "Finish review",
                "status": "in_progress", "owner": {"instance_id": self.member.id()},
                "blocked_by": [], "metadata": {}
            })
            .to_string(),
        )
        .unwrap();
        path
    }
}

fn internal_prompt(text: &str) -> TurnPrompt {
    TurnPrompt {
        prompt: text.into(),
        source: UserPromptSubmitSource::Internal,
        injected_context: Vec::new(),
        images: Vec::new(),
        tool_policy: Default::default(),
    }
}

#[tokio::test]
async fn prompt_context_covers_child_and_task_continuations() {
    let mut fixture = TeamFixture::with_policy(
        vec![Hook::prompt_context(|event| async move {
            if !event.delegated_child {
                return Ok(crate::core::lifecycle::PromptContextOutcome::default());
            }
            Ok(crate::core::lifecycle::PromptContextOutcome {
                contexts: vec![crate::context::InjectedContext {
                    source: crate::transcript::ContextSource::Hook,
                    content: format!("prepared-context:{}", event.prompt),
                }],
                ..Default::default()
            })
        })],
        TaskPolicy::Allowed,
    )
    .await;
    let request = fixture.request().await.to_string();
    assert!(request.contains("prepared-context:Review the assigned work."));
    fixture.release.notify_one();
    fixture.settled().await;
    let path = fixture.pending_task();
    let request = fixture.request().await.to_string();
    assert!(
        request.contains("prepared-context:Task 1 is assigned to you."),
        "{request}"
    );
    let mut task: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    task["status"] = json!("completed");
    std::fs::write(path, task.to_string()).unwrap();
    fixture.settled().await;
    fixture.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_success_waits_for_lifecycle_completion() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut fixture = TeamFixture::new(vec![Hook::agent_end({
        let entered = entered.clone();
        let release = release.clone();
        move |_| {
            let entered = entered.clone();
            let release = release.clone();
            async move {
                entered.notify_one();
                release.notified().await;
                Ok(())
            }
        }
    })])
    .await;
    let mut events = fixture.member.subscribe();
    fixture.request().await;
    fixture.release.notify_one();
    tokio::time::timeout(TEST_TIMEOUT, entered.notified())
        .await
        .unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(150), async {
        loop {
            if matches!(events.recv().await.unwrap(), AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, PhiEvent::TurnFinished { .. })) { break; }
        }
    }).await.is_err());
    let view = fixture.member.view().await.unwrap();
    assert!(view.snapshot.active_turn_id.is_some());
    release.notify_one();
    fixture.settled().await;
    assert!(std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, PhiEvent::TurnFinished { .. }))));
    fixture.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn later_teammate_idle_reports_answer_once_to_lead() {
    let mut fixture = TeamFixture::new(Vec::new()).await;
    fixture.request().await;
    fixture.release.notify_one();
    fixture.settled().await;
    let inbox = fixture
        .root
        .join("teams")
        .join(crate::harness::manager::team::team_namespace(fixture.lead.id().as_str()).unwrap())
        .join("inboxes/lead.json");
    let initial: Value = serde_json::from_slice(&std::fs::read(&inbox).unwrap()).unwrap();
    assert!(initial["messages"].as_array().unwrap().is_empty());

    fixture
        .member
        .submit(internal_prompt("Report again."))
        .await
        .unwrap();
    fixture.request().await;
    fixture.settled().await;
    tokio::time::timeout(TEST_TIMEOUT, async {
        loop {
            let record: Value = serde_json::from_slice(&std::fs::read(&inbox).unwrap()).unwrap();
            if record["messages"]
                .as_array()
                .is_some_and(|messages| !messages.is_empty())
            {
                assert_eq!(record["messages"].as_array().unwrap().len(), 1);
                assert_eq!(record["messages"][0]["kind"], "teammate_idle");
                assert!(
                    record["messages"][0]["content"]
                        .as_str()
                        .unwrap()
                        .contains("review-result-2")
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn teammate_claim_requires_available_and_authorized_task_update() {
    for policy in [
        TaskPolicy::Unavailable,
        TaskPolicy::Removed,
        TaskPolicy::Denied,
    ] {
        let idle = Arc::new(AtomicUsize::new(0));
        let mut fixture = TeamFixture::with_policy(
            vec![Hook::teammate_idle({
                let idle = idle.clone();
                move |_| {
                    idle.fetch_add(1, Ordering::SeqCst);
                    async { Ok(TeammateIdleOutcome::Allow) }
                }
            })],
            policy,
        )
        .await;
        fixture.request().await;
        let path = fixture.pending_task();
        let before = std::fs::read(&path).unwrap();
        fixture.release.notify_one();
        assert_eq!(
            fixture.settled().await.snapshot.status,
            AgentInstanceStatus::Idle
        );
        tokio::time::sleep(IDLE_WORK_POLL_INTERVAL + Duration::from_millis(100)).await;
        assert_eq!(idle.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(fixture.requests.try_recv().is_err());
        fixture.manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn teammate_poll_claims_without_repeating_idle_hooks_or_completing_work() {
    let idle = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut fixture = TeamFixture::with_policy(
        vec![
            Hook::teammate_idle({
                let idle = idle.clone();
                move |_| {
                    idle.fetch_add(1, Ordering::SeqCst);
                    async { Ok(TeammateIdleOutcome::Allow) }
                }
            }),
            Hook::user_prompt_submit({
                let entered = entered.clone();
                let release = release.clone();
                move |event| {
                    let entered = entered.clone();
                    let release = release.clone();
                    let prompt = event.prompt.clone();
                    let automatic = prompt.starts_with("Task 1 is assigned to you.");
                    async move {
                        if automatic {
                            entered.notify_one();
                            release.notified().await;
                        }
                        Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                            notices: Vec::new(),
                            prompt,
                            injected_context: Vec::new(),
                        })
                    }
                }
            }),
        ],
        TaskPolicy::Allowed,
    )
    .await;
    fixture.request().await;
    fixture.release.notify_one();
    assert_eq!(
        fixture.settled().await.snapshot.status,
        AgentInstanceStatus::Idle
    );
    tokio::time::sleep(IDLE_WORK_POLL_INTERVAL + Duration::from_millis(100)).await;
    assert_eq!(idle.load(Ordering::SeqCst), 1);
    assert!(fixture.requests.try_recv().is_err());
    let path = fixture.pending_task();
    tokio::time::timeout(TEST_TIMEOUT, entered.notified())
        .await
        .unwrap();
    let mut task: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(task["owner"]["instance_id"], fixture.member.id().as_str());
    assert_eq!(task["status"], "pending");
    assert_eq!(idle.load(Ordering::SeqCst), 1);
    assert!(fixture.requests.try_recv().is_err());
    // External completion removes eligibility; turn success must not make this write.
    task["status"] = json!("completed");
    std::fs::write(&path, task.to_string()).unwrap();
    release.notify_one();
    let request = fixture.request().await.to_string();
    assert!(request.contains("Task 1 is assigned to you."));
    assert!(request.contains("Reviewer specialization"));
    fixture.settled().await;
    assert_eq!(idle.load(Ordering::SeqCst), 2);
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
        task
    );
    fixture.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn teammate_permission_revalidates_exact_work_and_interrupt_revokes_claim() {
    for invalidate in ["record", "interrupt", "deny", "stop", "allow"] {
        let mut fixture = TeamFixture::with_policy(Vec::new(), TaskPolicy::Ask).await;
        fixture.request().await;
        let path = fixture.pending_task();
        fixture.release.notify_one();
        let interaction = tokio::time::timeout(TEST_TIMEOUT, async {
            loop {
                if let Some(interaction) = fixture.member.snapshot().pending_interactions.first() {
                    break interaction.clone();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let crate::core::interaction::InteractionRequest::Permission(permission) =
            interaction.request
        else {
            panic!("expected Task permission");
        };
        assert_eq!(permission.tool, "Task");
        let input: Value = serde_json::from_str(&permission.input).unwrap();
        assert_eq!(input["task_id"], "1");
        assert_eq!(input["owner"]["instance_id"], fixture.member.id().as_str());
        match invalidate {
            "record" => {
                let mut task: Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                task["status"] = json!("completed");
                std::fs::write(&path, task.to_string()).unwrap();
            }
            "interrupt" => fixture
                .manager
                .interrupt(fixture.member.id())
                .await
                .unwrap(),
            "stop" => {
                fixture.manager.stop(fixture.member.id()).await.unwrap();
            }
            _ => {}
        }
        let before = std::fs::read(&path).unwrap();
        let result = fixture
            .member
            .respond_interaction(
                interaction.request_id,
                crate::core::interaction::InteractionResponse::Permission(
                    if invalidate == "deny" {
                        crate::core::interaction::PermissionDecision::Deny
                    } else {
                        crate::core::interaction::PermissionDecision::AllowOnce
                    },
                ),
            )
            .await;
        if invalidate == "allow" {
            result.unwrap();
            fixture.request().await;
            let task: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(task["owner"]["instance_id"], fixture.member.id().as_str());
            assert_eq!(task["status"], "pending");
            fixture.manager.shutdown().await.unwrap();
            continue;
        }
        if matches!(invalidate, "interrupt" | "stop") {
            assert!(result.is_err());
        } else {
            result.unwrap();
            fixture.settled().await;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(fixture.requests.try_recv().is_err());
        fixture.manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn teammate_end_turn_feedback_is_bounded_and_never_completes_work() {
    let completed = Arc::new(AtomicUsize::new(0));
    let idle = Arc::new(AtomicUsize::new(0));
    let mut fixture = TeamFixture::new(vec![
        Hook::task_completed({
            let completed = completed.clone();
            move |event| {
                assert_eq!(
                    event.trigger,
                    crate::core::lifecycle::TaskCompletionTrigger::TeammateEndTurn
                );
                completed.fetch_add(1, Ordering::SeqCst);
                async {
                    Ok(TaskGateOutcome::Reject {
                        reason: "Finish the review.".into(),
                    })
                }
            }
        }),
        Hook::teammate_idle({
            let idle = idle.clone();
            move |_| {
                idle.fetch_add(1, Ordering::SeqCst);
                async { Ok(TeammateIdleOutcome::Allow) }
            }
        }),
    ])
    .await;
    fixture.request().await;
    let path = fixture.owned_task();
    let before = std::fs::read(&path).unwrap();
    let mut events = fixture.member.subscribe();
    fixture.release.notify_one();
    for _ in 0..MAX_TEAMMATE_CONTINUATIONS {
        let request = fixture.request().await;
        let text = request.to_string();
        assert!(text.contains("Finish the review."));
        assert!(text.contains("Reviewer specialization"));
        assert!(
            request
                .get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
    }
    let view = fixture.settled().await;
    assert!(
        view.snapshot
            .latest_error
            .unwrap()
            .message
            .contains("continuation limit")
    );
    assert_eq!(
        completed.load(Ordering::SeqCst),
        MAX_TEAMMATE_CONTINUATIONS + 1
    );
    assert_eq!(idle.load(Ordering::SeqCst), 0);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let completion = fixture
        .member
        .complete_delegated_agent(watch::channel(false).1)
        .await
        .unwrap();
    assert!(
        completion
            .outcome
            .unwrap_err()
            .message
            .contains("continuation limit")
    );
    let observed = std::iter::from_fn(|| events.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(observed.iter().filter(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, PhiEvent::TurnStarted { .. }))).count(), 0);
    assert!(!observed.iter().any(|event| matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, PhiEvent::TurnFinished { .. }))));
    assert!(fixture.requests.try_recv().is_err());
    fixture
        .member
        .submit(internal_prompt("Resume with new instructions."))
        .await
        .unwrap();
    fixture.request().await;
    for _ in 0..MAX_TEAMMATE_CONTINUATIONS {
        fixture.request().await;
    }
    assert!(
        fixture
            .settled()
            .await
            .snapshot
            .latest_error
            .unwrap()
            .message
            .contains("continuation limit")
    );
    assert_eq!(
        completed.load(Ordering::SeqCst),
        2 * (MAX_TEAMMATE_CONTINUATIONS + 1)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    fixture.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn teammate_idle_uses_one_boundary_and_managed_stop() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut fixture = TeamFixture::new(vec![Hook::teammate_idle({
        let calls = calls.clone();
        move |_| {
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(match call {
                    0 => TeammateIdleOutcome::Continue {
                        feedback: "Inspect the remaining work.".into(),
                    },
                    1 => TeammateIdleOutcome::Allow,
                    _ => TeammateIdleOutcome::Stop,
                })
            }
        }
    })])
    .await;
    fixture.request().await;
    fixture.release.notify_one();
    assert!(
        fixture
            .request()
            .await
            .to_string()
            .contains("Inspect the remaining work.")
    );
    fixture.settled().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(fixture.requests.try_recv().is_err());
    fixture
        .member
        .submit(internal_prompt("Report again."))
        .await
        .unwrap();
    fixture.request().await;
    let mut finalization = fixture.member.subscribe_finalization().unwrap();
    tokio::time::timeout(
        TEST_TIMEOUT,
        finalization.wait_for(|status| {
            matches!(
                status,
                crate::agent_manager::AgentFinalizationStatus::Stopped
            )
        }),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        fixture.member.snapshot().status,
        AgentInstanceStatus::Stopped
    );
    assert!(fixture.manager.get(fixture.member.id()).is_err());
    assert!(fixture.manager.get(fixture.lead.id()).is_ok());
    fixture.manager.shutdown().await.unwrap();
}

#[tokio::test]
async fn teammate_gate_errors_fail_closed_without_repeating_or_releasing_work() {
    #[derive(Debug, Clone, Copy)]
    enum GateFailure {
        EndTurn,
        Idle,
        OversizedFeedback,
    }
    for failure in [
        GateFailure::EndTurn,
        GateFailure::Idle,
        GateFailure::OversizedFeedback,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let mut fixture = TeamFixture::new(vec![
            Hook::task_completed({
                let calls = calls.clone();
                move |_| {
                    let calls = calls.clone();
                    async move {
                        match failure {
                            GateFailure::EndTurn => {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Err(crate::hooks::HookError::new("completion gate unavailable"))
                            }
                            GateFailure::OversizedFeedback => {
                                calls.fetch_add(1, Ordering::SeqCst);
                                Ok(TaskGateOutcome::Reject {
                                    reason: "x".repeat(16 * 1024 + 1),
                                })
                            }
                            GateFailure::Idle => Ok(TaskGateOutcome::Allow),
                        }
                    }
                }
            }),
            Hook::teammate_idle({
                let calls = calls.clone();
                move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async { Err(crate::hooks::HookError::new("idle gate unavailable")) }
                }
            }),
        ])
        .await;
        fixture.request().await;
        let path = fixture.owned_task();
        let before = std::fs::read(&path).unwrap();
        let mut events = fixture.member.subscribe();
        fixture.release.notify_one();
        let view = fixture.settled().await;
        assert_eq!(
            view.snapshot.status,
            AgentInstanceStatus::Failed,
            "{failure:?}"
        );
        assert!(view.snapshot.latest_error.is_some());
        assert!(
            std::iter::from_fn(|| events.try_recv().ok())
                .any(|event| { matches!(event, AgentInstanceEvent::Phi(envelope) if matches!(envelope.event, PhiEvent::TurnFailed { .. })) })
        );
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(fixture.requests.try_recv().is_err());
        assert_eq!(std::fs::read(path).unwrap(), before);
        fixture.manager.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn teammate_blocked_hooks_are_responsive_and_stale_feedback_is_discarded() {
    #[derive(Debug, Clone, Copy)]
    enum Invalidation {
        Interrupt,
        Replace,
        Submit,
        Stop,
    }
    #[derive(Debug, Clone, Copy)]
    enum Boundary {
        Idle,
        Preparation,
    }
    let invalidations = [
        Invalidation::Interrupt,
        Invalidation::Replace,
        Invalidation::Submit,
        Invalidation::Stop,
    ];
    for (invalidation, boundary) in invalidations.into_iter().flat_map(|invalidation| {
        [Boundary::Idle, Boundary::Preparation].map(|boundary| (invalidation, boundary))
    }) {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut fixture = TeamFixture::new(vec![
            Hook::teammate_idle({
                let entered = entered.clone();
                let release = release.clone();
                let calls = calls.clone();
                move |_| {
                    let entered = entered.clone();
                    let release = release.clone();
                    let call = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        if call == 0 {
                            if matches!(boundary, Boundary::Idle) {
                                entered.notify_one();
                                release.notified().await;
                            }
                            Ok(TeammateIdleOutcome::Continue {
                                feedback: "stale feedback".into(),
                            })
                        } else {
                            Ok(TeammateIdleOutcome::Allow)
                        }
                    }
                }
            }),
            Hook::user_prompt_submit({
                let entered = entered.clone();
                let release = release.clone();
                move |event| {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        if event.prompt == "stale feedback"
                            && matches!(boundary, Boundary::Preparation)
                        {
                            assert!(matches!(event.source, UserPromptSubmitSource::Internal));
                            entered.notify_one();
                            release.notified().await;
                        }
                        Ok(crate::core::lifecycle::UserPromptSubmitOutcome::Continue {
                            notices: Vec::new(),
                            prompt: event.prompt,
                            injected_context: Vec::new(),
                        })
                    }
                }
            }),
        ])
        .await;
        fixture.request().await;
        let path = fixture.owned_task();
        let before = std::fs::read(&path).unwrap();
        fixture.release.notify_one();
        tokio::time::timeout(TEST_TIMEOUT, entered.notified())
            .await
            .unwrap();
        let view = tokio::time::timeout(TEST_TIMEOUT, fixture.member.view())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(view.snapshot.status, AgentInstanceStatus::Running);
        assert!(matches!(
            fixture.member.reset_context(None).await,
            Err(AgentInstanceError::Busy)
        ));
        let observed = fixture.member.clone();
        let mut quiescence = Box::pin(observed.wait_for_quiescence(None));
        assert!(futures::poll!(quiescence.as_mut()).is_pending());
        match invalidation {
            Invalidation::Interrupt => fixture
                .manager
                .interrupt(fixture.member.id())
                .await
                .unwrap(),
            Invalidation::Replace => {
                let candidate = fixture
                    .phi
                    .agent_instance_builder()
                    .transcript(view.transcript)
                    .delegated_child()
                    .working_dir(fixture.root.clone())
                    .task_list(
                        TaskListBinding::new(fixture.root.join("tasks"), "idle-list").unwrap(),
                    )
                    .build()
                    .await
                    .unwrap()
                    .into_runtime_candidate();
                assert!(matches!(
                    fixture.member.replace_runtime(candidate).await,
                    Err(AgentInstanceError::Busy)
                ));
                fixture
                    .manager
                    .interrupt(fixture.member.id())
                    .await
                    .unwrap();
            }
            Invalidation::Submit => {
                fixture
                    .member
                    .dispatch(
                        internal_prompt("New authorized work."),
                        SubmissionDispatch::Steer,
                    )
                    .await
                    .unwrap();
                assert!(
                    !fixture
                        .request()
                        .await
                        .to_string()
                        .contains("stale feedback")
                );
            }
            Invalidation::Stop => {
                fixture.manager.stop(fixture.member.id()).await.unwrap();
            }
        }
        release.notify_one();
        if !matches!(invalidation, Invalidation::Stop) {
            fixture.settled().await;
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            fixture.requests.try_recv().is_err(),
            "{invalidation:?}, {boundary:?}"
        );
        let saved = std::fs::read(path).unwrap();
        if matches!(invalidation, Invalidation::Stop) {
            let task: Value = serde_json::from_slice(&saved).unwrap();
            assert_eq!(task["status"], "pending");
            assert!(task.get("owner").is_none());
        } else {
            assert_eq!(saved, before);
        }
        drop(quiescence);
        fixture.manager.shutdown().await.unwrap();
    }
}
