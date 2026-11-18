# phi

Rust workspace for the Phi agent stack.

```text
AgentInstance owns execution and Agent lifecycle
Transcript owns history
tools own capabilities
hooks execute registered extension handlers at lifecycle boundaries and return outcomes to the host
hook subsystem owns registration and local handler scheduling/execution
Agent runtime applies hook outcomes and owns the main execution flow
AgentManager owns SDK membership and Agent orchestration
application runtime owns preparation, Session associations and persistence
main owns process composition and finalization
attachments own scoped access, never Agents
UI state owns projection and interaction
clear SDK boundary
```

## Architecture

- [Global architecture](docs/design.md)
- [AgentInstance, AgentManager, and application runtime](docs/agent/design-agent-runtime.md)
- [Parallel Agent design](docs/agent/design-parallel-agent.md)
- [`phi-sdk` design](docs/design-sdk.md)
- [Hooks and handlers](docs/design-hook.md)
- [`phi-tui` design](docs/tui/design-tui.md)
- [`phi-interactive` design](docs/design-interactive.md)
- [`phi-remote` and Gateway design](docs/design-remote.md)
- [`phi-remote` setup](phi-remote/README.md)
- [`phi-codemode` engine and SDK integration](phi-codemode/README.md)

## Intent layer

Before modifying a crate, read its local `AGENTS.md`:

- `phi-sdk/AGENTS.md` — reusable agent runtime and SDK.
- `phi-tui/AGENTS.md` — reusable terminal UI runtime.
- `phi-interactive/AGENTS.md` — CLI application, runtime preparation, protocols and adapters.
- `phi-codemode/AGENTS.md` — standalone JavaScript sandbox, host bridging, execution limits and cancellation.

## Workspace invariants

- Workspace members are `phi-sdk`, `phi-tui`, `phi-interactive`, `phi-codemode`, and `phi-remote`.
- Dependency direction is `phi-interactive -> phi-sdk + phi-tui + phi-remote`, `phi-remote -> phi-sdk`, and `phi-sdk -> phi-codemode`; `phi-sdk` and `phi-tui` are independent siblings.
- `phi-codemode` owns isolated JavaScript execution, host bridging, limits and cancellation, not Agents, Sessions, permissions or persistence.
- MCP and Lua are always compiled. Runtime configuration controls capability activation and resource loading.
- `AgentInstance` is the addressable live execution resource and the sole owner of mutable execution state and the canonical live Transcript.
- SDK AgentManager owns instance membership/delegation. Application runtime modules own preparation, Session associations, boundaries and persistence; main owns process finalization. Do not recreate a universal application facade.
- `AgentAttachment` and `SessionAccess` provide scoped internal access; dropping them never implicitly stops an Agent.
- Keep SDK/application semantics independent of mode adapters; expose only supported workflows. See [transport and wire boundaries](docs/agent/design-agent-runtime.md#transport-adapters-and-access-boundary).
- `AgentInstance` does not depend on TUI, Dashboard, transport, or any other attachment.
- External RPC callers never receive provider, permission, extension, MCP, sandbox or execution-runtime collaborators.
- Interactive/print call concrete application capabilities; sibling `src/rpc/` adapts version-2 tagged JSONL to one controlled root. Main finalizes every mode. No MCP controls/events/elicitation, generic Session/history service or legacy method aliases are exposed; SDK MCP execution and interactive MCP remain.
- EOF drain, explicit drain/cancel and output failure are distinct RPC policies, supervised through finalization.
- Keep provider tool definitions and dynamic prompt tool sections derived from the same resolved `ToolRegistry`.
- Application SettingsManager alone owns settings reads/refresh; application and resource consumers share its in-memory global/project/merged settings. No getter or resource loader may implicitly read settings. See [settings ownership](docs/design-interactive.md#settings-ownership).
- Runtime replacements must validate a complete candidate before mutating active or persisted state.
- Ordinary tree/rewind navigation is memory-only: retain live selection/resources, preserve history, and append no configuration event. Continuation records actual provider/model and Switch identity, not a configuration snapshot. Checkpoint/exit never persist its cursor; disk reopen selects the last appended entry. See [navigation semantics](docs/agent/design-agent-runtime.md#navigation-and-recovery).
- Never reuse display IDs or foreign provider/API/model protocol data as provider request identity. Build target-specific history from canonical Transcript without mutating it; keep tool calls and results correlated. See [provider history projection](docs/design-sdk.md#provider-history-projection).
- Use `working_dir`, `project_root`, and `session_scope` according to the definitions in `docs/design.md`.
- Do not read, expose, log, or persist secrets outside existing typed config/runtime paths.

## Common commands

ALWAYS use `test.sh` for phi tests.

set timeout 600s.

```bash
./test.sh full                       # parallel workspace tests and checks
./test.sh runtime                    # application runtime regressions
./test.sh sdk                        # SDK harness regressions
./test.sh repeat                     # repeat runtime/SDK suites, default 5 rounds
./test.sh focus phi-sdk TEST_FILTER  # focused test
./test.sh check                      # check, fmt and whitespace
```

`TEST_REPEAT_ROUNDS` configures repeat count. The helper requires Bash and `setsid`; each job isolates Session/config/temp data under `/tmp/` and deletes it on exit. Logs remain in the printed `/tmp/phi-validation-*` directory; existing Sessions are untouched.
