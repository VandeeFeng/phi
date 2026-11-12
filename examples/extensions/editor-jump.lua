-- Open the Write/Edit entry selected in /rewind with Emacsclient.
-- Copy this file to ~/.phi/extensions/editor-jump.lua; emacsclient uses normal command permissions.

register_extension({
  name = "editor-jump",
  capabilities = {
    process = {
      Keybinding = { "emacsclient" },
    },
  },
})

local function absolute_path(path, working_dir)
  if path:match("^/") or path:match("^%a:[/\\]") then
    return path
  end
  if not working_dir or working_dir == "" then
    return path
  end
  return working_dir:gsub("[/\\]+$", "") .. "/" .. path
end

register_keybinding("ctrl+e", {
  context = "picker.rewind",
  handler = function(event, ctx)
    if not event.selection then
      ctx.ui.notify("no rewind entry selected")
      return
    end
    local exchange = event.tool_exchange
    if not exchange then
      ctx.ui.notify("selected rewind entry is not a tool result")
      return
    end

    local tool_call = exchange.call
    if tool_call.name ~= "Edit" and tool_call.name ~= "Write" then
      ctx.ui.notify("selected rewind entry is not an Edit or Write")
      return
    end

    local path = tool_call.args and (tool_call.args.path or tool_call.args.file_path)
    if not path then
      ctx.ui.notify("selected tool call has no file path")
      return
    end

    local details = exchange.result.details
    local diff = details and details.diff or nil
    local line = diff and tonumber(diff.first_changed_line) or 1
    path = absolute_path(path, event.working_dir)
    ctx.process.spawn("emacsclient", { "-n", "+" .. line .. ":1", "--", path })
    ctx.ui.notify("opened " .. path .. ":" .. line)
  end,
})
