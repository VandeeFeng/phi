use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

const FALLBACK_INTERVAL: Duration = Duration::from_secs(2);
const WATCH_DEBOUNCE: Duration = Duration::from_millis(100);
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct WorkspaceView {
    pub(crate) working_dir: PathBuf,
    pub(crate) git_branch: Option<String>,
}

struct GitDirectories {
    repo_dir: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
}

fn git_directories(working_dir: &Path) -> Option<GitDirectories> {
    if !working_dir.is_dir() {
        return None;
    }
    for directory in working_dir.ancestors() {
        let dot_git = directory.join(".git");
        let git_dir = if dot_git.is_dir() {
            dot_git
        } else if dot_git.is_file() {
            let content = std::fs::read_to_string(dot_git).ok()?;
            directory.join(content.trim().strip_prefix("gitdir: ")?)
        } else {
            continue;
        };
        let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
            .map(|content| git_dir.join(content.trim()))
            .unwrap_or_else(|_| git_dir.clone());
        return Some(GitDirectories {
            repo_dir: directory.to_path_buf(),
            git_dir,
            common_dir,
        });
    }
    None
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
        let git_directories = git_directories(working_dir);
        let mut directories = Vec::new();
        // Ancestors expose repository creation/removal even from a nested working directory.
        for directory in working_dir.ancestors() {
            directories.push(directory.to_path_buf());
            if git_directories
                .as_ref()
                .is_some_and(|paths| paths.repo_dir == directory)
            {
                break;
            }
        }
        if let Some(paths) = &git_directories {
            for path in [&paths.git_dir, &paths.common_dir] {
                if !directories.contains(path) {
                    directories.push(path.clone());
                }
                let reftable = path.join("reftable");
                if reftable.is_dir() && !directories.contains(&reftable) {
                    directories.push(reftable);
                }
            }
        }
        let mut observation = match previous {
            Some(observation)
                if observation.view.working_dir == working_dir
                    && observation.directories == directories
                    && observation.healthy.load(Ordering::Acquire) =>
            {
                observation
            }
            _ => Self::watch(working_dir, directories),
        };
        // Clear before reading so concurrent switches remain pending for the next observation.
        observation.invalidated.store(false, Ordering::Release);
        observation.view.git_branch = git_branch(working_dir, git_directories.as_ref()).await;
        observation.observed_at = Instant::now();
        observation
    }

    fn watch(working_dir: &Path, directories: Vec<PathBuf>) -> Self {
        let invalidated = Arc::new(AtomicBool::new(false));
        let healthy = Arc::new(AtomicBool::new(true));
        let changed = invalidated.clone();
        let watch_healthy = healthy.clone();
        let watched_directories = directories.clone();
        let watcher =
            notify::recommended_watcher(
                move |result: notify::Result<notify::Event>| match result {
                    Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                        if event.need_rescan()
                            || event.paths.iter().any(|path| {
                                matches!(
                                    path.file_name().and_then(|name| name.to_str()),
                                    Some("HEAD" | ".git" | "reftable" | "tables.list")
                                ) || path
                                    .parent()
                                    .is_some_and(|parent| parent.ends_with("reftable"))
                                    || watched_directories.contains(path)
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
            healthy,
            _watcher: watcher,
            directories,
        }
    }
}

async fn git_branch(working_dir: &Path, directories: Option<&GitDirectories>) -> Option<String> {
    let mut git_branch = directories
        .and_then(|paths| std::fs::read_to_string(paths.git_dir.join("HEAD")).ok())
        .and_then(|head| {
            head.trim()
                .strip_prefix("ref: refs/heads/")
                .map(str::to_owned)
        });
    if git_branch.as_deref() == Some(".invalid") {
        let mut command = tokio::process::Command::new("git");
        command
            .args([
                "--no-optional-locks",
                "symbolic-ref",
                "--quiet",
                "--short",
                "HEAD",
            ])
            .current_dir(working_dir)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        git_branch = match tokio::time::timeout(GIT_TIMEOUT, command.output()).await {
            Ok(Ok(output)) if output.status.success() => String::from_utf8(output.stdout)
                .ok()
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty()),
            _ => None,
        };
    }
    git_branch
}
