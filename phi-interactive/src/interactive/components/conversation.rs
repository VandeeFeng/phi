//! Application-owned display projection; no terminal mounts, editor or execution authority.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hasher};
use std::io::Write;
use std::time::{Duration, Instant};

use anyhow::Context;
use phi_sdk::agent_instance::LiveRunSnapshot;

use phi_sdk::tools::{ToolDisplay, ToolDisplayFormatter};
use phi_sdk::transcript::{ImageSource, TranscriptEntry, UserContentBlock};
use phi_sdk::{PhiEvent, PhiFailure, PhiFailureKind, PhiUsage};
use phi_tui::{
    BlockBody, BlockRole, BlockStatus, ImageBlock, NotifyKind, NotifyMessage, TerminalImage,
    TranscriptTail,
};

use super::format::text_from_blocks;
use super::status::RunStatusView;
use crate::session::{AgentMessage, ContentBlock, Session, TranscriptEntryKind};

pub(in crate::interactive) type BlockId = u64;

#[derive(Debug, Clone)]
pub(in crate::interactive) struct UiBlock {
    pub(in crate::interactive) id: BlockId,
    pub(in crate::interactive) group_id: BlockId,
    pub(in crate::interactive) parent_id: Option<BlockId>,
    pub(in crate::interactive) agent_container: bool,
    pub(in crate::interactive) local_notification: bool,
    pub(in crate::interactive) tool_call_id: Option<String>,
    pub(in crate::interactive) role: BlockRole,
    pub(in crate::interactive) block: phi_tui::TrackedBlock,
}

pub(in crate::interactive) const TOOL_TIMER_INTERVAL: Duration = Duration::from_secs(1);
const STATUS_FRAME_INTERVAL: Duration = Duration::from_millis(80);

#[derive(Debug, Clone, Copy)]
pub(in crate::interactive) struct ToolTimer {
    pub(in crate::interactive) started_at: Instant,
    pub(in crate::interactive) finished_at: Option<Instant>,
    pub(in crate::interactive) last_observed_at: Instant,
}

impl ToolTimer {
    pub(in crate::interactive) fn start(now: Instant) -> Self {
        Self {
            started_at: now,
            finished_at: None,
            last_observed_at: now,
        }
    }

    pub(in crate::interactive) fn label(self, now: Instant) -> String {
        let elapsed = self
            .finished_at
            .unwrap_or(now)
            .saturating_duration_since(self.started_at);
        let prefix = if self.finished_at.is_some() {
            "Took"
        } else {
            "Elapsed"
        };
        format!("{prefix} {:.1}s", elapsed.as_secs_f64())
    }
}

#[derive(Debug)]
pub(in crate::interactive) struct ToolBlockMeta {
    pub(in crate::interactive) name: String,
    pub(in crate::interactive) args: String,
    pub(in crate::interactive) display: ToolDisplay,
    pub(in crate::interactive) tool_timer: Option<ToolTimer>,
}

// Routes display deltas only; AgentInstance/Transcript own execution and history.
#[derive(Debug, Clone, Default)]
pub(in crate::interactive) struct LiveRunView {
    pub(in crate::interactive) assistant_block: Option<BlockId>,
    pub(in crate::interactive) assistant_group: Option<BlockId>,
    pub(in crate::interactive) reasoning_block: Option<BlockId>,
    pub(in crate::interactive) tool_blocks: HashMap<String, BlockId>,
    pub(in crate::interactive) active_tool_block: Option<BlockId>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(in crate::interactive) struct UsageView {
    pub(in crate::interactive) input: u64,
    pub(in crate::interactive) output: u64,
    pub(in crate::interactive) cache_read: u64,
    pub(in crate::interactive) cost: f64,
}

impl UsageView {
    fn from_session(session: &Session) -> Self {
        let mut usage = Self::default();
        for entry in &session.entries {
            let TranscriptEntryKind::Message {
                message: AgentMessage::Assistant { usage: value, .. },
            } = &entry.kind
            else {
                continue;
            };
            usage.add(value.input, value.output, value.cache_read, value.cost);
        }
        usage
    }

    fn add(&mut self, input: u64, output: u64, cache_read: u64, cost: f64) {
        self.input = self.input.saturating_add(input);
        self.output = self.output.saturating_add(output);
        self.cache_read = self.cache_read.saturating_add(cache_read);
        self.cost += cost;
    }
}

#[derive(Clone, Debug)]
pub(in crate::interactive) struct ConversationSource(std::sync::Arc<()>);

impl PartialEq for ConversationSource {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ConversationSource {}

pub(in crate::interactive) struct ConversationView {
    source: ConversationSource,
    pub(in crate::interactive) blocks: Vec<UiBlock>,
    pub(in crate::interactive) retired_history_blocks: usize,
    pub(in crate::interactive) block_positions: HashMap<BlockId, usize>,
    pub(in crate::interactive) transcript_tail: TranscriptTail,
    pub(in crate::interactive) projection_revision: u64,
    pub(in crate::interactive) painted_history_revision: u64,
    pub(in crate::interactive) buffer_first_block: Option<BlockId>,
    pub(in crate::interactive) buffer_initialized: bool,
    pub(in crate::interactive) blocks_expanded: Option<bool>,
    pub(in crate::interactive) usage: UsageView,
    run_active: bool,
    // Image segmentation clears the live text block without clearing text observation.
    assistant_visible: bool,
    status_frame: usize,
    last_status_frame: Instant,
    pub(in crate::interactive) focused_block: Option<BlockId>,
    pub(in crate::interactive) live_run: Option<LiveRunView>,
    replay_tool_calls: HashMap<String, BlockId>,
    child_history: Option<ChildHistory>,
    pub(in crate::interactive) tool_block_meta: HashMap<BlockId, ToolBlockMeta>,
    pub(in crate::interactive) tool_display_name_projection: HashMap<String, String>,
    pub(in crate::interactive) next_block_id: BlockId,
}

impl Default for ConversationView {
    fn default() -> Self {
        Self {
            source: ConversationSource(std::sync::Arc::new(())),
            blocks: Vec::new(),
            retired_history_blocks: 0,
            block_positions: HashMap::new(),
            transcript_tail: TranscriptTail::default(),
            projection_revision: 0,
            painted_history_revision: 0,
            buffer_first_block: None,
            buffer_initialized: false,
            blocks_expanded: None,
            usage: UsageView::default(),
            run_active: false,
            assistant_visible: false,
            status_frame: 0,
            last_status_frame: Instant::now(),
            focused_block: None,
            live_run: None,
            replay_tool_calls: HashMap::new(),
            child_history: None,
            tool_block_meta: HashMap::new(),
            tool_display_name_projection: HashMap::new(),
            next_block_id: 1,
        }
    }
}

impl ConversationView {
    fn project_assistant_delta(&mut self, text: &str) {
        self.assistant_visible = true;
        self.append_live_assistant(text);
    }

    pub(in crate::interactive) fn reset_live_assistant_display(&mut self) {
        self.reset_live_assistant();
        self.assistant_visible = false;
    }

    pub(in crate::interactive) fn project_event(&mut self, event: PhiEvent) -> bool {
        match event {
            PhiEvent::NestedToolCall { call } => return self.project_nested_tool_call(&call),
            PhiEvent::SubmissionDiscarded { .. } | PhiEvent::UserInputInjected { .. } => {}
            PhiEvent::System { message } => {
                self.push_text_block(BlockRole::Event, message);
            }
            PhiEvent::ProviderRecovery { recovery } => {
                let action = match recovery.reason {
                    phi_sdk::RecoveryReason::ProviderRetry => "provider retry",
                    phi_sdk::RecoveryReason::ContextOverflow => "context overflow recovery",
                };
                self.push_text_block(
                    BlockRole::Event,
                    format!(
                        "{action}: {}/{} (attempt {}, delay {}ms)",
                        recovery.provider, recovery.model, recovery.attempt, recovery.delay_ms,
                    ),
                );
            }
            PhiEvent::TurnStarted { .. } => self.project_main_run_started(),
            PhiEvent::AssistantStarted { .. } => {
                if self.assistant_visible {
                    self.reset_live_assistant_display();
                }
            }
            PhiEvent::AssistantDelta { display, .. } => self.project_assistant_delta(&display),
            PhiEvent::AssistantImage { source } => {
                self.push_live_assistant_image(image_block(&source));
            }
            PhiEvent::ReasoningDelta { text } => {
                let visible = self.live_reasoning_is_visible();
                self.set_live_reasoning(&text);
                return visible;
            }
            PhiEvent::ReasoningFinished { text } => {
                if self.complete_live_reasoning().is_none() && !text.trim().is_empty() {
                    self.push_thinking_block(text);
                }
            }
            PhiEvent::ToolCallStarted {
                id,
                name,
                args,
                display,
                ..
            } => {
                if self.assistant_visible {
                    self.reset_live_assistant_display();
                }
                self.reset_live_reasoning();
                self.project_tool_call_started(
                    &id,
                    &name,
                    &args.to_string(),
                    display.unwrap_or_default(),
                );
            }
            PhiEvent::ToolCallFinished {
                id,
                output,
                is_error,
                details,
            } => {
                if !self.project_tool_call_finished(&id, &output, is_error, details.as_ref()) {
                    self.push_text_block(BlockRole::Event, output);
                }
            }
            PhiEvent::AssistantFinished {
                content,
                response,
                display,
                usage,
                ..
            } => {
                self.record_usage(&usage);
                let rendered = if display.is_empty() {
                    response
                } else {
                    display
                };
                let structured = content.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Image { .. } | ContentBlock::ToolCall { .. }
                    )
                });
                if !structured {
                    self.complete_assistant_text(&content, &rendered);
                }
                self.reset_live_assistant_display();
            }
            PhiEvent::TurnFinished { .. } => self.project_main_run_finished(),
            PhiEvent::TurnFailed { .. } => {
                if self.assistant_visible {
                    self.reset_live_assistant_display();
                }
                if let Some(block_id) = self.live_active_tool_block() {
                    self.set_tool_failed(block_id);
                }
                self.reset_live_assistant();
                self.project_main_run_finished();
            }
        }
        true
    }

    fn complete_assistant_text(&mut self, content: &[ContentBlock], rendered: &str) {
        let shared = single_text_payload(content).filter(|text| text.as_str() == rendered);
        if let Some(block_id) = self.live_assistant_block() {
            if let Some(text) = shared {
                self.set_shared_text_block(block_id, text);
            } else {
                self.set_text_block(block_id, rendered);
            }
        } else if let Some(text) = shared {
            self.push_shared_text_block(BlockRole::Assistant, text);
        } else if !rendered.is_empty() {
            self.push_text_block(BlockRole::Assistant, rendered);
        }
    }

    pub(in crate::interactive) fn source(&self) -> &ConversationSource {
        &self.source
    }

    pub(in crate::interactive) fn tool_names(&self) -> Vec<String> {
        self.tool_block_meta
            .values()
            .map(|meta| meta.name.clone())
            .collect()
    }

    pub(in crate::interactive) fn install_tool_titles(
        &mut self,
        names: HashMap<String, String>,
    ) -> bool {
        self.tool_display_name_projection = names;
        self.refresh_mcp_tool_titles()
    }

    pub(in crate::interactive) fn with_resources(
        session: &Session,
        tool_names: HashMap<String, String>,
    ) -> Self {
        Self {
            usage: UsageView::from_session(session),
            tool_display_name_projection: tool_names,
            ..Self::default()
        }
    }

    pub(in crate::interactive) fn acknowledge_history_render(
        &mut self,
        revision: u64,
        first_block: Option<BlockId>,
        retire_before: Option<BlockId>,
    ) {
        self.painted_history_revision = revision;
        self.buffer_first_block = first_block;
        self.buffer_initialized = true;
        if let Some(first_block) = retire_before {
            self.retire_transcript_before(first_block);
        }
    }

    pub(in crate::interactive) fn materialize(
        &mut self,
        request: phi_tui::TranscriptRenderRequest,
    ) -> phi_tui::MaterializedTranscript {
        let blocks = &self.blocks;
        let positions = &self.block_positions;
        self.transcript_tail.materialize(
            blocks
                .iter()
                .enumerate()
                .filter(|(_, block)| {
                    block.parent_id.is_none_or(|parent| {
                        positions.get(&parent).is_some_and(|position| {
                            !blocks[*position].agent_container
                                || blocks[*position].block.is_expanded()
                        })
                    })
                })
                .map(|(index, block)| {
                    block.block.view(
                        block.id,
                        block.role,
                        index == 0 || blocks[index - 1].group_id != block.group_id,
                    )
                }),
            request,
        )
    }

    pub(in crate::interactive) fn restore_live_run(
        &mut self,
        snapshot: &phi_sdk::agent_instance::AgentInstanceSnapshot,
    ) {
        if snapshot.active_turn_id.is_some() {
            self.restore_live_content(snapshot.live_run.as_ref());
        }
    }

    pub(in crate::interactive) fn restore_live_content(
        &mut self,
        live: Option<&phi_sdk::agent_instance::LiveRunSnapshot>,
    ) {
        self.project_main_run_started();
        if let Some(run) = live {
            if !run.reasoning_text.is_empty() {
                self.set_live_reasoning(&run.reasoning_text);
            }
            if !run.assistant_text.is_empty() {
                self.project_assistant_delta(&run.assistant_text);
            }
            if let Some(tool) = &run.tool {
                self.restore_live_tool(tool);
            }
        }
        self.replay_tool_calls.clear();
    }

    fn restore_live_tool(&mut self, tool: &phi_sdk::agent_instance::LiveToolSnapshot) {
        let existing = self
            .child_history
            .as_ref()
            .and_then(|history| history.tools.get(&tool.id))
            .copied()
            .or_else(|| self.replay_tool_calls.remove(&tool.id));
        if let Some(block_id) = existing {
            if tool.name.eq_ignore_ascii_case("bash") {
                self.start_tool_timing(block_id);
            }
            if let Some(view) = self.live_run.as_mut() {
                view.tool_blocks.insert(tool.id.clone(), block_id);
                view.active_tool_block = Some(block_id);
            }
        } else {
            self.project_tool_call_started(
                &tool.id,
                &tool.name,
                &tool.args.to_string(),
                tool.display.clone().unwrap_or_default(),
            );
        }
    }

    pub(in crate::interactive) fn child_from_session(
        session: &Session,
        expanded: Option<bool>,
    ) -> Self {
        let mut view = Self {
            blocks_expanded: expanded,
            ..Self::default()
        };
        view.rebuild_history(session);
        view
    }

    pub(in crate::interactive) fn run_status_view(&self) -> RunStatusView {
        RunStatusView {
            active: self.run_active_projection(),
            frame: self.status_frame,
        }
    }

    pub(in crate::interactive) fn next_clock_deadline(&self) -> Option<Instant> {
        let spinner = self
            .run_active
            .then_some(self.last_status_frame + STATUS_FRAME_INTERVAL);
        self.tool_block_meta
            .values()
            .filter_map(|meta| meta.tool_timer)
            .filter(|timer| timer.finished_at.is_none())
            .map(|timer| timer.last_observed_at + TOOL_TIMER_INTERVAL)
            .chain(spinner)
            .min()
    }

    pub(in crate::interactive) fn advance_clocks(&mut self, now: Instant) -> bool {
        let mut changed = self.refresh_tool_timing(now);
        if self.run_active
            && now.saturating_duration_since(self.last_status_frame) >= STATUS_FRAME_INTERVAL
        {
            self.status_frame = self.status_frame.wrapping_add(1);
            self.last_status_frame = now;
            changed = true;
        }
        changed
    }

    pub(in crate::interactive) fn project_main_run_started(&mut self) {
        self.start_live_run();
        self.run_active = true;
    }

    pub(in crate::interactive) fn project_main_run_finished(&mut self) {
        self.clear_live_run();
        self.run_active = false;
    }

    pub(in crate::interactive) fn run_active_projection(&self) -> bool {
        self.run_active
    }

    pub(in crate::interactive) fn reconcile_tool_results(
        &mut self,
        transcript: &phi_sdk::transcript::Transcript,
    ) {
        for entry in transcript.current_branch() {
            if let TranscriptEntryKind::Message {
                message:
                    AgentMessage::ToolResult {
                        tool_call_id,
                        content,
                        is_error,
                        details,
                        ..
                    },
            } = &entry.kind
                && self
                    .live_run
                    .as_ref()
                    .is_some_and(|run| run.tool_blocks.contains_key(tool_call_id.as_str()))
            {
                let output = text_from_blocks(content).unwrap_or_default();
                self.project_tool_call_finished(tool_call_id, &output, *is_error, details.as_ref());
            }
        }
    }

    pub(in crate::interactive) fn project_run_cancelled(&mut self) {
        self.assistant_visible = false;
        self.complete_live_reasoning();
        let unfinished_tools = self
            .live_run
            .as_ref()
            .map(|run| run.tool_blocks.values().copied().collect::<Vec<_>>())
            .unwrap_or_default();
        for block_id in unfinished_tools {
            if self
                .block_positions
                .get(&block_id)
                .and_then(|position| self.blocks.get(*position))
                .is_some_and(|block| block.block.status == BlockStatus::Running)
            {
                self.set_tool_failed(block_id);
            }
        }
        self.project_main_run_finished();
    }

    pub(in crate::interactive) fn record_usage(&mut self, usage: &PhiUsage) {
        self.usage.add(
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read,
            usage.cost,
        );
    }

    pub(in crate::interactive) fn recover_history(&mut self, session: &Session) {
        let retired_history_blocks = self.retired_history_blocks;
        let notices = self
            .blocks
            .drain(..)
            .enumerate()
            .filter(|(_, block)| block.local_notification)
            .collect::<Vec<_>>();
        self.rebuild_history(session);
        for (position, block) in notices {
            self.blocks.insert(
                position
                    .saturating_add(retired_history_blocks)
                    .min(self.blocks.len()),
                block,
            );
        }
        self.reindex_blocks();
    }

    pub(in crate::interactive) fn rebuild_history(&mut self, session: &Session) {
        self.clear_history_projection();
        self.usage = UsageView::from_session(session);
        self.project_history(session, &super::format::tool_display_formatter);
    }

    pub(in crate::interactive) fn project_history(
        &mut self,
        session: &Session,
        formatter: &dyn ToolDisplayFormatter,
    ) {
        let mut calls = HashMap::new();
        for entry in session.current_branch() {
            match &entry.kind {
                TranscriptEntryKind::Message { message } => {
                    project_transcript_message(self, message, formatter, &mut calls);
                }
                entry => self.project_history_event(entry),
            }
        }
        self.replay_tool_calls = calls;
    }

    fn clear_history_projection(&mut self) {
        self.assistant_visible = false;
        self.project_main_run_finished();
        self.blocks.clear();
        self.retired_history_blocks = 0;
        self.block_positions.clear();
        self.tool_block_meta.clear();
        self.replay_tool_calls.clear();
        self.child_history = None;
        self.focused_block = None;
        self.buffer_first_block = None;
        self.buffer_initialized = false;
        self.projection_revision = self.projection_revision.saturating_add(1);
    }
}

fn single_text_payload(content: &[ContentBlock]) -> Option<&phi_sdk::transcript::SharedText> {
    let [ContentBlock::Text { text }] = content else {
        return None;
    };
    Some(text)
}

#[derive(PartialEq, Eq)]
struct EntryStamp {
    id: String,
    fingerprint: u64,
    compaction: bool,
}

#[derive(Default)]
struct Fingerprint(DefaultHasher);

impl Write for Fingerprint {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl EntryStamp {
    fn capture(entry: &TranscriptEntry) -> anyhow::Result<Self> {
        let mut fingerprint = Fingerprint::default();
        serde_json::to_writer(&mut fingerprint, entry).context("cannot observe child history")?;
        Ok(Self {
            id: entry.id.to_string(),
            fingerprint: fingerprint.0.finish(),
            compaction: matches!(entry.kind, TranscriptEntryKind::Compaction { .. }),
        })
    }
}

struct ChildHistory {
    session: String,
    working_dir: String,
    entries: Vec<EntryStamp>,
    tools: HashMap<String, BlockId>,
}

impl ChildHistory {
    fn capture(session: &Session) -> anyhow::Result<Self> {
        let branch = session.current_branch();
        anyhow::ensure!(
            branch.last().map(|entry| &entry.id) == session.leaf_id.as_ref(),
            "child history leaf is unavailable"
        );
        anyhow::ensure!(
            branch
                .windows(2)
                .all(|entries| entries[1].parent_id.as_ref() == Some(&entries[0].id)),
            "child history is disconnected"
        );
        let mut ids = std::collections::HashSet::new();
        anyhow::ensure!(
            branch.iter().all(|entry| ids.insert(&entry.id)),
            "child history contains duplicate entries"
        );
        anyhow::ensure!(
            branch
                .first()
                .and_then(|entry| entry.parent_id.as_ref())
                .is_none_or(|parent| !ids.contains(parent)),
            "child history contains a cycle"
        );
        Ok(Self {
            session: session.id.to_string(),
            working_dir: session.working_dir.to_string(),
            entries: branch
                .into_iter()
                .map(EntryStamp::capture)
                .collect::<anyhow::Result<_>>()?,
            tools: HashMap::new(),
        })
    }

    fn extends(&self, previous: &Self) -> bool {
        self.session == previous.session
            && self.working_dir == previous.working_dir
            && self.entries.starts_with(&previous.entries)
            && !self.entries[previous.entries.len()..]
                .iter()
                .any(|entry| entry.compaction)
    }
}

impl ConversationView {
    pub(in crate::interactive) fn observe_child(
        &mut self,
        session: &Session,
        live: Option<&LiveRunSnapshot>,
        working: bool,
    ) -> anyhow::Result<()> {
        let mut observed = ChildHistory::capture(session)?;
        if self
            .child_history
            .as_ref()
            .is_none_or(|previous| !observed.extends(previous))
        {
            self.rebuild_child(session, observed, live, working);
            return Ok(());
        }
        let mut previous = self.child_history.take().expect("validated child history");
        for entry in session
            .current_branch()
            .into_iter()
            .skip(previous.entries.len())
        {
            match &entry.kind {
                TranscriptEntryKind::Message { message } => {
                    self.commit_child_message(message, &mut previous.tools);
                }
                entry => self.project_history_event(entry),
            }
        }
        observed.tools = previous.tools;
        self.usage = UsageView::from_session(session);
        self.child_history = Some(observed);
        self.refresh_child_live(live, working);
        Ok(())
    }

    fn project_history_event(&mut self, entry: &TranscriptEntryKind) {
        if let TranscriptEntryKind::Failure { failure, .. } = entry {
            self.project_failure(failure_notice(failure));
        }
        if let TranscriptEntryKind::Event { name, data } = entry
            && name == "nested_tool_call"
            && let Ok(call) = serde_json::from_value(data.clone())
        {
            self.project_nested_tool_call(&call);
        }
    }

    fn rebuild_child(
        &mut self,
        session: &Session,
        mut observed: ChildHistory,
        live: Option<&LiveRunSnapshot>,
        working: bool,
    ) {
        let mut replacement = Self::child_from_session(session, self.blocks_expanded);
        observed.tools = replacement.replay_tool_calls.clone();
        if working {
            replacement.restore_live_content(live);
        }
        replacement.child_history = Some(observed);
        *self = replacement;
    }

    fn commit_child_message(
        &mut self,
        message: &AgentMessage,
        calls: &mut HashMap<String, BlockId>,
    ) {
        if let AgentMessage::Assistant { content, .. } = message {
            self.commit_child_content(content, calls);
            return;
        }
        project_transcript_message(self, message, &super::format::tool_display_formatter, calls);
        if let AgentMessage::ToolResult { tool_call_id, .. } = message
            && let Some(run) = &mut self.live_run
        {
            let completed = run.tool_blocks.remove(tool_call_id.as_str());
            if run.active_tool_block == completed {
                run.active_tool_block = None;
            }
        }
    }

    fn commit_child_content(
        &mut self,
        content: &[ContentBlock],
        calls: &mut HashMap<String, BlockId>,
    ) {
        let mut group = None;
        let mut committed = Vec::new();
        for part in content {
            let grouped = matches!(part, ContentBlock::Text { .. } | ContentBlock::Image { .. });
            if !grouped {
                group = None;
            }
            if let Some(id) = self.commit_child_part(part, calls) {
                if grouped {
                    join_display_group(self, id, &mut group);
                }
                committed.push(id);
            }
        }
        self.order_child_commit(&committed);
        self.reset_live_assistant();
        self.complete_live_reasoning();
        self.reset_live_reasoning();
    }

    fn commit_child_part(
        &mut self,
        part: &ContentBlock,
        calls: &mut HashMap<String, BlockId>,
    ) -> Option<BlockId> {
        match part {
            ContentBlock::Text { text } => Some(self.commit_child_text(text)),
            ContentBlock::Image { source } => {
                Some(self.push_image_block(BlockRole::Assistant, image_block(source)))
            }
            ContentBlock::Thinking { text, .. } if !text.trim().is_empty() => {
                Some(self.commit_child_reasoning(text))
            }
            ContentBlock::Thinking { .. } => None,
            ContentBlock::ToolCall { id, name, args, .. } => {
                Some(self.commit_child_tool(id, name, args, calls))
            }
        }
    }

    fn commit_child_text(&mut self, text: &phi_sdk::transcript::SharedText) -> BlockId {
        if let Some(id) = self
            .live_run
            .as_mut()
            .and_then(|run| run.assistant_block.take())
        {
            self.set_shared_text_block(id, text);
            id
        } else {
            self.push_shared_text_block(BlockRole::Assistant, text)
        }
    }

    fn commit_child_reasoning(&mut self, text: &phi_sdk::transcript::SharedText) -> BlockId {
        let id = if self
            .live_run
            .as_ref()
            .is_some_and(|run| run.reasoning_block.is_some())
        {
            let id = self
                .set_live_reasoning(text)
                .expect("existing reasoning route");
            self.complete_live_reasoning();
            id
        } else {
            self.push_shared_thinking_block(text)
        };
        self.reset_live_reasoning();
        id
    }

    fn commit_child_tool(
        &mut self,
        id: &str,
        name: &str,
        args: &serde_json::Value,
        calls: &mut HashMap<String, BlockId>,
    ) -> BlockId {
        let existing = self
            .live_run
            .as_ref()
            .and_then(|run| run.tool_blocks.get(id))
            .copied()
            .or_else(|| calls.get(id).copied());
        let block = existing.unwrap_or_else(|| {
            self.project_history_tool(id, name, args, &super::format::tool_display_formatter)
        });
        calls.insert(id.to_string(), block);
        block
    }

    fn project_history_tool(
        &mut self,
        id: &str,
        name: &str,
        args: &serde_json::Value,
        formatter: &dyn ToolDisplayFormatter,
    ) -> BlockId {
        let block = self.project_tool_block(name, args.to_string(), Some(formatter));
        if let Some(position) = self.block_positions.get(&block) {
            self.blocks[*position].tool_call_id = Some(id.to_string());
        }
        block
    }

    fn order_child_commit(&mut self, ids: &[BlockId]) {
        let positions = ids
            .iter()
            .filter_map(|id| self.block_positions.get(id))
            .collect::<Vec<_>>();
        if positions.windows(2).all(|pair| pair[0] < pair[1]) {
            return;
        }
        let Some(first) = ids
            .iter()
            .filter_map(|id| self.block_positions.get(id))
            .min()
            .copied()
        else {
            return;
        };
        let mut committed = HashMap::new();
        let mut retained = Vec::with_capacity(self.blocks.len());
        for block in self.blocks.drain(..) {
            if ids.contains(&block.id) {
                committed.insert(block.id, block);
            } else {
                retained.push(block);
            }
        }
        self.blocks = retained;
        self.blocks.splice(
            first..first,
            ids.iter().filter_map(|id| committed.remove(id)),
        );
        self.reindex_blocks();
    }

    fn refresh_child_live(&mut self, live: Option<&LiveRunSnapshot>, working: bool) {
        if !working {
            self.complete_live_reasoning();
            self.project_main_run_finished();
            return;
        }
        if !self.run_active {
            self.project_main_run_started();
        }
        let Some(live) = live else { return };
        if !live.assistant_text.is_empty() {
            self.refresh_child_text(&live.assistant_text);
        }
        self.refresh_child_reasoning(&live.reasoning_text);
        if let Some(tool) = &live.tool {
            self.restore_live_tool(tool);
        }
    }

    fn refresh_child_reasoning(&mut self, text: &str) {
        let block = self
            .live_run
            .as_ref()
            .and_then(|run| run.reasoning_block)
            .and_then(|id| self.block_positions.get(&id))
            .map(|&position| &self.blocks[position]);
        if text.is_empty() {
            if block.is_some_and(|block| block.block.status == BlockStatus::Running) {
                self.complete_live_reasoning();
            }
        } else if !block.is_some_and(
            |block| matches!(&block.block.body, BlockBody::Thinking(current) if &**current == text),
        ) {
            self.set_live_reasoning(text);
        }
    }

    fn refresh_child_text(&mut self, text: &str) {
        let id = self.live_assistant_block();
        let current = id
            .and_then(|id| self.block_positions.get(&id))
            .and_then(|&position| match &self.blocks[position].block.body {
                BlockBody::Text(text) => Some(&**text),
                _ => None,
            })
            .unwrap_or("");
        if current == text {
            return;
        }
        if let Some(delta) = text.strip_prefix(current) {
            self.project_assistant_delta(delta);
        } else if let Some(id) = id {
            self.set_text_block(id, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_history_survives_recovery_and_new_run() {
        use phi_sdk::agent_instance::{
            AgentInstanceId, AgentInstanceSnapshot, AgentInstanceStatus,
        };

        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let failure = PhiFailure::new(
            PhiFailureKind::ProviderRejected,
            "provider failure details",
            None,
        );
        session
            .transcript
            .append_entry(TranscriptEntryKind::Failure {
                turn_id: Some("failed-turn".into()),
                failure: failure.clone(),
            });
        let mut view = ConversationView::default();
        view.notify(NotifyKind::Info, "retained notice");
        view.project_event(PhiEvent::TurnFailed { failure });
        assert_eq!(view.blocks.len(), 1);
        let mut snapshot = AgentInstanceSnapshot {
            id: AgentInstanceId::new(session.id.as_str()),
            definition: None,
            working_dir: Default::default(),
            status: AgentInstanceStatus::Failed,
            active_turn_id: None,
            live_run: None,
            progress: None,
            bash_executions: Vec::new(),
            pending_interactions: Vec::new(),
            latest_error: Some(PhiFailure::new(
                PhiFailureKind::Internal,
                "runtime-only error",
                None,
            )),
            revision: 1,
        };
        for _ in 0..2 {
            view.recover_history(&session);
            view.restore_live_run(&snapshot);
            assert_eq!(view.blocks.len(), 2);
            assert!(view.blocks[0].local_notification);
            let BlockBody::Notify(actual) = &view.blocks[1].block.body else {
                panic!("expected failure notification");
            };
            assert_eq!(actual.kind, NotifyKind::Error);
            assert_eq!(actual.message, "provider failure details");
        }
        snapshot.status = AgentInstanceStatus::Running;
        snapshot.active_turn_id = Some("next-turn".into());
        snapshot.latest_error = None;
        view.recover_history(&session);
        view.restore_live_run(&snapshot);
        assert!(view.run_active_projection());
        assert_eq!(view.blocks.len(), 2);

        view.rebuild_history(&session);
        assert_eq!(view.blocks.len(), 1);
        assert!(!view.blocks[0].local_notification);
        assert!(matches!(
            &view.blocks[0].block.body,
            BlockBody::Notify(actual) if actual.kind == NotifyKind::Error && actual.message == "provider failure details"
        ));
    }

    #[test]
    fn identical_failure_messages_follow_history_branch_identity() {
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let mut failures = Vec::new();
        for turn in ["first-turn", "second-turn"] {
            failures.push(
                session
                    .transcript
                    .append_entry(TranscriptEntryKind::Failure {
                        turn_id: Some(turn.into()),
                        failure: PhiFailure::new(
                            PhiFailureKind::ProviderRejected,
                            "same error",
                            None,
                        ),
                    }),
            );
        }
        let mut view = ConversationView::default();
        view.rebuild_history(&session);
        assert_eq!(view.blocks.len(), 2);
        session
            .transcript
            .select_branch(Some(&failures[0]))
            .unwrap();
        view.rebuild_history(&session);
        assert_eq!(view.blocks.len(), 1);
        session
            .transcript
            .select_branch(Some(&failures[1]))
            .unwrap();
        view.recover_history(&session);
        assert_eq!(view.blocks.len(), 2);
        view.project_event(PhiEvent::TurnStarted {
            submission_id: None,
            prompt: "retry".into(),
            source: "interactive".into(),
        });
        assert_eq!(view.blocks.len(), 2);
    }

    #[test]
    fn recovery_notice_does_not_finish_the_run() {
        let mut view = ConversationView::default();
        view.project_main_run_started();
        view.project_event(PhiEvent::ProviderRecovery {
            recovery: phi_sdk::ProviderRecovery {
                reason: phi_sdk::RecoveryReason::ProviderRetry,
                attempt: 1,
                delay_ms: 100,
                provider: "local".into(),
                model: "test".into(),
            },
        });
        assert!(view.run_active_projection());
        assert!(
            matches!(&view.blocks.last().unwrap().block.body, BlockBody::Text(text) if text.contains("provider retry: local/test"))
        );
    }

    fn append_child_response(session: &mut Session, content: Vec<ContentBlock>) {
        session.append_assistant_message(
            None,
            None,
            content,
            "local".into(),
            phi_sdk::providers::ProviderApi::OpenAiCompletions,
            "test".into(),
            phi_sdk::transcript::Usage {
                output: 7,
                ..Default::default()
            },
            "stop".into(),
        );
    }

    #[test]
    fn codemode_replay_and_child_updates_reconcile_nested_events_with_results() {
        use phi_sdk::tools::{NestedCallStatus, NestedCalls, NestedToolCall, ToolResultDetails};

        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        append_child_response(
            &mut session,
            vec![ContentBlock::ToolCall {
                id: "script".into(),
                name: "Codemode".into(),
                args: serde_json::json!({"code": "return await tools.Read({path: 'README.md'});"}),
                provider_call_id: None,
                provider_item_id: None,
            }],
        );
        let mut call = NestedToolCall {
            id: "script/1".into(),
            parent_tool_call_id: "script".into(),
            name: "Read".into(),
            status: NestedCallStatus::Running,
            arguments: None,
            duration_ms: 0,
            error: None,
            details: None,
        };
        let mut child = ConversationView::default();
        child.observe_child(&session, None, true).unwrap();
        session.append_entry(TranscriptEntryKind::Event {
            name: "nested_tool_call".into(),
            data: serde_json::to_value(&call).unwrap(),
        });
        let mut replay = ConversationView::default();
        replay.rebuild_history(&session);
        child.observe_child(&session, None, true).unwrap();
        let nested_id = child.blocks[1].id;
        for view in [&replay, &child] {
            assert_eq!(view.blocks.len(), 2);
            assert_eq!(view.blocks[1].block.status, BlockStatus::Running);
            assert_eq!(
                view.blocks[1].block.input.as_deref(),
                Some("[arguments unavailable]")
            );
        }
        call.status = NestedCallStatus::Cancelled;
        call.error = Some("nested cancellation diagnostic".into());
        call.duration_ms = 2000;
        session.append_tool_result(
            "script",
            "interrupted",
            true,
            Some(ToolResultDetails {
                nested_calls: Some(NestedCalls {
                    calls: vec![call],
                    complete: true,
                }),
                ..ToolResultDetails::default()
            }),
        );
        replay.rebuild_history(&session);
        child.observe_child(&session, None, false).unwrap();
        assert_eq!(child.blocks[1].id, nested_id);
        for view in [&replay, &child] {
            assert_eq!(view.blocks.len(), 2);
            assert_eq!(view.blocks[0].block.status, BlockStatus::Error);
            assert_eq!(view.blocks[1].parent_id, Some(view.blocks[0].id));
            assert_eq!(view.blocks[1].block.status, BlockStatus::Error);
            assert!(matches!(
                &view.blocks[1].block.body,
                BlockBody::Output(text) if text.is_empty()
            ));
            assert!(
                view.blocks[1]
                    .block
                    .footer
                    .as_ref()
                    .unwrap()
                    .text
                    .contains("Cancelled · Took 2.0s")
            );
        }
    }

    #[test]
    fn child_live_commit_preserves_identity_content_order_and_timing() {
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let mut view = ConversationView::default();
        let mut live = LiveRunSnapshot {
            assistant_text: "before".into(),
            reasoning_text: "reason".into(),
            tool: None,
        };
        view.observe_child(&session, Some(&live), true).unwrap();
        let source = view.source().clone();
        let reasoning = view.blocks[0].id;
        let text = view.blocks[1].id;
        let revision = view.projection_revision;
        view.observe_child(&session, Some(&live), true).unwrap();
        assert_eq!(view.projection_revision, revision);
        assert_eq!(view.blocks[0].id, reasoning);
        assert_eq!(view.blocks[1].id, text);
        let epoch = view.blocks[1]
            .block
            .view(text, BlockRole::Assistant, false)
            .content_epoch;
        live.assistant_text.push_str(" continued");
        view.observe_child(&session, Some(&live), true).unwrap();
        assert_eq!(view.blocks[1].id, text);
        assert_eq!(
            view.blocks[1]
                .block
                .view(text, BlockRole::Assistant, false)
                .content_epoch,
            epoch
        );
        live.assistant_text.clear();
        live.reasoning_text.clear();
        live.tool = Some(phi_sdk::agent_instance::LiveToolSnapshot {
            id: "call".into(),
            name: "Bash".into(),
            args: serde_json::json!({}),
            display: None,
        });
        view.observe_child(&session, Some(&live), true).unwrap();
        let tool = view.blocks.last().unwrap().id;
        let started = view.tool_block_meta[&tool].tool_timer.unwrap().started_at;
        let ids = view.blocks.iter().map(|block| block.id).collect::<Vec<_>>();
        let revision = view.projection_revision;
        view.observe_child(&session, Some(&live), true).unwrap();
        assert_eq!(view.projection_revision, revision);
        assert_eq!(
            view.blocks.iter().map(|block| block.id).collect::<Vec<_>>(),
            ids
        );
        let epoch = view
            .blocks
            .last()
            .unwrap()
            .block
            .view(tool, BlockRole::Event, false)
            .content_epoch;
        assert!(view.advance_clocks(started + Duration::from_secs(2)));
        assert_eq!(
            view.blocks
                .last()
                .unwrap()
                .block
                .view(tool, BlockRole::Event, false)
                .content_epoch,
            epoch
        );
        append_child_response(
            &mut session,
            vec![
                ContentBlock::Thinking {
                    text: "reason".into(),
                    metadata: None,
                },
                ContentBlock::Text {
                    text: "before continued".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Url {
                        url: "https://example.com/image.png".into(),
                        mime_type: None,
                    },
                },
                ContentBlock::Text {
                    text: "after".into(),
                },
                ContentBlock::ToolCall {
                    id: "call".into(),
                    name: "Bash".into(),
                    args: serde_json::json!({}),
                    provider_call_id: None,
                    provider_item_id: None,
                },
            ],
        );
        session.append_tool_result("call", "done", false, None);
        view.observe_child(&session, None, false).unwrap();
        assert_eq!(view.source(), &source);
        assert_eq!(view.blocks.len(), 5);
        assert_eq!(view.blocks[0].id, reasoning);
        assert_eq!(view.blocks[1].id, text);
        assert!(matches!(&view.blocks[2].block.body, BlockBody::Image(_)));
        assert!(matches!(&view.blocks[3].block.body, BlockBody::Text(text) if text == "after"));
        assert_eq!(view.blocks[4].id, tool);
        assert!(matches!(&view.blocks[4].block.body, BlockBody::Output(text) if text == "done"));
        assert_eq!(view.blocks[1].group_id, view.blocks[3].group_id);
        let timing = view.tool_block_meta[&tool].tool_timer.unwrap();
        assert_eq!(timing.started_at, started);
        assert!(timing.finished_at.is_some());
        let ids = view.blocks.iter().map(|block| block.id).collect::<Vec<_>>();
        view.observe_child(&session, None, false).unwrap();
        assert_eq!(
            view.blocks.iter().map(|block| block.id).collect::<Vec<_>>(),
            ids
        );
        assert_eq!(view.usage.output, 7);
        assert!(!view.advance_clocks(started + Duration::from_secs(60)));
        assert!(view.live_run.is_none());
    }

    #[test]
    fn child_retirement_survives_refresh_expansion_and_followup() {
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        for text in ["old", "retained", "tail"] {
            session.append_user_message(text);
        }
        let mut view = ConversationView::default();
        view.observe_child(&session, None, false).unwrap();
        let source = view.source().clone();
        let retained = view.blocks[1].id;
        view.acknowledge_history_render(42, Some(retained), Some(retained));
        session.append_user_message("next assignment");
        append_child_response(
            &mut session,
            vec![ContentBlock::Text {
                text: "followup".into(),
            }],
        );
        view.observe_child(&session, None, false).unwrap();
        view.toggle_all_collapsible_blocks();
        view.observe_child(&session, None, false).unwrap();
        assert_eq!(view.source(), &source);
        assert_eq!(view.blocks[0].id, retained);
        assert_eq!(view.buffer_first_block, Some(retained));
        assert_eq!(view.painted_history_revision, 42);
        assert!(view.buffer_initialized);
        assert_eq!(view.blocks.len(), 4);
        assert_eq!(view.usage.output, 7);
    }

    #[test]
    fn child_incompatible_history_rebuilds_explicitly() {
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        session.append_user_message("root");
        session.append_user_message("branch");
        let mut view = ConversationView::default();
        view.observe_child(&session, None, false).unwrap();
        view.blocks_expanded = Some(true);
        let source = view.source().clone();
        session.leaf_id = Some(session.entries[0].id.clone());
        view.observe_child(&session, None, false).unwrap();
        assert_ne!(view.source(), &source);
        assert_eq!(view.blocks.len(), 1);
        assert_eq!(view.blocks_expanded, Some(true));
        let source = view.source().clone();
        session.entries[0].kind = TranscriptEntryKind::Message {
            message: AgentMessage::User {
                content: vec![UserContentBlock::Text {
                    text: "replaced".into(),
                }],
            },
        };
        view.observe_child(&session, None, false).unwrap();
        assert_ne!(view.source(), &source);
        let source = view.source().clone();
        session.append_user_message("retained");
        let head = session.leaf_id.clone().unwrap();
        session
            .transcript
            .compress("summary".into(), &head)
            .unwrap();
        view.observe_child(&session, None, false).unwrap();
        assert_ne!(view.source(), &source);
        let source = view.source().clone();
        session.id = "different association".into();
        view.observe_child(&session, None, false).unwrap();
        assert_ne!(view.source(), &source);
        assert!(!view.buffer_initialized);
    }

    #[test]
    fn spinner_deadline_and_cancellation_are_observation_driven() {
        let mut view = ConversationView::default();
        let started = view.last_status_frame;
        assert_eq!(view.next_clock_deadline(), None);
        view.project_main_run_started();
        assert_eq!(
            view.next_clock_deadline(),
            Some(started + STATUS_FRAME_INTERVAL)
        );
        assert!(!view.advance_clocks(started));
        assert!(view.advance_clocks(started + STATUS_FRAME_INTERVAL));
        assert_eq!(view.run_status_view().frame, 1);
        assert!(!view.advance_clocks(started + STATUS_FRAME_INTERVAL));
        view.project_run_cancelled();
        assert_eq!(view.next_clock_deadline(), None);
        assert!(!view.advance_clocks(started + Duration::from_secs(60)));
    }
    #[test]
    fn assistant_restart_preserves_image_only_groups_but_splits_after_text() {
        for text in [None, Some(""), Some("before")] {
            let mut view = ConversationView::default();
            view.project_main_run_started();
            if let Some(text) = text {
                view.project_event(PhiEvent::AssistantDelta {
                    original: text.into(),
                    display: text.into(),
                });
            }
            view.project_event(PhiEvent::AssistantImage {
                source: ImageSource::Url {
                    url: "https://example.com/generated.png".into(),
                    mime_type: None,
                },
            });
            assert!(view.live_assistant_block().is_none());
            assert_eq!(view.assistant_visible, text.is_some());
            let image_group = view.blocks.last().unwrap().group_id;
            view.project_event(PhiEvent::AssistantStarted { id: "next".into() });
            view.project_event(PhiEvent::AssistantDelta {
                original: "after".into(),
                display: "after".into(),
            });
            assert_eq!(
                view.blocks.last().unwrap().group_id == image_group,
                text.is_none()
            );
        }
    }

    #[test]
    fn assistant_visibility_is_independent_of_routes_and_resets_at_display_boundaries() {
        let mut view = ConversationView::default();
        view.project_event(PhiEvent::AssistantDelta {
            original: "".into(),
            display: "".into(),
        });
        assert!(view.assistant_visible);
        assert!(view.live_run.is_none());
        assert!(!view.run_active_projection());
        view.project_main_run_started();
        view.project_event(PhiEvent::TurnFinished {
            response: "".into(),
            display: "".into(),
            usage: Default::default(),
            compacted: false,
        });
        assert!(view.assistant_visible);
        assert!(view.live_run.is_none());
        assert!(!view.run_active_projection());
        let session =
            crate::session::new("test", "model", 1_000, &std::env::current_dir().unwrap());
        view.recover_history(&session);
        assert!(!view.assistant_visible);
        for boundary in [
            PhiEvent::AssistantStarted { id: "next".into() },
            PhiEvent::ToolCallStarted {
                id: "tool".into(),
                name: "Read".into(),
                args: serde_json::json!({}),
                display: None,
                provider_call_id: None,
                provider_item_id: None,
            },
            PhiEvent::AssistantFinished {
                id: "done".into(),
                content: Vec::new(),
                response: "".into(),
                display: "".into(),
                usage: Default::default(),
                has_tool_calls: false,
            },
            PhiEvent::TurnFailed {
                failure: PhiFailure::new(PhiFailureKind::ProviderRejected, "rejected", None),
            },
        ] {
            view.restore_live_content(Some(&phi_sdk::agent_instance::LiveRunSnapshot {
                assistant_text: "restored partial".into(),
                ..Default::default()
            }));
            assert!(view.assistant_visible);
            view.project_event(boundary);
            assert!(!view.assistant_visible);
        }
        view.restore_live_content(Some(&phi_sdk::agent_instance::LiveRunSnapshot {
            assistant_text: "cancelled partial".into(),
            ..Default::default()
        }));
        view.project_run_cancelled();
        assert!(!view.assistant_visible);
        assert!(view.live_run.is_none());
    }

    #[test]
    fn failure_notice_maps_actionable_kinds_and_preserves_other_details() {
        let rate_limited = failure_notice(&PhiFailure::new(
            PhiFailureKind::RateLimited,
            "provider returned HTTP 429",
            Some(429),
        ));
        let rejected = failure_notice(&PhiFailure::new(
            PhiFailureKind::ProviderRejected,
            "context is too long",
            None,
        ));
        assert_eq!(rate_limited.kind, NotifyKind::Warning);
        assert_eq!(rate_limited.message, "rate limit reached; try again later");
        assert_eq!(rejected.kind, NotifyKind::Error);
        assert_eq!(rejected.message, "context is too long");
    }
}

fn failure_notice(failure: &PhiFailure) -> NotifyMessage {
    let (kind, message) = match failure.kind {
        PhiFailureKind::Authentication => (
            NotifyKind::Error,
            "authentication failed; check provider credentials",
        ),
        PhiFailureKind::RateLimited => (NotifyKind::Warning, "rate limit reached; try again later"),
        PhiFailureKind::ProviderUnavailable => (
            NotifyKind::Warning,
            "provider temporarily unavailable; try again later",
        ),
        PhiFailureKind::MaxTurns => (NotifyKind::Warning, "maximum turn limit reached"),
        PhiFailureKind::Cancelled => (NotifyKind::Warning, "request canceled"),
        PhiFailureKind::InvalidToolCall => {
            (NotifyKind::Error, "model returned an invalid tool call")
        }
        _ => (NotifyKind::Error, failure.message.as_str()),
    };
    NotifyMessage::new(kind, message)
}

pub(in crate::interactive) fn project_transcript_message(
    state: &mut ConversationView,
    message: &AgentMessage,
    tool_display: &dyn ToolDisplayFormatter,
    tool_calls: &mut HashMap<String, BlockId>,
) {
    match message {
        AgentMessage::User { content } => push_user_content(state, content),
        AgentMessage::Context { .. } | AgentMessage::Agent { .. } => {}
        AgentMessage::System { content } => {
            state.push_shared_text_block(BlockRole::System, content);
        }
        AgentMessage::Assistant { content, .. } => {
            let mut group_id = None;
            for block in content {
                match block {
                    ContentBlock::Text { text } => {
                        let block_id = state.push_shared_text_block(BlockRole::Assistant, text);
                        join_display_group(state, block_id, &mut group_id);
                    }
                    ContentBlock::Image { source } => {
                        let block_id =
                            state.push_image_block(BlockRole::Assistant, image_block(source));
                        join_display_group(state, block_id, &mut group_id);
                    }
                    ContentBlock::Thinking { text, .. } if !text.trim().is_empty() => {
                        group_id = None;
                        state.push_shared_thinking_block(text);
                    }
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::ToolCall { id, name, args, .. } => {
                        group_id = None;
                        let block_id = state.project_history_tool(id, name, args, tool_display);
                        tool_calls.insert(id.to_string(), block_id);
                    }
                }
            }
        }
        AgentMessage::ToolResult {
            tool_call_id,
            content,
            is_error,
            details,
            ..
        } => {
            let output = text_from_blocks(content).unwrap_or_default();
            if let Some(block_id) = tool_calls.remove(tool_call_id.as_str()) {
                state.project_tool_result(block_id, &output, *is_error, details.as_ref());
            } else {
                state.push_text_block(BlockRole::Event, output);
            }
        }
    }
}

fn join_display_group(
    state: &mut ConversationView,
    block_id: BlockId,
    group_id: &mut Option<BlockId>,
) {
    if let Some(group_id) = *group_id {
        state.set_block_group(block_id, group_id);
    } else {
        *group_id = Some(block_id);
    }
}

pub(in crate::interactive) fn push_user_content(
    state: &mut ConversationView,
    content: &[UserContentBlock],
) {
    let start = state.blocks.len();
    for part in content {
        match part {
            UserContentBlock::Text { text } if !text.is_empty() => push_user_message(state, text),
            UserContentBlock::Text { .. } => {}
            UserContentBlock::Image { source } => {
                state.push_image_block(BlockRole::User, image_block(source));
            }
        }
    }
    if let Some(group_id) = state.blocks.get(start).map(|block| block.id) {
        for block in &mut state.blocks[start..] {
            block.group_id = group_id;
        }
    }
}

pub(in crate::interactive) fn image_block(source: &ImageSource) -> ImageBlock {
    match source {
        ImageSource::Base64 { data, mime_type } => TerminalImage::from_base64(data, mime_type)
            .map(ImageBlock::terminal)
            .unwrap_or_else(|_| ImageBlock::fallback(format!("[Image: {mime_type} unavailable]"))),
        ImageSource::Url { url, .. } => data_url_image(url)
            .map(ImageBlock::terminal)
            .unwrap_or_else(|| ImageBlock::fallback(format!("[Image: {url}]"))),
    }
}

fn data_url_image(url: &str) -> Option<TerminalImage> {
    let encoded = url.strip_prefix("data:")?;
    let (metadata, data) = encoded.split_once(',')?;
    let (mime_type, encoding) = metadata.split_once(';')?;
    if encoding != "base64" {
        return None;
    }
    TerminalImage::from_base64(data, mime_type).ok()
}

pub(in crate::interactive) fn push_user_message(state: &mut ConversationView, prompt: &str) {
    if prompt.is_empty() {
        return;
    }
    if let Some(block) = crate::resources::prompts::skills::parse_block(prompt) {
        state.push_prompt_disclosure_block(
            BlockRole::User,
            format!("[skill] {}", block.name),
            block.content,
        );
        if let Some(message) = block.user_message {
            state.push_text_block(BlockRole::User, message);
        }
        return;
    }
    if let Some(block) = crate::resources::prompts::files::parse_custom_command_block(prompt) {
        state.push_prompt_disclosure_block(
            BlockRole::User,
            format!("[command] {}", block.name),
            block.content,
        );
        return;
    }
    state.push_text_block(BlockRole::User, prompt);
}
