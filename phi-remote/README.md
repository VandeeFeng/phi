# phi-remote

Use Phi through Web, TUI, Telegram or Feishu with a shared Gateway.
Closing a client does not stop its Agents.

Design: [Remote and Gateway](../docs/design-remote.md).

## Run

```bash
# Keep running in a separate terminal
phi gateway

# Open Web for the running service
phi --web

# Attach TUI to a hosted Session (on the Gateway host, directly or via SSH)
phi --gateway --session SESSION_ID

# Manage the service
phi gateway status
phi gateway stop
```

The default address is `127.0.0.1:3141`; use `phi gateway --port PORT` to change it.
Enable the Web plugin below before using `phi --web`.

`phi --web` and `phi --gateway` require a running Gateway. They select an existing
Session, or create one in the client's current directory if none exist.
Gateway's launch directory does not select a workspace; resumed Sessions use their
saved directories. `phi gateway stop` stops the service for all clients.

### Device authorization

On first browser access, get the pairing token in an interactive terminal:

```bash
phi gateway token
```

Enter it in the browser, then approve the request on the host:

```bash
phi gateway devices approve

# If multiple requests are pending, choose an exact ID
phi gateway devices list
phi gateway devices approve REQUEST_ID

# Reject a request or revoke a device
phi gateway devices reject REQUEST_ID
phi gateway devices revoke DEVICE_ID
```

The browser remembers its authorization. Approvals expire after 30 days by default;
expired devices must pair again. `devices list` shows expiry dates. Token output
cannot be redirected.

### Configuration

Use `gateway/config.json` under the Phi config directory, or `config.json` directly under `PHI_GATEWAY_DIR` when set. Keep the file private and restart Gateway after changes.

Complete configuration example (replace credentials and allowlist IDs):

```json
{
  "port": 3141,
  "publicOrigin": "https://phi.example.com",
  "trustedProxies": ["127.0.0.1"],
  "expiry": 30,
  "plugins": ["+builtin:web", "+builtin:telegram", "+builtin:feishu"],
  "telegram": {
    "botToken": "<bot-token>",
    "allowedUsers": [123456789]
  },
  "feishu": {
    "appId": "cli_replace_me",
    "appSecret": "<app-secret>",
    "allowedUsers": ["ou_replace_me"],
    "allowedGroups": ["oc_replace_me"]
  }
}
```

- `port`: defaults to `3141`; CLI `--port` overrides it.
- `publicOrigin`, `trustedProxies`: optional HTTPS reverse-proxy settings. Omit both for local-only use. Proxy entries must be exact IP addresses, not CIDRs. Gateway always listens on loopback.
- `expiry`: approval lifetime in days, default `30`; `0` means never expire. Changes affect new approvals only.
- `plugins`: defaults to `[]` (all disabled).
- `telegram`, `feishu`: required when enabling the corresponding plugin; otherwise omit them. User allowlists must be nonempty. Feishu `allowedGroups` is optional; omitting it disables group access.

Unknown fields and invalid values are rejected. Proxy deployment validation remains pending; see the [deployment and security guidance](../docs/design-remote.md).

## Built-in remote plugins

All plugins are disabled by default. Enable or disable them in Gateway config:

```json
{
  "plugins": ["+builtin:web", "-builtin:telegram", "-builtin:feishu"]
}
```

The last entry for each plugin wins. Restart Gateway to apply changes.
Disabling Web does not disable TUI access or device authorization.

For messaging clients, enable the plugin and add its platform configuration in the
same file:

- [Telegram setup and commands](plugins/telegram/README.md)
- [Feishu setup and commands](plugins/feishu/README.md)

No separate bridge command is needed. If a client fails, fix its configuration or
authorization and restart Gateway; other clients and Agents keep running.

## Build Web assets

```bash
pnpm --dir phi-remote/plugins/web install --frozen-lockfile
pnpm --dir phi-remote/plugins/web build
```

Node.js is needed only for building. Gateway and TUI work without Web assets.
If assets are missing, `phi --web` offers terminal setup; rerun it afterward.

## Check

```bash
cargo test -p phi-remote
cargo test -p phi-interactive --test remote
pnpm --dir phi-remote/plugins/web test
pnpm --dir phi-remote/plugins/web check
```

Cross-process tests require Node.js and Python 3. Test data stays under `/tmp/`.

## Web demo

```bash
pnpm --dir phi-remote/plugins/web install --frozen-lockfile
pnpm --dir phi-remote/plugins/web demo
```

Open `http://127.0.0.1:8080`. The demo uses simulated Agents and permissions; no
Gateway, token or provider is required. Source changes rebuild and refresh the page
automatically. Press Ctrl+C to stop.
