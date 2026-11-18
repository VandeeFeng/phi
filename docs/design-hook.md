# Hook Design

Hooks run extension handlers at lifecycle boundaries and return event-specific outcomes. The host owns the intercepted operation, permission checks and canonical Transcript.

## Model and terminology

```text
Hook registration
|-- owner / lifetime               who registers it and when it ceases to exist
|-- event                          the host lifecycle boundary
`-- matcher groups
    |-- matcher                    filter the event-defined target
    `-- handlers
        |-- handler A
        |   |-- condition          optional additional input filter
        |   |-- once: false        default: invoke on every match
        |   |-- timeout            callback deadline or command process budget
        |   `-- implementation     Rust callback / Lua callback / command
        `-- handler B
            |-- condition
            |-- once: true         at most one invocation attempt
            |-- timeout
            `-- implementation

Each invoked handler -> event-specific Outcome
Collected outcomes   -> event-specific reduction -> host application
```

This is a semantic model, not a requirement to add a struct at each indentation level. The declaration format remains event -> matcher group -> handlers. Owner/lifetime comes from the registering Instance, factory or extension; it is not another settings nesting level. Preparation can flatten groups into the existing registry.

- **Event**: owned input supplied when the host reaches a lifecycle boundary. The boundary exists even when no handler is registered.
- **Matcher group**: shares one event-target filter across handlers. It is not a serial execution group or a separate executor.
- **Handler**: one independently registered unit of extension logic, with its own condition, `once` flag and timeout. Two registrations using the same callback are distinct handlers; sibling handlers never share a one-shot allowance implicitly.
- **Outcome**: an event-specific decision or contribution, such as denial, input rewrite or added context. An execution error is not an outcome.

A registration associates an owner, event and handlers. A handler invocation is one execution attempt. Neither term adds another handler kind. **Hook** names the complete mechanism, not a fourth execution mechanism beside Rust, Lua and commands.

Phi follows Claude Code's traditional Hook model, including ordinary plugin hooks. Mods are out of scope: no `next(event)` middleware, replacement of the host operation or short-circuiting sibling handlers. An in-process Rust callback is still an ordinary handler.

The local reference is Claude Code's [Hook reference](https://code.claude.com/docs/en/hooks), [Hook guide](https://code.claude.com/docs/en/hooks-guide) and [SDK hooks](https://code.claude.com/docs/en/agent-sdk/hooks). These inform the model, not wire compatibility. Claude's Skill-only, successful-run `once` is not Phi's invocation-attempt `once: true`.

## Ownership and execution

```text
registration sources
    Rust factory/builtin callbacks | global settings commands | Lua declarations
                                  |
                                  v
                     prepare one candidate registry
                     validate declarations before activation
                                  |
                         accept runtime generation
                                  |
host reaches boundary ------------+
    |                             |
    | supplies owned event        v
    |                   Hook subsystem
    |                   1. match event target
    |                   2. check each handler condition
    |                   3. atomically check/consume once
    |                   4. execute handlers with their deadlines
    |                      Rust directly | Lua adapter | command adapter
    |                   5. collect and reduce event-specific outcomes
    |                             |
    +<----------------------------+
    | validates and applies outcome
    | runs ordinary authorization/sandbox checks where required
    ` commits canonical history and advances the original operation
```

The hook subsystem owns registration, matching, handler execution, deadlines, owned tasks and event-specific reduction. The Agent or application runtime reaches boundaries and advances its own flow. A handler does not own the intercepted operation, the Agent loop or the Transcript. Returning an outcome grants no authority to bypass permission or sandbox enforcement.

| Owner / location | Responsibility |
| --- | --- |
| Agent/application host | Reach boundaries, provide owned event input, validate/apply outcomes, own the original operation and canonical history |
| [`core/lifecycle.rs`](../phi-sdk/src/core/lifecycle.rs) | Define boundary events, reasons and accepted outcomes; no execution |
| [`hooks.rs`](../phi-sdk/src/hooks.rs) | Keep registrations, match handlers, account for `once`, invoke callbacks/adapters, enforce callback deadlines, reduce outcomes and own hook tasks/notices |
| [`hooks/command.rs`](../phi-sdk/src/hooks/command.rs) | Parse settings declarations, run host processes under application workspace trust and convert JSON/exit codes |
| [`extension/lua.rs`](../phi-sdk/src/extension/lua.rs) | Load Lua declarations, invoke callbacks in the generation's VM and convert dynamic values |
| [`context.rs`](../phi-sdk/src/context.rs) | Prepare prompts, call application expansion and gather contributions before host admission |
| [`extension/memory/hooks.rs`](../phi-sdk/src/extension/memory/hooks.rs) | Register ordinary handlers; own synchronization, retrieval and formatting, not generic Hook accounting |

Prepared generations retain factory/builtin callbacks, then settings command and Lua registrations in load/registration order. Lua binds to the candidate VM. A failed candidate leaves active resources and one-shot state untouched. Process authority and application activation policy are described below.

Existing execution mechanisms remain:

- typed Rust callbacks;
- Lua callbacks and their private adapters;
- settings command handlers.

`HookHandler` is an internal adapter trait, not a requirement to route Rust callbacks through JSON. Memory, retrieval and context insertion are business concerns and outcomes, not additional handler kinds. Model-prompt or verifier-Agent handlers are not implemented by merely supporting hooks on Agents.

`phi_sdk::lifecycle` exposes events and outcomes; `phi_sdk::hooks` exposes typed registrations, results, notices and timing. Registries, emitters and adapter JSON results remain private.

## Matching

The event defines the matcher target: tool name, notification type, command name or Session reason. A condition further checks the event input. Selection does not authorize execution.

- Rust uses typed predicates.
- Settings and Lua use declarative matchers and conditions.
- All handlers receive the same boundary input, not a sibling's rewrite.
- Unsupported event/filter combinations and invalid declarations fail preparation.

Plain matcher strings are exact and case-sensitive; `{ "regex": "^(Read|Write)$" }` explicitly selects a regex. Omitted, empty or `*` declarative matchers select all inputs. Conditions support `equals` or `regex` on supported event fields, including dotted object paths such as `tool_input.command`. Missing values do not match.

## Once and registration lifetime

Hooks normally invoke their handlers whenever event, matcher and condition match. The only additional invocation option is the boolean `once`, defaulting to `false`:

| Declaration | Behavior |
| --- | --- |
| omitted or `once: false` | Invoke on every match while registered |
| `once: true` | Start at most one invocation while this registration remains alive |

An unmatched event consumes nothing. A one-shot handler is consumed immediately before entering the callback or adapter execution path. Empty output, denial, errors, launch/permission failures, timeout, cancellation after start and recovery all leave it consumed. Host rejection or discarded contributions do not restore it. Work cancelled before execution starts consumes nothing. Concurrent matches cannot both start the same one-shot handler.

`once: true` needs no identity or scope configuration. Internal identities belong to registrations, not outputs or notices. Unnamed registrations use owner-qualified event/registration positions; reordering declarations can change those identities. Optional `with_id` (Rust) or `id` (settings/Lua) names a stable registration across reloads; duplicate identities within an owner are rejected. There is no public frequency policy or insertion receipt.

Owner/lifetime and `once` answer different questions without requiring independent control APIs:

- **Owner/lifetime**: which Instance or extension owns the registration, and when it is removed.
- **Once**: whether a still-registered handler may start another invocation.
- **Runtime generation**: which permission runtime, VM and tasks execute the current declaration; a reload is not by itself a new logical handler lifetime.

Reload preserves continuing handler identities and their consumed state, including retained old-generation work that has not started yet. Removed registrations end their lifetime. Failed candidates neither consume nor replace active state. Reset, compaction and history navigation do not rearm a one-shot handler. A new Instance has independent state; transient invocation accounting is not reconstructed from saved Transcript entries.

## Registration

Rust callbacks register through `PhiBuilder::hook`/`hooks`:

```rust
Hook::prompt_context(handler)
    .when(|event: &PromptContextEvent| !event.delegated_child)
    .with_once(true)
```

Handlers repeat by default; `with_once(true)` opts into one-shot invocation. `when` sets a typed condition, `with_matcher` selects an event target, `with_timeout` replaces one callback deadline and optional `with_id` names the registration. Rust callbacks retain event-specific types without passing through JSON.

Settings retain event -> matcher group -> handlers. This complete example belongs in global `~/.phi/settings.json` (`PHI_CONFIG_DIR` overrides the directory):

```json
{
  "hooks": {
    "trust": [
      "+/home/user/projects/app",
      "-/home/user/projects/untrusted"
    ],
    "SessionStart": [{
      "hooks": [{
        "type": "command",
        "id": "workspace-summary",
        "once": true,
        "command": "git status --short",
        "timeout": 2
      }]
    }],
    "PreToolUse": [{
      "matcher": "Bash",
      "hooks": [{
        "type": "command",
        "if": {"field": "tool_input.command", "regex": "deploy"},
        "command": "bash check-command.sh",
        "timeout": 2
      }]
    }],
    "PostToolUse": [{
      "matcher": {"regex": "^(Edit|Write)$"},
      "hooks": [{
        "type": "command",
        "command": "bash format-file.sh",
        "timeout": 5
      }]
    }]
  }
}
```

Replace the workspace paths and provide the example scripts. Commands run through `bash -c` in the generation's working directory, receive event JSON on stdin, and return JSON/plain output or an exit status. Script files are not auto-discovered. `/reload` applies declaration changes; script contents are read when executed. See [command input/output](../phi-interactive/README.md#command-hooks) for fields and exit-code conversion.

Lua retains `on(event, callback)` and also accepts individual handler declarations or matcher groups:

```lua
on("PromptContext", { once = true,
  handler = function(event, ctx)
    return { contexts = {} }
  end
})
```

`once` belongs to each handler, not the matcher group or callback function. Settings/Lua declarations support optional `id`, `if`, `once` and `timeout`; `matcher` belongs to the group or an individual-handler declaration. Owner/lifetime is supplied by registration, not by a configurable `scope` field. There is no generic `execution` or replacement `start_mode` field on handler declarations.

## Workspace trust and process authority

The application owns workspace approval and persistence; the SDK receives captured declarations and an `ExtensionHookPolicy`, without reading application settings.

- `hooks.enabled` defaults to `true` when omitted. Set it to `false` to disable settings commands and all Lua hooks without deleting declarations or prompting for workspace trust. Global and project settings support it; an explicit project value overrides the global value. `/reload` applies changes. Builtin/Rust callbacks and ordinary extension commands, tools and keybindings are unaffected.
- Settings command registrations and `hooks.trust` are global-only. Nonempty project command registrations and any project trust declaration are rejected before settings publication.
- Trust rules match the canonical workspace root exactly, not descendants. `+` trusts a workspace; an unprefixed absolute path means the same. `-` disables it and wins conflicting rules. Relative paths are rejected.
- Interactive startup requires an explicit trust/disable decision for an unknown workspace, saved through SettingsManager. Cancelling exits without a decision.
- Without a saved decision, process hooks block preparation outside print mode. Print permits missing trust but still honors disable rules. With no process hooks, RPC/web preparation needs no decision.
- Disable removes settings commands and all Lua hooks, including application boundaries, from prepared generations. Ordinary extension commands, tools and keybindings retain their own policies. Builtin/Rust callbacks are unaffected.

Settings and Lua Hook processes execute with host user authority, outside the tool sandbox and without Agent approval prompts. Lua still checks trusted source roots, declared programs and invocation-scoped capabilities; workspace approval does not authorize project Lua processes. Ordinary extension processes continue through the generation's permission runtime and sandbox. Hook execution never grants Agent permissions or exempts the intercepted operation from authorization.

## Timing, errors and reduction

Timing belongs to the boundary rather than a general handler configuration field. The event owner defines when the host must wait; ordinary handler declarations cannot select unsupported timing:

- Ordinary handlers are awaited.
- SessionStart's focused lifecycle API supports `Blocking` or `BeforeFirstPrompt` readiness. The latter starts owned work now; input preparation and `AgentRuntimeResources::ready()` await it. Cancelling a readiness wait retains the task. This does not introduce an async switch shared by every event.
- Notification is background observation. Its result cannot control an operation that already advanced or request recursive process approval.

Typed Rust and Lua callbacks default to 10 seconds. `PhiBuilder::hook_timeouts` changes defaults; per-handler deadlines replace, rather than stack with, the default. Settings commands have a 2-second process budget, configurable from 1 to 30 seconds, with no additional lifecycle deadline.

SessionEnd does not await startup readiness. It runs end handlers before draining notifications within one shared 1.5-second default budget. Callback deadlines also apply; increasing one does not increase the total budget. Expiration warns and cancels unfinished bounded async work. Persistence is outside this budget. Memory Git actions propagate cancellation through a drop guard: dropping their callback future cancels blocking process execution, workspace discovery and retry waits. On Unix, process cleanup also kills the child process group. Arbitrary blocking Rust work remains the handler's responsibility; dropping its callback future alone does not stop it.

Independent handlers run despite a sibling's denial. Registration order determines reduction, not execution order:

- UserPromptSubmit accumulates context and tool policy; the last rewrite differing from the boundary prompt wins. Any handled outcome prevents admission.
- UserPromptExpansion accumulates context; any block prevents application expansion.
- PreToolUse merges permissions with `deny > defer > ask > allow`; the last supplied input rewrite wins. Authorization checks the resolved input; Bash retains original-command evidence.
- Task and teammate gates aggregate their typed decisions.

Callback errors/timeouts remain separate from explicit decisions. UserPromptSubmit failures prevent admission; PromptContext errors reject preparation unless domain recovery handles them; pre-tool and completion gates fail closed. Observation failures produce notices. Hook outcomes never weaken authorization or sandboxing of the intercepted operation.

## Prompt preparation and Memory

```text
await readiness
    -> UserPromptSubmit
    -> optional application command description
        -> UserPromptExpansion
        -> application command expansion
    -> PromptContext
    -> host validates and admits input
    -> host commits prompt and contributed context
```

Application expansion is a host stage, not another submission handler. Recognition checks original and transformed command identity; Internal/Extension input and reserved commands bypass application Prompt/Skill expansion. Original text does not establish human authorship or permission. Merged tool policy is validated before admission, including delegated initial input and Stop feedback.

PromptContext supplies original/transformed text, source, dispatch, an owned Transcript snapshot, working directory and delegated-child status. Outcomes contribute typed `InjectedContext` and notices, not history mutations. PromptContext notices accompany accepted input; UserPromptSubmit notices are collected during reduction even if submission is later blocked. Hosts present notices separately from Transcript and tracing. `Context` contributes only to individual provider requests.

Memory uses ordinary handlers:

- SessionStart/SessionEnd run their configured Git actions. UserPromptSubmit runs only the separately configured `memory.hooks.userPromptSubmit` actions (default empty) and strips an original `/memory-inject` command. The default startup pull remains at SessionStart. See [synchronization defaults](design-sdk.md#memory-and-tape).
- Configured submission actions finish before PromptContext, not before sibling submission handlers. They may run even when a sibling handles input or later expansion/admission rejects it; completed Git effects are not rolled back.
- Automatic PromptContext injection declares `once: true` when configured and eligible. Its first execution attempt consumes the allowance, even if it produces no content, fails or its contribution is discarded. Recovery may warn but never rearms it.
- Explicit `/memory-inject` uses a separate default-repeatable handler. The automatic handler's condition excludes explicit requests, so manual injection neither bypasses nor consumes its one-shot state. Explicit injection remains available after automatic injection and when automatic injection is disabled. Unavailable, disabled or failed explicit injection rejects preparation.
- `delegated_child` in submission and context events lets Memory skip per-submission synchronization and automatic injection; it grants no authority.

Automatic injection has no separate insertion receipt, admission accounting or Reset retry. Tape keyword authorization retains its own admission accounting, independent of injection Once. Memory owns synchronization, retrieval/precomputation and formatting; the hook subsystem does not inspect Memory types or select files. See [Memory and Tape](design-sdk.md#memory-and-tape) and [Tape injection](../phi-sdk/src/extension/memory/docs/tape-design.md#memory-context-injection).

## Boundary catalog

An event's accepted effects and completion requirements are part of its boundary contract, not inferred from its handler language or `once` flag.

| Boundary | Host stage / accepted effect |
| --- | --- |
| SessionStart / SessionEnd | Prepared Session initialization, readiness and bounded cleanup |
| AgentStart / AgentEnd | Observe one execution run starting / its runner exiting |
| AgentTurnStart / AgentTurnEnd | Start observes an inner model/tool round; End may revise future context after message commit; retries remain within the round |
| BuildSystemPrompt | Replace the generation's next system prompt |
| UserPromptSubmit | Transform input, contribute context/tool policy/notices or handle input before admission |
| UserPromptExpansion | Add context or block recognized application Prompt/Skill expansion; never perform expansion |
| PromptContext | Prepare contributions for canonical context insertion with a submission |
| Context | Add request-local provider context; no canonical history mutation |
| PreToolUse | Continue, rewrite or deny an individual tool invocation |
| PostToolUse / PostToolUseFailure | Observe or rewrite a tool result before commit; never undo completed effects |
| Stop | Allow, continue with internal feedback or fail candidate completion |
| TaskCreated / TaskCompleted | Allow/reject checklist Task changes; not Agent creation or implicit Task completion |
| TeammateIdle | Allow, continue or stop a managed teammate proposing idle |
| Notification | Background observation of pending permission, accepted completion or explicit messages; no gate |

`AgentTurnEndEvent` supplies round information and an owned committed Transcript snapshot. Handlers return `AgentTurnEndOutcome.context_edits` as `ContextEditDraft` proposals, not committed entries. The pipeline validates the merged batch on a candidate and appends it atomically; invalid batches are discarded with a warning, and callback failures remain nonfatal notices. Only future model context changes; raw history is untouched. See [context-edit semantics](design-sdk.md#transcript-and-durable-effects).

Adapter/application boundaries do not imply public Rust constructors or support in every adapter:

| Boundary | Responsibility |
| --- | --- |
| BeforeProviderRequest / AfterProviderResponse | Observe a provider attempt, not rewrite provider payloads |
| PermissionRequest / PermissionDenied | Observe authorization, not replace the permission runtime |
| PostToolBatch | Observe a resolved batch; supported adapter output may add context or terminate the run |
| StopFailure | Observe runner failure, not candidate completion |
| PreCompact / PostCompact | Observe context compaction, not initialize a Session |
| MessageDisplay | Transform display-only text, not canonical content |
| resources_discover / session_tree | Application discovery/navigation, not Session boundaries |
| command_before / command_after / ui_render | Explicit application extension operations, not SDK execution control |

Settings command handlers currently support SessionStart, UserPromptSubmit, UserPromptExpansion, PreToolUse, PostToolUse, Stop and Notification. Typed Rust and Lua support follow their constructors/declarations; shared terminology does not promise equal event coverage.

## Session boundaries and hook tasks

Session boundaries belong to application policy; Agent, turn and tool boundaries belong to SDK execution. `Phi` lifecycle methods notify factory hooks only, not prepared generations.

| Operation | SessionStart | SessionEnd |
| --- | --- | --- |
| First standalone CLI runtime, new or restored | Startup | None |
| In-process Session creation | New | New only when explicitly ending the outgoing Session |
| In-process saved Session resume | Resume | Resume only when explicitly ending the outgoing Session |
| Select an already live target or clone/drop access | None for the target | Only if outgoing transition policy explicitly ends a Session |
| Same-ID reload | Reload for accepted generation | Reload for previous generation |
| Explicit stop/shutdown/Quit | None | Quit |

Selecting the current Session is a no-op. Reset, compaction, rewind and tree/anchor navigation are not Session boundaries. Typed reasons are authoritative even where compatibility payload strings share a spelling. Dropping an attachment never implicitly stops its Agent.

Hook tasks belong to their executing generation. Notification callbacks and SessionStart readiness work remain owned, bounded and cancellable; none transfers ownership of the host's main execution. In print (`-p`) teardown, application runtime calls `AgentRuntimeResources::cancel_background_hooks` before SessionEnd: unfinished background tasks are cancelled rather than awaited to completion, and emit `HookNotice::Cancelled` (`kind: "cancelled"`) with their event name. Cancellation closes background scheduling for that generation, drops callback futures and triggers their process cleanup; a fresh generation is unaffected. Completed task results remain intact. SessionEnd handlers still execute within their shared budget; persistence remains outside it. Interactive shutdown retains its bounded drain policy. This follows Claude Code's [async-hook teardown rule](https://code.claude.com/docs/en/hooks#run-hooks-in-the-background), not a promise that Phi supports its `async: true` command declaration. Hosts notify and drain notices from matching `AgentRuntimeResources`. Lua `ctx.ui.notify` messages remain separate extension notifications. Applications retain both kinds across reload/stop and present them without treating notices as Transcript entries or authorization.

BuildSystemPrompt finalizes the generation's base prompt at readiness or first execution, after SessionStart readiness work. Entering SessionStart drains previous startup tasks without finalizing the prompt; candidate construction never finalizes it. Specialized runs have their own boundary. Directed request instructions stay outside the generation cache.

Stop checks a candidate final response already committed to the Transcript, before completion publication:

- `Allow` accepts it, subject to pending input.
- `Continue { feedback }` prepares ordinary internal input before more work; feedback must be nonempty and at most 16 KiB. At most eight automatic continuations are accepted per run.
- `Fail`, callback/preparation failure, invalid feedback or exhausted budget fails the run.

Stop is not cancellation or a finalizer. AgentEnd observes runner exit; StopFailure observes runner errors; SessionEnd ends a Session/generation. Tool-driven termination and managed teammate gates do not become Stop events.

Boundary types live in [`core/lifecycle.rs`](../phi-sdk/src/core/lifecycle.rs). Application discovery, tree navigation, commands and display operations use explicit ExtensionRuntime methods, not a public generic event bus.

## Related designs

- [SDK lifecycle and Memory](design-sdk.md#lifecycle-model)
- [Agent execution and preparation](design-harnes.md)
- [Application Session policy](agent/design-agent-runtime.md#lifecycle-and-adapter-policy)
- [Application extension integration](design-interactive.md)

## Validation

Test owning flows, not only registration helpers: default repeat invocation; `once: true` without extra control configuration; matching before consumption; concurrent/cancelled one-shot invocation; empty/error/timeout/rejected contributions; reload continuity and independent Instances; Reset without rearming; same-input concurrent handlers and deterministic reduction; readiness/shutdown ownership; Memory automatic versus explicit injection and repeatable synchronization.
