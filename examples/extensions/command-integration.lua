-- Example phi Lua extension: RTK + UV command integration.
-- Copy this file to ~/.phi/extensions/command-integration.lua; process calls use normal command permissions.
--
-- phi runs the built-in Bash tool through Rust (`bash -c <command>`, or bwrap + bash
-- when sandboxing is enabled). This extension therefore rewrites Bash tool input in
-- the PreToolUse lifecycle hook instead of replacing Phi's built-in Bash tool.
-- Phi authorizes both the original command and the rewritten result, so an RTK
-- transport cannot hide a configured rule such as `git -C ** push **`.

register_extension({
  name = "command-integration",
  capabilities = {
    process = {
      SessionStart = { "rtk", "uv" },
      PreToolUse = { "rtk" },
      Command = { "rtk" },
    },
  },
})

local stats = {}
local rtk_available = false
local uv_available = false

local pip_message = "Error: pip is disabled. Use uv instead:\n\n" ..
  "  To install a package for a script: uv run --with PACKAGE python script.py\n" ..
  "  To add a dependency to the project: uv add PACKAGE\n"

local venv_message = "Error: 'python -m venv' is disabled. Use uv instead:\n\n" ..
  "  To create a virtual environment: uv venv\n"

local function trim(value)
  return (value or ""):gsub("^%s+", ""):gsub("%s+$", "")
end

local function shell_quote(value)
  return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
end

local function command_output(ctx, program, args)
  local ok, result = pcall(ctx.process.run, program, args)
  if not ok or not result.success then
    return nil
  end
  return trim(result.stdout)
end

local function tool_available(ctx, tool)
  local ok, result = pcall(ctx.process.run, tool, { "--version" })
  return ok and result.success
end

local function uv_shell_prefix()
  return [[
resolve_uv_python() {
  local candidate

  candidate="$(uv python find --managed-python 2>/dev/null || true)"
  if [ -n "$candidate" ] && [ -x "$candidate" ]; then
    echo "$candidate"
    return 0
  fi

  while IFS= read -r candidate; do
    [ -n "$candidate" ] || continue
    echo "$candidate"
    return 0
  done < <(type -aP python3 2>/dev/null)

  while IFS= read -r candidate; do
    [ -n "$candidate" ] || continue
    echo "$candidate"
    return 0
  done < <(type -aP python 2>/dev/null)

  return 1
}

python() {
  if [ "$1" = "-m" ]; then
    case "$2" in
      pip|pip*) echo ]] .. shell_quote(pip_message) .. [[ >&2; return 1 ;;
      venv|venv*) echo ]] .. shell_quote(venv_message) .. [[ >&2; return 1 ;;
      py_compile|py_compile*) echo "Error: 'python -m py_compile' is disabled because it writes .pyc files to __pycache__." >&2; return 1 ;;
    esac
  fi

  case "$1" in
    -mpip*|-m\pip) echo ]] .. shell_quote(pip_message) .. [[ >&2; return 1 ;;
    -mvenv*|-m\venv) echo ]] .. shell_quote(venv_message) .. [[ >&2; return 1 ;;
    -mpy_compile*|-m\py_compile) echo "Error: 'python -m py_compile' is disabled because it writes .pyc files to __pycache__." >&2; return 1 ;;
  esac

  local uv_python
  uv_python="$(resolve_uv_python)"
  if [ -z "$uv_python" ]; then
    echo "Error: Unable to locate a Python interpreter outside shim directories." >&2
    return 1
  fi

  uv run --python "$uv_python" python "$@"
}
python3() { python "$@"; }
pip() { echo ]] .. shell_quote(pip_message) .. [[ >&2; return 1; }
pip3() { pip "$@"; }
poetry() { echo 'Error: Use uv instead of poetry (uv init, uv add, uv sync, uv run)' >&2; return 1; }
export -f resolve_uv_python python python3 pip pip3 poetry
]]
end

local function command_tokens(command)
  local tokens = {}
  local i = 1
  local at_command = true
  local quote = nil

  while i <= #command do
    local char = command:sub(i, i)
    if quote ~= nil then
      if char == quote then
        quote = nil
      elseif char == "\\" and quote == '"' then
        i = i + 1
      end
    elseif char == "'" or char == '"' then
      quote = char
    elseif char:match("%s") then
      -- keep current command position
    elseif char == ";" or char == "&" or char == "|" or char == "(" or char == "\n" then
      at_command = true
    else
      local token = command:match("^[%w_%-%.]+", i)
      if token ~= nil then
        if at_command then
          table.insert(tokens, token)
        end
        at_command = false
        i = i + #token - 1
      else
        at_command = false
      end
    end
    i = i + 1
  end

  return tokens
end

local function should_use_uv_shim(command)
  for _, token in ipairs(command_tokens(command)) do
    if token == "python" or token == "python3" or token == "pip" or token == "pip3" or token == "poetry" then
      return true
    end
  end
  return false
end

local function uv_display_command(command)
  local trimmed = trim(command)
  local _, user_command = trimmed:match("^(.*export %-f resolve_uv_python python python3 pip pip3 poetry)%s*(.*)$")
  if user_command ~= nil then
    trimmed = trim(user_command)
  end

  local rest = trimmed:match("^python3?%s+(.+)$")
  if rest ~= nil then
    return "uv run python " .. rest
  end

  rest = trimmed:match("^pip3?%s+(.+)$")
  if rest ~= nil then
    return "uv add/run --with " .. rest
  end

  rest = trimmed:match("^poetry%s+(.+)$")
  if rest ~= nil then
    return "uv " .. rest
  end

  return "uv shim: redirected python/pip/poetry command"
end

local function rewrite_command(ctx, command)
  local original = trim(command)
  if original == "" or not rtk_available or original == "rtk" or original:find("^rtk%s") then
    return original
  end
  if original:find("%f[%w]find%f[%W]") or original:find("<<", 1, true) or original:find("$((", 1, true) then
    return original
  end

  local rewritten = command_output(ctx, "rtk", { "rewrite", original })
  if rewritten ~= nil and rewritten ~= "" and rewritten ~= original then
    return rewritten
  end
  return original
end

on("SessionStart", function(_event, ctx)
  rtk_available = tool_available(ctx, "rtk")
  uv_available = tool_available(ctx, "uv")
end)

on("PreToolUse", function(event, ctx)
  local tool_name = event.payload.tool_name or ""
  if tool_name ~= "Bash" and tool_name ~= "bash" then
    return nil
  end

  local input = event.payload.tool_input
  if type(input) ~= "table" or type(input.command) ~= "string" then
    return nil
  end

  local original = trim(input.command)
  local rewritten = rewrite_command(ctx, original)
  if rewritten ~= original then
    local base = original:match("^(%S+)")
    if base ~= nil then
      stats[base] = (stats[base] or 0) + 1
    end
  end

  if uv_available and should_use_uv_shim(rewritten) then
    rewritten = uv_shell_prefix() .. "\n" .. rewritten
  end

  if rewritten == input.command then
    return nil
  end

  input.command = rewritten
  return { updated_input = input }
end)

register_tool_display("Bash", function(input, _ctx)
  if type(input) ~= "table" or type(input.command) ~= "string" then
    return nil
  end
  if input.command:find("resolve_uv_python", 1, true) == nil then
    return nil
  end
  return { title = "Bash " .. uv_display_command(input.command) }
end)

register_command("rtk-stats", {
  description = "show RTK command rewrite statistics",
  handler = function(_args, ctx)
    local lines = {
      "📊 RTK Session Statistics",
      "──────────────────────────────────────────────────",
    }

    if not rtk_available then
      table.insert(lines, "RTK is not installed or not available in PATH")
      table.insert(lines, "")
      table.insert(lines, "Install RTK: https://github.com/rtk-ai/rtk")
      ctx.ui.notify(table.concat(lines, "\n"))
      return
    end

    local total = 0
    for _, count in pairs(stats) do
      total = total + count
    end

    if total == 0 then
      table.insert(lines, "(No commands rewritten yet in this session)")
    else
      table.insert(lines, "Commands rewritten this session: " .. total)
      table.insert(lines, "")
      table.insert(lines, "Top commands:")
      for command, count in pairs(stats) do
        table.insert(lines, "  " .. command .. ": " .. count)
      end
    end

    local global = command_output(ctx, "rtk", { "gain" })
    if global ~= nil and global ~= "" then
      table.insert(lines, "")
      table.insert(lines, "📈 Global Statistics")
      table.insert(lines, "──────────────────────────────────────────────────")
      table.insert(lines, global)
    end

    ctx.ui.notify(table.concat(lines, "\n"))
  end,
})
