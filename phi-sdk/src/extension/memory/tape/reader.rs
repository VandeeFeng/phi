use std::path::{Path, PathBuf};

use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use crate::transcript::{AgentMessage, Transcript, TranscriptEntry, TranscriptEntryKind};
use crate::utils::path::{phi_session_root, session_scope_dir};
use crate::utils::workspace::WorkspaceMeta;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub id: CompactString,
    #[serde(default)]
    pub name: CompactString,
    pub created_at: CompactString,
    pub updated_at: CompactString,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub total_cost: f64,
    #[serde(default)]
    pub total_cache_read: u64,
    #[serde(default)]
    pub total_cache_write: u64,
    #[serde(default)]
    pub total_estimated_tokens: u64,
    #[serde(default)]
    pub context_window: u64,
    #[serde(default)]
    pub model: CompactString,
    #[serde(default)]
    pub current_switch: CompactString,
    #[serde(default)]
    pub provider: CompactString,
    #[serde(default)]
    pub working_dir: CompactString,
    #[serde(default)]
    pub parent_session_id: Option<CompactString>,
    #[serde(default)]
    pub parent_agent_id: Option<CompactString>,
    #[serde(default)]
    pub leaf_id: Option<CompactString>,
}

#[derive(Debug, Clone)]
pub struct ParsedSessionFile {
    pub header: SessionHeader,
    pub entries: Vec<TranscriptEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextMessage {
    pub role: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionContextEntry {
    pub id: CompactString,
    #[serde(rename = "type")]
    pub entry_type: String,
    pub timestamp: CompactString,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<SessionContextMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<CompactString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_type: Option<CompactString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default)]
pub struct TapeReader;

impl TapeReader {
    pub fn entries<'a>(&self, transcript: &'a Transcript) -> &'a [TranscriptEntry] {
        &transcript.entries
    }
}

pub fn find_session_file(working_dir: &str, session_id: &str) -> anyhow::Result<Option<PathBuf>> {
    find_session_file_in(jsonl_files(&session_scope_dir(working_dir))?, session_id)
}

pub fn project_session_file_paths(working_dir: &str) -> anyhow::Result<Vec<PathBuf>> {
    let project_root = canonical_or_original(&WorkspaceMeta::discover(working_dir, None)?.root);
    all_session_file_paths()?
        .into_iter()
        .try_fold(Vec::new(), |mut paths, path| {
            if read_session_header(&path)?.is_some_and(|header| {
                !header.working_dir.is_empty()
                    && canonical_or_original(Path::new(header.working_dir.as_str()))
                        .starts_with(&project_root)
            }) {
                paths.push(path);
            }
            Ok::<_, anyhow::Error>(paths)
        })
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub fn find_project_session_file(
    working_dir: &str,
    session_id: &str,
) -> anyhow::Result<Option<PathBuf>> {
    find_session_file_in(project_session_file_paths(working_dir)?, session_id)
}

fn find_session_file_in(paths: Vec<PathBuf>, session_id: &str) -> anyhow::Result<Option<PathBuf>> {
    for path in paths {
        if read_session_header(&path)?.is_some_and(|header| header.id == session_id) {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn all_session_file_paths() -> anyhow::Result<Vec<PathBuf>> {
    let root = phi_session_root();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut paths = Vec::new();
    for scope in std::fs::read_dir(root)? {
        let scope = scope?;
        if scope.path().is_dir() {
            paths.extend(jsonl_files(&scope.path())?);
        }
    }
    Ok(paths)
}

fn jsonl_files(directory: &Path) -> anyhow::Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    Ok(std::fs::read_dir(directory)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect())
}

pub fn parse_session_file(file_path: &Path) -> anyhow::Result<Option<ParsedSessionFile>> {
    let content = match std::fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    parse_session_content(&content)
}

pub fn get_entries_after_timestamp(
    entries: Vec<TranscriptEntry>,
    timestamp: &str,
) -> Vec<TranscriptEntry> {
    let Some(target_time) = to_timestamp(timestamp) else {
        return entries;
    };
    entries
        .into_iter()
        .filter(|entry| to_timestamp(&entry.timestamp).is_some_and(|time| time > target_time))
        .collect()
}

pub fn get_entries_by_ids(entries: Vec<TranscriptEntry>, ids: &[String]) -> Vec<TranscriptEntry> {
    let id_set = ids.iter().collect::<std::collections::HashSet<_>>();
    entries
        .into_iter()
        .filter(|entry| id_set.contains(&entry.id.to_string()))
        .collect()
}

pub fn format_entry_as_context(entry: &TranscriptEntry) -> Option<SessionContextEntry> {
    let common = |entry_type: &str| SessionContextEntry {
        id: entry.id.clone(),
        entry_type: entry_type.to_string(),
        timestamp: entry.timestamp.clone(),
        message: None,
        summary: None,
        custom_type: None,
        data: None,
    };

    match &entry.kind {
        TranscriptEntryKind::ContextEdit {
            target_id,
            replacement,
        } => {
            let mut context = common("context_edit");
            context.data =
                Some(serde_json::json!({ "target_id": target_id, "replacement": replacement }));
            Some(context)
        }
        TranscriptEntryKind::Message { message } => {
            let mut context = common("message");
            context.message = Some(SessionContextMessage {
                role: message_role_name(message).to_string(),
                content: message_content_json(message),
            });
            Some(context)
        }
        TranscriptEntryKind::Compaction { summary, .. } => {
            let mut context = common("compaction");
            context.summary = Some(summary.clone());
            Some(context)
        }
        TranscriptEntryKind::Reset { handoff } => {
            let mut context = common("reset");
            context.message = handoff.as_ref().map(|text| SessionContextMessage {
                role: "user".into(),
                content: serde_json::Value::String(text.to_string()),
            });
            Some(context)
        }
        TranscriptEntryKind::Failure { turn_id, failure } => {
            let mut context = common("failure");
            context.data = Some(serde_json::json!({ "turn_id": turn_id, "failure": failure }));
            Some(context)
        }
        TranscriptEntryKind::Event { name, data } => {
            let mut context = common("custom");
            context.custom_type = Some(name.clone());
            context.data = Some(data.clone());
            Some(context)
        }
    }
}

fn read_session_header(file_path: &Path) -> anyhow::Result<Option<SessionHeader>> {
    let content = match std::fs::read_to_string(file_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let Some(first_line) = content.lines().next() else {
        return Ok(None);
    };
    parse_session_header_line(first_line)
}

fn parse_session_content(content: &str) -> anyhow::Result<Option<ParsedSessionFile>> {
    let mut header = None;
    let mut entries = Vec::new();

    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let raw: serde_json::Value = serde_json::from_str(line)?;
        match raw.get("type").and_then(|value| value.as_str()) {
            Some("metadata") => header = parse_session_header_value(raw)?,
            Some("entry") => {
                if let Some(entry_value) = raw.get("entry") {
                    entries.push(serde_json::from_value(entry_value.clone())?);
                }
            }
            _ => {}
        }
    }

    Ok(header.map(|header| ParsedSessionFile { header, entries }))
}

fn parse_session_header_line(line: &str) -> anyhow::Result<Option<SessionHeader>> {
    let raw: serde_json::Value = serde_json::from_str(line)?;
    parse_session_header_value(raw)
}

fn parse_session_header_value(raw: serde_json::Value) -> anyhow::Result<Option<SessionHeader>> {
    if raw.get("type").and_then(|value| value.as_str()) != Some("metadata") {
        return Ok(None);
    }
    Ok(Some(SessionHeader {
        id: value_string(&raw, "id"),
        name: value_string(&raw, "name"),
        created_at: value_string(&raw, "created_at"),
        updated_at: value_string(&raw, "updated_at"),
        total_tokens: value_u64(&raw, "total_tokens"),
        total_cost: value_f64(&raw, "total_cost"),
        total_cache_read: value_u64(&raw, "total_cache_read"),
        total_cache_write: value_u64(&raw, "total_cache_write"),
        total_estimated_tokens: value_u64(&raw, "total_estimated_tokens"),
        context_window: value_u64(&raw, "context_window"),
        model: value_string(&raw, "model"),
        current_switch: value_string(&raw, "current_switch"),
        provider: value_string(&raw, "provider"),
        working_dir: value_string(&raw, "working_dir"),
        parent_session_id: value_optional_string(&raw, "parent_session_id"),
        parent_agent_id: value_optional_string(&raw, "parent_agent_id"),
        leaf_id: value_optional_string(&raw, "leaf_id"),
    }))
}

fn message_role_name(message: &AgentMessage) -> &'static str {
    match message {
        AgentMessage::User { .. } => "user",
        AgentMessage::Context { .. } => "context",
        AgentMessage::Agent { .. } => "agent",
        AgentMessage::Assistant { .. } => "assistant",
        AgentMessage::ToolResult { .. } => "tool",
        AgentMessage::System { .. } => "system",
    }
}

fn message_content_json(message: &AgentMessage) -> serde_json::Value {
    match message {
        AgentMessage::User { content } => serde_json::to_value(content).unwrap_or_default(),
        AgentMessage::Context { content, .. }
        | AgentMessage::Agent { content }
        | AgentMessage::Assistant { content, .. }
        | AgentMessage::ToolResult { content, .. } => {
            serde_json::to_value(content).unwrap_or_default()
        }
        AgentMessage::System { content } => serde_json::Value::String(content.to_string()),
    }
}

fn value_string(raw: &serde_json::Value, key: &str) -> CompactString {
    value_optional_string(raw, key).unwrap_or_default()
}

fn value_optional_string(raw: &serde_json::Value, key: &str) -> Option<CompactString> {
    raw.get(key)
        .and_then(|value| value.as_str())
        .map(CompactString::new)
}

fn value_u64(raw: &serde_json::Value, key: &str) -> u64 {
    raw.get(key).and_then(|value| value.as_u64()).unwrap_or(0)
}

fn value_f64(raw: &serde_json::Value, key: &str) -> f64 {
    raw.get(key).and_then(|value| value.as_f64()).unwrap_or(0.0)
}

fn to_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis())
}
