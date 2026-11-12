# phi-codemode

Standalone JavaScript tool orchestration for Rust, modeled on `pi-codemode`.
QuickJS runs inside Wasmtime; scripts reach the host only through injected tools
and globals. This crate owns neither Agents nor Sessions, permissions or storage.

## Usage

Requires a Tokio runtime with time enabled. Construction compiles the embedded
WASM in parallel on the blocking pool. Sandboxes share the compiled module per
metering mode and cache directory within a process, never mutable execution state.

`SandboxOptions.cache_dir` optionally enables Wasmtime's persistent compilation
cache across processes. The default is no disk cache. Supply an absolute private
directory: cached artifacts contain native code and must not be writable by
untrusted users. On Unix, new directories use `0700`; existing shared directories,
foreign owners, symlink directories and unsafe writable ancestors are rejected.
An unavailable cache emits a tracing warning and falls back to compilation.
Wasmtime handles cache keys, version/configuration changes and invalid entries.

```rust
use phi_codemode::{
    CodemodeSandbox, CodemodeTool, ExecuteOptions, ExecutionOutcome,
    SandboxOptions, ToolDefinition,
};
use serde_json::json;

# async fn example() -> Result<(), Box<dyn std::error::Error>> {
let echo = CodemodeTool::new(
    ToolDefinition {
        name: "echo".into(),
        description: "Return a value".into(),
        input_schema: Some(json!({"type": "object"})),
        output_schema: None,
    },
    |args, context| async move {
        if context.cancellation.is_cancelled() {
            return Err("cancelled".into());
        }
        Ok(args)
    },
);
let sandbox = CodemodeSandbox::new(SandboxOptions {
    tools: vec![echo],
    ..Default::default()
}).await?;

let result = sandbox.execute(
    "const value = await tools.echo({count: 3}); text(value); return value.count;",
    ExecuteOptions::default(),
).await;
if let ExecutionOutcome::Success { value, store_writes } = result.outcome {
    assert_eq!(value, Some(json!(3)));
    assert!(store_writes.set.is_empty());
}
sandbox.close().await;
# Ok(())
# }
```

Tools receive and return `Option<serde_json::Value>`: `None` means JavaScript
`undefined`, while `Some(Value::Null)` means `null`. Tool errors reject the JS
promise. Arguments and results cross as JSON; schemas inform declarations only.
The host must run nested calls through its ordinary validation, authorization
and tool lifecycle path, not bypass it by invoking implementations directly.

## Script API

Input is an async function body: top-level `await` and `return` work.

| API | Purpose |
| --- | --- |
| `tools.name(args)` / `tools["original-name"](args)` | Invoke a tool |
| `ALL_TOOLS` | Callable identifiers and descriptions |
| `text(value)` / `console.log/info/warn/error/debug(...)` | Emit ordered text |
| `image(value)` | Emit an inline PNG, JPEG, GIF or WebP image |
| `exit()` | Complete immediately, keeping output and store writes |
| `store(key, value)` / `load(key)` | Read snapshot state and produce success-only changes |

`SandboxOptions.globals` injects host helpers such as `searchTools` or
`models.classify`. Names may be `name` or `namespace.member`.
`with_arguments(ArgumentMode::Spread)` passes an argument array;
`with_signature(...)` supplies its model-facing TypeScript signature.
Tool lists are immutable snapshots; build a new sandbox for a replacement set.
Namespace keys, including original-name aliases, follow pi's first-tool-wins
rule. Declarations list only the winning normalized names; avoid collisions.

`parse_codemode_source()` accepts an optional first-line directive:

```js
// @options: {"max_output_tokens": 2000, "timeout_ms": 30000}
return await tools.search({query: "example"});
```

Parsing preserves line numbers. It does not apply the options: the caller maps
`timeout_ms` to `ExecutionTimeout` and owns output token truncation.
`sandbox.declarations()` and the exported schema utilities render TypeScript
for model context. MCP result signatures require `MCP_TYPESCRIPT_PREAMBLE`.

## Lifecycle and limits

Each execution creates a fresh Wasmtime Store, WASM instance and QuickJS VM.
There are no filesystem, network, environment, process, timer or module-loading
capabilities. Minimal WASI shims provide clocks, randomness and discarded engine
diagnostics. Guest built-ins are locked down before script execution.

Defaults:

- Deadline: 300 seconds, including tools; override with `ExecutionTimeout`.
- QuickJS heap: 256 MiB; WASM linear memory: 320 MiB.
- Native WASM stack: 512 KiB; QuickJS stack guard: 32 KiB.
- Output: 16 Mi UTF-16 code units and 100,000 items.
- Bridge: 32 MiB total UTF-8 string fields per guest event or serialized tool result; source: 1 MiB.
- Calls: 1,000 total, 64 pending, including globals.
- Store: 256 Ki UTF-16 code units per serialized value, 1 Mi total including keys.
- Store diff: 10,000 unique keys and 16 MiB serialized UTF-8, including deletions.
- Optional fuel budget; disabled by default, without fuel instrumentation.

`ExecuteOptions.cancellation`, dropping the execution future, `close()`, and
sandbox drop cancel execution. `close()` also waits for worker and tool-future
cleanup and rejects subsequent executions. Pending/unawaited calls are cancelled
when the script finishes. Already-applied tool effects are not rolled back.
Tools must be nonblocking asynchronous implementations; cancellation cannot undo
side effects or preempt arbitrary host code. VM and bridge limits do not bound
allocations performed inside host tools.

Results distinguish script, timeout, aborted and sandbox failures. Partial output
and tool-call status/duration survive failures; failed executions expose no store
writes. The caller persists successful diffs and supplies the next store snapshot.
Rust strings cannot represent unpaired UTF-16 surrogates; such JSON payloads are
rejected at the bridge instead of silently changing their values.

## Phi SDK integration

Phi embeds this crate independently of pi. Enable the SDK's `Codemode` tool with
`PhiBuilder::codemode(CodemodeConfig { enabled: true, ..Default::default() })`,
CLI `phi --codemode`, or application settings:

```json
{
  "codemode": { "inlineBudget": 3000, "cache": true }
}
```

Application settings follow the [feature-block rules](../README.md#settings-reference).
SDK `CodemodeConfig::default()` is disabled. `inlineBudget` defaults to `3000`.
`cache` defaults to `true`: Phi stores compilation artifacts in
`~/.phi/cache/codemode/`, or `$PHI_CONFIG_DIR/cache/codemode/` when overridden.
Set `cache` to `false` to disable disk caching, or to a directory string to choose
another location. Relative paths resolve from the global Phi config directory,
not the project or Session directory. Empty paths are rejected. Parallel compilation
and process-local sharing remain enabled when disk caching is disabled.
Projects and Sessions share the selected cache; deleting it only requires
recompilation on the next process start.
`--no-tools` takes precedence. The tool accepts
`{"code":"return await tools.Read({path: 'README.md'});"}` and has no default
script deadline. An explicit `timeout_ms` shares one budget across discovery,
store loading, sandbox initialization and script execution. Nested calls retain current tool availability, permissions,
hooks and scheduling. Only script output enters provider tool history; nested
calls have separate audit records. Successful store writes follow the current
Transcript branch. Failure diagnostics follow console output before truncation.
Repeated images share one saved file per execution while retaining every image
block in output.

Tape mutations (`tape_anchor`, `tape_delete`, `tape_reset`) and tools
requiring durable finalization are not callable or discoverable from scripts.
Memory search/check and Tape info/read remain available when enabled. These
restrictions do not change direct tool access. `models.*` and `only` mode are not
implemented by this adapter.

### File concurrency

- Reads may overlap, including reads of the same file.
- Edits/writes to different files may overlap.
- Edits/writes to the same file retain the file mutation queue.
- Reading a mutation's result requires an explicit `await`; the queue does not order reads.
- Codemode scripts remain serial so store snapshots and commits do not race.

```js
await Promise.allSettled([tools.Read({path: "a.txt"}), tools.Read({path: "a.txt"})]);
await tools.Edit({path: "a.txt", old_text: "before", new_text: "after"});
return await tools.Read({path: "a.txt"});
```

Other tools marked sequential retain exclusive admission within a script.

### Deferred exposure

Rust tool declarations may use
`.exposure(phi_sdk::tools::ToolExposure::Deferred)`. Deferred tools are initially omitted
from provider declarations and system-prompt tool lists/guidelines, and always
omitted from Codemode's inline catalog, regardless of `inlineBudget`. Scripts still find them through
`ALL_TOOLS`, `searchTools()`, `describeTool()` and `describeNamespace()`, and call
them through the same governed nested execution path. Discovery includes tool
guidelines. Codemode discovery does not load direct declarations. ToolSearch can
load deferred tools for direct model calls from the next request.

MCP tools use the shared `exposure` and `toolExposure` configuration,
independently of Codemode enablement. The MCP default is `deferred`;
`direct` tools remain directly declared, while `hidden` tools are neither
discoverable nor callable. Transport restrictions and invocation permissions
still apply.
MCP metadata changes enter the existing validated runtime-generation replacement
flow. Deferred-only lazy servers do not preconnect at startup. Codemode loads
missing deferred metadata on demand into a validated script-scoped registry;
provider declarations stay unchanged and nested calls retain turn policy. Deferred-only
catalog changes do not change Codemode's inline description. Without Codemode,
direct calls and ToolSearch loading remain available.

Tool effects and Transcript persistence are not an atomic transaction. Cancellation
can leave applied filesystem or process effects without a completed result;
Write, Edit and Bash effects cannot be rolled back by the adapter.

## Validation

```sh
cargo test -p phi-codemode
cargo clippy -p phi-codemode --all-targets -- -D warnings
./test.sh focus phi-codemode timeout
cargo bench -p phi-codemode --bench runtime -- --mode unmetered --samples 20
```

The benchmark reports construction time and execution median/p95. Pass
`--cache-dir /tmp/phi-codemode-bench-cache` and repeat the command in a new process
to compare a cache miss with a persistent-cache hit. Run `--mode metered` in a
separate process for comparison; `--filter waiting_16` isolates
concurrent tool waits. `--wait-ms 2000` separates idle waiting from VM startup.

Rebuild the embedded WASM with `./phi-codemode/build-quickjs.sh`.
Requirements and licenses: [`vendor/README.md`](vendor/README.md).
Wasmtime 50 requires Rust 1.97 or newer.
