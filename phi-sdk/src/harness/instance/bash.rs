use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::core::event::{CoreEventScope, ToolOutputDetails};

const RECENT_EXECUTION_LIMIT: usize = 64;
const ACTIVE_EXECUTION_LIMIT: usize = 32;
const BACKGROUND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MAX_BACKGROUND_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

#[derive(Clone, Copy)]
pub(crate) enum BashLifetime {
    Interactive,
    Unattended,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BashMode {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BashStatus {
    Running,
    Completed,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashExecution {
    pub id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    pub command: String,
    pub mode: BashMode,
    pub status: BashStatus,
    pub output_path: PathBuf,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

pub(crate) struct BashRequest {
    pub(crate) command: tokio::process::Command,
    pub(crate) description: String,
    pub(crate) scope: CoreEventScope,
    pub(crate) mode: BashMode,
    pub(crate) timeout: Option<Duration>,
    pub(crate) lifetime: BashLifetime,
    pub(crate) reply: oneshot::Sender<Result<BashReply, String>>,
}

#[derive(Debug)]
pub(crate) enum BashReply {
    Finished(String, ToolOutputDetails),
    Background(BashExecution),
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BashStopReason {
    UserCancelled,
    InvocationInterrupted,
    OwnerEnded,
    AgentStopped,
    TimedOut,
}

impl BashStopReason {
    fn message(self) -> &'static str {
        match self {
            Self::UserCancelled => "Bash execution cancelled by the user",
            Self::InvocationInterrupted => "Bash invocation interrupted",
            Self::OwnerEnded => "Owning Agent run ended",
            Self::AgentStopped => "Agent stopped",
            Self::TimedOut => "Bash command timed out",
        }
    }
}

pub(crate) struct BashOutcome {
    pub(crate) execution: BashExecution,
    pub(crate) output: String,
    pub(crate) stop_reason: Option<BashStopReason>,
}

struct Execution {
    view: BashExecution,
    ready: bool,
    reply: Option<oneshot::Sender<Result<BashReply, String>>>,
    deadline: Option<Instant>,
    wait_limit: Option<Duration>,
    background_limit: Option<Duration>,
    acknowledged: bool,
    observed_status: Option<BashStatus>,
    auto_background: bool,
    cancellation: watch::Sender<Option<String>>,
    stop_reason: Option<BashStopReason>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl Execution {
    fn cancel(&mut self, reason: BashStopReason) {
        if self.stop_reason.is_none() {
            self.stop_reason = Some(reason);
            self.cancellation
                .send_replace(Some(reason.message().into()));
        }
    }
}

enum WorkerMessage {
    Started(String),
    Finished {
        id: String,
        result: Result<(String, ToolOutputDetails), String>,
        stopped: Option<String>,
    },
}

#[derive(Clone)]
pub struct BashOutputSubscription {
    changes: watch::Receiver<()>,
}

impl BashOutputSubscription {
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.changes.changed().await
    }
}

pub(crate) struct BashExecutions {
    output_changes: watch::Sender<()>,
    executions: VecDeque<Execution>,
    sender: mpsc::UnboundedSender<WorkerMessage>,
    receiver: mpsc::UnboundedReceiver<WorkerMessage>,
    outcomes: VecDeque<BashOutcome>,
}

impl Default for BashExecutions {
    fn default() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Self {
            output_changes: watch::channel(()).0,
            executions: VecDeque::new(),
            sender,
            receiver,
            outcomes: VecDeque::new(),
        }
    }
}

impl Drop for BashExecutions {
    fn drop(&mut self) {
        for execution in &self.executions {
            if let Some(worker) = &execution.worker {
                worker.abort();
            }
        }
    }
}

impl BashExecutions {
    pub(super) fn subscribe_output(&self) -> BashOutputSubscription {
        BashOutputSubscription {
            changes: self.output_changes.subscribe(),
        }
    }

    pub(crate) fn start(&mut self, request: BashRequest) {
        if request.reply.is_closed() {
            return;
        }
        if self
            .executions
            .iter()
            .filter(|entry| entry.view.status == BashStatus::Running)
            .count()
            >= ACTIVE_EXECUTION_LIMIT
            || self.outcomes.len() >= RECENT_EXECUTION_LIMIT
        {
            let _ = request
                .reply
                .send(Err("Bash execution limit reached".into()));
            return;
        }
        let Some(turn_id) = request.scope.turn_id else {
            let _ = request
                .reply
                .send(Err("Bash requires an active turn".into()));
            return;
        };
        let Some(tool_call_id) = request.scope.parent_tool_call_id else {
            let _ = request
                .reply
                .send(Err("Bash requires a tool-call identity".into()));
            return;
        };
        let id = uuid::Uuid::new_v4().to_string();
        let path = std::env::temp_dir().join(format!("phi-bash-{id}.log"));
        let (cancellation, cancelled) = watch::channel(None);
        let sender = self.sender.clone();
        let worker = tokio::spawn(run_execution(
            id.clone(),
            request.command,
            path.clone(),
            cancelled,
            sender,
            self.output_changes.clone(),
        ));
        let background_limit = match request.lifetime {
            BashLifetime::Interactive => None,
            BashLifetime::Unattended => Some(if request.mode == BashMode::Background {
                request
                    .timeout
                    .unwrap_or(BACKGROUND_TIMEOUT)
                    .min(MAX_BACKGROUND_TIMEOUT)
            } else {
                BACKGROUND_TIMEOUT
            }),
        };
        let auto_background = request.description.split_whitespace().next() != Some("sleep");
        self.executions.push_back(Execution {
            view: BashExecution {
                id,
                turn_id: turn_id.to_string(),
                tool_call_id: tool_call_id.to_string(),
                command: request.description,
                mode: request.mode,
                status: BashStatus::Running,
                output_path: path,
                exit_code: None,
                error: None,
            },
            ready: false,
            reply: Some(request.reply),
            deadline: None,
            wait_limit: request.timeout,
            background_limit,
            acknowledged: false,
            observed_status: None,
            auto_background,
            cancellation,
            stop_reason: None,
            worker: Some(worker),
        });
    }

    pub(crate) fn snapshots(&self) -> Vec<BashExecution> {
        self.executions
            .iter()
            .filter(|entry| entry.ready || entry.view.status != BashStatus::Running)
            .map(|entry| entry.view.clone())
            .collect()
    }

    pub(crate) fn take_changes(&mut self) -> Vec<BashExecution> {
        self.executions
            .iter_mut()
            .filter_map(|entry| {
                if !entry.acknowledged || entry.observed_status == Some(entry.view.status) {
                    return None;
                }
                entry.observed_status = Some(entry.view.status);
                Some(entry.view.clone())
            })
            .collect()
    }

    pub(crate) fn pending(&self) -> bool {
        self.has_running() || !self.outcomes.is_empty()
    }

    pub(crate) fn next_outcome(&self) -> Option<&BashOutcome> {
        self.outcomes.front()
    }

    pub(crate) fn take_outcome(&mut self) -> Option<BashOutcome> {
        self.outcomes.pop_front()
    }

    pub(crate) fn background(&mut self, id: &str) -> Result<BashExecution, String> {
        self.poll();
        let entry = self
            .executions
            .iter_mut()
            .find(|entry| entry.view.id == id)
            .ok_or_else(|| "Bash execution is unavailable".to_owned())?;
        if entry.view.status != BashStatus::Running {
            return Err("Bash execution has already ended".into());
        }
        if !entry.ready {
            return Err("Bash execution is still starting".into());
        }
        acknowledge_background(entry)?;
        Ok(entry.view.clone())
    }

    pub(crate) fn stop(&mut self, id: &str) -> Result<(), String> {
        self.poll();
        let entry = self
            .executions
            .iter_mut()
            .find(|entry| entry.view.id == id)
            .ok_or_else(|| "Bash execution is unavailable".to_owned())?;
        if entry.view.status == BashStatus::Running {
            entry.cancel(BashStopReason::UserCancelled);
        }
        Ok(())
    }

    pub(crate) fn discard_outcomes(&mut self) {
        self.outcomes.clear();
    }

    pub(crate) fn take_control_outcomes(&mut self) -> VecDeque<BashOutcome> {
        let (control, pending) =
            std::mem::take(&mut self.outcomes)
                .into_iter()
                .partition(|outcome| {
                    matches!(
                        outcome.stop_reason,
                        Some(
                            BashStopReason::UserCancelled
                                | BashStopReason::OwnerEnded
                                | BashStopReason::AgentStopped
                        )
                    )
                });
        self.outcomes = pending;
        control
    }

    pub(crate) fn has_running(&self) -> bool {
        self.executions
            .iter()
            .any(|entry| entry.view.status == BashStatus::Running)
    }

    pub(crate) fn cancel_all(&mut self) {
        for entry in &mut self.executions {
            if entry.view.status == BashStatus::Running {
                entry.cancel(BashStopReason::OwnerEnded);
            }
        }
    }

    pub(crate) fn cancel_foreground(&mut self) {
        for entry in &mut self.executions {
            if entry.view.mode == BashMode::Foreground && entry.view.status == BashStatus::Running {
                entry.cancel(BashStopReason::InvocationInterrupted);
            }
        }
    }

    pub(crate) async fn shutdown(&mut self) -> VecDeque<BashOutcome> {
        for entry in &mut self.executions {
            if entry.view.status == BashStatus::Running {
                entry.cancel(BashStopReason::AgentStopped);
            }
        }
        for entry in &mut self.executions {
            if let Some(worker) = entry.worker.take() {
                let _ = worker.await;
            }
        }
        self.poll();
        std::mem::take(&mut self.outcomes)
    }

    pub(crate) fn poll(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                WorkerMessage::Started(id) => {
                    if let Some(entry) =
                        self.executions.iter_mut().find(|entry| entry.view.id == id)
                    {
                        entry.ready = true;
                        entry.deadline = entry.wait_limit.map(|limit| Instant::now() + limit);
                        if entry.view.mode == BashMode::Background {
                            let _ = acknowledge_background(entry);
                        }
                    }
                }
                WorkerMessage::Finished {
                    id,
                    result,
                    stopped,
                } => {
                    let Some(entry) = self.executions.iter_mut().find(|entry| entry.view.id == id)
                    else {
                        continue;
                    };
                    let (output, details) = match result {
                        Ok((output, details)) => {
                            entry.view.exit_code = Some(details.exit_code);
                            entry.view.status = if details.exit_code == 0 {
                                BashStatus::Completed
                            } else {
                                BashStatus::Failed
                            };
                            (output, Some(details))
                        }
                        Err(error) => {
                            entry.view.status = if stopped.is_some() {
                                BashStatus::Stopped
                            } else {
                                BashStatus::Failed
                            };
                            entry.view.error = Some(error.clone());
                            (error, None)
                        }
                    };
                    if let Some(reply) = entry.reply.take() {
                        let result = match details {
                            Some(details) => Ok(BashReply::Finished(output, details)),
                            None => Err(output),
                        };
                        let _ = reply.send(result);
                    } else if entry.acknowledged {
                        self.outcomes.push_back(BashOutcome {
                            execution: entry.view.clone(),
                            output,
                            stop_reason: entry.stop_reason,
                        });
                    }
                }
            }
        }
        for entry in &mut self.executions {
            if entry.view.status != BashStatus::Running {
                continue;
            }
            if entry.reply.as_ref().is_some_and(oneshot::Sender::is_closed) {
                entry.cancel(BashStopReason::InvocationInterrupted);
            } else if entry
                .deadline
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                entry.deadline = None;
                if entry.view.mode == BashMode::Foreground && entry.ready && entry.auto_background {
                    let _ = acknowledge_background(entry);
                } else {
                    entry.cancel(BashStopReason::TimedOut);
                }
            }
        }
        while self.executions.len() > RECENT_EXECUTION_LIMIT {
            let Some(index) = self.executions.iter().position(|entry| {
                entry.view.status != BashStatus::Running
                    && (!entry.acknowledged || entry.observed_status == Some(entry.view.status))
            }) else {
                break;
            };
            self.executions.remove(index);
        }
    }
}

fn acknowledge_background(entry: &mut Execution) -> Result<(), String> {
    if let Some(reply) = entry.reply.take() {
        let mut view = entry.view.clone();
        view.mode = BashMode::Background;
        if reply.send(Ok(BashReply::Background(view))).is_err() {
            entry.cancel(BashStopReason::InvocationInterrupted);
            return Err("Bash invocation was interrupted".into());
        }
        entry.view.mode = BashMode::Background;
        entry.acknowledged = true;
        entry.deadline = entry.background_limit.map(|limit| Instant::now() + limit);
    }
    Ok(())
}

async fn run_execution(
    id: String,
    command: tokio::process::Command,
    path: PathBuf,
    cancellation: watch::Receiver<Option<String>>,
    sender: mpsc::UnboundedSender<WorkerMessage>,
    output_changes: watch::Sender<()>,
) {
    let (ready, mut started) = oneshot::channel();
    let operation = crate::agent::tools::run_managed_bash(
        command,
        path,
        ready,
        cancellation.clone(),
        output_changes,
    );
    tokio::pin!(operation);
    let mut reported = false;
    let result = loop {
        tokio::select! {
            biased;
            ready = &mut started, if !reported => {
                reported = true;
                if ready.is_ok() {
                    let _ = sender.send(WorkerMessage::Started(id.clone()));
                }
            }
            result = &mut operation => break result.map_err(|error| error.to_string()),
        }
    };
    if !reported && started.try_recv().is_ok() {
        let _ = sender.send(WorkerMessage::Started(id.clone()));
    }
    let stopped = result
        .is_err()
        .then(|| cancellation.borrow().clone())
        .flatten();
    let _ = sender.send(WorkerMessage::Finished {
        id,
        result,
        stopped,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::Sandbox;

    struct Fixture {
        root: PathBuf,
        executions: BashExecutions,
    }

    impl Fixture {
        fn new() -> Self {
            let root =
                PathBuf::from("/tmp").join(format!("phi-bash-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            Self {
                root,
                executions: Default::default(),
            }
        }

        fn start(
            &mut self,
            command: &str,
            mode: BashMode,
        ) -> oneshot::Receiver<Result<BashReply, String>> {
            let (reply, result) = oneshot::channel();
            self.executions.start(BashRequest {
                command: Sandbox::new(false, &self.root)
                    .unwrap()
                    .wrap_command(command),
                description: command.into(),
                scope: CoreEventScope {
                    turn_id: Some("turn".into()),
                    parent_tool_call_id: Some("call".into()),
                    ..Default::default()
                },
                mode,
                timeout: None,
                lifetime: BashLifetime::Interactive,
                reply,
            });
            result
        }

        async fn shutdown(&mut self) {
            tokio::time::timeout(Duration::from_secs(5), self.executions.shutdown())
                .await
                .expect("Bash test cleanup timed out");
        }

        async fn poll_until(&mut self, condition: impl Fn(&BashExecutions) -> bool) {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    self.executions.poll();
                    if condition(&self.executions) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for execution in self.executions.snapshots() {
                let _ = std::fs::remove_file(execution.output_path);
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const GATED_COMMAND: &str = "printf ready; deadline=$((SECONDS + 5)); while ! test -f release; do (( SECONDS < deadline )) || exit 124; sleep 0.05; done; printf done";

    #[tokio::test]
    async fn terminal_changes_survive_pruning_and_shutdown() {
        for status in [
            BashStatus::Completed,
            BashStatus::Failed,
            BashStatus::Stopped,
        ] {
            let mut executions = BashExecutions::default();
            let (reply, _result) = oneshot::channel();
            let mut reply = Some(reply);
            for index in 0..=RECENT_EXECUTION_LIMIT {
                let starting = index == RECENT_EXECUTION_LIMIT;
                let initial_status = if index == 0 || starting {
                    BashStatus::Running
                } else {
                    BashStatus::Completed
                };
                executions.executions.push_back(Execution {
                    view: BashExecution {
                        id: index.to_string(),
                        turn_id: "turn".into(),
                        tool_call_id: "call".into(),
                        command: "command".into(),
                        mode: BashMode::Background,
                        status: initial_status,
                        output_path: "/tmp/phi-terminal-change.log".into(),
                        exit_code: None,
                        error: None,
                    },
                    ready: !starting,
                    reply: if starting { reply.take() } else { None },
                    deadline: None,
                    wait_limit: None,
                    background_limit: None,
                    acknowledged: !starting,
                    observed_status: if starting { None } else { Some(initial_status) },
                    auto_background: true,
                    cancellation: watch::channel(None).0,
                    stop_reason: None,
                    worker: None,
                });
            }
            executions
                .sender
                .send(WorkerMessage::Started(RECENT_EXECUTION_LIMIT.to_string()))
                .unwrap();
            executions
                .sender
                .send(WorkerMessage::Finished {
                    id: "0".into(),
                    result: if status == BashStatus::Stopped {
                        Err("execution stopped".into())
                    } else {
                        Ok((
                            "result".into(),
                            ToolOutputDetails {
                                total_bytes: 6,
                                total_lines: 1,
                                truncated: false,
                                full_output_path: None,
                                exit_code: if status == BashStatus::Completed {
                                    0
                                } else {
                                    1
                                },
                            },
                        ))
                    },
                    stopped: (status == BashStatus::Stopped).then(|| "execution stopped".into()),
                })
                .unwrap();
            executions.shutdown().await;
            let changes = executions.take_changes();
            let terminal = changes
                .iter()
                .find(|execution| execution.id == "0")
                .expect("terminal execution must survive until its change is observed");
            assert_eq!(terminal.status, status);
            assert!(executions.take_changes().is_empty());
            assert!(executions.next_outcome().is_none());
            executions.poll();
            assert!(executions.snapshots().len() <= RECENT_EXECUTION_LIMIT);
        }
    }

    #[tokio::test]
    async fn handoff_keeps_process_output_and_delivers_one_outcome() {
        let mut fixture = Fixture::new();
        let mut output = fixture.executions.subscribe_output();
        let result = fixture.start(GATED_COMMAND, BashMode::Foreground);
        fixture
            .poll_until(|executions| !executions.snapshots().is_empty())
            .await;
        let initial = fixture.executions.snapshots().remove(0);
        tokio::time::timeout(Duration::from_secs(5), output.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            tokio::fs::read_to_string(&initial.output_path)
                .await
                .unwrap(),
            "ready"
        );
        let background = fixture.executions.background(&initial.id).unwrap();
        assert_eq!(background.id, initial.id);
        assert_eq!(background.output_path, initial.output_path);
        assert!(matches!(
            result.await.unwrap().unwrap(),
            BashReply::Background(_)
        ));
        fixture.executions.cancel_foreground();
        assert_eq!(
            fixture.executions.snapshots()[0].status,
            BashStatus::Running
        );
        std::fs::write(fixture.root.join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(5), output.changed())
            .await
            .unwrap()
            .unwrap();
        fixture
            .poll_until(|executions| executions.snapshots()[0].status != BashStatus::Running)
            .await;
        let outcome = fixture.executions.take_outcome().unwrap();
        assert_eq!(outcome.execution.status, BashStatus::Completed);
        assert_eq!(outcome.output, "readydone");
        assert_eq!(
            std::fs::read_to_string(background.output_path).unwrap(),
            "readydone"
        );
        assert!(fixture.executions.take_outcome().is_none());
        assert!(!fixture.executions.pending());
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn foreground_deadline_hands_off_except_sleep_commands() {
        for command in [GATED_COMMAND, "sleep 30"] {
            let mut fixture = Fixture::new();
            let result = fixture.start(command, BashMode::Foreground);
            fixture
                .poll_until(|executions| !executions.snapshots().is_empty())
                .await;
            fixture.executions.executions.front_mut().unwrap().deadline = Some(Instant::now());
            fixture.executions.poll();
            if command == GATED_COMMAND {
                assert!(matches!(
                    result.await.unwrap().unwrap(),
                    BashReply::Background(_)
                ));
            } else {
                fixture.poll_until(|executions| !executions.pending()).await;
                assert!(result.await.unwrap().unwrap_err().contains("timed out"));
            }
            fixture.shutdown().await;
            fixture.shutdown().await;
            assert!(!fixture.executions.pending());
        }
    }

    #[tokio::test]
    async fn foreground_completion_does_not_schedule_background_feedback() {
        let mut fixture = Fixture::new();
        let result = fixture.start("printf result; exit 7", BashMode::Foreground);
        fixture.poll_until(|executions| !executions.pending()).await;
        let BashReply::Finished(text, details) = result.await.unwrap().unwrap() else {
            panic!("expected final result");
        };
        assert!(text.contains("result"));
        assert_eq!(details.exit_code, 7);
        assert!(!details.truncated);
        let view = fixture.executions.snapshots().remove(0);
        assert_eq!(view.status, BashStatus::Failed);
        assert!(fixture.executions.background(&view.id).is_err());
        assert!(fixture.executions.take_outcome().is_none());
    }

    #[tokio::test]
    async fn dropped_foreground_waiter_stops_instead_of_backgrounding() {
        let mut fixture = Fixture::new();
        let result = fixture.start(GATED_COMMAND, BashMode::Foreground);
        fixture
            .poll_until(|executions| !executions.snapshots().is_empty())
            .await;
        drop(result);
        fixture.poll_until(|executions| !executions.pending()).await;
        assert_eq!(
            fixture.executions.snapshots()[0].status,
            BashStatus::Stopped
        );
        assert!(fixture.executions.take_outcome().is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_waits_for_the_owned_shell_to_exit() {
        let mut fixture = Fixture::new();
        let result = fixture.start("printf '%s' $$ > pid; sleep 5; :", BashMode::Background);
        fixture
            .poll_until(|executions| {
                executions
                    .executions
                    .front()
                    .is_some_and(|entry| entry.acknowledged)
            })
            .await;
        let BashReply::Background(view) = result.await.unwrap().unwrap() else {
            panic!("expected receipt");
        };
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(fixture.root.join("pid")).await
                    && let Ok(pid) = text.parse::<i32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        fixture.executions.stop(&view.id).unwrap();
        fixture
            .poll_until(|executions| executions.snapshots()[0].status == BashStatus::Stopped)
            .await;
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        fixture.shutdown().await;
    }

    #[tokio::test]
    async fn explicit_background_stop_and_shutdown_settle_workers() {
        let mut fixture = Fixture::new();
        let result = fixture.start(GATED_COMMAND, BashMode::Background);
        fixture
            .poll_until(|executions| {
                executions
                    .executions
                    .front()
                    .is_some_and(|entry| entry.acknowledged)
            })
            .await;
        let BashReply::Background(view) = result.await.unwrap().unwrap() else {
            panic!("expected receipt");
        };
        fixture.executions.stop(&view.id).unwrap();
        fixture
            .poll_until(|executions| executions.snapshots()[0].status == BashStatus::Stopped)
            .await;
        assert_eq!(
            fixture.executions.take_outcome().unwrap().execution.status,
            BashStatus::Stopped
        );
        fixture.executions.stop(&view.id).unwrap();
        let result = fixture.start(GATED_COMMAND, BashMode::Background);
        fixture
            .poll_until(|executions| {
                executions
                    .executions
                    .back()
                    .is_some_and(|entry| entry.acknowledged)
            })
            .await;
        assert!(matches!(
            result.await.unwrap().unwrap(),
            BashReply::Background(_)
        ));
        fixture.shutdown().await;
        assert!(!fixture.executions.pending());
        assert!(
            fixture
                .executions
                .snapshots()
                .iter()
                .all(|entry| entry.status == BashStatus::Stopped)
        );
    }
}
