-- Official phi Lua extension example: custom slash command.
-- Copy this file to .phi/extensions/custom-slash-command.lua and run phi.

register_command("hello", {
  description = "example Lua slash command",
  handler = function(args, ctx)
    local name = args ~= "" and args or "from Lua"
    ctx.ui.notify("hello " .. name)
  end,
})
