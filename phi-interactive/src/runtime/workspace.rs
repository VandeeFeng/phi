use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

const OBSERVATION_INTERVAL: Duration = Duration::from_secs(2);
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkspaceView {
    pub(crate) working_dir: PathBuf,
    pub(crate) git_branch: Option<String>,
}

impl WorkspaceView {
    pub(super) async fn observe(working_dir: PathBuf) -> Self {
        let mut command = tokio::process::Command::new("git");
        command
            .args(["branch", "--show-current"])
            .current_dir(&working_dir)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let git_branch = match tokio::time::timeout(GIT_TIMEOUT, command.output()).await {
            Ok(Ok(output)) if output.status.success() => String::from_utf8(output.stdout)
                .ok()
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty()),
            _ => None,
        };
        Self {
            working_dir,
            git_branch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(8))]

        #[test]
        fn observations_never_cross_directories(
            selections in proptest::collection::vec(0usize..4, 1..24),
        ) {
            let startup_dir = std::env::current_dir().unwrap();
            let directory = PathBuf::from("/tmp")
                .join(format!("phi-workspace-{}", uuid::Uuid::new_v4()));
            let directories = [
                directory.join("first"),
                directory.join("second workspace"),
                directory.join("not-a-repository"),
                directory.join("missing"),
            ];
            for (working_dir, branch) in directories.iter().zip(["first-branch", "second-branch"]) {
                std::fs::create_dir_all(working_dir).unwrap();
                assert!(std::process::Command::new("git")
                    .args(["init", "--quiet", "--initial-branch", branch])
                    .arg(working_dir)
                    .status().unwrap().success());
            }
            std::fs::create_dir_all(&directories[2]).unwrap();
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().unwrap();
            let observations = executor.block_on(async {
                let mut cache = WorkspaceCache::default();
                let mut observations = Vec::new();
                for &selection in &selections {
                    observations.push(cache.observe(&directories[selection]).await);
                }
                observations
            });
            std::fs::remove_dir_all(directory).unwrap();
            for (selection, view) in selections.into_iter().zip(observations) {
                proptest::prop_assert_eq!(view.working_dir, directories[selection].clone());
                proptest::prop_assert_eq!(view.git_branch.as_deref(), match selection {
                    0 => Some("first-branch"),
                    1 => Some("second-branch"),
                    _ => None,
                });
            }
            proptest::prop_assert_eq!(std::env::current_dir().unwrap(), startup_dir);
        }
    }

    #[tokio::test]
    async fn cache_refreshes_branch_detachment_and_failures() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-workspace-cache-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let git = |arguments: &[&str]| {
            let output = std::process::Command::new("git")
                .args(arguments)
                .current_dir(&directory)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--quiet", "--initial-branch", "original"]);
        let mut cache = WorkspaceCache::default();
        assert_eq!(
            cache.observe(&directory).await.git_branch.as_deref(),
            Some("original")
        );
        git(&["symbolic-ref", "HEAD", "refs/heads/changed"]);
        assert_eq!(
            cache.observe(&directory).await.git_branch.as_deref(),
            Some("original")
        );
        cache.observation.as_mut().unwrap().0 = Instant::now() - OBSERVATION_INTERVAL;
        assert_eq!(
            cache.observe(&directory).await.git_branch.as_deref(),
            Some("changed")
        );
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "fixture",
        ]);
        git(&["checkout", "--quiet", "--detach"]);
        cache.observation.as_mut().unwrap().0 = Instant::now() - OBSERVATION_INTERVAL;
        assert_eq!(cache.observe(&directory).await.git_branch, None);
        std::fs::remove_dir_all(directory.join(".git")).unwrap();
        cache.observation.as_mut().unwrap().0 = Instant::now() - OBSERVATION_INTERVAL;
        assert_eq!(cache.observe(&directory).await.git_branch, None);
        git(&["init", "--quiet", "--initial-branch", "recovered"]);
        assert_eq!(cache.observe(&directory).await.git_branch, None);
        cache.observation.as_mut().unwrap().0 = Instant::now() - OBSERVATION_INTERVAL;
        assert_eq!(
            cache.observe(&directory).await.git_branch.as_deref(),
            Some("recovered")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[derive(Default)]
pub(super) struct WorkspaceCache {
    observation: Option<(Instant, WorkspaceView)>,
}

impl WorkspaceCache {
    pub(super) async fn observe(&mut self, working_dir: &Path) -> WorkspaceView {
        if let Some((observed_at, view)) = &self.observation
            && view.working_dir == working_dir
            && observed_at.elapsed() < OBSERVATION_INTERVAL
        {
            return view.clone();
        }
        let view = WorkspaceView::observe(working_dir.to_path_buf()).await;
        self.observation = Some((Instant::now(), view.clone()));
        view
    }
}
