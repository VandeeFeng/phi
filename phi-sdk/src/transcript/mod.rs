use crate::core::event::ToolResultDetails;
use compact_str::CompactString;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

mod context;
mod types;

pub use context::TranscriptContext;
pub use types::{
    AgentMessage, ContentBlock, ContextContent, ContextKey, ContextReplacement, ContextScope,
    ContextSource, ImageSource, ReasoningMetadata, ReasoningMetadataContent, SharedText,
    SwitchSnapshot, SwitchSnapshotRecord, TranscriptEntry, TranscriptEntryKind, TranscriptToolCall,
    TranscriptToolExchange, TranscriptToolResult, Usage, UserContentBlock,
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transcript {
    pub id: CompactString,
    #[serde(default)]
    pub entries: Vec<TranscriptEntry>,
    pub switch_snapshots: Vec<SwitchSnapshotRecord>,
    #[serde(default)]
    pub leaf_id: Option<CompactString>,
    pub created_at: CompactString,
    pub updated_at: CompactString,
    pub total_tokens: u64,
    pub total_cost: f64,
    #[serde(default)]
    pub total_cache_read: u64,
    #[serde(default)]
    pub total_cache_write: u64,
}

impl Transcript {
    pub fn new() -> Self {
        let now = CompactString::new(chrono::Utc::now().to_rfc3339());
        Self {
            id: CompactString::new(Uuid::new_v4().to_string()),
            entries: Vec::new(),
            switch_snapshots: Vec::new(),
            leaf_id: None,
            created_at: now.clone(),
            updated_at: now,
            total_tokens: 0,
            total_cost: 0.0,
            total_cache_read: 0,
            total_cache_write: 0,
        }
    }

    pub fn append_user_message(&mut self, content: &str) {
        self.append_user_content(vec![UserContentBlock::Text {
            text: SharedText::new(content),
        }]);
    }

    pub fn append_user_content(&mut self, content: Vec<UserContentBlock>) {
        self.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::User { content },
        });
    }

    pub fn append_agent_message(&mut self, content: &str) {
        self.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::Agent {
                content: vec![ContentBlock::Text {
                    text: SharedText::new(content),
                }],
            },
        });
    }

    pub fn has_user_message(&self) -> bool {
        self.current_branch().iter().any(|entry| {
            matches!(
                &entry.kind,
                TranscriptEntryKind::Message {
                    message: AgentMessage::User { .. }
                }
            )
        })
    }

    pub fn append_context_message(
        &mut self,
        source: ContextSource,
        scope: ContextScope,
        content: &str,
    ) {
        self.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::Context {
                source,
                scope,
                content: vec![ContentBlock::Text {
                    text: SharedText::new(content),
                }],
            },
        });
    }

    pub fn append_system_instruction(&mut self, content: &str) {
        self.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::System {
                content: SharedText::new(content),
            },
        });
    }

    pub fn append_assistant_message(
        &mut self,
        id: Option<CompactString>,
        turn_id: Option<CompactString>,
        content: Vec<ContentBlock>,
        provider: CompactString,
        api: crate::provider::ProviderApi,
        model: CompactString,
        usage: Usage,
        stop_reason: CompactString,
    ) {
        self.append_assistant_with_switch(
            id,
            turn_id,
            content,
            provider,
            Some(api),
            model,
            None,
            usage,
            stop_reason,
        );
    }

    pub(crate) fn append_assistant_with_switch(
        &mut self,
        id: Option<CompactString>,
        turn_id: Option<CompactString>,
        content: Vec<ContentBlock>,
        provider: CompactString,
        api: Option<crate::provider::ProviderApi>,
        model: CompactString,
        switch: Option<SwitchSnapshot>,
        usage: Usage,
        stop_reason: CompactString,
    ) {
        self.total_tokens = self.total_tokens.saturating_add(usage.output);
        self.total_cache_read = self.total_cache_read.saturating_add(usage.cache_read);
        self.total_cache_write = self.total_cache_write.saturating_add(usage.cache_write);
        self.total_cost += usage.cost;
        let switch = switch.map(|snapshot| self.register_switch_snapshot(snapshot));
        self.append_entry(TranscriptEntryKind::Message {
            message: AgentMessage::Assistant {
                id,
                turn_id,
                content,
                provider,
                api,
                model,
                switch,
                usage,
                stop_reason,
            },
        });
    }

    fn register_switch_snapshot(&mut self, snapshot: SwitchSnapshot) -> CompactString {
        if let Some(record) = self
            .switch_snapshots
            .iter()
            .find(|record| record.switch == snapshot)
        {
            return record.id.clone();
        }
        let version = self
            .switch_snapshots
            .iter()
            .filter(|record| record.switch.name == snapshot.name)
            .count()
            + 1;
        let id = CompactString::new(format!("{}-{version}", snapshot.name));
        self.switch_snapshots.push(SwitchSnapshotRecord {
            id: id.clone(),
            switch: snapshot,
        });
        id
    }

    pub fn validate_switch_snapshots(&self) -> anyhow::Result<()> {
        let mut snapshots = HashMap::new();
        let mut versions = HashMap::new();
        let mut configurations = HashSet::new();
        for record in &self.switch_snapshots {
            let version = versions
                .entry(record.switch.name.as_str())
                .or_insert(0usize);
            *version += 1;
            anyhow::ensure!(
                record.id == format!("{}-{version}", record.switch.name),
                "invalid Switch snapshot ID `{}`",
                record.id
            );
            anyhow::ensure!(
                snapshots
                    .insert(record.id.as_str(), &record.switch)
                    .is_none(),
                "duplicate Switch snapshot `{}`",
                record.id
            );
            anyhow::ensure!(
                configurations.insert(&record.switch),
                "duplicate Switch snapshot configuration `{}`",
                record.id
            );
        }
        for entry in &self.entries {
            if let TranscriptEntryKind::Message {
                message:
                    AgentMessage::Assistant {
                        switch: Some(id),
                        provider,
                        model,
                        ..
                    },
            } = &entry.kind
            {
                let snapshot = snapshots
                    .get(id.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing Switch snapshot `{id}`"))?;
                anyhow::ensure!(
                    snapshot.provider == *provider && snapshot.model == *model,
                    "Switch snapshot `{id}` does not match response provider/model"
                );
            }
        }
        Ok(())
    }

    pub fn append_tool_result(
        &mut self,
        tool_call_id: &str,
        output: &str,
        is_error: bool,
        details: Option<ToolResultDetails>,
    ) {
        let (tool_name, provider_call_id, provider_item_id) =
            self.find_tool_call_metadata(tool_call_id);
        self.append_tool_result_with_metadata(
            None,
            tool_call_id,
            provider_call_id,
            provider_item_id,
            tool_name,
            output,
            is_error,
            details,
        );
    }

    pub(crate) fn append_tool_result_with_entry_id(
        &mut self,
        entry_id: CompactString,
        tool_call_id: &str,
        provider_call_id: Option<CompactString>,
        provider_item_id: Option<CompactString>,
        tool_name: Option<CompactString>,
        output: &str,
        is_error: bool,
        details: Option<ToolResultDetails>,
    ) {
        let (stored_name, stored_call_id, stored_item_id) =
            self.find_tool_call_metadata(tool_call_id);
        self.append_tool_result_with_metadata(
            Some(entry_id),
            tool_call_id,
            provider_call_id.or(stored_call_id),
            provider_item_id.or(stored_item_id),
            tool_name.or(stored_name),
            output,
            is_error,
            details,
        );
    }

    fn append_tool_result_with_metadata(
        &mut self,
        entry_id: Option<CompactString>,
        tool_call_id: &str,
        provider_call_id: Option<CompactString>,
        provider_item_id: Option<CompactString>,
        tool_name: Option<CompactString>,
        output: &str,
        is_error: bool,
        details: Option<ToolResultDetails>,
    ) {
        let mut content = Vec::new();
        let mut cursor = 0;
        if let Some(details) = &details {
            let ordered = details.image_offsets.len() == details.images.len()
                && details
                    .image_offsets
                    .windows(2)
                    .all(|pair| pair[0] <= pair[1])
                && details
                    .image_offsets
                    .iter()
                    .all(|offset| output.is_char_boundary(*offset));
            for (index, source) in details.images.iter().enumerate() {
                let offset = if ordered {
                    details.image_offsets[index]
                } else {
                    output.len()
                };
                if offset > cursor {
                    content.push(ContentBlock::Text {
                        text: SharedText::new(&output[cursor..offset]),
                    });
                }
                content.push(ContentBlock::Image {
                    source: source.clone(),
                });
                cursor = offset;
            }
        }
        if cursor < output.len() || content.is_empty() {
            content.push(ContentBlock::Text {
                text: SharedText::new(&output[cursor..]),
            });
        }
        let kind = TranscriptEntryKind::Message {
            message: AgentMessage::ToolResult {
                tool_call_id: CompactString::new(tool_call_id),
                provider_call_id,
                provider_item_id,
                tool_name: tool_name.unwrap_or_else(|| CompactString::new("tool")),
                content,
                is_error,
                details,
            },
        };
        match entry_id {
            Some(entry_id) => self.append_entry_with_id(entry_id, kind),
            None => self.append_entry(kind),
        };
    }

    pub(crate) fn append_nested_tool_call(
        &mut self,
        entry_id: CompactString,
        call: &crate::core::event::NestedToolCall,
    ) {
        self.append_entry_with_id(
            entry_id,
            TranscriptEntryKind::Event {
                name: "nested_tool_call".into(),
                data: serde_json::to_value(call).expect("nested call is JSON serializable"),
            },
        );
    }

    pub(crate) fn new_entry_id() -> CompactString {
        CompactString::new(&Uuid::new_v4().to_string()[..8])
    }

    pub fn append_entry(&mut self, kind: TranscriptEntryKind) -> CompactString {
        self.append_entry_with_id(Self::new_entry_id(), kind)
    }

    fn append_entry_with_id(
        &mut self,
        id: CompactString,
        kind: TranscriptEntryKind,
    ) -> CompactString {
        self.append_entry_with_head(id, None, kind)
    }

    fn append_entry_with_head(
        &mut self,
        id: CompactString,
        head: Option<CompactString>,
        kind: TranscriptEntryKind,
    ) -> CompactString {
        let entry = TranscriptEntry {
            id: id.clone(),
            parent_id: self.leaf_id.clone(),
            head,
            timestamp: CompactString::new(chrono::Utc::now().to_rfc3339()),
            kind,
        };
        self.entries.push(entry);
        self.leaf_id = Some(id.clone());
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        id
    }

    pub fn select_branch(&mut self, leaf_id: Option<&str>) -> anyhow::Result<()> {
        if let Some(id) = leaf_id {
            anyhow::ensure!(
                self.entries.iter().any(|entry| entry.id == id),
                "unknown transcript branch entry `{id}`"
            );
        }
        let previous = self.leaf_id.clone();
        self.leaf_id = leaf_id.map(CompactString::new);
        if let Err(error) = self.effective_context() {
            self.leaf_id = previous;
            return Err(error);
        }
        self.updated_at = CompactString::new(chrono::Utc::now().to_rfc3339());
        Ok(())
    }

    pub fn current_branch(&self) -> Vec<&TranscriptEntry> {
        let mut branch = Vec::new();
        if self.leaf_id.is_none() {
            return branch;
        }
        // Index once: scanning every ancestor is O(n²) on long session chains.
        // Keep the index local because public entries can be replaced or reordered.
        // or_insert preserves the previous first-match behavior for duplicate IDs.
        let mut entries = HashMap::with_capacity(self.entries.len());
        for entry in &self.entries {
            entries.entry(entry.id.as_str()).or_insert(entry);
        }
        let mut current_id = self.leaf_id.as_deref();
        let mut visited = HashSet::new();
        while let Some(id) = current_id {
            if !visited.insert(id) {
                break;
            }
            let Some(entry) = entries.get(id).copied() else {
                break;
            };
            branch.push(entry);
            current_id = entry.parent_id.as_deref();
        }
        branch.reverse();
        branch
    }

    pub fn tool_exchange(&self, entry_id: &str) -> Option<TranscriptToolExchange> {
        let entry = self.entries.iter().find(|entry| entry.id == entry_id)?;
        let TranscriptEntryKind::Message {
            message:
                AgentMessage::ToolResult {
                    tool_call_id,
                    content,
                    is_error,
                    details,
                    ..
                },
        } = &entry.kind
        else {
            return None;
        };
        let mut ancestor_id = entry.parent_id.as_ref();
        let call = loop {
            let ancestor = self
                .entries
                .iter()
                .find(|candidate| Some(&candidate.id) == ancestor_id)?;
            if let TranscriptEntryKind::Message {
                message: AgentMessage::Assistant { content, .. },
            } = &ancestor.kind
                && let Some(call) = content.iter().find_map(|block| match block {
                    ContentBlock::ToolCall { id, name, args, .. } if id == tool_call_id => {
                        Some(TranscriptToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            args: args.clone(),
                        })
                    }
                    _ => None,
                })
            {
                break call;
            }
            ancestor_id = ancestor.parent_id.as_ref();
        };
        Some(TranscriptToolExchange {
            call,
            result: TranscriptToolResult {
                entry_id: entry.id.clone(),
                tool_call_id: tool_call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
                details: details.clone(),
            },
        })
    }

    fn find_tool_call_metadata(
        &self,
        tool_call_id: &str,
    ) -> (
        Option<CompactString>,
        Option<CompactString>,
        Option<CompactString>,
    ) {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                TranscriptEntryKind::Message {
                    message: AgentMessage::Assistant { content, .. },
                } => content.iter().find_map(|block| match block {
                    ContentBlock::ToolCall {
                        id,
                        name,
                        provider_call_id,
                        provider_item_id,
                        ..
                    } if id == tool_call_id => Some((
                        Some(name.clone()),
                        provider_call_id.clone(),
                        provider_item_id.clone(),
                    )),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or((None, None, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    proptest::proptest! {
        #[test]
        fn switch_snapshots_reuse_session_wide_ids_across_branches(
            selections in proptest::collection::vec((
                proptest::sample::select(vec!["review", "plan", "review-1"]),
                "[a-z]{1,6}", proptest::option::of("[a-z ]{0,20}")
            ), 1..24),
        ) {
            let mut transcript = Transcript::new();
            transcript.append_user_message("shared root");
            let root = transcript.leaf_id.clone().unwrap();
            let mut expected = Vec::<SwitchSnapshot>::new();
            for (name, model, prompt) in selections {
                let snapshot = SwitchSnapshot {
                    name: name.into(), provider: "provider".into(), model: model.into(),
                    thinking_level: Some("high".into()), mode: "plan".into(),
                    prompt: prompt.map(Into::into),
                };
                let index = match expected.iter().position(|existing| existing == &snapshot) {
                    Some(index) => index,
                    None => { expected.push(snapshot.clone()); expected.len() - 1 }
                };
                transcript.select_branch(Some(&root)).unwrap();
                transcript.append_assistant_with_switch(
                    None, None, Vec::new(), snapshot.provider.clone(), Some(crate::provider::ProviderApi::OpenAiCompletions), snapshot.model.clone(),
                    Some(snapshot), Usage::default(), "stop".into(),
                );
                let TranscriptEntryKind::Message { message: AgentMessage::Assistant { switch, .. } } =
                    &transcript.entries.last().unwrap().kind else { unreachable!() };
                let version = expected[..=index].iter().filter(|snapshot| snapshot.name == name).count();
                let expected_id = format!("{name}-{version}");
                proptest::prop_assert_eq!(switch.as_deref(), Some(expected_id.as_str()));
                proptest::prop_assert_eq!(transcript.switch_snapshots.len(), expected.len());
                transcript.validate_switch_snapshots().unwrap();
                transcript = serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
            }
            let first = expected[0].clone();
            let first_id = format!("{}-1", first.name);
            transcript.select_branch(Some(&root)).unwrap();
            transcript.append_assistant_with_switch(
                None, None, Vec::new(), first.provider.clone(), Some(crate::provider::ProviderApi::OpenAiCompletions), first.model.clone(),
                Some(first), Usage::default(), "stop".into(),
            );
            proptest::prop_assert_eq!(transcript.switch_snapshots.len(), expected.len());
            proptest::prop_assert!(matches!(&transcript.entries.last().unwrap().kind,
                TranscriptEntryKind::Message { message: AgentMessage::Assistant { switch, .. } }
                    if switch.as_deref() == Some(first_id.as_str())
            ), "original configuration must reuse its session-wide ID");
            proptest::prop_assert_eq!(transcript.current_branch().len(), 2);
            transcript.validate_switch_snapshots().unwrap();
        }
    }

    #[test]
    fn switch_snapshot_validation_rejects_corrupt_references_and_records() {
        let mut transcript = Transcript::new();
        let snapshot = SwitchSnapshot {
            name: "review".into(),
            provider: "provider".into(),
            model: "model".into(),
            thinking_level: None,
            mode: "default".into(),
            prompt: None,
        };
        transcript.append_assistant_with_switch(
            None,
            None,
            Vec::new(),
            "provider".into(),
            Some(crate::provider::ProviderApi::OpenAiCompletions),
            "model".into(),
            Some(snapshot),
            Usage::default(),
            "stop".into(),
        );
        let mut missing = transcript.clone();
        missing.switch_snapshots.clear();
        let mut duplicate = transcript.clone();
        duplicate
            .switch_snapshots
            .push(duplicate.switch_snapshots[0].clone());
        let mut mismatched = transcript.clone();
        mismatched.switch_snapshots[0].switch.model = "other".into();
        let mut invalid_id = transcript.clone();
        invalid_id.switch_snapshots[0].id = "review-2".into();
        for invalid in [missing, duplicate, mismatched, invalid_id] {
            assert!(invalid.validate_switch_snapshots().is_err());
        }
        transcript.validate_switch_snapshots().unwrap();
    }

    #[test]
    fn committed_single_text_payload_is_shared() {
        let mut transcript = Transcript::new();
        let text = SharedText::new("committed response");
        transcript.append_assistant_message(
            None,
            None,
            vec![ContentBlock::Text { text: text.clone() }],
            CompactString::new("provider"),
            crate::provider::ProviderApi::OpenAiCompletions,
            CompactString::new("model"),
            Usage::default(),
            CompactString::new("stop"),
        );

        let TranscriptEntryKind::Message {
            message: AgentMessage::Assistant { content, .. },
        } = &transcript.entries[0].kind
        else {
            panic!("assistant entry missing");
        };
        let ContentBlock::Text { text: committed } = &content[0] else {
            panic!("assistant text missing");
        };
        assert!(Arc::ptr_eq(&committed.shared(), &text.shared()));
    }

    #[test]
    fn selecting_branch_preserves_history_and_selects_context() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("first");
        let first = transcript.leaf_id.clone().unwrap();
        transcript.append_user_message("second");
        let entries = transcript.entries.len();

        transcript.select_branch(Some(&first)).unwrap();
        assert_eq!(transcript.effective_context().unwrap().entries.len(), 1);
        assert_eq!(transcript.current_branch()[0].id, first);
        assert_eq!(transcript.entries.len(), entries);
        assert!(transcript.select_branch(Some("missing")).is_err());
        assert_eq!(transcript.leaf_id.as_deref(), Some(first.as_str()));

        transcript.select_branch(None).unwrap();
        assert!(transcript.current_branch().is_empty());
        assert!(transcript.effective_context().unwrap().entries.is_empty());
    }

    #[test]
    fn current_branch_follows_parent_ids_after_entries_are_reordered() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("root");
        let root = transcript.leaf_id.clone().unwrap();
        transcript.append_user_message("left");
        let left = transcript.leaf_id.clone().unwrap();
        transcript.select_branch(Some(&root)).unwrap();
        transcript.append_user_message("right");
        let right = transcript.leaf_id.clone().unwrap();
        transcript.entries.reverse();

        let ids = transcript
            .current_branch()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![root.clone(), right]);
        transcript.select_branch(Some(&left)).unwrap();
        let ids = transcript
            .current_branch()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![root, left]);
        assert_eq!(
            transcript.effective_context().unwrap().entries[1].id,
            transcript.leaf_id.clone().unwrap()
        );
    }

    #[test]
    fn current_branch_preserves_first_match_and_missing_parent_behavior() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("first");
        let first = transcript.entries[0].clone();
        let mut duplicate = first.clone();
        duplicate.parent_id = Some(CompactString::new("missing"));
        transcript.entries.push(duplicate);
        assert!(std::ptr::eq(
            transcript.current_branch()[0],
            &transcript.entries[0]
        ));

        transcript.entries[0].parent_id = Some(CompactString::new("missing"));
        assert_eq!(transcript.current_branch().len(), 1);
        transcript.leaf_id = Some(CompactString::new("missing"));
        assert!(transcript.current_branch().is_empty());
    }

    #[test]
    fn current_branch_restores_a_long_history_in_canonical_order() {
        let mut transcript = Transcript::new();
        for index in 0..10_000 {
            transcript.append_user_message("history");
            let entry = transcript.entries.last_mut().unwrap();
            entry.id = CompactString::new(format!("entry-{index}"));
            transcript.leaf_id = Some(entry.id.clone());
        }
        let branch = transcript.current_branch();
        assert_eq!(branch.len(), transcript.entries.len());
        assert!(
            branch
                .iter()
                .zip(&transcript.entries)
                .all(|(actual, expected)| std::ptr::eq(*actual, expected))
        );
    }

    #[test]
    fn tool_exchange_correlates_result_with_call() {
        let transcript: Transcript = serde_json::from_value(serde_json::json!({
            "id": "session",
            "messages": [],
            "entries": [
                {
                    "id": "assistant",
                    "parent_id": null,
                    "timestamp": "now",
                    "kind": "message",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_call",
                            "id": "call",
                            "name": "Edit",
                            "args": { "path": "src/main.rs" }
                        }],
                        "provider": "test",
                        "api": crate::provider::ProviderApi::OpenAiCompletions,
                        "model": "test",
                        "usage": {
                            "input": 0,
                            "output": 0,
                            "total_tokens": 0,
                            "cost": 0.0
                        },
                        "stop_reason": "tool_use"
                    }
                },
                {
                    "id": "result",
                    "parent_id": "assistant",
                    "timestamp": "now",
                    "kind": "message",
                    "message": {
                        "role": "tool_result",
                        "tool_call_id": "call",
                        "tool_name": "Edit",
                        "content": [],
                        "is_error": false
                    }
                },
                {
                    "id": "other-branch",
                    "parent_id": null,
                    "timestamp": "later",
                    "kind": "message",
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_call",
                            "id": "call",
                            "name": "Write",
                            "args": { "path": "wrong-branch.rs" }
                        }],
                        "provider": "test",
                        "api": crate::provider::ProviderApi::OpenAiCompletions,
                        "model": "test",
                        "usage": {
                            "input": 0,
                            "output": 0,
                            "total_tokens": 0,
                            "cost": 0.0
                        },
                        "stop_reason": "tool_use"
                    }
                }
            ],
            "switch_snapshots": [],
            "leaf_id": "result",
            "created_at": "now",
            "updated_at": "now",
            "total_tokens": 0,
            "total_cost": 0.0,
            "total_cache_read": 0,
            "total_cache_write": 0,
            "total_estimated_tokens": 0
        }))
        .unwrap();

        let exchange = transcript.tool_exchange("result").unwrap();

        assert_eq!(exchange.call.name, "Edit");
        assert_eq!(exchange.call.args["path"], "src/main.rs");
        assert_eq!(exchange.result.entry_id, "result");
    }
}
