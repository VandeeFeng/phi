# Telegram

Use Phi from private Telegram text chats. No webhook or inbound port is needed.

## Setup

1. Create a bot with BotFather and obtain your numeric Telegram user ID.
2. Add to `gateway/config.json` under the Phi config directory (or `PHI_GATEWAY_DIR`):

```json
{
  "plugins": ["+builtin:telegram"],
  "telegram": {
    "botToken": "<bot-token>",
    "allowedUsers": [123456789]
  }
}
```

3. Start or restart `phi gateway`, then send the bot a message.

Keep the config private. Only one client may use the bot token; remove any existing
webhook first. Add `+builtin:web` to `plugins` to also enable browser access.

## Commands

| Command | Action |
| --- | --- |
| `/new` | Create and select a Session in the plugin workspace |
| `/stop` | Cancel the selected task |
| `/status` | Show execution status |
| `/help`, `/start` | Show help |

Each chat has its own Session selection. `/new` does not stop the previous task.
Permission buttons support **allow once** and **deny**; use Web/TUI for persistent
or oversized approvals.

## Notes

- Groups, media and edited messages are unsupported. Replies contain completed assistant text, not reasoning or tool output.
- After a Gateway restart, resume missing Sessions through Phi or use `/new`.
- Replies can be lost on interruption. Check Web/TUI before resending a request.
- Allowlisted users must be trusted: the allowlist does not isolate Sessions from other authorized clients.
- For expired or revoked device access, inspect `phi gateway devices list` and follow the [Gateway authorization instructions](../../README.md#run).

[Gateway setup](../../README.md#built-in-remote-plugins)
