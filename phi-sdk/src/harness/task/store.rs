use super::model::{Task, TaskId, TaskListBinding};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) const MAX_TASKS: usize = 4096;
const MAX_RECORD_BYTES: u64 = 128 * 1024;
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = MAX_TASKS + 64;
const LOCK_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_RETRY: Duration = Duration::from_millis(10);
const TEMP_PREFIX: &str = ".task-write-";
const HIGH_WATER_FILE: &str = ".highwatermark";
const MAX_WATERMARK_BYTES: u64 = 20;

#[derive(Debug, thiserror::Error)]
pub(crate) enum TaskStoreError {
    #[error("task file operation failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid task storage at {path}: {reason}")]
    Invalid { path: PathBuf, reason: String },
    #[error("task list lock contention exceeded its deadline")]
    LockTimeout,
    #[error("task list exceeds its {0} bound")]
    Limit(&'static str),
    #[error("task ID space is exhausted")]
    IdExhausted,
}

fn io_error(path: &Path, source: std::io::Error) -> TaskStoreError {
    TaskStoreError::Io {
        path: path.to_owned(),
        source,
    }
}

fn invalid(path: &Path, reason: impl Into<String>) -> TaskStoreError {
    TaskStoreError::Invalid {
        path: path.to_owned(),
        reason: reason.into(),
    }
}

pub(super) struct TaskStore {
    directory: PathBuf,
    _lock: File,
}

impl TaskStore {
    pub(super) fn lock(binding: &TaskListBinding) -> Result<Self, TaskStoreError> {
        let directory = binding.directory();
        create_directory(&directory)?;
        let path = directory.join(".lock");
        let lock = open_file(
            &path,
            OpenOptions::new().read(true).write(true).create(true),
        )?;
        let started = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    if started.elapsed() >= LOCK_TIMEOUT {
                        return Err(TaskStoreError::LockTimeout);
                    }
                    std::thread::sleep(LOCK_RETRY);
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(io_error(&path, error)),
            }
        }
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    pub(super) fn read_all(&self) -> Result<Vec<Task>, TaskStoreError> {
        let mut records = Vec::new();
        let mut total_bytes = 0;
        for (index, entry) in fs::read_dir(&self.directory)
            .map_err(|error| io_error(&self.directory, error))?
            .enumerate()
        {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(TaskStoreError::Limit("directory entry count"));
            }
            let entry = entry.map_err(|error| io_error(&self.directory, error))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid(&path, "non-UTF-8 filename"))?;
            if name == ".lock" || name == HIGH_WATER_FILE || name.starts_with(TEMP_PREFIX) {
                continue;
            }
            let id = name
                .strip_suffix(".json")
                .and_then(|text| TaskId::try_from(text.to_owned()).ok())
                .ok_or_else(|| invalid(&path, "expected a numeric task JSON filename"))?;
            if records.len() >= MAX_TASKS {
                return Err(TaskStoreError::Limit("record count"));
            }
            let bytes = read_bounded(&path, MAX_RECORD_BYTES)?;
            total_bytes += bytes.len();
            if total_bytes > MAX_LIST_BYTES {
                return Err(TaskStoreError::Limit("total record bytes"));
            }
            let task: Task = serde_json::from_slice(&bytes)
                .map_err(|error| invalid(&path, error.to_string()))?;
            task.validate()
                .map_err(|error| invalid(&path, error.to_string()))?;
            if task.id != id {
                return Err(invalid(&path, "filename and record ID differ"));
            }
            records.push(task);
        }
        Ok(records)
    }

    pub(super) fn next_id<'a>(
        &self,
        records: impl ExactSizeIterator<Item = &'a Task>,
    ) -> Result<TaskId, TaskStoreError> {
        if records.len() >= MAX_TASKS {
            return Err(TaskStoreError::Limit("record count"));
        }
        let highest = records
            .map(|task| task.id.as_str().parse::<u64>().expect("validated task ID"))
            .max()
            .unwrap_or(0);
        let next = highest
            .max(self.read_watermark()?)
            .checked_add(1)
            .ok_or(TaskStoreError::IdExhausted)?;
        Ok(TaskId::try_from(next.to_string()).expect("positive allocated ID"))
    }

    pub(super) fn validate_size<'a>(
        records: impl IntoIterator<Item = &'a Task>,
    ) -> Result<(), TaskStoreError> {
        let mut total = 0;
        let mut count = 0;
        for task in records {
            count += 1;
            let size = serde_json::to_vec(task)
                .map_err(|error| invalid(Path::new(task.id.as_str()), error.to_string()))?
                .len();
            if size as u64 > MAX_RECORD_BYTES {
                return Err(TaskStoreError::Limit("record bytes"));
            }
            total += size;
            if count > MAX_TASKS || total > MAX_LIST_BYTES {
                return Err(TaskStoreError::Limit("list size"));
            }
        }
        Ok(())
    }

    pub(super) fn write(&self, task: &Task) -> Result<(), TaskStoreError> {
        let path = self.record_path(&task.id);
        let bytes = serde_json::to_vec(task).map_err(|error| invalid(&path, error.to_string()))?;
        if bytes.len() as u64 > MAX_RECORD_BYTES {
            return Err(TaskStoreError::Limit("record bytes"));
        }
        self.replace(&path, &bytes)
    }

    pub(super) fn delete(&self, id: &TaskId) -> Result<(), TaskStoreError> {
        let path = self.record_path(id);
        reject_non_file(&path)?;
        let number = id.as_str().parse::<u64>().expect("validated task ID");
        if number > self.read_watermark()? {
            self.replace(
                &self.directory.join(HIGH_WATER_FILE),
                id.as_str().as_bytes(),
            )?;
        }
        fs::remove_file(&path).map_err(|error| io_error(&path, error))?;
        super::observation::committed(&self.directory);
        Ok(())
    }

    fn record_path(&self, id: &TaskId) -> PathBuf {
        self.directory.join(format!("{}.json", id.as_str()))
    }

    fn read_watermark(&self) -> Result<u64, TaskStoreError> {
        let path = self.directory.join(HIGH_WATER_FILE);
        let bytes = match read_bounded(&path, MAX_WATERMARK_BYTES) {
            Ok(bytes) => bytes,
            Err(TaskStoreError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(0);
            }
            Err(error) => return Err(error),
        };
        let text =
            std::str::from_utf8(&bytes).map_err(|error| invalid(&path, error.to_string()))?;
        let value = text
            .parse::<u64>()
            .map_err(|error| invalid(&path, error.to_string()))?;
        if value.to_string() != text {
            return Err(invalid(&path, "invalid allocation watermark"));
        }
        Ok(value)
    }

    fn replace(&self, path: &Path, bytes: &[u8]) -> Result<(), TaskStoreError> {
        match reject_non_file(path) {
            Ok(()) => {}
            Err(TaskStoreError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let temporary = self
            .directory
            .join(format!("{TEMP_PREFIX}{}", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = open_file(&temporary, OpenOptions::new().write(true).create_new(true))?;
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| io_error(&temporary, error))?;
            fs::rename(&temporary, path).map_err(|error| io_error(path, error))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            super::observation::committed(&self.directory);
        }
        result
    }
}

fn create_directory(path: &Path) -> Result<(), TaskStoreError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component);
        match fs::create_dir(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(&current, error)),
        }
        if !fs::symlink_metadata(&current)
            .map_err(|error| io_error(&current, error))?
            .file_type()
            .is_dir()
        {
            return Err(invalid(&current, "expected a directory, not a symlink"));
        }
    }
    Ok(())
}

fn reject_non_file(path: &Path) -> Result<(), TaskStoreError> {
    if !fs::symlink_metadata(path)
        .map_err(|error| io_error(path, error))?
        .file_type()
        .is_file()
    {
        return Err(invalid(path, "expected a regular file, not a symlink"));
    }
    Ok(())
}

fn open_file(path: &Path, options: &mut OpenOptions) -> Result<File, TaskStoreError> {
    match reject_non_file(path) {
        Ok(()) => {}
        Err(TaskStoreError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
        }
        Err(error) => return Err(error),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|error| io_error(path, error))?;
    if !file
        .metadata()
        .map_err(|error| io_error(path, error))?
        .is_file()
    {
        return Err(invalid(path, "expected a regular file"));
    }
    Ok(file)
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, TaskStoreError> {
    let file = open_file(path, OpenOptions::new().read(true))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(path, error))?;
    if bytes.len() as u64 > limit {
        return Err(invalid(path, "file exceeds its byte limit"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::super::model::TaskStatus;
    use super::*;
    use serde_json::Map;

    struct TestStore {
        binding: TaskListBinding,
        root: PathBuf,
    }

    impl TestStore {
        fn new() -> Self {
            let root = Path::new("/tmp").join(format!("phi-task-store-{}", uuid::Uuid::new_v4()));
            Self {
                binding: TaskListBinding::new(&root, "shared").unwrap(),
                root,
            }
        }
    }

    impl Drop for TestStore {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn record(id: TaskId) -> Task {
        Task {
            id,
            subject: "Review".to_owned(),
            description: "Review changes".to_owned(),
            active_form: None,
            status: TaskStatus::Pending,
            owner: None,
            blocked_by: Vec::new(),
            metadata: Map::new(),
        }
    }

    #[test]
    fn allocator_preserves_ids() {
        let fixture = TestStore::new();
        let store = TaskStore::lock(&fixture.binding).unwrap();
        let watermark = fixture.binding.directory().join(HIGH_WATER_FILE);
        let mut records = Vec::new();
        for expected in 1..=16 {
            let id = store.next_id(records.iter()).unwrap();
            assert_eq!(id.as_str(), expected.to_string());
            assert_eq!(store.next_id(records.iter()).unwrap(), id);
            store.write(&record(id)).unwrap();
            records = store.read_all().unwrap();
            assert!(!watermark.exists());
        }
        let newest = TaskId::try_from("16".to_owned()).unwrap();
        store.delete(&newest).unwrap();
        assert_eq!(fs::read_to_string(&watermark).unwrap(), "16");
        #[cfg(unix)]
        let marker_inode = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&watermark).unwrap().ino()
        };
        for record in records.iter().filter(|task| task.id != newest) {
            store.delete(&record.id).unwrap();
            assert_eq!(fs::read_to_string(&watermark).unwrap(), "16");
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                assert_eq!(fs::metadata(&watermark).unwrap().ino(), marker_inode);
            }
        }
        assert!(store.read_all().unwrap().is_empty());
        let next = record(store.next_id([].iter()).unwrap());
        assert_eq!(next.id.as_str(), "17");
        store.write(&next).unwrap();
        assert_eq!(fs::read_to_string(&watermark).unwrap(), "16");
        assert_eq!(store.next_id([&next].into_iter()).unwrap().as_str(), "18");
    }

    #[test]
    fn watermark_errors_keep_records() {
        let fixture = TestStore::new();
        let store = TaskStore::lock(&fixture.binding).unwrap();
        let first = record(TaskId::try_from("1".to_owned()).unwrap());
        store.write(&first).unwrap();
        let watermark = fixture.binding.directory().join(HIGH_WATER_FILE);
        for contents in ["01", "invalid", "18446744073709551616"] {
            fs::write(&watermark, contents).unwrap();
            assert!(store.next_id([&first].into_iter()).is_err());
            assert!(store.delete(&first.id).is_err());
            assert_eq!(store.read_all().unwrap(), [first.clone()]);
        }
        fs::remove_file(&watermark).unwrap();
        assert_eq!(store.next_id([&first].into_iter()).unwrap().as_str(), "2");
        fs::write(&watermark, b"0").unwrap();
        assert_eq!(store.next_id([&first].into_iter()).unwrap().as_str(), "2");
        fs::write(&watermark, u64::MAX.to_string()).unwrap();
        assert!(matches!(
            store.next_id([&first].into_iter()),
            Err(TaskStoreError::IdExhausted)
        ));
        fs::remove_file(&watermark).unwrap();
        fs::create_dir(&watermark).unwrap();
        assert!(store.delete(&first.id).is_err());
        assert_eq!(store.read_all().unwrap(), [first]);
    }

    #[test]
    fn corrupt_files_fail_closed() {
        let fixture = TestStore::new();
        let store = TaskStore::lock(&fixture.binding).unwrap();
        let first = record(store.next_id([].iter()).unwrap());
        let path = store.record_path(&first.id);
        store.write(&first).unwrap();
        let mut wrong_id = first.clone();
        wrong_id.id = TaskId::try_from("2".to_owned()).unwrap();
        for bytes in [b"{broken".to_vec(), serde_json::to_vec(&wrong_id).unwrap()] {
            fs::write(&path, bytes).unwrap();
            assert!(store.read_all().is_err());
        }
        File::create(&path)
            .unwrap()
            .set_len(MAX_RECORD_BYTES + 1)
            .unwrap();
        assert!(store.read_all().is_err());
        store.write(&first).unwrap();
        assert_eq!(store.read_all().unwrap(), [first.clone()]);
        fs::rename(&path, fixture.binding.directory().join("01.json")).unwrap();
        assert!(store.read_all().is_err());
        assert!(
            !fixture
                .binding
                .directory()
                .read_dir()
                .unwrap()
                .any(|entry| {
                    entry
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(TEMP_PREFIX)
                })
        );
        let mut large = first;
        large.description = "a".repeat(MAX_RECORD_BYTES as usize);
        assert!(matches!(
            TaskStore::validate_size([&large]),
            Err(TaskStoreError::Limit(_))
        ));
        let small = record(TaskId::try_from("1".to_owned()).unwrap());
        assert!(matches!(
            TaskStore::validate_size(std::iter::repeat_n(&small, MAX_TASKS + 1)),
            Err(TaskStoreError::Limit(_))
        ));
        large.description = "a".repeat(64 * 1024);
        assert!(matches!(
            TaskStore::validate_size(std::iter::repeat_n(&large, 300)),
            Err(TaskStoreError::Limit(_))
        ));
    }

    #[test]
    fn bounded_lock_contention() {
        let fixture = TestStore::new();
        let store = TaskStore::lock(&fixture.binding).unwrap();
        assert!(matches!(
            TaskStore::lock(&fixture.binding),
            Err(TaskStoreError::LockTimeout)
        ));
        drop(store);
        assert!(TaskStore::lock(&fixture.binding).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_storage() {
        use std::os::unix::fs::symlink;

        let fixture = TestStore::new();
        let directory = fixture.binding.directory();
        let store = TaskStore::lock(&fixture.binding).unwrap();
        let outside = fixture.root.join("outside");
        fs::write(&outside, b"unchanged").unwrap();
        let id = TaskId::try_from("1".to_owned()).unwrap();
        let path = store.record_path(&id);
        symlink(&outside, &path).unwrap();
        assert!(store.read_all().is_err());
        assert!(store.write(&record(id)).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
        fs::remove_file(&path).unwrap();
        drop(store);
        let lock = directory.join(".lock");
        fs::remove_file(&lock).unwrap();
        symlink(&outside, &lock).unwrap();
        assert!(TaskStore::lock(&fixture.binding).is_err());
        fs::remove_dir_all(&directory).unwrap();
        let target = fixture.root.join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, &directory).unwrap();
        assert!(TaskStore::lock(&fixture.binding).is_err());
        assert!(target.read_dir().unwrap().next().is_none());
    }
}
