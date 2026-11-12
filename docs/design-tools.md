# Tool semantics and boundaries

Tool design separates four questions:

| Layer | Question |
| --- | --- |
| Registration | Which tool contracts exist in the runtime? |
| Availability | Which tools are usable in the current execution scope? |
| Exposure | How does the model discover and reach those capabilities? |
| Authorization | Is this specific invocation approved? |

Execution follows admission and authorization. These are responsibility layers, not a sequence of persistent tool states. Exposure projects capabilities for model access; final dispatch independently checks availability, entry-point eligibility and invocation authority.

This document defines the shared tool model. Ordinary and MCP tools use `Direct`, `Deferred` and `Hidden` exposure. MCP configuration uses `exposure` and `toolExposure`; the former `directTools` and `disabledTools` fields are removed, without compatibility aliases. See [SDK tool registry](design-sdk.md#tool-registry), [permission and sandbox](design-sdk.md#permission-and-sandbox-model), and [Prompt and Agent tool settings](design-interactive.md#prompt-and-agent-tool-settings).

## Design principles

- **Existence is not authority.** Registration establishes a contract; it grants neither capability use nor invocation approval.
- **Capability boundaries precede presentation.** Availability constrains every supported call path. Changing how a model discovers a capability cannot enlarge that boundary.
- **Access is entry-point-specific.** Direct model calls and tool-to-tool calls have explicit target sets. They need not present or invoke identical sets.
- **Presentation is not admission.** A declaration or discovery result grants nothing. Execution rechecks current availability, exposure-based reachability and invocation authority. `Hidden` withdraws access; it must never be implemented as declaration omission alone.
- **Composition preserves authority.** Calling through another tool must not widen the caller's Agent/scope capabilities.
- **Conceptual layers do not require parallel machinery.** Reuse resolved contracts and governed execution; do not invent a catalog, policy object or persistent state for every distinction.

## Architecture

```text
Registered contracts / delegated owner contracts
    |
    v
Availability: current Agent / execution-scope capability boundary
    |
    +-- Direct model entry point
    |     +-- Exposure: provider declarations and guidance
    |     `-- Invocation: targets supported by the direct entry point
    |
    `-- Tool-to-tool entry point
          +-- Callable: targets supported by scoped nested invocation
          `-- Exposure: discovery/guidance through Codemode

Invocation through any supported entry point
    -> recheck applicable Availability and entry-point eligibility
    -> Authorization of the concrete operation
    -> execution with independent effect constraints
    -> required finalization and canonical result handling
```

These branches are architectural access paths, not disjoint sets. A capability may be reachable through multiple paths. Exposure and invocation are distinct projections of the same applicable capability boundary; Exposure is not an obligatory stage in dispatch.

**Callable** names the target set of an execution entry point, including tools callable from Codemode. It is an entry-point-specific capability projection, not a fifth layer, another registry, or a permission decision. Registered does not imply Callable, and Callable does not imply approved. Omission from direct declarations does not prohibit a capability supported by an indirect entry point.

The SDK resolves Agent contracts through one ToolRegistry, including captured MCP tools. Cold deferred discovery adds validated contracts to a scoped registry. Phi Agent selection does not replace MCP target governance.

### Ownership and extension boundary

AgentInstance owns execution and the canonical live Transcript. The registry owns resolved Agent tool contracts; delegated owners retain their capability contracts. PermissionRuntime evaluates invocation permission. Entry-point adapters own declarations, discovery and input conversion, not independent execution authority.

Tool-to-tool invocation belongs within SDK-owned execution. Codemode receives a scoped host, not independent Registry/ToolSet authority. Nested calls inherit the caller's Agent identity, turn scope, permission constraints and cancellation/lifetime boundary. Discovery and dispatch use the same eligibility rules, with live admission rechecked for each invocation. This does not expose a general nested-call API to portable Rust tools or extensions.

## 1. Registration: the capability catalog

Registration establishes the runtime's tool contracts: name, schema, implementation, source, owner, governance, prompt and scheduling metadata, authorization adapter, and durable-finalization metadata. A declared `terminate_on_success` hint affects only the agent loop after successful execution; it neither authorizes a call nor suppresses its ToolResult.

`ToolCatalogBuilder` validates declarations and resolves collisions into one `ToolRegistry`. Explicit overrides replace the whole contract; cross-extension collisions fail. The registry owns the Rig ToolSet and is the source for definitions, guidance, availability and dispatch, not one of several parallel catalogs.

- **Registered** means a contract exists in the resolved catalog.
- **Registration lookup** answers only whether that contract exists.
- Registration grants neither availability nor invocation permission. A withdrawn tool may retain its contract for management and runtime replacement without appearing in Agent discovery.

Rust and extension authoring enter through supported declaration/registration APIs. Clients receive supported descriptions and observations, not mutable registry or ToolSet access.

## 2. Availability: the execution scope

Availability resolves the tools usable in the current execution scope from the catalog and applicable policies:

```text
registered catalog
  + global tool switch
  + Agent selection and exclusion
  + execution mode restrictions
  + outer-turn removals
  + owner authority
  + shared exposure withdrawal (`Hidden`)
  -> available tools
```

| Term | Meaning |
| --- | --- |
| Selected | Input policy selecting tools |
| Excluded | Input policy explicitly removing tools |
| Available | Resolved result: usable in this execution scope |
| Unavailable | Resolved result: not usable in this execution scope |

Exclusion is one cause of unavailability, not a separate peer state. A tool may remain registered while unavailable. Availability does not guarantee authorization of any input.

Internal availability predicates should use `is_available`, not `is_allowed`. A registration-existence predicate, if needed, must remain distinct. Selection and exclusion names belong here; Allow/Ask/Deny belong to permission decisions.

Current policy boundaries remain unchanged:

- `no_tools` disables tools across owners.
- Plan removes canonical Write/Edit from availability; it is not general filesystem isolation.
- Agent selection/exclusion narrows Phi-governed tools, not delegated MCP tools.
- Outer-turn removals apply until their execution scope retires.
- Delegated tools also require their owner's availability decision.
- MCP targets with `Hidden` exposure are withdrawn from every Agent entry point.
- Manual calls and extension overrides cannot restore unavailable tools.

Agent and Prompt configuration keeps its established fields. Boundary parsing translates those fields into internal availability intent without granting permission. MCP exposure is resolved through the shared tool contract.

## 3. Exposure: model access

Exposure describes supported access paths. Declaration is the concrete schema sent to the model; discovery obtains descriptions through an indirect entry point.

| Exposure | Direct declaration and prompt guidance | Indirect discovery and calls |
| --- | --- | --- |
| `Direct` | Included when available | Supported when available and eligible for that entry point |
| `Deferred` | Omitted until loaded by ToolSearch; always omitted from Codemode's inline catalog | Supported when available and eligible for that entry point |
| `Hidden` | Omitted | Omitted and rejected, even when the caller knows the name |

Ordinary tools default to `Direct`. `Deferred` does not mean disabled or permission-approved. `ToolSearch` searches deferred metadata and loads matches for direct calls from the next model request without changing their exposure. Loaded names persist as branch-local Transcript events, survive compaction and restore from the selected branch; current availability and permission still apply. Codemode discovery does not load direct declarations.

`Hidden` means withdrawn, not merely visually hidden. Its effective projection is unavailable to Agent execution. It may retain an internal registration or MCP metadata for administration; those facts cannot make it discoverable or callable. Do not maintain a second mutable disabled state alongside the resolved policy.

These values are not a state machine. Keep the distinctions explicit:

- **Registered**: a contract exists.
- **Available**: current capability constraints permit use.
- **Declared**: the current provider request contains its schema.
- **Discoverable**: a supported indirect entry point can describe it.
- **Callable**: a particular entry point can invoke it.
- **Approved**: permission evaluation permits the concrete invocation.

An available deferred tool may be called indirectly, or directly after ToolSearch loading. An available direct tool need not be Codemode-callable: recursion, Tape mutation and durable-finalization restrictions still apply. An unavailable tool is neither discoverable nor callable regardless of exposure. Neither direct declaration nor indirect discovery grants permission.

### Unified MCP policy

MCP tools use the same resolved exposure metadata as other tools. MCP owns server identity, transport, metadata and target availability; it must not own a parallel direct/deferred access model. Build registration, provider declarations, prompt guidance, ToolSearch and Codemode discovery from the captured contract and shared policy.

MCP uses the shared exposure values, serialized as `direct`, `deferred` and `hidden`:

```json
{
  "settings": { "exposure": "deferred" },
  "mcpServers": {
    "example": {
      "command": "example-mcp",
      "exposure": "deferred",
      "toolExposure": { "search": "direct", "delete": "hidden" }
    }
  }
}
```

`exposure` is a default for that server's tools, not server activation or connection status. `toolExposure` overrides individual tools by exact original server-tool name, not formatted Agent alias. Wildcards are unsupported; loading does not change exposure configuration.

Resolution precedence is server `toolExposure[name]`, server `exposure`, global `settings.toolExposure[name]`, then global `settings.exposure`. The global default is `Deferred`; ordinary non-MCP tools default to `Direct`. A hidden default may expose a whitelist through per-tool overrides. Once resolved, `Hidden` cannot be bypassed by an invocation path or permission answer.

Resolve once for each captured tool contract. There are no separate direct/disabled lists, catalog modes, or Codemode-dependent exposure policies. Configuration changes prepare a replacement contract rather than mutating a second runtime switch.

Migration preserves effects: former `directTools: true` becomes `exposure: "direct"`; `false` becomes `"deferred"`; a direct-name list becomes direct per-tool overrides on a deferred default; disabled targets become hidden overrides. Where old global/server lists overlapped, materialize the previously effective policy under the new precedence. Old fields are rejected rather than silently ignored.

Required effects:

- A direct MCP tool is declared like any other available direct tool. It also remains reachable through eligible indirect entry points.
- A deferred MCP tool is initially absent from direct declarations and guidance. ToolSearch can load it for direct calls; Codemode can independently discover and call it.
- A hidden MCP tool is absent from provider declarations, prompt tool lists/guidelines, Codemode's inline catalog, `ALL_TOOLS`, search and descriptions. Direct and nested calls reject it before effects, including calls through aliases or remembered names.
- `Hidden` is capability withdrawal, not a permission `Deny` rule. Approval, Yolo, hooks, discovery and loading cannot restore it.
- Management views may show disabled tools as disabled. They are not Agent-facing discovery.
- Codemode enablement does not change MCP exposure, ToolSearch loading or permission. Never silently promote deferred tools or auto-enable Codemode.
- Agent selection governs Phi-owned tools, including ToolSearch; MCP target policy remains owner-governed. Removing ToolSearch does not disable MCP, withdraw loaded declarations or grant target authority.

There is no `MCP` model tool. Agent and Prompt tool settings reject the retired `MCP`/`mcp` names and individual MCP tool identities. Registry dispatch rechecks current owner availability; a captured or loaded declaration is not permanent authority.

Exposure or metadata changes follow validated runtime-generation replacement, including deferred-only changes. A failed candidate preserves the active generation. Do not mutate accepted declarations in place or create separate direct-only and deferred-inclusive policy stores. Connection status, metadata freshness and exposure remain distinct.

### MCP connection policy

`Deferred` has the same access meaning for ordinary and MCP tools. Only MCP has a connection lifecycle; exposure itself is not connection state.

- Under the default `lazy` lifecycle, direct-tool metadata may trigger background connection because schemas are needed for direct declarations. A deferred-only server is not connected at startup merely to discover tools.
- ToolSearch, Codemode and restored loading prepare deferred metadata on demand; fresh usable cache avoids a discovery connection, while an actual call still connects as needed. Cold discovery may inspect multiple deferred servers concurrently.
- `eager` and `keep-alive` remain explicit lifecycle overrides and may connect without a tool call, regardless of exposure.
- Cold deferred contracts are validated through `ToolCatalogBuilder` into a scoped registry. It inherits the existing availability/permission policy and outer-turn ToolSet identity. Each resolved entry retains its dispatch set. Codemode discovery leaves provider declarations and prompt guidance unchanged; ToolSearch loading projects them on the next request. Ordinary generation replacement observes metadata changes separately.

This preserves the on-demand behavior previously provided by `directTools: false` without assigning a second meaning to ordinary `Deferred` tools.

### Reference: pi

[Pi tool exposure](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/extensions.md#tool-exposure) and [MCP exposure](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/mcp.md#control-tool-exposure) separate registered, active and callable tools, and share exposure between ordinary and MCP tools. Its `hidden` value is unreachable, not hidden-but-callable.

Phi adopts shared exposure and withdrawal semantics, not pi's complete enum or active-set machinery. Phi's `Deferred` supports ToolSearch loading and independent Codemode discovery, without Codemode inline listing. Do not infer identical behavior from the shared name.

## 4. Authorization: the invocation

Authorization evaluates a concrete operation and its evidence, not merely the existence of a tool name. Tool contracts declare where semantic authorization occurs: generic invocation, resolved path, command evidence, MCP operation, Agent operation, or specialized Memory/Tape operation.

Distinct boundaries remain distinct:

- **Permission** resolves user rules and scoped approval interactions.
- **Owner/capability authority** constrains supported capabilities.
- **Sandbox and protected roots** independently constrain effects.

Permission approval cannot expand availability or bypass capability, sandbox or protected-root checks. Yolo bypasses user permission rules only.

| Term | Meaning |
| --- | --- |
| Allow / Ask / Deny | Permission decisions |
| Preapproval | Invocation approval intent carried by an outer-turn submission |
| Grant | Confirmed, applied authorization, such as a Session AllowAlways grant |

Preapproval parsing activates nothing. Execution activates the intent in its outer-turn scope; permission evaluation uses it only to resolve Ask. It cannot override explicit or fallback Deny, restore removed tools, create registrations, persist Session grants, or authorize child execution. Completion, failure and cancellation retire the scope.

Input transformations retain normal authorization and sandbox enforcement. Bash authorization evaluates original and transformed command evidence using the stricter result.

## Internal and external contracts

| Layer | Internal terminology | External contract |
| --- | --- | --- |
| Registration | Registration, catalog | Supported tool declarations and registration APIs |
| Availability | Selection, exclusion, availability | Established Agent/global tool configuration |
| Exposure | Exposure, declaration, discovery | Provider schemas and supported indirect entry points |
| Authorization | Preapproval, grant, permission decision | Established Prompt YAML, permission settings and approval interactions |
| Execution | Dispatch, execution, finalization | Results and events |

External field names need not match internal names. Convert at the boundary rather than carrying ambiguous configuration vocabulary through every layer.

In particular, Prompt `allowed-tools` remains the established external field for outer-turn preapproval; it does not select tools. Prompt `disallowed-tools` remains whole-tool removal. Agent `tools` and `disabled-tools` remain availability settings. Their grammar is shared, but their effects are not.

Implementation naming distinguishes availability from permission and preapproval from confirmed grants. Registry and owner-authority predicates use `is_available`; the owner's dispatch-time capability check uses `ensure_available`, not invocation authorization. Provider declarations use `provider_declarations`; ToolSearch and Codemode use `discovery_registry`, while shared capability filtering uses availability names. Permission checker adapters use `check_*_with_preapprovals` and match both Session grants and turn preapprovals through `has_matching_approval`.

`TurnToolPolicy.allowances` and `ToolRules::into_allowances()` retain their established SDK names for preapproval intent. These compatibility names do not define a separate authorization concept. Terminology changes must not silently alter serialized fields, public SDK contracts or behavior.

## Tool composition

Codemode uses the ordinary governed tool pipeline for validation, hooks, permission and scheduling. It cannot call itself or ToolSearch, mutate Tape anchors, or invoke tools requiring durable finalization. These eligibility restrictions apply to discovery as well as dispatch and do not remove direct access to those tools.

Nested calls carry parent-call identity and separate audit records; raw nested results do not become provider ToolResults. Only the parent script's output enters provider tool history. Cancellation stops unfinished calls but cannot undo applied effects. Successful store writes follow the current Transcript branch.

Shared MCP exposure preserves these contracts. A general extension-facing nested-call API, pi's `model-only`/`codemode` variants and wildcard exposure configuration are outside its scope.

## Execution admission

```text
invocation and transformed input
  -> current registry availability
  -> delegated owner authority, where applicable
  -> exposure reachability and entry-point eligibility
  -> semantic permission authorization
  -> execution with independent effect constraints
  -> durable finalization, where applicable
  -> result and history
```

Different tools enforce semantic permission and effect constraints at their declared adapters; this flow does not require moving every check into registry dispatch. After finalization, the loop ends automatic continuation only if every result in a nonempty tool batch succeeded and has the termination hint. A tool error, an unmarked sibling or accepted Steer preserves the relevant model continuation. The result remains in canonical history; later asynchronous feedback uses its own admission path. See [agent-loop scheduling](design-harnes.md#turn-and-agent-loop-model).

The core invariants are:

- Registered does not imply available.
- Available does not imply permission-approved.
- Declaration omission does not imply unavailability for deferred capabilities.
- `Hidden` withdraws all Agent access; it is not declaration omission alone.
- Exclusion cannot be undone by preapproval or a permission grant.
- Discovery and alternate call entry points cannot bypass invocation governance.

## MCP refactor acceptance

Verify the business boundary across ToolSearch loading, direct model calls and Codemode:

- Exposure retains global inheritance and server default/per-tool overrides; removed configuration fields fail validation.
- Disabled targets never appear in Agent-facing projections or execute, including when also selected as direct.
- Deferred targets remain indirectly discoverable and callable; ToolSearch loading declares them on the next request. Cold deferred-only servers do not preconnect under `lazy`; discovery and calls connect as needed.
- `no_tools`, owner availability, transport restrictions and invocation permissions hold on every path.
- Codemode enabled/disabled, ToolSearch selected/removed, replacement failure and resumed history do not bypass these rules.
- Property tests vary exposure and availability: hiding or excluding a target can only remove reachability; loading cannot expand availability or grant permission.
