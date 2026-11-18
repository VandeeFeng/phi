# Parallel Agent design

## Scope

Phi supports Agent-driven parallel work through independent execution instances and one SDK orchestration authority:

```text
Application workflow -> runtime preparation/associations -> SDK AgentManager/AgentInstance
```

This document defines the design philosophy, ownership boundaries, and delegation lifecycle. Protocol invariants belong here; concrete types, schemas, limits, and private mechanics are linked to their implementation.

Related designs: [Agent runtime](design-agent-runtime.md), [SDK](../design-sdk.md), [application](../design-interactive.md), and [worktree capabilities](design-worktree.md).

## Decision, API, and runtime hierarchy

```text
Human goals, constraints, and intervention
                    |
                    v
Agent decides: decompose / delegate / review / synthesize
                    |
                    v
Tool or SDK API expresses typed management intent
                    |
                    v
AgentManager validates authority and coordinates work
                    |
                    v
Independent AgentInstance actors execute and commit history
```

Agent API names AgentManager's public contract, not another service or state owner. A leader is an Agent making business decisions; Manager implements authorized requests without planning, judging results, or choosing retries.

SDK consumers and model-tool adapters share orchestration protocols, not authority. Trusted SDK access and registration-bound Agent access are distinct. SDK-only orchestration needs neither application runtime modules nor Session files.

### Human agency and delegated autonomy

Autonomy serves human goals while preserving useful visibility, direction, and intervention throughout execution—not only an initial prompt and final report. Agents expose plans, progress, results, uncertainty, and blockers without requiring disclosure of private model reasoning.

This is a design principle, not a universal approval workflow or new Manager state machine. Existing user submissions, interactions, observations, and authorized controls provide human participation. Agents support human judgement; messages, definition instructions and Skills cannot manufacture human consent.

### Harness, environment, and application

See [Harness boundaries](../design-harnes.md#harness-environment-and-application).

## Concepts and independent boundaries

| Concept | Meaning | Must not be confused with |
| --- | --- | --- |
| AgentDefinition | Named specialization, model intent, generic tool policy, turn limits | Live identity, authority or Team |
| AgentInstance | Addressable execution resource owning mutable state and canonical Transcript | A task, attachment, or Session file |
| Master agent | Primary conversation Agent, distinct from delegated subagents | Privileged authority, Team lead or currently selected Agent |
| [Delegation mode](#foreground-and-background-delegation) | Waiting and initial result delivery | Instance lifetime, history storage, Team membership or application process mode |
| Creation lineage | Fixed `created_by` instance parent/child relationship | Ownership of every later task |
| Agent Team | Optional collaboration membership of instances | Creation subtree, work list, or directory scope |
| Working directory | Instance execution directory, plain or Git worktree | Authorization or storage identity |
| Application Session | Metadata and persistence envelope around one Transcript | Live execution truth |

Creation starts an independent instance. Messages submit follow-up work to an authorized existing instance, retaining specialization, directory, runtime and history. Neither operation creates a checklist Task.

Instance and canonical Transcript identity stay aligned. Application Session identity may align with them, but SDK identity does not imply a file or storage dependency.

```text
Creation lineage             Collaboration             Shared work list
A (master)                   Team T                    TaskList L
+-- B                        +-- A (lead)              +-- 1 -> owner B
`-- C                        +-- B                     `-- 2 -> owner C
    `-- D                    `-- C

B retains one Transcript across ordinary invocations.
D inherits list access without automatically joining the Team.
Task ownership records responsibility, not creation lineage or execution authority.
```

## Responsibilities

Manager, Instance and loop ownership is defined in [harness responsibility boundaries](../design-harnes.md#harness-responsibility-boundaries). Integration responsibilities follow:

| Layer | Owns | Does not own |
| --- | --- | --- |
| Model-tool adapter | Argument validation, typed intent conversion, model-facing receipts/errors | Authority, canonical identities, scheduling, task state |
| SDK construction | Complete validated runtime candidates and resolved definition constraints | Application config discovery or Session storage |
| Runtime modules and AgentSession | Runtime preparation, Session association/boundaries, access policy, checkpoints and recovery | A second execution registry or delegation state machine |
| AgentAttachment / SessionAccess | Scoped internal access | Agent lifetime or implicit cancellation |
| UI / transport | Observation, interaction presentation, DTO conversion | Task progression, result consumption, persistence acknowledgement |

The access chain does not require every operation to traverse all layers. Instance-local prompt submission and interaction replies remain instance operations inside the application's scoped access boundary.

```text
Application create/control:  mode -> concrete runtime capability -> SDK
Delegation:                  parent Instance -> AgentManager -> executor Instance
                                                   |
                                                   `-> application preparation/association seam
Checkpoint:                  canonical SDK facts -> checkpoint coordinator -> storage
```

Delegation never loops through a mode adapter. Attaching, switching views, and detaching change access only; execution does not require an observer.

### Authority

Registration establishes live identity, not a privileged Agent role. Relationship-based policy and explicit grants separate communication, history and control; master naming grants no bypass, including across Team boundaries. Master names the primary conversation Agent; parent/child describes relative delegation, and Team lead is a fixed coordination relationship. Definitions, aliases, saved parent references and model claims never grant authority; execution security remains enforced.

Authority has independent gates:

```text
Resolved ToolRegistry governance
  -> trusted caller registration + operation/target policy
  -> validated construction/admission
  -> existing permission, path, and sandbox enforcement
```

Same-Team access cannot grant control or full history. Caller and tool-call identity come from runtime state, never model-supplied claims. See [scope policy](../../phi-sdk/src/harness/manager/policy.rs) and [registrations and grants](../../phi-sdk/src/harness/manager/registry.rs).

### Agent Teams and collaboration boundaries

Each AgentInstance belongs to zero or one Team. Team initialization fixes the lead; named teammate admission reserves aliases through startup and cleanup. Ordinary children do not automatically join their creator's Team. See [Task and Team design](design-agent-task.md#7-team-and-subagent-semantics).

Same-Team members may discover public status and exchange messages subject to tool policy. Unrelated Team-less instances do not form an implicit Team; authorized parent/child communication still works without membership. Cross-Team communication needs explicit authorization, separate from Team selection or control grants.

Teams own no executor or Transcript. They can span directories, and different Teams can share a directory. The fixed lead coordinates members under the [Task and Team lifecycle](design-agent-task.md#7-team-and-subagent-semantics); Team cleanup is separate from retained Task files.

### Tool, Instance, and Manager validation boundaries

| Boundary | Authoritative validation |
| --- | --- |
| Tool adapter | Schema, mutually exclusive inputs, unknown fields, output formatting |
| Initiating Instance | Tool governance, actual turn/tool invocation and directed-delegation constraints |
| AgentManager | Registration identity, operation/target scope, Team membership, relationships, capacity and closure |
| Construction / target Instance | Complete candidate, execution security, serialized input admission and history commit |

Rules needed by direct SDK callers live below the tool adapter. Manager accepts semantic intent, not model action enums or JSON response contracts. Explicit model retained assignment cannot borrow trusted instance-handle bypass authority.

### Manager admission and internal validation

Asynchronous preparation makes entry checks insufficient. Manager reserves capacity atomically and revalidates captured registrations and closure at each effect boundary:

| Boundary | Required fact |
| --- | --- |
| Reservation | Authorized caller/context, bounded admission capacity and alias availability |
| Candidate registration | Canonical directory and complete candidate still match intent; caller and Team remain eligible |
| Startup acceptance | Registration, association where applicable, and startup succeeded; closure has not won |

Slow filesystem work, application callbacks and actor awaits run outside registry locks. Failed startup compensates captured obligations; cleanup failure is separate from execution failure. Captured registration epochs prevent reused IDs from receiving old work, grants or results.

See [creation coordination](../../phi-sdk/src/harness/manager/coordinator.rs) and [candidate preparation](../../phi-sdk/src/harness/manager/preparation.rs).

## Agent delegation and working directory

One `Agent` tool exposes creation, discovery and messaging through a flat JSON object:

```jsonl
{"action":"create","role":"reviewer","prompt":"Review changes."}
{"action":"list"}
{"action":"message","recipient":"agent-id","content":"Review authentication next."}
```

All calls require `action`. For `create`, the `alias`, `working_dir` and `mode` are optional. The provider schema advertises the flat field set; the Agent adapter rejects non-object input, unknown fields and fields mixed across actions before dispatch. OpenAI Responses and Codex convert Agent definitions with explicit `strict: false` at the provider model boundary; HTTP transport does not rewrite tool definitions. Typed SDK creation and messaging are unchanged. Local validation operates on provider-parsed arguments, without separate raw-fragment buffering or duplicate-key detection.

Creation and management inputs are mutually exclusive. Messages reject creation options and preserve recipient configuration. Sending returns acceptance, not recipient completion or a reply. The Agent chooses the recipient and work; Manager does not. The model surface has no executor, receipt-wait or delivery-timeout controls; typed SDK receipt observations remain available.

For creation, omitting `working_dir` uses the immediate parent's execution directory; relative paths resolve against that directory, not process CWD. The target must already exist and pass canonical-path and execution-security validation. Invalid, unauthorized, or disappearing directories fail without fallback or recreation.

Directory preparation runs on the blocking pool on a bounded ordered path before admission reservation. Abort, stop or caller abandonment discards unresolved preparation so late filesystem completion cannot create a child. Runtime replacement rejects pending preparation. `@agent` directed creation cannot be replaced by retained assignment or fallback Git commands. See [tool schema](../../phi-sdk/src/agent/tools/agent.rs), [Instance delegation](../../phi-sdk/src/harness/instance/execution/delegation.rs), and [directory validation](../../phi-sdk/src/harness/manager/preparation.rs).

### Foreground and background delegation

Ordinary subagents support both modes. Teammates are always created asynchronously and collaborate through messages.

| Effective mode | Agent tool return | Initial completion delivery |
| --- | --- | --- |
| `foreground` (synchronous) | Wait for the child and return its final output or failure | In the same tool result; no automatic follow-up turn |
| `background` (asynchronous) | Return an `async_launched` receipt after admission | Follow [execution feedback policy](../design-harnes.md#execution-feedback) |

Mode controls waiting and initial result delivery, not execution ownership. Both modes create independent AgentInstances with their own context and Transcript; neither embeds child history in the parent. Completion retains the child for follow-up work.

Tool-result invariants:

- Foreground results describe delegated execution, including failure or actual child cancellation. Admission alone is not tool success.
- Successful background creation opts its ToolResult into batch termination: after committing every result, a sole/all-terminating successful batch ends the caller's turn without a startup acknowledgement model request. Mixed batches and failures continue normally; queued Steers take precedence. Later background outcomes retain ordinary follow-up delivery.
- Aborting the parent turn leaves accepted children running and settles its foreground wait as call interruption, not `subagent cancelled`, without later initial feedback.
- Live UI and history recovery use canonical ToolResults for tool status; child lifecycle is projected separately.

Reference: Claude Code's [foreground/background delegation](https://code.claude.com/docs/en/sub-agents#run-subagents-in-foreground-or-background), [Agent hook results](https://code.claude.com/docs/en/hooks#agent), and [subagent errors](https://code.claude.com/docs/en/sub-agents#api-errors-in-subagents).

Manager resolves the effective mode once at admission, in this order:

1. Teammate creation or the caller's accepted `agent.background: true` policy forces background, including explicit foreground requests.
2. Otherwise, an explicit `mode` selects foreground or background.
3. If omitted, a master caller defaults to background; a delegated caller (ordinary subagent or teammate) defaults to foreground.

Each creation edge resolves independently. A master's background child therefore waits for its own ordinary child by default; the first edge does not force background on the next. `@agent` directed delegation remains explicitly asynchronous. There is no live mode conversion, new `@agent` syntax or Ctrl+B mode control.

The synchronous wait belongs to the Agent tool adapter, not the [SDK spawn API](../design-sdk.md#subagents). See [creation contracts](../../phi-sdk/src/harness/manager/preparation.rs) and [shared admission](../../phi-sdk/src/harness/manager/coordinator.rs).

### Worktree is an ordinary Agent capability

```text
Agent -> permitted Bash + Skill -> prepare existing directory/worktree
Agent -> delegation request -> Manager -> independent Instance in that directory
```

Git preparation and delegation are separate operations. Skills guide work; actual effects pass existing security checks. Delegation failure does not roll back a worktree, and completion/cancellation/detachment does not remove it. Shared directories may have conflicting writes; delegation adds no repository-wide locks or Git transaction scheduler. See [worktree design](design-worktree.md#execution-security-gate).

## End-to-end Agent and Session lifecycle

### Create and delegate

```text
Start the Session's master AgentInstance
(requested by interactive, print or RPC):
  Application runtime   prepare candidate and Session resources
           |
  AgentManager          register the master instance
           |
  Session associations  bind Session, install observers, run SessionStart
           |
  AgentInstance         accept prompt submission and execute

Child delegation with Session persistence:
  Parent Instance       request child creation
           |
  AgentManager          validate authority and reserve admission
           |
  Application runtime   prepare candidate and staged Session association
           |
  AgentManager          register child
           |
  Session associations  bind child Session, install observers, run SessionStart
           |
  AgentManager          start initial invocation, accept, return parent receipt
```

| Stage | Business contract | Implementation |
| --- | --- | --- |
| Application creation | Application runtime stages metadata/resources; registration establishes live identity; association precedes ready access | [Session associations](../../phi-interactive/src/runtime/associations.rs), [factory](../../phi-interactive/src/runtime/factory.rs) |
| New-child delegation | Resolve directory, reserve IDs/capacity, prepare candidate, register, associate, start, then accept | [Coordinator](../../phi-sdk/src/harness/manager/coordinator.rs), [application child integration](../../phi-interactive/src/runtime/subagents.rs) |
| Execution and interaction | Each actor owns its loop/history; authorized attachments answer existing interactions; initial prompt is submitted once | [Actor](../../phi-sdk/src/harness/instance/actor.rs), [delegated execution](../../phi-sdk/src/harness/instance/execution/delegation.rs) |
| Follow-up message | Capture an authorized recipient and submit work through its ordered mailbox | [Manager API](../../phi-sdk/src/harness/manager/api.rs), [message routing](../../phi-sdk/src/harness/manager/message.rs) |
| Completion and feedback | Preserve the initial invocation result and continue the creator through ordinary execution | [Instance outcome handling](../../phi-sdk/src/harness/instance/execution/delegation.rs) |
| Checkpoint | Save each association's canonical history and metadata | [application checkpoints](../../phi-interactive/src/runtime/checkpoints.rs) |
| Stop and release | Close admission first; actors finalize; retain final evidence through persistence failures | [SDK control](../../phi-sdk/src/harness/manager/control.rs), [AgentSession](../../phi-interactive/src/runtime/session.rs) |

Cross-layer registration is compensating coordination, not an atomic database transaction. A partially prepared/associated child is not accepted work. Application runtime prepares resources and establishes Session boundaries; it cannot take over startup, cancellation, or completion.

Task completion does not stop its instance or end its Session. A finished master turn likewise triggers checkpoints, not automatic shutdown. Runtime replacement validates a complete candidate before mutation and preserves identity/history.

### Open history or explicitly resume

| Request | Effect |
| --- | --- |
| Attach to a live instance | Reuse it; no new Session-start boundary |
| Read saved history | Load data only; no runtime, task replay, or delivery |
| Explicitly resume a stopped Session | Prepare a new actor through Manager using saved identity; reject duplicate live identity according to application resume policy |

Saved lineage is metadata, not authority. Stopped delegated Session execution is currently rejected until validated reconstruction is supported; passive reads and live-child attachment remain available. See [application open/rebuild rules](../../phi-interactive/src/runtime/associations.rs).

## General inter-Agent communication protocol

Ordinary messages are actionable Agent-authored prompt content, not authority or passive UI notifications. Automatic reports identify their harness origin; neither is human input or approval. Manager owns validated addressing and bounded routing; the recipient owns admission, canonical commit, and execution. Harness-delivered state facts and initial results do not require Agents to send status-sync messages. Callers choose actionable content; there is no semantic filter that turns Ordinary status chatter into observation-only delivery. See the [message/context/wakeup classification](../design-harnes.md#execution-feedback).

```text
Sender -> tool governance -> Manager communication scope -> bounded mailbox
  -> recipient actor's ordered submission boundary
  -> canonical AgentMessage + identity marker -> delivery acknowledgement
       enabled + idle: start ordinary message-processing turn
       enabled + busy: eligible coordination joins at a safe point; otherwise queue
       user-cancelled: reject ordinary messages; automatic reports stay passive
       closing/stopped: reject; never revive implicitly
```

| Communication | Effect |
| --- | --- |
| Ordinary message | Follow-up work, coordination, questions, recommendations or replies; no checklist Task mutation |
| Child result | Initial delivery follows [delegation mode](#foreground-and-background-delegation); later ordinary subagent results return to the creator |
| Teammate idle report | Later teammate answers or errors return automatically to the fixed lead |
| Teammate exit | Passive context unless unfinished Tasks were released; released work wakes an activation-enabled lead after earlier work |
| Status/update observation | Non-consuming facts; not an automatic model turn |
| Management request | Typed authorized API operation; cannot be created by message text |

Automatic-activation suspension overrides the table's normal wakeup behavior: results, idle reports and exit notices become passive context; ordinary messages are rejected. Preparation can still intercept model-context delivery. A delivery receipt records canonical commit, not a guarantee that a model request was started.

Canonical IDs address instances; Team aliases and creator-scoped ordinary child names resolve to captured registrations. Ordinary names do not confer Team membership or communication authority. Stable message identity and recipient commit markers prevent duplicate prompt/turn activation on retries. Queue acceptance, recipient admission, canonical commit and model processing are separate facts. Delivery means canonical commit, not completion of the resulting turn.

Ordinary coordination can join an active turn only while automatic activation is enabled, after initial delegated completion, outside directed execution, without overtaking queued Follow-ups, and while the runner accepts Steer. The recipient commits it as an Agent entry at the next safe point, not during a provider request. Unconsumed messages retain identity and start later turns; exhausted round budgets cannot consume them. This appends input to an unfinished turn, never rewrites a finished turn. Passive exit context also preserves earlier submission order.

Messaging reuses the ordinary turn/lifecycle/reducer pipeline, not a separate model runner. Bounded event subscriptions are observations, not reliable transport. Timeout or abandoned observation does not cancel accepted work. Pending messages are not replayed across restart; message-triggered turns retain existing execution/resource limits.

See [routing, receipts, waits, and bounds](../../phi-sdk/src/harness/manager/message.rs) and [recipient message execution](../../phi-sdk/src/harness/instance/execution/submission.rs).

## Child completion

Initial execution publishes one immutable completion before queued follow-up work can replace its outcome. Initial delivery follows the [delegation mode](#foreground-and-background-delegation), without requiring a child-authored message. Later ordinary subagent results return to the creator; later teammate answers or errors return to the fixed lead on idle. Initial completion is not duplicated as an idle report, and exit notices remain separate.

Background outcomes follow [execution feedback policy](../design-harnes.md#execution-feedback): normally later-turn input, but passive context while the recipient's automatic activation is suspended. Ordinary coordination may join activation-enabled execution at safe points. An Agent waiting only for background children should end its turn, not poll discovery or sleep.

The harness publishes admission, completion, failure, cancellation, and delivery receipts; Task state is read from its authoritative list. Agents send messages for questions, blockers, findings, and changes of direction—not to synchronize those facts. An initial completion must not also be forwarded as an ordinary message or repeated in an exit notice. Receipt delivery does not imply that the recipient processed or accepted a message.

See [child admission and initial completion](../../phi-sdk/src/harness/instance/child.rs) and [child outcome handling](../../phi-sdk/src/harness/instance/execution/delegation.rs).

## Cancellation and shutdown

| Operation | Scope and retained state |
| --- | --- |
| Abort ordinary turn / interrupt execution | Interrupt current execution; accepted children and instance context remain |
| Cancel child invocation | Cancel pending admission or initial execution; retain accepted instances, with user-cancellation suspension as below |
| Stop instance | Close admission and cooperatively stop its fixed creation subtree |
| Manager shutdown | Close membership/admission and finalize all captured instances |
| Detach or switch view | Access/projection only |

User cancellation of an accepted child's initial execution suspends that live instance's automatic activation; retaining it is not permission for Agent messages to resume it. Ordinary messages are rejected, including pending messages retried after cancellation. Background outcomes and structured reports remain passive context. Direct Internal submissions are rejected; only a successfully started direct non-Internal user/host submission restores automatic activation. Failed/intercepted requests and runtime replacement do not restore it, and resumption never replaces the immutable initial result. Cancelling an already completed initial invocation has no effect on a later run.

The instance-level gate follows Claude Code's documented [user-stopped subagent resumption boundary](https://code.claude.com/docs/en/sub-agents#resume-subagents): Agent messages cannot auto-resume a user-stopped agent until direct user resumption. Phi's direct host submission and passive-report rules are its own runtime contract, not claims about Claude internals. The separate instruction not to recreate cancelled work through another command, Agent or the model's own actions is Phi behavioral guidance inspired by that Claude cancellation boundary, not a reproduction of documented Claude prompt text or a semantic equivalent-work blocker. **Open refinement:** optimize the concrete cross-command/Agent guidance and its scope without treating cancellation of one execution as withdrawal of the overall goal; the current prompt-based implementation is provisional. Claude's public documentation does not establish a general equivalent-work detector or its absence. Cancellation alone does not revoke the overall goal or other requested work; newer user direction may resume the cancelled work. See [execution feedback](../design-harnes.md#execution-feedback).

Closure precedes awaits. Cancellation coordinates startup compensation and accepted execution effects. Dropping control waiters must not abandon accepted closure. Actors finalize their own history without synchronously waiting for another actor's lifetime.

Published outcomes and accepted ToolResults remain immutable. Shutdown attempts every captured target and reports cleanup/persistence failures separately. Stop returns canonical final history. None of these operations implicitly deletes Session history or worktrees. See [control coordination](../../phi-sdk/src/harness/manager/control.rs) and [actor finalization](../../phi-sdk/src/harness/instance/actor.rs).

## Consistent observation

| Read | Contents | Must not do |
| --- | --- | --- |
| Child observation | Creation, initial admission and completion | Consume feedback or infer checklist Task completion |
| Instance view | Transcript and execution snapshot from one actor serialization point | Pair unrelated watch/history reads |
| Historical Session read | Persisted history and application metadata | Construct a runtime or replay work |

UI cannot use stale links or TurnFinished events to invent outcomes. Application runtime validates access/association and wraps coherent SDK views in Session metadata. Reads do not checkpoint.

Snapshot subscriptions return owned latest values; consumer retention cannot block publication or shutdown. Coherent reads use `AgentInstanceView`; marked reads correlate `ViewBoundary` with the bounded event stream.

See [Instance control and owned observation](../../phi-sdk/src/harness/instance/handle.rs) and [application child views](../../phi-interactive/src/runtime/associations.rs).

## Session storage, retention, and recovery

Each persisted instance has its own Session journal. Root and teammate journals use `<timestamp>-<session-id>.jsonl` in their working-directory scope. Ordinary subagents inherit their immediate parent's storage association, regardless of execution directory, and use `<parent-session-id>/subagents/agent-<child-session-id>.jsonl`; nested children extend the ancestry. Follow-up retains the original file and creation association. Child history is never copied into parent history. Persisted child links require the resolved creation delegation mode; recovery never infers it from tool arguments, current defaults or outcomes. See [ordinary sidechains and teammate history](design-agent-task.md#ordinary-sidechains-and-teammate-history).

| State | Authority | Release boundary |
| --- | --- | --- |
| Live membership | AgentManager | Explicit stop/release coordination |
| Canonical live/final history | AgentInstance | Preserve final view through outstanding obligations |
| Session file and child links | Application runtime/storage | Existing application persistence policy |

Session establishment/persistence enablement remains application policy, not a side effect of SDK identity. Stopping membership, detaching, releasing a binding/file lock, and deleting saved history are distinct. Restart reads saved data but does not replay mailbox delivery or rerun tools. Missing worktrees do not delete history or silently alter execution paths.

See [Session envelope](../../phi-interactive/src/session/model.rs), [storage policy](../../phi-interactive/src/session/storage.rs), [JSONL journal](../../phi-interactive/src/session/journal.rs), and [application checkpoints](../../phi-interactive/src/runtime/checkpoints.rs).

## Module and directory architecture

See [Harness module and directory architecture](../design-harnes.md#module-and-directory-architecture).

### Agent API directions

| Semantic operation | Protocol owner | Execution effect |
| --- | --- | --- |
| Spawn | Manager creation admission | New independent child and initial invocation |
| Submit | Instance admission, through trusted instance access | Ordinary prompt work on the instance |
| Message | Manager mailbox, including caller-bound access | Recipient commits Agent-authored follow-up work or coordination |
| Observe / wait | Captured SDK observations | No consumption or scheduling side effect |
| Interrupt / cancel / stop | Scoped SDK control | Distinct turn, child invocation and instance closure |

Ordinary submissions, interaction replies and coherent views stay instance operations. Model tools package semantic operations without acquiring trusted grant mutation, shutdown, or durability authority. Private registries, channels and reducers are not public facade exports.

Task records and Team lifecycle follow [Task and Team design](design-agent-task.md). Out of scope: a workflow scheduler, automatic retries, instance pooling, runtime Team switching, distributed execution, crash-resumable workflows/mailbox replay and a new permission system. Worktree provisioning/cleanup remains an independently authorized Agent capability.
