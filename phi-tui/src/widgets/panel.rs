use crate::text::{display_width, pad, truncate};
use crate::{RenderContext, StyledLine, StyledSegment, ThemeColor, Widget};

#[derive(Debug, Clone)]
pub struct Panel {
    pub title: String,
    pub lines: Vec<StyledLine>,
    pub max_height: usize,
    pub border_color: ThemeColor,
}

impl Widget for Panel {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width;
        let width = width.max(8);
        let inner_width = width.saturating_sub(4) as usize;
        let mut rendered = Vec::new();
        rendered.push(box_top(&self.title, width, self.border_color));
        for line in self.lines.iter().take(self.max_height.saturating_sub(2)) {
            rendered.push(box_body(line, inner_width, self.border_color));
        }
        rendered.push(box_bottom(width, self.border_color));
        rendered
    }
}

fn box_top(title: &str, width: u16, color: ThemeColor) -> StyledLine {
    let inner = width.saturating_sub(2) as usize;
    let title = if title.trim().is_empty() {
        String::new()
    } else {
        format!(" {} ", title.trim())
    };
    let fill = "─".repeat(inner.saturating_sub(display_width(&title)));
    StyledLine::segments([
        (String::from("╭"), color),
        (title, ThemeColor::Title),
        (fill, color),
        (String::from("╮"), color),
    ])
}

fn box_body(line: &StyledLine, inner_width: usize, color: ThemeColor) -> StyledLine {
    let fitted = line.clone().fit(inner_width as u16);
    let mut segments = vec![StyledSegment::new("│ ", color)];

    if fitted.segments.is_empty() {
        segments.push(
            StyledSegment::new(
                pad(&truncate(&fitted.text, inner_width), inner_width),
                fitted.fg,
            )
            .with_bold(fitted.bold),
        );
    } else {
        segments.extend(fitted.segments);
    }

    segments.push(StyledSegment::new(" │", color));

    StyledLine::rich_segments(segments)
}

fn box_bottom(width: u16, color: ThemeColor) -> StyledLine {
    let inner = width.saturating_sub(2) as usize;
    StyledLine::segments([
        (String::from("╰"), color),
        ("─".repeat(inner), color),
        (String::from("╯"), color),
    ])
}
