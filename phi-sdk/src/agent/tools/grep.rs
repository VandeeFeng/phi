use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::{Regex, RegexBuilder};
use rig::tool::Tool;
use tokio::process::Command;
use tokio::task::yield_now;

use crate::agent::tools::{
    GrepArgs, GrepOutputMode, MAX_GREP_RESULTS, ToolError, check_perm_path, is_skip_dir,
    resolve_tool_path,
};
use crate::permission::runtime::PermissionRuntime;

pub struct GrepTool {
    pub permission: Option<PermissionRuntime>,
    working_dir: PathBuf,
}

impl GrepTool {
    pub fn new(permission: Option<PermissionRuntime>, working_dir: PathBuf) -> Self {
        Self {
            permission,
            working_dir,
        }
    }

    fn glob_to_regex(glob: &str) -> String {
        let mut re = String::with_capacity(glob.len() * 2);
        for c in glob.chars() {
            match c {
                '.' => re.push_str("\\."),
                '*' => re.push_str(".*"),
                '?' => re.push('.'),
                '{' => re.push_str("(?:"),
                '}' => re.push(')'),
                ',' => re.push('|'),
                _ => re.push(c),
            }
        }
        re
    }

    fn is_binary(data: &[u8]) -> bool {
        data.iter().take(8192).any(|&b| b == 0)
    }

    fn apply_limit<T>(
        items: Vec<T>,
        head_limit: Option<usize>,
        offset: Option<usize>,
    ) -> (Vec<T>, bool) {
        let offset = offset.unwrap_or(0);
        let total = items.len();
        let limit = head_limit.unwrap_or(MAX_GREP_RESULTS);
        if limit == 0 {
            return (items.into_iter().skip(offset).collect(), false);
        }
        let truncated = total.saturating_sub(offset) > limit;
        (
            items.into_iter().skip(offset).take(limit).collect(),
            truncated,
        )
    }

    async fn run_rg(
        &self,
        args: &GrepArgs,
        search_path: &Path,
    ) -> Result<Option<String>, ToolError> {
        let output_mode = args.output_mode.unwrap_or(GrepOutputMode::FilesWithMatches);
        let mut cmd = Command::new("rg");
        cmd.current_dir(&self.working_dir);

        cmd.arg("--max-columns")
            .arg("500")
            .arg("--glob")
            .arg("!node_modules")
            .arg("--glob")
            .arg("!target");

        match output_mode {
            GrepOutputMode::Content => {
                cmd.arg("-n").arg("--with-filename");
                if let Some(context) = args.context_lines {
                    cmd.arg("-C").arg(context.to_string());
                }
            }
            GrepOutputMode::FilesWithMatches => {
                cmd.arg("-l");
            }
            GrepOutputMode::Count => {
                cmd.arg("-c").arg("--with-filename");
            }
        }

        if args.case_insensitive.unwrap_or(false) {
            cmd.arg("-i");
        }
        if args.multiline.unwrap_or(false) {
            cmd.arg("-U").arg("--multiline-dotall");
        }
        if let Some(file_type) = &args.file_type {
            cmd.arg("--type").arg(file_type);
        }
        if let Some(include) = &args.include {
            cmd.arg("--glob").arg(include);
        }

        if args.pattern.starts_with('-') {
            cmd.arg("-e").arg(&args.pattern);
        } else {
            cmd.arg(&args.pattern);
        }

        cmd.arg(search_path);

        let output = match cmd.output().await {
            Ok(output) => output,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };

        if output.status.success() || output.status.code() == Some(1) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let raw_results: Vec<String> = stdout.lines().map(|l| l.to_string()).collect();
            let (results, truncated) = Self::apply_limit(raw_results, args.head_limit, args.offset);

            if results.is_empty() {
                return Ok(Some("No matches found.".to_string()));
            }

            let suffix = if truncated {
                "\n\n... output truncated; use offset/head_limit to paginate".to_string()
            } else {
                String::new()
            };

            return Ok(Some(format!(
                "{} results (ripgrep):\n{}{}",
                results.len(),
                results.join("\n"),
                suffix
            )));
        }

        Err(ToolError::Msg(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }

    async fn run_builtin(args: &GrepArgs, search_path: &Path) -> Result<String, ToolError> {
        if args.multiline.unwrap_or(false) {
            return Err(ToolError::Msg(
                "multiline grep requires ripgrep to be installed".to_string(),
            ));
        }

        if args.file_type.is_some() {
            return Err(ToolError::Msg(
                "file_type grep requires ripgrep to be installed".to_string(),
            ));
        }

        let re = RegexBuilder::new(&args.pattern)
            .case_insensitive(args.case_insensitive.unwrap_or(false))
            .build()
            .map_err(|e| ToolError::Msg(format!("Invalid regex pattern: {}", e)))?;

        let context = args.context_lines.unwrap_or(0);
        let output_mode = args.output_mode.unwrap_or(GrepOutputMode::FilesWithMatches);

        let include_re = args.include.as_ref().map(|g| {
            let pattern = format!("^(?:{})$", Self::glob_to_regex(g));
            Regex::new(&pattern).unwrap_or_else(|_| Regex::new(".*").unwrap())
        });

        let walker = WalkBuilder::new(search_path)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .hidden(false)
            .filter_entry(|entry| {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    !is_skip_dir(entry.file_name().to_str().unwrap_or(""))
                } else {
                    true
                }
            })
            .build();

        let mut file_count = 0;
        let mut visited = 0usize;
        let mut all_results: Vec<String> = Vec::new();
        let result_cap = match args.head_limit.unwrap_or(MAX_GREP_RESULTS) {
            0 => usize::MAX,
            limit => args.offset.unwrap_or(0).saturating_add(limit),
        };

        for entry in walker
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            visited += 1;
            if visited.is_multiple_of(32) {
                yield_now().await;
            }

            if all_results.len() >= result_cap {
                break;
            }

            if let Some(ref re_include) = include_re {
                let fname = entry.file_name().to_string_lossy();
                if !re_include.is_match(&fname) {
                    continue;
                }
            }

            if let Ok(meta) = entry.metadata()
                && meta.len() > 10 * 1024 * 1024
            {
                continue;
            }

            let path_str = entry.path().to_string_lossy().to_string();

            match tokio::fs::read(entry.path()).await {
                Ok(data) => {
                    if Self::is_binary(&data) {
                        continue;
                    }
                    file_count += 1;
                    let content = String::from_utf8_lossy(&data);
                    let lines: Vec<&str> = content.lines().collect();
                    let total = lines.len();

                    let match_lines: Vec<usize> = lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| re.is_match(l))
                        .map(|(i, _)| i)
                        .collect();

                    if match_lines.is_empty() {
                        continue;
                    }

                    match output_mode {
                        GrepOutputMode::FilesWithMatches => {
                            all_results.push(path_str.clone());
                        }
                        GrepOutputMode::Count => {
                            all_results.push(format!("{}:{}", path_str, match_lines.len()));
                        }
                        GrepOutputMode::Content => {
                            if context == 0 {
                                for &ml in &match_lines {
                                    all_results.push(format!(
                                        "{}:{}:{}",
                                        path_str,
                                        ml + 1,
                                        lines[ml]
                                    ));
                                    if all_results.len() >= result_cap {
                                        break;
                                    }
                                }
                            } else {
                                let mut shown = vec![false; total];
                                for &ml in &match_lines {
                                    let start = ml.saturating_sub(context);
                                    let end = (ml + 1 + context).min(total);
                                    for s in &mut shown[start..end] {
                                        *s = true;
                                    }
                                }

                                let mut i = 0;
                                while i < total && all_results.len() < result_cap {
                                    if !shown[i] {
                                        i += 1;
                                        continue;
                                    }

                                    if !all_results.is_empty() {
                                        all_results.push("--".to_string());
                                    }

                                    while i < total && shown[i] && all_results.len() < result_cap {
                                        let is_match = match_lines.binary_search(&i).is_ok();
                                        let sep = if is_match { ':' } else { '-' };
                                        all_results.push(format!(
                                            "{}-{}{} {}",
                                            path_str,
                                            i + 1,
                                            sep,
                                            lines[i]
                                        ));
                                        i += 1;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(_) => continue,
            }
        }

        let (all_results, truncated) = Self::apply_limit(all_results, args.head_limit, args.offset);

        if all_results.is_empty() {
            return Ok("No matches found.".to_string());
        }

        let suffix = if truncated {
            "\n\n... output truncated; use offset/head_limit to paginate"
        } else {
            ""
        };

        Ok(format!(
            "{} results (searched {} files):\n{}{}",
            all_results.len(),
            file_count,
            all_results.join("\n"),
            suffix
        ))
    }
}

impl Tool for GrepTool {
    const NAME: &'static str = "Grep";

    type Error = ToolError;
    type Args = GrepArgs;
    type Output = String;

    fn description(&self) -> String {
        "Search file contents with a regex pattern. Defaults to returning matching file paths; use output_mode: content to show matches.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for (supports Rust regex syntax)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search in (defaults to current working directory)"
                },
                "include": {
                    "type": "string",
                    "description": "Optional file glob pattern to filter (e.g. '*.rs', '*.{ts,tsx}')"
                },
                "context_lines": {
                    "type": "integer",
                    "description": "Number of context lines to show before and after each match (like grep -C)"
                },
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Output mode. Defaults to files_with_matches."
                },
                "head_limit": {
                    "type": "integer",
                    "description": "Limit output entries. Defaults to 200. Use 0 for unlimited."
                },
                "offset": {
                    "type": "integer",
                    "description": "Skip this many output entries before applying head_limit."
                },
                "file_type": {
                    "type": "string",
                    "description": "Ripgrep file type filter, e.g. rust, js, py."
                },
                "case_insensitive": {
                    "type": "boolean",
                    "description": "Search case-insensitively."
                },
                "multiline": {
                    "type": "boolean",
                    "description": "Enable ripgrep multiline mode."
                }
            },
            "required": ["pattern"]
        })
    }

    async fn call(
        &self,
        _context: &mut rig::tool::ToolContext,
        args: GrepArgs,
    ) -> Result<String, ToolError> {
        let search_path = resolve_tool_path(&self.working_dir, args.path.as_deref());
        check_perm_path(
            &self.permission,
            "grep",
            search_path.to_string_lossy().as_ref(),
        )
        .await?;

        if let Some(output) = self.run_rg(&args, &search_path).await? {
            return Ok(output);
        }

        Self::run_builtin(&args, &search_path).await
    }
}
