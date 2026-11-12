# phi-sdk Design

`phi-sdk` is the reusable Agent runtime and public domain layer. It owns execution, Transcript semantics, provider/tool integration, permissions, sandboxing, extensions, Memory/Tape, lifecycle and SDK orchestration.

It is independent of `phi-tui` and `phi-interactive`. CLI precedence, application Session storage, terminal UX, RPC messages and application attachment policy stay outside SDK. [Global architecture](design.md) defines workspace ownership; [Agent runtime](agent/design-agent-runtime.md) and [parallel Agent design](agent/design-parallel-agent.md) define application runtime and delegation boundaries.

## Public API contract

[lib.rs](../phi-sdk/src/lib.rs) is the compatibility boundary. An item is public only when an external consumer needs it to express intent, supply a stable capability, control an SDK domain service or observe a structured result.

Every public item must pass three tests:

1. **Semantic necessity:** it enables a supported external SDK use case.
2. **Ownership safety:** it cannot bypass instance ownership, governance, security, history reduction or validated replacement.
3. **Contract stability:** it describes a domain concept, not a channel, runner, mutable registry, cache or backend implementation.

Official application use is evidence of a required capability, not permission to expose the mechanism behind it. When a use case is missing, add a narrow semantic operation or immutable observation.

### No-wrapper rule

A facade must narrow authority or stabilize semantics: translate intent, validate candidates, preserve ownership/ordering, return domain facts, or hide synchronization/framework details. A forwarding alias or duplicate DTO with identical semantics is not a useful abstraction.

Define contracts once in their owning layer and curate them with explicit re-exports. Do not expose internals merely to avoid a wrapper, and do not add package/module abstractions without concrete installation, identity, dependency or validation needs.

### Public and private boundaries

| Domain | Public contract | Private mechanism |
| --- | --- | --- |
| Factory and construction | Phi/builder, declarative intent, validated candidates, operation-specific errors | Provider assembly, resolved registries, runtime collaborators |
| Instance and Manager | Identity, scoped semantic control, views, events, assignment/message observations | Actors, reducers, queues, task maps, reply channels |
| Tools, lifecycle and hooks | Portable declarations, metadata/results, lifecycle boundary contracts, focused typed hooks | Execution context, authority, finalizers, Hook registry/storage |
| Providers and resources | Typed protocol/configuration, catalog snapshots, policy and service operations | Concrete Rig clients/models, caches, permission routing |
| Transcript and context | Durable schema, branch/query operations, submitted content | Provider history conversion, compaction execution, prompt assembly |
| Extensions, MCP, Memory | Domain capabilities, immutable declarations/snapshots, typed outcomes | Lua state, connection leases, graph/store/gate internals |

Contracts live beside their owning mechanisms: `factory.rs` owns construction/services, `harness/instance/` owns execution and observation, `harness/manager/` owns orchestration, `core/` owns foundational semantics, and `tools/` owns tool declarations and governance. `harness/agentic_loop/` owns the complete single-execution loop, including iteration, canonical reduction and completion gates. Public facades curate these definitions without duplicate DTOs or forwarding services.

`agent`, `instance`, `manager`, `core` and provider implementation modules are not compatibility paths. Facades use explicit exports, not wholesale subtree exports. Each domain contract has one canonical public facade path; crate-root exports stay limited to primary factory/convenience APIs and do not duplicate every domain export. Runtime services accept application intent; they are not an escape hatch to mutable execution collaborators. Exact names and signatures belong to [SDK exports](../phi-sdk/src/lib.rs) and their owning modules, not a parallel inventory here.

Claude Agent SDK informs boundary shape, not a feature checklist or a reason to copy its settings, subprocess or Session infrastructure.

## Rig integration boundary

Phi reuses Rig's low-level model/tool mechanisms without adopting its Agent, lifecycle or conversation ownership.

```text
Phi semantic API and execution policy
  -> private Rig model/tool adapters
  -> provider/tool IO
  -> Phi ordered events and canonical Transcript commit
```

| Rig capability | Phi boundary |
| --- | --- |
| Model clients, requests/responses, streaming | Private provider integration; project Phi's own content/events |
| Portable tools and canonical tool result values | Reuse at context-free authoring boundary; avoid equivalent wrappers |
| ToolSet/contextual tools/dynamic snapshots | Private dispatch backend under Phi registry/governance |
| AgentRun/AgentRunner/hooks/memory ownership | Not Phi's decision core, lifecycle stack or Session authority |
| Retrieval, RMCP and telemetry primitives | Reuse only for concrete Phi policy without competing registries or content logging by default |

Rig types become public only when they are the intended safe authoring contract. Dependency availability alone justifies no Phi abstraction. Version declarations and concrete adapters belong to [Cargo.toml](../phi-sdk/Cargo.toml), [provider implementation](../phi-sdk/src/provider/mod.rs) and [tool subsystem](../phi-sdk/src/tools/mod.rs).

### Agent-loop ownership rule

See [Harness loop ownership](design-harnes.md#agent-loop-ownership-rule).

### Tool value preservation

Keep typed tool output, failure and result metadata through governance/finalization. At the history boundary, project one consistent model-visible text result into provider history, Transcript and public events. Do not reduce everything to String early and reconstruct status later.

The persisted ToolResult content is not accompanied by a competing model payload. Phi-specific semantic diffs remain typed metadata. Tool-call correlation identity is Phi-owned and distinct from optional provider call/item IDs; conversion must not fabricate IDs.

## Error contracts

Errors identify their owning operation and recovery context: factory, instance build/control, turn submission, query/stream observation and delegation remain distinct. Internal dynamic errors may retain their original source chain under a stable outer variant, but diagnostic text is never the control-flow contract.

`SubmissionDisposition` reports Started, Queued or Handled admission results, not eventual execution outcomes. Query conveniences can report typed NotStarted when no streamable turn exists. Tool failures stay tool results; terminal execution failures use serializable `PhiFailure` facts.

Failure kinds are conservative and coarse, with available provider status preserved without exposing backend types. Provider integration classifies backend errors and context-overflow evidence without consuming the original error; Agent loop and Messages retain their own error mapping and recovery policies. Rust results retain source chains; events carry stable kind/status plus diagnostic text; applications decide presentation. SDK does not turn every internal failure into a notification or terminal write.

### Durable execution failures

`TranscriptEntryKind::Failure { turn_id, failure }` records execution failure independently of assistant content, preserving `PhiFailure` kind, message, provider status and cancellation reason. Turn identity is optional. Partial assistant output remains a separate message; failures without assistant output require no synthetic reply. Tool errors remain ToolResult content.

AgentInstance records TurnFailed facts before event publication and queued dispatch, including failures drained during abort. Execution-pump failures record an Internal failure and publish HistoryChanged for checkpoint/recovery. Failure entries are retained in history, not converted into model messages.

`AgentInstanceSnapshot.latest_error: Option<PhiFailure>` is runtime state. Turn start, cancellation and runtime replacement can clear it without deleting history; reopening history does not restore it. See [persistence](agent/design-agent-runtime.md#application-session-and-persistence), [UI projection](design-interactive.md#event-projection-and-persistence) and [Gateway recovery](design-remote.md#observations-and-recovery).

See [factory/query errors](../phi-sdk/src/factory.rs), [Instance build errors](../phi-sdk/src/harness/instance/builder.rs), [submission errors](../phi-sdk/src/harness/instance/execution/submission.rs), [delegation errors](../phi-sdk/src/harness/instance/execution/delegation.rs), [stream errors](../phi-sdk/src/harness/instance/handle.rs) and [public events](../phi-sdk/src/core/event.rs).

## Harness

See [Harness design](design-harnes.md) for factory/Instance boundaries, loop execution, event reduction, interactions and execution feedback.

## Construction and prepared resources

`Phi` is reusable construction intent, not a live Agent. `PhiRuntimeServices` supplies policy, initial grant seeds and shared infrastructure such as catalogs or MCP connections. Its grant methods affect future candidates only; confirmed runtime grants never flow back into these seeds. `isolated_session()` clears initial grants while retaining policy/infrastructure; it is not required to prevent permission-routing collisions.

Each build creates a fresh permission checker and fixed interaction routing. `AgentInstanceCandidate::resources()` returns the generation-scoped `AgentRuntimeResources` handle used by execution and application boundaries. Retain that handle for generation-local grants, extensions and Session lifecycle; cloning it shares that generation, not a new candidate. It exposes no raw permission channel and does not register, start or implicitly stop an Agent. See [construction and replacement](design-harnes.md#factory-and-agentinstance-boundaries).

## Transcript and durable effects

Transcript owns the canonical entry tree, selected leaf and cumulative usage. Original entries remain intact; `messages` is not independent history.

Ordinary branch selection moves the in-memory leaf without appending an entry or configuration change. Continuation appends under that leaf, preserving other branches and recording actual assistant provider/model, source API when known, and optional `switch` identity. Accepted `RuntimeConfig.switch` is an opaque execution identity carried through the canonical reducer, including partial/error/aborted responses; SDK does not resolve application Switch definitions. Missing `switch` remains valid for legacy/direct-SDK records. This attribution adds neither configuration events nor a thinking/mode/resource snapshot. Application persistence owns disk recovery: Phi reopens at the last appended entry, not the transient navigation cursor, including after checkpoint/finalization. Extension `session_tree` notifications are not automatic history entries. See [navigation and recovery](agent/design-agent-runtime.md#navigation-and-recovery).

`effective_context()` validates selected-branch ancestry and applies Compaction/Reset markers through `TranscriptEntry.head`. Compaction retains the suffix beginning at its head and supplies a summary; Reset heads itself, excludes prior model context and supplies optional user-role handoff text. Requests, compaction and token estimates share this projection and canonical entry references. Invalid ancestry or boundaries return errors; context estimates remain separate from cumulative usage.

Context edits append branch-local `ContextEdit` facts: `{ target_id, replacement }`, where `null` omits the target from model context and `{ content }` replaces only its content. Targets must be earlier editable messages on the selected branch, never system messages. The latest retained edit wins; Compaction/Reset select the retained range before edits apply. Requests, summaries and token estimates share the projection; source IDs, metadata, raw history and cumulative usage remain unchanged. Injected context keeps its canonical association despite omitted siblings. `AgentInstance::edit_context()` admits idle-only edits; execution-time drafts use [AgentTurnEnd](design-hook.md#boundary-catalog). This adds no automatic pruning or rewind behavior.

Same-Session `reset_context` preserves identity, entries, branches, cumulative usage, configuration and permissions. Instance rejects running/pending work, waiting tasks and unresolved interactions with Busy, without queueing or aborting input. Successful reset appends a marker, publishes a snapshot and `ContextReset`, and returns a coherent view. It starts no turn and replaces no runtime.

Compaction uses entry-ID boundaries that preserve tool exchanges and User/Context groups. It summarizes the previous context preamble and newly compacted content, including reasoning, tools, Agent feedback and Context; tool results are limited and images use text placeholders. Main and specialized execution use the same request-boundary compaction path and selected execution model. Only successful, nonempty, nontruncated output commits a Compaction before the next provider request; original entries remain intact.

```text
Preallocate entry ID -> execute -> finalize durable effect against that ID
  -> emit result -> reducer commits result using the same ID
```

Finalization failure is the same tool error observed by the model, history and adapters. File tools compute one semantic diff; previews, event details and patch data derive from it rather than terminal-formatted strings or another application diff.

Branch traversal and representation mechanics belong to [Transcript](../phi-sdk/src/transcript/mod.rs), [history conversion](../phi-sdk/src/context.rs) and [Agentic Loop pipeline](../phi-sdk/src/harness/agentic_loop/pipeline.rs).

## Tool registry

See [tool semantics and boundaries](design-tools.md) for the registration, availability, exposure and authorization layers and their internal/external terminology.

One resolved ToolRegistry is the source for provider declarations, dynamic prompt sections, availability, scheduling metadata, authority, dispatch and durable-finalization entry. Its owned Rig ToolSet supplies typed execution/schema; there is no independent definition list. `tools/` owns this subsystem: `declaration.rs` defines Rust declarations and registration conversion, `policy.rs` owns settings parsing and availability, `catalog.rs` keeps registration through resolved projections/dispatch together, and `display.rs` owns structured display adapters. `tools/mod.rs` curates the existing public facade; implementation modules remain private. Built-in tool implementations stay in `agent/tools/`. Loop consumes the registry and owns scheduling/lifecycle; memory owns durable effect implementations.

`ToolRule`/`ToolRules` are shared setting values, not executable declarations. The curated `tools` facade exposes their grammar from `tools/policy.rs`; parsing loads no resources and grants no permission. `into_names()` discards patterns for whole-tool selection/removal; `into_allowances()` retains pre-approval patterns. Resource loaders own schemas and source diagnostics, ToolRegistry owns availability, and PermissionRuntime owns invocation decisions. [Frontmatter field contracts](design-interactive.md#prompt-and-agent-tool-settings) use this grammar.

Scalar-boundary validation preserves source locations. Immutable `McpToolContracts::validate_tool_rule` rejects candidate MCP tool names, including unprefixed aliases, without exposing schemas or registries. Application Prompt loading, captured Agent headers and Rust Agent settings use the same contracts; `ContextFiles` excludes invalid file definitions without rereading source files and rejects invalid Rust declarations during candidate preparation. SDK Instance preparation validates and builds from frozen MCP declarations. Resnapshotting retains those declarations; live catalog observation still detects replacement needs.

Registration establishes the runtime's tool catalog; availability projects the tools usable in the current execution scope. A registered tool may remain in the catalog while unavailable. Unavailable tools are excluded from discovery, provider definitions, dynamic guidance and final dispatch—not merely hidden from the model. Invocation permission is a separate decision: registration or availability grants no approval, and permission denial does not itself remove a tool from model exposure.

Outer-turn removals narrow availability until that scope retires. `no_tools` disables all tools, and mode restrictions apply across owners; extension overrides or manually formed calls cannot restore a filtered tool. Agent selection/exclusion narrows Phi-governed tools. Delegated tools retain their authority's availability; omission from an Agent selection does not blanket-disable MCP, whose configuration remains dedicated. MCP tools share `ToolExposure::{Direct, Deferred, Hidden}` with ordinary tools. Deferred contracts remain indirectly discoverable/callable; ToolSearch can load them for direct declarations on the selected history branch. Hidden withdraws all Agent access. MCP config resolves `exposure`/`toolExposure` into captured contracts, not separate direct/disabled lists. See [tool exposure](design-tools.md#unified-mcp-policy).

Rust declarations use context-free portable tool contracts, with optional guidance/scheduling metadata. SDK wraps invocation in governance; authoring does not expose registry, permission runtime, finalizers or mutable Agent context. In-process Rust implementations must be trusted by the embedding application: invocation permission does not sandbox arbitrary Rust code.

### Ownership and governance

Phi-owned tools use Phi policy/security. Delegated extension tools retain scoped owner authority for capability availability and calls; owner admission is distinct from invocation permission. Cross-extension collisions fail; explicit override owns the entire replacement contract. Storage backend replacement semantics do not replace Phi admission rules.

### Tool-call lifecycle

```text
Provider tool call -> canonicalize name -> concurrent PreToolUse handlers -> merge outcomes
  -> authorize final input -> execute -> durable finalization
  -> typed PostToolUse / PostToolUseFailure -> history reduction
```

Each handler receives the same boundary input. Outcomes merge in registration order after handlers complete; denial blocks the tool invocation, not the other handlers. Allow/defer still passes normal authorization and sandbox enforcement. Lua converts dynamic values at its boundary, not inside the public Rust hook contract.

Implementation: [tool declarations](../phi-sdk/src/tools/declaration.rs), [catalog and governed execution](../phi-sdk/src/tools/catalog.rs) and [loop tool execution](../phi-sdk/src/harness/agentic_loop/tools.rs).

## Agent modes

Default and Plan are SDK execution modes, distinct from responsibility roles and AgentDefinition specialization. The active mode applies to subagents and can only narrow Agent tool policy.

Default adds no restrictions. Plan denies canonical Write/Edit in provider visibility and final dispatch. Bash remains subject to permission settings; read-only Bash guidance is a prompt policy, not filesystem isolation.

Mode-prompt override replaces only the mode-specific prompt, never the SDK base prompt. A full Plan override replaces its default guidance without hidden appended guards; the hard guarantee remains its denied edit tools. Settings allowlists cannot restore filtered tools. See [Agent configuration/modes](../phi-sdk/src/config.rs) and [tool availability policy](../phi-sdk/src/tools/policy.rs).

## Permission and sandbox model

The private checker/runtime resolves Allow/Ask/Deny through scoped interactions. Built-ins, Tape and trusted extension processes use the same policy. An interaction answer confirms that the runtime applied the decision; a closed reply or missing application confirmation cannot report an applied grant. Noninteractive Ask follows explicit policy or fails closed, without creating an interaction or an Always grant. Permission approval is independent of [registry filtering](#tool-registry), capability/Manager authority, sandbox and protected-root checks; neither approval nor Yolo restores removed tools.

One resolved yolo/ask/allow/deny mode supplies unmatched-operation policy, without implicit rm/external-directory/manual rules. Yolo bypasses user permission rules only. Otherwise explicit matches use Deny > Ask > Allow; durable grants and outer-turn allowances resolve Ask, never explicit or fallback Deny. Allowances share the generic/path/Bash/typed-command decision core without mutating Session allowlists. Bare-tool allowance covers all inputs, including paths and multiline text; patterned allowances retain original/transformed evidence checks.

Sandbox validates configured paths and must not fall back to unrelated writable directories. A role, Skill, rewritten argument or ordinary Agent message is not permission evidence.

Applications select protected roots; SDK enforces them independently of permission approval. Phi's root/trust policy and trusted Memory write paths follow [application write protection](agent/design-agent-runtime.md#configuration-write-access), not a second SDK exception matrix. These guards do not provide complete filesystem/process isolation; inode aliases and external path races remain limitations.

### Bash analysis

Command analysis conservatively collects visible operations, including composed shell execution, and treats dynamic/unbounded forms as ambiguous rather than fabricating matches. Known wrappers can be peeled without claiming arbitrary scripts are safe.

Original and transformed Bash evidence are both evaluated using the stricter result. Direct extension argv is structural; only explicit shell execution enters shell analysis.

See [permission runtime](../phi-sdk/src/permission/runtime.rs), [shell analysis](../phi-sdk/src/permission/shell.rs), [command requests](../phi-sdk/src/permission/command.rs), and [sandbox](../phi-sdk/src/sandbox.rs).

## Provider protocols and reasoning

Provider identity, implementation adapter and wire protocol are distinct. Custom providers declare a supported protocol explicitly; typed configuration is the extension point, not arbitrary injected Rig clients/runtimes. Credential interpolation/resolution stays in typed client construction and never enters catalog diagnostics or Session metadata.

Codex subscription authentication has a Phi-owned 30×24-hour lease, independent of Rig token expiry. Each streaming/non-streaming model request (including compaction and specialized Agents) checks persisted authorization before Rig credential maintenance and checks again before publishing refreshed credentials. Rig retains token refresh and account resolution, with device-flow fallback disabled for ordinary requests. Explicit trusted local login uses an empty candidate to require fresh authentication; only successful publication establishes a new lease. Static API-key providers are unchanged.

Each provider auth file atomically stores a versioned lease and credentials together. Lease start/deadline survive refresh, runtime reconstruction, reload and reconnect; persisted last-check time rejects clock rollback until the clock recovers or a new local login succeeds. Missing, malformed, legacy or expired authorization fails closed, without migration to a new lease. Atomic creation of a provider-scoped `.json.lock` directory serializes clients/processes; the owner removes it on completion, error or cancellation. Waits are cancellable and bounded to 30 seconds. Unlike pi's heartbeat/stale recovery, orphan locks from abrupt termination require manual removal after other Phi processes stop. Rig writes only a private candidate; failure/cancellation discards it, and atomic rename commits complete credentials plus lease. A directory-sync failure after that commit is a durability warning, not a failed-login rollback. Already issued requests may finish after expiry. These contracts do not claim resistance to external filesystem races or a trusted wall clock.

Runtime validates a complete catalog descriptor against provider/backend protocol. Model capability, usage/cost identity, output limit and reasoning mapping derive from that resolved value; explicit overrides retain their defined precedence. Subagent model selection repeats validation, not inherited backend guesses.

Provider usage observation preserves unknown fields separately from reported zero where adapter evidence permits; zero-default Rig metrics alone cannot prove a reported zero. Messages projects these observations without token estimates and reports cost only with sufficient counts and known rates. Agent Transcript accounting retains its numeric defaults and text-based output fallback. Both paths use the existing model cost formula; provider observation never adds token estimates.

Submitted images are first-class prompt content. Local files snapshot before submission; MIME/data/capability and decoded-size validation precede hooks/history mutation. Provider-neutral user and assistant image content persists and replays without dependence on mutable local paths or UI decoding.

Reasoning controls and replay remain typed and provider-specific. Final assistant content retains provider-neutral reasoning identity/metadata; provisional deltas are not history truth.

### Provider history projection

Requests use the canonical Transcript's selected branch after `effective_context()` applies Compaction/Reset boundaries. `convert_history_for_target()` projects it for the accepted provider/API/model, including compaction retries. `ProviderHistoryPolicy` handles provider-specific images afterward. Target-free `convert_history()` remains a generic view, not the request path.

Assistant entries record source provider/model, API when known, and optional provider-issued message ID. Direct SDK assistant writes require an API argument; missing historical API remains unknown, never evidence of a match. Display/event IDs are separate and never substitute for absent provider IDs. Phi tool-call correlation IDs are separate from provider call/item IDs.

Complete responses with an exact provider/API/model match retain protocol identity and reasoning metadata. Mismatched and `error`/`aborted` responses retain visible content, render nonempty reasoning as text, and drop protocol identity and opaque reasoning data. Partial responses are not discarded. Rig serializes id-less Responses assistant text; Phi neither fabricates `msg_` IDs nor infers origin from prefixes.

Request projection correlates tool results with their calls, including recurring IDs. Calls without native provider identity use target-specific ID rules owned by provider conversion; changed IDs use deterministic hash suffixes and request-local collision checks. Calls and results share the mapping. Native replay identities remain unchanged; optional wire IDs omitted by Rig are not fabricated.

Unanswered calls receive request-only `No result provided` results before the next assistant/user message or request end. Intervening system messages follow the completed exchange; orphan or duplicate results are omitted from requests, including after context edits remove calls. These repairs neither mutate Transcript nor execute tools. Gateway history remains a display projection, not replay authority. See [history projection](../phi-sdk/src/context.rs), [provider conversion](../phi-sdk/src/provider/generation.rs), [streaming](../phi-sdk/src/harness/agentic_loop/runner.rs) and [reduction](../phi-sdk/src/core/reducer.rs).

Catalog updates validate complete source candidates, atomically persist before publishing immutable snapshots, and preserve last valid partitions on failure. Bootstrap/source filters, schema versions, freshness limits and provider request fields belong to [models](../phi-sdk/src/models/), [provider integration](../phi-sdk/src/provider/mod.rs) and [history conversion](../phi-sdk/src/context.rs), not duplicated here.

## Lifecycle model

Lifecycle names boundaries reached by execution/application owners, independently of registered hooks. `phi_sdk::lifecycle` exposes their events, reasons and accepted outcomes; `phi_sdk::hooks` exposes typed registrations, callback errors/results, notices and timing. Generic registries, handlers and emitters remain private.

Hooks execute independent extension handlers and return event-specific outcomes; hosts apply them and retain the main execution flow and canonical history ownership. Registrations retain owner/lifetime, event and matcher groups; each handler has its own condition, optional timeout and boolean `once` (default `false`). `once: true` consumes its one execution attempt at invocation start, independent of output acceptance. There is no public frequency/scope policy or generic execution field; readiness and background timing belong to their lifecycle boundaries. Context insertion is an outcome, not a handler type or a second invocation-accounting mechanism. Memory uses ordinary one-shot and repeatable handlers.

See [Hook design](design-hook.md) for architecture, handler contracts, `once` lifetime, boundary catalog, concurrent reduction, input preparation, failure policy, completion gates and Session hook tasks. Application transition policy remains in [Agent runtime design](agent/design-agent-runtime.md#lifecycle-and-adapter-policy).

## Extensions

MCP and Lua are always compiled. Runtime configuration controls MCP activation and extension loading.

`RuntimeConfig.extensions` holds the typed `extension::ExtensionConfig`: `disabled`, `memory: Option<MemoryConfig>` and `codemode: CodemodeConfig`. Each extension owns its configuration fields and defaults.

`ExtensionConfig::is_enabled(BuiltinExtension)` resolves effective enablement without mutation: an entry in `disabled` always wins; otherwise Memory uses its configured flag (defaulting to enabled when absent), Codemode uses its flag (defaulting to disabled), and MCP is enabled. Enablement is not resource availability: MCP still requires a bound manager, and absent Memory configuration does not implicitly provide configured tools or Skills.

`ExtensionConfig::resolve()` idempotently applies exclusions to typed settings before resource preparation. It disables Codemode and materializes a disabled Memory configuration when necessary, preserving other options. Application preparation and the SDK builder resolve after their overrides, so an override cannot bypass exclusion. MCP exclusion removes service bindings through the same enablement query. The application owns user configuration format, scoped merging and feature-block parsing; see [settings ownership](design-interactive.md#settings-ownership).

Each candidate creates an independent Lua VM from captured source content and canonical source paths, without rereading files or inheriting another VM's mutable state. Registrations and keybinding identities derive from that candidate VM. One prepared extension generation supplies tools, hooks, commands, keybindings and display handlers. Ordinary extension processes share the permission runtime of built-ins, Tape, MCP, Task and teammate operations; Hook activation and process authority follow the [host policy contract](design-hook.md#workspace-trust-and-process-authority). Application runtime retains the candidate's matching resource for application-boundary operations. Serializing declarations does not expose the VM or executable registry to clients.

Builtin extensions aggregate delegated-tool restrictions through an internal extension policy entry point; execution hosts do not call Memory policy directly. Restrictions remain independent of capability activation. `extension::builtin_skills` returns bundled Skill names and availability from captured runtime configuration, including disabled declarations so applications can retire owned links. Each extension owns its Skill eligibility; applications own discovery, loading, priority, link reconciliation and diagnostics. Undeclared bundled Skills retain default availability, and user-owned resources remain untouched.

Memory's private `runtime.rs` owns its service, Tape state, original-input eligibility and keyword evidence. Hosts retain opaque `MemorySession`, `MemorySubmission` and `MemoryDispatch` handles: prepare before input transformation, confirm accounting only after successful admission, activate queued evidence at actual dispatch and notify Memory when the turn ends or aborts. Agent options and Instance execution carry handles, not Memory services, Tape state or keyword DTOs. This is a Memory-local lifecycle contract for Tape keyword authorization, not a general extension session framework. Keyword admission accounting does not control automatic context injection; that uses the Hook handler's ordinary `once` state.

Rust consumers compose focused builder methods. Applications inspect immutable adapter declarations and use explicit semantic boundary operations, not a generic lifecycle bus. Failed load/reload preserves active generation and bindings.

Command invocations can receive a scoped `ExtensionPromptSubmitter`. Lua exposes it as `ctx.agent.send_user_message`: complete text enters ordinary Instance submission with Extension source, preserving input preparation, dispatch, tool governance and canonical User history. Application runtime binds the target and generation; Lua cannot select another instance or retain the capability after the handler exits. Default delivery starts only when idle; explicit Steer/Follow-up reuse existing Instance admission. Extension-generated text is not Tape keyword consent. See [extension authoring](../examples/extensions/README.md#submit-a-message-from-a-command).

Extension process effects require trusted source, declared executable/context, command permission and sandbox policy together. Permission can restrict a capability, never expand it. Capabilities are invocation-scoped; display handlers stay side-effect free. See [extension implementation](../phi-sdk/src/extension/).

## MCP

MCP is an optional extension capability under the same registry. There is no `MCP` model tool. The Phi-governed ToolSearch loads deferred tools for direct calls; Codemode independently discovers and calls them. MCP activation, discovery-tool availability, exposure and invocation permission are independent. Removing ToolSearch does not stop connections or withdraw loaded declarations. Delegated capability availability fails closed when matching metadata/settings remove authority.

Connection pooling and reusable metadata stay private behind McpClientManager. Instance-specific permission grants, elicitation and replies cannot cross Session routing. Accepting elicitation answers server-requested input; it grants no invocation permission. Direct and Codemode calls share semantic MCP authorization and remain subject to dedicated capability/transport checks that Yolo cannot bypass. Transient metadata is valid only for its active connection generation. Cancellation closes pending elicitation so the broker can prune requests without a fabricated answer.

Background initialization is idempotent per pool; candidate handles freeze direct and deferred tool contracts, not pooled metadata or invocation authority. Snapshots/events report changes for [application contract acceptance](agent/design-agent-runtime.md#mcp-runtime-contracts).

Command transports use the complete runtime Sandbox; configured cwd does not expand writable mounts. Applications bind before eager startup, SDK Instance construction before tool use; standalone manager operations remain explicit trusted access. Pools require equal execution environments and transport policies. Rebinding creates an independent pool using only TTL-valid persisted metadata, not another connection's private metadata/authority.

SDK enforces an application-selected typed transport policy independently of Sandbox, before network or cached-tool use; it does not infer remote mode. URL-server filesystem effects cannot be contained by the client Sandbox. See [application policy](agent/design-agent-runtime.md#configuration-write-access) and [MCP implementation](../phi-sdk/src/extension/mcp/).

## Memory and Tape

Markdown memories and anchor JSONL are canonical. MemoryService lazily reconciles a deterministic materialized graph; that graph is a rebuildable read model, never a mutation source. Memory is a builtin extension under `extension/memory/`. External consumers use `phi_sdk::extension::memory`, not stores/Git/gate internals.

`MemoryHooks` registers ordinary SessionStart/SessionEnd, UserPromptSubmit and PromptContext handlers. Git synchronization and context injection are separate operations:

| Memory configuration | Default | Execution |
| --- | --- | --- |
| `hooks.sessionStart` | `["pull"]` | SessionStart; in-process New/Resume skips these actions |
| `hooks.sessionEnd` | `[]` | SessionEnd |
| `hooks.userPromptSubmit` | `[]` | Each non-delegated submission, before expansion and PromptContext |

Each list accepts `pull`/`push` in declaration order. Startup actions remain at SessionStart; configuring submission actions does not move or replace them. Hosts choose blocking startup or `background_startup()` readiness, which input preparation awaits. Submission actions may complete even when input is later handled or rejected. Git failures return notices; a UserPromptSubmit callback timeout prevents admission under the ordinary Hook failure policy.

UserPromptSubmit also strips an original `/memory-inject`. PromptContext selects context from transformed/expanded text through one automatic `once: true` handler and a separate repeatable explicit handler. Synchronization-before-retrieval follows host stages, not sibling registration order. Context building and Tape activity precomputation run on the blocking pool on demand, not in an application startup cache. Candidate validation or restored-history painting must not require eager Memory materialization.

Git fetch/clone/push retry recognized transient transport failures up to three total attempts with bounded delays. Classification stays at the raw Git process boundary with deterministic diagnostic locale; unknown errors, authentication/configuration failures, conflicts and local Git operations are not retried. Local parsing, context building and precomputation also run without retries.

Automatic context injection requires `injection: "message-append"`; omitted or `false` disables it without disabling Memory tools or prompt synchronization. Its first eligible handler invocation consumes the one-shot allowance even on empty content, error, timeout, cancellation after start or discarded/rejected contributions. Recovery may warn but never rearms it. Reset, compaction and navigation do not reopen it; reload preserves continuing handler state and a new Instance starts fresh. `/memory-inject [prompt]` independently rebuilds context on each explicit request regardless of automatic settings or prior use; it does not consume the automatic handler's allowance. An omitted prompt provides no selection query. Memory callbacks use shared Hook deadlines. Explicit unavailable/disabled/failed injection rejects preparation. Memory injection never mutates the runtime system prompt. Delegated children receive neither automatic injection, per-submission synchronization nor keyword permission; permitted read-only retrieval remains available. Typed injection notices describe source, item count and explicit intent, not rendered-tag parsing.

Smart selection combines source-validated graph state with recent file activity as transient evidence and uses one canonical query ordering. Prompt-time precomputation does not persist a frozen candidate list. Retrieval materialization is best-effort; explicit refresh/reset remains strict. Derived paths/scores and activity ranges are not canonical persisted state.

Anchor metadata owns workspace and Git checkpoint identity. See [hooks](../phi-sdk/src/extension/memory/hooks.rs), [Memory service](../phi-sdk/src/extension/memory/service.rs), [graph](../phi-sdk/src/extension/memory/graph/), and [Memory/Tape domain designs](../phi-sdk/src/extension/memory/docs/).

### Tape mutation

TapeMutationGate is the single authorization/staging boundary using configured policy, session-scoped state and PermissionRuntime. Keyword candidates derive from original user-authored text before prompt expansion or rewrites; `PromptContext` contributes their instructions. Only Started/Queued admission records their deduplication signatures; handled, rejected or cancelled preparation leaves retries eligible. Evidence activates at actual dispatch, authorizes only the exact generated anchor during that turn, and is consumed only after durable finalization. Manual mutation uses ordinary permission; read-only actions remain distinct.

Turn completion/abort clears ephemeral keyword evidence. Model-composed tasks, definitions and role claims cannot create it. Original user-authored `@agent` input follows the master's normal preparation; child generated text does not. Delegated mutation/mixed-write tools remain denied.

Mutations finalize against the real preallocated Transcript entry ID, never Session ID. See [gate](../phi-sdk/src/extension/memory/tape/gate.rs) and [transaction/finalization](../phi-sdk/src/extension/memory/tape/transaction.rs).

## Agent orchestration API

See [Harness orchestration API](design-harnes.md#agent-orchestration-api).

## Subagents

AgentDefinition describes specialization, model intent, generic tool policy and turn limits, not instance authority, Team membership or UI metadata. Rust declarations and Markdown resources share validation, while Markdown loading reports per-file failures and retains valid definitions. Root/path ordering gives deterministic priority; direct declarations cannot silently override duplicates. Environment/model/tool availability validates at runtime, not declaration load.

Markdown field semantics and diagnostics follow [Prompt and Agent tool settings](design-interactive.md#prompt-and-agent-tool-settings); shared parsing and candidate validation follow [Tool registry](#tool-registry).

Each child uses independent instance execution and history. Manager resolves `AgentSpawnRequest.mode` and the caller's accepted `RuntimeConfig.agent_background` into an effective `AgentCreation.mode` at admission; see [delegation mode rules](agent/design-parallel-agent.md#foreground-and-background-delegation). Direct SDK spawn methods return creation facts in either mode; consumers await child completion separately.

`@agent` remains asynchronous: it is a constrained master turn, not an adapter spawn path. Its directed batch contains exactly one Agent call and stops at admission; absent, wrong-name or duplicate delegation fails visibly. Retained assignment, fallback tools, retries or directory downgrades cannot satisfy required named creation. Worktree setup is separate. See [definitions/catalog](../phi-sdk/src/agent/custom.rs) and [directed admission tests](../phi-sdk/src/harness/tests/delegation.rs).

SubagentPreparation returns a complete candidate and staged association, not task identity or startup authority. Association receives the registered idle instance; Manager starts only after it succeeds. Rejection/cancellation/abandonment compensates captured registration and association without removing a reused identity. Application runtime supplies config/resources/persistence, not another scheduler.

The child actor publishes an immutable initial result before queued follow-up execution. Later ordinary subagent answers or errors return automatically to the creator; later teammate answers or errors return to the fixed lead on idle. These reports do not duplicate initial completion or copy the child's full Transcript. Completion retains the instance, and observers cannot author outcomes.

Admission, cancellation and communication rules belong to [parallel Agent design](agent/design-parallel-agent.md). Implementation: [preparation seam](../phi-sdk/src/harness/manager/preparation.rs), [coordinator](../phi-sdk/src/harness/manager/coordinator.rs), and [delegated execution](../phi-sdk/src/harness/instance/execution/delegation.rs).

## Application and protocol boundary

SDK exposes addressable instance/Manager capabilities and semantic facts, not application transports or mutable runtime access. Application runtime modules own preparation, Session associations and persistence; main owns process finalization. Mode adapters, wire messages, correlation and scoped attachments remain outside SDK. No universal application facade is required. See [application runtime design](agent/design-agent-runtime.md).
