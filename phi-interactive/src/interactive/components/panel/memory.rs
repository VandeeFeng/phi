use std::collections::HashMap;

use phi_sdk::extension::memory::TapeAnchor;
use phi_tui::{
    KeyInput, OverlayOptions, OverlaySize, RenderContext, StyledLine, TextInput, Theme, ThemeColor,
    Widget,
    text::{display_width, pad, truncate},
};

use crate::interactive::TapeReview as MemoryReview;
use crate::interactive::components::input::UiAction;

const STAT_BAR_WIDTH: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryReviewMode {
    Timeline,
    Relations,
    Stats,
}

#[derive(Default, Clone)]
struct SearchState {
    query: String,
    active: bool,
}

#[derive(Clone)]
pub(crate) struct MemoryReviewState {
    pub(crate) anchors: Vec<TapeAnchor>,
    pub(crate) selected: usize,
    pub(crate) mode: MemoryReviewMode,
    timeline_search: SearchState,
    relation_search: SearchState,
    review: MemoryReview,
    pub(crate) confirming_delete: bool,
}

impl MemoryReviewState {
    pub(in crate::interactive) fn overlay_options(&self) -> OverlayOptions {
        OverlayOptions {
            width: Some(OverlaySize::Percent(80)),
            max_width: None,
            max_height: Some(OverlaySize::Percent(80)),
            ..OverlayOptions::upper_centered_panel()
        }
    }

    pub(crate) fn new(review: MemoryReview) -> Self {
        let anchors = review.snapshot();
        Self {
            anchors,
            selected: 0,
            mode: MemoryReviewMode::Timeline,
            timeline_search: SearchState::default(),
            relation_search: SearchState::default(),
            review,
            confirming_delete: false,
        }
    }
}

impl MemoryReviewState {
    pub(crate) fn handle_key(&mut self, key: KeyInput) -> UiAction {
        if !matches!(key, KeyInput::Enter | KeyInput::Esc) {
            self.confirming_delete = false;
        }
        match key {
            KeyInput::Enter if self.confirming_delete => {
                delete_selected(self);
                self.confirming_delete = false;
            }
            KeyInput::Enter => {
                if let Some(anchor) = selectable_anchors(self).get(self.selected) {
                    return UiAction::OpenMemoryAnchor(anchor.id.to_string());
                }
            }
            KeyInput::Esc if self.confirming_delete => self.confirming_delete = false,
            KeyInput::Esc if self.search_active() => {
                let search = self.active_search_mut().expect("searchable mode");
                search.query.clear();
                search.active = false;
                reset_selection(self);
            }
            KeyInput::Esc | KeyInput::Ctrl('c') | KeyInput::Char('q') if !self.search_active() => {
                return UiAction::CloseActiveOverlay;
            }
            KeyInput::Ctrl('d') if self.mode != MemoryReviewMode::Stats => {
                self.confirming_delete = true
            }
            KeyInput::Left => previous_mode(self),
            KeyInput::Right | KeyInput::Tab => next_mode(self),
            KeyInput::Up => move_selection(self, -1),
            KeyInput::Down => move_selection(self, 1),
            KeyInput::Char('k') if !self.search_active() => move_selection(self, -1),
            KeyInput::Char('j') if !self.search_active() => move_selection(self, 1),
            KeyInput::Char('t') if !self.search_active() => {
                set_mode(self, MemoryReviewMode::Timeline)
            }
            KeyInput::Char('r') if !self.search_active() => {
                set_mode(self, MemoryReviewMode::Relations)
            }
            KeyInput::Char('s') if !self.search_active() => set_mode(self, MemoryReviewMode::Stats),
            KeyInput::Backspace if self.search_active() => {
                phi_tui::pop_grapheme(
                    &mut self.active_search_mut().expect("searchable mode").query,
                );
                clamp_selection(self);
            }
            KeyInput::Char('/') if !self.search_active() && self.active_search().is_some() => {
                let search = self.active_search_mut().expect("searchable mode");
                search.query.clear();
                search.active = true;
                reset_selection(self);
            }
            KeyInput::Char(c) if self.search_active() => {
                self.active_search_mut()
                    .expect("searchable mode")
                    .query
                    .push(c);
                reset_selection(self);
            }
            _ => {}
        }
        UiAction::None
    }

    fn active_search(&self) -> Option<&SearchState> {
        match self.mode {
            MemoryReviewMode::Timeline => Some(&self.timeline_search),
            MemoryReviewMode::Relations => Some(&self.relation_search),
            MemoryReviewMode::Stats => None,
        }
    }

    fn active_search_mut(&mut self) -> Option<&mut SearchState> {
        match self.mode {
            MemoryReviewMode::Timeline => Some(&mut self.timeline_search),
            MemoryReviewMode::Relations => Some(&mut self.relation_search),
            MemoryReviewMode::Stats => None,
        }
    }

    fn search_active(&self) -> bool {
        self.active_search().is_some_and(|search| search.active)
    }

    fn search_query(&self) -> &str {
        self.active_search().map_or("", |search| &search.query)
    }

    pub(crate) fn render_lines(&self, width: u16, height: usize, theme: &Theme) -> Vec<StyledLine> {
        render_review(self, width, height, theme)
    }
}

fn render_review(
    review: &MemoryReviewState,
    width: u16,
    height: usize,
    theme: &Theme,
) -> Vec<StyledLine> {
    let width = usize::from(width.max(1));
    let frame_width = width.saturating_sub(2);
    let content_width = frame_width.saturating_sub(4).max(1);
    let body_height = height.saturating_sub(8);
    let (body, selected_row) = body_rows(review, content_width);
    let body = visible_body(review, body, selected_row, body_height, content_width);

    let mut lines = Vec::new();
    lines.push(border_top(frame_width));
    lines.push(frame_styled_line(
        StyledLine::new(center("Memory Review", frame_width), ThemeColor::Title).bold(),
        frame_width,
        0,
    ));
    lines.push(frame_styled_line(
        tabs_line(review, frame_width),
        frame_width,
        0,
    ));
    lines.push(frame_styled_line(
        hint_line(review, content_width, theme),
        content_width,
        2,
    ));
    lines.push(separator(frame_width));
    for line in body {
        lines.push(frame_styled_line(line, content_width, 2));
    }
    lines.push(separator(frame_width));
    lines.push(frame_line(&detail(review, content_width), content_width, 2));
    lines.push(border_bottom(frame_width));
    lines
}

fn body_rows(review: &MemoryReviewState, width: usize) -> (Vec<StyledLine>, Option<usize>) {
    if review.anchors.is_empty() {
        return (
            vec![StyledLine::new("No tape anchors found.", ThemeColor::Dim)],
            None,
        );
    }
    match review.mode {
        MemoryReviewMode::Timeline => timeline_rows(review),
        MemoryReviewMode::Relations => relation_rows(review),
        MemoryReviewMode::Stats => (stats_rows(review, width), None),
    }
}

fn timeline_rows(review: &MemoryReviewState) -> (Vec<StyledLine>, Option<usize>) {
    let anchors = search_anchors(review);
    if anchors.is_empty() {
        return (
            vec![StyledLine::new("No anchors match.", ThemeColor::Dim)],
            None,
        );
    }
    let mut rows = vec![
        StyledLine::new(
            "Format: anchor-name [purpose]/[keyword] yyyy-MM-dd-HHmm",
            ThemeColor::Dim,
        ),
        StyledLine::new("", ThemeColor::Default),
    ];
    let selected_row = (review.selected < anchors.len()).then_some(rows.len() + review.selected);
    rows.extend(anchor_rows(review, anchors));
    (rows, selected_row)
}

fn relation_rows(review: &MemoryReviewState) -> (Vec<StyledLine>, Option<usize>) {
    let groups = relation_groups(review);
    if groups.is_empty() {
        return (
            vec![StyledLine::new("No anchors match.", ThemeColor::Dim)],
            None,
        );
    }
    let mut rows = vec![
        StyledLine::new("Keywords", ThemeColor::Title),
        StyledLine::new("", ThemeColor::Default),
    ];
    let mut index = 0usize;
    let mut selected_row = None;
    for (keyword, anchors) in groups {
        rows.push(StyledLine::segments([
            ("[".to_string(), ThemeColor::Default),
            (keyword, ThemeColor::Warning),
            ("]".to_string(), ThemeColor::Default),
        ]));
        for anchor in anchors {
            if index == review.selected {
                selected_row = Some(rows.len());
            }
            rows.push(anchor_row(review, anchor, index, "  "));
            index += 1;
        }
    }
    (rows, selected_row)
}

fn anchor_rows<'a>(review: &MemoryReviewState, anchors: Vec<&'a TapeAnchor>) -> Vec<StyledLine> {
    anchors
        .into_iter()
        .enumerate()
        .map(|(index, anchor)| anchor_row(review, anchor, index, ""))
        .collect()
}

fn anchor_row(
    review: &MemoryReviewState,
    anchor: &TapeAnchor,
    visible_index: usize,
    indent: &str,
) -> StyledLine {
    let selected = visible_index == review.selected;
    let meta = anchor.meta.as_ref();
    let purpose = meta
        .and_then(|meta| meta.purpose.as_deref())
        .unwrap_or("unset");
    let keywords = meta
        .map(|meta| meta.keywords.join(","))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| String::from("unset"));
    let name_color = if review.confirming_delete && selected {
        ThemeColor::Error
    } else {
        ThemeColor::Default
    };
    StyledLine::segments([
        (indent.to_string(), ThemeColor::Default),
        (
            if selected { "● " } else { "○ " }.to_string(),
            if selected {
                ThemeColor::Accent
            } else {
                ThemeColor::Dim
            },
        ),
        (anchor.name.to_string(), name_color),
        (" [".to_string(), ThemeColor::Default),
        (purpose.to_string(), ThemeColor::Warning),
        ("]/ [".replace(" ", ""), ThemeColor::Default),
        (keywords, ThemeColor::Warning),
        ("] ".to_string(), ThemeColor::Default),
        (format_timestamp(anchor.timestamp.as_str()), ThemeColor::Dim),
    ])
    .bold_if(selected)
}

trait BoldIf {
    fn bold_if(self, value: bool) -> Self;
}

impl BoldIf for StyledLine {
    fn bold_if(self, value: bool) -> Self {
        if value { self.bold() } else { self }
    }
}

fn stats_rows(review: &MemoryReviewState, width: usize) -> Vec<StyledLine> {
    let mut purposes = HashMap::<String, usize>::new();
    let mut triggers = HashMap::<String, usize>::new();
    let mut keywords = HashMap::<String, usize>::new();

    for anchor in search_anchors(review) {
        let meta = anchor.meta.as_ref();
        count(
            &mut purposes,
            meta.and_then(|meta| meta.purpose.as_deref())
                .unwrap_or("unset"),
        );
        count(
            &mut triggers,
            meta.and_then(|meta| meta.trigger)
                .map(|trigger| format!("{trigger:?}"))
                .as_deref()
                .unwrap_or("unset"),
        );
        let anchor_keywords = meta.map(|meta| meta.keywords.as_slice()).unwrap_or(&[]);
        if anchor_keywords.is_empty() {
            count(&mut keywords, "unset");
        } else {
            for keyword in anchor_keywords {
                count(&mut keywords, keyword);
            }
        }
    }

    let separator_width = display_width(" │ ") * 2;
    let column_width = ((width.saturating_sub(separator_width)) / 3).max(18);
    let columns = [
        stat_column("Purposes", purposes, column_width),
        stat_column("Triggers", triggers, column_width),
        stat_column("Keywords", keywords, column_width),
    ];
    let row_count = columns.iter().map(Vec::len).max().unwrap_or(0);

    (0..row_count)
        .map(|row| {
            let left = stat_cell(columns[0].get(row), column_width);
            let middle = stat_cell(columns[1].get(row), column_width);
            let right = stat_cell(columns[2].get(row), column_width);
            StyledLine::segments([
                (left, ThemeColor::Default),
                (" │ ".to_string(), ThemeColor::Dim),
                (middle, ThemeColor::Default),
                (" │ ".to_string(), ThemeColor::Dim),
                (right, ThemeColor::Default),
            ])
            .fit(width as u16)
        })
        .collect()
}

fn visible_body(
    review: &MemoryReviewState,
    mut rows: Vec<StyledLine>,
    selected_row: Option<usize>,
    body_height: usize,
    width: usize,
) -> Vec<StyledLine> {
    if body_height == 0 {
        return Vec::new();
    }
    let selected_row = selected_row.unwrap_or(0);
    let start = if review.mode == MemoryReviewMode::Stats {
        0
    } else {
        selected_row.saturating_sub(body_height - 1)
    }
    .min(rows.len().saturating_sub(body_height));
    rows = rows.into_iter().skip(start).take(body_height).collect();
    while rows.len() < body_height {
        rows.push(StyledLine::new("", ThemeColor::Default));
    }
    if let Some(indicator) = selection_indicator(review) {
        let last = rows.len().saturating_sub(1);
        let current = rows[last].text.clone();
        let gap = width.saturating_sub(display_width(&current) + display_width(&indicator));
        rows[last] = StyledLine::segments([
            (
                truncate(
                    &current,
                    width.saturating_sub(display_width(&indicator) + 1),
                ),
                ThemeColor::Dim,
            ),
            (" ".repeat(gap.max(1)), ThemeColor::Default),
            (indicator, ThemeColor::Dim),
        ]);
    }
    rows
}

fn search_anchors(review: &MemoryReviewState) -> Vec<&TapeAnchor> {
    let query = review.search_query().to_lowercase();
    review
        .anchors
        .iter()
        .filter(|anchor| {
            query.is_empty() || anchor_search_text(anchor).to_lowercase().contains(&query)
        })
        .collect()
}

fn relation_groups(review: &MemoryReviewState) -> Vec<(String, Vec<&TapeAnchor>)> {
    let mut groups = HashMap::<String, Vec<&TapeAnchor>>::new();
    for anchor in search_anchors(review) {
        let keywords = anchor
            .meta
            .as_ref()
            .map(|meta| meta.keywords.as_slice())
            .unwrap_or(&[]);
        if keywords.is_empty() {
            groups
                .entry(String::from("unset"))
                .or_default()
                .push(anchor);
        } else {
            for keyword in keywords {
                groups.entry(keyword.clone()).or_default().push(anchor);
            }
        }
    }
    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by(|left, right| left.0.cmp(&right.0));
    groups
}

fn set_mode(review: &mut MemoryReviewState, mode: MemoryReviewMode) {
    review.mode = mode;
    review.confirming_delete = false;
    clamp_selection(review);
}

fn reset_selection(review: &mut MemoryReviewState) {
    review.selected = 0;
}

fn move_selection(review: &mut MemoryReviewState, delta: isize) {
    if review.mode != MemoryReviewMode::Stats {
        review.selected = wrap_index(review.selected, delta, selectable_count(review));
    }
}

fn wrap_index(index: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    ((index as isize + delta).rem_euclid(count as isize)) as usize
}

fn next_mode(review: &mut MemoryReviewState) {
    set_mode(
        review,
        match review.mode {
            MemoryReviewMode::Timeline => MemoryReviewMode::Relations,
            MemoryReviewMode::Relations => MemoryReviewMode::Stats,
            MemoryReviewMode::Stats => MemoryReviewMode::Timeline,
        },
    );
}

fn previous_mode(review: &mut MemoryReviewState) {
    set_mode(
        review,
        match review.mode {
            MemoryReviewMode::Timeline => MemoryReviewMode::Stats,
            MemoryReviewMode::Relations => MemoryReviewMode::Timeline,
            MemoryReviewMode::Stats => MemoryReviewMode::Relations,
        },
    );
}

fn clamp_selection(review: &mut MemoryReviewState) {
    review.selected = review
        .selected
        .min(selectable_count(review).saturating_sub(1));
}

fn selectable_count(review: &MemoryReviewState) -> usize {
    selectable_anchors(review).len()
}

fn delete_selected(review: &mut MemoryReviewState) {
    let id = selectable_anchors(review)
        .get(review.selected)
        .map(|anchor| anchor.id.to_string());
    let Some(id) = id else {
        return;
    };
    if let Ok(anchors) = review.review.delete(&id) {
        review.anchors = anchors;
        clamp_selection(review);
    }
}

fn tabs_line(review: &MemoryReviewState, width: usize) -> StyledLine {
    let modes = [
        (MemoryReviewMode::Timeline, "timeline"),
        (MemoryReviewMode::Relations, "relations"),
        (MemoryReviewMode::Stats, "stats"),
    ];
    let mut used = 0usize;
    let mut segments = Vec::new();
    for (index, (mode, label)) in modes.iter().enumerate() {
        let text = if *mode == review.mode {
            format!("[{label}]")
        } else {
            (*label).to_string()
        };
        let center = ((index * 2 + 1) * width) / (modes.len() * 2);
        let start = center.saturating_sub(display_width(&text) / 2);
        let pad = start.saturating_sub(used);
        if pad > 0 {
            segments.push((" ".repeat(pad), ThemeColor::Default));
            used += pad;
        }
        used += display_width(&text);
        segments.push((
            text,
            if *mode == review.mode {
                ThemeColor::Accent
            } else {
                ThemeColor::Dim
            },
        ));
    }
    StyledLine::segments(segments)
}

fn hint_line(review: &MemoryReviewState, width: usize, theme: &Theme) -> StyledLine {
    if review.search_active() {
        let input = TextInput {
            text: review.search_query().to_string(),
            label: Some(String::from("/")),
        };
        return input
            .render(&RenderContext::width(width as u16, theme))
            .remove(0);
    }
    if review.confirming_delete {
        return StyledLine::new(
            center(
                "Delete anchor? press enter to confirm · esc to cancel",
                width,
            ),
            ThemeColor::Error,
        );
    }
    let delete = if review.mode == MemoryReviewMode::Stats {
        ""
    } else {
        " · ctrl+d delete"
    };
    StyledLine::new(
        center(
            &format!(
                "←/→/tab switch · ↑/↓/j/k select · enter open · / search{delete} · q/ctrl+c close"
            ),
            width,
        ),
        ThemeColor::Dim,
    )
}

fn detail(review: &MemoryReviewState, width: usize) -> String {
    if review.mode == MemoryReviewMode::Stats {
        return String::new();
    }
    let Some(anchor) = selectable_anchors(review).get(review.selected).copied() else {
        return String::new();
    };
    let summary = anchor
        .meta
        .as_ref()
        .and_then(|meta| meta.summary.as_deref())
        .unwrap_or("");
    truncate(&format!("{} · {}", anchor.name, summary), width)
}

fn selection_indicator(review: &MemoryReviewState) -> Option<String> {
    if review.mode == MemoryReviewMode::Stats {
        return None;
    }
    let total = selectable_count(review);
    (total > 0).then(|| format!("{}/{}", review.selected + 1, total))
}

fn selectable_anchors(review: &MemoryReviewState) -> Vec<&TapeAnchor> {
    if review.mode == MemoryReviewMode::Relations {
        return relation_groups(review)
            .into_iter()
            .flat_map(|(_, anchors)| anchors)
            .collect();
    }
    search_anchors(review)
}

fn frame_styled_line(line: StyledLine, width: usize, padding: usize) -> StyledLine {
    let mut line = line.fit(width as u16);
    line = line.prepend_segment(" ".repeat(padding), ThemeColor::Default);
    line.prepend_segment("│", ThemeColor::Dim)
        .prepend_segment("", ThemeColor::Dim)
        .append_segment(" ".repeat(padding), ThemeColor::Default)
        .append_segment("│", ThemeColor::Dim)
}

trait AppendSegment {
    fn append_segment(self, text: impl Into<String>, fg: ThemeColor) -> Self;
}

impl AppendSegment for StyledLine {
    fn append_segment(mut self, text: impl Into<String>, fg: ThemeColor) -> Self {
        let text = text.into();
        self.text.push_str(&text);
        self.segments.push(phi_tui::StyledSegment::new(text, fg));
        self
    }
}

fn frame_line(text: &str, width: usize, padding: usize) -> StyledLine {
    let content = truncate(text, width);
    let gap = width.saturating_sub(display_width(&content));
    StyledLine::segments([
        ("│".to_string(), ThemeColor::Dim),
        (" ".repeat(padding), ThemeColor::Default),
        (content, ThemeColor::Dim),
        (" ".repeat(gap + padding), ThemeColor::Default),
        ("│".to_string(), ThemeColor::Dim),
    ])
}

fn separator(width: usize) -> StyledLine {
    StyledLine::segments([
        ("├".to_string(), ThemeColor::Dim),
        ("─".repeat(width), ThemeColor::Dim),
        ("┤".to_string(), ThemeColor::Dim),
    ])
}

fn border_top(width: usize) -> StyledLine {
    StyledLine::segments([
        ("┌".to_string(), ThemeColor::Dim),
        ("─".repeat(width), ThemeColor::Dim),
        ("┐".to_string(), ThemeColor::Dim),
    ])
}

fn border_bottom(width: usize) -> StyledLine {
    StyledLine::segments([
        ("└".to_string(), ThemeColor::Dim),
        ("─".repeat(width), ThemeColor::Dim),
        ("┘".to_string(), ThemeColor::Dim),
    ])
}

fn center(text: &str, width: usize) -> String {
    let text = truncate(text, width);
    format!(
        "{}{}",
        " ".repeat(width.saturating_sub(display_width(&text)) / 2),
        text
    )
}

fn anchor_search_text(anchor: &TapeAnchor) -> String {
    let meta = anchor.meta.as_ref();
    format!(
        "{} {} {} {} {}",
        anchor.name,
        anchor.timestamp,
        meta.and_then(|meta| meta.summary.as_deref()).unwrap_or(""),
        meta.and_then(|meta| meta.purpose.as_deref()).unwrap_or(""),
        meta.map(|meta| meta.keywords.join(" ")).unwrap_or_default()
    )
}

fn count(map: &mut HashMap<String, usize>, key: &str) {
    *map.entry(key.to_string()).or_default() += 1;
}

fn stat_column(title: &str, values: HashMap<String, usize>, width: usize) -> Vec<String> {
    let mut values = values.into_iter().collect::<Vec<_>>();
    values.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));

    let max = values.first().map(|(_, count)| *count).unwrap_or(0);
    let value_width = max.to_string().len().max(1);
    let bar_start = width
        .saturating_sub(STAT_BAR_WIDTH + value_width + 1)
        .max(6);

    let mut rows = vec![title.to_string(), String::new()];
    rows.extend(values.into_iter().take(8).map(|(label, value)| {
        let label = truncate(&label, bar_start.saturating_sub(1).max(1));
        let gap = " ".repeat(bar_start.saturating_sub(display_width(&label)).max(1));
        format!("{label}{gap}{} {value}", stat_bar(value, max))
    }));
    rows
}

fn stat_cell(text: Option<&String>, width: usize) -> String {
    pad(text.map(String::as_str).unwrap_or(""), width)
}

fn stat_bar(value: usize, max: usize) -> String {
    let size = if max > 0 {
        ((value * STAT_BAR_WIDTH) + (max / 2)) / max
    } else {
        0
    }
    .clamp(usize::from(value > 0), STAT_BAR_WIDTH);
    format!("{}{}", "█".repeat(size), " ".repeat(STAT_BAR_WIDTH - size))
}

fn format_timestamp(value: &str) -> String {
    let value = value.replace('T', "-");
    let value = value.replace(':', "");
    truncate(&value, 15)
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::extension::memory::{TapeAnchorType, TapeService};

    #[test]
    fn selected_anchor_stays_visible_in_grouped_views() {
        use phi_sdk::extension::memory::TapeAnchorMeta;

        let tape = TapeService::default();
        let anchors = (0..12)
            .map(|index| {
                tape.create_anchor_without_transcript(
                    format!("checkpoint/{index}"),
                    TapeAnchorType::Normal,
                    Some(TapeAnchorMeta {
                        keywords: vec![format!("group/{index}")],
                        ..Default::default()
                    }),
                )
                .unwrap()
            })
            .collect();
        let mut review = MemoryReviewState::new(MemoryReview::new(anchors, tape));
        for mode in [MemoryReviewMode::Timeline, MemoryReviewMode::Relations] {
            set_mode(&mut review, mode);
            for selected in 0..selectable_count(&review) {
                review.selected = selected;
                let expected = selectable_anchors(&review)[selected].name.to_string();
                for height in [0, 1, 3, 5, 12] {
                    let (rows, selected_row) = body_rows(&review, 100);
                    let visible = visible_body(&review, rows, selected_row, height, 100);
                    assert_eq!(visible.len(), height);
                    if height > 0 {
                        assert!(
                            visible
                                .iter()
                                .any(|line| line.text.contains(&format!("● {expected} "))),
                            "{mode:?}, selected {selected}, height {height}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn search_input_and_delete_consent_follow_selected_anchor() {
        let tape = TapeService::default();
        let anchors = ["task/jump", "task/keep"]
            .into_iter()
            .map(|name| {
                tape.create_anchor_without_transcript(name, TapeAnchorType::Normal, None)
                    .unwrap()
            })
            .collect();
        let mut review = MemoryReviewState::new(MemoryReview::new(anchors, tape.clone()));
        review.handle_key(KeyInput::Char('/'));
        for character in "task/jump".chars() {
            review.handle_key(KeyInput::Char(character));
        }
        assert_eq!(review.search_query(), "task/jump");
        assert_eq!(search_anchors(&review).len(), 1);
        review.handle_key(KeyInput::Esc);
        review.handle_key(KeyInput::Ctrl('d'));
        assert!(review.confirming_delete);
        review.handle_key(KeyInput::Down);
        assert!(!review.confirming_delete);
        assert!(matches!(
            review.handle_key(KeyInput::Enter),
            UiAction::OpenMemoryAnchor(_)
        ));
        assert_eq!(tape.search(Default::default()).unwrap().len(), 2);
    }

    #[test]
    fn review_cycles_views_searches_and_deletes_selected_anchor() {
        let tape = TapeService::default();
        let first = tape
            .create_anchor_without_transcript("design", TapeAnchorType::Normal, None)
            .unwrap();
        let second = tape
            .create_anchor_without_transcript("implementation", TapeAnchorType::Normal, None)
            .unwrap();
        let mut review =
            MemoryReviewState::new(MemoryReview::new(vec![first, second.clone()], tape.clone()));
        for expected in [
            MemoryReviewMode::Relations,
            MemoryReviewMode::Stats,
            MemoryReviewMode::Timeline,
        ] {
            review.handle_key(KeyInput::Tab);
            assert_eq!(review.mode, expected);
        }
        review.handle_key(KeyInput::Left);
        assert_eq!(review.mode, MemoryReviewMode::Stats);
        review.handle_key(KeyInput::Char('t'));
        review.handle_key(KeyInput::Char('/'));
        for character in "implementation".chars() {
            review.handle_key(KeyInput::Char(character));
        }
        assert_eq!(search_anchors(&review).len(), 1);
        assert!(
            matches!(review.handle_key(KeyInput::Enter), UiAction::OpenMemoryAnchor(id) if id == second.id)
        );
        review.handle_key(KeyInput::Ctrl('d'));
        assert!(review.confirming_delete);
        review.handle_key(KeyInput::Enter);
        assert!(!review.confirming_delete);
        assert!(search_anchors(&review).is_empty());
        assert_eq!(tape.search(Default::default()).unwrap().len(), 1);
    }
}
