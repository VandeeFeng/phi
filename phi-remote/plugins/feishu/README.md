# Feishu

Use Phi from Feishu private text chats and group @mentions. Supports mainland
self-built enterprise bots; no public callback or inbound port is needed.

## Setup

1. In the Feishu developer console, enable the Bot capability and these permissions:
   - `im:message.p2p_msg:readonly`
   - `im:message.group_at_msg:readonly`
   - `im:message:send_as_bot`
   - `im:message.reactions:write_only`
   - `cardkit:card:write`
2. Select long-connection event subscription and subscribe to `im.message.receive_v1`.
3. Add to `gateway/config.json` under the Phi config directory (or `PHI_GATEWAY_DIR`):

```json
{
  "plugins": ["+builtin:feishu"],
  "feishu": {
    "appId": "cli_replace_me",
    "appSecret": "<app-secret>",
    "allowedUsers": ["ou_replace_me"],
    "allowedGroups": ["oc_replace_me"]
  }
}
```

4. Start or restart `phi gateway`. If Feishu requires a live connection, finish event
   subscription now. Publish the application, obtain any required administrator
   approval, set its availability scope, and add the bot to the desired groups.

Keep the config private. Run only one client per app. `allowedUsers` must be nonempty;
omit `allowedGroups` to disable groups. Group messages require an allowed sender,
an allowed group and an @mention. Add `+builtin:web` to also enable browser access.

## Commands

In groups, @mention the bot before every command.

| Command | Action |
| --- | --- |
| `/session` | Show selected Session and working directory |
| `/session new [directory]` | Create and select a Session; defaults to the current Session's environment |
| `/session list` | List live and saved Sessions |
| `/session resume <id>` | Select a live Session or resume saved history |
| `/new` | Create and select a Session in the plugin workspace |
| `/stop` | Cancel the selected task |
| `/status` | Show execution status |
| `/help`, `/start` | Show help |

Each private user has a selection; each group shares one. Switching Sessions does
not stop the previous task. Directories must exist; absolute paths and `~` work.
Relative paths use `<gateway>/plugins/feishu/workspace/`.

## Notes

- Replies stream as cards, with plain-text fallback. Reasoning, tool details, files and images are not included.
- Use Web/TUI for permission approval; project Hook trust requires host-side approval.
- After restart, Sessions are restored but interrupted tasks are not rerun. If recovery fails, use `/session resume <id>` or `/new`.
- Replies can be lost or incomplete on interruption. Check Web/TUI before resending a request.
- Group replies are visible to all members. Allowlists do not provide Session isolation.
- For expired or revoked device access, inspect `phi gateway devices list` and follow the [Gateway authorization instructions](../../README.md#run).

[Gateway setup](../../README.md#built-in-remote-plugins)
