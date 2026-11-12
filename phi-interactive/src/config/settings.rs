use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use anyhow::Context;
use filetime::FileTime;
use serde::{Deserialize, Serialize};

use crate::config::{Settings, SwitchConfig};

const LOCK_STALE_AFTER: Duration = Duration::from_secs(10);
const LOCK_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const LOCK_HEARTBEAT_RETRY: Duration = Duration::from_secs(1);

pub(crate) struct SettingsLock {
    path: PathBuf,
    heartbeat: Arc<Mutex<LockHeartbeat>>,
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

enum MtimePrecision {
    Seconds,
    Milliseconds,
}

struct LockHeartbeat {
    modified: FileTime,
    precision: MtimePrecision,
    last_update: SystemTime,
    failure: Option<io::Error>,
}

impl LockHeartbeat {
    fn check(&self, path: &Path) -> io::Result<()> {
        if let Some(error) = &self.failure {
            return Err(io::Error::new(error.kind(), error.to_string()));
        }
        let modified = FileTime::from_last_modification_time(&std::fs::metadata(path)?);
        if modified != self.modified {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings lock modification time changed",
            ));
        }
        Ok(())
    }

    fn refresh(&mut self, path: &Path) -> Option<Duration> {
        let result: io::Result<()> = (|| {
            self.check(path)?;
            let now = FileTime::now();
            let modified = match self.precision {
                MtimePrecision::Seconds => FileTime::from_unix_time(
                    now.unix_seconds() + i64::from(now.nanoseconds() != 0),
                    0,
                ),
                MtimePrecision::Milliseconds => FileTime::from_unix_time(
                    now.unix_seconds(),
                    now.nanoseconds() / 1_000_000 * 1_000_000,
                ),
            };
            filetime::set_file_mtime(path, modified)?;
            self.modified = modified;
            self.last_update = SystemTime::now();
            Ok(())
        })();
        match result {
            Ok(()) => Some(LOCK_HEARTBEAT_INTERVAL),
            Err(error) => {
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::InvalidData
                ) || self.last_update.elapsed().unwrap_or_default() > LOCK_STALE_AFTER
                {
                    self.failure = Some(error);
                    None
                } else {
                    Some(LOCK_HEARTBEAT_RETRY)
                }
            }
        }
    }
}

impl SettingsLock {
    pub(crate) fn path_for(settings: &Path) -> PathBuf {
        let mut path = settings.as_os_str().to_os_string();
        path.push(".lock");
        PathBuf::from(path)
    }

    fn try_acquire(settings: &Path) -> io::Result<Self> {
        let path = Self::path_for(settings);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match std::fs::create_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                match std::fs::metadata(&path) {
                    Ok(metadata)
                        if metadata.modified()?.elapsed().unwrap_or_default()
                            > LOCK_STALE_AFTER =>
                    {
                        match std::fs::remove_dir(&path) {
                            Ok(()) => {}
                            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                            Err(error) => return Err(error),
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "settings are locked",
                        ));
                    }
                }
                std::fs::create_dir(&path).map_err(|error| {
                    if error.kind() == io::ErrorKind::AlreadyExists {
                        io::Error::new(io::ErrorKind::WouldBlock, "settings are locked")
                    } else {
                        error
                    }
                })?;
            }
            Err(error) => return Err(error),
        }
        let initialized = (|| {
            let now = FileTime::now();
            let probe = FileTime::from_unix_time(
                now.unix_seconds() + i64::from(now.nanoseconds() != 0),
                5_000_000,
            );
            filetime::set_file_mtime(&path, probe)?;
            let modified = FileTime::from_last_modification_time(&std::fs::metadata(&path)?);
            let precision = if modified.nanoseconds() == 0 {
                MtimePrecision::Seconds
            } else {
                MtimePrecision::Milliseconds
            };
            let heartbeat = Arc::new(Mutex::new(LockHeartbeat {
                modified,
                precision,
                last_update: SystemTime::now(),
                failure: None,
            }));
            let (stop, stopped) = mpsc::channel();
            let worker_path = path.clone();
            let worker_heartbeat = heartbeat.clone();
            let worker = std::thread::Builder::new()
                .name("settings-lock-heartbeat".into())
                .spawn(move || {
                    let mut delay = LOCK_HEARTBEAT_INTERVAL;
                    while matches!(
                        stopped.recv_timeout(delay),
                        Err(mpsc::RecvTimeoutError::Timeout)
                    ) {
                        let mut heartbeat = worker_heartbeat
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        match heartbeat.refresh(&worker_path) {
                            Some(next_delay) => delay = next_delay,
                            None => break,
                        }
                    }
                })?;
            Ok(Self {
                path: path.clone(),
                heartbeat,
                stop,
                worker: Some(worker),
            })
        })();
        if initialized.is_err() {
            let _ = std::fs::remove_dir(&path);
        }
        initialized
    }

    fn check(&self) -> io::Result<()> {
        self.heartbeat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .check(&self.path)
    }
}

impl Drop for SettingsLock {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if self.check().is_ok()
            && let Err(error) = std::fs::remove_dir(&self.path)
        {
            tracing::warn!(%error, path = %self.path.display(), "failed to release settings lock");
        }
    }
}

pub fn phi_path() -> PathBuf {
    phi_sdk::workspace::phi_config_dir()
}

pub fn settings_file_path() -> PathBuf {
    phi_path().join("settings.json")
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub(crate) enum SettingsWriteOutcome {
    Saved,
    Denied { message: String },
    Failed,
}

pub(crate) fn persist_default_model(model: String) -> SettingsWriteOutcome {
    write_settings(|settings| {
        settings.insert("defaultModel".into(), serde_json::json!(model));
    })
}

pub(crate) fn save_switches(switches: Option<Vec<SwitchConfig>>) -> SettingsWriteOutcome {
    write_settings(|settings| {
        settings.insert("switches".into(), serde_json::json!(switches));
        settings.remove("modes");
    })
}

fn write_settings(
    update: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
) -> SettingsWriteOutcome {
    let path = settings_file_path();
    if let Err(error) = crate::security::ensure_writable(&path) {
        return SettingsWriteOutcome::Denied {
            message: error.to_string(),
        };
    }
    match super::SettingsManager::update_file(&path, |settings| {
        update(settings);
        Ok(())
    }) {
        Ok(()) => SettingsWriteOutcome::Saved,
        Err(_) => SettingsWriteOutcome::Failed,
    }
}

pub(super) fn update_settings_file<T>(
    path: &Path,
    update: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    crate::security::ensure_writable(path)?;
    const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);
    let started = std::time::Instant::now();
    let lock = loop {
        match SettingsLock::try_acquire(path) {
            Ok(lock) => break lock,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < LOCK_TIMEOUT =>
            {
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to lock {}", path.display()));
            }
        }
    };
    let mut value = match std::fs::read(path) {
        Ok(content) => serde_json::from_slice::<serde_json::Value>(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", path.display()));
        }
    };
    let settings = value
        .as_object_mut()
        .context("settings must be a JSON object")?;
    let result = update(settings)?;
    let content = format!("{}\n", serde_json::to_string_pretty(&value)?);
    lock.check()
        .with_context(|| format!("lost settings lock for {}", path.display()))?;
    replace_config_file(path, &content)
        .with_context(|| format!("failed to save {}", path.display()))?;
    Ok(result)
}

pub fn project_scope_dir() -> Option<PathBuf> {
    project_scope_dir_for(&std::env::current_dir().ok()?)
}

pub(crate) fn project_scope_dir_for(working_dir: &Path) -> Option<PathBuf> {
    let home = phi_sdk::workspace::expand_home_path("~");
    scope_dir_for(working_dir, &home, &phi_path())
}

pub(crate) fn same_directory(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
        == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
}

fn scope_dir_for(cwd: &Path, home: &Path, global_dir: &Path) -> Option<PathBuf> {
    let root = phi_sdk::workspace::project_scope_root(cwd);
    let project = root.join(".phi");
    (!same_directory(&root, home) && !same_directory(&project, global_dir)).then_some(project)
}

pub fn mcp_config_file_path() -> PathBuf {
    phi_path().join("mcp.json")
}

pub fn global_agents_path() -> PathBuf {
    phi_path().join("AGENTS.md")
}

pub fn model_catalog_options(offline: bool) -> phi_sdk::models::ModelCatalogOptions {
    phi_sdk::models::ModelCatalogOptions {
        store_path: phi_path().join("models-store.json"),
        user_paths: vec![phi_path().join("models.json")],
        offline,
    }
}

pub fn offline_mode() -> anyhow::Result<bool> {
    let Some(value) = std::env::var_os("PHI_OFFLINE") else {
        return Ok(false);
    };
    let value = value
        .into_string()
        .map_err(|_| anyhow::anyhow!("PHI_OFFLINE must be valid UTF-8"))?;
    parse_offline(Some(&value))
}

fn parse_offline(value: Option<&str>) -> anyhow::Result<bool> {
    let Some(value) = value else {
        return Ok(false);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => anyhow::bail!(
            "invalid PHI_OFFLINE value {value:?}; expected 1/true/yes/on or 0/false/no/off"
        ),
    }
}

pub fn auth_dir() -> PathBuf {
    std::env::var_os("PHI_AUTH_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| phi_path().join("auth"))
}

pub fn provider_auth_file(provider: &str) -> PathBuf {
    auth_dir().join(format!("{provider}.json"))
}

#[cfg(test)]
pub(crate) fn load() -> anyhow::Result<Settings> {
    Ok(super::SettingsManager::load()?.settings())
}

pub(super) fn read_sources(working_dir: &Path) -> anyhow::Result<(Settings, Settings)> {
    let global =
        load_settings_file(&settings_file_path())?.merge(load_mcp_file(&mcp_config_file_path())?);
    let project = match project_scope_dir_for(working_dir) {
        Some(dir) => load_settings_file(&dir.join("settings.json"))?,
        None => Settings::default(),
    };
    Ok((global, project))
}

fn load_settings_file(path: &PathBuf) -> anyhow::Result<Settings> {
    let mut config = read_settings(path)?;
    config.mcp_servers = None;
    config.mcp_settings = None;
    Ok(config)
}

fn load_mcp_file(path: &PathBuf) -> anyhow::Result<Settings> {
    let config = read_settings(path)?;
    Ok(Settings {
        mcp_servers: config.mcp_servers,
        mcp_settings: config.mcp_settings,
        ..Settings::default()
    })
}

#[derive(Debug)]
pub(crate) struct McpServerToolUpdate {
    pub(crate) server_name: String,
    pub(crate) tool_exposure: std::collections::HashMap<String, phi_sdk::tools::ToolExposure>,
}

pub(crate) fn update_mcp_config(
    document: &mut serde_json::Map<String, serde_json::Value>,
    updates: &[McpServerToolUpdate],
) -> anyhow::Result<()> {
    let servers = document
        .get_mut("mcpServers")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("MCP config is missing an mcpServers object"))?;
    for update in updates {
        let server = servers
            .get_mut(&update.server_name)
            .and_then(serde_json::Value::as_object_mut)
            .ok_or_else(|| anyhow::anyhow!("MCP server {:?} is missing", update.server_name))?;
        let tools = server
            .entry("toolExposure")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("toolExposure must be an object"))?;
        for (name, exposure) in &update.tool_exposure {
            tools.insert(name.clone(), serde_json::to_value(exposure)?);
        }
    }
    Ok(())
}

pub(crate) fn replace_config_file(path: &Path, content: &str) -> anyhow::Result<()> {
    crate::security::ensure_writable(path)?;
    use std::io::Write;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        anyhow::Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
pub(crate) fn load_file(path: &PathBuf) -> anyhow::Result<Settings> {
    read_settings(path)
}

fn read_settings(path: &PathBuf) -> anyhow::Result<Settings> {
    if !path.exists() {
        return Ok(Settings::default());
    }

    let content = std::fs::read_to_string(path).map_err(|error| {
        anyhow::anyhow!(crate::resources::format_diagnostic(
            "Config error",
            path,
            &format!("failed to read config: {error}")
        ))
    })?;
    serde_json::from_str(&content).map_err(|error| {
        let message = error.to_string();
        let message = match message.strip_prefix("expected value at line ") {
            Some(location) => format!("missing or invalid value at line {location}"),
            None => message,
        };
        anyhow::anyhow!(crate::resources::format_diagnostic(
            "Config error",
            path,
            &format!("invalid JSON: {message}")
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory() -> PathBuf {
        let directory =
            Path::new("/tmp").join(format!("phi-settings-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn directory_lock_excludes_writers_and_cleans_up() {
        let directory = test_directory();
        let settings = directory.join("settings.json");
        let lock = SettingsLock::try_acquire(&settings).unwrap();
        let path = SettingsLock::path_for(&settings);
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        assert_eq!(
            SettingsLock::try_acquire(&settings).err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(lock);
        assert!(!path.exists());
        drop(SettingsLock::try_acquire(&settings).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn stale_lock_directory_is_reclaimed() {
        let directory = test_directory();
        let settings = directory.join("settings.json");
        let path = SettingsLock::path_for(&settings);
        std::fs::create_dir(&path).unwrap();
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let lock = SettingsLock::try_acquire(&settings).unwrap();
        assert!(path.is_dir());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
        drop(lock);
        assert!(!path.exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn heartbeat_updates_directory_without_marker_files() {
        let directory = test_directory();
        let settings = directory.join("settings.json");
        let lock = SettingsLock::try_acquire(&settings).unwrap();
        let before = std::fs::metadata(&lock.path).unwrap().modified().unwrap();
        let started = std::time::Instant::now();
        while std::fs::metadata(&lock.path).unwrap().modified().unwrap() == before {
            assert!(started.elapsed() < LOCK_HEARTBEAT_INTERVAL + Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(20));
        }
        lock.check().unwrap();
        assert_eq!(std::fs::read_dir(&lock.path).unwrap().count(), 0);
        drop(lock);
        assert!(!SettingsLock::path_for(&settings).exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unchanged_mtime_can_recover_after_the_stale_threshold() {
        let directory = test_directory();
        let lock = SettingsLock::try_acquire(&directory.join("settings.json")).unwrap();
        {
            let mut heartbeat = lock.heartbeat.lock().unwrap();
            let old = SystemTime::now() - LOCK_STALE_AFTER - Duration::from_secs(1);
            heartbeat.modified = FileTime::from_system_time(old);
            heartbeat.last_update = old;
            filetime::set_file_mtime(&lock.path, heartbeat.modified).unwrap();
            assert_eq!(heartbeat.refresh(&lock.path), Some(LOCK_HEARTBEAT_INTERVAL));
            assert!(heartbeat.failure.is_none());
        }
        lock.check().unwrap();
        drop(lock);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn changed_mtime_rejects_write_and_preserves_the_lock_directory() {
        let directory = test_directory();
        let settings = directory.join("settings.json");
        std::fs::write(&settings, "{}").unwrap();
        let result = update_settings_file(&settings, |document| {
            filetime::set_file_mtime(SettingsLock::path_for(&settings), FileTime::zero())?;
            document.insert("rejected".into(), serde_json::json!(true));
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{}");
        assert!(SettingsLock::path_for(&settings).is_dir());
        drop(SettingsLock::try_acquire(&settings).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn temporary_heartbeat_errors_retry_until_the_stale_threshold() {
        use std::os::unix::fs::PermissionsExt;
        let directory = test_directory();
        let lock = SettingsLock::try_acquire(&directory.join("settings.json")).unwrap();
        let mut heartbeat = lock.heartbeat.lock().unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o000)).unwrap();
        let retry = heartbeat.refresh(&lock.path);
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(retry, Some(LOCK_HEARTBEAT_RETRY));
        assert!(heartbeat.failure.is_none());
        assert_eq!(heartbeat.refresh(&lock.path), Some(LOCK_HEARTBEAT_INTERVAL));
        heartbeat.last_update = SystemTime::now() - LOCK_STALE_AFTER - Duration::from_secs(1);
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o000)).unwrap();
        let expired = heartbeat.refresh(&lock.path);
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(expired.is_none());
        assert!(heartbeat.failure.is_some());
        drop(heartbeat);
        let path = lock.path.clone();
        drop(lock);
        assert!(path.is_dir());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn failed_settings_updates_release_the_directory_lock() {
        let directory = test_directory();
        let settings = directory.join("settings.json");
        for original in ["invalid", "{}"] {
            std::fs::write(&settings, original).unwrap();
            let result: anyhow::Result<()> =
                update_settings_file(&settings, |_| anyhow::bail!("rejected update"));
            assert!(result.is_err());
            assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
            assert!(!SettingsLock::path_for(&settings).exists());
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn config_replacement_cleans_temp() {
        let root = std::env::temp_dir().join(format!("phi-config-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("settings.json");
        std::fs::write(&path, "original").unwrap();
        replace_config_file(&path, "replacement").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "replacement");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let retained = path.join("retained");
        std::fs::write(&retained, "untouched").unwrap();
        assert!(replace_config_file(&path, "rejected").is_err());
        assert_eq!(std::fs::read_to_string(retained).unwrap(), "untouched");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_json_reports_clear_reason_and_location() {
        let path =
            std::env::temp_dir().join(format!("phi-invalid-json-{}.json", uuid::Uuid::new_v4()));
        for value in ["", "undefined", "'value'"] {
            std::fs::write(&path, format!("{{\n  \"defaultModel\": {value}\n}}")).unwrap();
            let error = super::load_file(&path).unwrap_err().to_string();
            assert!(
                error.starts_with(&format!(
                    "[Config error]\n  {}\n  invalid JSON: ",
                    path.display()
                )),
                "{error}"
            );
            assert!(
                error.contains("missing or invalid value at line"),
                "{error}"
            );
            assert_eq!(error.matches(" at line ").count(), 1, "{error}");
            assert_eq!(error.matches(" column ").count(), 1, "{error}");
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn settings_transactions_serialize_process_writers() {
        const CHILD_ROLE: &str = "PHI_SETTINGS_TRANSACTION_ROLE";
        const TEST_ROOT: &str = "PHI_SETTINGS_TRANSACTION_ROOT";
        const WAIT_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);
        fn wait_for(path: &std::path::Path) {
            let started = std::time::Instant::now();
            while !path.exists() {
                assert!(
                    started.elapsed() < WAIT_LIMIT,
                    "timed out waiting for {}",
                    path.display()
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        if let Ok(role) = std::env::var(CHILD_ROLE) {
            let root = std::path::PathBuf::from(std::env::var_os(TEST_ROOT).unwrap());
            std::fs::write(root.join(format!("{role}.started")), "").unwrap();
            super::update_settings_file(&root.join("settings.json"), |settings| {
                if role == "first" {
                    std::fs::write(root.join("held"), "")?;
                    wait_for(&root.join("release"));
                    std::fs::remove_file(root.join("held"))?;
                } else {
                    anyhow::ensure!(
                        !root.join("held").exists(),
                        "overlapping settings transactions"
                    );
                }
                settings.insert(role, serde_json::json!(true));
                Ok(())
            })
            .unwrap();
            return;
        }
        for initial in [None, Some(r#"{"untouched":true}"#)] {
            let root = std::path::PathBuf::from("/tmp")
                .join(format!("phi-settings-transaction-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("settings.json");
            if let Some(initial) = initial {
                std::fs::write(&path, initial).unwrap();
            }
            let spawn_writer = |role| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "config::settings::tests::settings_transactions_serialize_process_writers",
                        "--nocapture",
                    ])
                    .env(CHILD_ROLE, role)
                    .env(TEST_ROOT, &root)
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            };
            let first = spawn_writer("first");
            wait_for(&root.join("held"));
            let second = spawn_writer("second");
            wait_for(&root.join("second.started"));
            std::thread::sleep(std::time::Duration::from_millis(150));
            std::fs::write(root.join("release"), "").unwrap();
            let first = first.wait_with_output().unwrap();
            let second = second.wait_with_output().unwrap();
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            std::fs::remove_dir_all(root).unwrap();
            assert!(
                first.status.success(),
                "{}",
                String::from_utf8_lossy(&first.stderr)
            );
            assert!(
                second.status.success(),
                "{}",
                String::from_utf8_lossy(&second.stderr)
            );
            assert_eq!(saved["first"], true);
            assert_eq!(saved["second"], true);
            if initial.is_some() {
                assert_eq!(saved["untouched"], true);
            }
        }
    }

    #[test]
    fn settings_edits_preserve_invalid_files() {
        const CHILD_MARKER: &str = "PHI_SETTINGS_WRITE_TEST";
        if std::env::var_os(CHILD_MARKER).is_none() {
            let root = std::path::PathBuf::from("/tmp")
                .join(format!("phi-settings-write-{}", uuid::Uuid::new_v4()));
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "config::settings::tests::settings_edits_preserve_invalid_files",
                    "--nocapture",
                ])
                .env(CHILD_MARKER, "1")
                .env("PHI_CONFIG_DIR", &root)
                .status()
                .unwrap();
            let _ = std::fs::remove_dir_all(root);
            assert!(status.success());
            return;
        }
        let path = settings_file_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        for content in ["{\"defaultModel\":", "not JSON", "[]", "null", "17"] {
            for model_only in [false, true] {
                std::fs::write(&path, content).unwrap();
                let outcome = if model_only {
                    persist_default_model("selected".into())
                } else {
                    save_switches(None)
                };
                assert!(matches!(outcome, SettingsWriteOutcome::Failed));
                assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            }
        }
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            persist_default_model("initial".into()),
            SettingsWriteOutcome::Saved
        ));
        let mut original: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        original["untouched"] = serde_json::json!({"label": "retained"});
        std::fs::write(&path, original.to_string()).unwrap();
        assert!(matches!(save_switches(None), SettingsWriteOutcome::Saved));
        assert!(matches!(
            persist_default_model("selected".into()),
            SettingsWriteOutcome::Saved
        ));
        let saved = std::fs::read(&path).unwrap();
        let settings: serde_json::Value = serde_json::from_slice(&saved).unwrap();
        assert_eq!(settings["untouched"], original["untouched"]);
        assert_eq!(settings["defaultModel"], "selected");
        assert!(settings["switches"].is_null());
        assert!(
            super::update_settings_file::<()>(&path, |settings| {
                settings.insert("uncommitted".into(), serde_json::json!(true));
                anyhow::bail!("update rejected")
            })
            .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), saved);
    }

    #[test]
    fn invalid_permission_config_reports_source_and_field() {
        let path = std::env::temp_dir().join(format!(
            "phi-invalid-permission-{}.json",
            uuid::Uuid::new_v4()
        ));
        for permission in [
            serde_json::json!({"read":"deny", "bash":"aks"}),
            serde_json::json!({"glbo":"deny"}),
        ] {
            std::fs::write(
                &path,
                serde_json::json!({"no_tools":true, "permission":permission}).to_string(),
            )
            .unwrap();
            let error = super::load_file(&path).unwrap_err().to_string();
            assert!(error.contains(path.to_str().unwrap()), "{error}");
            assert!(error.contains("permission."), "{error}");
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn home_and_global_directory_have_no_project_scope() {
        let home = std::env::temp_dir().join(format!("phi-home-{}", uuid::Uuid::new_v4()));
        let cwd = home.join("project");
        let global = home.join(".phi");

        assert_eq!(scope_dir_for(&home, &home, &global), None);
        assert_eq!(scope_dir_for(&cwd, &home, &global), Some(cwd.join(".phi")));
        assert_eq!(scope_dir_for(&cwd, &home, &cwd.join(".phi")), None);
    }

    #[test]
    fn project_scope_uses_git_root_from_subdirectory() {
        let root = std::env::temp_dir().join(format!("phi-project-{}", uuid::Uuid::new_v4()));
        let nested = root.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        assert!(
            std::process::Command::new("git")
                .env_remove("GIT_INDEX_FILE")
                .args(["init", "--quiet"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let home = root.join("home");
        let global = home.join(".phi");
        assert_eq!(
            scope_dir_for(&nested, &home, &global),
            Some(root.join(".phi"))
        );
        assert!(
            std::process::Command::new("git")
                .env_remove("GIT_INDEX_FILE")
                .args([
                    "-c",
                    "user.name=Phi Test",
                    "-c",
                    "user.email=phi-test@localhost",
                    "commit",
                    "--quiet",
                    "--allow-empty",
                    "-m",
                    "base",
                ])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let linked = root.with_extension("linked");
        assert!(
            std::process::Command::new("git")
                .env_remove("GIT_INDEX_FILE")
                .args(["worktree", "add", "--quiet", "-b", "linked"])
                .arg(&linked)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let linked_nested = linked.join("src");
        std::fs::create_dir_all(&linked_nested).unwrap();
        assert_eq!(
            scope_dir_for(&linked_nested, &home, &global),
            Some(linked.join(".phi"))
        );
        assert_eq!(
            project_scope_dir_for(&linked_nested),
            Some(linked.join(".phi"))
        );
        std::fs::remove_dir_all(linked).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::{load_mcp_file, load_settings_file};

    #[test]
    fn mcp_exposure_updates_preserve_defaults_and_unseen_overrides() {
        use phi_sdk::tools::ToolExposure;
        use serde_json::json;
        let original = json!({"mcpServers":{"fixture":{
            "command":"fixture", "exposure":"hidden",
            "toolExposure":{"search":"direct", "unseen":"hidden"}
        }, "other":{"command":"other"}}});
        let updates = [super::McpServerToolUpdate {
            server_name: "fixture".into(),
            tool_exposure: std::collections::HashMap::from([(
                "search".into(),
                ToolExposure::Deferred,
            )]),
        }];
        let mut updated = original.clone();
        super::update_mcp_config(updated.as_object_mut().unwrap(), &updates).unwrap();
        assert_eq!(updated["mcpServers"]["fixture"]["exposure"], "hidden");
        assert_eq!(
            updated["mcpServers"]["fixture"]["toolExposure"],
            json!({"search":"deferred", "unseen":"hidden"})
        );
        assert_eq!(
            updated["mcpServers"]["other"],
            original["mcpServers"]["other"]
        );
    }

    #[test]
    fn settings_and_mcp_files_keep_separate_ownership() {
        let path = std::env::temp_dir().join(format!("phi-settings-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{
                "mcp": false,
                "permission": { "mcp": "deny" },
                "mcpServers": { "test": { "command": "test-server" } }
            }"#,
        )
        .unwrap();

        let settings = load_settings_file(&path).unwrap();
        let mcp = load_mcp_file(&path).unwrap();

        let _ = std::fs::remove_file(path);
        assert_eq!(settings.mcp, Some(false));
        assert!(settings.permission.is_some());
        assert!(settings.mcp_servers.is_none());
        assert_eq!(mcp.mcp, None);
        assert!(mcp.permission.is_none());
        assert!(mcp.mcp_servers.is_some());
    }
}
