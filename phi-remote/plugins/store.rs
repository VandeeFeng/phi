use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, ensure};
use serde::{Serialize, de::DeserializeOwned};

const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) struct Store {
    directory: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(directory: &Path) -> anyhow::Result<Self> {
        prepare_directory(directory)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(directory.join("bridge.lock"))?;
        check_private(&directory.join("bridge.lock"), FileKind::Regular)?;
        lock.try_lock()
            .context("another remote client owns this state directory")?;
        Ok(Self {
            directory: directory.to_owned(),
            _lock: lock,
        })
    }

    pub fn workspace(&self) -> anyhow::Result<PathBuf> {
        use std::os::unix::fs::DirBuilderExt;
        let path = self.directory.join("workspace");
        if !path.exists() {
            fs::DirBuilder::new().mode(0o700).create(&path)?;
        }
        check_private(&path, FileKind::Directory)?;
        path.canonicalize().map_err(Into::into)
    }

    pub fn load<T: DeserializeOwned>(&self, name: &str) -> anyhow::Result<Option<T>> {
        let path = self.directory.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => read_private(&path).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save<T: Serialize>(&self, name: &str, value: &T) -> anyhow::Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() as u64 <= MAX_FILE_BYTES,
            "client state exceeds size limit"
        );
        let target = self.directory.join(name);
        if fs::symlink_metadata(&target).is_ok() {
            check_private(&target, FileKind::Regular)?;
            let mut previous = Vec::new();
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&target)?
                .take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut previous)?;
            if previous == bytes {
                return Ok(());
            }
        }
        let temporary = self.directory.join(format!(".{name}.tmp"));
        // The process lock makes recovery of this client's temporary file exclusive.
        if fs::symlink_metadata(&temporary).is_ok() {
            check_private(&temporary, FileKind::Regular)?;
            fs::remove_file(&temporary)?;
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, target)?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}

pub(super) fn prepare_directory(directory: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    if !directory.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
    }
    check_private(directory, FileKind::Directory)
}

enum FileKind {
    Directory,
    Regular,
}

fn check_private(path: &Path, kind: FileKind) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let correct_kind = match kind {
        FileKind::Directory => metadata.is_dir(),
        FileKind::Regular => metadata.is_file() && metadata.nlink() == 1,
    };
    ensure!(
        correct_kind
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "{} must be owner-only, owned by the current user, and not a symlink or hard link",
        path.display()
    );
    Ok(())
}

fn read_private<T: DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    check_private(path, FileKind::Regular)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    ensure!(
        file.metadata()?.len() <= MAX_FILE_BYTES,
        "private file exceeds size limit"
    );
    serde_json::from_reader(file).context("invalid remote client state")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::client::protocol::Device;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn device_persists_and_is_locked() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(directory.path()).unwrap();
        assert!(Store::open(directory.path()).is_err());
        let device = Device::generate().unwrap();
        store.save("device.json", &device).unwrap();
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        let restored: Device = store.load("device.json").unwrap().unwrap();
        assert_eq!(restored.secret, device.secret);
        assert!(restored.credential.is_none());
    }

    #[test]
    fn unchanged_state_preserves_file_and_checks_permissions() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(directory.path()).unwrap();
        let state = serde_json::json!({"offset":42});
        let target = directory.path().join("state.json");
        store.save("state.json", &state).unwrap();
        let original = fs::metadata(&target).unwrap();
        store.save("state.json", &state).unwrap();
        let unchanged = fs::metadata(&target).unwrap();
        assert_eq!(original.ino(), unchanged.ino());
        assert_eq!(original.modified().unwrap(), unchanged.modified().unwrap());
        store
            .save("state.json", &serde_json::json!({"offset":43}))
            .unwrap();
        assert_eq!(
            store
                .load::<serde_json::Value>("state.json")
                .unwrap()
                .unwrap()["offset"],
            43
        );
        fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            store
                .save("state.json", &serde_json::json!({"offset":43}))
                .is_err()
        );
    }

    #[test]
    fn client_identities_stay_separate() {
        let parent = tempfile::tempdir_in("/tmp").unwrap();
        let telegram = Store::open(&parent.path().join("telegram")).unwrap();
        let feishu = Store::open(&parent.path().join("feishu")).unwrap();
        let first = Device::generate().unwrap();
        let second = Device::generate().unwrap();
        assert_ne!(first.secret, second.secret);
        telegram.save("device.json", &first).unwrap();
        feishu.save("device.json", &second).unwrap();
        telegram
            .save("state.json", &serde_json::json!({"offset":42}))
            .unwrap();
        assert!(
            feishu
                .load::<serde_json::Value>("state.json")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            telegram
                .load::<Device>("device.json")
                .unwrap()
                .unwrap()
                .secret,
            first.secret
        );
        assert_eq!(
            feishu
                .load::<Device>("device.json")
                .unwrap()
                .unwrap()
                .secret,
            second.secret
        );
        assert_ne!(telegram.workspace().unwrap(), feishu.workspace().unwrap());
    }

    #[test]
    fn rejects_public_files_and_links() {
        let directory = tempfile::tempdir_in("/tmp").unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(directory.path()).unwrap();
        store.save("state.json", &serde_json::json!({})).unwrap();
        let target = directory.path().join("state.json");
        std::os::unix::fs::symlink(&target, directory.path().join("link.json")).unwrap();
        assert!(store.load::<serde_json::Value>("link.json").is_err());
        fs::hard_link(&target, directory.path().join("hard.json")).unwrap();
        assert!(store.load::<serde_json::Value>("state.json").is_err());
        fs::remove_file(directory.path().join("hard.json")).unwrap();
        fs::set_permissions(target, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load::<serde_json::Value>("state.json").is_err());
    }
}
