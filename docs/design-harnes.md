# Harness design

## Harness, environment, and application

| Concern | Owner | Boundary |
| --- | --- | --- |
| Harness | SDK Manager and Instance | Coordination, model/tool execution, context, canonical history |
| Environment | Existing execution resources and security mechanisms | Files, commands, directories, permissions, sandbox |
| Application | Runtime preparation and Session associations | Configuration, Session association, persistence, client access |
| Presentation and transport | Clients, UI, server adapters | Access and projection, never execution ownership |

External Agent APIs inform these ownership boundaries without defining Phi's domain contracts. [OpenAI multi-agent](https://developers.openai.com/api/docs/guides/tools-multi-agent), [Agents API architecture](https://developers.openai.com/api/docs/guides/agents-api/architecture), [Codex subagents](https://developers.openai.com/codex/agent-configuration/subagents), and [Claude Code teams](https://code.claude.com/docs/en/agent-teams) inform runtime/tool separation, not additional compatibility layers.

Human-control references: Anthropic's [trustworthy-agent framework](https://www.anthropic.com/news/our-framework-for-developing-safe-and-trustworthy-agents) and [Building effective agents](https://www.anthropic.com/engineering/building-effective-agents), and OpenAI's [practical guide](https://openai.com/business/guides-and-resources/a-practical-guide-to-building-ai-agents/).

## Factory and AgentInstance boundaries

```text
PhiBuilder -> Phi -> validated AgentInstanceCandidate -> AgentManager -> AgentInstance
```

Phi is reusable factory intent/services, not a live Agent. Repeated builds create independent validated candidates with their own permission state, fixed interaction routing and extension VM; preparation alone does not register or start execution. The candidate retains matching generation resources for application boundaries. Manager consumes a creation candidate and registers the private actor. One-shot query/stream helpers use the same instance path, not another loop.

AgentInstance owns canonical history, session-scoped Memory/Tape state, progression/reduction, submissions, interactions and exactly one active runtime generation. Manager owns cross-instance task relationships, not instance history. Application AgentAttachment/SessionAccess are not SDK execution types.

Same-ID replacement validates a complete generation, optional branch selection and configuration-history intent before committing history/generation at one actor boundary. Only acceptance additively inherits the active generation's confirmed Session grants, including grants confirmed after preparation; candidate policy still applies. Rejection preserves active state, and retired resources cannot mutate the new checker. Acceptance preserves identity and unrelated stable state. SDK enforces readiness, pending preparation and active/waiting task restrictions. Mutable routing stays generation-scoped within its instance even when immutable resources or connections are pooled. See [construction intent](../phi-sdk/src/harness/instance/builder.rs), [resource assembly and acceptance](../phi-sdk/src/harness/instance/execution/runtime.rs) and [runtime replacement](agent/design-agent-runtime.md#candidate-first-runtime-replacement).

### Harness responsibility boundaries

Manager manages relationships and communication; Instance owns Agent lifetime; loop drives one execution through completion.

| Boundary | Owns | Excludes |
| --- | --- | --- |
| `manager/` | Phi harness management: membership, registration-bound relationships/authority, Teams, creation/delegation admission, inter-Agent addressing/mailboxes/delivery and scoped control coordination | Model iteration, mutable instance history, business planning or result judgement |
| `instance/` | One live Phi Agent: identity, serialized lifecycle, runtime generation, specialization, submissions, interactions, canonical Transcript, active-loop hosting and local child-result/idle handling | Other Agents' execution, global membership or application Session persistence |
| `agentic_loop/` | One execution: canonical request context/compaction, model/tool iteration, retries, Steer safe points, tool scheduling/governance/finalization, reduction, completion gates and terminal ordering | Agent lifetime, registration/Team authority, mailbox routing or application policy |

The loop follows [Claude's agent-loop design](https://code.claude.com/docs/en/agent-sdk/agent-loop): receive a prompt, evaluate, execute tools, feed results back, repeat and return an outcome. Completion feedback may continue that same execution before a final outcome is published. This is a behavioral reference, not an adoption of Claude's subprocess architecture, Session storage, API vocabulary or internal module layout; Phi retains its own turn and Transcript contracts.

A loop finishing, failing or being interrupted is not Instance termination. An Instance may host successive executions while retaining identity, specialization and history. Manager authorizes and coordinates creation and creation-subtree stop; the affected Instances serialize startup, cancellation and finalization themselves. Initial child completion and later reports remain distinct from membership retirement.

Completion gates stay in loop because they control whether the current execution finishes or continues. Registration readiness, teammate identity and Task cooperation policy are supplied through Instance-owned integration, not interpreted as loop-owned orchestration. Task owns recorded work facts; Instance owns retained idle episodes and future execution admission. Manager routes resulting messages without deciding their business meaning. Model-tool schemas and result formatting remain adapter concerns.

These are ownership constraints, not a claim that every current implementation dependency already conforms. The source map below describes the current layout; moving code must preserve actor ordering, cancellation and completion gates without creating a second state owner or a speculative generic policy framework.

### Instance implementation boundaries

Files group complete responsibility chains, not fixed line counts. Colocated mechanisms retain separate semantic ownership; private implementation paths are not SDK compatibility paths.

| Mechanism | Source and boundary |
| --- | --- |
| Construction intent | [builder.rs](../phi-sdk/src/harness/instance/builder.rs): semantic options and opaque validated candidates, not concrete resource assembly |
| Runtime resources | [execution/runtime.rs](../phi-sdk/src/harness/instance/execution/runtime.rs): isolated candidate assembly, scoped services and generation acceptance |
| SDK instance access | [handle.rs](../phi-sdk/src/harness/instance/handle.rs): control, identity, owned views/subscriptions, streams and queries; no mutable execution ownership |
| Actor serialization | [actor.rs](../phi-sdk/src/harness/instance/actor.rs): commands, progression, publication, interaction observations, quiescence, finalization and retained teammate idle episodes, including polling, continuation bounds and worker invalidation |
| Hosted execution | [execution/mod.rs](../phi-sdk/src/harness/instance/execution/mod.rs): canonical state, one active loop, interruption and trusted tool dispatch |
| Input lifecycle | [execution/submission.rs](../phi-sdk/src/harness/instance/execution/submission.rs): prompt recording/startup, admission, queues, messages and input-time Memory wiring |
| Delegation | [execution/delegation.rs](../phi-sdk/src/harness/instance/execution/delegation.rs): specialized invocation and runner construction, directed outcomes and child coordination/feedback; never child execution ownership |
| Teammate cooperation | [execution/teammate.rs](../phi-sdk/src/harness/instance/execution/teammate.rs): permission checks, continuation preparation, idle reporting and managed stop wiring |
| Child invocation facts | [child.rs](../phi-sdk/src/harness/instance/child.rs): admission, cancellation, immutable initial completion and child observation contracts; not loop completion gates |
| Interaction routing | [interaction.rs](../phi-sdk/src/harness/instance/interaction.rs): permission/elicitation routing and pending replies, separate from event observation |

Tests follow the verified responsibility, not their fixture dependencies. Small private-mechanism tests remain inline. [Instance tests](../phi-sdk/src/harness/instance/tests.rs) group observation, runtime replacement/reset, workspace binding, cross-turn tool policy, interruption, delegated completion and teammate behavior under [instance/tests/](../phi-sdk/src/harness/instance/tests/). [Loop tests](../phi-sdk/src/harness/agentic_loop/tests.rs) cover canonical commits, compaction/budget, retries, completion gates and tool-output finalization. [Harness tests](../phi-sdk/src/harness/tests/) cover delegation, message delivery and scoped Task-tool integration across owners. All three suites reuse [support.rs](../phi-sdk/src/harness/tests/support.rs); using Manager or a mock provider alone does not justify lifting a test. Test relocation does not widen production visibility.

## Agent-loop ownership rule

Phi owns model/tool iteration, turn counting/boundaries, Steer safe points, lifecycle, finalization and event ordering. Rig is not a second loop/history owner.

Per-tool scheduling remains Phi policy: Bash defaults to parallel execution; any sequential call makes the batch sequential, otherwise bounded concurrent execution preserves provider result order. Dependent Bash commands must use one ordered shell command or separate model turns. Authorization follows final input rewriting, while Bash also retains original-command evidence. Durable effects finalize against a preallocated Transcript entry ID before result publication.

## Turn and agent-loop model

Intent, submission, prompt and turn follow [global terminology](design.md#intent-submission-prompt-and-turn). One outer turn may contain several provider responses/tool batches.

```text
Outer turn
  -> canonical Transcript projection -> provider response -> optional tool batch
  -> commit -> subsequent rounds (Steer safe points)
  -> completion gate -> feedback continues this turn, or finish
  -> final response / failure / cancellation
```

Instance admits input and starts runners; `context.rs` awaits startup hook readiness, runs `UserPromptSubmit`, optionally intercepts command expansion through `UserPromptExpansion` before invoking the application expander, then prepares `PromptContext` contributions before scheduling. Expansion is a host stage, not a hook handler's workflow; see [Hook design](design-hook.md#prompt-preparation-and-memory). Preparation does not commit history, but a started Once handler is already consumed. Slow input preparation is polled outside command handling so execution, permission replies, observation and abort/stop remain responsive. Each provider attempt awaits canonical context after preceding events commit; runners keep no growing history. Slow compaction and completion hooks run in abortable workers, with short commits on the actor. Terminal success follows completion gates and observational end hooks. Core defines foundational prompt/event semantics without starting execution. `SubmissionDispatch` selects Steer/Follow-up routing; the preparation hook receives the resolved `UserPromptDispatch`:

| Dispatch | Execution boundary |
| --- | --- |
| Immediate | Start a turn when idle |
| Steer | Join active work at a safe provider-request boundary |
| Follow-up | Start after prior work and earlier queued submissions |

Drain policy selects how many pending submissions dispatch; it does not prepare or classify content. Unconsumed Steer returns to the persistent queue and precedes ordinary Follow-ups. A safe point consumes input only when another model round remains; exhausted budgets leave input for a new turn. If the active runner ends during preparation, later Follow-ups still queue behind accepted input; a Steer queues without requiring a live runner and retains its priority. No runner is not equivalent to no pending input. Abort/stop discards pending preparations before they can commit.

A tool declaration or an individual tool result may opt into terminating automatic continuation after a successful result. The loop still publishes and commits every ToolResult; it skips the next model request only when the nonempty batch's finalized results all opt in and succeed. Failed, denied, mixed or non-opted-in batches continue normally. Pending Steers are handled before termination while a model round remains. This is a scheduling decision, not a report of background work completion or a new delivery channel; later feedback follows ordinary Instance admission. [Workflow launch](design-workflow.md#result-delivery) and successful background Agent creation use it to avoid a redundant startup reply. Agent opts in from the admitted effective mode, not the requested mode; foreground results, discovery and messaging retain normal continuation.

After successful foreground directed delegation, the summary request exposes no tool declarations or dynamic tool guidance and selects no tool calls. This request-local projection uses the admitted effective mode, preserves canonical ToolResults and leaves the execution registry and hard directed-call guards unchanged. Later ordinary turns retain their tools; background directed delegation still ends after its launch receipt.

`TurnPrompt.tool_policy` carries transient `TurnToolPolicy` allowances and whole-tool removals, not resource metadata or durable grants. Caller-supplied policy validates before preparation. Rust `UserPromptSubmitOutcome::ContinueWithPolicy` contributions merge with that policy; malformed intent and resolved MCP identities are rejected before admission/commit, including delegated initial input and Stop continuation feedback. Prompt rewrites preserve the merged intent. Queued, Handled and rejected inputs activate nothing. Outer dispatch initializes a scope; actual Steer injection unions/deduplicates both sets, with removals winning. New turns, including Follow-ups and unconsumed Steers, use only their own intent. Inner AgentTurn boundaries do not expire policy; completion/failure and synchronous runner abort/drop retire it. Children, turns started by Agent messages, new generations and unrelated extension commands inherit none. Same-turn peer injection contributes no tool policy and does not clear human Tape evidence; canonical history retains the Agent role.

Permission owns [outer-turn scoped state](../phi-sdk/src/permission/execution_scope.rs), bound to ToolSet and permission-checker identities; execution controls activation and retirement. Parallel tool futures share the scope; model Lua tools explicitly capture it into blocking work. Each provider request projects tool definitions and dynamic tool guidance together.

Initial and retained task prompts use the same instance preparation path with Internal source when selected for execution. Candidates do not commit raw task prompts. Prepared content/context commit once after runner validation and closure checks; cancelled queued tasks start no turn. Intercepted initial preparation rejects startup; an already accepted follow-up settles independently.

`SubmissionId` correlates ordinary accepted work, not editor actions, turn/task identity or durable replay. It remains pending until canonical commit or explicit discard, including runner handoff. Receipts preserve accepted content/identity; coherent views list pending IDs without queue payload copies. Transient submission identity is not a new service/entity.

Implementation: [input admission](../phi-sdk/src/harness/instance/execution/submission.rs), [hosted execution](../phi-sdk/src/harness/instance/execution/mod.rs), [prompt preparation](../phi-sdk/src/context.rs), [actor dispatch/views](../phi-sdk/src/harness/instance/actor.rs) and [core turn contracts](../phi-sdk/src/core/turn.rs).

The [agentic_loop/](../phi-sdk/src/harness/agentic_loop/) module separates resolved runtime/request configuration, asynchronous runner iteration, canonical pipeline/context/completion workers, response normalization and tool execution/finalization. Runner and pipeline are mechanisms of the Instance's active loop, not independent history or execution owners. Their commits occur through the actor's short exclusive Transcript access.

## Events, reduction, and interactions

```text
Private runner events -> EventPipeline/reducer -> canonical Transcript + CoreEvent
  -> scoped public PhiEventEnvelope -> application/adapter projection
```

Reducer is the normal commit path for assistant content, reasoning, tool calls/results, usage and turn completion. Final canonical content—not provisional display deltas—is commit truth. On explicit abort, the Instance stops the runner, drains already queued events and commits any unfinished nonempty Assistant content once with `stop_reason = "aborted"`. Partial original text, reasoning and images remain in the Transcript; completed messages are not duplicated and unavailable final usage is not estimated. Abort uses the same reducer completion path and publishes already queued events and final `ReasoningFinished`/`AssistantFinished` observations before `RunCancelled`, allowing clients to end streaming in place without snapshot replay. Cancellation does not synthesize successful `TurnFinished`. Outer turn events and inner Agent-turn lifecycle are distinct.

Classified transient provider failures permit at most three retries after the initial request, including interrupted streams. Partial Assistant content commits through the same settlement path with `stop_reason = "error"` before retry or `TurnFailed`; already executed tools are not replayed. Tool errors return as model-visible tool results. Esc preserves content and never retries.

Blocking interactions use immutable scoped requests and semantic replies separately from event streams. Instance's [interaction Broker](../phi-sdk/src/harness/instance/interaction.rs) owns pending maps and routing. Raw permission/MCP reply channels never escape the SDK; clients cannot author completion through observation.

`AgentInstanceView` captures history and execution at one actor boundary. `view_with_event_boundary` adds a correlated `ViewBoundary` for bounded-stream reconciliation; it neither starts execution nor checkpoints. See [coherent observation and recovery](agent/design-agent-runtime.md#snapshot-and-event-recovery).

`AgentSnapshotSubscription` keeps Tokio watch private. `snapshot()` copies without marking the value seen; `changed()` returns an owned latest value and marks it seen. Updates may coalesce, clones track consumption independently, and cancelled waits consume no notification. An unseen final update remains readable before channel closure. Retaining a returned value cannot block publication, Stop or resource retirement; dropping a subscription never stops the Agent.

### Execution feedback

Execution facts, model context and model wakeup are independent decisions. An event may update state, UI/RPC and persistence without becoming model input; model-visible context need not start execution. Turn below means an outer Phi turn, not one provider request.

| Message or fact | Model-context delivery | Execution scheduling |
| --- | --- | --- |
| Admission/delivery receipts, status, progress, output growth, mode handoff, save/cleanup progress, UI/desktop notifications | No automatic prompt; handled by deterministic control/observation paths | None |
| Workflow launch / background Agent creation acceptance | Canonical ToolResult; not a terminal execution result | Successful sole/all-terminating batch ends the launching turn without an extra provider request; queued Steers take precedence |
| Permission request/answer | Interaction protocol, not a new task prompt; denial or failure returns through the original ToolResult | Settle the existing call |
| Foreground Bash/Agent completion, failure or cancellation | Existing ToolResult | Continue the current turn; no extra feedback turn |
| Background Bash or initial background subagent completion, unexpected failure, timeout or interruption | Terminal result with provenance and relevant recovery context | Queue a later turn, never inject into an active turn |
| Explicit user cancellation of background work | Passive canonical Agent feedback | No wakeup or automatic recovery turn |
| Lifecycle cleanup | Retain terminal observations; no recovery prompt | None |
| Undelivered background Bash history from a previous runtime | Passive context; a historical running receipt is not evidence of a live execution | None; do not wait for the old runtime's completion notice |
| Ordinary Agent coordination | Agent-authored prompt content, not human input or approval | Eligible safe-point injection while busy; otherwise a later turn |
| Later subagent result / teammate idle report | Automatic Agent report to creator / fixed lead | Follow-up turn, not safe-point peer injection |
| Teammate exit without released unfinished Tasks | Passive Agent context | None |
| Teammate exit releasing unfinished Tasks | Automatic exit fact with released Task IDs | Follow-up turn for coordination |
| Completion-gate correction | Internal feedback before final completion publication | Continue the current turn within its limits |
| Retained idle teammate claims eligible Task work | Prepared internal task prompt | New turn |

Model inputs still pass their owning admission/preparation paths, including hooks for internal reports and Agent messages. Terminal internal feedback preparation/start failure retires that attempt rather than rerunning hooks on every pump. `phi_sdk.internal_feedback_failed` retains original feedback and diagnostics as a Transcript event, excluded from provider history; Bash/Workflow delivery markers settle their respective queues without rewriting execution outcomes. The actor reports the error through its existing failure observation and can reach quiescence, which does not imply success. Preparation failure does not commit contributed context, but any one-shot Hook handler that already started remains consumed. User-cancellation suspension below overrides automatic activation, including reports and exit notices that would normally wake a recipient. Task state remains authoritative recorded work, not a request to generate status-sync messages.

Passive context can be visible on an active turn's next normal provider request; it does not require another human message and never initiates a request itself. Eligible initial background results wait behind the active turn and queued Steer/Follow-up submissions. Already finished turns are never rewritten. Background startup receipts and published outcomes remain immutable. Lifecycle cleanup is not normal completion and need not originate from a user action.

Automatic feedback identifies its harness origin; ordinary messages identify Agent authorship. Initial background subagent outcomes, later reports, exit notices, cancellation and restored-runtime feedback explicitly distinguish themselves from human input or approval. Canonical Agent entries become provider user-role messages; Internal submission metadata is not model-visible provenance or a permission grant. Factual results need source/outcome semantics, not an unconditional instruction to resume work.

Ordinary messages are for actionable follow-up work, questions, blockers, findings or changed direction. Callers must not use them for routine status updates already covered by structured observations. The harness does not semantically classify or filter status chatter sent as Ordinary; misuse can still start or extend execution.

**Cancellation enforcement and guidance are distinct:**

- Typed termination reasons determine delivery; intentional cancellation and lifecycle cleanup do not schedule recovery or replay.
- When a retained child observes user cancellation of its initial execution, Instance suspends automatic activation. Pending/incoming ordinary Agent messages are rejected before prompt preparation; background terminal results and structured automatic reports remain passive context. Internal direct submissions cannot resume it.
- A successfully started direct non-Internal user/host submission restores automatic activation. Rejected, intercepted or Internal requests do not. Runtime replacement retains suspension; the immutable initial cancellation result is not replaced. This is a live-instance admission policy, not proof of human consent from source metadata.
- Cancelling a Bash execution does not suspend its owning Agent. Aborting an ordinary turn is interruption, not cancellation of the broader goal or automatic-activation suspension.
- Model guidance forbids autonomously retrying or recreating cancelled work through another command, Agent or the model's own actions. Resume that work only on newer user direction; ask if intent is unclear. Other requested work and the broader goal remain valid. There is no semantic equivalent-work detector or blanket ban on future tool calls; prompts do not enforce deduplication, permissions or scheduling.

Implementation: [submission and message admission](../phi-sdk/src/harness/instance/execution/submission.rs), [Instance activation policy](../phi-sdk/src/harness/instance/execution/mod.rs), [initial cancellation observation](../phi-sdk/src/harness/instance/actor.rs), [Bash feedback](../phi-sdk/src/harness/instance/execution/bash.rs), and [child outcomes](../phi-sdk/src/harness/instance/execution/delegation.rs).

## SDK Agent Instance runtime

The serialized actor admits work and interactions, drives provider/tools, reduces history, accepts complete runtime replacements and publishes canonical facts. Status is an execution projection, not application persistence or task truth. Idle interruption does not imply a fictional turn outcome. Immediate busy admission returns typed `PhiTurnError::Busy`; hook-handled input remains a separate disposition.

The SDK uses coarse [Instance implementation boundaries](#instance-implementation-boundaries): `handle.rs` groups control and owned observation, `actor.rs` groups serialization, publication and retained teammate idle episodes; `execution/` groups state/loop hosting, resource assembly/acceptance, input admission, delegation and teammate cooperation wiring. `child.rs` retains initial invocation facts; `interaction.rs` routes replies. The shared `agentic_loop/` owns one loop's mechanics, not another execution resource. Application preparation and persistence remain outside these modules.

## SDK AgentManager and parallel orchestration boundary

Manager holds instance capabilities, not mutable execution references. It rejects duplicate live IDs and retains reliable final facts after membership removal. Registry locks never span preparation, callbacks or actor awaits.

Application startup registers the master through ordinary registration, without privileged Agent authority. Master names the primary conversation Agent, not a permission role. Live relationships, Team scope and explicit grants determine access; master naming, saved lineage, definitions and aliases do not grant permission bypasses. Delegation stays inside SDK; application child preparation establishes resources, Session associations and persistence without adding a scheduler. Creation lineage, Team membership and checklist Task ownership remain separate. Completion does not automatically stop instances.

See [authority](agent/design-parallel-agent.md#tool-instance-and-manager-validation-boundaries), [Task and Team design](agent/design-agent-task.md) and [child application integration](../phi-interactive/src/runtime/subagents.rs).

## Agent orchestration API

AgentManager exposes one semantic Agent API for membership, creation/delegation, communication, observation and scoped control. Models express intent through adapters; trusted SDK and caller-bound model authority stay distinct.

Tool parsing/action dispatch/JSON presentation belong to [Agent adapter](../phi-sdk/src/agent/tools/agent.rs) and [initiating instance](../phi-sdk/src/harness/instance/execution/delegation.rs). Manager accepts typed intent. Instance-local submissions, interaction replies and coherent views do not need Manager wrappers.

### Team and access contracts

Trusted registration binds capabilities to current registration, not just instance ID. Master names the primary conversation Agent; ordinary registration grants it no privileged bypass or legacy AgentRole authority. Definitions, aliases, creation lineage and Team membership are distinct; reused IDs inherit no grants. Caller-bound policy separates collaboration, history and control. Prompt claims and tool availability never manufacture authority.

Creation reserves identity/capacity and any Team or creator-scoped ordinary child alias, validates candidates, associates where applicable, starts, then accepts. Caller-bound follow-up work uses `AgentAccess::send_message`; the model adapter exposes `action="message"` and `action="list"`, not a separate invocation or receipt-wait action. Messages preserve instance configuration; acceptance, canonical delivery and completed processing remain distinct. Trusted instance-local submissions remain available without a Manager invocation wrapper. Closure/epochs are revalidated across asynchronous boundaries; failed startup compensates captured obligations before release.

See [authority](agent/design-parallel-agent.md#authority), [validation](agent/design-parallel-agent.md#manager-admission-and-internal-validation), [communication](agent/design-parallel-agent.md#general-inter-agent-communication-protocol) and [Task and Team design](agent/design-agent-task.md).

## Module and directory architecture

Instance contracts and execution live together in `src/harness/instance/`; Manager contracts and coordination live in `src/harness/manager/`. The shared `harness/` groups execution and orchestration without merging their ownership. `lib.rs` curates the public facades. Manager serves both direct SDK use and parallel orchestration without application runtime or Session storage. Model tools adapt that API with registration-bound authority. AgentSession and persistence remain in the application.

| Responsibility | Source |
| --- | --- |
| Curated public facades: `agent_manager`, `agent_instance`, `agents` | [SDK exports](../phi-sdk/src/lib.rs) |
| Membership, Teams, registration epochs and explicit grants | [manager/registry.rs](../phi-sdk/src/harness/manager/registry.rs) |
| Creation, preparation/association, compensation | [manager/coordinator.rs](../phi-sdk/src/harness/manager/coordinator.rs), [preparation.rs](../phi-sdk/src/harness/manager/preparation.rs) |
| Access, messaging and control | [policy.rs](../phi-sdk/src/harness/manager/policy.rs), [message.rs](../phi-sdk/src/harness/manager/message.rs), [control.rs](../phi-sdk/src/harness/manager/control.rs) |
| Instance control, observation, progression and canonical history | [instance/handle.rs](../phi-sdk/src/harness/instance/handle.rs), [actor.rs](../phi-sdk/src/harness/instance/actor.rs), [execution/mod.rs](../phi-sdk/src/harness/instance/execution/mod.rs) |
| Child admission, cancellation and immutable initial completion | [instance/child.rs](../phi-sdk/src/harness/instance/child.rs) |
| Specialized invocation/runner construction and child coordination/feedback | [execution/delegation.rs](../phi-sdk/src/harness/instance/execution/delegation.rs) |
| Teammate permission/continuation, idle reporting and managed stop wiring | [execution/teammate.rs](../phi-sdk/src/harness/instance/execution/teammate.rs) |
| Retained teammate idle episode and worker invalidation | [actor.rs](../phi-sdk/src/harness/instance/actor.rs) |
| Definition and model-tool adaptation | [agent/custom.rs](../phi-sdk/src/agent/custom.rs), [tools/agent.rs](../phi-sdk/src/agent/tools/agent.rs) |
| Application Session associations/transitions | [runtime/associations.rs](../phi-interactive/src/runtime/associations.rs), [session.rs](../phi-interactive/src/runtime/session.rs) |
| Application runtime preparation and delegated association | [runtime/factory.rs](../phi-interactive/src/runtime/factory.rs), [subagents.rs](../phi-interactive/src/runtime/subagents.rs) |
| Checkpoints and durable files | [runtime/save.rs](../phi-interactive/src/runtime/save.rs), [session/](../phi-interactive/src/session/) |
