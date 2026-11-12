mod layout;
pub mod render;
mod runtime;
mod terminal;
pub mod widgets;

pub use layout::CodingFrame;
pub use render::{
    Cursor, Frame, HistoryBudget, HistoryFootprint, HistorySnapshot, ImageProtocol, Overlay,
    OverlayAnchor, OverlayFrame, OverlayLayer, OverlayMargin, OverlayOptions, OverlaySize,
    RenderState, StyledLine, StyledSegment, TerminalImage, Theme, ThemeColor, TuiRenderer,
    render_markdown, render_overlay_layers, resolve_overlay_frame, text,
};
pub use runtime::{
    ComposedFrame, CursorPosition, FocusId, InputContext, InputOutcome, KeyBinding,
    KeyBindingResolver, MountOptions, MountPlacement, RenderArea, RenderContext, RenderInvalidator,
    RenderPriority, RenderRequest, RenderScheduler, RuntimeEvent, TuiRuntime, Widget, WidgetHandle,
};
pub use terminal::{KeyInput, TerminalGuard, poll_key_input, poll_key_input_with_timeout};
pub use widgets::autocomplete::{Autocomplete, AutocompleteAction};
pub use widgets::dialog::{
    Choice, ChoiceAction, ConfirmAction, ConfirmDialog, TextInputAction, TextInputDialog,
};
pub use widgets::footer::{
    Footer, FooterAlign, FooterLine, FooterSection, current_git_branch, model_with_thinking_level,
    path_with_git_branch,
};
pub use widgets::menu::{
    MenuState, Picker, PickerAction, PickerState, SelectItem, SelectMenu, SelectMenuState,
    VisibleRange, centered_visible_range,
};
pub use widgets::notification::{Notification, NotifyKind, NotifyMessage};
pub use widgets::transcript::{
    BlockBody, BlockExpansion, BlockRole, BlockStatus, BlockText, ImageBlock,
    MaterializedTranscript, OutputSection, Transcript, TranscriptBlock, TranscriptBlockView,
    TranscriptGroup, TranscriptRenderRequest, TranscriptTail,
};
pub use widgets::{
    DiffLine, DiffLineKind, DiffView, Editor, EditorChange, EditorHistoryEntry, EditorPasteState,
    EditorState, FileMentionState, Panel, TextInput, TrackedBlock, complete_file_mention,
    expand_file_mentions, render_diff,
};

pub use render::text::{
    abbreviate_home_paths, compact_path, home_to_tilde, next_grapheme_boundary, pop_grapheme,
    previous_grapheme_boundary,
};

pub fn terminal_width() -> anyhow::Result<u16> {
    Ok(crossterm::terminal::size()?.0.saturating_sub(1).max(24))
}
