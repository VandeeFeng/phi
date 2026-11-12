# AgentInstance, AgentManager, and application runtime

## Scope and design intent

Phi separates SDK execution and orchestration from application preparation, Session persistence and mode policy. Embedded interactive, print, RPC and Gateway hosting use concrete application capabilities. Gateway clients access hosted Sessions through HTTP.

```text
main: configuration, composition and process finalization
  +-- interactive: serial Session transitions, interaction and projection
  +-- print: one prompt and final response
  +-- rpc: controlled execution stream over stdin/stdout
  `-- gateway: hosted Sessions for local TUI/Web clients
          |
          v
application runtime: preparation, Session associations, checkpoints
          |
          v
SDK AgentManager: harness management, relationships and inter-Agent communication
SDK AgentInstance: one Agent's lifecycle, execution and canonical live Transcript
  `-- agentic loop: one model/tool execution through completion
```

There is no universal application facade, arbitrary live-Agent attachment service or multi-root RPC product. The `phi-remote` gateway hosts application Sessions through public SDK APIs. Stdio RPC controls one root; gateway clients address gateway-owned Sessions. Independent foreground processes own separate live instances. [Parallel Agent design](design-parallel-agent.md) owns delegation protocols; [SDK design](../design-sdk.md) owns reusable execution; [application design](../design-interactive.md) owns CLI and UI workflows.

## Ownership and access model

SDK ownership is defined in [harness responsibility boundaries](../design-harnes.md#harness-responsibility-boundaries). Application responsibilities follow:

| Owner | Responsibility | Excludes |
| --- | --- | --- |
| AgentRuntimeFactory | Complete isolated candidates and matching resources | Active Session, scheduling or commit |
| RuntimePreparation | Candidate preparation, accepted resource/default generations and child preparation policy | Universal operation forwarding or execution |
| SessionAssociations | Application bindings, transitions, locks and finalization | A second SDK membership authority |
| AgentSession | Session metadata, matching resources, transition/checkpoint gates and persistence obligations | Another live Transcript or execution loop |
| AgentAttachment / SessionAccess | Scoped internal access and coherent observations | Agent lifetime or implicit cancellation on drop |
| Mode adapters | Admission/exit policy, wire conversion or presentation | Canonical execution/history or persistence authority |

Application code can use SDK handles internally. External RPC callers receive only supported messages and safe observations, never providers, permission runtimes, extension VMs, MCP managers or sandboxes. Execution progresses without observers.

## Concepts and terminology

A definition specializes an Agent; an instance is a live execution identity. Session is the durable application envelope, not an execution owner. Session, instance and Transcript IDs stay aligned. A runtime generation is replaceable without changing identity, history or queues. A RuntimeView observes a matching Session, snapshot and declarations; it is not replacement authority.

Immediate and Follow-up submissions can start turns; Steer can join active work. Submission receipt, turn outcome, message delivery and Session persistence are distinct facts. User intent may span multiple submissions and turns. See [domain terminology](../design.md#intent-submission-prompt-and-turn).

## SDK execution and application recovery

See [SDK Agent Instance runtime](../design-harnes.md#sdk-agent-instance-runtime).

### Snapshot and event recovery

Lightweight snapshot subscriptions expose owned values, never publisher guards that could pin Stop or retirement. A coherent SDK view captures history and execution at one actor boundary. AgentSession wraps it with matching application metadata/declarations while serializing against replacement.

Interactive recovery subscribes before requesting a marked view, installs recovered state, discards only through the matching ViewBoundary and then consumes later events. Lag or a missing marker requires retry or explicit failure, never blind queue clearing. Stopped recovery does not restart execution. Queue chrome reconciles pending submission IDs, not prompt text or reconstructed payloads.

Pending interaction reads are lightweight and side-effect-free. Replies return actual SDK answer status and confirmed AllowAlways deltas; queuing a reply is not approval. Stale or wrongly scoped request IDs fail.

RPC does not expose a generic snapshot/history/recovery service. Its internal ordered observation and quiescence boundaries remain necessary for correct output and drain. See [SDK views](../../phi-sdk/src/harness/instance/handle.rs), [application recovery](../../phi-interactive/src/runtime/session.rs) and [attachments](../../phi-interactive/src/runtime/attachment.rs).

## SDK AgentManager and parallel orchestration boundary

See [Harness orchestration boundary](../design-harnes.md#sdk-agentmanager-and-parallel-orchestration-boundary).

## Application runtime construction

### AgentRuntimeFactory and resource isolation

Factory resolves configuration, target history and initial grants into a complete candidate. SDK construction creates fresh permission state, fixed interaction routing and an independent extension VM. Application runtime retains the candidate's `AgentRuntimeResources` for live grants, extension operations and Session boundaries, rather than using factory services as live state. Sandbox policy matches the target instance/generation. Immutable catalogs and sources may be shared; MCP connections/metadata may be pooled, but grants, elicitation and replies remain scoped. See [construction resources](../design-sdk.md#construction-and-prepared-resources).

Candidates resolve instructions, Skills, extensions, permission roots and sandbox from the target Session working directory. Reload binds that directory before startup, captures extension sources and retains child preparation. SDK creates the candidate VM from that snapshot without rereading files; application operations use the exact prepared VM. No failed preparation mutates active resources. Startup may report an extension-load warning and use an empty runtime; explicit reload/replacement is strict.

See [factory](../../phi-interactive/src/runtime/factory.rs) and [preparation](../../phi-interactive/src/runtime/preparation.rs).

### Prompt resources

Each accepted generation retains command declarations, template content, diagnostics and auto-compaction policy. Its application `PromptExpander` expands leading Prompt/Skill commands for Interactive, Print, RPC and SDK sources after `UserPromptSubmit` hooks and before `PromptContext`, contributing content plus `TurnToolPolicy`, never grants. `UserPromptExpansion` hooks may block expansion or add context; the expander, not a hook, performs the operation. Internal/Extension input and reserved `/memory-inject`/Workflow prompts bypass this expansion. Explicit expansion previews use the same committed resources without submission, history changes or permission activation. Skills retain prompt-time reads; builtin symlink setup is process startup policy.

A `BuildSystemPrompt` hook appends available Skill metadata for non-delegated generations instead of modifying ContextFiles. Delegated system prompts omit this summary; SDK still applies system-prompt hooks to each specialized initial/follow-up run.

Loaders retain validated Prompt declarations and original file/field/line diagnostics. Expansion never reparses YAML. Candidate MCP contract validation can revalidate frozen sources without file reads, retaining priority and valid resources; fatal candidate failures preserve the active generation. See [resource contracts](../design-interactive.md#prompt-and-agent-tool-settings).

## Runtime preparation and AgentSession

The transition gate serializes replacement and coherent reads; the checkpoint gate serializes writes/finalization. Permission accounting has a separate gate because lifecycle hooks may await permission replies. Association map locks remain short-lived.

### Creation, open and resume

SessionWorkflows resolves new/saved intent, validates origin/identity, prepares a complete candidate and acquires the applicable lock before registration. Association and checkpoint/finalization observers are installed before SessionStart. Failure after registration requires compensation; preparation alone is not ready access.

First process runtime emits Startup, whether fresh or restored. In-process new/resume use their own reasons. Stopped delegated Session execution remains rejected until validated reconstruction is supported; historical reads and scoped live child views remain available. Durable IDs identify data, not executable caller-authored Sessions.

Interactive can transition serially between roots. RPC initializes one root, fresh or saved-ID, and never accepts another root or arbitrary live attachment. See [workflows](../../phi-interactive/src/runtime/preparation.rs) and [associations](../../phi-interactive/src/runtime/associations.rs).

## Candidate-first runtime replacement

```text
prepare complete isolated candidate
  -> SDK validates identity, branch and readiness
  -> SDK accepts execution generation
  -> non-fallible matching application resource/metadata swap
  -> capture coherent accepted view under transition guard
  -> checkpoint; report save warnings independently
```

Preparation/admission failure preserves active state. Acceptance retains identity, directory, grants, specialization and original delegation linkage. SDK merges confirmed active grants only at acceptance; preparation never shares or rebinds the active checker/channel. A different Transcript ID requires a different instance. A failed save after SDK acceptance cannot roll back only application state or be presented as replacement rejection.

### Runtime configuration authority

Separate raw settings, startup intent and accepted runtime configuration. The shared application SettingsManager owns settings reads/refresh and retains global/project/merged settings; resource consumers query memory only. Application preparation refreshes explicitly, while missing/mismatched managers fail at resource loading. Manager refresh is not runtime acceptance. See [settings ownership](../design-interactive.md#settings-ownership).

- Raw settings merge global/project configuration and retain defaults and Switch definitions. Never overwrite raw defaults with effective Switch values.
- CLI provider/model select the initial runtime, not permanent overrides. Explicit process options such as `--no-tools` remain effective across selection and Session changes. Derive build options per candidate; never reuse resolved startup options as overrides.
- Explicit Switch selection resolves provider, model, thinking level, mode and prompt from the Agent's accepted Switch definitions and defaults. It applies only those parameters, retaining unrelated runtime configuration and loaded resources/catalog; it never refreshes settings or rediscovers resources. Definition/resource updates require explicit reload. Unspecified thinking inherits the unmodified `defaultThinkingLevel`. SDK parameters, tools, prompts and footer must reflect the same accepted resolution, not independently reconstructed values.
- `/new` captures Session, source settings and effective configuration from one accepted generation under the transition guard. Preserve actual runtime values, not just the Switch name; do not rederive them from startup defaults or unaccepted file edits. Missing source configuration is an error, not permission to fall back globally.
- Thinking-only selection is an application RuntimeSelection, not a direct SDK state mutation. It clamps the requested standard level to the accepted model descriptor and reuses candidate-first replacement; unchanged levels are no-ops. It preserves actual model, Switch identity, mode, prompt and loaded settings/resources without editing defaults. Subsequent assistant records reference the actual execution SwitchSnapshot; selection alone creates no snapshot.
- Model-only selection preserves effective Switch fields and accepted settings/resources; it records temporary selection without changing saved defaults. Resume prepares the target-scope environment and restores branch execution selection; validate provider/model together. Configuration history commits with SDK acceptance.
- Reload accepts refreshed configuration and reapplies the current Switch, intentionally overriding temporary model/thinking selection. Record changed runtime selection so later `/new` cannot restore stale branch values. Background MCP contract rebuild preserves the accepted generation. MCP Save only persists MCP settings; it never refreshes settings/resources, replaces runtime or changes Session history. Saved MCP configuration takes effect on explicit reload.
- Model selection queries the latest catalog. Each prepared generation retains an independent catalog snapshot, including model descriptors and custom provider definitions; in-place catalog updates cannot invalidate retained-environment navigation, child preparation or `/new`.
- Picker seeds/drafts support presentation and settings edits, never runtime acceptance. Saving settings and accepting a runtime selection are separate operations.

Selection alone does not publish a new default factory. Reload emits Reload boundaries and publishes resources for future creation; other instances retain their generations.

### Navigation and recovery

Ordinary tree/rewind navigation resolves canonical entries and validates the target before SDK acceptance. Rewinding a User entry selects its parent and returns the draft to the editor; explicit Entry intent selects that entry. Same-Session navigation retains identity, live model/Switch/thinking/mode and accepted settings/resources. It changes the in-memory cursor only: no entry, configuration snapshot, history deletion, settings refresh or save.

Failure entries are valid Entry targets; selection moves the cursor without retrying execution or restoring runtime error state.

Disk reopen selects the last appended canonical entry, not the last navigation position. Journal metadata contains no navigation cursor; historical metadata `leaf_id` is ignored. Unrelated metadata/permission checkpoints and finalization cannot commit a transient cursor. Save observations describe the durable last-entry boundary, not the live navigation leaf. Without an appended entry, exiting after navigation reopens the previous branch.

Continuation starts appending ordinary input/response entries at the selected leaf while retaining the current model, thinking level, Switch and loaded environment. It does not reapply Switch definitions or inherit the historical target's runtime. Assistant records contain actual provider/model, source API when known, and `switch` identity from the accepted execution generation, including partial/error/aborted responses. Historical provider IDs and protocol-specific reasoning/tools are projected for the target model at the next request, not rewritten on Switch acceptance. See [provider history projection](../design-sdk.md#provider-history-projection). Switch definitions, thinking/mode and resource contents are not copied into an execution snapshot. Navigation/continuation append no synthetic `RuntimeConfigChange`; initial selection and explicit configuration operations remain configuration records.

Disk recovery is separate: newest branch configuration or assistant attribution supplies provider/model; newest configuration or Switch-bearing assistant supplies Switch identity. A response's recorded Switch resolves through current definitions, including existing built-in mode defaults; its defined thinking/mode/prompt fields take precedence, and still-unspecified fields retain historical configuration fallback. Actual provider/model are not replaced by the Switch's model default. Later explicit configuration records take precedence; old assistant records without `switch` remain valid and do not erase known selection. Reopening loads the current environment, not a historical resource VM.

```text
historical target: A / default / low  / default
live selection:    B / plan    / high / plan
navigate + reply:  keep live selection; record B + plan
reopen:            B + plan; resolve plan fields, then missing-field fallback
/reload:           reapply plan, including its model default
```

Pi retains live model/thinking/environment during navigation and continuation; Phi additionally retains and records Switch identity. A temporary `/model B` may differ from the model declared by `plan`, so neither field substitutes for the other.

`session_tree` is an extension notification, not an automatic history entry. A branch-leaving summary, when explicitly requested, belongs in `branch_summary`; an explicit label belongs in `label`. Plain navigation must not synthesize either. Phi currently exposes neither summary nor label navigation operations; these boundaries do not introduce new commands or journal variants.

Cross-Session anchor jumps prepare Resume before stopping the outgoing root, then navigate separately: accepted Resume plus navigation failure returns the incoming Session and a separate error, not fictitious rollback. Accepted transactions survive waiter loss.

### Same-Session context reset

Scoped reset uses SDK idle-only admission. Busy neither queues nor aborts work. Reset preserves identity/history/usage/configuration/grants, adding a Reset marker and optional handoff to effective context. AgentSession retains transition ownership through SDK reset and coherent capture, then checkpoints. Reset does not rearm one-shot Hook handlers, including automatic Memory injection. Save failure is retryable, not rollback. Reset emits no Session boundary or turn and has no RPC control or TUI slash command.

### MCP runtime contracts

AgentSession observes catalog/readiness and accepts isolated candidates only against the captured generation. Unchanged contracts do not replace runtimes; failed work remains retryable. Shared connections do not imply shared grants or Session mutation.

Interactive panels own drafts/forms/notifications; scoped application operations own reads, reconnect, cancellation, selection and saves. MCP Save validates tool identities against available metadata and persists only changed exposure settings through protected, serialized read-modify-write. Saved exposure may differ from live exposure until explicit reload; live exposure is not the persistence baseline. Save neither prepares a candidate nor checkpoints a Session, and unrelated invalid settings/resources cannot block it. Admitted metadata/reload work survives waiter loss. Subscriptions follow replacement.

RPC exposes no MCP controls, dedicated MCP events or elicitation question/answer messages. SDK MCP tool execution remains available under process security policy. A blocking interaction other than permission fails RPC explicitly and triggers cleanup; it is never silently approved or left waiting indefinitely.

### Configuration-write access

Process security is selected from `--rpc` before startup effects. Global-only boolean `trust_remote` is captured once; changes require restart. It relaxes optional file guards, not permission, sandbox, authentication or RPC capabilities. Global/project settings and auth remain write-protected under RPC regardless of trust or Yolo. Missing settings protect their directory until locally created and re-prepared.

Protected RPC execution permits only sandbox-bound Command MCP. URL servers, including loopback, are blocked before connection or cached-tool use. Ordinary local mode permits URL MCP. Removing RPC MCP controls does not change these execution restrictions. Local users may edit files and request reload; invalid files neither rewrite themselves nor replace active runtime state.

Configuration guards do not replace file-tool isolation, credential policy, Session persistence or model-cache write paths. Untrusted RPC Memory is read-only; trusted execution retains ordinary sandbox/permission rules. Memory Git/Tape effects keep their existing typed paths; this is not complete isolation against trusted helpers, hooks or filesystem aliases. See [security](../../phi-interactive/src/security.rs).

### Local subscription authentication

RPC policy denies login before picker, device-flow or auth-directory effects. Local login is not a wire capability. Credential usage and lease refresh belong to SDK provider policy, not Session persistence.

### Reload semantics

Reload prepares/accepts one selected instance and publishes a new default generation only after acceptance. Other live instances are unchanged. Post-commit warnings do not undo acceptance; failed preparation/admission changes neither active state nor defaults.

## Application Session and persistence

AgentSession creates checkpoint representations from canonical SDK history. Each association writes only its own file. Automatic checkpoints retry once; explicit accepted checkpoints survive waiter loss. Permission and finalization retry obligations remain distinct.

Save observations describe association, attempt, pending operation and completed result without loading history. Unknown/missing observations never mean success. UI polls these read-only observations; RPC reports operation warnings and final cleanup status, not a generic save-observation wire service.

### Save boundaries

Finalization uses the captured SDK registration and canonical final view. Checkpoints cover completion/failure/cancellation, child observations, grants/configuration and finalization, not ordinary navigation. Saves preserve all canonical entries but never commit the transient navigation cursor. Event lag recovers canonical state rather than abandoning persistence.

Checkpoints preserve canonical Failure entries, including failures without assistant output. SDK HistoryChanged also triggers checkpointing for execution-pump failures. Reopen restores failure history, not runtime `latest_error`; local save/transport notices are not persisted. See [SDK failure semantics](../design-sdk.md#durable-execution-failures).

AllowAlways first commits SDK permission state, then application accounting records the confirmed delta and attempts save. Lost waiters do not revoke accepted work. Save failure retains applied grants and retryable metadata. No second UI grant call or wire grant-recording endpoint exists. Prompt tool policy is transient and never durable grants.

Nonpersistent checkpoints update projections without claiming a saved file. Linked/delegated metadata-only Sessions can still require real files. Invocations retain the instance's creation file. Failed writes never rewrite outcomes or redeliver messages.

See [checkpoints and save observations](../../phi-interactive/src/runtime/save.rs), [Session writes](../../phi-interactive/src/runtime/session.rs) and [storage](../../phi-interactive/src/session/storage.rs).

### Stop, finalization and retry

Explicit process policy requests stop; SDK closes the captured creation subtree and publishes reliable registration finalization. Application checkpoints outstanding obligations, emits SessionEnd once, saves final state and releases only the matching association/lock. Stop, lifecycle end, save and release are separate facts.

Cleanup/save failure retains closed associations, canonical evidence and locks for retry. Other peers still receive cleanup attempts. Retry must not repeat SessionEnd. Old callbacks cannot remove a reused identity. Observers request checkpoints from SDK facts; they do not author outcomes or independently stop instances.

`main.rs` owns finalization of process-owned Sessions for embedded interactive, print, RPC and Gateway service modes, including mode errors and startup MCP shutdown. Print output is published only after successful cleanup. Embedded interactive stops terminal input before awaiting cleanup while TerminalGuard remains alive. Gateway TUI exit disconnects without finalizing hosted Sessions. RPC keeps input/output supervision active through main-owned finalization and final flush.

## Session identity and multiple terminals

Independent foreground processes have independent SDK Managers and application associations. Shared storage leases prevent deletion of associated Sessions; short journal locks serialize reads and branch-preserving appends. Reopening disk history creates an independent instance, not live attachment. Gateway clients reconnect to a gateway-owned live instance. Scoped internal attachment clones do not own lifetime.

Every persisted Agent has its own Session journal. Root and teammate journals live in their working-directory scope; ordinary subagents inherit the parent's storage association and use `<parent-session-id>/subagents/agent-<child-session-id>.jsonl`, extending the ancestry for nested children. Follow-ups preserve the file and creation linkage. File placement grants no execution authority. See [ordinary sidechains and teammate history](design-agent-task.md#ordinary-sidechains-and-teammate-history).

## Lifecycle and adapter policy

| Boundary | Policy |
| --- | --- |
| First runtime, including RPC saved-ID initialization | Startup |
| Interactive new/resume | New / Resume |
| View switch or attachment clone/drop | No lifecycle boundary |
| Same-ID reload | Reload end/start |
| Explicit stop / process shutdown | Typed end reason once |
| Turn/child completion | Checkpoint as applicable, not implicit SessionEnd |

### Memory administration

Process-local Memory administration freezes its startup configuration and the captured `startup_dir`, even when the initial Session resumes elsewhere. Selected Session changes do not rebase it. Agent-generation Memory preparation instead follows the target Session directory. Review retains an opaque context/service subset until released; UI receives observations, not stores. Rewind annotations filter by canonical Session identity, do not navigate or synchronize Memory, and degrade without inventing anchors. These local workflows have no RPC endpoints.

### Memory preparation

The application factory installs `MemoryHooks` on the prepared SDK generation; AgentSession retains matching `AgentRuntimeResources`, not a startup task, precompute cache or injection guard. The Hook subsystem owns readiness, notices and per-handler `once` state. Memory callbacks own Git synchronization and content generation; Instance owns preparation, queueing and Tape authorization, not automatic-insertion accounting. Startup synchronization remains at SessionStart; optional submission actions run at UserPromptSubmit. See [Memory defaults](../design-sdk.md#memory-and-tape) and [Hook integration](../design-hook.md#prompt-preparation-and-memory).

AgentSession awaits readiness before submitting original input and maps typed hook notices for adapters. Retained submission work holds the transition guard through the SDK receipt despite waiter loss. Preparation waits remain cancellable; SDK abort/stop cancels pending preparation without admitting stale input. Automatic injection consumes Once when its handler starts, not on Started/Queued admission; rejection or discarded output does not restore it. Reset, compaction and navigation do not rearm it; replacement preserves continuing registrations. Tape keyword admission accounting remains separate.

Automatic Memory build failure warns and continues with its attempt consumed. Explicit injection is repeatable and rejects unavailable/disabled/failed context; an omitted prompt provides no selection query. These Memory callback policies are shared by application submission paths, not implemented by adapters.

### TUI transition behavior

New/resume prepares the target and checkpoints before ending the outgoing root. Selecting the current identity is a no-op. Same-ID reload replaces a generation; Quit explicitly stops before process exit. Preparation/checkpoint failure preserves the old root; failure after stop does not promise resurrection. UI installs only accepted Session/declarations, restoring editor text for rewind. These workflows do not imply independent-root RPC creation.

## Client contract and deployment modes

Modes call concrete capabilities directly: SessionWorkflows for creation/transitions, RuntimePreparation for candidates, AgentSession/attachments for scoped work and SessionAssociations for binding/finalization. No mandatory forwarding chain or transport is imposed on interactive/print. RPC is a mode adapter, not the application's internal API.

### Transport adapters and access boundary

`phi --rpc` uses strict version-2 tagged JSONL, not JSON-RPC method envelopes. Initialization selects fresh or saved-ID intent for one root. Caller-authored Session, Transcript, working-directory overrides and runtime collaborators are rejected. Stdio inherits launcher authority; initialization is not authentication or multi-tenant authorization.

Ordinary inputs and admissions have input/submission identity; SDK outcomes have turn identity; controls/questions have independent correlation. Steer need not create a turn. Bounded workers keep replies and interruption responsive. Observation loss and output failure are fatal, not successful drain. EOF drains admitted work unless a required permission answer is unreachable; explicit close chooses drain or cancel. Drain keeps permission answers and cancellation live, rejects new inputs and can escalate to cancel. Final closed output reflects cleanup and output health; successful flush is not proof the caller consumed bytes.

See [messages](../../phi-interactive/src/rpc/messages.rs), [I/O](../../phi-interactive/src/rpc/io.rs), [lifecycle](../../phi-interactive/src/rpc/mod.rs) and [subprocess tests](../../phi-interactive/tests/rpc_stdio.rs).

### Wire surface and semantic scope

Controls are initialization, interruption, runtime info, model/switch selection, reload, prompt expansion, scoped task stop and close. Questions are permission-only. There is no MCP management/elicitation surface, arbitrary subscriptions, Session/history administration, raw Manager forwarding, terminal navigation, local settings/auth writing or compatibility aliases.

Local model/Switch drafts and persisted definitions remain distinct from accepted runtime state. Safe startup catalog/seed data supports menus without exposing credentials. Configuration writes return typed saved/denied/failed outcomes and cannot gate independent selection. Catalog maintenance, Memory administration and diagnostic-only `/mcp-auth` stay local. Interactive command hooks and extension effects remain scoped application collaboration, not automatic wire endpoints.

## Interactive projection

UI consumes owned snapshots, coherent recovery and bounded events. It never owns execution or durability. Child details are observation/navigation only; opening or returning does not schedule, consume outcomes, stop or checkpoint. Existing child authority and persistence remain governed by [parallel Agent design](design-parallel-agent.md).

## Module layout and implementation index

| Responsibility | Source |
| --- | --- |
| SDK execution and orchestration | [instance/](../../phi-sdk/src/harness/instance/), [manager/](../../phi-sdk/src/harness/manager/) |
| Process composition/finalization | [main.rs](../../phi-interactive/src/main.rs) |
| Candidate preparation and Session workflows | [preparation.rs](../../phi-interactive/src/runtime/preparation.rs) |
| Candidate construction | [factory.rs](../../phi-interactive/src/runtime/factory.rs) |
| Associations/transitions/cleanup | [associations.rs](../../phi-interactive/src/runtime/associations.rs) |
| Session/resources/checkpoints | [session.rs](../../phi-interactive/src/runtime/session.rs), [save.rs](../../phi-interactive/src/runtime/save.rs) |
| Scoped access | [attachment.rs](../../phi-interactive/src/runtime/attachment.rs) |
| Descendant preparation | [subagents.rs](../../phi-interactive/src/runtime/subagents.rs) |
| RPC adapter | [rpc/](../../phi-interactive/src/rpc/) |
| Durable envelope/lookup/JSONL | [session/](../../phi-interactive/src/session/) |

## Conformance invariants

One SDK execution and membership authority; no UI-driven progression or universal application facade. Complete candidates precede mutation. Accepted execution/resources remain aligned after save failure. Recovery preserves actor ordering. Finalization uses the captured registration. Failed saves retain retry obligations without outcome/lifecycle replay. Mode exit policy stays explicit.
