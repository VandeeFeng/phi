use phi_tui::{
    ConfirmAction, ConfirmDialog, InputContext, MountOptions, OverlayOptions, TerminalGuard, Theme,
    ThemeColor, TuiRenderer, TuiRuntime,
};

#[derive(Clone)]
enum AppAction {
    Confirmed,
    Cancelled,
}

fn main() -> anyhow::Result<()> {
    let _terminal = TerminalGuard::enter()?;
    let theme = Theme::dark().with_color(ThemeColor::Accent, crossterm::style::Color::Blue);
    let mut renderer = TuiRenderer::new(theme.clone());
    let mut runtime = TuiRuntime::<AppAction>::new();

    runtime.mount(
        ConfirmDialog::new("continue?", "phi-tui owns focus, input, and rendering"),
        MountOptions::overlay(InputContext::Dialog, OverlayOptions::centered_panel()),
        |action| match action {
            ConfirmAction::Confirmed => AppAction::Confirmed,
            ConfirmAction::Cancelled => AppAction::Cancelled,
        },
    );
    runtime.render_widgets(&mut renderer, &theme)?;

    std::thread::sleep(std::time::Duration::from_secs(2));
    renderer.finish()?;
    Ok(())
}
