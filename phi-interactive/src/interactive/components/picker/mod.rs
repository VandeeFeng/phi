pub(in crate::interactive) mod model;
pub(in crate::interactive) mod session;
pub(in crate::interactive) mod switch;

// Generated model defaults must remain visible to Switch editing.
pub(in crate::interactive) type PickerDraft =
    std::rc::Rc<std::cell::RefCell<crate::config::SwitchSettings>>;

use phi_tui::{
    Cursor, RenderContext, SelectMenu, StyledLine, TextInput, Theme, ThemeColor, Widget,
};

use crate::interactive::state::{PickerKind, UiState};

pub(super) fn render_menu(
    state: &UiState,
    width: u16,
    theme: &Theme,
) -> (Vec<StyledLine>, Option<Cursor>) {
    if state.naming_switch {
        let mut lines = render_input_lines(state.editor.text(), width, theme);
        lines.push(StyledLine::new("  input switch name", ThemeColor::Dim).fit(width));
        let cursor = Cursor {
            row: 0,
            col: ("> ".len()
                + phi_tui::text::display_width(&state.editor.text()[..state.editor.cursor()]))
                as u16,
        };
        return (lines, Some(cursor));
    }

    let Some(picker) = &state.picker else {
        return (Vec::new(), None);
    };
    let mut lines = Vec::new();
    let cursor = if picker.filterable {
        lines.extend(render_input_lines(&picker.query, width, theme));
        Some(Cursor {
            row: 0,
            col: ("> ".len() + phi_tui::text::display_width(&picker.query)) as u16,
        })
    } else {
        None
    };
    if matches!(picker.kind, PickerKind::Rewind) {
        lines.extend(session::render_rewind_items(picker, width));
    } else if picker.filterable {
        lines.extend(model::render_items(picker, width));
    } else {
        let max_visible = if matches!(picker.kind, PickerKind::Resume) {
            session::RESUME_PICKER_MAX_VISIBLE
        } else {
            5
        };
        let widget = SelectMenu {
            title: Some(picker.title.clone()),
            items: picker.items.clone(),
            selected: picker.menu.selected,
            max_visible,
        };
        lines.extend(widget.render(&RenderContext::width(width, theme)));
    }
    if matches!(picker.kind, PickerKind::Model)
        && let Some(notice) = &state.model_catalog_notice
    {
        let color = match notice.kind {
            phi_tui::NotifyKind::Success => ThemeColor::Success,
            phi_tui::NotifyKind::Warning => ThemeColor::Warning,
            phi_tui::NotifyKind::Error => ThemeColor::Error,
            phi_tui::NotifyKind::Info => ThemeColor::Dim,
        };
        lines.push(StyledLine::new("", ThemeColor::Default));
        lines.push(StyledLine::new(format!("  {}", notice.message), color).fit(width));
    }
    (lines, cursor)
}

fn render_input_lines(text: &str, width: u16, theme: &Theme) -> Vec<StyledLine> {
    TextInput {
        text: text.to_string(),
        label: Some(String::from(">")),
    }
    .render(&RenderContext::width(width, theme))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::state::{ModelCatalogNotice, PickerState};
    use phi_tui::{NotifyKind, SelectItem, text::display_width};

    #[test]
    fn standard_menus_preserve_title_selection_and_visible_limit() {
        for (kind, visible_limit) in [
            (PickerKind::Generic, 5),
            (PickerKind::Login, 5),
            (PickerKind::SwitchSelect, 5),
            (PickerKind::Resume, session::RESUME_PICKER_MAX_VISIBLE),
        ] {
            let items = (0..12)
                .map(|index| SelectItem::new(format!("item-{index}"), "description"))
                .collect();
            let mut picker = PickerState::with_kind(kind, "picker title", items);
            picker.menu.selected = 6;
            let state = UiState {
                picker: Some(picker),
                ..Default::default()
            };
            let (lines, cursor) = render_menu(&state, 80, &Theme::default());
            assert!(cursor.is_none());
            assert_eq!(lines.len(), visible_limit + 2);
            assert_eq!(lines[0].text.trim(), "picker title");
            assert!(lines.iter().any(|line| line.text.starts_with("→ item-6")));
            assert_eq!(lines.last().unwrap().text.trim(), "(7/12)");
        }
    }

    #[test]
    fn menu_preserves_search_cursor_notice_and_naming_priority() {
        let mut state = UiState::default();
        state.picker = Some(PickerState {
            filterable: true,
            query: "模型".into(),
            ..PickerState::with_kind(
                PickerKind::Model,
                "select model",
                vec![SelectItem::new("model", "[provider] ✔")],
            )
        });
        state.model_catalog_notice = Some(ModelCatalogNotice {
            kind: NotifyKind::Warning,
            message: "catalog offline".into(),
        });
        for width in 1..=80 {
            let (lines, cursor) = render_menu(&state, width, &Theme::default());
            let cursor = cursor.unwrap();
            assert_eq!(cursor.row, 0);
            assert_eq!(usize::from(cursor.col), display_width("> 模型"));
            assert!(
                lines
                    .iter()
                    .all(|line| display_width(&line.text) <= usize::from(width))
            );
            if width == 80 {
                assert!(lines.iter().any(|line| {
                    line.text.contains("catalog offline") && line.fg == ThemeColor::Warning
                }));
            }
        }

        state.naming_switch = true;
        state.editor.set_text("审查");
        let (lines, cursor) = render_menu(&state, 80, &Theme::default());
        assert_eq!(usize::from(cursor.unwrap().col), display_width("> 审查"));
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("input switch name"))
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.text.contains("catalog offline"))
        );

        state.naming_switch = false;
        state.picker = Some(PickerState {
            filterable: true,
            ..PickerState::with_kind(PickerKind::Rewind, "rewind", Vec::new())
        });
        let (lines, cursor) = render_menu(&state, 80, &Theme::default());
        assert_eq!(cursor.unwrap().row, 0);
        assert!(
            lines
                .iter()
                .any(|line| line.text.contains("no matching rewind points"))
        );
    }
}
