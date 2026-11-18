# phi-interactive

## Purpose

CLI application behind `phi`: interactive, print, RPC and gateway modes. Owns configuration/resource resolution, application Sessions, runtime preparation, mode policy, persistence and terminal projection. SDK owns execution; `phi-tui` owns reusable terminal mechanics.

Architecture: [application](../docs/design-interactive.md), [runtime boundaries](../docs/agent/design-agent-runtime.md), [workspace](../docs/design.md).

## Code map

- `src/main.rs` — configuration, concrete capability composition, mode dispatch and process finalization.
- `src/cli/` — flags/precedence, pre-Agent Resume, config/list commands, print adapter and gateway composition.
- `src/cli/web.rs` — gateway discovery/process lifecycle, browser opening and frontend setup.
- `src/runtime/gateway.rs` — gateway host Session bindings and polling-based TUI access, submissions and permission persistence.
- `src/rpc/` — strict tagged JSONL messages, bounded I/O, controls, observations and RPC lifecycle; sibling of `interactive/`, not beneath CLI parsing.
- `src/config/manager.rs` — shared mutable SettingsManager; sole owner of settings loading/refresh and the global/project/merged in-memory source.
- `src/config/` — typed configuration, shared Switch policy, manager-owned settings storage/paths and safe picker seeds.
- `src/security.rs` — startup-captured RPC write/transport policy and optional file trust.
- `src/runtime/preparation.rs` — candidate/configuration preparation, catalog reads and Session workflows.
- `src/runtime/factory.rs` — isolated SDK/application candidates.
- `src/runtime/associations.rs` — Session associations, locks, transitions and cleanup, not SDK membership.
- `src/runtime/session.rs` — AgentSession metadata/resources, gates, Memory accounting, replacement and canonical checkpoint representations.
- `src/runtime/attachment.rs` — scoped internal access, coherent recovery and save-warning cursor.
- `src/runtime/save.rs` — per-Session checkpoints, retries and typed save observations.
- `src/runtime/mcp.rs` / `subagents.rs` — catalog-driven runtime acceptance and delegated application preparation.
- `src/session/` — durable envelope, bounded discovery, locks, storage policy and JSONL journal.
- `src/commands.rs` — application command definitions, grammar, typed invocations, reserved names and declaration aggregation/projection; no UI matching or execution.
- `src/resources/` — discovery/selection, Prompt/Skill loading, resource declarations/expansion and diagnostics; not the command registry.
- `src/interactive/mod.rs` — terminal event loop, transcript metadata synchronization and read-only save notices.
- `src/interactive/agent/` — main recovery/submission/queue projection and passive child observations/details.
- `src/interactive/controller.rs` / `commands.rs` — typed command execution, local handlers and accepted transactional transitions.
- `src/interactive/completion.rs` — presentation-only completion ranking and fuzzy matching shared by slash input, argument hints and mentions.
- `src/interactive/state.rs` / `conversation.rs` / `blocks.rs` / `render.rs` — interaction state, display projection, indexed mutations and frame composition.
- `src/interactive/activities.rs` — Tasks overlay with Checklist (recorded work) and Activities (subagents/Bash); projection, refresh and panel state synchronization, not execution ownership.
- `src/interactive/memory.rs` — process-local Memory administration shared by commands, Rewind and Review; `runtime/session.rs` coordinates generation-scoped hook notices and submission.
- `src/interactive/components/panel/memory.rs` — Memory Review panel state, input and rendering.
- `src/interactive/overlays.rs` — active-panel dispatch, mounting and dismissal; business modules own panel layout.
- `src/interactive/components/`, `extensions.rs`, `mcp/` — input/picker/prompt surfaces, extension effects and MCP UX.
- `src/runtime/tests/` — orchestration regressions grouped by business boundary; fixture data is not a forwarding service.

## Application boundaries

- Keep main as the imperative shell. Modes call concrete owning capabilities; never recreate a universal application facade, including in tests.
- AgentInstance alone owns mutable execution and canonical live Transcript. Manager owns SDK membership/delegation. Application associations own metadata, locks, transitions and persistence, not scheduling.
- AgentAttachment and SessionAccess are scoped internal access; dropping them never implicitly stops an Agent. Runtime collaborators never appear in external wire payloads.
- Use AgentRuntimeFactory for complete isolated candidates. Retain SDK resources from the candidate for live grants, extensions and Session boundaries; factory services contain policy/initial seeds, not live permission state. Validate before active/persisted mutation. SDK acceptance precedes non-fallible matching resource/metadata swap; post-commit save failure is a warning, not rollback.
- Capture coherent Session/snapshot/declarations under the transition guard and install together. RuntimeView is observation, not acceptance authority. No follow-up declaration rereads to complete acceptance.
- Register the master through ordinary SDK registration, without privileged Agent authority. Master names the primary conversation Agent, not a permission role. Preserve specialization/directory/grants/origin during rebuild. Reject stopped delegated execution until validated reconstruction exists; passive history is data only.
- Preserve accepted transactions, submission/Memory accounting and confirmed permission answers despite waiter loss. Preparation before admission remains cancellable.
- Permission accounting has its own gate: lifecycle hooks may require replies. Do not await slow work while holding association map locks.
- Main finalizes Sessions after mode success/failure and shuts down startup MCP resources. Print teardown cancels background hooks with outcome `cancelled` before bounded SessionEnd execution; successful output follows persistence and resource cleanup. Interactive retains input and TerminalGuard while awaiting root cleanup, warns once when hooks remain in flight, and permits a subsequent Ctrl+C to restore the terminal and force exit with code 130, bypassing remaining finalization. Normal completion stops input after the final frame. See [shutdown policy](../docs/design-interactive.md#interactive-event-loop).
- Save/SDK finalization failures retain closed associations, canonical evidence and locks for retry. Do not repeat SessionEnd, fabricate outcomes or remove a reused registration.

## RPC scope

- One controlled root initialized from new intent or saved ID; no arbitrary live attachment, independent roots, Session/history service or raw Manager forwarding.
- Version-2 tagged JSONL, not legacy JSON-RPC methods. Inputs/admissions, outcomes, controls and permission questions have distinct identities. Steer need not create a turn; typed Busy is not Handled.
- Keep messages/framing out of SDK and interactive/print. Bounded worker waits must not block interruption, answers or close.
- EOF drains admitted work unless a required permission answer is unreachable. Explicit close drains/cancels; drain rejects inputs but retains answers/cancellation and supports escalation.
- Supervise bounded input/output/observations through main-owned finalization and final flush. Lag, output failure and unsupported blocking interactions fail explicitly; never report successful drain or silently approve.
- No MCP controls, dedicated MCP event stream or elicitation question/answer schema. Agent-side MCP tools remain under existing security policy; interactive MCP is unchanged.
- No settings/auth writes, login, local editor/admin commands or compatibility aliases. Stdio inherits launcher authority; initialization is not authentication.

## Configuration, Session and persistence

- Keep CLI precedence explicit; settings changes require parsing, defaults, validation and runtime/UI wiring.
- SDK builtin configuration belongs to `RuntimeConfig.extensions`. Preparation resolves exclusions after CLI/runtime overrides and before resource preparation; `ExtensionConfig::is_enabled` determines effective builtin enablement. Settings files define an `extensions` selection array and top-level Memory/Codemode blocks; SettingsManager owns scoped merging.
- Follow [settings ownership](../docs/design-interactive.md#settings-ownership): share one SettingsManager across application/resource consumers for the prepared scope. Keep raw global/project settings alongside merged settings; queries use memory only. Resource consumers reject missing managers/scope mismatches, never load settings as fallback.
- Application owns Hook workspace trust; SettingsManager alone validates and persists it. Pass captured declarations and policy into SDK candidate preparation, never execute hooks in mode adapters. Agent permissions never substitute for workspace or Lua source trust. See [Hook trust design](../docs/design-hook.md#workspace-trust-and-process-authority).
- Refresh settings explicitly in application preparation through SettingsManager, once per refresh operation, never in getters/loaders. Validate before replacing manager state. Manager refresh and protected persistence do not accept an execution generation; keep candidate validation, accepted configuration and loaded generation resources separate.
- Separate raw settings, initial CLI selection and accepted runtime configuration. Never mutate raw defaults with Switch values or reuse resolved startup options. `/new` inherits the accepted effective configuration, not only its Switch name; SDK parameters and UI must agree. Follow [configuration authority](../docs/agent/design-agent-runtime.md#runtime-configuration-authority).
- App Session wraps SDK history; never create another reducer or retain duplicate full history. Read/release history at concrete consumers. Keep metadata synchronization separate from save observation.
- Ordinary tree/rewind navigation changes only the in-memory cursor, preserving live model/Switch/thinking/mode and accepted resources. No settings refresh, history deletion, checkpoint or fabricated configuration entry. Journal recovery selects the last appended entry, never metadata leaf_id; later checkpoints/finalization must not make navigation durable.
- Continuation retains the current model/thinking/Switch/environment and appends actual provider/model plus Switch identity through the SDK reducer. On reopen, recover actual model/Switch; resolve that Switch's defined fields before historical fallback. Do not overwrite actual model attribution with the Switch model default; explicit `/reload` still reapplies strong Switch selection. No full execution snapshot or navigation configuration event. `session_tree` is notification only; summaries/labels require explicit operations. See [navigation and recovery](../docs/agent/design-agent-runtime.md#navigation-and-recovery).
- UI polls save observations; finish/cancel events never request checkpoints. Missing observations are unknown, not success. Each association saves only its own canonical history and metadata.
- Saved operations preserve locks and reject deletion of associated Sessions. Storage owns paths; discovery owns bounded lookup; journal owns JSONL mechanics. Preserve entry IDs, branches, usage, runtime config and delegation linkage.
- Same-ID reset is SDK idle-only admission, preserving history/configuration/grants and adding effective-context markers. Busy never queues/aborts. Accounting and checkpoint survive admitted waiter loss.
- `working_dir`, `project_root` and `session_scope` follow workspace definitions. Resolve project scope once with `config::settings::project_scope_dir`; home has no separate project `.phi` scope.
- Load Agent/Skill/Prompt roots via `resources::resource_roots`, project first. Instructions prefer nonempty AGENTS.md over CLAUDE.md. Builtin Skill links never overwrite real user files/directories. Loaders consume SDK extension availability declarations, not Memory/Tape configuration; applications retain discovery, priority and owned-link reconciliation.
- Settings persistence, picker drafts and accepted runtime selection are separate. MCP Save only persists MCP settings; explicit reload applies them. Save never refreshes settings/resources, replaces runtime or checkpoints a Session. Pickers consume safe seeds, never Config/credentials. No TUI filesystem fallback or saved-config acceptance authority.
- RPC settings/auth protection is mandatory. Global startup `trust_remote` relaxes optional guards only, never authorization or capabilities. Preserve command-only MCP transport policy under protected execution.
- Do not read, expose or persist secrets outside existing typed configuration/runtime paths.

## Resources, Memory and interactions

- Application commands own syntax and declarations; CommandRegistry aggregates declarations and resolves typed intent. Resources provide Prompt/Skill declarations and expansion; controllers execute intent without reparsing slash text. SlashCommand.arguments is generated UI metadata, not a second syntax configuration. Completion ranking and fuzzy matching belong to interactive, never application command semantics.
- Prompt `allowed-tools` is outer-turn invocation preapproval; `disallowed-tools` removes whole tools. Agent `tools` selects and `disabled-tools` removes Phi-governed tools, including ToolSearch. Skills are context-only and reject tool fields.
- Prompt and Agent settings reject retired `MCP`/`mcp` names and individual server-tool identities. Exposure/withdrawal remains MCP policy. Use immutable candidate validation and SDK grammar.
- Retain validated Prompt declarations and source diagnostics at load time; expansion never reparses YAML. MCP catalog revalidation uses frozen sources, preserves valid priority and does not reread files.
- Install generation-scoped Prompt/Skill expansion through the application `PromptExpander`; modes submit original TurnPrompt input. SDK runs `UserPromptSubmit`, intercepts recognized expansion through `UserPromptExpansion`, then calls the application expander. Expansion contributes content/policy together, never grants. SDK validates before admission/commit and owns activation/retirement. Durable grants enter only through confirmed AllowAlways answers, with retained save warnings/retry and no second UI call.
- Memory administration freezes process startup scope; runtime preparation remains Agent-generation-scoped. SDK owns execution/store/Tape authorization. Import Memory APIs only through `phi_sdk::extension::memory`.
- UI submits original input, never infers Tape keyword authorization. Generated task text grants no human consent.
- Permission UX uses scoped envelopes/request IDs, never raw SDK reply channels. MCP interaction schema/browser conversion remains application-owned.

## Interactive and terminal

- UiState owns interaction; ConversationView owns display projection, never canonical history. Project public PhiEventEnvelope, never SDK internal Core events.
- Reconcile recovery with matching actor boundaries; on lag/missing markers retry or fail, never clear blindly. Queue projection uses submission IDs. Enter is Steer; Alt+Enter is Follow-up; SDK owns draining.
- Controller requests preparation/navigation intent and installs accepted results. New/resume prepares before stopping the outgoing root; no promise of resurrection after stop. Reads never navigate.
- Keep UI Activity vocabulary separate from SDK Execution semantics. Checklist progress and Activity status never determine each other; retain SDK `BashExecution` and explicit typed control intents.
- Child presentation/selection never schedules, consumes outcomes or implicitly cancels/checkpoints. SDK owns identity/status/cancellation; application persists ordinary children as `<parent-id>/subagents/agent-<id>.jsonl` sidechains.
- Project Agent tool status from canonical ToolResults consistently live and on resume, never from child admission. Foreground results describe execution; background receipts describe startup and stay unchanged by later child outcomes. Parent wait interruption is not child cancellation. See [delegation invariants](../docs/agent/design-parallel-agent.md#foreground-and-background-delegation).
- Map SDK diff hunks to phi-tui DiffView; never recompute canonical diffs. Keep routing/retirement in application and display clocks in TrackedBlock.
- Compile extension keybindings once per accepted generation. Keep the matching Arc<ExtensionRuntime> for SDK and application effects; UI gets serializable declarations, not VM/Phi/Sandbox. Execute only after authorization with matching sandbox/generation.
- MCP snapshots/events are SDK-canonical; UI owns drafts/forms, application owns scoped acceptance. Contract changes are full reloads, never in-place tool mutation. `/mcp-auth` is diagnostic-only, not OAuth or a wire feature.
- Commands/components/background tasks never write directly to terminal. Use typed effects/channels and render in the main loop. Initial diagnostics go to stderr before terminal entry; protocol stdout is framing-only.
- Use TuiRuntime focus/contexts/overlays/invalidation, TerminalGuard and explicit root Frame. Widgets emit typed UiAction; controllers retain reply authority. Preserve editor/overlay state during recovery.
- MCP and Lua are always compiled; configuration controls loading/activation. Do not make phi-tui depend on SDK or application.

## Related

- [Workspace rules](../AGENTS.md)
- [SDK rules](../phi-sdk/AGENTS.md)
- [TUI rules](../phi-tui/AGENTS.md)
