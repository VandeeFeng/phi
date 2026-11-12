use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Coordinates every canonical Tape writer for one project.
#[derive(Debug)]
pub(crate) struct TapeProjectTransaction {
    /// The stable project Memory directory doubles as the advisory lock target.
    lock_directory: PathBuf,
    /// Canonical Anchor output replaced by a committed candidate.
    anchor_path: PathBuf,
}

/// Holds the project writer lock.
pub(crate) struct TapeProjectGuard<'a> {
    transaction: &'a TapeProjectTransaction,
    /// Keeps the OS lock alive through the caller's write.
    _lock: File,
}

impl TapeProjectTransaction {
    pub(crate) fn new(base: &Path, project_name: &str) -> Self {
        Self {
            lock_directory: base.to_path_buf(),
            anchor_path: base.join(format!("{project_name}__anchors.jsonl")),
        }
    }

    pub(crate) fn lock(&self) -> anyhow::Result<TapeProjectGuard<'_>> {
        fs::create_dir_all(&self.lock_directory)?;
        self.lock_existing()?.ok_or_else(|| {
            anyhow::anyhow!(
                "Tape directory disappeared: {}",
                self.lock_directory.display()
            )
        })
    }

    pub(crate) fn lock_existing(&self) -> anyhow::Result<Option<TapeProjectGuard<'_>>> {
        let lock = match File::open(&self.lock_directory) {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        lock.lock()?;
        Ok(Some(TapeProjectGuard {
            transaction: self,
            _lock: lock,
        }))
    }
}

impl TapeProjectGuard<'_> {
    pub(crate) fn write_anchor(&self, content: &str) -> anyhow::Result<()> {
        atomic_write(&self.transaction.anchor_path, content.as_bytes())
    }
}

fn atomic_write(path: &Path, content: &[u8]) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(content)?;
    file.sync_all()?;
    drop(file);
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    sync_parent(path)
}

fn sync_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}
