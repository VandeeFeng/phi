use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::Regex;
use rig::tool::Tool;
use tokio::process::Command;
use tokio::task::yield_now;

use crate::agent::tools::{GlobArgs, ToolError, check_perm_path, is_skip_dir, resolve_tool_path};
use crate::permission::runtime::PermissionRuntime;

const DEFAULT_GLOB_LIMIT: usize = 100;
const MAX_GLOB_BYTES: usize = 50 * 1024;

pub struct GlobTool {
    pub permission: Option<PermissionRuntime>,
    working_dir: PathBuf,
}

impl GlobTool {
    pub fn new(permission: Option<PermissionRuntime>, working_dir: PathBuf) -> Self {
        Self {
            permission,
            working_dir,
        }
    }

    fn is_env_truthy(name: &str, default: bool) -> bool {
        std::env::var(name)
            .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "on"))
            .unwrap_or(default)
    }

    fn split_absolute_pattern(pattern: &str) -> Option<(PathBuf, String)> {
        let path = Path::new(pattern);
        if !path.is_absolute() {
            return None;
        }

        let glob_index = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
        let static_prefix = &pattern[..glob_index];
        let separator = static_prefix.rfind(['/', std::path::MAIN_SEPARATOR])?;
        let base = if separator == 0 {
            PathBuf::from("/")
        } else {
            PathBuf::from(&pattern[..separator])
        };
        let relative = pattern[separator + 1..].to_string();
        Some((base, relative))
    }

    fn format_results(results: Vec<String>, truncated: bool) -> String {
        if results.is_empty() {
            return "No files found".to_string();
        }

        let suffix = if truncated {
            "\n(Results are truncated. Consider using a more specific path or pattern.)"
        } else {
            ""
        };
        format!("{}{}", results.join("\n"), suffix)
    }

    fn glob_to_regex(glob: &str) -> String {
        let mut regex = String::from("^");
        let chars: Vec<char> = glob.chars().collect();
        let mut index = 0;
        while index < chars.len() {
            match chars[index] {
                '*' if chars.get(index + 1) == Some(&'*') => {
                    regex.push_str(".*");
                    index += 2;
                }
                '*' => {
                    regex.push_str("[^/]*");
                    index += 1;
                }
                '?' => {
                    regex.push_str("[^/]");
                    index += 1;
                }
                '.' | '+' | '(' | ')' | '|' | '^' | '$' | '[' | ']' | '{' | '}' | '\\' => {
                    regex.push('\\');
                    regex.push(chars[index]);
                    index += 1;
                }
                char => {
                    regex.push(char);
                    index += 1;
                }
            }
        }
        regex.push('$');
        regex
    }

    async fn run_rg(
        &self,
        args: &GlobArgs,
        search_path: &Path,
        limit: usize,
    ) -> Result<Option<String>, ToolError> {
        let mut cmd = Command::new("rg");
        cmd.current_dir(&self.working_dir);
        cmd.arg("--files")
            .arg("--glob")
            .arg(&args.pattern)
            .arg("--sort=modified");

        if Self::is_env_truthy("PHI_GLOB_NO_IGNORE", true) {
            cmd.arg("--no-ignore");
        }
        if Self::is_env_truthy("PHI_GLOB_HIDDEN", true) {
            cmd.arg("--hidden");
        }

        cmd.arg("--glob")
            .arg("!node_modules")
            .arg("--glob")
            .arg("!target")
            .arg(search_path);

        let output = match cmd.output().await {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        if output.status.success() || output.status.code() == Some(1) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut all: Vec<String> = stdout.lines().map(|line| line.to_string()).collect();
            let truncated = all.len() > limit;
            all.truncate(limit);
            return Ok(Some(Self::format_results(all, truncated)));
        }

        Err(ToolError::Msg(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

impl Tool for GlobTool {
    const NAME: &'static str = "Glob";

    type Error = ToolError;
    type Args = GlobArgs;
    type Output = String;

    fn description(&self) -> String {
        "Find files by glob pattern. Results are sorted by modification time and capped at 100 files by default."
                .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Required glob pattern to match files, e.g. '*', '*.rs', '**/*.json', or 'src/**/*.test.ts'" },
                "path": { "type": "string", "description": "Directory to search in (default: current directory; empty means current directory)" },
                "limit": { "type": "integer", "description": "Maximum number of results (default: 100)" }
            },
            "required": ["pattern"]
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        mut args: GlobArgs,
    ) -> Result<String, ToolError> {
        if args.pattern.trim().is_empty() {
            return Err(ToolError::Msg(
                "Glob pattern is required. Use '*' to list files in the current directory or '**/*' to search recursively."
                    .to_string(),
            ));
        }

        let mut search_path = resolve_tool_path(&self.working_dir, args.path.as_deref());
        if let Some((base, pattern)) = Self::split_absolute_pattern(&args.pattern) {
            search_path = resolve_tool_path(&self.working_dir, base.to_str());
            args.pattern = pattern;
        }
        check_perm_path(
            &self.permission,
            "glob",
            search_path.to_string_lossy().as_ref(),
        )
        .await?;

        let limit = args
            .limit
            .unwrap_or(DEFAULT_GLOB_LIMIT)
            .min(DEFAULT_GLOB_LIMIT);

        if let Some(output) = self.run_rg(&args, &search_path, limit).await? {
            return Ok(output);
        }

        let pattern = Regex::new(&Self::glob_to_regex(&args.pattern))
            .map_err(|error| ToolError::Msg(format!("Invalid glob pattern: {error}")))?;
        let walker = WalkBuilder::new(&search_path)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .hidden(false)
            .filter_entry(|entry| {
                if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    !is_skip_dir(entry.file_name().to_str().unwrap_or(""))
                } else {
                    true
                }
            })
            .build();

        let mut bytes = 0usize;
        let mut visited = 0usize;
        let mut results = Vec::new();
        for entry in walker.flatten().filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        }) {
            visited += 1;
            if visited.is_multiple_of(64) {
                yield_now().await;
            }

            let Ok(relative) = entry.path().strip_prefix(&search_path) else {
                continue;
            };
            let relative = relative
                .to_string_lossy()
                .trim_start_matches('/')
                .to_string();
            if !pattern.is_match(&relative)
                && !pattern.is_match(entry.file_name().to_string_lossy().as_ref())
            {
                continue;
            }

            let line_bytes = relative.len() + usize::from(!results.is_empty());
            if results.len() >= limit || bytes + line_bytes > MAX_GLOB_BYTES {
                break;
            }
            bytes += line_bytes;
            results.push(relative);
        }

        Ok(Self::format_results(results, false))
    }
}
