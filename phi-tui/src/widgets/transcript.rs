use crate::text::{display_width, wrap, wrap_head, wrap_tail};
use crate::widgets::diff::{DiffView, render_diff_tail};
use crate::{
    NotifyKind, NotifyMessage, RenderContext, StyledLine, TerminalImage, ThemeColor, Widget,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ops::Deref;
use std::sync::Arc;

use crate::render::{IncrementalMarkdownState, line_footprint, render_markdown_tail};
use crate::{HistoryBudget, HistoryFootprint, HistorySnapshot};

#[derive(Debug, Clone, Default)]
pub struct Transcript {
    pub groups: Vec<TranscriptGroup>,
    pub expansion_key_hint: Option<String>,
}

impl Transcript {
    pub fn has_collapsed_blocks(&self) -> bool {
        self.groups
            .iter()
            .flat_map(|group| &group.blocks)
            .any(|block| block.is_collapsible() && !block.is_expanded())
    }

    pub fn set_all_expanded(&mut self, expanded: bool) -> bool {
        let mut changed = false;
        for block in self.groups.iter_mut().flat_map(|group| &mut group.blocks) {
            changed |= block.set_expanded(expanded);
        }
        changed
    }

    pub fn toggle_all_expansions(&mut self) -> bool {
        let expanded = self.has_collapsed_blocks();
        self.set_all_expanded(expanded)
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptGroup {
    pub role: BlockRole,
    pub blocks: Vec<TranscriptBlock>,
}

impl TranscriptGroup {
    pub fn new(role: BlockRole, blocks: Vec<TranscriptBlock>) -> Self {
        Self { role, blocks }
    }
}

#[derive(Debug, Clone)]
pub struct TranscriptBlock {
    pub title: String,
    pub title_suffix: Option<String>,
    pub input: Option<String>,
    pub body: BlockBody,
    pub footer: Option<StyledLine>,
    pub expansion: BlockExpansion,
    pub status: BlockStatus,
    pub indent: usize,
}

impl TranscriptBlock {
    pub fn text(text: impl Into<BlockText>) -> Self {
        Self {
            title: String::new(),
            title_suffix: None,
            input: None,
            body: BlockBody::Text(text.into()),
            footer: None,
            expansion: BlockExpansion::Fixed,
            status: BlockStatus::Idle,
            indent: 0,
        }
    }

    pub fn collapsible_text(text: impl Into<BlockText>) -> Self {
        Self {
            expansion: BlockExpansion::Expanded,
            ..Self::text(text)
        }
    }

    pub fn image(image: ImageBlock) -> Self {
        Self {
            title: String::new(),
            title_suffix: None,
            input: None,
            body: BlockBody::Image(image),
            footer: None,
            expansion: BlockExpansion::Fixed,
            status: BlockStatus::Idle,
            indent: 0,
        }
    }

    pub fn output(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            title_suffix: None,
            input: None,
            body: BlockBody::Output(String::new().into()),
            footer: None,
            expansion: BlockExpansion::Collapsed,
            status: BlockStatus::Running,
            indent: 0,
        }
    }

    pub fn is_collapsible(&self) -> bool {
        self.expansion != BlockExpansion::Fixed
    }

    pub fn is_expanded(&self) -> bool {
        self.expansion != BlockExpansion::Collapsed
    }

    pub fn set_expanded(&mut self, expanded: bool) -> bool {
        if !self.is_collapsible() {
            return false;
        }
        let expansion = if expanded {
            BlockExpansion::Expanded
        } else {
            BlockExpansion::Collapsed
        };
        let changed = self.expansion != expansion;
        self.expansion = expansion;
        changed
    }

    pub fn toggle_expansion(&mut self) -> bool {
        if !self.is_collapsible() {
            return false;
        }
        self.set_expanded(!self.is_expanded())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockExpansion {
    Fixed,
    Collapsed,
    Expanded,
}

#[derive(Debug, Clone)]
pub enum BlockBody {
    Text(BlockText),
    Image(ImageBlock),
    Notify(NotifyMessage),
    Styled(Vec<StyledLine>),
    Prompt(String),
    Thinking(BlockText),
    Output(BlockText),
    OutputSections(Vec<OutputSection>),
    Group(BlockText),
    Diff(DiffView),
    Log(String),
}

#[derive(Debug, Clone)]
pub struct OutputSection {
    pub text: BlockText,
    pub preview_chars: Option<usize>,
}

#[derive(Debug, Clone)]
pub enum BlockText {
    Mutable(String),
    Shared(Arc<str>),
}

impl BlockText {
    pub fn shared(text: Arc<str>) -> Self {
        Self::Shared(text)
    }

    pub fn append(&mut self, text: &str) {
        match self {
            Self::Mutable(content) => content.push_str(text),
            Self::Shared(content) => {
                let mut mutable = content.to_string();
                mutable.push_str(text);
                *self = Self::Mutable(mutable);
            }
        }
    }
}

impl Deref for BlockText {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Mutable(text) => text,
            Self::Shared(text) => text,
        }
    }
}

impl PartialEq<str> for BlockText {
    fn eq(&self, other: &str) -> bool {
        &**self == other
    }
}

impl PartialEq<&str> for BlockText {
    fn eq(&self, other: &&str) -> bool {
        &**self == *other
    }
}

impl From<String> for BlockText {
    fn from(text: String) -> Self {
        Self::Mutable(text)
    }
}

impl From<&str> for BlockText {
    fn from(text: &str) -> Self {
        Self::Mutable(text.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct ImageBlock {
    pub image: Option<TerminalImage>,
    pub fallback: String,
}

impl ImageBlock {
    pub fn terminal(image: TerminalImage) -> Self {
        let fallback = image.fallback();
        Self {
            image: Some(image),
            fallback,
        }
    }

    pub fn fallback(text: impl Into<String>) -> Self {
        Self {
            image: None,
            fallback: text.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PartFlow {
    Start,
    Continue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockRole {
    User,
    Assistant,
    System,
    Event,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStatus {
    Idle,
    Running,
    Success,
    Error,
}

#[derive(Debug, Clone, Copy)]
pub struct TranscriptBlockView<'a> {
    pub id: u64,
    pub layout_revision: u64,
    /// Changes only when text is replaced; append-only growth keeps the same epoch.
    pub content_epoch: u64,
    pub role: BlockRole,
    pub starts_group: bool,
    pub block: &'a TranscriptBlock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptRenderRequest {
    pub width: u16,
    pub row_budget: usize,
    pub projection_revision: u64,
    pub theme_revision: u64,
    pub expansion_key_hint: Option<&'static str>,
    pub text_expansion_row_limit: Option<usize>,
}

#[derive(Debug, Clone, Default)]
pub struct MaterializedTranscript {
    pub logical_origin: usize,
    pub history_revision: u64,
    pub first_block: Option<u64>,
    pub retire_before: Option<u64>,
    pub lines: Vec<StyledLine>,
    pub history: HistorySnapshot,
    pub footprint: HistoryFootprint,
}

#[derive(Debug, Clone, Default)]
pub struct TranscriptTail {
    history_budget: HistoryBudget,
    history_revision: u64,
    retained_blocks: Vec<BlockRows>,
    cache: LayoutCache,
    incremental_markdown: HashMap<u64, IncrementalMarkdownLayout>,
    last_request: Option<TranscriptRenderRequest>,
    last_materialized: MaterializedTranscript,
}

#[derive(Debug, Clone)]
struct BlockRows {
    id: u64,
    key: LayoutKey,
    footprint: HistoryFootprint,
    lines: Arc<[StyledLine]>,
    image_ids: Arc<[u32]>,
}

#[derive(Debug, Clone)]
struct IncrementalMarkdownLayout {
    content_epoch: u64,
    width: u16,
    theme_revision: u64,
    flow: PartFlow,
    markdown: IncrementalMarkdownState,
}

const DEFAULT_LAYOUT_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LayoutKey {
    id: u64,
    role: BlockRole,
    flow: PartFlow,
    layout_revision: u64,
    width: u16,
    theme_revision: u64,
    expansion_key_hint: Option<&'static str>,
    expanded: bool,
}

#[derive(Debug, Clone)]
struct CachedLayout {
    rendered: BlockRender,
    bytes: usize,
}

#[derive(Debug, Clone)]
struct LayoutCache {
    entries: HashMap<LayoutKey, CachedLayout>,
    order: VecDeque<LayoutKey>,
    bytes: usize,
    budget: usize,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            budget: DEFAULT_LAYOUT_CACHE_BYTES,
        }
    }
}

impl TranscriptTail {
    pub fn set_history_budget(&mut self, budget: HistoryBudget) {
        if self.history_budget != budget {
            self.history_budget = budget;
            self.last_request = None;
        }
    }

    pub fn materialize<'a>(
        &mut self,
        blocks: impl DoubleEndedIterator<Item = TranscriptBlockView<'a>>,
        request: TranscriptRenderRequest,
    ) -> MaterializedTranscript {
        if self.last_request == Some(request) {
            return self.last_materialized.clone();
        }
        let (retained_blocks, footprint, retire_before) = self.select_history(blocks, request);
        let width_changed = self
            .last_request
            .is_some_and(|old| old.width != request.width);
        if width_changed || self.history_replaced(&retained_blocks, request.row_budget) {
            self.history_revision = self.history_revision.saturating_add(1);
        }
        let retained_ids = retained_blocks
            .iter()
            .map(|block| block.id)
            .collect::<HashSet<_>>();
        self.incremental_markdown
            .retain(|id, _| retained_ids.contains(id));
        let first_block = retained_blocks.first().map(|block| block.id);
        let history = history_snapshot(&retained_blocks, request.width);
        let lines = history.tail(request.row_budget.max(1));
        self.retained_blocks = retained_blocks;
        let materialized = MaterializedTranscript {
            logical_origin: history.rows().saturating_sub(lines.len()),
            history_revision: self.history_revision,
            first_block,
            retire_before,
            lines,
            history,
            footprint,
        };
        self.last_request = Some(request);
        self.last_materialized = materialized.clone();
        materialized
    }

    fn select_history<'a>(
        &mut self,
        blocks: impl DoubleEndedIterator<Item = TranscriptBlockView<'a>>,
        request: TranscriptRenderRequest,
    ) -> (Vec<BlockRows>, HistoryFootprint, Option<u64>) {
        let previous = self
            .retained_blocks
            .iter()
            .map(|block| (block.id, block.clone()))
            .collect::<HashMap<_, _>>();
        let first = self.retained_blocks.first().map(|block| block.id);
        let separator = line_footprint(&block_separator(request.width), request.width);
        let mut retained = Vec::new();
        let mut footprint = HistoryFootprint::default();
        let mut overflow = false;
        let mut newest = None;
        let mut reached_first = false;
        for block in blocks
            .rev()
            .take(self.history_budget.measurement_rows(request.width).max(1))
        {
            if reached_first {
                break;
            }
            reached_first = Some(block.id) == first;
            newest.get_or_insert(block.id);
            let Some(block) = self.retained_block(block, request, previous.get(&block.id)) else {
                overflow = true;
                break;
            };
            if block.lines.is_empty() {
                continue;
            }
            let mut next = footprint.add(block.footprint);
            if !retained.is_empty() {
                next = next.add(separator);
            }
            if !self.history_budget.fits(next) {
                overflow = true;
                break;
            }
            footprint = next;
            retained.push(block);
        }
        if overflow && !previous.is_empty() {
            let target = self.history_budget.eviction_target();
            while retained.len() > 1 && !target.fits(footprint) {
                footprint = footprint
                    .subtract(retained.pop().unwrap().footprint)
                    .subtract(separator);
            }
        }
        retained.reverse();
        let retire_before = retained.first().map(|block| block.id).or(newest);
        (retained, footprint, retire_before)
    }

    fn retained_block(
        &mut self,
        block: TranscriptBlockView<'_>,
        request: TranscriptRenderRequest,
        previous: Option<&BlockRows>,
    ) -> Option<BlockRows> {
        if source_bytes(block.block) > self.history_budget.resident_bytes {
            return None;
        }
        let rows = self.history_budget.measurement_rows(request.width).max(1);
        let expanded = self.resolve_text_expansion(block, request, rows);
        let key = LayoutKey::new(block, request, expanded);
        if let Some(previous) = previous.filter(|previous| previous.key == key) {
            return Some(previous.clone());
        }
        if let Some(previous) = previous.filter(|previous| {
            LayoutKey {
                expanded: previous.key.expanded,
                ..key
            } == previous.key
        }) {
            let rendered = self.layout_block(block, request, rows, key.expanded);
            return Some(BlockRows::new(key, previous.footprint, rendered.lines));
        }
        let mut rendered = self.layout_block(block, request, rows, true);
        if rendered.omitted_rows > 0 {
            return None;
        }
        let mut footprint = rendered.footprint(request.width);
        if block.block.is_collapsible() {
            let collapsed = self.layout_block(block, request, rows, false);
            footprint = footprint.maximum(collapsed.footprint(request.width));
            if !key.expanded {
                rendered = collapsed;
            }
        }
        footprint.resident_bytes +=
            std::mem::size_of::<BlockRows>() + 2 * std::mem::size_of::<Arc<[StyledLine]>>();
        Some(BlockRows::new(key, footprint, rendered.lines))
    }

    fn resolve_text_expansion(
        &mut self,
        block: TranscriptBlockView<'_>,
        request: TranscriptRenderRequest,
        row_budget: usize,
    ) -> bool {
        let Some(row_limit) = request.text_expansion_row_limit else {
            return block.block.is_expanded();
        };
        if !block.block.is_collapsible() || !matches!(block.block.body, BlockBody::Text(_)) {
            return block.block.is_expanded();
        }
        let rendered = self.layout_block(block, request, row_budget, true);
        rendered.omitted_rows == 0 && rendered.lines.len() <= row_limit
    }

    fn layout_block(
        &mut self,
        block: TranscriptBlockView<'_>,
        request: TranscriptRenderRequest,
        row_budget: usize,
        expanded: bool,
    ) -> BlockRender {
        let TranscriptRenderRequest {
            width,
            theme_revision,
            expansion_key_hint,
            ..
        } = request;
        let key = LayoutKey::new(block, request, expanded);
        self.cache
            .remove_block_revisions(block.id, block.layout_revision);
        if let BlockBody::Text(text) = &block.block.body
            && block.role == BlockRole::Assistant
            && block.block.indent == 0
        {
            return self.render_incremental_markdown(
                block,
                text,
                width,
                theme_revision,
                row_budget,
            );
        }
        if let Some(rendered) = self.cache.get(key, row_budget) {
            return rendered;
        }
        let rendered =
            render_block_expansion(block, width, expansion_key_hint, row_budget, expanded);
        self.cache.insert(key, rendered.clone());
        rendered
    }

    fn render_incremental_markdown(
        &mut self,
        block: TranscriptBlockView<'_>,
        text: &str,
        width: u16,
        theme_revision: u64,
        row_budget: usize,
    ) -> BlockRender {
        let flow = block.flow();
        let layout = self
            .incremental_markdown
            .entry(block.id)
            .or_insert_with(|| IncrementalMarkdownLayout {
                content_epoch: block.content_epoch,
                width,
                theme_revision,
                flow,
                markdown: IncrementalMarkdownState::default(),
            });
        if layout.content_epoch != block.content_epoch
            || layout.width != width
            || layout.theme_revision != theme_revision
            || layout.flow != flow
        {
            *layout = IncrementalMarkdownLayout {
                content_epoch: block.content_epoch,
                width,
                theme_revision,
                flow,
                markdown: IncrementalMarkdownState::default(),
            };
        }
        let body_width = block_body_width(width);
        let markdown =
            layout
                .markdown
                .render_tail(text.trim(), body_width, ThemeColor::Default, row_budget);
        render_assistant_markdown(markdown, width, flow, row_budget)
    }

    fn history_replaced(&self, next: &[BlockRows], paint_rows: usize) -> bool {
        if self.retained_blocks.is_empty() {
            return false;
        }
        if self.retained_blocks.first().map(|block| block.id) != next.first().map(|block| block.id)
        {
            return true;
        }
        if next.len() < self.retained_blocks.len() {
            return true;
        }
        let rows = next
            .iter()
            .map(|block| block.lines.len() + 1)
            .sum::<usize>()
            .saturating_sub(1);
        let mut prefix = rows.saturating_sub(paint_rows);
        self.retained_blocks.iter().zip(next).any(|(old, new)| {
            let count = prefix.min(new.lines.len());
            prefix = prefix.saturating_sub(new.lines.len() + 1);
            old.id != new.id
                || old.key.expanded != new.key.expanded
                || old.key != new.key && old.lines.get(..count) != new.lines.get(..count)
        })
    }
}

impl BlockRows {
    fn new(key: LayoutKey, footprint: HistoryFootprint, lines: Vec<StyledLine>) -> Self {
        let image_ids = lines
            .iter()
            .filter_map(|line| line.image.as_ref())
            .map(|image| image.id())
            .collect::<Vec<_>>()
            .into();
        Self {
            id: key.id,
            key,
            footprint,
            lines: lines.into(),
            image_ids,
        }
    }
}

impl LayoutKey {
    fn new(
        block: TranscriptBlockView<'_>,
        request: TranscriptRenderRequest,
        expanded: bool,
    ) -> Self {
        Self {
            id: block.id,
            role: block.role,
            flow: block.flow(),
            layout_revision: block.layout_revision,
            width: request.width,
            theme_revision: request.theme_revision,
            expansion_key_hint: request.expansion_key_hint,
            expanded,
        }
    }
}

fn source_bytes(block: &TranscriptBlock) -> usize {
    let body = match &block.body {
        BlockBody::Text(text)
        | BlockBody::Thinking(text)
        | BlockBody::Output(text)
        | BlockBody::Group(text) => text.len(),
        BlockBody::OutputSections(sections) => {
            sections.iter().map(|section| section.text.len()).sum()
        }
        BlockBody::Prompt(text) | BlockBody::Log(text) => text.len(),
        BlockBody::Notify(message) => message.message.len(),
        BlockBody::Image(image) => image.fallback.len(),
        BlockBody::Styled(lines) => styled_lines_bytes(lines),
        BlockBody::Diff(diff) => {
            diff.lines
                .iter()
                .map(|line| match line {
                    crate::DiffLine::Change { text, .. }
                    | crate::DiffLine::Prefixed { text, .. } => text.len(),
                    crate::DiffLine::Omitted => 0,
                })
                .fold(0usize, usize::saturating_add)
        }
    };
    body.saturating_add(block.title.len())
        .saturating_add(block.title_suffix.as_ref().map_or(0, String::len))
        .saturating_add(block.input.as_ref().map_or(0, String::len))
        .saturating_add(block.footer.as_ref().map_or(0, |footer| footer.text.len()))
}

fn history_snapshot(blocks: &[BlockRows], width: u16) -> HistorySnapshot {
    let separator: Arc<[StyledLine]> = vec![block_separator(width)].into();
    let mut chunks = Vec::with_capacity(blocks.len().saturating_mul(2));
    for block in blocks {
        if !chunks.is_empty() {
            chunks.push(separator.clone());
        }
        chunks.push(block.lines.clone());
    }
    let image_ids = blocks
        .iter()
        .flat_map(|block| block.image_ids.iter().copied())
        .collect();
    HistorySnapshot::new(chunks, image_ids)
}

impl LayoutCache {
    fn get(&mut self, key: LayoutKey, row_budget: usize) -> Option<BlockRender> {
        let rendered = self.entries.get(&key)?.rendered.clone();
        if rendered.omitted_rows > 0 && rendered.lines.len() < row_budget {
            return None;
        }
        self.touch(key);
        Some(retain_render_tail(rendered, row_budget))
    }

    fn insert(&mut self, key: LayoutKey, rendered: BlockRender) {
        let bytes = styled_lines_bytes(&rendered.lines)
            + std::mem::size_of::<CachedLayout>()
            + 2 * std::mem::size_of::<LayoutKey>();
        if bytes > self.budget {
            return;
        }
        if let Some(previous) = self.entries.insert(key, CachedLayout { rendered, bytes }) {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
        }
        self.bytes += bytes;
        self.touch(key);
        while self.bytes > self.budget {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.bytes);
            }
        }
    }

    fn touch(&mut self, key: LayoutKey) {
        self.order.retain(|candidate| *candidate != key);
        self.order.push_back(key);
    }

    fn remove_block_revisions(&mut self, id: u64, layout_revision: u64) {
        let stale = self
            .entries
            .keys()
            .copied()
            .filter(|key| key.id == id && key.layout_revision != layout_revision)
            .collect::<Vec<_>>();
        for key in stale {
            if let Some(removed) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(removed.bytes);
            }
            self.order.retain(|candidate| *candidate != key);
        }
    }
}

fn styled_lines_bytes(lines: &[StyledLine]) -> usize {
    lines.iter().fold(0, |bytes, line| {
        let segment_bytes = line
            .segments
            .iter()
            .map(|segment| segment.heap_capacity() + std::mem::size_of_val(segment))
            .sum::<usize>();
        bytes + line.text.capacity() + segment_bytes + std::mem::size_of_val(line)
    })
}

fn render_transcript<'a>(
    blocks: impl IntoIterator<Item = TranscriptBlockView<'a>>,
    width: u16,
    expansion_key_hint: Option<&str>,
) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    for block in blocks {
        let rendered = render_block(block, width, expansion_key_hint, usize::MAX).lines;
        if rendered.is_empty() {
            continue;
        }
        if !lines.is_empty() {
            lines.push(block_separator(width));
        }
        lines.extend(rendered);
    }
    lines
}

fn block_separator(width: u16) -> StyledLine {
    StyledLine::new(String::new(), ThemeColor::Default).fit(width)
}

#[derive(Debug, Clone)]
struct BlockRender {
    lines: Vec<StyledLine>,
    omitted_rows: usize,
}

impl BlockRender {
    fn footprint(&self, width: u16) -> HistoryFootprint {
        self.lines
            .iter()
            .fold(HistoryFootprint::default(), |total, line| {
                total.add(line_footprint(line, width))
            })
    }
}

struct TailBuffer {
    lines: VecDeque<StyledLine>,
    total_rows: usize,
    row_budget: usize,
}

impl TailBuffer {
    fn new(row_budget: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            total_rows: 0,
            row_budget: row_budget.max(1),
        }
    }

    fn omit_rows(&mut self, rows: usize) {
        self.total_rows = self.total_rows.saturating_add(rows);
    }

    fn push(&mut self, line: StyledLine) {
        self.total_rows = self.total_rows.saturating_add(1);
        self.lines.push_back(line);
        if self.lines.len() > self.row_budget {
            self.lines.pop_front();
        }
    }

    fn extend(&mut self, lines: impl IntoIterator<Item = StyledLine>) {
        for line in lines {
            self.push(line);
        }
    }

    fn finish(self) -> BlockRender {
        let lines = self.lines.into_iter().collect::<Vec<_>>();
        BlockRender {
            omitted_rows: self.total_rows.saturating_sub(lines.len()),
            lines,
        }
    }
}

fn retain_block_tail(lines: Vec<StyledLine>, row_budget: usize) -> BlockRender {
    retain_render_tail(
        BlockRender {
            lines,
            omitted_rows: 0,
        },
        row_budget,
    )
}

fn retain_render_tail(mut rendered: BlockRender, row_budget: usize) -> BlockRender {
    let dropped = rendered.lines.len().saturating_sub(row_budget);
    if dropped > 0 {
        rendered.lines = rendered.lines.split_off(dropped);
        rendered.omitted_rows = rendered.omitted_rows.saturating_add(dropped);
    }
    rendered
}

fn render_block(
    block: TranscriptBlockView<'_>,
    width: u16,
    expansion_key_hint: Option<&str>,
    row_budget: usize,
) -> BlockRender {
    render_block_expansion(
        block,
        width,
        expansion_key_hint,
        row_budget,
        block.block.is_expanded(),
    )
}

fn render_block_expansion(
    block: TranscriptBlockView<'_>,
    width: u16,
    expansion_key_hint: Option<&str>,
    row_budget: usize,
    expanded: bool,
) -> BlockRender {
    if let BlockBody::Text(text) = &block.block.body
        && block.role == BlockRole::Assistant
        && block.block.indent == 0
    {
        return render_assistant(text, width, block.flow(), row_budget);
    }
    let prefix = "  ".repeat(block.block.indent);
    let inner_width = (width as usize)
        .saturating_sub(display_width(&prefix))
        .max(8) as u16;
    let rendered = match &block.block.body {
        BlockBody::Output(output) => render_output(
            block.block,
            output,
            expanded,
            inner_width,
            expansion_key_hint,
            row_budget,
        ),
        BlockBody::OutputSections(sections) => {
            let output = render_output_sections(sections, expanded);
            render_output(
                block.block,
                &output,
                expanded,
                inner_width,
                expansion_key_hint,
                row_budget,
            )
        }
        BlockBody::Group(output) => render_group(
            block.block,
            output,
            expanded,
            inner_width,
            expansion_key_hint,
            row_budget,
        ),
        BlockBody::Diff(diff) => render_diff(block.block, diff, expanded, inner_width, row_budget),
        BlockBody::Prompt(text) => render_prompt(
            block.block,
            block.role,
            text,
            expanded,
            inner_width,
            expansion_key_hint,
            row_budget,
        ),
        BlockBody::Thinking(text) => render_thinking(
            block.block,
            text,
            expanded,
            inner_width,
            expansion_key_hint,
            row_budget,
        ),
        BlockBody::Log(text) => render_log(text, inner_width, row_budget),
        BlockBody::Styled(lines) => render_styled(lines, inner_width, row_budget),
        BlockBody::Text(text) if block.block.is_collapsible() => render_collapsible_text(
            block.role,
            text,
            expanded,
            inner_width,
            expansion_key_hint,
            row_budget,
        ),
        BlockBody::Text(text) => render_text(
            block_prefix(block.role),
            text,
            inner_width,
            block_color(block.role),
            row_budget,
        ),
        BlockBody::Notify(message) => render_notification(message, inner_width, row_budget),
        BlockBody::Image(image) => retain_block_tail(
            render_image_block(block.role, block.flow(), image, inner_width),
            row_budget,
        ),
    };
    let rendered = if let Some(footer) = &block.block.footer {
        let mut rendered = rendered;
        rendered.lines.push(footer.clone().fit(inner_width));
        retain_render_tail(rendered, row_budget)
    } else {
        rendered
    };
    if prefix.is_empty() {
        return rendered;
    }
    BlockRender {
        lines: rendered
            .lines
            .into_iter()
            .map(|line| {
                line.prepend_segment(prefix.as_str(), ThemeColor::Default)
                    .fit(width)
            })
            .collect(),
        omitted_rows: rendered.omitted_rows,
    }
}

fn render_styled(lines: &[StyledLine], width: u16, row_budget: usize) -> BlockRender {
    let total_rows = lines.len();
    let lines = lines
        .iter()
        .rev()
        .take(row_budget)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|line| line.fit(width))
        .collect::<Vec<_>>();
    BlockRender {
        omitted_rows: total_rows.saturating_sub(lines.len()),
        lines,
    }
}

impl TranscriptBlockView<'_> {
    fn flow(self) -> PartFlow {
        if self.starts_group {
            PartFlow::Start
        } else {
            PartFlow::Continue
        }
    }
}

impl Widget for Transcript {
    type Action = ();

    fn render(&self, context: &RenderContext<'_>) -> Vec<StyledLine> {
        let area = context.area;
        let width = area.width;
        render_transcript(
            self.groups.iter().flat_map(|group| {
                group
                    .blocks
                    .iter()
                    .enumerate()
                    .map(|(index, block)| TranscriptBlockView {
                        id: 0,
                        layout_revision: 0,
                        content_epoch: 0,
                        role: group.role,
                        starts_group: index == 0,
                        block,
                    })
            }),
            width,
            self.expansion_key_hint.as_deref(),
        )
    }
}

fn block_prefix(role: BlockRole) -> &'static str {
    match role {
        BlockRole::User => ">",
        BlockRole::Assistant => "",
        BlockRole::System => "system",
        BlockRole::Event => "⏺",
        BlockRole::Error => "",
    }
}

fn block_color(role: BlockRole) -> ThemeColor {
    match role {
        BlockRole::Event => ThemeColor::Title,
        BlockRole::System => ThemeColor::Dim,
        BlockRole::Error => ThemeColor::Error,
        BlockRole::User | BlockRole::Assistant => ThemeColor::Default,
    }
}

const BLOCK_PREFIX: &str = "⏺  ";

fn block_indent() -> String {
    " ".repeat(display_width(BLOCK_PREFIX))
}

fn block_body_width(width: u16) -> u16 {
    (width as usize)
        .saturating_sub(display_width(BLOCK_PREFIX))
        .max(8) as u16
}

fn render_assistant(text: &str, width: u16, flow: PartFlow, row_budget: usize) -> BlockRender {
    let row_budget = row_budget.max(1);
    let body_width = block_body_width(width);
    let markdown = render_markdown_tail(text.trim(), body_width, ThemeColor::Default, row_budget);
    render_assistant_markdown(markdown, width, flow, row_budget)
}

fn render_assistant_markdown(
    markdown: crate::render::MarkdownTail,
    width: u16,
    flow: PartFlow,
    row_budget: usize,
) -> BlockRender {
    let row_budget = row_budget.max(1);
    let indent = block_indent();
    let omitted_rows = markdown.omitted_rows;
    let total_rows = omitted_rows + markdown.lines.len();
    let omitted_rows = total_rows.saturating_sub(row_budget);
    let mut lines = Vec::new();
    lines.extend(markdown.lines.into_iter().enumerate().map(|(index, line)| {
        let prefix = if index == 0 && flow == PartFlow::Start && omitted_rows == 0 {
            BLOCK_PREFIX
        } else {
            indent.as_str()
        };
        line.prepend_segment(prefix, ThemeColor::Default).fit(width)
    }));
    BlockRender {
        lines,
        omitted_rows,
    }
}

fn render_text(
    prefix: &str,
    text: &str,
    width: u16,
    color: ThemeColor,
    row_budget: usize,
) -> BlockRender {
    let label = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix} ")
    };
    let body_width = (width as usize)
        .saturating_sub(display_width(&label))
        .max(8);
    let (wrapped, wrapped_rows) = wrap_tail(text, body_width, row_budget);
    let mut retained = TailBuffer::new(row_budget);
    retained.omit_rows(wrapped_rows.saturating_sub(wrapped.len()));
    let first_index = wrapped_rows.saturating_sub(wrapped.len());
    retained.extend(wrapped.into_iter().enumerate().map(|(offset, line)| {
        let rendered = if first_index + offset == 0 {
            format!("{label}{line}")
        } else {
            format!("{}{line}", " ".repeat(label.len()))
        };
        StyledLine::new(rendered, color).fit(width)
    }));
    retained.finish()
}

fn text_label_width(role: BlockRole) -> usize {
    let prefix = block_prefix(role);
    if prefix.is_empty() {
        0
    } else {
        display_width(prefix).saturating_add(1)
    }
}

fn render_collapsible_text(
    role: BlockRole,
    text: &str,
    expanded: bool,
    width: u16,
    expansion_key_hint: Option<&str>,
    row_budget: usize,
) -> BlockRender {
    if expanded {
        return render_text(
            block_prefix(role),
            text,
            width,
            block_color(role),
            row_budget,
        );
    }
    render_collapsed_text(role, text, width, expansion_key_hint, row_budget)
}

fn render_collapsed_text(
    role: BlockRole,
    text: &str,
    width: u16,
    expansion_key_hint: Option<&str>,
    row_budget: usize,
) -> BlockRender {
    const PREVIEW_ROWS: usize = 3;
    let body_width = (width as usize)
        .saturating_sub(text_label_width(role))
        .max(8);
    let (rows, total_rows) = wrap_head(text, body_width, PREVIEW_ROWS);
    let preview_rows = rows.len();
    let label = match block_prefix(role) {
        "" => String::new(),
        prefix => format!("{prefix} "),
    };
    let indent = " ".repeat(display_width(&label));
    let mut rendered = BlockRender {
        lines: rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let prefix = if index == 0 { &label } else { &indent };
                StyledLine::new(format!("{prefix}{row}"), block_color(role)).fit(width)
            })
            .collect(),
        omitted_rows: 0,
    };
    let omitted_rows = total_rows.saturating_sub(preview_rows);
    if omitted_rows > 0 {
        rendered.lines.push(collapsed_text_footer(
            role,
            omitted_rows,
            width,
            expansion_key_hint,
        ));
    }
    retain_render_tail(rendered, row_budget)
}

fn collapsed_text_footer(
    role: BlockRole,
    omitted_rows: usize,
    width: u16,
    expansion_key_hint: Option<&str>,
) -> StyledLine {
    let indent = " ".repeat(text_label_width(role));
    let hint = expansion_hint(false, expansion_key_hint);
    let text = match omitted_rows {
        0 => format!("{indent}└╴{hint}"),
        1 => format!("{indent}└╴ +1 line{hint}"),
        count => format!("{indent}└╴ +{count} lines{hint}"),
    };
    StyledLine::new(text, ThemeColor::Dim).fit(width)
}

fn render_image_block(
    role: BlockRole,
    flow: PartFlow,
    image: &ImageBlock,
    width: u16,
) -> Vec<StyledLine> {
    let mut lines = Vec::new();
    let prefix = match flow {
        PartFlow::Start => block_prefix(role),
        PartFlow::Continue => "",
    };
    let label = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix} ")
    };
    let line = if let Some(image) = &image.image {
        StyledLine::image(image.clone())
    } else {
        StyledLine::new(&image.fallback, block_color(role))
    };
    lines.push(line.prepend_segment(label, block_color(role)).fit(width));
    lines
}

fn render_prompt(
    entry: &TranscriptBlock,
    role: BlockRole,
    text: &str,
    expanded: bool,
    width: u16,
    expansion_key_hint: Option<&str>,
    row_budget: usize,
) -> BlockRender {
    let prefix = block_prefix(role);
    let label = if prefix.is_empty() {
        String::new()
    } else {
        format!("{prefix} ")
    };
    let mut retained = TailBuffer::new(row_budget);
    retained.push(
        StyledLine::segments([
            (label, block_color(role)),
            (entry.title.clone(), block_color(role)),
            (
                expansion_hint(expanded, expansion_key_hint),
                ThemeColor::Dim,
            ),
        ])
        .fit(width),
    );
    if expanded && !text.trim().is_empty() {
        let markdown = render_markdown_tail(text.trim(), width, ThemeColor::Dim, row_budget);
        retained.omit_rows(markdown.omitted_rows);
        retained.extend(markdown.lines);
    }
    retained.finish()
}

fn render_notification(message: &NotifyMessage, width: u16, row_budget: usize) -> BlockRender {
    let text = message.message.trim();
    if text.is_empty() {
        return retain_block_tail(Vec::new(), row_budget);
    }

    let (prefix, color) = match message.kind {
        NotifyKind::Info => ("", ThemeColor::Dim),
        NotifyKind::Success => ("", ThemeColor::Success),
        NotifyKind::Warning => ("Warning: ", ThemeColor::Warning),
        NotifyKind::Error => ("Error: ", ThemeColor::Error),
    };
    render_text(prefix, text, width, color, row_budget)
}

fn render_output(
    entry: &TranscriptBlock,
    output: &str,
    expanded: bool,
    width: u16,
    expansion_key_hint: Option<&str>,
    row_budget: usize,
) -> BlockRender {
    let mut retained = TailBuffer::new(row_budget);
    retained.extend(render_output_header(entry, width, expanded));
    if output.is_empty() {
        return retained.finish();
    }
    if !expanded {
        retained.extend(
            wrap(
                &collapsed_output(output, expansion_key_hint),
                width as usize,
            )
            .into_iter()
            .map(|line| StyledLine::new(line, ThemeColor::Dim).fit(width)),
        );
        return retained.finish();
    }
    let output = output.trim_end();
    if output.is_empty() {
        retained.extend(
            wrap(&format!("{}done", output_prefix()), width as usize)
                .into_iter()
                .map(|line| StyledLine::new(line, ThemeColor::Dim).fit(width)),
        );
        return retained.finish();
    }
    let prefix = output_prefix();
    let continuation = " ".repeat(display_width(&prefix));
    let content_width = (width as usize)
        .saturating_sub(display_width(&prefix))
        .max(1);
    for (index, source) in output.lines().enumerate() {
        for (wrapped_index, line) in wrap(source, content_width).into_iter().enumerate() {
            let indent = if index == 0 && wrapped_index == 0 {
                &prefix
            } else {
                &continuation
            };
            retained.push(
                StyledLine::new(line, ThemeColor::Dim)
                    .prepend_segment(indent, ThemeColor::Dim)
                    .fit(width),
            );
        }
    }
    retained.finish()
}

fn render_output_sections(sections: &[OutputSection], expanded: bool) -> String {
    let mut output = String::new();
    for section in sections {
        if expanded
            && let Some(preview_chars) = section.preview_chars
            && let Some((end, _)) = section.text.char_indices().nth(preview_chars)
        {
            output.push_str(section.text[..end].trim_end());
            output.push_str(" ...");
            let text = section.text.trim();
            if text.starts_with('{') && text.ends_with('}') {
                output.push('}');
            }
            output.push('\n');
        } else {
            output.push_str(&section.text);
        }
    }
    output
}

fn render_group(
    entry: &TranscriptBlock,
    output: &str,
    expanded: bool,
    width: u16,
    key_hint: Option<&str>,
    row_budget: usize,
) -> BlockRender {
    if expanded {
        return render_output(entry, output, true, width, key_hint, row_budget);
    }
    let mut header = entry.clone();
    header.title_suffix = Some(format!(
        "{}{}",
        entry.title_suffix.as_deref().unwrap_or_default(),
        expansion_hint(false, key_hint)
    ));
    let mut retained = TailBuffer::new(row_budget);
    retained.extend(render_output_header(&header, width, false));
    retained.finish()
}

fn render_log(text: &str, width: u16, row_budget: usize) -> BlockRender {
    let mut retained = TailBuffer::new(row_budget);
    for source in text.trim_end().lines() {
        let color = log_color(source);
        retained.extend(
            wrap(source, width as usize)
                .into_iter()
                .map(|line| StyledLine::new(line, color).fit(width)),
        );
    }
    retained.finish()
}

fn log_color(line: &str) -> ThemeColor {
    match line.trim_start().split_once(' ').map(|(level, _)| level) {
        Some("WARN") => ThemeColor::Warning,
        Some("ERROR") => ThemeColor::Error,
        _ => ThemeColor::Dim,
    }
}

fn render_diff(
    entry: &TranscriptBlock,
    diff: &DiffView,
    expanded: bool,
    width: u16,
    row_budget: usize,
) -> BlockRender {
    let mut retained = TailBuffer::new(row_budget);
    retained.extend(render_output_header(entry, width, expanded));
    if expanded && !diff.is_empty() {
        retained.push(StyledLine::new(String::new(), ThemeColor::Default).fit(width));
        let indent = block_indent();
        let diff_width = (width as usize)
            .saturating_sub(display_width(&indent))
            .max(8) as u16;
        let (lines, total_rows) = render_diff_tail(diff, diff_width, row_budget);
        retained.omit_rows(total_rows.saturating_sub(lines.len()));
        retained.extend(lines.into_iter().map(|line| {
            line.prepend_segment(indent.as_str(), ThemeColor::Dim)
                .fit(width)
        }));
    }
    retained.finish()
}

fn render_thinking(
    entry: &TranscriptBlock,
    text: &str,
    expanded: bool,
    width: u16,
    expansion_key_hint: Option<&str>,
    row_budget: usize,
) -> BlockRender {
    let mut retained = TailBuffer::new(row_budget);
    let hint = if entry.is_collapsible() {
        expansion_hint(expanded, expansion_key_hint)
    } else {
        String::new()
    };
    let title = if entry.status == BlockStatus::Running {
        StyledLine::segments([
            (String::from("∴"), ThemeColor::Running),
            (format!(" Thinking…{hint}"), ThemeColor::Dim),
        ])
        .fit(width)
    } else {
        StyledLine::new(format!("∴ Thinking{hint}"), ThemeColor::Dim).fit(width)
    };
    retained.push(title);
    if expanded && !text.trim().is_empty() {
        let body_width = block_body_width(width);
        let markdown = render_markdown_tail(text.trim(), body_width, ThemeColor::Dim, row_budget);
        retained.omit_rows(markdown.omitted_rows);
        let prefix = block_indent();
        retained.extend(markdown.lines.into_iter().map(|line| {
            line.prepend_segment(prefix.as_str(), ThemeColor::Dim)
                .fit(width)
        }));
    }
    retained.finish()
}

const INPUT_PREVIEW_ROWS: usize = 3;

fn render_output_header(entry: &TranscriptBlock, width: u16, expanded: bool) -> Vec<StyledLine> {
    let indent = block_indent();
    let body_width = block_body_width(width) as usize;
    let dot_color = status_color(entry.status);
    let header = match entry.input.as_deref().filter(|input| !input.is_empty()) {
        Some(input) => format!("{} {input}", entry.title),
        None => entry.title.clone(),
    };
    let (mut title_lines, omitted_rows) =
        if !expanded && entry.is_collapsible() && entry.input.is_some() {
            let (lines, total) = wrap_head(&header, body_width, INPUT_PREVIEW_ROWS);
            (lines, total.saturating_sub(INPUT_PREVIEW_ROWS))
        } else {
            (wrap(&header, body_width), 0)
        };
    if omitted_rows > 0 {
        let closing = if entry
            .input
            .as_deref()
            .is_some_and(|input| input.starts_with('{') && input.ends_with('}'))
        {
            "}"
        } else {
            ""
        };
        let preview = format!(
            "{} … {omitted_rows} more lines{closing}",
            title_lines.join("\n").trim_end()
        );
        title_lines = wrap(&preview, body_width);
    }
    let annotation_start = (omitted_rows > 0).then(|| {
        title_lines
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, line)| line.rfind('…').map(|offset| (index, offset)))
            .expect("truncated input has an omission marker")
    });
    let suffix = entry.title_suffix.as_deref();
    let append_suffix = suffix.is_some_and(|suffix| {
        title_lines.last().is_some_and(|line| {
            display_width(line).saturating_add(display_width(suffix)) <= body_width
        })
    });
    let last_index = title_lines.len().saturating_sub(1);
    let mut lines = Vec::new();

    for (index, line) in title_lines.drain(..).enumerate() {
        let dim_offset = annotation_start
            .and_then(|(start_line, offset)| {
                if index == start_line {
                    Some(offset)
                } else if index > start_line {
                    Some(0)
                } else {
                    None
                }
            })
            .unwrap_or(line.len());
        let dim_end = if annotation_start.is_some() && index == last_index && line.ends_with('}') {
            line.len() - 1
        } else {
            line.len()
        };
        let mut segments = vec![
            (line[..dim_offset].to_string(), ThemeColor::Title),
            (line[dim_offset..dim_end].to_string(), ThemeColor::Dim),
            (line[dim_end..].to_string(), ThemeColor::Title),
        ];
        if append_suffix && index == last_index {
            segments.push((suffix.unwrap_or_default().to_string(), ThemeColor::Dim));
        }
        let title = StyledLine::segments(segments);
        let rendered = if index == 0 {
            title.prepend_styled_segment(BLOCK_PREFIX, dot_color, false, false, false)
        } else {
            title.prepend_segment(indent.as_str(), ThemeColor::Title)
        };
        lines.push(rendered.fit(width));
    }
    if let Some(suffix) = suffix.filter(|_| !append_suffix) {
        lines.extend(
            wrap(suffix.trim_start(), body_width)
                .into_iter()
                .map(|line| {
                    StyledLine::new(line, ThemeColor::Dim)
                        .prepend_segment(indent.as_str(), ThemeColor::Title)
                        .fit(width)
                }),
        );
    }
    lines
}

fn status_color(status: BlockStatus) -> ThemeColor {
    match status {
        BlockStatus::Success => ThemeColor::Success,
        BlockStatus::Error => ThemeColor::Error,
        BlockStatus::Running => ThemeColor::Running,
        BlockStatus::Idle => ThemeColor::Dim,
    }
}

fn output_prefix() -> String {
    format!("{}└╴ ", block_indent())
}

fn collapsed_output(output: &str, expansion_key_hint: Option<&str>) -> String {
    let line_count = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    let prefix = output_prefix();
    if line_count == 0 {
        return format!("{prefix}done");
    }
    let suffix = if line_count == 1 { "line" } else { "lines" };
    format!(
        "{prefix}+{line_count} {suffix}{}",
        expansion_hint(false, expansion_key_hint)
    )
}

fn expansion_hint(expanded: bool, key_hint: Option<&str>) -> String {
    let action = if expanded { "collapse" } else { "expand" };
    key_hint.map_or_else(
        || format!(" ({action})"),
        |key| format!(" ({key} to {action})"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_status_uses_running_color() {
        for theme in [crate::Theme::dark(), crate::Theme::light()] {
            assert_eq!(
                theme.color(status_color(BlockStatus::Running)),
                theme.color(ThemeColor::Warning)
            );
        }
    }

    #[test]
    fn collapsed_user_text_uses_render_width() {
        let row = "x".repeat(38);
        let text = [&row, &row, &row, &row].map(String::as_str).join(" ");
        let mut block = TranscriptBlock::collapsible_text(text.as_str());
        assert!(block.set_expanded(false));

        let rendered = render_block(
            TranscriptBlockView {
                role: BlockRole::User,
                ..transcript_item(1, &block)
            },
            40,
            Some("ctrl+o"),
            usize::MAX,
        );

        assert_eq!(rendered.lines.len(), 4);
        assert_eq!(rendered.lines[0].text.trim_end(), format!("> {row}"));
        assert_eq!(rendered.lines[2].text.trim_end(), format!("  {row}"));
        assert!(rendered.lines[3].text.contains("└╴ +1 line"));
        assert!(rendered.lines[3].text.contains("ctrl+o to expand"));
    }

    #[test]
    fn transcript_toggles_all_collapsible_blocks() {
        let mut transcript = Transcript {
            groups: vec![TranscriptGroup::new(
                BlockRole::User,
                vec![
                    TranscriptBlock::text("hello"),
                    TranscriptBlock::output("tool"),
                ],
            )],
            expansion_key_hint: None,
        };

        assert!(transcript.toggle_all_expansions());
        assert!(
            transcript.groups[0]
                .blocks
                .iter()
                .all(TranscriptBlock::is_expanded)
        );
        assert!(transcript.toggle_all_expansions());
        assert!(transcript.groups[0].blocks[0].is_expanded());
        assert!(!transcript.groups[0].blocks[1].is_expanded());
    }

    #[test]
    fn buffer_membership_uses_expanded_bytes_even_when_collapsed() {
        let mut blocks = (0..4)
            .map(|_| TranscriptBlock::output("tool"))
            .collect::<Vec<_>>();
        for block in &mut blocks {
            block.body = BlockBody::Output("first\nsecond".into());
        }
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 80,
            row_budget: 7,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: Some("ctrl+o"),
            text_expansion_row_limit: None,
        };
        let cost = render_block_expansion(
            transcript_item(0, &blocks[0]),
            80,
            Some("ctrl+o"),
            100,
            true,
        )
        .footprint(80);
        let separator = line_footprint(&block_separator(80), 80);
        tail.set_history_budget(HistoryBudget {
            terminal_bytes: cost.terminal_bytes * 2 + separator.terminal_bytes,
            ..HistoryBudget::default()
        });
        let collapsed = tail.materialize(transcript_items(&blocks), request);
        assert_eq!(collapsed.first_block, Some(2));
        let cached_layouts = tail.cache.entries.len();
        for block in &mut blocks {
            block.set_expanded(true);
        }
        let expanded = tail.materialize(
            transcript_items(&blocks),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );
        assert_eq!(expanded.first_block, collapsed.first_block);
        assert_eq!(expanded.lines.len(), 7);
        assert_eq!(tail.cache.entries.len(), cached_layouts);
        assert_eq!(expanded.history_revision, collapsed.history_revision + 1);
        for block in &mut blocks {
            block.set_expanded(false);
        }
        let restored = tail.materialize(
            transcript_items(&blocks),
            TranscriptRenderRequest {
                projection_revision: 2,
                ..request
            },
        );
        assert_eq!(restored.first_block, collapsed.first_block);
        assert_eq!(restored.lines, collapsed.lines);
        assert_eq!(restored.footprint, collapsed.footprint);
        assert_eq!(expanded.footprint, collapsed.footprint);
        assert_eq!(restored.history_revision, expanded.history_revision + 1);
        assert_eq!(tail.cache.entries.len(), cached_layouts);
        assert!(tail.cache.bytes <= tail.cache.budget);
    }

    #[test]
    fn large_expansion_keeps_history_beyond_the_paint_tail() {
        let mut block = TranscriptBlock::output("tool");
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 40,
            row_budget: 5,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };

        let first = tail.materialize(std::iter::once(transcript_item(1, &block)), request);
        assert_eq!(first.logical_origin, 0);

        block.body = BlockBody::Diff(DiffView::from_prefixed_text(
            &(0..1_000)
                .map(|index| format!("+line {index}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ));
        block.set_expanded(true);
        let second = tail.materialize(
            std::iter::once(TranscriptBlockView {
                id: 1,
                layout_revision: 1,
                content_epoch: 0,
                role: BlockRole::Event,
                starts_group: true,
                block: &block,
            }),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );

        assert_eq!(
            second.logical_origin,
            second.history.rows() - request.row_budget
        );
        assert!(second.logical_origin > 0);
        assert_eq!(second.history_revision, first.history_revision + 1);
    }

    #[test]
    fn transcript_tail_reuses_unchanged_projection_and_invalidates_changed_block() {
        let mut blocks = vec![styled_block("one")];
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 20,
            row_budget: 2,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };
        let first = tail.materialize(transcript_items(&blocks), request);
        let cached = tail.materialize(
            std::iter::once_with(|| panic!("cached render consumed transcript items")),
            request,
        );
        assert_eq!(line_texts(&cached.lines), line_texts(&first.lines));

        blocks[0].body = BlockBody::Styled(vec![StyledLine::new("two", ThemeColor::Default)]);
        let updated = tail.materialize(
            blocks.iter().map(|block| TranscriptBlockView {
                id: 0,
                layout_revision: 1,
                content_epoch: 1,
                role: BlockRole::Event,
                starts_group: true,
                block,
            }),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );
        assert_eq!(line_texts(&updated.lines), ["two"]);
        assert_eq!(tail.cache.entries.len(), 1);
        assert!(
            tail.cache
                .entries
                .keys()
                .all(|key| key.layout_revision == 1)
        );
    }

    #[test]
    fn oversized_source_is_rejected_before_layout_without_pinning_old_history() {
        let blocks = vec![
            styled_block("old"),
            TranscriptBlock::text("x".repeat(1_000)),
        ];
        let mut tail = TranscriptTail::default();
        tail.set_history_budget(HistoryBudget {
            resident_bytes: 128,
            ..HistoryBudget::default()
        });
        let rendered = tail.materialize(
            transcript_items(&blocks),
            TranscriptRenderRequest {
                width: 80,
                row_budget: 24,
                projection_revision: 0,
                theme_revision: 0,
                expansion_key_hint: None,
                text_expansion_row_limit: None,
            },
        );
        assert!(rendered.lines.is_empty());
        assert_eq!(rendered.first_block, None);
        assert_eq!(rendered.retire_before, Some(1));
        assert!(tail.cache.entries.is_empty());
    }

    #[test]
    fn editing_history_outside_the_paint_tail_requests_replacement() {
        let mut blocks = vec![styled_block("old prefix"), styled_block("tail")];
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 80,
            row_budget: 1,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };
        let first = tail.materialize(transcript_items(&blocks), request);
        blocks[0] = styled_block("new prefix");
        let updated = tail.materialize(
            blocks
                .iter()
                .enumerate()
                .map(|(index, block)| TranscriptBlockView {
                    layout_revision: u64::from(index == 0),
                    ..transcript_item(index as u64, block)
                }),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );
        assert_eq!(first.lines, updated.lines);
        assert_eq!(updated.history_revision, first.history_revision + 1);
    }

    #[test]
    fn retained_boundary_uses_display_order_not_numeric_block_ids() {
        let blocks = [
            (90, styled_block("retired prefix")),
            (20, styled_block("retained first")),
            (5, styled_block("older notification id")),
            (30, styled_block("newest output")),
        ];
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 80,
            row_budget: 20,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };
        let first = tail.materialize(
            blocks[1..]
                .iter()
                .map(|(id, block)| transcript_item(*id, block)),
            request,
        );
        let next = tail.materialize(
            blocks.iter().map(|(id, block)| transcript_item(*id, block)),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );
        assert_eq!(next.first_block, Some(20));
        assert_eq!(next.history_revision, first.history_revision);
        assert_eq!(next.lines, first.lines);
    }

    #[test]
    fn overflow_evicts_in_batches_and_reuses_unchanged_blocks_without_layout_cache() {
        let mut blocks = (0..20).map(|_| styled_block("line")).collect::<Vec<_>>();
        let mut tail = TranscriptTail::default();
        let row_bytes = line_footprint(&block_separator(80), 80).terminal_bytes;
        let budget = HistoryBudget {
            terminal_bytes: row_bytes * 39,
            ..HistoryBudget::default()
        };
        tail.set_history_budget(budget);
        let request = TranscriptRenderRequest {
            width: 80,
            row_budget: 4,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };
        let initial = tail.materialize(transcript_items(&blocks), request);
        assert_eq!(initial.first_block, Some(0));
        let unchanged = tail.retained_blocks.last().unwrap().lines.clone();
        tail.cache = LayoutCache {
            budget: 1,
            ..LayoutCache::default()
        };
        blocks.push(styled_block("line"));
        let evicted = tail.materialize(
            transcript_items(&blocks),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );
        assert!(evicted.first_block.unwrap() > 1);
        assert!(budget.eviction_target().fits(evicted.footprint));
        assert!(Arc::ptr_eq(
            &unchanged,
            &tail.retained_blocks[tail.retained_blocks.len() - 2].lines
        ));
        blocks.push(styled_block("line"));
        let appended = tail.materialize(
            transcript_items(&blocks),
            TranscriptRenderRequest {
                projection_revision: 2,
                ..request
            },
        );
        assert_eq!(appended.first_block, evicted.first_block);
        assert_eq!(appended.history_revision, evicted.history_revision);
        assert_eq!(appended.lines.len(), request.row_budget);
        assert!(budget.fits(appended.footprint));
    }

    #[test]
    fn content_epoch_resets_incremental_markdown_source() {
        let mut block = TranscriptBlock::text("first paragraph\n\ntail");
        let mut tail = TranscriptTail::default();
        let request = TranscriptRenderRequest {
            width: 40,
            row_budget: 20,
            projection_revision: 0,
            theme_revision: 0,
            expansion_key_hint: None,
            text_expansion_row_limit: None,
        };
        tail.materialize(
            std::iter::once(TranscriptBlockView {
                id: 1,
                layout_revision: 0,
                content_epoch: 0,
                role: BlockRole::Assistant,
                starts_group: true,
                block: &block,
            }),
            request,
        );

        block.body = BlockBody::Text("replacement text\n\nnext".into());
        let replaced = tail.materialize(
            std::iter::once(TranscriptBlockView {
                id: 1,
                layout_revision: 1,
                content_epoch: 1,
                role: BlockRole::Assistant,
                starts_group: true,
                block: &block,
            }),
            TranscriptRenderRequest {
                projection_revision: 1,
                ..request
            },
        );

        assert!(
            line_texts(&replaced.lines)
                .iter()
                .any(|line| line.ends_with("replacement text"))
        );
        assert!(
            line_texts(&replaced.lines)
                .iter()
                .all(|line| !line.contains("first paragraph"))
        );
    }

    #[test]
    fn block_rendering_respects_row_budget() {
        let assistant = render_assistant(
            "```text\none\ntwo\nthree\nfour\nfive\n```",
            40,
            PartFlow::Start,
            3,
        );
        assert!(assistant.lines.len() <= 3);
        assert!(assistant.omitted_rows > 0);

        let mut output = TranscriptBlock::output("tool");
        output.set_expanded(true);
        let output_text = (0..1_000)
            .map(|index| format!("output {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rendered = render_output(&output, &output_text, true, 40, None, 5);
        assert!(rendered.omitted_rows > 0);
        assert_eq!(rendered.lines.len(), 5);

        let diff_text = (0..1_000)
            .map(|index| format!("+line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = DiffView::from_prefixed_text(&diff_text);
        let rendered = render_diff(&output, &diff, true, 40, 5);
        assert!(rendered.omitted_rows > 0);
        assert_eq!(rendered.lines.len(), 5);
    }

    #[test]
    fn split_input_preserves_layout_and_survives_result_changes() {
        let mut entry = TranscriptBlock::output("Tool");
        entry.input = Some("  first\n第二行 long input".into());
        let header = render_output_header(&entry, 16, true);
        let mut legacy = TranscriptBlock::output("Tool   first\n第二行 long input");
        for suffix in [None, Some(" (timeout 20s)".to_string())] {
            let mut split = entry.clone();
            split.title_suffix = suffix.clone();
            legacy.title_suffix = suffix;
            for width in [4, 8, 16, 32, 80] {
                assert_eq!(
                    render_output_header(&split, width, true),
                    render_output_header(&legacy, width, true)
                );
            }
        }
        assert!(header.iter().all(|line| display_width(&line.text) <= 16));
        assert_eq!(
            source_bytes(&entry),
            entry.title.len() + entry.input.as_ref().unwrap().len()
        );
        let diff = DiffView::from_prefixed_text("+result");
        for expanded in [false, true] {
            let output = render_output(&entry, "result", expanded, 16, None, usize::MAX);
            assert_eq!(&output.lines[..header.len()], header.as_slice());
            let output = render_diff(&entry, &diff, expanded, 16, usize::MAX);
            assert_eq!(&output.lines[..header.len()], header.as_slice());
        }
        let bounded = render_output(&entry, "result", true, 16, None, 2);
        assert_eq!(bounded.lines.len(), 2);
        assert!(bounded.omitted_rows > 0);
    }

    #[test]
    fn long_input_shares_expansion_with_output_even_before_output_arrives() {
        let mut entry = TranscriptBlock::output("Codemode");
        let input = format!(
            "{{{}return value;}}",
            "const value = '第二行';\n".repeat(12)
        );
        entry.input = Some(input.clone());
        for width in [16, 40, 80] {
            for output in ["", "script result"] {
                let collapsed =
                    render_output(&entry, output, false, width, Some("ctrl+o"), usize::MAX);
                let header = render_output_header(&entry, width, false);
                assert_eq!(&collapsed.lines[..header.len()], header.as_slice());
                let text = line_texts(&collapsed.lines).join("\n");
                let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(compact.contains(" more lines}"));
                assert!(!text.contains("input rows"));
                let mut annotation_seen = false;
                for segment in header.iter().flat_map(|line| &line.segments) {
                    annotation_seen |= segment.text().contains('…');
                    if annotation_seen && !segment.text().trim().is_empty() {
                        if segment.text() == "}" {
                            assert_eq!(segment.color(), ThemeColor::Title);
                        } else {
                            assert!(!segment.text().contains('}'));
                            assert_eq!(segment.color(), ThemeColor::Dim);
                        }
                    }
                }
                assert!(annotation_seen);
                assert_eq!(
                    text.matches("ctrl+o").count(),
                    usize::from(!output.is_empty())
                );
                assert!(header.iter().any(|line| line.text.contains("第二行'; …")) || width == 16);
                assert!(!text.contains("return value;"));
                let expanded =
                    render_output(&entry, output, true, width, Some("ctrl+o"), usize::MAX);
                let text = line_texts(&expanded.lines).join("\n");
                let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(compact.contains("return value;"));
                assert!(!text.contains("more lines"));
                assert!(
                    expanded
                        .lines
                        .iter()
                        .all(|line| display_width(&line.text) <= width as usize)
                );
                assert_eq!(entry.input.as_deref(), Some(input.as_str()));
            }
        }
    }

    #[test]
    fn output_sections_bound_unicode_previews_without_changing_collapsed_source() {
        for text in [
            "a".repeat(100),
            "a".repeat(101),
            "界".repeat(101),
            "line\n".repeat(100),
            format!("{{\"result\":\"{}\"}}\n", "content ".repeat(100)),
        ] {
            let sections = vec![
                OutputSection {
                    text: "==> text 1/1 <==\n".into(),
                    preview_chars: None,
                },
                OutputSection {
                    text: text.clone().into(),
                    preview_chars: Some(100),
                },
                OutputSection {
                    text: "\n[Full output: /tmp/output.txt]".into(),
                    preview_chars: None,
                },
            ];
            let original = format!("==> text 1/1 <==\n{text}\n[Full output: /tmp/output.txt]");
            assert_eq!(render_output_sections(&sections, false), original);
            let expanded = render_output_sections(&sections, true);
            let expected = if text.chars().count() > 100 {
                let closing = if text.trim().starts_with('{') && text.trim().ends_with('}') {
                    "}"
                } else {
                    ""
                };
                format!(
                    "{} ...{closing}\n",
                    text.chars().take(100).collect::<String>().trim_end()
                )
            } else {
                text
            };
            assert_eq!(
                expanded,
                format!("==> text 1/1 <==\n{expected}\n[Full output: /tmp/output.txt]")
            );
        }
    }

    #[test]
    fn expanded_tool_output_keeps_wrapped_json_and_unicode_indented() {
        let entry = TranscriptBlock::output("Agent");
        let output = format!("{{\"taskId\":\"{}\"}}\n第二行", "x".repeat(100));
        let lines = render_output(&entry, &output, true, 32, None, usize::MAX).lines;
        assert!(lines[1].text.starts_with(&output_prefix()));
        let continuation = " ".repeat(display_width(&output_prefix()));
        assert!(
            lines
                .iter()
                .skip(2)
                .all(|line| line.text.starts_with(&continuation))
        );
        assert!(lines.iter().all(|line| display_width(&line.text) <= 32));
        let reconstructed = lines
            .iter()
            .skip(1)
            .map(|line| {
                line.text
                    .trim_end()
                    .chars()
                    .skip(continuation.len())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(reconstructed, output.replace('\n', ""));
    }

    #[test]
    fn warning_logs_wrap_without_truncation_and_keep_warning_color() {
        let text = "WARN mcp: deepwiki discovery failed: Unexpected response type";
        let lines = render_log(text, 32, usize::MAX).lines;
        let rendered = lines
            .iter()
            .map(|line| line.text.trim_end())
            .collect::<Vec<_>>()
            .join(" ");

        assert_eq!(rendered, text);
        assert!(lines.iter().all(|line| line.fg == ThemeColor::Warning));
    }

    #[test]
    fn transcript_composition_puts_one_blank_row_between_every_block() {
        let user = TranscriptBlock::text("user");
        let thinking = TranscriptBlock {
            title: String::new(),
            title_suffix: None,
            input: None,
            body: BlockBody::Thinking("reasoning".into()),
            footer: None,
            expansion: BlockExpansion::Collapsed,
            status: BlockStatus::Success,
            indent: 0,
        };
        let mut output = TranscriptBlock::output("Read");
        output.input = Some("{README.md}".into());
        output.body = BlockBody::Output("result".into());
        let blocks = [user, thinking, output];
        let roles = [BlockRole::User, BlockRole::Event, BlockRole::Event];
        let lines = render_transcript(
            blocks
                .iter()
                .zip(roles)
                .enumerate()
                .map(|(index, (block, role))| TranscriptBlockView {
                    id: index as u64,
                    layout_revision: 0,
                    content_epoch: 0,
                    role,
                    starts_group: true,
                    block,
                }),
            60,
            Some("ctrl+o"),
        );

        assert_eq!(
            line_texts(&lines),
            [
                "> user",
                "",
                "∴ Thinking (ctrl+o to expand)",
                "",
                "⏺  Read {README.md}",
                "   └╴ +1 line (ctrl+o to expand)",
            ]
        );
    }

    fn styled_block(text: &str) -> TranscriptBlock {
        TranscriptBlock {
            title: String::new(),
            title_suffix: None,
            input: None,
            body: BlockBody::Styled(vec![StyledLine::new(text, ThemeColor::Default)]),
            footer: None,
            expansion: BlockExpansion::Fixed,
            status: BlockStatus::Idle,
            indent: 0,
        }
    }

    fn transcript_items(
        blocks: &[TranscriptBlock],
    ) -> impl DoubleEndedIterator<Item = TranscriptBlockView<'_>> {
        blocks
            .iter()
            .enumerate()
            .map(|(index, block)| transcript_item(index as u64, block))
    }

    fn transcript_item(id: u64, block: &TranscriptBlock) -> TranscriptBlockView<'_> {
        TranscriptBlockView {
            id,
            layout_revision: 0,
            content_epoch: 0,
            role: BlockRole::Event,
            starts_group: true,
            block,
        }
    }

    fn line_texts(lines: &[StyledLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.trim_end()).collect()
    }
}
