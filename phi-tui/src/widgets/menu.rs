use std::ops::Range;

use crate::text::{self, pop_grapheme};
use crate::{InputOutcome, KeyInput, RenderContext, StyledLine, ThemeColor, Widget};

const DEFAULT_PRIMARY_WIDTH: usize = 32;
const PRIMARY_GAP: usize = 2;
const MIN_DESCRIPTION_WIDTH: usize = 10;

#[derive(Debug, Clone)]
pub struct SelectItem {
    pub value: String,
    pub label: String,
    pub description: String,
}

impl SelectItem {
    pub fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
            description: description.into(),
        }
    }

    pub fn with_label(
        value: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuState {
    pub open: bool,
    pub selected: usize,
}

impl MenuState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.selected = 0;
    }

    pub fn toggle(&mut self) {
        if self.open {
            self.close();
        } else {
            self.open();
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    pub fn move_up(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected == 0 {
            self.selected = len - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self, len: usize) {
        if len == 0 || self.selected + 1 >= len {
            self.selected = 0;
        } else {
            self.selected += 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerAction {
    Cancelled,
    Selected(String),
}

#[derive(Debug, Clone)]
pub struct PickerState<K> {
    pub kind: K,
    pub title: String,
    pub items: Vec<SelectItem>,
    pub all_items: Vec<SelectItem>,
    pub query: String,
    pub filterable: bool,
    pub menu: SelectMenuState,
}

impl<K> PickerState<K> {
    pub fn with_kind(kind: K, title: impl Into<String>, items: Vec<SelectItem>) -> Self {
        Self {
            kind,
            title: title.into(),
            all_items: items.clone(),
            items,
            query: String::new(),
            filterable: false,
            menu: SelectMenuState::default(),
        }
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.menu.selected_item(&self.items)
    }

    pub fn move_up(&mut self) {
        self.menu.move_up(self.items.len());
    }

    pub fn move_down(&mut self) {
        self.menu.move_down(self.items.len());
    }

    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items.clone();
        self.all_items = items;
        self.menu.clamp(self.items.len());
    }

    pub fn filter_with<F>(&mut self, filter: F)
    where
        F: FnOnce(&[SelectItem], &str) -> Vec<SelectItem>,
    {
        self.items = filter(&self.all_items, &self.query);
        self.menu.selected = 0;
    }

    pub fn handle_input_with_filter<F>(
        &mut self,
        key: &KeyInput,
        filter: F,
    ) -> InputOutcome<PickerAction>
    where
        F: Fn(&[SelectItem], &str) -> Vec<SelectItem>,
    {
        match key {
            KeyInput::Up => self.move_up(),
            KeyInput::Down => self.move_down(),
            KeyInput::Esc | KeyInput::Ctrl('c') => {
                return InputOutcome::Action(PickerAction::Cancelled);
            }
            KeyInput::Enter => {
                let Some(item) = self.selected_item() else {
                    return InputOutcome::Consumed;
                };
                return InputOutcome::Action(PickerAction::Selected(item.value.clone()));
            }
            KeyInput::Backspace if self.filterable => {
                pop_grapheme(&mut self.query);
                self.filter_with(filter);
            }
            KeyInput::Char(c) if self.filterable => {
                self.query.push(*c);
                self.filter_with(filter);
            }
            KeyInput::Paste(text) if self.filterable => {
                self.query.push_str(text.trim());
                self.filter_with(filter);
            }
            _ => return InputOutcome::Ignored,
        }
        InputOutcome::Consumed
    }
}

pub struct Picker<K> {
    pub state: PickerState<K>,
    pub max_visible: usize,
}

impl<K> Picker<K> {
    pub fn new(state: PickerState<K>) -> Self {
        Self {
            state,
            max_visible: 10,
        }
    }

    fn default_filter(items: &[SelectItem], query: &str) -> Vec<SelectItem> {
        let query = query.to_lowercase();
        items
            .iter()
            .filter(|item| {
                item.label.to_lowercase().contains(&query)
                    || item.description.to_lowercase().contains(&query)
            })
            .cloned()
            .collect()
    }
}

impl<K> Widget for Picker<K> {
    type Action = PickerAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let title = if self.state.filterable {
            format!("{} {}", self.state.title, self.state.query)
        } else {
            self.state.title.clone()
        };
        SelectMenu {
            title: Some(title),
            items: self.state.items.clone(),
            selected: self.state.menu.selected,
            max_visible: self.max_visible,
        }
        .render(context)
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        self.state
            .handle_input_with_filter(key, Self::default_filter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRange {
    pub start: usize,
    pub end: usize,
}

impl VisibleRange {
    pub fn range(self) -> Range<usize> {
        self.start..self.end
    }

    pub fn len(self) -> usize {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(self) -> bool {
        self.start >= self.end
    }
}

pub fn centered_visible_range(selected: usize, total: usize, max_visible: usize) -> VisibleRange {
    if total == 0 || max_visible == 0 {
        return VisibleRange { start: 0, end: 0 };
    }

    let visible = total.min(max_visible);
    let selected = selected.min(total.saturating_sub(1));
    let start = selected
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));

    VisibleRange {
        start,
        end: start + visible,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectMenuState {
    pub selected: usize,
    pub open: bool,
}

impl Default for SelectMenuState {
    fn default() -> Self {
        Self {
            selected: 0,
            open: true,
        }
    }
}

impl SelectMenuState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn closed() -> Self {
        Self {
            selected: 0,
            open: false,
        }
    }

    pub fn open(&mut self) {
        self.open = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.selected = 0;
    }

    pub fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }

    pub fn move_up(&mut self, len: usize) {
        if len == 0 {
            self.selected = 0;
        } else if self.selected == 0 {
            self.selected = len - 1;
        } else {
            self.selected -= 1;
        }
    }

    pub fn move_down(&mut self, len: usize) {
        if len == 0 || self.selected + 1 >= len {
            self.selected = 0;
        } else {
            self.selected += 1;
        }
    }

    pub fn selected_item<'a>(&self, items: &'a [SelectItem]) -> Option<&'a SelectItem> {
        items.get(self.selected)
    }
}

#[derive(Debug, Clone, Default)]
pub struct SelectMenu {
    pub title: Option<String>,
    pub items: Vec<SelectItem>,
    pub selected: usize,
    pub max_visible: usize,
}

impl Widget for SelectMenu {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width;
        let selected = self.selected.min(self.items.len().saturating_sub(1));
        let range = centered_visible_range(selected, self.items.len(), self.max_visible.max(1));
        let visible = range.len();
        let mut lines = Vec::with_capacity(visible + 2);

        if let Some(title) = self.title.as_deref() {
            if let Some(rest) = title.strip_prefix("delete session?") {
                lines.push(
                    StyledLine::segments([
                        ("  delete session?", ThemeColor::Error),
                        (rest, ThemeColor::Dim),
                    ])
                    .fit(width),
                );
            } else {
                lines.push(StyledLine::new(format!("  {title}"), ThemeColor::Dim).fit(width));
            }
        }
        let visible_items = self.items[range.range()]
            .iter()
            .enumerate()
            .map(|(offset, item)| (range.start + offset, item));
        let primary_width = primary_width(self.items.iter(), width);

        for (row, item) in visible_items {
            lines.push(render_menu_item(
                &item.value,
                &item.label,
                &item.description,
                row == selected,
                width,
                primary_width,
            ));
        }
        if self.items.len() > visible {
            lines.push(
                StyledLine::new(
                    format!("  ({}/{})", selected + 1, self.items.len()),
                    ThemeColor::Dim,
                )
                .fit(width),
            );
        }

        lines
    }
}

fn primary_width<'a>(items: impl Iterator<Item = &'a SelectItem>, width: u16) -> usize {
    let widest = items
        .map(|item| text::display_width(&item.label))
        .max()
        .unwrap_or(DEFAULT_PRIMARY_WIDTH);
    (widest + PRIMARY_GAP)
        .min(DEFAULT_PRIMARY_WIDTH)
        .min(width.saturating_sub(2) as usize)
}

fn render_menu_item(
    value: &str,
    label: &str,
    description: &str,
    selected: bool,
    width: u16,
    primary_width: usize,
) -> StyledLine {
    let prefix = if label.starts_with("name: ") {
        ""
    } else if selected {
        "→ "
    } else {
        "  "
    };
    let prefix_width = text::display_width(prefix);

    if value.starts_with("resume:") && !description.is_empty() {
        return render_right_aligned_menu_item(prefix, label, description, selected, width);
    }

    if !description.is_empty() && width > 40 {
        let primary_width = primary_width.min((width as usize).saturating_sub(prefix_width + 4));
        let name = text::truncate(label, primary_width.saturating_sub(PRIMARY_GAP).max(1));
        let gap = " ".repeat(
            primary_width
                .saturating_sub(text::display_width(&name))
                .max(1),
        );
        let used = prefix_width + text::display_width(&name) + gap.len();
        let description_width = (width as usize).saturating_sub(used + 2);
        if description_width > MIN_DESCRIPTION_WIDTH {
            let description = text::truncate(description, description_width);
            return styled_menu_item(prefix, name, gap, description, selected);
        }
    }

    let name = text::truncate(label, (width as usize).saturating_sub(prefix_width + 2));
    styled_menu_item(prefix, name, String::new(), String::new(), selected)
}

fn render_right_aligned_menu_item(
    prefix: &str,
    label: &str,
    description: &str,
    selected: bool,
    width: u16,
) -> StyledLine {
    let width = width as usize;
    let prefix_width = text::display_width(prefix);
    let description_width = text::display_width(description);
    let available = width.saturating_sub(prefix_width + description_width + 1);
    let name = text::truncate(label, available.max(1));
    let used = prefix_width + text::display_width(&name) + description_width;
    let gap = " ".repeat(width.saturating_sub(used).max(1));
    styled_menu_item(prefix, name, gap, description.to_string(), selected)
}

fn styled_menu_item(
    prefix: &str,
    name: String,
    gap: String,
    description: String,
    selected: bool,
) -> StyledLine {
    let prefix_color = if selected {
        ThemeColor::Accent
    } else {
        ThemeColor::Muted
    };
    let name_color = if selected {
        ThemeColor::Accent
    } else {
        ThemeColor::Default
    };
    StyledLine::segments([
        (prefix.to_string(), prefix_color),
        (name, name_color),
        (gap, ThemeColor::Default),
        (description, ThemeColor::Dim),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_range_handles_boundaries() {
        let cases = [
            ((0, 0, 5), VisibleRange { start: 0, end: 0 }),
            ((1, 3, 5), VisibleRange { start: 0, end: 3 }),
            ((5, 10, 5), VisibleRange { start: 3, end: 8 }),
            ((0, 10, 5), VisibleRange { start: 0, end: 5 }),
            ((9, 10, 5), VisibleRange { start: 5, end: 10 }),
            ((99, 10, 5), VisibleRange { start: 5, end: 10 }),
            ((0, 10, 0), VisibleRange { start: 0, end: 0 }),
        ];

        for ((selected, total, max_visible), expected) in cases {
            assert_eq!(
                centered_visible_range(selected, total, max_visible),
                expected
            );
        }
    }

    #[test]
    fn moves_selection_with_wrapping() {
        let mut state = SelectMenuState::new();

        state.move_up(3);
        assert_eq!(state.selected, 2);
        state.move_down(3);
        assert_eq!(state.selected, 0);
        state.move_down(3);
        assert_eq!(state.selected, 1);
    }

    #[test]
    fn windows_around_selected_item() {
        let items = (0..6)
            .map(|index| SelectItem::new(format!("item-{index}"), ""))
            .collect::<Vec<_>>();
        let menu = SelectMenu {
            title: None,
            items,
            selected: 4,
            max_visible: 3,
        };

        let lines = menu.render(&RenderContext::new(
            crate::RenderArea::width(30),
            &crate::Theme::default(),
        ));

        assert!(lines[0].text.contains("item-3"));
        assert!(lines[1].text.contains("item-4"));
        assert!(lines[2].text.contains("item-5"));
        assert_eq!(lines[3].text.trim(), "(5/6)");
    }
}
