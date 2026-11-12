use compact_str::CompactString;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeAnchorType {
    Normal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeAnchorTrigger {
    Direct,
    Keyword,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitCheckpoint {
    /// User HEAD observed when the Anchor was finalized.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<CompactString>,
    /// Hidden Phi commit containing the captured workspace state.
    pub commit_oid: CompactString,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeAnchorMeta {
    /// Source workspace; present for both Git and non-Git projects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<CompactString>,
    pub trigger: Option<TapeAnchorTrigger>,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub summary: Option<String>,
    pub purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitCheckpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeAnchor {
    pub id: CompactString,
    pub name: CompactString,
    #[serde(rename = "type")]
    pub anchor_type: TapeAnchorType,
    pub session_id: CompactString,
    pub session_entry_id: CompactString,
    pub timestamp: CompactString,
    pub meta: Option<TapeAnchorMeta>,
}

impl TapeAnchor {
    pub fn new(name: impl Into<CompactString>, anchor_type: TapeAnchorType) -> Self {
        Self {
            id: CompactString::new(uuid::Uuid::new_v4().to_string()),
            name: name.into(),
            anchor_type,
            session_id: CompactString::default(),
            session_entry_id: CompactString::default(),
            timestamp: CompactString::new(chrono::Utc::now().to_rfc3339()),
            meta: None,
        }
    }

    pub(super) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.id.is_empty()
                && !self.name.is_empty()
                && !self.session_id.is_empty()
                && !self.session_entry_id.is_empty()
                && !self.timestamp.is_empty(),
            "required anchor fields must not be empty"
        );
        Ok(())
    }

    pub fn with_meta(mut self, meta: TapeAnchorMeta) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        let meta = self.meta.get_or_insert_with(TapeAnchorMeta::default);
        meta.summary = Some(summary.into());
        self
    }

    pub fn with_purpose(mut self, purpose: impl Into<String>) -> Self {
        let meta = self.meta.get_or_insert_with(TapeAnchorMeta::default);
        meta.purpose = Some(purpose.into());
        self
    }
}
