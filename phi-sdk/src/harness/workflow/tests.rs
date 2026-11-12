use super::*;
use crate::factory::Phi;
use crate::harness::instance::AgentInstanceError;
use crate::harness::manager::preparation::{
    AgentCreation, AgentResultRouting, PreparedSubagent, SubagentAssociation, SubagentPreparation,
};
use crate::harness::tests::support::{
    RegisteredInstance, completion, instance_for_provider, read_request,
};
use futures::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch};

async fn workflow_provider<F, Reply>(
    listener: TcpListener,
    requests: mpsc::UnboundedSender<Value>,
    respond: F,
) where
    F: Fn(Value) -> Reply + Send + Sync + 'static,
    Reply: std::future::Future<Output = Value> + Send + 'static,
{
    use tokio::io::AsyncWriteExt;
    let respond = Arc::new(respond);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (mut stream, _) = accepted.unwrap();
                let requests = requests.clone();
                let respond = respond.clone();
                connections.spawn(async move {
                    let request = read_request(&mut stream).await;
                    requests.send(request.clone()).unwrap();
                    let chunk = respond(request).await;
                    let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                    let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => { result.unwrap(); }
        }
    }
}

struct WorkflowFixture {
    root: std::path::PathBuf,
    registered: RegisteredInstance,
    requests: mpsc::UnboundedReceiver<Value>,
    server: tokio::task::JoinHandle<()>,
    preparation: Option<Arc<dyn SubagentPreparation>>,
    resources: Option<crate::factory::AgentRuntimeResources>,
}

impl WorkflowFixture {
    async fn new() -> Self {
        Self::with_reply(|request| std::future::ready(completion(&request))).await
    }

    async fn with_reply<F, Reply>(respond: F) -> Self
    where
        F: Fn(Value) -> Reply + Send + Sync + 'static,
        Reply: std::future::Future<Output = Value> + Send + 'static,
    {
        let root =
            std::path::PathBuf::from("/tmp").join(format!("phi-workflow-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (requests, observed) = mpsc::unbounded_channel();
        let server = tokio::spawn(workflow_provider(listener, requests, respond));
        let registered = instance_for_provider(&root, address, false).await;
        let mut fixture = Self {
            root,
            registered,
            requests: observed,
            server,
            preparation: None,
            resources: None,
        };
        let phi = fixture.registered.phi.clone().with_runtime_services(
            fixture
                .registered
                .phi
                .runtime_services
                .clone()
                .permission_policy(
                    crate::permission::PermissionConfig {
                        mode: crate::permission::PermissionMode::Allow,
                        ..Default::default()
                    },
                    Some(fixture.root.clone()),
                ),
        );
        fixture.replace(phi).await.unwrap();
        fixture
    }

    async fn replace(&mut self, phi: Phi) -> Result<(), AgentInstanceError> {
        let mut builder = phi
            .agent_instance_builder()
            .working_dir(self.root.clone())
            .task_list(self.registered.task_list().unwrap().clone())
            .transcript(self.registered.transcript().await.unwrap());
        if let Some(preparation) = &self.preparation {
            builder = builder.subagent_preparation(preparation.clone());
        }
        let candidate = builder.build().await.unwrap();
        let resources = candidate.resources();
        self.registered
            .replace_runtime(candidate.into_runtime_candidate())
            .await?;
        self.resources = Some(resources);
        self.registered.phi = phi;
        Ok(())
    }
}

impl Drop for WorkflowFixture {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn limits() -> WorkflowLimits {
    WorkflowLimits::new(2, 4, 8, 4096, 4096).unwrap()
}

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(30), future)
        .await
        .expect("Workflow Agent operation did not settle")
}

#[tokio::test]
async fn workflow_workers_keep_results_private_and_release_membership_under_forced_background() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let mut phi = fixture.registered.phi.clone();
        phi.cfg.agent_background = true;
        fixture.replace(phi).await.unwrap();
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let run = WorkflowRunId::new();
        let initial_entries = fixture.registered.transcript().await.unwrap().entries.len();
        for index in 0..8 {
            let invocation = scope
                .resolve_invocation(
                    InvocationId(index),
                    format!("inspect {index}"),
                    serde_json::from_value(json!({"label": "presentation-only"})).unwrap(),
                    Some("review".into()),
                )
                .unwrap();
            let mut admission = scope.reserve_agent(run, &invocation).await.unwrap();
            assert_eq!(
                admission.creation().result_routing(),
                AgentResultRouting::Workflow(run)
            );
            assert!(admission.creation().alias().is_none());
            scope
                .admit_agent(&mut admission, watch::channel(false).1)
                .await
                .unwrap();
            let id = admission.creation().instance_id().clone();
            assert_eq!(admission.await_completion().await.unwrap(), "child-result");
            let worker = fixture.registered.manager.get(&id).unwrap();
            admission.finalize_worker().await.unwrap();
            assert!(worker.finalization().unwrap().cleanup_error.is_none());
            assert!(fixture.registered.manager.get(&id).is_err());
            assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
            assert!(
                fixture
                    .registered
                    .subagent_snapshots()
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(fixture.requests.len(), 8);
        assert_eq!(
            fixture.registered.transcript().await.unwrap().entries.len(),
            initial_entries
        );
        drop(scope);
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

async fn wait_snapshot(
    run: &super::runtime::WorkflowRun,
    matches: impl Fn(&super::runtime::WorkflowSnapshot) -> bool,
) -> super::runtime::WorkflowSnapshot {
    let mut updates = run.subscribe();
    loop {
        let snapshot = updates.snapshot();
        if matches(&snapshot) {
            return snapshot;
        }
        updates.changed().await.unwrap();
    }
}

struct GatedAssociation {
    entered: tokio::sync::Semaphore,
    cleanup: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
    starts: AtomicUsize,
}

impl GatedAssociation {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Semaphore::new(0),
            cleanup: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            starts: AtomicUsize::new(0),
        })
    }
}

impl SubagentAssociation for GatedAssociation {
    fn associate(
        &self,
        _: crate::agent_instance::AgentInstance,
        _: AgentCreation,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async {
            self.starts.fetch_add(1, Ordering::SeqCst);
            self.entered.add_permits(1);
            std::future::pending().await
        })
    }

    fn rollback(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async {
            self.cleanup.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            Ok(())
        })
    }
}

impl WorkflowFixture {
    async fn install_association(&mut self, association: Arc<dyn SubagentAssociation>) {
        self.preparation = Some(Arc::new(AssociatedPreparation {
            phi: self.registered.phi.clone(),
            association,
        }));
        let phi = self.registered.phi.clone();
        self.replace(phi).await.unwrap();
    }
}

#[tokio::test]
async fn supervisor_preserves_total_limit_and_releases_generation_after_completion() {
    bounded(async {
        let fixture = WorkflowFixture::new().await;
        let scope = fixture
            .registered
            .capture_workflow_scope(
                Some("leaf".into()),
                WorkflowLimits::new(1, 1, 1, 4096, 4096).unwrap(),
            )
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        assert_eq!(
            host.request_agent("inspect".into(), WorkflowAgentOptions::default(), None)
                .await
                .unwrap(),
            "child-result"
        );
        assert!(matches!(
            host.request_agent("again".into(), WorkflowAgentOptions::default(), None)
                .await
                .unwrap_err()
                .as_ref(),
            WorkflowError::InvocationLimit
        ));
        finished
            .send(super::runtime::ScriptOutcome::Returned(Some(json!(42))))
            .unwrap();
        let snapshot = wait_snapshot(&run, |snapshot| {
            snapshot.status() == WorkflowStatus::Completed
        })
        .await;
        assert_eq!(snapshot.completed(), 1);
        assert_eq!(snapshot.active(), 0);
        assert_eq!(snapshot.value(), Some(&json!(42)));
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        fixture.registered.reset_context(None).await.unwrap();
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn script_completion_keeps_unobserved_preparation_owned_until_compensation_finishes() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let gate = GatedAssociation::new();
        fixture.install_association(gate.clone()).await;
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        let waiter = tokio::spawn(async move {
            host.request_agent("inspect".into(), WorkflowAgentOptions::default(), None)
                .await
        });
        gate.entered.acquire().await.unwrap().forget();
        waiter.abort();
        finished
            .send(super::runtime::ScriptOutcome::Returned(Some(json!(
                "report"
            ))))
            .unwrap();
        gate.cleanup.acquire().await.unwrap().forget();
        let snapshot = wait_snapshot(&run, |snapshot| {
            snapshot.status() == WorkflowStatus::Finishing
        })
        .await;
        assert_eq!(snapshot.active(), 1);
        assert!(snapshot.value().is_none());
        assert!(matches!(
            fixture.registered.reset_context(None).await,
            Err(AgentInstanceError::Busy)
        ));
        gate.release.add_permits(1);
        let snapshot = wait_snapshot(&run, |snapshot| {
            snapshot.status() == WorkflowStatus::Completed
        })
        .await;
        assert_eq!(snapshot.value(), Some(&json!("report")));
        assert_eq!(snapshot.active(), 0);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn stop_closes_queue_and_waits_for_startup_compensation_without_admitting_more() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let gate = GatedAssociation::new();
        fixture.install_association(gate.clone()).await;
        let scope = fixture
            .registered
            .capture_workflow_scope(
                Some("leaf".into()),
                WorkflowLimits::new(1, 1, 2, 4096, 4096).unwrap(),
            )
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        let cancellation = host.cancellation();
        let first = tokio::spawn({
            let host = host.clone();
            async move {
                host.request_agent("first".into(), WorkflowAgentOptions::default(), None)
                    .await
            }
        });
        gate.entered.acquire().await.unwrap().forget();
        let second = tokio::spawn(async move {
            host.request_agent("second".into(), WorkflowAgentOptions::default(), None)
                .await
        });
        wait_snapshot(&run, |snapshot| snapshot.pending() == 1).await;
        let stopping = tokio::spawn({
            let run = run.clone();
            async move { run.stop().await }
        });
        cancellation.cancelled().await;
        finished
            .send(super::runtime::ScriptOutcome::Cancelled)
            .unwrap();
        gate.cleanup.acquire().await.unwrap().forget();
        assert!(!stopping.is_finished());
        assert_eq!(run.snapshot().status(), WorkflowStatus::Stopping);
        assert!(matches!(
            second.await.unwrap().unwrap_err().as_ref(),
            WorkflowError::Stopped
        ));
        gate.release.add_permits(1);
        assert_eq!(
            stopping.await.unwrap().unwrap().status(),
            WorkflowStatus::Stopped
        );
        assert!(first.await.unwrap().is_err());
        assert_eq!(gate.starts.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        assert!(fixture.requests.is_empty());
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn concurrency_slots_include_preparation_and_cleanup() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let gate = GatedAssociation::new();
        fixture.install_association(gate.clone()).await;
        let scope = fixture
            .registered
            .capture_workflow_scope(
                Some("leaf".into()),
                WorkflowLimits::new(2, 1, 4, 4096, 4096).unwrap(),
            )
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        let first = tokio::spawn({
            let host = host.clone();
            async move {
                host.request_agent("first".into(), WorkflowAgentOptions::default(), None)
                    .await
            }
        });
        let second = tokio::spawn({
            let host = host.clone();
            async move {
                host.request_agent("second".into(), WorkflowAgentOptions::default(), None)
                    .await
            }
        });
        gate.entered.acquire_many(2).await.unwrap().forget();
        let third = tokio::spawn({
            let host = host.clone();
            async move {
                host.request_agent("third".into(), WorkflowAgentOptions::default(), None)
                    .await
            }
        });
        let snapshot = wait_snapshot(&run, |snapshot| snapshot.pending() == 1).await;
        assert_eq!(snapshot.active(), 2);
        assert!(matches!(
            host.request_agent("overflow".into(), WorkflowAgentOptions::default(), None)
                .await
                .unwrap_err()
                .as_ref(),
            WorkflowError::PendingLimit
        ));
        finished
            .send(super::runtime::ScriptOutcome::Returned(None))
            .unwrap();
        gate.cleanup.acquire_many(2).await.unwrap().forget();
        assert_eq!(run.snapshot().active(), 2);
        assert_eq!(run.snapshot().status(), WorkflowStatus::Finishing);
        gate.release.add_permits(2);
        let snapshot = wait_snapshot(&run, |snapshot| {
            snapshot.status() == WorkflowStatus::Completed
        })
        .await;
        assert_eq!(snapshot.active(), 0);
        assert_eq!(snapshot.completed(), 3);
        assert!(first.await.unwrap().is_err());
        assert!(second.await.unwrap().is_err());
        assert!(third.await.unwrap().is_err());
        assert_eq!(gate.starts.load(Ordering::SeqCst), 2);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn creator_closure_stops_an_accepted_worker_and_the_owned_run() {
    bounded(async {
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let fixture = WorkflowFixture::with_reply({
            let entered = entered.clone();
            let release = release.clone();
            move |request| {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    completion(&request)
                }
            }
        })
        .await;
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        let cancellation = host.cancellation();
        let result = tokio::spawn(async move {
            host.request_agent("inspect".into(), WorkflowAgentOptions::default(), None)
                .await
        });
        entered.acquire().await.unwrap().forget();
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 2);
        let script = tokio::spawn(async move {
            cancellation.cancelled().await;
            let _ = finished.send(super::runtime::ScriptOutcome::Cancelled);
        });
        fixture.registered.manager.shutdown().await.unwrap();
        let snapshot = wait_snapshot(&run, |snapshot| {
            snapshot.status() == WorkflowStatus::Stopped
        })
        .await;
        assert_eq!(snapshot.active(), 0);
        assert_eq!(snapshot.pending(), 0);
        assert!(result.await.unwrap().is_err());
        script.await.unwrap();
        runtime.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn run_does_not_inherit_launch_turn_preapprovals() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let phi = fixture.registered.phi.clone().with_runtime_services(
            fixture
                .registered
                .phi
                .runtime_services
                .clone()
                .permission_policy(
                    crate::permission::PermissionConfig {
                        mode: crate::permission::PermissionMode::Ask,
                        ..Default::default()
                    },
                    Some(fixture.root.clone()),
                ),
        );
        fixture.replace(phi).await.unwrap();
        let resources = fixture.resources.as_ref().unwrap().clone();
        let permission = resources.permission();
        let policy = permission.outer_turn_policy(crate::core::turn::TurnToolPolicy {
            allowances: vec![("Agent".into(), "**".into())],
            ..Default::default()
        });
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let run = policy
            .run(async {
                permission
                    .authorize_tool("Agent", "launch-turn-only", None, None)
                    .await
                    .unwrap();
                let (run, host, finished) = runtime
                    .start_run(scope, host_request().await, WorkflowRunId::new())
                    .unwrap();
                let error = host
                    .request_agent("inspect".into(), WorkflowAgentOptions::default(), None)
                    .await
                    .unwrap_err();
                assert!(matches!(error.as_ref(), WorkflowError::Permission(_)));
                finished
                    .send(super::runtime::ScriptOutcome::Failed(error))
                    .unwrap();
                run
            })
            .await;
        wait_snapshot(&run, |snapshot| snapshot.status() == WorkflowStatus::Failed).await;
        assert!(fixture.requests.is_empty());
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn result_budget_covers_workers_and_final_script_value() {
    bounded(async {
        for budget in [8, 20] {
            let fixture = WorkflowFixture::new().await;
            let scope = fixture
                .registered
                .capture_workflow_scope(
                    Some("leaf".into()),
                    WorkflowLimits::new(1, 1, 1, budget, 4096).unwrap(),
                )
                .await
                .unwrap();
            let runtime = WorkflowRuntime::default();
            let (run, host, finished) = runtime
                .start_run(scope, host_request().await, WorkflowRunId::new())
                .unwrap();
            let result = host
                .request_agent("inspect".into(), WorkflowAgentOptions::default(), None)
                .await;
            if budget == 8 {
                assert!(matches!(
                    result.unwrap_err().as_ref(),
                    WorkflowError::ResultLimit
                ));
            } else {
                assert_eq!(result.unwrap(), "child-result");
            }
            finished
                .send(super::runtime::ScriptOutcome::Returned(Some(json!(
                    "abcdefgh"
                ))))
                .unwrap();
            let snapshot =
                wait_snapshot(&run, |snapshot| snapshot.status() == WorkflowStatus::Failed).await;
            assert!(matches!(snapshot.error(), Some(WorkflowError::ResultLimit)));
            assert!(snapshot.value().is_none());
            assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
            runtime.shutdown().await.unwrap();
            fixture.registered.manager.shutdown().await.unwrap();
        }
    })
    .await;
}

#[tokio::test]
async fn lost_script_completion_is_failure_not_success() {
    bounded(async {
        let fixture = WorkflowFixture::new().await;
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        drop(host);
        drop(finished);
        let snapshot =
            wait_snapshot(&run, |snapshot| snapshot.status() == WorkflowStatus::Failed).await;
        assert!(matches!(
            snapshot.error(),
            Some(WorkflowError::Coordination)
        ));
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn supervisor_retains_failed_cleanup_for_registration_scoped_retry() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let association = Arc::new(RejectAssociation {
            rollback_attempts: AtomicUsize::new(0),
        });
        fixture.install_association(association.clone()).await;
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let registration = scope.registration();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        let failure = host
            .request_agent("inspect".into(), WorkflowAgentOptions::default(), None)
            .await
            .unwrap_err();
        finished
            .send(super::runtime::ScriptOutcome::Failed(failure))
            .unwrap();
        let snapshot = wait_snapshot(&run, |snapshot| {
            snapshot.status() == WorkflowStatus::Finishing
        })
        .await;
        assert_eq!(snapshot.active(), 1);
        assert_eq!(
            snapshot.invocations()[0].status(),
            WorkflowInvocationStatus::Finalizing
        );
        assert!(snapshot.invocations()[0].worker().is_some());
        assert!(matches!(
            snapshot.invocations()[0].error(),
            Some(WorkflowError::Admission(_))
        ));
        assert_eq!(association.rollback_attempts.load(Ordering::SeqCst), 1);
        runtime.stop_owned(registration).await.unwrap();
        assert_eq!(run.snapshot().status(), WorkflowStatus::Stopped);
        assert_eq!(run.snapshot().active(), 0);
        assert_eq!(
            run.snapshot().invocations()[0].status(),
            WorkflowInvocationStatus::Failed
        );
        assert_eq!(
            run.snapshot().invocations()[0].worker(),
            snapshot.invocations()[0].worker()
        );
        assert!(!run.snapshot().cleanup_errors().is_empty());
        assert!(run.snapshot().error().is_some());
        assert_eq!(association.rollback_attempts.load(Ordering::SeqCst), 2);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

struct PanickingAssociation {
    rollbacks: AtomicUsize,
}

impl SubagentAssociation for PanickingAssociation {
    fn associate(
        &self,
        _: crate::agent_instance::AgentInstance,
        _: AgentCreation,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { panic!("startup association panicked") })
    }

    fn rollback(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async {
            self.rollbacks.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}

#[tokio::test]
async fn startup_panic_does_not_abandon_registered_worker_or_association() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let association = Arc::new(PanickingAssociation {
            rollbacks: AtomicUsize::new(0),
        });
        fixture.install_association(association.clone()).await;
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let runtime = WorkflowRuntime::default();
        let (run, host, finished) = runtime
            .start_run(scope, host_request().await, WorkflowRunId::new())
            .unwrap();
        let error = host
            .request_agent("inspect".into(), WorkflowAgentOptions::default(), None)
            .await
            .unwrap_err();
        assert!(matches!(error.as_ref(), WorkflowError::Coordination));
        finished
            .send(super::runtime::ScriptOutcome::Failed(error))
            .unwrap();
        wait_snapshot(&run, |snapshot| snapshot.status() == WorkflowStatus::Failed).await;
        assert_eq!(association.rollbacks.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

struct RejectAssociation {
    rollback_attempts: AtomicUsize,
}

impl SubagentAssociation for RejectAssociation {
    fn associate(
        &self,
        _: crate::agent_instance::AgentInstance,
        _: AgentCreation,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Err("association rejected".into()) })
    }

    fn rollback(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async {
            if self.rollback_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("rollback temporarily unavailable".into())
            } else {
                Ok(())
            }
        })
    }
}

struct AssociatedPreparation {
    phi: Phi,
    association: Arc<dyn SubagentAssociation>,
}

impl SubagentPreparation for AssociatedPreparation {
    fn prepare(
        &self,
        creation: AgentCreation,
        _: watch::Receiver<bool>,
    ) -> BoxFuture<'static, Result<PreparedSubagent, String>> {
        let phi = self.phi.clone();
        let association = self.association.clone();
        Box::pin(async move {
            let candidate =
                crate::harness::manager::preparation::prepare_sdk_child(&phi, &creation)
                    .await
                    .map_err(|error| error.to_string())?;
            Ok(PreparedSubagent::new(candidate, association))
        })
    }
}

#[tokio::test]
async fn failed_startup_retains_compensation_until_explicit_retry() {
    bounded(async {
        let fixture = WorkflowFixture::new().await;
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let invocation = scope
            .resolve_invocation(
                InvocationId(0),
                "inspect".into(),
                WorkflowAgentOptions::default(),
                None,
            )
            .unwrap();
        let mut admission = scope
            .reserve_agent(WorkflowRunId::new(), &invocation)
            .await
            .unwrap();
        let association = Arc::new(RejectAssociation {
            rollback_attempts: AtomicUsize::new(0),
        });
        let preparation = Arc::new(AssociatedPreparation {
            phi: fixture.registered.phi.clone(),
            association: association.clone(),
        });
        assert!(matches!(
            admission
                .admit_agent(
                    &fixture.registered.phi,
                    Some(preparation),
                    watch::channel(false).1
                )
                .await,
            Err(WorkflowError::Admission(
                crate::agent_manager::AgentManagerError::Compensation { .. }
            ))
        ));
        assert_eq!(association.rollback_attempts.load(Ordering::SeqCst), 1);
        admission.finalize_worker().await.unwrap();
        assert_eq!(association.rollback_attempts.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        assert!(fixture.requests.is_empty());
        drop(scope);
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn permission_and_owner_closure_reject_before_worker_creation() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let phi = fixture.registered.phi.clone().with_runtime_services(
            fixture
                .registered
                .phi
                .runtime_services
                .clone()
                .permission_policy(
                    crate::permission::PermissionConfig {
                        mode: crate::permission::PermissionMode::Deny,
                        ..Default::default()
                    },
                    Some(fixture.root.clone()),
                ),
        );
        fixture.replace(phi).await.unwrap();
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let invocation = scope
            .resolve_invocation(
                InvocationId(0),
                "inspect".into(),
                WorkflowAgentOptions::default(),
                None,
            )
            .unwrap();
        assert!(matches!(
            scope.reserve_agent(WorkflowRunId::new(), &invocation).await,
            Err(WorkflowError::Permission(_))
        ));
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        drop(scope);
        let phi = fixture.registered.phi.clone().with_runtime_services(
            fixture
                .registered
                .phi
                .runtime_services
                .clone()
                .permission_policy(
                    crate::permission::PermissionConfig {
                        mode: crate::permission::PermissionMode::Allow,
                        ..Default::default()
                    },
                    Some(fixture.root.clone()),
                ),
        );
        fixture.replace(phi).await.unwrap();
        let scope = fixture
            .registered
            .capture_workflow_scope(Some("leaf".into()), limits())
            .await
            .unwrap();
        let mut closing = scope.subscribe_closure();
        fixture.registered.manager.shutdown().await.unwrap();
        closing.changed().await.unwrap();
        assert!(matches!(
            scope.reserve_agent(WorkflowRunId::new(), &invocation).await,
            Err(WorkflowError::Admission(_))
        ));
        assert!(!matches!(
            *closing.borrow(),
            crate::agent_manager::AgentFinalizationStatus::Open
        ));
        assert!(
            fixture
                .registered
                .capture_workflow_scope(None, limits())
                .await
                .is_err()
        );
        assert!(fixture.requests.is_empty());
    })
    .await;
}

async fn host_request() -> WorkflowRequest {
    WorkflowRequest::parse(
        "export const meta = { name: 'host', description: 'supervisor host boundary' };",
        None,
    )
    .await
    .unwrap()
}

async fn launch_script(
    fixture: &WorkflowFixture,
    runtime: &WorkflowRuntime,
    body: &str,
    args: Option<Value>,
) -> super::runtime::WorkflowRun {
    let source = format!(
        "export const meta = {{ name: 'test', description: 'exercise workflow' }};\n{body}"
    );
    let request = WorkflowRequest::parse(&source, args).await.unwrap();
    runtime
        .launch(request, &fixture.registered, Some("leaf".into()), limits())
        .await
        .unwrap()
}

#[tokio::test]
async fn public_workflow_boundary_retains_control_after_handle_loss() {
    use crate::workflow::{WorkflowRequest, WorkflowRuntime, WorkflowStatus};

    bounded(async {
        let mut fixture = WorkflowFixture::with_reply(|request| async move {
            if request["messages"].to_string().contains("held-workflow-worker") {
                std::future::pending::<()>().await;
            }
            completion(&request)
        })
        .await;
        let runtime = WorkflowRuntime::default();
        let source = "// captured source\nexport const meta = { name: 'boundary', description: 'SDK boundary', phases: ['waiting'] };\nphase('waiting'); return await agent(args.prompt);";
        let arguments = json!({"prompt": "held-workflow-worker", "__proto__": {"retained": true}});
        let request = WorkflowRequest::parse(source, Some(arguments.clone()))
            .await
            .unwrap();
        assert!(fixture.requests.is_empty());
        let rejected = runtime
            .launch(request.clone(), &fixture.registered, Some("absent".into()), limits())
            .await;
        assert!(matches!(rejected, Err(error) if matches!(error.as_ref(), WorkflowError::UnknownRole(_))));
        assert!(fixture.requests.is_empty());
        let registration = fixture.registered.registration_id().unwrap();
        assert!(runtime.runs_owned(registration).unwrap().is_empty());
        let run = runtime
            .launch(request.clone(), &fixture.registered, Some("leaf".into()), limits())
            .await
            .unwrap();
        let id = run.id();
        fixture.requests.recv().await.unwrap();
        let snapshot = run.snapshot();
        assert_eq!(snapshot.id(), id);
        assert_eq!(snapshot.creator(), fixture.registered.id());
        assert_eq!(snapshot.registration(), registration);
        drop(run);
        assert!(matches!(
            fixture.registered.reset_context(None).await,
            Err(AgentInstanceError::Busy)
        ));
        let phi = fixture.registered.phi.clone();
        assert!(matches!(
            fixture.replace(phi).await,
            Err(AgentInstanceError::Busy)
        ));
        let mut events = fixture.registered.subscribe();
        fixture
            .registered
            .submit(crate::core::turn::TurnPrompt {
                prompt: "ordinary master turn".into(),
                source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        assert!(fixture.requests.recv().await.is_some());
        loop {
            if matches!(events.recv().await.unwrap(), crate::agent_instance::AgentInstanceEvent::Phi(envelope)
                if matches!(envelope.event, crate::PhiEvent::TurnFinished { .. })) {
                break;
            }
        }
        let mut runs = runtime.runs_owned(registration).unwrap();
        assert_eq!(runs.len(), 1);
        let retained = runs.pop().unwrap();
        assert_eq!(retained.id(), id);
        assert_eq!(retained.request().source(), source);
        assert_eq!(retained.request().args(), Some(&arguments));
        assert_eq!(retained.snapshot().status(), WorkflowStatus::Running);
        runtime.shutdown().await.unwrap();
        let settled = retained.snapshot();
        assert_eq!(settled.status(), WorkflowStatus::Stopped);
        assert_eq!(settled.active(), 0);
        assert_eq!(settled.pending(), 0);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        let rejected = runtime
            .launch(request, &fixture.registered, Some("leaf".into()), limits())
            .await;
        assert!(matches!(rejected, Err(error) if matches!(error.as_ref(), WorkflowError::Stopped)));
        fixture.registered.wait_for_quiescence(None).await.unwrap();
        fixture.registered.reset_context(None).await.unwrap();
        let phi = fixture.registered.phi.clone();
        fixture.replace(phi).await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn staged_launch_installs_ownership_before_effects() {
    bounded(async {
        let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fixture = WorkflowFixture::with_reply({
            let installed = installed.clone();
            move |request| {
                assert!(installed.load(std::sync::atomic::Ordering::SeqCst));
                async move { completion(&request) }
            }
        }).await;
        let runtime = WorkflowRuntime::default();
        let registration = fixture.registered.registration_id().unwrap();
        let request = WorkflowRequest::parse("export const meta = {name: 'staged', description: 'pre-effect ownership'}; return await agent('inspect');", Some(json!(null))).await.unwrap();
        let abandoned = runtime.prepare(request.clone(), &fixture.registered, Some("leaf".into()), limits()).await.unwrap();
        assert!(runtime.runs_owned(registration).unwrap().is_empty());
        assert!(fixture.requests.is_empty());
        drop(abandoned);
        fixture.registered.reset_context(None).await.unwrap();
        let launch = runtime.prepare(request.clone(), &fixture.registered, Some("leaf".into()), limits()).await.unwrap();
        let id = launch.id();
        assert_eq!(launch.request().args(), Some(&json!(null)));
        let run = launch.start(&runtime, |run| {
            assert_eq!(run.id(), id);
            assert_eq!(run.request().source(), request.source());
            assert!(fixture.requests.is_empty());
            installed.store(true, std::sync::atomic::Ordering::SeqCst);
        }).unwrap();
        wait_snapshot(&run, is_settled).await;
        assert_eq!(run.snapshot().status(), WorkflowStatus::Completed);
        let launch = runtime.prepare(request.clone(), &fixture.registered, Some("leaf".into()), limits()).await.unwrap();
        runtime.shutdown().await.unwrap();
        assert!(matches!(launch.start(&runtime, |_| panic!("closed runtime installed an owner")), Err(error) if matches!(error.as_ref(), WorkflowError::Stopped)));
        fixture.registered.reset_context(None).await.unwrap();
        let open_runtime = WorkflowRuntime::default();
        let launch = open_runtime.prepare(request, &fixture.registered, Some("leaf".into()), limits()).await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
        assert!(matches!(launch.start(&open_runtime, |_| panic!("closed creator installed an owner")), Err(error) if matches!(error.as_ref(), WorkflowError::Stopped)));
    }).await;
}

#[tokio::test]
async fn terminal_feedback_is_correlated_and_once_only() {
    bounded(async {
        for body in [
            "log('private-progress'); return {terminal: 'only-report'};",
            "throw new Error('script failed');",
            "while (true) {}",
        ] {
            let mut fixture = WorkflowFixture::new().await;
            let wrong_creator = WorkflowFixture::new().await;
            let runtime = WorkflowRuntime::default();
            let run = launch_script(&fixture, &runtime, body, Some(json!({"private_args": true}))).await;
            if body.contains("while") {
                assert!(matches!(run.deliver_to(&fixture.registered).await, Err(error) if matches!(error.as_ref(), WorkflowError::InvalidRequest(_))));
                run.stop().await.unwrap();
            } else {
                wait_snapshot(&run, is_settled).await;
            }
            assert!(run.deliver_to(&wrong_creator.registered).await.is_err());
            assert!(wrong_creator.requests.is_empty());
            let mut abandoned = Box::pin(run.deliver_to(&fixture.registered));
            std::future::poll_fn(|context| {
                assert!(std::future::Future::poll(abandoned.as_mut(), context).is_pending());
                std::task::Poll::Ready(())
            }).await;
            drop(abandoned);
            for result in futures::future::join_all((0..8).map(|_| run.deliver_to(&fixture.registered))).await {
                result.unwrap();
            }
            let view = fixture.registered.wait_for_quiescence(None).await.unwrap();
            let markers = view.transcript.entries.iter().filter(|entry| matches!(&entry.kind, crate::transcript::TranscriptEntryKind::Event {name, data} if name == "phi_sdk.workflow_feedback" && data["run_id"] == json!(run.id()))).count();
            assert_eq!(markers, 1);
            let history = serde_json::to_string(&view.transcript).unwrap();
            assert_eq!(history.matches("Workflow terminal report").count(), 1);
            assert!(!history.contains("private-progress"));
            assert!(!history.contains("private_args"));
            let mut requests = Vec::new();
            while let Ok(request) = fixture.requests.try_recv() { requests.push(request); }
            assert_eq!(requests.len(), usize::from(run.snapshot().status() != WorkflowStatus::Stopped));
            for request in requests {
                assert!(request.to_string().contains("not human input or approval"));
                assert!(!request.to_string().contains("private-progress"));
            }
            fixture.registered.stop().await.unwrap();
            run.deliver_to(&fixture.registered).await.unwrap();
            assert_eq!(serde_json::to_string(&fixture.registered.view().await.unwrap().transcript).unwrap().matches("Workflow terminal report").count(), 1);
            runtime.shutdown().await.unwrap();
            wrong_creator.registered.manager.shutdown().await.unwrap();
        }
    }).await;
}

#[tokio::test]
async fn busy_creator_flushes_admitted_feedback_on_stop() {
    bounded(async {
        let mut fixture =
            WorkflowFixture::with_reply(|_| async { std::future::pending::<Value>().await }).await;
        fixture
            .registered
            .submit(crate::core::turn::TurnPrompt {
                prompt: "hold creator".into(),
                source: crate::core::lifecycle::UserPromptSubmitSource::Sdk,
                injected_context: Vec::new(),
                images: Vec::new(),
                tool_policy: Default::default(),
            })
            .await
            .unwrap();
        fixture.requests.recv().await.unwrap();
        let runtime = WorkflowRuntime::default();
        let run = launch_script(&fixture, &runtime, "return 'terminal only';", None).await;
        wait_snapshot(&run, is_settled).await;
        run.deliver_to(&fixture.registered).await.unwrap();
        run.deliver_to(&fixture.registered).await.unwrap();
        assert!(
            !serde_json::to_string(&fixture.registered.transcript().await.unwrap())
                .unwrap()
                .contains("Workflow terminal report")
        );
        let final_history = fixture.registered.stop().await.unwrap();
        let history = serde_json::to_string(&final_history).unwrap();
        assert_eq!(history.matches("Workflow terminal report").count(), 1);
        assert_eq!(history.matches("phi_sdk.workflow_feedback").count(), 1);
        assert!(fixture.requests.is_empty());
        run.deliver_to(&fixture.registered).await.unwrap();
        runtime.shutdown().await.unwrap();
    })
    .await;
}

fn is_settled(snapshot: &super::runtime::WorkflowSnapshot) -> bool {
    matches!(
        snapshot.status(),
        WorkflowStatus::Completed | WorkflowStatus::Failed | WorkflowStatus::Stopped
    )
}

#[tokio::test]
async fn script_pipeline_streams_in_order_and_keeps_progress_private() {
    bounded(async {
        let slow_release = Arc::new(tokio::sync::Semaphore::new(0));
        let fast_verified = Arc::new(tokio::sync::Semaphore::new(0));
        let mut fixture = WorkflowFixture::with_reply({
            let slow_release = slow_release.clone();
            let fast_verified = fast_verified.clone();
            move |request| {
                let slow_release = slow_release.clone();
                let fast_verified = fast_verified.clone();
                async move {
                    let messages = request["messages"].to_string();
                    let output = if messages.contains("slow-first") {
                        slow_release.acquire().await.unwrap().forget();
                        "slow"
                    } else if messages.contains("fast-first") {
                        "fast"
                    } else if messages.contains("fast-second") {
                        fast_verified.add_permits(1);
                        "fast-checked"
                    } else {
                        "slow-checked"
                    };
                    let mut response = completion(&request);
                    response["choices"][0]["delta"]["content"] = json!(output);
                    response
                }
            }
        })
        .await;
        let mut phi = fixture.registered.phi.clone();
        phi.cfg.agent_background = true;
        fixture.replace(phi).await.unwrap();
        let initial_entries = fixture.registered.transcript().await.unwrap().entries.len();
        let runtime = WorkflowRuntime::default();
        let run = launch_script(
            &fixture,
            &runtime,
            r#"
            phase('scan'); log({ input: args });
            const results = await pipeline(['slow', 'fast'],
                item => agent(item + '-first', {label: item}),
                value => agent(value + '-second'));
            phase('done'); return results;
        "#,
            Some(json!({"batch": 2})),
        )
        .await;
        fast_verified.acquire().await.unwrap().forget();
        let running = wait_snapshot(&run, |snapshot| {
            snapshot.invocations().iter().any(|invocation| {
                invocation.prompt() == "slow-first"
                    && invocation.status() == super::WorkflowInvocationStatus::Running
            })
        })
        .await;
        assert_eq!(running.phase(), Some("scan"));
        let slow = &running.invocations()[0];
        assert_eq!(slow.label(), Some("slow"));
        assert!(slow.worker().unwrap().registration().is_some());
        assert!(slow.value().is_none());
        slow_release.add_permits(1);
        let snapshot = wait_snapshot(&run, is_settled).await;
        assert_eq!(
            snapshot.status(),
            WorkflowStatus::Completed,
            "{:?}",
            snapshot.error()
        );
        assert_eq!(
            snapshot.value(),
            Some(&json!(["slow-checked", "fast-checked"]))
        );
        assert_eq!(snapshot.phase(), Some("done"));
        assert_eq!(snapshot.logs(), &[r#"{"input":{"batch":2}}"#]);
        assert_eq!(snapshot.completed(), 4);
        assert_eq!(
            snapshot
                .invocations()
                .iter()
                .map(|invocation| invocation.prompt())
                .collect::<Vec<_>>(),
            ["slow-first", "fast-first", "fast-second", "slow-second"]
        );
        for (index, invocation) in snapshot.invocations().iter().enumerate() {
            assert_eq!(invocation.id(), index);
            assert_eq!(invocation.phase(), Some("scan"));
            assert_eq!(invocation.role(), "leaf");
            assert_eq!(
                invocation.status(),
                super::WorkflowInvocationStatus::Succeeded
            );
            assert!(invocation.value().is_some());
            assert!(invocation.error().is_none());
            let worker = invocation.worker().unwrap();
            assert!(worker.registration().is_some());
            assert!(
                fixture
                    .registered
                    .manager
                    .get(worker.instance_id())
                    .is_err()
            );
        }
        assert_eq!(
            running.invocations()[0].status(),
            super::WorkflowInvocationStatus::Running
        );
        assert!(running.invocations()[0].value().is_none());
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        assert_eq!(
            fixture.registered.transcript().await.unwrap().entries.len(),
            initial_entries
        );
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn script_parallel_bounds_fanout_and_allows_nested_composition() {
    bounded(async {
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let fixture = WorkflowFixture::with_reply({
            let entered = entered.clone(); let release = release.clone();
            let active = active.clone(); let peak = peak.clone();
            move |request| {
                let entered = entered.clone(); let release = release.clone();
                let active = active.clone(); let peak = peak.clone();
                async move {
                    let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(count, Ordering::SeqCst);
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    active.fetch_sub(1, Ordering::SeqCst);
                    completion(&request)
                }
            }
        }).await;
        let runtime = WorkflowRuntime::default();
        let run = launch_script(&fixture, &runtime,
            "return await parallel(Array.from({length: 6}, (_, index) => () => agent('item-' + index)));", None).await;
        entered.acquire_many(2).await.unwrap().forget();
        assert_eq!(active.load(Ordering::SeqCst), 2);
        assert_eq!(run.snapshot().active(), 2);
        release.add_permits(6);
        let snapshot = wait_snapshot(&run, is_settled).await;
        assert_eq!(snapshot.status(), WorkflowStatus::Completed, "{:?}", snapshot.error());
        assert_eq!(snapshot.completed(), 6);
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        release.add_permits(4);
        let nested = launch_script(&fixture, &runtime,
            "return await parallel([() => pipeline([1, 2], item => agent(String(item))), () => parallel([() => agent('nested')])]);", None).await;
        let snapshot = wait_snapshot(&nested, is_settled).await;
        assert_eq!(snapshot.status(), WorkflowStatus::Completed, "{:?}", snapshot.error());
        assert_eq!(snapshot.value(), Some(&json!([["child-result", "child-result"], ["child-result"]])));
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    }).await;
}

#[tokio::test]
async fn script_composition_only_filters_operational_worker_failure() {
    bounded(async {
        for (body, expected) in [
            ("return await agent('cancel-me');", None),
            ("return await parallel([() => agent('cancel-me'), () => 'kept']);", Some(json!([null, "kept"]))),
            ("return await pipeline([1], () => agent('cancel-me'), () => { throw new Error('later stage must not run'); });", Some(json!([null]))),
        ] {
            let release = Arc::new(tokio::sync::Semaphore::new(0));
            let mut fixture = WorkflowFixture::with_reply({
                let release = release.clone();
                move |request| {
                    let release = release.clone();
                    async move { release.acquire().await.unwrap().forget(); completion(&request) }
                }
            }).await;
            let runtime = WorkflowRuntime::default();
            let run = launch_script(&fixture, &runtime, body, None).await;
            fixture.requests.recv().await.unwrap();
            let worker = fixture.registered.manager.discover().unwrap().into_iter()
                .find(|member| member.id != *fixture.registered.id()).unwrap();
            fixture.registered.manager.stop(&worker.id).await.unwrap();
            release.add_permits(1);
            let snapshot = wait_snapshot(&run, is_settled).await;
            if let Some(expected) = expected {
                assert_eq!(snapshot.status(), WorkflowStatus::Completed, "{:?}", snapshot.error());
                assert_eq!(snapshot.value(), Some(&expected));
            } else {
                assert_eq!(snapshot.status(), WorkflowStatus::Failed);
            }
            assert_eq!(snapshot.completed(), 1);
            let invocation = &snapshot.invocations()[0];
            assert_eq!(invocation.status(), WorkflowInvocationStatus::Cancelled);
            assert!(invocation.value().is_none());
            assert!(matches!(invocation.error(), Some(WorkflowError::Worker(failure)) if failure.kind == crate::PhiFailureKind::Cancelled));
            assert_eq!(invocation.worker().unwrap().instance_id(), &worker.id);
            assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
            runtime.shutdown().await.unwrap();
            fixture.registered.manager.shutdown().await.unwrap();
        }
        let fixture = WorkflowFixture::new().await;
        let runtime = WorkflowRuntime::default();
        for body in [
            "return await parallel([() => agent('inspect', {role: 'absent'})]);",
            "return await parallel([() => agent('inspect', {schema: undefined})]);",
            "return await pipeline([1], () => { throw new Error('programming error'); });",
            "return await parallel([Promise.resolve('already started')]);",
        ] {
            let run = launch_script(&fixture, &runtime, body, None).await;
            let snapshot = wait_snapshot(&run, is_settled).await;
            assert_eq!(snapshot.status(), WorkflowStatus::Failed, "{body}");
            assert_eq!(snapshot.completed(), 0);
        }
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    }).await;
}

#[tokio::test]
async fn script_permission_denial_is_not_a_null_result() {
    bounded(async {
        let mut fixture = WorkflowFixture::new().await;
        let phi = fixture.registered.phi.clone().with_runtime_services(
            fixture
                .registered
                .phi
                .runtime_services
                .clone()
                .permission_policy(
                    crate::permission::PermissionConfig {
                        mode: crate::permission::PermissionMode::Deny,
                        ..Default::default()
                    },
                    Some(fixture.root.clone()),
                ),
        );
        fixture.replace(phi).await.unwrap();
        let runtime = WorkflowRuntime::default();
        let run = launch_script(
            &fixture,
            &runtime,
            "return await parallel([() => agent('inspect')]);",
            None,
        )
        .await;
        let snapshot = wait_snapshot(&run, is_settled).await;
        assert_eq!(snapshot.status(), WorkflowStatus::Failed);
        assert_eq!(snapshot.completed(), 1);
        let invocation = &snapshot.invocations()[0];
        assert_eq!(invocation.status(), WorkflowInvocationStatus::Failed);
        assert!(matches!(
            invocation.error(),
            Some(WorkflowError::Permission(_))
        ));
        assert!(invocation.worker().is_none());
        assert!(invocation.value().is_none());
        assert!(fixture.requests.is_empty());
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn script_stop_interrupts_composition_and_synchronous_execution() {
    bounded(async {
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let mut fixture = WorkflowFixture::with_reply({
            let release = release.clone();
            move |request| {
                let release = release.clone();
                async move { release.acquire().await.unwrap().forget(); completion(&request) }
            }
        }).await;
        let runtime = WorkflowRuntime::default();
        let run = launch_script(&fixture, &runtime, "try { await parallel([() => agent('blocked')]); } catch {} return await agent('must-not-start');", None).await;
        fixture.requests.recv().await.unwrap();
        let snapshot = run.stop().await.unwrap();
        assert_eq!(snapshot.status(), WorkflowStatus::Stopped);
        assert_eq!(snapshot.completed(), 1);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        release.add_permits(2);
        let busy = launch_script(&fixture, &runtime, "await agent('ready'); while (true) {}", None).await;
        wait_snapshot(&busy, |snapshot| snapshot.completed() == 1).await;
        assert_eq!(busy.stop().await.unwrap().status(), WorkflowStatus::Stopped);
        assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    }).await;
}

#[tokio::test]
async fn script_return_finalizes_unawaited_workers_and_preserves_args() {
    bounded(async {
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let fixture = WorkflowFixture::with_reply({
            let started = started.clone();
            move |request| {
                let started = started.clone();
                async move {
                    if request["messages"].to_string().contains("blocked-worker") {
                        started.add_permits(1);
                        std::future::pending::<()>().await;
                    }
                    started.acquire().await.unwrap().forget();
                    completion(&request)
                }
            }
        })
        .await;
        let runtime = WorkflowRuntime::default();
        for (args, expected) in [
            (None, json!(["undefined", false])),
            (Some(Value::Null), json!(["object", true])),
        ] {
            let run = launch_script(
                &fixture,
                &runtime,
                "agent('blocked-worker'); await agent('quick-worker'); return [typeof args, args === null];",
                args,
            )
            .await;
            let snapshot = wait_snapshot(&run, is_settled).await;
            assert_eq!(snapshot.status(), WorkflowStatus::Completed);
            assert_eq!(snapshot.value(), Some(&expected));
            assert_eq!(snapshot.completed(), 2);
            assert_eq!(fixture.registered.manager.discover().unwrap().len(), 1);
        }
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn script_output_budget_bounds_logs_and_diagnostics() {
    bounded(async {
        let fixture = WorkflowFixture::new().await;
        let runtime = WorkflowRuntime::default();
        for body in [
            "throw new Error('original failure: ' + 'x'.repeat(65536));",
            "log('p'.repeat(2048)); const error = new Error('original failure: ' + '界'.repeat(8192)); error.name = '名'.repeat(8192); error.stack = '帧'.repeat(8192); throw error;",
        ] {
            let run = launch_script(&fixture, &runtime, body, None).await;
            let snapshot = wait_snapshot(&run, is_settled).await;
            assert_eq!(snapshot.status(), WorkflowStatus::Failed);
            let Some(WorkflowError::Script(error)) = snapshot.error() else {
                panic!("script failure must retain its typed diagnostic");
            };
            let bytes = snapshot.logs().iter().map(String::len).sum::<usize>()
                + snapshot.error().unwrap().to_string().len()
                + error.name.as_ref().map_or(0, String::len)
                + error.stack.as_ref().map_or(0, String::len);
            assert!(bytes <= limits().output_bytes(), "diagnostic retained {bytes} bytes");
            assert!(error.message.starts_with("original failure:"));
            assert!(error.message.ends_with("[truncated]"));
        }
        for body in [
            "for (let index = 0; index < 100; index++) log('x'.repeat(100)); return 1;".to_owned(),
            format!("phase('x'.repeat({})); return 1;", limits().output_bytes() + 1),
        ] {
            let run = launch_script(&fixture, &runtime, &body, None).await;
            let snapshot = wait_snapshot(&run, is_settled).await;
            assert_eq!(snapshot.status(), WorkflowStatus::Failed);
            assert!(matches!(
                snapshot.error(),
                Some(WorkflowError::Script(error))
                    if error.name.as_deref() == Some("RangeError")
                        && error.message.starts_with("workflow output limit reached")
            ), "{body}: {:?}", snapshot.error());
            let bytes = snapshot.logs().iter().map(String::len).sum::<usize>()
                + snapshot.phase().map_or(0, str::len);
            assert!(bytes <= limits().output_bytes());
            assert!(snapshot.value().is_none());
        }
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn script_execution_preserves_async_body_grammar_and_json_keys() {
    bounded(async {
        let fixture = WorkflowFixture::new().await;
        let runtime = WorkflowRuntime::default();
        let mut observed = Vec::new();
        for (body, args) in [
            ("return typeof new.target;", None),
            (
                "return [Object.hasOwn(args, '__proto__'), args.__proto__.value];",
                Some(json!({"__proto__": {"value": 17}})),
            ),
        ] {
            let run = launch_script(&fixture, &runtime, body, args).await;
            let snapshot = wait_snapshot(&run, is_settled).await;
            observed.push((snapshot.status(), snapshot.value().cloned()));
        }
        assert_eq!(
            observed,
            [
                (WorkflowStatus::Completed, Some(json!("undefined"))),
                (WorkflowStatus::Completed, Some(json!([true, 17]))),
            ]
        );
        runtime.shutdown().await.unwrap();
        fixture.registered.manager.shutdown().await.unwrap();
    })
    .await;
}
