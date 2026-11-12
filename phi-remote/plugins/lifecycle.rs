use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, ensure};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::{Id, JoinError, JoinSet};
use tokio::time::Instant;

use super::{PluginContext, PluginRegistry, PluginTask};
use crate::GatewayScheduler;

const CONTROL_CAPACITY: usize = 16;
const PLUGIN_STOP_TIMEOUT: Duration = Duration::from_secs(30);

type Reply = oneshot::Sender<anyhow::Result<()>>;

#[derive(Clone, Copy, Debug)]
enum Operation {
    Stop,
    Restart,
}

struct Command {
    name: String,
    operation: Operation,
    reply: Reply,
}

/// Controls background tasks only; statically composed HTTP routes remain installed.
/// The host must continuously poll `Gateway::wait()` to drive these requests.
#[derive(Clone)]
pub struct PluginControl {
    commands: mpsc::Sender<Command>,
    shutdown: watch::Receiver<bool>,
}

impl PluginControl {
    /// Waits for the task and its plugin-owned cleanup to finish.
    /// Dropping the waiter does not cancel an accepted request.
    pub async fn stop(&self, name: &str) -> anyhow::Result<()> {
        self.request(name, Operation::Stop).await
    }

    /// Starts a fresh run only after successful cleanup of the previous run.
    /// Success confirms launch, not readiness. Plugins without background tasks are rejected.
    /// Cleanup timeout retains the old task and cancels this restart intent; retry after cleanup.
    pub async fn restart(&self, name: &str) -> anyhow::Result<()> {
        self.request(name, Operation::Restart).await
    }

    async fn request(&self, name: &str, operation: Operation) -> anyhow::Result<()> {
        let mut shutdown = self.shutdown.clone();
        ensure!(!*shutdown.borrow(), "Gateway is stopping");
        let (reply, response) = oneshot::channel();
        let command = Command {
            name: name.to_owned(),
            operation,
            reply,
        };
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopping| *stopping) => anyhow::bail!("Gateway is stopping"),
            result = self.commands.send(command) => result.context("Gateway plugin control is closed")?,
        }
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopping| *stopping) => anyhow::bail!("Gateway is stopping"),
            result = response => result.context("Gateway plugin control is closed")?,
        }
    }
}

struct Pending {
    operation: Operation,
    reply: Reply,
    deadline: Instant,
}

enum TaskPhase {
    Running,
    Stopping,
}

struct TaskResult {
    phase: TaskPhase,
    result: anyhow::Result<()>,
}

struct PluginRun {
    scheduler: GatewayScheduler,
    shutdown: watch::Sender<bool>,
}

impl PluginRun {
    fn stop(&self) {
        self.scheduler.close_scope();
        self.shutdown.send_replace(true);
    }
}

struct Entry {
    factory: PluginTask,
    run: Option<PluginRun>,
    pending: Option<Pending>,
    cleanup_failure: Option<String>,
}

/// Gateway-owned task custody. No work is supervised by a detached driver.
pub(crate) struct PluginTasks {
    entries: HashMap<String, Entry>,
    names: HashMap<Id, String>,
    tasks: JoinSet<TaskResult>,
    commands: mpsc::Receiver<Command>,
    control: PluginControl,
    directory: PathBuf,
    endpoint: String,
    scheduler: GatewayScheduler,
    stopping: bool,
    failures: Vec<String>,
}

impl PluginTasks {
    pub(crate) fn new(shutdown: watch::Receiver<bool>, scheduler: GatewayScheduler) -> Self {
        let (commands, receiver) = mpsc::channel(CONTROL_CAPACITY);
        Self {
            entries: HashMap::new(),
            names: HashMap::new(),
            tasks: JoinSet::new(),
            commands: receiver,
            control: PluginControl { commands, shutdown },
            directory: PathBuf::new(),
            endpoint: String::new(),
            scheduler,
            stopping: false,
            failures: Vec::new(),
        }
    }

    pub(crate) fn control(&self) -> PluginControl {
        self.control.clone()
    }

    pub(crate) fn start(&mut self, registry: PluginRegistry, directory: &Path, endpoint: &str) {
        self.directory = directory.to_owned();
        self.endpoint = endpoint.to_owned();
        for (name, plugin) in registry.entries {
            if let Some(factory) = plugin.task {
                self.entries.insert(
                    name.clone(),
                    Entry {
                        factory,
                        run: None,
                        pending: None,
                        cleanup_failure: None,
                    },
                );
                self.launch(&name);
            }
        }
    }

    fn launch(&mut self, name: &str) {
        let entry = self.entries.get_mut(name).expect("plugin entry missing");
        let (shutdown, receiver) = watch::channel(false);
        let context = PluginContext {
            gateway_directory: self.directory.clone(),
            endpoint: self.endpoint.clone(),
            shutdown: receiver,
            scheduler: self.scheduler.scope(),
        };
        let factory = entry.factory.clone();
        let mut gateway_shutdown = self.control.shutdown.clone();
        let stopped = shutdown.clone();
        let running = context.clone();
        let scheduler = context.scheduler.clone();
        let task = self.tasks.spawn(async move {
            if *stopped.borrow() || *gateway_shutdown.borrow() {
                scheduler.close_scope();
                stopped.send_replace(true);
                return TaskResult {
                    phase: TaskPhase::Stopping,
                    result: Ok(()),
                };
            }
            let work = async move { factory(running).await };
            tokio::pin!(work);
            let closing = async {
                let _ = gateway_shutdown.wait_for(|stopping| *stopping).await;
            };
            let result = tokio::select! {
                result = &mut work => result,
                _ = closing => {
                    stopped.send_replace(true);
                    work.await
                }
            };
            let phase = if *stopped.borrow() || *gateway_shutdown.borrow() {
                TaskPhase::Stopping
            } else {
                TaskPhase::Running
            };
            scheduler.close_scope();
            stopped.send_replace(true);
            TaskResult { phase, result }
        });
        self.names.insert(task.id(), name.to_owned());
        entry.run = Some(PluginRun {
            scheduler: context.scheduler,
            shutdown,
        });
    }

    fn command(&mut self, command: Command) {
        let Command {
            name,
            operation,
            reply,
        } = command;
        let result = (|| {
            ensure!(
                !self.stopping && !*self.control.shutdown.borrow(),
                "Gateway is stopping"
            );
            let entry = self
                .entries
                .get_mut(&name)
                .context("unknown background plugin")?;
            ensure!(entry.pending.is_none(), "remote plugin {name} is stopping");
            if let Some(failure) = &entry.cleanup_failure {
                anyhow::bail!("remote plugin {name} cleanup failed: {failure}");
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = reply.send(Err(error));
            return;
        }
        let entry = self.entries.get_mut(&name).expect("plugin entry missing");
        if entry.run.is_none() {
            if matches!(operation, Operation::Restart) {
                self.launch(&name);
            }
            let _ = reply.send(Ok(()));
            return;
        }
        entry.run.as_ref().expect("plugin run missing").stop();
        entry.pending = Some(Pending {
            operation,
            reply,
            deadline: Instant::now() + PLUGIN_STOP_TIMEOUT,
        });
    }

    fn completed(&mut self, result: Result<(Id, TaskResult), JoinError>) {
        let id = match &result {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        let name = self.names.remove(&id).expect("plugin task name missing");
        let entry = self.entries.get_mut(&name).expect("plugin entry missing");
        let retain_failure = match &result {
            Ok((_, result)) => {
                matches!(result.phase, TaskPhase::Stopping)
                    || result
                        .result
                        .as_ref()
                        .err()
                        .is_some_and(|error| error.is::<super::CleanupFailure>())
            }
            Err(_) => true,
        };
        entry.run.take().expect("plugin run missing").stop();
        let failure = match result {
            Ok((_, result)) => result
                .result
                .err()
                .map(|error| format!("remote plugin {name} failed: {error:#}")),
            Err(error) => Some(format!("remote plugin {name} task failed: {error}")),
        };
        if let Some(failure) = &failure {
            tracing::error!(%failure, "remote plugin stopped; Gateway remains available");
            if retain_failure {
                entry.cleanup_failure = Some(failure.clone());
                self.failures.push(failure.clone());
            }
        }
        if let Some(pending) = entry.pending.take() {
            if let Some(failure) = failure {
                let _ = pending.reply.send(Err(anyhow::anyhow!(failure)));
            } else if !self.stopping && !*self.control.shutdown.borrow() {
                if matches!(pending.operation, Operation::Restart) {
                    self.launch(&name);
                }
                let _ = pending.reply.send(Ok(()));
            } else {
                let _ = pending
                    .reply
                    .send(Err(anyhow::anyhow!("Gateway is stopping")));
            }
        }
    }

    /// Each poll either handles one command/result or expires a cleanup observer.
    pub(crate) async fn next(&mut self) {
        let deadline = self
            .entries
            .values()
            .filter_map(|entry| entry.pending.as_ref().map(|pending| pending.deadline))
            .min();
        let expires = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            _ = expires => {
                for (name, entry) in &mut self.entries {
                    if entry.pending.as_ref().is_some_and(|pending| pending.deadline <= Instant::now()) {
                        let pending = entry.pending.take().expect("plugin stop missing");
                        let _ = pending.reply.send(Err(anyhow::anyhow!("remote plugin {name} cleanup timed out; task is still owned")));
                    }
                }
            }
            Some(result) = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => self.completed(result),
            Some(command) = self.commands.recv(), if !self.stopping => self.command(command),
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.stopping = true;
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            let _ = command
                .reply
                .send(Err(anyhow::anyhow!("Gateway is stopping")));
        }
        for entry in self.entries.values_mut() {
            if let Some(run) = &entry.run {
                run.stop();
            }
            if let Some(pending) = entry.pending.take() {
                let _ = pending
                    .reply
                    .send(Err(anyhow::anyhow!("Gateway is stopping")));
            }
        }
    }

    pub(crate) async fn drain(&mut self) -> Vec<String> {
        self.shutdown();
        while let Some(result) = self.tasks.join_next_with_id().await {
            self.completed(result);
        }
        std::mem::take(&mut self.failures)
    }

    pub(crate) async fn abort(&mut self) -> Vec<String> {
        self.shutdown();
        self.tasks.abort_all();
        self.drain().await
    }
}

impl Drop for PluginTasks {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::Plugin;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::Semaphore;

    struct Active(Arc<AtomicUsize>);

    impl Drop for Active {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    struct Fixture {
        tasks: PluginTasks,
        started: mpsc::UnboundedReceiver<PluginContext>,
        cleanup: Arc<Semaphore>,
        active: Arc<AtomicUsize>,
        shutdown: watch::Sender<bool>,
        driver: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let (shutdown, closed) = watch::channel(false);
            let (scheduler, driver) = crate::gateway::scheduler::test_scheduler(closed.clone());
            let (started, receiver) = mpsc::unbounded_channel();
            let cleanup = Arc::new(Semaphore::new(0));
            let active = Arc::new(AtomicUsize::new(0));
            let released = cleanup.clone();
            let running = active.clone();
            let mut registry = PluginRegistry::new();
            registry
                .register(
                    "test:worker",
                    Plugin::new().with_task(move |context| {
                        let started = started.clone();
                        let cleanup = released.clone();
                        let active = running.clone();
                        async move {
                            assert_eq!(
                                active.fetch_add(1, Ordering::SeqCst),
                                0,
                                "overlapping plugin runs"
                            );
                            let _active = Active(active);
                            let mut shutdown = context.shutdown();
                            started.send(context).unwrap();
                            let _ = shutdown.wait_for(|stopping| *stopping).await;
                            cleanup.acquire().await.unwrap().forget();
                            Ok(())
                        }
                    }),
                )
                .unwrap();
            let mut tasks = PluginTasks::new(closed, scheduler);
            tasks.start(registry, Path::new("/tmp"), "http://127.0.0.1:1/");
            Self {
                tasks,
                started: receiver,
                cleanup,
                active,
                shutdown,
                driver,
            }
        }

        async fn drive(
            &mut self,
            request: impl std::future::Future<Output = anyhow::Result<()>>,
        ) -> anyhow::Result<()> {
            tokio::pin!(request);
            loop {
                tokio::select! {
                    result = &mut request => return result,
                    () = self.tasks.next() => {}
                }
            }
        }

        async fn close(mut self) {
            self.cleanup.add_permits(1);
            self.shutdown.send_replace(true);
            assert!(self.tasks.drain().await.is_empty());
            self.driver.await.unwrap();
            assert_eq!(self.active.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    async fn restart_joins_cleanup_and_retires_old_scope() {
        let mut fixture = Fixture::new().await;
        let first = fixture.started.recv().await.unwrap();
        let control = fixture.tasks.control();
        let restart = control.restart("test:worker");
        tokio::pin!(restart);
        assert!(futures_util::poll!(&mut restart).is_pending());
        fixture.tasks.next().await;
        assert!(*first.shutdown().borrow());
        assert_eq!(first.scheduler().wakeup().wait().await, None);
        assert!(fixture.started.try_recv().is_err());
        assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
        fixture.cleanup.add_permits(1);
        fixture.drive(restart).await.unwrap();
        let second = fixture.started.recv().await.unwrap();
        assert!(!*second.shutdown().borrow());
        assert_eq!(first.scheduler().wakeup().wait().await, None);
        let mut wakeup = second.scheduler().wakeup();
        wakeup.arm(Instant::now());
        assert_eq!(wakeup.wait().await, Some(()));
        fixture.cleanup.add_permits(1);
        fixture.drive(control.stop("test:worker")).await.unwrap();
        fixture.drive(control.stop("test:worker")).await.unwrap();
        assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
        fixture.drive(control.restart("test:worker")).await.unwrap();
        fixture.started.recv().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test(start_paused = true)]
    async fn stop_timeout_retains_custody_and_never_restarts_automatically() {
        let mut fixture = Fixture::new().await;
        fixture.started.recv().await.unwrap();
        let control = fixture.tasks.control();
        let restart = control.restart("test:worker");
        tokio::pin!(restart);
        assert!(futures_util::poll!(&mut restart).is_pending());
        fixture.tasks.next().await;
        fixture.tasks.next().await;
        assert!(
            restart
                .await
                .unwrap_err()
                .to_string()
                .contains("cleanup timed out")
        );
        assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
        fixture.cleanup.add_permits(1);
        fixture.tasks.next().await;
        assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
        assert!(fixture.started.try_recv().is_err());
        fixture.drive(control.restart("test:worker")).await.unwrap();
        fixture.started.recv().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn conflicting_request_keeps_the_original_cleanup_owner() {
        let mut fixture = Fixture::new().await;
        fixture.started.recv().await.unwrap();
        let control = fixture.tasks.control();
        let restart = control.restart("test:worker");
        tokio::pin!(restart);
        assert!(futures_util::poll!(&mut restart).is_pending());
        fixture.tasks.next().await;
        let error = fixture
            .drive(control.stop("test:worker"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("is stopping"));
        fixture.cleanup.add_permits(1);
        fixture.drive(restart).await.unwrap();
        fixture.started.recv().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn canceled_control_waiter_does_not_cancel_accepted_restart() {
        let mut fixture = Fixture::new().await;
        fixture.started.recv().await.unwrap();
        let control = fixture.tasks.control();
        {
            let restart = control.restart("test:worker");
            tokio::pin!(restart);
            assert!(futures_util::poll!(&mut restart).is_pending());
            fixture.tasks.next().await;
        }
        fixture.cleanup.add_permits(1);
        fixture.tasks.next().await;
        fixture.started.recv().await.unwrap();
        fixture.close().await;
    }

    #[tokio::test]
    async fn shutdown_cancels_pending_restart_and_rejects_new_commands() {
        let mut fixture = Fixture::new().await;
        fixture.started.recv().await.unwrap();
        let control = fixture.tasks.control();
        let restart = control.restart("test:worker");
        tokio::pin!(restart);
        assert!(futures_util::poll!(&mut restart).is_pending());
        fixture.tasks.next().await;
        fixture.shutdown.send_replace(true);
        assert!(restart.await.is_err());
        assert!(control.restart("test:worker").await.is_err());
        fixture.cleanup.add_permits(1);
        assert!(fixture.tasks.drain().await.is_empty());
        assert!(fixture.started.try_recv().is_err());
        fixture.driver.await.unwrap();
    }

    #[tokio::test]
    async fn finished_run_closes_retained_scope_before_result_collection() {
        let (shutdown, closed) = watch::channel(false);
        let (scheduler, driver) = crate::gateway::scheduler::test_scheduler(closed.clone());
        let (started, mut ready) = mpsc::unbounded_channel();
        let mut registry = PluginRegistry::new();
        registry
            .register(
                "test:finite",
                Plugin::new().with_task(move |context| {
                    let started = started.clone();
                    async move {
                        started.send(context).unwrap();
                        Ok(())
                    }
                }),
            )
            .unwrap();
        let mut tasks = PluginTasks::new(closed, scheduler);
        tasks.start(registry, Path::new("/tmp"), "http://127.0.0.1:1/");
        let context = ready.recv().await.unwrap();
        assert!(*context.shutdown().borrow());
        assert_eq!(context.scheduler().wakeup().wait().await, None);
        assert!(tasks.drain().await.is_empty());
        shutdown.send_replace(true);
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn cleanup_failure_blocks_replacement_and_preserves_plugin_name() {
        let (shutdown, closed) = watch::channel(false);
        let (scheduler, driver) = crate::gateway::scheduler::test_scheduler(closed.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let (started, mut ready) = mpsc::unbounded_channel();
        let mut registry = PluginRegistry::new();
        registry
            .register(
                "test:broken",
                Plugin::new().with_task(move |context| {
                    let calls = counted.clone();
                    let started = started.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        started.send(()).unwrap();
                        let mut shutdown = context.shutdown();
                        let _ = shutdown.wait_for(|stopping| *stopping).await;
                        anyhow::bail!("cleanup rejected")
                    }
                }),
            )
            .unwrap();
        let mut tasks = PluginTasks::new(closed, scheduler);
        tasks.start(registry, Path::new("/tmp"), "http://127.0.0.1:1/");
        ready.recv().await.unwrap();
        let control = tasks.control();
        for _ in 0..2 {
            let restart = control.restart("test:broken");
            tokio::pin!(restart);
            let error = loop {
                tokio::select! {
                    result = &mut restart => break result.unwrap_err(),
                    () = tasks.next() => {}
                }
            };
            assert!(error.to_string().contains("test:broken"));
            assert!(error.to_string().contains("cleanup rejected"));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(tasks.drain().await.len(), 1);
        shutdown.send_replace(true);
        driver.await.unwrap();
    }

    #[tokio::test]
    async fn runtime_cleanup_failure_blocks_restart_and_clean_close() {
        let (shutdown, closed) = watch::channel(false);
        let (scheduler, driver) = crate::gateway::scheduler::test_scheduler(closed.clone());
        let mut registry = PluginRegistry::new();
        registry
            .register(
                "test:cleanup",
                Plugin::new().with_task(|_| async {
                    Err(anyhow::anyhow!("presentation cleanup timed out")
                        .context(super::super::CleanupFailure))
                }),
            )
            .unwrap();
        let mut tasks = PluginTasks::new(closed, scheduler);
        tasks.start(registry, Path::new("/tmp"), "http://127.0.0.1:1/");
        tasks.next().await;
        let control = tasks.control();
        let restart = control.restart("test:cleanup");
        tokio::pin!(restart);
        let result = loop {
            tokio::select! {
                result = &mut restart => break result,
                () = tasks.next() => {}
            }
        };
        assert!(result.is_err(), "cleanup failure must block restart");
        let failures = tasks.drain().await;
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("test:cleanup"));
        assert!(failures[0].contains("presentation cleanup timed out"));
        shutdown.send_replace(true);
        driver.await.unwrap();
    }

    proptest::proptest! {
        #[test]
        fn repeated_controls_never_overlap_runs(
            operations in proptest::collection::vec(
                proptest::prop_oneof![
                    proptest::strategy::Just(Operation::Stop),
                    proptest::strategy::Just(Operation::Restart),
                ],
                0..24,
            )
        ) {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time().build().unwrap();
            runtime.block_on(async {
                let mut fixture = Fixture::new().await;
                fixture.started.recv().await.unwrap();
                let control = fixture.tasks.control();
                for operation in operations {
                    fixture.cleanup.add_permits(1);
                    match operation {
                        Operation::Stop => {
                            fixture.drive(control.stop("test:worker")).await.unwrap();
                            assert_eq!(fixture.active.load(Ordering::SeqCst), 0);
                        }
                        Operation::Restart => {
                            fixture.drive(control.restart("test:worker")).await.unwrap();
                            fixture.started.recv().await.unwrap();
                            assert_eq!(fixture.active.load(Ordering::SeqCst), 1);
                        }
                    }
                }
                fixture.close().await;
            });
        }
    }

    #[tokio::test]
    async fn factory_panic_keeps_identity_and_blocks_restart() {
        let (shutdown, closed) = watch::channel(false);
        let (scheduler, driver) = crate::gateway::scheduler::test_scheduler(closed.clone());
        let mut registry = PluginRegistry::new();
        registry
            .register(
                "test:panic",
                Plugin::new().with_task(|_| {
                    panic!("factory panicked");
                    #[allow(unreachable_code)]
                    async {
                        Ok(())
                    }
                }),
            )
            .unwrap();
        let mut tasks = PluginTasks::new(closed, scheduler);
        tasks.start(registry, Path::new("/tmp"), "http://127.0.0.1:1/");
        tasks.next().await;
        let failures = tasks.drain().await;
        assert!(failures[0].contains("test:panic"));
        assert!(failures[0].contains("factory panicked"));
        shutdown.send_replace(true);
        driver.await.unwrap();
    }
}
