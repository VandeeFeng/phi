use crate::render::text::wrap_tail;
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
    render_diff_tail(diff, width, usize::MAX).0
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
    let mut retained = Vec::new();
    let mut total_rows = 0;
    for line in diff.lines.iter().rev() {
        let (text, color) = format_diff_line(line, line_width);
        let remaining_rows = max_rows.saturating_sub(retained.len());
        let (lines, rows) = wrap_tail(&expand_tabs(&text), width as usize, remaining_rows);
        total_rows += rows;
        retained.extend(
            lines
                .into_iter()
                .rev()
                .map(|text| StyledLine::new(text, color).fit(width)),
        );
    }
    retained.reverse();
    (retained, total_rows)
}

fn format_diff_line(line: &DiffLine, line_width: usize) -> (String, ThemeColor) {
    match line {
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
    }
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
    fn wraps_long_diff_lines_without_losing_content() {
        for text in [
            "document-content-".repeat(20),
            "文档内容完整显示".repeat(20),
            "👩‍💻🇨🇳e\u{301}".repeat(20),
        ] {
            let diff = DiffView {
                lines: vec![
                    DiffLine::change(DiffLineKind::Delete, Some(12), None, "old"),
                    DiffLine::change(DiffLineKind::Insert, None, Some(12), &text),
                ],
            };
            for width in [8, 20, 80] {
                let lines = render_diff(&diff, width);
                let actual = lines
                    .iter()
                    .map(|line| line.text.trim_end())
                    .collect::<String>();
                assert_eq!(actual, format!("-12 old+12 {text}"));
                assert!(
                    lines.iter().all(
                        |line| crate::render::text::display_width(&line.text) <= width as usize
                    )
                );
                for budget in [0, 1, 2, usize::MAX] {
                    let (tail, total) = render_diff_tail(&diff, width, budget);
                    assert_eq!(total, lines.len());
                    assert_eq!(tail, lines[lines.len().saturating_sub(budget)..]);
                }
            }
        }
    }

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
