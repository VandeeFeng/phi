use compact_str::CompactString;
use serde::{Deserialize, Serialize};

use super::anchor::TapeAnchorType;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TapeContextStrategy {
    RecentOnly,
    Smart,
}

impl TapeContextStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RecentOnly => "recent-only",
            Self::Smart => "smart",
        }
    }
}

impl Default for TapeContextStrategy {
    fn default() -> Self {
        Self::Smart
    }
}

impl Default for TapeContextConfig {
    fn default() -> Self {
        Self {
            strategy: Some(TapeContextStrategy::Smart),
            file_limit: Some(10),
            memory_scan: Some((72, 168)),
            always_include: Vec::new(),
            whitelist: Vec::new(),
            blacklist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeSessionScanOptions {
    pub scan: Option<String>,
    #[serde(default)]
    pub types: Vec<String>,
    pub limit: Option<usize>,
    pub since_anchor: Option<String>,
    pub last_anchor: Option<bool>,
    pub between_anchors: Option<AnchorRange>,
    pub between_dates: Option<DateRange>,
    pub entry_scope: Option<TapeScope>,
    pub anchor_scope: Option<TapeScope>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeAnchorScanMode {
    Latest,
    All,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeAnchorScanOptions {
    pub id: Option<CompactString>,
    pub session_id: Option<CompactString>,
    pub session_entry_id: Option<CompactString>,
    pub name: Option<String>,
    pub name_case_insensitive: bool,
    pub scan: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    #[serde(rename = "type")]
    pub anchor_type: Option<TapeAnchorType>,
    pub summary: Option<String>,
    pub purpose: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub limit: Option<usize>,
    pub mode: Option<TapeAnchorScanMode>,
    pub include_orphans: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnchorRange {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DateRange {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeScope {
    Session,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSelection {
    pub files: Vec<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TapeKeywordConfig {
    #[serde(default)]
    pub global: Vec<String>,
    #[serde(default)]
    pub project: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TapeAnchorMode {
    Auto,
    Manual,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeConfig {
    pub enabled: Option<bool>,
    pub only_git: Option<bool>,
    #[serde(default)]
    pub exclude_dirs: Vec<String>,
    pub context: Option<TapeContextConfig>,
    pub anchor: Option<TapeAnchorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeContextConfig {
    pub strategy: Option<TapeContextStrategy>,
    pub file_limit: Option<usize>,
    pub memory_scan: Option<(u64, u64)>,
    #[serde(default)]
    pub always_include: Vec<String>,
    #[serde(default)]
    pub whitelist: Vec<String>,
    #[serde(default)]
    pub blacklist: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TapeAnchorConfig {
    pub label_prefix: Option<String>,
    pub mode: Option<TapeAnchorMode>,
    pub keywords: Option<TapeKeywordConfig>,
}
