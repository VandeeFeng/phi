# Lua extensions

Lua extensions run without ambient OS, I/O, package-loading, or file-loading APIs. Host operations must use an explicitly scoped capability or typed effect. Candidate generations evaluate captured sources in independent VMs without rereading files; mutable Lua state is not inherited from the loading template or another candidate. Within a generation, hooks, commands, keybindings, display handlers, and `register_tool` declarations share the same runtime and state. SDK execution and application operations use that generation's matching permissions.

## Process capability

Lifecycle hooks and commands declare exact executables per handler context:

```lua
register_extension({
  name = "command-policy",
  capabilities = {
    process = {
      SessionStart = { "rtk" },
      PreToolUse = { "rtk" },
      Command = { "git" },
    },
  },
})
```

Declaration limits the process API exposed to the handler. Execution then uses the same command permission rules as Phi's Bash tool; no extension-specific settings are required. Existing rules apply regardless of whether the command came from the agent or an extension:

```json
{
  "permission": {
    "bash": {
      "rtk **": "allow",
      "git status **": "allow",
      "emacsclient **": "ask"
    }
  }
}
```

When no rule matches, the active permission mode decides whether Phi asks, allows, or denies. Permission rules cannot expand the executable set declared by Lua. Process execution also remains limited to extension sources under the global Phi config root; repository-controlled `.phi` extensions cannot authorize process access through project settings.

A declared and permitted handler can await a process result:

```lua
on("PreToolUse", function(event, ctx)
  local result = ctx.process.run("rtk", {
    "rewrite",
    event.payload.tool_input.command,
  }, {
    timeout_ms = 1000,
  })

  if result.success then
    return {
      updated_input = { command = result.stdout },
    }
  end
end)
```

`ctx.process.run` returns `success`, `exit_code`, `stdout`, `stderr`, and `truncated`. The host executes the program without shell interpolation, applies its sandbox, caps time/output, and cancels the child if the handler is dropped.

Capabilities are limited to the declaring handler invocation and the intersection of trusted source, declared context/program, and command permission. Tool-display handlers never receive process capability.

Prepared lifecycle callbacks share one SDK registry: factory/builtin Rust callbacks run first, settings commands next, then individual Lua callbacks in extension load/registration order. `PreToolUse` handlers run in that order. A returned `updated_input` becomes the input seen by the next handler and the final registered tool. Bash permission evaluates both the model's original command and the final rewritten command, taking the strictest result across their AST operations; an RTK rewrite therefore cannot hide a configured Git or destructive-command rule. Ordinary hooks cannot auto-approve execution. Return `block = true` with a reason to deny a call.

## User prompt dispatch

`UserPromptSubmit` runs before input admission, including internal reports and Agent messages. Its payload includes `original_prompt`, the latest transformed `prompt`, `source` and resolved `dispatch` (`immediate`, `steer`, or `follow_up`). `original_prompt` remains unchanged across transforms; Internal/Extension text is not human approval. Hooks may transform or handle input and add structured context, but cannot change dispatch or control queues. Each transform sees the preceding result. `injected_context` is a list of `{ source, content }` values, not a string.

```lua
on("UserPromptSubmit", function(event)
  if event.payload.dispatch == "steer" then
    return { injected_context = {{ source = "hook", content = "Keep the active response concise." }} }
  end
end)
```

`PromptContext` runs after all input transforms. Its payload contains `original_prompt`, transformed `prompt`, `source`, `dispatch`, a Transcript snapshot, `working_dir` and `delegated_child`. Return a contribution, not a prompt rewrite:

```lua
on("PromptContext", function(event)
  if event.payload.delegated_child then return end
  return {
    contexts = {{ source = "hook", content = "Check the project conventions before editing." }},
  }
end)
```

Generic contributions are prepared for each input and have no key-based deduplication. Memory's automatic injection is the exception, managed internally by MemoryHooks and Instance admission. Notices may accompany a contribution; they are host observations, not model input.

Each prepared Lua callback has its own 10-second timeout by default, configurable by the SDK host. Settings commands have separate process deadlines; see [command hooks](../../phi-interactive/README.md#command-hooks). UserPromptSubmit failure/timeout handles input without scheduling it; PromptContext failure/invalid output rejects preparation. Preparation remains cancellable by abort/stop. A completed internal feedback attempt is not automatically replayed after failure. Slow hooks cannot reorder Follow-ups; queued Steers retain priority even when the previous turn ends during preparation.

`UserInputInjected` reports that accepted input was committed into the active run, not merely prepared or queued.

## Completion and notifications

`Stop` runs before candidate completion is accepted. Return `{ block = true, reason = "Verify the remaining work." }` to request bounded continuation through ordinary prompt preparation. Its payload includes `response`, `continuations` and `stop_hook_active`. It does not run again after acceptance.

`Notification` observes `permission_prompt`, `idle_prompt` (accepted completion) and explicit `message` events asynchronously. Payload fields are `notification_type`, `level`, `message` and optional `tool_name`. Failures warn without controlling execution. Process capabilities must already be authorized; notification callbacks cannot create approval questions. SessionEnd runs end hooks before draining pending notifications within its shared budget, aborting unfinished notifications on expiration.

## Command idle coordination

Slash commands execute immediately, including while the main agent is streaming. Reject interactive operations that require an idle agent:

```lua
register_command("safe-update", {
  handler = function(args, ctx)
    if not ctx.require_idle("apply the update") then return end
    ctx.ui.notify("applying " .. args)
  end,
})
```

`ctx.require_idle(operation)` returns `false` and reports the conflict when the agent is busy. Use `ctx.wait_for_idle()` only when delayed execution is intentional; it resumes after the main run and queued follow-ups settle. Prompt templates and skills are agent input rather than control commands: Enter steers the active run, while Alt+Enter queues a follow-up.

## Submit a message from a command

Command handlers receive an invocation-scoped Agent capability. It submits a complete prompt to the bound Agent using the normal input preparation, execution, and Transcript path:

```lua
register_command("review", {
  description = "Review code changes",
  handler = function(args, ctx)
    if not ctx.require_idle("start review") then return end
    ctx.agent.send_user_message("Review changes against " .. args)
  end,
})
```

`ctx.agent.send_user_message(text, options)` awaits submission acceptance, not completion. The receipt has `status` (`started`, `queued`, or `handled`) and the applicable `turn_id`, `submission_id`, `dispatch`, `prompt`, or `reason` fields. A handled submission is reported as a rejected command; invalid input and submission failures raise errors.

By default, an idle Agent starts a turn and a busy Agent handles the request without starting it. Explicit `{ deliver_as = "steer" }` joins active work; `{ deliver_as = "follow_up" }` queues a new turn. Both start immediately when idle. Queue order and cancellation belong to AgentInstance.

Messages must be nonempty strings. They are complete prompt content: slash commands, skills, and templates are not expanded again. Existing input hooks still run. History records a User message with `extension` submission source; generated text cannot authorize Tape keyword mutations. Model/tool permissions and sandbox restrictions remain unchanged.

This capability is available only inside a bound command invocation, expires when the handler exits, and rejects submission after runtime replacement or Agent stop. It cannot select another Agent. Standalone SDK `ExtensionRuntime::run_command` has no bound Agent; SDK hosts can supply `ExtensionPromptSubmitter` through `run_command_with_agent`. No additional Lua capability declaration is required.

The [review example](review.lua) registers `/review` and submits its generated prompt directly; it does not use an input hook.

## Tool registration and override

New tool names register normally. Reusing a canonical name such as `Bash` or `bash` requires explicit replacement intent:

```lua
register_tool("Bash", {
  override = true,
  description = "Provide a complete custom Bash contract",
  parameters = {
    type = "object",
    required = { "command" },
    properties = {
      command = { type = "string" },
    },
  },
  handler = function(input, ctx)
    -- Complete replacement implementation.
  end,
})
```

A duplicate without `override = true` rejects candidate runtime construction. Explicit overrides may replace built-in tools and memory tools regardless of registration order. Tools provided by MCP servers cannot be overridden by Lua. A full replacement owns its complete contract and does not inherit built-in permission, sandbox, timeout, durable finalization, or output behavior. Use `PreToolUse` when only rewriting commands.

Lua tool handlers currently have no process capability. Process-backed Bash replacement remains deferred until Phi exposes a typed built-in operations boundary; use the built-in Bash plus `PreToolUse` for RTK and command policy today.

## Keybinding effects

Keybindings request fire-and-forget interactive effects instead of awaiting process output. They must declare the executable under `Keybinding`:

```lua
register_extension({
  name = "editor-jump",
  capabilities = {
    process = { Keybinding = { "emacsclient" } },
  },
})

ctx.process.spawn("emacsclient", { "-n", "file.rs" })
```

The SDK validates the declaration and trusted source before producing an effect, then applies normal command permission before execution. The interactive adapter applies the resolved sandbox policy, executes argv without shell interpolation, and includes extension identity in failures.
