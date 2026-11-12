# Messages API design

## Intent

Messages API is Phi's client-side, provider-neutral capability for generating one model response from explicit input. It does not create an AgentInstance, run an Agent loop, execute tools or own conversation history.

Phi supports two execution units:

- **Model generation**: provide instructions and messages; receive the next model response.
- **Agent execution**: provide a task to an AgentInstance; its AgenticLoop manages model/tool rounds through completion.

Direct SDK callers can use Messages for classification, extraction, evaluation and synthesis over explicit input without creating an AgentInstance. Reading files, modifying code and other autonomous tool-driven work remain Agent execution.

Related: [global architecture](design.md), [SDK](design-sdk.md), [harness](design-harnes.md), [Workflow](design-workflow.md), and [tools](design-tools.md).

## Terminology

| Concept | Meaning |
| --- | --- |
| Messages API | Phi SDK capability for explicit-context model generation; not an HTTP server |
| Generation | One logical request for the next model response, not a complete Agent task |
| Provider attempt | One underlying provider request; retries, where allowed, are distinct attempts |
| Message | Ordered model input or output content, not an Agent mailbox message |
| Generation result | Completed model response with content, finish reason and usage facts |
| Agent turn | Phi's outer execution from dispatched prompt to completion, potentially containing several generations and tool batches |

A generation may contain model reasoning and provider-internal processing. It is not necessarily one forward pass. A generation does not iterate over Phi tools. Transport success does not mean business success, schema validity or completion of an Agent task.

Messages API is distinct from AgentManager inter-Agent messaging and application stdio RPC. It does not introduce another meaning for Session, submission or turn.

## Architecture

```text
                 Direct SDK caller
                         |
                 Messages capability
                 explicit input and policy
                         |
                         v
AgentInstance       Shared generation core       Compaction
      |                    ^                         |
 AgenticLoop               |                         |
      +--------------------+-------------------------+
                         |
                private Rig adapters
                         |
                model provider service
```

The shared generation core owns model resolution, request configuration, provider IO and provider-neutral response conversion. Messages API supplies explicit input. AgenticLoop supplies context projected from canonical Transcript and resolved tool declarations. Compaction supplies its summarization request and applies its own acceptance rules.

These paths share the same model mechanisms, not the same execution policy. There is no second Agent loop, parallel HTTP stack or public raw-provider escape hatch.

### Ownership

| Owner | Owns | Does not own |
| --- | --- | --- |
| Messages capability | Generation admission, explicit input validation, bounded execution and structured outcomes | Agent lifetime, canonical Transcript, tools or Session persistence |
| Shared generation core | Validated model binding, request settings, provider dispatch and normalized response facts | Agent planning or application configuration discovery |
| AgentInstance / AgenticLoop | Agent history, context preparation, tool governance, iteration, recovery and completion gates | A second provider implementation |
| Application runtime | Typed configuration, trusted capability preparation and application persistence policy | Model-generation or Agent loops |
| Rig | Provider request/response adaptation, transport and stream assembly | Phi history, permissions, lifecycle or orchestration authority |

Messages API belongs inside `phi-sdk`. Model contracts remain beside provider mechanisms; the public facade exposes only supported semantic operations and immutable values.

## Client and server boundary

Phi's Messages API is a client capability over its configured providers:

```text
Phi SDK
    -> Phi generation core
    -> private Rig provider adapter
    -> Anthropic Messages | OpenAI Responses/Chat | other configured protocol
```

Phi does not expose an HTTP Messages endpoint or a raw model RPC endpoint. Stdio RPC remains an application adapter for supported Agent operations.

## Generation contract

### Input

A generation request contains explicit model intent, instructions, ordered messages and supported generation settings.

- Model selection resolves against a prepared catalog and configured provider. A trusted caller may prepare a default; requests cannot choose credentials, base URLs or arbitrary provider configuration.
- Instructions are request-scoped. They do not implicitly include AGENTS.md, Agent roles, Skills, Memory, parent history or the launching Agent's system prompt.
- Messages are caller-owned values. Message order is preserved; provider-specific adaptation uses the existing history policy rather than generic-loop special cases.
- Generation settings include output-token limit, optional temperature and typed reasoning intent. Resolve model defaults and explicit overrides through the same rules used by Agent execution.
- A supported native output schema constrains generated JSON. Schema support is capability-checked; unsupported schemas or protocols fail explicitly, without silent prompt-only fallback.
- The request includes no Session identity, branch selection, working-directory override, permission grants or executable collaborators.

The standalone surface accepts text instructions and user/assistant text messages, with optional native JSON output schema. Tool declarations/results, multimodal input, provider-hosted tools and arbitrary provider parameters are outside this surface. Reject unsupported fields and content before dispatch; do not silently discard them.

The shared core still supports the content and tool declaration forms required by the existing Agent path. A smaller standalone surface must not reduce Agent capabilities or lossy-convert existing reasoning/tool history.

Request validation rejects empty message lists, invalid content, unsupported model/settings combinations and exceeded resource bounds. Do not interpret assistant messages as authorization. Provider-specific prefill and system-message placement are capabilities, not universally accepted conventions.

### Result

A completed generation returns structured facts rather than only a String:

- Ordered content blocks, preserving supported text and reasoning metadata.
- A normalized finish reason, retaining an unknown/provider-specific reason without pretending it means success.
- Requested/resolved model identity and provider-reported model identity when available; do not conflate them.
- Usage and derived cost where known, with explicit availability rather than treating missing metrics as measured zero.
- Provider message, response and request identifiers when available, kept distinct from local generation/attempt correlation.
- Parsed JSON only when requested output passes parsing and schema validation.

A text convenience view may derive from content; it is not another canonical response. Never discard content or finish metadata merely because a caller expects text. Public results do not expose credentials or unfiltered provider payloads.

Length-limited output, filtering, refusal and unknown termination remain distinguishable. Preserve the available reason and content; a provider that does not classify a condition must not gain a fabricated classification. A completed but truncated response is not an ordinary full answer. Schema validation does not establish factual or business correctness.

The standalone surface performs no tool execution. An unexpected tool call is an explicit unsupported-output outcome, not text to ignore or authority to dispatch a tool.

### History

The caller supplies all context needed for each generation. The capability does not append input/output to an Agent Transcript, mutate a Session, remember prior requests or automatically compact context.

A caller may construct the next request from previous results. Content not accepted by the current input surface cannot be silently round-tripped as plain text; preserve it or reject unsupported replay. Stateless here means no service-owned conversation history, not an absence of provider caches or reusable connections.

Generation results may be consumed by an Agent or stored by an application through existing supported operations. Generating a result alone does neither.

## Semantic contracts

This section defines domain names, field meanings and operation boundaries. Rust blocks describe the semantic data model, not public field access. Fields stay private; validated constructors/builders and immutable accessors expose only the documented intent and observations. Supporting derives, imports and synchronization fields are omitted.

The public facade is `phi_sdk::messages`. It curates typed Rust Messages contracts, not serialized bridge DTOs; applications own any persistence or wire projection. Concrete provider types remain private. `Phi::messages(options)` prepares the capability without preparing an AgentInstance. This is factory preparation, not a universal application facade.

### Core vocabulary and Claude correspondence

| Phi contract | Meaning | Claude reference / deliberate difference |
| --- | --- | --- |
| `Messages` | Reusable, policy-bound generation capability; no history or Agent identity | Client SDK `client.messages`, not Agent SDK `Query` |
| `MessageRequest` | Explicit next-response intent | `MessageCreateParams`; Phi exposes a narrower provider-neutral subset |
| `InputMessage` | Caller-supplied user/assistant text, not a submission or mailbox message | `MessageParam`; no Session, routing priority or human-origin claim |
| `GenerationSettings` | Model-generation settings, independent of Agent execution limits | Messages `max_tokens`, `temperature`, thinking controls; no `max_turns` |
| `GenerationReport` | Facts observed from one provider attempt, including incomplete output | Messages content/stop/usage; interrupted output is not a successful Message |
| `MessageResponse` | Completed generation accepted by the standalone surface | Client SDK Message plus locally validated JSON; not Agent SDK `SDKResultMessage` |
| `GenerationId` | Local identity of one admitted logical generation | Not Claude message ID, Agent ID, Session ID or WorkflowRunId |
| `GenerationAttemptId` | Local identity of one actual provider attempt | Allows Agent retries without merging responses or accounting |
| `MessageError` | Failure with bounded diagnostic and available generation evidence | Distinguishes API failure, interrupted output and local validation |
| `GenerationCore` | Private shared model mechanism | Reuses Rig CompletionModel; no claim about Claude Code internal classes |

Claude Code's `SDKAssistantMessage.message` is documented as an Anthropic `BetaMessage` containing content, model, stop reason and usage. Its outer envelope adds Agent/Session provenance. `SDKResultMessage` instead describes the end of an Agent loop and aggregate usage/cost. Phi preserves this separation: a generation result does not acquire Agent lifecycle fields merely because an Agent consumed it. See [Assistant message](https://code.claude.com/docs/en/agent-sdk/typescript#sdkassistantmessage) and [Agent result](https://code.claude.com/docs/en/agent-sdk/typescript#sdkresultmessage).

### Capability, policy and request

```rust
struct Messages {
    core: Arc<GenerationCore>,
    policy: Arc<MessagePolicy>,
}

struct MessagesOptions {
    allowed_models: Vec<ModelSelection>,
    default_model: Option<ModelSelection>,
    defaults: GenerationSettings,
    limits: MessageLimits,
}

struct MessagePolicy {
    allowed_models: Vec<ModelSelection>,
    default_model: Option<ModelSelection>,
    defaults: GenerationSettings,
    limits: MessageLimits,
}

struct ModelSelection {
    provider: String,
    model: String,
}

struct MessageRequest {
    model: Option<ModelSelection>,
    instructions: Option<SharedText>,
    messages: Vec<InputMessage>,
    settings: GenerationSettings,
    output: MessageOutput,
}

struct InputMessage {
    role: MessageRole,
    content: SharedText,
}

enum MessageRole {
    User,
    Assistant,
}

struct GenerationSettings {
    max_output_tokens: Option<NonZeroU64>,
    temperature: Option<f64>,
    thinking_level: Option<ThinkingLevel>,
}

enum MessageOutput {
    Text,
    JsonSchema { schema: serde_json::Value },
}
```

`MessagesOptions` is trusted preparation intent; private `MessagePolicy` is its validated, normalized snapshot. They are not independently mutable stores. Clone the capability to share prepared resources, not conversation state. No separate public client builder or injectable backend is required.

Reuse existing `transcript::SharedText` and `models::ThinkingLevel`. `InputMessage` is intentionally not `AgentMessage`: it carries no history identity, usage, Context source, tool authority or Session semantics. The text-only shape is intentional, not a placeholder for arbitrary JSON content.

| Field | Fixed meaning and invariant |
| --- | --- |
| `Messages.core` | Shared private generation mechanism; never returned to callers |
| `Messages.policy` | Immutable validated defaults, selections and ceilings for this capability |
| `allowed_models` | Nonempty, deduplicated configured provider/model pairs; every selection must resolve during preparation |
| `default_model` | Optional member of `allowed_models`; omission in both policy and request is an invalid request, never a catalog-order fallback |
| `defaults` | Trusted generation defaults only; no inherited Agent instructions or transient turn settings |
| `limits` | Positive validated ceilings; callers cannot enlarge them through a request |
| `ModelSelection.provider` | Configured provider identity, not an adapter name, protocol guess or URL |
| `ModelSelection.model` | Catalog model ID within that provider, not an Agent role or display label |
| `MessageRequest.model` | Optional selection override within the allowed set; the resolved choice is captured for this generation |
| `instructions` | Optional top-level system instructions supplied as content; no project resource expansion |
| `messages` | Nonempty ordered text messages; preserve order and reject unsupported replay/prefill before dispatch |
| `InputMessage.role` | Model conversational role only; never authorization or provenance |
| `InputMessage.content` | Nonempty text; do not trim or rewrite caller content during validation |
| `settings` | Per-request overrides; resolve each missing field from policy defaults, then validated model defaults |
| `max_output_tokens` | Positive generation output ceiling, not a total-token/spend budget; zero-token cache warming is outside this contract |
| `temperature` | Finite value accepted by the selected provider/model; omission does not force an artificial universal value |
| `thinking_level` | Existing typed intent resolved through the same model-level mapping/clamping as Agent execution; no duplicate reasoning vocabulary |
| `output` | Explicit text or native JSON-schema expectation; Text does not forbid separate reasoning blocks |
| `JsonSchema.schema` | A bounded validated JSON Schema document accepted by that backend; not arbitrary provider parameters or instructions masquerading as a schema |

Schema validation includes local schema compilation and backend-subset checks before dispatch. Unknown schema support is unsupported, not permission to try a hidden fallback. Existing catalog descriptors alone do not prove native schema support; combine known adapter support and model constraints conservatively, rejecting provider-declared incompatibility explicitly.

### Resource limits

```rust
struct MessageLimits {
    max_input_bytes: NonZeroUsize,
    max_output_bytes: NonZeroUsize,
    max_response_bytes: NonZeroUsize,
    max_schema_bytes: NonZeroUsize,
    max_schema_nodes: NonZeroUsize,
    max_schema_depth: NonZeroUsize,
    max_concurrent_generations: NonZeroUsize,
    max_duration: Duration,
}
```

| Field | Enforcement boundary |
| --- | --- |
| `max_input_bytes` | Instructions and message content combined, measured as UTF-8 bytes before provider adaptation |
| `max_output_bytes` | All retained output content and reasoning metadata, not only visible text; stop collection while consuming the stream |
| `max_response_bytes` | Provider response entity bytes before adapter parsing, including stream framing and error bodies; independent of retained output |
| `max_schema_bytes` | Serialized schema bytes, independently bounded rather than hidden inside the prompt allowance |
| `max_schema_nodes`, `max_schema_depth` | Bound schema traversal/compilation; external reference fetching is disabled and recursive references require explicit bounded support |
| `max_concurrent_generations` | Active attempts across clones of this capability; one attempt holds one permit, never one per consumer |
| `max_duration` | Positive wall-clock duration covering admission wait, provider IO and output validation |

`Messages.create()` does not maintain an unbounded pending queue: the operation remains owned by its awaiting caller. Cloned capabilities share concurrency enforcement. Direct SDK callers own the number of outstanding futures, aggregate input and retained completed responses; per-call limits do not imply caller-wide retention or spend limits.

### Generation identities and observations

```rust
struct GenerationId(Uuid);
struct GenerationAttemptId(Uuid);

struct GenerationIdentity {
    generation_id: GenerationId,
    attempt_id: GenerationAttemptId,
    provider_message_id: Option<String>,
    provider_response_id: Option<String>,
    provider_request_id: Option<String>,
}

struct GenerationModel {
    requested: Option<ModelSelection>,
    selected: ModelSelection,
    reported_model: Option<String>,
}

struct GenerationReport {
    identity: GenerationIdentity,
    model: GenerationModel,
    completion: GenerationCompletion,
    content: Vec<ContentBlock>,
    finish_reason: GenerationFinish,
    usage: GenerationUsage,
}

enum GenerationCompletion {
    Complete,
    Incomplete,
}

enum GenerationFinish {
    Stop,
    ToolCalls,
    Length,
    ContentFilter,
    Refusal,
    Other(String),
    Unknown,
}

struct MessageResponse {
    report: GenerationReport,
    parsed_json: Option<serde_json::Value>,
}
```

Reuse `transcript::ContentBlock` and its reasoning/image/tool identity types as immutable content values; holding them does not commit a Transcript entry. Do not clone the content enum into a second equivalent Messages DTO. The standalone acceptance check admits text/thinking blocks only. Agent execution continues using the full content vocabulary through the same core. An internal schema adapter may use Rig types, but public contracts do not expose them.

| Field / state | Fixed meaning and invariant |
| --- | --- |
| `generation_id` | Assigned at logical admission; cannot be supplied to impersonate an earlier operation or request remote idempotency |
| `attempt_id` | Assigned when dispatch begins; fresh for each Agent retry; standalone calls dispatch at most one attempt |
| `provider_message_id` | Provider's assistant-message identity, used only under that provider's replay rules |
| `provider_response_id` | Response-level identifier; never substituted for message identity |
| `provider_request_id` | Transport/support request identifier; never a Session or correlation authority |
| `GenerationModel.requested` | Explicit request selection; None records default selection, not an unknown resolved model |
| `selected` | Actual validated provider/model pair; model policy, usage attribution and catalog rates use this identity |
| `reported_model` | Provider-reported model string, absent when not reported; do not rewrite `selected` from an unvalidated response |
| `completion` | Whether collection reached verified normal termination; independent of finish reason and business success |
| `content` | Ordered observed content, including bounded partial content on failure; never silently reduced to text |
| `finish_reason` | Normalized provider fact; Unknown when unavailable, including incomplete responses; never infer Stop from EOF |
| `usage` | Available attempt usage and cost estimate, not a session aggregate |
| `MessageResponse.report` | Always Complete; unsupported output and requested-schema failures return MessageError with the report instead |
| `parsed_json` | Some only after native-schema output is complete, parseable and validated; JSON null is Some(Value::Null), not absence |

`Complete + Length`, `Complete + Refusal` and `Complete + ContentFilter` are valid observed provider outcomes. Text mode returns their report without pretending it is a full answer. Schema mode returns `OutputValidation` unless normal successful termination and validation both hold. `Other` and `Unknown` do not become schema success merely because text happens to parse. Unexpected tool/image output is `UnsupportedOutput` under the standalone contract, including when a provider reports Stop.

Rig's generic finish reason does not universally identify refusal. Map a normalized reason or explicitly modeled provider refusal indicator only when authoritative adapter evidence exists; otherwise retain Other/Unknown. Do not classify refusal by matching generated prose or expose the raw provider body to obtain it.

### Usage and cost

```rust
struct GenerationUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    total_tokens: Option<u64>,
    cache_read_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
    estimated_cost_usd: Option<f64>,
}
```

| Field | Fixed meaning |
| --- | --- |
| `input_tokens` | Rig-normalized provider input count under the existing provider accounting rules |
| `output_tokens` | Provider output count; do not add reasoning tokens again when included |
| `total_tokens` | Provider/adapter-normalized total when available; do not unconditionally derive it by summing cache/reasoning fields |
| `cache_read_tokens`, `cache_write_tokens` | Provider-reported cache categories; inclusion in input totals is provider-specific |
| `reasoning_tokens` | Reported reasoning subset/category, not visible reasoning character length |
| `estimated_cost_usd` | Finite nonnegative estimate from known usage categories and validated selected-model rates; None when prerequisites are missing |
| `None` versus `Some(0)` | Unknown/unreported versus an explicitly known zero; no sentinel-zero conversion into fabricated observations |

This is not another Transcript `Usage` or PhiEvent aggregate: those existing contracts cannot represent missing metrics and do not define generation completion. Reuse their cost calculation functions, not their zero-default assumptions. Conversion into Agent observations belongs at the Agent boundary and preserves their accounting semantics.

Rig's Usage contains zero-default fields and an all-zero missing-data sentinel; it cannot by itself prove per-field presence or a reported zero. Adapter normalization must retain modeled presence where available. Until a field's presence is knowable, report None conservatively. No separate second network request is made to recover usage. A cost value of zero in a default descriptor alone does not prove a known free price.

### Errors and evidence

```rust
enum MessageErrorKind {
    InvalidRequest,
    UnsupportedCapability,
    Unauthorized,
    Provider,
    UnsupportedOutput,
    OutputValidation,
    LimitExceeded,
    Cancelled,
    Coordination,
}

struct MessageError {
    generation_id: Option<GenerationId>,
    kind: MessageErrorKind,
    message: String,
    provider_status: Option<u16>,
    report: Option<Box<GenerationReport>>,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}
```

| Field / category | Fixed meaning |
| --- | --- |
| `generation_id` | Present after admission, including failures before provider dispatch |
| `kind` | Stable domain classification; consumers do not match diagnostic strings |
| `message` | Bounded, redacted human diagnostic; never request headers, credentials or raw provider bodies |
| `provider_status` | Actual provider HTTP status if known; not synthesized from a local error |
| `report` | Available attempt facts, whether Complete but rejected or Incomplete; None when no attempt evidence exists |
| `source` | Private Rust source chain; never serialized |
| `UnsupportedCapability` / `UnsupportedOutput` | Request cannot be supported before dispatch / provider produced content outside the standalone contract |
| `OutputValidation` | Requested JSON is incomplete, malformed, schema-invalid or lacks acceptable normal termination; no hidden repair |
| `LimitExceeded` | Local input/output/schema/deadline ceiling, distinct from a provider Length finish reason |
| `Cancelled` | Local cancellation won settlement; partial report remains evidence, never success |
| `Provider` | Provider/transport failure, preserving available status and partial facts |
| `Coordination` | Lost owner, invalid settlement or other internal supervision failure |

A report is evidence, not a success receipt. Usage remains reportable on errors. A cancelled operation never turns into MessageResponse when a late provider response arrives. Errors before dispatch have no attempt ID; do not fabricate an attempt to fill the report. Source extraction and public serialization are separate boundaries.

### Private generation core

```rust
struct GenerationCore {
    catalog: CatalogSnapshot,
    clients: BTreeMap<String, AnyClient>,
}

struct GenerationInput {
    requested: Option<ModelSelection>,
    selected: ModelSelection,
    messages: Vec<rig::completion::Message>,
    settings: GenerationSettings,
    tools: Vec<rig::completion::ToolDefinition>,
    tool_choice: Option<rig::message::ToolChoice>,
    output_schema: Option<serde_json::Value>,
}

struct PreparedGeneration {
    generation_id: GenerationId,
    model: ResolvedModel,
    requested: Option<ModelSelection>,
    selected: ModelSelection,
    request: rig::completion::CompletionRequest,
}

struct GenerationStream {
    identity: GenerationIdentity,
    model: GenerationModel,
    stream: rig::streaming::StreamingCompletionResponse,
}
```

| Field | Internal contract |
| --- | --- |
| `catalog` | Captured validated model/rate facts for this prepared core; updates require new preparation, not mid-call reinterpretation |
| `clients` | Trusted configured provider clients, including guarded authentication wrappers; private, no caller-supplied raw backend |
| `GenerationInput.requested` | Explicit caller selection, or None for default resolution; preserved through preparation into GenerationModel |
| `GenerationInput.selected` | Selection already admitted by the caller's policy, then revalidated against this core's catalog and clients |
| `GenerationInput.messages` | Explicit provider-neutral Rig content, including top-level system instructions and prepared context; no implicit history reads |
| `GenerationInput.settings` | Resolved generation intent; shared mapping builds provider-specific settings without exposing arbitrary parameters |
| `GenerationInput.tools`, `tool_choice` | Model-visible declarations and selection policy, never executable tools; empty/None for standalone Messages |
| `GenerationInput.output_schema` | Locally validated schema; None for text mode; no implicit schema repair |
| `PreparedGeneration.generation_id` | Identity preserved from admission through validation and dispatch |
| `model` | Validated private model binding; never an AgentRuntime with a hidden ToolRegistry |
| `PreparedGeneration.requested` | Original selection intent retained independently of the resolved binding |
| `selected` | Resolved model identity used for response attribution and request policy |
| `request` | Private Rig request after shared settings/history adaptation; Agent declarations are derived from its resolved ToolRegistry before entry |
| `GenerationStream.identity`, `model` | Captured local correlation and selected/requested model facts; provider IDs and reported model are filled only from observed evidence |
| `GenerationStream.stream` | Private single-owner Rig stream, drained by Agent execution or standalone collection, never both |

PreparedGeneration contains no Session, Transcript, tools executor, permission runtime or lifecycle bus. It performs no effects until dispatch. The core accepts prepared Agent messages as well as standalone text, but it cannot prepare Agent context or authorize tool effects.

### Key functions and settlement

```rust
impl Phi {
    fn messages(&self, options: MessagesOptions)
        -> Result<Messages, MessageError>;
}

impl Messages {
    async fn create(&self, request: MessageRequest)
        -> Result<MessageResponse, MessageError>;
}

impl GenerationCore {
    fn prepare(&self, input: GenerationInput, generation_id: GenerationId)
        -> Result<PreparedGeneration, MessageError>;

    async fn start_attempt(
        &self,
        prepared: PreparedGeneration,
        attempt_id: GenerationAttemptId,
    ) -> Result<GenerationStream, MessageError>;
}
```

These signatures fix semantic ownership and return types, not Rust visibility. Private collection/control arguments remain owned by the calling execution boundary, not public dependencies.

| Operation | Input -> output | Owning semantics |
| --- | --- | --- |
| `Phi::messages(options)` | Trusted MessagesOptions -> `Result<Messages, MessageError>` | Validate capability defaults, selections and limits; capture catalog/clients without preparing an Instance or loading Agent runtime resources |
| `Messages::create(request)` | MessageRequest -> async `Result<MessageResponse, MessageError>` | Own admission, deadline, concurrency permit, collection and standalone acceptance; no hidden Agent loop |
| `GenerationCore::prepare(input, generation_id)` | GenerationInput + ID -> PreparedGeneration | Reuse model validation, history adaptation, settings mapping and request building; caller enforces capability policy; no network request |
| `GenerationCore::start_attempt(prepared, attempt_id)` | PreparedGeneration + fresh attempt ID -> GenerationStream | Dispatch through the guarded Rig model; expose no public raw stream or automatic retry |
| `collect_generation(...)` | Private stream + limits + cancellation -> GenerationReport or error evidence | One drain owner; bound all retained content/metadata, distinguish terminal/incomplete states, release transport resources |
| `validate_output(...)` | Complete GenerationReport + MessageOutput -> MessageResponse or MessageError | Reject unsupported content; parse and validate schema only under acceptable finish conditions |
| `MessageResponse::text()` | Response -> derived String | Concatenate only text blocks in order with no injected separators; never substitute for content or strip reasoning metadata |

Helper rows using `...` describe private control argument groups, not a generic JSON API or public collaborator injection. The standalone contract has no public stream method, `parse()` variant, arbitrary retry callback or tool runner. `create()` plus explicit output intent is sufficient; no compatibility aliases are exposed.

The core is shared at the request/attempt/response boundary. AgenticLoop retains its own retry controller and event publication; it may consume a private stream rather than the standalone collector. Common content/identity/usage conversion is reused, while Agent display deltas and canonical commits stay above it. Compaction uses the same core with its summary-specific acceptance outside the core.

```text
logical admission -> validate/prepare -> wait for permit -> provider attempt
       |                    |                 |                  |
       +--------------------+-----------------+------------------+
                            error / cancel                      |
                                  |                    collect and validate
                                  |                              |
                                  +-------------> settle <-------+
                                                  |
                                        exactly one local outcome
```

Deadline starts at admission, not dispatch. Cancellation/deadline and completion contend at the owning settlement boundary; once a terminal outcome is accepted, it cannot be replaced. Exactly one local outcome does not promise exactly-once provider execution or billing.

## Execution chains

### Standalone generation

```text
request
  -> admit under prepared capability and limits
  -> validate model, input and settings
  -> resolve configured model and request policy
  -> build Rig request
  -> dispatch one provider attempt
  -> collect bounded normalized response
  -> validate requested structured output
  -> settle accounting and return outcome
```

No Manager registration, AgentInstance actor, ToolRegistry, extension VM, permission interaction Broker or Session association is created for this operation. Shared provider clients and model catalog snapshots may be reused; mutable Agent runtime resources are not repurposed as generation state.

### Agent execution

```text
AgentInstance canonical Transcript
  -> context preparation, budget and compaction
  -> resolved ToolRegistry declarations and dynamic instructions
  -> shared generation core
  -> Agent response events and canonical reduction
  -> governed tool execution or completion gates
  -> next generation when required
```

AgentInstance remains the only owner of canonical execution history. Every retry reads the appropriate committed context. Partial response settlement, tool-call correlation, Steer and terminal event ordering retain their existing contracts.

A Messages API call is not an alternative implementation of AgentInstance query/stream. Existing one-shot Agent helpers continue to use Instance and the event/reducer pipeline.

### Workflow boundary

Workflow composes ordinary code and Agent executions through `agent()`. Agent workers use the shared generation core through AgenticLoop. Workflow owns orchestration; AgentInstance owns each worker's execution and canonical Transcript.

## Security, limits and lifecycle

### Authority

Trusted SDK preparation resolves providers, credentials, model policy and bounds. Requests cannot replace provider endpoints, read secrets, load arbitrary runtime resources or execute provider-hosted tools under this surface.

All requests retain typed credential resolution and existing authorization enforcement, including Phi's ChatGPT subscription lease checks for streaming and non-streaming requests. Never construct a raw Rig client to bypass Phi's guarded model wrapper.

No Agent tools run, so Agent tool invocation approval is not fabricated or required as a substitute for generation admission. Generation remains governed by its prepared model policy and resource limits. Caller-supplied input is not evidence of human approval.

### Cancellation and resource bounds

Bound input bytes, schema size/complexity, generated/retained output, concurrency and execution time. SDK callers own aggregate pending work and retained results. Output-token settings alone do not bound local memory. Collect streams incrementally under byte limits rather than allocating the complete response first. A separate transport response ceiling bounds adapter buffering and hidden stream events before parsing.

Generation time includes admission wait, execution and validation within the capability deadline. Cancellation releases local resources; late responses cannot publish success after cancellation has won settlement.

Dropping a standalone generation operation cancels its local work; this does not stop an unrelated Agent. Cancellation or transport termination does not prove that the remote provider stopped computing or billing. Report known usage and retain unknown usage as unknown.

### Streaming

The shared core supports Rig's non-streaming and streaming mechanisms. Agent execution continues consuming streams; the standalone Messages API returns a collected result rather than exposing a public stream.

One owner drains each stream. Deltas are provisional; only a completed outcome is a completed response. Stream failure, explicit cancellation and normal termination are distinct local facts even if a transport represents cancellation as end-of-stream. Never infer success from EOF or from the presence of partial text.

Response conversion and metadata rules are shared across collected and streamed calls. There is no separate String-only implementation. Content ordering, part identity, bounded buffering and exactly one terminal outcome are preserved.

### Errors and retries

Separate invalid input, unsupported capability, authorization failure, provider failure, output validation, limits, cancellation and coordination failure. Preserve provider status and source chains internally; expose bounded, redacted domain diagnostics.

The standalone contract has one explicit provider attempt and no Phi-managed automatic retries. Do not claim network exactly-once execution: transport/provider behavior may be uncertain. Caller-managed retries must be explicit, bounded and separately counted.

Agent execution retains its existing transient retry and context-overflow recovery policy above the shared generation core. Compaction retains its nonempty/nontruncated summary acceptance policy. Do not move Transcript mutation or automatic compaction into a generic retry helper. Never invisibly restart a partially consumed stream and splice attempts into one answer.

### Accounting and persistence

Report usage per attempt, including failed or cancelled attempts when usage is available. Caller-owned aggregation counts each source once; returning usage in a result does not increment totals again. Standalone usage does not implicitly enter Agent or Workflow totals.

Cost is derived from the resolved catalog and available usage facts, not a provider billing guarantee. No hard monetary/token-budget guarantee exists without reservation, concurrent overshoot and termination semantics. Concurrency and invocation limits are not spend caps.

The capability creates no Session journal. Applications own any retained generation requests, results and reports. Store bounded input/output only under explicit retention policy; never persist credentials or raw provider payloads. Observation IDs and saved records do not restore execution authority or make replay deterministic.

## References

These references document public API behavior and Agent orchestration. They do not specify Phi's internal implementation.

| Reference | Topic |
| --- | --- |
| [Create a Message](https://platform.claude.com/docs/en/api/messages/create) | Structured input, response content, stop reason and usage |
| [Working with Messages](https://platform.claude.com/docs/en/build-with-claude/working-with-messages) | Stateless generation with caller-supplied conversational history |
| [Tool use overview](https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview) | Client-side and provider-hosted tool execution |
| [Tool runner: advanced usage](https://platform.claude.com/docs/en/agents-and-tools/tool-use/tool-runner#advanced-usage) | Model/tool iteration and tool-result history |
| [Structured outputs](https://platform.claude.com/docs/en/build-with-claude/structured-outputs) | Native JSON schema and output validation |
| [Streaming messages](https://platform.claude.com/docs/en/build-with-claude/streaming) | Text, tool and thinking events |
| [Search results](https://platform.claude.com/docs/en/build-with-claude/search-results) | Retrieved evidence supplied as generation input |
| [Building Effective Agents](https://www.anthropic.com/engineering/building-effective-agents) | Prompt chains, routing, parallelization and evaluator-optimizer patterns |
| [Dynamic workflows](https://code.claude.com/docs/en/workflows) and [official Cookbook](https://github.com/anthropics/claude-cookbooks/blob/main/claude_agent_sdk/08_Dynamic_workflows.ipynb) | Agent orchestration through `agent()`, `parallel()` and `pipeline()` |
| [Agent SDK overview](https://code.claude.com/docs/en/agent-sdk/overview) and [Agent loop](https://code.claude.com/docs/en/agent-sdk/agent-loop) | Autonomous execution, tools, permissions and context management |
| [Assistant message](https://code.claude.com/docs/en/agent-sdk/typescript#sdkassistantmessage) and [Agent result](https://code.claude.com/docs/en/agent-sdk/typescript#sdkresultmessage) | Generation observations and Agent-level completion results |

## Design invariants

- One provider integration mechanism serves Agent rounds, standalone generation and compaction.
- Model generation creates no AgentInstance, Manager membership or Session.
- Agent execution retains Instance ownership and the canonical event/reducer path.
- Explicit context never becomes implicit history or permission.
- No tool is executed by the standalone generation surface.
- Unsupported capability fails explicitly; provider facts are not invented or silently flattened.
- Cancellation, partial output, truncation, validation failure and completed generation remain distinguishable.
- Workflow orchestrates Agent executions; AgentInstance owns worker execution and history.
- Library consumers receive semantic values, never raw runtime collaborators or secrets.
