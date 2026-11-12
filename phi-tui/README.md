# phi-tui

Reusable terminal UI infrastructure and widgets for coding/agent applications, built on crossterm.

`phi-tui` provides:

- `TuiRuntime<A>` for widget ownership, focus, input routing, overlays, cursor resolution, and render scheduling
- a small `Widget` protocol with typed local actions
- typed `WidgetHandle<W>` values for updating and removing mounted widgets
- editor, picker, dialog, transcript, footer, diff, panel, and notification widgets
- structured `StyledLine` rendering, themes, Unicode text helpers, and incremental terminal output

Applications own business state and application-specific components. To mount an application component, implement `Widget`; `TuiRuntime::mount` maps its local action into the application action type.

## Architecture

```text
crossterm event
-> TuiRuntime focus/context dispatch
-> Widget::handle_input
-> typed application action
-> application state/widget update
-> runtime render scheduling and composition
-> Frame
-> TuiRenderer differential output
```

`Widget::render` returns width-bounded `StyledLine` values. `CodingFrame` combines the fixed transcript/status/editor/menu/footer regions used by coding TUIs; it is not a generic layout engine.

Mount placements are explicit:

- `MountOptions::after_root(context)` appends visible widget lines after the application root frame.
- `MountOptions::input(context)` participates in focus and input routing without rendering lines.
- `MountOptions::overlay(context, options)` renders a focused capturing overlay.

`show_widget` preserves focus; `show_and_focus_widget` explicitly transfers it. Use `set_widget_input_context` when one logical input target changes mode, and `InputContext::custom("name")` for application-specific binding contexts.

## Minimal runtime

```rust
use phi_tui::{
    ConfirmAction, ConfirmDialog, InputContext, InputOutcome, MountOptions, OverlayOptions, Theme,
    TuiRuntime, TuiRenderer,
};

#[derive(Clone)]
enum AppAction {
    Confirmed,
    Cancelled,
}

fn render(renderer: &mut TuiRenderer) -> anyhow::Result<()> {
    let theme = Theme::default();
    let mut runtime = TuiRuntime::<AppAction>::new();
    let dialog = runtime.mount(
        ConfirmDialog::new("continue?", "render through the widget runtime"),
        MountOptions::overlay(InputContext::Dialog, OverlayOptions::centered_panel()),
        |action| match action {
            ConfirmAction::Confirmed => AppAction::Confirmed,
            ConfirmAction::Cancelled => AppAction::Cancelled,
        },
    );

    if matches!(
        runtime.dispatch_key(phi_tui::KeyInput::Enter),
        InputOutcome::Action(AppAction::Confirmed)
    ) {
        runtime.update_widget(dialog, |dialog| {
            dialog.message = String::from("widget updates automatically request rendering");
        });
    }

    runtime.render_widgets(renderer, &theme)
}
```

`compose(area, frame, theme)` provides deterministic headless composition over an explicit application root frame. `render_with_base` paints the same composition and commits a pending render request only after a successful terminal write; failed writes requeue the consumed priority.

Run the included example:

```bash
cargo run --example minimal --manifest-path phi-tui/Cargo.toml
```
