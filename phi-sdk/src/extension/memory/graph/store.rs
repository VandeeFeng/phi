use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use super::index::MemoryGraphRecord;

pub struct MemoryGraphStore {
    file_path: PathBuf,
    write_lock: Mutex<()>,
}

impl MemoryGraphStore {
    pub fn new(memory_project_dir: impl Into<PathBuf>, project_name: &str) -> anyhow::Result<Self> {
        let memory_project_dir = memory_project_dir.into();
        Ok(Self {
            file_path: memory_project_dir.join(format!("{project_name}__graph.json")),
            write_lock: Mutex::new(()),
        })
    }

    pub fn fingerprint(&self) -> anyhow::Result<u64> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.file_path.exists().hash(&mut hasher);
        if self.file_path.exists() {
            fs::read(&self.file_path)?.hash(&mut hasher);
        }
        Ok(hasher.finish())
    }

    pub fn load(&self) -> anyhow::Result<MemoryGraphRecord> {
        let mut graph: MemoryGraphRecord =
            serde_json::from_str(&fs::read_to_string(&self.file_path)?)?;
        graph.normalize()?;
        Ok(graph)
    }

    pub fn save_if_changed(&self, graph: &MemoryGraphRecord) -> anyhow::Result<bool> {
        let _guard = self.lock()?;
        let mut graph = graph.clone();
        graph.normalize()?;
        let content = serde_json::to_string_pretty(&graph)?;
        fs::create_dir_all(self.file_path.parent().unwrap())?;
        let _file_guard = GraphFileLock::acquire(&self.file_path)?;
        if fs::read_to_string(&self.file_path).ok().as_deref() == Some(&content) {
            return Ok(false);
        }
        atomic_write(&self.file_path, content.as_bytes())?;
        Ok(true)
    }

    pub fn remove_if_unchanged(&self, expected_fingerprint: u64) -> anyhow::Result<bool> {
        let _guard = self.lock()?;
        if !self.file_path.try_exists()? {
            return Ok(false);
        }
        let _file_guard = GraphFileLock::acquire(&self.file_path)?;
        if self.fingerprint()? != expected_fingerprint {
            return Ok(false);
        }
        match fs::remove_file(&self.file_path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn lock(&self) -> anyhow::Result<std::sync::MutexGuard<'_, ()>> {
        self.write_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("memory graph store lock poisoned"))
    }
}

struct GraphFileLock {
    path: PathBuf,
}

impl GraphFileLock {
    fn acquire(graph_path: &Path) -> anyhow::Result<Self> {
        let path = graph_path.with_extension("json.lock");
        for _ in 0..200 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    remove_stale_lock(&path);
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!(
            "Timed out waiting for memory graph lock: {}",
            path.display()
        )
    }
}

impl Drop for GraphFileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn remove_stale_lock(path: &Path) {
    let stale = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > Duration::from_secs(30));
    if stale {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::graph::memory_graph_sources;

    #[test]
    fn directory_is_created_only_on_save() {
        let directory =
            std::env::temp_dir().join(format!("phi-graph-store-{}", uuid::Uuid::new_v4()));
        let store = MemoryGraphStore::new(&directory, "project").unwrap();
        assert!(!directory.exists());
        assert!(
            !store
                .remove_if_unchanged(store.fingerprint().unwrap())
                .unwrap()
        );
        assert!(!directory.exists());
        let graph = MemoryGraphRecord::new(memory_graph_sources("project"));
        assert!(store.save_if_changed(&graph).unwrap());
        assert!(directory.join("project__graph.json").exists());
        assert!(!store.save_if_changed(&graph).unwrap());
        fs::remove_dir_all(directory).unwrap();
    }
}

fn atomic_write(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, content)?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        error
    })?;
    Ok(())
}
