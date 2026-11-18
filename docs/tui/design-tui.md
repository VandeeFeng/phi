# phi-tui Design

`phi-tui` is a reusable terminal UI runtime for coding and agent applications. It owns widget contracts, mounted-widget lifecycle, focus and input routing, frame composition, render scheduling, styled text, overlays, differential terminal output, and terminal lifecycle.

It must remain application-agnostic. Phi sessions, providers, tools, permissions, MCP, memory, and application configuration do not belong here.

## Semantic model

```text
Application owns business state.
TuiRuntime owns mounted widgets, focus, routing, scheduling, and composition.
Widget owns reusable local UI behavior and emits typed actions.
StyledLine owns structured terminal presentation.
CodingFrame owns fixed coding-screen composition.
TuiRenderer owns clipping, diffing, viewport state, and crossterm painting.
```

`Component` is an application-layer concept, not a `phi-tui` public protocol. The library intentionally avoids a virtual DOM, generic layout tree, public cell buffer, or broad view abstraction until a concrete coding-TUI requirement needs one.

## Widget contract

`Widget` is the single public UI protocol:

```text
render(RenderContext) -> Vec<StyledLine>
handle_input(KeyInput) -> InputOutcome<Action>
cursor(RenderArea) -> Option<CursorPosition>
```

Rendering is immutable and deterministic. Local editing/navigation may mutate widget state and produce a typed action. Widget cursors are local coordinates.

`RenderContext` carries area, theme, and focus. Parent widgets derive child areas with `with_area` so inherited state remains intact.

Widgets own reusable local mechanics only. Persistence, async work, business actions, and event loops stay with the consuming application.

## Terminal UI runtime

`TuiRuntime<A>` owns heterogeneous mounted widget instances behind typed `WidgetHandle<W>` APIs. It is responsible for:

- mount, update, remove, visibility, and ordering;
- focus history and active input context;
- capture-input priority;
- mapping local widget actions into application actions;
- render invalidation, revision coalescing, and priority scheduling;
- after-root and overlay composition;
- cursor translation and renderer invocation.

Applications mutate mounted widgets through TuiRuntime APIs so render scheduling remains runtime-owned.

### Input routing

```text
normalized KeyInput
  -> focused visible widget
  -> context keybinding if ignored
  -> application action
```

A capturing widget converts ignored input into consumed input. Context bindings take precedence over global fallback. Third-party contexts use custom `InputContext` values rather than adding application assumptions to the library.

Mount placement is explicit:

- `AfterRoot` appends visible lines after the application root frame.
- `InputOnly` participates in focus/input without rendering lines.
- `Overlay` renders as a separately positioned layer.

Overlay meaning remains application-owned; TuiRuntime manages only generic visibility, focus, capture, order, and composition.

## Frame and rendering model

`StyledLine` and `StyledSegment` are the presentation primitives, with private segment representation and stable construction/access APIs. Wrapping preserves Unicode width, styles and complete link destinations. Hyperlinks and images remain semantic data; terminal escapes are emitted only by Renderer with supported-capability detection and readable fallback.

Concrete detection/override and protocol behavior: [lines](../../phi-tui/src/render/line.rs), [images](../../phi-tui/src/render/image.rs), and [terminal capabilities](../../phi-tui/src/terminal/capabilities.rs).

The application supplies an explicit root `Frame`. TuiRuntime appends mounted after-root content and carries overlays separately until paint time. Only the focused widget contributes a translated cursor; an input-only widget does not erase the root cursor.

`CodingFrame` is intentionally a fixed coding-TUI layout:

```text
transcript
status
editor
menu
footer
```

Transcript blocks form scrollback-like content, including retained historical notifications. Status combines transient component states and the running indicator. Editor and menu form the input region. Status, editor, menus, footer, dialogs, and overlays are transient chrome. Editor and menu cursors are block-local; CodingFrame translates them after composing preceding regions.

Overlays resolve terminal-relative geometry, margins, clipping, and cursor visibility before painting. They do not become part of durable transcript content.

## Render scheduling and painting

Render requests coalesce by revision and priority. Interactive and immediate updates may bypass normal frame delay; animation/background updates may be throttled. Failed paint attempts requeue the consumed priority.

`TuiRenderer` owns:

- synchronized crossterm output;
- terminal viewport tracking;
- line-level differential updates;
- full-render fallback on resize or unsafe shrink;
- overlay composition and clipping;
- hardware cursor movement;
- minimum-width and maximum-line safety limits.

Renderer owns image normalization and reserved-row footprints. It translates cursors, excludes overlays from image rows, expands differential updates to complete owning footprints and releases replaced image resources. Unsupported/unreliable terminal graphics use text fallback. Widgets never write graphics sequences or manage terminal image IDs. See [image handling](../../phi-tui/src/render/image.rs) and [painting](../../phi-tui/src/render/renderer.rs).

TuiRuntime decides when and what to compose. Renderer decides how to paint it efficiently. Terminal writes do not belong in widgets or application command handlers.

### Paint intent and terminal ownership

```text
history_revision -> invalidate_history -> advisory HistoryChange
  -> Renderer checks geometry, retained origins and changed rows
  -> differential update or history replacement
```

History invalidation requests reconciliation, not clearing. Renderer compares retained prefixes against the last successful shared snapshot. Existing scrollback alone does not require replacement: reachable tail changes use differential painting; changed prefixes, unreachable changes and discontinuous history origins require replacement. Snapshot and application history revision are acknowledged only after successful paint; failures retain retry intent. Session switches request reconciliation without application-level clearing.

### Scrollback replacement invariant

Incremental painting preserves main-screen terminal scrollback. A forced full redraw is different: it emits the complete retained tail and chrome again, so it is a replacement operation rather than an append operation. Before writing that replacement frame, the renderer must perform these terminal operations in order:

```text
clear viewport -> move cursor home -> purge scrollback -> write retained frame
```

First paint writes retained history without clearing. Later width or height changes replace the frame. `PHI_CLEAR_ON_SHRINK` defaults off; when enabled, content below the maximum rendered row count triggers replacement only without overlays. Safe deletion uses cursor movement and line clearing, not scrolling. Differential paints never purge; replacement purges all native scrollback, including shell output, to prevent stale or duplicate rows.

The history window is byte-budgeted by expanded footprint, independent of paint-tail height. Ctrl+O changes presentation within that range. Retained prefixes are written on first paint and replacement, not differential updates; retirement follows successful paint. Long replacements preview the viewport before synchronous history replay. See [performance contracts](design-tui-performance.md#terminal-renderer-and-acknowledged-replacement) and [Renderer](../../phi-tui/src/render/renderer.rs).

## Terminal lifecycle

`TerminalGuard` owns terminal mode setup/cleanup through RAII, including partial startup failure. Keyboard protocol changes are balanced and limited to consumed events. Applications choose when to stop input reporting; shutdown may retain input and raw mode for explicit escalation. The interactive application's wait/warn/force-exit policy belongs to its [event loop](../design-interactive.md#interactive-event-loop). Final cleanup discards pending TUI input before restoring raw mode. Platform/protocol details belong to [terminal guard](../../phi-tui/src/terminal/guard.rs).

Crossterm events normalize into the small `KeyInput` vocabulary before runtime or widget routing. This keeps reusable widgets independent from raw terminal event details. `poll_key_input_with_timeout` and `TuiRuntime::poll_event_with_timeout` accept caller-supplied wait durations; zero is nonblocking, and convenience methods use 50ms. Wake cadence and async orchestration remain application-owned.

## Reusable state boundaries

### Editor

`EditorState` is authoritative for local text-editing behavior:

- grapheme-aware horizontal movement;
- Unicode display-width visual rows;
- vertical cursor preference;
- paste placeholders and expansion;
- draft restoration;
- bounded local history.

`EditorState::handle_input` reuses direct editing mechanics for character, paste, backspace and horizontal movement. It returns `EditorChange` (unchanged/cursor/text), or `None` for application keys. Newline/vertical-navigation policy, completion, submitted history and prompt/command mapping remain application-owned.

### Display transcript

The TUI `Transcript` is a collection of display `TranscriptBlock`s, not the SDK transcript. Blocks own semantic body variants, status, indentation, expansion behavior, separate `title`/`input` fields, and optional styled title-suffix/footer presentation. Output headers render title and input inline, preserving layouts such as `Read {README.md}`. Collapsible blocks preview long input in three visual rows; input and body share expansion, including when the body is empty. `title_suffix` uses Dim styling, appended when it fits or wrapped below otherwise. Applications attach domain identity, assign presentation meaning and project events into this model.

`TrackedBlock` encapsulates mutable display content with automatic layout revision and source epoch updates. Append-only text preserves its source epoch; body replacement advances it, while title/input/status/footer/indent updates invalidate layout only. Expansion intent remains a separate presentation input. Read-only access cannot bypass tracked mutation. It supplies the existing borrowed `TranscriptBlockView` to `TranscriptTail`; it is not a second transcript collection or rendering protocol. Callers retain identity allocation, collection-wide projection invalidation, grouping, domain routing and retirement decisions. Direct `TranscriptBlockView` consumers can still provide their own clocks.

Bodies retain distinct semantics for log, tool output, diff, thinking, prompt disclosure, notifications and images. `OutputSections` preserves caller-supplied section text and optional character-preview limits: collapsed summaries use the original source, while expansion previews bounded sections without counting omitted lines. Expansion is presentation, never source deletion or execution state. TranscriptGroup is the message-level visual unit; transcript composition owns outer spacing, while blocks own internal wrapping/indentation. Concrete status, disclosure and group styles belong to [display transcript](../../phi-tui/src/widgets/transcript.rs), not SDK history.

Assistant Markdown uses `pulldown-cmark` as the syntax parser. Phi owns only terminal presentation and projection: explicit parser extensions, parser-event-to-`StyledLine` mapping, themes, Unicode wrapping, tables, row budgets, and incremental stable/mutable layout. Stable source boundaries come from the same parser event stream and offsets used for presentation; `phi-tui` must not maintain a second handwritten Markdown grammar.

### Other widgets

`Choice` renders horizontal options within the supplied width. Left/Right/Tab wrap selection; Enter emits its index, Esc/Ctrl+C cancel, and empty choices cannot select. Compose it within a prompt; business mappings remain application-owned.

Pickers, menus, dialogs, autocomplete, panels, footer, notifications, diff, and file completion expose app-neutral state and typed actions. `DiffView` renders supplied semantic lines and omission rows; it does not compute file differences, patches, or editor navigation metadata. A child should be mounted independently only when it needs independent focus, visibility, capture, or overlay lifecycle.

## Terminal behavior tests

Test-only `vt100` coverage checks viewport, scrollback, cursor and subsequent frames across terminal sizes, safe updates and history replacements. A tested adapter supplies unsupported ED3 behavior by discarding scrollback while preserving viewport and cursor. Graphics and synchronized-output framing use command-level tests.

## Rendering invariants

Rendering must remain deterministic, Unicode-correct, width-bounded, and theme-driven. Reusable code uses `ThemeColor` tokens rather than hardcoded colors. `ThemeColor::Running` marks execution in progress and inherits the theme's warning palette unless explicitly overridden. Overlay composition preserves uncovered segment styles and slices by display columns. Terminal mode changes and writes remain confined to render/terminal modules.

Implementation boundaries: [Widget contract](../../phi-tui/src/runtime/widget.rs), [TuiRuntime](../../phi-tui/src/runtime/tui.rs), [focus/input routing](../../phi-tui/src/runtime/input.rs), [CodingFrame](../../phi-tui/src/layout.rs), and [public exports](../../phi-tui/src/lib.rs).
