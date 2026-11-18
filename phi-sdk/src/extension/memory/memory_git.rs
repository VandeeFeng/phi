use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime};

use chrono::Local;

use crate::utils::workspace::{WorkspaceMeta, git_command};

const TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const NETWORK_MAX_ATTEMPTS: usize = 3;
const NETWORK_RETRY_DELAY: Duration = Duration::from_millis(250);
const RETRY_POLL_INTERVAL: Duration = Duration::from_millis(5);
const TRANSIENT_NETWORK_ERRORS: &[&str] = &[
    "connection timed out",
    "connection timeout",
    "connection reset",
    "connection refused",
    "could not resolve host",
    "temporary failure in name resolution",
    "failed to connect",
    "remote end hung up unexpectedly",
];
const TIMEOUT_MESSAGE: &str = "Unable to connect to git repository, connection timeout (10s). Please check your network connection or try again later.";

#[derive(Debug, Clone, Default)]
pub struct GitResult {
    pub stdout: String,
    pub success: bool,
    pub timeout: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SyncResult {
    pub success: bool,
    pub message: String,
    pub updated: bool,
    pub level: Option<SyncLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncLevel {
    Info,
    Warning,
    Error,
}

pub trait MemoryGit: Send + Sync {
    fn status(&self) -> anyhow::Result<GitResult>;
    fn pull(&self) -> anyhow::Result<SyncResult>;
    fn push(&self) -> anyhow::Result<SyncResult>;
}

#[derive(Debug, Clone)]
pub struct LocalMemoryGit {
    local_path: PathBuf,
    repo_url: Option<String>,
    timeout: Duration,
    cancellation: tokio_util::sync::CancellationToken,
}

impl LocalMemoryGit {
    pub fn new(local_path: impl Into<PathBuf>, repo_url: Option<String>) -> Self {
        Self {
            local_path: local_path.into(),
            repo_url,
            timeout: TIMEOUT,
            cancellation: tokio_util::sync::CancellationToken::new(),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub(crate) fn with_cancellation(
        mut self,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub fn git_exec(&self, cwd: impl AsRef<Path>, args: &[&str]) -> GitResult {
        let cwd = cwd.as_ref();
        retry_git_command(
            args,
            || execute_git(git_command(cwd, args), self.timeout, &self.cancellation),
            |delay| {
                let started = std::time::Instant::now();
                while !self.cancellation.is_cancelled() && started.elapsed() < delay {
                    thread::sleep(RETRY_POLL_INTERVAL.min(delay.saturating_sub(started.elapsed())));
                }
            },
        )
    }

    fn repo_name(&self) -> &str {
        self.repo_url
            .as_deref()
            .and_then(repo_name_from_url)
            .unwrap_or("memory")
    }

    fn ensure_repo_url(&self) -> Result<&str, SyncResult> {
        self.repo_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
            .ok_or_else(|| SyncResult {
                success: false,
                message: "Git repository URL or local path not configured".to_string(),
                ..SyncResult::default()
            })
    }

    fn is_initialized_repo(&self) -> bool {
        WorkspaceMeta::discover(&self.local_path, Some(&self.cancellation)).is_ok_and(|workspace| {
            workspace.git_dir().is_some() && same_path(workspace.root, &self.local_path)
        })
    }

    fn should_fetch(&self) -> bool {
        let result = self.git_exec(&self.local_path, &["rev-parse", "--git-path", "FETCH_HEAD"]);
        if !result.success {
            return true;
        }
        let fetch_head = result.stdout.trim();
        if fetch_head.is_empty() {
            return true;
        }
        let path = PathBuf::from(fetch_head);
        let path = if path.is_absolute() {
            path
        } else {
            self.local_path.join(path)
        };
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .map_or(true, |modified| fetch_is_stale(modified))
    }

    fn has_upstream_branch(&self) -> bool {
        self.git_exec(&self.local_path, &["rev-parse", "--abbrev-ref", "@{u}"])
            .success
    }

    fn behind_count(&self) -> Option<usize> {
        if !self.has_upstream_branch() {
            return None;
        }
        let result = self.git_exec(&self.local_path, &["rev-list", "--count", "HEAD..@{u}"]);
        result
            .success
            .then(|| parse_count(&result.stdout))
            .flatten()
    }

    fn has_commits_to_push(&self) -> bool {
        if !self.has_upstream_branch() {
            return true;
        }
        let result = self.git_exec(&self.local_path, &["rev-list", "--count", "@{u}..HEAD"]);
        !result.success || parse_count(&result.stdout).unwrap_or(0) > 0
    }
}

impl MemoryGit for LocalMemoryGit {
    fn status(&self) -> anyhow::Result<GitResult> {
        Ok(self.git_exec(&self.local_path, &["status", "--porcelain"]))
    }

    fn pull(&self) -> anyhow::Result<SyncResult> {
        let repo_url = match self.ensure_repo_url() {
            Ok(repo_url) => repo_url,
            Err(result) => return Ok(result),
        };
        let repo_name = self.repo_name();

        if self.local_path.exists() && self.is_initialized_repo() {
            if self.should_fetch() {
                let fetch = self.git_exec(&self.local_path, &["fetch"]);
                if fetch.timeout {
                    return Ok(timeout_result());
                }
                if !fetch.success {
                    return Ok(failed(fetch.stdout, "Fetch failed"));
                }
            }

            let behind_count = self.behind_count();
            if behind_count == Some(0) {
                return Ok(SyncResult {
                    success: true,
                    message: format!("[{repo_name}] is already latest"),
                    updated: false,
                    level: None,
                });
            }

            let update = self.git_exec(&self.local_path, &["rebase", "--autostash", "@{u}"]);
            if update.timeout {
                return Ok(timeout_result());
            }
            if !update.success {
                return Ok(failed(update.stdout, "Update failed"));
            }

            if behind_count.is_some_and(|count| count > 0) {
                if let Some(remaining) = self.behind_count().filter(|count| *count > 0) {
                    return Ok(SyncResult {
                        success: false,
                        message: format!(
                            "Pull did not update [{repo_name}], still behind by {remaining} commit(s). Please resolve these git issues manually."
                        ),
                        updated: false,
                        level: Some(SyncLevel::Warning),
                    });
                }
            }

            let updated = behind_count.is_some_and(|count| count > 0);
            return Ok(SyncResult {
                success: true,
                message: if updated {
                    format!("Pulled latest changes from [{repo_name}]")
                } else {
                    format!("[{repo_name}] is already latest")
                },
                updated,
                level: None,
            });
        }

        if self.local_path.exists()
            && (!self.local_path.is_dir() || !is_empty_dir(&self.local_path)?)
        {
            return Ok(SyncResult {
                success: false,
                message: format!(
                    "Memory path exists but is not a git repo: {}. Please clone the memory repo into this path first.",
                    self.local_path.display()
                ),
                updated: false,
                level: Some(SyncLevel::Warning),
            });
        }

        if let Some(parent) = self.local_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let clone = self.git_exec(
            self.local_path.parent().unwrap_or_else(|| Path::new(".")),
            &[
                "clone",
                repo_url,
                self.local_path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("memory"),
            ],
        );
        if clone.timeout {
            return Ok(timeout_result());
        }
        if clone.success {
            return Ok(SyncResult {
                success: true,
                message: format!("Cloned [{repo_name}] successfully"),
                updated: true,
                level: None,
            });
        }
        Ok(failed(clone.stdout, "Clone failed"))
    }

    fn push(&self) -> anyhow::Result<SyncResult> {
        if let Err(result) = self.ensure_repo_url() {
            return Ok(result);
        }
        if !self.is_initialized_repo() {
            return Ok(SyncResult {
                success: false,
                message: format!(
                    "Git repository not initialized: {}",
                    self.local_path.display()
                ),
                ..SyncResult::default()
            });
        }

        let repo_name = self.repo_name();
        let status = self.status()?;
        if !status.success {
            return Ok(failed(status.stdout, "Git status failed"));
        }

        let has_changes = !status.stdout.trim().is_empty();
        if has_changes {
            let add = self.git_exec(&self.local_path, &["add", "."]);
            if !add.success {
                return Ok(failed(add.stdout, "Git add failed"));
            }
            let message = format!("Update memory - {}", format_commit_timestamp());
            let commit = self.git_exec(&self.local_path, &["commit", "-m", &message]);
            if !commit.success {
                return Ok(failed(commit.stdout, "Commit failed"));
            }
        }

        if !has_changes && !self.has_commits_to_push() {
            return Ok(SyncResult {
                success: true,
                message: format!("[{repo_name}] has no memory changes to push"),
                updated: false,
                level: None,
            });
        }

        let push = self.git_exec(&self.local_path, &["push"]);
        if push.timeout {
            return Ok(timeout_result());
        }
        if !push.success {
            return Ok(failed(push.stdout, "Push failed"));
        }
        Ok(SyncResult {
            success: true,
            message: format!("[{repo_name}] pushed memory changes"),
            updated: true,
            level: None,
        })
    }
}

fn retry_git_command(
    args: &[&str],
    mut execute: impl FnMut() -> GitResult,
    mut wait: impl FnMut(Duration),
) -> GitResult {
    let network_command = matches!(args.first().copied(), Some("fetch" | "clone" | "push"));
    let mut result = execute();
    for attempt in 1..NETWORK_MAX_ATTEMPTS {
        if result.success || !network_command || !transient_network_failure(&result) {
            break;
        }
        wait(NETWORK_RETRY_DELAY * attempt as u32);
        result = execute();
    }
    result
}

fn transient_network_failure(result: &GitResult) -> bool {
    let message = result.stdout.to_ascii_lowercase();
    result.timeout
        || TRANSIENT_NETWORK_ERRORS
            .iter()
            .any(|signature| message.contains(signature))
}

fn execute_git(
    mut command: Command,
    timeout: Duration,
    cancellation: &tokio_util::sync::CancellationToken,
) -> GitResult {
    match crate::utils::process::run_sync(&mut command, timeout, Some(cancellation)) {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            GitResult {
                stdout: if output.status.success() || stderr.is_empty() {
                    stdout
                } else {
                    stderr
                },
                success: output.status.success(),
                timeout: false,
            }
        }
        Err(error) => GitResult {
            stdout: error.to_string(),
            success: false,
            timeout: error.kind() == std::io::ErrorKind::TimedOut,
        },
    }
}

fn repo_name_from_url(repo_url: &str) -> Option<&str> {
    repo_url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .map(|name| name.strip_suffix(".git").unwrap_or(name))
        .filter(|name| !name.is_empty())
}

fn same_path(left: impl AsRef<Path>, right: impl AsRef<Path>) -> bool {
    let left = left
        .as_ref()
        .canonicalize()
        .unwrap_or_else(|_| left.as_ref().to_path_buf());
    let right = right
        .as_ref()
        .canonicalize()
        .unwrap_or_else(|_| right.as_ref().to_path_buf());
    normalize_path(&left) == normalize_path(&right)
}

fn normalize_path(path: &Path) -> String {
    let value = path.display().to_string();
    #[cfg(windows)]
    let value = value.replace('\\', "/");
    value.to_lowercase()
}

fn fetch_is_stale(modified: SystemTime) -> bool {
    SystemTime::now()
        .duration_since(modified)
        .map_or(true, |elapsed| elapsed > FETCH_TTL)
}

fn is_empty_dir(path: &Path) -> anyhow::Result<bool> {
    if !path.is_dir() {
        return Ok(false);
    }
    Ok(fs::read_dir(path)?.next().is_none())
}

fn parse_count(value: &str) -> Option<usize> {
    value.trim().parse().ok()
}

fn failed(message: String, fallback: &str) -> SyncResult {
    SyncResult {
        success: false,
        message: if message.trim().is_empty() {
            fallback.to_string()
        } else {
            message
        },
        updated: false,
        level: None,
    }
}

fn timeout_result() -> SyncResult {
    SyncResult {
        success: false,
        message: TIMEOUT_MESSAGE.to_string(),
        updated: false,
        level: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_sync_tool_pushes_and_git_clones_without_service_policy() {
        use crate::extension::memory::tools::{MemorySyncAction, MemorySyncArgs, MemorySyncTool};
        use rig::tool::Tool;
        let root = std::env::temp_dir().join(format!("phi-memory-sync-{}", uuid::Uuid::new_v4()));
        let memory = root.join("memory");
        let remote = root.join("remote.git");
        fs::create_dir_all(&memory).unwrap();
        assert_git(
            &root,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                remote.to_str().unwrap(),
            ],
        );
        assert_git(&memory, &["init", "--initial-branch=main"]);
        assert_git(&memory, &["config", "user.name", "Test"]);
        assert_git(&memory, &["config", "user.email", "test@localhost"]);
        assert_git(&memory, &["config", "push.default", "current"]);
        assert_git(
            &memory,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        fs::write(memory.join("note.md"), "content").unwrap();
        let tool = MemorySyncTool::new(
            memory.clone(),
            Some(remote.display().to_string()),
            Some(crate::permission::runtime::PermissionRuntime::open()),
        );
        tool.call(
            &mut rig::tool::ToolContext::new(),
            MemorySyncArgs {
                action: MemorySyncAction::Push,
            },
        )
        .await
        .unwrap();
        let clone = root.join("clone");
        let git = LocalMemoryGit::new(&clone, Some(remote.display().to_string()));
        assert!(git.pull().unwrap().success);
        assert_eq!(
            fs::read_to_string(clone.join("note.md")).unwrap(),
            "content"
        );
        assert_git(&clone, &["rev-parse", "HEAD"]);
        let invalid = LocalMemoryGit::new(
            root.join("invalid"),
            Some(root.join("missing.git").display().to_string()),
        );
        assert!(!invalid.pull().unwrap().success);
        fs::remove_dir_all(root).unwrap();
    }

    fn assert_git(directory: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn network_git_retries_are_bounded_and_stop_on_success() {
        for command in ["fetch", "clone", "push"] {
            for successful_attempt in [Some(1), Some(2), Some(3), None] {
                let mut attempts = 0;
                let mut delays = Vec::new();
                let result = retry_git_command(
                    &[command],
                    || {
                        attempts += 1;
                        GitResult {
                            success: successful_attempt == Some(attempts),
                            timeout: successful_attempt != Some(attempts),
                            ..Default::default()
                        }
                    },
                    |delay| delays.push(delay),
                );
                let expected_attempts = successful_attempt.unwrap_or(3);
                assert_eq!(attempts, expected_attempts);
                assert_eq!(delays.len(), expected_attempts - 1);
                assert_eq!(result.success, successful_attempt.is_some());
                assert!(delays.windows(2).all(|pair| pair[0] < pair[1]));
            }
        }
    }

    #[test]
    fn network_retry_stops_when_failure_becomes_permanent() {
        let mut results = std::collections::VecDeque::from([
            GitResult {
                timeout: true,
                ..Default::default()
            },
            GitResult {
                stdout: "Authentication failed".into(),
                ..Default::default()
            },
        ]);
        let mut delays = Vec::new();
        let result = retry_git_command(
            &["fetch"],
            || results.pop_front().expect("unexpected extra retry"),
            |delay| delays.push(delay),
        );
        assert_eq!(result.stdout, "Authentication failed");
        assert!(!result.success);
        assert_eq!(delays.len(), 1);
        assert!(results.is_empty());
    }

    #[test]
    fn only_transient_network_git_failures_are_retried() {
        for (command, message, timeout, expected_attempts) in [
            (
                "fetch",
                "fatal: unable to access remote: Connection reset by peer",
                false,
                3,
            ),
            (
                "clone",
                "ssh: Could not resolve hostname host: Temporary failure in name resolution",
                false,
                3,
            ),
            ("push", "fatal: Authentication failed", false, 1),
            ("push", "Permission denied (publickey)", false, 1),
            ("push", "rejected (non-fast-forward)", false, 1),
            ("fetch", "repository not found", false, 1),
            ("fetch", "unknown failure", false, 1),
            ("rebase", "CONFLICT (content): Merge conflict", false, 1),
            ("rebase", "", true, 1),
            ("commit", "", true, 1),
        ] {
            let mut attempts = 0;
            let result = retry_git_command(
                &[command],
                || {
                    attempts += 1;
                    GitResult {
                        stdout: message.into(),
                        timeout,
                        success: false,
                    }
                },
                |_| {},
            );
            assert_eq!(attempts, expected_attempts, "{command}: {message}");
            assert!(!result.success);
            assert_eq!(result.stdout, message);
        }
    }
}

fn format_commit_timestamp() -> String {
    Local::now().format("%Y-%m-%d-%H%M").to_string()
}
