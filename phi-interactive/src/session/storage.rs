use std::path::{Path, PathBuf};

use phi_sdk::transcript::Transcript;
use phi_sdk::workspace::session_scope_dir;

use super::workflow::{WorkflowRecord, WorkflowState};
use super::{AgentMessage, Session, TranscriptEntryKind};
use crate::file_lock::FileLock;
use crate::session::{discovery, journal};

pub fn session_path(session: &Session) -> PathBuf {
    let directory = session.sidechain.as_ref().map_or_else(
        || session_scope_dir(session.working_dir.as_str()),
        |sidechain| sidechain.directory(),
    );
    directory.join(session_file_name(session))
}

fn session_file_name(session: &Session) -> String {
    if session.sidechain.is_some() {
        return format!("agent-{}.jsonl", session.id);
    }
    let timestamp = chrono::DateTime::parse_from_rfc3339(&session.created_at)
        .map(|dt| {
            dt.with_timezone(&chrono::Utc)
                .format("%Y%m%dT%H%M%SZ")
                .to_string()
        })
        .unwrap_or_else(|_| {
            session
                .created_at
                .chars()
                .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
                .collect()
        });
    format!("{timestamp}-{}.jsonl", session.id)
}

pub fn save_session(session: &Session) -> anyhow::Result<()> {
    save_session_with_transcript(session, &session.transcript)
}

pub fn save_session_with_transcript(
    session: &Session,
    transcript: &Transcript,
) -> anyhow::Result<()> {
    session.validate_storage()?;
    transcript.validate_switch_snapshots()?;
    anyhow::ensure!(
        transcript.id == session.id,
        "Session Transcript identity mismatch"
    );
    let path = session_path(session);
    validate_session_path(&path)?;
    save_session_at_path(session, transcript, &path)
}

fn save_session_at_path(
    session: &Session,
    transcript: &Transcript,
    path: &Path,
) -> anyhow::Result<()> {
    let retains_metadata = session.is_delegated()
        || !session.children.is_empty()
        || !session.workflow_run_ids.is_empty();
    let has_conversation = transcript.entries.iter().any(|entry| {
        matches!(
            &entry.kind,
            TranscriptEntryKind::Message {
                message: AgentMessage::User { .. }
                    | AgentMessage::Agent { .. }
                    | AgentMessage::Assistant { .. }
                    | AgentMessage::ToolResult { .. }
            } | TranscriptEntryKind::Reset { .. }
                | TranscriptEntryKind::ContextEdit { .. }
                | TranscriptEntryKind::Failure { .. }
        )
    });
    if !retains_metadata && !has_conversation {
        journal::remove_empty_checkpoint(path)?;
        return Ok(());
    }
    if !retains_metadata
        && !path.exists()
        && !transcript.entries.iter().any(|entry| {
            matches!(
                &entry.kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::Assistant { .. }
                } | TranscriptEntryKind::Reset { .. }
                    | TranscriptEntryKind::ContextEdit { .. }
                    | TranscriptEntryKind::Failure { .. }
            )
        })
    {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    journal::append_or_checkpoint_session(path, session, transcript)
}

#[derive(Clone)]
pub(crate) struct WorkflowStorage {
    path: PathBuf,
}

impl WorkflowStorage {
    pub(crate) fn new(
        session: &Session,
        id: phi_sdk::workflow::WorkflowRunId,
    ) -> anyhow::Result<Self> {
        session.validate_storage()?;
        let directory = session_path(session)
            .parent()
            .ok_or_else(|| anyhow::anyhow!("missing Session directory"))?
            .join(session.id.as_str())
            .join("workflows");
        Ok(Self {
            path: directory.join(format!("{id}.json")),
        })
    }

    pub(crate) fn load<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        validate_session_path(&self.path)?;
        let file = std::fs::File::open(&self.path)?;
        Ok(serde_json::from_reader(std::io::BufReader::new(file))?)
    }

    pub(crate) fn save(&self, record: &impl serde::Serialize) -> anyhow::Result<()> {
        use std::io::Write;
        validate_session_path(&self.path)?;
        let directory = self.path.parent().expect("Workflow directory");
        std::fs::create_dir_all(directory)?;
        let temporary = directory.join(format!(".workflow-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> anyhow::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            serde_json::to_writer(&mut file, record)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)?;
            std::fs::File::open(directory)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) struct SavedSessionPath {
    path: PathBuf,
    id: String,
}

impl SavedSessionPath {
    pub(crate) fn resolve(locator: &str) -> anyhow::Result<Self> {
        Self::from_path(discovery::resolve_session_path(locator)?, locator)
    }

    pub(crate) fn resolve_id(id: &str) -> anyhow::Result<Self> {
        Self::from_path(discovery::resolve_session_id(id)?, id)
    }

    fn from_path(path: Option<PathBuf>, locator: &str) -> anyhow::Result<Self> {
        let path = path.ok_or_else(|| anyhow::anyhow!("session not found: {locator}"))?;
        let path = std::path::absolute(path)?;
        validate_session_path(&path)?;
        validate_session_path(&path.with_extension("lock"))?;
        let id = session_file_id(&path)
            .ok_or_else(|| anyhow::anyhow!("invalid saved Session path"))?
            .to_owned();
        Ok(Self { path, id })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn validate(&self, session: &Session) -> anyhow::Result<()> {
        session.validate_storage()?;
        anyhow::ensure!(
            session.id == self.id && session_path(session) == self.path,
            "saved Session identity or storage association mismatch"
        );
        Ok(())
    }

    pub(crate) fn open(self) -> anyhow::Result<(Session, SessionLock)> {
        let lock = lease_session_path(&self.path)?;
        let mut session = journal::load_session_file(&self.path)?;
        self.validate(&session)?;
        restore_workflow_children(&mut session);
        Ok((session, lock))
    }
}

#[cfg(test)]
pub fn load_session(id_or_path: &str) -> anyhow::Result<Session> {
    let path = discovery::resolve_session_path(id_or_path)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {id_or_path}"))?;
    let mut session = journal::load_session_file(&path)?;
    if session.validate_storage().is_ok() && session_path(&session) == path {
        restore_workflow_children(&mut session);
    }
    Ok(session)
}

pub fn load_session_id(id: &str) -> anyhow::Result<Session> {
    let path = discovery::resolve_session_id(id)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
    let mut session = journal::load_session_file(&path)?;
    session.validate_storage()?;
    anyhow::ensure!(
        session.id == id && session_path(&session) == path,
        "saved Session identity or storage association mismatch"
    );
    restore_workflow_children(&mut session);
    Ok(session)
}

#[derive(serde::Deserialize)]
struct WorkflowReceipt {
    run_id: phi_sdk::workflow::WorkflowRunId,
    status: String,
}

fn restore_workflow_children(session: &mut Session) {
    if session.workflow_run_ids.is_empty() || session.children.is_empty() {
        return;
    }
    let receipts: Vec<_> = session
        .current_branch()
        .into_iter()
        .filter_map(|entry| {
            let TranscriptEntryKind::Message {
                message:
                    AgentMessage::ToolResult {
                        tool_call_id,
                        tool_name,
                        content,
                        is_error: false,
                        ..
                    },
            } = &entry.kind
            else {
                return None;
            };
            if tool_name != "Workflow" {
                return None;
            }
            let [super::ContentBlock::Text { text }] = content.as_slice() else {
                return None;
            };
            let receipt: WorkflowReceipt = serde_json::from_str(text).ok()?;
            (receipt.status == "accepted" && session.workflow_run_ids.contains(&receipt.run_id))
                .then(|| (receipt.run_id, tool_call_id.to_string()))
        })
        .collect();
    for (run_id, call_id) in receipts {
        // Auxiliary observation records cannot make otherwise valid history unreadable.
        let Ok(record) = WorkflowRecord::load(session, run_id) else {
            continue;
        };
        let WorkflowState::Settled { invocations, .. } = record.state else {
            continue;
        };
        for invocation in invocations {
            let Some(instance_id) = invocation
                .worker(&session.children)
                .map(|child| child.child_agent_id.clone())
            else {
                continue;
            };
            if let Some(child) = session.children.iter_mut().find(|child| {
                child.child_agent_id == instance_id
                    && child.child_session_id.as_str() == instance_id
            }) {
                if child
                    .parent_tool_call_id
                    .as_ref()
                    .is_some_and(|saved| saved != &call_id)
                {
                    continue;
                }
                child.parent_tool_call_id = Some(call_id.clone());
                child.recovered_outcome = invocation.outcome();
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn load_session_checkpoint(id: &str) -> anyhow::Result<Session> {
    let path = discovery::resolve_session_id(id)?
        .ok_or_else(|| anyhow::anyhow!("session not found: {id}"))?;
    let _lock = lease_session_path(&path)?;
    let session = journal::load_session_file(&path)?;
    anyhow::ensure!(
        session.id == id && session_path(&session) == path,
        "saved Session identity mismatch"
    );
    Ok(session)
}

pub struct SessionLock {
    _lock: FileLock,
}

pub fn cleanup_stale_session_locks(directory: &Path) -> std::io::Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for path in discovery::session_tree_files(directory).map_err(std::io::Error::other)? {
        if path.extension().is_none_or(|extension| extension != "lock") {
            continue;
        }
        match FileLock::try_acquire_existing(&path) {
            Ok(Some(lock)) => lock.remove_file()?,
            Ok(None) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn session_file_id(path: &Path) -> Option<&str> {
    let extension = path.extension()?.to_str()?;
    if !matches!(extension, "jsonl" | "lock") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let id = match stem.strip_prefix("agent-") {
        Some(id) => id,
        None => {
            let (timestamp, id) = stem.split_once('-')?;
            chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ").ok()?;
            id
        }
    };
    super::model::valid_session_id(id).then_some(id)
}

pub(crate) fn lock_session(session: &Session) -> anyhow::Result<SessionLock> {
    session.validate_storage()?;
    let path = session_path(session);
    validate_session_path(&path)?;
    validate_session_path(&path.with_extension("lock"))?;
    Ok(lease_session_path(&path)?)
}

fn validate_session_path(path: &Path) -> anyhow::Result<()> {
    let root = phi_sdk::workspace::phi_session_root();
    let relative = path.strip_prefix(&root)?;
    let mut current = root;
    for component in relative.components() {
        anyhow::ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "invalid Session path"
        );
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) => anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "Session path contains a symlink: {}",
                current.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn lease_session_path(path: &Path) -> std::io::Result<SessionLock> {
    Ok(SessionLock {
        _lock: FileLock::try_acquire_shared(&path.with_extension("lock"))?,
    })
}

pub(crate) fn lock_session_path(path: &Path) -> std::io::Result<SessionLock> {
    Ok(SessionLock {
        _lock: FileLock::try_acquire(&path.with_extension("lock"))?,
    })
}

pub fn delete_session(id: &str) -> anyhow::Result<()> {
    let Some(path) = discovery::resolve_session_id(id)? else {
        return Ok(());
    };
    delete_session_path(&path, id)
}

fn delete_session_path(path: &Path, id: &str) -> anyhow::Result<()> {
    let _lock = lock_for_deletion(path, id)?;
    let directory = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("missing Session directory"))?
        .join(id);
    let has_sidechains = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => {
            anyhow::ensure!(metadata.is_dir(), "invalid Session sidechain directory");
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let mut child_locks = Vec::new();
    if has_sidechains {
        let children = directory.join("subagents");
        match std::fs::symlink_metadata(&children) {
            Ok(metadata) => {
                anyhow::ensure!(metadata.is_dir(), "invalid Session subagents directory");
                let child_paths = discovery::session_tree_files(&children)?
                    .into_iter()
                    .map(|path| path.with_extension("jsonl"))
                    .collect::<std::collections::BTreeSet<_>>();
                for child_path in child_paths {
                    let child_id = session_file_id(&child_path)
                        .ok_or_else(|| anyhow::anyhow!("invalid child Session path"))?;
                    child_locks.push(lock_for_deletion(&child_path, child_id)?);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        std::fs::remove_dir_all(&directory)?;
    }
    std::fs::remove_file(path)?;
    Ok(())
}

fn lock_for_deletion(path: &Path, id: &str) -> anyhow::Result<SessionLock> {
    lock_session_path(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::WouldBlock {
            anyhow::anyhow!("cannot delete an associated session `{id}`; stop the Agent first")
        } else {
            error.into()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_open_retains_lock_and_rejects_mismatched_storage() {
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.working_dir = format!("/tmp/phi-saved-open-{}", uuid::Uuid::new_v4()).into();
        session.transcript.reset_context(None).unwrap();
        save_session(&session).unwrap();
        let path = session_path(&session);
        let (loaded, lock) = SavedSessionPath::resolve_id(&session.id)
            .unwrap()
            .open()
            .unwrap();
        assert_eq!(loaded.id, session.id);
        drop(lock_session(&session).expect("independent readers may resume the same Session"));
        assert!(lock_session_path(&path).is_err());
        assert!(delete_session(&session.id).is_err());
        drop(lock);

        let mut mismatched = session.clone();
        mismatched.working_dir = "/tmp/phi-other-storage".into();
        save_session_at_path(&mismatched, &mismatched.transcript, &path).unwrap();
        let error = SavedSessionPath::resolve(path.to_str().unwrap())
            .unwrap()
            .open()
            .err()
            .expect("mismatched storage must not be admitted");
        assert!(error.to_string().contains("storage association mismatch"));
        drop(lock_session(&session).expect("failed loading must release its lock"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn cannot_delete_session_while_association_holds_its_lock() {
        let root = std::env::temp_dir().join(format!("phi-session-{}", uuid::Uuid::new_v4()));
        let directory = root.join("scope");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("active.jsonl");
        std::fs::write(&path, "session").unwrap();
        let lock = lock_session_path(&path).unwrap();
        let error = delete_session_path(&path, "active").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot delete an associated session"),
            "{error:#}"
        );
        assert!(path.exists());
        drop(lock);
        delete_session_path(&path, "active").unwrap();
        assert!(!path.exists());
        assert!(!path.with_extension("lock").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_removes_stale_locks_but_preserves_active_and_unrelated_files() {
        let root = std::env::temp_dir().join(format!("phi-session-{}", uuid::Uuid::new_v4()));
        let directory = root.join("scope");
        std::fs::create_dir_all(&directory).unwrap();
        let active_path =
            directory.join(format!("20260925T103424Z-{}.jsonl", uuid::Uuid::new_v4()));
        let stale_path = directory.join(format!("20260925T103425Z-{}.lock", uuid::Uuid::new_v4()));
        let unrelated_path = directory.join("unrelated.lock");
        let active_lock = lock_session_path(&active_path).unwrap();
        std::fs::write(&stale_path, "").unwrap();
        std::fs::write(&unrelated_path, "").unwrap();

        cleanup_stale_session_locks(&directory).unwrap();
        assert!(active_path.with_extension("lock").exists());
        assert!(!stale_path.exists());
        assert!(unrelated_path.exists());

        drop(active_lock);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn session_crash_releases_ownership_without_manual_cleanup() {
        const CHILD_PATH: &str = "PHI_TEST_SESSION_LOCK_CRASH_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let path = PathBuf::from(path);
            let _lock = lock_session_path(&path).unwrap();
            std::fs::write(path.with_extension("ready"), "ready").unwrap();
            std::future::pending::<()>().await;
            return;
        }
        let root = std::env::temp_dir().join(format!("phi-session-{}", uuid::Uuid::new_v4()));
        let path = root.join(format!("20260925T103424Z-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&path, "session").unwrap();
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "session::storage::tests::session_crash_releases_ownership_without_manual_cleanup",
                "--nocapture",
            ])
            .env(CHILD_PATH, &path)
            .env("PHI_CONFIG_DIR", &root)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !path.with_extension("ready").exists() {
                assert!(
                    child.try_wait().unwrap().is_none(),
                    "lock holder exited before readiness"
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(lock_session_path(&path).is_err());
        assert!(delete_session_path(&path, "active").is_err());
        cleanup_stale_session_locks(&root).unwrap();
        assert!(path.with_extension("lock").exists());
        child.kill().await.unwrap();
        assert!(path.with_extension("lock").exists());
        let recovered = lock_session_path(&path).unwrap();
        cleanup_stale_session_locks(&root).unwrap();
        assert!(path.with_extension("lock").exists());
        drop(recovered);
        assert!(!path.with_extension("lock").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cleanup_does_not_create_missing_directory() {
        let root = std::env::temp_dir().join(format!("phi-session-{}", uuid::Uuid::new_v4()));
        let directory = root.join("scope");
        cleanup_stale_session_locks(&directory).unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn ordinary_children_follow_parent_storage_not_execution_directory() {
        let mut parent = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        parent.working_dir = "/tmp/phi-parent".into();
        parent.created_at = "2026-10-01T10:00:00Z".into();
        assert_eq!(
            session_file_name(&parent),
            format!("20261001T100000Z-{}.jsonl", parent.id)
        );
        let mut directory = session_path(&parent).parent().unwrap().to_path_buf();
        for depth in 0..super::super::model::MAX_SIDECHAIN_DEPTH {
            let mut child = delegated_session(&parent);
            child.working_dir = format!("/tmp/phi-other-worktree-{depth}").into();
            directory = directory.join(parent.id.as_str()).join("subagents");
            assert_eq!(
                session_path(&child),
                directory.join(format!("agent-{}.jsonl", child.id))
            );
            child.validate_storage().unwrap();
            parent = child;
        }
        assert!(delegated_session(&parent).validate_storage().is_err());
    }

    #[test]
    fn execution_failure_is_saved_without_an_assistant_reply() {
        let root =
            PathBuf::from("/tmp").join(format!("phi-failure-history-{}", uuid::Uuid::new_v4()));
        let failure = phi_sdk::PhiFailure::new(
            phi_sdk::PhiFailureKind::Authentication,
            "provider rejected credentials",
            None,
        );
        for turn in [None, Some("failed-turn")] {
            let mut session = crate::session::new(
                "provider",
                "model",
                128_000,
                &std::env::current_dir().unwrap(),
            );
            if turn.is_some() {
                session.append_user_message("inspect");
            }
            let failure_id = session
                .transcript
                .append_entry(TranscriptEntryKind::Failure {
                    turn_id: turn.map(Into::into),
                    failure: failure.clone(),
                });
            let path = root.join(session_file_name(&session));
            for _ in 0..2 {
                save_session_at_path(&session, &session.transcript, &path).unwrap();
                let restored = journal::load_session_file(&path).unwrap();
                assert_eq!(restored.transcript.leaf_id.as_ref(), Some(&failure_id));
                let failures = restored
                    .transcript
                    .entries
                    .iter()
                    .filter(|entry| {
                        matches!(
                            &entry.kind,
                            TranscriptEntryKind::Failure { turn_id, failure: stored }
                                if turn_id.as_deref() == turn && stored == &failure
                        )
                    })
                    .count();
                assert_eq!(failures, 1);
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workflow_references_retain_metadata_without_conversation() {
        let root =
            PathBuf::from("/tmp").join(format!("phi-workflow-metadata-{}", uuid::Uuid::new_v4()));
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let path = root.join(session_file_name(&session));
        save_session_at_path(&session, &session.transcript, &path).unwrap();
        assert!(!path.exists());
        for _ in 0..8 {
            session
                .workflow_run_ids
                .push(uuid::Uuid::new_v4().to_string().parse().unwrap());
            save_session_at_path(&session, &session.transcript, &path).unwrap();
            let restored = journal::load_session_file(&path).unwrap();
            assert_eq!(restored.workflow_run_ids, session.workflow_run_ids);
            assert!(restored.transcript.entries.is_empty());
        }
        session.workflow_run_ids.clear();
        save_session_at_path(&session, &session.transcript, &path).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn delegated_session(parent: &Session) -> Session {
        let mut child = crate::session::new(
            &parent.provider,
            &parent.model,
            parent.context_window,
            &std::env::current_dir().unwrap(),
        );
        child.working_dir = parent.working_dir.clone();
        child.bind_test_parent(parent);
        child
    }

    #[test]
    fn delegated_session_save_and_followup_round_trip_one_file() {
        let root = PathBuf::from("/tmp").join(format!("phi-child-save-{}", uuid::Uuid::new_v4()));
        let parent = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let parent_path = root.join(session_file_name(&parent));
        save_session_at_path(&parent, &parent.transcript, &parent_path).unwrap();
        assert!(!parent_path.exists());
        let mut child = delegated_session(&parent);
        child.validate_storage().unwrap();
        let filename = session_file_name(&child);
        let path = root.join(&filename);
        save_session_at_path(&child, &child.transcript, &path).unwrap();
        let empty = journal::load_session_file(&path).unwrap();
        assert!(empty.transcript.entries.is_empty());
        assert_eq!(empty.agent_kind, child.agent_kind);
        assert_eq!(empty.sidechain, child.sidechain);
        assert_eq!(empty.created_at, child.created_at);
        child.append_user_message("Review the changes");
        save_session_at_path(&child, &child.transcript, &path).unwrap();
        assert_eq!(
            journal::load_session_file(&path).unwrap().agent_kind,
            child.agent_kind
        );
        child.append_user_message("Review the followup");
        assert_eq!(session_file_name(&child), filename);
        save_session_at_path(&child, &child.transcript, &path).unwrap();
        let loaded = journal::load_session_file(&path).unwrap();
        assert_eq!(loaded.agent_kind, child.agent_kind);
        assert_eq!(loaded.created_at, child.created_at);
        assert_eq!(
            serde_json::to_value(&loaded.transcript).unwrap(),
            serde_json::to_value(&child.transcript).unwrap()
        );
        save_session_at_path(&empty, &empty.transcript, &path).unwrap();
        assert_eq!(
            journal::load_session_file(&path)
                .unwrap()
                .transcript
                .entries
                .len(),
            loaded.transcript.entries.len()
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleting_parent_removes_sidechains_only_after_all_locks_are_released() {
        let root =
            std::env::temp_dir().join(format!("phi-sidechain-locks-{}", uuid::Uuid::new_v4()));
        let parent_path = root.join("20261001T100000Z-parent.jsonl");
        let child_path = root.join("parent/subagents/agent-child.jsonl");
        let orphan_lock = root.join("parent/subagents/child/subagents/agent-orphan.lock");
        std::fs::create_dir_all(orphan_lock.parent().unwrap()).unwrap();
        std::fs::write(&parent_path, "parent").unwrap();
        std::fs::write(&child_path, "child").unwrap();
        std::fs::write(&orphan_lock, "").unwrap();
        let active = lock_session_path(&child_path).unwrap();
        cleanup_stale_session_locks(&root).unwrap();
        assert!(!orphan_lock.exists());
        assert!(child_path.with_extension("lock").exists());
        assert!(delete_session_path(&parent_path, "parent").is_err());
        assert!(parent_path.exists());
        assert!(child_path.exists());
        drop(active);

        let nested_path = orphan_lock.with_extension("jsonl");
        std::fs::write(&nested_path, "nested").unwrap();
        let nested_lock = lock_session_path(&nested_path).unwrap();
        assert!(delete_session_path(&parent_path, "parent").is_err());
        assert!(parent_path.exists());
        assert!(child_path.exists());
        assert!(nested_path.exists());
        drop(nested_lock);

        let unrelated_path = root.join("20261001T100000Z-unrelated.jsonl");
        std::fs::write(&unrelated_path, "unrelated").unwrap();
        delete_session_path(&parent_path, "parent").unwrap();
        assert!(!parent_path.exists());
        assert!(!root.join("parent").exists());
        assert!(unrelated_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn deleting_child_removes_only_its_own_sidechain() {
        let root = PathBuf::from("/tmp").join(format!("phi-child-delete-{}", uuid::Uuid::new_v4()));
        let parent_path = root.join("20261001T100000Z-parent.jsonl");
        let children = root.join("parent/subagents");
        let child_path = children.join("agent-child.jsonl");
        let sibling_path = children.join("agent-sibling.jsonl");
        let nested_path = children.join("child/subagents/agent-nested.jsonl");
        std::fs::create_dir_all(nested_path.parent().unwrap()).unwrap();
        for path in [&parent_path, &child_path, &sibling_path, &nested_path] {
            std::fs::write(path, "session").unwrap();
        }

        delete_session_path(&child_path, "child").unwrap();

        assert!(!child_path.exists());
        assert!(!children.join("child").exists());
        assert!(parent_path.exists());
        assert!(sibling_path.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn teammate_storage_is_independent_but_its_ordinary_child_is_a_sidechain() {
        let parent = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let mut teammate = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        teammate.working_dir = "/tmp/phi-teammate".into();
        teammate.agent_kind = phi_sdk::agents::AgentKind::Teammate {
            creator: phi_sdk::agent_instance::AgentInstanceId::new(parent.id.as_str()),
            team: serde_json::from_value(serde_json::json!("team")).unwrap(),
        };
        teammate.definition = Some("reviewer".into());
        teammate.alias = Some(phi_sdk::agent_manager::AgentAlias::new("security").unwrap());
        teammate.validate_parent(&parent).unwrap();
        assert_eq!(
            session_path(&teammate).parent().unwrap(),
            session_scope_dir(&teammate.working_dir)
        );
        let mut child = delegated_session(&teammate);
        child.working_dir = "/tmp/phi-unrelated".into();
        child.validate_parent(&teammate).unwrap();
        assert_eq!(
            session_path(&child),
            session_scope_dir(&teammate.working_dir)
                .join(teammate.id.as_str())
                .join("subagents")
                .join(format!("agent-{}.jsonl", child.id))
        );
        assert!(child.validate_parent(&parent).is_err());
        let encoded = serde_json::to_value(&teammate).unwrap();
        assert!(encoded.get("creator_id").is_none());
        assert_eq!(encoded["agent_kind"]["kind"], "teammate");
        assert_eq!(encoded["alias"], "security");
        assert_eq!(encoded["definition"], "reviewer");
        let restored: Session = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored.agent_kind, teammate.agent_kind);
    }

    #[test]
    fn unsafe_or_inconsistent_saved_storage_cannot_be_written() {
        let parent = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let child = delegated_session(&parent);
        assert!(save_session_with_transcript(&child, &Transcript::new()).is_err());
        let encoded = serde_json::to_value(&child).unwrap();
        let oversized = "a".repeat(257);
        for invalid in [
            "",
            ".",
            "..",
            "../escape",
            "nested/id",
            "absolute\\\\id",
            oversized.as_str(),
        ] {
            for pointer in ["/id", "/sidechain/ancestors/0", "/agent_kind/creator"] {
                let mut candidate = encoded.clone();
                *candidate.pointer_mut(pointer).unwrap() = serde_json::json!(invalid);
                let candidate: Session = serde_json::from_value(candidate).unwrap();
                assert!(
                    candidate.validate_storage().is_err(),
                    "{pointer}: {invalid}"
                );
            }
        }
        for (pointer, value) in [
            ("/sidechain/working_dir", serde_json::json!("relative")),
            ("/sidechain/ancestors", serde_json::json!([])),
            (
                "/sidechain/ancestors",
                serde_json::json!([parent.id, parent.id]),
            ),
            ("/sidechain", serde_json::Value::Null),
        ] {
            let mut candidate = encoded.clone();
            *candidate.pointer_mut(pointer).unwrap() = value;
            let candidate: Session = serde_json::from_value(candidate).unwrap();
            assert!(candidate.validate_storage().is_err(), "{pointer}");
        }
    }

    #[test]
    fn recognizes_only_session_filenames() {
        for extension in ["jsonl", "lock"] {
            for name in ["agent-child-instance", "20261001T100000Z-child-instance"] {
                assert_eq!(
                    session_file_id(Path::new(&format!("{name}.{extension}"))),
                    Some("child-instance")
                );
            }
        }
        for name in [
            "unrelated.lock",
            "unrelated-child-instance.jsonl",
            "agent-.lock",
            "agent-child-instance.tmp",
            "20261301T100000Z-child-instance.jsonl",
        ] {
            assert_eq!(session_file_id(Path::new(name)), None, "{name}");
        }
    }
}
