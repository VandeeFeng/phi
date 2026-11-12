---
name: extension-builder
description: Build, test, install, and reload Phi Lua extensions. Use when creating a Phi extension, Lua lifecycle hook, slash command, agent tool, tool display, keybinding, or process-backed extension.
---

# Phi Lua Extension Builder

Read [references/api.md](references/api.md) before writing an extension. Use only APIs documented there.

## Workflow

1. Before designing or writing files, ask one grouped round of 3–5 questions covering the most relevant unresolved details:
   - What exact behavior and success example does the user expect?
   - What should trigger it: lifecycle event, slash command, agent tool, display, or keybinding?
   - What inputs, outputs, errors, and session state should it handle?
   - Does it need external processes? If so, which exact executables and operations?
   - Should it be global or project-specific?

   Tailor the questions to the request; do not ask irrelevant or duplicate questions. Wait for the answers before continuing.
2. Propose a descriptive lowercase kebab-case plugin name based on the answers, or restate the user's requested name. Explicitly ask the user to confirm it. Do not create the candidate until the name is confirmed.
3. Choose the smallest matching extension surface and create one candidate at `/tmp/<confirmed-plugin-name>.lua`. Never write the first version directly into a loaded extension directory.
4. Keep all state local to the file. Set `register_extension.name` to the confirmed plugin name; declare only exact process executables and callback contexts that are required.
5. Run:

   ```bash
   lua scripts/test-extension.lua /tmp/<confirmed-plugin-name>.lua
   ```

6. Fix every syntax, load, registration, or schema error and rerun the test. Do not install a failing candidate.
7. Install by moving the tested file:

   ```bash
   scripts/install-extension.sh /tmp/<confirmed-plugin-name>.lua global
   ```

   Use `project` instead of `global` only for repository-specific extensions that need no process capability. Global installs target `${PHI_CONFIG_DIR:-$HOME/.phi}/extensions`; project installs target `$PWD/.phi/extensions`.

8. Tell the user to submit `/reload` in the running Phi session. `/reload` is a Phi UI command, not a shell command. Confirm success by invoking the new slash command/keybinding/tool or triggering its hook.

## Base format

```lua
register_extension({ name = "example" })

local state = {}

on("SessionStart", function(event, ctx)
  state.started_for = event.payload.reason
  ctx.ui.notify("example loaded")
end)

register_command("example", {
  description = "show example status",
  handler = function(args, ctx)
    if not ctx.require_idle("show example status") then return end
    ctx.ui.notify(args ~= "" and args or state.started_for or "ready")
  end,
})
```

Remove unused capabilities and registrations. Do not use `io`, `os`, `package`, `require`, `dofile`, or `loadfile`; Phi intentionally omits them.

## Safety rules

- Prefer hooks or new tools over overriding built-ins.
- Never set `override = true` unless the user explicitly requests complete replacement behavior.
- Pass process arguments as an argv table; never build shell command strings.
- Use a global extension for process execution. Project-controlled `.phi` extensions are not trusted for process capability.
- Treat process denial and failure as expected when appropriate; use `pcall` and inspect `success`, `stderr`, and `truncated`.
- Use `ctx.require_idle` for commands that mutate session/application state.
- Do not claim reload succeeded until the user runs `/reload` without an extension reload error. Failed reload preserves the previous active runtime.
