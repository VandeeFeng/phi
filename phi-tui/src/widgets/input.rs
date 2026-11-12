use crate::text::pad;
use crate::{RenderContext, StyledLine, ThemeColor, Widget};

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    pub label: Option<String>,
}

impl Widget for TextInput {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width;
        let prefix = self
            .label
            .as_deref()
            .filter(|label| !label.is_empty())
            .map(|label| format!("{label} "))
            .unwrap_or_default();
        let available = width.saturating_sub(prefix.chars().count() as u16) as usize;
        vec![
            StyledLine::segments([
                (prefix, ThemeColor::Muted),
                (pad(&self.text, available), ThemeColor::Default),
            ])
            .fit(width),
        ]
    }
}
