# phi-interactive Design

`phi-interactive` is the `phi` CLI application around independent `phi-sdk` and `phi-tui` libraries, with interactive, print, and RPC machine-control entry points. It resolves CLI/configuration and resources, hosts application Sessions, dispatches modes, presents interactions, and projects terminal UI.

Agent behavior, task protocols and canonical history remain SDK-owned. Reusable terminal mechanics remain TUI-owned. [Global architecture](design.md), [Agent runtime design](agent/design-agent-runtime.md) and [SDK design](design-sdk.md) define those boundaries; this document focuses on application workflows.

## Startup and mode selection

```text
capture startup_dir -> CLI/security -> saved Session selection and lock
  -> target working directory -> settings/trust/catalog/resources
  -> new or restored Session -> startup configuration resolution
  -> compose runtime capabilities -> mode dispatch
  -> complete runtime preparation -> SDK admission/application binding
```

`main.rs` is the imperative shell. Configuration precedence is resolved before construction; a project switches array replaces the global list, while omission preserves it. Directory/project/Session scope follows [path terminology](design.md#path-terminology). Credentials use typed config/runtime paths and are never Session metadata.

Embedded new-Session startup prepares resources for `startup_dir`; saved startup uses `Session.working_dir` without changing process cwd. SettingsManager, Hook trust, sandbox, permissions and extension resource discovery use that target directory. Startup configuration resolution selects Switch/provider/model without constructing a temporary Session. Configuration resolution does not admit an Agent; mode workflows own that step.

CLI resume selection happens before runtime construction, using the same picker semantics as in-process resume. First standalone runtime emits Startup whether new or restored; in-process New/Resume use their corresponding reasons. Loading/reconciling history is separate from lifecycle reason selection.

Print, RPC and interactive workflows use concrete runtime preparation and Session associations over the same SDK instance path.

Custom providers declare a supported wire protocol, endpoint, credential reference and models together in the model catalog configuration. Provider/model-specific fields, bootstrap formats and source filters belong to [catalog/config implementation](../phi-sdk/src/models/) and [application settings](../phi-interactive/src/config/settings.rs), not duplicated design contracts.

## Settings ownership

```text
application preparation
  -> SettingsManager (one shared instance per prepared scope)
       +-- raw global settings
       +-- raw project settings
       `-- merged settings
              |
              +-- application configuration and runtime preparation
              `-- resource policy -> loaded Skills/Prompts/extensions
                                       -> SDK candidate
```

`config::SettingsManager` is the only settings read/refresh owner. `config::settings` provides its file storage, paths and protected persistence primitives. Startup creates the manager; configuration copies retain a handle to the same mutable manager, not another settings reader. Resource loading queries its memory. Global/project sources remain separate so relative paths and selection precedence keep their original scope.

Application parsing resolves [feature-block enablement](../README.md#settings-reference) before SettingsManager stores and merges typed settings. The SDK receives resolved values; it does not interpret block presence. SDK `CodemodeConfig::default()` remains disabled.

Settings writes use an empty adjacent `settings.json.lock/` directory, following pi's `proper-lockfile` mechanism: atomic `mkdir`, directory mtime heartbeats every five seconds, and stale-lock reclamation after ten seconds on the next acquisition. Heartbeats verify the previous mtime; unchanged ownership can recover after a scheduling delay. Temporary I/O failures retry every second until the stale threshold; a missing directory or changed mtime immediately compromises the lock. Writes check lock health before replacement, and release stops the heartbeat and removes an uncompromised lock. RPC protects global and project lock paths even with `trust_remote`. Session file locks are unchanged. Stop old processes before upgrading: the former global `settings-locks/` protocol does not coordinate with directory locks; its unused directory can then be removed.

SDK `RuntimeConfig.extensions` contains builtin exclusions and typed Memory/Codemode options. After CLI/runtime overrides, preparation applies `ExtensionConfig::resolve()` before resource preparation; exclusions take precedence over extension flags and overrides. Consumers query effective enablement through `is_enabled()`. MCP configuration defines connections; the bound manager determines runtime availability. In `settings.json`, `extensions` is a resource-selection array, while `memory` and `codemode` are top-level feature blocks. SettingsManager owns their scoped merge rules. See [SDK extension configuration](design-sdk.md#extensions).

`settings()` returns a copy of the stored merged configuration. `resource_policy_for()` derives policy from the same manager's stored global/project settings; it never reads settings files. Missing managers and scope mismatches are errors at resource consumers, never permission to load a replacement. Target-directory preparation explicitly supplies a matching manager. Same-scope children share the parent's manager; a different scope is prepared separately.

Refresh is an explicit application preparation operation through `SettingsManager::reload()`, not a getter or resource-loader side effect. Model/Switch selection, same-Session navigation and child preparation retain the accepted environment. Switch selection applies definitions from the Agent's accepted settings without refreshing settings or rediscovering resources; explicit reload applies definition/resource updates. Resume prepares the target scope. MCP Save only persists MCP settings; explicit reload applies them. Reload performs one refresh before loading resources and reapplies the current Switch, overriding temporary model selection. The manager validates both scopes and resource rules before replacing its in-memory state. A failed refresh preserves the previous settings.

SettingsManager validates and persists Hook workspace trust; startup obtains the local decision, and Factory passes captured declarations and activation policy into SDK candidate preparation. Modes do not execute hooks independently. Trust rules, mode behavior and process authority are defined in [Hook design](design-hook.md#workspace-trust-and-process-authority), with a [settings example](design-hook.md#registration).

Loaded settings are not accepted execution configuration. Refreshing the shared manager does not change an Agent's accepted runtime, loaded resources or Session metadata. Runtime replacement still requires complete candidate validation and SDK acceptance. Other Agents retain their loaded generation resources. No separate immutable configuration-snapshot layer is introduced.

Settings persistence remains separate from runtime acceptance. Protected read-modify-write operations run through the manager's storage boundary and preserve unrelated file edits; these deliberate transaction reads are not query-time refreshes. Resource content discovery/loading may read resource files, never settings files independently.

Implementation: [manager](../phi-interactive/src/config/manager.rs), [storage](../phi-interactive/src/config/settings.rs), [resource policy](../phi-interactive/src/resources/policy.rs). See [runtime configuration authority](agent/design-agent-runtime.md#runtime-configuration-authority).

## Application modes

| Mode | Process owner | Interface exit |
| --- | --- | --- |
| Interactive `phi` | Foreground application process | Explicit shutdown request before process exit |
| Print `phi -p` | Foreground application process | Stop/checkpoint after one prompt |
| `phi --rpc` | Foreground subprocess | EOF drain or explicit drain/cancel, followed by main-owned finalization |

Embedded-local modes own execution in the foreground process. Gateway mode hosts execution in the service process; local TUI and Web clients access its Sessions. See [Remote and Gateway design](design-remote.md).

Modes call [concrete owning capabilities](agent/design-agent-runtime.md#client-contract-and-deployment-modes), not a universal client/service facade. Attachments are internal scoped access only. SDK owns execution/membership/delegation; runtime modules own preparation, Session association and storage policy. Main owns process finalization, including mode errors and startup MCP shutdown.

Human submissions, observations, scoped interactions and explicit controls preserve direction/intervention during autonomy without introducing an application planner or universal approval workflow. See [human agency](agent/design-parallel-agent.md#human-agency-and-delegated-autonomy).

## Runtime preparation and Session associations

Factory prepares complete isolated candidates and retains their SDK generation resources for application effects and lifecycle. Factory services supply policy and initial grants, never live permission state. SessionAssociations binds accepted SDK instances with Session metadata and matching application resources; private bindings are not another layer of execution ownership.

New/different-ID workflows create instances; same-ID changes replace complete generations. Application/UI state changes only after acceptance. Failed preparation/admission preserves active state, while failed post-commit save is a persistence warning, not rollback. Runtime associations own checkpoints independently of observers; main owns process finalization.

The authoritative chain, generation isolation, recovery and save/lock protocol are in [Agent runtime design](agent/design-agent-runtime.md). Implementation: [Factory](../phi-interactive/src/runtime/factory.rs), [preparation](../phi-interactive/src/runtime/preparation.rs), [associations](../phi-interactive/src/runtime/associations.rs) and [AgentSession](../phi-interactive/src/runtime/session.rs).

## Mode adapters

### Print

Print starts through SessionWorkflows, submits one prompt and waits for SDK quiescence plus the matching ordered event boundary, including delegated continuation. It returns the final root response; main finalizes/persists before printing it. Errors also pass through cleanup. The adapter never runs a private Agent loop.

### RPC execution stream

`src/rpc/` is a sibling mode adapter to `interactive/`. Strict version-2 tagged JSONL separates input/admission, execution events/outcomes, control results and permission questions. It is not JSON-RPC or an internal application API. Initialize once with new intent or saved ID; arbitrary roots, live attachments, caller-authored Sessions and old method aliases are rejected.

Controls support runtime info/selection, reload, prompt expansion, interruption, scoped task stop and close. There are no MCP controls, dedicated MCP events or elicitation messages. SDK MCP execution remains subject to process security policy; unsupported blocking interactions explicitly fail and finalize rather than hang or approve.

Bounded asynchronous workers keep answers/interruption responsive. EOF drains admitted work unless a required answer becomes unreachable. Explicit close drains or cancels; drain rejects new input, accepts pending permission answers/cancellation and can escalate. SDK quiescence and ordered event boundaries determine execution completion, not input closure. Output/observation failure triggers cleanup and nonzero exit. Main-owned finalization remains supervised before final closed/flush.

Stdio inherits launcher authority; initialization is not authentication. See [access/lifecycle policy](agent/design-agent-runtime.md#transport-adapters-and-access-boundary), [messages](../phi-interactive/src/rpc/messages.rs) and [I/O](../phi-interactive/src/rpc/io.rs).

### Gateway

Gateway hosts the application runtime. Embedded-local and Gateway TUI access use one interactive loop; Gateway TUI and Web address service-owned Sessions. Client disconnection does not stop execution; main in the service process owns finalization. `phi-remote` owns transport, Web assets, device authorization and protected Gateway storage. The application selects and explicitly passes the deployment directory, composes the host and coordinates shutdown; CLI retains terminal-only token disclosure and browser-opening policy. Session, Settings and provider-credential persistence remain application-owned. See [Remote and Gateway design](design-remote.md).

Gateway is a service mode, not a builtin/plugin capability. CLI owns service management and browser opening; there is no builtin `/web` command. `phi gateway` runs the foreground service. `phi --web` opens its Web page; `phi --gateway` attaches the ordinary TUI. Both client entries require a running service. Service startup and client Session selection follow the [Gateway entry semantics](design-remote.md#cli-and-web-entry-semantics). Embedded-local instances execute independently of the Gateway. Commands and setup: [phi-remote](../phi-remote/README.md).

### Interactive

The interactive adapter obtains scoped attachment recovery and events. It subscribes before correlated recovery, then discards only through the matching actor boundary so already recovered deltas do not duplicate history. Lag/missing markers leave recovery pending or report failure rather than guessing. Queue chrome reconciles by pending submission identity, not text.

Pending interactions use the canonical lightweight read, shared by routing and display reconciliation. Failed reads preserve existing prompts and report errors. Reply receipts contain actual SDK answer status; queueing a transport response is not approval. See [recovery contract](agent/design-agent-runtime.md#snapshot-and-event-recovery) and [attachment operations](../phi-interactive/src/runtime/attachment.rs).

#### Capability composition

UI modules define one shared capability model and the dependencies each capability requires. Assembly selects capabilities and supplies their dependencies for each component instance. Local and Gateway name current assembly configurations, not capability categories. A capability omitted from one assembly is not inherently exclusive to the other; neither mode defines the module's capability boundaries.

This design applies to the whole interactive adapter, not only pickers. `assembly.rs` selects module-defined capabilities to build the components needed by each mode. It does not define another capability taxonomy, application service or execution owner.

```text
UI modules
    |
    +-- define capability boundaries
    +-- implement capability behavior
    `-- declare required data / operations
                    |
                    v
          component assembly (assembly.rs)
          choose capabilities + supply dependencies
                    |
           +--------+--------+
           |                 |
    Local components   Gateway components
    selected abilities selected abilities
           |                 |
           +--------+--------+
                    |
                    v
          shared interactive runtime
          input / dispatch / projection / render

Session transition and lifetime policies are supplied separately.
```

##### Component contracts

Each UI module decides how to divide its responsibilities into selectable capabilities. The following families identify responsibilities to consider, not mandatory capability bundles or a requirement for a struct, trait or directory for every item:

```text
UI modules define selectable capabilities
    |
    +-- input / commands      editor, completion, command interaction
    +-- runtime selection     Model, Switch, thinking; optional administration
    +-- Session / navigation  discovery, selection, new, deletion, branch navigation
    +-- conversation          history, live projection, expansion
    +-- Activities            observations, details, explicit control intents
    +-- interactions          permission and MCP elicitation presentation
    +-- administration        Memory, MCP configuration, authentication
    `-- extensions / shell    bindings, commands, operation results
```

A module owns capability definitions, behavior and component interaction. Assembly chooses the capabilities enabled on a component instance and supplies compatible data sources and operation collaborators. The instance consumes only the dependencies its selected capabilities require. Components emit semantic actions and present results, while existing application/SDK owners retain authorization, execution, acceptance and persistence.

Capabilities must be selectable at the module's defined boundaries, rather than forced into `Local`/`Gateway` or `Management`/`Selection` bundles. Selection is free within explicit dependency constraints; capabilities that require another capability must declare that requirement. A data source does not implicitly enable administrative operations. Disabled capabilities must not expose their interaction paths or require placeholder settings, fabricated collaborators or fallback configuration loading.

No particular construction mechanism is required: concrete collaborators and constructors/builders are sufficient. Separate structs, traits or runtime flags are not mandatory. Assembly must not become a universal facade, and the controller must not receive one catch-all administration dependency bag.

##### Example: ModelPicker

The Model module separates candidate display/search/selection, catalog updates and default-model initialization. Assembly can enable the capabilities needed by a component without enabling the others. For example, local catalog candidates can be used with selection alone, without updates or configuration writes:

```text
Model module defines capabilities; assembly selects them
    |
    +-- candidate source: catalog + safe draft, or attachment runtime catalog
    |                         |
    |                         v
    |                 +------------------------------+
    |                 | ModelPicker                  |
    |                 | display / search / selection |
    |                 +------------------------------+
    |
    +-- optional CatalogUpdates
    |     update / discovery tasks, results and notices
    |
    `-- optional DefaultModelConfiguration
          generate and persist a missing default model

current assembly selections:
Local:   selection + catalog updates + default initialization
Gateway: selection with runtime catalog; no updates or default initialization

another valid selection:
selection with local catalog; no updates or default initialization
```

`DefaultModelConfiguration` is the existing default-model initialization workflow, not a general model-configuration editor. Candidate selection alone never implies configuration writes. Local Model and Switch components share only the safe picker draft so generated defaults remain visible to Switch editing; that draft is neither application Settings nor accepted runtime configuration.

##### Mode policy

Local and Gateway consume the same module-defined capabilities. The table records their current assembly selections, not two sets of capability definitions or the only valid combinations. Changing a selection changes component assembly, not module capability boundaries. Available collaborators and application/transport authorization constrain valid assembly choices; capability selection does not bypass them. Execution ownership and Session lifetime remain separate policies.

| Assembly | Local (embedded) | Gateway client |
| --- | --- | --- |
| Application access | In-process attachment | Gateway attachment to service-owned Sessions |
| Module capability selection | Enable shared interaction, administration, Activities, extension handlers, shell, Agent mentions and Workflow observation | Enable shared interaction and runtime/Session selection; omit the administrative, activity-control, extension-handler, shell, Agent-mention and Workflow-observation capabilities |
| Picker capability selection | Inject catalog updates/default initialization, Switch editing and Session deletion separately | Omit those capabilities; inject runtime catalog and saved-Session discovery without placeholder configuration |
| Session choices | Saved Session discovery | Gateway-side saved Session discovery; excludes unsaved hosted Sessions |
| New/resume | SessionWorkflows replaces/finalizes the outgoing root | Selects another attachment; outgoing Session keeps running |
| Exit | Cleans up owned execution | Disconnects; service retains execution ownership |

##### Interaction and acceptance

Editor, command parsing, component interaction, permission prompts, accepted-result installation, history projection and rendering use the shared runtime. Each component reports the capabilities enabled during its assembly. `InteractiveFeatures` projects those selections for command discovery, dispatch, shortcuts and recovered declarations; it neither defines the module's capabilities nor enables behavior by itself. Component-specific action support belongs to the owning module. Recovery and Session selection retain the assembly's UI capability limits rather than enabling unsupported operations from recovered declarations. UI availability never substitutes for application authorization or runtime acceptance.

```text
assembled component
    |
    +-- capability facts -> InteractiveFeatures -> discovery / input filtering
    |
    +-- display / search / editing -> component-owned interaction state
    |
    +-- administrative operation -> injected collaborator -> result / notice
    |                              (not runtime selection acceptance)
    |
    `-- runtime selection intent -> controller -> application acceptance
                                                     |
                                                     v
                                               accepted result
                                                     |
                                                     v
                                         controller installs projection
```

Switch editing preserves its existing save-before-selection behavior and returns selection intents rather than receiving the whole controller. Controller performs runtime selection and, when requested, reopens configuration after the selection attempt. `SessionSelection` owns Session candidate sources and transition policy independently of deletion support. Scoped attachment operations carry execution/runtime intents; they do not decide UI availability.

Gateway `/resume` intentionally lists only saved histories, not all hosted Sessions. Unsaved hosted Sessions remain absent after switching away; this is not a discovery failure. Newly hosted Sessions without an accepted first user submission expire after five minutes; see [Gateway lifecycle](design-remote.md#lifecycle-and-process-supervision).

Implementation: [UI assembly](../phi-interactive/src/interactive/assembly.rs), [shared loop](../phi-interactive/src/interactive/mod.rs), [ModelPicker](../phi-interactive/src/interactive/components/picker/model.rs), [SwitchPicker](../phi-interactive/src/interactive/components/picker/switch.rs), [SessionPicker](../phi-interactive/src/interactive/components/picker/session.rs), [availability rules](../phi-interactive/src/interactive/assembly.rs), [controller](../phi-interactive/src/interactive/controller.rs) and [Session selection policy](../phi-interactive/src/runtime/selection.rs).

#### Commands, storage and resources

Both current assemblies enable Session/runtime selection, reload and Prompt/Skill expansion through accepted runtime resources. The Gateway assembly omits MCP configuration, Memory administration, extension handlers, authentication and shell execution; these remain module-defined capabilities, not a separate category of Local capabilities. MCP elicitation decline/cancel remains available independently of MCP administration; Gateway transport does not permit elicitation approval. When MCP reads/reconnects/selections are enabled, they use Session-scoped runtime resources and candidate-first replacement. RPC controls remain responsive during asynchronous operations.

Controller installs the [coherent accepted result](agent/design-agent-runtime.md#candidate-first-runtime-replacement) synchronously through a one-shot declaration handoff, not a second resource store. Recovery uses the same projection path; failure preserves the existing view and remains pending. Declaration installation preserves editor/overlay state; persistence warnings do not undo acceptance.

Persistent or saved Session associations hold shared storage leases. Independent processes can resume the same disk history; short journal locks serialize reads and branch-preserving appends. Deletion requires exclusive ownership of the Session journal and its ordinary subagent sidechain tree before removing any journal. Teammate journals are independent. Stale-lock cleanup removes only locks proven unheld. Acquisition validates descriptor/path identity, obsolete openers retry, and the final lease release obtains exclusive ownership before removing its lock path. Process death releases OS ownership; residual files are recoverable without manual deletion. Exact file-lock rules and DTO fields belong to [file ownership](../phi-interactive/src/file_lock.rs), [storage](../phi-interactive/src/session/storage.rs) and [RPC messages](../phi-interactive/src/rpc/messages.rs).

### Resource configuration

`phi config` discovers/selects Skills, prompts and optional extensions without starting an Agent or requiring a catalog download. Bundled Skills remain discoverable before their user-directory links are installed; selection rules match only `builtin:skills/<name>/SKILL.md` identities (or their parent directories), never filesystem paths, keeping them distinct from user Skills. Built-in resources share one section with Extensions and Skills subheadings. Skill checkmarks represent persisted selection, independent of builtin availability. Unavailable Skills retain their selection, show the reason and render entirely dimmed; runtime loading still requires SDK-declared availability. Global/project selection and search are transient UI; startup/reload use the same resolved resource policy.

Project definitions take precedence over same-name global definitions with diagnostics. Home has no separate project `.phi` resource scope. Config syntax and exclusions/inclusions belong to [resource loaders](../phi-interactive/src/resources/policy.rs) and [config selector](../phi-interactive/src/cli/config.rs), not a new package/theme system.

### Saved Session listing

`phi list` reads recent saved Sessions locally without starting an Agent or discovering another process. Listing owns no live Agent observation or lifecycle policy. See [list](../phi-interactive/src/cli/list.rs).

### Error presentation

Execution failures display from canonical Transcript Failure entries. User-operation, transport observation and save errors remain local notices; tool errors remain tool results. None writes directly to the terminal. Recoverable control-flow failures remain typed and undisplayed unless they affect the user operation.

Print and pre-adapter fatal errors return to the terminal boundary with original context. RPC operations return structured errors; fatal I/O/observation, unreachable/unsupported interaction, execution or finalization failure produces a failed process result. Presentation depends on operation context, not just Rust error type or diagnostic-text matching.

## App Session and persistence

Session wraps SDK history with application provider/model/switch/mode, directory, permission and linkage metadata. It is a durable envelope, not a live execution owner. Branch-local runtime configuration comes from canonical Transcript entries; context window refresh uses the validated selected descriptor unless explicitly overridden.

TUI retains Session metadata, not a second full history alongside ConversationView. Controller construction performs no Transcript read. Rewind picker and Rewind extension tool-exchange events read history only when consumed and release it after labels/event capture. Accepted navigation captures editor text before replay/release; configure/reload discard unneeded accepted payload. MCP Save produces no accepted runtime payload. Startup/recovery and child detail release source history after projection. Temporary child-identity observations remain consumer-scoped. Metadata synchronization remains separate from save observation.

Storage owns independent files, bounded discovery, JSONL append/checkpoint and lookup. Journals persist canonical Transcript entries, including Compaction/Reset markers and their heads, not duplicate text messages or a second app context-boundary list. Context-token metadata is derived from the SDK effective projection, never an execution authority; TUI retains that display value after releasing history. Legacy text-only message and compaction records are unsupported. Canonical Compaction entries require a valid head; journal loading does not infer or migrate context boundaries. Metadata-only delegated Sessions can require files even without conversation; ordinary empty-root omission is separate policy. Later invocations retain the instance's creation file. See [storage](../phi-interactive/src/session/storage.rs), [discovery](../phi-interactive/src/session/discovery.rs) and [journal](../phi-interactive/src/session/journal.rs).

Journals preserve Failure entries and their structured payloads, including failures without assistant output or turn identity. These entries qualify for saving under the empty-root policy. See [SDK failure semantics](design-sdk.md#durable-execution-failures).

Runtime checkpoint/finalization policy and retry obligations are defined in [application persistence](agent/design-agent-runtime.md#application-session-and-persistence). UI never writes parent/child files. Saved projections do not restore execution, authority or mailbox replay.

Application delegation, discovery and messaging validate Session associations and use SDK admission, not an application scheduler. Human additions normally enter as Session prompt submissions; the Agent decides whether to execute or delegate. Admission, completion, message delivery and persistence remain distinct. See [parallel Agent design](agent/design-parallel-agent.md) and [application associations](../phi-interactive/src/runtime/associations.rs).

## Interactive event loop

The loop multiplexes attachment events/interactions, child observations, MCP/extension/model tasks, terminal input and render scheduling. It updates projection and asks TuiRuntime to paint; it does not advance Agent execution or deliver outcomes.

On exit, stop terminal input-reporting before awaiting shutdown/detach, retaining raw mode until cleanup so repeated input cannot interrupt persistence. Terminal polling uses the earliest selected-view clock, child-observation or render deadline, capped at 50ms, then yields to async work. This bounds passive observation independently of key-repeat cadence, but does not make awaited application/network operations nonblocking.

History restoration first reconstructs child summaries from parent links/admission history without opening every child file. Historical trace expansion reads canonical child data only when needed; folds reuse projected headers. Detailed child opening refreshes canonical recovery through the scoped attachment.

Implementation: [UI loop](../phi-interactive/src/interactive/mod.rs), [main projection](../phi-interactive/src/interactive/agent/agent.rs), and [child projection](../phi-interactive/src/interactive/agent/subagent.rs).

## UiState projection

UiState owns TuiRuntime, editor/overlay state, queue originals and installed runtime declarations; active_menu tracks the picker category for Switch-specific exit handling. Its TUI-private ConversationView owns display blocks and indexed mutation, history projection, one optional live route per view, usage/status, local notices and retained/paint state. Display event mapping and live snapshot reconstruction belong to ConversationView; the main adapter retains submission/queue accounting, recovery retries and read-only save observation. Assistant text observation is private view state, not optional text-block presence: image segmentation clears the text route without clearing that observation, and image-only output does not set it. Run-status observation remains independent of both. UiState has no implicit dereference bridge to conversation state. Neither owns canonical history, tool authority or execution state. Application UiBlock retains identity/group/tool correlation and uses the [TrackedBlock display contract](tui/design-tui.md#display-transcript). ConversationView supplies borrowed display views; application projection revision, retained boundaries and pinning remain application-owned.

Main frames compose terminal/editor/menu chrome with the main conversation. Child frames accept only ConversationView + Session; child detail constructs no TuiRuntime/input mount/editor. History replay is display-only, while the main interaction adapter seeds editor history separately. Selected-child refresh validates branch fingerprints, then applies new entries and live changes without replacing block identity, expansion, clocks or the retained/painted history window. Provisional content reconciles in canonical order by content position and tool-call ID; repeated observations do not duplicate content or usage. Non-append history changes, compaction or Session/working-directory changes rebuild the projection; invalid observations preserve it for retry. Refresh still reads full snapshots and scans branch fingerprints, releasing source history after projection.

Restore/rewind/compaction rebuild selected-branch display; public events incrementally update it. One message can contain ordered text/image blocks under a visual group. Inline image data becomes semantic terminal images; URL/undecodable sources use fallbacks without UI network fetches.

Normal cancellation is incremental: SDK completion events finalize existing content before `RunCancelled` ends live routes, tool timers and run status and adds the cancellation notice. It preserves block identity, notification position, expansion intent and the retained history window; cancellation itself never requests history replacement. Queued input restoration happens after preceding commit events and retains the current editor draft. Failed abort requests leave live projection and queued input intact.

Coherent recovery after event loss/failure preserves editor/overlay state, expansion intent and retained local notification blocks while replacing execution/history projection. Local notices are not persisted in the Transcript; Session/branch replacement still starts a fresh notification projection. Covered deltas are not replayed; later events remain queued. Existing queued chrome retains original editor/paste text for still-pending IDs, while committed/discarded IDs disappear. New attachments do not invent unknown queue payloads.

See [UI state](../phi-interactive/src/interactive/state.rs), [block projection](../phi-interactive/src/interactive/blocks.rs), and [attachment recovery](../phi-interactive/src/runtime/attachment.rs).

## Input and prompt flow

```text
Editor/command routing -> original TurnPrompt submission
  -> SDK preparation: generation-scoped prompt expansion hook
  -> policy validation/admission -> canonical commit -> projected history
```

Enter requests Steer while active; Alt+Enter requests ordinary Follow-up. Idle requests start immediately through SDK dispatch. Queue ownership, safe points and drain policy remain SDK-owned. Commit/discard events correlate queue projection by submission ID, even for identical text; cancellation restores original expanded editor content rather than transformed execution text.

Modes submit original input; a `UserPromptSubmit` hook expands Prompt/Skill commands through [generation-scoped committed resources](agent/design-agent-runtime.md#prompt-resources). Expansion contributes content and `TurnToolPolicy` together; SDK merges and validates policy before admission/commit. Activation follows the [SDK outer-turn scope](design-harnes.md#turn-and-agent-loop-model), not expansion or a grant-save operation. Permission answers refresh confirmed durable grants from the attachment and show separate [save warnings](agent/design-agent-runtime.md#application-session-and-persistence), without a second grant call.

Startup Agent and Prompt/Skill diagnostics come from coherent recovered capabilities, not client ContextFiles. The interactive adapter prints them to stderr before entering the terminal and waits for Enter when stdin/stderr are terminals; recovery/subscription still has one owner. Warnings remain initial-only. Diagnostics display a category heading, indented path and indented reason: `[Config error]` for fatal config failures; `[Agent conflicts]`, `[Prompt conflicts]` and `[Skill conflicts]` for skipped resources. RPC runtime observations expose `agent_diagnostics` and `prompt_diagnostics`; interactive recovery carries the matching typed declarations. Prompt/Skill diagnostics carry a `resource` category; absent categories default to Prompt. Footer `auto` labels use committed `compactEnabled` facts installed on acceptance/recovery; child views use their own runtime fact. Missing facts in saved-only child history omit the label without client-config fallback. MCP notify/show-status preferences remain startup-captured client presentation choices.

Footer paths come from the displayed Session, never process cwd. Runtime observes Git facts off the render path; Gateway carries host-side observations. `interactive/workspace.rs` follows the actual render target, including the main view shown during permission interactions, and discards pending observations on attachment or displayed-Session changes. Child observations resolve through the owning attachment's validated Session links and support saved-only history without starting an Agent. Main and child footers share directory matching and omit branches from foreign directories. Git facts are transient observations, not Session metadata. File completion binds the selected Session's absolute directory; missing/relative directories never fall back to process cwd. Native Gateway TUI completion remains same-host filesystem access.

Skill discovery exposes name, description and path; full SKILL.md content loads on demand. Loading a Skill adds context, not tool policy; scripts use ordinary governed tools. Skills, templates and file mentions cannot bypass governance. Intent terminology follows [domain definitions](design.md#intent-submission-prompt-and-turn).

A leading `@name` is directed Agent intent only when `name` exactly matches the current Agent catalog; otherwise it remains a normal file-mention prompt, with or without trailing text. Agent names take precedence over identical file paths, consistently with file-reference extraction. `@agent` passes exact original user text plus typed named-Agent intent to the master, never directly spawns from UI. The Agent creates/assigns through scoped Manager protocols; directory preparation is separate. Directed creation cannot be satisfied by retained assignment. See [directed delegation](agent/design-worktree.md#directed-delegation).

All submission modes use [application submission coordination](agent/design-agent-runtime.md#memory-preparation) over SDK-owned Memory hooks and context accounting. UI projects typed submission/readiness notices from scoped attachment observations, never constructs or retrieves execution-side Memory context, and paints restored history without waiting for startup. Successful manual injection uses the same Dim notification as automatic injection, prefixed with `/memory-inject ·`; an omitted prompt creates no User display block. Notifications remain UI-only, outside the canonical Transcript. Retrieval and manual injection follow [SDK Memory](design-sdk.md#memory-and-tape).

### Prompt and Agent tool settings

Prompt templates and Agent definition files configure tool use through YAML frontmatter; their bodies remain prompt content, not tool implementations.

| Resource | Field | Effect |
| --- | --- | --- |
| Prompt command | `allowed-tools` | Outer-turn invocation pre-approval; does not select or add tools |
| Prompt command | `disallowed-tools` | Remove whole tools for that outer turn |
| Agent definition | `tools` | Select Phi-governed tools; no invocation approval |
| Agent definition | `disabled-tools` | Remove whole tools from that selection |
| SKILL.md | None | Context only; every tool-related field is unsupported |

Values use YAML string lists (recommended) or comma-separated strings. Bare names and `Tool(pattern)` use SDK `ToolRules`; only `allowed-tools` retains invocation patterns. All selection/removal fields normalize specifiers to whole tools: `Bash(git push *)` removes/selects Bash, not just matching commands. Empty lists add no entries; Agent `tools: []` selects no Phi-governed tools, while omitted `tools` adds no selection restriction. Duplicates normalize to sets. Agent file removal wins selection overlap; Rust declaration conflicts remain errors. Empty strings/items, non-string entries, maps, malformed specifiers and unsupported tool-field aliases are rejected with file, field and original Markdown line/column diagnostics. Skill headers reject unsupported fields. Resource metadata schemas remain distinct; this is Phi's contract, not a universal Markdown standard.

Agent `tools`/`disabled-tools` may select/remove ToolSearch like any Phi-governed tool. ToolSearch loads deferred tools for direct calls without enabling Codemode. Agent and Prompt fields reject the retired `MCP`/`mcp` names, individual MCP tool names and server/tool patterns; configure MCP exposure through `exposure`/`toolExposure`. Validation uses [candidate MCP contracts](design-sdk.md#tool-registry). Removing ToolSearch does not disable MCP, withdraw loaded declarations or change invocation permission.

Selection/removal and pre-approval remain separate under [SDK tool availability](design-sdk.md#tool-registry) and [authorization](design-sdk.md#permission-and-sandbox-model). See [user examples](../phi-interactive/README.md#prompt-and-agent-tool-settings).

### Tape keyword prompts

Only original user-authored input—including directed master input—can produce trusted keyword-anchor evidence. TUI never creates it, matches keywords independently or authorizes mutation. Generated task/definition text cannot relay that permission to children. Consumption/cleanup remains inside SDK gate/finalization.

## Retained history and Ctrl+O

Main and opened-child views each use a byte-budgeted history window measured at fully expanded footprint, not terminal height. Parent child-agent traces are compact projection, not another child history window. Ctrl+O changes presentation in the current retained range without changing its membership; tool updates/new blocks preserve explicit expansion intent.

Large prompt folding is display-only and preserves canonical content. Buffer/expansion changes request renderer-owned replacement. The terminal retains one selected render target through preparation, paint and acknowledgment; a view-source switch requests replacement independently of numeric history revisions. Prepared frames carry private conversation identity and reject another view's paint/ack path. Only successful paint updates the terminal's displayed source and that view's revision/boundary or retires unpinned groups; failures leave them retryable. Resume does not call terminal-exit cleanup or implement another history replay loop.

Accounting, cache and replacement invariants belong to [TUI performance](tui/design-tui-performance.md); exact folding/layout policy belongs to [block renderer](../phi-tui/src/widgets/transcript.rs) and [UI projection](../phi-interactive/src/interactive/blocks.rs).

## Event projection and persistence

Public scoped events route started/finished tools by call ID, not latest block. Replay reconstructs the same identity. SDK supplies semantic diff/display metadata; application maps it to TUI primitives without recomputing diffs or receiving executable formatter registries. Tool projection keeps `ToolDisplay.title`, `input`, and `title_suffix` separate. The application formatter supplies Bash timeout metadata through `title_suffix`; Read result ranges update `input`, not the title. Codemode input displays JavaScript source inside braces, with ` … N more lines` appended inside the braces when truncated. Input and output share expansion, including before output arrives. Collapsed output retains its original line-count summary; expanded output preserves text-region headings and previews the first 100 characters per region with a Dim ` ...` suffix. Script failure diagnostics use `<script_error>` tags in the tool result. Full-output links and failure diagnostics remain outside region previews and appear only when expanded. Nested calls project into indented tracked blocks from public events and canonical history/result summaries, displaying tool headers, inputs, status and duration, never output, error bodies or diffs. Nested headers remain visible when the script is collapsed; missing terminal observations remain unknown, never inferred successful.

`TurnFailed` and SDK `PumpFailed` request coherent recovery. ConversationView projects Failure entries through the same typed mapping for live recovery, replay and reopen; neither events nor `snapshot.latest_error` append duplicate error notices. Local transport/save notices remain ephemeral.

Footer usage accumulates per-assistant usage, not cumulative turn usage again. Status, tool timing and notifications are transient. Only the selected view advances display clocks; tool timing retains observed start/finish Instants without per-tool tasks or tick queues, and freezes on completion/cancellation. Clock updates request Animation rendering, child observations request Background rendering, and input/resize retain higher priorities. Failure presentation branches on typed kind, not text. Exact labels/timer periods belong to UI code.

Streaming does not deep-copy canonical history per delta. The interactive adapter polls association save observations independently of execution events. The attachment save cursor deduplicates association/failure warnings and reports recovery only after matching successful completion with no pending save. Missing/unreadable status never implies success. Finish/cancel event consumption never requests another checkpoint. Metadata changes, transitions, finalization and deferred branch saves retain [application persistence policy](agent/design-agent-runtime.md#application-session-and-persistence).

## Subagent presentation

Instance owns execution/history and child admission/completion observations. Application SubagentProjection owns presentation/navigation only.

Child views validate the parent/child association and wrap coherent live recovery or saved history. Live views include the instance's current history, including later invocations. See [application child views](../phi-interactive/src/runtime/associations.rs).

### Main view: child projection only

The ordinary Agent tool block is followed by a separate folded `[agent] <name>` trace beneath its matching invocation. Expansion exposes instance/directory facts, execution status and ordered **tool headers**, not child payloads.

Headers retain full invocation summaries, embedded newlines, normal width wrapping and normal running/success/error markers, without a subagent-specific length cutoff. They have no individual output-body expansion. Thinking uses fixed empty status rendering, never reasoning text. Whole-trace folding persists through completion/status updates.

Child tool results/diffs/reasoning/messages and raw delivered outcome prompts stay out of the parent display. The master's own response remains normal conversation output. Resume restores invocation identity so traces do not migrate to the history tail. Exact styling/indentation is in [subagent projection](../phi-interactive/src/interactive/agent/subagent.rs).

### Child view: detailed TUI

Opening a live child exposes that instance's detailed history, live tools/content and scoped interactions; Ctrl+O uses its own history range. Historical reads never create a runtime or move files. Only the selected detail view retains full child projection; historical header extraction discards its source payload.

Authorized descendant observation keeps permission requests actionable even when details are closed. Viewing, closing or returning never starts, cancels, checkpoints or consumes outcomes.

### Keys and notifications

| Context/key | Action |
| --- | --- |
| Ctrl+T / `/tasks` | Toggle / open the Tasks overlay |
| Tasks Tab / Left / Right | Switch Checklist / Activities |
| Activities Up/Down, Enter | Select activity; enter child view or open Bash output |
| Activities Ctrl+D | Cancel selected running child invocation or stop selected running Bash |
| Main / child Ctrl+B | Open an inline selector for that Agent's running foreground Bash without changing execution mode |
| Bash selector Up/Down, Enter | Select and background one command |
| Bash selector Escape / Ctrl+C | Dismiss without backgrounding; preserve the editor draft and child view |
| Bash output Ctrl+B | Background only that running foreground Bash |
| Overlay Escape / Ctrl+T / Ctrl+C | Dismiss, not cancel |
| Child Escape | Return to main, not stop |
| Main cancellation | Abort main turn; accepted children continue |
| Child invocation cancellation | Cancel initial execution, retaining the accepted instance with automatic activation suspended |
| Ctrl+O | Toggle current-view details/whole traces, never projected tool payloads |

UI notifications identify the child and completion state without stealing focus; they do not determine [model wakeup or execution feedback](design-harnes.md#execution-feedback). A retained user-cancelled child rejects ordinary Agent messages; automatic results/reports remain passive until a direct non-Internal user/host submission successfully starts execution. Viewing the child, switching attachments or reloading its runtime does not resume it. Permission requests stay child-scoped and cannot be silently approved or hidden by projection. Interaction list/replies use canonical scoped operations; switching/detaching does not close them.

The Tasks overlay separates Checklist (recorded work and progress) from Activities (`agents & commands`). Activities projects ordinary subagents admitted during the current AgentSession, including finished ones, and Bash commands; restored child history does not enter the activity list. Agent rows show known delegation mode before status (`name · foreground/background · status`), with mode dimmed and retained after completion; unknown modes remain omitted. Application attachment observations collect running foreground Bash and background-managed Bash from the attached instance and explicitly foreground-delegated children. Foreground commands appear in Commands only while running and leave on completion, failure or stop unless handed off to background management. Background management begins through explicit background startup, Ctrl+B handoff or automatic foreground-timeout handoff, not elapsed duration alone; managed commands retain their terminal status and output after completion, failure or stop within the SDK's retained execution history. Background children remain instance-level activities; none of their internal Bash is flattened into the parent's command collection. It provides viewing/control intents; SDK retains execution authority. `activities.rs` owns both tabs and is the sole writer of their activity projection; child-view changes trigger refresh without waiting for the periodic poll. Rendering does not synchronize panel state. `overlays.rs` owns active-panel dispatch, mounting and dismissal; business modules retain their panel state, input and layout. SDK `BashExecution` retains its execution meaning.

Team projection shows instances; TaskList projection shows recorded work. Neither UI nor notifications infer Task completion from execution events. Task completion/deletion is separate from Session/instance lifetime; [Task and Team design](agent/design-agent-task.md) owns those rules.

Canonical authorship stays SDK-owned: directed input is User, delegation records Assistant ToolCall/one admission ToolResult, child feedback is Agent content in the creator, and projection adds no history entry. Provider wire conversion cannot change canonical authorship.

## Commands and application control

Controller intercepts semantic commands before expansion/dispatch. Model/switch selection, new/resume/rewind, reload, login, MCP and Memory views enforce readiness at their owning boundary. Skills/templates remain prompt expansion, not privileged commands.

Model/Switch editors, login, MCP auth instructions and Memory administration are interactive application workflows, not automatic RPC capabilities. MCP operations and Memory injection use scoped application operations, without RPC endpoints. Extension declarations are not suppressed merely for sharing a builtin name. Security and diagnostic-only `/mcp-auth` contracts follow [application policy](agent/design-agent-runtime.md#configuration-write-access) and [lifecycle/adapter policy](agent/design-agent-runtime.md#lifecycle-and-adapter-policy); UI preserves diagnostic usage/error and disabled-build presentation. Local login completion is observed without blocking the interaction loop or owning durable lease state.

Rewind includes Failure entries as selectable points under ordinary Entry navigation; selection does not retry the failed input.

Rewind projects canonical history plus optional [process-scoped Tape annotations](agent/design-agent-runtime.md#memory-administration). UI owns target-label mapping, deduplication and tree rendering; canonical Tape tool-result names remain visible even when annotations are unavailable. `interactive/memory.rs` owns the Memory Review panel and retains its opaque process-owned context until the overlay closes. UI owns query/selection/confirmation and installs observations only after successful store effects; failed writes preserve projection without new notices. Opening a Rewind picker is observation, not navigation. Ordinary navigation retains live execution selection/resources and moves only the in-memory leaf; it writes no entry or cursor. Checkpoint/exit cannot persist that position: disk reopen selects the last appended entry. Continuation retains current model/thinking/Switch and loaded resources. Input/response entries establish the new branch without deleting the old one or fabricating configuration changes; responses record actual provider/model plus Switch identity. On reopen, Switch definitions fill their defined fields before historical fallback, without replacing actual model attribution or reconstructing an old environment. `session_tree` remains an extension notification; summary/label recording requires explicit operations. See [navigation and recovery](agent/design-agent-runtime.md#navigation-and-recovery).

Model/Switch catalog, seed, persistence and selection follow [application contracts](agent/design-agent-runtime.md#wire-surface-and-semantic-scope). UI retains the safe draft, labels/filtering/cycling and automatic/manual presentation. Current/default model inputs are UI-lifetime facts, not saved configuration or runtime acceptance. Failed reads preserve an existing picker/draft without creating overrides or falling back to attaching-client configuration. Policy-denied saves show a warning; I/O failures show an error, but neither gates subsequent selection. Ctrl+Up/Down cycles names from the safe startup-seeded draft.

Switch identity/configuration comes from persisted switch definitions and branch-local runtime entries. Mode-prompt override changes only the mode-specific prompt, never SDK base instructions or denied-tool guarantees. Descriptor/context-window validation precedes acceptance.

Catalog first-run bootstrap and local-first refresh are application resource policy; background updates do not block normal picker usage. Offline without a usable store fails directly. Sources, schema, versions, refresh periods and aliases belong to [models](../phi-sdk/src/models/) and [settings](../phi-interactive/src/config/settings.rs). Picker presentation and update-result handling belong to [ModelPicker](../phi-interactive/src/interactive/components/picker/model.rs); accepted runtime selection remains with the [controller](../phi-interactive/src/interactive/controller.rs) and application runtime.

## Transactional transitions

Controller sends selection/navigation/reload intent to the owning workflow or scoped attachment, then installs the accepted Session and declarations. Rewind also restores editor text from the returned Session. Preparation, branch resolution, commit and failure semantics belong to [runtime replacement](agent/design-agent-runtime.md#candidate-first-runtime-replacement) and [Session transitions](agent/design-agent-runtime.md#tui-transition-behavior), not UI runtime construction.

## Extensions and MCP application boundary

Attachment exposes serializable extension declarations and semantic requests. TUI compiles supported contexts/keys once per accepted generation; AgentSession validates source/capability/permission and executes effects with the matching active sandbox/extension resource. VM, Phi and runtime collaborators never cross the client boundary.

The same extension generation serves SDK execution hooks/tools and application boundaries. Prepared lifecycle callbacks enter the SDK's ordered Hook registry; application-only boundaries remain explicit `ExtensionRuntime` operations. Explicit lifecycle notifications use matching `AgentRuntimeResources::notify`, not direct extension emission. Memory uses ordinary typed hooks, not an application-specific injection runner.

Application-bound commands can submit complete prompts through the SDK's scoped extension capability. AgentSession validates the captured generation against replacement and submits to its own AgentInstance; clients receive ordinary command status and scoped execution events, never a prompt they must execute themselves. TUI projects committed extension input from TurnStarted/UserInputInjected. These interactive extension effects do not imply RPC command endpoints.

MCP UI owns drafts, dirty baselines, expansion, forms and notifications; subscriptions follow accepted runtime replacements. Catalog-driven acceptance and save/reconnect semantics belong to [application MCP contracts](agent/design-agent-runtime.md#mcp-runtime-contracts); transport restrictions belong to [application security policy](agent/design-agent-runtime.md#configuration-write-access). See [extension projection](../phi-interactive/src/interactive/extensions.rs) and [MCP UI](../phi-interactive/src/interactive/components/panel/mcp.rs).

## TUI integration

Application uses `TuiRuntime<UiAction>` with explicit CodingFrame composition. `interactive/components/input.rs` selects surface/Picker contexts and resolves context names; `interactive/extensions.rs` uses that contract to compile extension bindings. Reusable widget/terminal mechanics follow the [TUI runtime contract](tui/design-tui.md#terminal-ui-runtime); business workflows remain application-owned.

`components/status.rs` composes Checklist progress, Workflow status, interaction prompts, queued inputs and the running indicator into the CodingFrame status block; business modules own their content. Historical notifications remain in the conversation projection.

Permission and MCP prompts compose reusable Choice/editor/menu mechanics in one input-only Widget, rendered in CodingFrame status chrome using RenderContext geometry. Visible overlays take input priority; Confirm bindings precede local keys, while other global shortcuts are consumed. The application retains the pending request envelope and replies through its owning attachment/request ID; schema conversion, allowlist policy and browser effects stay outside the reusable library.

Startup Resume and resource configuration use the same runtime with mounted Widgets and an explicit root Frame; their event loops and effects remain application-owned. Idle polling does not repaint unchanged screens. Resume storage remains pre-Agent CLI policy; config toggles emit write intent and retain drafts on failure.

Design: [reusable TUI](tui/design-tui.md). Implementation: [UI](../phi-interactive/src/interactive/), [scoped access](../phi-interactive/src/runtime/attachment.rs), and [runtime regressions](../phi-interactive/src/runtime/tests/).
