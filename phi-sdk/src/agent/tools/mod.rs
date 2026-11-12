mod agent;
mod bash;
mod diff;
mod edit;
mod glob;
mod grep;
pub(crate) mod lua;
mod read;
pub(crate) mod task;
mod write;

pub use agent::AgentTool;
pub(crate) use agent::{
    AgentArgs, AgentManagementArgs, AgentToolOutput, AgentToolRequest, AgentToolSender,
};
pub use bash::BashTool;
pub(crate) use bash::run_managed_bash;
pub use edit::EditTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use read::ReadTool;
pub use write::WriteTool;

use std::{
    collections::HashMap,
    future::Future,
    io,
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
};

use tokio::sync::Mutex;

use serde::Deserialize;

use crate::permission::runtime::PermissionRuntime;
pub const MAX_GREP_RESULTS: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Msg(String),
}

impl From<io::Error> for ToolError {
    fn from(e: io::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

impl From<serde_json::Error> for ToolError {
    fn from(e: serde_json::Error) -> Self {
        ToolError::Msg(e.to_string())
    }
}

pub fn is_skip_dir(name: &str) -> bool {
    matches!(name, "node_modules" | "target")
}

static FILE_MUTATION_QUEUES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

pub async fn with_file_mutation_queue<T, F>(path: impl Into<PathBuf>, f: F) -> T
where
    F: Future<Output = T>,
{
    let requested_path = path.into();
    let key = std::fs::canonicalize(&requested_path).unwrap_or(requested_path);
    let queues = FILE_MUTATION_QUEUES.get_or_init(|| Mutex::new(HashMap::new()));
    let queue = {
        let mut queues = queues.lock().await;
        queues.entry(key).or_default().clone()
    };
    let _guard = queue.lock().await;
    f.await
}

#[derive(Deserialize)]
pub struct ReadArgs {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Clone, Deserialize)]
pub struct WriteArgs {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Deserialize)]
pub struct EditArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    pub replace_all: Option<bool>,
}

#[derive(Deserialize)]
pub struct BashArgs {
    pub command: String,
    pub timeout: Option<u64>,
    pub run_in_background: Option<bool>,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum GrepOutputMode {
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Deserialize)]
pub struct GrepArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub include: Option<String>,
    pub context_lines: Option<usize>,
    pub output_mode: Option<GrepOutputMode>,
    pub head_limit: Option<usize>,
    pub offset: Option<usize>,
    pub file_type: Option<String>,
    pub case_insensitive: Option<bool>,
    pub multiline: Option<bool>,
}

#[derive(Deserialize)]
pub struct GlobArgs {
    pub pattern: String,
    pub path: Option<String>,
    pub limit: Option<usize>,
}

pub(super) fn resolve_tool_path(working_dir: &Path, path: Option<&str>) -> PathBuf {
    let path = path.filter(|path| !path.trim().is_empty()).unwrap_or(".");
    let requested = working_dir.join(crate::utils::path::expand_home_path(path));
    let (mut resolved, suffix) = requested
        .ancestors()
        .find_map(|ancestor| {
            ancestor
                .canonicalize()
                .ok()
                .map(|base| (base, requested.strip_prefix(ancestor).unwrap()))
        })
        .unwrap_or_else(|| (PathBuf::new(), requested.as_path()));
    for component in suffix.components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            component => resolved.push(component.as_os_str()),
        }
    }
    resolved
}

pub async fn check_bash_perm(
    permission: &Option<PermissionRuntime>,
    command: &str,
) -> Result<(), ToolError> {
    let Some(permission) = permission else {
        return Ok(());
    };
    let original = crate::core::interaction::active_original_bash_command();
    permission
        .authorize_bash(command, original.as_deref())
        .await
        .map_err(to_tool_error)
}

pub async fn check_perm_path(
    permission: &Option<PermissionRuntime>,
    tool: &str,
    path: &str,
) -> Result<(), ToolError> {
    let Some(permission) = permission else {
        return Ok(());
    };
    permission
        .authorize_path(tool, path, None)
        .await
        .map_err(to_tool_error)
}

pub async fn check_perm_path_with_preview<F, Fut>(
    permission: &Option<PermissionRuntime>,
    tool: &str,
    path: &str,
    preview: F,
) -> Result<(), ToolError>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<String>, ToolError>>,
{
    let Some(permission) = permission else {
        return Ok(());
    };
    permission
        .authorize_path_with_preview(tool, path, None, || async {
            preview().await.map_err(anyhow::Error::new)
        })
        .await
        .map_err(to_tool_error)
}

fn to_tool_error(error: anyhow::Error) -> ToolError {
    ToolError::Msg(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_tools_share_injected_write_protection_without_permission_rules() {
        use rig::tool::Tool;
        let root = std::env::temp_dir().join(format!("phi-file-access-{}", uuid::Uuid::new_v4()));
        let protected = root.join(".phi");
        let memory = protected.join("memory");
        std::fs::create_dir_all(&memory).unwrap();
        let settings = protected.join("settings.json");
        std::fs::write(&settings, "original").unwrap();
        let sandbox = crate::sandbox::Sandbox::new(false, &root)
            .unwrap()
            .with_write_protection(vec![protected], vec![memory.clone()])
            .unwrap();
        let file_access = Arc::new(sandbox);
        let write = WriteTool::new(None, root.clone(), file_access.clone());
        let edit = EditTool::new(None, root.clone(), file_access);
        let mut context = rig::tool::ToolContext::new();
        let error = write
            .call(
                &mut context,
                WriteArgs {
                    path: ".phi/settings.json".into(),
                    content: "changed".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read-only"));
        let error = edit
            .call(
                &mut context,
                EditArgs {
                    path: ".phi/settings.json".into(),
                    old_text: "original".into(),
                    new_text: "changed".into(),
                    replace_all: None,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("read-only"));
        assert_eq!(std::fs::read_to_string(settings).unwrap(), "original");
        write
            .call(
                &mut context,
                WriteArgs {
                    path: ".phi/memory/new/note.md".into(),
                    content: "original".into(),
                },
            )
            .await
            .unwrap();
        edit.call(
            &mut context,
            EditArgs {
                path: ".phi/memory/new/note.md".into(),
                old_text: "original".into(),
                new_text: "changed".into(),
                replace_all: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(memory.join("new/note.md")).unwrap(),
            "changed"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn file_mutations_queue_per_path_without_blocking_reads() {
        use rig::tool::Tool;
        use std::task::Poll;

        let root = Path::new("/tmp").join(format!("phi-file-queue-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("shared.txt");
        std::fs::write(&path, "original").unwrap();
        let sandbox = Arc::new(crate::sandbox::Sandbox::new(false, &root).unwrap());
        let write = WriteTool::new(None, root.clone(), sandbox.clone());
        let edit = EditTool::new(None, root.clone(), sandbox);
        let read = ReadTool::new(None, root.clone());
        let (release, released) = tokio::sync::oneshot::channel();
        let held = with_file_mutation_queue(path, async { released.await.unwrap() });
        tokio::pin!(held);
        assert!(matches!(futures::poll!(&mut held), Poll::Pending));

        let mut write_context = rig::tool::ToolContext::new();
        let mut edit_context = rig::tool::ToolContext::new();
        let writing = write.call(
            &mut write_context,
            WriteArgs {
                path: "shared.txt".into(),
                content: "written".into(),
            },
        );
        let editing = edit.call(
            &mut edit_context,
            EditArgs {
                path: "./shared.txt".into(),
                old_text: "written".into(),
                new_text: "edited".into(),
                replace_all: None,
            },
        );
        tokio::pin!(writing, editing);
        assert!(matches!(futures::poll!(&mut writing), Poll::Pending));
        assert!(matches!(futures::poll!(&mut editing), Poll::Pending));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let content = read
                .call(
                    &mut rig::tool::ToolContext::new(),
                    ReadArgs {
                        path: "shared.txt".into(),
                        offset: None,
                        limit: None,
                    },
                )
                .await
                .unwrap();
            assert!(content.contains("original"), "{content}");
            write
                .call(
                    &mut rig::tool::ToolContext::new(),
                    WriteArgs {
                        path: "independent.txt".into(),
                        content: "independent".into(),
                    },
                )
                .await
                .unwrap();
        })
        .await
        .expect("unrelated file operations must not wait for the mutation queue");

        release.send(()).unwrap();
        held.await;
        writing.await.unwrap();
        editing.await.unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("shared.txt")).unwrap(),
            "edited"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn scoped_paths_resolve_defaults_home_and_external_targets() {
        use rig::tool::Tool;
        let root = std::env::temp_dir().join(format!("phi-tool-path-{}", uuid::Uuid::new_v4()));
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let child = child.canonicalize().unwrap();
        let outside = child.parent().unwrap().join("outside.txt");
        std::fs::write(&outside, "outside").unwrap();
        assert_eq!(resolve_tool_path(&child, None), child);
        assert_eq!(resolve_tool_path(&child, Some("")), child);
        assert_eq!(resolve_tool_path(&child, Some("../outside.txt")), outside);
        assert_eq!(
            resolve_tool_path(&child, Some("new/../../outside.txt")),
            outside
        );
        let mut permission = crate::permission::checker::PermissionChecker::new(
            &crate::permission::PermissionConfig::default(),
            Some(child.clone()),
        );
        assert!(matches!(
            permission.check_path("read", outside.to_str().unwrap()),
            crate::permission::checker::CheckResult::Ask
        ));
        assert_eq!(
            resolve_tool_path(&child, Some(outside.to_str().unwrap())),
            outside
        );
        assert_eq!(
            resolve_tool_path(&child, Some("new/deep/../file.txt")),
            child.join("new/file.txt")
        );
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                resolve_tool_path(&child, Some("~")),
                home.canonicalize().unwrap()
            );
        }
        let runtime = PermissionRuntime::new(Arc::new(std::sync::Mutex::new(permission)));
        let glob = GlobTool::new(Some(runtime), child.clone());
        let mut patterns = vec![child.join("../outside.txt")];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, child.join("link.txt")).unwrap();
            assert_eq!(resolve_tool_path(&child, Some("link.txt")), outside);
            std::os::unix::fs::symlink(&root, child.join("external")).unwrap();
            patterns.push(child.join("external/outside.txt"));
        }
        for pattern in patterns {
            let result = glob
                .call(
                    &mut rig::tool::ToolContext::new(),
                    GlobArgs {
                        pattern: pattern.to_string_lossy().into_owned(),
                        path: None,
                        limit: None,
                    },
                )
                .await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("Permission denied"),
                "external Glob target must require authorization"
            );
        }
        let config =
            crate::permission::PermissionConfig::try_from_value(serde_json::json!({"glob":"deny"}))
                .unwrap();
        let checker =
            crate::permission::checker::PermissionChecker::new(&config, Some(child.clone()));
        let runtime = PermissionRuntime::new(Arc::new(std::sync::Mutex::new(checker)));
        let glob = GlobTool::new(Some(runtime), child);
        let result = glob
            .call(
                &mut rig::tool::ToolContext::new(),
                GlobArgs {
                    pattern: "*".into(),
                    path: None,
                    limit: None,
                },
            )
            .await;
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Permission denied")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
