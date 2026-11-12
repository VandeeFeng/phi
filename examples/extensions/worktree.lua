-- Example phi Lua extension: git worktree commands.
-- Copy this file to ~/.phi/extensions/worktree.lua; process calls use normal command permissions.

register_extension({
  name = "worktree",
  capabilities = {
    process = {
      Command = { "git" },
    },
  },
})

local function trim(value)
  return (value:gsub("^%s+", ""):gsub("%s+$", ""))
end

local function run_git(ctx, args)
  local ok, result = pcall(ctx.process.run, "git", args)
  if not ok then
    ctx.ui.notify("git command failed: " .. result)
    return
  end
  local output = trim(result.success and result.stdout or result.stderr)
  if output == "" then
    output = result.success and "done" or "git command failed"
  end
  ctx.ui.notify(output)
end

register_command("worktree", {
  description = "show git worktree status or run `worktree new <branch>`",
  handler = function(args, ctx)
    args = trim(args)
    local branch = args:match("^new%s+(.+)$")
    if branch ~= nil then
      run_git(ctx, { "worktree", "add", "-b", branch, "../" .. branch })
      return
    end
    run_git(ctx, { "worktree", "list" })
  end,
})

register_command("worktree-new", {
  description = "create a git worktree branch",
  handler = function(args, ctx)
    local branch = trim(args)
    if branch == "" then
      ctx.ui.notify("usage: /worktree-new <branch-name>")
      return
    end
    run_git(ctx, { "worktree", "add", "-b", branch, "../" .. branch })
  end,
})
