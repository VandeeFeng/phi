use std::borrow::Cow;
use std::collections::HashMap;

use super::{
    AgentMessage, ContentBlock, ContextContent, ContextReplacement, Transcript, TranscriptEntry,
    TranscriptEntryKind, UserContentBlock,
};

const ESTIMATED_IMAGE_TOKENS: u64 = 1_200;

/// Branch-local model context; entry references retain canonical identity.
#[derive(Debug)]
pub struct TranscriptContext<'a> {
    pub marker: Option<&'a TranscriptEntry>,
    pub entries: Vec<&'a TranscriptEntry>,
}

impl<'a> TranscriptContext<'a> {
    pub fn preamble_text(&self) -> Option<&str> {
        match &self.marker?.kind {
            TranscriptEntryKind::Compaction { summary, .. } => Some(summary),
            TranscriptEntryKind::Reset { handoff } => handoff.as_deref(),
            _ => None,
        }
    }

    /// Branch-local content projection; borrowed entries remain canonical sources.
    pub fn projected_entries(&self) -> Vec<Cow<'a, TranscriptEntry>> {
        self.project_entries().flatten().collect()
    }

    fn project_entries(
        &self,
    ) -> impl DoubleEndedIterator<Item = Option<Cow<'a, TranscriptEntry>>> + ExactSizeIterator + '_
    {
        let edits = self
            .entries
            .iter()
            .filter_map(|entry| match &entry.kind {
                TranscriptEntryKind::ContextEdit {
                    target_id,
                    replacement,
                } => Some((target_id.as_str(), replacement)),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        self.entries
            .iter()
            .map(move |entry| project_entry(entry, &edits))
    }

    pub fn estimated_tokens(&self) -> u64 {
        let preamble_tokens = self
            .preamble_text()
            .map(Transcript::estimate_tokens)
            .unwrap_or(0);
        self.project_entries()
            .flatten()
            .fold(preamble_tokens, |total, entry| {
                total.saturating_add(entry.estimated_tokens())
            })
    }

    fn retained_boundary(&self, entry_id: &str) -> Option<usize> {
        let boundary = self.entries.iter().position(|entry| entry.id == entry_id)?;
        (boundary > 0 && self.safe_boundary(boundary) == boundary).then_some(boundary)
    }

    pub(crate) fn compaction_boundary(&self, keep_recent_tokens: u64) -> usize {
        let mut kept_tokens: u64 = 0;
        let mut boundary = 0;
        for (index, entry) in self.project_entries().enumerate().rev() {
            let tokens = entry.map_or(0, |entry| entry.estimated_tokens());
            kept_tokens = kept_tokens.saturating_add(tokens);
            boundary = index;
            if kept_tokens >= keep_recent_tokens {
                break;
            }
        }
        self.safe_boundary(boundary)
    }

    pub(crate) fn safe_boundary(&self, mut boundary: usize) -> usize {
        let mut calls = HashMap::new();
        let dependencies = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| entry.context_dependency(index, &mut calls))
            .collect::<Vec<_>>();
        for index in (0..dependencies.len()).rev() {
            if index < boundary {
                break;
            }
            boundary = boundary.min(dependencies[index]);
        }
        boundary
    }
}

fn validate_context_edit<'a>(
    branch: &[&'a TranscriptEntry],
    target_id: &str,
    replacement: Option<&ContextReplacement>,
) -> anyhow::Result<&'a TranscriptEntry> {
    let target = branch
        .iter()
        .find(|entry| entry.id == target_id)
        .ok_or_else(|| {
            anyhow::anyhow!("context edit target must precede it on the active branch: {target_id}")
        })?;
    let TranscriptEntryKind::Message { message } = &target.kind else {
        anyhow::bail!("context edit target does not produce editable message content");
    };
    anyhow::ensure!(
        !matches!(message, AgentMessage::System { .. }),
        "system messages cannot be context-edited"
    );
    if matches!(message, AgentMessage::User { .. })
        && let Some(ContextReplacement {
            content: ContextContent::Blocks(blocks),
        }) = replacement
    {
        anyhow::ensure!(
            blocks.iter().all(|block| matches!(
                block,
                ContentBlock::Text { .. } | ContentBlock::Image { .. }
            )),
            "user context replacement accepts only text and images"
        );
    }
    Ok(target)
}

fn project_entry<'a>(
    entry: &'a TranscriptEntry,
    edits: &HashMap<&str, &Option<ContextReplacement>>,
) -> Option<Cow<'a, TranscriptEntry>> {
    let TranscriptEntryKind::Message { .. } = &entry.kind else {
        return None;
    };
    let Some(replacement) = edits.get(entry.id.as_str()) else {
        return Some(Cow::Borrowed(entry));
    };
    let replacement = replacement.as_ref()?;
    let blocks = match &replacement.content {
        ContextContent::Text(text) => vec![ContentBlock::Text { text: text.clone() }],
        ContextContent::Blocks(blocks) => blocks.clone(),
    };
    let mut projected = entry.clone();
    let TranscriptEntryKind::Message { message } = &mut projected.kind else {
        unreachable!();
    };
    match message {
        AgentMessage::User { content } => {
            *content = blocks
                .into_iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(UserContentBlock::Text { text }),
                    ContentBlock::Image { source } => Some(UserContentBlock::Image { source }),
                    _ => None,
                })
                .collect();
        }
        AgentMessage::Assistant { content, .. }
        | AgentMessage::ToolResult { content, .. }
        | AgentMessage::Agent { content }
        | AgentMessage::Context { content, .. } => *content = blocks,
        AgentMessage::System { .. } => unreachable!("context edits cannot target system messages"),
    }
    Some(Cow::Owned(projected))
}

impl Transcript {
    pub fn estimate_tokens(text: &str) -> u64 {
        (text.len() as u64 / 4).max(1)
    }

    pub fn needs_compaction(&self, context_window: u64, reserve_tokens: u64) -> bool {
        if context_window == 0 {
            return false;
        }
        self.estimated_context_tokens() > context_window.saturating_sub(reserve_tokens)
    }

    pub fn effective_context(&self) -> anyhow::Result<TranscriptContext<'_>> {
        let branch = self.current_branch();
        anyhow::ensure!(
            branch.last().map(|entry| &entry.id) == self.leaf_id.as_ref()
                && branch.first().is_none_or(|entry| entry.parent_id.is_none()),
            "context branch has missing or cyclic ancestry"
        );
        let mut context = TranscriptContext {
            marker: None,
            entries: Vec::new(),
        };
        for (index, entry) in branch.iter().enumerate() {
            match &entry.kind {
                TranscriptEntryKind::Reset { .. } => {
                    anyhow::ensure!(
                        entry.head.as_ref() == Some(&entry.id),
                        "reset must head itself"
                    );
                    context.marker = Some(entry);
                    context.entries.clear();
                }
                TranscriptEntryKind::Compaction { summary, .. } => {
                    anyhow::ensure!(!summary.trim().is_empty(), "compaction summary is empty");
                    let boundary = entry
                        .head
                        .as_deref()
                        .and_then(|head| context.retained_boundary(head));
                    let boundary = boundary.ok_or_else(|| anyhow::anyhow!(
                        "compaction head is outside the active context or splits a tool exchange or injected context"
                    ))?;
                    context.marker = Some(entry);
                    context.entries.drain(..boundary);
                }
                TranscriptEntryKind::ContextEdit {
                    target_id,
                    replacement,
                } => {
                    anyhow::ensure!(
                        entry.head.is_none(),
                        "context edit cannot have a context head"
                    );
                    validate_context_edit(&branch[..index], target_id, replacement.as_ref())?;
                    context.entries.push(entry);
                }
                _ => {
                    anyhow::ensure!(entry.head.is_none(), "non-marker entry has a context head");
                    if matches!(entry.kind, TranscriptEntryKind::Message { .. }) {
                        context.entries.push(entry);
                    }
                }
            }
        }
        Ok(context)
    }

    pub fn estimated_context_tokens(&self) -> u64 {
        self.effective_context()
            .map(|context| context.estimated_tokens())
            .unwrap_or(0)
    }

    pub fn compress(&mut self, summary: String, head: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!summary.trim().is_empty(), "compaction summary is empty");
        let context = self.effective_context()?;
        anyhow::ensure!(
            context.retained_boundary(head).is_some(),
            "compaction head is outside the active context or splits a tool exchange or injected context"
        );
        let tokens_before = context.estimated_tokens();
        self.append_entry_with_head(
            Self::new_entry_id(),
            Some(head.into()),
            TranscriptEntryKind::Compaction {
                summary: summary.into(),
                tokens_before,
            },
        );
        Ok(())
    }

    /// Append an edit on the current branch without rewriting its source entry.
    pub fn append_context_edit(
        &mut self,
        target_id: &str,
        mut replacement: Option<ContextReplacement>,
    ) -> anyhow::Result<compact_str::CompactString> {
        let branch = self.current_branch();
        let target = validate_context_edit(&branch, target_id, replacement.as_ref())?;
        if !matches!(
            &target.kind,
            TranscriptEntryKind::Message {
                message: AgentMessage::User { .. }
            }
        ) && let Some(replacement) = &mut replacement
            && let ContextContent::Text(text) = &replacement.content
        {
            replacement.content =
                ContextContent::Blocks(vec![ContentBlock::Text { text: text.clone() }]);
        }
        self.effective_context()?;
        Ok(self.append_entry(TranscriptEntryKind::ContextEdit {
            target_id: target_id.into(),
            replacement,
        }))
    }

    pub fn reset_context(
        &mut self,
        handoff: Option<String>,
    ) -> anyhow::Result<compact_str::CompactString> {
        self.effective_context()?;
        let id = Self::new_entry_id();
        let handoff = handoff
            .filter(|text| !text.trim().is_empty())
            .map(Into::into);
        Ok(self.append_entry_with_head(
            id.clone(),
            Some(id),
            TranscriptEntryKind::Reset { handoff },
        ))
    }
}

impl TranscriptEntry {
    fn context_dependency<'a>(
        &'a self,
        index: usize,
        calls: &mut HashMap<&'a str, usize>,
    ) -> usize {
        let TranscriptEntryKind::Message { message } = &self.kind else {
            return index;
        };
        match message {
            AgentMessage::Assistant { content, .. } => {
                for block in content {
                    if let ContentBlock::ToolCall { id, .. } = block {
                        calls.insert(id.as_str(), index);
                    }
                }
                index
            }
            AgentMessage::ToolResult { tool_call_id, .. } => {
                calls.get(tool_call_id.as_str()).copied().unwrap_or(0)
            }
            AgentMessage::Context { .. } => index.saturating_sub(1),
            _ => index,
        }
    }

    pub(crate) fn estimated_tokens(&self) -> u64 {
        let TranscriptEntryKind::Message { message } = &self.kind else {
            return 0;
        };
        match message {
            AgentMessage::User { content } => content.iter().fold(0_u64, |total, block| {
                total.saturating_add(match block {
                    UserContentBlock::Text { text } => Transcript::estimate_tokens(text),
                    UserContentBlock::Image { .. } => ESTIMATED_IMAGE_TOKENS,
                })
            }),
            AgentMessage::System { content } => Transcript::estimate_tokens(content),
            AgentMessage::Agent { content }
            | AgentMessage::Context { content, .. }
            | AgentMessage::Assistant { content, .. }
            | AgentMessage::ToolResult { content, .. } => {
                content.iter().fold(0_u64, |total, block| {
                    total.saturating_add(block.estimated_tokens())
                })
            }
        }
    }
}

impl ContentBlock {
    fn estimated_tokens(&self) -> u64 {
        match self {
            Self::Text { text } | Self::Thinking { text, .. } => Transcript::estimate_tokens(text),
            Self::Image { .. } => ESTIMATED_IMAGE_TOKENS,
            Self::ToolCall { name, args, .. } => Transcript::estimate_tokens(name)
                .saturating_add(Transcript::estimate_tokens(&args.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{ContextScope, ContextSource, ImageSource, Usage};

    fn replacement(text: &str) -> Option<ContextReplacement> {
        Some(ContextReplacement {
            content: ContextContent::Text(text.into()),
        })
    }

    #[test]
    fn context_edits_are_append_only_branch_local_and_content_only() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("original question");
        let user = transcript.leaf_id.clone().unwrap();
        transcript.append_assistant_message(
            Some("provider-message".into()),
            Some("turn".into()),
            vec![ContentBlock::Text {
                text: "original answer".into(),
            }],
            "provider".into(),
            crate::provider::ProviderApi::OpenAiCompletions,
            "model".into(),
            Usage {
                input: 7,
                output: 2,
                total_tokens: 9,
                cost: 0.5,
                cache_read: 3,
                cache_write: 1,
            },
            "stop".into(),
        );
        let source = transcript.entries.last().unwrap().clone();
        let initial = serde_json::to_value(&transcript).unwrap();
        transcript
            .append_context_edit(&user, replacement("revised question"))
            .unwrap();
        transcript.append_context_edit(&source.id, None).unwrap();
        let omitted = transcript.leaf_id.clone().unwrap();
        let projection = transcript.effective_context().unwrap().projected_entries();
        assert_eq!(projection.len(), 1);
        assert_eq!(projection[0].id, user);
        assert!(
            serde_json::to_string(&projection[0])
                .unwrap()
                .contains("revised question")
        );
        transcript
            .append_context_edit(&source.id, replacement("revised answer"))
            .unwrap();
        let reopened: Transcript =
            serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
        let projection = reopened.effective_context().unwrap().projected_entries();
        let TranscriptEntryKind::Message {
            message: AgentMessage::Assistant { content, .. },
        } = &projection[1].kind
        else {
            panic!("expected assistant");
        };
        assert!(
            matches!(&content[..], [ContentBlock::Text { text }] if text.as_str() == "revised answer")
        );
        let mut projected = serde_json::to_value(&projection[1]).unwrap();
        let original = serde_json::to_value(&source).unwrap();
        projected["message"]["content"] = original["message"]["content"].clone();
        assert_eq!(projected, original);
        assert_eq!(
            serde_json::to_value(&transcript.entries[0]).unwrap(),
            initial["entries"][0]
        );
        assert_eq!(
            serde_json::to_value(&transcript.entries[1]).unwrap(),
            initial["entries"][1]
        );
        let current = serde_json::to_value(&transcript).unwrap();
        for field in [
            "total_tokens",
            "total_cost",
            "total_cache_read",
            "total_cache_write",
            "switch_snapshots",
        ] {
            assert_eq!(current[field], initial[field]);
        }
        transcript.select_branch(Some(&source.id)).unwrap();
        assert!(
            serde_json::to_string(&transcript.effective_context().unwrap().projected_entries())
                .unwrap()
                .contains("original question")
        );
        transcript.select_branch(Some(&omitted)).unwrap();
        assert_eq!(
            transcript
                .effective_context()
                .unwrap()
                .projected_entries()
                .len(),
            1
        );
        transcript.append_user_message("continue here");
        assert_eq!(
            transcript.entries.last().unwrap().parent_id.as_ref(),
            Some(&omitted)
        );
    }

    #[test]
    fn compaction_and_reset_select_edits_before_projecting_content() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("prefix");
        let prefix = transcript.leaf_id.clone().unwrap();
        transcript.append_user_message("retained original");
        let retained = transcript.leaf_id.clone().unwrap();
        transcript
            .append_context_edit(&retained, replacement("retained edited"))
            .unwrap();
        transcript.append_context_edit(&prefix, None).unwrap();
        transcript.compress("summary".into(), &retained).unwrap();
        let projected = transcript.effective_context().unwrap().projected_entries();
        assert_eq!(projected.len(), 1);
        assert!(
            serde_json::to_string(&projected)
                .unwrap()
                .contains("retained edited")
        );
        transcript.reset_context(Some("handoff".into())).unwrap();
        transcript
            .append_context_edit(&retained, replacement("outside reset"))
            .unwrap();
        assert!(
            transcript
                .effective_context()
                .unwrap()
                .projected_entries()
                .is_empty()
        );
        transcript.select_branch(Some(&retained)).unwrap();
        assert!(
            serde_json::to_string(&transcript.effective_context().unwrap().projected_entries())
                .unwrap()
                .contains("retained original")
        );
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config { failure_persistence: None, ..Default::default() })]
        #[test]
        fn latest_branch_edit_controls_only_projected_content(
            original in ".{1,128}",
            replacements in proptest::collection::vec(proptest::option::of(".{0,128}"), 1..16),
        ) {
            let mut transcript = Transcript::new();
            transcript.append_user_message(&original);
            let source = transcript.entries[0].clone();
            let mut leaves = Vec::new();
            for text in &replacements {
                let previous = transcript.leaf_id.clone();
                transcript.append_context_edit(&source.id, text.as_deref().and_then(replacement)).unwrap();
                proptest::prop_assert_eq!(&transcript.entries.last().unwrap().parent_id, &previous);
                leaves.push(transcript.leaf_id.clone().unwrap());
            }
            let mut restored: Transcript = serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
            for (leaf, text) in leaves.iter().zip(&replacements) {
                restored.select_branch(Some(leaf)).unwrap();
                let projection = restored.effective_context().unwrap().projected_entries();
                proptest::prop_assert_eq!(projection.len(), usize::from(text.is_some()));
                if let Some(text) = text {
                    let TranscriptEntryKind::Message { message: AgentMessage::User { content } } = &projection[0].kind else { unreachable!() };
                    let matches = matches!(&content[..], [UserContentBlock::Text { text: projected }] if projected.as_str() == text);
                    proptest::prop_assert!(matches);
                }
                proptest::prop_assert_eq!(serde_json::to_value(&restored.entries[0]).unwrap(), serde_json::to_value(&source).unwrap());
            }
        }
    }

    #[test]
    fn invalid_context_edits_never_append_or_move_the_leaf() {
        assert!(
            serde_json::from_value::<TranscriptEntryKind>(
                serde_json::json!({"kind":"context_edit", "target_id":"source"})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<crate::core::lifecycle::ContextEditDraft>(
                serde_json::json!({"target_id":"source"})
            )
            .is_err()
        );
        let mut transcript = Transcript::new();
        transcript.append_user_message("left");
        let foreign = transcript.leaf_id.clone().unwrap();
        transcript.select_branch(None).unwrap();
        transcript.append_system_instruction("system");
        let system = transcript.leaf_id.clone().unwrap();
        transcript.reset_context(None).unwrap();
        let marker = transcript.leaf_id.clone().unwrap();
        transcript.append_user_message("right");
        let user = transcript.leaf_id.clone().unwrap();
        let before = serde_json::to_value(&transcript).unwrap();
        for target in [
            "missing",
            foreign.as_str(),
            system.as_str(),
            marker.as_str(),
        ] {
            assert!(transcript.append_context_edit(target, None).is_err());
            assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
        }
        let invalid = Some(ContextReplacement {
            content: ContextContent::Blocks(vec![ContentBlock::Thinking {
                text: "not user content".into(),
                metadata: None,
            }]),
        });
        assert!(transcript.append_context_edit(&user, invalid).is_err());
        assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
    }

    fn assistant(transcript: &mut Transcript, content: Vec<ContentBlock>) {
        transcript.append_assistant_message(
            None,
            None,
            content,
            "test".into(),
            crate::provider::ProviderApi::OpenAiCompletions,
            "test".into(),
            Usage::default(),
            "stop".into(),
        );
    }

    #[test]
    fn reset_preserves_history_identity_and_accounting() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("old");
        transcript.total_tokens = 17;
        transcript.total_cost = 0.5;
        transcript.total_cache_read = 3;
        transcript.total_cache_write = 2;
        let before = serde_json::to_value(&transcript).unwrap();
        let reset = transcript.reset_context(Some("handoff".into())).unwrap();
        let entry = transcript.entries.last().unwrap();
        assert_eq!(entry.id, reset);
        assert_eq!(entry.head.as_ref(), Some(&reset));
        assert_eq!(
            entry.parent_id,
            transcript.entries.first().map(|entry| entry.id.clone())
        );
        let after = serde_json::to_value(&transcript).unwrap();
        for field in [
            "id",
            "created_at",
            "total_tokens",
            "total_cost",
            "total_cache_read",
            "total_cache_write",
        ] {
            assert_eq!(after[field], before[field], "changed {field}");
        }
        assert_eq!(after["entries"][0], before["entries"][0]);
        let context = transcript.effective_context().unwrap();
        assert_eq!(context.preamble_text(), Some("handoff"));
        assert!(context.entries.is_empty());
    }

    #[test]
    fn repeated_compaction_and_branch_selection_keep_canonical_history() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("old");
        let root = transcript.leaf_id.clone().unwrap();
        transcript.append_user_message("retained");
        let retained = transcript.leaf_id.clone().unwrap();
        transcript
            .compress("first summary".into(), &retained)
            .unwrap();
        transcript.append_agent_message("delegated result");
        let feedback = transcript.leaf_id.clone().unwrap();
        transcript
            .compress("updated summary".into(), &feedback)
            .unwrap();
        let count = transcript.entries.len();
        let context = transcript.effective_context().unwrap();
        assert_eq!(context.preamble_text(), Some("updated summary"));
        assert_eq!(context.entries.len(), 1);
        assert_eq!(context.entries[0].id, feedback);
        transcript.select_branch(Some(&root)).unwrap();
        assert!(transcript.effective_context().unwrap().marker.is_none());
        assert_eq!(transcript.effective_context().unwrap().entries.len(), 1);
        assert_eq!(transcript.entries.len(), count);
    }

    #[test]
    fn generated_boundaries_round_trip_and_restore_each_branch_context() {
        #[derive(Clone, Copy)]
        enum Action {
            Append,
            Reset,
            Handoff,
            Compact,
        }
        const ACTIONS: [Action; 4] = [
            Action::Append,
            Action::Reset,
            Action::Handoff,
            Action::Compact,
        ];
        const SEQUENCE_LENGTH: u32 = 4;
        for sequence in 0..ACTIONS.len().pow(SEQUENCE_LENGTH) {
            let mut choices = sequence;
            let mut transcript = Transcript::new();
            let mut expected_entries: Vec<compact_str::CompactString> = Vec::new();
            let mut expected_preamble: Option<String> = None;
            let mut expected_marker = None;
            let mut snapshots = Vec::new();
            for step in 0..SEQUENCE_LENGTH {
                let before = serde_json::to_value(&transcript.entries).unwrap();
                let action = ACTIONS[choices % ACTIONS.len()];
                choices /= ACTIONS.len();
                match action {
                    Action::Append => {
                        transcript.append_user_message(&format!("message-{step}"));
                        expected_entries.push(transcript.leaf_id.clone().unwrap());
                    }
                    Action::Reset | Action::Handoff => {
                        expected_preamble = match action {
                            Action::Handoff => Some(format!("handoff-{step}")),
                            _ => None,
                        };
                        expected_marker =
                            Some(transcript.reset_context(expected_preamble.clone()).unwrap());
                        expected_entries.clear();
                    }
                    Action::Compact if expected_entries.len() > 1 => {
                        let cut = expected_entries.len() / 2;
                        expected_preamble = Some(format!("summary-{step}"));
                        transcript
                            .compress(expected_preamble.clone().unwrap(), &expected_entries[cut])
                            .unwrap();
                        expected_marker = transcript.leaf_id.clone();
                        expected_entries = expected_entries.split_off(cut);
                    }
                    Action::Compact => continue,
                }
                let after = serde_json::to_value(&transcript.entries).unwrap();
                for (index, entry) in before.as_array().unwrap().iter().enumerate() {
                    assert_eq!(&after[index], entry);
                }
                snapshots.push((
                    transcript.leaf_id.clone().unwrap(),
                    expected_entries.clone(),
                    expected_preamble.clone(),
                    expected_marker.clone(),
                ));
                let encoded = serde_json::to_value(&transcript).unwrap();
                transcript = serde_json::from_value(encoded.clone()).unwrap();
                assert_eq!(serde_json::to_value(&transcript).unwrap(), encoded);
            }
            let history = serde_json::to_value(&transcript.entries).unwrap();
            for (leaf, expected_entries, preamble, marker) in snapshots {
                transcript.select_branch(Some(&leaf)).unwrap();
                let context = transcript.effective_context().unwrap();
                assert_eq!(
                    context
                        .entries
                        .iter()
                        .map(|entry| entry.id.clone())
                        .collect::<Vec<_>>(),
                    expected_entries
                );
                assert_eq!(context.preamble_text(), preamble.as_deref());
                assert_eq!(context.marker.map(|entry| entry.id.clone()), marker);
                assert_eq!(serde_json::to_value(&transcript.entries).unwrap(), history);
            }
        }
    }

    #[test]
    fn reset_cuts_cannot_be_reversed_by_a_later_head() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("private old context");
        let old = transcript.leaf_id.clone().unwrap();
        transcript.reset_context(None).unwrap();
        transcript.append_user_message("new prefix");
        transcript.append_user_message("retained");
        let before = serde_json::to_value(&transcript).unwrap();
        assert!(transcript.compress("summary".into(), &old).is_err());
        assert_eq!(serde_json::to_value(&transcript).unwrap(), before);
        let mut corrupt = transcript.clone();
        corrupt.append_entry_with_head(
            Transcript::new_entry_id(),
            Some(old),
            TranscriptEntryKind::Compaction {
                summary: "corrupt summary".into(),
                tokens_before: 0,
            },
        );
        assert!(corrupt.effective_context().is_err());
        assert!(crate::context::convert_history(&corrupt).is_err());
        let reset_index = 1;
        for head in [None, Some("missing".into())] {
            let mut corrupt = transcript.clone();
            corrupt.entries[reset_index].head = head;
            assert!(corrupt.effective_context().is_err());
        }
        transcript.entries[0].head = Some(transcript.entries[0].id.clone());
        assert!(transcript.effective_context().is_err());
    }

    #[test]
    fn boundaries_keep_tool_batch_and_user_context_together() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("old");
        transcript.append_user_message("request");
        let request = transcript.leaf_id.clone().unwrap();
        transcript.append_context_message(
            ContextSource::FileMention,
            ContextScope::Turn,
            "context",
        );
        let context_id = transcript.leaf_id.clone().unwrap();
        assistant(
            &mut transcript,
            vec![
                ContentBlock::ToolCall {
                    id: "call".into(),
                    provider_call_id: None,
                    provider_item_id: None,
                    name: "read".into(),
                    args: serde_json::json!({"path":"src"}),
                },
                ContentBlock::ToolCall {
                    id: "second-call".into(),
                    provider_call_id: None,
                    provider_item_id: None,
                    name: "read".into(),
                    args: serde_json::json!({"path":"tests"}),
                },
            ],
        );
        let call = transcript.leaf_id.clone().unwrap();
        transcript.append_tool_result("call", "result", false, None);
        transcript.append_tool_result("second-call", "second result", false, None);
        let result = transcript.leaf_id.clone().unwrap();
        let context = transcript.effective_context().unwrap();
        assert_eq!(context.compaction_boundary(1), 3);
        assert!(transcript.compress("summary".into(), &result).is_err());
        assert!(transcript.compress("summary".into(), &context_id).is_err());
        transcript.compress("summary".into(), &request).unwrap();
        transcript.compress("summary".into(), &call).unwrap();
        assert_eq!(transcript.effective_context().unwrap().entries.len(), 3);
    }

    #[test]
    fn invalid_persisted_head_fails_closed_without_changing_history() {
        let mut transcript = Transcript::new();
        transcript.append_user_message("request");
        assistant(
            &mut transcript,
            vec![ContentBlock::ToolCall {
                id: "call".into(),
                provider_call_id: None,
                provider_item_id: None,
                name: "read".into(),
                args: serde_json::json!({}),
            }],
        );
        transcript.append_tool_result("call", "result", false, None);
        let result = transcript.leaf_id.clone().unwrap();
        for invalid in [result.as_str(), "missing"] {
            let mut candidate = transcript.clone();
            candidate.append_entry_with_head(
                Transcript::new_entry_id(),
                Some(invalid.into()),
                TranscriptEntryKind::Compaction {
                    summary: "invalid summary".into(),
                    tokens_before: 0,
                },
            );
            let before = serde_json::to_value(&candidate).unwrap();
            assert!(candidate.effective_context().is_err());
            assert!(candidate.reset_context(None).is_err());
            assert_eq!(serde_json::to_value(candidate).unwrap(), before);
        }
        transcript.append_tool_result("unknown-call", "orphan result", true, None);
        assert_eq!(
            transcript
                .effective_context()
                .unwrap()
                .compaction_boundary(1),
            0
        );
    }

    #[test]
    fn estimates_are_derived_from_all_effective_content_after_restore_and_navigation() {
        let mut transcript = Transcript::new();
        transcript.append_user_content(vec![UserContentBlock::Image {
            source: ImageSource::Url {
                url: "https://example.com/image.png".into(),
                mime_type: None,
            },
        }]);
        transcript.append_agent_message("feedback");
        transcript.append_context_message(
            ContextSource::FileMention,
            ContextScope::Turn,
            "context",
        );
        assistant(
            &mut transcript,
            vec![ContentBlock::Thinking {
                text: "thinking".into(),
                metadata: None,
            }],
        );
        let tokens = transcript.estimated_context_tokens();
        assert_eq!(tokens, ESTIMATED_IMAGE_TOKENS + 5);
        let mut restored: Transcript =
            serde_json::from_value(serde_json::to_value(&transcript).unwrap()).unwrap();
        restored
            .select_branch(transcript.leaf_id.as_deref())
            .unwrap();
        assert_eq!(tokens, restored.estimated_context_tokens());
        let before = serde_json::to_value(&restored).unwrap();
        assert!(before.get("messages").is_none());
        assert!(before.get("total_estimated_tokens").is_none());
        assert!(
            restored
                .compress("".into(), transcript.leaf_id.as_deref().unwrap())
                .is_err()
        );
        assert_eq!(serde_json::to_value(restored).unwrap(), before);
    }
}
