use crate::text::wrap;
use crate::{RenderContext, StyledLine, ThemeColor, Widget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyMessage {
    pub kind: NotifyKind,
    pub message: String,
}

impl NotifyMessage {
    pub fn new(kind: NotifyKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Notification {
    pub message: Option<NotifyMessage>,
}

impl Widget for Notification {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width;
        let Some(message) = &self.message else {
            return Vec::new();
        };
        let text = message.message.trim();
        if text.is_empty() {
            return Vec::new();
        }

        let (prefix, color) = match message.kind {
            NotifyKind::Info => ("", ThemeColor::Dim),
            NotifyKind::Success => ("", ThemeColor::Success),
            NotifyKind::Warning => ("Warning: ", ThemeColor::Warning),
            NotifyKind::Error => ("Error: ", ThemeColor::Error),
        };
        let body_width = (width as usize).saturating_sub(prefix.len()).max(8);
        let mut lines = vec![StyledLine::new(String::new(), ThemeColor::Default).fit(width)];

        for (index, line) in wrap(text, body_width).into_iter().enumerate() {
            let rendered = if index == 0 {
                format!("{prefix}{line}")
            } else {
                format!("{}{}", " ".repeat(prefix.len()), line)
            };
            lines.push(StyledLine::new(rendered, color).fit(width));
        }

        lines
    }
}
