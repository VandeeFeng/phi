use std::sync::Arc;

use compact_str::CompactString;

use super::anchor::{TapeAnchor, TapeAnchorMeta, TapeAnchorType};
use super::checkpoint::GitCheckpointStore;
use super::reader::{
    find_project_session_file, find_session_file, get_entries_after_timestamp, parse_session_file,
    project_session_file_paths,
};
use super::store::AnchorStore;
use super::types::{TapeAnchorScanMode, TapeAnchorScanOptions, TapeScope, TapeSessionScanOptions};
use crate::transcript::{Transcript, TranscriptEntry};

#[derive(Debug, Clone, Default)]
pub struct TapeQuery {
    pub session_id: Option<CompactString>,
    pub name_contains: Option<String>,
    pub summary_contains: Option<String>,
    pub purpose_contains: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
struct TapeScanBounds {
    start_time: Option<String>,
    end_time: Option<String>,
}

#[derive(Clone)]
pub struct TapeService {
    anchor_store: Arc<AnchorStore>,
    workspace_id: String,
    session_id: CompactString,
    working_dir: String,
    checkpoint_store: Option<GitCheckpointStore>,
}

impl Default for TapeService {
    fn default() -> Self {
        Self::new(Arc::new(AnchorStore::default()))
    }
}

impl TapeService {
    pub fn new(anchor_store: Arc<AnchorStore>) -> Self {
        Self {
            anchor_store,
            workspace_id: String::new(),
            session_id: CompactString::new(uuid::Uuid::new_v4().to_string()),
            working_dir: String::new(),
            checkpoint_store: None,
        }
    }

    pub fn create(
        tape_base_path: impl Into<std::path::PathBuf>,
        project_name: &str,
        session_id: impl Into<CompactString>,
        working_dir: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let tape_base_path = tape_base_path.into();
        let working_dir = working_dir.into();
        let session_id = session_id.into();
        let workspace = crate::utils::workspace::WorkspaceMeta::discover(&working_dir, None)?;
        let checkpoint_store = GitCheckpointStore::discover(&workspace, session_id.as_str());
        let anchor_store = Arc::new(AnchorStore::new(tape_base_path, project_name)?);
        Ok(Self {
            anchor_store,
            workspace_id: workspace
                .worktree_id()
                .map(|id| format!("git:{id}"))
                .unwrap_or_else(|| format!("directory:{}", workspace.root.display())),
            session_id,
            working_dir,
            checkpoint_store,
        })
    }

    pub fn session_id(&self) -> &CompactString {
        &self.session_id
    }

    pub(crate) fn with_canonical_lock<T>(
        &self,
        mut action: impl FnMut() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let Some(transaction) = self.anchor_store.transaction() else {
            return action();
        };
        if let Some(_guard) = transaction.lock_existing()? {
            return action();
        }
        let result = action();
        // A first writer may have created the directory during the unlocked read.
        if let Some(_guard) = transaction.lock_existing()? {
            return action();
        }
        result
    }

    pub fn record_anchor_at_entry(
        &self,
        session_id: impl Into<CompactString>,
        entry_id: impl Into<CompactString>,
        anchor: TapeAnchor,
    ) -> anyhow::Result<()> {
        let mut anchor = anchor;
        anchor.session_id = session_id.into();
        anchor.session_entry_id = entry_id.into();
        self.append_anchor(anchor).map(drop)
    }

    pub fn create_anchor_without_transcript(
        &self,
        name: impl Into<CompactString>,
        anchor_type: TapeAnchorType,
        meta: Option<TapeAnchorMeta>,
    ) -> anyhow::Result<TapeAnchor> {
        let mut anchor = TapeAnchor::new(name, anchor_type);
        anchor.session_id = self.session_id.clone();
        anchor.session_entry_id = anchor.id.clone();
        anchor.meta = meta;
        self.append_anchor(anchor)
    }

    fn append_anchor(&self, mut anchor: TapeAnchor) -> anyhow::Result<TapeAnchor> {
        anchor.validate()?;
        anchor.meta.get_or_insert_default().workspace =
            Some(CompactString::new(&self.workspace_id));
        if let Some(store) = &self.checkpoint_store {
            anchor.meta.get_or_insert_default().git = Some(store.capture(&anchor)?);
        }
        self.anchor_store.append(anchor.clone())?;
        Ok(anchor)
    }

    pub fn search(&self, query: TapeQuery) -> anyhow::Result<Vec<TapeAnchor>> {
        self.search_anchors(TapeAnchorScanOptions {
            session_id: query.session_id,
            name: query.name_contains,
            summary: query.summary_contains,
            purpose: query.purpose_contains,
            limit: query.limit,
            ..TapeAnchorScanOptions::default()
        })
    }

    pub fn search_anchors(
        &self,
        options: TapeAnchorScanOptions,
    ) -> anyhow::Result<Vec<TapeAnchor>> {
        let limit = options.limit;
        let filter_orphans = options.include_orphans != Some(true);
        let mut anchors = self.anchor_store.scan(options)?;
        if filter_orphans {
            anchors = self.filter_orphan_anchors(anchors)?;
        }
        if let Some(limit) = limit {
            if anchors.len() > limit {
                anchors = anchors.split_off(anchors.len() - limit);
            }
        }
        Ok(anchors)
    }

    pub fn is_orphan_anchor(&self, anchor: &TapeAnchor) -> anyhow::Result<bool> {
        if self.working_dir.is_empty() || anchor.session_id.is_empty() {
            return Ok(false);
        }
        Ok(find_project_session_file(&self.working_dir, &anchor.session_id)?.is_none())
    }

    pub fn filter_orphan_anchors(
        &self,
        anchors: Vec<TapeAnchor>,
    ) -> anyhow::Result<Vec<TapeAnchor>> {
        anchors
            .into_iter()
            .filter_map(|anchor| match self.is_orphan_anchor(&anchor) {
                Ok(false) => Some(Ok(anchor)),
                Ok(true) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub fn search_anchors_with_fallback(
        &self,
        options: TapeAnchorScanOptions,
    ) -> anyhow::Result<Vec<TapeAnchor>> {
        let anchors = self.search_anchors(options.clone())?;
        if !anchors.is_empty() || options.session_id.is_none() {
            return Ok(anchors);
        }
        self.search_anchors(TapeAnchorScanOptions {
            session_id: None,
            ..options
        })
    }

    pub fn scan(
        &self,
        transcript: &Transcript,
        options: &TapeSessionScanOptions,
    ) -> anyhow::Result<Vec<TranscriptEntry>> {
        self.filter_entries(transcript.entries.clone(), options)
    }

    pub fn scan_persisted_entries(
        &self,
        options: &TapeSessionScanOptions,
    ) -> anyhow::Result<Vec<TranscriptEntry>> {
        let paths = match options.entry_scope.unwrap_or(TapeScope::Project) {
            TapeScope::Session => find_session_file(&self.working_dir, &self.session_id)?
                .into_iter()
                .collect(),
            TapeScope::Project => project_session_file_paths(&self.working_dir)?,
        };
        let mut entries = paths
            .into_iter()
            .try_fold(Vec::new(), |mut entries, path| {
                if let Some(session) = parse_session_file(&path)? {
                    entries.extend(session.entries);
                }
                Ok::<_, anyhow::Error>(entries)
            })?;
        entries.sort_by(|left, right| left.timestamp.cmp(&right.timestamp));
        self.filter_entries(entries, options)
    }

    fn filter_entries(
        &self,
        mut entries: Vec<TranscriptEntry>,
        options: &TapeSessionScanOptions,
    ) -> anyhow::Result<Vec<TranscriptEntry>> {
        let bounds = self.resolve_scan_bounds(options)?;

        if let Some(start_time) = bounds.start_time {
            entries = get_entries_after_timestamp(entries, &start_time);
        }
        if let Some(end_time) = bounds.end_time {
            if let Some(end_timestamp) = to_timestamp(&end_time) {
                entries.retain(|entry| {
                    to_timestamp(&entry.timestamp).is_some_and(|ts| ts <= end_timestamp)
                });
            }
        }
        if !options.types.is_empty() {
            entries.retain(|entry| {
                options
                    .types
                    .iter()
                    .any(|kind| kind == entry_kind_name(entry))
            });
        }
        if let Some(scan) = &options.scan {
            let needle = scan.to_lowercase();
            entries.retain(|entry| {
                serde_json::to_string(entry)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&needle)
            });
        }
        if let Some(limit) = options.limit {
            if entries.len() > limit {
                entries = entries.split_off(entries.len() - limit);
            }
        }
        Ok(entries)
    }

    pub fn delete_anchor(&self, id: &str) -> anyhow::Result<bool> {
        Ok(!self.delete_anchor_entries(&[id.to_string()])?.is_empty())
    }

    pub fn delete_anchor_entries(&self, ids: &[String]) -> anyhow::Result<Vec<TapeAnchor>> {
        self.anchor_store.delete(ids)
    }

    pub fn get_anchor_store(&self) -> &AnchorStore {
        &self.anchor_store
    }

    pub fn reset(&self) -> anyhow::Result<()> {
        self.anchor_store.clear()
    }

    fn resolve_anchor(
        &self,
        name: Option<&str>,
        anchor_scope: TapeScope,
    ) -> anyhow::Result<Option<TapeAnchor>> {
        let session_id = if anchor_scope == TapeScope::Session {
            Some(self.session_id.clone())
        } else {
            None
        };
        Ok(self
            .anchor_store
            .scan(TapeAnchorScanOptions {
                name: name.map(str::to_string),
                name_case_insensitive: true,
                session_id,
                mode: Some(TapeAnchorScanMode::Latest),
                ..TapeAnchorScanOptions::default()
            })?
            .into_iter()
            .next())
    }

    fn resolve_scan_bounds(
        &self,
        options: &TapeSessionScanOptions,
    ) -> anyhow::Result<TapeScanBounds> {
        if let Some(between_dates) = &options.between_dates {
            return Ok(TapeScanBounds {
                start_time: Some(between_dates.start.clone()),
                end_time: Some(between_dates.end.clone()),
            });
        }

        let anchor_scope = options.anchor_scope.unwrap_or(TapeScope::Session);
        if let Some(between_anchors) = &options.between_anchors {
            let start_anchor = self.resolve_anchor(Some(&between_anchors.start), anchor_scope)?;
            let end_anchor = self.resolve_anchor(Some(&between_anchors.end), anchor_scope)?;
            if let (Some(start), Some(end)) = (start_anchor, end_anchor) {
                return Ok(TapeScanBounds {
                    start_time: Some(start.timestamp.to_string()),
                    end_time: Some(end.timestamp.to_string()),
                });
            }
            return Ok(TapeScanBounds::default());
        }

        if options.last_anchor.unwrap_or(false) {
            return Ok(TapeScanBounds {
                start_time: self
                    .resolve_anchor(None, anchor_scope)?
                    .map(|anchor| anchor.timestamp.to_string()),
                end_time: None,
            });
        }

        if let Some(since_anchor) = &options.since_anchor {
            return Ok(TapeScanBounds {
                start_time: self
                    .resolve_anchor(Some(since_anchor), anchor_scope)?
                    .map(|anchor| anchor.timestamp.to_string()),
                end_time: None,
            });
        }

        Ok(TapeScanBounds::default())
    }
}

fn entry_kind_name(entry: &TranscriptEntry) -> &'static str {
    match entry.kind {
        crate::transcript::TranscriptEntryKind::Message { .. } => "message",
        crate::transcript::TranscriptEntryKind::Event { .. } => "custom",
        crate::transcript::TranscriptEntryKind::Compaction { .. } => "compaction",
        crate::transcript::TranscriptEntryKind::Reset { .. } => "reset",
        crate::transcript::TranscriptEntryKind::Failure { .. } => "failure",
        crate::transcript::TranscriptEntryKind::ContextEdit { .. } => "context_edit",
    }
}

fn to_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn deletion_removes_only_requested_ids(
            requested in prop::collection::vec(0usize..12, 0..24),
        ) {
            let service = TapeService::default();
            let anchors = (0..8)
                .map(|index| {
                    let mut anchor = TapeAnchor::new("checkpoint", TapeAnchorType::Normal);
                    anchor.id = index.to_string().into();
                    anchor
                })
                .collect::<Vec<_>>();
            for anchor in anchors {
                service.record_anchor_at_entry("session", "entry", anchor).unwrap();
            }
            let requested = requested.iter().map(usize::to_string).collect::<Vec<_>>();
            let removed = service.delete_anchor_entries(&requested).unwrap();
            let surviving = service.search(TapeQuery::default()).unwrap();
            prop_assert_eq!(removed.len() + surviving.len(), 8);
            prop_assert!(removed.iter().all(|anchor| requested.contains(&anchor.id.to_string())));
            prop_assert!(surviving.iter().all(|anchor| !requested.contains(&anchor.id.to_string())));
            prop_assert!(service.delete_anchor_entries(&requested).unwrap().is_empty());
        }
    }

    #[test]
    fn anchor_bounds_preserve_scope_and_interval() {
        use super::super::types::{AnchorRange, DateRange};

        let service = TapeService {
            session_id: "current".into(),
            ..Default::default()
        };
        let timestamp = |second| format!("2026-01-01T00:00:0{second}Z");
        for (session, name, second) in [
            ("current", "start", 1),
            ("other", "start", 2),
            ("current", "end", 3),
            ("other", "end", 4),
        ] {
            let mut anchor = TapeAnchor::new(name, TapeAnchorType::Normal);
            anchor.timestamp = timestamp(second).into();
            service
                .record_anchor_at_entry(session, "entry", anchor)
                .unwrap();
        }
        let mut transcript = Transcript::new();
        for second in 1..=4 {
            transcript.append_user_message("evidence");
            transcript.entries.last_mut().unwrap().timestamp = timestamp(second).into();
        }
        let options = TapeSessionScanOptions {
            scan: None,
            types: Vec::new(),
            limit: None,
            since_anchor: None,
            last_anchor: None,
            between_anchors: Some(AnchorRange {
                start: "START".into(),
                end: "end".into(),
            }),
            between_dates: None,
            entry_scope: Some(TapeScope::Session),
            anchor_scope: Some(TapeScope::Session),
        };
        for (scope, expected_interval, expected_after_last) in [
            (
                TapeScope::Session,
                vec![timestamp(2), timestamp(3)],
                vec![timestamp(4)],
            ),
            (
                TapeScope::Project,
                vec![timestamp(3), timestamp(4)],
                Vec::new(),
            ),
        ] {
            let options = TapeSessionScanOptions {
                anchor_scope: Some(scope),
                ..options.clone()
            };
            let entries = service.scan(&transcript, &options).unwrap();
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| entry.timestamp.to_string())
                    .collect::<Vec<_>>(),
                expected_interval
            );
            let entries = service
                .scan(
                    &transcript,
                    &TapeSessionScanOptions {
                        between_anchors: None,
                        last_anchor: Some(true),
                        ..options
                    },
                )
                .unwrap();
            assert_eq!(
                entries
                    .iter()
                    .map(|entry| entry.timestamp.to_string())
                    .collect::<Vec<_>>(),
                expected_after_last
            );
        }
        let entries = service
            .scan(
                &transcript,
                &TapeSessionScanOptions {
                    between_dates: Some(DateRange {
                        start: timestamp(1),
                        end: timestamp(2),
                    }),
                    ..options
                },
            )
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].timestamp, timestamp(2));
    }

    #[test]
    fn search_limit_does_not_truncate_store() {
        let service = TapeService::default();
        for index in 0..30 {
            service
                .create_anchor_without_transcript(
                    format!("anchor/{index}"),
                    TapeAnchorType::Normal,
                    None,
                )
                .unwrap();
        }
        let anchors = service
            .search_anchors(TapeAnchorScanOptions {
                limit: Some(25),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(anchors.len(), 25);
        assert_eq!(service.search(TapeQuery::default()).unwrap().len(), 30);
    }

    #[test]
    fn invalid_anchor_cannot_create_git_checkpoint() {
        let directory = std::path::PathBuf::from("/tmp")
            .join(format!("phi-anchor-checkpoint-{}", uuid::Uuid::new_v4()));
        let repository = directory.join("work");
        std::fs::create_dir_all(&repository).unwrap();
        assert!(
            std::process::Command::new("git")
                .arg("init")
                .arg(&repository)
                .output()
                .unwrap()
                .status
                .success()
        );
        let service = TapeService::create(
            directory.join("memory"),
            "project",
            "session",
            repository.display().to_string(),
        )
        .unwrap();
        let result = service.record_anchor_at_entry(
            "session",
            "entry",
            TapeAnchor::new("", TapeAnchorType::Normal),
        );
        assert!(result.is_err());
        let references = std::process::Command::new("git")
            .args(["for-each-ref", "refs/phi/tape/"])
            .current_dir(&repository)
            .output()
            .unwrap();
        assert!(references.status.success());
        assert!(references.stdout.is_empty());
        assert!(!directory.join("memory").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn duplicate_anchor_identity_is_rejected_in_all_storage_modes() {
        let directory = std::path::PathBuf::from("/tmp")
            .join(format!("phi-anchor-identity-{}", uuid::Uuid::new_v4()));
        let disk = TapeService::new(Arc::new(AnchorStore::new(&directory, "test").unwrap()));
        for service in [TapeService::default(), disk] {
            let anchor = TapeAnchor::new("checkpoint", TapeAnchorType::Normal);
            service
                .record_anchor_at_entry("session", "entry", anchor.clone())
                .unwrap();
            assert!(
                service
                    .record_anchor_at_entry("session", "other", anchor)
                    .is_err()
            );
            assert_eq!(
                service.get_anchor_store().get_all_anchors().unwrap().len(),
                1
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persisted_delete_and_reset_are_visible_to_other_instances() {
        let directory = std::path::PathBuf::from("/tmp")
            .join(format!("phi-anchor-service-{}", uuid::Uuid::new_v4()));
        let service = TapeService::new(Arc::new(AnchorStore::new(&directory, "test").unwrap()));
        let observer = TapeService::new(Arc::new(AnchorStore::new(&directory, "test").unwrap()));
        let first = service
            .create_anchor_without_transcript("first", TapeAnchorType::Normal, None)
            .unwrap();
        let second = service
            .create_anchor_without_transcript("second", TapeAnchorType::Normal, None)
            .unwrap();
        assert_eq!(observer.search(TapeQuery::default()).unwrap().len(), 2);
        let removed = service
            .delete_anchor_entries(&[first.id.to_string(), first.id.to_string(), "missing".into()])
            .unwrap();
        assert_eq!(removed.len(), 1);
        let surviving = observer.search(TapeQuery::default()).unwrap();
        assert_eq!(surviving.len(), 1);
        assert_eq!(surviving[0].id, second.id);
        observer.reset().unwrap();
        assert!(service.search(TapeQuery::default()).unwrap().is_empty());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
