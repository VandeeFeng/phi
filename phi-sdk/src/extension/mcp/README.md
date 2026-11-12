# MCP client

Phi prefers MCP 2026-07-28 through RMCP 3.1, `server/discover`, self-contained request metadata, and `subscriptions/listen`. When a server returns method-not-found for `server/discover`, Phi falls back on the same transport to the MCP 2025-11-25 `initialize` lifecycle. Streamable HTTP and stdio are supported; legacy SSE is not.

## Configuration

Extension activation and MCP invocation permission belong in `~/.phi/settings.json` (or project `.phi/settings.json`):

```json
{
  "mcp": true,
  "permission": {
    "mcp": "ask"
  }
}
```

MCP server and tool-exposure configuration belongs only in the global `~/.phi/mcp.json`:

```json
{
  "settings": {
    "toolPrefix": "server",
    "idleTimeout": 10,
    "showStatus": true
  },
  "mcpServers": {
    "local": {
      "command": "npx",
      "args": ["-y", "some-mcp-server"],
      "lifecycle": "lazy"
    },
    "remote": {
      "url": "https://example.com/mcp",
      "bearerTokenEnv": "EXAMPLE_MCP_TOKEN",
      "toolExposure": { "search": "direct", "delete": "hidden" }
    }
  }
}
```

### Server options

| Field | Description |
|---|---|
| `command` | Executable for stdio transport |
| `args` | Command arguments |
| `env` | Environment variables with `${VAR}` interpolation |
| `cwd` | Child-process working directory |
| `url` | Stateless Streamable HTTP endpoint |
| `auth` | `"bearer"` or temporary manual-token `"oauth"` mode |
| `bearerToken` / `bearerTokenEnv` | Explicit token or environment-variable name |
| `lifecycle` | `"lazy"` (default), `"eager"`, or `"keep-alive"` |
| `idleTimeout` | Per-server idle timeout in minutes |
| `exposeResources` | Expose resources as generated MCP server tools (default: true) |
| `exposure` | Default tool exposure: `direct`, `deferred`, or `hidden`; inherits global settings |
| `toolExposure` | Exact original tool names mapped to exposure overrides |
| `debug` | Show server stderr (default: false) |

### Global settings

| Setting | Description |
|---|---|
| `toolPrefix` | `"server"` (default), `"short"`, or `"none"` |
| `idleTimeout` | Global idle timeout in minutes; `0` disables it |
| `exposure` | Default tool exposure for all servers (`deferred`) |
| `toolExposure` | Global exact-name tool exposure overrides |
| `showStatus` | Show MCP status in the footer |
| `notify` | Show MCP runtime notifications |

Use `phi --log-file /path/to/diagnostics.log` with `RUST_LOG=warn,rig=off,rmcp=debug,mcp=debug` to capture MCP and transport diagnostics. Without `--log-file`, tracing output is discarded; it never enters the conversation.

## Lifecycle and metadata

- **`lazy`** connects on first use and disconnects after the idle timeout.
- **`eager`** connects at startup without keep-alive reconnection.
- **`keep-alive`** connects at startup and reconnects after failures.

HTTP handshakes retry transient connection, timeout, body-read, and closed-transport failures twice, after 500 ms and 1 s. Authentication and protocol errors are not retried. Persistent DNS or network failures still surface after the final attempt.

Phi first requests MCP 2026-07-28 through `server/discover`. Only a method-not-found response triggers a 2025-11-25 `initialize` fallback; authentication, transport, and other protocol errors remain failures. Modern dynamic tool, resource, and prompt changes arrive through one capability-filtered `subscriptions/listen` stream per connection. Legacy peers use their advertised `notifications/*/list_changed` capabilities instead. A fresh identity-independent legacy cache entry also records the negotiated lifecycle, so later starts can use `initialize` directly until that metadata expires.

RMCP owns protocol response caching, TTL, invalidation, pagination, and stale-on-error behavior. Legacy list responses have no wire TTL or cache scope, so Phi applies a seven-day public cache policy and persists identity-independent metadata in `~/.phi/mcp-cache.json`. Modern wire policies take precedence. Tools are required, while invalid optional resource or prompt lists degrade to empty metadata; malformed list shapes are debug-only and operational failures remain warnings. Private, environment-dependent, custom-header, bearer, and OAuth metadata remains in memory. Cache writes merge compatible entries already on disk before atomic replacement to reduce concurrent-session clobbering. Incompatible cache schemas are discarded.

## Shared tool exposure

There is no `MCP` model tool. Tool discovery belongs to `ToolSearch`, which searches deferred metadata and loads matches into the next model request. Invoke loaded tools by their registered names. Codemode independently discovers and calls tools through its script APIs. Discovery uses cached metadata where valid and connects as needed; actual calls connect automatically. Connection status and explicit connection/reconnection belong to the user-side `/mcp` panel and SDK manager operations.

`ToolSearch` is a normal Phi-governed tool: availability follows Agent selection/exclusion, outer-turn removals and `no_tools`. Loading uses ToolSearch invocation permission and never grants permission for a target invocation. The retired `MCP`/`mcp` names are rejected in Agent/Prompt tool settings; remove them rather than replacing them with server-tool names.

Top-level `mcp: false` disables the MCP extension independently of tool visibility and permission. It prevents the manager, server-tool registrations, lifecycle, and elicitation from starting; the default is enabled.

Server tools use the same `ToolExposure` as other tools. MCP retains target authority and semantic invocation permission checks under `permission.mcp`; ToolSearch availability and loading do not grant target authority or approval.

- `direct`: declared to the Agent like a built-in tool; also reachable through eligible indirect entry points.
- `deferred`: omitted from initial declarations and Codemode inline guidance; `ToolSearch` can load it for direct model calls without enabling Codemode. Scripts can also discover/call it independently.
- `hidden`: absent from Agent discovery and rejected by direct and nested calls, even if previously loaded or in Yolo. Management views may still show it.

Precedence: server `toolExposure[name]`, server `exposure`, global `toolExposure[name]`, global `exposure`. Names are exact original tool names; wildcard matching and discovery-driven exposure changes are unsupported. MCP defaults to `deferred`, independently of Codemode enablement.

```json
{
  "settings": {
    "exposure": "deferred"
  },
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "toolExposure": {
        "search_repositories": "direct",
        "get_file_contents": "direct"
      }
    },
    "browser": {
      "command": "npx",
      "args": ["-y", "browser-mcp"],
      "exposure": "direct",
      "toolExposure": { "delete_history": "hidden" }
    }
  }
}
```

Runtime assembly captures both direct and deferred contracts from fresh public cache or matching active-connection metadata and never waits for discovery. Missing or expired direct-tool metadata refreshes in the background with at most ten concurrent connections. Deferred-only servers remain disconnected under the default `lazy` lifecycle until ToolSearch/Codemode discovery, restored loading or explicit connection; fresh metadata cache can satisfy discovery without connecting. Explicit `eager`/`keep-alive` lifecycle still connects proactively. Successful refresh emits a tool-catalog change; an idle interactive client rebuilds its immutable registry transactionally, retaining the previous runtime if rebuilding fails. Cold discovery validates newly discovered deferred contracts into a scoped registry. Codemode discovery does not change provider declarations. ToolSearch records loaded tool names as branch-local Transcript events; the next request projects their schemas and guidance from current available contracts. Resume, fork, navigation and runtime replacement restore loading from the selected branch without restoring revoked authority. Compaction preserves these records. Deferred-only metadata changes also trigger ordinary generation replacement. Private, zero-TTL, or identity-sensitive metadata remains valid only for its active connection generation.

Run `/mcp` to inspect connections and edit exposure. Saving changes only the selected tool overrides, preserving server defaults and unobserved overrides. Catalog updates apply when the active client becomes idle.

The former `directTools` and `disabledTools` fields are rejected. Replace booleans with `exposure`, direct-name lists with direct overrides on a deferred default, and disabled names with hidden overrides. Materialize old effective policy where global and server lists overlapped; see [tool semantics](../../../../docs/design-tools.md#unified-mcp-policy).

## Elicitation

Phi supports MCP form and URL elicitation during active tool calls:

- TUI forms handle primitive fields and single/multi enums with required and type validation.
- URL requests display the server origin and URL, open only HTTP(S) URLs without shell interpolation, and require explicit completion. Credentials entered in the browser are never collected by Phi.
- RPC does not expose MCP elicitation; unsupported interactions are declined.
- Print mode deterministically declines because no interaction adapter is attached.

Elicitation is not Permission: it never creates allow rules or mutates permission state.

## Tool results

Text is preserved directly. Successful structured content is rendered when textual content is absent; error results always retain structured diagnostics alongside readable text. Images, audio, binary resources, embedded text, and resource links are projected into bounded transcript-safe output; raw media payloads are not copied into model history.

## Execution environment

Bind a manager with `with_sandbox` before calling `start` when subprocesses require execution restrictions. Eager startup, lazy connection and metadata refresh use the same bound command-launch path. A configured `cwd` does not expand writable mounts. Equal Sandbox environments and transport policies may share a pool; changing directories, mount rules, Host write protection or transport policy creates a fresh pool without copying live connections or private metadata. Existing persisted-cache TTL rules are unchanged.

SDK Instance runtime construction also binds supplied managers before tool use. Standalone manager operations are trusted SDK access. `with_execution_environment` accepts a Sandbox and `McpTransportPolicy`; `CommandOnly` rejects URL connections, refreshes and calls, including cached tools. URL servers run outside the client's process Sandbox. Phi's protected remote mode therefore excludes them; ordinary local mode retains them. See [application policy](../../../../docs/agent/design-agent-runtime.md#configuration-write-access). See [SDK security model](../../../../docs/design-sdk.md#permission-and-sandbox-model).

## OAuth status

MCP OAuth discovery, issuer binding, credential storage, refresh, and login/logout remain deferred. Phi does not read foreign credential files or provide a manual-token-file setup flow. With `auth: "oauth"`, connections require explicitly configured authorization headers, `bearerToken`, or `bearerTokenEnv`; otherwise they report that MCP OAuth is not implemented. `/mcp-auth <server>` reports this limitation rather than initiating authentication. Metadata-cache TTL is independent of authentication validity.
