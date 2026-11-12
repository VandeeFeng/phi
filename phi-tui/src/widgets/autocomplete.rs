use crate::text::pop_grapheme;
use crate::{
    InputOutcome, KeyInput, RenderContext, SelectItem, SelectMenu, SelectMenuState, StyledLine,
    Widget,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutocompleteAction {
    Cancelled,
    Selected(String),
    QueryChanged(String),
}

#[derive(Debug, Clone)]
pub struct Autocomplete {
    pub query: String,
    pub items: Vec<SelectItem>,
    pub menu: SelectMenuState,
    pub title: Option<String>,
    pub max_visible: usize,
}

impl Autocomplete {
    pub fn new(items: Vec<SelectItem>) -> Self {
        Self {
            query: String::new(),
            items,
            menu: SelectMenuState::new(),
            title: None,
            max_visible: 5,
        }
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.menu.selected_item(&self.items)
    }

    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.menu.clamp(self.items.len());
    }
}

impl Widget for Autocomplete {
    type Action = AutocompleteAction;

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let menu = SelectMenu {
            title: self.title.clone(),
            items: self.items.clone(),
            selected: self.menu.selected,
            max_visible: self.max_visible,
        };
        menu.render(context)
    }

    fn handle_input(&mut self, key: &KeyInput) -> InputOutcome<Self::Action> {
        match key {
            KeyInput::Up => self.menu.move_up(self.items.len()),
            KeyInput::Down => self.menu.move_down(self.items.len()),
            KeyInput::Tab | KeyInput::Enter => {
                let Some(item) = self.selected_item() else {
                    return InputOutcome::Consumed;
                };
                return InputOutcome::Action(AutocompleteAction::Selected(item.value.clone()));
            }
            KeyInput::Esc | KeyInput::Ctrl('c') => {
                return InputOutcome::Action(AutocompleteAction::Cancelled);
            }
            KeyInput::Backspace => {
                pop_grapheme(&mut self.query);
                return InputOutcome::Action(AutocompleteAction::QueryChanged(self.query.clone()));
            }
            KeyInput::Char(c) => {
                self.query.push(*c);
                return InputOutcome::Action(AutocompleteAction::QueryChanged(self.query.clone()));
            }
            KeyInput::Paste(text) => {
                self.query.push_str(text.trim());
                return InputOutcome::Action(AutocompleteAction::QueryChanged(self.query.clone()));
            }
            _ => return InputOutcome::Ignored,
        }
        InputOutcome::Consumed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_query_changed() {
        let mut autocomplete = Autocomplete::new(Vec::new());

        assert_eq!(
            autocomplete.handle_input(&KeyInput::Char('a')),
            InputOutcome::Action(AutocompleteAction::QueryChanged(String::from("a")))
        );
    }

    #[test]
    fn selects_active_item() {
        let mut autocomplete =
            Autocomplete::new(vec![SelectItem::new("one", ""), SelectItem::new("two", "")]);
        autocomplete.handle_input(&KeyInput::Down);

        assert_eq!(
            autocomplete.handle_input(&KeyInput::Enter),
            InputOutcome::Action(AutocompleteAction::Selected(String::from("two")))
        );
    }
}
