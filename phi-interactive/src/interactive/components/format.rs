use phi_tui::{BlockBody, DiffLine, DiffLineKind, DiffView, OutputSection};

use phi_sdk::tools::{ToolDiffChange, ToolDiffChangeKind, ToolDiffDetails, ToolDisplay};
use phi_sdk::transcript::{ContentBlock, UserContentBlock};

const CODEMODE_PREVIEW_CHARS: usize = 100;

pub(in crate::interactive) fn codemode_output_body(output: &str) -> BlockBody {
    let mut sections = Vec::new();
    let mut text = String::new();
    let mut preview_chars = None;
    for line in output.split_inclusive('\n') {
        let heading = line.starts_with("==> ") && line.trim_end().ends_with(" <==");
        let output_start = line.trim_end() == "Output:";
        let console_boundary = matches!(line.trim_end(), "<console_output>" | "</console_output>");
        let diagnostic = line.starts_with("[Full output: ")
            || line.starts_with("[Could not save full output: ")
            || line.starts_with("Script error:")
            || matches!(line.trim_end(), "<script_error>" | "</script_error>");
        if heading || output_start || console_boundary || diagnostic {
            if !text.is_empty() {
                sections.push(OutputSection {
                    text: std::mem::take(&mut text).into(),
                    preview_chars,
                });
            }
            if heading || output_start || console_boundary {
                sections.push(OutputSection {
                    text: line.to_string().into(),
                    preview_chars: None,
                });
                preview_chars = (line.starts_with("==> text ")
                    || output_start
                    || line.trim_end() == "<console_output>")
                    .then_some(CODEMODE_PREVIEW_CHARS);
                continue;
            }
            preview_chars = None;
        }
        text.push_str(line);
    }
    if !text.is_empty() {
        sections.push(OutputSection {
            text: text.into(),
            preview_chars,
        });
    }
    BlockBody::OutputSections(sections)
}

pub(in crate::interactive) fn tool_diff_view(details: &ToolDiffDetails) -> DiffView {
    let mut lines = Vec::new();
    for (index, hunk) in details.hunks.iter().enumerate() {
        if index > 0 {
            lines.push(DiffLine::omitted());
        }
        lines.extend(hunk.changes.iter().map(tool_diff_line));
    }
    DiffView { lines }
}

fn tool_diff_line(change: &ToolDiffChange) -> DiffLine {
    let kind = match change.kind {
        ToolDiffChangeKind::Equal => DiffLineKind::Equal,
        ToolDiffChangeKind::Insert => DiffLineKind::Insert,
        ToolDiffChangeKind::Delete => DiffLineKind::Delete,
    };
    DiffLine::change(kind, change.old_line, change.new_line, change.text.clone())
}

pub(crate) fn tool_display_formatter(
    name: &str,
    args: &str,
) -> anyhow::Result<Option<ToolDisplay>> {
    Ok(Some(format_tool_display(name, args)))
}

pub(in crate::interactive) fn format_agent_title(name: &str) -> String {
    format!("[agent] {name}")
}

pub(in crate::interactive) fn format_tool_title(
    title: &str,
    mcp_display_name: Option<&str>,
) -> String {
    let title = mcp_display_name
        .map(|name| format!("MCP {name}"))
        .unwrap_or_else(|| title.to_string());
    phi_tui::abbreviate_home_paths(&title)
}

pub(in crate::interactive) fn format_tool_display(name: &str, args: &str) -> ToolDisplay {
    let title = match name {
        "bash" | "Bash" => "Bash",
        "read" | "Read" => "Read",
        _ => name,
    };
    let value = serde_json::from_str::<serde_json::Value>(args).ok();
    let input = value
        .as_ref()
        .and_then(|value| tool_invocation_input(name, value))
        .unwrap_or_else(|| args.to_string());
    let title_suffix = value
        .as_ref()
        .filter(|_| name.eq_ignore_ascii_case("bash"))
        .and_then(|value| value.get("timeout"))
        .and_then(|timeout| timeout.as_u64())
        .map(|timeout| format!(" (timeout {timeout}s)"));
    ToolDisplay {
        title: Some(title.to_string()),
        input: Some(input),
        title_suffix,
        ..ToolDisplay::default()
    }
}

fn tool_invocation_input(name: &str, value: &serde_json::Value) -> Option<String> {
    let path = value
        .get("path")
        .and_then(|value| value.as_str())
        .map(phi_tui::compact_path);
    match (name, path.as_deref()) {
        ("Codemode", _) => value
            .get("code")
            .and_then(|value| value.as_str())
            .map(|source| format!("{{{source}}}")),
        ("bash" | "Bash", _) => value
            .get("command")
            .and_then(|value| value.as_str())
            .map(|command| format!("{{{command}}}")),
        ("read" | "Read", Some(path)) => Some(format_read_input(path, value)),
        (_, Some(path)) => Some(path.to_string()),
        _ => None,
    }
}

pub(in crate::interactive) fn read_input_from_output(
    args: &str,
    input: &str,
    output: &str,
) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(args).ok()?;
    let path = phi_tui::compact_path(value.get("path")?.as_str()?);
    if input != format_read_input(&path, &value) {
        return None;
    }
    let header = output.lines().next()?;
    let (_, range) = header.split_once("showing lines ")?;
    let range = range.strip_suffix(')')?;
    Some(format!("{{{path}:{range}}}"))
}

pub(in crate::interactive) fn text_from_user_blocks(blocks: &[UserContentBlock]) -> Option<String> {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text { text } => Some(text.as_str()),
            UserContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

pub(in crate::interactive) fn text_from_blocks(blocks: &[ContentBlock]) -> Option<String> {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn format_read_input(path: &str, value: &serde_json::Value) -> String {
    let Some(offset) = value.get("offset").and_then(|value| value.as_u64()) else {
        return format!("{{{path}}}");
    };
    let Some(limit) = value.get("limit").and_then(|value| value.as_u64()) else {
        return format!("{{{path}:{}}}", offset.max(1));
    };
    let end = offset.saturating_add(limit).saturating_sub(1).max(offset);
    format!("{{{path}:{}-{end}}}", offset.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_labels_and_input_summaries_are_separate() {
        for (name, args, title, input) in [
            (
                "Bash",
                r#"{"command":"echo one\necho two"}"#,
                "Bash",
                "{echo one\necho two}",
            ),
            (
                "read",
                r#"{"path":"README.md","offset":2,"limit":3}"#,
                "Read",
                "{README.md:2-4}",
            ),
            ("Edit", r#"{"path":"src/main.rs"}"#, "Edit", "src/main.rs"),
            (
                "ToolSearch",
                r#"{"query":"docs"}"#,
                "ToolSearch",
                r#"{"query":"docs"}"#,
            ),
            (
                "Codemode",
                r#"{"code":"const result = await tools.Read({path: 'README.md'});\nreturn result;"}"#,
                "Codemode",
                "{const result = await tools.Read({path: 'README.md'});\nreturn result;}",
            ),
            ("custom", "invalid json", "custom", "invalid json"),
        ] {
            let display = format_tool_display(name, args);
            assert_eq!(display.title.as_deref(), Some(title));
            assert_eq!(display.input.as_deref(), Some(input));
        }
        let display = format_tool_display("server_search", r#"{"query":"docs"}"#);
        assert_eq!(
            format_tool_title(display.title.as_deref().unwrap(), Some("server: search")),
            "MCP server: search"
        );
        assert_eq!(display.input.as_deref(), Some(r#"{"query":"docs"}"#));
    }

    #[test]
    fn read_result_updates_only_the_input_range() {
        assert_eq!(
            read_input_from_output(
                r#"{"path":"README.md","offset":2,"limit":3}"#,
                "{README.md:2-4}",
                "(100 lines, showing lines 2-3)\ntext"
            ),
            Some("{README.md:2-3}".into())
        );
        assert_eq!(
            read_input_from_output(r#"{"path":"README.md"}"#, "{README.md}", "read failed"),
            None
        );
    }
}
