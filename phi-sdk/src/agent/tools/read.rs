use rig::tool::Tool;

use crate::agent::tools::{ReadArgs, ToolError, check_perm_path, resolve_tool_path};
use crate::permission::runtime::PermissionRuntime;

const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
const MAX_READ_LINES: usize = 2000;
const MAX_READ_BYTES: usize = 50 * 1024;

pub struct ReadTool {
    pub permission: Option<PermissionRuntime>,
    working_dir: std::path::PathBuf,
}

impl ReadTool {
    pub fn new(permission: Option<PermissionRuntime>, working_dir: std::path::PathBuf) -> Self {
        Self {
            permission,
            working_dir,
        }
    }

    fn take_limited_lines<'a>(
        lines: impl Iterator<Item = (usize, &'a str)>,
        requested_limit: usize,
    ) -> (Vec<(usize, &'a str)>, bool) {
        let max_lines = requested_limit.min(MAX_READ_LINES);
        let mut bytes = 0;
        let mut selected = Vec::new();
        let mut truncated = false;

        for (line_number, line) in lines.take(requested_limit) {
            if selected.len() >= max_lines {
                truncated = true;
                break;
            }
            let line_bytes = line.len() + usize::from(!selected.is_empty());
            if bytes + line_bytes > MAX_READ_BYTES {
                truncated = true;
                break;
            }
            bytes += line_bytes;
            selected.push((line_number, line));
        }

        (selected, truncated)
    }
}

impl Tool for ReadTool {
    const NAME: &'static str = "Read";

    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    fn description(&self) -> String {
        "Read the contents of a file. Supports text files. Output is limited to 2000 lines or 50KB, whichever is hit first. Use offset/limit for large files.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                "offset": { "type": "integer", "description": "Line number to start from (1-indexed)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        mut args: ReadArgs,
    ) -> Result<String, ToolError> {
        args.path = resolve_tool_path(&self.working_dir, Some(&args.path))
            .to_string_lossy()
            .into_owned();
        check_perm_path(&self.permission, "read", &args.path).await?;

        let metadata = tokio::fs::metadata(&args.path).await?;
        let file_size = metadata.len();
        if file_size > MAX_FILE_BYTES {
            return Err(ToolError::Msg(format!(
                "File too large ({} bytes). Max 10MB.",
                file_size
            )));
        }
        let content = tokio::fs::read_to_string(&args.path).await?;
        let total_lines = content.lines().count();

        let offset = args.offset.unwrap_or(1).max(1) - 1;
        if offset >= total_lines && total_lines > 0 {
            return Err(ToolError::Msg(format!(
                "Offset {} is beyond end of file ({} lines total).",
                offset + 1,
                total_lines
            )));
        }
        let requested_limit = args.limit.unwrap_or(MAX_READ_LINES).max(1);
        let (selected, truncated) =
            Self::take_limited_lines(content.lines().enumerate().skip(offset), requested_limit);
        let end = selected.last().map(|(index, _)| index + 1).unwrap_or(0);

        let number_width = end.max(1).to_string().len();
        let mut excerpt = selected
            .iter()
            .map(|(index, line)| format!("{:>width$}  {}", index + 1, line, width = number_width))
            .collect::<Vec<_>>()
            .join("\n");
        if truncated || end < total_lines && args.limit.is_none() {
            let next_offset = end + 1;
            excerpt.push_str(&format!(
                "\n\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                offset + 1,
                end,
                total_lines,
                next_offset
            ));
        } else if let Some(limit) = args.limit
            && offset + limit < total_lines
        {
            excerpt.push_str(&format!(
                "\n\n[{} more lines in file. Use offset={} to continue.]",
                total_lines - offset - limit,
                offset + limit + 1
            ));
        }
        let info = format!(
            "File: {} ({} lines total, showing lines {}-{})\n\n{}",
            args.path,
            total_lines,
            offset + 1,
            end,
            excerpt
        );
        Ok(info)
    }
}
