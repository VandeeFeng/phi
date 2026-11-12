use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use compact_str::CompactString;
use phi_sdk::workspace::{phi_session_root, session_scope_dir};

use super::journal::{SessionJsonlEntry, SessionMetadata};
use super::{AgentMessage, TranscriptEntry, TranscriptEntryKind, UserContentBlock};

const SESSION_SUMMARY_BYTES: u64 = 1024 * 1024;
const RECENT_SESSION_LIMIT: usize = 50;
const MAX_SESSION_ENTRIES: usize = 100_000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionSummary {
    pub id: CompactString,
    pub name: CompactString,
    pub updated_at: CompactString,
    pub first_user_prompt: Option<CompactString>,
}

fn current_session_scope_dir() -> PathBuf {
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    session_scope_dir(&working_dir.to_string_lossy())
}

pub(crate) fn discover_sessions(query: &str) -> anyhow::Result<Vec<SessionSummary>> {
    if query.is_empty() {
        find_recent_sessions(RECENT_SESSION_LIMIT)
    } else {
        find_sessions_by_prefix(query)
    }
}

pub fn find_sessions_by_prefix(prefix: &str) -> anyhow::Result<Vec<SessionSummary>> {
    let mut sessions = load_session_summaries(&current_session_scope_dir())?;
    sessions.retain(|session| session.id.starts_with(prefix));
    Ok(sessions)
}

pub fn find_recent_sessions(limit: usize) -> anyhow::Result<Vec<SessionSummary>> {
    let mut sessions = load_session_summaries(&current_session_scope_dir())?;
    sessions.truncate(limit);
    Ok(sessions)
}

fn load_session_summaries(dir: &Path) -> anyhow::Result<Vec<SessionSummary>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for (index, entry) in std::fs::read_dir(dir)?.enumerate() {
        anyhow::ensure!(
            index < MAX_SESSION_ENTRIES,
            "Session discovery entry limit exceeded"
        );
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file()
            && path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(session) = read_session_summary(&path)
            && super::storage::session_file_id(&path) == Some(session.id.as_str())
        {
            sessions.push(session);
        }
    }
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

pub(super) fn read_session_summary(path: &Path) -> anyhow::Result<SessionSummary> {
    let mut file = std::fs::File::open(path)?;
    let mut summary =
        session_summary_from_jsonl(BufReader::new(&mut file).take(SESSION_SUMMARY_BYTES))?;
    let file_len = file.metadata()?.len();
    let start = file_len.saturating_sub(SESSION_SUMMARY_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    if start > 0 {
        let mut partial = String::new();
        reader.read_line(&mut partial)?;
    }
    for line in reader.lines() {
        if let Ok(SessionJsonlEntry::Metadata { metadata }) = serde_json::from_str(&line?) {
            ensure_root_session(&metadata)?;
            summary.name = metadata.name;
            summary.updated_at = metadata.updated_at;
        }
    }
    Ok(summary)
}

pub(super) fn session_summary_from_jsonl(reader: impl BufRead) -> anyhow::Result<SessionSummary> {
    let mut metadata = None;
    let mut first_user_prompt = None;
    for line in reader.lines() {
        let line = line?;
        let Ok(entry) = serde_json::from_str::<SessionJsonlEntry>(&line) else {
            break;
        };
        match entry {
            SessionJsonlEntry::Metadata {
                metadata: session_metadata,
            } => metadata = Some(session_metadata),
            SessionJsonlEntry::Entry {
                entry:
                    TranscriptEntry {
                        kind:
                            TranscriptEntryKind::Message {
                                message: AgentMessage::User { content },
                            },
                        ..
                    },
            } => first_user_prompt = user_prompt_text(&content),
            _ => {}
        }
        if metadata.is_some() && first_user_prompt.is_some() {
            break;
        }
    }
    let metadata = metadata.ok_or_else(|| anyhow::anyhow!("missing session metadata"))?;
    ensure_root_session(&metadata)?;
    Ok(SessionSummary {
        id: metadata.id,
        name: metadata.name,
        updated_at: metadata.updated_at,
        first_user_prompt,
    })
}

fn ensure_root_session(metadata: &SessionMetadata) -> anyhow::Result<()> {
    anyhow::ensure!(
        metadata.agent_kind.creator().is_none() && metadata.sidechain.is_none(),
        "delegated history is not a recent root Session"
    );
    Ok(())
}

fn user_prompt_text(content: &[UserContentBlock]) -> Option<CompactString> {
    let text = content
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text { text } => Some(text.as_str()),
            UserContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    (!text.trim().is_empty()).then(|| CompactString::new(text))
}

pub(super) fn resolve_session_path(id_or_path: &str) -> anyhow::Result<Option<PathBuf>> {
    let explicit_path = Path::new(id_or_path);
    if explicit_path.exists() {
        return Ok(Some(explicit_path.to_path_buf()));
    }
    resolve_session_id(id_or_path)
}

pub(super) fn resolve_session_id(id: &str) -> anyhow::Result<Option<PathBuf>> {
    anyhow::ensure!(super::model::valid_session_id(id), "invalid Session ID");
    resolve_session_id_in(&phi_session_root(), id)
}

fn resolve_session_id_in(root: &Path, id: &str) -> anyhow::Result<Option<PathBuf>> {
    if !root.exists() {
        return Ok(None);
    }

    let mut paths = Vec::new();
    let mut remaining = MAX_SESSION_ENTRIES;
    for scope in std::fs::read_dir(root)? {
        consume_entry(&mut remaining)?;
        let scope = scope?;
        if scope.file_type()?.is_dir() {
            collect_session_files(&scope.path(), 0, &mut remaining, &mut paths)?;
        }
    }
    let mut matches = paths.into_iter().filter(|path| {
        path.extension()
            .is_some_and(|extension| extension == "jsonl")
            && super::storage::session_file_id(path) == Some(id)
    });
    let path = matches.next();
    anyhow::ensure!(matches.next().is_none(), "ambiguous Session ID `{id}`");
    Ok(path)
}

pub(super) fn session_tree_files(directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut remaining = MAX_SESSION_ENTRIES;
    collect_session_files(directory, 0, &mut remaining, &mut paths)?;
    Ok(paths)
}

fn consume_entry(remaining: &mut usize) -> anyhow::Result<()> {
    *remaining = remaining
        .checked_sub(1)
        .ok_or_else(|| anyhow::anyhow!("Session discovery entry limit exceeded"))?;
    Ok(())
}

fn collect_session_files(
    directory: &Path,
    depth: usize,
    remaining: &mut usize,
    paths: &mut Vec<PathBuf>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        consume_entry(remaining)?;
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_file() && super::storage::session_file_id(&path).is_some() {
            paths.push(path);
        } else if file_type.is_dir()
            && entry
                .file_name()
                .to_str()
                .is_some_and(super::model::valid_session_id)
        {
            let children = path.join("subagents");
            match std::fs::symlink_metadata(&children) {
                Ok(metadata) if metadata.is_dir() => {
                    anyhow::ensure!(
                        depth < super::model::MAX_SIDECHAIN_DEPTH,
                        "Session sidechain depth limit exceeded"
                    );
                    collect_session_files(&children, depth + 1, remaining, paths)?;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn discovers_nested_history_without_promoting_it_to_recent_roots() {
        let root = std::env::temp_dir().join(format!("phi-discovery-{}", uuid::Uuid::new_v4()));
        let scope = root.join("scope");
        std::fs::create_dir_all(&scope).unwrap();
        let mut parent = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        parent.append_user_message("root prompt");
        let mut child = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        child.bind_test_parent(&parent);
        child.working_dir = "/tmp/another-worktree".into();
        child.definition = Some("reviewer".into());
        let mut nested = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        nested.bind_test_parent(&child);
        let mut teammate = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        teammate.agent_kind = phi_sdk::agents::AgentKind::Teammate {
            creator: phi_sdk::agent_instance::AgentInstanceId::new(parent.id.as_str()),
            team: serde_json::from_value(serde_json::json!("team")).unwrap(),
        };
        let parent_path = scope.join(format!("20261001T100000Z-{}.jsonl", parent.id));
        let child_path = scope
            .join(parent.id.as_str())
            .join("subagents")
            .join(format!("agent-{}.jsonl", child.id));
        let nested_path = child_path
            .parent()
            .unwrap()
            .join(child.id.as_str())
            .join("subagents")
            .join(format!("agent-{}.jsonl", nested.id));
        let teammate_path = scope.join(format!("20261001T100000Z-{}.jsonl", teammate.id));
        for (path, session) in [
            (&parent_path, &parent),
            (&child_path, &child),
            (&nested_path, &nested),
            (&teammate_path, &teammate),
        ] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            super::super::journal::append_or_checkpoint_session(path, session, &session.transcript)
                .unwrap();
            assert_eq!(
                resolve_session_id_in(&root, &session.id).unwrap(),
                Some(path.clone())
            );
        }
        let unrelated = scope.join(format!("unrelated-{}.jsonl", parent.id));
        std::fs::copy(&parent_path, &unrelated).unwrap();
        assert_eq!(
            resolve_session_id_in(&root, &parent.id).unwrap(),
            Some(parent_path.clone())
        );
        let summaries = load_session_summaries(&scope).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, parent.id);
        std::fs::remove_file(unrelated).unwrap();
        std::fs::remove_file(&parent_path).unwrap();
        assert_eq!(
            resolve_session_id_in(&root, &nested.id).unwrap(),
            Some(nested_path.clone())
        );
        let duplicate = root
            .join("other-scope")
            .join(child_path.file_name().unwrap());
        std::fs::create_dir_all(duplicate.parent().unwrap()).unwrap();
        std::fs::copy(&child_path, &duplicate).unwrap();
        assert!(
            resolve_session_id_in(&root, &child.id)
                .unwrap_err()
                .to_string()
                .contains("ambiguous")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_checks_updated_delegation_metadata() {
        let root = PathBuf::from(format!("/tmp/phi-discovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut parent = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        parent.append_user_message("root prompt");
        let path = root.join(format!("20261001T100000Z-{}.jsonl", parent.id));
        super::super::journal::append_or_checkpoint_session(&path, &parent, &parent.transcript)
            .unwrap();
        let initial = std::fs::read_to_string(&path).unwrap();
        let metadata: serde_json::Value =
            serde_json::from_str(initial.lines().next().unwrap()).unwrap();
        let mut child = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        child.bind_test_parent(&parent);
        let teammate = phi_sdk::agents::AgentKind::Teammate {
            creator: phi_sdk::agent_instance::AgentInstanceId::new(parent.id.as_str()),
            team: serde_json::from_value(serde_json::json!("team")).unwrap(),
        };
        for (field, value) in [
            (
                "agent_kind",
                serde_json::to_value(&child.agent_kind).unwrap(),
            ),
            ("agent_kind", serde_json::to_value(teammate).unwrap()),
            ("sidechain", serde_json::to_value(&child.sidechain).unwrap()),
        ] {
            let mut updated = metadata.clone();
            updated[field] = value;
            std::fs::write(&path, format!("{initial}{updated}\n")).unwrap();
            assert!(
                load_session_summaries(&root).unwrap().is_empty(),
                "delegated history with updated {field} must not appear in root listings"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lookup_ignores_symlinks_and_unrelated_directory_trees() {
        let root = std::env::temp_dir().join(format!("phi-lookup-{}", uuid::Uuid::new_v4()));
        let scope = root.join("scope");
        let outside = root.join("outside");
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let filename = "agent-hidden.jsonl";
        std::fs::write(outside.join(filename), "not a Session").unwrap();
        std::os::unix::fs::symlink(&outside, scope.join("linked-parent")).unwrap();
        std::fs::create_dir_all(scope.join("parent")).unwrap();
        std::os::unix::fs::symlink(&outside, scope.join("parent/subagents")).unwrap();
        std::os::unix::fs::symlink(outside.join(filename), scope.join(filename)).unwrap();
        std::fs::create_dir_all(scope.join("unrelated/nested")).unwrap();
        std::fs::write(
            scope.join("unrelated/nested").join(filename),
            "not a Session",
        )
        .unwrap();
        let paths = session_tree_files(&scope).unwrap();
        assert!(paths.is_empty(), "{paths:?}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_summary_without_loading_full_session() {
        let mut session = crate::session::new(
            "provider",
            "model",
            128_000,
            &std::env::current_dir().unwrap(),
        );
        session.append_user_message("hello");
        let entry = serde_json::json!({"type": "entry", "entry": session.entries[0]});
        let input = format!(
            "{}\n{entry}\n",
            r#"{"type":"metadata","id":"session-id","name":"","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","total_tokens":0,"total_cost":0.0,"total_estimated_tokens":0,"context_window":128000,"model":"model","provider":"provider","working_dir":"/tmp","agent_kind":{"kind":"master"}}"#,
        );

        let summary = session_summary_from_jsonl(Cursor::new(input)).unwrap();

        assert_eq!(summary.id, "session-id");
        assert_eq!(summary.first_user_prompt.as_deref(), Some("hello"));
    }
}
