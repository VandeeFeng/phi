use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::Context;

const GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceMeta {
    pub root: PathBuf,
    git_dir: Option<PathBuf>,
    common_dir: Option<PathBuf>,
}

impl WorkspaceMeta {
    pub fn discover(
        working_dir: impl AsRef<Path>,
        cancellation: Option<&tokio_util::sync::CancellationToken>,
    ) -> anyhow::Result<Self> {
        let working_dir = working_dir.as_ref();
        anyhow::ensure!(
            working_dir.is_dir(),
            "invalid working directory: {}",
            working_dir.display()
        );
        let output = crate::utils::process::run_sync(
            &mut git_command(working_dir, &["rev-parse", "--show-toplevel"]),
            GIT_TIMEOUT,
            cancellation,
        )
        .context("unable to discover Git workspace")?;
        if !output.status.success() {
            let error = String::from_utf8_lossy(&output.stderr);
            if error.starts_with("fatal: not a git repository (or any") {
                for directory in working_dir.ancestors() {
                    match std::fs::symlink_metadata(directory.join(".git")) {
                        Ok(_) => anyhow::bail!("Git workspace discovery failed: {}", error.trim()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                return Ok(Self {
                    root: if working_dir.is_absolute() {
                        working_dir.to_path_buf()
                    } else {
                        std::env::current_dir()?.join(working_dir)
                    },
                    git_dir: None,
                    common_dir: None,
                });
            }
            anyhow::bail!("Git workspace discovery failed: {}", error.trim());
        }
        let root = String::from_utf8(output.stdout)?;
        let output = crate::utils::process::run_sync(
            &mut git_command(working_dir, &["rev-parse", "--absolute-git-dir"]),
            GIT_TIMEOUT,
            cancellation,
        )?;
        anyhow::ensure!(
            output.status.success(),
            "Git directory query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let git_dir = String::from_utf8(output.stdout)?;
        let git_dir = std::fs::canonicalize(git_dir.strip_suffix('\n').unwrap_or(&git_dir))
            .context("invalid Git directory")?;
        let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
            Ok(path) => git_dir.join(path.strip_suffix('\n').unwrap_or(&path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => git_dir.clone(),
            Err(error) => return Err(error.into()),
        };
        Ok(Self {
            root: PathBuf::from(root.strip_suffix('\n').unwrap_or(&root)),
            git_dir: Some(git_dir),
            common_dir: Some(
                std::fs::canonicalize(common_dir).context("invalid shared Git directory")?,
            ),
        })
    }

    pub fn name(&self) -> &str {
        self.root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("project")
    }

    pub fn git_dir(&self) -> Option<&Path> {
        self.git_dir.as_deref()
    }

    pub fn common_dir(&self) -> Option<&Path> {
        self.common_dir.as_deref()
    }

    pub fn is_worktree(&self) -> bool {
        self.git_dir != self.common_dir
    }

    pub fn worktree_id(&self) -> Option<String> {
        let git_dir = self.git_dir()?;
        let common_dir = self.common_dir()?;
        if git_dir == common_dir {
            return Some("main".to_owned());
        }
        git_dir
            .strip_prefix(common_dir)
            .ok()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
            .filter(|id| !id.is_empty())
    }

    pub async fn git_branch(&self) -> anyhow::Result<Option<String>> {
        let Some(git_dir) = self.git_dir() else {
            return Ok(None);
        };
        let head = tokio::fs::read_to_string(git_dir.join("HEAD")).await?;
        let Some(branch) = head.trim().strip_prefix("ref: refs/heads/") else {
            return Ok(None);
        };
        if branch != ".invalid" {
            return Ok(Some(branch.to_owned()));
        }
        let mut command = tokio::process::Command::from(git_command(
            &self.root,
            &["symbolic-ref", "--quiet", "--short", "HEAD"],
        ));
        command.kill_on_drop(true);
        let output = tokio::time::timeout(GIT_TIMEOUT, command.output()).await??;
        if output.status.code() == Some(1) {
            return Ok(None);
        }
        anyhow::ensure!(
            output.status.success(),
            "Git branch query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned())
            .filter(|branch| !branch.is_empty()))
    }
}

pub(crate) fn git_command(directory: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(["--no-optional-locks"])
        .args(args)
        .current_dir(directory)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    // Internal operations target their explicit directory, never the launcher's repository/index.
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
    ] {
        command.env_remove(variable);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Repository(PathBuf);

    impl Repository {
        fn new() -> Self {
            let root =
                PathBuf::from("/tmp").join(format!("phi workspace-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn git(&self, directory: &Path, args: &[&str]) {
            let output = git_command(directory, args).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        fn init(&self) {
            self.git(&self.0, &["init", "--quiet", "--initial-branch=main"]);
            self.git(
                &self.0,
                &[
                    "-c",
                    "user.name=Phi Test",
                    "-c",
                    "user.email=phi-test@localhost",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "base",
                ],
            );
        }
    }

    impl Drop for Repository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn identifies_main_and_linked_worktrees() {
        let repository = Repository::new();
        repository.init();
        let linked = repository.0.join("linked");
        repository.git(
            &repository.0,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        let main = WorkspaceMeta::discover(&repository.0, None).unwrap();
        assert!(!main.is_worktree());
        assert_eq!(main.worktree_id().as_deref(), Some("main"));
        let child = WorkspaceMeta::discover(&linked, None).unwrap();
        assert!(child.is_worktree());
        assert_eq!(child.root, linked);
        assert_eq!(child.common_dir(), main.git_dir());
        assert!(child.worktree_id().unwrap().starts_with("worktrees/"));
        fs::create_dir_all(linked.join("src")).unwrap();
        assert_eq!(
            WorkspaceMeta::discover(linked.join("src"), None).unwrap(),
            child
        );
    }

    #[test]
    fn discovery_uses_requested_directory() {
        const TARGET_DIRECTORY: &str = "PHI_TEST_WORKSPACE_TARGET";
        if let Some(target) = std::env::var_os(TARGET_DIRECTORY) {
            let target = PathBuf::from(target);
            assert_eq!(WorkspaceMeta::discover(&target, None).unwrap().root, target);
            return;
        }
        let repository = Repository::new();
        let target = repository.0.join("target");
        let foreign = repository.0.join("foreign");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&foreign).unwrap();
        repository.git(&target, &["init", "--quiet"]);
        repository.git(&foreign, &["init", "--quiet"]);
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "utils::workspace::tests::discovery_uses_requested_directory",
                "--nocapture",
            ])
            .env(TARGET_DIRECTORY, &target)
            .env("GIT_DIR", foreign.join(".git"))
            .env("GIT_WORK_TREE", &foreign)
            .env("GIT_COMMON_DIR", foreign.join(".git"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn distinguishes_plain_directories_from_invalid_metadata() {
        let repository = Repository::new();
        let plain = WorkspaceMeta::discover(&repository.0, None).unwrap();
        assert_eq!(plain.root, repository.0);
        assert_eq!(plain.git_dir(), None);
        assert_eq!(plain.worktree_id(), None);
        fs::create_dir(repository.0.join(".git")).unwrap();
        assert!(WorkspaceMeta::discover(&repository.0, None).is_err());
        fs::remove_dir(repository.0.join(".git")).unwrap();
        fs::write(repository.0.join(".git"), "gitdir: missing\n").unwrap();
        assert!(WorkspaceMeta::discover(&repository.0, None).is_err());
        assert!(WorkspaceMeta::discover(repository.0.join("missing"), None).is_err());
    }

    #[test]
    fn separate_git_directory_handles_path_delimiters() {
        let repository = Repository::new();
        let checkout = repository.0.join("checkout\nwith spaces");
        let metadata = repository.0.join("metadata\nwith spaces");
        fs::create_dir_all(&checkout).unwrap();
        repository.git(
            &checkout,
            &[
                "init",
                "--quiet",
                "--separate-git-dir",
                metadata.to_str().unwrap(),
            ],
        );
        let workspace = WorkspaceMeta::discover(&checkout, None).unwrap();
        assert_eq!(workspace.root, checkout);
        assert_eq!(workspace.git_dir(), Some(metadata.as_path()));
        assert!(!workspace.is_worktree());
        assert_eq!(workspace.worktree_id().as_deref(), Some("main"));
        #[cfg(unix)]
        {
            let alias = repository.0.join("alias");
            std::os::unix::fs::symlink(&checkout, &alias).unwrap();
            assert_eq!(WorkspaceMeta::discover(&alias, None).unwrap(), workspace);
        }
    }

    #[test]
    fn submodule_is_its_own_workspace_not_a_linked_worktree() {
        let repository = Repository::new();
        repository.init();
        let parent = repository.0.join("parent");
        fs::create_dir_all(&parent).unwrap();
        repository.git(&parent, &["init", "--quiet"]);
        repository.git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--quiet",
                repository.0.to_str().unwrap(),
                "library",
            ],
        );
        let submodule = WorkspaceMeta::discover(parent.join("library"), None).unwrap();
        let parent = WorkspaceMeta::discover(&parent, None).unwrap();
        assert_eq!(submodule.root, parent.root.join("library"));
        assert_ne!(submodule.git_dir(), parent.git_dir());
        assert_eq!(submodule.git_dir(), submodule.common_dir());
        assert!(!submodule.is_worktree());
        assert_eq!(submodule.worktree_id().as_deref(), Some("main"));
    }
}
