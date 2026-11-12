use std::path::{Path, PathBuf};

use textwrap::{Options, WordSplitter, wrap as textwrap_wrap};
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
    if text.is_empty() {
        return ((max_rows > 0).then(String::new).into_iter().collect(), 1);
    }
    let options = Options::new(width.max(1))
        .break_words(true)
        .word_splitter(WordSplitter::NoHyphenation);
    let mut retained = std::collections::VecDeque::new();
    let mut total = 0;
    for source in text.split('\n') {
        let wrapped = if source.is_empty() {
            vec![std::borrow::Cow::Borrowed("")]
        } else {
            textwrap_wrap(source, &options)
        };
        for line in wrapped {
            total += 1;
            if retained.len() < max_rows {
                retained.push_back(line.into_owned());
            } else if retained_rows == RetainedRows::Tail && max_rows > 0 {
                retained.pop_front();
                retained.push_back(line.into_owned());
            }
        }
    }
    (retained.into_iter().collect(), total)
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
