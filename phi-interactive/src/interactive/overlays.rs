use phi_tui::{
    InputContext, InputOutcome, KeyInput, MountOptions, OverlayOptions, RenderContext, StyledLine,
    Widget,
};

use super::components::input::UiAction;
use super::components::panel::activities;
use super::components::panel::mcp::McpPanelState;
use super::components::panel::memory::MemoryReviewState;
use super::state::UiState;

#[derive(Clone)]
pub(crate) enum ActiveOverlay {
    Mcp(McpPanelState),
    Tasks(activities::TasksOverlay),
    BashDetails(activities::BashDetails),
    MemoryReview(MemoryReviewState),
}

impl Widget for ActiveOverlay {
    type Action = UiAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let height = area.height.unwrap_or(24);
        match self {
            Self::MemoryReview(review) => review.render_lines(area.width, height, context.theme),
            Self::Mcp(panel) => panel.render_lines(area.width),
            Self::Tasks(panel) => panel.render_lines(area.width, height),
            Self::BashDetails(panel) => panel.render_lines(area.width, height, context.theme),
        }
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        InputOutcome::Action(match self {
            Self::MemoryReview(review) => review.handle_key(key.clone()),
            Self::Mcp(panel) => panel.handle_key(key.clone()),
            Self::Tasks(panel) => panel.handle_key(key.clone()),
            Self::BashDetails(panel) => panel.handle_key(key.clone()),
        })
    }
}

pub(crate) fn close_active(state: &mut UiState) {
    let Some(handle) = state.active_overlay.take() else {
        return;
    };
    match state.runtime.remove_widget(handle) {
        Some(ActiveOverlay::Tasks(panel)) => {
            state.activity_selection = panel.activities.selection();
            state.tasks = panel.checklist;
        }
        _ => {}
    }
}

pub(super) fn open_bash_output(state: &mut UiState, panel: activities::BashDetails) {
    open_overlay(state, ActiveOverlay::BashDetails(panel));
}

pub(super) fn open_tasks(state: &mut UiState) {
    close_active(state);
    open_overlay(
        state,
        ActiveOverlay::Tasks(activities::TasksOverlay::new(state)),
    );
}

pub(crate) fn open_mcp(state: &mut UiState, panel: McpPanelState) {
    open_overlay(state, ActiveOverlay::Mcp(panel));
}

pub(crate) fn update_mcp<R>(
    state: &mut UiState,
    update: impl FnOnce(&mut McpPanelState) -> R,
) -> Option<R> {
    let handle = state.active_overlay?;
    state
        .runtime
        .update_widget(handle, |overlay| match overlay {
            ActiveOverlay::Mcp(panel) => Some(update(panel)),
            _ => None,
        })?
}

pub(crate) fn take_mcp(state: &mut UiState) -> Option<McpPanelState> {
    let handle = state.active_overlay.take()?;
    match state.runtime.remove_widget(handle)? {
        ActiveOverlay::Mcp(panel) => Some(panel),
        overlay => {
            open_overlay(state, overlay);
            None
        }
    }
}

pub(crate) fn open_memory_review(state: &mut UiState, review: MemoryReviewState) {
    open_overlay(state, ActiveOverlay::MemoryReview(review));
}

pub(crate) fn memory_review(state: &UiState) -> Option<&MemoryReviewState> {
    match active(state) {
        Some(ActiveOverlay::MemoryReview(review)) => Some(review),
        _ => None,
    }
}

pub(crate) fn close_memory_review(state: &mut UiState) {
    if matches!(active(state), Some(ActiveOverlay::MemoryReview(_))) {
        close_active(state);
    }
}

fn active(state: &UiState) -> Option<&ActiveOverlay> {
    state
        .active_overlay
        .and_then(|handle| state.runtime.widget(handle))
}

fn open_overlay(state: &mut UiState, overlay: ActiveOverlay) {
    close_active(state);
    let options = match &overlay {
        ActiveOverlay::MemoryReview(review) => review.overlay_options(),
        ActiveOverlay::Tasks(_) => activities::work_panel_options(),
        _ => OverlayOptions::upper_centered_panel(),
    };
    state.active_overlay = Some(state.runtime.mount_widget(
        overlay,
        MountOptions::overlay(InputContext::Overlay, options),
    ));
}
