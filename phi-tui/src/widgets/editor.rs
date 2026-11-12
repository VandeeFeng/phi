use std::{collections::HashMap, ops::Range};

use unicode_segmentation::UnicodeSegmentation;

use crate::text::{display_width, pad};
use crate::{CursorPosition, KeyInput, RenderArea, RenderContext, StyledLine, ThemeColor, Widget};

const HISTORY_LIMIT: usize = 100;
const DEFAULT_WIDTH: u16 = 80;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EditorPasteState {
    pastes: HashMap<u64, String>,
    counter: u64,
}

impl EditorPasteState {
    pub fn insert_paste(&mut self, text: &mut String, pasted: &str) {
        let normalized = normalize_paste(pasted);
        let line_count = normalized.split('\n').count();
        let char_count = normalized.chars().count();
        if line_count <= 10 && char_count <= 1000 {
            text.push_str(&normalized);
            return;
        }

        self.counter += 1;
        let paste_id = self.counter;
        self.pastes.insert(paste_id, normalized);
        if line_count > 10 {
            text.push_str(&format!("[paste #{paste_id} +{line_count} lines]"));
        } else {
            text.push_str(&format!("[paste #{paste_id} {char_count} chars]"));
        }
    }

    pub fn expand_markers(&self, text: &str) -> String {
        let mut expanded = text.to_string();
        for (paste_id, content) in &self.pastes {
            let line_marker = format!("[paste #{paste_id} +{} lines]", content.split('\n').count());
            let char_marker = format!("[paste #{paste_id} {} chars]", content.chars().count());
            expanded = expanded.replace(&line_marker, content);
            expanded = expanded.replace(&char_marker, content);
        }
        expanded
    }

    pub fn clear(&mut self) {
        self.pastes.clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorHistoryEntry {
    text: String,
    pastes: EditorPasteState,
}

#[derive(Debug, Clone)]
struct EditorDraft {
    text: String,
    cursor: usize,
    pastes: EditorPasteState,
    cursor_row: Option<usize>,
    preferred_column: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorChange {
    Unchanged,
    Cursor,
    Text,
}

#[derive(Debug, Clone)]
pub struct EditorState {
    text: String,
    cursor: usize,
    pastes: EditorPasteState,
    history: Vec<EditorHistoryEntry>,
    history_index: Option<usize>,
    history_draft: Option<EditorDraft>,
    cursor_row: Option<usize>,
    preferred_column: Option<usize>,
    width: u16,
}

impl Default for EditorState {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            pastes: EditorPasteState::default(),
            history: Vec::new(),
            history_index: None,
            history_draft: None,
            cursor_row: None,
            preferred_column: None,
            width: DEFAULT_WIDTH,
        }
    }
}

impl EditorState {
    /// Handles mechanical editing only; `None` leaves application keys unconsumed.
    pub fn handle_input(&mut self, key: &KeyInput) -> Option<EditorChange> {
        let previous = (self.text.len(), self.cursor);
        match key {
            KeyInput::Char(character) => self.insert(&character.to_string()),
            KeyInput::Paste(text) => self.insert_paste(text),
            KeyInput::Backspace => self.backspace(),
            KeyInput::Left => self.move_left(),
            KeyInput::Right => self.move_right(),
            _ => return None,
        }
        Some(if self.text.len() != previous.0 {
            EditorChange::Text
        } else if self.cursor != previous.1 {
            EditorChange::Cursor
        } else {
            EditorChange::Unchanged
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn cursor_row(&self) -> Option<usize> {
        self.cursor_row
    }

    pub fn set_width(&mut self, width: u16) {
        let width = width.max(1);
        if self.width != width {
            self.width = width;
            self.reset_vertical_navigation();
        }
    }

    pub fn set_text(&mut self, text: impl Into<String>) {
        self.replace_text(text);
        self.pastes.clear();
    }

    pub fn replace_text(&mut self, text: impl Into<String>) {
        self.exit_history();
        self.text = text.into();
        self.cursor = self.text.len();
        self.reset_vertical_navigation();
    }

    pub fn clear(&mut self) {
        self.set_text(String::new());
    }

    pub fn expanded_text(&self, text: &str) -> String {
        self.pastes.expand_markers(text)
    }

    pub fn history_entry(&self, text: impl Into<String>) -> EditorHistoryEntry {
        EditorHistoryEntry {
            text: text.into(),
            pastes: self.pastes.clone(),
        }
    }

    pub fn add_to_history(&mut self, text: impl Into<String>) {
        self.add_history_entry(EditorHistoryEntry {
            text: text.into(),
            pastes: EditorPasteState::default(),
        });
    }

    pub fn add_history_entry(&mut self, mut entry: EditorHistoryEntry) {
        entry.text = entry.text.trim().to_string();
        if entry.text.is_empty()
            || self
                .history
                .first()
                .is_some_and(|item| item.text == entry.text)
        {
            return;
        }
        self.history.insert(0, entry);
        self.history.truncate(HISTORY_LIMIT);
    }

    pub fn insert(&mut self, text: &str) {
        self.before_edit();
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    pub fn insert_paste(&mut self, pasted: &str) {
        self.before_edit();
        let mut inserted = String::new();
        self.pastes.insert_paste(&mut inserted, pasted);
        self.text.insert_str(self.cursor, &inserted);
        self.cursor += inserted.len();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.before_edit();
        let previous = crate::previous_grapheme_boundary(&self.text, self.cursor);
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
    }

    pub fn pop_if(&mut self, expected: char) -> bool {
        if !self.text.ends_with(expected) || self.cursor != self.text.len() {
            return false;
        }
        self.before_edit();
        self.text.pop();
        self.cursor = self.text.len();
        true
    }

    pub fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        let cursor = range.start + replacement.len();
        self.before_edit();
        self.text.replace_range(range, replacement);
        self.cursor = cursor;
    }

    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = crate::previous_grapheme_boundary(&self.text, self.cursor);
            self.reset_vertical_navigation();
        }
    }

    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = crate::next_grapheme_boundary(&self.text, self.cursor);
            self.reset_vertical_navigation();
        }
    }

    pub fn move_up(&mut self) {
        if self.move_vertical(-1) {
            return;
        }
        let layout = EditorLayout::new(&self.text, self.width as usize);
        let (_, column) = layout.cursor_position(self.cursor, self.cursor_row);
        if self.text.is_empty() || self.history_index.is_some() || column == 0 {
            self.navigate_history(true);
        } else {
            self.cursor = 0;
            self.reset_vertical_navigation();
        }
    }

    pub fn move_down(&mut self) {
        if self.move_vertical(1) {
            return;
        }
        if self.history_index.is_some() {
            self.navigate_history(false);
        } else {
            self.cursor = self.text.len();
            self.reset_vertical_navigation();
        }
    }

    pub fn move_cursor_up(&mut self) {
        self.move_vertical(-1);
    }

    pub fn move_cursor_down(&mut self) {
        self.move_vertical(1);
    }

    pub fn visual_rows(&self) -> usize {
        EditorLayout::new(&self.text, self.width as usize)
            .rows
            .len()
    }

    fn move_vertical(&mut self, direction: isize) -> bool {
        let layout = EditorLayout::new(&self.text, self.width as usize);
        let (row, current_column) = layout.cursor_position(self.cursor, self.cursor_row);
        let Some(target_row) = row
            .checked_add_signed(direction)
            .filter(|row| *row < layout.rows.len())
        else {
            return false;
        };
        let column = self.preferred_column.unwrap_or(current_column);
        self.cursor = layout.cursor_at_column(target_row, column);
        self.cursor_row = Some(target_row);
        self.preferred_column = Some(column);
        true
    }

    fn navigate_history(&mut self, older: bool) {
        let next = match (self.history_index, older) {
            (None, true) if !self.history.is_empty() => Some(0),
            (Some(index), true) if index + 1 < self.history.len() => Some(index + 1),
            (Some(0), false) => None,
            (Some(index), false) => Some(index - 1),
            _ => return,
        };
        if self.history_index.is_none() {
            self.history_draft = Some(self.draft());
        }
        self.history_index = next;
        if let Some(index) = next {
            let entry = self.history[index].clone();
            self.text = entry.text;
            self.pastes = entry.pastes;
            self.cursor = if older { 0 } else { self.text.len() };
            self.reset_vertical_navigation();
        } else if let Some(draft) = self.history_draft.take() {
            self.restore(draft);
        }
    }

    fn draft(&self) -> EditorDraft {
        EditorDraft {
            text: self.text.clone(),
            cursor: self.cursor,
            pastes: self.pastes.clone(),
            cursor_row: self.cursor_row,
            preferred_column: self.preferred_column,
        }
    }

    fn restore(&mut self, draft: EditorDraft) {
        self.text = draft.text;
        self.cursor = draft.cursor;
        self.pastes = draft.pastes;
        self.cursor_row = draft.cursor_row;
        self.preferred_column = draft.preferred_column;
    }

    fn before_edit(&mut self) {
        self.exit_history();
        self.reset_vertical_navigation();
    }

    fn exit_history(&mut self) {
        self.history_index = None;
        self.history_draft = None;
    }

    fn reset_vertical_navigation(&mut self) {
        self.preferred_column = None;
        self.cursor_row = None;
    }
}

#[derive(Debug, Clone)]
pub struct Editor {
    pub text: String,
    pub height: u16,
    pub label: Option<String>,
    pub cursor: Option<usize>,
    pub cursor_row: Option<usize>,
}

impl Editor {
    pub fn cursor(&self, width: u16, top: u16) -> (u16, u16) {
        let layout = EditorLayout::new(&self.text, width as usize);
        self.cursor_with_layout(&layout, width, top)
    }

    pub fn render_with_cursor(
        &self,
        context: &RenderContext<'_>,
        top: u16,
    ) -> (Vec<StyledLine>, (u16, u16)) {
        let width = context.area.width;
        let layout = EditorLayout::new(&self.text, width as usize);
        (
            self.render_with_layout(width, &layout),
            self.cursor_with_layout(&layout, width, top),
        )
    }

    fn cursor_with_layout(&self, layout: &EditorLayout<'_>, width: u16, top: u16) -> (u16, u16) {
        let rows = self.visible_rows();
        let (row, col) =
            layout.cursor_position(self.cursor.unwrap_or(self.text.len()), self.cursor_row);
        let visible_start = self.visible_start(layout, rows);
        (
            (col as u16).min(width.saturating_sub(1)),
            top + 1 + row.saturating_sub(visible_start) as u16,
        )
    }

    fn visible_rows(&self) -> usize {
        self.height.saturating_sub(2).max(1) as usize
    }

    fn visible_start(&self, layout: &EditorLayout<'_>, rows: usize) -> usize {
        let cursor = self.cursor.unwrap_or(self.text.len());
        let (cursor_row, _) = layout.cursor_position(cursor, self.cursor_row);
        cursor_row
            .saturating_add(1)
            .saturating_sub(rows)
            .min(layout.rows.len().saturating_sub(rows))
    }

    fn render_with_layout(&self, width: u16, layout: &EditorLayout<'_>) -> Vec<StyledLine> {
        let border_color = ThemeColor::BorderMuted;
        let mut lines = vec![
            StyledLine::new(editor_border(width, self.label.as_deref()), border_color).fit(width),
        ];
        let visible_rows = self.visible_rows();
        let start = self.visible_start(layout, visible_rows);
        for row in 0..visible_rows {
            let text = layout
                .rows
                .get(start + row)
                .map_or("", |line| &self.text[line.clone()]);
            lines.push(StyledLine::new(pad(text, width as usize), ThemeColor::Default).fit(width));
        }
        lines.push(StyledLine::new(editor_border(width, None), border_color).fit(width));
        lines
    }
}

impl Widget for Editor {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let width = context.area.width;
        let layout = EditorLayout::new(&self.text, width as usize);
        self.render_with_layout(width, &layout)
    }

    fn cursor(&self, area: RenderArea) -> Option<CursorPosition> {
        let (col, row) = Editor::cursor(self, area.width, 0);
        Some(CursorPosition {
            row: row as usize,
            col,
        })
    }
}

struct EditorLayout<'a> {
    text: &'a str,
    rows: Vec<Range<usize>>,
}

impl<'a> EditorLayout<'a> {
    fn new(text: &'a str, width: usize) -> Self {
        let mut rows = Vec::new();
        let mut start = 0;
        for line in text.split('\n') {
            push_wrapped_rows(&mut rows, text, start, start + line.len(), width.max(1));
            start += line.len() + 1;
        }
        Self { text, rows }
    }

    fn cursor_position(&self, cursor: usize, row_hint: Option<usize>) -> (usize, usize) {
        let cursor = cursor.min(self.text.len());
        let row = row_hint
            .filter(|row| {
                self.rows
                    .get(*row)
                    .is_some_and(|line| line.contains_cursor(cursor))
            })
            .unwrap_or_else(|| {
                self.rows
                    .iter()
                    .position(|line| line.end >= cursor)
                    .unwrap_or(self.rows.len() - 1)
            });
        let line = &self.rows[row];
        (
            row,
            display_width(&self.text[line.start..cursor.min(line.end)]),
        )
    }

    fn cursor_at_column(&self, row: usize, column: usize) -> usize {
        let line = &self.rows[row];
        let mut cursor = line.start;
        for (offset, grapheme) in self.text[line.clone()].grapheme_indices(true) {
            let next = line.start + offset + grapheme.len();
            if display_width(&self.text[line.start..next]) > column {
                break;
            }
            cursor = next;
        }
        cursor
    }
}

trait CursorRange {
    fn contains_cursor(&self, cursor: usize) -> bool;
}

impl CursorRange for Range<usize> {
    fn contains_cursor(&self, cursor: usize) -> bool {
        self.start <= cursor && cursor <= self.end
    }
}

fn push_wrapped_rows(
    rows: &mut Vec<Range<usize>>,
    text: &str,
    start: usize,
    end: usize,
    width: usize,
) {
    let mut row_start = start;
    let mut used = 0;
    for (offset, grapheme) in text[start..end].grapheme_indices(true) {
        let index = start + offset;
        let grapheme_width = display_width(grapheme);
        if used > 0 && used + grapheme_width > width {
            rows.push(row_start..index);
            row_start = index;
            used = 0;
        }
        used += grapheme_width;
    }
    rows.push(row_start..end);
}

fn editor_border(width: u16, label: Option<&str>) -> String {
    let Some(label) = label.filter(|label| !label.is_empty()) else {
        return "─".repeat(width as usize);
    };
    let prefix = format!("── {label} ");
    if prefix.chars().count() >= width as usize {
        return prefix.chars().take(width as usize).collect();
    }
    format!(
        "{prefix}{}",
        "─".repeat(width as usize - prefix.chars().count())
    )
}

fn normalize_paste(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_keys_report_consumption_text_and_cursor_changes() {
        let mut editor = EditorState::default();
        assert_eq!(editor.handle_input(&KeyInput::Enter), None);
        assert_eq!(editor.handle_input(&KeyInput::Up), None);
        assert_eq!(
            editor.handle_input(&KeyInput::Backspace),
            Some(EditorChange::Unchanged)
        );
        assert_eq!(
            editor.handle_input(&KeyInput::Char('界')),
            Some(EditorChange::Text)
        );
        assert_eq!(
            editor.handle_input(&KeyInput::Left),
            Some(EditorChange::Cursor)
        );
        assert_eq!(
            editor.handle_input(&KeyInput::Left),
            Some(EditorChange::Unchanged)
        );
        assert_eq!(
            editor.handle_input(&KeyInput::Right),
            Some(EditorChange::Cursor)
        );
        assert_eq!(
            editor.handle_input(&KeyInput::Paste("e\u{301}".into())),
            Some(EditorChange::Text)
        );
        assert_eq!(
            editor.handle_input(&KeyInput::Backspace),
            Some(EditorChange::Text)
        );
        assert_eq!(editor.text(), "界");
        assert_eq!(
            editor.handle_input(&KeyInput::Paste(String::new())),
            Some(EditorChange::Unchanged)
        );
    }

    #[test]
    fn cursor_uses_local_unicode_display_width() {
        let editor = Editor {
            text: String::from("a界"),
            height: 3,
            label: None,
            cursor: None,
            cursor_row: None,
        };
        assert_eq!(
            Widget::cursor(&editor, RenderArea::width(20)),
            Some(CursorPosition { row: 1, col: 3 })
        );
    }

    #[test]
    fn history_restores_the_exact_pasted_draft() {
        let mut editor = EditorState::default();
        editor.add_to_history("one\ntwo\nthree");
        let pasted = "x".repeat(1001);
        editor.insert_paste(&pasted);
        let draft = editor.text().to_string();

        editor.move_up();
        editor.move_up();
        assert_eq!(editor.text(), "one\ntwo\nthree");

        let projection = Editor {
            text: editor.text().to_string(),
            height: 4,
            label: None,
            cursor: Some(editor.cursor()),
            cursor_row: editor.cursor_row(),
        };
        let lines = projection.render(&RenderContext::width(20, &crate::Theme::default()));
        assert_eq!(lines[1].text.trim_end(), "one");
        assert_eq!(lines[2].text.trim_end(), "two");

        editor.move_down();
        editor.move_down();
        editor.move_down();
        assert_eq!(editor.text(), draft);
        assert_eq!(editor.cursor(), 0);
        assert_eq!(editor.expanded_text(editor.text()), pasted);
    }

    #[test]
    fn visual_rows_precede_history_and_keep_cursor_in_bounds() {
        let mut editor = EditorState::default();
        editor.set_width(4);
        editor.add_to_history("previous");
        editor.set_text("abcdefgh");

        editor.move_up();
        assert_eq!(editor.text(), "abcdefgh");
        assert_eq!(editor.cursor_row(), Some(0));
        editor.move_up();
        assert_eq!(editor.cursor(), 0);
        editor.move_up();
        assert_eq!(editor.text(), "previous");

        let projection = Editor {
            text: String::from("abcd"),
            height: 3,
            label: None,
            cursor: None,
            cursor_row: None,
        };
        assert_eq!(projection.cursor(4, 0), (3, 1));
    }

    #[test]
    fn history_deduplicates_consecutive_entries_and_caps_size() {
        let mut editor = EditorState::default();
        editor.add_to_history("same");
        editor.add_to_history("same");
        for index in 0..110 {
            editor.add_to_history(format!("prompt {index}"));
        }

        assert_eq!(editor.history.len(), HISTORY_LIMIT);
        assert_eq!(editor.history[0].text, "prompt 109");
    }
}
