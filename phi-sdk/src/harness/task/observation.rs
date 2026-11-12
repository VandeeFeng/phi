use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{broadcast, watch};

use super::operation::TaskListAccess;
use super::store::TaskStore;

const COMMIT_BUFFER: usize = 256;

fn commits() -> &'static broadcast::Sender<PathBuf> {
    static COMMITS: OnceLock<broadcast::Sender<PathBuf>> = OnceLock::new();
    COMMITS.get_or_init(|| broadcast::channel(COMMIT_BUFFER).0)
}

pub(super) fn committed(directory: &Path) {
    let _ = commits().send(directory.to_owned());
}

/// Invalidations, not Task state or permission to mutate records.
pub struct TaskSubscription {
    _watcher: RecommendedWatcher,
    directory: PathBuf,
    commits: broadcast::Receiver<PathBuf>,
    changes: watch::Receiver<Option<String>>,
}

impl TaskListAccess {
    pub(crate) async fn subscribe(self) -> anyhow::Result<TaskSubscription> {
        tokio::task::spawn_blocking(move || {
            let binding = self.validate()?;
            let _store = TaskStore::lock(binding)?;
            let commits = commits().subscribe();
            let (changes, receiver) = watch::channel(None);
            let directory = binding.directory();
            let watched_directory = directory.clone();
            let mut watcher = notify::recommended_watcher(
                move |event: notify::Result<notify::Event>| match event {
                    Ok(event)
                        if matches!(
                            event.kind,
                            EventKind::Remove(_)
                                | EventKind::Modify(notify::event::ModifyKind::Name(_))
                        ) && event.paths.contains(&watched_directory) =>
                    {
                        changes.send_replace(Some(
                            "Task observation directory was removed or renamed".into(),
                        ));
                    }
                    Ok(event)
                        if !matches!(event.kind, EventKind::Access(_))
                            && (event.need_rescan()
                                || event.paths.iter().any(|path| relevant(path))) =>
                    {
                        changes.send_modify(|_| {});
                    }
                    Err(error) => {
                        changes.send_replace(Some(error.to_string()));
                    }
                    _ => {}
                },
            )?;
            watcher.watch(&directory, RecursiveMode::NonRecursive)?;
            self.validate()?;
            Ok(TaskSubscription {
                _watcher: watcher,
                directory,
                commits,
                changes: receiver,
            })
        })
        .await?
    }
}

impl TaskSubscription {
    /// Coalesces changes; consumers must reread canonical records after notification.
    pub async fn changed(&mut self) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                event = self.commits.recv() => match event {
                    Ok(directory) if directory == self.directory => return self.acknowledge(),
                    Err(broadcast::error::RecvError::Lagged(_)) => return self.acknowledge(),
                    Err(error) => return Err(error.into()),
                    _ => {}
                },
                event = self.changes.changed() => {
                    event?;
                    return self.acknowledge();
                }
            }
        }
    }

    fn acknowledge(&mut self) -> anyhow::Result<()> {
        for _ in 0..COMMIT_BUFFER {
            match self.commits.try_recv() {
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
                _ => break,
            }
        }
        match self.changes.borrow_and_update().clone() {
            Some(error) => Err(anyhow::anyhow!(error)),
            None => Ok(()),
        }
    }
}

fn relevant(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| {
            name != ".lock" && name != ".highwatermark" && !name.starts_with(".task-write-")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::TaskListBinding;

    #[test]
    fn commit_notifications_follow_record_writes() {
        let root = PathBuf::from("/tmp").join(format!("phi-task-commits-{}", uuid::Uuid::new_v4()));
        let binding = TaskListBinding::new(root.clone(), "shared").unwrap();
        let store = TaskStore::lock(&binding).unwrap();
        let task = serde_json::from_value(serde_json::json!({
            "id": "1", "subject": "inspect", "description": "verify commits", "status": "pending",
        }))
        .unwrap();
        let mut notifications = commits().subscribe();
        store.write(&task).unwrap();
        let mut committed = false;
        while let Ok(directory) = notifications.try_recv() {
            committed |= directory == binding.directory();
        }
        assert!(committed);
        std::fs::remove_file(binding.directory().join("1.json")).unwrap();
        std::fs::create_dir(binding.directory().join("1.json")).unwrap();
        assert!(store.write(&task).is_err());
        while let Ok(directory) = notifications.try_recv() {
            assert_ne!(directory, binding.directory());
        }
        drop(store);
        std::fs::remove_dir_all(root).unwrap();
    }
}
