use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub cwd: String,
    pub git_root: Option<String>,
    pub root: String,
    pub name: String,
    pub is_worktree: bool,
    pub main_root: Option<String>,
}

pub fn git_root(cwd: impl AsRef<Path>) -> Option<PathBuf> {
    exec_git_sync(cwd, &["rev-parse", "--show-toplevel"]).map(PathBuf::from)
}

pub fn project_scope_root(cwd: impl AsRef<Path>) -> PathBuf {
    git_root(&cwd).unwrap_or_else(|| cwd.as_ref().to_path_buf())
}

pub fn git_worktree_id(cwd: impl AsRef<Path>) -> Option<String> {
    let cwd = cwd.as_ref();
    let git_dir = PathBuf::from(exec_git_sync(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    )?);
    let common_dir = PathBuf::from(exec_git_sync(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?);
    if git_dir == common_dir {
        return Some("main".to_string());
    }
    git_dir
        .strip_prefix(common_dir)
        .ok()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
        .filter(|id| !id.is_empty())
}

pub fn get_project_meta(cwd: impl AsRef<Path>) -> ProjectMeta {
    let absolute_cwd = absolutize(cwd.as_ref());
    let git_root = exec_git_sync(&absolute_cwd, &["rev-parse", "--show-toplevel"]);
    let root = git_root
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| absolute_cwd.clone());
    let main_root = git_worktree_roots(&absolute_cwd)
        .ok()
        .and_then(|roots| roots.into_iter().next())
        .map(|path| path.to_string_lossy().into_owned());
    let root_string = root.to_string_lossy().to_string();
    let is_worktree = main_root.as_ref().is_some_and(|main| main != &root_string);

    ProjectMeta {
        cwd: absolute_cwd.to_string_lossy().to_string(),
        git_root,
        root: root_string,
        name: root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
            .to_string(),
        is_worktree,
        main_root: is_worktree.then_some(main_root).flatten(),
    }
}

pub(crate) fn git_worktree_roots(cwd: &Path) -> anyhow::Result<Vec<PathBuf>> {
    Ok(git_output(cwd, &["worktree", "list", "--porcelain", "-z"])?
        .split('\0')
        .filter_map(|field| field.strip_prefix("worktree ").map(PathBuf::from))
        .collect())
}

pub(crate) fn git_output(directory: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(directory)
        .stdin(Stdio::null())
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "git {} failed: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut stdout = String::from_utf8(output.stdout)?;
    if stdout.ends_with('\n') {
        stdout.pop();
    }
    Ok(stdout)
}

fn exec_git_sync(cwd: impl AsRef<Path>, args: &[&str]) -> Option<String> {
    git_output(cwd.as_ref(), args)
        .ok()
        .filter(|value| !value.is_empty())
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn identifies_main_and_linked_worktrees() {
        let repository =
            std::env::temp_dir().join(format!("phi workspace-test-{}", uuid::Uuid::new_v4()));
        let linked = repository.with_extension("linked");
        fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "--quiet"]);
        git(&repository, &["config", "user.name", "Phi Test"]);
        git(&repository, &["config", "user.email", "phi-test@localhost"]);
        fs::write(repository.join("tracked.txt"), "base\n").unwrap();
        git(&repository, &["add", "tracked.txt"]);
        git(&repository, &["commit", "--quiet", "-m", "base"]);
        git(
            &repository,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );

        let main = get_project_meta(&repository);
        assert!(!main.is_worktree);
        assert!(main.main_root.is_none());
        let child = get_project_meta(&linked);
        assert!(child.is_worktree);
        assert_eq!(child.main_root.as_deref(), repository.to_str());
        assert_eq!(git_worktree_id(&repository).as_deref(), Some("main"));
        let linked_id = git_worktree_id(&linked).unwrap();
        assert!(linked_id.starts_with("worktrees/"));
        assert!(!linked_id.contains(repository.to_string_lossy().as_ref()));
        fs::create_dir_all(linked.join("src")).unwrap();
        fs::create_dir_all(repository.join("src")).unwrap();
        assert_eq!(project_scope_root(linked.join("src")), linked);
        assert_eq!(project_scope_root(repository.join("src")), repository);

        let _ = fs::remove_dir_all(&linked);
        let _ = fs::remove_dir_all(&repository);
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
}
