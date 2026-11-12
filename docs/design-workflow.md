# Dynamic Workflow design

## Status and intent

Proposed design, not an implemented capability. Phi already has independent Agent execution, caller-bound Agent APIs and an isolated JavaScript engine. Workflow connects these through a harness-owned orchestration runtime; it does not introduce another Agent executor.

### First release decisions

- Background execution and complete managed stop; no pause, resume, worker restart or automatic attempt retry.
- `agent(prompt, { role?, label? })` returns text. `role` selects an existing definition; omission uses an explicitly configured, validated default role. If no default is configured, omission is a contract error. Never select the first catalog entry or synthesize a definition per call.
- Defer `schema`, per-invocation `model`, `isolation` and `stallMs`; reject unsupported options. Definition-level model/tool policy remains effective. Structured extraction examples below describe the later surface, not first-release behavior.
- Support bounded `parallel()` and streaming multistage `pipeline()`, phase/log observations and one terminal report. No hard token budget in the first release.
- Use a shared 300-second wall-clock deadline for each Workflow script execution, including all Agent calls, composition and permission waits; managed cleanup follows termination. This is a first-release simplification: 300 seconds is considered sufficient for routine Agent tasks. Execution without a whole-run deadline is a future direction, not a current guarantee.
- Bind a run to its creator's registration and captured runtime generation. Reject creator runtime replacement/reset or Session/branch rebinding while its runs are active; ordinary main-session turns may continue. Stop runs before explicit context/resource transitions.
- Admit a dedicated run capability through the governed Workflow launch. Never preserve the launching turn's transient approvals, raw permission channels or general SDK authority. Creation is limited by captured definition/capability ceilings and current registration/security checks; worker tool effects use ordinary isolated permission runtimes.
- Workers remain ordinary creator-descendant Agents, never teammates. Run-owned result delivery is explicit and independent of foreground/background defaults. Do not use `label` as a Team alias.
- Workers are invocation-scoped: capture completion, explicitly stop each worker subtree through Manager and release live membership before returning its result/slot. Persist history through existing application finalization associations; do not retain live workers for follow-up messaging. Unresolved cleanup retains run supervision and prevents a settled terminal status.
- Run stop closes admission first, cancels queued/startup work, stops captured worker subtrees and awaits compensation/cleanup. Successful run completion also settles every admitted worker, including unawaited calls. Cleanup failure is not successful completion.

Later lifecycle and replay sections record the target design and do not expand this first-release scope.

Long conversations mix planning, execution and intermediate results in one context window. This makes exhaustive work, independent verification and preservation of constraints harder. Workflow moves the plan into executable code: loops enforce coverage, separate workers perform independent checks, and explicit prompts carry constraints. Code makes these rules enforceable; it does not guarantee correct decomposition or judgement.

The main Agent understands the request and writes a script. The script holds control flow and intermediate results. The harness executes it and delivers one terminal report. Script execution makes no model requests of its own; worker execution consumes tokens.

Related: [harness](design-harnes.md), [Agent API and parallel Agents](agent/design-parallel-agent.md), [SDK](design-sdk.md), [Task and Team](agent/design-agent-task.md), [application runtime](agent/design-agent-runtime.md), [tools](design-tools.md), and [worktrees](agent/design-worktree.md).

## Architecture and ownership

```text
Human intent
    |
    v
Main Agent                         understands request, writes JS
    |
    | Workflow tool / typed SDK request
    v
harness/workflow/                  owns live Workflow runs
    |
    +-- phi-codemode                isolated JS execution
    |     control flow, variables, composition helpers
    |
    +-- scoped Agent capability     caller identity, policy, admission
    |           |
    |           v
    |       AgentManager            membership, creation, managed stop
    |           |
    |           v
    |       AgentInstances          independent loops and Transcripts
    |           |
    |           `-- completion ---> script variables
    |
    +-- run journal                 invocation order, attempts, results
    `-- observations / terminal report
                |
                v
Application runtime                Session association and persistence
    |
    v
UI / print / RPC                   supported projection and interaction
```

Workflow implementation belongs under `phi-sdk/src/harness/workflow/`. Start with coarse modules for run execution, script hosting and journal/replay; do not create a separate scheduler service or split modules before responsibilities require it.

| Owner | Responsibility | Not its responsibility |
| --- | --- | --- |
| Main Agent | Generate or revise orchestration and report results | Decide every worker admission while the script runs |
| Workflow runtime | Run lifecycle, invocation queue, concurrency, attempts, replay and terminal delivery | Execute provider/tool loops or own worker Transcripts |
| Script | Choose prompts, branches, loops and stage composition | Grant authority or directly create processes |
| AgentManager | Validate registrations, relationships, capacity, creation and managed stop | Interpret workflow stages or decide business retries |
| AgentInstance | Worker execution, tools, interactions and canonical Transcript | Own the Workflow run |
| `phi-codemode` | JS sandbox, host bridge, execution limits and cancellation | Agent semantics, permissions, Sessions or durable run storage |
| Application runtime | Prepare resources, associate Sessions, persist journal and script snapshots | Own a second live run registry or schedule workers |
| UI | Project run/phase/worker observations and submit control intents | Advance stages or infer completion |
| Main | Compose resources and finalize runs and Agents | Plan workflow work |

Scheduling remains in the harness. Script control flow determines what work is requested; runtime queues and admits it. A worker's model decides only how to perform its own invocation. Main-session turns are not needed for each stage transition.

## Independent concepts

| Concept | Meaning |
| --- | --- |
| Workflow definition | Script metadata and body, optionally saved for reuse |
| Workflow run | One addressable background execution with immutable input snapshots |
| Invocation | One logical `agent()` call with a stable start-order position |
| Attempt | One execution of an invocation; restart creates another attempt |
| Worker | Independent AgentInstance executing an attempt |
| Phase | Display grouping for invocations, not a DAG node or barrier |
| Checklist Task | Recorded work managed by the existing TaskList |
| Background activity | Application projection of a run, distinct from a checklist Task |

A Workflow is not an Agent Team and does not use shared TaskList eligibility to schedule work. Workers are ordinary subagents, not teammates. They may use permitted Task capabilities, but checklist state never substitutes for invocation completion or replay state.

The run owns invocation-to-worker associations. AgentManager remains the only Agent membership authority. Existing creation lineage remains authoritative for descendant closure; run control targets its captured workers, not all of the creator's children.

## Semantic contract

This section fixes the first-release vocabulary, ownership and internal contracts. Later replay/attempt descriptions do not introduce first-release types. Rust sketches describe semantic fields, not public visibility or concrete channel/lock choices. Keep fields private and expose only supported operations through curated facades.

### Reference and adaptation

| Claude public contract | Fixed Phi interpretation |
| --- | --- |
| Script holds control flow and intermediate results | One harness-supervised run executes JS; main-session turns do not schedule each stage |
| `agent(prompt, options)` creates a subagent | One invocation requests one ordinary AgentInstance through the existing Agent API |
| Figure A uses `agentType` | Phi uses `role`, matching `AgentSpawnRequest.role`; no compatibility alias |
| Workflow tool returns an asynchronous receipt, then a completion | Launch acceptance, live observation and terminal outcome are distinct facts |
| SDK distinguishes background `taskId` and Workflow `runId` | Use `WorkflowRunId` for run identity; an adapter activity ID, if required, is not a checklist Task or another execution owner |
| `parallel()` accepts deferred functions; `pipeline()` streams stages | Runtime admission controls execution; phases and helpers do not become independent schedulers |
| Batch helpers retain failed entries as null; directly awaited operational failures can fail the script | Preserve typed internal failures and map them at the composition boundary, not by matching error strings |
| Stopped workers must exit before relaunch | Complete managed closure before releasing worker ownership or reporting a settled run |

Sources: [script and lifecycle guide](https://code.claude.com/docs/en/workflows), [SDK launch/receipt](https://code.claude.com/docs/en/agent-sdk/typescript#workflow), and [official article Figure A](https://claude.dev/blog/a-harness-for-every-task-dynamic-workflows-in-claude-code/). These establish public behavior, not Claude's internal struct names, process model or registry layout. Generation exclusion, invocation-scoped worker release and the following Rust vocabulary are Phi decisions.

### Vocabulary and identity

| Term / type | Meaning | Must not mean |
| --- | --- | --- |
| `WorkflowDefinition` | Validated metadata and script body | Live execution or permissions |
| `WorkflowRequest` | Definition and captured JSON arguments for one launch | Caller identity or SDK collaborators |
| `WorkflowRunId` | Identity of one accepted run | Session ID, Task ID or AgentInstanceId |
| `WorkflowRun` | Cloneable observation/control handle | Mutable canonical run state or lifecycle owner |
| `WorkflowRuntime` | Harness component supervising runs | Another AgentManager or application facade |
| `WorkflowScope` | Trusted generation-bound authority and preparation scope | Model-supplied configuration or permanent grants |
| `InvocationId` | Monotonic request-order identity within a run | Worker identity, completion order or recovery checkpoint |
| `AgentInvocation` | Resolved request for one `agent()` call | An Agent definition, checklist Task or reusable worker |
| worker | Actual AgentInstance assigned to an invocation | New Agent kind or separate executor |
| phase | Invocation display grouping | Stage barrier, dependency or permission scope |
| completion | Worker execution result published by AgentInstance | Resource release or Session-save acknowledgement |
| finalization | Managed closure and resource release | Model execution success |
| outcome | Immutable settled result | Progress event or start receipt |

Use `request` for untrusted launch intent, `invocation` for resolved worker work, `creation` for existing Agent admission facts, and `outcome` for results. Avoid new `AgentBridge`, `WorkflowManager`, `WorkflowTask`, `WorkerId`, generic DAG nodes or a production `WorkflowExecutor` abstraction. There is no attempt/retry identity in the first release.

### Ownership and control graph

```text
trusted composition
    |
    v
WorkflowRuntime -- owns supervision --> run execution task
    |                                      |
    | returns WorkflowRun                  +-- script execution
    |                                      +-- invocation queue/records
    |                                      +-- accepted worker obligations
    |                                      `-- finalization + terminal delivery
    |
model / SDK / UI -- supported controls --> WorkflowRun
                                           |
                                           v
                                    serialized run commands

run execution task -- scoped creation/control --> existing Agent API
                                                       |
                                                       v
                                                  AgentManager
                                                       |
                                                       v
                                                  AgentInstance
                                                  loop + Transcript
```

The execution task is the single writer of canonical run state. `WorkflowRuntime.runs` retains supervision/control records, not a second copy of run state. Creator registration associations allow owned-run closure and generation-transition exclusion; they do not make AgentInstance the owner of script variables or queues. Install ownership before the first asynchronous effect, and serialize owner closure against launch.

Dropping a handle, observation subscription, script host waiter or launch response receiver does not abandon accepted worker cleanup. A failed supervisor must transfer or settle its captured obligations through the runtime's finalization path; task failure is not proof that workers stopped.

### Definition, request and scope fields

```rust
struct WorkflowDefinition {
    metadata: WorkflowMetadata,
    body: String,
}

struct WorkflowMetadata {
    name: String,
    description: String,
    phases: Vec<String>,
}

struct WorkflowRequest {
    definition: WorkflowDefinition,
    args: Option<Value>,
}

struct WorkflowAgentOptions {
    role: Option<String>,
    label: Option<String>,
}

struct AgentInvocation {
    id: InvocationId,
    role: String,
    prompt: String,
    label: Option<String>,
    phase: Option<String>,
}
```

| Field / boundary | Contract |
| --- | --- |
| `metadata` | Parse literal metadata without executing user code; require nonempty name/description |
| `body` | Async JS body after metadata extraction; validate syntax before accepted launch |
| `args` | None is JS undefined; Some(null) is JS null; capture by value |
| `WorkflowAgentOptions` | Deserialize only role/label; unknown or unsupported fields fail before admission |
| `AgentInvocation.role` | Resolve against captured AgentCatalog before queue admission; never use label as a selector |
| `AgentInvocation.prompt` | Nonempty script-authored work text, never human approval |
| `AgentInvocation.phase` | Capture when the call is requested; later phase changes do not relabel existing work |
| `InvocationId` | Allocate in accepted request order, including queued calls; never reuse within a run |
| default role | Explicit trusted configuration validated against AgentCatalog; absent default plus omitted role fails, even if the catalog has only one entry |

`WorkflowScope` is constructed by the owning runtime path, never deserialized from `WorkflowRequest`. Fix its semantic fields as follows; existing private preparation/authority types supply the implementation rather than new universal service containers:

| Scope field | Meaning |
| --- | --- |
| `creator` | Initiating AgentInstanceId |
| `registration` | Captured AgentRegistrationId; ID reuse grants nothing |
| `generation` | Captured generation-bound resources and transition exclusion, not a freely replaceable runtime |
| `definitions` | Immutable validated AgentCatalog ceiling for this run |
| `default_role` | Optional trusted role selector, validated when configured |
| `working_dir` | Captured execution directory, revalidated through existing path/security checks |
| `agent_access` | Existing caller-bound Agent API authority; never trusted Manager administration |
| `preparation` | Existing child construction/application association capabilities with isolated per-worker resources |
| `limits` | Validated WorkflowLimits, independent of model advice |

WorkflowLimits uses explicit ceilings: `max_concurrent_workers`, `max_pending_invocations`, `max_total_invocations`, `max_result_bytes` and `max_output_bytes`. Positive values and aggregate resource bounds validate before launch. Result and output limits apply cumulatively as well as at the engine bridge; no unbounded completed-result/log collection is permitted.

### Canonical run state and owned views

| Structure | Semantic fields / responsibility |
| --- | --- |
| `WorkflowRuntime` | `runs`: run identity, creator-registration association and supervision handles |
| `WorkflowRun` | `id`, `commands`, `observations`: only control and owned observation access |
| `WorkflowRunState` | `id`, `scope`, `request`, `state`, `invocations`, `pending`, `active`, `next_invocation_id`, `current_phase`, `logs`, `cleanup_errors` |
| `InvocationRecord` | `invocation`, `state`, captured worker identity/completion while active, retained bounded outcome after finalization |
| `WorkflowSnapshot` | Owned run identity, status, phase, derived counts, bounded invocation/log views, cleanup diagnostics and optional settled outcome |
| `WorkflowSubscription` | Returns owned snapshots; no raw receiver or borrowed publisher guard in the SDK surface |

The run owns queue order and invocation facts. `pending` and `active` are scheduling indexes into records, not independent outcome stores. Derive snapshot status/counts from canonical state; do not maintain divergent completion booleans or publish provisional success. Worker identity is the existing AgentInstanceId plus captured AgentRegistrationId/creation facts; no new worker registry or identity namespace is required.

A snapshot is read-only projection, never authority to start/finish an invocation. Final report delivery is correlated by WorkflowRunId and tracked separately from observational reads, so reopening a view cannot consume or duplicate reports.

### State and finalization

```text
Run:
  Running -- script return/error --> Finishing -- obligations settled --> Completed / Failed
     |
     `------ explicit stop -------> Stopping  -- obligations settled --> Stopped

  Finishing -- explicit stop -----> Stopping

Invocation:
  Queued --> Preparing --> Running --> Finalizing --> Succeeded / Failed / Cancelled
     |           |           |
     `-----------+-----------+-- cancellation --> Finalizing
```

`WorkflowStatus` is the projection vocabulary: Running, Finishing, Stopping, Completed, Failed and Stopped. The internal `RunState` stores the transition phase and, once settled, `WorkflowOutcome`; status is derived, not independently writable. `Finishing` means normal/error script termination and cleanup. `Stopping` means explicit cancellation; normal completion must never be labelled stopping.

Capture `ScriptOutcome` when JS ends, independently of resource cleanup. Successful script return retains `Option<Value>`; failure retains structured script/runtime diagnostics; cancellation is distinct. `WorkflowOutcome` records settled run termination and that script fact. Retain typed invocation outcomes using existing SubagentCompletion/PhiFailure rather than changing all failures into strings or null.

| Situation | Required behavior |
| --- | --- |
| JS returns with unawaited calls | Close admission, cancel pending/unawaited work, finalize accepted workers, then settle |
| Worker returns text | Capture completion, finalize its subtree, release membership/slot, then resolve agent() |
| Explicit run stop | Close admission first, cancel preparation/script waits and stop captured worker subtrees |
| Cleanup fails with obligations outstanding | Remain Finishing/Stopping; retain identities and cleanup errors; stop() reports the error and can retry cleanup |
| Cleanup later settles after an error | Preserve execution and cleanup diagnostics; never overwrite the worker's canonical execution result |
| Launch waiter or script host promise disappears | Supervisor retains accepted obligations and completes cleanup |
| Whole run is stopped | Cancellation cannot be swallowed by composition helpers to continue spawning work |

A concurrency slot covers preparation through worker-subtree finalization, not only provider inference. No settled terminal status while startup compensation or managed worker closure is outstanding. Application persistence failures are reported separately through existing finalization associations; a saved view and a released registration are distinct facts.

### Errors and JS adaptation

| Category | Internal representation | Script behavior |
| --- | --- | --- |
| Invalid metadata/source/options, missing/unknown role or limit violation | Typed WorkflowError | Reject/fail; no null filtering |
| Permission/authority denial or stale scope | Typed WorkflowError retaining the cause | Reject/fail; not a retry or partial finding |
| Worker operational failure | Existing PhiFailure in invocation outcome | Direct agent() rejects; parallel/pipeline preserve a null slot |
| Worker cancelled independently of whole-run stop | Typed cancelled invocation outcome | Composition may preserve a null slot |
| Whole-run cancellation | Run control state | Stop scheduling and unwind the script |
| Managed cleanup failure | Cleanup diagnostic plus retained obligation | Cannot masquerade as an empty worker result or settled success |

The official guide's overview broadly describes null returns, while its stall-failure section distinguishes direct await from composition. Phi follows the explicit direct-await/composition distinction. JS adapters use typed host result envelopes/errors, not string matching or catch-all conversion that hides programming and permission errors. Pipeline does not invoke later stages for a failed item's null slot.

### Functions and module boundaries

```text
WorkflowRuntime.launch(request, scope)
    -> run_script
         -> JS agent(prompt, options)
              -> request_agent
              -> admit_agent          existing Agent API / Manager admission
              -> await_completion     existing initial-completion protocol
              -> finalize_worker      existing managed subtree closure
              -> resolve invocation
         -> finish_run                settle all remaining obligations
    -> terminal report                creator feedback, once

WorkflowRun.stop()
    -> request_stop                   no new admission after acceptance
    -> settle pending/startup work
    -> finalize_worker for each captured worker subtree
    -> publish settled outcome
```

| Operation | Owner / authority |
| --- | --- |
| `launch`, `stop_owned`, `shutdown` | WorkflowRuntime; owned-run controls use captured registration scope |
| `id`, `snapshot`, `subscribe`, `stop` | WorkflowRun handle; no external invoke(), finish() or outcome mutation |
| `run_script`, `request_agent` | Script host adaptation inside harness/workflow |
| `admit_agent`, `await_completion`, `finalize_worker` | Run orchestration using existing owning Agent APIs; private field access stays with Instance/Manager |
| `finish_run`, `request_stop` | Serialized supervisor state transitions, not model instructions |
| `phase`, `log` | Script-local presentation operations with bounded run observations |
| `parallel`, `pipeline` | JS composition sharing the same run admission path; no separate executor or capacity pool |

```text
phi-sdk/src/harness/workflow/
  mod.rs        composition and necessary exports
  runtime.rs    supervision, queue, canonical state, controls and finalization
  script.rs     metadata/body adaptation, Codemode hosting and JS helpers
```

Keep the complete run state machine together until an actual responsibility justifies extraction. Agent/API integration may add focused methods beside existing Instance/Manager owners, without widening private members. Use existing cancellation, interaction and finalization mechanisms; do not establish parallel raw-provider or CLI execution paths.

## Script contract

The reference surface follows Claude Code's published Workflow API:

```javascript
export const meta = {
  name: "audit-routes",
  description: "Audit route handlers and verify findings",
  phases: ["Discover", "Audit", "Verify"],
};

phase("Discover");
const found = await agent("List every .ts file under src/routes/.", {
  schema: {
    type: "object",
    required: ["files"],
    properties: {
      files: { type: "array", items: { type: "string" } },
    },
  },
});

phase("Audit");
const results = await pipeline(
  found.files,
  file => agent(`Audit ${file} for missing authentication checks.`, {
    role: "reviewer",
    label: file,
  }),
  findings => agent(`Independently verify these findings: ${JSON.stringify(findings)}`),
);

return results.filter(result => result !== null);
```

This illustrates the proposed surface, not currently executable Phi code. Agent definitions must exist. Phase attribution is captured when an invocation is requested; an optional metadata phase list is presentation, not execution order.

### Source and globals

- The first statement is `export const meta = { name, description, phases? }`, with literal metadata; loading metadata never executes user code.
- The body is an async function body with top-level `await` and `return`. Metadata parsing is separate from Codemode body execution.
- `args` contains the captured JSON input, or `undefined` when omitted.
- `phase(title)` selects a display group; `log(value)` emits run-local progress.
- Only the terminal return value and terminal diagnostics become the main conversation report. Progress and worker output remain available through observations and saved records.
- No direct filesystem, shell, network, environment, module loading or ordinary `tools.*` catalog is exposed to scripts. Agents perform external work through existing governed tools.
- Replay-sensitive nondeterminism is unavailable, including `Date.now()`, `Math.random()` and argument-less `new Date()`. Supply external inputs through `args` or recorded Agent results.

Ordinary Codemode keeps its existing tool-composition semantics. Workflow reuses the engine with a different host surface; it does not turn the existing Codemode tool into a durable background service.

### Agent invocation

```text
agent(prompt, options?) -> Promise<text | JSON | null>
```

| Option | Intent | Phi mapping |
| --- | --- | --- |
| `role` | Select built-in or custom specialization | Existing `AgentSpawnRequest.role`; omission requires an explicitly configured, validated default |
| `model` | Override model for this invocation | Extend typed creation intent and existing candidate/model validation |
| `schema` | Return validated JSON | New invocation output contract; not merely instructions to emit JSON |
| `label` | Display name | Run observation metadata, not Team alias or authority |
| `stallMs` | Stall detection window | Runtime attempt watchdog |
| `isolation` | Reference API isolation intent | Initial Phi support is inherited or separately prepared execution directory; no remote mode or implicit worktree creation |

Unknown or unsupported options fail before admission. Do not silently ignore isolation or fall back to a different model/directory. Exact supported option schemas are validated as a complete request before effects.

Without `schema`, successful completion returns text. With `schema`, validate the schema before startup and validate output before publishing success. Bounded output-repair attempts must be explicit and recorded; parsing a string as JSON alone is insufficient.

Stopped workers and exhausted operational failures yield a null result within composition helpers. Directly awaited operational failures reject and can fail the run. Invalid scripts, options and schemas are contract errors, not silently missing findings. Preserve typed failure reasons in the journal and UI even when a helper returns null.

### Composition

`parallel(functions)` accepts deferred functions, not already-started promises. It requests their execution under runtime limits and returns after all settle. Results retain input positions.

`pipeline(items, ...stages)` streams each item through successive stages. Once an item's stage completes, its next stage can proceed without waiting for other items. A failed item retains a null output slot and does not pass null into later stages. Results retain item positions.

Both helpers share the run's admission queue and concurrency accounting. Nested composition must not acquire a worker slot while waiting for another composition operation; slots cover actual worker execution, not JS callbacks. Helpers do not bypass limits by calling `agent()` directly.

The script can encode barriers, verification and loops without a separate DAG format. All open-ended loops remain subject to host invocation and execution limits.

## Reusing the Agent API

Existing implementation provides:

- `AgentAccess::spawn_agent()` for caller-bound creation; trusted SDK creation remains a separate authority path.
- `AgentSpawnRequest` with role, prompt, alias, directory and delegation mode.
- `AgentInstance::complete_delegated_agent()` for immutable initial completion.
- Instance snapshots/events for observation and scoped Manager interruption/stop for control.
- Isolated child candidates and `SubagentPreparation` for application resource/Session association.
- Codemode host globals with spread arguments and asynchronous cancellation-aware handlers.

Script host adaptation uses an internal scoped capability, not a separate Agent bridge layer or raw AgentManager, Phi factory or AgentInstance handles supplied to JS. AgentAccess enforces registration/relationship authority; it does not replace invocation permission, tool availability or definition constraints.

For the first release, capture the creator registration epoch, runtime generation, working directory, validated definition catalog, capability ceiling and required child-preparation services at launch. Keep generation-bound services live through run finalization, while serializing incompatible creator replacement/reset/rebinding against run admission. Each worker receives isolated execution resources; this does not share a mutable checker or extension VM between workers. A Workflow-authored prompt retains script provenance and cannot supply human approval.

The launch authorizes bounded scripted delegation within this captured scope, not arbitrary subsequent tool calls or SDK administration. Check creator closure, run state and exact creation intent again at asynchronous effect boundaries. Launch-turn preapprovals expire normally and are not inherited by workers. Capability withdrawal/closure cancels pending work; new Session grants do not silently broaden the captured capability ceiling. Worker permission requests use existing interaction routing and may still require user approval.

```text
JS agent(prompt, options)
  -> validate typed invocation
  -> validate run scope, creator registration and capability policy
  -> queue under run limits
  -> authorize exact creation intent
  -> existing candidate preparation and Manager admission
  -> capture worker registration and await initial completion
  -> validate output and journal terminal facts
  -> resolve JS promise
```

A background run outlives its launching tool call. It must hold an explicitly admitted run capability, not reuse an expired tool-call/turn scope or fabricate a fresh one. Define capability capture and revocation against the existing runtime-generation policy; startup approval never grants unrestricted future Agent creation. Closure and stale registrations fail closed.

### Result delivery

SDK spawn returns creation facts separately from completion. Existing foreground delegation suppresses automatic initial feedback to the creator, making it a useful mechanism for returning results only to the script.

However, current caller policy can force background delegation, and named creation can route to Teams. Extend the shared internal creation protocol with explicit run-owned result routing and ordinary-subagent intent; do not add a parallel admission path. Keep existing model-facing foreground/background semantics unchanged. This routing is an internal delivery choice, not permission to bypass governance.

Only the run consumes worker initial results. The creator may observe worker activity without receiving automatic per-worker model submissions. Nested ordinary delegation returns results to the invoking worker, not the main conversation. Each invocation's worker subtree is finalized after its initial result; later retained messaging is not a Workflow operation.

The model-facing Workflow tool returns a `run_id` acceptance ToolResult, not execution success. On successful launch its declaration opts into [tool-batch termination](design-harnes.md#turn-and-agent-loop-model): commit the ToolResult, then end the launching turn without another model request just to acknowledge startup, provided every result in the batch opts in. A failed launch or a mixed batch retains normal continuation; an accepted Steer still takes precedence. No UI-only suppression or removal of the canonical ToolResult is involved. This ends only the launching turn, not the run. The later terminal report is delivered once through creator feedback and can start the model turn that summarizes the result, even if it arrives before the launch turn settles.

A main turn interruption after accepted launch does not implicitly stop the run. Explicit run stop and process finalization do. Final run delivery uses existing feedback admission rules, including passive handling while the creator is user-cancellation-suspended. Delivery identity must prevent duplicate terminal reports during in-process recovery; observations never fabricate model prompts.

## Scheduling and lifecycle

The runtime maintains bounded pending invocations, active worker obligations, invocation count and output/memory use. Script calls request work; slots cover admitted preparation through finalization. AgentManager independently retains its capacity/security checks. First-release transitions are fixed in the Semantic contract; the following diagram includes future pause/replay support.

```text
accepted -> running -> finishing -> completed | failed
                |
                +-> pausing -> paused -> replaying -> running    (future)
                |
                `-> stopping -> stopped
```

- Launch validates and captures script/args before acknowledging an accepted run. Syntax or metadata failure is launch failure, never a successful launch receipt with an embedded error.
- Pause stops new admissions, cancels active attempts and waits for them to settle. Completed results remain reusable; resume replays the script. This is Phi's proposed cooperative pause policy, not a claim about Claude's undocumented pause internals.
- Stop cancels the script, pending admissions and captured worker subtrees. Remain stopping until cleanup settles; no successor run overlaps old attempts.
- Restarting a worker settles the old attempt before creating a fresh instance from the same invocation intent. It does not inherit the failed attempt's Transcript.
- Cancelling JS futures alone is insufficient: accepted AgentInstances have independent lifetimes and require explicit managed control.
- Tool/filesystem side effects and spent tokens survive failure, cancellation and restart. No rollback or exactly-once execution is promised.

Stall detection uses execution progress, excluding known tool/permission waits. Attempt and output-repair retries are bounded and separately observable. Permission denial is not an automatic retry opportunity. Provider-specific usage-limit waiting is a later policy, not a universal runtime assumption.

Use separate limits for concurrent workers, total invocations, retry attempts, pending requests and journal/result bytes. Model-facing size guidance and token warnings are advisory; they are not hard budgets. Any later hard budget must define reservation, overshoot and termination semantics explicitly.

Current Manager membership is capped at 1,024 retained instances; ordinary completed children remain retained. Workflow workers instead have invocation-scoped lifetime: after capturing their immutable completion, use managed stop to close the entire worker subtree, then release the execution slot. Manager's existing finalization removes registrations and preserves final views for application persistence; Workflow never edits membership maps.

Retain bounded run records, returned values and saved worker Transcript references for inspection, not live execution handles indefinitely. Also bound or retire the creator's child-observation/result bookkeeping: removing Manager membership alone does not bound every parent-side collection. Cleanup or save failures remain visible and retain the recovery obligations needed by existing finalization. Do not advertise Claude-scale limits until repeated-run tests cover both Manager membership and parent bookkeeping.

## Journal and replay

Future scope only: do not introduce journal/checkpoint/attempt structures in the first release.

Recovery re-executes the script; it does not serialize the JS heap or resume a suspended VM instruction.

Capture immutable script source, metadata, args and creator/run identity. Journal each invocation's start-order position, effective execution intent, attempt/worker registration identities, terminal output or failure and diagnostic usage. Completion order must not replace start order.

The harness defines journal records and append/flush boundaries; the application supplies durable storage and Session associations. A result becomes replay-reusable only after its successful record is durably acknowledged. Persistence failure prevents publishing that result as recoverable success; it does not undo already-applied effects.

Replay reuses the valid completed prefix:

```text
previous: A completed | B failed  | C completed | D completed
replay:   A reused    | B rerun   | C rerun     | D rerun
```

- Match canonical effective invocation intent, including prompt, definition/model, output schema and directory/security-relevant construction choices. Exact encoding/versioning is an implementation contract, not a prompt-only cache key.
- The first failed, incomplete or mismatched invocation invalidates reuse for it and every later invocation, including completed ones.
- Reordering invocations or editing prompts therefore invalidates the affected suffix.
- Replay uses recorded results to reconstruct intermediate JS values and branches.
- Wait for predecessor attempts and startup compensation to finish before relaunch.
- Missing or corrupt journals fail explicitly; never silently start a new run under resume intent.
- Recovery is scoped to the original application Session, including reopening that Session after process exit. Direct SDK consumers supply an equivalent explicit persistence scope.

Snapshot effective construction intent sufficiently to detect incompatible runtime changes. Resume still revalidates current authority and execution security. A saved record is neither a permission grant nor proof that the external filesystem remains unchanged.

## Application and observation boundary

Expose semantic run snapshots and scoped events: run status, phase, invocation/attempt state, worker identity, usage, progress and terminal outcome. Worker Transcripts remain canonical in AgentInstances and their existing application associations. The run journal stores orchestration facts and returned values, not duplicate worker history.

Activities show ordinary Agents admitted during the current AgentSession, including finished Agents; resuming a Session does not reinsert saved children into activities. Saved child Sessions remain available for history reads. Missing historical outcome evidence remains interrupted/unknown; neither launch receipts nor transcript text imply completion. Runtime ownership filtering excludes Workflow workers and their commands from agents & commands, both live and after resume. Saved runs and invocations remain in Session records, including terminal results, phase and logs when recorded; the interactive status area does not display historical runs. Older records may omit phase/logs. Missing or invalid records do not block Session history. Resuming never restores execution, SDK membership or control authority. Worker history remains in child Session records, not the interactive status area.

Session loading may recover missing workflow-worker parent-call associations from an accepted Workflow receipt and its versioned terminal record. Require matching Session, run, worker and child-Session identities; preserve explicit associations. Missing or invalid auxiliary records must not invent an association or prevent history reads. This restores display provenance, not execution or recovery state.

When Workflow is enabled, `/workflow <task>` submits a prompt; bare `/workflow` shows usage. `/tasks` contains only checklist and agents & commands. The interactive status area shows live runs only: a total while running, up to three one-line run summaries, and `+N workflows` for the rest. Running and failed runs take priority. Settled counts include succeeded, failed and cancelled invocations; the denominator is accepted requests and may grow. Phases appear only when present, errors are truncated to one line, and completed runs show a short conclusion. No worker, prompt, log, history or stop control appears there. Empty status is hidden; terminal entries disappear after the next accepted user submission, while results remain in normal conversation output.

The UI observes live runs without controlling their lifecycle. Print and RPC adapters expose only their supported workflows; adding Workflow must not restore removed remote, serve or generic history surfaces.

Application storage retains per-run script/args snapshots and versioned journal data under the owning Session. Saved reusable definitions are separate project/user resources. Validate paths and permissions before reading source; avoid executable metadata discovery and untrusted cache loading. Concrete layout and command names are deferred.

Finalization closes run admission, cancels or drains according to the selected mode policy, settles workers, flushes journals and then completes Session persistence. Cleanup and persistence failures remain distinct from worker failure. Dropping observers never stops execution.

### Stdio RPC support

RPC here means `phi --rpc`: version-2 tagged JSONL over stdin/stdout, controlling one application root. It is not HTTP, a socket service or remote attachment. Support Workflow within that conversation, not a separate Workflow management service.

First-release requirements:

- **Governed launch:** the Agent can call the ordinary `Workflow` tool through the existing ToolRegistry, permission evaluation and application Session admission path. No direct script-launch RPC is required.
- **Run observations:** the client receives the run identifier, status and settled terminal outcome. Launch acceptance is not execution success; a main-turn result is not whole-run completion. Use scoped events in the existing stream; a dedicated list control or interactive-panel projection is not required.
- **Worker permissions:** route permission questions and answers for run-owned worker trees through the existing scoped interaction protocol, including during drain. No silent approval or unreachable wait: if EOF makes a required answer unavailable, fail explicitly and finalize. Permission routing must not inject intermediate worker results into the creator's model context.
- **Whole-run stop:** the client can explicitly stop a run belonging to the controlled root's scope. Stop closes admission and settles queued/startup work and captured worker subtrees. Main-turn interruption remains distinct from run stop; no per-worker stop, pause, resume or restart surface is required.
- **Drain and cancel:** EOF and explicit drain wait for admitted Workflow execution, terminal delivery and resulting Agent work, including successor runs. Drain rejects new inputs but retains permission answers, run stop and escalation to cancel. Cancel closes admission and fully finalizes runs, workers and Session persistence through main-owned cleanup. Cleanup or output failure must not be reported as successful drain/cancel.

Worker history browsing, a direct script-launch endpoint and a generic Workflow administration API are outside this first-release RPC scope. Wire names and payloads remain adapter decisions; external clients never receive SDK runtime collaborators.

#### Claude documentation comparison

| Claude public reference | Documented behavior | Phi adaptation |
| --- | --- | --- |
| [Workflow approval and supported surfaces](https://code.claude.com/docs/en/workflows#approve-the-plan-before-it-runs) | Workflow is available in `-p` and Agent SDK runs; launch uses ordinary tool permission evaluation, including host approval | Enable governed model-tool launch in stdio RPC, not a privileged launch endpoint |
| [Workflow input/output](https://code.claude.com/docs/en/agent-sdk/typescript#workflow) | Immediate asynchronous receipt carries `taskId` and Workflow `runId`; final result arrives later as task completion | Keep acceptance, `WorkflowRunId`, status and settled outcome distinct; no extra task identity is required |
| [SDK Query controls](https://code.claude.com/docs/en/agent-sdk/typescript#query-object) | Generic `stopTask(taskId)` stops background work; `close()` terminates the query and cleans up resources | Provide scoped whole-run stop and complete process cancellation; no requirement to copy dedicated Workflow management methods |
| [SDK permission evaluation](https://code.claude.com/docs/en/agent-sdk/permissions#how-permissions-are-evaluated) | Unresolved approvals reach the host's `canUseTool` callback | Preserve worker question/answer routing under Phi's existing permission policy |
| [Background tasks at exit](https://code.claude.com/docs/en/headless#background-tasks-at-exit) | After turn completion and stdin EOF, the process waits for workflows; background results can trigger more turns | Drain the complete run/delivery/Agent chain, not just the launching turn |
| [Session state events](https://code.claude.com/docs/en/agent-sdk/typescript#sdksessionstatechangedmessage) | Session running/idle state and turn results are distinct; live workflows can keep the session running | Do not infer Workflow settlement from main-turn completion or temporary absence of workers |

These references establish public behavior, not Claude's private stdio protocol. Phi's explicit drain/cancel policy and wire shape remain Phi decisions. Claude's documented print idle-wait ceiling is not Phi's approved 300-second script deadline; do not conflate adapter waiting with script execution limits.

## Implementation phases

1. **First release: background execution and complete stop.** Restricted globals; basic text-returning `agent()`; validated definition selection; ordinary-subagent/run-owned result delivery; bounded parallel/pipeline; generation-bound run capability; worker release; scoped run observations/control; application association, terminal delivery and process finalization. Prove intermediate results stay out of main context and no workers survive settled runs. Do not expose pause/resume/restart APIs or recovery-journal claims.
2. **Typed invocation options.** Add validated structured output and per-invocation model selection through the shared creation protocol, with explicit failure/repair semantics. Add saved definitions/authoring guidance as needed.
3. **Durable replay and advanced lifecycle.** Journal ordering and acknowledgement, prefix reuse, pause/resume, restart exclusion, bounded retries, same-Session recovery and storage failures.

Do not require a new Task scheduler, Agent Team, remote backend or generic DAG engine for any phase.

## Validation boundaries

- Independent worker context and history; results return to JS and only the run report reaches the main conversation.
- Direct `agent()`, parallel and pipeline obey the same admission/concurrency policy; pipeline progresses per item without a stage-wide barrier.
- Permission/definition denial, forced-background policy, Team-enabled creators, stale registrations and runtime replacement cannot bypass run authority or redirect worker feedback.
- Stop during preparation, admission, execution and journal writes settles captured obligations without late workers or duplicate relaunches.
- Schema failure, null slots, bounded retries and partial operational failure remain distinguishable from successful empty findings.
- Replay ordering, effective-input mismatch, invalid suffixes, missing/corrupt journals and persistence failure preserve explicit failure semantics.
- Membership limits and retained workers remain bounded across repeated runs; observation loss does not change execution outcomes.
- Stdio RPC preserves governed launch, run-scoped observations/stop and worker permission answers; EOF/drain cover zero-worker gaps and report-triggered successor runs, while cancel and output failure retain complete finalization.

## Reference boundaries

- [Claude Workflow guide](https://code.claude.com/docs/en/workflows): script examples, background execution, failure/replay behavior and published limits.
- [Claude Agent SDK reference](https://code.claude.com/docs/en/agent-sdk/typescript#workflow): launch inputs and asynchronous receipts.
- [Anthropic Workflow article, Figure A](https://claude.dev/blog/a-harness-for-every-task-dynamic-workflows-in-claude-code/): `agent(prompt, options)`, deferred `parallel()` functions and streaming multistage `pipeline()`.
- [Phi Codemode engine](../phi-codemode/README.md): host globals, sandbox limits and cancellation.

These sources establish observable Claude contracts, not its internal process implementation. Phi uses its existing Rust Agent API; no CLI subprocess is required to create workers. Remote isolation, Anthropic model aliases and subscription-specific policies are not inherited requirements.
