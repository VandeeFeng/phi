use std::sync::Arc;

use rig::tool::Tool;

use crate::agent::tools::{
    EditArgs, ToolError, check_perm_path_with_preview, diff, resolve_tool_path,
    with_file_mutation_queue,
};
use crate::core::event::ToolResultDetails;
use crate::permission::runtime::PermissionRuntime;
use crate::sandbox::Sandbox;

pub struct EditTool {
    pub permission: Option<PermissionRuntime>,
    working_dir: std::path::PathBuf,
    sandbox: Arc<Sandbox>,
}

impl EditTool {
    pub fn new(
        permission: Option<PermissionRuntime>,
        working_dir: std::path::PathBuf,
        sandbox: Arc<Sandbox>,
    ) -> Self {
        Self {
            permission,
            working_dir,
            sandbox,
        }
    }
}

impl Tool for EditTool {
    const NAME: &'static str = "Edit";

    type Error = ToolError;
    type Args = EditArgs;
    type Output = String;

    fn description(&self) -> String {
        "Edit a file by replacing exact text. If old_text appears multiple times and replace_all is false, returns all match locations. Use replace_all: true to replace every occurrence. Handles LF and CRLF line endings.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                "old_text": { "type": "string", "description": "Exact text to find and replace" },
                "new_text": { "type": "string", "description": "New text to replace with" },
                "replace_all": { "type": "boolean", "description": "Replace all occurrences instead of just the first" }
            },
            "required": ["path", "old_text", "new_text"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        mut args: EditArgs,
    ) -> Result<String, ToolError> {
        args.path = resolve_tool_path(&self.working_dir, Some(&args.path))
            .to_string_lossy()
            .into_owned();
        if args.old_text.is_empty() {
            return Err(ToolError::Msg(
                "old_text must not be empty. Provide the exact text to replace.".to_string(),
            ));
        }

        let preview_args = args.clone();
        check_perm_path_with_preview(&self.permission, "edit", &args.path, || async {
            Ok(Some(preview_edit_diff(preview_args).await?))
        })
        .await?;

        with_file_mutation_queue(args.path.clone(), apply_edit(context, args, &self.sandbox)).await
    }
}

async fn apply_edit(
    context: &mut rig::tool::ToolContext,
    args: EditArgs,
    sandbox: &Sandbox,
) -> Result<String, ToolError> {
    let bytes = tokio::fs::read(&args.path).await?;
    let has_crlf = bytes.windows(2).any(|w| w == b"\r\n");
    let content = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");

    if !content.contains(&args.old_text) {
        return Err(ToolError::Msg(format!(
            "old_text not found in '{}'.\nEnsure the exact text matches including whitespace and line endings.",
            args.path
        )));
    }

    let match_positions: Vec<usize> = content
        .match_indices(&args.old_text)
        .map(|(i, _)| i)
        .collect();

    let do_replace_all = args.replace_all.unwrap_or(false);

    if match_positions.len() > 1 && !do_replace_all {
        let line_starts: Vec<usize> = std::iter::once(0)
            .chain(content.match_indices('\n').map(|(i, _)| i + 1))
            .collect();

        let mut match_info = Vec::new();
        for &byte_idx in &match_positions {
            let line_num = match line_starts.binary_search(&byte_idx) {
                Ok(i) => i + 1,
                Err(i) => i,
            };
            let line_start = line_starts.get(line_num - 1).copied().unwrap_or(0);
            let line_end = content[line_start..]
                .find('\n')
                .map(|e| line_start + e)
                .unwrap_or(content.len());
            let line_text = &content[line_start..line_end];
            let truncated: String = line_text.chars().take(100).collect();
            match_info.push(format!("  Line {}: {}", line_num, truncated));
        }

        return Err(ToolError::Msg(format!(
            "old_text matched {} times in {}:\n{}\n\nUse replaceAll: true to replace all occurrences, or provide more surrounding context in old_text to narrow the match.",
            match_positions.len(),
            args.path,
            match_info.join("\n"),
        )));
    }

    let new_content = if do_replace_all {
        content.replace(&args.old_text, &args.new_text)
    } else {
        content.replacen(&args.old_text, &args.new_text, 1)
    };

    let diff = diff::file_diff(&args.path, &content, &new_content);
    let output = if has_crlf {
        new_content.replace('\n', "\r\n")
    } else {
        new_content
    };

    sandbox
        .write_file(std::path::Path::new(&args.path), output.as_bytes(), false)
        .await
        .map_err(|error| ToolError::Msg(error.to_string()))?;

    let mut result = format!("Applied edit to {}", args.path);
    if do_replace_all {
        result.push_str(&format!(" ({} replacements)", match_positions.len()));
    }

    context.insert_result(ToolResultDetails {
        diff: Some(diff),
        ..Default::default()
    });

    Ok(result)
}

async fn preview_edit_diff(args: EditArgs) -> Result<String, ToolError> {
    let bytes = tokio::fs::read(&args.path).await?;
    let content = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");

    if args.old_text.is_empty() {
        return Err(ToolError::Msg("old_text must not be empty".to_string()));
    }
    if !content.contains(&args.old_text) {
        return Err(ToolError::Msg(format!(
            "old_text not found in '{}'",
            args.path
        )));
    }

    let do_replace_all = args.replace_all.unwrap_or(false);
    let new_content = if do_replace_all {
        content.replace(&args.old_text, &args.new_text)
    } else {
        content.replacen(&args.old_text, &args.new_text, 1)
    };

    let details = diff::file_diff(&args.path, &content, &new_content);
    Ok(diff::format_preview(&details))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_rejects_empty_old_text() {
        let tool = EditTool::new(
            None,
            std::env::temp_dir(),
            Arc::new(Sandbox::new(false, std::env::temp_dir()).unwrap()),
        );
        let args = EditArgs {
            path: "/tmp/test.txt".to_string(),
            old_text: String::new(),
            new_text: "replacement".to_string(),
            replace_all: None,
        };
        let result = tool.call(&mut rig::tool::ToolContext::new(), args).await;
        assert!(result.is_err());
        match result {
            Err(ToolError::Msg(msg)) => {
                assert!(
                    msg.contains("old_text must not be empty"),
                    "unexpected msg: {msg}"
                );
            }
            _ => panic!("expected ToolError::Msg"),
        }
    }
}
