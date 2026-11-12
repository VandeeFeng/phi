# phi-interactive

`phi-interactive` is the application crate behind the `phi` CLI. It provides interactive sessions, print execution, Session management, and machine control through stdin/stdout JSONL on top of [`phi-sdk`](../phi-sdk/) and [`phi-tui`](../phi-tui/). Install Phi and configure a provider using the [main README](../README.md#get-started).

## What the application does

- **Interactive CLI:** work in the current directory with one Agent, saved Sessions, model and switch selection, and interactive permission requests.
- **Machine control:** run `phi --rpc` as a foreground subprocess and exchange structured requests, responses and events over stdin/stdout.
- **Non-interactive runs:** print a response for one prompt.
- **Resources:** load project instructions, skills, custom prompts and Agents, optional MCP servers and Lua extensions, and configured Memory/Tape capabilities.

## Run modes

| Command | Behavior |
|---|---|
| `phi` / `phi "prompt"` | Open a standalone interactive session, optionally with an initial prompt. |
| `phi -p "prompt"` | Run one prompt, print the response, and exit. |
| `phi --rpc` | Run the machine-control protocol over stdin/stdout JSONL. |
| `phi list` | List up to ten recent saved Sessions in the current working directory. |
| `phi -c` / `phi -r` | Continue the most recent saved Session or choose one from the resume picker. |
| `phi --session ID` | Resume a specific local Session by ID or file path. |
| `phi -p --no-session "prompt"` | Print one response without saving the Session. |
| `phi --log-file /path/to/diagnostics.log` | Append tracing diagnostics to a file, outside the conversation and RPC stdout. |

Quote multi-word prompts so the shell passes them as one argument, e.g. `phi "Explain this repository"`. Single-word prompts need no quotes. For a prompt matching a subcommand, use `phi -- "list"`: quotes alone are removed by the shell before clap parses arguments.

Interactive, print and RPC modes use the same concrete preparation and Session persistence capabilities. The foreground process owns finalization; no shared client/server facade is required. Tracing is discarded unless `--log-file` is set; `RUST_LOG` controls its level (default: `warn,rig=off,rmcp=off`). Hook reports and explicit extension notifications remain separate from tracing: interactive shows notifications, print writes warnings and extension messages to stderr, and RPC emits `hook_notice` / `extension_notification` notices. For additional flags, use `phi --help`.

## Machine control

`phi --rpc` reserves stdout for protocol JSONL and uses stdin for requests and interaction replies. It starts without an Agent; create or open a Session through the protocol. Runtime flags such as `--model`, `--provider`, and permission options configure startup policy. RPC cannot be combined with print, positional prompts, CLI Session selectors, or subcommands.

There is no Dashboard, detached startup, `serve`, or cross-process `attach`. RPC uses strict version-2 tagged JSONL: initialize one new/saved root, send inputs, consume admissions/events/outcomes and answer permission questions. EOF drains reachable work; explicit close drains or cancels. Output/observation failure triggers cleanup and nonzero exit. RPC exposes no MCP management, dedicated MCP events or elicitation answers; unsupported blocking interactions fail explicitly. Agent-side MCP tools retain settings/auth protection and command-only transport policy under protected execution. See [RPC contract](../docs/agent/design-agent-runtime.md#transport-adapters-and-access-boundary).

## Interactive workflow

| Input | Action |
|---|---|
| `/new`, `/resume`, `/rewind` | Start a Session, reopen a saved Session, or return to an earlier point. |
| `/model` | Choose a model; `/model update` refreshes the model catalog. |
| `/switch` | Choose or configure a switch (provider, model, thinking level, mode, prompt). Use `/switch NAME` or an alias for direct selection; Ctrl+↑/↓ cycles presets. |
| `/reload` | Reload settings, prompts, context, and extensions. |
| `@file`, `/skill:` | Include file context or expand a skill into the prompt. |
| `/mcp` | Open the MCP panel. |
| `/memory-inject [prompt]` | Rebuild and inject Memory/Tape context with an optional prompt; when Memory is enabled. |
| `/tasks`, Ctrl+T | Open the task panel; Tab or ← / → switches between checklist and execution tabs. |
| Ctrl+B | Background the current Agent's foreground Bash; choose an invocation when several are running. |
| ↑ / ↓, Enter, Ctrl+D | In the execution region: select, open details/output, or stop the selected running execution. |
| Esc | Close a panel or return from a child view. |
| Ctrl+O | Toggle details and whole subagent traces; expanding a parent trace shows metadata and tool headers only. |

`@reviewer inspect` directs a named asynchronous delegation prompt to the master Agent. `@@` is not supported. Master names the primary conversation Agent, not a privileged role. The master Agent chooses the task and an existing working directory from your prompt and invokes the Agent tool. Optional child `role` and `team` are validated by SDK authority and are never inherited. Delegation performs no Git operations; worktree preparation through ordinary command tools remains subject to the active permission and sandbox policy. Child Agents keep separate histories and tool permissions; finishing a task never removes its worktree. The directed prompt is a normal master turn, eligible for its first Memory injection and user-authored Tape keyword anchors.

Bash can also start with `run_in_background: true`. It returns an execution ID and output path; completion is delivered automatically to the originating Agent. Notifications instruct the Agent not to restart explicitly user-cancelled commands without a new request, and to safely recover unexpected interruptions after checking for partial effects. A foreground timeout moves the command to the background, except commands starting with `sleep`. Foreground-subagent commands stop when that run ends. Unattended background commands default to 30 minutes, with an explicit limit capped at two hours; interactive background commands have no runtime deadline. Output files are limited to 5 GB; the panel shows the latest 50 KiB. Stop and shutdown clean the original Unix process group; processes that escape it with `setsid` are not yet covered. Executions are process-local and are not restarted on resume.

Opening or leaving a child view never stops it or consumes its outcome. A subagent trace appears below its `Agent` tool call and defaults to folded, showing `[agent] <name> (ctrl+o to expand)` without a line count. Expanding it shows task metadata and uniformly indented tool headers without connectors; thinking status reuses the normal style and remains within the trace after completion, without reasoning text or separate expansion controls. Complete messages, reasoning, outputs, and diffs stay in the child view. Resume keeps each trace beneath its original Agent tool call. Child permissions remain actionable from either view, and completion never switches views.

Custom prompt commands and Agent definitions are loaded from the Host's resources. `/memory-inject` uses the Host's Memory/Tape context builder. Use the [switch and settings guide](../README.md#settings) to configure presets and permissions.

## Prompt and Agent tool settings

Configure tool use in the YAML frontmatter of Prompt templates and Agent definition files.

Prompt/Skill slash inputs expand automatically through generation-scoped hooks in interactive, print, RPC and application SDK submissions; Internal/Extension text is not expanded. Prompt commands use `allowed-tools` for temporary pre-approval and `disallowed-tools` for whole-tool removal. For example, save `.phi/prompts/review.md` and submit `/review` in an interactive session:

```markdown
---
description: Review changes without writing files
allowed-tools:
  - Read
  - 'Bash(git diff *)'
disallowed-tools:
  - Write
  - Edit
---
Review the changes with git diff --stat and read relevant files.
```

`Read` pre-approves every invocation; `Bash(git diff *)` pre-approves matching command evidence, not every Bash call. Explicit Deny still wins outside Yolo. Pre-approval adds no tools or capabilities and cannot restore removed tools. Removal lasts through the entire outer response, including tool/model rounds. Queued input activates nothing until dispatched; injected Steers union allowances/removals, with removal winning. Completion, failure, abort or stop retires policy; later turns and children inherit none. Prompt policy is never saved as a Session grant; confirmed AllowAlways remains durable.

Agent definitions instead select tools with `tools` and remove them with `disabled-tools`. For example, `.phi/agents/reviewer.md`:

```markdown
---
name: reviewer
description: Inspect code without shell execution
tools:
  - Read
  - Grep
  - Bash
disabled-tools:
  - 'Bash(git push *)'
---
Read and search relevant files; report findings without edits.
```

Here `Bash(git push *)` removes **all Bash**, not just push commands. Agent selection/removal never grants invocation permission. Use YAML lists as shown; comma-separated strings are also supported. ToolSearch loads deferred tools for direct calls without enabling Codemode. Agent `tools: [Read]` excludes ToolSearch; `tools: [Read, ToolSearch]` retains it, and `disabled-tools: [ToolSearch]` removes it. Removing ToolSearch does not disable MCP, withdraw loaded declarations or grant invocation permission. Agent and Prompt tool fields reject the retired `MCP`/`mcp` names, individual MCP tool names (including unprefixed aliases) and server/tool patterns; use MCP `exposure`/`toolExposure` configuration instead.

SKILL.md supports no tool fields: Skills provide context, and their scripts use ordinary tools/permissions. A Prompt requesting a Skill keeps its own policy. Invalid resources are excluded with file/field/original line/column diagnostics before interactive terminal entry; valid resources remain available. These are Phi's contracts, not a claim of full Claude Code compatibility. See [settings details](../docs/design-interactive.md#prompt-and-agent-tool-settings).

## Configuration

Phi reads global `~/.phi/settings.json` and the working directory's `.phi/settings.json` for application settings. Custom provider/model definitions belong in `~/.phi/models.json`; MCP servers belong in `~/.phi/mcp.json`. Sessions are saved under `~/.phi/projects/` by default, scoped to the working directory. See the [main settings reference](../README.md#settings) and [MCP configuration](../phi-sdk/src/extension/mcp/README.md) rather than duplicating those formats here.

Use `phi config` (or `phi config --local`) to search and enable/disable discovered skills, prompts, and Lua extensions. Tab switches global/project scope; space toggles global resources or cycles project inherit/load/unload overrides. Changes are saved in the respective `settings.json`; reload an active standalone session with `/reload`.

### Command hooks

`hooks.enabled` defaults to `true`; no explicit configuration is needed. Set `{"hooks":{"enabled":false}}` in global or project `settings.json` to disable settings command hooks and all Lua hooks and skip workspace trust prompts. An explicit project value overrides the global value. Builtin/Rust hooks and ordinary extension commands, tools and keybindings remain enabled. Apply changes with `/reload`.

Register command hooks in global `~/.phi/settings.json` (`PHI_CONFIG_DIR` overrides the directory). Rust callbacks, settings commands and Extension/Lua registrations share the SDK Hook layer. Commands support `SessionStart`, `UserPromptSubmit`, `UserPromptExpansion`, `PreToolUse`, `PostToolUse`, `Stop` and `Notification`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.phi/hooks/check-command.sh\"",
            "timeout": 2
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Edit|Write",
        "hooks": [
          {
            "type": "command",
            "command": "bash \"$HOME/.phi/hooks/format-file.sh\""
          }
        ]
      }
    ]
  }
}
```

Plain `matcher` strings match exact, case-sensitive identities; `{ "regex": "pattern" }` explicitly enables regex matching. Targets are tool names, typed `SessionStart` reasons, expansion command names or notification types. Omitted, empty or `*` matches everything; nonempty matchers on `UserPromptSubmit` or `Stop` are rejected. Commands run concurrently through `bash -c` outside the tool sandbox, with the user's process permissions and no Agent approval prompts; results reduce in registration order. Hook outcomes never bypass authorization of Agent tools. Nonempty project hook registrations remain rejected.

Interactive startup asks whether to trust or disable hooks for the canonical workspace root. Global `hooks.trust` stores exact absolute paths: `"+/path/to/workspace"` (or an unprefixed path) trusts it; `"-/path/to/workspace"` disables settings commands and Lua hooks, including application hooks. Disable wins when rules conflict. Ordinary extension commands, tools and keybindings retain their own policies. RPC and web startup require a saved decision when process hooks exist; print permits missing trust but honors disable rules. Lua process capabilities still require a trusted extension source; workspace trust does not authorize project extension processes.

Each handler may set `id`, `once`, `if` and `timeout`. `once: true` consumes one invocation attempt, including process/output errors and timeout; `id` is optional and stabilizes identity across reloads. Reload preserves continuing identities; Reset does not rearm them. `if` supports event fields with `equals` or `regex`, including dotted paths such as `tool_input.command`. Notification is background-only; other settings commands are awaited. Retired `scope` and `execution` fields are rejected. See [Hook registration](../docs/design-hook.md#registration).

Each command receives JSON on stdin with `hook_event_name` and event-specific fields:

| Event | Fields | Effect |
| --- | --- | --- |
| `SessionStart` | `source`, `reason`, `previous_session_file` | Initialization; failures warn |
| `UserPromptSubmit` | `original_prompt`, `prompt`, `source`, `dispatch` | Transform input, add context or reject |
| `UserPromptExpansion` | `command_name`, `command_args`, `command_source`, `prompt` | Add context or block application expansion |
| `PreToolUse` | `tool_name`, `tool_use_id`, `tool_input` | Transform parameters or reject |
| `PostToolUse` | Tool fields, `tool_response`, `duration_ms` | Adjust successful results; never undo effects |
| `Stop` | `response`, `continuations`, `stop_hook_active` | Accept completion or request bounded continuation |
| `Notification` | `notification_type`, `level`, `message`, `tool_name` | Background observation/actions only |

`original_prompt` preserves the submitted text across rewrites; `prompt` contains preceding transforms. Internal/Extension source text is not human approval.

Notification types are `permission_prompt`, `idle_prompt` (accepted completion), and explicit `message`. Notifications do not block Agent controls or create permission questions.

For example, `check-command.sh` can inspect the requested command:

```bash
#!/usr/bin/env bash
set -euo pipefail

command=$(jq -r '.tool_input.command // ""')
if [[ "$command" == *"git push --force"* ]]; then
  echo "Force push is blocked." >&2
  exit 2
fi
```

This string match is an example, not a complete command security policy. Use permission rules for authorization. Formatting scripts should read `.tool_input.path` and invoke the formatter appropriate to that file type.

- Exit `0` continues unless JSON requests rejection. JSON may be a direct object or use `hookSpecificOutput`; top-level decisions are retained.
- `UserPromptSubmit` accepts `prompt` and `additionalContext`; plain stdout also adds prompt context. `PreToolUse` accepts `updatedInput`. `PostToolUse` accepts `updated_output`.
- Exit `2`, or JSON `{"decision":"block","reason":"..."}`, rejects input/tool use or returns Stop continuation feedback. Other input/pre-tool failures fail closed; Stop failures reject completion.
- SessionStart, PostToolUse and Notification failures warn and continue later hooks. Post-tool failure never reverses completed effects.
- `timeout` is seconds, defaults to `2`, and accepts `1..30`, independently per command. It covers process execution; cancellation interrupts the process. There is no additional outer handler deadline.
- Captured stdout/stderr are bounded to 1 MiB each. Cancellation/timeout kills the original process group on Unix; descendants that create a new session can escape it.

Script files are not auto-discovered. Register them in settings; simple commands can be written inline instead. `/reload` applies registration changes; existing generations retain their configuration until replaced. Script contents are read when the command runs.

## Development

From the workspace root, run without installing:

```bash
cargo run -p phi-interactive -- "Fix the failing test"
cargo run -p phi-interactive -- -p "Summarize this repository"
cargo test -p phi-interactive
```

MCP and Lua are always compiled. Runtime configuration controls MCP connections and Lua extension loading.

| Path | Responsibility |
|---|---|
| `src/main.rs`, `src/cli/`, `src/config/` | Startup, CLI/print adapters and application settings. |
| `src/runtime/` | Candidate preparation, Session associations, scoped access and canonical persistence. |
| `src/rpc/` | Execution/control messages, bounded stdio and RPC lifecycle. |
| `src/session/`, `src/resources/`, `src/interactive/` | Durable storage, resource policy/Prompt/Skill/command declarations and terminal interaction. |

See the [application design](../docs/design-interactive.md) and [Agent runtime design](../docs/agent/design-agent-runtime.md) for ownership boundaries.
