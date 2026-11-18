# Agent Task and Team design

## Scope

Phi coordinates recorded work through Tasks and independent Agents through Teams. The model decides how to decompose work, delegate, report and complete it. The harness enforces scoped access, record consistency and Agent lifecycle rules.

```text
Phi CLI / application runtime
  | prepares runtime resources and Session associations
  v
AgentInstance                         execution + canonical Transcript
  |
  +-- Agent ------------------------> AgentManager
  |                                    | creation, membership, aliases, messages
  |                                    v
  |                               other AgentInstances
  |
  +-- Task (create/get/list/update) -> TaskList
                                        | validation + scoped file operations
                                        v
                                   Task JSON files
```

A Task is a recorded unit of work, not a runner. An AgentInstance performs work through its existing model/tool loop and can undertake several Tasks. A TaskList groups records; it does not own execution. Ordinary conversation, Agent invocation and messages can operate without Tasks.

Related designs: [parallel Agents](design-parallel-agent.md), [Agent runtime](design-agent-runtime.md), [SDK](../design-sdk.md) and [application runtime](../design-interactive.md).

## Concepts and identity

| Concept | Meaning |
| --- | --- |
| `AgentDefinition` | Reusable specialization: instructions, model, tool policy and limits. Its name and description guide model delegation. |
| `role` | Agent creation selector matching `AgentDefinition.name`. |
| `alias` | Runtime instance name, unique within a Team or ordinary creator's child-name scope. |
| `prompt` | Concrete instructions for an Agent invocation. |
| `AgentInstanceId` | Identity of an independent live actor. |
| `AgentKind` | Creation-time kind and associations: `Master`, `Subagent { creator }` or `Teammate { creator, team }`. |
| `master` | Primary conversation Agent. |
| `parent` / `child` | Relative creation relationship between instances. |
| `AgentTeamId` | Live Team identity, separate from its filesystem namespace. |
| `lead` | Fixed coordinating instance of a Team; also its reserved alias. |
| `TaskId` | Positive decimal string, unique within one TaskList. |
| `TaskListId` | Validated namespace for a collection of Task records. |
| `TaskOwner` | Typed reference to the instance assigned responsibility for work. |
| `Session` | Application persistence envelope for one Transcript and its metadata. |

Definition, instance name, creation relationship and work ownership are independent facts. Two instances can use the same definition with different aliases and prompts. An instance can change its work without changing its alias or kind.

```text
Team T, lead A
  security    -> instance B, definition code-reviewer
  performance -> instance C, definition code-reviewer

Task 1 -> owner B
Task 2 -> owner C, blocked_by [1]
```

A definition name, alias, Task owner or saved identity grants no execution-control authority. Live operations resolve and capture registrations, including their epochs, before effects.

## Responsibilities and code boundaries

| Owner | Responsibilities |
| --- | --- |
| AgentInstance | Execution, canonical live Transcript, interactions, generation-local tool dispatch and teammate continuation. |
| AgentManager | Trusted registrations, creation/admission, fixed creation relationships, Team membership, alias reservations, messaging and scoped control. |
| Harness Task module | Task model, caller-bound access, typed operations, dependency rules, lifecycle gates and canonical file reads/writes. |
| Task tools | Model schemas, argument conversion, permission evidence and result JSON. |
| Application runtime | Typed storage roots, initial list selection, complete runtime candidates, Session associations and persistence. |
| Main | Mode composition and explicit process finalization. |
| Interactive / print / RPC | Supported adapter workflows and projection, without ownership of Task state or Agent execution. |

```text
phi-sdk/src/
  agent/tools/task.rs                     model adapters and permission formatting
  agent/tools/agent.rs                    instance creation, invocation and communication
  harness/task/model.rs                   Task records and binding types
  harness/task/operation.rs               captured access, typed requests/results and read DTOs
  harness/task/teammate.rs                 work selection, claims and Task boundary cooperation
  harness/task/list.rs                    work-record rules and lifecycle gates
  harness/task/store.rs                   locked, bounded file operations
  harness/manager/                        registrations, aliases and messages
  harness/manager/team.rs                 Team lifecycle and config/inbox file operations
  harness/instance/execution/mod.rs       scoped Task tool dispatch
  harness/instance/execution/teammate.rs   teammate permission/continuation wiring
  harness/instance/actor.rs                retained idle episode and worker invalidation
  harness/agentic_loop/pipeline.rs         in-turn Task/teammate completion gates
  core/lifecycle.rs                       typed Task and teammate events/outcomes
  hooks.rs                                ordered Task and teammate hook registration/dispatch

phi-interactive/src/
  runtime/                   preparation, Session associations and finalization
  session/                   journals, storage layout, discovery and locks
  interactive/activities.rs  Tasks overlay, Checklist/subagent/Bash projection and interaction intents
  rpc/                       running-work control and wire adaptation
```

Direct SDK consumers supply typed storage intent without requiring application Sessions. The public `tasks` facade exposes bindings and owned read views; canonical Task types, mutation rules and stores remain internal.

## Task records

Each Task has one canonical file at `tasks/<list-id>/<task-id>.json`.

| Field | Meaning |
| --- | --- |
| `id` | Generated canonical positive decimal `u64` string, such as `"2"`. |
| `subject` | Nonempty concise work label. |
| `description` | Work instructions and acceptance criteria. |
| `active_form` | Optional wording displayed while work is in progress. |
| `status` | `pending`, `in_progress` or `completed`. |
| `owner` | Optional `{ "instance_id": "..." }` reference. |
| `blocked_by` | Prerequisite Task IDs in this list. |
| `metadata` | JSON object for annotations; empty metadata is omitted on write. |

```json
{
  "id": "2",
  "subject": "Review authentication",
  "description": "Review auth changes and report security findings.",
  "active_form": "Reviewing authentication",
  "status": "pending",
  "owner": { "instance_id": "agent-security" },
  "blocked_by": ["1"]
}
```

List identity comes from the containing directory. Task IDs are allocated monotonically under the list lock. A `.highwatermark` preserves non-reuse after deletion; it records identity allocation, not execution progress.

Only `blocked_by` is persisted. Reads derive:

- `blocks`: Tasks that name this Task as a prerequisite.
- `unresolved`: prerequisites whose status is not `completed`.
- `eligible`: the Task is unfinished and has no unresolved prerequisites. Ownership is evaluated separately when selecting work.

Records contain work facts, not providers, channels, runtime generations, per-Task Transcripts or execution outcomes. Reports belong in ordinary messages and conversation. Task state survives context compaction and remains independent of Session saves.

### Mutation semantics

Creation publishes pending, unowned work with no dependencies. Status changes are explicit:

```text
Task(create) -> pending -> in_progress -> completed
                 ^            ^            |
                 +------------+------------+
                          explicit reopen
```

| Operation | Effect |
| --- | --- |
| Assignment | Updates owner; preserves status unless the request also changes it. |
| Self-claim | Assigns the caller after a locked check that work is unowned, unfinished and unblocked. |
| Release | Clears owner; preserves status unless explicitly changed. |
| Progress | Explicitly sets `pending`, `in_progress` or `completed`. |
| Reopen | Allows completed work to return to pending or in progress. |
| Dependency addition | Adds same-list edges through `add_blocks` or `add_blocked_by`. |
| Metadata patch | Merges top-level keys; a null value removes that key. |
| Deletion | Removes the record and prunes its ID from surviving prerequisites. |

Assignment to another instance and self-claim are distinct. Assigning the caller when it already owns the Task is not a competing claim. Claim checks apply to the patched candidate, including dependencies added in the same request. Starting work and claiming it can share one request.

Live callers sharing the list cooperate without an additional owner-only or lead-only mutation ACL. Assignments resolve a live instance ID or same-Team alias to a captured registration on the exact same list binding. Concurrent self-claims cannot both succeed.

Dependencies must exist in the same list and remain acyclic, without self-edges. Reopening a prerequisite changes current eligibility but does not interrupt or roll back downstream work. Deletion removes a constraint; it is not completion. Missing or corrupt prerequisites fail explicitly rather than silently satisfying work.

There is no one-in-progress-Task-per-instance restriction. Turn completion, actor idle, tool success and Session persistence do not complete Tasks. Completing or deleting a Task does not stop its owner.

## Model tools and dispatch

All tool fields use snake_case. Caller identity, list binding and filesystem paths come from trusted runtime context, not model arguments.

One `Task` tool requires an `action`: `create`, `get`, `list` or `update`.

| Action | Input | Result |
| --- | --- | --- |
| `create` | Required `subject`, `description`; optional `active_form`, `metadata`. | `task: { id, subject }`. |
| `get` | `task_id`. | Full Task plus derived dependency detail, or `task: null`. |
| `list` | Optional `after` cursor and `limit` from 1 to 100; default 100. | Summaries and optional `next` cursor. |
| `update` | `task_id`; optional content, `status`, `owner`, `add_blocks`, `add_blocked_by`, `metadata`. | Updated Task detail, or deletion acknowledgement. |

List summaries include ID, subject, status, optional owner/active form, prerequisites, unresolved prerequisites and eligibility. Get and successful Update include the full record plus `blocks`, `unresolved` and `eligible`. Pagination follows numeric Task ID order; pass `next` as `after` to continue reading.

Update patch intent is explicit:

- Omitted fields remain unchanged.
- `owner` accepts a live instance ID or same-Team alias; `null` releases ownership.
- Other optional fields reject whole-field null values. Metadata key-level null values remove keys.
- `status: "deleted"` accepts only `action: "update"`, `task_id` and `status`; it is an action, not a persisted status.
- Unknown fields are rejected.

The Task tool registers through the same ToolRegistry used for schemas, prompt guidance, availability, exposure, scheduling and dispatch. All actions are sequential within a tool batch; tool policy and permissions apply to `Task`, with the action included in permission evidence. Definition tool policy, `no_tools`, removals, outer-turn policy and permissions remain effective.

The call path separates adapter, execution and domain responsibilities:

```text
model JSON
  -> tool argument conversion to TaskRequest
  -> Instance validates generation, registration, turn and tool-call scope
  -> prepare request with captured list access and resolved owner
  -> authorize exact semantic request
  -> execute typed harness operation
  -> TaskResult
  -> tool result JSON
```

Permission evidence contains the resolved typed owner rather than an unresolved alias. Access revalidation covers caller registration, owner registration, binding, finalization and invocation lifetime, including work queued on the blocking pool or returning from a hook. Cancelled or stale requests cannot use a successor registration to publish work. Portable tools receive no raw Manager or store access.

## Connecting Agents to work

The Agent tool creates instances with `action="create"` and three separate fields:

| Field | Purpose | Example |
| --- | --- | --- |
| `role` | Select definition. | `code-reviewer` |
| `alias` | Name this instance. | `security` |
| `prompt` | Specify this invocation's work. | `Review Task 1 and report security findings.` |

Agent creation and Task creation are independent primitives. Shared list access, instructions and optional ownership connect them:

1. Create work, use its returned ID in the worker's prompt, and let the worker inspect, claim, start and complete it.
2. Admit a worker, assign it work through Task with action `update`, and communicate instructions through a prompt or message.
3. Give a teammate general work instructions and let it select eligible work from the shared list.
4. Create an Agent without a Task and receive its foreground result or automatic background summary.

An Agent prompt contains intent, not access grants. A Task ID in text neither creates a record nor supplies a list binding. Agent has no Task creation-binding parameter. Use `Agent(action="message", recipient, content)` for follow-up work on an authorized retained instance; it preserves history and configuration and returns acceptance, not completion. `Agent(action="list")` discovers authorized members without consuming results or scheduling work.

Definition names and descriptions reach model-visible delegation guidance. Directed `@agent` selection constrains definition-based creation independently of alias and preserves the same security and admission checks.

<a id="7-team-and-subagent-semantics"></a>

## Teams

### Capability and startup

Application configuration uses boolean `agent.team`, false by default; see [Agent configuration](../../README.md#agent-delegation). Only interactive application Sessions honor enabled Teams; print and RPC candidates disable Team capability, including runtime replacements, so named children remain ordinary subagents. Direct SDK callers retain explicit `TeamCapability` control. Capability, initialized Team scope and actual teammate instances are separate facts.

Teammates are always created asynchronously and collaborate through messages; ordinary subagents follow the [delegation mode rules](design-parallel-agent.md#foreground-and-background-delegation).

With Teams enabled at Session startup, the application resolves the list before candidate construction, registers the lead and awaits Team infrastructure initialization before SessionStart/ready access. Initial config and the lead inbox establish a fixed lead and shared list; they do not create workers or define a roster.

The lead model chooses teammate count, aliases, definitions and work prompts from the user's collaboration request.

| Creation request | Routing |
| --- | --- |
| No alias | Ordinary subagent, including when the parent belongs to a Team. |
| Alias with enabled capability and initialized Team scope | Teammate admission, authorized for the fixed lead. |
| Alias with disabled capability or no initialized scope | Named ordinary subagent, without Team membership. |

A Team has one fixed lead and belongs to that lead Session. Teammates do not create nested Teams or transfer membership between Team identities. `lead` is reserved for the lead. Ordinary children inherit list access without inheriting Team membership.

Accepted lead runtime replacement updates subsequent named-spawn routing in the initialized scope. Rejected candidates do not mutate routing. Disabling capability does not stop members, remove Team files or rebind the list. Enabling capability through replacement does not initialize an absent scope.

### Admission and alias publication

Teammate startup is staged:

```text
capture lead registration and reserve alias
  -> prepare complete isolated candidate
  -> validate identity, kind, definition, directory and inherited list
  -> register Starting member and establish application Session association
  -> publish member config and inbox
  -> accept initial startup and publish Ready membership
```

Starting teammate registrations do not provide caller-bound discovery, communication or Task access. Alias reservations cover pending startup and closing members. Failed startup compensates only its captured reservation and registration. File failures retain closing membership and aliases for retry rather than freeing a name still associated with unfinished cleanup.

Team namespace collisions are rejected before adoption or mutation. Shortened directory names never merge unrelated Teams. Team files and actor startup use compensation across boundaries, not a distributed transaction.

### Authority and communication

Authority derives from captured live registration, creation relationships, fixed Team lead association and explicit grants. `AgentKind`, master naming, definition selection and aliases describe identity or relationships, not permission levels.

Same-Team membership permits scoped discovery and messaging. Shared lists permit work-record cooperation. Neither grants arbitrary sibling execution control, stop or full-history access. Target-local permissions, sandbox, protected roots and extension/MCP isolation remain effective; delegated Agents cannot mutate Tape.

Team aliases resolve within the caller's Team; creators can also address their named ordinary children without Team membership. Names resolve to captured registrations and grant no authority; use the instance ID when a name is unavailable or ambiguous. Messages retain the captured recipient across alias reuse. Bounded ordered messaging uses the recipient's existing loop:

```text
capture target and reserve ordered message
  -> record Team inbox file
  -> publish message ready for recipient admission
  -> commit recipient canonical history
  -> process through ordinary execution
```

Sender success requires inbox publication to succeed. Recording, delivery and completed processing are different observations. Accepted recording survives waiter loss and does not allow later messages to overtake it. Mailbox reads and UI polling do not consume messages or activate a model. Inbox files are runtime records, not a restart replay service.

Each child's full history remains in its own Transcript. Initial report or failure delivery follows [delegation mode](design-parallel-agent.md#foreground-and-background-delegation), with no child-authored message. This applies whether or not Teams are enabled. Team-scoped idle and exit behavior applies only to instances admitted as teammates; an unnamed ordinary child remains a subagent even when its creator belongs to a Team. Following Claude's Team contract, a teammate's idle notification to the lead includes its final answer or error, not just a status bit. Idle notification is distinct from initial completion and from explicit shutdown; delivering the same initial report twice is not useful. Phi returns initial completion and later ordinary subagent answers or errors to the creator, publishes later teammate answers or errors to the lead on idle, and records shutdown exit notices separately. Task state remains in the shared list rather than status messages. Addressed messages carry questions, blockers, findings, or changes of direction. Completion and messages use ordinary actor admission/continuation paths independently of Session save success, subject to [execution feedback policy](../design-harnes.md#execution-feedback): observations do not become prompts, ordinary messages are actionable content rather than status sync, and automatic reports remain passive while the recipient is user-cancellation-suspended.

## Lifecycle gates and teammate cooperation

Typed hooks receive Task content and trusted instance/list/Team context. Lua converts dynamic values at its adapter boundary.

| Hook | Trigger and behavior |
| --- | --- |
| `TaskCreated` | Gates creation before publishing the Task record. Rejection publishes no Task. |
| `TaskCompleted` with `ExplicitUpdate` | Gates a transition from unfinished to completed. Rejection preserves prior work state and dependency blocking. |
| `TaskCompleted` with `TeammateEndTurn` | Checks the teammate's owned in-progress Tasks at the turn boundary without changing their status. |
| `TeammateIdle` | Allows idle cooperation, returns feedback for continuation, or requests managed stop. |

Hooks run outside list and registry locks. After callbacks, the harness rereads canonical records and revalidates the captured context, affected records and dependencies—not whole-list equality. Completion merges into the latest records and validates the resulting graph; unrelated edits survive. Creation also rechecks its allocator candidate. Relevant conflicts return feedback instead of overwriting concurrent changes. Errors fail closed. Reads, deletion and repeated completed status do not repeat creation/completion gates.

### Retained teammate loop

Before publishing TurnFinished, the active Agentic Loop checks owned in-progress Tasks and runs TeammateIdle. Rejection supplies feedback through ordinary internal prompt preparation and continues the same turn; it does not publish provisional success or start another turn. Feedback continuation is bounded to eight rounds, and external Steer resets that bound. Hook errors fail the active turn. Stop requests managed termination after completion.

Once execution, submissions and interactions are idle:

1. Check current definition/ToolRegistry availability for Task.
2. Select eligible owned work first, then unowned pending work, using the lowest numeric ID in either group.
3. Authorize the exact candidate and owner patch through the generation-local permission runtime.
4. Reacquire the list lock and compare the candidate before owner-only claim.
5. Prepare an internal prompt through the ordinary prompt hook path and start recorded work on the existing actor.

Claim does not mark work in progress or completed. The model explicitly inspects, starts and completes it. A missing candidate schedules a one-second poll without rerunning idle hooks or issuing a provider request. Permission denial ends the check. Automatic continuation is bounded to eight turns per episode; external work resets the episode.

Interrupt and stop cancel in-turn gates; Steer discards stale gate feedback. Runtime replacement remains Busy during active execution. After completion, replacement and new submissions invalidate stale idle checks and preparation. Gate hooks and blocking Task work do not block actor command handling. This cooperation belongs to the retained teammate lifecycle, not a separate scheduler or worker launcher. A file refresh alone never dispatches work.

### Termination and cleanup

Ordinary turn interruption is not teammate termination. An idle report announces finished work while the teammate remains addressable; it is not an exit or Task completion. On known teammate shutdown or termination, captured cleanup access resets its unfinished owned Tasks to pending/unowned, preserves completed and other-owner work, and records an exit notice for the captured lead before removing membership files and releasing the alias. Exit notices report cleanup facts, not another copy of the initial report.

Cleanup progress supports in-process retries across partial writes, notice publication failures and concurrent stop requests. Known lead/root closure does not reactivate the lead for an exit notice. Lost stop waiters do not abandon accepted cleanup.

Main-owned finalization closes creation descendants before removing the Team config/inbox directory. Task files remain. Session-save and cleanup failures are reported independently and retain the resources needed for retry. Abrupt process death does not imply automatic owner repair, lead promotion, worker reconstruction or mailbox replay.

## Storage and Session associations

```text
<phi config root>/
  tasks/<task-list-id>/
    .lock
    .highwatermark
    <task-id>.json
  teams/<team-namespace>/
    config.json
    inboxes/<alias>.json

<phi session root>/<encoded-session-scope>/
  <root-session-file>.jsonl
  <parent-session-id>/subagents/agent-<child-session-id>.jsonl
  <teammate-session-file>.jsonl
```

### List binding

`TaskListBinding` captures an absolute storage root and validated list namespace. Constructing it performs no IO and grants no live access.

Selection occurs before execution:

1. Explicit typed binding, checked against any saved list reference.
2. Saved Session list ID.
3. Team namespace for a fresh Team-enabled master Session.
4. Full Session ID for a fresh standalone Session.

Ordinary children inherit the initiating resolved binding; teammates share the lead's binding. Without an explicit or saved selection, Team namespace and list ID are `session-` plus the first eight characters of the lead Session ID. An explicit shared list can differ from the Team directory namespace.

Resume preserves the selected reference. Runtime replacement preserves the binding; conflicting intent fails without moving records. Sharing files across processes grants no live attach/control capability. Task retention is independent of Session/Team lifetime; finalization performs no Task expiry sweep.

### Independent records

| Record | Canonical responsibility |
| --- | --- |
| Task JSON | Work content, status, owner and prerequisite identity. |
| Team config | Team ID, namespace, fixed lead ID, list ID and member references: instance, alias, optional definition and working directory. |
| Team inbox | Recorded messages for the live collaboration scope. |
| Session journal | Transcript and application metadata, including historical kind/definition/alias, list reference and storage association. |

Session metadata does not mirror a Task board. Saving a Transcript neither authorizes Task completion nor acknowledges Task-file durability. A later Session save failure does not roll back or hide a successful Task mutation.

### Ordinary sidechains and teammate history

Ordinary subagent history uses its immediate parent's storage association, even if execution working directories differ. `SessionSidechain` captures the storage working directory and parent ancestry. Each parent contributes `<session-id>/subagents/`; nested children extend that ancestry. A teammate has an independent Session in its own scope; its ordinary children start sidechains there.

Path IDs identify Sessions: `<parent-session-id>` is the immediate parent's Session ID, and `<child-session-id>` is the child's Session ID. Session, AgentInstance and Transcript IDs stay aligned, but storage ancestry expresses Session association, not runtime authority.

Root and teammate journals retain the timestamp-ID filename convention. Historical kind and creator links remain passive references, not restored registrations or resumable delegated workers.

Exact lookup traverses the bounded scope/parent/subagents grammar without following symlinks; ambiguous IDs fail. Recent root lists exclude delegated histories. Passive child reads verify canonical ID and creator linkage. Session deletion removes only its journal, leaving child histories and Tasks independently discoverable. Journal identity and storage ancestry are validated before deriving write or lock paths.

### File correctness

Task operations reread the complete bounded list under a cross-process list lock. The Task module validates IDs, record shape, graph consistency and payload sizes; the store validates filesystem objects and bounds lock contention. Blocking IO runs on the blocking pool, outside registry locks.

Writes use same-directory temporary files and safe replacement. Multi-record dependency/deletion operations report partial failures explicitly; the lock does not make several file replacements crash-atomic. Invalid remaining graph data fails closed. There is no multi-store commit or replay protocol.

Storage bounds include 4,096 Tasks and 16 MiB per list, 128 KiB per record, 1 KiB subject/active form, 64 KiB description and 16 KiB metadata. Tool pages contain at most 100 Tasks. Canonical rereads recover missed changes without a file-notification dependency.

## Application projection and controls

`AgentInstance::observe_tasks` returns a complete owned `TaskListView` from one canonical locked read. Its `TaskItemView` values include content, owner, status and derived dependencies without exposing mutable records or stores. Observation is bound to a live registration and never claims or executes work.

The interactive adapter refreshes in the background once per second and rejects results from outgoing registrations. Read errors preserve the last successful projection and appear separately from work status.

- The progress area shows counts and up to three unfinished items.
- Ctrl+T toggles the Tasks overlay; `/tasks` opens it. Checklist shows recorded work, descriptions and dependencies; Activities (`agents & commands`) shows running and finished subagents/Bash commands. Tab or Left/Right switches tabs.
- Status markers are `[ ]`, `[-]` and `[x]`; marker color distinguishes pending, in-progress and completed work.
- Enter opens the assigned Agent's existing Session UI only through authorized live/history access. Missing owners or denied access produce a notice, not navigation or execution.

Agent presentation uses structured definition, alias, instance and invocation identities. Activities and Checklist remain separate projections; activity completion never implies Task completion. Activity controls request SDK effects rather than mutate Task records.

Print and RPC use the same runtime/finalization rules. Successful or failed standalone print execution leaves unfinished checklist records unchanged. RPC `task_stop` targets captured running subagent work through its accepted initial-turn identity; checklist IDs, instance IDs and stale running-work bindings are invalid targets. Stopping execution is not Task deletion or completion. RPC exposes no generic Task/Team service.

## Worked flow

```text
Lead A starts with Teams enabled
  -> initialize Team scope/config/inbox and resolved shared list
  -> no teammate workers yet
A calls Task(action=create) for security review               returns "1"
A calls Task(action=create) for performance review            returns "2"
A calls Task(action=update, task_id="2", add_blocked_by=["1"])
A creates two Agents with action=create:
  role="code-reviewer", alias="security",
    prompt="Claim Task 1, review security, report and explicitly complete it."
  role="code-reviewer", alias="performance",
    prompt="Review Task 2 and start when its prerequisite is completed."
  -> B and C have independent Transcripts and share A's list
B claims Task 1 and explicitly marks it in_progress
C reads Task 2; an early self-claim is rejected
B reports findings and explicitly completes Task 1
  -> completion gate passes; Task 2 becomes eligible
C continues through coordination or its retained idle check
  -> claims Task 2, starts, reports and explicitly completes it
A synthesizes the reports
Finalization closes members and saves their Sessions
  -> remove Team config/inboxes; retain Task records
```

Without recorded work, `Agent(action="create", role="code-reviewer", prompt="Review auth changes and return findings.")` uses the same independent Agent execution without any Task operation.

## Design references

Phi's Task/Team design draws on Claude Code's separation of Agents, recorded work, Team coordination and conversation history. These references preserve the design rationale and source context. Phi's contracts are defined in this document; external private formats are not compatibility requirements.

### Official documentation

| Reference | Design concern |
| --- | --- |
| [Task tool inputs](https://code.claude.com/docs/en/agent-sdk/typescript#tool-input-types) and [outputs](https://code.claude.com/docs/en/agent-sdk/typescript#tool-output-types) | Work-item identity, content, status, ownership and dependency operations. |
| [Todo lifecycle](https://code.claude.com/docs/en/agent-sdk/todo-tracking#todo-lifecycle) and [progress tracking](https://code.claude.com/docs/en/agent-sdk/todo-tracking#display-progress-in-real-time) | Explicit work progress, assigned IDs and tool/result correlation. |
| [Agent Team architecture](https://code.claude.com/docs/en/agent-teams#architecture), [assignment and claiming](https://code.claude.com/docs/en/agent-teams#assign-and-claim-tasks), and [limitations](https://code.claude.com/docs/en/agent-teams#limitations) | Shared lists, fixed lead, member coordination and concurrent claims. |
| [Team enablement](https://code.claude.com/docs/en/agent-teams#enable-agent-teams) and [runtime disabling](https://code.claude.com/docs/en/agent-teams#claude-spawns-teammates-instead-of-subagents) | Capability activation and named-spawn routing. |
| [TaskCreated](https://code.claude.com/docs/en/hooks#taskcreated), [TaskCompleted](https://code.claude.com/docs/en/hooks#taskcompleted) and [TeammateIdle](https://code.claude.com/docs/en/hooks#teammateidle) | Distinct work-record and teammate lifecycle gates. |
| [Interactive Task lists](https://code.claude.com/docs/en/interactive-mode#task-list), [application data](https://code.claude.com/docs/en/claude-directory#application-data), and [subagent history](https://code.claude.com/docs/en/sub-agents#resume-subagents) | Shared-list scope, separate canonical records and parent-scoped history. |
| [Tools reference](https://code.claude.com/docs/en/tools-reference) and [SDK Query controls](https://code.claude.com/docs/en/agent-sdk/typescript#query) | Agent operations, checklist tools and separate running-work stop control. |

### Implementation observations

These version-pinned third-party sources informed file layout, allocation and cooperation semantics. They are implementation observations, not official guarantees of Claude Code's private schema or algorithms. Source mirrors and renamed bundles are not authenticated official releases.

| Reference | Design concern |
| --- | --- |
| [claude-wrapper 0.14.7 Task parser](https://docs.rs/claude-wrapper/0.14.7/src/claude_wrapper/tasks.rs.html), observed against CLI 2.1.219 | Independent read-only observation of numeric filenames and persisted Task fields. |
| [External Task Injection](https://github.com/shcv/harness-investigations/blob/710e007fd0d078ac47cd7e6c079378a4a8888402/docs/external-task-injection.org) and [Task-management analysis](https://github.com/shcv/harness-investigations/blob/710e007fd0d078ac47cd7e6c079378a4a8888402/features/task-management.org) | Per-record files, locks and allocation bookkeeping; the older 2.1.19 analysis also documents version-dependent naming. |
| [Task storage implementation mirror](https://github.com/leaf-kit/claude-analysis/blob/18277dfe971b230e4f1e49c0099becdc35d98d93/src/utils/tasks.ts) | Decimal-string allocation, high-water mark, locked owner-only claim, dependency pruning and teammate-exit release. |
| [TaskUpdate handler mirror](https://github.com/leaf-kit/claude-analysis/blob/18277dfe971b230e4f1e49c0099becdc35d98d93/src/tools/TaskUpdateTool/TaskUpdateTool.ts#L123-L359) | Cooperative updates, assignment, metadata merge/null deletion and completion gates. |
| [Renamed CLI 2.1.215 bundle](https://github.com/vibewatch/claude-code-internals/blob/714330920c7528c652b8ca560d0d92fe85e9a60d/claude-code-pkg/src/entrypoints/cli.renamed.js) | [Startup initialization](https://github.com/vibewatch/claude-code-internals/blob/714330920c7528c652b8ca560d0d92fe85e9a60d/claude-code-pkg/src/entrypoints/cli.renamed.js#L932000-L932440), [named-spawn gate](https://github.com/vibewatch/claude-code-internals/blob/714330920c7528c652b8ca560d0d92fe85e9a60d/claude-code-pkg/src/entrypoints/cli.renamed.js#L394326-L394390), [list selection](https://github.com/vibewatch/claude-code-internals/blob/714330920c7528c652b8ca560d0d92fe85e9a60d/claude-code-pkg/src/entrypoints/cli.renamed.js#L341045-L341050), and [teammate idle claim/polling](https://github.com/vibewatch/claude-code-internals/blob/714330920c7528c652b8ca560d0d92fe85e9a60d/claude-code-pkg/src/entrypoints/cli.renamed.js#L391728-L391932). |

### Official Python SDK boundary

The inspected `anthropics/claude-agent-sdk-python` version is **0.2.168**, commit **f93e29cc951cd5fddcaaed9f3c49c3c38dedd6ab**.

- [CLI transport](https://github.com/anthropics/claude-agent-sdk-python/blob/f93e29cc951cd5fddcaaed9f3c49c3c38dedd6ab/src/claude_agent_sdk/_internal/transport/subprocess_cli.py): the Python SDK launches the CLI; built-in Task storage belongs behind that boundary.
- [Message types](https://github.com/anthropics/claude-agent-sdk-python/blob/f93e29cc951cd5fddcaaed9f3c49c3c38dedd6ab/src/claude_agent_sdk/types.py) and [parser](https://github.com/anthropics/claude-agent-sdk-python/blob/f93e29cc951cd5fddcaaed9f3c49c3c38dedd6ab/src/claude_agent_sdk/_internal/message_parser.py): generic tool results and typed background lifecycle observations are separate surfaces.
- [Client stop control](https://github.com/anthropics/claude-agent-sdk-python/blob/f93e29cc951cd5fddcaaed9f3c49c3c38dedd6ab/src/claude_agent_sdk/client.py#L443-L469) and [internal query handling](https://github.com/anthropics/claude-agent-sdk-python/blob/f93e29cc951cd5fddcaaed9f3c49c3c38dedd6ab/src/claude_agent_sdk/_internal/query.py#L857-L935): stopping running background work is distinct from mutating checklist records.
