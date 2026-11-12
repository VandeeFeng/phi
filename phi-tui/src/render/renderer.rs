use std::{
    env,
    io::{self, Write},
};

use crossterm::cursor::{Hide, MoveDown, MoveToColumn, MoveUp, Show};
use crossterm::queue;
use crossterm::style::{
    Attribute, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, size};
use unicode_segmentation::UnicodeSegmentation;

use crate::render::image::{
    delete_kitty_image, detect_image_protocol, encode_iterm2, encode_kitty,
};
use crate::{HistorySnapshot, ImageProtocol, StyledLine, StyledSegment, Theme, ThemeColor};

const MIN_WIDTH: u16 = 24;
const MAX_RENDER_LINES: usize = 2_000;
const HISTORY_REPLAY_BATCH_ROWS: usize = 64;
const OUTPUT_BUFFER_BYTES: usize = 64 * 1024;
const OSC8_OPEN_PREFIX: &str = "\x1b]8;;";
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: usize,
    pub col: u16,
}

#[derive(Debug, Clone, Default)]
pub struct Frame {
    pub lines: Vec<StyledLine>,
    pub cursor: Option<Cursor>,
    pub logical_origin: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overlay<'a> {
    pub row: usize,
    pub col: u16,
    pub lines: &'a [StyledLine],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayAnchor {
    Center,
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
    LeftCenter,
    RightCenter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlaySize {
    Cells(u16),
    Percent(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OverlayMargin {
    pub top: usize,
    pub right: u16,
    pub bottom: usize,
    pub left: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayOptions {
    pub width: Option<OverlaySize>,
    pub min_width: u16,
    pub max_width: Option<u16>,
    pub max_height: Option<OverlaySize>,
    pub anchor: OverlayAnchor,
    pub row: Option<OverlaySize>,
    pub col: Option<OverlaySize>,
    pub offset_x: i16,
    pub offset_y: isize,
    pub margin: OverlayMargin,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            width: None,
            min_width: 1,
            max_width: None,
            max_height: None,
            anchor: OverlayAnchor::Center,
            row: None,
            col: None,
            offset_x: 0,
            offset_y: 0,
            margin: OverlayMargin::default(),
        }
    }
}

impl OverlayOptions {
    pub fn centered_panel() -> Self {
        Self {
            width: Some(OverlaySize::Percent(100)),
            min_width: 32,
            max_width: Some(72),
            margin: OverlayMargin::horizontal(4),
            anchor: OverlayAnchor::Center,
            ..Self::default()
        }
    }

    pub fn upper_centered_panel() -> Self {
        Self {
            row: Some(OverlaySize::Percent(33)),
            ..Self::centered_panel()
        }
    }
}

impl OverlayMargin {
    pub fn all(value: u16) -> Self {
        Self {
            top: value as usize,
            right: value,
            bottom: value as usize,
            left: value,
        }
    }

    pub fn horizontal(value: u16) -> Self {
        Self {
            left: value,
            right: value,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverlayFrame {
    pub row: usize,
    pub col: u16,
    pub width: u16,
    pub max_height: Option<usize>,
}

impl OverlayFrame {
    pub fn as_overlay<'a>(&self, lines: &'a [StyledLine]) -> Overlay<'a> {
        Overlay {
            row: self.row,
            col: self.col,
            lines,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryChange {
    Unchanged,
    Invalidated,
    Discontinuous,
}

#[derive(Debug, Clone, Copy)]
struct PaintContext<'a> {
    width: u16,
    height: usize,
    cursor: Cursor,
    show_cursor: bool,
    theme: &'a Theme,
    image_protocol: Option<ImageProtocol>,
    content_len: usize,
    history: &'a HistorySnapshot,
    history_prefix_rows: usize,
}

#[derive(Debug, Clone)]
pub struct RenderState {
    previous_painted_rows: Vec<StyledLine>,
    previous_history: HistorySnapshot,
    previous_painted_origin: usize,
    previous_logical_origin: usize,
    previous_logical_row_heights: Vec<usize>,
    previous_width: u16,
    previous_height: u16,
    previous_viewport_top: usize,
    hardware_cursor_row: usize,
    max_lines_rendered: usize,
    previous_content_len: usize,
    clear_on_shrink: bool,
}

impl Default for RenderState {
    fn default() -> Self {
        Self {
            previous_painted_rows: Vec::new(),
            previous_history: HistorySnapshot::default(),
            previous_painted_origin: 0,
            previous_logical_origin: 0,
            previous_logical_row_heights: Vec::new(),
            previous_width: 0,
            previous_height: 0,
            previous_viewport_top: 0,
            hardware_cursor_row: 0,
            max_lines_rendered: 0,
            previous_content_len: 0,
            clear_on_shrink: env::var("PHI_CLEAR_ON_SHRINK").map_or(false, |value| {
                matches!(value.as_str(), "1" | "true") || !matches!(value.as_str(), "0" | "false")
            }),
        }
    }
}

impl RenderState {
    fn resolve_painted_origin(&self, logical_origin: usize) -> (usize, bool) {
        if self.previous_painted_rows.is_empty() {
            return (logical_origin, false);
        }
        if logical_origin < self.previous_logical_origin {
            return (logical_origin, true);
        }
        let dropped_logical_rows = logical_origin - self.previous_logical_origin;
        if dropped_logical_rows >= self.previous_logical_row_heights.len() {
            return (logical_origin, dropped_logical_rows > 0);
        }
        let dropped_painted_rows = self.previous_logical_row_heights[..dropped_logical_rows]
            .iter()
            .sum::<usize>();
        (
            self.previous_painted_origin
                .saturating_add(dropped_painted_rows),
            false,
        )
    }

    fn align_painted_origin(&mut self, origin: usize) -> bool {
        if self.previous_painted_rows.is_empty() {
            self.previous_painted_origin = origin;
            return false;
        }
        if origin < self.previous_painted_origin {
            return true;
        }
        let dropped = origin - self.previous_painted_origin;
        if dropped == 0 {
            return false;
        }
        if dropped >= self.previous_painted_rows.len() {
            return true;
        }
        self.previous_painted_rows.drain(..dropped);
        self.previous_viewport_top = self.previous_viewport_top.saturating_sub(dropped);
        self.hardware_cursor_row = self.hardware_cursor_row.saturating_sub(dropped);
        self.max_lines_rendered = self.max_lines_rendered.saturating_sub(dropped);
        self.previous_content_len = self.previous_content_len.saturating_sub(dropped);
        self.previous_painted_origin = origin;
        false
    }
}

#[derive(Debug, Clone)]
pub struct TuiRenderer {
    state: RenderState,
    theme: Theme,
    image_protocol: Option<ImageProtocol>,
    history_invalidated: bool,
    history: HistorySnapshot,
    retired_image_ids: Vec<u32>,
}

impl TuiRenderer {
    pub fn set_history(&mut self, history: HistorySnapshot) {
        let current = history
            .image_ids()
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        self.retired_image_ids.retain(|id| !current.contains(id));
        self.retired_image_ids.extend(
            self.history
                .image_ids()
                .iter()
                .copied()
                .filter(|id| !current.contains(id)),
        );
        self.history = history;
    }

    pub fn invalidate_history(&mut self) {
        self.history_invalidated = true;
    }

    pub fn new(theme: Theme) -> Self {
        Self {
            state: RenderState::default(),
            theme,
            image_protocol: detect_image_protocol(),
            history_invalidated: false,
            history: HistorySnapshot::default(),
            retired_image_ids: Vec::new(),
        }
    }

    pub fn with_image_protocol(mut self, protocol: Option<ImageProtocol>) -> Self {
        self.image_protocol = protocol;
        self
    }

    pub fn render(&mut self, lines: &[StyledLine], cursor: Option<Cursor>) -> anyhow::Result<()> {
        self.render_with_overlays(lines, &[], cursor)
    }

    pub fn render_with_overlays(
        &mut self,
        lines: &[StyledLine],
        overlays: &[Overlay<'_>],
        cursor: Option<Cursor>,
    ) -> anyhow::Result<()> {
        self.render_materialized_with_overlays(lines.to_vec(), overlays, cursor, 0)
    }

    pub(crate) fn render_materialized_with_overlays(
        &mut self,
        lines: Vec<StyledLine>,
        overlays: &[Overlay<'_>],
        cursor: Option<Cursor>,
        logical_origin: usize,
    ) -> anyhow::Result<()> {
        let stdout = io::stdout();
        let mut stdout = io::BufWriter::with_capacity(OUTPUT_BUFFER_BYTES, stdout.lock());
        let (width, height) = size()?;
        let width = width.saturating_sub(1).max(MIN_WIDTH);
        let height = height as usize;
        let show_cursor = cursor.is_some();
        let cursor = cursor.unwrap_or(Cursor {
            row: lines.len().saturating_sub(1),
            col: 0,
        });
        let (image_lines, cursor, mut logical_row_heights) =
            prepare_owned_image_lines(lines, width, self.image_protocol, cursor);
        let painted_line_count = image_lines.len();
        let mut rendered_lines = if overlays.is_empty() {
            image_lines
        } else {
            compose_owned_overlays(image_lines, overlays, width, height)
        };
        logical_row_heights.extend(std::iter::repeat_n(
            1,
            rendered_lines.len().saturating_sub(painted_line_count),
        ));
        let mut trim_start = rendered_lines.len().saturating_sub(MAX_RENDER_LINES);
        while trim_start > 0 && rendered_lines[trim_start].image_owner.is_some() {
            trim_start -= 1;
        }
        let trimmed_logical_rows = logical_rows_before(&logical_row_heights, trim_start);
        let trimmed_painted_rows = logical_row_heights[..trimmed_logical_rows]
            .iter()
            .sum::<usize>();
        logical_row_heights.drain(..trimmed_logical_rows);
        if trim_start > 0 {
            rendered_lines = rendered_lines.split_off(trim_start);
        }
        let cursor = Cursor {
            row: cursor.row.saturating_sub(trim_start),
            col: cursor.col,
        };
        let retained_logical_origin = logical_origin.saturating_add(trimmed_logical_rows);
        let (painted_origin, origin_discontinuity) =
            self.state.resolve_painted_origin(retained_logical_origin);
        let painted_origin =
            painted_origin.saturating_add(trim_start.saturating_sub(trimmed_painted_rows));
        let mut candidate = self.state.clone();
        let history_change =
            if origin_discontinuity || candidate.align_painted_origin(painted_origin) {
                HistoryChange::Discontinuous
            } else if self.history_invalidated {
                HistoryChange::Invalidated
            } else {
                HistoryChange::Unchanged
            };
        let content_len = rendered_lines.len();
        queue!(stdout, BeginSynchronizedUpdate)?;
        if self.image_protocol == Some(ImageProtocol::Kitty) {
            for id in &self.retired_image_ids {
                queue!(stdout, Print(delete_kitty_image(*id)))?;
            }
        }
        let paint = PaintContext {
            width,
            height,
            cursor,
            show_cursor,
            theme: &self.theme,
            image_protocol: self.image_protocol,
            content_len,
            history: &self.history,
            history_prefix_rows: retained_logical_origin,
        };
        let result = render_diff(
            &mut stdout,
            &mut candidate,
            &rendered_lines,
            paint,
            !overlays.is_empty(),
            history_change,
        );
        queue!(stdout, EndSynchronizedUpdate)?;
        result?;
        stdout.flush()?;
        candidate.previous_painted_rows = rendered_lines;
        candidate.previous_history = self.history.clone();
        candidate.previous_painted_origin = painted_origin;
        candidate.previous_logical_origin = retained_logical_origin;
        candidate.previous_logical_row_heights = logical_row_heights;
        self.state = candidate;
        self.retired_image_ids.clear();
        self.history_invalidated = false;
        Ok(())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        let mut stdout = io::stdout();
        if self.image_protocol == Some(ImageProtocol::Kitty) {
            for id in self
                .history
                .image_ids()
                .iter()
                .chain(&self.retired_image_ids)
            {
                queue!(stdout, Print(delete_kitty_image(*id)))?;
            }
        }
        delete_kitty_images(
            &mut stdout,
            &self.state.previous_painted_rows,
            self.image_protocol,
        )?;
        queue!(
            stdout,
            Hide,
            Clear(ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            Clear(ClearType::Purge)
        )?;
        stdout.flush()?;
        self.state = RenderState::default();
        self.history_invalidated = false;
        self.history = HistorySnapshot::default();
        self.retired_image_ids.clear();
        Ok(())
    }

    pub fn finish(&mut self) -> anyhow::Result<()> {
        if self.state.previous_painted_rows.is_empty() {
            return Ok(());
        }

        let mut stdout = io::stdout();
        let visible_lines = self.state.previous_height as usize;
        let target_row = if visible_lines == 0 {
            self.state.previous_content_len.saturating_sub(1)
        } else {
            self.state
                .previous_viewport_top
                .saturating_add(visible_lines.saturating_sub(1))
                .min(self.state.previous_content_len.saturating_sub(1))
        };
        move_to_row(
            &mut stdout,
            self.state.hardware_cursor_row,
            self.state.previous_viewport_top,
            target_row,
            self.state.previous_viewport_top,
        )?;
        queue!(stdout, MoveToColumn(0), Print("\r\n"), Show)?;
        stdout.flush()?;
        self.state = RenderState::default();
        Ok(())
    }
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new(Theme::default())
    }
}

pub fn resolve_overlay_frame(
    options: OverlayOptions,
    term_width: u16,
    term_height: usize,
    content_height: usize,
) -> OverlayFrame {
    let margin_left = options.margin.left.min(term_width);
    let margin_right = options
        .margin
        .right
        .min(term_width.saturating_sub(margin_left));
    let margin_top = options.margin.top.min(term_height);
    let margin_bottom = options
        .margin
        .bottom
        .min(term_height.saturating_sub(margin_top));
    let available_width = term_width.saturating_sub(margin_left + margin_right).max(1);
    let available_height = term_height
        .saturating_sub(margin_top + margin_bottom)
        .max(1);
    let mut width = options
        .width
        .map(|size| resolve_width(size, term_width))
        .unwrap_or(available_width.min(80));
    width = width.max(options.min_width);
    if let Some(max_width) = options.max_width {
        width = width.min(max_width);
    }
    width = width.min(term_width).max(1);
    let max_height = options
        .max_height
        .map(|size| resolve_height(size, term_height).min(available_height));
    let height = max_height.map_or(content_height, |max| content_height.min(max));
    let row = resolve_overlay_row(options, height, term_height, available_height, margin_top);
    let col = resolve_overlay_col(options, width, term_width, available_width, margin_left);
    OverlayFrame {
        row,
        col,
        width,
        max_height,
    }
}

fn resolve_width(size: OverlaySize, term_width: u16) -> u16 {
    match size {
        OverlaySize::Cells(value) => value,
        OverlaySize::Percent(percent) => term_width.saturating_mul(percent.min(100)) / 100,
    }
}

fn resolve_height(size: OverlaySize, term_height: usize) -> usize {
    match size {
        OverlaySize::Cells(value) => value as usize,
        OverlaySize::Percent(percent) => {
            term_height.saturating_mul(percent.min(100) as usize) / 100
        }
    }
}

fn resolve_overlay_row(
    options: OverlayOptions,
    height: usize,
    term_height: usize,
    available_height: usize,
    margin_top: usize,
) -> usize {
    let ignore_vertical_margins = height > available_height;
    let min_row = if ignore_vertical_margins {
        0
    } else {
        margin_top
    };
    let max_row = if ignore_vertical_margins {
        term_height.saturating_sub(height)
    } else {
        term_height
            .saturating_sub(options.margin.bottom)
            .saturating_sub(height)
    };
    let base = if let Some(row) = options.row {
        match row {
            OverlaySize::Cells(value) => value as usize,
            OverlaySize::Percent(percent) => {
                margin_top
                    + available_height.saturating_sub(height) * percent.min(100) as usize / 100
            }
        }
    } else {
        match options.anchor {
            OverlayAnchor::TopLeft | OverlayAnchor::TopCenter | OverlayAnchor::TopRight => {
                margin_top
            }
            OverlayAnchor::BottomLeft
            | OverlayAnchor::BottomCenter
            | OverlayAnchor::BottomRight => margin_top + available_height.saturating_sub(height),
            OverlayAnchor::LeftCenter | OverlayAnchor::Center | OverlayAnchor::RightCenter => {
                margin_top + available_height.saturating_sub(height) / 2
            }
        }
    };
    apply_offset(base, options.offset_y, min_row, max_row)
}

fn resolve_overlay_col(
    options: OverlayOptions,
    width: u16,
    term_width: u16,
    available_width: u16,
    margin_left: u16,
) -> u16 {
    let ignore_horizontal_margins = width > available_width;
    let min_col = if ignore_horizontal_margins {
        0
    } else {
        margin_left
    };
    let max_col = if ignore_horizontal_margins {
        term_width.saturating_sub(width)
    } else {
        term_width
            .saturating_sub(options.margin.right)
            .saturating_sub(width)
    };
    let base = if let Some(col) = options.col {
        match col {
            OverlaySize::Cells(value) => value,
            OverlaySize::Percent(percent) => {
                margin_left + available_width.saturating_sub(width) * percent.min(100) / 100
            }
        }
    } else {
        match options.anchor {
            OverlayAnchor::TopLeft | OverlayAnchor::LeftCenter | OverlayAnchor::BottomLeft => {
                margin_left
            }
            OverlayAnchor::TopRight | OverlayAnchor::RightCenter | OverlayAnchor::BottomRight => {
                margin_left + available_width.saturating_sub(width)
            }
            OverlayAnchor::TopCenter | OverlayAnchor::Center | OverlayAnchor::BottomCenter => {
                margin_left + available_width.saturating_sub(width) / 2
            }
        }
    };
    apply_offset(
        base as usize,
        options.offset_x as isize,
        min_col as usize,
        max_col as usize,
    ) as u16
}

fn apply_offset(value: usize, offset: isize, min: usize, max: usize) -> usize {
    value
        .saturating_add_signed(offset)
        .clamp(min.min(max), max.max(min))
}

#[cfg(test)]
fn prepare_image_lines(
    lines: &[StyledLine],
    width: u16,
    protocol: Option<ImageProtocol>,
    cursor: Cursor,
) -> (Vec<StyledLine>, Cursor) {
    let (lines, cursor, _) = prepare_owned_image_lines(lines.to_vec(), width, protocol, cursor);
    (lines, cursor)
}

fn prepare_owned_image_lines(
    lines: Vec<StyledLine>,
    width: u16,
    protocol: Option<ImageProtocol>,
    mut cursor: Cursor,
) -> (Vec<StyledLine>, Cursor, Vec<usize>) {
    let mut rendered = Vec::new();
    let mut logical_row_heights = Vec::new();
    for (index, mut source) in lines.into_iter().enumerate() {
        let Some(image) = &source.image else {
            rendered.push(source);
            logical_row_heights.push(1);
            continue;
        };
        let prefix_width = crate::text::display_width(&source.text) as u16;
        if protocol.is_none() || prefix_width >= width {
            let fallback = image.fallback();
            source.image = None;
            source.segments.clear();
            source.text.push_str(&fallback);
            rendered.push(source.fit(width));
            logical_row_heights.push(1);
            continue;
        }
        let size = image.cell_size(width.saturating_sub(prefix_width));
        let image_id = image.id();
        rendered.push(source);
        logical_row_heights.push(size.rows as usize);
        rendered.extend((1..size.rows).map(|_| {
            let mut line = StyledLine::new(String::new(), ThemeColor::Default).fit(width);
            line.image_owner = Some(image_id);
            line
        }));
        if index < cursor.row {
            cursor.row = cursor
                .row
                .saturating_add(size.rows.saturating_sub(1) as usize);
        }
    }
    (rendered, cursor, logical_row_heights)
}

fn logical_rows_before(logical_row_heights: &[usize], painted_rows: usize) -> usize {
    let mut consumed = 0;
    logical_row_heights
        .iter()
        .take_while(|span| {
            if consumed + **span > painted_rows {
                return false;
            }
            consumed += **span;
            true
        })
        .count()
}

fn compose_owned_overlays(
    mut rendered: Vec<StyledLine>,
    overlays: &[Overlay<'_>],
    width: u16,
    height: usize,
) -> Vec<StyledLine> {
    let min_lines_needed = overlays
        .iter()
        .map(|overlay| overlay.row + overlay.lines.len())
        .max()
        .unwrap_or(0);
    let working_height = rendered.len().max(height).max(min_lines_needed);
    rendered.resize_with(working_height, || {
        StyledLine::new(String::new(), ThemeColor::Default).fit(width)
    });
    let viewport_top = working_height.saturating_sub(height);
    for overlay in overlays {
        for (index, line) in overlay.lines.iter().enumerate() {
            let row = viewport_top + overlay.row + index;
            if row >= rendered.len() {
                continue;
            }
            rendered[row] =
                overlay_line(&rendered[row], line, overlay.col as usize, width as usize);
        }
    }
    rendered
}

#[cfg(test)]
mod overlay_style_tests {
    use super::*;

    #[test]
    fn overlay_slices_unicode_by_display_columns() {
        let base = StyledLine::new("界e\u{301}界tail", ThemeColor::Running);
        let overlay = StyledLine::new("X", ThemeColor::Text);
        for (column, expected) in [
            (0, "X e\u{301}界tail"),
            (1, " Xe\u{301}界tail"),
            (2, "界X界tail"),
            (3, "界e\u{301}X tail"),
            (4, "界e\u{301} Xtail"),
        ] {
            let composed = overlay_line(&base, &overlay, column, 9);
            assert_eq!(composed.text, expected);
            assert_eq!(crate::text::display_width(&composed.text), 9);
        }
    }

    #[test]
    fn overlay_preserves_running_dot_color() {
        let base = StyledLine::rich_segments([
            StyledSegment::new("⏺  ", ThemeColor::Running),
            StyledSegment::new("[agent] probe", ThemeColor::Title),
        ]);
        let overlay = StyledLine::new("panel", ThemeColor::Text);
        let composed = overlay_line(&base, &overlay, 5, 24);
        assert_eq!(composed.segments[0], base.segments[0]);
    }
}

fn overlay_line(base: &StyledLine, overlay: &StyledLine, col: usize, width: usize) -> StyledLine {
    if base.image.is_some() || base.image_owner.is_some() {
        return base.clone();
    }
    let col = col.min(width);
    let overlay_width = crate::text::display_width(&overlay.text).min(width - col);
    let overlay = overlay.clone().fit(overlay_width as u16);
    let suffix_start = col + crate::text::display_width(&overlay.text);
    let mut segments = slice_line_segments(base, 0, col);
    segments.extend(slice_line_segments(&overlay, 0, suffix_start - col));
    segments.extend(slice_line_segments(base, suffix_start, width));
    let mut line = StyledLine::rich_segments(segments);
    line.bg = overlay.bg;
    line.fit(width as u16)
}

fn slice_line_segments(line: &StyledLine, start: usize, end: usize) -> Vec<StyledSegment> {
    let fallback;
    let segments = if line.segments.is_empty() {
        fallback = [StyledSegment::new(&line.text, line.fg).with_bold(line.bold)];
        &fallback[..]
    } else {
        &line.segments[..]
    };
    let mut column = 0;
    let mut sliced = Vec::new();
    for segment in segments {
        let mut text = String::new();
        for grapheme in segment.text().graphemes(true) {
            let next_column = column + crate::text::display_width(grapheme);
            if column >= start && next_column <= end {
                text.push_str(grapheme);
            } else if column < end && next_column > start {
                // A partially covered wide grapheme cannot be painted intact.
                text.push_str(&" ".repeat(next_column.min(end) - column.max(start)));
            }
            column = next_column;
        }
        if !text.is_empty() {
            sliced.push(
                StyledSegment::new(text, segment.color())
                    .with_bold(segment.is_bold())
                    .with_italic(segment.is_italic())
                    .with_underline(segment.is_underlined())
                    .with_strikethrough(segment.is_strikethrough())
                    .with_link_uri(segment.link_uri().map(str::to_owned)),
            );
        }
        if column >= end {
            break;
        }
    }
    if column < end {
        sliced.push(StyledSegment::new(
            " ".repeat(end - column.max(start)),
            line.fg,
        ));
    }
    sliced
}

fn render_diff(
    stdout: &mut impl Write,
    state: &mut RenderState,
    lines: &[StyledLine],
    paint: PaintContext<'_>,
    has_overlays: bool,
    history_change: HistoryChange,
) -> anyhow::Result<()> {
    let PaintContext {
        width,
        height,
        cursor,
        show_cursor,
        theme,
        image_protocol,
        content_len,
        ..
    } = paint;
    if state.previous_width == 0 {
        return full_render(stdout, state, lines, paint, false);
    }
    let width_changed = state.previous_width != width;
    let height_changed = state.previous_height != height as u16;
    let replace_history = match history_change {
        HistoryChange::Unchanged => false,
        HistoryChange::Invalidated => {
            state.previous_logical_origin > paint.history_prefix_rows
                || history_prefix_changed(
                    &state.previous_history,
                    paint.history,
                    paint.history_prefix_rows,
                )
        }
        HistoryChange::Discontinuous => {
            state.previous_viewport_top > 0
                || state.previous_logical_origin > 0
                || state.previous_painted_origin > 0
                || paint.history_prefix_rows > 0
        }
    };
    if state.previous_painted_rows.is_empty() || width_changed || height_changed || replace_history
    {
        return full_render(
            stdout,
            state,
            lines,
            paint,
            width_changed || height_changed || replace_history,
        );
    }

    let mut prev_viewport_top = state.previous_viewport_top;
    let mut viewport_top = prev_viewport_top;
    let mut hardware_cursor_row = state.hardware_cursor_row;

    if state.clear_on_shrink && !has_overlays && lines.len() < state.max_lines_rendered {
        return full_render(stdout, state, lines, paint, true);
    }

    let (first_changed, last_changed) = changed_range(&state.previous_painted_rows, lines);
    let Some(first_changed) = first_changed else {
        position_cursor(stdout, state, cursor, show_cursor)?;
        state.previous_content_len = content_len;
        state.previous_height = height as u16;
        return Ok(());
    };
    let last_changed = last_changed.unwrap_or(first_changed);
    let (first_changed, last_changed) = expand_changed_range_for_images(
        &state.previous_painted_rows,
        lines,
        first_changed,
        last_changed,
        width,
    );
    let prev_viewport_bottom = prev_viewport_top + height.saturating_sub(1);
    if first_changed < prev_viewport_top {
        return full_render(stdout, state, lines, paint, true);
    }

    let appended_lines = lines.len() > state.previous_painted_rows.len();
    let append_start =
        appended_lines && first_changed == state.previous_painted_rows.len() && first_changed > 0;
    let removed_rows = state
        .previous_painted_rows
        .len()
        .saturating_sub(lines.len());
    let move_target = if first_changed >= lines.len() {
        lines.len().saturating_sub(1)
    } else if append_start {
        first_changed - 1
    } else {
        first_changed
    };
    if move_target < prev_viewport_top || removed_rows > height {
        return full_render(stdout, state, lines, paint, true);
    }

    queue!(stdout, Hide)?;
    if first_changed < state.previous_painted_rows.len() {
        let end = last_changed.min(state.previous_painted_rows.len() - 1);
        delete_kitty_images(
            stdout,
            &state.previous_painted_rows[first_changed..=end],
            image_protocol,
        )?;
    }
    if move_target > prev_viewport_bottom {
        let current_screen_row = hardware_cursor_row
            .saturating_sub(prev_viewport_top)
            .min(height.saturating_sub(1));
        let move_to_bottom = height.saturating_sub(1).saturating_sub(current_screen_row);
        if move_to_bottom > 0 {
            queue!(stdout, MoveDown(move_to_bottom as u16))?;
        }
        let scroll = move_target - prev_viewport_bottom;
        for _ in 0..scroll {
            queue!(stdout, Print("\r\n"))?;
        }
        prev_viewport_top += scroll;
        viewport_top += scroll;
        hardware_cursor_row = move_target;
    }

    move_to_row(
        stdout,
        hardware_cursor_row,
        prev_viewport_top,
        move_target,
        viewport_top,
    )?;
    if append_start {
        queue!(stdout, Print("\r\n"))?;
    } else {
        queue!(stdout, MoveToColumn(0))?;
    }
    let render_end = last_changed.min(lines.len().saturating_sub(1));
    let mut final_cursor_row = if first_changed >= lines.len() {
        move_target
    } else {
        write_lines(
            stdout,
            lines,
            first_changed,
            render_end,
            width,
            theme,
            image_protocol,
        )?
    };
    if removed_rows > 0 {
        if render_end < lines.len().saturating_sub(1) {
            let move_down = lines.len() - 1 - render_end;
            queue!(stdout, MoveDown(move_down as u16))?;
            final_cursor_row = lines.len() - 1;
        }
        let clear_offset = usize::from(!lines.is_empty());
        queue!(stdout, MoveToColumn(0))?;
        if clear_offset > 0 {
            queue!(stdout, MoveDown(clear_offset as u16))?;
        }
        for row in 0..removed_rows {
            queue!(stdout, Clear(ClearType::CurrentLine))?;
            if row + 1 < removed_rows {
                queue!(stdout, MoveDown(1))?;
            }
        }
        let move_back = removed_rows - 1 + clear_offset;
        if move_back > 0 {
            queue!(stdout, MoveUp(move_back as u16))?;
        }
    }

    state.hardware_cursor_row = final_cursor_row;
    state.max_lines_rendered = state.max_lines_rendered.max(lines.len());
    state.previous_viewport_top =
        prev_viewport_top.max(final_cursor_row.saturating_add(1).saturating_sub(height));
    position_cursor(stdout, state, cursor, show_cursor)?;
    state.previous_content_len = content_len;
    state.previous_width = width;
    state.previous_height = height as u16;
    Ok(())
}

fn full_render(
    stdout: &mut impl Write,
    state: &mut RenderState,
    lines: &[StyledLine],
    paint: PaintContext<'_>,
    clear: bool,
) -> anyhow::Result<()> {
    let PaintContext {
        width,
        height,
        cursor,
        show_cursor,
        theme,
        image_protocol,
        content_len,
        ..
    } = paint;
    // Do not make the current viewport wait behind a full scrollback replay.
    // The following replacement purges this preview, so it cannot duplicate history.
    if clear && height > 0 && (paint.history_prefix_rows > 0 || lines.len() > height) {
        render_visible_preview(stdout, state, lines, paint)?;
    }
    queue!(stdout, Hide)?;
    if clear {
        delete_kitty_images(stdout, &state.previous_painted_rows, image_protocol)?;
        queue!(
            stdout,
            Clear(ClearType::All),
            crossterm::cursor::MoveTo(0, 0),
            // A forced full redraw re-emits the retained transcript tail. Purge the old
            // terminal scrollback first so those rows cannot accumulate as duplicate
            // session history after resize, origin discontinuity, or renderer recovery.
            // Incremental renders do not take this path and therefore preserve native
            // scrollback. The order is intentional: clear the viewport, return home,
            // then discard only the stale scrollback before writing the replacement frame.
            Clear(ClearType::Purge)
        )?;
    }

    write_history_prefix(stdout, paint)?;
    let buffer_length = height.max(lines.len());
    let viewport_top = buffer_length.saturating_sub(height);
    let render_start = 0;
    let render_lines = &lines[render_start..];
    if !render_lines.is_empty() {
        write_lines(
            stdout,
            lines,
            render_start,
            lines.len() - 1,
            width,
            theme,
            image_protocol,
        )?;
    }

    state.previous_content_len = content_len;
    state.previous_width = width;
    state.previous_height = height as u16;
    state.max_lines_rendered = if clear {
        lines.len()
    } else {
        state.max_lines_rendered.max(lines.len())
    };
    state.previous_viewport_top = viewport_top;
    state.hardware_cursor_row = render_start.saturating_add(render_lines.len().saturating_sub(1));
    position_cursor(stdout, state, cursor, show_cursor)?;
    Ok(())
}

fn render_visible_preview(
    stdout: &mut impl Write,
    state: &mut RenderState,
    lines: &[StyledLine],
    mut paint: PaintContext<'_>,
) -> anyhow::Result<()> {
    let start = lines.len().saturating_sub(paint.height);
    let mut visible = lines[start..].to_vec();
    // A clipped image continuation has no image header; only full replay can restore it.
    for line in visible
        .iter_mut()
        .take_while(|line| line.image_owner.is_some())
    {
        *line = StyledLine::new(String::new(), ThemeColor::Default);
    }
    paint.show_cursor &= paint.cursor.row >= start;
    paint.cursor.row = paint.cursor.row.saturating_sub(start);
    paint.content_len = visible.len();
    paint.history_prefix_rows = 0;
    full_render(stdout, state, &visible, paint, true)?;
    // Flushing alone does not commit a synchronized frame; end it before replay starts.
    queue!(stdout, EndSynchronizedUpdate)?;
    stdout.flush()?;
    queue!(stdout, BeginSynchronizedUpdate)?;
    Ok(())
}

fn write_history_prefix(stdout: &mut impl Write, paint: PaintContext<'_>) -> anyhow::Result<()> {
    let mut remaining = paint.history_prefix_rows;
    for chunk in paint.history.chunks() {
        let count = remaining.min(chunk.len());
        for batch in chunk[..count].chunks(HISTORY_REPLAY_BATCH_ROWS) {
            let (lines, _, _) = prepare_owned_image_lines(
                batch.to_vec(),
                paint.width,
                paint.image_protocol,
                Cursor { row: 0, col: 0 },
            );
            write_lines(
                stdout,
                &lines,
                0,
                lines.len() - 1,
                paint.width,
                paint.theme,
                paint.image_protocol,
            )?;
            queue!(stdout, Print("\r\n"))?;
        }
        remaining -= count;
        if remaining == 0 {
            break;
        }
    }
    Ok(())
}

fn expand_changed_range_for_images(
    previous: &[StyledLine],
    lines: &[StyledLine],
    first_changed: usize,
    last_changed: usize,
    width: u16,
) -> (usize, usize) {
    let mut expanded = (first_changed, last_changed);
    for source in [previous, lines] {
        for (row, line) in source.iter().enumerate() {
            let Some(image) = &line.image else {
                continue;
            };
            let prefix = crate::text::display_width(&line.text) as u16;
            let end = row + image.cell_size(width.saturating_sub(prefix)).rows as usize - 1;
            if row <= last_changed && end >= first_changed {
                expanded.0 = expanded.0.min(row);
                expanded.1 = expanded.1.max(end);
            }
        }
    }
    expanded
}

fn history_prefix_changed(
    previous_history: &HistorySnapshot,
    current_history: &HistorySnapshot,
    rows: usize,
) -> bool {
    let mut remaining = rows;
    for (previous, current) in previous_history
        .chunks()
        .iter()
        .zip(current_history.chunks())
    {
        if remaining == 0 {
            return false;
        }
        if previous.len() != current.len() {
            return history_prefix_rows_differ(previous_history, current_history, rows);
        }
        let count = remaining.min(current.len());
        if !std::sync::Arc::ptr_eq(previous, current) && previous[..count] != current[..count] {
            return true;
        }
        remaining -= count;
    }
    remaining > 0
}

fn history_prefix_rows_differ(
    previous: &HistorySnapshot,
    current: &HistorySnapshot,
    rows: usize,
) -> bool {
    previous
        .chunks()
        .iter()
        .flat_map(|chunk| chunk.iter())
        .take(rows)
        .ne(current
            .chunks()
            .iter()
            .flat_map(|chunk| chunk.iter())
            .take(rows))
}

fn changed_range(previous: &[StyledLine], lines: &[StyledLine]) -> (Option<usize>, Option<usize>) {
    let mut first_changed = None;
    let mut last_changed = None;
    for row in 0..previous.len().max(lines.len()) {
        if previous.get(row) != lines.get(row) {
            first_changed.get_or_insert(row);
            last_changed = Some(row);
        }
    }
    (first_changed, last_changed)
}

fn move_to_row(
    stdout: &mut impl Write,
    hardware_cursor_row: usize,
    prev_viewport_top: usize,
    target_row: usize,
    viewport_top: usize,
) -> anyhow::Result<()> {
    let current_screen_row = hardware_cursor_row.saturating_sub(prev_viewport_top) as isize;
    let target_screen_row = target_row.saturating_sub(viewport_top) as isize;
    match target_screen_row - current_screen_row {
        diff if diff > 0 => queue!(stdout, MoveDown(diff as u16))?,
        diff if diff < 0 => queue!(stdout, MoveUp((-diff) as u16))?,
        _ => {}
    }
    Ok(())
}

fn position_cursor(
    stdout: &mut impl Write,
    state: &mut RenderState,
    cursor: Cursor,
    show: bool,
) -> anyhow::Result<()> {
    let viewport_bottom = state
        .previous_viewport_top
        .saturating_add((state.previous_height as usize).saturating_sub(1));
    let cursor_row = cursor
        .row
        .clamp(state.previous_viewport_top, viewport_bottom);
    move_to_row(
        stdout,
        state.hardware_cursor_row,
        state.previous_viewport_top,
        cursor_row,
        state.previous_viewport_top,
    )?;
    queue!(stdout, MoveToColumn(cursor.col))?;
    if show {
        queue!(stdout, Show)?;
    } else {
        queue!(stdout, Hide)?;
    }
    state.hardware_cursor_row = cursor_row;
    Ok(())
}

fn write_lines<W: Write>(
    stdout: &mut W,
    lines: &[StyledLine],
    start: usize,
    end: usize,
    width: u16,
    theme: &Theme,
    protocol: Option<ImageProtocol>,
) -> anyhow::Result<usize> {
    let mut row = start;
    while row <= end {
        if row > start {
            queue!(stdout, Print("\r\n"))?;
        }
        let Some(image) = &lines[row].image else {
            queue!(stdout, Clear(ClearType::CurrentLine))?;
            write_line(stdout, &lines[row], width, theme, protocol)?;
            row += 1;
            continue;
        };
        let prefix = crate::text::display_width(&lines[row].text) as u16;
        let rows = image.cell_size(width.saturating_sub(prefix)).rows as usize;
        let rows = rows.min(end - row + 1);
        queue!(stdout, Clear(ClearType::CurrentLine))?;
        for _ in 1..rows {
            queue!(stdout, Print("\r\n"), Clear(ClearType::CurrentLine))?;
        }
        if rows > 1 {
            queue!(stdout, MoveUp((rows - 1) as u16))?;
        }
        write_line(stdout, &lines[row], width, theme, protocol)?;
        if protocol == Some(ImageProtocol::Kitty) && rows > 1 {
            queue!(stdout, MoveDown((rows - 1) as u16))?;
        }
        row += rows;
    }
    Ok(row.saturating_sub(1))
}

fn delete_kitty_images<W: Write>(
    stdout: &mut W,
    lines: &[StyledLine],
    protocol: Option<ImageProtocol>,
) -> anyhow::Result<()> {
    if protocol != Some(ImageProtocol::Kitty) {
        return Ok(());
    }
    for image in lines.iter().filter_map(|line| line.image.as_ref()) {
        queue!(stdout, Print(delete_kitty_image(image.id())))?;
    }
    Ok(())
}

fn write_line<W: Write>(
    stdout: &mut W,
    line: &StyledLine,
    width: u16,
    theme: &Theme,
    image_protocol: Option<ImageProtocol>,
) -> anyhow::Result<()> {
    queue!(
        stdout,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(theme.color(line.fg))
    )?;
    if let Some(bg) = line.bg {
        queue!(stdout, SetBackgroundColor(theme.color(bg)))?;
    }
    if line.bold {
        queue!(stdout, SetAttribute(Attribute::Bold))?;
    }
    if line.segments.is_empty() {
        queue!(stdout, Print(&line.text))?;
    } else {
        for segment in &line.segments {
            queue!(
                stdout,
                SetAttribute(Attribute::Reset),
                SetForegroundColor(theme.color(segment.color()))
            )?;
            if let Some(bg) = line.bg {
                queue!(stdout, SetBackgroundColor(theme.color(bg)))?;
            }
            if segment.is_bold() {
                queue!(stdout, SetAttribute(Attribute::Bold))?;
            }
            if segment.is_italic() {
                queue!(stdout, SetAttribute(Attribute::Italic))?;
            }
            if segment.is_underlined() {
                queue!(stdout, SetAttribute(Attribute::Underlined))?;
            }
            if segment.is_strikethrough() {
                queue!(stdout, SetAttribute(Attribute::CrossedOut))?;
            }
            write_segment(stdout, segment)?;
        }
    }
    queue!(stdout, SetAttribute(Attribute::Reset), ResetColor)?;
    if let (Some(image), Some(protocol)) = (&line.image, image_protocol) {
        let prefix_width = crate::text::display_width(&line.text) as u16;
        let size = image.cell_size(width.saturating_sub(prefix_width));
        let sequence = match protocol {
            ImageProtocol::Kitty => encode_kitty(image, size),
            ImageProtocol::Iterm2 => encode_iterm2(image, size),
        };
        queue!(stdout, Print(sequence))?;
    }
    Ok(())
}

fn write_segment<W: Write>(stdout: &mut W, segment: &StyledSegment) -> anyhow::Result<()> {
    let Some(uri) = segment.link_uri().filter(|uri| link_uri_safe(uri)) else {
        queue!(stdout, Print(segment.text()))?;
        return Ok(());
    };
    write!(stdout, "{OSC8_OPEN_PREFIX}{uri}\x1b\\")?;
    queue!(stdout, Print(segment.text()))?;
    write!(stdout, "{OSC8_CLOSE}")?;
    Ok(())
}

fn link_uri_safe(uri: &str) -> bool {
    !uri.is_empty() && !uri.chars().any(char::is_control)
}

#[cfg(test)]
mod render_state_tests {
    use super::*;

    #[test]
    fn linked_segment_uses_complete_osc8_destination() {
        let segment = StyledSegment::new("wrapped label", ThemeColor::Default)
            .with_underline(true)
            .with_link_uri(Some("https://example.com/full/destination".to_string()));
        let mut output = Vec::new();

        write_segment(&mut output, &segment).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "\x1b]8;;https://example.com/full/destination\x1b\\wrapped label\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn logical_origin_accounts_for_image_rows_and_rebases_retained_lines() {
        let mut state = RenderState {
            previous_painted_rows: (0..7)
                .map(|index| StyledLine::new(index.to_string(), ThemeColor::Default))
                .collect(),
            previous_painted_origin: 20,
            previous_logical_origin: 10,
            previous_logical_row_heights: vec![5, 1, 1],
            previous_viewport_top: 5,
            hardware_cursor_row: 6,
            max_lines_rendered: 7,
            previous_content_len: 7,
            ..RenderState::default()
        };

        let (painted_origin, discontinuity) = state.resolve_painted_origin(11);
        assert_eq!((painted_origin, discontinuity), (25, false));
        assert!(!state.align_painted_origin(painted_origin));
        assert_eq!(state.previous_painted_origin, 25);
        assert_eq!(state.previous_painted_rows.len(), 2);
        assert_eq!(state.previous_viewport_top, 0);
        assert_eq!(state.hardware_cursor_row, 1);
        assert_eq!(state.max_lines_rendered, 2);
        assert_eq!(state.previous_content_len, 2);
    }

    #[test]
    fn non_overlapping_or_backward_origins_require_redraw() {
        let mut state = RenderState {
            previous_painted_rows: vec![StyledLine::new("line", ThemeColor::Default)],
            previous_painted_origin: 5,
            ..RenderState::default()
        };

        assert!(state.align_painted_origin(4));
        assert!(state.align_painted_origin(6));
        assert_eq!(state.previous_painted_origin, 5);
    }

    #[test]
    fn no_cursor_stays_hidden_on_full_and_unchanged_frames() {
        let lines = vec![StyledLine::new("dashboard", ThemeColor::Default)];
        let theme = Theme::default();
        let mut state = RenderState::default();
        let history = HistorySnapshot::default();
        let paint = PaintContext {
            width: 80,
            height: 24,
            cursor: Cursor { row: 0, col: 0 },
            show_cursor: false,
            theme: &theme,
            image_protocol: None,
            content_len: lines.len(),
            history: &history,
            history_prefix_rows: 0,
        };
        let mut output = Vec::new();
        render_diff(
            &mut output,
            &mut state,
            &lines,
            paint,
            false,
            HistoryChange::Unchanged,
        )
        .unwrap();
        assert!(output.ends_with(b"\x1b[?25l"));
        state.previous_painted_rows = lines.clone();
        output.clear();
        render_diff(
            &mut output,
            &mut state,
            &lines,
            paint,
            false,
            HistoryChange::Unchanged,
        )
        .unwrap();
        assert!(output.ends_with(b"\x1b[?25l"));
    }

    #[derive(Default)]
    struct FlushedOutput {
        pending: Vec<u8>,
        frames: Vec<String>,
    }

    impl Write for FlushedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.pending.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.frames
                .push(String::from_utf8(std::mem::take(&mut self.pending)).unwrap());
            Ok(())
        }
    }

    #[test]
    fn replacement_commits_visible_editor_and_footer_before_history_replay() {
        let history = HistorySnapshot::new(
            vec![
                (0..3_000)
                    .map(|index| StyledLine::new(format!("history-{index}"), ThemeColor::Default))
                    .collect::<Vec<_>>()
                    .into(),
            ],
            Vec::new(),
        );
        let mut tail = history.tail(48);
        tail.extend(["editor", "footer"].map(|text| StyledLine::new(text, ThemeColor::Default)));
        let theme = Theme::default();
        let paint = PaintContext {
            width: 80,
            height: 24,
            cursor: Cursor { row: 48, col: 3 },
            show_cursor: true,
            theme: &theme,
            image_protocol: None,
            content_len: tail.len(),
            history: &history,
            history_prefix_rows: history.rows() - 48,
        };
        let mut state = RenderState {
            previous_painted_rows: vec![
                StyledLine::new("previous-session", ThemeColor::Default);
                tail.len()
            ],
            previous_width: 80,
            previous_height: 24,
            previous_viewport_top: 26,
            hardware_cursor_row: 48,
            previous_content_len: tail.len(),
            ..RenderState::default()
        };
        let mut output = FlushedOutput::default();
        queue!(output, BeginSynchronizedUpdate).unwrap();
        render_diff(
            &mut output,
            &mut state,
            &tail,
            paint,
            false,
            HistoryChange::Invalidated,
        )
        .unwrap();
        queue!(output, EndSynchronizedUpdate).unwrap();
        output.flush().unwrap();

        assert_eq!(output.frames.len(), 2);
        let visible = &output.frames[0];
        assert!(visible.starts_with("\x1b[?2026h"));
        assert!(visible.ends_with("\x1b[?2026l"));
        assert!(visible.contains("editor") && visible.contains("footer"));
        assert!(!visible.contains("previous-session"));
        assert_eq!(visible.matches("history-").count(), 22);
        assert!(!visible.contains("history-0"));
        let replacement = &output.frames[1];
        assert!(replacement.starts_with("\x1b[?2026h"));
        assert!(replacement.ends_with("\x1b[?2026l"));
        assert!(replacement.find("\x1b[3J").unwrap() < replacement.find("history-0").unwrap());
        assert_eq!(replacement.matches("history-").count(), 3_000);
        assert_eq!(replacement.matches("editor").count(), 1);
        assert_eq!(replacement.matches("footer").count(), 1);
        assert_eq!(state.previous_viewport_top, 26);
        assert_eq!(state.hardware_cursor_row, 48);
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::*;

    const SCROLLBACK_CAPACITY: usize = 1_000;
    const PURGE_SCROLLBACK: &str = "\x1b[3J";

    struct TestTerminal {
        parser: vt100::Parser,
        state: RenderState,
        history: HistorySnapshot,
        has_overlays: bool,
    }

    impl TestTerminal {
        fn new(width: u16, height: u16) -> Self {
            let mut parser = vt100::Parser::new(height, width, SCROLLBACK_CAPACITY);
            for row in 0..height + 2 {
                parser.process(format!("shell-{row}\r\n").as_bytes());
            }
            Self {
                parser,
                state: RenderState {
                    clear_on_shrink: false,
                    ..RenderState::default()
                },
                history: HistorySnapshot::default(),
                has_overlays: false,
            }
        }

        fn paint(&mut self, lines: &[StyledLine], change: HistoryChange) -> Vec<u8> {
            let (height, width) = self.parser.screen().size();
            let prefix = self.history.rows().saturating_sub(lines.len());
            let paint = PaintContext {
                width: width - 1,
                height: height as usize,
                cursor: Cursor {
                    row: lines.len().saturating_sub(1),
                    col: 0,
                },
                show_cursor: !lines.is_empty(),
                theme: &Theme::default(),
                image_protocol: None,
                content_len: lines.len(),
                history: &self.history,
                history_prefix_rows: prefix,
            };
            let mut output = Vec::new();
            render_diff(
                &mut output,
                &mut self.state,
                lines,
                paint,
                self.has_overlays,
                change,
            )
            .unwrap();
            self.process(&output);
            self.state.previous_painted_rows = lines.to_vec();
            self.state.previous_history = self.history.clone();
            self.state.previous_logical_origin = prefix;
            self.state.previous_painted_origin = prefix;
            output
        }

        fn process(&mut self, output: &[u8]) {
            let output = std::str::from_utf8(output).unwrap();
            for (index, chunk) in output.split(PURGE_SCROLLBACK).enumerate() {
                if index > 0 {
                    // vt100 lacks ED3; keep the viewport while discarding its scrollback.
                    let screen = self.parser.screen().state_formatted();
                    let (height, width) = self.parser.screen().size();
                    self.parser = vt100::Parser::new(height, width, SCROLLBACK_CAPACITY);
                    self.parser.process(&screen);
                }
                self.parser.process(chunk.as_bytes());
            }
        }

        fn buffer(&mut self) -> Vec<String> {
            self.parser.screen_mut().set_scrollback(usize::MAX);
            let count = self.parser.screen().scrollback();
            let width = self.parser.screen().size().1;
            let mut rows = Vec::new();
            for offset in (1..=count).rev() {
                self.parser.screen_mut().set_scrollback(offset);
                rows.push(self.parser.screen().rows(0, width).next().unwrap());
            }
            self.parser.screen_mut().set_scrollback(0);
            rows.extend(self.parser.screen().rows(0, width));
            rows
        }

        fn assert_viewport(&self, lines: &[StyledLine]) {
            let (height, width) = self.parser.screen().size();
            let mut expected = lines
                .iter()
                .map(|line| line.text.clone())
                .collect::<Vec<_>>();
            expected.resize(height as usize, String::new());
            assert_eq!(
                self.parser.screen().rows(0, width).collect::<Vec<_>>(),
                expected
            );
            assert_eq!(
                self.parser.screen().cursor_position(),
                (lines.len().saturating_sub(1) as u16, 0)
            );
        }
    }

    fn lines(count: usize) -> Vec<StyledLine> {
        (0..count)
            .map(|row| StyledLine::new(format!("row-{row}"), ThemeColor::Default))
            .collect()
    }

    #[test]
    fn visible_completion_preserves_shell_buffer_and_terminal_anchor() {
        for height in [8_u16, 38, 60] {
            for width in [32, 80] {
                for change in [-3_isize, 0, 1, 5] {
                    let mut terminal = TestTerminal::new(width, height);
                    let previous = lines(height as usize - 2);
                    terminal.paint(&previous, HistoryChange::Unchanged);
                    let mut expected = terminal.buffer();
                    let anchor = expected.iter().position(|row| row == "row-0").unwrap();
                    let mut completed = lines(previous.len().saturating_add_signed(change));
                    completed.last_mut().unwrap().text = "completed".into();
                    let affected_rows = previous.len().max(completed.len());
                    expected.resize(anchor + affected_rows, String::new());
                    for row in 0..affected_rows {
                        expected[anchor + row] = completed
                            .get(row)
                            .map_or_else(String::new, |line| line.text.clone());
                    }
                    terminal.paint(&completed, HistoryChange::Invalidated);
                    assert_eq!(terminal.buffer(), expected);
                    let cursor_row = (height as isize - 1 + change.min(0)) as u16;
                    assert_eq!(terminal.parser.screen().cursor_position(), (cursor_row, 0));
                }
            }
        }
    }

    #[test]
    fn deletion_then_append_and_middle_update_keep_cursor_and_history_stable() {
        let mut terminal = TestTerminal::new(40, 8);
        let previous = lines(8);
        terminal.paint(&previous, HistoryChange::Unchanged);
        let before = terminal.buffer();
        let history_len = before.len() - 8;
        for count in [3, 0, 1, 6] {
            terminal.paint(&previous[..count], HistoryChange::Unchanged);
            terminal.assert_viewport(&previous[..count]);
        }
        let mut updated = previous[..6].to_vec();
        updated[1].text = "updated".into();
        terminal.paint(&updated, HistoryChange::Unchanged);
        terminal.assert_viewport(&updated);
        let after = terminal.buffer();
        assert_eq!(after.len(), before.len());
        assert_eq!(&after[..history_len], &before[..history_len]);
    }

    #[test]
    fn session_replacement_shrink_clears_old_rows_without_corrupting_next_append() {
        for height in [8_u16, 38, 60] {
            let height_rows = height as usize;
            for (previous_rows, frame_start) in [
                (height_rows, 0),
                (height_rows + 16, 0),
                (height_rows + 16, 16),
            ] {
                let previous = lines(previous_rows);
                for replacement_rows in [0, 1, 3] {
                    let mut terminal = TestTerminal::new(80, height);
                    terminal.history =
                        HistorySnapshot::new(vec![previous.clone().into()], Vec::new());
                    terminal.paint(&previous[frame_start..], HistoryChange::Unchanged);
                    let before = terminal.buffer();
                    let requires_replacement = previous_rows > height_rows;
                    let expected_prefix = if requires_replacement {
                        Vec::new()
                    } else {
                        before[..before.len() - height_rows].to_vec()
                    };
                    let mut replacement = (0..replacement_rows)
                        .map(|row| {
                            StyledLine::new(format!("new-session-{row}"), ThemeColor::Default)
                        })
                        .collect::<Vec<_>>();
                    terminal.history =
                        HistorySnapshot::new(vec![replacement.clone().into()], Vec::new());
                    let output = terminal.paint(&replacement, HistoryChange::Invalidated);
                    assert_eq!(
                        String::from_utf8(output)
                            .unwrap()
                            .matches(PURGE_SCROLLBACK)
                            .count(),
                        usize::from(requires_replacement)
                    );
                    terminal.assert_viewport(&replacement);
                    let buffer = terminal.buffer();
                    assert_eq!(buffer.len(), expected_prefix.len() + height_rows);
                    assert_eq!(&buffer[..expected_prefix.len()], expected_prefix);
                    replacement.push(StyledLine::new("new-session-append", ThemeColor::Default));
                    terminal.history =
                        HistorySnapshot::new(vec![replacement.clone().into()], Vec::new());
                    let output = terminal.paint(&replacement, HistoryChange::Unchanged);
                    assert!(
                        !String::from_utf8(output)
                            .unwrap()
                            .contains(PURGE_SCROLLBACK)
                    );
                    terminal.assert_viewport(&replacement);
                    let buffer = terminal.buffer();
                    assert_eq!(buffer.len(), expected_prefix.len() + height_rows);
                    assert_eq!(&buffer[..expected_prefix.len()], expected_prefix);
                }
            }
        }
    }

    #[test]
    fn overlay_closure_restores_frame_and_shell_history() {
        let mut terminal = TestTerminal::new(40, 8);
        let base = lines(8);
        terminal.paint(&base, HistoryChange::Unchanged);
        let before = terminal.buffer();
        let panel = vec![StyledLine::new("permission", ThemeColor::Default)];
        let overlay = Overlay {
            row: 2,
            col: 3,
            lines: &panel,
        };
        let composed = compose_owned_overlays(base.clone(), &[overlay], 39, 8);
        terminal.has_overlays = true;
        terminal.paint(&composed, HistoryChange::Unchanged);
        assert!(terminal.parser.screen().contents().contains("permission"));
        terminal.has_overlays = false;
        terminal.paint(&base, HistoryChange::Unchanged);
        terminal.assert_viewport(&base);
        assert_eq!(terminal.buffer(), before);
    }

    #[test]
    fn purge_adapter_preserves_viewport_and_cursor() {
        let mut terminal = TestTerminal::new(40, 8);
        let viewport = terminal.parser.screen().contents();
        let cursor = terminal.parser.screen().cursor_position();
        assert!(terminal.buffer().len() > 8);
        terminal.process(PURGE_SCROLLBACK.as_bytes());
        assert_eq!(terminal.buffer().len(), 8);
        assert_eq!(terminal.parser.screen().contents(), viewport);
        assert_eq!(terminal.parser.screen().cursor_position(), cursor);
    }

    #[test]
    fn visible_invalidation_with_offscreen_history_does_not_replace() {
        for prefix_rows in [0, 32] {
            let mut terminal = TestTerminal::new(40, 8);
            let previous = lines(40);
            terminal.history = HistorySnapshot::new(vec![previous.clone().into()], Vec::new());
            terminal.paint(&previous[prefix_rows..], HistoryChange::Unchanged);
            let before = terminal.buffer();
            let mut updated = previous.clone();
            updated[39].text = "completed".into();
            terminal.history = HistorySnapshot::new(vec![updated.clone().into()], Vec::new());
            let output = terminal.paint(&updated[prefix_rows..], HistoryChange::Invalidated);
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains(PURGE_SCROLLBACK)
            );
            let mut expected = before;
            *expected.last_mut().unwrap() = "completed".into();
            assert_eq!(terminal.buffer(), expected);
            assert_eq!(terminal.parser.screen().cursor_position(), (7, 0));
            let output = terminal.paint(&updated[prefix_rows..], HistoryChange::Invalidated);
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains(PURGE_SCROLLBACK)
            );
            assert_eq!(terminal.buffer(), expected);
        }
    }

    #[test]
    fn advancing_paint_tail_does_not_replace_unchanged_prefix() {
        let mut terminal = TestTerminal::new(40, 8);
        let previous = lines(40);
        terminal.history = HistorySnapshot::new(vec![previous.clone().into()], Vec::new());
        terminal.paint(&previous[32..], HistoryChange::Unchanged);
        let before = terminal.buffer();
        let appended = lines(44);
        terminal.history = HistorySnapshot::new(vec![appended.clone().into()], Vec::new());
        assert!(!terminal.state.align_painted_origin(36));
        let output = terminal.paint(&appended[36..], HistoryChange::Invalidated);
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains(PURGE_SCROLLBACK)
        );
        let mut expected = before;
        expected.extend(appended[40..].iter().map(|line| line.text.clone()));
        assert_eq!(terminal.buffer(), expected);
        terminal.assert_viewport(&appended[36..]);
    }

    #[test]
    fn rechunking_unchanged_history_does_not_replace() {
        for split in [8, 16, 31, 32] {
            let mut terminal = TestTerminal::new(40, 8);
            let history = lines(40);
            terminal.history = HistorySnapshot::new(vec![history.clone().into()], Vec::new());
            terminal.paint(&history[32..], HistoryChange::Unchanged);
            let before = terminal.buffer();
            terminal.history = HistorySnapshot::new(
                vec![
                    history[..split].to_vec().into(),
                    history[split..].to_vec().into(),
                ],
                Vec::new(),
            );
            let output = terminal.paint(&history[32..], HistoryChange::Invalidated);
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains(PURGE_SCROLLBACK)
            );
            assert_eq!(terminal.buffer(), before);
        }
    }

    #[test]
    fn changed_prefix_replaces_even_when_visible_tail_is_unchanged() {
        let mut terminal = TestTerminal::new(40, 8);
        let mut history = lines(40);
        terminal.history = HistorySnapshot::new(vec![history.clone().into()], Vec::new());
        terminal.paint(&history[32..], HistoryChange::Unchanged);
        history[0].text = "updated prefix".into();
        terminal.history = HistorySnapshot::new(vec![history.clone().into()], Vec::new());
        let output = terminal.paint(&history[32..], HistoryChange::Invalidated);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains(PURGE_SCROLLBACK)
        );
        assert_eq!(
            terminal.buffer(),
            history
                .iter()
                .map(|line| line.text.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn origin_discontinuity_replaces_even_matching_rows() {
        let mut terminal = TestTerminal::new(40, 8);
        let history = lines(40);
        terminal.history = HistorySnapshot::new(vec![history.clone().into()], Vec::new());
        terminal.paint(&history[32..], HistoryChange::Unchanged);
        let output = terminal.paint(&history[32..], HistoryChange::Discontinuous);
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains(PURGE_SCROLLBACK)
        );
        assert_eq!(
            terminal.buffer(),
            history
                .iter()
                .map(|line| line.text.clone())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn geometry_changes_replace_once_then_allow_incremental_append() {
        for (height, width) in [(6, 40), (12, 40), (8, 32), (8, 60)] {
            let mut terminal = TestTerminal::new(40, 8);
            terminal.paint(&lines(4), HistoryChange::Unchanged);
            terminal.parser.screen_mut().set_size(height, width);
            let output = terminal.paint(&lines(4), HistoryChange::Unchanged);
            assert!(
                String::from_utf8(output)
                    .unwrap()
                    .contains(PURGE_SCROLLBACK)
            );
            terminal.assert_viewport(&lines(4));
            let output = terminal.paint(&lines(5), HistoryChange::Unchanged);
            assert!(
                !String::from_utf8(output)
                    .unwrap()
                    .contains(PURGE_SCROLLBACK)
            );
            terminal.assert_viewport(&lines(5));
        }
    }

    #[test]
    fn clear_on_shrink_controls_optional_replacement() {
        for clear_on_shrink in [false, true] {
            for has_overlays in [false, true] {
                let mut terminal = TestTerminal::new(40, 8);
                terminal.state.clear_on_shrink = clear_on_shrink;
                terminal.has_overlays = has_overlays;
                terminal.paint(&lines(8), HistoryChange::Unchanged);
                let output = terminal.paint(&lines(3), HistoryChange::Unchanged);
                assert_eq!(
                    String::from_utf8(output)
                        .unwrap()
                        .contains(PURGE_SCROLLBACK),
                    clear_on_shrink && !has_overlays
                );
                terminal.assert_viewport(&lines(3));
                let output = terminal.paint(&lines(4), HistoryChange::Unchanged);
                assert!(
                    !String::from_utf8(output)
                        .unwrap()
                        .contains(PURGE_SCROLLBACK)
                );
                terminal.assert_viewport(&lines(4));
            }
        }
    }

    #[test]
    fn offscreen_history_replacement_replays_once_and_resets_cursor() {
        let mut terminal = TestTerminal::new(40, 8);
        terminal.history = HistorySnapshot::new(vec![lines(40).into()], Vec::new());
        let output = terminal.paint(&lines(40)[32..], HistoryChange::Invalidated);
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains(PURGE_SCROLLBACK)
        );
        let retained = lines(20);
        terminal.history = HistorySnapshot::new(vec![retained.clone().into()], Vec::new());
        terminal.paint(&retained[12..], HistoryChange::Invalidated);
        let expected = retained
            .iter()
            .map(|line| line.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(terminal.buffer(), expected);
        assert_eq!(terminal.parser.screen().cursor_position(), (7, 0));
        let output = terminal.paint(&retained[12..], HistoryChange::Invalidated);
        assert!(
            !String::from_utf8(output)
                .unwrap()
                .contains(PURGE_SCROLLBACK)
        );
        assert_eq!(terminal.buffer(), expected);
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;
    use crate::TerminalImage;

    fn image_line() -> StyledLine {
        let image = TerminalImage::from_base64(
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
            "image/png",
        )
        .unwrap();
        StyledLine::image(image)
    }

    #[test]
    fn supported_images_reserve_rows_and_translate_cursor() {
        let lines = vec![image_line(), StyledLine::new("editor", ThemeColor::Default)];
        let cursor = Cursor { row: 1, col: 0 };

        let (rendered, cursor) =
            prepare_image_lines(&lines, 60, Some(ImageProtocol::Kitty), cursor);

        assert_eq!(rendered.len(), 31);
        assert_eq!(cursor.row, 30);
        assert!(rendered[0].image.is_some());
        assert!(
            rendered[1..30]
                .iter()
                .all(|line| line.image_owner.is_some())
        );
    }

    #[test]
    fn kitty_image_footprint_is_cleared_then_painted_atomically() {
        let (lines, _) = prepare_image_lines(
            &[image_line()],
            60,
            Some(ImageProtocol::Kitty),
            Cursor { row: 0, col: 0 },
        );
        let mut output = Vec::new();

        write_lines(
            &mut output,
            &lines,
            0,
            lines.len() - 1,
            60,
            &Theme::default(),
            Some(ImageProtocol::Kitty),
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("\x1b[29A"));
        assert!(output.contains("\x1b_Ga=T,f=100,q=2,C=1,c=60,r=30"));
        assert!(output.contains("\x1b[29B"));
    }

    #[test]
    fn visible_preview_does_not_emit_a_clipped_image() {
        let (lines, cursor) = prepare_image_lines(
            &[image_line(), StyledLine::new("editor", ThemeColor::Default)],
            60,
            Some(ImageProtocol::Kitty),
            Cursor { row: 1, col: 0 },
        );
        let theme = Theme::default();
        let history = HistorySnapshot::default();
        let paint = PaintContext {
            width: 60,
            height: 24,
            cursor,
            show_cursor: true,
            theme: &theme,
            image_protocol: Some(ImageProtocol::Kitty),
            content_len: lines.len(),
            history: &history,
            history_prefix_rows: 0,
        };
        let mut output = Vec::new();
        full_render(
            &mut output,
            &mut RenderState::default(),
            &lines,
            paint,
            true,
        )
        .unwrap();
        let output = String::from_utf8(output).unwrap();
        let (visible, replacement) = output.split_once("\x1b[?2026l\x1b[?2026h").unwrap();
        assert!(visible.contains("editor"));
        assert!(!visible.contains("\x1b_G"));
        assert!(replacement.contains("\x1b_Ga=T"));
    }

    #[test]
    fn unsupported_images_render_compact_fallbacks() {
        let (rendered, _) =
            prepare_image_lines(&[image_line()], 60, None, Cursor { row: 0, col: 0 });

        assert_eq!(rendered.len(), 1);
        assert!(rendered[0].text.starts_with("[Image: image/png 1x1]"));
        assert!(rendered[0].image.is_none());
    }
}
