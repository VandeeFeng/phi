mod history;
mod image;
mod line;
mod markdown;
mod overlay;
mod renderer;
pub mod text;
mod theme;

pub(crate) use history::line_footprint;
pub use history::{HistoryBudget, HistoryFootprint, HistorySnapshot};
pub use image::{ImageProtocol, TerminalImage};
pub use line::{StyledLine, StyledSegment};
pub use markdown::render_markdown;
pub(crate) use markdown::{IncrementalMarkdownState, MarkdownTail, render_markdown_tail};
pub use overlay::{OverlayLayer, render_overlay_layers};
pub use renderer::{
    Cursor, Frame, Overlay, OverlayAnchor, OverlayFrame, OverlayMargin, OverlayOptions,
    OverlaySize, RenderState, TuiRenderer, resolve_overlay_frame,
};
pub use theme::{Theme, ThemeColor};
