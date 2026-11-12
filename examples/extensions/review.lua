-- Inspired by https://github.com/mitsuhiko/agent-stuff/blob/main/extensions/review.ts
register_extension({
  name = "review",
  capabilities = { process = { Command = { "git", "gh" } } },
})

local REVIEW_RUBRIC = [=[
# Review Guidelines

You are acting as a code reviewer for a proposed code change made by another engineer.

Below are default guidelines for determining what to flag. These are not the final word — if you encounter more specific guidelines elsewhere (in a developer message, user message, file, or project review guidelines appended below), those override these general instructions.

## Determining what to flag

Flag issues that:
1. Meaningfully impact the accuracy, performance, security, or maintainability of the code.
2. Are discrete and actionable (not general issues or multiple combined issues).
3. Don't demand rigor inconsistent with the rest of the codebase.
4. Were introduced in the changes being reviewed (not pre-existing bugs).
5. The author would likely fix if aware of them.
6. Don't rely on unstated assumptions about the codebase or author's intent.
7. Have provable impact on other parts of the code — it is not enough to speculate that a change may disrupt another part, you must identify the parts that are provably affected.
8. Are clearly not intentional changes by the author.
9. Be particularly careful with untrusted user input and follow the specific guidelines to review.
10. Treat silent local error recovery (especially parsing/IO/network fallbacks) as high-signal review candidates unless there is explicit boundary-level justification.

## Untrusted User Input

1. Be careful with open redirects, they must always be checked to only go to trusted domains (?next_page=...)
2. Always flag SQL that is not parametrized
3. In systems with user supplied URL input, http fetches always need to be protected against access to local resources (intercept DNS resolver!)
4. Escape, don't sanitize if you have the option (eg: HTML escaping)

## Comment guidelines

1. Be clear about why the issue is a problem.
2. Communicate severity appropriately - don't exaggerate.
3. Be brief - at most 1 paragraph.
4. Keep code snippets under 3 lines, wrapped in inline code or code blocks.
5. Use ```suggestion blocks ONLY for concrete replacement code (minimal lines; no commentary inside the block). Preserve the exact leading whitespace of the replaced lines.
6. Explicitly state scenarios/environments where the issue arises.
7. Use a matter-of-fact tone - helpful AI assistant, not accusatory.
8. Write for quick comprehension without close reading.
9. Avoid excessive flattery or unhelpful phrases like "Great job...".

## Review priorities

1. Surface critical non-blocking human callouts (migrations, dependency churn, auth/permissions, compatibility, destructive operations) at the end.
2. Prefer simple, direct solutions over wrappers or abstractions without clear value.
3. Treat back pressure handling as critical to system stability.
4. Apply system-level thinking; flag changes that increase operational risk or on-call wakeups.
5. Ensure that errors are always checked against codes or stable identifiers, never error messages.

## Fail-fast error handling (strict)

When reviewing added or modified error handling, default to fail-fast behavior.

1. Evaluate every new or changed `try/catch`: identify what can fail and why local handling is correct at that exact layer.
2. Prefer propagation over local recovery. If the current scope cannot fully recover while preserving correctness, rethrow (optionally with context) instead of returning fallbacks.
3. Flag catch blocks that hide failure signals (e.g. returning `null`/`[]`/`false`, swallowing JSON parse failures, logging-and-continue, or “best effort” silent recovery).
4. JSON parsing/decoding should fail loudly by default. Quiet fallback parsing is only acceptable with an explicit compatibility requirement and clear tested behavior.
5. Boundary handlers (HTTP routes, CLI entrypoints, supervisors) may translate errors, but must not pretend success or silently degrade.
6. If a catch exists only to satisfy lint/style without real handling, treat it as a bug.
7. When uncertain, prefer crashing fast over silent degradation.

## Required human callouts (non-blocking, at the very end)

After findings/verdict, you MUST append this final section:

## Human Reviewer Callouts (Non-Blocking)

Include only applicable callouts (no yes/no lines):

- **This change adds a database migration:** <files/details>
- **This change introduces a new dependency:** <package(s)/details>
- **This change changes a dependency (or the lockfile):** <files/package(s)/details>
- **This change modifies auth/permission behavior:** <what changed and where>
- **This change introduces backwards-incompatible public schema/API/contract changes:** <what changed and where>
- **This change includes irreversible or destructive operations:** <operation and scope>

Rules for this section:
1. These are informational callouts for the human reviewer, not fix items.
2. Do not include them in Findings unless there is an independent defect.
3. These callouts alone must not change the verdict.
4. Only include callouts that apply to the reviewed change.
5. Keep each emitted callout bold exactly as written.
6. If none apply, write "- (none)".

## Priority levels

Tag each finding with a priority level in the title:
- [P0] - Drop everything to fix. Blocking release/operations. Only for universal issues that do not depend on assumptions about inputs.
- [P1] - Urgent. Should be addressed in the next cycle.
- [P2] - Normal. To be fixed eventually.
- [P3] - Low. Nice to have.

## Output format

Provide your findings in a clear, structured format:
1. List each finding with its priority tag, file location, and explanation.
2. Findings must reference locations that overlap with the actual diff — don't flag pre-existing code.
3. Keep line references as short as possible (avoid ranges over 5-10 lines; pick the most suitable subrange).
4. Provide an overall verdict: "correct" (no blocking issues) or "needs attention" (has blocking issues).
5. Ignore trivial style issues unless they obscure meaning or violate documented standards.
6. Do not generate a full PR fix — only flag issues and optionally provide short suggestion blocks.
7. End with the required "Human Reviewer Callouts (Non-Blocking)" section and all applicable bold callouts (no yes/no).

Output all findings the author would fix if they knew about them. If there are no qualifying findings, explicitly state the code looks good. Don't stop at the first finding - list every qualifying issue. Then append the required non-blocking callouts section.
]=]

local function trim(value)
  return (value or ""):match("^%s*(.-)%s*$")
end

local function tokenize(value)
  local tokens, current, quote = {}, "", nil
  local index = 1
  while index <= #value do
    local char = value:sub(index, index)
    if quote and char == "\\" and index < #value then
      index = index + 1
      current = current .. value:sub(index, index)
    elseif quote and char == quote then
      quote = nil
    elseif quote then
      current = current .. char
    elseif char == '"' or char == "'" then
      quote = char
    elseif char:match("%s") then
      if current ~= "" then tokens[#tokens + 1], current = current, "" end
    else
      current = current .. char
    end
    index = index + 1
  end
  if quote then error("Unclosed quote", 0) end
  if current ~= "" then tokens[#tokens + 1] = current end
  return tokens
end

local function parse_args(value)
  local tokens, parts, extra = tokenize(value), {}, nil
  local index = 1
  while index <= #tokens do
    if tokens[index] == "--extra" then
      index = index + 1
      extra = tokens[index]
      if not extra or trim(extra) == "" then error("Missing value for --extra", 0) end
    elseif tokens[index]:sub(1, 8) == "--extra=" then
      extra = tokens[index]:sub(9)
      if trim(extra) == "" then error("Missing value for --extra", 0) end
    else
      parts[#parts + 1] = tokens[index]
    end
    index = index + 1
  end
  return parts, extra
end

local function process(ctx, program, args, optional)
  local result = ctx.process.run(program, args)
  if result.truncated then error(program .. " output was truncated", 0) end
  if not result.success then
    if optional then return nil end
    error(program .. " failed: " .. trim(result.stderr or result.stdout), 0)
  end
  return trim(result.stdout)
end

local function git(ctx, args, optional)
  return process(ctx, "git", args, optional)
end

local function resolve_commit(ctx, reference)
  if reference:sub(1, 1) == "-" then error("Invalid Git reference: " .. reference, 0) end
  return git(ctx, { "rev-parse", "--verify", "--end-of-options", reference .. "^{commit}" })
end

local function default_branch(ctx)
  local remote = git(ctx, { "symbolic-ref", "refs/remotes/origin/HEAD", "--short" }, true)
  if remote and remote ~= "" then return remote:gsub("^origin/", "") end
  local branches = "\n" .. git(ctx, { "branch", "--format=%(refname:short)" }) .. "\n"
  if branches:find("\nmaster\n", 1, true) and not branches:find("\nmain\n", 1, true) then
    return "master"
  end
  return "main"
end

local function smart_target(ctx)
  if git(ctx, { "status", "--porcelain" }) ~= "" then return { "uncommitted" } end
  local current = git(ctx, { "branch", "--show-current" })
  local base = default_branch(ctx)
  if current ~= "" and current ~= base then return { "branch", base } end
  return { "commit", "HEAD" }
end

local function branch_prompt(ctx, branch)
  local resolved = resolve_commit(ctx, branch)
  local upstream = git(ctx, {
    "rev-parse", "--verify", "--end-of-options", branch .. "@{upstream}^{commit}",
  }, true)
  local merge_base = git(ctx, { "merge-base", "HEAD", upstream or resolved })
  return "Review the code changes against the base branch '" .. branch .. "'. " ..
    "The merge base commit for this comparison is " .. merge_base .. ". " ..
    "Run `git diff " .. merge_base .. "` to inspect the changes. Provide prioritized, actionable findings."
end

local function check_clean(ctx)
  for line in git(ctx, { "status", "--porcelain" }):gmatch("[^\n]+") do
    if line:sub(1, 2) ~= "??" then
      error("Cannot checkout PR: commit or stash tracked working-tree changes first.", 0)
    end
  end
end

local function pr_prompt(ctx, reference)
  local number = reference:match("^([1-9]%d*)$") or
    reference:match("^https://github%.com/[^/]+/[^/]+/pull/([1-9]%d*)/?$") or
    reference:match("^github%.com/[^/]+/[^/]+/pull/([1-9]%d*)/?$")
  if not number then error("Invalid PR reference: use a number or GitHub PR URL.", 0) end
  if reference:match("^github%.") then reference = "https://" .. reference end
  check_clean(ctx)
  local info = process(ctx, "gh", {
    "pr", "view", reference, "--json", "baseRefName,title", "--template", "{{.baseRefName}}\n{{.title}}",
  })
  local base, title = info:match("^([^\n]+)\n(.+)$")
  if not base then error("Invalid PR metadata from gh", 0) end
  check_clean(ctx)
  process(ctx, "gh", { "pr", "checkout", reference })
  return 'Review pull request #' .. number .. ' ("' .. title .. '"). ' .. branch_prompt(ctx, base)
end

local function target_prompt(ctx, parts)
  local mode = parts[1]:lower()
  if mode == "uncommitted" and #parts == 1 then
    return "Review the current code changes (staged, unstaged, and untracked files) and provide prioritized findings. " ..
      "Use `git status --porcelain`, `git diff`, `git diff --staged`, and `git ls-files --others --exclude-standard`."
  end
  if mode == "branch" and #parts == 2 then return branch_prompt(ctx, parts[2]) end
  if mode == "pr" and #parts == 2 then return pr_prompt(ctx, parts[2]) end
  if mode == "commit" and #parts >= 2 then
    local sha = resolve_commit(ctx, parts[2])
    local title = table.concat(parts, " ", 3)
    return "Review the code changes introduced by commit " .. sha .. " (" .. title .. "). " ..
      "Provide prioritized, actionable findings."
  end
  if mode == "folder" and #parts >= 2 then
    local paths = {}
    for index = 2, #parts do paths[#paths + 1] = string.format("%q", parts[index]) end
    return "Review the code in the following paths: " .. table.concat(paths, ", ") .. ". " ..
      "This is a snapshot review (not a diff). Read files directly. Flag existing actionable defects in this scope; " ..
      "the rubric's introduced-in-diff and diff-location restrictions do not apply."
  end
  error("Usage: /review [uncommitted | branch <base> | commit <sha> | pr <number-or-url> | folder <paths...>] [--extra \"instructions\"]", 0)
end

local function review(ctx, args)
  local parts, extra = parse_args(args)
  git(ctx, { "rev-parse", "--git-dir" })
  if #parts == 0 then parts = smart_target(ctx) end
  local prompt = REVIEW_RUBRIC .. "\n\n---\n\nPlease perform a code review with the following focus:\n\n" ..
    target_prompt(ctx, parts)
  if extra then prompt = prompt .. "\n\nAdditional user-provided review instruction:\n\n" .. extra end
  return prompt .. "\n\nLook for REVIEW_GUIDELINES.md beside the nearest ancestor .phi or .pi directory. " ..
    "If present, read it and apply its project-specific review instructions. Do not read .env files. " ..
    "Only review; do not modify files or implement fixes."
end

register_command("review", {
  description = "Review uncommitted changes, a branch, commit, PR, or folder",
  handler = function(args, ctx)
    if not ctx.require_idle("start review") then return end
    ctx.agent.send_user_message(review(ctx, args))
  end,
})
