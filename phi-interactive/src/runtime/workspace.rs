use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use phi_sdk::workspace::WorkspaceMeta;

const FALLBACK_INTERVAL: Duration = Duration::from_secs(2);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkspaceView {
    pub(crate) working_dir: PathBuf,
    pub(crate) git_branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WorkspaceFixture(PathBuf);

    impl WorkspaceFixture {
        fn new() -> Self {
            let directory =
                PathBuf::from("/tmp").join(format!("phi-workspace-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&directory).unwrap();
            Self(directory)
        }

        fn git(&self, working_dir: &Path, arguments: &[&str]) -> std::process::Output {
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(arguments)
                .current_dir(working_dir)
                .output()
                .unwrap()
        }

        fn run_git(&self, working_dir: &Path, arguments: &[&str]) {
            let output = self.git(working_dir, arguments);
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    impl Drop for WorkspaceFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config::with_cases(8))]

        #[test]
        fn observations_never_cross_directories(
            selections in proptest::collection::vec(0usize..4, 1..24),
        ) {
            let startup_dir = std::env::current_dir().unwrap();
            let fixture = WorkspaceFixture::new();
            let directories = [
                fixture.0.join("first"),
                fixture.0.join("second workspace"),
                fixture.0.join("not-a-repository"),
                fixture.0.join("missing"),
            ];
            for (working_dir, branch) in directories.iter().zip(["first-branch", "second-branch"]) {
                std::fs::create_dir_all(working_dir).unwrap();
                fixture.run_git(working_dir, &["init", "--quiet", "--initial-branch", branch]);
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
    async fn nested_workspace_follows_repository_lifecycle() {
        let fixture = WorkspaceFixture::new();
        let directory = &fixture.0;
        let nested = directory.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let mut cache = WorkspaceCache::default();
        assert_eq!(cache.observe(&nested).await.git_branch, None);

        fixture.run_git(
            directory,
            &["init", "--quiet", "--initial-branch", "original"],
        );
        wait_for_branch(&mut cache, &nested, Some("original")).await;
        fixture.run_git(
            directory,
            &["commit", "--quiet", "--allow-empty", "-m", "fixture"],
        );
        fixture.run_git(directory, &["switch", "--quiet", "-c", "changed"]);
        wait_for_branch(&mut cache, &nested, Some("changed")).await;
        fixture.run_git(directory, &["branch", "-m", "renamed"]);
        wait_for_branch(&mut cache, &nested, Some("renamed")).await;
        fixture.run_git(directory, &["checkout", "--quiet", "--detach"]);
        wait_for_branch(&mut cache, &nested, None).await;
        fixture.run_git(directory, &["switch", "--quiet", "renamed"]);
        wait_for_branch(&mut cache, &nested, Some("renamed")).await;
        std::fs::remove_dir_all(directory.join(".git")).unwrap();
        wait_for_branch(&mut cache, &nested, None).await;
        fixture.run_git(
            directory,
            &["init", "--quiet", "--initial-branch", "recreated"],
        );
        wait_for_branch(&mut cache, &nested, Some("recreated")).await;
    }

    #[tokio::test]
    async fn worktree_branch_is_independent_of_parent_for_each_ref_format() {
        for format in ["files", "reftable"] {
            let fixture = WorkspaceFixture::new();
            let directory = &fixture.0;
            let initialized = fixture.git(
                directory,
                &[
                    "init",
                    "--quiet",
                    "--initial-branch=main",
                    &format!("--ref-format={format}"),
                ],
            );
            if !initialized.status.success() && format == "reftable" {
                assert!(
                    String::from_utf8_lossy(&initialized.stderr).contains("unknown"),
                    "{}",
                    String::from_utf8_lossy(&initialized.stderr)
                );
                eprintln!("skipping unsupported reftable format");
                continue;
            }
            assert!(initialized.status.success());
            fixture.run_git(
                directory,
                &["commit", "--quiet", "--allow-empty", "-m", "fixture"],
            );
            let worktree = directory.join("worktree");
            fixture.run_git(
                directory,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    "child",
                    worktree.to_str().unwrap(),
                ],
            );
            let nested = worktree.join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            let mut cache = WorkspaceCache::default();
            assert_eq!(
                cache.observe(&nested).await.git_branch.as_deref(),
                Some("child")
            );
            fixture.run_git(directory, &["branch", "-m", "parent-renamed"]);
            assert_eq!(
                cache.observe(&nested).await.git_branch.as_deref(),
                Some("child")
            );
            fixture.run_git(&worktree, &["switch", "--quiet", "-c", "changed"]);
            wait_for_branch(&mut cache, &nested, Some("changed")).await;
            fixture.run_git(&worktree, &["checkout", "--quiet", "--detach"]);
            wait_for_branch(&mut cache, &nested, None).await;
            let mut parent_cache = WorkspaceCache::default();
            assert_eq!(
                parent_cache.observe(directory).await.git_branch.as_deref(),
                Some("parent-renamed")
            );
        }
    }

    async fn wait_for_branch(cache: &mut WorkspaceCache, directory: &Path, branch: Option<&str>) {
        tokio::time::timeout(Duration::from_millis(1500), async {
            loop {
                let view = cache.observe(directory).await;
                assert_eq!(view.working_dir, directory);
                if view.git_branch.as_deref() == branch {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Git metadata changes should refresh without waiting for the polling fallback");
    }

    #[tokio::test]
    async fn watcher_failure_polls_then_recovers_live_updates() {
        let fixture = WorkspaceFixture::new();
        let directory = &fixture.0;
        fixture.run_git(
            directory,
            &["init", "--quiet", "--initial-branch", "original"],
        );
        let mut cache = WorkspaceCache::default();
        assert_eq!(
            cache.observe(directory).await.git_branch.as_deref(),
            Some("original")
        );

        // Inject an unavailable watcher; assertions below use only observable results.
        let observation = cache.observation.as_mut().unwrap();
        observation._watcher = None;
        observation.healthy.store(false, Ordering::Release);
        fixture.run_git(directory, &["symbolic-ref", "HEAD", "refs/heads/fallback"]);
        assert_eq!(
            cache.observe(directory).await.git_branch.as_deref(),
            Some("original")
        );
        tokio::time::sleep(FALLBACK_INTERVAL).await;
        assert_eq!(
            cache.observe(directory).await.git_branch.as_deref(),
            Some("fallback")
        );

        fixture.run_git(directory, &["symbolic-ref", "HEAD", "refs/heads/recovered"]);
        wait_for_branch(&mut cache, directory, Some("recovered")).await;
    }
}

#[derive(Default)]
pub(super) struct WorkspaceCache {
    observation: Option<WorkspaceObservation>,
}

struct WorkspaceObservation {
    view: WorkspaceView,
    observed_at: Instant,
    invalidated: Arc<AtomicBool>,
    topology_changed: Arc<AtomicBool>,
    workspace: Option<WorkspaceMeta>,
    healthy: Arc<AtomicBool>,
    _watcher: Option<RecommendedWatcher>,
    directories: Vec<PathBuf>,
}

impl WorkspaceCache {
    pub(super) async fn observe(&mut self, working_dir: &Path) -> WorkspaceView {
        if let Some(observation) = &self.observation
            && observation.view.working_dir == working_dir
        {
            let healthy = observation.healthy.load(Ordering::Acquire);
            let invalidated = observation.invalidated.load(Ordering::Acquire);
            let elapsed = observation.observed_at.elapsed();
            if (healthy && (!invalidated || elapsed < WATCH_DEBOUNCE))
                || (!healthy && elapsed < FALLBACK_INTERVAL)
            {
                return observation.view.clone();
            }
        }
        let observation = WorkspaceObservation::observe(working_dir, self.observation.take()).await;
        let view = observation.view.clone();
        self.observation = Some(observation);
        view
    }
}

impl WorkspaceObservation {
    async fn observe(working_dir: &Path, previous: Option<Self>) -> Self {
        let previous = previous.filter(|observation| observation.view.working_dir == working_dir);
        let rediscover = previous.as_ref().is_none_or(|observation| {
            !observation.healthy.load(Ordering::Acquire)
                || observation.topology_changed.swap(false, Ordering::AcqRel)
        });
        if let Some(observation) = &previous {
            observation.invalidated.store(false, Ordering::Release);
        }
        let workspace = if rediscover {
            let directory = working_dir.to_path_buf();
            tokio::task::spawn_blocking(move || WorkspaceMeta::discover(directory, None))
                .await
                .ok()
                .and_then(Result::ok)
        } else {
            previous
                .as_ref()
                .and_then(|observation| observation.workspace.clone())
        };
        let mut directories = Vec::new();
        // Ancestors expose repository creation/removal even from a nested working directory.
        for directory in working_dir.ancestors() {
            directories.push(directory.to_path_buf());
            if workspace.as_ref().is_some_and(|workspace| {
                workspace.git_dir().is_some() && workspace.root == directory
            }) {
                break;
            }
        }
        if let Some(workspace) = &workspace {
            for path in [workspace.git_dir(), workspace.common_dir()]
                .into_iter()
                .flatten()
            {
                if !directories.iter().any(|directory| directory == path) {
                    directories.push(path.to_path_buf());
                }
                let reftable = path.join("reftable");
                if reftable.is_dir() && !directories.contains(&reftable) {
                    directories.push(reftable);
                }
            }
        }
        let mut observation = match previous {
            Some(observation)
                if observation.directories == directories
                    && observation.healthy.load(Ordering::Acquire) =>
            {
                observation
            }
            _ => Self::watch(working_dir, directories),
        };
        observation.view.git_branch = match &workspace {
            Some(workspace) => match workspace.git_branch().await {
                Ok(branch) => branch,
                Err(_) => {
                    observation.healthy.store(false, Ordering::Release);
                    None
                }
            },
            None => {
                observation.healthy.store(false, Ordering::Release);
                None
            }
        };
        observation.workspace = workspace;
        observation.observed_at = Instant::now();
        observation
    }

    fn watch(working_dir: &Path, directories: Vec<PathBuf>) -> Self {
        let invalidated = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let topology_changed = Arc::new(AtomicBool::new(false));
        let topology = topology_changed.clone();
        let changed = invalidated.clone();
        let watch_healthy = healthy.clone();
        let watched_directories = directories.clone();
        let watcher =
            notify::recommended_watcher(
                move |result: notify::Result<notify::Event>| match result {
                    Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                        let layout_changed = event.need_rescan()
                            || event.paths.iter().any(|path| {
                                matches!(
                                    path.file_name().and_then(|name| name.to_str()),
                                    Some(".git" | "commondir" | "gitdir" | "config")
                                ) || watched_directories.contains(path)
                            });
                        if layout_changed {
                            topology.store(true, Ordering::Release);
                        }
                        if layout_changed
                            || event.paths.iter().any(|path| {
                                matches!(
                                    path.file_name().and_then(|name| name.to_str()),
                                    Some("HEAD" | "reftable" | "tables.list")
                                ) || path
                                    .parent()
                                    .is_some_and(|parent| parent.ends_with("reftable"))
                            })
                        {
                            changed.store(true, Ordering::Release);
                        }
                    }
                    Err(_) => {
                        watch_healthy.store(false, Ordering::Release);
                        changed.store(true, Ordering::Release);
                    }
                    _ => {}
                },
            )
            .ok()
            .map(|mut watcher| {
                for directory in &directories {
                    if watcher
                        .watch(directory, RecursiveMode::NonRecursive)
                        .is_err()
                    {
                        healthy.store(false, Ordering::Release);
                    }
                }
                watcher
            });
        if watcher.is_none() {
            healthy.store(false, Ordering::Release);
        }
        Self {
            view: WorkspaceView {
                working_dir: working_dir.to_path_buf(),
                git_branch: None,
            },
            observed_at: Instant::now(),
            invalidated,
            topology_changed,
            workspace: None,
            healthy,
            _watcher: watcher,
            directories,
        }
    }
}
