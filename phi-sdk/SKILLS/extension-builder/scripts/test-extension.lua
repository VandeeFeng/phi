local candidate = assert(arg[1], "usage: test-extension.lua /tmp/<extension>.lua")
assert(candidate:match("^/tmp/.+%.lua$") and not candidate:match("/%.%./"),
  "extension candidate must be a .lua file under /tmp: " .. candidate)

local registrations = {
  commands = {},
  hooks = {},
  keybindings = {},
  tool_displays = {},
  tools = {},
}
local current_extension = candidate

local function expect(value, expected, field)
  assert(type(value) == expected, field .. " must be " .. expected)
end

local function expect_handler(spec)
  expect(spec, "table", "registration")
  expect(spec.handler, "function", "handler")
end

local function canonical_tool_name(name)
  local names = {
    read = "Read", Read = "Read", write = "Write", Write = "Write",
    edit = "Edit", Edit = "Edit", bash = "Bash", Bash = "Bash",
    grep = "Grep", Grep = "Grep", glob = "Glob", Glob = "Glob",
    agent = "Agent", Agent = "Agent", mcp = "MCP", MCP = "MCP",
  }
  return names[name] or name
end

local environment = {
  _VERSION = _VERSION,
  assert = assert,
  coroutine = coroutine,
  error = error,
  getmetatable = getmetatable,
  ipairs = ipairs,
  math = math,
  next = next,
  pairs = pairs,
  pcall = pcall,
  rawequal = rawequal,
  rawget = rawget,
  rawlen = rawlen,
  rawset = rawset,
  select = select,
  setmetatable = setmetatable,
  string = string,
  table = table,
  tonumber = tonumber,
  tostring = tostring,
  type = type,
  utf8 = utf8,
  xpcall = xpcall,
}
environment._G = environment

environment.register_extension = function(spec)
  expect(spec, "table", "extension")
  expect(spec.name, "string", "extension.name")
  current_extension = spec.name
  local process = spec.capabilities and spec.capabilities.process
  if process ~= nil then
    expect(process, "table", "capabilities.process")
    for context, programs in pairs(process) do
      expect(context, "string", "process context")
      expect(programs, "table", "process programs")
      for _, program in ipairs(programs) do
        expect(program, "string", "process program")
      end
    end
  end
end

environment.on = function(event, handler)
  expect(event, "string", "event")
  expect(handler, "function", "handler")
  registrations.hooks[event] = (registrations.hooks[event] or 0) + 1
end

environment.register_command = function(name, spec)
  expect(name, "string", "command name")
  expect_handler(spec)
  if spec.description ~= nil then
    expect(spec.description, "string", "command description")
  end
  registrations.commands[name] = current_extension
end

environment.register_keybinding = function(key, spec)
  expect(key, "string", "keybinding key")
  expect_handler(spec)
  expect(spec.context, "string", "keybinding context")
  table.insert(registrations.keybindings, key:lower())
end

environment.register_tool_display = function(name, handler)
  expect(name, "string", "tool display name")
  expect(handler, "function", "tool display handler")
  registrations.tool_displays[name] = current_extension
end

environment.register_tool = function(name, spec)
  expect(name, "string", "tool name")
  expect_handler(spec)
  expect(spec.description, "string", "tool description")
  if spec.parameters ~= nil then
    expect(spec.parameters, "table", "tool parameters")
    assert(spec.parameters.type == "object", "tool parameters must declare type = 'object'")
    if spec.parameters.properties ~= nil then
      expect(spec.parameters.properties, "table", "tool parameter properties")
    end
    if spec.parameters.required ~= nil then
      expect(spec.parameters.required, "table", "tool required")
      for _, field in ipairs(spec.parameters.required) do
        expect(field, "string", "required field")
      end
    end
  end
  local canonical = canonical_tool_name(name)
  assert(not registrations.tools[canonical] or spec.override == true,
    "tool '" .. canonical .. "' is already registered; set override = true to replace it")
  registrations.tools[canonical] = current_extension
end

local chunk = assert(loadfile(candidate, "t", environment))
chunk()

local hook_count = 0
for _, count in pairs(registrations.hooks) do hook_count = hook_count + count end
local function count(values)
  local total = 0
  for _ in pairs(values) do total = total + 1 end
  return total
end

print(string.format(
  "extension load test passed: %d hook(s), %d command(s), %d keybinding(s), %d display(s), %d tool(s)",
  hook_count,
  count(registrations.commands),
  #registrations.keybindings,
  count(registrations.tool_displays),
  count(registrations.tools)
))
