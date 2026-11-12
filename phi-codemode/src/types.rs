use std::{
    collections::BTreeMap, future::Future, path::PathBuf, pin::Pin, sync::Arc, time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub const MAX_STORE_VALUE_CHARS: usize = 256 * 1024;
pub const MAX_STORE_TOTAL_CHARS: usize = 1024 * 1024;
pub const MAX_OUTPUT_CHARS: usize = 16 * 1024 * 1024;
pub const MAX_OUTPUT_ITEMS: usize = 100_000;

pub type ToolResult = Result<Option<Value>, String>;
type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + Send>>;
type ToolHandler = dyn Fn(Option<Value>, ToolContext) -> ToolFuture + Send + Sync;

/// Schemas describe the model-facing API; the host tool validates actual arguments.
#[derive(Clone, Debug, Default)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
}

#[derive(Clone)]
pub struct ToolContext {
    pub cancellation: CancellationToken,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArgumentMode {
    #[default]
    First,
    Spread,
}

#[derive(Clone)]
pub struct CodemodeTool {
    definition: ToolDefinition,
    pub(crate) arguments: ArgumentMode,
    pub(crate) signature: Option<String>,
    handler: Arc<ToolHandler>,
}

impl CodemodeTool {
    pub fn new<F, Fut>(definition: ToolDefinition, handler: F) -> Self
    where
        F: Fn(Option<Value>, ToolContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ToolResult> + Send + 'static,
    {
        Self {
            definition,
            arguments: ArgumentMode::First,
            signature: None,
            handler: Arc::new(move |args, context| Box::pin(handler(args, context))),
        }
    }

    pub fn definition(&self) -> &ToolDefinition {
        &self.definition
    }

    /// Only globals use spread arguments and custom TypeScript signatures.
    pub fn with_arguments(mut self, arguments: ArgumentMode) -> Self {
        self.arguments = arguments;
        self
    }

    pub fn with_signature(mut self, signature: impl Into<String>) -> Self {
        self.signature = Some(signature.into());
        self
    }

    pub(crate) async fn call(&self, args: Option<Value>, context: ToolContext) -> ToolResult {
        (self.handler)(args, context).await
    }
}

#[derive(Clone, Debug)]
pub struct SandboxLimits {
    pub heap_bytes: usize,
    pub memory_bytes: usize,
    pub fuel: Option<u64>,
    pub max_calls: usize,
    pub max_pending_calls: usize,
    pub max_output_chars: usize,
    pub max_output_items: usize,
    pub max_bridge_bytes: usize,
    pub max_source_bytes: usize,
}

impl Default for SandboxLimits {
    fn default() -> Self {
        const MIB: usize = 1024 * 1024;
        Self {
            heap_bytes: 256 * MIB,
            memory_bytes: 320 * MIB,
            fuel: None,
            max_calls: 1000,
            max_pending_calls: 64,
            max_output_chars: MAX_OUTPUT_CHARS,
            max_output_items: MAX_OUTPUT_ITEMS,
            max_bridge_bytes: 32 * MIB,
            max_source_bytes: MIB,
        }
    }
}

/// HostOnly exposes injected globals without Codemode tools, output or store helpers.
/// It also rejects clock and random inputs not supplied explicitly by the host.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SandboxSurface {
    #[default]
    Codemode,
    HostOnly,
}

pub struct SandboxOptions {
    pub surface: SandboxSurface,
    /// Optional private directory for best-effort persistent compilation caching.
    pub cache_dir: Option<PathBuf>,
    pub tools: Vec<CodemodeTool>,
    pub globals: Vec<CodemodeTool>,
    pub timeout: Option<Duration>,
    pub limits: SandboxLimits,
}

impl Default for SandboxOptions {
    fn default() -> Self {
        Self {
            surface: SandboxSurface::Codemode,
            cache_dir: None,
            tools: Vec::new(),
            globals: Vec::new(),
            timeout: Some(Duration::from_secs(300)),
            limits: SandboxLimits::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub enum ExecutionTimeout {
    #[default]
    Inherit,
    Unlimited,
    After(Duration),
}

#[derive(Default)]
pub struct ExecuteOptions {
    pub cancellation: CancellationToken,
    pub timeout: ExecutionTimeout,
    pub store: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutputItem {
    Text { text: String, console: bool },
    Image { data: String, mime_type: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallStatus {
    Ok,
    Error,
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct CallRecord {
    pub name: String,
    pub status: CallStatus,
    pub duration: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Script,
    Timeout,
    Aborted,
    Sandbox,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodemodeError {
    pub kind: ErrorKind,
    pub name: Option<String>,
    pub message: String,
    pub stack: Option<String>,
}

impl CodemodeError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            name: None,
            message: message.into(),
            stack: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoreWrites {
    pub set: BTreeMap<String, Value>,
    pub delete: Vec<String>,
}

#[derive(Clone, Debug)]
pub enum ExecutionOutcome {
    Success {
        value: Option<Value>,
        store_writes: StoreWrites,
    },
    Failure(CodemodeError),
}

#[derive(Clone, Debug)]
pub struct ExecutionResult {
    pub outcome: ExecutionOutcome,
    pub output: Vec<OutputItem>,
    pub calls: Vec<CallRecord>,
}

impl ExecutionResult {
    pub(crate) fn failure(error: CodemodeError) -> Self {
        Self {
            outcome: ExecutionOutcome::Failure(error),
            output: Vec::new(),
            calls: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxBuildError {
    #[error("invalid codemode options: {0}")]
    InvalidOptions(String),
    #[error("could not compile codemode runtime: {0}")]
    Compilation(String),
}
