use crate::core::event::ToolResultDetails;
use compact_str::CompactString;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

/// Immutable payload text shared across transcript, event, and adapter projections.
///
/// This type describes storage, not canonical identity. `TranscriptEntry::id` and
/// the content index identify canonical content. Live streams remain mutable
/// `String`s and are frozen into `SharedText` only at committed boundaries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SharedText(Arc<str>);

impl SharedText {
    pub fn new(text: impl AsRef<str>) -> Self {
        Self(Arc::from(text.as_ref()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn shared(&self) -> Arc<str> {
        Arc::clone(&self.0)
    }
}

impl Deref for SharedText {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl AsRef<str> for SharedText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for SharedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl PartialEq<&str> for SharedText {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&str> for SharedText {
    fn from(text: &str) -> Self {
        Self::new(text)
    }
}

impl From<String> for SharedText {
    fn from(text: String) -> Self {
        Self(Arc::from(text))
    }
}

impl From<CompactString> for SharedText {
    fn from(text: CompactString) -> Self {
        Self::new(text)
    }
}

impl Serialize for SharedText {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SharedText {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub id: CompactString,
    pub parent_id: Option<CompactString>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<CompactString>,
    pub timestamp: CompactString,
    #[serde(flatten)]
    pub kind: TranscriptEntryKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TranscriptEntryKind {
    Message {
        message: AgentMessage,
    },
    Event {
        name: CompactString,
        data: serde_json::Value,
    },
    Failure {
        turn_id: Option<CompactString>,
        failure: crate::core::event::PhiFailure,
    },
    Compaction {
        summary: CompactString,
        tokens_before: u64,
    },
    Reset {
        handoff: Option<SharedText>,
    },
    ContextEdit {
        target_id: CompactString,
        #[serde(deserialize_with = "Option::deserialize")]
        replacement: Option<ContextReplacement>,
    },
}

/// Content-only replacement; the source message retains its role and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextReplacement {
    pub content: ContextContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContextContent {
    Text(SharedText),
    Blocks(Vec<ContentBlock>),
}

/// Effective configuration captured from the accepted execution generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SwitchSnapshot {
    pub name: CompactString,
    pub provider: CompactString,
    pub model: CompactString,
    pub thinking_level: Option<CompactString>,
    pub mode: CompactString,
    pub prompt: Option<SharedText>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchSnapshotRecord {
    pub id: CompactString,
    pub switch: SwitchSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum AgentMessage {
    User {
        content: Vec<UserContentBlock>,
    },
    Agent {
        content: Vec<ContentBlock>,
    },
    /// Non-authoritative context injected into a turn.
    ///
    /// This is not a system prompt or system instruction. Runtime code may merge it
    /// into a user prompt or other context carrier when constructing provider input.
    Context {
        source: ContextSource,
        scope: ContextScope,
        content: Vec<ContentBlock>,
    },
    Assistant {
        #[serde(default)]
        id: Option<CompactString>,
        #[serde(default)]
        turn_id: Option<CompactString>,
        content: Vec<ContentBlock>,
        provider: CompactString,
        api: Option<crate::provider::ProviderApi>,
        model: CompactString,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        switch: Option<CompactString>,
        usage: Usage,
        stop_reason: CompactString,
    },
    ToolResult {
        /// Refers to the Phi tool-call ID stored in `ContentBlock::ToolCall.id`.
        tool_call_id: CompactString,
        provider_call_id: Option<CompactString>,
        provider_item_id: Option<CompactString>,
        tool_name: CompactString,
        content: Vec<ContentBlock>,
        is_error: bool,
        #[serde(default)]
        details: Option<ToolResultDetails>,
    },
    /// Reserved for explicit mid-conversation system-role instructions.
    ///
    /// This is not the top-level runtime system prompt. Use only for operator-level
    /// instructions that should enter provider message history as a system role.
    System {
        content: SharedText,
    },
}

/// Stable extension-owned context provenance.
/// Identity establishes neither instruction authority nor insertion eligibility.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ContextKey {
    name: CompactString,
    kind: CompactString,
}

impl ContextKey {
    pub fn new(name: impl Into<CompactString>, kind: impl Into<CompactString>) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    FileMention,
    Hook,
    WorkflowIntro,
    Extension(ContextKey),
}

impl ContextSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileMention => "file_mention",
            Self::Hook => "hook",
            Self::WorkflowIntro => "workflow_intro",
            Self::Extension(_) => "extension",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextScope {
    Turn,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReasoningMetadata {
    #[serde(default)]
    pub id: Option<CompactString>,
    pub content: Vec<ReasoningMetadataContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReasoningMetadataContent {
    Text {
        text: CompactString,
        #[serde(default)]
        signature: Option<CompactString>,
    },
    Encrypted {
        data: CompactString,
    },
    Redacted {
        data: CompactString,
    },
    Summary {
        text: CompactString,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UserContentBlock {
    Text { text: SharedText },
    Image { source: ImageSource },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Url {
        url: CompactString,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime_type: Option<CompactString>,
    },
    Base64 {
        data: CompactString,
        mime_type: CompactString,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: SharedText,
    },
    Image {
        source: ImageSource,
    },
    Thinking {
        text: SharedText,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        metadata: Option<ReasoningMetadata>,
    },
    ToolCall {
        /// Phi's correlation ID for this call and its tool result, independent of provider IDs.
        id: CompactString,
        provider_call_id: Option<CompactString>,
        provider_item_id: Option<CompactString>,
        name: CompactString,
        args: serde_json::Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptToolCall {
    pub id: CompactString,
    pub name: CompactString,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptToolResult {
    pub entry_id: CompactString,
    pub tool_call_id: CompactString,
    pub content: Vec<ContentBlock>,
    pub is_error: bool,
    pub details: Option<ToolResultDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptToolExchange {
    pub call: TranscriptToolCall,
    pub result: TranscriptToolResult,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub total_tokens: u64,
    pub cost: f64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_kinds_round_trip_as_flat_fields() {
        use serde_json::json;

        for payload in [
            json!({"kind": "message", "message": {"role": "user", "content": []}}),
            json!({"kind": "event", "name": "notice", "data": {}}),
            json!({"kind": "failure", "turn_id": null, "failure": {"kind": "internal", "message": "failed"}}),
            json!({"kind": "compaction", "summary": "summary", "tokens_before": 100}),
            json!({"kind": "reset", "handoff": "continue here"}),
            json!({"kind": "context_edit", "target_id": "source", "replacement": null}),
            json!({"kind": "context_edit", "target_id": "source", "replacement": {"content": "replacement"}}),
        ] {
            let entry = TranscriptEntry {
                id: "entry".into(),
                parent_id: Some("parent".into()),
                head: None,
                timestamp: "now".into(),
                kind: serde_json::from_value(payload.clone()).unwrap(),
            };
            let mut expected = json!({"id": "entry", "parent_id": "parent", "timestamp": "now"});
            expected
                .as_object_mut()
                .unwrap()
                .extend(payload.as_object().unwrap().clone());
            assert_eq!(serde_json::to_value(&entry).unwrap(), expected);
            let restored: TranscriptEntry = serde_json::from_value(expected.clone()).unwrap();
            assert_eq!(serde_json::to_value(restored).unwrap(), expected);
        }
    }

    #[test]
    fn legacy_thinking_block_deserializes_without_metadata() {
        let block: ContentBlock = serde_json::from_value(serde_json::json!({
            "type": "thinking",
            "text": "summary"
        }))
        .unwrap();

        assert!(matches!(
            block,
            ContentBlock::Thinking { metadata: None, .. }
        ));
    }
}
