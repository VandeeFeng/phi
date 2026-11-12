# phi-remote and Gateway Design

## Scope

Phi supports embedded-local and Gateway deployments through one TUI. In Gateway mode, TUI and Web operate on the same live Sessions. Gateway is an application access service, not another execution center: the SDK owns Agent execution, and the application runtime owns preparation, Session associations and Session persistence. Web reuses those capabilities rather than building another Agent loop, queue or history authority.

The native listener remains IPv4 loopback. Browser access may use a configured HTTPS public origin through a trusted reverse proxy, with explicit device pairing. Native TUI discovery remains local; remote terminal access uses SSH to run Phi on the Gateway host. Arbitrary cross-host TUI endpoints, direct non-loopback listeners and managed tunnels are not provided.

Related: [global architecture](design.md), [application design](design-interactive.md), [Agent runtime](agent/design-agent-runtime.md), and [phi-remote setup](../phi-remote/README.md).

## Deployment model

### Embedded local

```text
foreground phi process
  TUI -> application operations / observations
                |
       preparation / Session associations
                |
       SDK AgentManager / AgentInstance
                |
          canonical Transcript
```

The foreground process owns resources, live instances and finalization. It does not start a Gateway or expose its Agents through Web.

### Local Gateway

```text
same host

TUI process                  browser
  ordinary TUI                  |
         |                      |
         +--- REST + WebSocket --+
                      |
Gateway service process
  phi-remote: authentication / requests / observations
                      |
  application operations / preparation / Session associations
                      |
  SDK AgentManager / AgentInstance
                      |
                canonical Transcript
```

The service process hosts the application runtime and starts with an empty hosted-Session registry. Startup loads global settings independently of the launch directory. Session creation and saved-history resume prepare and start a binding before publishing the hosted identity. TUI and Web address that identity.

Each client owns its selection, editor draft, overlays, scroll position and render caches. Clients can select different Sessions independently.

Opening saved history in an embedded runtime creates an independent execution resource. Storage locks and branch-preserving appends do not synchronize live state or attach to a hosted instance.

## Ownership and crate boundaries

| Owner | Responsibility | Excludes |
| --- | --- | --- |
| `phi-sdk` | Agent execution, admission, queues, delegation, cancellation and canonical Transcript | Gateway, terminal, browser and application persistence |
| Application runtime in `phi-interactive` | Settings/resources, candidate preparation, Session associations, accepted transitions, permission persistence and checkpoints | Transport scheduling |
| Gateway composition in `phi-interactive` | Directory selection, startup options, service reuse/retry, terminal disclosure policy and browser opening | Device authorization and protected Gateway storage |
| `phi-remote` | HTTP/WS transport, ingress checks, pairing/authorization, protected deployment config and storage, endpoint publication/discovery/probing, process lock, typed wire contracts, safe event projections and Web assets | Application Settings/provider credentials, Session storage policy, Agent construction or execution ownership |
| Interactive adapter in `phi-interactive` | Terminal input, controller, projection and rendering | Transport ownership |
| Web client | Browser interaction and display projection | Canonical history, execution or persistence authority |
| `main` | Deployment composition, signals and process finalization | A universal application facade |

Dependency direction is `phi-interactive -> phi-remote -> phi-sdk`. `phi-remote` does not import the application. `phi-tui` is independent of SDK and transport.

The application injects typed `RemoteHost` capabilities for Session preparation, recovery, configuration/reload, submission, scoped Agent messaging, close and permission persistence. Gateway uses an SDK Manager handle internally for scoped observations and root cancellation; Agent messaging goes through the application host. The `PairingStore` contract separates the authentication protocol from its authorization backend; the default durable authority and its protected file storage live together in `phi-remote::deployment`, not in the application. SDK handles and runtime collaborators never cross the wire; application-owned mutations use application acceptance and persistence.

The application resolves `PHI_GATEWAY_DIR` or the default `<Phi config>/gateway` directory and passes it explicitly to deployment operations. A startup/reuse or stop-wait flow retains that selected directory; `phi-remote` never reselects it from environment or application settings. Clients reuse protected endpoint reads and authenticated probing, not their own pairing-file interpretation. Token retrieval is a library operation; terminal-only disclosure is CLI policy.

SDK AgentManager owns instance membership. Application Session associations own hosting bindings and persistence. Transport lookups and resume requests resolve through those owners.

Gateway snapshots project SDK observations in `phi-remote` to select transport-visible fields and omit internal metadata and scope. SDK `AgentInstanceSnapshot`, `AgentInstanceView`, `InteractionEnvelope` and `CoreEventScope` remain in-process contracts. Safe public SDK values, including interaction requests, submission dispositions, Hook notices and message-delivery states, are reused directly: their semantics and schema are shared, not independently versioned by Gateway. Submission receipts contain `disposition` and `notices`; client input remains restricted to explicit transport fields. Define transport-specific types only where filtering, authority or semantics differ.

## TUI access paths

Terminal input handling, command semantics, interaction prompts, recovery, conversation projection and rendering use the ordinary interactive loop. Across the interactive adapter, UI modules define their capabilities and required dependencies. `assembly.rs` chooses which capabilities to enable when building components for Local or Gateway, then supplies the matching data and operations. Modes consume those capability selections; they do not define capability boundaries. Session transition and lifetime policies are supplied separately.

```text
UI modules define selectable capabilities
input / selection / conversation / Activities / interactions /
administration / extensions / shell
                              |
                              v
                  component assembly (assembly.rs)
                  choose capabilities + dependencies
                  +-----------+-----------+
                  |                       |
             local assembly          gateway assembly
                  |                       |
       selected capabilities       selected capabilities
       + required collaborators    + required collaborators
                  |                       |
                  +-----------+-----------+
                              |
                  same interactive runtime
                  input / controller / projection / render
                              |
                 supported operations and observations
                              |
                  +-----------+-----------+
                  |                       |
          embedded attachment     Gateway attachment
                  |                       |
          application runtime     HTTP -> host application runtime
```

`AgentAttachment` selects embedded or Gateway access inside the application; its internals are not wire contracts. Transport access and component capability selection are separate decisions. Local and Gateway are current assembly configurations, not fixed capability bundles. Each component reports its enabled capabilities through `InteractiveFeatures` for discovery, dispatch and keybindings, including after recovery or Session selection. This projection does not define or enable capabilities. UI does not infer capabilities from transport or missing local services, and UI availability never authorizes execution. See [component capability composition](design-interactive.md#capability-composition) for module ownership, assembly and acceptance boundaries.

The current Gateway TUI assembly enables prompt submission, root cancellation, scoped interaction replies, recovery, display expansion, model/Switch/thinking selection, reload, new/resume and server-side Prompt/Skill expansion. Initial Session selection follows the [client entry rules](#cli-and-web-entry-semantics). Its picker instances omit catalog updates/default initialization, Switch editing and saved-Session deletion. Its other components omit branch navigation, child detail/activity controls, Memory/MCP administration, extension handlers, authentication, shell execution, Agent mentions and Workflow observation. These are choices within the shared module capability model, not Gateway-specific capability definitions or capabilities inherently exclusive to Local. Enabling an omitted capability requires compatible dependencies and supported application/transport operations. Unavailable actions report a warning or error.

Web has its own renderer. Browser summaries are derived views; canonical history belongs to the SDK. Gateway TUI reuses the ordinary TUI projection; transport filters non-display SDK metadata without introducing another display or execution model. TUI uses Session history and execution observations, not browser-specific `office` JSON or display strings.

## Session operations and concurrent clients

The HTTP service and application host capabilities cover:

- Discover hosted Sessions and saved histories separately.
- Create a Session, resume saved history, and select a hosted Session.
- Read coherent root Session state, with separately queried child interactions.
- Read runtime model/Switch choices, change model/Switch/thinking selection and reload resources.
- Submit `TurnPrompt` input with explicit dispatch semantics, or send a scoped Agent message.
- Answer scoped permission requests and request root cancellation.
- Explicitly close a hosted Session.

Access to a hosted Session uses its binding without reopening its file. Concurrent resume of the same hosted identity converges on one binding. Creation requests require an absolute `workingDir` pointing to an existing directory; preparation loads that directory's project settings and resources. Saved-history resume uses `Session.working_dir`. Each Session owns its working directory, prepared scope and accepted resources. These service operations are not all exposed in the TUI or Web UI. Gateway TUI new/resume changes only that client's selection and never stops the outgoing hosted Session. New inherits the source Session's accepted effective configuration and resources; resume reuses an existing hosted binding or prepares saved history. Embedded selection retains its standalone replacement/finalization policy.

Gateway TUI `/resume` queries saved histories on the service, not the hosted-Session registry. Unsaved hosted Sessions are intentionally excluded, so switching away does not make them selectable through the picker. Hosted-Session discovery remains a separate service operation.

SDK admission determines ordering, Busy, Steer and Follow-up behavior. Gateway does not own an execution queue or turn scheduler.

Permission questions are scoped requests visible to authenticated clients. The interaction owner accepts answers; stale or competing answers return errors. AllowAlways persistence follows confirmed SDK acceptance. Permission details exceeding the transport's approval-display limit cannot be approved, but can still be denied. For MCP elicitation, the transport maps `deny` to cancellation; it does not expose approval or a distinct decline response. These transport restrictions are separate from whether the assembly enables MCP administration. Disconnecting a client neither answers a question nor cancels execution.

Accepted operations follow application persistence guarantees independently of the waiting connection. Timeout or disconnect does not establish rejection. Clients do not automatically retry submissions after a lost response; cancellation is explicit.

## Observations and recovery

The transport uses two channels: REST invokes business operations and reads authoritative state; WS authenticates/pairs devices, controls subscriptions and pushes live observations. There is no parallel WS business-command catalog. WS version 1 uses tagged request/response/event envelopes with strictly increasing per-connection request IDs and explicit protocol negotiation. Its version is independent of the version-2 native snapshot/runtime-update projection. REST responses report operation outcomes, not execution completion; WS events do not replace admission receipts. Their arrival order is not guaranteed.

The server supports one selected hosted-root watch per WS connection. `session.watch` installs the subscription before acknowledging its request ID as `watchId`; `session.unwatch` clears it. Ordered `session.event` envelopes carry Session/watch identity, connection-wide sequence numbers and allowlisted display events, SDK progress, boundary markers or refresh reasons. They do not expose raw SDK/Core events or imply descendant/workflow coverage. Bounded queues, write deadlines and lag handling retire incomplete streams rather than silently dropping events. Agent publishers never await client delivery.

Native snapshot recovery supports a client-generated `eventBoundary` correlation marker through the existing application/SDK root boundary. Subscribe first, request the correlated snapshot, install it and discard events through its matching marker; retain later events. Root history, execution and root questions share this boundary; child questions are queried separately, not captured in an atomic descendant snapshot. Markers are not replay cursors. Recovery buffers must be bounded; gaps, missing markers and disconnected or retired selections require recovery rather than continued delta application.

Gateway TUI forwards selected-root WS display events to its ordinary event reducer and retains SDK progress facts. Its connection task serializes correlated REST recovery and event forwarding; a local ordered marker lets the ordinary attachment receiver discard covered events without dropping later deltas. Reconnect establishes a fresh watch and recovers before resuming observation; no command is resent. Coalesced periodic reads retain application metadata, declarations, interactions and save observations not fully covered by the root stream. Configuration/reload operations return the captured accepted Session, execution snapshot and matching declarations through a separately filtered runtime-update contract; they do not fabricate queue or interaction observations or complete acceptance with a later recovery read.

Web authenticates its device, acknowledges the selected-root watch and reads a bounded browser-specific projection of conversation, completed/failed tool summaries, Agents and pending interactions. WS adds bounded disposable text/tool overlays; lifecycle and interaction hints request a coalesced OfficeView refresh. OfficeView and browser overlays omit tool arguments, full outputs and reasoning. This is a UI projection, not a separate authorization boundary: the shared authenticated WS display stream carries reasoning text and tool arguments/outputs for TUI use, while filtering internal provider metadata. Authoritative OfficeView reads replace overlays rather than appending them into committed history. OfficeView has no atomic event boundary: deltas overlapping an in-flight read are ignored, and later text overlays show newly observed fragments, not a promise of complete live-message reconstruction. Periodic refresh remains the fallback for uncovered values. Child-only interactions still require refresh even when the root revision is unchanged. Application save observations are available through native snapshots, not OfficeView.

Each TUI connection observes its client-selected Session; Web scopes requests to its URL-selected Session or the first listed Session. Retired sockets/watch IDs cannot update the current selection. TUI reports stale observation and retries connection/recovery; Web retires invalid or disconnected streams and offers explicit device reconnect. Recovery reads current authoritative state, not a durable event log.

Polling is not durable event replay. Clients do not infer missing deltas or treat missing save status as success. Slow or disconnected clients cannot block Agent execution, interaction answers or shutdown.

Service restart does not restore hosted bindings or in-flight execution. Saved history is resumed explicitly through application preparation. Gateway identity, pending pairing requests, device approvals and credential digests survive restart; native local credentials and WS connection identities do not.

### Failure history and client projection

Gateway snapshot version 2 preserves typed Failure history and current runtime error separately, including optional turn identity and structured `PhiFailure` payloads. Gateway TUI uses ordinary ConversationView recovery; Web derives bounded `error`-role summaries from Failure entries. Neither renders the current runtime error as a duplicate history message. Polling failures remain local notices and trigger retry, not Agent Failure entries.

## Transport and security

HTTP serves typed Session operations, filtered history/execution views, local device management and Web assets; `/ws` uses the versioned `phi.remote.v1` subprotocol. The listener binds IPv4 loopback. Deployment-owned `gateway/config.json` accepts optional `port`, `publicOrigin`, `trustedProxies` and `expiry`, rejecting unknown fields and invalid values. `expiry` is a nonnegative integer in days (default 30; 0 disables expiry). CLI `--port` overrides only the port. The public origin must be an HTTPS origin without credentials, path, query or fragment; trusted proxies are exact IPs, not CIDRs.

Shared ingress validation matches Host and any supplied Origin to the same configured entry. Public ingress requires a trusted proxy peer and one overwritten `X-Forwarded-For` IP. `Forwarded`, other `X-Forwarded-*` and `X-Real-IP` headers are rejected. Local ingress requires a direct IPv4 loopback peer without forwarding metadata; public/proxy ingress never acquires local management authority. REST mutations additionally require Origin and exactly one `Content-Type: application/json`. Bounded request bodies, separate ordinary/control capacity and timeouts protect the transport.

Protected REST accepts the current approved device bearer credential independently of an online WS connection. WS device authentication additionally requires fresh connection-bound Ed25519 proof. Both use the same durable pairing authority and recheck current authorization before protected delivery; REST also rechecks after body collection and before invocation. Revocation and credential replacement invalidate prior authorization. Accepted HTTP work remains supervised after timeout or waiter loss.

Browser first use requires explicit pairing-token entry, device-key proof and approval of a specific pending request by the local operator. `phi gateway devices approve <request-id>` selects it explicitly; without an ID, the CLI queries pending requests and approves only when exactly one exists. Zero requests cause no approval; multiple requests list their IDs and require rerunning with an explicit ID. Pending requests expire after ten minutes. The waiting browser receives the decision, then opens a fresh proof-bound claim connection to obtain its credential; rejection/expiry never automatically enrolls again.

Remembered device identity/credentials support proof-bound reconnect without the pairing token. Explicit credential recovery instead requires the pairing token, the same private key and a still-approved device identity; it replaces the current credential without another approval. Expiry, revocation or key loss requires enrollment and approval again. Approval expiry blocks device REST and WebSocket access, not pairing-token or local management use. The pairing token alone does not authorize business REST access. Browser credentials cannot manage devices or stop the Gateway. Native local management uses a separate per-start credential on strict local ingress.

A config-scoped endpoint descriptor and process lock support discovery and reuse. Discovery requires a successful authenticated Session-list response from the recorded endpoint. Attaching clients do not change the service's working directory or accepted configuration.

Discovery records and credentials use the protected Gateway directory under the Phi config directory. `pairing.json` stores the stable Gateway identity and pairing token, pending requests, approved public keys, approval times and device credential digests, never device bearer plaintext. Approval fixes `expiresAt = approvedAt + expiry × 86400` (`null` for no expiry). Policy changes apply only to new approvals; reconnect, credential recovery and restart never renew approval. Unknown fields, invalid values and missing deadlines are rejected. `gateway.lock` excludes a second authority process. Authorization transitions validate and persist a complete candidate before reporting success; external state changes or uncertain post-replacement persistence fail closed until restart. Durable Gateway storage currently requires Unix and rejects unsafe path types, ownership, permissions and hard-linked files.

`phi-remote::deployment` publishes a clean local endpoint, a clean browser URL and an explicit local credential in protected `endpoint.json`. Its registration retains directory ownership through application finalization and retires only its own endpoint. Endpoint retirement checks the selected directory and process lock, not HTTP availability alone. No query/fragment URL-token entry is supported. Credentials do not belong in launch URLs, ordinary diagnostics or Session metadata. `phi gateway token` reveals the persistent pairing token only with interactive terminal input/output; redirected reveal is rejected.

Provider credentials, permission runtimes, extension VMs, MCP managers, sandboxes and unrestricted Manager handles are not wire payloads. Device authorization permits supported hosted-Session workflows, not local device/Gateway management, settings/auth storage writes, shell administration or unrestricted SDK access. Service stop requires local management authority. Unsupported interaction approval returns an error; MCP elicitation supports cancellation.

## Lifecycle and process supervision

| Action | Embedded local | Gateway deployment |
| --- | --- | --- |
| Quit TUI | Owning process finalizes its Sessions | Client disconnects |
| Close/refresh Web | Not applicable | Client disconnects |
| Change client selection | Local transition policy | Client view changes |
| Explicitly close hosted Session | Not applicable | Application stops/finalizes that Session |
| Stop Gateway | Not applicable | Host closes admission and finalizes hosted Sessions |

The server owns transport connection tasks. The SDK owns Agent execution tasks. After the last client disconnects, execution can continue and pending interactions are available to later clients. Client count does not control Agent lifetime or permission approval.

The application closes newly hosted Sessions after five minutes unless a first user submission has been accepted. Empty means no accepted first user submission, not merely an idle Agent or an absent saved file. Polling, editor drafts, configuration operations and client selection do not extend this one-shot deadline. Accepted submissions permanently exempt the Session, independently of execution status or persistence; resumed history is exempt. Expiry finalizes and removes the hosted Session, rather than merely disconnecting a client, and is serialized with submission admission, including after the submitting client disconnects.

Gateway owns transport, scheduler and plugin tasks. The host must continuously poll `Gateway::wait()` to collect results, apply failure policy and drive plugin controls; there is no autonomous supervisor. Cancelling that waiter pauses supervision without cancelling owned work. Scheduler failure initiates Gateway shutdown; ordinary plugin runtime errors remain diagnostics and do not fail an otherwise clean close. Plugin panic, cleanup failure and forced cancellation prevent a clean close, with task identity retained.

Service shutdown rejects new transport admissions and signals plugins while draining supervised HTTP/WS work. `Gateway::close()` allows a 40-second grace, then aborts and joins plugin, HTTP-server and scheduler tasks and reports failure; abort does not establish successful plugin cleanup. `Drop` is best effort, not awaited teardown. Main coordinates application/SDK Session finalization, persistence errors, MCP shutdown and Gateway closure. SDK execution teardown remains SDK-owned. Server termination and partial startup failures enter the same application finalization path.

Foreground and service-manager-launched Gateways use the same host path. Process supervision owns background launch.

## CLI and Web entry semantics

Gateway availability and lifecycle are controlled by service commands and process supervision. Phi `RuntimeConfig.extensions` does not control Gateway transport or remote plugins. Deployment-owned remote plugin selection enables platform clients, not the native REST/WS transport.

| Command | Behavior |
| --- | --- |
| `phi` / `phi --local` | Embedded runtime and ordinary TUI |
| `phi gateway` | Reuse an existing service, or host a foreground service without a terminal UI |
| `phi gateway status` | Inspect the local service |
| `phi gateway stop` | Stop the local service |
| `phi gateway token` | Explicit terminal-only pairing-token reveal |
| `phi gateway devices list` | List pending requests and approved devices |
| `phi gateway devices approve [request-id]` | Approve an exact request, or the sole pending request when no ID is supplied |
| `phi gateway devices reject <request-id>` | Reject an exact pairing request |
| `phi gateway devices revoke [device-id]` | Revoke an exact device, or the sole approved device; multiple candidates list IDs for an explicit retry |
| `phi --gateway --session <id>` | Ordinary TUI attached to a hosted Session |
| `phi --web` | Open the running Gateway's Web page |

`phi --web` and `phi --gateway` require a running Gateway and share initial Session selection. Without an ID, the client selects the first hosted Session or creates one in the CLI process's current directory when the registry is empty. `phi --gateway --session <id>` selects the specified hosted Session or returns an error.

Built-in remote integrations live under `phi-remote/plugins/`, independently of Phi extensions. Deployment-owned `gateway/config.json` selects integrations through a `plugins` array of `+builtin:name` / `-builtin:name` entries, without a total plugin switch. The last entry for each plugin wins. All plugins default to disabled; an omitted or empty array enables none. Explicit `+builtin:web` enables browser assets; disabling it removes static browser routes, not native REST/WS, device authorization or TUI access. Telegram and Feishu parameters live in top-level `telegram` and `feishu` objects in the same protected config. Selected clients require valid options; providing options alone does not enable a client. Complete configuration validation precedes authority creation; protected plugin storage is prepared before endpoint publication. Gateway composes Web routes behind shared ingress checks and starts selected messaging clients after publishing its endpoint. No installation, discovery, dynamic loading, hot replacement or standalone bridge command is provided.

Telegram and Feishu remain independent clients of the same Gateway, even though Gateway supervises their tasks in its process. Each retains its own device key, credential, REST requests, proof-bound WS access and private state under `<gateway>/plugins/<id>/`. First startup uses the existing enrollment, exact own-request local approval and claim protocol. Local operator material is short-lived; ordinary work uses only that client's credential. Reconnect never automatically reapproves expired or revoked devices. Platform identity/allowlists remain client-owned checks, not multi-tenant Session isolation. Plugins never receive SDK/application collaborators or bypass transport authorization.

Shared plugin code owns protected client storage, pairing and bounded HTTP/WS protocol handling, not a shared client identity. Platform modules own routing, recovery and delivery semantics: Telegram keeps explicit recovery for missing hosted Sessions; Feishu can resume saved bindings without replaying interrupted input. Both preserve at-most-once delivery and uncertain-mutation handling. A client failure is recorded and stops that client without stopping Gateway, other clients or Agents. Gateway shutdown signals clients and joins their connection/presentation cleanup alongside transport drain; retained failures and cleanup ownership survive cancellation of a waiter. Plugins do not own Agent execution or Session persistence.

Gateway access is selected before embedded runtime preparation, Session locks or provider resource loading. Attaching clients do not apply provider/model/settings overrides.

The Web page displays a copyable command for its selected Session:

```bash
phi --gateway --session <id>
```

Users run it in a system terminal on the Gateway host, including after SSH login. The page does not launch a terminal, register an OS URL handler or embed a PTY. Browser opening belongs to the CLI.

The Web UI selects a Session from the URL fragment's `session` field, or the first hosted Session. It sends root text through `/message` with FollowUp dispatch, sends scoped messages to selected child Agents and answers permission questions. Session creation/resume/close, runtime configuration/reload and cancellation remain API capabilities without Web controls.

The top-right device-access entry uses status text and a colored dot, opening a pairing/credential dropdown on click. Authentication state (`Paired`) is distinct from the OfficeView connection/freshness status (`CONNECTED · PHI` or stale view). The browser stores its Ed25519 key pair, pinned Gateway identity and credential in per-origin localStorage; HTTPS or a loopback secure context and working site storage are required. Disconnect retains the saved identity and credential. Forget deletes the complete local device record and prompts the user to request host-side revocation; it does not revoke approval.

Gateway's built-in Web plugin serves assets from `phi-remote/plugins/web/dist/public/`. Source changes require `pnpm build` in `phi-remote/plugins/web`. Service startup accepts missing Web assets; API and native TUI access do not depend on them. Missing browser resources return HTTP 404 without stopping the Gateway. With the service running, `phi --web` offers terminal setup for missing assets and exits after setup; a subsequent invocation opens the page. Serving built assets requires no Node.js runtime.

TUI and Web both submit to the hosted Session and observe its conversation, execution output, tool state and pending interactions. Concurrent submissions use SDK admission semantics. Shared canonical state does not imply identical layouts, synchronized drafts or zero transport latency.

Print and version-2 stdio RPC own their standalone runtime and protocol contracts. Gateway does not expose stdio RPC as a multi-Session attachment service.

## Plugin API boundary

`phi_remote::plugins` exposes `PluginRegistry`, `Plugin`, `PluginContext` and `PluginControl`, plus `PluginClient` and `PluginWatch` on Unix. Hosts register optional HTTP routes and/or background tasks, then pass the registry to `deployment::start_with_plugins`. `Plugin::with_task` registers a repeatable factory creating a fresh run on each invocation; every run must release its resources before returning. Registration does not execute plugins, create storage or connect to Gateway. Deployment configuration selects builtins only; custom Rust registrations come from the host.

A running plugin requests a client through `context.client(identity_directory).await`. The directory is owner-only; client and watch handles retain its lock. Builtins share one protected store and lock for device identity and business state. The factory owns pairing and authentication, not business retries. Clients expose supported Session operations, submission, permissions and observations, never device keys, local management credentials or SDK/application collaborators.

Plugins own connection timing, platform access checks, presentation, history interpretation, recovery, retries and business state. Non-API plugin HTTP routes require their own authentication. Plugins are compiled integrations, not Phi extensions or dynamically loaded packages.

### Background plugin lifecycle

```text
host continuously polls Gateway::wait()
                      |
PluginControl --------+--> Gateway lifecycle owner
  stop / restart              |
                              +--> close this run's scheduler scope
                              +--> signal this run's shutdown
                              +--> join plugin-owned cleanup
                                     |
                                     +-- success --> stop: remain stopped
                                     |              restart: new run + scope
                                     +-- timeout --> retain task; cancel restart intent
                                     +-- failure --> block restart

sibling plugin runs and static HTTP routes remain unaffected
```

`gateway.plugin_control()` submits named `stop` / `restart` requests to the `Gateway::wait()`-driven lifecycle owner. Controls affect background runs only, not statically composed HTTP routes, Agent execution or persistent plugin selection. No CLI commands or remote endpoints expose these controls. Accepted requests survive cancellation of their waiter; conflicting requests for a plugin with a pending control request are rejected.

Each run has its own `PluginContext::shutdown()` signal and scheduler scope. Stopping closes that scope and signals the run without affecting siblings. Retained scheduler handles stay closed; restart creates a fresh signal and scope. The shared Gateway scheduler owns wakeups, not plugin business execution. Plugins own and join their child work.

Restart waits for successful completion of the old run and cleanup before launching another; success confirms launch, not readiness. A stop observer times out after 30 seconds, retains task custody and cancels pending restart intent without aborting the task. After successful cleanup, a later explicit restart is allowed. Cleanup failure or panic blocks replacement. Gateway shutdown cancels pending restarts and rejects new controls.

### Idle-aware refresh

```text
plugin running
      |
      +-- idle: no tracked work --> cancel refresh deadline
      |                                  |
      |                            wait for new work
      |
      +-- pending work --> refresh immediately
                                |
                          GatewayScheduler wakeup
                                |
                          refresh every 2 seconds
                                |
                                +-- still pending --> rearm
                                +-- idle ----------> cancel
```

Idle suspends periodic state refresh, not the plugin. Platform ingress and long polling retain their own timing and cancellation policies. The scheduler only wakes consumers; plugins execute the refresh work.

## References

- [OpenClaw Gateway architecture](https://docs.openclaw.ai/concepts/architecture)
- [OpenClaw TUI: Gateway and embedded local modes](https://docs.openclaw.ai/web/tui)
