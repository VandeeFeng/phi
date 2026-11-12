use similar::{ChangeTag, TextDiff};

use crate::core::event::{ToolDiffChange, ToolDiffChangeKind, ToolDiffDetails, ToolDiffHunk};

const CONTEXT_LINES: usize = 4;

pub(super) fn file_diff(path: &str, old: &str, new: &str) -> ToolDiffDetails {
    let text_diff = TextDiff::from_lines(old, new);
    let raw_changes = text_diff.iter_all_changes().collect::<Vec<_>>();
    let first_changed_line = raw_changes
        .iter()
        .find(|change| change.tag() != ChangeTag::Equal)
        .and_then(|change| change.new_index().or_else(|| change.old_index()))
        .map(|index| index + 1);
    let hunks = compact_hunks(&raw_changes);
    let patch = text_diff
        .unified_diff()
        .header(path, path)
        .context_radius(CONTEXT_LINES)
        .to_string();

    ToolDiffDetails {
        hunks,
        patch: Some(patch),
        first_changed_line,
    }
}

fn compact_hunks(changes: &[similar::Change<&str>]) -> Vec<ToolDiffHunk> {
    let mut hunks = Vec::new();
    let mut current = Vec::new();
    for (index, change) in changes.iter().enumerate() {
        if change.tag() == ChangeTag::Equal && !near_change(changes, index) {
            if !current.is_empty() {
                hunks.push(ToolDiffHunk {
                    changes: std::mem::take(&mut current),
                });
            }
            continue;
        }
        current.push(ToolDiffChange {
            kind: map_kind(change.tag()),
            old_line: change.old_index().map(|index| index + 1),
            new_line: change.new_index().map(|index| index + 1),
            text: change.to_string().trim_end_matches('\n').to_string(),
        });
    }
    if !current.is_empty() {
        hunks.push(ToolDiffHunk { changes: current });
    }
    hunks
}

fn near_change(changes: &[similar::Change<&str>], index: usize) -> bool {
    let start = index.saturating_sub(CONTEXT_LINES);
    let end = (index + CONTEXT_LINES + 1).min(changes.len());
    changes[start..end]
        .iter()
        .any(|change| change.tag() != ChangeTag::Equal)
}

fn map_kind(tag: ChangeTag) -> ToolDiffChangeKind {
    match tag {
        ChangeTag::Equal => ToolDiffChangeKind::Equal,
        ChangeTag::Insert => ToolDiffChangeKind::Insert,
        ChangeTag::Delete => ToolDiffChangeKind::Delete,
    }
}

pub(super) fn format_preview(details: &ToolDiffDetails) -> String {
    let hunks = &details.hunks;
    let line_width = hunks
        .iter()
        .flat_map(|hunk| &hunk.changes)
        .flat_map(|change| [change.old_line, change.new_line])
        .flatten()
        .max()
        .unwrap_or(1)
        .to_string()
        .len();
    let omitted = format!(" {:>width$} ...", "", width = line_width);
    hunks
        .iter()
        .map(|hunk| {
            hunk.changes
                .iter()
                .map(|change| format_preview_change(change, line_width))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join(&format!("\n{omitted}\n"))
}

fn format_preview_change(change: &ToolDiffChange, line_width: usize) -> String {
    let (prefix, line) = match change.kind {
        ToolDiffChangeKind::Equal => (' ', change.old_line.or(change.new_line)),
        ToolDiffChangeKind::Insert => ('+', change.new_line),
        ToolDiffChangeKind::Delete => ('-', change.old_line),
    };
    format!(
        "{prefix}{:>width$} {}",
        line.unwrap_or_default(),
        change.text,
        width = line_width
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_semantic_changes_patch_and_changed_line() {
        let diff = file_diff("test.txt", "a\nb\nc\n", "a\nB\nc\n");

        let changes = diff
            .hunks
            .iter()
            .flat_map(|hunk| &hunk.changes)
            .collect::<Vec<_>>();
        assert!(changes.iter().any(|change| {
            change.kind == ToolDiffChangeKind::Delete
                && change.old_line == Some(2)
                && change.text == "b"
        }));
        assert!(changes.iter().any(|change| {
            change.kind == ToolDiffChangeKind::Insert
                && change.new_line == Some(2)
                && change.text == "B"
        }));
        assert!(diff.patch.as_deref().unwrap().contains("--- test.txt"));
        assert_eq!(diff.first_changed_line, Some(2));
        let preview = format_preview(&diff);
        assert!(preview.contains("-2 b"));
        assert!(preview.contains("+2 B"));

        let old = (1..=20).map(|line| format!("{line}\n")).collect::<Vec<_>>();
        let mut new = old.clone();
        new[1] = "two\n".to_string();
        new[18] = "nineteen\n".to_string();
        let distant = file_diff("test.txt", &old.concat(), &new.concat());
        assert_eq!(distant.hunks.len(), 2);
        assert!(format_preview(&distant).contains("..."));
    }
}
