use crate::text::{pad, pop_grapheme};
use crate::{
    CursorPosition, InputOutcome, KeyInput, RenderArea, RenderContext, StyledLine, ThemeColor,
    Widget,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChoiceAction {
    Selected(usize),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct Choice {
    options: Vec<String>,
    menu: crate::SelectMenuState,
}

impl Choice {
    pub fn new(options: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            options: options.into_iter().map(Into::into).collect(),
            menu: crate::SelectMenuState::default(),
        }
    }
}

impl Widget for Choice {
    type Action = ChoiceAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        if self.options.is_empty() {
            return Vec::new();
        }
        let segments = self.options.iter().enumerate().map(|(index, option)| {
            if index == self.menu.selected {
                (format!(" [ {option} ]   "), ThemeColor::Text)
            } else {
                (format!("   {option}     "), ThemeColor::Dim)
            }
        });
        vec![StyledLine::segments(segments).fit(context.area.width)]
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        match key {
            KeyInput::Left => self.menu.move_up(self.options.len()),
            KeyInput::Right | KeyInput::Tab => self.menu.move_down(self.options.len()),
            KeyInput::Enter if !self.options.is_empty() => {
                return InputOutcome::Action(ChoiceAction::Selected(self.menu.selected));
            }
            KeyInput::Enter => {}
            KeyInput::Esc | KeyInput::Ctrl('c') => {
                return InputOutcome::Action(ChoiceAction::Cancelled);
            }
            _ => return InputOutcome::Ignored,
        }
        InputOutcome::Consumed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmAction {
    Confirmed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct ConfirmDialog {
    pub title: String,
    pub message: String,
    pub confirm_label: String,
    pub cancel_label: String,
    selected_confirm: bool,
}

impl ConfirmDialog {
    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            confirm_label: String::from("yes"),
            cancel_label: String::from("no"),
            selected_confirm: false,
        }
    }
}

impl Widget for ConfirmDialog {
    type Action = ConfirmAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width.max(24);
        let confirm_color = if self.selected_confirm {
            ThemeColor::Accent
        } else {
            ThemeColor::Muted
        };
        let cancel_color = if self.selected_confirm {
            ThemeColor::Muted
        } else {
            ThemeColor::Accent
        };

        vec![
            StyledLine::new(format!("  {}", self.title), ThemeColor::Title).fit(width),
            StyledLine::new(format!("  {}", self.message), ThemeColor::Default).fit(width),
            StyledLine::segments([
                (String::from("  "), ThemeColor::Default),
                (format!("[{}]", self.confirm_label), confirm_color),
                (String::from("  "), ThemeColor::Default),
                (format!("[{}]", self.cancel_label), cancel_color),
            ])
            .fit(width),
        ]
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        match key {
            KeyInput::Left | KeyInput::Right | KeyInput::Tab => {
                self.selected_confirm = !self.selected_confirm;
                InputOutcome::Consumed
            }
            KeyInput::Char('y') => InputOutcome::Action(ConfirmAction::Confirmed),
            KeyInput::Char('n') | KeyInput::Esc | KeyInput::Ctrl('c') => {
                InputOutcome::Action(ConfirmAction::Cancelled)
            }
            KeyInput::Enter if self.selected_confirm => {
                InputOutcome::Action(ConfirmAction::Confirmed)
            }
            KeyInput::Enter => InputOutcome::Action(ConfirmAction::Cancelled),
            _ => InputOutcome::Ignored,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextInputAction {
    Submitted(String),
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct TextInputDialog {
    pub title: String,
    pub label: String,
    pub value: String,
}

impl TextInputDialog {
    pub fn new(title: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            label: label.into(),
            value: String::new(),
        }
    }
}

impl Widget for TextInputDialog {
    type Action = TextInputAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width.max(24);
        let prefix = format!("{} ", self.label);
        let available = width.saturating_sub(prefix.chars().count() as u16 + 2) as usize;
        vec![
            StyledLine::new(format!("  {}", self.title), ThemeColor::Title).fit(width),
            StyledLine::segments([
                (String::from("  "), ThemeColor::Default),
                (prefix, ThemeColor::Muted),
                (pad(&self.value, available), ThemeColor::Default),
            ])
            .fit(width),
        ]
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        match key {
            KeyInput::Esc | KeyInput::Ctrl('c') => InputOutcome::Action(TextInputAction::Cancelled),
            KeyInput::Enter => InputOutcome::Action(TextInputAction::Submitted(self.value.clone())),
            KeyInput::Backspace => {
                pop_grapheme(&mut self.value);
                InputOutcome::Consumed
            }
            KeyInput::Char(c) => {
                self.value.push(*c);
                InputOutcome::Consumed
            }
            KeyInput::Paste(text) => {
                self.value.push_str(text.trim());
                InputOutcome::Consumed
            }
            _ => InputOutcome::Ignored,
        }
    }

    fn cursor(&self, _area: RenderArea) -> Option<CursorPosition> {
        Some(CursorPosition {
            row: 1,
            col: 3 + self.label.chars().count() as u16 + self.value.chars().count() as u16,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_wrap_select_cancel_and_respect_geometry() {
        let mut choices = Choice::new(["once", "always", "deny"]);
        assert_eq!(
            choices.handle_input(&KeyInput::Left),
            InputOutcome::Consumed
        );
        assert_eq!(
            choices.handle_input(&KeyInput::Enter),
            InputOutcome::Action(ChoiceAction::Selected(2))
        );
        choices.handle_input(&KeyInput::Tab);
        assert_eq!(
            choices.handle_input(&KeyInput::Enter),
            InputOutcome::Action(ChoiceAction::Selected(0))
        );
        choices.handle_input(&KeyInput::Right);
        assert_eq!(
            choices.handle_input(&KeyInput::Enter),
            InputOutcome::Action(ChoiceAction::Selected(1))
        );
        for key in [KeyInput::Esc, KeyInput::Ctrl('c')] {
            assert_eq!(
                choices.handle_input(&key),
                InputOutcome::Action(ChoiceAction::Cancelled)
            );
        }
        assert_eq!(
            choices.handle_input(&KeyInput::Ctrl('y')),
            InputOutcome::Ignored
        );
        let theme = crate::Theme::default();
        for width in [0, 1, 12, 80] {
            let lines = choices.render(&RenderContext::width(width, &theme));
            assert!(
                lines
                    .iter()
                    .all(|line| crate::text::display_width(&line.text) <= width as usize)
            );
        }
        let mut empty = Choice::new(std::iter::empty::<String>());
        assert_eq!(empty.handle_input(&KeyInput::Left), InputOutcome::Consumed);
        assert_eq!(empty.handle_input(&KeyInput::Enter), InputOutcome::Consumed);
        assert!(empty.render(&RenderContext::width(80, &theme)).is_empty());
    }

    #[test]
    fn confirm_dialog_emits_actions() {
        let mut dialog = ConfirmDialog::new("delete?", "cannot undo");

        assert_eq!(
            dialog.handle_input(&KeyInput::Char('y')),
            InputOutcome::Action(ConfirmAction::Confirmed)
        );
        assert_eq!(
            dialog.handle_input(&KeyInput::Esc),
            InputOutcome::Action(ConfirmAction::Cancelled)
        );
    }

    #[test]
    fn text_input_edits_and_submits() {
        let mut dialog = TextInputDialog::new("name", "value");

        dialog.handle_input(&KeyInput::Char('a'));
        dialog.handle_input(&KeyInput::Char('b'));
        dialog.handle_input(&KeyInput::Backspace);

        assert_eq!(dialog.value, "a");
        assert_eq!(
            dialog.handle_input(&KeyInput::Enter),
            InputOutcome::Action(TextInputAction::Submitted(String::from("a")))
        );
    }
}
