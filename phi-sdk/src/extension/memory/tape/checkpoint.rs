use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use compact_str::CompactString;

use super::anchor::{GitCheckpoint, TapeAnchor};
use crate::utils::workspace::git_root;

const MAX_REF_UPDATE_ATTEMPTS: usize = 8;

#[derive(Clone)]
pub(super) struct GitCheckpointStore {
    repository: PathBuf,
    reference: String,
}

impl GitCheckpointStore {
    pub(super) fn discover(cwd: &Path, session_id: &str) -> Option<Self> {
        Some(Self {
            repository: git_root(cwd)?,
            reference: format!("refs/phi/tape/s-{}", hex(session_id.as_bytes())),
        })
    }

    pub(super) fn capture(&self, anchor: &TapeAnchor) -> anyhow::Result<GitCheckpoint> {
        let index = TemporaryIndex::new();
        self.initialize_index(&index.path)?;
        self.git(&["add", "-A"], Some(&index.path))?;
        let tree_oid = self.git(&["write-tree"], Some(&index.path))?;
        let head_oid = self.optional_oid("HEAD");
        for _ in 0..MAX_REF_UPDATE_ATTEMPTS {
            let reference_oid = self.optional_oid(&self.reference);
            let parent_oid = reference_oid.as_ref().or(head_oid.as_ref());
            let commit_oid = self.commit_tree(&tree_oid, parent_oid.map(String::as_str), anchor)?;
            if self.update_reference(&commit_oid, reference_oid.as_deref())? {
                return Ok(GitCheckpoint {
                    head_oid: head_oid.map(CompactString::new),
                    commit_oid: CompactString::new(commit_oid),
                });
            }
        }
        anyhow::bail!("git checkpoint ref remained busy after {MAX_REF_UPDATE_ATTEMPTS} attempts")
    }

    fn initialize_index(&self, index: &Path) -> anyhow::Result<()> {
        let index_tree = self
            .git(&["write-tree"], None)
            .ok()
            .or_else(|| self.optional_oid("HEAD^{tree}"));
        match index_tree {
            Some(tree) => self.git(&["read-tree", &tree], Some(index)),
            None => self.git(&["read-tree", "--empty"], Some(index)),
        }
        .map(drop)
    }

    fn optional_oid(&self, revision: &str) -> Option<String> {
        self.git(&["rev-parse", "--verify", revision], None).ok()
    }

    fn commit_tree(
        &self,
        tree_oid: &str,
        parent_oid: Option<&str>,
        anchor: &TapeAnchor,
    ) -> anyhow::Result<String> {
        let mut args = vec!["commit-tree", tree_oid];
        if let Some(parent_oid) = parent_oid {
            args.extend(["-p", parent_oid]);
        }
        args.extend(["-m", anchor.name.as_str()]);
        let mut command = self.command(&args, None);
        command
            .env("GIT_AUTHOR_NAME", "Phi")
            .env("GIT_AUTHOR_EMAIL", "phi@localhost")
            .env("GIT_COMMITTER_NAME", "Phi")
            .env("GIT_COMMITTER_EMAIL", "phi@localhost");
        checked_output(command)
    }

    fn update_reference(&self, commit_oid: &str, expected: Option<&str>) -> anyhow::Result<bool> {
        let output = self
            .command(
                &[
                    "update-ref",
                    &self.reference,
                    commit_oid,
                    expected.unwrap_or(""),
                ],
                None,
            )
            .output()?;
        if output.status.success() {
            return Ok(true);
        }
        if self.optional_oid(&self.reference).as_deref() != expected {
            return Ok(false);
        }
        anyhow::bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }

    fn git(&self, args: &[&str], index: Option<&Path>) -> anyhow::Result<String> {
        checked_output(self.command(args, index))
    }

    fn command(&self, args: &[&str], index: Option<&Path>) -> Command {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.repository)
            .stdin(Stdio::null());
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        command
    }
}

struct TemporaryIndex {
    path: PathBuf,
}

impl TemporaryIndex {
    fn new() -> Self {
        Self {
            path: std::env::temp_dir().join(format!("phi-git-index-{}", uuid::Uuid::new_v4())),
        }
    }
}

impl Drop for TemporaryIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_file(self.path.with_extension("lock"));
    }
}

fn checked_output(mut command: Command) -> anyhow::Result<String> {
    let output = command.output()?;
    anyhow::ensure!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;

    bytes.iter().fold(String::new(), |mut encoded, byte| {
        let _ = write!(encoded, "{byte:02x}");
        encoded
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::tape::{TapeAnchor, TapeAnchorType};

    #[test]
    fn captures_anchor_worktree_without_mutating_user_state() {
        let repository = TestRepository::new();
        repository.write(".gitignore", "ignored*.txt\n");
        repository.write("tracked.txt", "base\n");
        repository.write("ignored-tracked.txt", "base\n");
        repository.git(&["add", ".gitignore", "tracked.txt"]);
        repository.git(&["add", "--force", "ignored-tracked.txt"]);
        repository.git(&["commit", "-m", "base"]);
        repository.write("tracked.txt", "changed\n");
        repository.write("ignored-tracked.txt", "changed\n");
        repository.write("new.txt", "new\n");
        repository.write("ignored.txt", "ignored\n");
        let status = repository.git(&["status", "--short"]);
        let store = GitCheckpointStore::discover(&repository.path, "session-1").unwrap();

        let first = store
            .capture(&TapeAnchor::new("first", TapeAnchorType::Normal))
            .unwrap();
        repository.write("new.txt", "newer\n");
        let second = store
            .capture(&TapeAnchor::new("second", TapeAnchorType::Normal))
            .unwrap();

        assert_eq!(repository.git(&["status", "--short"]), status);
        assert_eq!(
            first.head_oid.as_deref(),
            Some(repository.git(&["rev-parse", "HEAD"]).as_str())
        );
        assert_eq!(second.head_oid, first.head_oid);
        assert_eq!(
            repository.git(&[
                "diff",
                "--name-status",
                &first.commit_oid,
                &second.commit_oid
            ]),
            "M\tnew.txt"
        );
        let files = repository.git(&["ls-tree", "-r", "--name-only", &first.commit_oid]);
        assert!(files.lines().any(|path| path == "new.txt"));
        assert!(files.lines().any(|path| path == "ignored-tracked.txt"));
        assert!(!files.lines().any(|path| path == "ignored.txt"));
    }

    #[test]
    fn rejects_stale_checkpoint_parent() {
        let repository = TestRepository::new();
        repository.write("tracked.txt", "base\n");
        repository.git(&["add", "tracked.txt"]);
        repository.git(&["commit", "-m", "base"]);
        let store = GitCheckpointStore::discover(&repository.path, "session-1").unwrap();
        let first = store
            .capture(&TapeAnchor::new("first", TapeAnchorType::Normal))
            .unwrap();
        let second = store
            .capture(&TapeAnchor::new("second", TapeAnchorType::Normal))
            .unwrap();

        assert!(
            !store
                .update_reference(&first.commit_oid, Some(&first.commit_oid))
                .unwrap()
        );
        assert_eq!(
            repository.git(&["rev-parse", &store.reference]),
            second.commit_oid
        );
    }

    #[test]
    fn captures_repository_with_unborn_head() {
        let repository = TestRepository::new();
        repository.write("new.txt", "new\n");
        let store = GitCheckpointStore::discover(&repository.path, "session-1").unwrap();

        let checkpoint = store
            .capture(&TapeAnchor::new("first", TapeAnchorType::Normal))
            .unwrap();

        assert!(checkpoint.head_oid.is_none());
        assert_eq!(
            repository.git(&["ls-tree", "-r", "--name-only", &checkpoint.commit_oid]),
            "new.txt"
        );
    }

    struct TestRepository {
        path: PathBuf,
    }

    impl TestRepository {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("phi-git-checkpoint-test-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            let repository = Self { path };
            repository.git(&["init", "--quiet"]);
            repository.git(&["config", "user.name", "Phi Test"]);
            repository.git(&["config", "user.email", "phi-test@localhost"]);
            repository
        }

        fn write(&self, path: &str, content: &str) {
            fs::write(self.path.join(path), content).unwrap();
        }

        fn git(&self, args: &[&str]) -> String {
            let output = Command::new("git")
                .args(args)
                .current_dir(&self.path)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
