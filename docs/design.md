# Phi Architecture

Phi separates application workflows, execution, terminal UI, sandboxing and remote transport:

```text
phi-interactive: CLI workflows, Session storage, mode composition
       |                   |                       |
       v                   v                       v
phi-remote ------------> phi-sdk                phi-tui
HTTP gateway             Agent runtime          terminal UI
                           |
                           v
                       phi-codemode
```

Libraries do not depend on the application. `phi-sdk` and `phi-tui` are independent. RPC messages are application-local. The gateway consumes public SDK APIs and a narrow application host contract; its browser assets live in `phi-remote/plugins/web/`.

## Design intent

Phi separates execution, orchestration, application preparation, access, persistence and presentation. Agents organize work on behalf of humans; runtime mechanisms enforce requests without becoming planners. Human direction, useful visibility and intervention remain available during autonomous execution—not merely at its beginning and end. This does not prescribe per-action approvals.

Direct CLI and stdio RPC adapters share one application business model. Neither a network service nor an attached observer is part of the Agent execution model. Modes call [concrete application capabilities](agent/design-agent-runtime.md#client-contract-and-deployment-modes).

## Ownership model

- **Harness** — Manager manages relationships and inter-Agent communication; Instance owns Agent lifetime; loop drives one execution through completion. See [responsibility boundaries](design-harnes.md#harness-responsibility-boundaries).
- **Transcript** defines conversation history, entry identity, branches and compaction semantics.
- **Application runtime modules** own preparation, Session associations/boundaries, Session locks and persistence; `main` owns composition/finalization.
- **phi-remote** owns Gateway transport, device authorization and protected deployment storage. The application selects the Gateway directory and composes its lifecycle; persistence belongs to its domain owner, not automatically to the application. See [Gateway ownership](design-remote.md#ownership-and-crate-boundaries).
- **SettingsManager** owns application settings reads and explicit refresh. Application/resource consumers share its in-memory global/project/merged settings; loaders never reread settings. See [settings ownership](design-interactive.md#settings-ownership).
- **AgentAttachment / SessionAccess** provide scoped internal access, never Agent lifetime. There is no universal application facade.
- **UI state** owns projection and interaction; **TuiRuntime** owns reusable widget/terminal mechanics.
- **Tools** own capabilities; one resolved ToolRegistry governs definitions, prompt summaries and dispatch.
- **Lifecycle** names when a Session, execution run, inner model/tool round or operation reaches a boundary; events/outcomes define that boundary's input and accepted effects. **Hooks** execute registered extension handlers at those boundaries and return outcomes to the host: observation, transformation, context contribution or gating. **Handlers** implement the extension logic. The hook subsystem owns registration, matching, local handler scheduling/execution and outcome aggregation; Agent runtime retains the main execution flow and canonical Transcript ownership stays with Transcript. Core defines contracts; execution/application owners reach boundaries, apply outcomes and advance their lifecycle. `phi_sdk::lifecycle` exposes boundary contracts; `phi_sdk::hooks` builds registration and scoped control on those contracts. `Hook::stop` consumes completion contracts, not Agent shutdown intent. See [Hook design](design-hook.md).

Each running Agent, including a child, is an independent AgentInstance. A durable Session or cached UI snapshot is a representation of canonical facts, not another runtime owner.

## Intent, submission, prompt, and turn

| Concept | Meaning |
| --- | --- |
| User intent | Human goals, requirements and corrections, potentially spanning submissions/turns |
| Submission | One unit of admission, scheduling, consumption or discard |
| Prompt | Execution content from user intent, Agent messages or task instructions |
| Turn | An outer execution from dispatched prompt to completion/failure/cancellation, containing inner provider/tool rounds |
| Input | Technical editor, keyboard, stream, tool-argument or provider terminology |

A submission carries `TurnPrompt`; preparation produces `PreparedTurnPrompt`. Immediate/Follow-up can start turns; Steer joins active work. Rewriting or injecting context does not redefine original intent. Agent/task text is prompt content, not human consent. User intent introduces no separate entity or scheduler.

An inner `AgentTurn` is one provider response plus an optional tool batch, not an outer-turn expiry boundary. Submission tool policy follows the [SDK outer-turn lifetime](design-harnes.md#turn-and-agent-loop-model); resource fields follow [Prompt and Agent tool settings](design-interactive.md#prompt-and-agent-tool-settings).

## Agent-driven orchestration

```text
Human goals and constraints
  -> Agent decides how to organize work
  -> tool/API expresses a scoped request
  -> AgentManager validates and coordinates
  -> independent AgentInstance executes and commits history
```

Agent API is Manager's public contract, not another service. Direct SDK calls, model tools and application requests share protocols with different authority. Ordinary submissions, interaction replies and recovery remain instance operations.

Definition specialization, instance alias, creation origin (master, subagent or teammate), creation lineage, optional Team, TaskList, directory and storage scope are independent dimensions. Master names the primary conversation Agent, not privileged authority; parent/child and Team lead describe separate relationships. The Agent tool's role selects a definition; collaboration membership and recorded Task ownership are not control authority.

Agent creation and invocation are independent of checklist Tasks. The model coordinates work through shared lists, prompt instructions and optional owner assignment/self-claim. Neither creation nor ordinary messages implicitly create Tasks; Task completion does not stop an instance. Instance stop follows fixed creation lineage. See [Task and Team design](agent/design-agent-task.md) for the authoritative contract.

Detailed ownership, authority, admission, messaging and capacity contracts belong to [parallel Agent design](agent/design-parallel-agent.md). Worktree preparation/cleanup is an independently authorized command capability, not a delegation transaction; see [worktree design](agent/design-worktree.md).

## Primary runtime flow

```text
main -> mode workflow -> runtime preparation/Session associations -> SDK instances
  -> provider/tool execution -> canonical Transcript reduction
  -> public snapshots/events -> print | protocol | interactive projection
```

Blocking interactions use scoped requests and semantic replies separately from streamed observations. Raw reply channels stay SDK-private; external callers never receive runtime collaborators.

## Application modes

Embedded interactive, print, `phi --rpc` and Gateway service modes compose concrete preparation/persistence capabilities without a universal application facade. `main` finalizes owned Sessions and startup MCP resources on success or failure. Print publishes the final response after cleanup. Embedded interactive uses serial Session transitions and explicit shutdown; Gateway TUI exit disconnects without stopping hosted execution.

RPC is a version-2 tagged JSONL execution stream with one initialized root, input/admission/outcome correlation and permission-only questions. EOF drains reachable admitted work; explicit close drains or cancels. Output/observation failures trigger cleanup, never successful closure. No MCP controls/events/elicitation, arbitrary live attachment, multiple live roots or old method aliases are exposed. SDK MCP execution and interactive MCP remain available under existing security policy.

Dropping an internal attachment does not stop its Agent. Agents execute in their owning process. Gateway clients use HTTP observations and commands, not SDK attachments to independent processes. See [application runtime design](agent/design-agent-runtime.md).

## State boundaries

`Phi` holds reusable SDK construction intent, policy, initial grant seeds and shared infrastructure, not live permission state. Each candidate has isolated mutable runtime resources; AgentInstance owns transcript-backed execution and one accepted generation. Application boundaries retain that generation's matching resources. Manager coordinates instances; application associations bind resources and durable Session metadata.

Modes call the owning capability directly. Agent-tool delegation stays inside SDK; application child preparation supplies resources and Session association, not scheduling.

UI rebuilds from coherent canonical recovery and applies scoped live events. Checkpoint policy belongs to runtime associations; adapters do not write cached history as live truth.

## Transactional replacement

Prepare and validate a complete candidate before changing active state. Same-ID replacement is accepted by the instance, followed by a non-fallible matching application metadata/resource swap. Different identity requires a new instance.

Preparation/admission failure preserves active state. Persistence failure **after** accepted replacement is a save error, not replacement failure; rolling back only one layer would create mismatched generations. See [replacement protocol](agent/design-agent-runtime.md#candidate-first-runtime-replacement).

## Scoped identity and ordering

Runtime output carries instance and applicable turn/tool scope. Delivery order is authoritative; timestamps do not establish global order across concurrent Agents. Each instance owns its history, and child observations are distinct from coherent Instance views.

Independent persisted Session files retain creation identity and typed links across scopes. Storage layout does not determine authority or execution ownership. History review uses the instance's canonical Transcript or saved Session, not per-Task copies.

## Path terminology

- `startup_dir` — process working directory captured once at entry; retained for default Session creation and process-local Memory administration.
- `working_dir` — execution directory, authoritative on the Session.
- `project_root` — current Git/worktree root; outside Git, project resource resolution uses the working directory.
- `session_scope` — history-storage bucket keyed by working directory.

Session construction requires an explicit working directory. Fresh startup uses `startup_dir`; saved startup uses the saved Session directory. Interactive `/new` inherits the current Session directory and accepted configuration. No workflow implicitly changes the process working directory.

Project `.phi` resources and instructions resolve from project scope even when launched in a subdirectory. Instructions prefer nonempty `AGENTS.md`, otherwise `CLAUDE.md`, never both. Execution and Session scoping remain working-directory based. The on-disk `projects/` name is a compatibility detail, not domain terminology.

## Capability boundaries

MCP and Lua are always compiled. Runtime configuration controls capability activation and resource loading; `phi-tui` remains independent.

## Design map

- [Harness](design-harnes.md): Manager/Instance/loop ownership, execution, feedback and module boundaries.
- [Agent/application runtime](agent/design-agent-runtime.md): construction, replacement, persistence and scoped access.
- [Parallel Agents](agent/design-parallel-agent.md): delegation, Teams, admission and communication.
- [SDK](design-sdk.md): public contracts, Agent loop, tools, security, providers and context.
- [Interactive application](design-interactive.md): CLI, prompts, commands, adapters and projection.
- [Remote and Gateway](design-remote.md): embedded/local service deployments, shared TUI/Web Sessions, transport and lifetime boundaries.
- [Worktrees](agent/design-worktree.md): directory preparation, execution security and independent cleanup.
- [TUI](tui/design-tui.md) and [performance](tui/design-tui-performance.md): reusable terminal semantics and bounded rendering.

Workspace placement and dependency declarations: [Cargo.toml](../Cargo.toml). Curated contracts: [SDK exports](../phi-sdk/src/lib.rs), [TUI exports](../phi-tui/src/lib.rs), and [application runtime](../phi-interactive/src/runtime/).
