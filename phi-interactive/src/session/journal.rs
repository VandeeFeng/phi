use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use phi_sdk::transcript::{SwitchSnapshotRecord, Transcript};

use super::{ChildSessionLink, PermissionAllowEntry, Session, TranscriptEntry};

#[derive(Clone, Default)]
struct JournalCursor {
    entry_count: usize,
    last_entry_id: Option<CompactString>,
    snapshot_count: usize,
    last_snapshot_id: Option<CompactString>,
    permissions: Vec<PermissionAllowEntry>,
    file_len: u64,
    truncated_tail: bool,
}

fn journal_cursors() -> &'static Mutex<HashMap<PathBuf, JournalCursor>> {
    static CURSORS: OnceLock<Mutex<HashMap<PathBuf, JournalCursor>>> = OnceLock::new();
    CURSORS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn load_session_file(path: &Path) -> anyhow::Result<Session> {
    let _lock = crate::file_lock::FileLock::acquire(&path.with_extension("journal-lock"))?;
    session_from_jsonl(BufReader::new(std::fs::File::open(path)?))
}

pub(super) fn remove_empty_checkpoint(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _lock = crate::file_lock::FileLock::acquire(&path.with_extension("journal-lock"))?;
    if !path.exists() {
        return Ok(());
    }
    let saved = session_from_jsonl(BufReader::new(std::fs::File::open(path)?))?;
    if saved.transcript.entries.is_empty() && saved.children.is_empty() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

pub(super) fn append_or_checkpoint_session(
    path: &Path,
    session: &Session,
    transcript: &Transcript,
) -> anyhow::Result<()> {
    let _lock = crate::file_lock::FileLock::acquire(&path.with_extension("journal-lock"))?;
    let mut cursors = journal_cursors()
        .lock()
        .map_err(|_| anyhow::anyhow!("session journal cursor lock poisoned"))?;
    let file_len = path.metadata().map(|metadata| metadata.len()).unwrap_or(0);
    let cached = cursors
        .get(path)
        .filter(|cursor| cursor.file_len == file_len);
    let cursor = match cached {
        Some(cursor) => cursor.clone(),
        None if path.exists() => scan_journal_cursor(path)?,
        None => JournalCursor::default(),
    };
    let merged;
    let persisted = if !path.exists() {
        write_session_checkpoint(path, session, transcript)?;
        transcript
    } else if cached.is_some() && cursor_matches_session(&cursor, transcript) {
        append_session_records(path, session, transcript, &cursor)?;
        transcript
    } else {
        let saved = session_from_jsonl(BufReader::new(std::fs::File::open(path)?))?;
        anyhow::ensure!(saved.id == session.id, "session journal identity mismatch");
        merged = merge_transcripts(&saved.transcript, transcript)?;
        let mut envelope = session.clone();
        for child in saved.children {
            if !envelope
                .children
                .iter()
                .any(|existing| existing.child_session_id == child.child_session_id)
            {
                envelope.children.push(child);
            }
        }
        for run in saved.workflow_run_ids {
            if !envelope.workflow_run_ids.contains(&run) {
                envelope.workflow_run_ids.push(run);
            }
        }
        if cursor.truncated_tail {
            write_session_checkpoint(path, &envelope, &merged)?;
        } else {
            append_session_records(path, &envelope, &merged, &cursor)?;
        }
        if envelope.children.len() != session.children.len()
            || envelope.workflow_run_ids.len() != session.workflow_run_ids.len()
        {
            cursors.remove(path);
            return Ok(());
        }
        &merged
    };
    let mut updated = cursor_from_session(session, persisted);
    updated.file_len = path.metadata()?.len();
    cursors.insert(path.to_path_buf(), updated);
    Ok(())
}

fn merge_transcripts(saved: &Transcript, incoming: &Transcript) -> anyhow::Result<Transcript> {
    let mut merged = saved.clone();
    let mut snapshots: HashMap<_, _> = saved
        .switch_snapshots
        .iter()
        .map(|record| (&record.id, record))
        .collect();
    for record in &incoming.switch_snapshots {
        if let Some(existing) = snapshots.insert(&record.id, record) {
            anyhow::ensure!(
                existing == record,
                "conflicting Switch snapshot {}",
                record.id
            );
        } else {
            merged.switch_snapshots.push(record.clone());
        }
    }
    let mut entries: HashSet<_> = saved.entries.iter().map(|entry| &entry.id).collect();
    for entry in &incoming.entries {
        if !entries.insert(&entry.id) {
            continue;
        }
        if let phi_sdk::transcript::TranscriptEntryKind::Message {
            message: phi_sdk::transcript::AgentMessage::Assistant { usage, .. },
        } = &entry.kind
        {
            merged.total_tokens = merged.total_tokens.saturating_add(usage.output);
            merged.total_cache_read = merged.total_cache_read.saturating_add(usage.cache_read);
            merged.total_cache_write = merged.total_cache_write.saturating_add(usage.cache_write);
            merged.total_cost += usage.cost;
        }
        merged.entries.push(entry.clone());
        merged.updated_at = incoming.updated_at.clone();
    }
    merged.leaf_id = merged.entries.last().map(|entry| entry.id.clone());
    merged.effective_context()?;
    merged.validate_switch_snapshots()?;
    Ok(merged)
}

fn cursor_matches_session(cursor: &JournalCursor, transcript: &Transcript) -> bool {
    !cursor.truncated_tail
        && cursor.snapshot_count <= transcript.switch_snapshots.len()
        && cursor.last_snapshot_id.as_ref()
            == cursor
                .snapshot_count
                .checked_sub(1)
                .and_then(|index| transcript.switch_snapshots.get(index))
                .map(|record| &record.id)
        && cursor.entry_count <= transcript.entries.len()
        && cursor.last_entry_id.as_ref()
            == cursor
                .entry_count
                .checked_sub(1)
                .and_then(|index| transcript.entries.get(index))
                .map(|entry| &entry.id)
}

fn cursor_from_session(session: &Session, transcript: &Transcript) -> JournalCursor {
    JournalCursor {
        entry_count: transcript.entries.len(),
        last_entry_id: transcript.entries.last().map(|entry| entry.id.clone()),
        snapshot_count: transcript.switch_snapshots.len(),
        last_snapshot_id: transcript
            .switch_snapshots
            .last()
            .map(|record| record.id.clone()),
        permissions: session.permission_allowlist.clone(),
        file_len: 0,
        truncated_tail: false,
    }
}

fn visit_jsonl_entries(
    mut reader: impl BufRead,
    mut visit: impl FnMut(SessionJsonlEntry),
) -> anyhow::Result<bool> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(false);
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(entry) => visit(entry),
            Err(error) if !line.ends_with('\n') && error.is_eof() => return Ok(true),
            Err(error) => return Err(error.into()),
        }
        if !line.ends_with('\n') {
            return Ok(true);
        }
    }
}

fn scan_journal_cursor(path: &Path) -> anyhow::Result<JournalCursor> {
    let mut cursor = JournalCursor::default();
    cursor.truncated_tail = visit_jsonl_entries(
        BufReader::new(std::fs::File::open(path)?),
        |record| match record {
            SessionJsonlEntry::Entry { entry } => {
                cursor.entry_count += 1;
                cursor.last_entry_id = Some(entry.id);
            }
            SessionJsonlEntry::PermissionSnapshot { entries } => cursor.permissions = entries,
            SessionJsonlEntry::SwitchSnapshot { record } => {
                cursor.snapshot_count += 1;
                cursor.last_snapshot_id = Some(record.id);
            }
            SessionJsonlEntry::Metadata { .. } => {}
        },
    )?;
    cursor.file_len = path.metadata()?.len();
    Ok(cursor)
}

fn append_session_records(
    path: &Path,
    session: &Session,
    transcript: &Transcript,
    cursor: &JournalCursor,
) -> anyhow::Result<()> {
    let file = std::fs::OpenOptions::new().append(true).open(path)?;
    let mut writer = BufWriter::new(file);
    if cursor.permissions != session.permission_allowlist {
        write_jsonl_entry(
            &mut writer,
            &SessionJsonlEntryRef::PermissionSnapshot {
                entries: &session.permission_allowlist,
            },
        )?;
    }
    for record in &transcript.switch_snapshots[cursor.snapshot_count..] {
        write_jsonl_entry(
            &mut writer,
            &SessionJsonlEntryRef::SwitchSnapshot { record },
        )?;
    }
    for entry in &transcript.entries[cursor.entry_count..] {
        write_jsonl_entry(&mut writer, &SessionJsonlEntryRef::Entry { entry })?;
    }
    write_metadata(&mut writer, session, transcript)?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}

fn write_session_checkpoint(
    path: &Path,
    session: &Session,
    transcript: &Transcript,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("session path has no parent"))?;
    let temporary = parent.join(format!(".session-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let file = std::fs::File::create(&temporary)?;
        let mut writer = BufWriter::new(file);
        write_checkpoint(&mut writer, session, transcript)?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok::<_, anyhow::Error>(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn write_checkpoint(
    writer: &mut impl Write,
    session: &Session,
    transcript: &Transcript,
) -> anyhow::Result<()> {
    write_metadata(writer, session, transcript)?;
    write_jsonl_entry(
        writer,
        &SessionJsonlEntryRef::PermissionSnapshot {
            entries: &session.permission_allowlist,
        },
    )?;
    for record in &transcript.switch_snapshots {
        write_jsonl_entry(writer, &SessionJsonlEntryRef::SwitchSnapshot { record })?;
    }
    for entry in &transcript.entries {
        write_jsonl_entry(writer, &SessionJsonlEntryRef::Entry { entry })?;
    }
    write_metadata(writer, session, transcript)
}

fn write_metadata(
    writer: &mut impl Write,
    session: &Session,
    transcript: &Transcript,
) -> anyhow::Result<()> {
    let metadata = SessionMetadata::from_session(session, transcript);
    write_jsonl_entry(
        writer,
        &SessionJsonlEntryRef::Metadata {
            metadata: &metadata,
        },
    )
}

fn write_jsonl_entry(writer: &mut impl Write, entry: &impl Serialize) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *writer, entry)?;
    writer.write_all(b"\n")?;
    Ok(())
}

fn session_from_jsonl(reader: impl BufRead) -> anyhow::Result<Session> {
    let mut metadata = None;
    let mut permission_allowlist = Vec::new();
    let mut entries = Vec::new();
    let mut switch_snapshots = Vec::new();

    visit_jsonl_entries(reader, |record| match record {
        SessionJsonlEntry::Metadata {
            metadata: session_metadata,
        } => metadata = Some(session_metadata),
        SessionJsonlEntry::PermissionSnapshot { entries } => {
            permission_allowlist = entries;
        }
        SessionJsonlEntry::Entry { entry } => entries.push(entry),
        SessionJsonlEntry::SwitchSnapshot { record } => switch_snapshots.push(record),
    })?;

    let metadata = metadata.ok_or_else(|| anyhow::anyhow!("missing session metadata"))?;
    let transcript = Transcript {
        id: metadata.id,
        leaf_id: entries.last().map(|entry| entry.id.clone()),
        entries,
        switch_snapshots,
        created_at: metadata.created_at,
        updated_at: metadata.updated_at,
        total_tokens: metadata.total_tokens,
        total_cost: metadata.total_cost,
        total_cache_read: metadata.total_cache_read,
        total_cache_write: metadata.total_cache_write,
    };
    transcript.effective_context()?;
    transcript.validate_switch_snapshots()?;
    Ok(Session {
        total_estimated_tokens: transcript.estimated_context_tokens(),
        transcript,
        name: metadata.name,
        context_window: metadata.context_window,
        model: metadata.model,
        current_switch: metadata.current_switch,
        provider: metadata.provider,
        working_dir: metadata.working_dir,
        task_list_id: metadata.task_list_id,
        agent_kind: metadata.agent_kind,
        definition: metadata.definition,
        alias: metadata.alias,
        sidechain: metadata.sidechain,
        children: metadata.children,
        workflow_run_ids: metadata.workflow_run_ids,
        permission_allowlist,
    })
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) struct SessionMetadata {
    pub(super) id: CompactString,
    pub(super) name: CompactString,
    pub(super) created_at: CompactString,
    pub(super) updated_at: CompactString,
    pub(super) total_tokens: u64,
    pub(super) total_cost: f64,
    #[serde(default)]
    pub(super) total_cache_read: u64,
    #[serde(default)]
    pub(super) total_cache_write: u64,
    #[serde(default)]
    pub(super) total_estimated_tokens: u64,
    pub(super) context_window: u64,
    pub(super) model: CompactString,
    #[serde(default = "crate::session::default_switch")]
    pub(super) current_switch: CompactString,
    pub(super) provider: CompactString,
    pub(super) working_dir: CompactString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) task_list_id: Option<CompactString>,
    pub(super) agent_kind: phi_sdk::agents::AgentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) definition: Option<CompactString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) alias: Option<phi_sdk::agent_manager::AgentAlias>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) sidechain: Option<super::SessionSidechain>,
    #[serde(default)]
    pub(super) children: Vec<ChildSessionLink>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) workflow_run_ids: Vec<phi_sdk::workflow::WorkflowRunId>,
}

impl SessionMetadata {
    fn from_session(session: &Session, transcript: &Transcript) -> Self {
        Self {
            id: transcript.id.clone(),
            name: session.name.clone(),
            created_at: transcript.created_at.clone(),
            updated_at: transcript.updated_at.clone(),
            total_tokens: transcript.total_tokens,
            total_cost: transcript.total_cost,
            total_cache_read: transcript.total_cache_read,
            total_cache_write: transcript.total_cache_write,
            total_estimated_tokens: transcript.estimated_context_tokens(),
            context_window: session.context_window,
            model: session.model.clone(),
            current_switch: session.current_switch.clone(),
            provider: session.provider.clone(),
            working_dir: session.working_dir.clone(),
            task_list_id: session.task_list_id.clone(),
            agent_kind: session.agent_kind.clone(),
            definition: session.definition.clone(),
            alias: session.alias.clone(),
            sidechain: session.sidechain.clone(),
            children: session.children.clone(),
            workflow_run_ids: session.workflow_run_ids.clone(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum SessionJsonlEntry {
    Metadata {
        #[serde(flatten)]
        metadata: SessionMetadata,
    },
    PermissionSnapshot {
        entries: Vec<PermissionAllowEntry>,
    },
    Entry {
        entry: TranscriptEntry,
    },
    SwitchSnapshot {
        #[serde(flatten)]
        record: SwitchSnapshotRecord,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SessionJsonlEntryRef<'a> {
    Metadata {
        #[serde(flatten)]
        metadata: &'a SessionMetadata,
    },
    PermissionSnapshot {
        entries: &'a [PermissionAllowEntry],
    },
    Entry {
        entry: &'a TranscriptEntry,
    },
    SwitchSnapshot {
        #[serde(flatten)]
        record: &'a SwitchSnapshotRecord,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use phi_sdk::transcript::SharedText;
    use std::io::Cursor;

    #[tokio::test]
    async fn cross_process_appends_keep_every_branch() {
        const CHILD_DIRECTORY: &str = "PHI_TEST_JOURNAL_DIRECTORY";
        const CHILD_INDEX: &str = "PHI_TEST_JOURNAL_WRITER";
        const WRITERS: usize = 3;
        const MESSAGES: usize = 8;
        if let Some(directory) = std::env::var_os(CHILD_DIRECTORY) {
            let directory = PathBuf::from(directory);
            let path = directory.join("session.jsonl");
            let writer = std::env::var(CHILD_INDEX).unwrap();
            let _lease =
                crate::file_lock::FileLock::try_acquire_shared(&path.with_extension("lock"))
                    .unwrap();
            let mut session = load_session_file(&path).unwrap();
            std::fs::write(directory.join(format!("ready-{writer}")), "").unwrap();
            while !directory.join("start").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            for message in 0..MESSAGES {
                session.append_user_message(&format!("writer {writer}, message {message}"));
                append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
            }
            for _ in 0..2 {
                append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
            }
            assert_eq!(session.entries.len(), MESSAGES + 1);
            return;
        }
        let directory =
            PathBuf::from("/tmp").join(format!("phi-journal-processes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("session.jsonl");
        let mut initial =
            super::super::new("local", "model", 128_000, &std::env::current_dir().unwrap());
        initial.append_user_message("common ancestor");
        append_or_checkpoint_session(&path, &initial, &initial.transcript).unwrap();
        let mut children = Vec::new();
        for writer in 0..WRITERS {
            children.push(
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "session::journal::tests::cross_process_appends_keep_every_branch",
                    ])
                    .env(CHILD_DIRECTORY, &directory)
                    .env(CHILD_INDEX, writer.to_string())
                    .kill_on_drop(true)
                    .stdout(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !(0..WRITERS).all(|writer| directory.join(format!("ready-{writer}")).exists()) {
                for child in &mut children {
                    assert!(child.try_wait().unwrap().is_none());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert!(crate::file_lock::FileLock::try_acquire(&path.with_extension("lock")).is_err());
        std::fs::write(directory.join("start"), "").unwrap();
        for child in children {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
                    .await
                    .unwrap()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let saved = load_session_file(&path).unwrap();
        assert_eq!(saved.entries.len(), WRITERS * MESSAGES + 1);
        assert_eq!(
            saved
                .entries
                .iter()
                .map(|entry| &entry.id)
                .collect::<HashSet<_>>()
                .len(),
            saved.entries.len()
        );
        let ancestor = &initial.entries[0].id;
        assert_eq!(
            saved
                .entries
                .iter()
                .filter(|entry| entry.parent_id.as_ref() == Some(ancestor))
                .count(),
            WRITERS
        );
        assert_eq!(
            saved.effective_context().unwrap().entries.len(),
            MESSAGES + 1
        );
        drop(crate::file_lock::FileLock::try_acquire(&path.with_extension("lock")).unwrap());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn switch_snapshots_append_once_before_references_and_survive_reopen() {
        use phi_sdk::transcript::{AgentMessage, SwitchSnapshot, TranscriptEntryKind};
        let root =
            std::env::temp_dir().join(format!("phi-switch-journal-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("session.jsonl");
        let mut session =
            super::super::new("local", "model", 128_000, &std::env::current_dir().unwrap());
        session
            .transcript
            .switch_snapshots
            .push(SwitchSnapshotRecord {
                id: "review-1".into(),
                switch: SwitchSnapshot {
                    name: "review".into(),
                    provider: "local".into(),
                    model: "model".into(),
                    thinking_level: Some("high".into()),
                    mode: "plan".into(),
                    prompt: Some("unique original prompt".into()),
                },
            });
        let append_response = |session: &mut Session, snapshot: &str| {
            session
                .transcript
                .append_entry(TranscriptEntryKind::Message {
                    message: AgentMessage::Assistant {
                        id: None,
                        turn_id: None,
                        content: Vec::new(),
                        provider: "local".into(),
                        api: None,
                        model: "model".into(),
                        switch: Some(snapshot.into()),
                        usage: Default::default(),
                        stop_reason: "stop".into(),
                    },
                });
        };
        session.transcript.append_user_message("first request");
        append_response(&mut session, "review-1");
        append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
        let prefix = std::fs::read(&path).unwrap();
        session
            .transcript
            .switch_snapshots
            .push(SwitchSnapshotRecord {
                id: "review-2".into(),
                switch: SwitchSnapshot {
                    prompt: Some("unique revised prompt".into()),
                    ..session.transcript.switch_snapshots[0].switch.clone()
                },
            });
        append_response(&mut session, "review-2");
        append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
        assert!(std::fs::read(&path).unwrap().starts_with(&prefix));
        let mut reopened = load_session_file(&path).unwrap();
        journal_cursors().lock().unwrap().remove(&path);
        reopened
            .transcript
            .select_branch(session.entries.first().map(|entry| entry.id.as_str()))
            .unwrap();
        append_response(&mut reopened, "review-1");
        append_or_checkpoint_session(&path, &reopened, &reopened.transcript).unwrap();
        append_or_checkpoint_session(&path, &reopened, &reopened.transcript).unwrap();
        let journal = std::fs::read_to_string(&path).unwrap();
        assert_eq!(journal.matches("unique original prompt").count(), 1);
        assert_eq!(journal.matches("unique revised prompt").count(), 1);
        let mut known = std::collections::HashSet::new();
        for line in journal.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record["type"] == "switch_snapshot" {
                assert!(known.insert(record["id"].as_str().unwrap().to_string()));
            } else if let Some(reference) = record["entry"]["message"]["switch"].as_str() {
                assert!(known.contains(reference));
            }
        }
        assert_eq!(known.len(), 2);
        let loaded = load_session_file(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded.transcript).unwrap(),
            serde_json::to_value(&reopened.transcript).unwrap()
        );
        let records: Vec<serde_json::Value> = journal
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let missing = records
            .iter()
            .filter(|record| record["id"] != "review-1")
            .map(|record| format!("{record}\n"))
            .collect::<String>();
        assert!(session_from_jsonl(Cursor::new(missing)).is_err());
        let duplicate = format!(
            "{journal}{}\n",
            records
                .iter()
                .find(|record| record["type"] == "switch_snapshot")
                .unwrap()
        );
        assert!(session_from_jsonl(Cursor::new(duplicate)).is_err());
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"switch_snapshot\",\"id\":\"review-3\"")
            .unwrap();
        drop(file);
        append_or_checkpoint_session(&path, &reopened, &reopened.transcript).unwrap();
        let repaired = load_session_file(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&repaired.transcript).unwrap(),
            serde_json::to_value(&reopened.transcript).unwrap()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn task_list_reference_roundtrips_without_storage_or_task_state() {
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let binding =
            phi_sdk::tasks::TaskListBinding::new("/tmp/phi-task-records", "shared-review").unwrap();
        session.bind_task_list(Some(&binding)).unwrap();
        session
            .transcript
            .append_user_message("Review the changes.");
        let mut bytes = Vec::new();
        write_checkpoint(&mut bytes, &session, &session.transcript).unwrap();
        let saved = String::from_utf8(bytes.clone()).unwrap();
        assert!(!saved.contains("/tmp/phi-task-records"));
        assert!(!saved.contains("blocked_by"));
        let loaded = session_from_jsonl(Cursor::new(bytes)).unwrap();
        assert_eq!(loaded.task_list_id.as_deref(), Some("shared-review"));
        assert_eq!(loaded.transcript.leaf_id, session.transcript.leaf_id);
        let mut restored = loaded;
        for conflicting in [
            None,
            Some(phi_sdk::tasks::TaskListBinding::new("/tmp/phi-task-records", "other").unwrap()),
        ] {
            assert!(restored.bind_task_list(conflicting.as_ref()).is_err());
            assert_eq!(restored.task_list_id.as_deref(), Some("shared-review"));
        }
    }

    #[test]
    fn context_edits_survive_incremental_journal_and_checkpoint_without_rewriting_sources() {
        use phi_sdk::transcript::{ContextContent, ContextReplacement};
        let path = PathBuf::from(format!(
            "/tmp/phi-context-edit-journal-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.append_user_message("original question");
        let target = session.leaf_id.clone().unwrap();
        session.append_user_message("continue on this branch");
        let sources = serde_json::to_value(&session.entries).unwrap();
        append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
        session
            .transcript
            .append_context_edit(
                &target,
                Some(ContextReplacement {
                    content: ContextContent::Text("replacement question".into()),
                }),
            )
            .unwrap();
        append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
        let journal = std::fs::read_to_string(&path).unwrap();
        let edit = journal
            .lines()
            .rev()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|record| record["type"] == "entry")
            .unwrap();
        assert_eq!(edit["entry"]["kind"], "context_edit");
        assert_eq!(edit["entry"]["target_id"], target.as_str());
        assert_eq!(
            edit["entry"]["replacement"]["content"],
            "replacement question"
        );
        for checkpoint in [false, true] {
            if checkpoint {
                std::fs::remove_file(&path).unwrap();
                append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
            }
            let loaded = load_session_file(&path).unwrap();
            assert_eq!(loaded.leaf_id, session.leaf_id);
            assert_eq!(serde_json::to_value(&loaded.entries[..2]).unwrap(), sources);
            assert_eq!(loaded.total_tokens, session.total_tokens);
            let projected = serde_json::to_string(
                &phi_sdk::context::convert_history(&loaded.transcript).unwrap(),
            )
            .unwrap();
            assert!(projected.contains("replacement question"));
            assert!(!projected.contains("original question"));
            assert!(projected.contains("continue on this branch"));
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn navigation_cursor_never_changes_disk_resume() {
        let path = PathBuf::from(format!(
            "/tmp/phi-navigation-journal-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.append_user_message("first branch");
        let first = session.leaf_id.clone().unwrap();
        session.append_user_message("last appended entry");
        let durable_leaf = session.leaf_id.clone();
        let original = serde_json::to_value(&session.entries).unwrap();
        append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();

        for target in [None, Some(first.as_str())] {
            session.select_branch(target).unwrap();
            for checkpoint in [false, true] {
                if checkpoint {
                    std::fs::remove_file(&path).unwrap();
                }
                session.name = "renamed while navigating".into();
                append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
                let loaded = load_session_file(&path).unwrap();
                assert_eq!(loaded.leaf_id, durable_leaf);
                assert_eq!(serde_json::to_value(&loaded.entries).unwrap(), original);
                assert_eq!(loaded.name, session.name);
                for line in std::fs::read_to_string(&path).unwrap().lines() {
                    let record: serde_json::Value = serde_json::from_str(line).unwrap();
                    if record["type"] == "metadata" {
                        assert!(record.get("leaf_id").is_none());
                    }
                }
            }
        }

        session.select_branch(Some(&first)).unwrap();
        session.append_user_message("new branch");
        let continued = session.entries.last().unwrap();
        assert_eq!(continued.parent_id.as_ref(), Some(&first));
        append_or_checkpoint_session(&path, &session, &session.transcript).unwrap();
        let loaded = load_session_file(&path).unwrap();
        assert_eq!(loaded.leaf_id, session.leaf_id);
        assert_eq!(loaded.entries.len(), 3);
        assert_eq!(
            serde_json::to_value(&loaded.entries[..2]).unwrap(),
            original
        );

        let historical = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| {
                let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
                if record["type"] == "metadata" {
                    record["leaf_id"] = serde_json::json!(first);
                }
                format!("{record}\n")
            })
            .collect::<String>();
        let loaded = session_from_jsonl(Cursor::new(historical)).unwrap();
        assert_eq!(loaded.leaf_id, session.leaf_id);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn session_kind_is_required() {
        let session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let mut bytes = Vec::new();
        write_checkpoint(&mut bytes, &session, &session.transcript).unwrap();
        for creator in [None, Some("historical-parent")] {
            let historical = String::from_utf8(bytes.clone())
                .unwrap()
                .lines()
                .map(|line| {
                    let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
                    if record["type"] == "metadata" {
                        record.as_object_mut().unwrap().remove("agent_kind");
                        if let Some(creator) = creator {
                            record["creator_id"] = serde_json::json!(creator);
                        }
                    }
                    format!("{record}\n")
                })
                .collect::<String>();
            let error = session_from_jsonl(Cursor::new(&historical)).unwrap_err();
            assert!(error.to_string().contains("agent_kind"), "{error:#}");
            assert!(
                super::super::discovery::session_summary_from_jsonl(Cursor::new(&historical))
                    .is_err()
            );
        }
        let mut value = serde_json::to_value(&session).unwrap();
        value.as_object_mut().unwrap().remove("agent_kind");
        assert!(serde_json::from_value::<Session>(value).is_err());
    }

    #[test]
    fn child_links_survive_restart_without_status_or_transcript_mirrors() {
        use crate::session::ChildSessionLink;

        let root = std::env::temp_dir().join(format!("phi-links-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let parent_path = root.join("parent.jsonl");
        let child_path = root.join("child.jsonl");
        let mut parent = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let mut child = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        parent.working_dir = root.join("parent-worktree").to_string_lossy().into();
        child.working_dir = root.join("deleted-worktree").to_string_lossy().into();
        parent.transcript.append_user_message("parent task");
        child.transcript.append_user_message("child task");
        let link = ChildSessionLink {
            child_session_id: child.id.clone(),
            child_agent_id: child.id.to_string(),
            child_working_dir: child.working_dir.clone(),
            definition: Some("probe".into()),
            alias: Some("security".into()),
            parent_tool_call_id: Some("agent-call".into()),
            workflow_run_id: None,
            mode: phi_sdk::agent_manager::DelegationMode::Background,
            recovered_outcome: None,
        };
        child.bind_test_parent(&parent);
        parent.children.push(link.clone());
        let saved_link = serde_json::to_value(&parent).unwrap()["children"][0].clone();
        assert_eq!(saved_link["mode"], "background");
        for field in [
            "outcome",
            "admission",
            "delivery",
            "task_id",
            "initial_run_id",
        ] {
            assert!(saved_link.get(field).is_none());
        }
        append_or_checkpoint_session(&parent_path, &parent, &parent.transcript).unwrap();
        append_or_checkpoint_session(&child_path, &child, &child.transcript).unwrap();
        parent.name = "updated parent".into();
        append_or_checkpoint_session(&parent_path, &parent, &parent.transcript).unwrap();

        let committed = std::fs::read(&parent_path).unwrap();
        for tail in [b"{\"type\":\"metadata\"".as_slice(), b""] {
            let mut torn = committed.clone();
            if tail.is_empty() {
                torn.pop();
            } else {
                torn.extend_from_slice(tail);
            }
            std::fs::write(&parent_path, torn).unwrap();
            let recovered = load_session_file(&parent_path).unwrap();
            assert_eq!(recovered.children, parent.children);
            append_or_checkpoint_session(&parent_path, &recovered, &recovered.transcript).unwrap();
            assert_eq!(
                load_session_file(&parent_path).unwrap().children,
                parent.children
            );
        }
        let loaded_parent = load_session_file(&parent_path).unwrap();
        let loaded_child = load_session_file(&child_path).unwrap();
        assert_eq!(loaded_parent.children, parent.children);
        assert_eq!(
            loaded_parent.children[0].child_working_dir,
            child.working_dir
        );
        assert_eq!(loaded_child.agent_kind, child.agent_kind);
        assert_eq!(loaded_child.sidechain, child.sidechain);
        assert_eq!(
            loaded_parent
                .transcript
                .effective_context()
                .unwrap()
                .entries
                .len(),
            1
        );
        assert_eq!(
            loaded_child
                .transcript
                .effective_context()
                .unwrap()
                .entries
                .len(),
            1
        );
        assert!(!std::path::Path::new(loaded_child.working_dir.as_str()).exists());
        assert_ne!(
            crate::session::storage::session_path(&loaded_parent).parent(),
            crate::session::storage::session_path(&loaded_child).parent()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_heads_support_ordinary_records_but_do_not_migrate_compaction() {
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.append_user_message("older");
        session.append_user_message("retained");
        let mut ordinary = Vec::new();
        write_checkpoint(&mut ordinary, &session, &session.transcript).unwrap();
        let loaded = session_from_jsonl(Cursor::new(&ordinary)).unwrap();
        assert_eq!(
            loaded.transcript.effective_context().unwrap().entries.len(),
            2
        );
        assert!(loaded.entries.iter().all(|entry| entry.head.is_none()));

        let head = session.leaf_id.clone().unwrap();
        session
            .transcript
            .compress("summary".into(), &head)
            .unwrap();
        let mut canonical = Vec::new();
        write_checkpoint(&mut canonical, &session, &session.transcript).unwrap();
        assert!(session_from_jsonl(Cursor::new(&canonical)).is_ok());
        let mut legacy = Vec::new();
        for line in String::from_utf8(canonical).unwrap().lines() {
            let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
            if record["entry"]["kind"] == "compaction" {
                let head = record["entry"]
                    .as_object_mut()
                    .unwrap()
                    .remove("head")
                    .unwrap();
                record["entry"]["first_kept_entry_id"] = head;
            }
            serde_json::to_writer(&mut legacy, &record).unwrap();
            legacy.push(b'\n');
        }
        let error = session_from_jsonl(Cursor::new(legacy)).unwrap_err();
        assert!(error.to_string().contains("compaction head"));
    }

    #[test]
    fn checkpoint_round_trips_session_data() {
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.name = CompactString::new("session");
        session.append_user_message("hello");
        session.permission_allowlist.push(PermissionAllowEntry {
            tool: String::from("read"),
            pattern: String::from("src/**"),
        });
        session.transcript.append_assistant_message(
            None,
            None,
            vec![phi_sdk::transcript::ContentBlock::ToolCall {
                id: "phi-call".into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "read".into(),
                args: serde_json::json!({}),
            }],
            "provider".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "model".into(),
            phi_sdk::transcript::Usage::default(),
            "tool_use".into(),
        );

        let mut jsonl = Vec::new();
        write_checkpoint(&mut jsonl, &session, &session.transcript).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_slice(jsonl.split(|byte| *byte == b'\n').next().unwrap()).unwrap();
        let loaded = session_from_jsonl(Cursor::new(&jsonl)).unwrap();
        let entry: serde_json::Value = jsonl
            .split(|byte| *byte == b'\n')
            .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
            .find(|line| line["type"] == "entry" && line["entry"]["message"]["role"] == "assistant")
            .unwrap();
        let call = &entry["entry"]["message"]["content"][0];

        assert_eq!(call["id"], "phi-call");
        assert!(call["provider_call_id"].is_null());
        assert!(call["provider_item_id"].is_null());
        assert!(call.get("provider_id_missing").is_none());
        assert_eq!(loaded.transcript.entries.len(), 2);
        assert_eq!(metadata["type"], "metadata");
        assert_eq!(metadata["id"], session.id.as_str());
        assert_eq!(loaded.id, session.id);
        assert_eq!(loaded.name, session.name);
        assert_eq!(
            serde_json::to_value(&loaded.entries).unwrap(),
            serde_json::to_value(&session.entries).unwrap()
        );
        assert!(
            !String::from_utf8(jsonl)
                .unwrap()
                .contains("\"type\":\"message\"")
        );
        assert_eq!(loaded.permission_allowlist[0].pattern, "src/**");
    }

    #[test]
    fn compaction_save_resume_and_navigation_use_only_canonical_entries() {
        let path = PathBuf::from(format!(
            "/tmp/phi-compaction-journal-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        let session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        let mut active = session.transcript.clone();
        active.append_user_message("old history");
        let root = active.leaf_id.clone().unwrap();
        active.append_user_message("retained history");
        let retained = active.leaf_id.clone().unwrap();
        active.total_tokens = 42;
        active.total_cost = 0.5;
        append_or_checkpoint_session(&path, &session, &active).unwrap();
        let initial = std::fs::read(&path).unwrap();
        active.compress("summary".into(), &retained).unwrap();
        active.append_agent_message("new feedback");
        append_or_checkpoint_session(&path, &session, &active).unwrap();
        let appended = std::fs::read(&path).unwrap();
        assert!(appended.starts_with(&initial));
        let mut loaded = load_session_file(&path).unwrap();
        assert_eq!(
            serde_json::to_value(&loaded.entries).unwrap(),
            serde_json::to_value(&active.entries).unwrap()
        );
        assert_eq!(loaded.total_tokens, 42);
        assert_eq!(loaded.total_cost, 0.5);
        assert_eq!(
            loaded.total_estimated_tokens,
            active.estimated_context_tokens()
        );
        assert_eq!(
            loaded.effective_context().unwrap().preamble_text(),
            Some("summary")
        );
        assert_eq!(loaded.effective_context().unwrap().entries.len(), 2);
        loaded.select_branch(Some(&root)).unwrap();
        append_or_checkpoint_session(&path, &loaded, &loaded.transcript).unwrap();
        let loaded = load_session_file(&path).unwrap();
        assert_eq!(loaded.leaf_id, active.leaf_id);
        assert_eq!(
            loaded.effective_context().unwrap().preamble_text(),
            Some("summary")
        );
        assert_eq!(loaded.entries.len(), active.entries.len());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("\"type\":\"message\""));
        assert!(!text.contains("\"type\":\"compaction\""));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_noncanonical_history_records() {
        let session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        for record in [
            serde_json::json!({"type": "message", "role": "user", "content": "hello"}),
            serde_json::json!({"type": "compaction"}),
            serde_json::json!({"type": "permission_allow", "tool": "read", "pattern": "src/**"}),
        ] {
            let mut journal = Vec::new();
            write_metadata(&mut journal, &session, &session.transcript).unwrap();
            write_jsonl_entry(&mut journal, &record).unwrap();
            assert!(session_from_jsonl(Cursor::new(journal)).is_err());
        }
    }

    #[test]
    fn journal_borrows_active_transcript_and_appends_updates() {
        let path = PathBuf::from(format!(
            "/tmp/phi-session-journal-{}.jsonl",
            uuid::Uuid::new_v4()
        ));
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.permission_allowlist.push(PermissionAllowEntry {
            tool: String::from("read"),
            pattern: String::from("src/**"),
        });
        let mut active = session.transcript.clone();
        active.append_user_message("hello");
        active.append_assistant_message(
            None,
            None,
            vec![phi_sdk::transcript::ContentBlock::Text {
                text: SharedText::new("world"),
            }],
            CompactString::new("provider"),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            CompactString::new("model"),
            phi_sdk::transcript::Usage::default(),
            CompactString::new("stop"),
        );
        append_or_checkpoint_session(&path, &session, &active).unwrap();
        let initial = std::fs::read(&path).unwrap();

        session.permission_allowlist.clear();
        session.name = CompactString::new("updated name");
        active.append_system_instruction("next");
        append_or_checkpoint_session(&path, &session, &active).unwrap();
        let appended = std::fs::read(&path).unwrap();
        let loaded = load_session_file(&path).unwrap();
        let summary = crate::session::discovery::read_session_summary(&path).unwrap();

        assert!(session.entries.is_empty());
        assert!(appended.starts_with(&initial));
        assert_eq!(loaded.entries.len(), active.entries.len());
        assert!(loaded.permission_allowlist.is_empty());
        assert_eq!(summary.name, "updated name");
        std::fs::remove_file(path).unwrap();
    }
}
