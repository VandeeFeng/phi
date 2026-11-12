use std::collections::{HashMap, HashSet};

use crate::interactive::RewindAnnotations;
use crate::interactive::state::{MenuKind, PickerKind, PickerState, UiState};
use crate::session::discovery::{self, SessionSummary};
use crate::session::{AgentMessage, ContentBlock, Session, TranscriptEntryKind, UserContentBlock};
use phi_sdk::extension::memory::TapeAnchorType;
use phi_tui::{
    InputOutcome, KeyInput, NotifyKind, PickerAction, SelectItem, StyledLine, ThemeColor,
    text::display_width,
};

use crate::interactive::components::input::UiAction;
use crate::runtime::attachment::AgentAttachment;
use crate::runtime::selection::SessionSelection;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::interactive) struct SessionFeatures {
    pub(in crate::interactive) deletion: bool,
}

impl SessionFeatures {
    pub(in crate::interactive) fn action(self, action: &UiAction) -> Option<bool> {
        match action {
            UiAction::DeletePick(value) if value.starts_with("resume:") => Some(self.deletion),
            _ => None,
        }
    }
}

pub(in crate::interactive) struct SessionDeletion {
    sessions: Arc<crate::runtime::SessionAssociations>,
}

impl SessionDeletion {
    pub(in crate::interactive) fn new(sessions: Arc<crate::runtime::SessionAssociations>) -> Self {
        Self { sessions }
    }
}

pub(in crate::interactive) struct SessionPicker {
    pub(in crate::interactive) selection: SessionSelection,
    deletion: Option<SessionDeletion>,
}

impl SessionPicker {
    pub(in crate::interactive) fn new(selection: SessionSelection) -> Self {
        Self {
            selection,
            deletion: None,
        }
    }

    pub(in crate::interactive) fn with_deletion(mut self, deletion: SessionDeletion) -> Self {
        self.deletion = Some(deletion);
        self
    }

    pub(in crate::interactive) fn features(&self) -> SessionFeatures {
        SessionFeatures {
            deletion: self.deletion.is_some(),
        }
    }

    pub(in crate::interactive) async fn open(
        &self,
        attachment: &AgentAttachment,
        query: &str,
        state: &mut UiState,
        session: &Session,
    ) {
        match self.selection.choices(attachment, query).await {
            Ok(summaries) => show_resume_picker(
                state,
                resume_picker_from_summaries(&summaries, session.id.as_str()),
            ),
            Err(error) => state.notify(
                NotifyKind::Error,
                format!("failed to list Sessions: {error}"),
            ),
        }
    }

    pub(in crate::interactive) async fn delete(
        &self,
        id: &str,
        attachment: &AgentAttachment,
        state: &mut UiState,
        session: &Session,
    ) {
        let Some(deletion) = &self.deletion else {
            state.notify(
                NotifyKind::Warning,
                "Session deletion is unavailable in this interface.",
            );
            return;
        };
        if id == session.id.as_str() {
            state.notify(NotifyKind::Warning, "cannot delete current session");
            return;
        }
        match deletion.sessions.delete_session(id) {
            Ok(()) => self.open(attachment, "", state, session).await,
            Err(error) => {
                state.notify(NotifyKind::Error, format!("session delete failed: {error}"))
            }
        }
    }
}

pub(crate) const RESUME_PICKER_TITLE: &str = "resume session · enter to resume · ctrl+d delete";
pub(crate) const RESUME_PICKER_MAX_VISIBLE: usize = 10;
const REWIND_PREVIEW_CHARS: usize = 72;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResumePickerAction {
    None,
    Selected(String),
    Cancelled,
    Delete(String),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ResumeDeleteOutcome {
    Deleted,
    Current,
}

pub(crate) fn resume_picker(query: &str, current_id: &str) -> anyhow::Result<PickerState> {
    let sessions = if query.is_empty() {
        discovery::find_recent_sessions(50)?
    } else {
        discovery::find_sessions_by_prefix(query)?
    };
    Ok(resume_picker_from_summaries(&sessions, current_id))
}

pub(crate) fn resume_picker_from_summaries(
    sessions: &[SessionSummary],
    current_id: &str,
) -> PickerState {
    PickerState::with_kind(
        PickerKind::Resume,
        RESUME_PICKER_TITLE,
        session_items(sessions, current_id),
    )
}

pub(crate) fn handle_resume_picker_input(
    picker: &mut PickerState,
    confirming_delete: &mut Option<String>,
    key: &KeyInput,
) -> ResumePickerAction {
    if let Some(action) = confirm_delete_action(picker, confirming_delete, key) {
        return action;
    }
    if key == &KeyInput::Ctrl('d') {
        return begin_delete_confirmation(picker, confirming_delete);
    }
    match picker.handle_input_with_filter(key, |items, _| items.to_vec()) {
        InputOutcome::Action(PickerAction::Cancelled) => ResumePickerAction::Cancelled,
        InputOutcome::Action(PickerAction::Selected(value)) => ResumePickerAction::Selected(value),
        InputOutcome::Consumed | InputOutcome::Ignored => ResumePickerAction::None,
    }
}

fn confirm_delete_action(
    picker: &mut PickerState,
    confirming_delete: &mut Option<String>,
    key: &KeyInput,
) -> Option<ResumePickerAction> {
    let value = confirming_delete.clone()?;
    Some(match key {
        KeyInput::Enter => ResumePickerAction::Delete(value),
        KeyInput::Esc | KeyInput::Ctrl('c') => {
            *confirming_delete = None;
            picker.title = String::from(RESUME_PICKER_TITLE);
            ResumePickerAction::None
        }
        _ => ResumePickerAction::None,
    })
}

fn begin_delete_confirmation(
    picker: &mut PickerState,
    confirming_delete: &mut Option<String>,
) -> ResumePickerAction {
    let Some(item) = picker.selected_item() else {
        return ResumePickerAction::None;
    };
    *confirming_delete = Some(item.value.clone());
    picker.title = String::from("delete session? enter confirm · esc/ctrl+c cancel");
    ResumePickerAction::None
}

pub(crate) fn delete_resume_candidate(
    id: &str,
    current_id: &str,
) -> anyhow::Result<ResumeDeleteOutcome> {
    if !current_id.is_empty() && id == current_id {
        return Ok(ResumeDeleteOutcome::Current);
    }
    crate::session::storage::delete_session(id)?;
    Ok(ResumeDeleteOutcome::Deleted)
}

pub(crate) fn show_resume_picker(state: &mut UiState, mut picker: PickerState) {
    if !state.features.sessions.deletion {
        picker.title = String::from("resume session · enter to resume");
    }
    if picker.items.is_empty() {
        state.notify(NotifyKind::Info, "no matching sessions");
    } else {
        state.open_picker(MenuKind::Other, picker);
        state.runtime.request_render();
    }
}

fn session_items(sessions: &[SessionSummary], current_id: &str) -> Vec<SelectItem> {
    sessions
        .iter()
        .map(|session| {
            let current = if session.id == current_id {
                "current · "
            } else {
                ""
            };
            let short_id = session.id.chars().take(8).collect::<String>();
            SelectItem::with_label(
                format!("resume:{}", session.id),
                format!("{}{}", current, session_label(session)),
                format!("{} · {}", short_time(session.updated_at.as_str()), short_id),
            )
        })
        .collect()
}

pub(crate) fn session_label(session: &SessionSummary) -> String {
    let label = if !session.name.is_empty() {
        session.name.to_string()
    } else {
        session
            .first_user_prompt
            .as_deref()
            .and_then(prompt_text)
            .unwrap_or_else(|| String::from("empty session"))
    };
    label.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn prompt_text(text: &str) -> Option<String> {
    if let Some(block) = crate::resources::prompts::skills::parse_block(text) {
        return Some(format!("/skill:{}", block.name));
    }
    if let Some(block) = crate::resources::prompts::files::parse_custom_command_block(text) {
        return Some(format!("/{}", block.name));
    }
    (!text.trim().is_empty()).then(|| text.chars().take(48).collect())
}

pub(crate) fn short_time(value: &str) -> String {
    value
        .split_once('T')
        .map(|(date, time)| format!("{} {}", date, time.chars().take(5).collect::<String>()))
        .unwrap_or_else(|| value.to_string())
}

pub(crate) fn open_rewind_picker(
    state: &mut UiState,
    session: &Session,
    annotations: &RewindAnnotations,
) {
    let items = rewind_items(session, annotations);
    if items.is_empty() {
        state.notify(NotifyKind::Info, "no rewind points");
        return;
    }
    state.open_picker(
        MenuKind::Other,
        PickerState::with_kind(
            PickerKind::Rewind,
            "rewind conversation · / search · enter restore · esc cancel",
            items,
        ),
    );
    state.runtime.request_render();
}

fn rewind_items(session: &Session, annotations: &RewindAnnotations) -> Vec<phi_tui::SelectItem> {
    let active_path = active_path_ids(session);
    let anchor_labels = rewind_anchor_labels(session, annotations);
    let entries_by_id = session
        .entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect::<HashMap<_, _>>();
    flatten_rewind_tree(session, &active_path)
        .into_iter()
        .filter_map(|node| {
            let entry = entries_by_id.get(node.entry_id.as_str())?;
            let label = rewind_entry_label(session, &annotations.label_prefix, &entry.kind)?;
            let marker = active_path
                .contains(entry.id.as_str())
                .then_some("• ")
                .unwrap_or("");
            let label = rewind_item_label(&label, anchor_labels.get(entry.id.as_str()));
            Some(phi_tui::SelectItem::with_label(
                format!("rewind:{}", entry.id),
                format!("{}{}{}", rewind_tree_prefix(&node), marker, label),
                String::new(),
            ))
        })
        .collect()
}

#[derive(Clone)]
struct RewindFlatNode {
    entry_id: String,
    display_indent: usize,
    show_connector: bool,
    is_last: bool,
    foldable: bool,
    gutters: RewindGutters,
}

type RewindGutters = Vec<(usize, bool)>;

#[derive(Clone)]
struct RewindWalkState {
    indent: usize,
    just_branched: bool,
    show_connector: bool,
    is_last: bool,
    gutters: RewindGutters,
    is_virtual_root_child: bool,
}

fn flatten_rewind_tree(session: &Session, active_path: &HashSet<String>) -> Vec<RewindFlatNode> {
    let children_by_parent = rewind_children_by_parent(session, active_path);
    let mut roots = session
        .entries
        .iter()
        .filter(|entry| entry.parent_id.is_none())
        .map(|entry| entry.id.to_string())
        .collect::<Vec<_>>();
    sort_active_first(&mut roots, active_path, &children_by_parent);

    let multiple_roots = roots.len() > 1;
    let mut flat = Vec::new();
    for (index, root) in roots.iter().enumerate() {
        append_rewind_flat_node(
            root,
            RewindWalkState {
                indent: multiple_roots.then_some(1).unwrap_or(0),
                just_branched: multiple_roots,
                show_connector: multiple_roots,
                is_last: index + 1 == roots.len(),
                gutters: Vec::new(),
                is_virtual_root_child: multiple_roots,
            },
            multiple_roots,
            session.leaf_id.as_deref(),
            &children_by_parent,
            &mut flat,
        );
    }
    flat
}

fn append_rewind_flat_node(
    entry_id: &str,
    state: RewindWalkState,
    multiple_roots: bool,
    current_leaf_id: Option<&str>,
    children_by_parent: &HashMap<String, Vec<String>>,
    flat: &mut Vec<RewindFlatNode>,
) {
    let children = children_by_parent
        .get(entry_id)
        .cloned()
        .unwrap_or_default();
    let foldable = !children.is_empty();
    let display_indent = if multiple_roots {
        state.indent.saturating_sub(1)
    } else {
        state.indent
    };
    flat.push(RewindFlatNode {
        entry_id: entry_id.to_string(),
        display_indent,
        show_connector: state.show_connector && !state.is_virtual_root_child,
        is_last: state.is_last,
        foldable,
        gutters: state.gutters.clone(),
    });

    let branches_here =
        children.len() > 1 || (current_leaf_id == Some(entry_id) && !children.is_empty());
    let child_indent = if branches_here || (state.just_branched && state.indent > 0) {
        state.indent + 1
    } else {
        state.indent
    };
    let child_gutters = if state.show_connector && !state.is_virtual_root_child {
        let mut next = state.gutters;
        next.push((display_indent.saturating_sub(1), !state.is_last));
        next
    } else {
        state.gutters
    };

    for (index, child) in children.iter().enumerate() {
        append_rewind_flat_node(
            child,
            RewindWalkState {
                indent: child_indent,
                just_branched: branches_here,
                show_connector: branches_here,
                is_last: index + 1 == children.len(),
                gutters: child_gutters.clone(),
                is_virtual_root_child: false,
            },
            multiple_roots,
            current_leaf_id,
            children_by_parent,
            flat,
        );
    }
}

fn rewind_children_by_parent(
    session: &Session,
    active_path: &HashSet<String>,
) -> HashMap<String, Vec<String>> {
    let mut children = HashMap::<String, Vec<String>>::new();
    for entry in &session.entries {
        if let Some(parent_id) = &entry.parent_id {
            children
                .entry(parent_id.to_string())
                .or_default()
                .push(entry.id.to_string());
        }
    }
    let snapshot = children.clone();
    for ids in children.values_mut() {
        sort_active_first(ids, active_path, &snapshot);
    }
    children
}

fn sort_active_first(
    ids: &mut [String],
    active_path: &HashSet<String>,
    children_by_parent: &HashMap<String, Vec<String>>,
) {
    ids.sort_by_key(|id| !subtree_contains_active(id, active_path, children_by_parent));
}

fn subtree_contains_active(
    id: &str,
    active_path: &HashSet<String>,
    children_by_parent: &HashMap<String, Vec<String>>,
) -> bool {
    active_path.contains(id)
        || children_by_parent.get(id).is_some_and(|children| {
            children
                .iter()
                .any(|child| subtree_contains_active(child, active_path, children_by_parent))
        })
}

fn rewind_tree_prefix(node: &RewindFlatNode) -> String {
    let connector_position = node
        .show_connector
        .then_some(node.display_indent.saturating_sub(1));
    let mut prefix = String::with_capacity(node.display_indent * 3);
    for index in 0..(node.display_indent * 3) {
        let level = index / 3;
        let pos = index % 3;
        if connector_position == Some(level) {
            prefix.push(match pos {
                0 if node.is_last => '└',
                0 => '├',
                1 if node.foldable => '⊟',
                1 => '─',
                _ => ' ',
            });
        } else if let Some((_, show)) = node.gutters.iter().find(|(position, _)| *position == level)
        {
            prefix.push(if pos == 0 && *show { '│' } else { ' ' });
        } else {
            prefix.push(' ');
        }
    }
    prefix
}

fn rewind_item_label(label: &str, anchor_label: Option<&String>) -> String {
    let Some(anchor_label) = anchor_label else {
        return label.to_string();
    };
    if label.starts_with(anchor_label) {
        label.to_string()
    } else {
        format!("{anchor_label} {label}")
    }
}

fn rewind_anchor_labels(
    session: &Session,
    annotations: &RewindAnnotations,
) -> HashMap<String, String> {
    let mut by_target = HashMap::<String, Vec<AnchorLabel>>::new();
    for anchor in &annotations.anchors {
        let Some(target) = resolve_anchor_label_target(
            session,
            &annotations.label_prefix,
            anchor.session_entry_id.as_str(),
        ) else {
            continue;
        };
        let anchor_type = anchor_type_label(anchor.anchor_type).to_string();
        by_target.entry(target).or_default().push(AnchorLabel {
            name: anchor_display_name(anchor.name.as_str(), anchor_type.as_str()),
            anchor_type,
        });
    }

    let label_prefix = &annotations.label_prefix;
    by_target
        .into_iter()
        .filter_map(|(entry_id, anchors)| {
            build_anchor_label(anchors, label_prefix).map(|label| (entry_id, label))
        })
        .collect()
}

#[derive(Clone, PartialEq, Eq)]
struct AnchorLabel {
    anchor_type: String,
    name: String,
}

fn build_anchor_label(anchors: Vec<AnchorLabel>, label_prefix: &str) -> Option<String> {
    if anchors.is_empty() {
        return None;
    }
    let mut unique = Vec::<AnchorLabel>::new();
    for anchor in anchors {
        if !unique.iter().any(|known| known == &anchor) {
            unique.push(anchor);
        }
    }
    let start = unique.len().saturating_sub(3);
    let suffix = if start > 0 {
        format!(" +{start}")
    } else {
        String::new()
    };
    let labels = unique[start..]
        .iter()
        .map(|anchor| format!("{}/{name}", anchor.anchor_type, name = anchor.name))
        .collect::<Vec<_>>()
        .join(" · ");
    Some(format!("[{label_prefix}{labels}{suffix}]"))
}

fn anchor_display_name(name: &str, anchor_type: &str) -> String {
    name.strip_prefix(&format!("{anchor_type}/"))
        .unwrap_or(name)
        .to_string()
}

fn anchor_type_label(anchor_type: TapeAnchorType) -> &'static str {
    match anchor_type {
        TapeAnchorType::Normal => "normal",
    }
}

fn resolve_anchor_label_target(
    session: &Session,
    label_prefix: &str,
    session_entry_id: &str,
) -> Option<String> {
    let root = session
        .entries
        .iter()
        .find(|entry| entry.id == session_entry_id)?;
    if rewind_entry_label(session, label_prefix, &root.kind).is_some() {
        return Some(root.id.to_string());
    }

    let mut child_map = HashMap::<String, Vec<String>>::new();
    for entry in &session.entries {
        let Some(parent_id) = entry.parent_id.as_deref() else {
            continue;
        };
        child_map
            .entry(parent_id.to_string())
            .or_default()
            .push(entry.id.to_string());
    }

    let mut queue = child_map.get(root.id.as_str()).cloned().unwrap_or_default();
    while let Some(entry_id) = queue.first().cloned() {
        queue.remove(0);
        let Some(entry) = session.entries.iter().find(|entry| entry.id == entry_id) else {
            continue;
        };
        if rewind_entry_label(session, label_prefix, &entry.kind).is_some() {
            return Some(entry.id.to_string());
        }
        queue.extend(
            child_map
                .get(entry.id.as_str())
                .cloned()
                .unwrap_or_default(),
        );
    }

    let mut current_id = root.parent_id.as_deref();
    while let Some(entry_id) = current_id {
        let Some(entry) = session.entries.iter().find(|entry| entry.id == entry_id) else {
            break;
        };
        if rewind_entry_label(session, label_prefix, &entry.kind).is_some() {
            return Some(entry.id.to_string());
        }
        current_id = entry.parent_id.as_deref();
    }

    Some(root.id.to_string())
}

fn active_path_ids(session: &Session) -> HashSet<String> {
    session
        .current_branch()
        .into_iter()
        .map(|entry| entry.id.to_string())
        .collect()
}

fn rewind_entry_label(
    session: &Session,
    label_prefix: &str,
    kind: &TranscriptEntryKind,
) -> Option<String> {
    if let TranscriptEntryKind::Failure { failure, .. } = kind {
        let message = failure.message.replace('\n', " ");
        return Some(format!(
            "error: {}",
            truncate_chars(message.trim(), REWIND_PREVIEW_CHARS, "…")
        ));
    }
    let TranscriptEntryKind::Message { message } = kind else {
        return None;
    };
    let (role, content) = match message {
        AgentMessage::User { content } => {
            return Some(format!("user: {}", summarize_user_blocks(content)));
        }
        AgentMessage::Context {
            source:
                phi_sdk::transcript::ContextSource::FileMention
                | phi_sdk::transcript::ContextSource::WorkflowIntro,
            ..
        } => return None,
        AgentMessage::Context { source, .. }
            if phi_sdk::extension::memory::MemoryContextKind::from_source(source)
                == Some(phi_sdk::extension::memory::MemoryContextKind::KeywordIntro) =>
        {
            return None;
        }
        AgentMessage::Context {
            source, content, ..
        } => (context_source_label(source), content),
        AgentMessage::Agent { content } => {
            let summary = summarize_blocks(content);
            return (!summary.is_empty()).then(|| format!("agent: {summary}"));
        }
        AgentMessage::Assistant { content, .. } => {
            let summary = summarize_blocks(content);
            return (!summary.is_empty()).then(|| format!("assistant: {summary}"));
        }
        AgentMessage::ToolResult {
            tool_call_id,
            tool_name,
            content,
            ..
        } => {
            return Some(format_tool_result(
                session,
                label_prefix,
                tool_call_id,
                tool_name,
                content,
            ));
        }
        AgentMessage::System { content } => return Some(format!("system: {content}")),
    };
    Some(format!("{role}: {}", summarize_blocks(content)))
}

fn context_source_label(source: &phi_sdk::transcript::ContextSource) -> String {
    match source {
        phi_sdk::transcript::ContextSource::FileMention => "file-context",
        phi_sdk::transcript::ContextSource::Hook => "hook-context",
        phi_sdk::transcript::ContextSource::WorkflowIntro => "workflow-intro",
        phi_sdk::transcript::ContextSource::Extension(key) => {
            return format!("{}/{}", key.name(), key.kind());
        }
    }
    .to_string()
}

fn format_tool_result(
    session: &Session,
    label_prefix: &str,
    tool_call_id: &str,
    fallback_name: &str,
    result_content: &[ContentBlock],
) -> String {
    for entry in &session.entries {
        let TranscriptEntryKind::Message {
            message: AgentMessage::Assistant { content, .. },
        } = &entry.kind
        else {
            continue;
        };
        for block in content {
            let ContentBlock::ToolCall { id, name, args, .. } = block else {
                continue;
            };
            if id == tool_call_id {
                return format_tool_call(name, label_prefix, args, result_content);
            }
        }
    }
    format!("[{fallback_name}]")
}

fn format_tool_call(
    name: &str,
    label_prefix: &str,
    args: &serde_json::Value,
    result_content: &[ContentBlock],
) -> String {
    let tool_call = format!("[{name}: {}]", summarize_tool_args(args));
    if name == "tape_anchor" {
        let anchor_label = tape_anchor_result_label(result_content, label_prefix)
            .unwrap_or_else(|| format_tape_anchor_label(args, label_prefix));
        return format!("{anchor_label}: {tool_call}");
    }
    tool_call
}

fn tape_anchor_result_label(content: &[ContentBlock], label_prefix: &str) -> Option<String> {
    let text = summarize_blocks(content);
    let value = serde_json::from_str::<serde_json::Value>(&text).ok()?;
    let name = value.get("name").and_then(|value| value.as_str())?;
    let anchor_type = value
        .get("type")
        .and_then(|value| value.as_str())
        .unwrap_or("anchor");
    let name = anchor_display_name(name, anchor_type);
    Some(format!("[{label_prefix}{anchor_type}/{name}]"))
}

fn format_tape_anchor_label(args: &serde_json::Value, label_prefix: &str) -> String {
    args.get("name")
        .and_then(|value| value.as_str())
        .map(|name| format!("[{label_prefix}normal/{name}]"))
        .unwrap_or_else(|| format!("[{label_prefix}normal/tape_anchor]"))
}

fn summarize_tool_args(args: &serde_json::Value) -> String {
    let text = args
        .get("path")
        .or_else(|| args.get("file_path"))
        .or_else(|| args.get("command"))
        .or_else(|| args.get("query"))
        .or_else(|| args.get("pattern"))
        .and_then(|value| value.as_str())
        .map(ToString::to_string)
        .unwrap_or_else(|| compact_arg_summary(args));
    let text = text.replace(['\n', '\t'], " ");
    let text = text.trim();
    truncate_chars(text, REWIND_PREVIEW_CHARS, "...")
}

fn truncate_chars(text: &str, max_chars: usize, suffix: &str) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}{suffix}")
    } else {
        truncated
    }
}

fn compact_arg_summary(args: &serde_json::Value) -> String {
    let Some(object) = args.as_object() else {
        return args.to_string();
    };
    object
        .iter()
        .next()
        .map(|(key, value)| match value.as_str() {
            Some(value) => format!("{key}: {value}"),
            None => format!("{key}: {value}"),
        })
        .unwrap_or_default()
}

fn summarize_user_blocks(blocks: &[UserContentBlock]) -> String {
    let summary = blocks
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text { text } => Some(text.as_str()),
            UserContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\n', " ");
    truncate_chars(summary.trim(), REWIND_PREVIEW_CHARS, "…")
}

fn summarize_blocks(blocks: &[ContentBlock]) -> String {
    let summary = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\n', " ");
    let summary = summary.trim();
    truncate_chars(summary, REWIND_PREVIEW_CHARS, "…")
}

pub(super) fn render_rewind_items(picker: &PickerState, width: u16) -> Vec<StyledLine> {
    let mut lines =
        vec![StyledLine::new(format!("  {}", picker.title), ThemeColor::Dim).fit(width)];
    if picker.items.is_empty() {
        lines.push(StyledLine::new("  no matching rewind points", ThemeColor::Dim).fit(width));
        return lines;
    }

    let selected = picker.menu.selected.min(picker.items.len() - 1);
    let range = phi_tui::centered_visible_range(selected, picker.items.len(), 8);
    for (offset, item) in picker.items[range.range()].iter().enumerate() {
        let row = range.start + offset;
        lines.push(render_rewind_item(item, row == selected, width));
    }
    lines.push(
        StyledLine::new(
            format!("  ({}/{})", selected + 1, picker.items.len()),
            ThemeColor::Dim,
        )
        .fit(width),
    );
    lines
}

fn render_rewind_item(item: &SelectItem, selected: bool, width: u16) -> StyledLine {
    let cursor = if selected { "› " } else { "  " };
    let (prefix, role, content) = split_rewind_label(&item.label);
    let role_color = match role {
        "user" => ThemeColor::Accent,
        "assistant" => ThemeColor::Success,
        "context" => ThemeColor::Muted,
        "" => ThemeColor::Muted,
        _ => ThemeColor::Dim,
    };
    let content_color = match role {
        "user" | "assistant" => ThemeColor::Default,
        _ => role_color,
    };
    let separator = if role.is_empty() { "" } else { ": " };
    let reserved = display_width(cursor)
        + display_width(prefix)
        + display_width(role)
        + display_width(separator)
        + 1;
    let content =
        phi_tui::text::truncate(content, (width as usize).saturating_sub(reserved).max(1));
    let line = StyledLine::segments([
        (
            cursor,
            if selected {
                ThemeColor::Accent
            } else {
                ThemeColor::Muted
            },
        ),
        (prefix, ThemeColor::Dim),
        (role, role_color),
        (separator, ThemeColor::Dim),
        (content.as_str(), content_color),
    ])
    .fit(width);
    if selected {
        line.bg(ThemeColor::SelectedBg)
    } else {
        line
    }
}

fn split_rewind_label(label: &str) -> (&str, &str, &str) {
    let Some((before, content)) = label.split_once(": ") else {
        let content_start = label.rfind('[').unwrap_or(0);
        return (&label[..content_start], "", &label[content_start..]);
    };
    let role_start = before.rfind(' ').map(|index| index + 1).unwrap_or(0);
    (&before[..role_start], &before[role_start..], content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_turn_is_a_selectable_rewind_point() {
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        session.append_user_message("inspect");
        let id = session
            .transcript
            .append_entry(TranscriptEntryKind::Failure {
                turn_id: Some("failed-turn".into()),
                failure: phi_sdk::PhiFailure::new(
                    phi_sdk::PhiFailureKind::Authentication,
                    "credentials\nrejected",
                    None,
                ),
            });
        let items = rewind_items(
            &session,
            &RewindAnnotations {
                anchors: Vec::new(),
                label_prefix: String::new(),
            },
        );
        assert!(items.iter().any(|item| item.value == format!("rewind:{id}")
            && item.label.contains("error: credentials rejected")));
    }

    #[test]
    fn rewind_render_handles_unicode_anchor_names() {
        for name in ["review", "审查", "phase─one", "阶段─一"] {
            let anchor = format!("[tape/normal/{name}]");
            let label = format!("{anchor}: [tape_anchor: {name}]");
            let item = SelectItem::with_label("rewind:anchor", &label, "");
            for width in 0..=80 {
                let line = render_rewind_item(&item, true, width);
                assert!(display_width(&line.text) <= usize::from(width));
                if width == 80 {
                    assert_eq!(line.text.trim_end(), format!("› {label}"));
                }
            }
        }
    }

    #[test]
    fn rewind_labels_keep_local_prefix_and_canonical_anchor_without_memory_settings() {
        let args = serde_json::json!({"name": "canonical-anchor"});
        let local = format_tool_call("tape_anchor", "local/", &args, &[]);
        let remote = format_tool_call("tape_anchor", "", &args, &[]);
        assert!(local.contains("local/"));
        assert!(!remote.contains("local/"));
        assert!(remote.contains("canonical-anchor"));
    }

    #[test]
    fn rewind_annotations_keep_target_mapping_dedup_and_canonical_tree() {
        let mut session =
            crate::session::new("local", "test", 1_000, &std::env::current_dir().unwrap());
        let hidden = session
            .transcript
            .append_entry(TranscriptEntryKind::Message {
                message: AgentMessage::Agent {
                    content: Vec::new(),
                },
            });
        session.transcript.append_user_message("visible");
        let visible = session.entries.last().unwrap().id.to_string();
        let trailing = session
            .transcript
            .append_entry(TranscriptEntryKind::Message {
                message: AgentMessage::Agent {
                    content: Vec::new(),
                },
            });
        assert_eq!(
            resolve_anchor_label_target(&session, "", hidden.as_str()),
            Some(visible.clone())
        );
        assert_eq!(
            resolve_anchor_label_target(&session, "", trailing.as_str()),
            Some(visible.clone())
        );
        assert!(resolve_anchor_label_target(&session, "", "missing").is_none());
        let anchors = ["one", "two", "three", "four", "four"]
            .into_iter()
            .map(|name| {
                let mut anchor =
                    phi_sdk::extension::memory::TapeAnchor::new(name, TapeAnchorType::Normal);
                anchor.session_entry_id = hidden.clone();
                anchor
            })
            .collect();
        let annotations = RewindAnnotations {
            anchors,
            label_prefix: "startup/".into(),
        };
        let items = rewind_items(&session, &annotations);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].value, format!("rewind:{visible}"));
        assert!(
            items[0]
                .label
                .contains("[startup/normal/two · normal/three · normal/four +1]")
        );
        let canonical = rewind_items(&session, &RewindAnnotations::default());
        assert_eq!(canonical[0].value, items[0].value);
        assert!(canonical[0].label.contains("user: visible"));
        assert!(!canonical[0].label.contains("startup/"));
    }

    #[test]
    fn resume_picker_maps_selection_cancellation_and_delete() {
        let mut picker = PickerState::with_kind(
            PickerKind::Resume,
            RESUME_PICKER_TITLE,
            vec![SelectItem::new("resume:session-id", "session")],
        );
        let mut confirming_delete = None;

        assert_eq!(
            handle_resume_picker_input(&mut picker, &mut confirming_delete, &KeyInput::Enter),
            ResumePickerAction::Selected(String::from("resume:session-id"))
        );
        assert_eq!(
            handle_resume_picker_input(&mut picker, &mut confirming_delete, &KeyInput::Esc),
            ResumePickerAction::Cancelled
        );

        handle_resume_picker_input(&mut picker, &mut confirming_delete, &KeyInput::Ctrl('d'));
        assert_eq!(confirming_delete.as_deref(), Some("resume:session-id"));
        assert_eq!(
            handle_resume_picker_input(&mut picker, &mut confirming_delete, &KeyInput::Esc),
            ResumePickerAction::None
        );
        assert_eq!(picker.title, RESUME_PICKER_TITLE);

        handle_resume_picker_input(&mut picker, &mut confirming_delete, &KeyInput::Ctrl('d'));
        assert_eq!(
            handle_resume_picker_input(&mut picker, &mut confirming_delete, &KeyInput::Enter),
            ResumePickerAction::Delete(String::from("resume:session-id"))
        );
    }
}
