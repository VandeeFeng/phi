use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;

use super::anchor::TapeAnchor;
use super::transaction::TapeProjectTransaction;
use super::types::{TapeAnchorScanMode, TapeAnchorScanOptions};

#[derive(Default)]
pub struct AnchorStore {
    index_path: Option<PathBuf>,
    transaction: Option<TapeProjectTransaction>,
    anchors: Mutex<Vec<TapeAnchor>>,
}

impl AnchorStore {
    pub fn new(tape_base_path: impl Into<PathBuf>, project_name: &str) -> anyhow::Result<Self> {
        let anchor_dir = tape_base_path.into();
        let store = Self {
            index_path: Some(anchor_dir.join(format!("{project_name}__anchors.jsonl"))),
            transaction: Some(TapeProjectTransaction::new(&anchor_dir, project_name)),
            anchors: Mutex::new(Vec::new()),
        };
        let _project = store.transaction.as_ref().unwrap().lock_existing()?;
        Ok(store)
    }

    pub fn append(&self, entry: TapeAnchor) -> anyhow::Result<()> {
        entry.validate()?;
        self.update(|entries| {
            anyhow::ensure!(
                !entries.iter().any(|existing| existing.id == entry.id),
                "Tape Anchor identity already exists"
            );
            entries.push(entry);
            Ok(())
        })
    }

    pub(crate) fn delete(&self, ids: &[String]) -> anyhow::Result<Vec<TapeAnchor>> {
        let ids = ids.iter().map(String::as_str).collect::<HashSet<_>>();
        self.update(|entries| {
            let mut removed = Vec::new();
            entries.retain(|anchor| {
                if ids.contains(anchor.id.as_str()) {
                    removed.push(anchor.clone());
                    false
                } else {
                    true
                }
            });
            Ok(removed)
        })
    }

    fn update<T>(
        &self,
        action: impl FnOnce(&mut Vec<TapeAnchor>) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if let Some(transaction) = &self.transaction {
            let guard = transaction.lock()?;
            let path = self.index_path.as_ref().expect("persistent anchor path");
            let mut anchors = read_anchors(path)?;
            let result = action(&mut anchors)?;
            guard.write_anchor(&serialize_anchors(&anchors)?)?;
            Ok(result)
        } else {
            let mut anchors = self.lock_anchors()?;
            action(&mut anchors)
        }
    }

    pub fn get_all_anchors(&self) -> anyhow::Result<Vec<TapeAnchor>> {
        let _project = self
            .transaction
            .as_ref()
            .map(|transaction| transaction.lock_existing())
            .transpose()?;
        match &self.index_path {
            Some(path) => read_anchors(path),
            None => Ok(self.lock_anchors()?.clone()),
        }
    }

    fn replace_all(&self, entries: Vec<TapeAnchor>) -> anyhow::Result<()> {
        if let Some(transaction) = &self.transaction {
            transaction
                .lock()?
                .write_anchor(&serialize_anchors(&entries)?)
        } else {
            *self.lock_anchors()? = entries;
            Ok(())
        }
    }

    pub(crate) fn transaction(&self) -> Option<&TapeProjectTransaction> {
        self.transaction.as_ref()
    }

    pub fn scan(&self, options: TapeAnchorScanOptions) -> anyhow::Result<Vec<TapeAnchor>> {
        let anchors = filter_by_scan_options(self.get_all_anchors()?, &options);
        let mut anchors = sort_anchors_by_timestamp(anchors);
        if options.mode == Some(TapeAnchorScanMode::Latest) {
            return Ok(anchors.pop().into_iter().collect());
        }
        Ok(anchors)
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        self.replace_all(Vec::new())
    }

    fn lock_anchors(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Vec<TapeAnchor>>> {
        self.anchors
            .lock()
            .map_err(|_| anyhow::anyhow!("anchor store lock poisoned"))
    }
}

pub(crate) fn read_anchors(path: &Path) -> anyhow::Result<Vec<TapeAnchor>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let mut anchors = Vec::new();
    let mut seen = HashSet::new();
    for (index, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let location = format!("invalid anchor in {} at line {}", path.display(), index + 1);
        let anchor: TapeAnchor = serde_json::from_str(line).with_context(|| location.clone())?;
        anchor.validate().with_context(|| location.clone())?;
        anyhow::ensure!(
            seen.insert(anchor.id.clone()),
            "{location}: duplicate anchor identity"
        );
        anchors.push(anchor);
    }
    Ok(anchors)
}

fn filter_by_scan_options(
    anchors: Vec<TapeAnchor>,
    options: &TapeAnchorScanOptions,
) -> Vec<TapeAnchor> {
    let since = options.since.as_deref().and_then(to_timestamp);
    let until = options.until.as_deref().and_then(to_timestamp);
    let lower_name = options
        .name_case_insensitive
        .then(|| options.name.as_ref().map(|name| name.to_lowercase()))
        .flatten();

    anchors
        .into_iter()
        .filter(|anchor| {
            if options.id.as_ref().is_some_and(|id| anchor.id != *id) {
                return false;
            }
            if options
                .session_id
                .as_ref()
                .is_some_and(|session_id| anchor.session_id != *session_id)
            {
                return false;
            }
            if options
                .session_entry_id
                .as_ref()
                .is_some_and(|entry_id| anchor.session_entry_id != *entry_id)
            {
                return false;
            }
            if options
                .anchor_type
                .is_some_and(|anchor_type| anchor.anchor_type != anchor_type)
            {
                return false;
            }
            if let Some(lower_name) = &lower_name {
                if !anchor.name.to_lowercase().contains(lower_name) {
                    return false;
                }
            } else if options
                .name
                .as_ref()
                .is_some_and(|name| !anchor.name.contains(name))
            {
                return false;
            }
            let anchor_time = to_timestamp(&anchor.timestamp).unwrap_or(0);
            if since.is_some_and(|since| anchor_time < since) {
                return false;
            }
            if until.is_some_and(|until| anchor_time > until) {
                return false;
            }
            if options.summary.as_ref().is_some_and(|summary| {
                !anchor
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.summary.as_deref())
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&summary.to_lowercase())
            }) {
                return false;
            }
            if options.purpose.as_ref().is_some_and(|purpose| {
                !anchor
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.purpose.as_deref())
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains(&purpose.to_lowercase())
            }) {
                return false;
            }
            if !keywords_match(anchor, &options.keywords) {
                return false;
            }
            if let Some(scan) = &options.scan {
                let needle = scan.to_lowercase();
                let meta = anchor
                    .meta
                    .as_ref()
                    .and_then(|meta| serde_json::to_string(meta).ok())
                    .unwrap_or_default()
                    .to_lowercase();
                if !anchor.name.to_lowercase().contains(&needle)
                    && !format!("{:?}", anchor.anchor_type)
                        .to_lowercase()
                        .contains(&needle)
                    && !meta.contains(&needle)
                {
                    return false;
                }
            }
            true
        })
        .collect()
}

fn keywords_match(anchor: &TapeAnchor, keywords: &[String]) -> bool {
    if keywords.is_empty() {
        return true;
    }
    let anchor_keywords = anchor
        .meta
        .as_ref()
        .map(|meta| {
            meta.keywords
                .iter()
                .map(|keyword| keyword.to_lowercase())
                .collect::<HashSet<_>>()
        })
        .unwrap_or_default();
    keywords
        .iter()
        .all(|keyword| anchor_keywords.contains(&keyword.to_lowercase()))
}

fn sort_anchors_by_timestamp(mut anchors: Vec<TapeAnchor>) -> Vec<TapeAnchor> {
    anchors.sort_by_key(|anchor| to_timestamp(&anchor.timestamp).unwrap_or(0));
    anchors
}

fn serialize_anchors(entries: &[TapeAnchor]) -> anyhow::Result<String> {
    let lines = entries
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    })
}

fn to_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::memory::tape::TapeAnchorType;
    use proptest::prelude::*;

    #[test]
    fn invalid_anchor_writes_preserve_readable_store() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-anchor-validation-{}", uuid::Uuid::new_v4()));
        let disk = AnchorStore::new(&directory, "project").unwrap();
        for store in [&AnchorStore::default(), &disk] {
            let mut anchor = TapeAnchor::new("checkpoint", TapeAnchorType::Normal);
            anchor.session_id = "session".into();
            anchor.session_entry_id = "entry".into();
            store.append(anchor.clone()).unwrap();
            for field in ["id", "name", "sessionId", "sessionEntryId", "timestamp"] {
                let mut invalid = serde_json::to_value(&anchor).unwrap();
                invalid["id"] = "invalid".into();
                invalid[field] = "".into();
                let invalid = serde_json::from_value(invalid).unwrap();
                assert!(store.append(invalid).is_err(), "empty {field}");
                assert_eq!(store.get_all_anchors().unwrap().len(), 1);
            }
            anchor.id = "next".into();
            store.append(anchor).unwrap();
            assert_eq!(store.get_all_anchors().unwrap().len(), 2);
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_canonical_records_block_reads_and_updates() {
        let directory =
            PathBuf::from("/tmp").join(format!("phi-anchor-corruption-{}", uuid::Uuid::new_v4()));
        let store = AnchorStore::new(&directory, "project").unwrap();
        let mut anchor = TapeAnchor::new("checkpoint", TapeAnchorType::Normal);
        anchor.session_id = "session".into();
        anchor.session_entry_id = "entry".into();
        store.append(anchor.clone()).unwrap();
        let path = directory.join("project__anchors.jsonl");
        let valid = fs::read_to_string(&path).unwrap();
        let mut invalid_records = vec!["{".to_string(), "{}".to_string(), valid.clone()];
        for field in ["id", "name", "sessionId", "sessionEntryId", "timestamp"] {
            let mut invalid = serde_json::to_value(&anchor).unwrap();
            invalid[field] = "".into();
            invalid_records.push(invalid.to_string());
        }
        for invalid in invalid_records {
            let content = format!("{valid}{invalid}\n");
            fs::write(&path, &content).unwrap();
            let error = store
                .get_all_anchors()
                .expect_err("invalid source must fail");
            assert!(error.to_string().contains(&path.display().to_string()));
            assert!(error.to_string().contains("line 2"));
            let mut appended = anchor.clone();
            appended.id = "new-anchor".into();
            assert!(store.append(appended).is_err());
            assert!(store.delete(&[anchor.id.to_string()]).is_err());
            assert_eq!(fs::read_to_string(&path).unwrap(), content);
        }
        fs::remove_dir_all(directory).unwrap();
    }

    proptest! {
        #[test]
        fn disk_and_memory_scans_share_filter_and_order(
            count in 0usize..150,
            session in 0usize..4,
            mode in prop::sample::select(vec![TapeAnchorScanMode::All, TapeAnchorScanMode::Latest]),
        ) {
            let directory = PathBuf::from("/tmp")
                .join(format!("phi-anchor-scan-{}", uuid::Uuid::new_v4()));
            let memory = AnchorStore::default();
            let disk = AnchorStore::new(&directory, "project").unwrap();
            let anchors = (0..count).rev().map(|index| {
                let mut anchor = TapeAnchor::new("checkpoint", TapeAnchorType::Normal);
                anchor.id = index.to_string().into();
                anchor.session_id = (index % 4).to_string().into();
                anchor.session_entry_id = format!("entry/{index}").into();
                anchor.timestamp = chrono::DateTime::from_timestamp(index as i64, 0)
                    .unwrap().to_rfc3339().into();
                anchor
            }).collect::<Vec<_>>();
            memory.replace_all(anchors.clone()).unwrap();
            disk.replace_all(anchors).unwrap();
            let options = TapeAnchorScanOptions {
                session_id: Some(session.to_string().into()),
                mode: Some(mode),
                ..Default::default()
            };
            let mut expected = (0..count).filter(|index| index % 4 == session)
                .map(|index| index.to_string()).collect::<Vec<_>>();
            if mode == TapeAnchorScanMode::Latest {
                expected = expected.pop().into_iter().collect();
            }
            for store in [&memory, &disk] {
                let actual = store.scan(options.clone()).unwrap().into_iter()
                    .map(|anchor| anchor.id.to_string()).collect::<Vec<_>>();
                prop_assert_eq!(actual, expected.clone());
            }
            fs::remove_dir_all(directory).unwrap();
        }
    }
}
