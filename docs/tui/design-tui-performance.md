# TUI Performance Design

## Scope and intent

Phi's performance design covers input, streaming, projection/layout, frame composition, terminal IO and persistence. Long history is one workload, not the sole concern.

The ownership split remains unchanged: SDK owns canonical execution/history; application owns projection and Session persistence; `phi-tui` owns app-neutral layout/scheduling/painting. See [global architecture](../design.md), [TUI design](design-tui.md), and [application design](../design-interactive.md).

The goal is bounded derived state and responsive changed-state work, not bounded canonical history or a whole-process memory guarantee. Implementation optimizations must not change execution, history, Unicode, image, lifecycle or candidate-first replacement semantics.

## Performance model

| Cost | Required focus |
| --- | --- |
| Input-to-paint latency | Priority scheduling and coalescing |
| Per-frame CPU | Changed-block projection, parsing and layout |
| Retained/transient memory | Early byte bounds and shared immutable content |
| Terminal write volume | Differential output and buffered writes |
| Persistence latency | Journal append and explicit checkpoint boundaries |

These costs are independent: reducing terminal output does not fix full-history parsing; limiting display retention does not fix repeated streaming copies.

Limits apply **before** expensive derived state is materialized. Projecting/parsing/wrapping the whole transcript and truncating only in Renderer is not an acceptable bounding strategy.

```text
Canonical content -> bounded projection/layout -> shared retained chunks
  -> bounded paint tail + chrome -> differential terminal output
```

Steady-state work should track changed blocks and bounded visible state. Restoration, reflow, parser-sensitive changes, candidate preparation and forced replacement are explicit larger-work boundaries—not evidence that every operation is constant-time.

## Goals and non-goals

- Keep editing/focus/overlays responsive during streaming through priority scheduling and coalescing.
- Avoid full Transcript or accumulated-response copies per delta/frame.
- Bound retained projection, layout caches, materialized rows, image resources and renderer differential state.
- Preserve native main-screen scrollback as the only ordinary transcript navigation model.
- Append normal Session growth without reconstructing the entire file.
- Preserve deterministic presentation and canonical content even when display history is evicted.

No alternate-screen transcript viewport, application scroll offset, generic virtual DOM, unbounded historical expansion or terminal-specific optimization framework is introduced. Non-TUI persistence/export can still access full canonical history.

## Ownership model

### Canonical content

AgentInstance is the sole mutable Transcript owner. Large committed payloads use shared immutable storage; canonical identity is entry ID/content index, not pointer/allocation identity. Live text has one mutable buffer per projected stream route; committed shared handles do not create another mutable history owner.

Application checkpointing reads canonical history. Full Session/Transcript representations are permitted at explicit boundaries such as recovery, checkpoint/save evidence, load, branch/candidate construction and shutdown. These copies must not become continuously synchronized per-event UI state. This is a performance boundary, not a claim that current checkpoint reads are zero-copy.

See [Transcript/shared content](../../phi-sdk/src/transcript/types.rs), [application checkpoints](../../phi-interactive/src/runtime/checkpoints.rs), and [AgentSession snapshots/writes](../../phi-interactive/src/runtime/session.rs).

### UI projection and display views

Display ownership follows [application projection](../design-interactive.md#uistate-projection) and the [TrackedBlock contract](design-tui.md#display-transcript). When canonical resolution is needed they address entry/content coordinates; immediate display may hold a shared payload handle. Ephemeral content covers live partials/notifications not yet canonical.

Indexed block/tool lookup avoids scanning old blocks for every delta. The application supplies semantic display views without cloning complete groups each frame; `phi-tui` does not resolve SDK identity.

After successful paint acknowledges the retained boundary, application retires complete unpinned groups and rebuilds lookup indexes. Retirement must not split groups or lose live routes. Pinned routing state outside the display range does not expand Ctrl+O scope. Compaction/rebuild removes unreachable projection/cache identities.

Implementation: [UI state](../../phi-interactive/src/interactive/state.rs), [blocks](../../phi-interactive/src/interactive/blocks.rs), [render composition](../../phi-interactive/src/interactive/render.rs), and [display transcript](../../phi-tui/src/widgets/transcript.rs).

## Default transcript model: bounded history buffer

HistoryBudget independently bounds estimated terminal footprint, resident display storage and image storage; layout caches have their own budget. Default policy uses a pinned Ghostty v1.3.0 reference with headroom, not a query of the user's terminal or a percentage of process RSS. Exact constants/accounting are in [history.rs](../../phi-tui/src/render/history.rs).

The estimate includes cells/rows, styles, multi-codepoint graphemes and allocation allowances; text or ANSI length alone is insufficient. Images reserve terminal rows and charge decoded/encoded storage separately. It is conservative policy, not Ghostty allocator ABI or a guarantee against terminal bugs. [Ghostty's PageList incident](https://mitchellh.com/writing/ghostty-memory-leak-fix) motivates headroom; external calibration is separate validation.

Select complete blocks newest-first using **fully expanded** footprint, conservatively covering larger collapsed disclosures too. Retained boundaries follow display order and block identity, never numeric ID ordering; reinserted notifications and nested blocks can have older IDs than preceding blocks. Ctrl+O never changes membership or refills evicted history. Growth/reflow can advance the boundary; oversized sources are guarded before layout and blocks that do not fit stay outside the review window.

Batch eviction leaves headroom to amortize replacement. Retained lines are immutable shared chunks; unchanged chunks/measurements survive separate layout-cache eviction. Normal frames flatten only a bounded paint tail. Canonical history is unaffected by display eviction.

Ctrl+O applies expansion intent to collapsible blocks inside the retained range. Completion/updates/new blocks preserve explicit intent; display defaults apply only before user override.

## No application-owned transcript viewport

Phi uses the terminal's native main-screen scrollback, not alternate screen or an internal transcript-wide scroll offset. Its byte-bounded buffer defines the maximum review range; a smaller terminal scrollback can retain fewer rows.

Phi replaces its retained range without knowing which native scrollback row the user is viewing. Evicted history is removed through explicit replacement, not left indefinitely in terminal memory. Historical search/export must not smuggle a second normal transcript viewport into this model.

## Bounded transcript materialization

The three history ranges have different purposes and limits:

| Range | Purpose | Limit |
| --- | --- | --- |
| Canonical Transcript | Execution/history truth, persistence and review | Not capped by UI retention |
| Retained display history | Native-scrollback review range, shared replay chunks and Ctrl+O scope | Byte-budgeted expanded footprint |
| Paint tail | Flattened rows used for ordinary frame painting | Per-render row work budget; not history capacity |

Render intent carries width, row budget, revisions and expansion policy. Output carries tail lines, shared history and footprint/boundary facts. Screen-height overscan cannot define the retained review range.

Changed blocks are measured under source/row guards; unchanged blocks reuse reservations/chunks. Only the tail is flattened into the frame. Defensive Renderer row limits do not discard the retained shared prefix on resize/recovery.

Concrete request/result fields and limits: [TranscriptTail](../../phi-tui/src/widgets/transcript.rs), [application paint-tail policy](../../phi-interactive/src/interactive/render.rs), and [Renderer](../../phi-tui/src/render/renderer.rs).

## Block revisions and cache invalidation

Mutation clocks follow the [TrackedBlock contract](design-tui.md#display-transcript). Role and group-flow inputs belong to the borrowed display view and are included in retained/layout keys, so their changes cannot reuse incompatible rows. Expansion is a separate cache input: toggling it invalidates composition without pretending source text changed. Content epoch distinguishes replacement from append-only growth so incremental Markdown cannot reuse an unrelated prefix. Applications still advance collection-wide projection revision when display content/order/presentation changes.

Keys cover identity/revision, width, theme and presentation; incremental parsing uses source epoch rather than unrelated status changes. Width invalidates wrapping, theme invalidates styling, and session/branch/compaction changes retire unreachable IDs. A live delta should invalidate its active block, not unchanged history.

Cache eviction accounts estimated bytes including metadata/empty layouts, not entry count. Cached tails may serve smaller requests; larger requests rematerialize safely. Exact key structures/accounting belong to [display transcript caches](../../phi-tui/src/widgets/transcript.rs).

## Markdown pipeline

`pulldown-cmark` owns syntax. Phi owns configured extensions, event-to-line presentation, Unicode wrapping and row-bounded incremental layout—not another AST or handwritten block grammar.

Append-grown Markdown reuses parser-confirmed stable top-level sections and reparses the mutable trailing section. Unclosed constructs and reference-sensitive content cannot be assumed stable; later reference definitions may require resetting earlier layout. Inline links do not automatically imply the same reset.

Stability comes from the same options/event stream/source offsets used for rendering. Width/theme/source changes reset incompatible checkpoints. If stability cannot be proven, retain mutable source or fall back safely. Derived state leaves with its block; it is not history truth. See [Markdown renderer/parser integration](../../phi-tui/src/render/markdown.rs).

## Frame and line ownership

Bounded materialization flows forward through CodingFrame, TuiRuntime composition and Renderer preprocessing. Consume/borrow owned sections instead of cloning entire line vectors at every layer. Image expansion is limited to admitted frame/image footprint.

Avoid duplicate complete text plus segmented text representations where possible; sharing/fingerprints are optimizations, not canonical identity. Equality remains collision-safe and Unicode/style/link metadata remains intact. See [lines](../../phi-tui/src/render/line.rs), [layout](../../phi-tui/src/layout.rs), and [TuiRuntime](../../phi-tui/src/runtime/tui.rs).

## Scheduling and streaming

TuiRuntime owns render scheduling and paint-failure retries; see the [scheduling contract](design-tui.md#render-scheduling-and-painting). Application history retirement is acknowledged only after successful paint.

Streaming appends in place to its one live buffer, updates block revision and requests coalesced painting rather than one paint per delta. Materialize only necessary bounded layout; animation/timing changes should not parse unchanged Transcript Markdown. Scheduling rates and render priorities belong to [TUI runtime](../../phi-tui/src/runtime/tui.rs).

## Terminal renderer and acknowledged replacement

Renderer owns synchronized IO, clipping, images, cursors and differential state. It keeps bounded tail/chrome rows and a shared last-successful history snapshot. Prefix comparison skips identical chunks and compares remaining rows without flattening history. Buffered differential writes neither replay prefixes nor scroll deleted rows; invalidation is advisory. See [redraw boundaries](design-tui.md#paint-intent-and-terminal-ownership).

Forced redraw is a **replacement**, not an append:

```text
Clear viewport -> move home -> purge old scrollback -> replay retained history + chrome
```

Replacement purges all native scrollback to prevent stale or duplicate rows. First paint and differential updates do not purge; existing scrollback or layout-cache eviction alone does not authorize replacement. Geometry changes, prefix differences, origin discontinuities, unreachable updates and [optional shrink](design-tui.md#scrollback-replacement-invariant) can require replacement.

Long replacements first commit the actual current viewport/editor/footer as a bounded synchronized preview, then purge that preview and replay retained prefix in bounded batches. Clipped image continuation rows stay blank until full replay. This is not a loading screen or asynchronous input service: history replay remains synchronous.

History revision/boundary and image release become acknowledged only after successful output. Failed paint preserves retryable replacement intent; unchanged repeated frames must not replace again. Exact buffer/batch sizes, image deletion and preview behavior are in [Renderer and regressions](../../phi-tui/src/render/renderer.rs).

### Session replacement latency

Restoring canonical history, preparing resources and painting the current screen are separate costs. Borrowed indexed branch traversal avoids repeated parent scans; indexed application projection avoids repeated block scans. Larger transition work is measured separately from steady streaming.

Memory readiness must not block restored-history paint. Warmup sends synchronous filesystem/graph work to the blocking pool; async spawning alone does not isolate it. Preparation/accounting belongs to [application Memory](../agent/design-agent-runtime.md#memory-preparation), synchronization/retrieval to [SDK Memory](../design-sdk.md#memory-and-tape).

## Persistence

Application runtime owns persistence; journal optimization changes IO mechanics, not save policy. Canonical history can grow while serialization/write buffers remain bounded; a full checkpoint read is distinct from per-event projection.

Ordinary compatible growth appends new records with a persistence cursor. Structural changes needing rewrite stream a checkpoint into a same-directory temporary file and atomically replace the journal. Loading is incremental; discovery reads bounded summaries and fully loads only selected history.

A torn or newline-missing final record is repaired before append, preventing concatenation/corruption. Exact record variants/cursor rules belong to [journal](../../phi-interactive/src/session/journal.rs), [discovery](../../phi-interactive/src/session/discovery.rs) and [storage](../../phi-interactive/src/session/storage.rs), not a second protocol inventory here.

Performance optimization must not bypass application save policy, expose unaccepted candidate state or persist secrets. See [application persistence](../agent/design-agent-runtime.md#application-session-and-persistence).

## Crate responsibilities and acceptance invariants

SDK provides canonical identity/history and shared immutable committed content. Application projects incrementally, supplies bounded render intent and owns checkpoints. TUI bounds materialization/caches and owns scheduling/terminal output without SDK knowledge.

For fixed budgets/width, acceptance checks must verify:

- retained display, layout-cache and renderer differential state remain bounded independently of canonical history length;
- per-frame line copies are paint-tail bounded; unchanged history is shared, not flattened repeatedly;
- deltas do not clone the full response/Transcript and unrelated chrome updates do not reparse history;
- display retirement/expansion follows successful paint acknowledgement without changing canonical data;
- replacement purges stale/duplicate scrollback while ordinary painting preserves it;
- journal append avoids rewriting prior history and checkpoint serialization remains buffered;
- performance failures preserve retryable state, Unicode/image semantics, security and replacement ownership.

These are validation targets, not a claim of zero-copy checkpoints, terminal RSS bounds or constant-time restoration. Core regressions live in [transcript layout](../../phi-tui/src/widgets/transcript.rs), [renderer](../../phi-tui/src/render/renderer.rs), [Markdown](../../phi-tui/src/render/markdown.rs), [application render](../../phi-interactive/src/interactive/render.rs) and [journal](../../phi-interactive/src/session/journal.rs).
