use std::{path::Path, sync::Arc};

use rig::tool::Tool;

use crate::agent::tools::{
    ToolError, WriteArgs, check_perm_path_with_preview, diff, resolve_tool_path,
    with_file_mutation_queue,
};
use crate::core::event::ToolResultDetails;
use crate::permission::runtime::PermissionRuntime;
use crate::sandbox::Sandbox;

pub struct WriteTool {
    pub permission: Option<PermissionRuntime>,
    working_dir: std::path::PathBuf,
    sandbox: Arc<Sandbox>,
}

impl WriteTool {
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

impl Tool for WriteTool {
    const NAME: &'static str = "Write";

    type Error = ToolError;
    type Args = WriteArgs;
    type Output = String;

    fn description(&self) -> String {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        })
    }

    async fn call(
        &self,
        context: &mut rig::tool::ToolContext,
        mut args: WriteArgs,
    ) -> Result<String, ToolError> {
        args.path = resolve_tool_path(&self.working_dir, Some(&args.path))
            .to_string_lossy()
            .into_owned();
        let preview_args = args.clone();
        check_perm_path_with_preview(&self.permission, "write", &args.path, || async {
            Ok(Some(preview_write_diff(preview_args).await?))
        })
        .await?;

        with_file_mutation_queue(args.path.clone(), apply_write(context, args, &self.sandbox)).await
    }
}

async fn apply_write(
    context: &mut rig::tool::ToolContext,
    args: WriteArgs,
    sandbox: &Sandbox,
) -> Result<String, ToolError> {
    let path = Path::new(&args.path);
    let old_content = read_existing_content(path).await?;
    let new_content = normalize_platform_newlines(&args.content);
    let diff = diff::file_diff(&args.path, &old_content, &new_content);

    let bytes = new_content.len();
    sandbox
        .write_file(path, new_content.as_bytes(), true)
        .await
        .map_err(|error| ToolError::Msg(error.to_string()))?;

    context.insert_result(ToolResultDetails {
        diff: Some(diff),
        ..Default::default()
    });

    Ok(format!("Written {} bytes to {}", bytes, args.path))
}

async fn preview_write_diff(args: WriteArgs) -> Result<String, ToolError> {
    let old_content = read_existing_content(Path::new(&args.path)).await?;
    let new_content = normalize_platform_newlines(&args.content);
    let details = diff::file_diff(&args.path, &old_content, &new_content);
    Ok(diff::format_preview(&details))
}

fn normalize_platform_newlines(content: &str) -> String {
    let lf = content.replace("\r\n", "\n").replace('\r', "\n");

    #[cfg(windows)]
    {
        lf.replace('\n', "\r\n")
    }

    #[cfg(not(windows))]
    {
        lf
    }
}

async fn read_existing_content(path: &Path) -> Result<String, ToolError> {
    match tokio::fs::read(path).await {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}
