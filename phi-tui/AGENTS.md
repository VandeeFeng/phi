# phi-tui

## Purpose

Reusable terminal UI runtime for coding and agent applications: widget lifecycle, focus/input routing, frame composition, rendering, themes, terminal lifecycle, and app-neutral widgets.

Architecture: [`../docs/tui/design-tui.md`](../docs/tui/design-tui.md).

Do not add Phi sessions, providers, tools, permissions, MCP, memory, SDK runtime behavior, or application configuration here.

## Code map

- `src/lib.rs` — curated public exports.
- `src/runtime/widget.rs` — `Widget`, contexts, outcomes, mount options, and typed handles.
- `src/runtime/tui.rs` — `TuiRuntime`, events, scheduling, composition, and painting coordination.
- `src/runtime/mounts.rs` — private erased heterogeneous widget runtime.
- `src/runtime/focus.rs` / `input.rs` — focus history, input contexts, and keybindings.
- `src/layout.rs` — fixed `CodingFrame` composition.
- `src/render/` — styled lines, text, markdown, overlays, themes, and renderer.
- `src/terminal/` — crossterm event normalization and RAII terminal lifecycle.
- `src/widgets/` — reusable editor, transcript, menu, dialog, panel, and display widgets.

## Local invariants

- `Widget` is the single public UI protocol; do not introduce parallel view/component traits.
- Keep widget rendering deterministic and based on `RenderContext`.
- Widget cursors are local; mounted runtime translates them and clips overlay cursors.
- Widgets own local interaction only and emit typed actions; callers own persistence, async work, and business workflows.
- Map local widget actions into the application action type at mount time.
- Mutate mounted widgets through `TuiRuntime` so render invalidation remains runtime-owned.
- Keep keybinding resolution separate from widget-local input handling.
- Use `InputContext` and typed handles instead of application booleans for routing.
- Keep runtime scheduling/composition separate from Renderer painting/diffing.
- Keep overlays as separate layers; overlay meaning stays application-owned.
- `CodingFrame` remains the fixed transcript/status/editor/menu/footer layout; editor and menu cursors are local to their blocks.
- `StyledLine`/`StyledSegment` are the primary presentation primitives.
- Use `pulldown-cmark` for Markdown syntax; Phi owns terminal presentation, wrapping, budgeting, and incremental layout, not a parallel parser grammar.
- Derive Markdown stability boundaries from the same configured parser event stream used for rendering.
- Preserve Unicode grapheme and display-width behavior in editing, wrapping, truncation, padding, and cursor movement.
- Use `ThemeColor` tokens instead of hardcoded reusable-widget colors.
- Terminal writes and mode changes belong only in render/terminal modules.
- `EditorState` owns editing mechanics; callers decide submitted-history policy.
- TUI `Transcript` is a display model, never the SDK transcript. `TrackedBlock` owns display mutation/clocks, not domain identity/routing/retirement; follow the existing [display transcript contract](../docs/tui/design-tui.md#display-transcript).
- Keep `BlockBody` centralized and semantic; raw log-like content uses `BlockBody::Log`.
- Diff widgets render `DiffView` data only; do not add old/new file comparison, patch generation, or editor-navigation logic to `phi-tui`.
- Do not expose the erased mounted-widget runtime.

## Change patterns

- Add reusable widgets under `src/widgets/`; export from `widgets/mod.rs` and re-export from `lib.rs` only when stable.
- Prefer direct structs and `Default`; constructors should enforce an invariant or improve call sites.
- Compose visual children inside a parent unless they need independent focus, visibility, capture, or overlay lifecycle.
- Use custom input contexts for third-party/application-specific contexts.
- Parse textual key declarations once, then compare native `KeyInput` values at runtime.
- Preserve minimum-width and maximum-line safeguards when modifying rendering.

## Avoid

- Async runtimes or application event loops.
- SDK/app configuration or environment-driven business behavior.
- Business routing in `TuiRuntime`, `CodingFrame`, scheduler, or renderer.
- Generic virtual DOM/layout abstractions without a demonstrated coding-TUI requirement.
- Public internals added only for one application call site.

## Related

- Workspace rules: `../AGENTS.md`
- Consuming app: `../phi-interactive/AGENTS.md`
- SDK rules: `../phi-sdk/AGENTS.md`
