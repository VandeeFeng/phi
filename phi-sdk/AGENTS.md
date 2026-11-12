# phi-sdk

## Purpose

Reusable agent runtime and public SDK surface. It owns client/runtime construction, providers, context, transcript reduction, tools, permissions, sandboxing, extensions, memory/Tape, subagents, and lifecycle events.

Architecture: [`../docs/design-sdk.md`](../docs/design-sdk.md), [`../docs/design-hook.md`](../docs/design-hook.md), [`../docs/agent/design-agent-runtime.md`](../docs/agent/design-agent-runtime.md), and [`../docs/agent/design-parallel-agent.md`](../docs/agent/design-parallel-agent.md).

Do not add CLI workflows, application session UX, config discovery, or terminal rendering policy here.

## Code map

- `src/lib.rs` — curated public modules and re-exports.
- `src/factory.rs` — `Phi`, builders, construction policy/seeds, prepared generation resources and construction errors.
- `src/harness/` — Agent execution and orchestration infrastructure; Instance and Manager retain separate ownership.
- `src/harness/manager/` — Agent API, membership, Teams/aliases, authority, admission/startup/compensation, messaging and control. Direct SDK use needs no application runtime or Session storage. `mod.rs` composes modules and exports; `api.rs` owns the developer-facing API and identity contracts.
- `src/harness/instance/` — independent retained execution resources; each child uses its own registered instance.
  - `builder.rs` — semantic construction intent and opaque candidates; `execution/runtime.rs` assembles isolated resources and accepts runtime generations.
  - `handle.rs` — instance control, identity, owned snapshots/views/subscriptions, streams and query conveniences.
  - `actor.rs` — serialized commands, progression, publication, interactions, quiescence and retained teammate idle episodes, including worker invalidation.
  - `child.rs` — child observation contracts, admission/cancellation and immutable initial completion; `interaction.rs` owns the interaction Broker.
  - `execution/mod.rs` — canonical state, hosted loop lifetime and scoped tool dispatch; `execution/submission.rs` owns prompt commits, admission, queues, messages and extension admission/dispatch boundaries.
  - `execution/delegation.rs` — specialization and specialized runner construction, child coordination/feedback and directed outcomes; `execution/teammate.rs` owns permission checks, continuation preparation, idle reporting and managed stop wiring.
  - `tests.rs` / `tests/` — Instance observation, runtime replacement/reset, workspace binding, cross-turn tool policy, interruption, delegated completion and retained teammate regressions. Small private-mechanism tests remain inline.
- `src/harness/agentic_loop/` — complete single-execution loop: runtime/request configuration, runner iteration/retries/Steer, canonical pipeline/completion gates, response normalization and tool governance/finalization; `tests.rs` covers pipeline commits, compaction/budget, retries, completion gates and tool-output finalization.
- `src/harness/tests/` — delegation, message delivery and scoped Task-tool integration across harness owners; `support.rs` holds shared test fixtures for all harness suites. Using Manager or a mock provider alone does not make a test harness-level.
- `src/agent/` — execution options, runtime construction, custom agents and built-in tools.
- `src/tools/` — curated public facade and tool subsystem: `declaration.rs` owns Rust declarations and registration conversion; `policy.rs` owns settings parsing, name normalization and availability; `catalog.rs` owns registration, resolution, projections, dispatch and finalization entry; `display.rs` owns structured display adapters. Built-in implementations remain in `src/agent/tools/`.
- `src/core/` — provider-agnostic events, turns, interactions, lifecycle contracts, and reducer.
- `src/hooks.rs` — Hook registration, local handler scheduling/execution, outcome aggregation, callback deadlines and generation-owned hook tasks shared by Rust, settings commands and Extension/Lua adapters; `src/hooks/command.rs` owns command declarations, matching and JSON/exit-code conversion.
- `src/context.rs` — instructions, attachments, system prompt, turn prompt preparation, history conversion and compaction.
- `src/transcript/` — `mod.rs` owns the entry tree, history operations and public exports; `types.rs` defines persisted content; `context.rs` derives effective context, compaction boundaries and token estimates.
- `src/permission/` / `src/sandbox.rs` — authorization, outer-turn execution scopes and execution isolation.
- `src/extension/` — builtin capabilities, Lua and process capabilities; `config.rs` owns typed `ExtensionConfig`, effective enablement and exclusion resolution.
- `src/extension/memory/` — memory graph, Tape persistence, mutation gate, and tools; `runtime.rs` owns service/state assembly, keyword preparation, admission accounting, dispatch evidence and cleanup through opaque Memory session/submission/dispatch handles. `hooks.rs` registers ordinary Rust callbacks that run Memory synchronization and content generation. Hook registration Once accounting stays in `src/hooks.rs` and continues across accepted generations of the same Instance.
- `src/provider/` / `src/models/` / `src/config.rs` — provider and runtime configuration.
- `src/utils/` — shared paths, bounded process execution and workspace discovery.

## Local invariants

### API and execution

- Define contracts beside their owning mechanisms in `factory`, `instance`, `manager`, `core` and `tools`; curate public exports in `src/lib.rs` and `src/tools/mod.rs`. Implementation modules remain private. Manager serves both direct developer Agent API and parallel orchestration; model tools are adapters, not its capability boundary.
- `phi-sdk` must remain independent of both `phi-tui` and `phi-interactive`.
- MCP and Lua are always compiled; control activation and loading through runtime configuration.
- `RuntimeConfig.extensions` owns typed builtin configuration. `ExtensionConfig::is_enabled` determines effective enablement; `resolve` applies exclusions after overrides and before resource preparation. Exclusions win. Enablement does not imply resource availability. Each extension owns its configuration fields and defaults.
- Route one-shot, stream and subagent execution through `AgentInstance` and the existing event/reducer pipeline. Each provider attempt reads committed canonical Transcript; do not maintain growing runner history or publish completion before its gate.
- Resolve delegation directories on the blocking pool before Manager admission; pending actor requests remain bounded, ordered and cancellable.
- Keep execution, command serialization, interactions, and canonical Transcript in each independent `AgentInstance`; keep membership/delegation coordination in `AgentManager`. Never add parent-owned child pipelines or manager-owned execution state.
- Master names the primary conversation Agent, not a privileged role; do not rename it main or root. Parent/child and Team lead are separate relationships. All registrations use ordinary `insert`, relationship/Team scope and explicit grants; legacy AgentRole permissions and master bypasses remain removed. Definitions, aliases and reused IDs never grant authority; execution security still applies.
- Route managed instance stop and caller-bound stop through Manager's epoch-captured creation subtree closure before actor awaits. Keep actor-local shutdown private.
- Preserve initial admission/completion across later invocations. Cancellation retains instances; startup failure compensates captured registrations, associations and alias reservations before release. Report cleanup failures separately; never overwrite published outcomes or accepted ToolResults.
- Agent tool results follow [delegation mode](../docs/agent/design-parallel-agent.md#foreground-and-background-delegation): foreground reports execution, including child cancellation; background acknowledges startup and later outcomes never rewrite it. Parent abort interrupts the foreground call, not accepted children; never record child cancellation for an interrupted wait.
- Follow-ups use ordinary instance admission and preserve specialization/history. Checklist Tasks are separate recorded work, not execution or persistence acknowledgements; follow [Task and Team design](../docs/agent/design-agent-task.md).
- Keep application client, transport, attachment, Session persistence, and UI semantics outside `phi-sdk`.
- Runtime construction accepts semantic intent only; reusable collaborators, policy and initial grant seeds belong in `PhiRuntimeServices`. Every candidate gets a fresh checker, fixed routing and independent extension VM. Live grants never mutate factory seeds; same-Instance replacement inherits confirmed grants only at acceptance.
- Do not expose provider internals, raw permission channels, reply senders, runners, or reducer state.
- Keep `CoreEventEnvelope` as the internal scoped event surface and project to `PhiEventEnvelope` at every external adapter boundary, including the official interactive app.
- Preserve one active stream drain owner.
- Keep runner startup and interaction Broker ownership in Instance, prompt preparation in context, and outer-turn permission scope in permission. Core defines semantics and reduction, not execution startup.
- Use `SubmissionDispatch` for requested routing and `SubmissionDisposition` for admission results, not execution outcomes. Coherent SDK observation uses `AgentInstanceView` and `ViewBoundary`.
- Snapshot subscriptions return owned values only. Never expose raw watch receivers, borrowed guards or consumer callbacks under publisher locks.
### History, tools, and security

- Transcript changes must preserve entry IDs, parent/leaf branch structure, tool call IDs, ordering, and compaction semantics. Reset and compaction share validated branch-local head/marker projection; reset changes effective context, not identity, history, usage, configuration or permissions.
- Assistant execution facts include provider/model, source API when known, and optional Switch identity from the accepted generation, including partial/error/aborted responses. SDK records Switch as an opaque identity; application owns definition resolution. Do not fabricate configuration events or full snapshots on navigation/continuation. Records without Switch remain valid.
- Durable tool effects must finalize before provider history and use the same preallocated transcript entry ID as reducer commit.
- Resolve tool definitions, metadata, ownership, governance, visibility, prompt summaries, and dispatch through `tools/catalog.rs`'s `ToolCatalogBuilder`/`ToolRegistry`; do not restore parallel maps. `ToolRule`/`ToolRules` are settings values, not executable declarations. Shared parsing and availability policy belong in `tools/policy.rs`; invocation authorization belongs in `PermissionRuntime`. Parsing must neither load resources nor grant permission.
- Use [tool semantics](../docs/design-tools.md): registration establishes the catalog, availability gates use, exposure projects model access, and permission evaluates invocations. Internal availability checks use `is_available`; preapproval intent is not a confirmed grant. Preserve external YAML and SDK compatibility names.
- Keep Rust tool declarations context-free through Rig's `PortableTool` boundary; do not expose registry, authority, permission, lifecycle, finalizer, or mutable runtime context mechanisms.
- ToolSearch is Phi-governed and loads deferred tools for direct calls on the selected Transcript branch; Codemode discovery is independent. Agent/Prompt settings reject retired `MCP`/`mcp` names and individual MCP tool names, including unprefixed aliases. Validate against immutable candidate MCP contracts without exposing registries or rereading captured declarations. Removing ToolSearch never disables MCP, withdraws loaded declarations or grants invocation permission.
- Retain submission tool policy through preparation and queueing; activate only at outer dispatch or actual Steer injection. Injected Steers union/deduplicate allowances and removals; removals win. Inner AgentTurn boundaries do not expire policy; completion/failure and synchronous runner abort/drop retire it. New turns and children inherit no transient policy. Bind scope to ToolSet/checker identities; explicitly propagate it into model-tool blocking Lua work, never unrelated extension commands. Never persist it as Session grants.
- Tool input rewrites never bypass permission or sandbox checks. Bash authorization must retain original and transformed command evidence.
- Keep Agent invocation permissions separate from host-authorized Hook execution; neither grants the other authority. Preserve Lua source/program capability checks. SDK consumes host Hook policy, never application trust settings. See [process authority](../docs/design-hook.md#workspace-trust-and-process-authority).
- Sandbox setup must validate/canonicalize paths and fail closed; do not fall back to an unrelated writable directory.
- Tape mutations must go through `TapeMutationGate`; tools must not duplicate keyword/manual permission state. Host execution holds only opaque Memory session/submission/dispatch handles, never Memory services, Tape state or keyword DTOs. Keep this lifecycle boundary inside Memory until another extension needs the same contract.
- Subagents must not receive Tape mutation tools. Execution hosts consume aggregated extension delegated-tool restrictions, not Memory-specific policy functions; restrictions remain independent of capability activation.
- Builtin extensions declare bundled Skill availability from captured runtime configuration. Keep extension-specific eligibility in the extension; applications own resource loading and link reconciliation.
- Memory Markdown and anchor JSONL are canonical; the materialized graph is rebuildable and must not become a mutation source.
- Keep Tape smart file ordering in `MemoryGraph::select`; recent tool activity is transient query evidence, not a parallel sorter or persisted graph state, and concrete historical line ranges must not enter injected context.
### Runtime resources and adapters

- Build each candidate's extension VM from captured source content/paths without rereading files or sharing mutable VM state. Use one prepared `ExtensionRuntime` for hooks, commands, keybindings, display handlers and tools, with the generation's permission runtime. Failed reloads must preserve the active generation.
- Expose application commands and keybindings through `AdapterDeclarations`; retain candidate `AgentRuntimeResources` for Session boundaries, lifecycle notifications and live resource access. `Phi` lifecycle operations notify factory hooks only. Keep runtime tools and hooks on SDK-owned resolution paths.
- MCP connection and metadata state belongs in `McpClientManager`; expose domain snapshots/events rather than UI state.
- Project canonical history for the target provider/API/model at the request boundary. Keep display IDs separate from provider message IDs; never infer provider identity from an ID prefix or reuse foreign reasoning/tool wire data. Preserve tool call/result correlation. Keep provider-specific image behavior behind `ProviderHistoryPolicy`. See [provider history projection](../docs/design-sdk.md#provider-history-projection).
- Runtime instructions load the explicit global `AGENTS.md` and one project-root instruction file: prefer nonempty `AGENTS.md`, otherwise `CLAUDE.md`. The project root is the current Git/worktree root (or working directory outside Git); do not scan subdirectories or ancestors.
- Provider and credential data must flow through typed config/options; do not scatter environment reads or log secrets. Keep subscription lease authority in `provider/auth.rs`, separate from token refresh; follow [provider authentication contracts](../docs/design-sdk.md#provider-protocols-and-reasoning).
- SDK code emits structured events or typed results; consumers decide presentation.
- File mutation tools own canonical semantic diff computation, patch generation, and first-change metadata. Never emit terminal-formatted diff strings.
- Keep permission checker/runtime, reducers, runners, concrete agent tools, and memory implementation modules crate-private. Expose memory/Tape operations and DTOs only through the curated `extension::memory` facade. Memory session and prompt behavior use ordinary typed hooks from `MemoryHooks`; do not create a separate Memory hook path.

## Lifecycle guidance

Contracts, execution flow and examples belong in [Hook design](../docs/design-hook.md).

- Expose boundary contracts through `phi_sdk::lifecycle` and typed Hook registration/results through `phi_sdk::hooks`; keep registries, handlers, emitters and dynamic adapter results private.
- Hook subsystem owns handler execution and accounting; hosts own the intercepted operation, outcome application and canonical Transcript. Use one registry, direct typed Rust callbacks and boundary-local Lua/command conversion.
- Choose the narrow existing boundary for each concern. Keep observation, completion gates, Session shutdown and application-only operations distinct; do not add a broad `BeforeAgentStart` hook without an uncovered concern. See the [boundary catalog](../docs/design-hook.md#boundary-catalog).
- Preserve original input and validate merged contributions before admission/commit. Keep application expansion separate from submission hooks and callback errors distinct from Transcript preparation errors.
- Keep invocation accounting in Hooks and content generation in extensions. Never rearm consumed handlers through recovery or history navigation. See [registration lifetime](../docs/design-hook.md#once-and-registration-lifetime).
- Bind tasks, readiness, notifications and cleanup to matching generation resources. Preserve cancellation and owning deadlines; never layer a lifecycle deadline over a command process budget. See [Session/task design](../docs/design-hook.md#session-boundaries-and-hook-tasks).

## Change patterns

- Add client intent through the option struct, fluent builder, validation, and assembly path together.
- Add tools as complete registrations with explicit metadata and governance.
- Add provider data through typed config/options and local conversion helpers.
- Add Rust Agent definitions through `PhiBuilder::agent(s)` and resolve them into `AgentCatalog`; reject invalid definitions and duplicate names before constructing `Phi`.
- Keep `PhiBuildError`, resource diagnostics, client-construction errors, and execution failures as separate typed/recoverable boundaries.
- Preserve caller Agent-root priority, sort files within each root, and keep first-file-wins behavior deterministic.
- Validate only declaration-local Agent semantics during factory build; resolve model, tool, Lua and MCP availability during runtime preparation.
- Keep Agent definitions limited to runtime semantics; presentation metadata belongs to application adapters.
- Prefer handles and narrow methods over exposing internal run objects.
- Keep `mod.rs` focused on composition/exports and state machines in their owning files. Prefer coarse modules covering complete responsibility chains; do not extract a few hundred lines merely to shorten a file. Colocation does not merge Instance, loop, Manager or Task ownership. Tests follow the verified boundary: small private-mechanism tests stay inline, Instance behavior belongs in `instance/tests.rs` and `instance/tests/`, cross-harness flows in `harness/tests/`, and cross-layer application tests in `phi-interactive`. Never widen production visibility to move tests.
- Reuse existing context/history conversion and text-collection paths; do not add parallel loops or reducers.
- For cross-crate UI needs, add structured SDK values rather than rendering logic.
- When changing optional runtime capabilities, test enabled and disabled configurations.

## Related

- Workspace rules: `../AGENTS.md`
- App adapter rules: `../phi-interactive/AGENTS.md`
- TUI rules: `../phi-tui/AGENTS.md`
- Codemode engine rules: `../phi-codemode/AGENTS.md`
