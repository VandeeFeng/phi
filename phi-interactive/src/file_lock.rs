use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct FileLock {
    file: File,
    path: PathBuf,
    shared: bool,
}

impl FileLock {
    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        loop {
            let file = open_lock_file(path, true)?;
            file.lock()?;
            if matches_path(&file, path)? {
                return Ok(Self {
                    file,
                    path: path.to_path_buf(),
                    shared: false,
                });
            }
        }
    }

    pub(crate) fn try_acquire_shared(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        loop {
            let file = open_lock_file(path, true)?;
            let acquired = file.try_lock_shared();
            if !matches_path(&file, path)? {
                continue;
            }
            acquired?;
            return Ok(Self {
                file,
                path: path.to_path_buf(),
                shared: true,
            });
        }
    }

    pub(crate) fn try_acquire(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        loop {
            let file = open_lock_file(path, true)?;
            if let Some(lock) = Self::try_lock_current(file, path)? {
                return Ok(lock);
            }
        }
    }

    pub(crate) fn try_acquire_existing(path: &Path) -> io::Result<Option<Self>> {
        match open_lock_file(path, false) {
            Ok(file) => Self::try_lock_current(file, path),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn remove_file(&self) -> io::Result<()> {
        if matches_path(&self.file, &self.path)? {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }

    fn try_lock_current(file: File, path: &Path) -> io::Result<Option<Self>> {
        let acquired = file.try_lock();
        // An opener can retain an unlinked inode after its previous owner releases it.
        if !matches_path(&file, path)? {
            return Ok(None);
        }
        acquired?;
        Ok(Some(Self {
            file,
            path: path.to_path_buf(),
            shared: false,
        }))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if self.shared && (self.file.unlock().is_err() || self.file.try_lock().is_err()) {
            return;
        }
        // Unlink only while excluding other leases; openers validate file identity.
        if let Err(error) = self.remove_file() {
            tracing::warn!(%error, path = %self.path.display(), "failed to remove owned lock file");
        }
    }
}

fn open_lock_file(path: &Path, create: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(create);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn matches_path(file: &File, path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let current = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let opened = file.metadata()?;
    Ok(opened.dev() == current.dev() && opened.ino() == current.ino())
}

#[cfg(not(unix))]
fn matches_path(file: &File, path: &Path) -> io::Result<bool> {
    let opened = same_file::Handle::from_file(file.try_clone()?)?;
    match same_file::Handle::from_path(path) {
        Ok(current) => Ok(opened == current),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path() -> PathBuf {
        std::env::temp_dir().join(format!("phi-lock-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn shared_leases_exclude_deletion_until_the_last_release() {
        let path = test_path();
        let first = FileLock::try_acquire_shared(&path).unwrap();
        let second = FileLock::try_acquire_shared(&path).unwrap();
        assert!(FileLock::try_acquire(&path).is_err());
        drop(first);
        assert!(path.exists());
        assert!(FileLock::try_acquire(&path).is_err());
        drop(second);
        assert!(!path.exists());
        drop(FileLock::try_acquire(&path).unwrap());
    }

    #[tokio::test]
    async fn obsolete_process_opener_cannot_claim_or_remove_current_lock() {
        const CHILD_PATH: &str = "PHI_TEST_OBSOLETE_LOCK_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            let opened = open_lock_file(&path, false).unwrap();
            std::fs::write(path.with_extension("ready"), "ready").unwrap();
            while !path.with_extension("proceed").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(FileLock::try_lock_current(opened, &path).unwrap().is_none());
            assert_eq!(
                FileLock::try_acquire(&path).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            return;
        }
        let path = test_path();
        let owner = FileLock::try_acquire(&path).unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "file_lock::tests::obsolete_process_opener_cannot_claim_or_remove_current_lock",
                "--nocapture",
            ])
            .env(CHILD_PATH, &path)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !path.with_extension("ready").exists() {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "opener exited before readiness"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        drop(owner);
        let replacement = FileLock::try_acquire(&path).unwrap();
        std::fs::write(path.with_extension("proceed"), "proceed").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success()
        );
        assert!(path.exists());
        assert_eq!(
            FileLock::try_acquire(&path).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(replacement);
        assert!(!path.exists());
        std::fs::remove_file(path.with_extension("ready")).unwrap();
        std::fs::remove_file(path.with_extension("proceed")).unwrap();
    }

    #[test]
    fn stale_cleanup_preserves_active_and_replaced_ownership() {
        let path = test_path();
        let owner = FileLock::try_acquire(&path).unwrap();
        assert_eq!(
            FileLock::try_acquire_existing(&path).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        std::fs::remove_file(&path).unwrap();
        let replacement = FileLock::try_acquire(&path).unwrap();
        drop(owner);
        assert!(path.exists());
        assert_eq!(
            FileLock::try_acquire(&path).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(replacement);
        std::fs::write(&path, "").unwrap();
        drop(FileLock::try_acquire_existing(&path).unwrap().unwrap());
        assert!(!path.exists());
        assert!(FileLock::try_acquire_existing(&path).unwrap().is_none());
    }
}
