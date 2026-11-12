# phi-codemode

## Purpose

Standalone JavaScript execution and tool orchestration. QuickJS runs inside Wasmtime; the crate owns sandbox lifecycle, host bridging, execution limits and cancellation.

Contracts and SDK integration: [`README.md`](README.md). Guest build requirements and licenses: [`vendor/README.md`](vendor/README.md).

Do not add Agents, Sessions, authorization policy, Transcript persistence, CLI configuration or terminal rendering here. The host owns tool governance and durable storage.

## Code map

- `src/lib.rs` — curated public exports and sandbox behavior tests.
- `src/types.rs` — tool definitions, options, limits, output, outcomes and errors.
- `src/sandbox.rs` — sandbox lifecycle and asynchronous host-tool coordination.
- `src/runtime.rs` — Wasmtime compilation/cache, execution workers, guest bridge and resource enforcement.
- `src/prelude.js` — guest tool/global bindings, output and snapshot-store API.
- `src/source.rs` — source directives and line-preserving parsing.
- `src/declarations.rs` / `src/mcp-types.d.ts` — model-facing TypeScript declarations and schema rendering.
- `vendor/quickjs-wasi/` / `build-quickjs.sh` — embedded guest engine and reproducible rebuild entry point.
- `benches/` — construction and execution benchmarks.

## Local invariants

- Keep this crate independent of `phi-sdk`, `phi-tui` and `phi-interactive`.
- Each execution gets a fresh Wasmtime Store, WASM instance and QuickJS VM. Share compiled modules, never mutable execution state.
- Scripts access the host only through explicitly injected tools and globals. Do not introduce filesystem, network, environment, process or module-loading capabilities.
- Host calls must retain the caller's validation, authorization and tool lifecycle; sandbox isolation is not invocation permission.
- Preserve `None` as JavaScript `undefined` and JSON null as `null`. Reject unrepresentable bridge values rather than silently changing them.
- Keep declaration naming and runtime tool/global resolution consistent, including first-tool-wins collisions.
- Source parsing does not apply execution deadlines or output-token budgets; callers own that policy.
- Enforce guest, bridge, output, call and store limits at their owning boundaries. Host tools remain responsible for their own resource use and nonblocking behavior.
- Cancellation, execution-future drop and sandbox shutdown must stop guest execution and cancel pending host calls. `close()` waits for cleanup and rejects later executions.
- Preserve partial output and call diagnostics on failure; expose store diffs only on success. The host supplies snapshots and persists successful diffs.
- Never imply tool side effects are transactional or rolled back on failure or cancellation.
- Persistent compilation caches contain native code. Preserve private-path validation and safe fallback when caching is unavailable.

## Change patterns

- Update Rust bridge handling and guest bindings together when changing the wire contract.
- Keep declaration and runtime behavior aligned; schemas describe tools, not runtime argument validation.
- Add observable sandbox regressions beside existing tests; keep private-mechanism tests with their modules.
- Update `README.md` when changing public options, script APIs, limits or lifecycle guarantees.
- Rebuild embedded WASM with `build-quickjs.sh` when changing guest engine sources; follow `vendor/README.md`.

## Validation

```bash
cargo test -p phi-codemode
cargo clippy -p phi-codemode --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Related

- Workspace rules: [`../AGENTS.md`](../AGENTS.md)
- SDK host rules: [`../phi-sdk/AGENTS.md`](../phi-sdk/AGENTS.md)
- Workflow integration: [`../docs/design-workflow.md`](../docs/design-workflow.md)
