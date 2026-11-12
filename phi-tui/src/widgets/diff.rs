use crate::{StyledLine, ThemeColor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Equal,
    Insert,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Change {
        kind: DiffLineKind,
        old_line: Option<usize>,
        new_line: Option<usize>,
        text: String,
    },
    Omitted,
    Prefixed {
        color: ThemeColor,
        text: String,
    },
}

impl DiffLine {
    pub fn change(
        kind: DiffLineKind,
        old_line: Option<usize>,
        new_line: Option<usize>,
        text: impl Into<String>,
    ) -> Self {
        Self::Change {
            kind,
            old_line,
            new_line,
            text: text.into(),
        }
    }

    pub fn omitted() -> Self {
        Self::Omitted
    }

    fn prefixed(color: ThemeColor, text: impl Into<String>) -> Self {
        Self::Prefixed {
            color,
            text: text.into(),
        }
    }

    fn line_numbers(&self) -> [Option<usize>; 2] {
        match self {
            Self::Change {
                old_line, new_line, ..
            } => [*old_line, *new_line],
            Self::Omitted | Self::Prefixed { .. } => [None, None],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffView {
    pub lines: Vec<DiffLine>,
}

impl DiffView {
    pub fn from_prefixed_text(text: &str) -> Self {
        let lines = text
            .lines()
            .map(|line| {
                let color = match line.as_bytes().first() {
                    Some(b'+') => ThemeColor::Success,
                    Some(b'-') => ThemeColor::Error,
                    _ => ThemeColor::Dim,
                };
                DiffLine::prefixed(color, line)
            })
            .collect();
        Self { lines }
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

pub fn render_diff(diff: &DiffView, width: u16) -> Vec<StyledLine> {
    render_diff_tail(diff, width, diff.lines.len()).0
}

pub(crate) fn render_diff_tail(
    diff: &DiffView,
    width: u16,
    max_rows: usize,
) -> (Vec<StyledLine>, usize) {
    let line_width = diff
        .lines
        .iter()
        .flat_map(DiffLine::line_numbers)
        .flatten()
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let start = diff.lines.len().saturating_sub(max_rows);
    let lines = diff.lines[start..]
        .iter()
        .map(|line| render_diff_line(line, line_width, width))
        .collect();
    (lines, diff.lines.len())
}

fn render_diff_line(line: &DiffLine, line_width: usize, width: u16) -> StyledLine {
    let (text, color) = match line {
        DiffLine::Change {
            kind,
            old_line,
            new_line,
            text,
        } => (
            format_change(*kind, *old_line, *new_line, text, line_width),
            line_color(*kind),
        ),
        DiffLine::Omitted => (
            format!(" {:>width$} ...", "", width = line_width),
            ThemeColor::Dim,
        ),
        DiffLine::Prefixed { color, text } => (text.clone(), *color),
    };
    StyledLine::new(expand_tabs(&text), color).fit(width)
}

fn format_change(
    kind: DiffLineKind,
    old_line: Option<usize>,
    new_line: Option<usize>,
    text: &str,
    line_width: usize,
) -> String {
    let (prefix, number) = match kind {
        DiffLineKind::Equal => (' ', old_line.or(new_line)),
        DiffLineKind::Insert => ('+', new_line),
        DiffLineKind::Delete => ('-', old_line),
    };
    format!(
        "{prefix}{:>width$} {text}",
        number.unwrap_or_default(),
        width = line_width
    )
}

fn line_color(kind: DiffLineKind) -> ThemeColor {
    match kind {
        DiffLineKind::Insert => ThemeColor::Success,
        DiffLineKind::Delete => ThemeColor::Error,
        DiffLineKind::Equal => ThemeColor::Dim,
    }
}

fn expand_tabs(text: &str) -> String {
    text.replace('\t', "   ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_semantic_and_prefixed_diff_lines() {
        let semantic = DiffView {
            lines: vec![
                DiffLine::change(DiffLineKind::Delete, Some(2), None, "b"),
                DiffLine::change(DiffLineKind::Insert, None, Some(2), "B"),
            ],
        };
        let semantic_lines = render_diff(&semantic, 80);
        assert_eq!(semantic_lines[0].text.trim_end(), "-2 b");
        assert_eq!(semantic_lines[1].text.trim_end(), "+2 B");

        let prefixed = DiffView::from_prefixed_text("-2 old\n+2 new");
        let prefixed_lines = render_diff(&prefixed, 80);
        assert_eq!(prefixed_lines[0].text.trim_end(), "-2 old");
        assert_eq!(prefixed_lines[1].text.trim_end(), "+2 new");
    }
}
