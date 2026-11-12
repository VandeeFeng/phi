# Phi Lua extension API

Phi evaluates each configured `.lua` file once. Top-level locals persist across hooks, commands, keybindings, displays, and tools in that runtime generation.

## Runtime and load paths

Load order:

1. `${PHI_CONFIG_DIR:-~/.phi}/extensions/*.lua`
2. `.phi/extensions/*.lua`

Files inside each directory are sorted. The runtime exposes coroutine, table, string, utf8, and math libraries, but no ambient `io`, `os`, `package`, `require`, `dofile`, or `loadfile`.

## Registration API

### `register_extension(spec)`

Set extension identity and capabilities for registrations that follow it in the same file.

```lua
register_extension({
  name = "my-extension",
  capabilities = {
    process = {
      SessionStart = { "git" },
      Command = { "git" },
      Keybinding = { "emacsclient" },
    },
  },
})
```

Process capability is the intersection of exact declarations, trusted source, normal command permission, and sandbox policy. Process execution is trusted only from the global Phi config root; use a global extension for process-backed behavior.

### `on(event_name, handler)`

Register a lifecycle hook. The handler is `function(event, ctx)` where:

- `event.name` and `event.type` contain the event name.
- `event.payload` contains the fields listed below.
- `ctx.ui.notify(text)` emits a notification.
- `ctx.process.run(...)` exists only when declared for this exact event.

Hooks run in load/registration order. Return `nil` for no change. Common control fields are `block = true`, `cancel = true`, and `reason = "..."`.

### `register_command(name, spec)`

```lua
register_command("hello", {
  description = "say hello",
  handler = function(args, ctx)
    if not ctx.require_idle("say hello") then return end
    ctx.ui.notify("hello " .. args)
  end,
})
```

The slash command is `/<name>`. `description` is optional. The handler receives the unparsed argument string and:

- `ctx.ui.notify(text)` — report command output/status.
- `ctx.require_idle(operation)` — return false and reject when the agent is busy.
- `ctx.wait_for_idle()` — await idle state when delayed execution is intentional.
- `ctx.process.run(...)` — available only when declared under `Command`.

### `register_tool(name, spec)`

```lua
register_tool("word_count", {
  description = "Count whitespace-delimited words",
  parameters = {
    type = "object",
    required = { "text" },
    properties = { text = { type = "string" } },
  },
  handler = function(input, _ctx)
    local _, count = input.text:gsub("%S+", "")
    return { count = count }
  end,
})
```

`description` and `handler` are required. `parameters` defaults to an empty JSON object schema and, when supplied, must declare `type = "object"`; `properties` must be a table and `required` an array of strings. Input is decoded JSON. A string return is emitted directly; other Lua values are JSON-encoded. Tool handlers have `ctx.ui.notify` but no process API.

Tool names `read`, `write`, `edit`, `bash`, `grep`, `glob`, `agent`, and `mcp` canonicalize to Phi's built-in capitalization. Replacing any existing canonical tool requires `override = true`. A replacement owns the complete contract and does not inherit built-in permission, sandbox, timeout, finalization, or output behavior. Prefer `PreToolUse` for input policy/rewrites.

### `register_tool_display(name, handler)`

Customize a tool-call display without changing execution:

```lua
register_tool_display("word_count", function(input, _ctx)
  return { title = "Count words", expand = false, content = input.text }
end)
```

Return a title string, `{ title?, expand?, content? }`, or `nil` for fallback display. Display handlers have no process API.

### `register_keybinding(key, spec)`

```lua
register_keybinding("ctrl+e", {
  context = "picker.rewind",
  handler = function(event, ctx)
    ctx.ui.notify(event.selection and event.selection.label or "no selection")
  end,
})
```

Supported contexts: `editor`, `slash_menu`, `file_mentions`, `picker.generic`, `picker.model`, `picker.resume`, `picker.rewind`, `picker.switch_select`, `picker.switch_configure`, `picker.switch_edit`, `picker.switch_name`, `picker.switch_model`, and `confirm`.

`enter`, `alt+enter`, `esc`, and `ctrl+c` are protected. The event contains:

```lua
{
  selection = { value = "...", label = "...", description = "..." } or nil,
  tool_exchange = { call = ..., result = ... } or nil,
  working_dir = "...",
}
```

With a `Keybinding` process declaration, `ctx.process.spawn(program, argv)` requests a sandboxed, fire-and-forget process effect. It returns no process output.

## Process API

Hooks and commands with an exact matching declaration receive:

```lua
local result = ctx.process.run("git", { "status", "--short" }, {
  timeout_ms = 1000, -- optional; default 2000
})
```

Result fields:

```lua
{
  success = true,
  exit_code = 0,       -- may be nil
  stdout = "...",
  stderr = "...",
  truncated = false,
}
```

Arguments are argv, not shell text; no shell interpolation occurs. Wrap calls with `pcall` when denial, timeout, or process failure should not fail the whole callback. A capability cannot be saved and used after its callback returns.

## Lifecycle events

Use these exact event names and payload fields:

| Event | Payload |
|---|---|
| `resources_discover` | `cwd`, `reason` |
| `SessionStart` | `source`, `reason`, `previous_session_file` |
| `SessionEnd` | `reason`, `target_session_file` |
| `session_tree` | `reason`, `target_id`, `old_leaf_id`, `new_leaf_id`, `editor_text` |
| `BuildSystemPrompt` | `system_prompt`, `cwd` |
| `AgentStart`, `AgentEnd` | none |
| `AgentTurnStart`, `AgentTurnEnd` | `turn_id`, `index` |
| `MessageDisplay` | `turn_id`, `message_id`, `index`, `final`, `delta` |
| `UserPromptSubmit` | `original_prompt`, `prompt`, `source`, `dispatch` (`immediate`, `steer`, or `follow_up`) |
| `PromptContext` | `original_prompt`, `prompt`, `source`, `dispatch`, `transcript`, `working_dir`, `delegated_child` |
| `context` | `turn_id` |
| `BeforeProviderRequest`, `AfterProviderResponse` | `turn_id` |
| `PreToolUse` | `tool_name`, `tool_use_id`, `tool_input` |
| `PermissionRequest`, `PermissionDenied` | `tool_name`, `tool_input` |
| `PostToolUse` | `tool_name`, `tool_use_id`, `tool_input`, `tool_response`, `duration_ms` |
| `PostToolUseFailure` | `tool_name`, `tool_use_id`, `tool_input`, `error`, `is_interrupt`, `duration_ms` |
| `PostToolBatch` | `tool_calls` |
| `TaskCreated` | `context`, `task` |
| `TaskCompleted` | `context`, `task`, `trigger` (`explicit_update` or `teammate_end_turn`) |
| `TeammateIdle` | `context` |
| `Stop` | `response`, `continuations`, `stop_hook_active` |
| `StopFailure` | `error` |
| `PreCompact` | `message_count` |
| `PostCompact` | `message_count`, `summary` |
| `Notification` | `notification_type`, `level`, `message`, optional `tool_name` |
| `command_before` | `command` |
| `command_after` | `command`, `handled` |
| `ui_render` | none |

`UserPromptSubmit.original_prompt` preserves the submitted text; `prompt` reflects preceding transforms. Original text from Internal/Extension sources is not human approval. Lua returns can transform prompt/context, not contribute tool policy.

Prepared lifecycle registrations share the SDK Hook layer with Rust callbacks and settings commands. Each prepared Lua callback has its own lifecycle timeout, defaulting to 10 seconds. `Stop` gates candidate completion; feedback uses bounded continuation, not a second end callback. Notifications observe permission prompts, accepted completion and explicit messages asynchronously; notification process capabilities cannot request permission.

Impactful return shapes:

```lua
-- Transform or handle accepted user input.
return {
  action = "transform",
  prompt = "...",
  injected_context = {{ source = "hook", content = "..." }},
}
return { action = "handled", reason = "..." }

-- Contribute after all prompt transformations (PromptContext).
return { contexts = {{ source = "hook", content = "..." }}, notices = {} }

-- Reject TaskCreated/TaskCompleted; nil allows the operation.
return { block = true, reason = "Narrow the task scope." }

-- TeammateIdle accepts only these action/feedback shapes; nil also allows.
return { action = "allow" }
return { action = "continue", feedback = "Check the remaining tasks." }
return { action = "stop" }

-- Ask for another completion attempt from Stop.
return { block = true, reason = "Verify the remaining work." }

-- Replace the system prompt.
return { system_prompt = "..." }

-- Rewrite/block a tool and add model context.
return {
  updated_input = { ... },
  permission_decision = "deny", -- allow, ask, defer, deny
  reason = "...",
  additional_context = "...",  -- string or array
}

-- Rewrite a successful result or failure and add context.
return { updated_output = ..., additional_context = "..." }

-- Add context after a tool batch.
return { additional_context = { "..." } }

-- Replace rendered assistant text.
return { display_content = "..." }

-- Add provider context from `context`.
return { "first context block", "second context block" }
```

For `PreToolUse`, later hooks see earlier `updated_input` values. Phi evaluates both original and rewritten Bash input under permission policy. Ordinary hooks cannot safely bypass permission; use `block = true` or a deny decision to reject execution.
