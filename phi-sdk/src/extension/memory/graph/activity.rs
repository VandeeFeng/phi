use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::extension::memory::tape::types::{TapeAnchorScanMode, TapeAnchorScanOptions};
use crate::extension::memory::tape::{TapeAnchor, TapeAnchorTrigger, TapeAnchorType, TapeService};
use crate::transcript::{AgentMessage, ContentBlock, TranscriptEntry, TranscriptEntryKind};

const ANCHOR_BOOST: f64 = 30.0;
const KEYWORD_ANCHOR_BOOST: f64 = 40.0;
const ACCESS_DECAY_HOURS: f64 = 24.0;
const ANCHOR_DECAY_HOURS: f64 = 12.0;
const ANCHOR_ENTRY_BOOST_WINDOW: usize = 15;

#[derive(Debug, Clone, Default)]
pub struct MemoryPathStats {
    pub count: usize,
    pub score: f64,
    pub read_count: usize,
    pub edit_count: usize,
    pub write_count: usize,
    pub anchor_scores: HashMap<String, f64>,
}

impl MemoryPathStats {
    pub fn final_score(&self) -> f64 {
        let diversity = [self.read_count, self.edit_count, self.write_count]
            .into_iter()
            .filter(|count| *count > 0)
            .count()
            .saturating_sub(1) as f64
            * 2.0;
        self.score + diversity
    }
}

#[derive(Debug, Clone)]
struct AnchorWindow {
    anchor_id: String,
    timestamp: Option<String>,
    window_end: Option<i64>,
    boost: f64,
}

pub fn analyze_path_access(
    entries: &[TranscriptEntry],
    service: &TapeService,
    scan_hours: u64,
    project_root: &Path,
) -> (HashMap<String, MemoryPathStats>, usize) {
    let anchor_windows = create_anchor_windows(entries, service, scan_hours);
    let mut path_stats = HashMap::new();
    let mut total_accesses = 0;

    for entry in entries {
        collect_entry_accesses(
            entry,
            project_root,
            &anchor_windows,
            &mut path_stats,
            &mut total_accesses,
        );
    }
    (path_stats, total_accesses)
}

fn collect_entry_accesses(
    entry: &TranscriptEntry,
    project_root: &Path,
    anchor_windows: &[AnchorWindow],
    path_stats: &mut HashMap<String, MemoryPathStats>,
    total_accesses: &mut usize,
) {
    let TranscriptEntryKind::Message {
        message: AgentMessage::Assistant { content, .. },
    } = &entry.kind
    else {
        return;
    };
    let access_time = to_timestamp(&entry.timestamp).unwrap_or_else(now_ms);
    for block in content {
        let ContentBlock::ToolCall { name, args, .. } = block else {
            continue;
        };
        let Some(entry_path) = args.get("path").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(path) = resolve_tracked_path(name, entry_path, project_root) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        *total_accesses += 1;
        record_access(path, name, access_time, anchor_windows, path_stats);
    }
}

fn record_access(
    path: PathBuf,
    tool_name: &str,
    access_time: i64,
    anchor_windows: &[AnchorWindow],
    path_stats: &mut HashMap<String, MemoryPathStats>,
) {
    let stats = path_stats
        .entry(path.to_string_lossy().to_string())
        .or_default();
    stats.score += access_event_score(tool_name, stats.count, access_time, anchor_windows);
    stats.count += 1;
    record_anchor_access(stats, access_time, anchor_windows);
    match tool_name {
        "read" => stats.read_count += 1,
        "edit" => stats.edit_count += 1,
        "write" => stats.write_count += 1,
        _ => {}
    }
}

fn resolve_tracked_path(tool_name: &str, entry_path: &str, project_root: &Path) -> Option<PathBuf> {
    matches!(tool_name, "read" | "edit" | "write")
        .then(|| absolutize(project_root.join(entry_path)))
}

fn create_anchor_windows(
    entries: &[TranscriptEntry],
    service: &TapeService,
    scan_hours: u64,
) -> Vec<AnchorWindow> {
    let handoff = latest_anchor(service, scan_hours, false);
    let keyword = latest_anchor(service, scan_hours, true);
    vec![
        anchor_window(handoff.as_ref(), entries, ANCHOR_BOOST),
        anchor_window(keyword.as_ref(), entries, KEYWORD_ANCHOR_BOOST),
    ]
}

fn anchor_window(
    anchor: Option<&TapeAnchor>,
    entries: &[TranscriptEntry],
    boost: f64,
) -> AnchorWindow {
    AnchorWindow {
        anchor_id: anchor.map(|value| value.id.to_string()).unwrap_or_default(),
        timestamp: anchor.map(|value| value.timestamp.to_string()),
        window_end: anchor_window_end(anchor, entries),
        boost,
    }
}

fn latest_anchor(service: &TapeService, scan_hours: u64, keyword_only: bool) -> Option<TapeAnchor> {
    service
        .search_anchors_with_fallback(TapeAnchorScanOptions {
            since: Some(
                (chrono::Utc::now() - chrono::Duration::hours(scan_hours as i64)).to_rfc3339(),
            ),
            anchor_type: Some(TapeAnchorType::Normal),
            mode: Some(TapeAnchorScanMode::All),
            ..TapeAnchorScanOptions::default()
        })
        .ok()?
        .into_iter()
        .rev()
        .find(|anchor| {
            !keyword_only
                || anchor.meta.as_ref().and_then(|meta| meta.trigger)
                    == Some(TapeAnchorTrigger::Keyword)
        })
}

fn anchor_window_end(anchor: Option<&TapeAnchor>, entries: &[TranscriptEntry]) -> Option<i64> {
    let anchor_time = to_timestamp(anchor?.timestamp.as_str())?;
    entries
        .iter()
        .filter(|entry| to_timestamp(&entry.timestamp).is_some_and(|time| time >= anchor_time))
        .take(ANCHOR_ENTRY_BOOST_WINDOW)
        .last()
        .and_then(|entry| to_timestamp(&entry.timestamp))
}

fn access_event_score(
    tool_name: &str,
    previous_count: usize,
    access_time: i64,
    anchor_windows: &[AnchorWindow],
) -> f64 {
    let access_score = match tool_name {
        "write" => 30.0,
        "edit" => 28.0,
        "read" => 20.0,
        _ => 0.0,
    };
    let recency = recency_decay(access_time, ACCESS_DECAY_HOURS);
    let repeat = 1.0 / ((previous_count + 1) as f64).sqrt();
    let anchor_boost = anchor_windows
        .iter()
        .map(|window| anchor_boost(access_time, window))
        .sum::<f64>();
    (access_score * recency + anchor_boost) * repeat
}

fn record_anchor_access(
    stats: &mut MemoryPathStats,
    access_time: i64,
    anchor_windows: &[AnchorWindow],
) {
    for window in anchor_windows {
        let score = anchor_boost(access_time, window);
        if score > 0.0 {
            *stats
                .anchor_scores
                .entry(window.anchor_id.clone())
                .or_default() += score;
        }
    }
}

fn recency_decay(access_time: i64, decay_hours: f64) -> f64 {
    let hours = ((now_ms() - access_time).max(0) as f64) / 3_600_000.0;
    (-hours / decay_hours).exp()
}

fn anchor_boost(access_time: i64, window: &AnchorWindow) -> f64 {
    let Some(anchor_time) = window.timestamp.as_deref().and_then(to_timestamp) else {
        return 0.0;
    };
    let Some(window_end) = window.window_end else {
        return 0.0;
    };
    if access_time < anchor_time || access_time > window_end {
        return 0.0;
    }
    let hours = (access_time - anchor_time) as f64 / 3_600_000.0;
    window.boost * (-hours / ANCHOR_DECAY_HOURS).exp()
}

fn to_timestamp(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn absolutize(path: PathBuf) -> PathBuf {
    path.canonicalize().unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_activity_scoring_formula() {
        let access_time = now_ms();
        let windows = Vec::new();
        let write = access_event_score("write", 0, access_time, &windows);
        let edit = access_event_score("edit", 0, access_time, &windows);
        let read = access_event_score("read", 0, access_time, &windows);
        let repeated_read = access_event_score("read", 1, access_time, &windows);
        let mixed = MemoryPathStats {
            score: 10.0,
            read_count: 1,
            edit_count: 1,
            ..MemoryPathStats::default()
        };

        assert!(write > edit && edit > read);
        assert!(read > repeated_read && read + repeated_read > read);
        assert_eq!(mixed.final_score(), 12.0);
    }

    #[test]
    fn aggregates_read_edit_and_write_calls_by_file() {
        let root = std::env::temp_dir().join(format!("phi-activity-{}", uuid::Uuid::new_v4()));
        let file = root.join("active.rs");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&file, "fn active() {}\n").unwrap();
        let mut transcript = crate::transcript::Transcript::new();
        let calls = ["read", "edit", "write"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| ContentBlock::ToolCall {
                id: format!("call-{index}").into(),
                provider_call_id: Some(format!("call-{index}").into()),
                provider_item_id: None,
                name: name.into(),
                args: serde_json::json!({ "path": "active.rs" }),
            })
            .collect();
        transcript.append_assistant_message(
            None,
            None,
            calls,
            "test".into(),
            crate::provider::ProviderApi::OpenAiCompletions,
            "test".into(),
            crate::transcript::Usage::default(),
            "tool_use".into(),
        );

        let (activity, total) =
            analyze_path_access(&transcript.entries, &TapeService::default(), 72, &root);
        let stats = activity
            .get(&file.canonicalize().unwrap().to_string_lossy().to_string())
            .unwrap();

        assert_eq!(total, 3);
        assert_eq!(
            (stats.read_count, stats.edit_count, stats.write_count),
            (1, 1, 1)
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
