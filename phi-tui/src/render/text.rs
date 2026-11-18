use std::path::{Path, PathBuf};

use textwrap::{WordSeparator, core::Fragment, wrap_algorithms::wrap_first_fit};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn fit(text: &str, width: usize) -> String {
    let mut out = truncate(text, width);
    pad_in_place(&mut out, width);
    out
}

pub fn pad(text: &str, width: usize) -> String {
    let mut out = truncate(text, width);
    pad_in_place(&mut out, width);
    out
}

pub fn truncate(text: &str, width: usize) -> String {
    let mut end = 0;
    let mut used = 0;
    for grapheme in text.graphemes(true) {
        let grapheme_width = display_width(grapheme);
        if used + grapheme_width > width {
            break;
        }
        used += grapheme_width;
        end += grapheme.len();
    }
    text[..end].to_string()
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    wrap_bounded(text, width, usize::MAX, RetainedRows::Tail).0
}

pub(crate) fn wrap_head(text: &str, width: usize, max_rows: usize) -> (Vec<String>, usize) {
    wrap_bounded(text, width, max_rows, RetainedRows::Head)
}

pub(crate) fn wrap_tail(text: &str, width: usize, max_rows: usize) -> (Vec<String>, usize) {
    wrap_bounded(text, width, max_rows, RetainedRows::Tail)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainedRows {
    Head,
    Tail,
}

fn wrap_bounded(
    text: &str,
    width: usize,
    max_rows: usize,
    retained_rows: RetainedRows,
) -> (Vec<String>, usize) {
    let width = width.max(1);
    let mut retained = std::collections::VecDeque::new();
    let mut total = 0;
    for source in text.split('\n') {
        let source = source.strip_suffix('\r').unwrap_or(source);
        for line in wrap_line(source, width) {
            total += 1;
            if retained.len() < max_rows {
                retained.push_back(line.to_owned());
            } else if retained_rows == RetainedRows::Tail && max_rows > 0 {
                retained.pop_front();
                retained.push_back(line.to_owned());
            }
        }
    }
    (retained.into_iter().collect(), total)
}

#[derive(Debug)]
struct WrapFragment<'a> {
    text: &'a str,
    whitespace: &'a str,
}

impl Fragment for WrapFragment<'_> {
    fn width(&self) -> f64 {
        display_width(self.text) as f64
    }

    fn whitespace_width(&self) -> f64 {
        display_width(self.whitespace) as f64
    }

    fn penalty_width(&self) -> f64 {
        0.0
    }
}

fn wrap_line(source: &str, width: usize) -> Vec<&str> {
    if display_width(source) <= width {
        return vec![source.trim_end_matches(' ')];
    }
    let mut fragments = Vec::new();
    for word in WordSeparator::new().find_words(source) {
        let mut start = 0;
        let mut used = 0;
        // textwrap's forced word breaks count code points rather than graphemes.
        for (index, grapheme) in word.word.grapheme_indices(true) {
            let grapheme_width = display_width(grapheme);
            if used > 0 && used + grapheme_width > width {
                fragments.push(WrapFragment {
                    text: &word.word[start..index],
                    whitespace: "",
                });
                start = index;
                used = 0;
            }
            used += grapheme_width;
        }
        fragments.push(WrapFragment {
            text: &word.word[start..],
            whitespace: word.whitespace,
        });
    }
    let mut offset = 0;
    wrap_first_fit(&fragments, &[width as f64])
        .into_iter()
        .map(|line| {
            let length = line
                .iter()
                .map(|fragment| fragment.text.len() + fragment.whitespace.len())
                .sum::<usize>();
            let trailing = line.last().map_or(0, |fragment| fragment.whitespace.len());
            let text = &source[offset..offset + length - trailing];
            offset += length;
            text
        })
        .collect()
}

pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn previous_grapheme_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

pub fn next_grapheme_boundary(text: &str, cursor: usize) -> usize {
    cursor
        + text[cursor..]
            .graphemes(true)
            .next()
            .map(str::len)
            .unwrap_or(0)
}

pub fn pop_grapheme(text: &mut String) {
    let index = previous_grapheme_boundary(text, text.len());
    text.truncate(index);
}

/// Compacts a standalone display path relative to the cwd or HOME.
pub fn compact_path(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    if !path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    compact_absolute_path(
        path,
        std::env::current_dir().ok().as_deref(),
        home_dir().as_deref(),
    )
}

/// Replaces the home prefix of one display path with `~`.
pub fn home_to_tilde(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    home_dir()
        .as_deref()
        .and_then(|home| compact_path_from(path, home, "~", "~/"))
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Abbreviates HOME paths embedded in display text such as Bash commands.
pub fn abbreviate_home_paths(text: &str) -> String {
    let Some(home) = home_dir() else {
        return text.to_string();
    };
    abbreviate_home_paths_from(text, &home.to_string_lossy())
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn abbreviate_home_paths_from(text: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return text.to_string();
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for (index, matched) in text.match_indices(home) {
        let end = index + matched.len();
        let suffix = &text[end..];
        output.push_str(&text[cursor..index]);
        let boundary = suffix.chars().next().is_none_or(|character| {
            character.is_whitespace() || "/\\'\"})],;&|".contains(character)
        });
        if boundary {
            output.push('~');
        } else {
            output.push_str(matched);
        }
        cursor = end;
    }
    output.push_str(&text[cursor..]);
    output
}

fn compact_absolute_path(path: &Path, cwd: Option<&Path>, home: Option<&Path>) -> String {
    cwd.and_then(|cwd| compact_path_from(path, cwd, ".", ""))
        .or_else(|| home.and_then(|home| compact_path_from(path, home, "~", "~/")))
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn compact_path_from(path: &Path, base: &Path, base_label: &str, prefix: &str) -> Option<String> {
    let relative = path.strip_prefix(base).ok()?;
    Some(if relative.as_os_str().is_empty() {
        base_label.to_string()
    } else {
        format!("{prefix}{}", relative.to_string_lossy())
    })
}

fn pad_in_place(text: &mut String, width: usize) {
    let len = display_width(text);
    if len < width {
        text.push_str(&" ".repeat(width - len));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_preserves_graphemes_and_row_budgets() {
        for unit in ["", "abc", "中文", "e\u{301}", "👩‍💻", "🇨🇳", "👨‍👩‍👧‍👦"]
        {
            for repetitions in 1..=4 {
                let text = unit.repeat(repetitions);
                let graphemes = text.graphemes(true).collect::<Vec<_>>();
                let widest = graphemes
                    .iter()
                    .map(|text| display_width(text))
                    .max()
                    .unwrap_or(0);
                for width in 0..=8 {
                    let lines = wrap(&text, width);
                    assert_eq!(lines.concat(), text);
                    assert_eq!(
                        lines
                            .iter()
                            .flat_map(|line| line.graphemes(true))
                            .collect::<Vec<_>>(),
                        graphemes,
                        "{text:?}, width {width}",
                    );
                    assert!(
                        lines
                            .iter()
                            .all(|line| display_width(line) <= width.max(widest))
                    );
                    for budget in [0, 1, 2, usize::MAX] {
                        assert_eq!(
                            wrap_tail(&text, width, budget),
                            (
                                lines[lines.len().saturating_sub(budget)..].to_vec(),
                                lines.len()
                            )
                        );
                        assert_eq!(
                            wrap_head(&text, width, budget),
                            (lines[..lines.len().min(budget)].to_vec(), lines.len())
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn crlf_wrapping_matches_lf_with_blank_and_trailing_lines() {
        let lf = "first paragraph wraps here\n\n中文文档\n";
        for text in [lf.replace('\n', "\r\n"), lf.replacen('\n', "\r\n", 1)] {
            for width in [1, 8, 20] {
                let expected = wrap(lf, width);
                assert_eq!(wrap(&text, width), expected);
                for budget in [0, 1, 2, usize::MAX] {
                    assert_eq!(
                        wrap_tail(&text, width, budget),
                        wrap_tail(lf, width, budget)
                    );
                    assert_eq!(
                        wrap_head(&text, width, budget),
                        wrap_head(lf, width, budget)
                    );
                }
            }
        }
    }

    #[test]
    fn compacts_cwd_and_home_paths() {
        let cwd = Path::new("/home/test/project");
        let home = Path::new("/home/test");
        assert_eq!(
            compact_absolute_path(
                Path::new("/home/test/project/src/main.rs"),
                Some(cwd),
                Some(home)
            ),
            "src/main.rs"
        );
        assert_eq!(compact_absolute_path(cwd, Some(cwd), Some(home)), ".");
        assert_eq!(
            compact_absolute_path(Path::new("/home/test/notes.md"), Some(cwd), Some(home)),
            "~/notes.md"
        );
        assert_eq!(compact_absolute_path(home, Some(cwd), Some(home)), "~");
        assert_eq!(
            compact_path_from(Path::new("/home/test/notes.md"), home, "~", "~/"),
            Some(String::from("~/notes.md"))
        );
        assert_eq!(
            compact_path_from(Path::new("/home/tester"), home, "~", "~/"),
            None
        );
    }
}
