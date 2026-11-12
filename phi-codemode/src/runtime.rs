use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, mpsc},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wasmtime::{
    AsContextMut, Cache, CacheConfig, Caller, Config, Engine, Extern, Instance, InstancePre,
    Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder, TypedFunc,
};

use crate::{declarations::to_codemode_identifier, types::*};

const WASM: &[u8] = include_bytes!("../vendor/quickjs-wasi/quickjs.wasm");
const PRELUDE: &str = include_str!("prelude.js");
const BRIDGE_NAME: &str = "bridge";
const BRIDGE_FIELD_COUNT: usize = 4;
const MAX_STORE_DIFF_ENTRIES: usize = 10_000;
// Allow nested JSON escaping of a full store, but bound deletion-only diffs too.
const MAX_STORE_DIFF_BYTES: usize = 16 * MAX_STORE_TOTAL_CHARS;
// Leave native worker-stack headroom; QuickJS guards a separate linear-memory stack.
const WASM_STACK_BYTES: usize = 512 * 1024;
const JS_STACK_BYTES: i32 = 32 * 1024;
const INTERRUPT_INTERVAL: Duration = Duration::from_millis(1);
const WASI_SUCCESS: i32 = 0;
const WASI_BADF: i32 = 8;
const WASI_NOSYS: i32 = 52;

#[derive(Clone, Copy)]
pub(crate) enum CallTarget {
    Tool,
    Global,
}

pub(crate) struct Invocation {
    pub id: u64,
    pub target: CallTarget,
    pub name: String,
    pub args: Option<Value>,
}

pub(crate) struct RunInput {
    pub surface: SandboxSurface,
    pub code: String,
    pub saved: BTreeMap<String, Value>,
    pub tools: Vec<CodemodeTool>,
    pub globals: Vec<CodemodeTool>,
    pub limits: SandboxLimits,
    pub control: RunControl,
}

pub(crate) struct Completion {
    pub id: u64,
    pub result: ToolResult,
}

#[derive(Clone)]
pub(crate) struct RunControl {
    pub cancellation: CancellationToken,
    parent: CancellationToken,
    pub finished: CancellationToken,
    deadline: Option<Instant>,
}

impl RunControl {
    pub fn new(
        cancellation: CancellationToken,
        parent: CancellationToken,
        timeout: Option<Duration>,
    ) -> Result<Self, CodemodeError> {
        let deadline = timeout
            .map(|duration| {
                Instant::now()
                    .checked_add(duration)
                    .ok_or_else(|| CodemodeError::new(ErrorKind::Sandbox, "timeout is too large"))
            })
            .transpose()?;
        Ok(Self {
            cancellation,
            parent,
            finished: CancellationToken::new(),
            deadline,
        })
    }

    pub(crate) async fn stopped(&self) {
        let deadline = async {
            match self.deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = self.finished.cancelled() => (),
            _ = self.cancellation.cancelled() => (),
            _ = self.parent.cancelled() => (),
            _ = deadline => (),
        }
    }

    fn check(&self) -> Result<()> {
        match self.failure() {
            Some(error) => Err(GuestFailure(error).into()),
            None => Ok(()),
        }
    }

    fn failure(&self) -> Option<CodemodeError> {
        if self.cancellation.is_cancelled() || self.parent.is_cancelled() {
            Some(CodemodeError::new(ErrorKind::Aborted, "execution aborted"))
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some(CodemodeError::new(
                ErrorKind::Timeout,
                "execution timed out",
            ))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Metering {
    Unmetered,
    Metered,
}

pub(crate) struct Compiled {
    engine: Engine,
    instance: InstancePre<HostState>,
}

type Compilation = OnceLock<Result<Arc<Compiled>, String>>;
type CompilationKey = (Metering, Option<PathBuf>);

impl Compiled {
    pub(crate) fn shared(
        metering: Metering,
        cache_dir: Option<PathBuf>,
    ) -> Result<Arc<Self>, String> {
        static COMPILED: OnceLock<Mutex<HashMap<CompilationKey, Arc<Compilation>>>> =
            OnceLock::new();
        let compiled = COMPILED
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .entry((metering, cache_dir.clone()))
            .or_default()
            .clone();
        compiled
            .get_or_init(|| {
                Self::compile(metering, cache_dir.as_deref())
                    .map(Arc::new)
                    .map_err(|error| format!("{error:#}"))
            })
            .clone()
    }

    fn compile(metering: Metering, cache_dir: Option<&Path>) -> Result<Self> {
        let mut config = Config::new();
        config
            .consume_fuel(matches!(metering, Metering::Metered))
            .epoch_interruption(true)
            .max_wasm_stack(WASM_STACK_BYTES)
            .parallel_compilation(true);
        if let Some(directory) = cache_dir {
            match compilation_cache(directory) {
                Ok(cache) => {
                    config.cache(Some(cache));
                }
                Err(error) => tracing::warn!(
                    error = %error,
                    "codemode compilation cache unavailable; compiling without disk cache"
                ),
            }
        }
        let engine = Engine::new(&config)?;
        let module = Module::new(&engine, WASM)?;
        let mut linker = Linker::new(&engine);
        link_imports(&mut linker)?;
        let instance = linker.instantiate_pre(&module)?;
        Ok(Self { engine, instance })
    }

    pub(crate) fn interrupt(&self) {
        self.engine.increment_epoch();
    }

    pub(crate) async fn watch(&self, control: RunControl) {
        control.stopped().await;
        if control.finished.is_cancelled() {
            return;
        }
        // Keep ticking until cleanup; a single tick can precede Store initialization.
        loop {
            self.interrupt();
            tokio::select! {
                _ = control.finished.cancelled() => return,
                _ = tokio::time::sleep(INTERRUPT_INTERVAL) => (),
            }
        }
    }

    pub(crate) fn run(
        &self,
        mut input: RunInput,
        requests: tokio::sync::mpsc::Sender<Invocation>,
        completed: mpsc::Receiver<Completion>,
    ) -> ExecutionResult {
        let control = input.control.clone();
        let _finish = control.finished.clone().drop_guard();
        let code = std::mem::take(&mut input.code);
        let result = self.prepare(input, requests);
        let mut guest = match result {
            Ok(guest) => guest,
            Err(error) => return ExecutionResult::failure(classify(error, &control)),
        };
        let execution = guest.drive(&code, completed);
        let mut outcome = match guest.store.data_mut().outcome.take() {
            Some(outcome) => outcome,
            None => match execution {
                Ok(outcome) => outcome,
                Err(error) => ExecutionOutcome::Failure(classify(error, &control)),
            },
        };
        if matches!(outcome, ExecutionOutcome::Success { .. })
            && let Some(error) = control.failure()
        {
            outcome = ExecutionOutcome::Failure(error);
        }
        let state = guest.store.data_mut();
        for pending in state.pending.values() {
            if let Some(index) = pending.record {
                state.calls[index].duration = pending.started.elapsed();
            }
        }
        ExecutionResult {
            outcome,
            output: std::mem::take(&mut state.output),
            calls: std::mem::take(&mut state.calls),
        }
    }

    fn prepare(
        &self,
        input: RunInput,
        requests: tokio::sync::mpsc::Sender<Invocation>,
    ) -> Result<Guest> {
        let RunInput {
            surface,
            saved,
            tools,
            globals,
            limits,
            control,
            ..
        } = input;
        control.check()?;
        let saved_json = serialize_store(&saved, &control)?;
        let tool_json: Vec<_> = tools.iter().map(|tool| {
            let definition = tool.definition();
            json!({"name":definition.name,"jsName":to_codemode_identifier(&definition.name),"description":definition.description})
        }).collect();
        let global_json: Vec<_> = globals.iter().map(|tool| json!({"name":tool.definition().name,"spread":tool.arguments == ArgumentMode::Spread})).collect();
        let mut store = Store::new(
            &self.engine,
            HostState {
                store_limits: StoreLimitsBuilder::new()
                    .memory_size(limits.memory_bytes)
                    .instances(1)
                    .memories(1)
                    .build(),
                limits,
                control: control.clone(),
                requests,
                tools: tools
                    .iter()
                    .map(|tool| tool.definition().name.clone())
                    .collect(),
                globals: globals
                    .iter()
                    .map(|tool| tool.definition().name.clone())
                    .collect(),
                saved,
                pending: HashMap::new(),
                calls: Vec::new(),
                call_count: 0,
                output: Vec::new(),
                output_chars: 0,
                outcome: None,
                started: Instant::now(),
                exports: None,
            },
        );
        store.limiter(|state| &mut state.store_limits);
        if let Some(fuel) = store.data().limits.fuel {
            store.set_fuel(fuel)?;
        }
        store.set_epoch_deadline(1);
        store.epoch_deadline_callback(move |_| {
            if let Some(error) = control.failure() {
                return Err(wasmtime::Error::msg(error.message));
            }
            Ok(wasmtime::UpdateDeadline::Continue(1))
        });
        let instance = self.instance.instantiate(&mut store)?;
        let exports = Arc::new(GuestExports::new(&instance, &mut store)?);
        store.data_mut().exports = Some(exports.clone());
        let mut guest = Guest {
            store,
            instance,
            exports,
            undefined: 0,
            true_value: 0,
            false_value: 0,
            api: 0,
            run: 0,
            settle: 0,
            stalled: 0,
            handles: Vec::new(),
        };
        guest.initialize(
            &serde_json::to_string(&tool_json)?,
            &serde_json::to_string(&global_json)?,
            &saved_json,
            surface,
        )?;
        Ok(guest)
    }
}

fn compilation_cache(directory: &Path) -> Result<Cache> {
    ensure!(directory.is_absolute(), "cache directory must be absolute");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(directory)
        .context("create compilation cache directory")?;
    let metadata = std::fs::symlink_metadata(directory)?;
    ensure!(metadata.is_dir(), "cache directory must not be a symlink");
    let directory = directory.canonicalize()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let owner = rustix::process::geteuid().as_raw();
        ensure!(
            metadata.uid() == owner,
            "cache directory belongs to another user"
        );
        ensure!(
            metadata.mode() & 0o077 == 0,
            "cache directory must be private (0700)"
        );
        // Native code is trusted on load; writable ancestors can replace a private directory.
        for ancestor in directory.ancestors().skip(1) {
            let metadata = ancestor.metadata()?;
            ensure!(
                metadata.uid() == owner || metadata.uid() == 0,
                "cache ancestor belongs to another user"
            );
            ensure!(
                metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0,
                "cache ancestor is writable by other users"
            );
        }
    }
    let mut config = CacheConfig::new();
    config.with_directory(directory);
    Ok(Cache::new(config)?)
}

#[derive(Debug, thiserror::Error)]
#[error("{}", .0.message)]
struct GuestFailure(CodemodeError);

fn classify(error: anyhow::Error, control: &RunControl) -> CodemodeError {
    if let Some(error) = control.failure() {
        return error;
    }
    if let Some(error) = error.downcast_ref::<GuestFailure>() {
        return error.0.clone();
    }
    CodemodeError::new(ErrorKind::Sandbox, error.root_cause().to_string())
}

struct PendingCall {
    record: Option<usize>,
    started: Instant,
}

struct HostState {
    store_limits: StoreLimits,
    limits: SandboxLimits,
    control: RunControl,
    requests: tokio::sync::mpsc::Sender<Invocation>,
    tools: Vec<String>,
    globals: Vec<String>,
    saved: BTreeMap<String, Value>,
    pending: HashMap<u64, PendingCall>,
    calls: Vec<CallRecord>,
    call_count: usize,
    output: Vec<OutputItem>,
    output_chars: usize,
    outcome: Option<ExecutionOutcome>,
    started: Instant,
    exports: Option<Arc<GuestExports>>,
}

impl HostState {
    fn event(&mut self, event: [Value; BRIDGE_FIELD_COUNT]) -> Result<()> {
        self.control.check()?;
        ensure!(
            self.outcome.is_none(),
            "bridge call after execution finished"
        );
        match event[0].as_str() {
            Some("call" | "global") => {
                let target = if event[0] == "call" {
                    CallTarget::Tool
                } else {
                    CallTarget::Global
                };
                let id = event[1].as_u64().context("invalid call id")?;
                let name = event[2].as_str().context("invalid tool name")?;
                let names = match target {
                    CallTarget::Tool => &self.tools,
                    CallTarget::Global => &self.globals,
                };
                ensure!(
                    names.iter().any(|candidate| candidate == name),
                    "unknown tool"
                );
                ensure!(!self.pending.contains_key(&id), "duplicate call id");
                ensure!(
                    self.pending.len() < self.limits.max_pending_calls,
                    "pending call budget exceeded"
                );
                ensure!(
                    self.call_count < self.limits.max_calls,
                    "tool call budget exceeded"
                );
                self.call_count += 1;
                let args = parse_payload(&event[3])?;
                let record = match target {
                    CallTarget::Tool => {
                        let index = self.calls.len();
                        self.calls.push(CallRecord {
                            name: name.to_owned(),
                            status: CallStatus::Cancelled,
                            duration: Duration::ZERO,
                        });
                        Some(index)
                    }
                    CallTarget::Global => None,
                };
                self.pending.insert(
                    id,
                    PendingCall {
                        record,
                        started: Instant::now(),
                    },
                );
                self.requests
                    .blocking_send(Invocation {
                        id,
                        target,
                        name: name.to_owned(),
                        args,
                    })
                    .map_err(|_| anyhow::anyhow!("execution coordinator closed"))?;
            }
            Some("output") => {
                let data = event[2].as_str().context("invalid output data")?;
                self.output_chars = self
                    .output_chars
                    .checked_add(data.encode_utf16().count())
                    .context("output size overflow")?;
                ensure!(
                    self.output_chars <= self.limits.max_output_chars
                        && self.output.len() < self.limits.max_output_items,
                    "host output budget exceeded"
                );
                let output =
                    match event[1].as_str() {
                        Some("text" | "console") => OutputItem::Text {
                            text: data.to_owned(),
                            console: event[1] == "console",
                        },
                        Some("image") => {
                            let mime_type = event[3].as_str().context("missing image mime type")?;
                            ensure!(
                                ["image/png", "image/jpeg", "image/gif", "image/webp"]
                                    .contains(&mime_type),
                                "unsupported image type"
                            );
                            ensure!(
                                !data.is_empty()
                                    && data.len().is_multiple_of(4)
                                    && data.bytes().all(|byte| byte.is_ascii_alphanumeric()
                                        || b"+/=".contains(&byte)),
                                "invalid image encoding"
                            );
                            OutputItem::Image {
                                data: data.to_owned(),
                                mime_type: mime_type.to_owned(),
                            }
                        }
                        _ => bail!("invalid output kind"),
                    };
                self.output.push(output);
            }
            Some("done") => {
                let ok = event[1].as_bool().context("invalid completion flag")?;
                self.outcome = Some(if ok {
                    let value = parse_payload(&event[2])?;
                    let store_writes = parse_writes(
                        event[3].as_str().context("missing store writes")?,
                        &self.saved,
                        &self.control,
                    )?;
                    ExecutionOutcome::Success {
                        value,
                        store_writes,
                    }
                } else {
                    let error = parse_payload(&event[2])?.context("missing script error")?;
                    let message = error["message"]
                        .as_str()
                        .context("invalid script error message")?
                        .to_owned();
                    let name = optional_string(&error["name"])?;
                    let stack = optional_string(&error["stack"])?;
                    ExecutionOutcome::Failure(CodemodeError {
                        kind: ErrorKind::Script,
                        name,
                        message,
                        stack,
                    })
                });
            }
            _ => bail!("unknown bridge event"),
        }
        Ok(())
    }
}

fn optional_string(value: &Value) -> Result<Option<String>> {
    if value.is_null() {
        Ok(None)
    } else {
        Ok(Some(value.as_str().context("expected string")?.to_owned()))
    }
}

fn parse_payload(value: &Value) -> Result<Option<Value>> {
    match value {
        Value::Null => Ok(None),
        Value::String(payload) => Ok(Some(serde_json::from_str(payload)?)),
        _ => bail!("payload must be a JSON string or null"),
    }
}

fn serialize_store_value(key: &str, value: &Value, total: &mut usize) -> Result<String> {
    let json = serde_json::to_string(value)?;
    let chars = json.encode_utf16().count();
    ensure!(
        chars <= MAX_STORE_VALUE_CHARS,
        "store value budget exceeded"
    );
    *total = total
        .checked_add(key.encode_utf16().count() + chars)
        .context("store size overflow")?;
    ensure!(*total <= MAX_STORE_TOTAL_CHARS, "store budget exceeded");
    Ok(json)
}

fn serialize_store(saved: &BTreeMap<String, Value>, control: &RunControl) -> Result<String> {
    let mut total = 0;
    let mut serialized = BTreeMap::new();
    for (key, value) in saved {
        control.check()?;
        serialized.insert(key, serialize_store_value(key, value, &mut total)?);
    }
    Ok(serde_json::to_string(&serialized)?)
}

fn parse_writes(
    payload: &str,
    saved: &BTreeMap<String, Value>,
    control: &RunControl,
) -> Result<StoreWrites> {
    control.check()?;
    ensure!(
        payload.len() <= MAX_STORE_DIFF_BYTES,
        "store diff byte budget exceeded"
    );
    struct DiffEntries<'a>(&'a RunControl);

    impl<'de> serde::de::Visitor<'de> for DiffEntries<'_> {
        type Value = Vec<Vec<Value>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a bounded array of store entries")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            let mut entries = Vec::new();
            loop {
                self.0.check().map_err(A::Error::custom)?;
                let Some(entry) = sequence.next_element::<Vec<Value>>()? else {
                    return Ok(entries);
                };
                if entries.len() >= MAX_STORE_DIFF_ENTRIES {
                    return Err(A::Error::custom("store diff entry budget exceeded"));
                }
                entries.push(entry);
            }
        }
    }

    let mut deserializer = serde_json::Deserializer::from_str(payload);
    let entries =
        serde::de::Deserializer::deserialize_seq(&mut deserializer, DiffEntries(control))?;
    deserializer.end()?;
    let mut result = StoreWrites::default();
    let mut keys = HashSet::new();
    for entry in entries {
        control.check()?;
        ensure!(matches!(entry.len(), 1 | 2), "invalid store entry");
        let key = entry[0].as_str().context("invalid store key")?.to_owned();
        ensure!(keys.insert(key.clone()), "duplicate store key");
        if entry.len() == 1 {
            result.delete.push(key);
        } else {
            let value: Value =
                serde_json::from_str(entry[1].as_str().context("invalid store value")?)?;
            result.set.insert(key, value);
        }
    }
    let mut total = 0;
    for (key, value) in saved
        .iter()
        .filter(|(key, _)| !keys.contains(*key))
        .chain(result.set.iter())
    {
        control.check()?;
        serialize_store_value(key, value, &mut total)?;
    }
    control.check()?;
    Ok(result)
}

struct GuestExports {
    memory: Memory,
    allocate: TypedFunc<i32, i32>,
    free: TypedFunc<i32, ()>,
    new_string: TypedFunc<(i32, i32), i32>,
    new_number: TypedFunc<f64, i32>,
    eval: TypedFunc<(i32, i32, i32, i32), i32>,
    call: TypedFunc<(i32, i32, i32, i32), i32>,
    property: TypedFunc<(i32, i32), i32>,
    is_exception: TypedFunc<i32, i32>,
    exception: TypedFunc<(), i32>,
    is_undefined: TypedFunc<i32, i32>,
    is_null: TypedFunc<i32, i32>,
    is_string: TypedFunc<i32, i32>,
    is_number: TypedFunc<i32, i32>,
    is_bool: TypedFunc<i32, i32>,
    number: TypedFunc<i32, f64>,
    boolean: TypedFunc<i32, i32>,
    undefined: TypedFunc<(), i32>,
    string: TypedFunc<(i32, i32), i32>,
    free_string: TypedFunc<i32, ()>,
    free_value: TypedFunc<i32, ()>,
    pending_job: TypedFunc<(), i32>,
}

impl GuestExports {
    fn new(instance: &Instance, store: &mut Store<HostState>) -> Result<Self> {
        Ok(Self {
            memory: instance
                .get_memory(&mut *store, "memory")
                .context("missing guest memory")?,
            allocate: instance.get_typed_func(&mut *store, "wasm_malloc")?,
            free: instance.get_typed_func(&mut *store, "wasm_free")?,
            new_string: instance.get_typed_func(&mut *store, "qjs_new_string")?,
            new_number: instance.get_typed_func(&mut *store, "qjs_new_number")?,
            eval: instance.get_typed_func(&mut *store, "qjs_eval")?,
            call: instance.get_typed_func(&mut *store, "qjs_call")?,
            property: instance.get_typed_func(&mut *store, "qjs_get_prop_string")?,
            is_exception: instance.get_typed_func(&mut *store, "qjs_is_exception")?,
            exception: instance.get_typed_func(&mut *store, "qjs_get_exception")?,
            is_undefined: instance.get_typed_func(&mut *store, "qjs_is_undefined")?,
            is_null: instance.get_typed_func(&mut *store, "qjs_is_null")?,
            is_string: instance.get_typed_func(&mut *store, "qjs_is_string")?,
            is_number: instance.get_typed_func(&mut *store, "qjs_is_number")?,
            is_bool: instance.get_typed_func(&mut *store, "qjs_is_bool")?,
            number: instance.get_typed_func(&mut *store, "qjs_get_float64")?,
            boolean: instance.get_typed_func(&mut *store, "qjs_get_bool")?,
            undefined: instance.get_typed_func(&mut *store, "qjs_get_undefined")?,
            string: instance.get_typed_func(&mut *store, "qjs_get_string_len")?,
            free_string: instance.get_typed_func(&mut *store, "qjs_free_cstring")?,
            free_value: instance.get_typed_func(&mut *store, "qjs_free_value")?,
            pending_job: instance.get_typed_func(&mut *store, "qjs_execute_pending_job")?,
        })
    }

    fn read_string(
        &self,
        mut store: impl AsContextMut<Data = HostState>,
        handle: i32,
        max_bytes: usize,
    ) -> Result<String> {
        let length_pointer = self.allocate.call(&mut store, 4)?;
        ensure!(length_pointer != 0, "guest allocation failed");
        let pointer = self.string.call(&mut store, (handle, length_pointer))?;
        ensure!(pointer != 0, "guest string conversion failed");
        let result = (|| {
            let mut bytes = [0u8; 4];
            self.memory
                .read(&store, length_pointer as usize, &mut bytes)?;
            let length = u32::from_le_bytes(bytes) as usize;
            ensure!(length <= max_bytes, "string bridge budget exceeded");
            let mut bytes = vec![0; length];
            self.memory.read(&store, pointer as usize, &mut bytes)?;
            Ok(String::from_utf8(bytes)?)
        })();
        self.free_string.call(&mut store, pointer)?;
        self.free.call(&mut store, length_pointer)?;
        result
    }

    fn bridge_value(
        &self,
        caller: &mut Caller<'_, HostState>,
        handle: i32,
        remaining: &mut usize,
    ) -> Result<Value> {
        if self.is_string.call(&mut *caller, handle)? != 0 {
            let text = self.read_string(&mut *caller, handle, *remaining)?;
            *remaining -= text.len();
            Ok(Value::String(text))
        } else if self.is_number.call(&mut *caller, handle)? != 0 {
            let number = self.number.call(&mut *caller, handle)?;
            ensure!(
                (1.0..=f64::from(u32::MAX)).contains(&number) && number.fract() == 0.0,
                "invalid call id"
            );
            Ok(Value::from(number as u64))
        } else if self.is_bool.call(&mut *caller, handle)? != 0 {
            Ok(Value::Bool(self.boolean.call(&mut *caller, handle)? != 0))
        } else if self.is_undefined.call(&mut *caller, handle)? != 0
            || self.is_null.call(&mut *caller, handle)? != 0
        {
            Ok(Value::Null)
        } else {
            bail!("bridge fields must be primitives")
        }
    }
}

struct Guest {
    store: Store<HostState>,
    instance: Instance,
    exports: Arc<GuestExports>,
    undefined: i32,
    true_value: i32,
    false_value: i32,
    api: i32,
    run: i32,
    settle: i32,
    stalled: i32,
    handles: Vec<i32>,
}

impl Guest {
    fn initialize(
        &mut self,
        tools: &str,
        globals: &str,
        saved: &str,
        surface: SandboxSurface,
    ) -> Result<()> {
        self.instance
            .get_typed_func::<(), ()>(&mut self.store, "_initialize")?
            .call(&mut self.store, ())?;
        let initialized = self.zero("qjs_init")?;
        ensure!(initialized == 0, "QuickJS initialization failed");
        self.undefined = self.owned_zero("qjs_get_undefined")?;
        self.true_value = self.owned_zero("qjs_get_true")?;
        self.false_value = self.owned_zero("qjs_get_false")?;
        self.void(
            "qjs_set_memory_limit",
            self.store.data().limits.heap_bytes as i32,
        )?;
        self.void("qjs_set_max_stack_size", JS_STACK_BYTES)?;
        self.void("qjs_set_interrupt_handler", 1)?;
        let name = self.allocate(BRIDGE_NAME)?;
        let bridge = self
            .instance
            .get_typed_func::<(i32, i32, i32), i32>(&mut self.store, "qjs_new_host_function")?
            .call(
                &mut self.store,
                (name, BRIDGE_NAME.len() as i32, BRIDGE_FIELD_COUNT as i32),
            )?;
        self.exports.free.call(&mut self.store, name)?;
        self.handles.push(bridge);
        let prelude = self.eval_named(PRELUDE, "codemode-prelude.js")?;
        let tools = self.new_string(tools)?;
        let globals = self.new_string(globals)?;
        let saved = self.new_string(saved)?;
        let limits = self.new_string(
            &json!({
                "hostOnly": surface == SandboxSurface::HostOnly,
                "storeValueChars": MAX_STORE_VALUE_CHARS,
                "storeTotalChars": MAX_STORE_TOTAL_CHARS,
                "outputChars": MAX_OUTPUT_CHARS,
                "outputItems": MAX_OUTPUT_ITEMS,
            })
            .to_string(),
        )?;
        self.api = self.call(prelude, 0, &[bridge, tools, globals, saved, limits])?;
        self.settle = self.property(self.api, "settle")?;
        self.stalled = self.property(self.api, "stalled")?;
        self.run = self.property(self.api, "run")?;
        Ok(())
    }

    fn drive(
        &mut self,
        code: &str,
        completed: mpsc::Receiver<Completion>,
    ) -> Result<ExecutionOutcome> {
        self.store.data().control.check()?;
        self.scoped(|guest| {
            let script = guest.eval_named(
                &format!("(async (tools, console) => {{{code}\n}})"),
                "codemode.js",
            )?;
            guest.call(guest.run, guest.api, &[script])?;
            Ok(())
        })?;
        loop {
            self.store.data().control.check()?;
            self.drain()?;
            if self.store.data().outcome.is_none() {
                self.scoped(|guest| {
                    guest.call(guest.stalled, guest.api, &[])?;
                    Ok(())
                })?;
            }
            if let Some(outcome) = self.store.data_mut().outcome.take() {
                return Ok(outcome);
            }
            let completion = completed.recv().context("execution coordinator closed")?;
            self.complete(completion)?;
        }
    }

    fn complete(&mut self, completion: Completion) -> Result<()> {
        let pending = self
            .store
            .data_mut()
            .pending
            .remove(&completion.id)
            .context("unknown completion id")?;
        let ok = completion.result.is_ok();
        if let Some(index) = pending.record {
            let record = &mut self.store.data_mut().calls[index];
            record.status = if ok {
                CallStatus::Ok
            } else {
                CallStatus::Error
            };
            record.duration = pending.started.elapsed();
        }
        let payload = match completion.result {
            Ok(value) => value
                .map(|value| serde_json::to_string(&value))
                .transpose()?,
            Err(error) => Some(error),
        };
        ensure!(
            payload
                .as_ref()
                .is_none_or(|payload| payload.len() <= self.store.data().limits.max_bridge_bytes),
            "tool result bridge budget exceeded"
        );
        self.scoped(|guest| {
            let id = guest
                .exports
                .new_number
                .call(&mut guest.store, completion.id as f64)?;
            let id = guest.check(id)?;
            let ok = if ok {
                guest.true_value
            } else {
                guest.false_value
            };
            let payload = match payload {
                Some(payload) => guest.new_string(&payload)?,
                None => guest.undefined,
            };
            guest.call(guest.settle, guest.api, &[id, ok, payload])?;
            Ok(())
        })
    }

    fn zero(&mut self, name: &str) -> Result<i32> {
        Ok(self
            .instance
            .get_typed_func::<(), i32>(&mut self.store, name)?
            .call(&mut self.store, ())?)
    }

    fn owned_zero(&mut self, name: &str) -> Result<i32> {
        let handle = self.zero(name)?;
        ensure!(handle != 0, "guest allocation failed");
        self.handles.push(handle);
        Ok(handle)
    }

    fn void(&mut self, name: &str, argument: i32) -> Result<()> {
        self.instance
            .get_typed_func::<i32, ()>(&mut self.store, name)?
            .call(&mut self.store, argument)?;
        Ok(())
    }

    fn allocate(&mut self, text: &str) -> Result<i32> {
        ensure!(
            text.len() <= self.store.data().limits.max_bridge_bytes,
            "guest input budget exceeded"
        );
        let pointer = self
            .exports
            .allocate
            .call(&mut self.store, i32::try_from(text.len() + 1)?)?;
        ensure!(pointer != 0, "guest allocation failed");
        let memory = self.exports.memory;
        memory.write(&mut self.store, pointer as usize, text.as_bytes())?;
        memory.write(&mut self.store, pointer as usize + text.len(), &[0])?;
        Ok(pointer)
    }

    fn new_string(&mut self, text: &str) -> Result<i32> {
        let pointer = self.allocate(text)?;
        let handle = self
            .exports
            .new_string
            .call(&mut self.store, (pointer, i32::try_from(text.len())?))?;
        self.exports.free.call(&mut self.store, pointer)?;
        self.check(handle)
    }

    fn eval_named(&mut self, source: &str, filename: &str) -> Result<i32> {
        let pointer = self.allocate(source)?;
        let filename = self.allocate(filename)?;
        let handle = self.exports.eval.call(
            &mut self.store,
            (pointer, i32::try_from(source.len())?, filename, 0),
        )?;
        self.exports.free.call(&mut self.store, pointer)?;
        self.exports.free.call(&mut self.store, filename)?;
        self.check(handle)
    }

    fn check(&mut self, handle: i32) -> Result<i32> {
        ensure!(handle != 0, "guest allocation failed");
        self.handles.push(handle);
        if self.exports.is_exception.call(&mut self.store, handle)? != 0 {
            let exception = self.exports.exception.call(&mut self.store, ())?;
            ensure!(exception != 0, "guest allocation failed");
            self.handles.push(exception);
            let mut fields = Vec::new();
            for name in ["name", "message", "stack"] {
                let field = self.property(exception, name)?;
                let value = if self.exports.is_undefined.call(&mut self.store, field)? != 0 {
                    None
                } else {
                    Some(self.read_string(field)?)
                };
                fields.push(value);
            }
            return Err(GuestFailure(CodemodeError {
                kind: ErrorKind::Script,
                name: fields[0].clone(),
                message: fields[1].clone().unwrap_or_else(|| "script failed".into()),
                stack: fields[2].clone(),
            })
            .into());
        }
        Ok(handle)
    }

    fn property(&mut self, handle: i32, name: &str) -> Result<i32> {
        let pointer = self.allocate(name)?;
        let result = self
            .exports
            .property
            .call(&mut self.store, (handle, pointer))?;
        self.exports.free.call(&mut self.store, pointer)?;
        self.check(result)
    }

    fn call(&mut self, function: i32, receiver: i32, arguments: &[i32]) -> Result<i32> {
        let receiver = if receiver == 0 {
            self.undefined
        } else {
            receiver
        };
        let size = arguments
            .len()
            .checked_mul(4)
            .context("argument size overflow")?;
        let pointer = self
            .exports
            .allocate
            .call(&mut self.store, i32::try_from(size.max(1))?)?;
        ensure!(pointer != 0, "argument allocation failed");
        let bytes: Vec<_> = arguments
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        self.exports
            .memory
            .write(&mut self.store, pointer as usize, &bytes)?;
        let handle = self.exports.call.call(
            &mut self.store,
            (function, receiver, i32::try_from(arguments.len())?, pointer),
        )?;
        self.exports.free.call(&mut self.store, pointer)?;
        self.check(handle)
    }

    fn scoped<T>(&mut self, action: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let start = self.handles.len();
        let result = action(self)?;
        while self.handles.len() > start {
            let handle = self.handles.pop().unwrap();
            self.exports.free_value.call(&mut self.store, handle)?;
        }
        Ok(result)
    }

    fn read_string(&mut self, handle: i32) -> Result<String> {
        let max_bytes = self.store.data().limits.max_bridge_bytes;
        self.exports.read_string(&mut self.store, handle, max_bytes)
    }

    fn drain(&mut self) -> Result<()> {
        loop {
            if self.store.data().outcome.is_some() {
                return Ok(());
            }
            let status = self.exports.pending_job.call(&mut self.store, ())?;
            ensure!(status >= 0, "QuickJS pending job failed");
            if status == 0 {
                return Ok(());
            }
        }
    }
}

fn guest_memory(caller: &mut Caller<'_, HostState>) -> Result<Memory> {
    caller
        .get_export("memory")
        .and_then(Extern::into_memory)
        .context("missing guest memory")
}

fn read_bytes(caller: &mut Caller<'_, HostState>, pointer: i32, length: usize) -> Result<Vec<u8>> {
    ensure!(
        length <= caller.data().limits.max_bridge_bytes,
        "host bridge budget exceeded"
    );
    let memory = guest_memory(caller)?;
    let start = pointer as u32 as usize;
    let end = start
        .checked_add(length)
        .context("guest pointer overflow")?;
    let slice = memory
        .data(&*caller)
        .get(start..end)
        .context("invalid guest memory range")?;
    Ok(slice.to_vec())
}

fn read_u32(caller: &mut Caller<'_, HostState>, pointer: i32) -> Result<u32> {
    ensure!(
        size_of::<u32>() <= caller.data().limits.max_bridge_bytes,
        "host bridge budget exceeded"
    );
    let mut bytes = [0; size_of::<u32>()];
    guest_memory(caller)?.read(&*caller, pointer as u32 as usize, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn bridge(
    mut caller: Caller<'_, HostState>,
    name_pointer: i32,
    name_length: i32,
    _receiver: i32,
    count: i32,
    arguments: i32,
) -> Result<i32> {
    ensure!(count == BRIDGE_FIELD_COUNT as i32, "malformed bridge event");
    ensure!(
        read_bytes(&mut caller, name_pointer, name_length as u32 as usize)?
            == BRIDGE_NAME.as_bytes(),
        "unknown host callback"
    );
    let exports = caller
        .data()
        .exports
        .clone()
        .context("guest exports unavailable")?;
    let mut remaining = caller.data().limits.max_bridge_bytes;
    let mut handles = [0u8; BRIDGE_FIELD_COUNT * size_of::<i32>()];
    exports
        .memory
        .read(&caller, arguments as u32 as usize, &mut handles)?;
    let mut event = [const { Value::Null }; BRIDGE_FIELD_COUNT];
    for (field, &bytes) in event
        .iter_mut()
        .zip(handles.as_chunks::<{ size_of::<i32>() }>().0)
    {
        let handle = i32::from_le_bytes(bytes);
        *field = exports.bridge_value(&mut caller, handle, &mut remaining)?;
    }
    caller.data_mut().event(event)?;
    // A host trap prevents caught exit() or output-limit errors from continuing the script.
    ensure!(
        caller.data().outcome.is_none(),
        "codemode execution completed"
    );
    // The C trampoline owns and frees the returned handle.
    let handle = exports.undefined.call(&mut caller, ())?;
    ensure!(handle != 0, "guest allocation failed");
    Ok(handle)
}

fn wasm_error(error: anyhow::Error) -> wasmtime::Error {
    wasmtime::Error::msg(format!("{error:#}"))
}

fn link_imports(linker: &mut Linker<HostState>) -> Result<()> {
    linker.func_wrap(
        "env",
        "host_call",
        |caller: Caller<'_, HostState>,
         name: i32,
         length: i32,
         receiver: i32,
         count: i32,
         args: i32| bridge(caller, name, length, receiver, count, args).map_err(wasm_error),
    )?;
    linker.func_wrap(
        "env",
        "host_interrupt",
        |caller: Caller<'_, HostState>| -> wasmtime::Result<i32> {
            // Cancellation must work even after the Tokio watchdog is shut down.
            if let Some(error) = caller.data().control.failure() {
                return Err(wasmtime::Error::msg(error.message));
            }
            Ok(0)
        },
    )?;
    linker.func_wrap(
        "env",
        "host_get_timezone_offset",
        |_high: i32, _low: i32| 0i32,
    )?;
    linker.func_wrap("env", "host_module_normalize", |_base: i32, _name: i32| {
        0i32
    })?;
    linker.func_wrap("env", "host_module_load", |_name: i32, _length: i32| 0i32)?;
    linker.func_wrap(
        "env",
        "host_promise_rejection",
        |mut caller: Caller<'_, HostState>,
         promise: i32,
         reason: i32,
         _handled: i32|
         -> wasmtime::Result<()> {
            let free = caller
                .get_export("qjs_free_value")
                .and_then(Extern::into_func)
                .ok_or_else(|| wasmtime::Error::msg("missing value deallocator"))?
                .typed::<i32, ()>(&caller)?;
            free.call(&mut caller, promise)?;
            free.call(&mut caller, reason)?;
            Ok(())
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |mut caller: Caller<'_, HostState>,
         clock: i32,
         _precision: i64,
         result: i32|
         -> wasmtime::Result<i32> {
            let nanos = match clock {
                0 => SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|error| wasmtime::Error::msg(error.to_string()))?
                    .as_nanos(),
                1 => caller.data().started.elapsed().as_nanos(),
                _ => return Ok(WASI_NOSYS),
            };
            guest_memory(&mut caller).map_err(wasm_error)?.write(
                &mut caller,
                result as u32 as usize,
                &(nanos as u64).to_le_bytes(),
            )?;
            Ok(WASI_SUCCESS)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "random_get",
        |mut caller: Caller<'_, HostState>, pointer: i32, length: i32| -> wasmtime::Result<i32> {
            let length = length as u32 as usize;
            if length > caller.data().limits.max_bridge_bytes {
                return Err(wasmtime::Error::msg("random request budget exceeded"));
            }
            let memory = guest_memory(&mut caller).map_err(wasm_error)?;
            let end = (pointer as u32 as usize)
                .checked_add(length)
                .ok_or_else(|| wasmtime::Error::msg("guest pointer overflow"))?;
            let bytes = memory
                .data_mut(&mut caller)
                .get_mut(pointer as u32 as usize..end)
                .ok_or_else(|| wasmtime::Error::msg("invalid random buffer"))?;
            getrandom::fill(bytes).map_err(|error| wasmtime::Error::msg(error.to_string()))?;
            Ok(WASI_SUCCESS)
        },
    )?;
    linker.func_wrap("wasi_snapshot_preview1", "fd_close", |_fd: i32| WASI_BADF)?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_seek",
        |_fd: i32, _offset: i64, _whence: i32, _result: i32| WASI_BADF,
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_fdstat_get",
        |mut caller: Caller<'_, HostState>, fd: i32, pointer: i32| -> wasmtime::Result<i32> {
            if !matches!(fd, 1 | 2) {
                return Ok(WASI_BADF);
            }
            let mut stat = [0u8; 24];
            stat[0] = 2;
            guest_memory(&mut caller).map_err(wasm_error)?.write(
                &mut caller,
                pointer as u32 as usize,
                &stat,
            )?;
            Ok(WASI_SUCCESS)
        },
    )?;
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_write",
        |mut caller: Caller<'_, HostState>,
         fd: i32,
         vectors: i32,
         count: i32,
         result: i32|
         -> wasmtime::Result<i32> {
            if !matches!(fd, 1 | 2) {
                return Ok(WASI_BADF);
            }
            if !(0..=1024).contains(&count) {
                return Err(wasmtime::Error::msg("invalid iovec count"));
            }
            let mut written = 0u32;
            for index in 0..count {
                let offset = vectors
                    .checked_add(index * 8)
                    .ok_or_else(|| wasmtime::Error::msg("iovec pointer overflow"))?;
                let pointer = read_u32(&mut caller, offset).map_err(wasm_error)?;
                let length = read_u32(
                    &mut caller,
                    offset
                        .checked_add(4)
                        .ok_or_else(|| wasmtime::Error::msg("iovec pointer overflow"))?,
                )
                .map_err(wasm_error)?;
                let end = (pointer as usize)
                    .checked_add(length as usize)
                    .ok_or_else(|| wasmtime::Error::msg("iovec size overflow"))?;
                if end
                    > guest_memory(&mut caller)
                        .map_err(wasm_error)?
                        .data_size(&caller)
                {
                    return Err(wasmtime::Error::msg("invalid diagnostic buffer"));
                }
                written = written
                    .checked_add(length)
                    .ok_or_else(|| wasmtime::Error::msg("diagnostic size overflow"))?;
            }
            guest_memory(&mut caller).map_err(wasm_error)?.write(
                &mut caller,
                result as u32 as usize,
                &written.to_le_bytes(),
            )?;
            Ok(WASI_SUCCESS)
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn compilation_cache_rejects_replaceable_directories() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        let cache = directory.path().join("cache");
        drop(compilation_cache(&cache).unwrap());
        assert_eq!(
            cache.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        let link = directory.path().join("link");
        symlink(&cache, &link).unwrap();
        assert!(compilation_cache(&link).is_err());
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(compilation_cache(&cache).is_err());
        assert!(compilation_cache(Path::new("relative-cache")).is_err());
    }

    fn control(timeout: Option<Duration>) -> RunControl {
        RunControl::new(CancellationToken::new(), CancellationToken::new(), timeout).unwrap()
    }

    #[test]
    fn store_diff_checks_native_cancellation_and_deadline() {
        for (control, expected) in [
            (control(None), ErrorKind::Aborted),
            (control(Some(Duration::ZERO)), ErrorKind::Timeout),
        ] {
            if expected == ErrorKind::Aborted {
                control.cancellation.cancel();
            }
            let error = parse_writes(r#"[["absent"]]"#, &BTreeMap::new(), &control).unwrap_err();
            assert_eq!(classify(error, &control).kind, expected);
        }
    }

    #[test]
    fn store_diff_validates_final_snapshot_without_mutating_saved() {
        const VALUE_HEADROOM: usize = 16;
        const SNAPSHOT_ENTRIES: usize = MAX_STORE_TOTAL_CHARS / MAX_STORE_VALUE_CHARS;
        let value = json!("x".repeat(MAX_STORE_VALUE_CHARS - VALUE_HEADROOM));
        let saved: BTreeMap<_, _> = (0..SNAPSHOT_ENTRIES)
            .map(|index| (format!("old{index}"), value.clone()))
            .collect();
        let value_json = serde_json::to_string(&value).unwrap();
        let replacement = json!([["new", value_json], ["old3"]]).to_string();
        let writes = parse_writes(&replacement, &saved, &control(None)).unwrap();
        assert_eq!(writes.set, BTreeMap::from([("new".into(), value.clone())]));
        assert_eq!(writes.delete, ["old3"]);
        assert_eq!(saved.len(), SNAPSHOT_ENTRIES);
        assert_eq!(saved["old3"], value);

        let excess = json!([["new", value_json]]).to_string();
        assert!(
            parse_writes(&excess, &saved, &control(None))
                .unwrap_err()
                .to_string()
                .contains("store budget exceeded")
        );
        let replacement = json!([["old3", "1"]]).to_string();
        assert_eq!(
            parse_writes(&replacement, &saved, &control(None))
                .unwrap()
                .set["old3"],
            1
        );
    }

    #[test]
    fn store_diff_enforces_entry_and_byte_budgets() {
        let control = control(None);
        let entries: Vec<_> = (0..MAX_STORE_DIFF_ENTRIES)
            .map(|index| vec![index.to_string()])
            .collect();
        let payload = serde_json::to_string(&entries).unwrap();
        assert_eq!(
            parse_writes(&payload, &BTreeMap::new(), &control)
                .unwrap()
                .delete
                .len(),
            MAX_STORE_DIFF_ENTRIES
        );
        let excess = format!("[{},[\"extra\"]]", &payload[1..payload.len() - 1]);
        assert!(
            parse_writes(&excess, &BTreeMap::new(), &control)
                .unwrap_err()
                .to_string()
                .contains("entry budget")
        );
        let excess = " ".repeat(MAX_STORE_DIFF_BYTES + 1);
        assert!(
            parse_writes(&excess, &BTreeMap::new(), &control)
                .unwrap_err()
                .to_string()
                .contains("byte budget")
        );
    }

    proptest::proptest! {
        #[cfg(unix)]
        #[test]
        fn compilation_cache_rejects_shared_permissions(shared_bits in 0u32..=0o077) {
            use std::os::unix::fs::PermissionsExt;
            let directory = tempfile::tempdir_in("/tmp").unwrap();
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700 | shared_bits)).unwrap();
            proptest::prop_assert_eq!(compilation_cache(directory.path()).is_ok(), shared_bits == 0);
        }

        #[test]
        fn store_diff_rejects_all_duplicate_forms(key in "[a-z]{0,32}", first_set in proptest::bool::ANY, second_set in proptest::bool::ANY) {
            let entry = |set| if set { json!([key, "1"]) } else { json!([key]) };
            let payload = json!([entry(first_set), entry(second_set)]).to_string();
            let error = parse_writes(&payload, &BTreeMap::new(), &control(None)).unwrap_err();
            proptest::prop_assert!(error.to_string().contains("duplicate store key"));
        }
    }
}
