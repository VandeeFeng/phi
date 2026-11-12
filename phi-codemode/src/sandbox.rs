use std::{
    collections::HashSet,
    panic::AssertUnwindSafe,
    sync::{Arc, Mutex},
    time::Instant,
};

use futures::FutureExt;
use tokio::{sync::Notify, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    declarations::is_identifier,
    runtime::{CallTarget, Compiled, Completion, Invocation, Metering, RunControl, RunInput},
    types::*,
};

struct Lifecycle {
    state: Mutex<LifecycleState>,
    cancellation: CancellationToken,
    changed: Notify,
}

#[derive(Default)]
struct LifecycleState {
    closed: bool,
    active: usize,
}

struct ExecutionLease(Arc<Lifecycle>);

struct CancelExecution {
    control: RunControl,
    compiled: Arc<Compiled>,
}

impl Drop for CancelExecution {
    fn drop(&mut self) {
        self.control.cancellation.cancel();
        if !self.control.finished.is_cancelled() {
            self.compiled.interrupt();
        }
    }
}

impl Drop for ExecutionLease {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().active -= 1;
        self.0.changed.notify_waiters();
    }
}

/// Independent sandbox; tool authorization and persistence belong to its caller.
pub struct CodemodeSandbox {
    compiled: Arc<Compiled>,
    options: SandboxOptions,
    lifecycle: Arc<Lifecycle>,
}

impl CodemodeSandbox {
    /// Shares compilation per metering mode and cache directory, off the Tokio executor.
    pub async fn new(options: SandboxOptions) -> Result<Self, SandboxBuildError> {
        validate_options(&options)?;
        let metering = match options.limits.fuel {
            Some(_) => Metering::Metered,
            None => Metering::Unmetered,
        };
        let cache_dir = options.cache_dir.clone();
        let compiled = tokio::task::spawn_blocking(move || Compiled::shared(metering, cache_dir))
            .await
            .map_err(|error| SandboxBuildError::Compilation(error.to_string()))?
            .map_err(SandboxBuildError::Compilation)?;
        Ok(Self {
            compiled,
            options,
            lifecycle: Arc::new(Lifecycle {
                state: Mutex::new(LifecycleState::default()),
                cancellation: CancellationToken::new(),
                changed: Notify::new(),
            }),
        })
    }

    pub fn declarations(&self) -> String {
        crate::render_declarations(&self.options.tools, &self.options.globals)
    }

    pub fn tools(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.options.tools.iter().map(CodemodeTool::definition)
    }

    pub fn globals(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.options.globals.iter().map(CodemodeTool::definition)
    }

    /// Script failures are returned as typed outcomes, including partial output.
    /// Dropping this future cancels execution and its nested tool calls.
    pub async fn execute(&self, code: &str, options: ExecuteOptions) -> ExecutionResult {
        if self.options.surface == SandboxSurface::HostOnly && !options.store.is_empty() {
            return ExecutionResult::failure(CodemodeError::new(
                ErrorKind::Sandbox,
                "host-only execution does not accept a store",
            ));
        }
        if code.len() > self.options.limits.max_source_bytes {
            return ExecutionResult::failure(CodemodeError::new(
                ErrorKind::Sandbox,
                "source budget exceeded",
            ));
        }
        let lease = {
            let mut state = self.lifecycle.state.lock().unwrap();
            if state.closed {
                return ExecutionResult::failure(CodemodeError::new(
                    ErrorKind::Aborted,
                    "sandbox is closed",
                ));
            }
            state.active += 1;
            ExecutionLease(self.lifecycle.clone())
        };
        let timeout = match options.timeout {
            ExecutionTimeout::Inherit => self.options.timeout,
            ExecutionTimeout::Unlimited => None,
            ExecutionTimeout::After(duration) => Some(duration),
        };
        let control = match RunControl::new(
            options.cancellation.child_token(),
            self.lifecycle.cancellation.clone(),
            timeout,
        ) {
            Ok(control) => control,
            Err(error) => return ExecutionResult::failure(error),
        };
        let cancel_on_drop = CancelExecution {
            control: control.clone(),
            compiled: self.compiled.clone(),
        };
        let compiled = self.compiled.clone();
        let tools = self.options.tools.clone();
        let globals = self.options.globals.clone();
        let limits = self.options.limits.clone();
        let surface = self.options.surface;
        let code = code.to_owned();
        // The supervisor outlives the caller future so close() can await actual cleanup.
        let task = tokio::spawn(async move {
            let _lease = lease;
            supervise(
                compiled,
                RunInput {
                    surface,
                    code,
                    saved: options.store,
                    tools,
                    globals,
                    limits,
                    control,
                },
            )
            .await
        });
        let result = match task.await {
            Ok(result) => result,
            Err(error) => ExecutionResult::failure(CodemodeError::new(
                ErrorKind::Sandbox,
                format!("execution supervisor failed: {error}"),
            )),
        };
        drop(cancel_on_drop);
        result
    }

    /// Rejects new executions and waits for workers and nested tool futures to stop.
    pub async fn close(&self) {
        self.lifecycle.state.lock().unwrap().closed = true;
        self.lifecycle.cancellation.cancel();
        loop {
            let notified = self.lifecycle.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.lifecycle.state.lock().unwrap().active == 0 {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for CodemodeSandbox {
    fn drop(&mut self) {
        self.lifecycle.cancellation.cancel();
    }
}

async fn supervise(compiled: Arc<Compiled>, input: RunInput) -> ExecutionResult {
    let control = input.control.clone();
    let tools = input.tools.clone();
    let globals = input.globals.clone();
    let capacity = input.limits.max_pending_calls;
    let (requests, mut receiver) = tokio::sync::mpsc::channel(capacity);
    // Each queued result still occupies a guest pending slot, bounding this channel.
    let (completions, completed) = std::sync::mpsc::channel();
    let mut completions = Some(completions);
    let watchdog_control = control.clone();
    let watchdog_compiled = compiled.clone();
    let watchdog = tokio::spawn(async move { watchdog_compiled.watch(watchdog_control).await });
    let mut worker = tokio::task::spawn_blocking(move || compiled.run(input, requests, completed));
    let mut tasks = JoinSet::new();
    let mut coordinator_error = None;
    let result = loop {
        tokio::select! {
            result = &mut worker => break result,
            _ = control.stopped(), if completions.is_some() => {
                // Disconnect also wakes an idle worker when the supervisor is dropped.
                completions.take();
            }
            request = receiver.recv() => {
                if let Some(request) = request {
                    let candidates = match request.target { CallTarget::Tool => &tools, CallTarget::Global => &globals };
                    let tool = candidates.iter().find(|tool| tool.definition().name == request.name).cloned();
                    tasks.spawn(invoke(tool, request, control.clone()));
                } else {
                    receiver.close();
                    // The worker owns the sender and is about to finish.
                    break worker.await;
                }
            }
            completion = tasks.join_next(), if !tasks.is_empty() => {
                match completion {
                    Some(Ok(completion)) => {
                        if let Some(completions) = &completions {
                            let _ = completions.send(completion);
                        }
                    }
                    Some(Err(error)) => {
                        coordinator_error = Some(error.to_string());
                        control.cancellation.cancel();
                    }
                    None => (),
                }
            }
        }
    };
    control.cancellation.cancel();
    control.finished.cancel();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let _ = watchdog.await;
    let mut result = result.unwrap_or_else(|error| {
        ExecutionResult::failure(CodemodeError::new(
            ErrorKind::Sandbox,
            format!("sandbox worker failed: {error}"),
        ))
    });
    if let Some(error) = coordinator_error {
        result.outcome = ExecutionOutcome::Failure(CodemodeError::new(ErrorKind::Sandbox, error));
    }
    result
}

async fn invoke(
    tool: Option<CodemodeTool>,
    request: Invocation,
    control: RunControl,
) -> Completion {
    let cancellation = control.cancellation.child_token();
    let result = match tool {
        Some(tool) => {
            let call = AssertUnwindSafe(async {
                tool.call(
                    request.args,
                    ToolContext {
                        cancellation: cancellation.clone(),
                    },
                )
                .await
            })
            .catch_unwind();
            tokio::select! {
                biased;
                _ = control.stopped() => { cancellation.cancel(); Err("tool call cancelled".into()) },
                result = call => result.unwrap_or_else(|_| Err("tool handler panicked".into())),
            }
        }
        None => Err(format!("unknown tool: {}", request.name)),
    };
    Completion {
        id: request.id,
        result,
    }
}

fn validate_options(options: &SandboxOptions) -> Result<(), SandboxBuildError> {
    let invalid = |message: &str| SandboxBuildError::InvalidOptions(message.to_owned());
    if options.surface == SandboxSurface::HostOnly && !options.tools.is_empty() {
        return Err(invalid("host-only sandboxes do not accept tools"));
    }
    let limits = &options.limits;
    if limits.heap_bytes == 0
        || limits.heap_bytes > i32::MAX as usize
        || limits.memory_bytes == 0
        || limits.memory_bytes > i32::MAX as usize
        || limits.max_pending_calls == 0
        || limits.max_pending_calls > limits.max_calls
        || limits.max_calls > u32::MAX as usize
        || limits.max_bridge_bytes == 0
        || limits.max_bridge_bytes > i32::MAX as usize
        || limits.max_source_bytes == 0
        || limits.max_source_bytes > limits.max_bridge_bytes
        || limits.max_output_chars > MAX_OUTPUT_CHARS
        || limits.max_output_items > MAX_OUTPUT_ITEMS
    {
        return Err(invalid("invalid memory, call, output or bridge limits"));
    }
    if options
        .timeout
        .is_some_and(|duration| Instant::now().checked_add(duration).is_none())
    {
        return Err(invalid("timeout is too large"));
    }
    let mut names = HashSet::new();
    for tool in &options.tools {
        if tool.definition().name.is_empty() || !names.insert(&tool.definition().name) {
            return Err(invalid("tool names must be non-empty and unique"));
        }
    }
    names.clear();
    let mut namespaces = HashSet::new();
    const RESERVED: &[&str] = &[
        "tools",
        "ALL_TOOLS",
        "console",
        "text",
        "image",
        "exit",
        "globalThis",
        "store",
        "load",
    ];
    for global in &options.globals {
        let name = &global.definition().name;
        let parts: Vec<_> = name.split('.').collect();
        if parts.len() > 2
            || !parts.iter().all(|part| is_identifier(part))
            || RESERVED.contains(&parts[0])
            || !names.insert(name)
        {
            return Err(invalid("invalid or duplicate global name"));
        }
        if parts.len() == 2 {
            namespaces.insert(parts[0]);
        }
    }
    if names.iter().any(|name| namespaces.contains(name.as_str())) {
        return Err(invalid("global conflicts with a namespace"));
    }
    Ok(())
}
