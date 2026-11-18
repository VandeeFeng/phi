use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::extension::memory::graph::{
    MemoryGraph, MemoryGraphEdge, MemoryGraphEdgeKind, MemoryGraphNodeKind, MemoryGraphNodeView,
    MemoryGraphQuery, MemoryGraphSelection, MemoryPathStats, RankedMemoryGraphNode,
    analyze_path_access, anchor_node_id, project_file_node_id,
};
use crate::extension::memory::memory_core::{
    self, AnchorMode, MemoryContextMode, MemoryContextOptions,
};
use crate::extension::memory::service::{MemoryService, warm_memory_graph};
use crate::extension::memory::tape::TapeService;
use crate::extension::memory::tape::types::TapeScope;
use crate::extension::memory::utils::graph_value;
use crate::transcript::{
    AgentMessage, ContentBlock, Transcript, TranscriptEntry, TranscriptEntryKind, UserContentBlock,
};
use crate::utils::workspace::WorkspaceMeta;

use super::reader::get_entries_after_timestamp;
use super::types::TapeSessionScanOptions;

const CHARS_PER_TOKEN: usize = 4;
const MIN_SMART_ACCESS_SAMPLES: usize = 5;
pub const DEFAULT_FORMATTED_ENTRY_CONTENT_CHARS: usize = 300;

#[derive(Debug, Clone)]
pub struct TapeMessage {
    pub role: TapeMessageRole,
    pub content: String,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapeMessageRole {
    User,
    Agent,
    Assistant,
    System,
    Tool,
}

pub fn extract_message_content(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => {
                Some(text.as_str())
            }
            ContentBlock::Image { .. } | ContentBlock::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn extract_user_message_content(content: &[UserContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            UserContentBlock::Text { text } => Some(text.as_str()),
            UserContentBlock::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub fn format_entries_as_messages(entries: &[TranscriptEntry]) -> Vec<TapeMessage> {
    entries.iter().filter_map(entry_to_message).collect()
}

pub fn format_entry_line(
    entry: &TranscriptEntry,
    max_content_chars: Option<usize>,
) -> Option<String> {
    let time = to_locale_time(&entry.timestamp);

    match &entry.kind {
        TranscriptEntryKind::Message { message } => {
            let role = message_role_label(message);
            let content = message_content(message);
            let formatted_content = truncate_content(&content, max_content_chars);
            Some(format!("[{time}] {role}: {formatted_content}"))
        }
        TranscriptEntryKind::Event { name, .. } => Some(format!("[{time}] -- {name} --")),
        TranscriptEntryKind::ContextEdit { .. } => None,
        TranscriptEntryKind::Failure { failure, .. } => {
            let text = truncate_content(&failure.message, max_content_chars);
            Some(format!("[{time}] [Failure] {text}"))
        }
        TranscriptEntryKind::Compaction { summary, .. } => {
            let formatted_summary = truncate_content(summary, max_content_chars);
            Some(format!("[{time}] [Compaction] {formatted_summary}"))
        }
        TranscriptEntryKind::Reset { handoff } => {
            let text = truncate_content(handoff.as_deref().unwrap_or(""), max_content_chars);
            Some(format!("[{time}] [Reset] {text}"))
        }
    }
}

#[derive(Debug, Clone)]
pub struct SmartTapePrecompute {
    pub(crate) activity: HashMap<String, MemoryPathStats>,
    pub scan_hours: u64,
    pub(crate) warnings: Vec<String>,
}

impl SmartTapePrecompute {
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}

pub struct ConversationSelector {
    max_tokens: usize,
    max_entries: usize,
}

impl ConversationSelector {
    pub fn new(max_tokens: usize, max_entries: usize) -> Self {
        Self {
            max_tokens,
            max_entries,
        }
    }

    pub fn select_from_anchor(
        &self,
        entries: &[TranscriptEntry],
        anchor_timestamp: Option<&str>,
    ) -> Vec<TranscriptEntry> {
        let selected = anchor_timestamp
            .map(|timestamp| get_entries_after_timestamp(entries.to_vec(), timestamp))
            .unwrap_or_else(|| entries.to_vec())
            .into_iter()
            .rev()
            .take(self.max_entries)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();
        self.filter_by_token_budget(selected)
    }

    pub fn select_recent<'a>(&self, entries: &'a [TranscriptEntry]) -> Vec<&'a TranscriptEntry> {
        let selected = entries
            .iter()
            .rev()
            .take(self.max_entries)
            .collect::<Vec<_>>();
        selected.into_iter().rev().collect()
    }

    pub fn build_formatted_context(&self, entries: &[TranscriptEntry]) -> String {
        let lines = entries
            .iter()
            .filter_map(|entry| {
                format_entry_line(entry, Some(DEFAULT_FORMATTED_ENTRY_CONTENT_CHARS))
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            String::new()
        } else {
            format!("{}\n\n---\n", lines.join("\n"))
        }
    }

    pub fn build_formatted_context_refs(&self, entries: &[&TranscriptEntry]) -> String {
        let owned = entries
            .iter()
            .map(|entry| (*entry).clone())
            .collect::<Vec<_>>();
        self.build_formatted_context(&owned)
    }

    fn filter_by_token_budget(&self, entries: Vec<TranscriptEntry>) -> Vec<TranscriptEntry> {
        let mut total_tokens = 0;
        let mut filtered = Vec::new();

        for entry in entries.into_iter().rev() {
            let tokens = serde_json::to_string(&entry)
                .unwrap_or_default()
                .len()
                .div_ceil(CHARS_PER_TOKEN);
            if total_tokens + tokens > self.max_tokens {
                if filtered.is_empty() {
                    filtered.push(entry);
                }
                break;
            }

            total_tokens += tokens;
            filtered.push(entry);
        }

        filtered.reverse();
        filtered
    }
}

const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".cache",
    ".git",
    ".hg",
    ".idea",
    ".next",
    ".nuxt",
    ".pnpm-store",
    ".svn",
    ".turbo",
    ".venv",
    ".vscode",
    ".yarn",
    "__pycache__",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "target",
    "temp",
    "tmp",
    "venv",
];

const DEFAULT_IGNORED_FILES: &[&str] = &[
    ".DS_Store",
    "bun.lockb",
    "composer.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

pub fn matches_default_ignored_path(
    file_path: impl AsRef<Path>,
    project_root: Option<&Path>,
) -> bool {
    let absolute_path = absolutize(file_path.as_ref());
    let relative_path = project_root
        .and_then(|root| absolute_path.strip_prefix(root).ok())
        .unwrap_or(&absolute_path);

    let ignored_files = DEFAULT_IGNORED_FILES
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let ignored_dirs = DEFAULT_IGNORED_DIRS.iter().copied().collect::<HashSet<_>>();
    let base_name = absolute_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    if ignored_files.contains(base_name) || base_name.starts_with('.') {
        return true;
    }

    relative_path.components().any(|component| {
        let segment = component.as_os_str().to_string_lossy();
        ignored_dirs.contains(segment.as_ref()) || segment.starts_with('.')
    })
}

#[derive(Debug, Clone)]
struct ContextFileEntry {
    absolute_path: PathBuf,
    display_path: String,
}

struct GraphRankContext<'a> {
    service: &'a TapeService,
    transcript: &'a Transcript,
    scan_hours: u64,
    prompt: Option<&'a str>,
}

struct ActivityQueryGraph {
    graph: MemoryGraph,
    paths: HashMap<String, String>,
    seed_scores: Vec<(String, f64)>,
}

pub struct MemoryFileSelector {
    memory_dir: PathBuf,
    project_name: String,
    project_root: PathBuf,
    whitelist: Vec<String>,
    blacklist: Vec<String>,
    last_selection_scan_hours: Option<u64>,
    last_graph_selection: Option<MemoryGraphSelection>,
    project_graph: Option<MemoryGraph>,
    is_worktree: bool,
}

impl MemoryFileSelector {
    pub fn new(
        memory_dir: impl Into<PathBuf>,
        workspace: &WorkspaceMeta,
        whitelist: Vec<String>,
        blacklist: Vec<String>,
    ) -> Self {
        let project_root = workspace.root.clone();
        let memory_dir = memory_dir.into();
        let is_worktree = workspace.is_worktree();
        Self {
            memory_dir,
            project_name: workspace.name().to_string(),
            project_root,
            whitelist: dedup(whitelist),
            blacklist: dedup(blacklist),
            last_selection_scan_hours: None,
            last_graph_selection: None,
            project_graph: None,
            is_worktree,
        }
    }

    pub fn select_files_for_context(
        &mut self,
        strategy: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        if strategy == "recent-only" {
            self.last_selection_scan_hours = None;
            return self.scan_memory_directory(limit);
        }
        self.select_smart(limit)
    }

    pub fn select_files_for_context_with_tape(
        &mut self,
        strategy: &str,
        limit: usize,
        memory_scan: (u64, u64),
        service: &TapeService,
        transcript: &Transcript,
    ) -> anyhow::Result<Vec<String>> {
        if strategy == "recent-only" {
            self.last_selection_scan_hours = None;
            return self.scan_memory_directory(limit);
        }
        self.select_smart_with_tape(limit, memory_scan, service, transcript)
    }

    pub fn finalize_context_files(&self, file_paths: Vec<String>) -> anyhow::Result<Vec<String>> {
        let selected_paths = self.filter_injectable_paths(file_paths)?;
        let selected_set = selected_paths
            .iter()
            .map(|path| absolutize(&self.to_absolute_path(path)))
            .collect::<HashSet<_>>();
        let whitelisted_paths = self
            .resolve_listed_paths(&self.whitelist)?
            .into_iter()
            .filter(|path| !selected_set.contains(&absolutize(path)))
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        Ok(whitelisted_paths
            .into_iter()
            .chain(selected_paths)
            .collect())
    }

    pub(crate) fn build_context_from_files_with_highlights(
        &self,
        file_paths: Vec<String>,
        highlighted_files: &[String],
        anchor_mode: Option<crate::extension::memory::tape::types::TapeAnchorMode>,
    ) -> anyhow::Result<Option<memory_core::MemoryInjection>> {
        let file_entries = self.prepare_context_entries(file_paths)?;
        if file_entries.is_empty() {
            return Ok(None);
        }
        let highlighted_paths = highlighted_files
            .iter()
            .map(|path| {
                absolutize(&self.to_absolute_path(path))
                    .to_string_lossy()
                    .to_string()
            })
            .collect::<HashSet<_>>();
        self.render_context_from_entries(&file_entries, &highlighted_paths, anchor_mode)
    }

    pub fn last_selection_scan_hours(&self) -> Option<u64> {
        self.last_selection_scan_hours
    }

    fn select_smart(&mut self, limit: usize) -> anyhow::Result<Vec<String>> {
        self.last_selection_scan_hours = None;
        self.scan_memory_directory(limit)
    }

    fn select_smart_with_tape(
        &mut self,
        limit: usize,
        memory_scan: (u64, u64),
        service: &TapeService,
        transcript: &Transcript,
    ) -> anyhow::Result<Vec<String>> {
        let (activity, scan_hours) =
            self.analyze_smart_activity(memory_scan, service, transcript)?;
        self.ensure_project_graph()?;
        let candidates = self.smart_candidate_paths(&activity)?;
        if candidates.is_empty() {
            return if self.is_worktree {
                Ok(Vec::new())
            } else {
                self.scan_memory_directory(limit)
            };
        }
        self.last_selection_scan_hours = Some(scan_hours);
        Ok(self.rank_by_intent_graph(
            &candidates,
            &activity,
            limit,
            GraphRankContext {
                service,
                transcript,
                scan_hours,
                prompt: None,
            },
        ))
    }

    pub fn precompute_smart_with_tape(
        &mut self,
        memory_scan: (u64, u64),
        service: &TapeService,
        transcript: &Transcript,
    ) -> anyhow::Result<SmartTapePrecompute> {
        let (activity, scan_hours) =
            self.analyze_smart_activity(memory_scan, service, transcript)?;
        let warnings = if self.project_graph.is_none() {
            match warm_memory_graph(&self.memory_dir, &self.project_name) {
                Ok(graph) => {
                    self.project_graph = Some(graph);
                    Vec::new()
                }
                Err(error) => vec![error.to_string()],
            }
        } else {
            Vec::new()
        };
        Ok(SmartTapePrecompute {
            activity,
            scan_hours,
            warnings,
        })
    }

    pub fn select_from_precompute(
        &mut self,
        precompute: &SmartTapePrecompute,
        limit: usize,
        service: &TapeService,
        transcript: &Transcript,
        prompt: &str,
    ) -> anyhow::Result<Vec<String>> {
        self.ensure_project_graph()?;
        self.last_selection_scan_hours = Some(precompute.scan_hours);
        let candidates = self.smart_candidate_paths(&precompute.activity)?;
        Ok(self.rank_by_intent_graph(
            &candidates,
            &precompute.activity,
            limit,
            GraphRankContext {
                service,
                transcript,
                scan_hours: precompute.scan_hours,
                prompt: Some(prompt),
            },
        ))
    }

    fn prepare_context_entries(
        &self,
        file_paths: Vec<String>,
    ) -> anyhow::Result<Vec<ContextFileEntry>> {
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for file_path in file_paths {
            let absolute_path = absolutize(&self.to_absolute_path(&file_path));
            if !absolute_path.exists() || !seen.insert(absolute_path.clone()) {
                continue;
            }
            entries.push(ContextFileEntry {
                display_path: absolute_path.to_string_lossy().to_string(),
                absolute_path,
            });
        }
        Ok(entries)
    }

    fn render_context_from_entries(
        &self,
        file_entries: &[ContextFileEntry],
        highlighted_paths: &HashSet<String>,
        anchor_mode: Option<crate::extension::memory::tape::types::TapeAnchorMode>,
    ) -> anyhow::Result<Option<memory_core::MemoryInjection>> {
        let mut lines = memory_core::memory_context_header(
            MemoryContextMode::Tape,
            MemoryContextOptions {
                anchor_mode: anchor_mode.map(|mode| match mode {
                    crate::extension::memory::tape::types::TapeAnchorMode::Auto => AnchorMode::Auto,
                    crate::extension::memory::tape::types::TapeAnchorMode::Manual => {
                        AnchorMode::Manual
                    }
                }),
            },
        );
        let entries = file_entries
            .iter()
            .map(|entry| {
                (
                    format!("file:{}", entry.absolute_path.to_string_lossy()),
                    entry,
                )
            })
            .collect::<HashMap<_, _>>();
        let selection = self.last_graph_selection.clone().unwrap_or_default();

        lines.push("<memory_graph_context mode=\"smart\">".to_string());
        if !selection.seeds.is_empty() {
            lines.push("<seeds>".to_string());
            for id in &selection.seeds {
                if let Some(node) = selection.nodes.iter().find(|node| &node.node.id == id) {
                    push_graph_node(
                        &mut lines,
                        node,
                        entries.get(id).copied(),
                        highlighted_paths,
                    );
                }
            }
            lines.push("</seeds>".to_string());
        }
        lines.push("<nodes>".to_string());
        let mut emitted = HashSet::new();
        for node in &selection.nodes {
            emitted.insert(node.node.id.clone());
            push_graph_node(
                &mut lines,
                node,
                entries.get(&node.node.id).copied(),
                highlighted_paths,
            );
        }
        for (id, entry) in &entries {
            if emitted.contains(id) {
                continue;
            }
            let node = RankedMemoryGraphNode {
                node: MemoryGraphNodeView {
                    id: (*id).clone(),
                    kind: if self.to_memory_relative_path(&entry.absolute_path).is_some() {
                        MemoryGraphNodeKind::MemoryFile
                    } else {
                        MemoryGraphNodeKind::ProjectFile
                    },
                    label: entry.display_path.clone(),
                    search_text: String::new(),
                },
                score: 0.0,
                seed_score: 0.0,
                relation_score: 0.0,
                distance: 0,
            };
            push_graph_node(&mut lines, &node, Some(entry), highlighted_paths);
        }
        lines.push("</nodes>".to_string());
        if !selection.edges.is_empty() {
            lines.push("<edges>".to_string());
            for edge in &selection.edges {
                lines.push(format!(
                    "<edge from=\"{}\" to=\"{}\" kind=\"{:?}\" confidence=\"{:?}\" weight=\"{:.2}\">{}</edge>",
                    escape_xml(&edge.from),
                    escape_xml(&edge.to),
                    graph_value(&edge.kind),
                    graph_value(&edge.confidence),
                    edge.weight,
                    escape_xml(&edge.reason),
                ));
            }
            lines.push("</edges>".to_string());
        }
        lines.push("</memory_graph_context>".to_string());
        lines.push("</memory_context>".to_string());
        Ok(Some(memory_core::MemoryInjection::new(vec![
            memory_core::MemoryContext::new(
                lines.join("\n"),
                crate::extension::memory::MemoryContextKind::Graph,
                file_entries.len(),
            ),
        ])))
    }

    fn scan_memory_directory(&self, limit: usize) -> anyhow::Result<Vec<String>> {
        let mut files = Vec::new();
        collect_markdown_files(&self.memory_dir, &mut files)?;
        files.retain(|path| !matches_default_ignored_path(path, Some(&self.project_root)));
        files.sort_by_key(|path| {
            std::fs::metadata(path)
                .and_then(|metadata| metadata.modified())
                .ok()
        });
        files.reverse();
        Ok(files
            .into_iter()
            .take(limit)
            .map(|path| path.to_string_lossy().to_string())
            .collect())
    }

    fn filter_injectable_paths(&self, file_paths: Vec<String>) -> anyhow::Result<Vec<String>> {
        let blacklist = self.resolve_listed_paths(&self.blacklist)?;
        Ok(file_paths
            .into_iter()
            .filter(|file_path| {
                let absolute_path = absolutize(&self.to_absolute_path(file_path));
                !blacklist
                    .iter()
                    .any(|blocked| path_inside_or_equal(&absolute_path, blocked))
                    && absolute_path.exists()
                    && (self.to_memory_relative_path(&absolute_path).is_some()
                        || !matches_default_ignored_path(&absolute_path, Some(&self.project_root)))
            })
            .collect())
    }

    fn entries_within_hours(
        &self,
        service: &TapeService,
        transcript: &Transcript,
        hours: u64,
    ) -> anyhow::Result<Vec<TranscriptEntry>> {
        service
            .scan(
                transcript,
                &TapeSessionScanOptions {
                    since_anchor: None,
                    last_anchor: None,
                    between_anchors: None,
                    between_dates: None,
                    scan: None,
                    types: Vec::new(),
                    limit: None,
                    entry_scope: Some(TapeScope::Project),
                    anchor_scope: Some(TapeScope::Project),
                },
            )
            .map(|entries| {
                let since = chrono::Utc::now() - chrono::Duration::hours(hours as i64);
                entries
                    .into_iter()
                    .filter(|entry| {
                        chrono::DateTime::parse_from_rfc3339(&entry.timestamp)
                            .map(|time| time.with_timezone(&chrono::Utc) >= since)
                            .unwrap_or(false)
                    })
                    .collect()
            })
    }

    fn ensure_project_graph(&mut self) -> anyhow::Result<()> {
        if self.project_graph.is_none() {
            self.project_graph =
                Some(MemoryService::new(&self.memory_dir, &self.project_name)?.graph()?);
        }
        Ok(())
    }

    fn analyze_smart_activity(
        &self,
        memory_scan: (u64, u64),
        service: &TapeService,
        transcript: &Transcript,
    ) -> anyhow::Result<(HashMap<String, MemoryPathStats>, u64)> {
        let (start_hours, max_hours) = memory_core::normalize_memory_scan_range(Some(memory_scan));
        let mut hours = start_hours;
        let mut activity = HashMap::new();
        let mut effective_hours = start_hours;
        while hours <= max_hours {
            let entries = self.entries_within_hours(service, transcript, hours)?;
            let (stats, total) = analyze_path_access(&entries, service, hours, &self.project_root);
            if !stats.is_empty() {
                activity = stats;
                effective_hours = hours;
            }
            if total >= MIN_SMART_ACCESS_SAMPLES {
                break;
            }
            hours += 24;
        }
        Ok((activity, effective_hours))
    }

    fn smart_candidate_paths(
        &self,
        activity: &HashMap<String, MemoryPathStats>,
    ) -> anyhow::Result<Vec<String>> {
        let mut paths = self
            .project_graph
            .iter()
            .flat_map(MemoryGraph::nodes)
            .filter_map(graph_file_path)
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut activity_paths = activity.keys().cloned().collect::<Vec<_>>();
        activity_paths.sort();
        paths.extend(activity_paths);
        let mut seen = HashSet::new();
        let paths = paths
            .into_iter()
            .filter(|path| seen.insert(absolutize(&self.to_absolute_path(path))))
            .collect();
        self.filter_injectable_paths(paths)
    }

    fn rank_by_intent_graph(
        &mut self,
        candidates: &[String],
        activity: &HashMap<String, MemoryPathStats>,
        limit: usize,
        context: GraphRankContext<'_>,
    ) -> Vec<String> {
        let fallback = candidates.iter().take(limit).cloned().collect::<Vec<_>>();
        let Some(query_graph) = self.query_graph(candidates, activity) else {
            return fallback;
        };
        let intent = self.intent_query(context.service, context.transcript, context.scan_hours);
        let query = context
            .prompt
            .map(|prompt| format!("{prompt} {intent}"))
            .unwrap_or(intent);
        if query.trim().is_empty() && query_graph.seed_scores.is_empty() {
            return fallback;
        }
        let selection = query_graph.graph.select(&MemoryGraphQuery {
            prompt: query,
            seed_scores: query_graph.seed_scores,
            max_depth: 2,
            ..MemoryGraphQuery::default()
        });
        self.last_graph_selection = Some(selection.clone());
        let mut selected = selection
            .nodes
            .into_iter()
            .filter_map(|node| query_graph.paths.get(&node.node.id).cloned())
            .collect::<Vec<_>>();
        selected.truncate(limit);
        if selected.is_empty() {
            fallback
        } else {
            selected
        }
    }

    fn query_graph(
        &self,
        candidates: &[String],
        activity: &HashMap<String, MemoryPathStats>,
    ) -> Option<ActivityQueryGraph> {
        let persisted = self.project_graph.as_ref()?;
        let mut nodes = persisted.nodes().cloned().collect::<Vec<_>>();
        let mut edges = persisted.edges().to_vec();
        let existing = persisted
            .nodes()
            .filter_map(|node| {
                let path = graph_file_path(node)?;
                Some((absolutize(&self.to_absolute_path(path)), node.id.clone()))
            })
            .collect::<HashMap<_, _>>();
        let activity = activity
            .iter()
            .map(|(path, stats)| (absolutize(Path::new(path)), stats))
            .collect::<HashMap<_, _>>();
        let mut paths = HashMap::new();
        let mut seed_scores = Vec::new();
        for candidate in candidates {
            let path = absolutize(&self.to_absolute_path(candidate));
            let id = existing
                .get(&path)
                .cloned()
                .unwrap_or_else(|| self.push_activity_node(&mut nodes, &path));
            paths.insert(id.clone(), candidate.clone());
            if let Some(stats) = activity.get(&path) {
                seed_scores.push((id.clone(), stats.final_score()));
                push_activity_edges(&mut edges, &id, stats);
            }
        }
        Some(ActivityQueryGraph {
            graph: MemoryGraph::new(nodes, edges),
            paths,
            seed_scores,
        })
    }

    fn push_activity_node(&self, nodes: &mut Vec<MemoryGraphNodeView>, path: &Path) -> String {
        let is_memory = self.to_memory_relative_path(path).is_some();
        let id = if is_memory {
            format!("memory:{}", path.to_string_lossy())
        } else {
            project_file_node_id(path.to_string_lossy().as_ref())
        };
        if !nodes.iter().any(|node| node.id == id) {
            nodes.push(MemoryGraphNodeView {
                id: id.clone(),
                kind: if is_memory {
                    MemoryGraphNodeKind::MemoryFile
                } else {
                    MemoryGraphNodeKind::ProjectFile
                },
                label: path.to_string_lossy().to_string(),
                search_text: String::new(),
            });
        }
        id
    }

    fn intent_query(
        &self,
        service: &TapeService,
        transcript: &Transcript,
        scan_hours: u64,
    ) -> String {
        let user_text = self
            .entries_within_hours(service, transcript, scan_hours)
            .ok()
            .and_then(|entries| {
                entries
                    .into_iter()
                    .rev()
                    .find_map(|entry| match entry.kind {
                        TranscriptEntryKind::Message {
                            message: AgentMessage::User { content },
                        } => Some(extract_user_message_content(&content)),
                        _ => None,
                    })
            })
            .unwrap_or_default();
        let anchor_text = service
            .search_anchors_with_fallback(
                crate::extension::memory::tape::types::TapeAnchorScanOptions {
                    since: Some(
                        (chrono::Utc::now() - chrono::Duration::hours(scan_hours as i64))
                            .to_rfc3339(),
                    ),
                    limit: Some(10),
                    ..crate::extension::memory::tape::types::TapeAnchorScanOptions::default()
                },
            )
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(3)
            .map(|anchor| {
                let meta = anchor.meta.unwrap_or_default();
                [meta.summary, meta.purpose, Some(meta.keywords.join(" "))]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
            .join(" ");
        format!("{user_text} {anchor_text}")
    }

    fn resolve_listed_paths(&self, paths: &[String]) -> anyhow::Result<Vec<PathBuf>> {
        let mut resolved = Vec::new();
        for path in paths {
            let absolute_path = absolutize(&self.to_absolute_path(path));
            if absolute_path.is_dir() {
                collect_markdown_files(&absolute_path, &mut resolved)?;
            } else if absolute_path.exists() {
                resolved.push(absolute_path);
            }
        }
        Ok(resolved)
    }

    fn to_absolute_path(&self, file_path: &str) -> PathBuf {
        let path = Path::new(file_path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            let memory_path = self.memory_dir.join(path);
            if memory_path.exists() {
                memory_path
            } else {
                self.project_root.join(path)
            }
        }
    }

    fn to_memory_relative_path(&self, file_path: &Path) -> Option<PathBuf> {
        absolutize(file_path)
            .strip_prefix(&self.memory_dir)
            .ok()
            .map(PathBuf::from)
    }
}

fn graph_file_path(node: &MemoryGraphNodeView) -> Option<&str> {
    match node.kind {
        MemoryGraphNodeKind::MemoryFile => node.id.strip_prefix("memory:"),
        MemoryGraphNodeKind::ProjectFile => node.id.strip_prefix("file:"),
        _ => None,
    }
}

fn push_activity_edges(edges: &mut Vec<MemoryGraphEdge>, file_id: &str, stats: &MemoryPathStats) {
    edges.extend(stats.anchor_scores.iter().map(|(anchor_id, score)| {
        let mut edge = MemoryGraphEdge::extracted(
            anchor_node_id(anchor_id),
            file_id,
            MemoryGraphEdgeKind::AccessedFile,
            "file access after anchor",
        );
        edge.weight = (score / 40.0).clamp(0.2, 1.0);
        edge
    }));
}

fn entry_to_message(entry: &TranscriptEntry) -> Option<TapeMessage> {
    match &entry.kind {
        TranscriptEntryKind::Failure { .. } | TranscriptEntryKind::ContextEdit { .. } => None,
        TranscriptEntryKind::Message { message } => Some(TapeMessage {
            role: message_tape_role(message),
            content: message_content(message),
            tool_call_id: tool_call_id(message),
            name: tool_name(message),
        }),
        TranscriptEntryKind::Event { name, data } => Some(TapeMessage {
            role: TapeMessageRole::Assistant,
            content: format!(
                "[{}] {}",
                name.split('/').next_back().unwrap_or("custom"),
                serde_json::to_string_pretty(data).unwrap_or_default()
            ),
            tool_call_id: None,
            name: None,
        }),
        TranscriptEntryKind::Compaction { summary, .. } => Some(TapeMessage {
            role: TapeMessageRole::Assistant,
            content: format!("[Compaction] {summary}"),
            tool_call_id: None,
            name: None,
        }),
        TranscriptEntryKind::Reset { handoff } => Some(TapeMessage {
            role: TapeMessageRole::User,
            content: format!("[Reset] {}", handoff.as_deref().unwrap_or("")),
            tool_call_id: None,
            name: None,
        }),
    }
}

fn message_tape_role(message: &AgentMessage) -> TapeMessageRole {
    match message {
        AgentMessage::User { .. } => TapeMessageRole::User,
        AgentMessage::Assistant { .. } => TapeMessageRole::Assistant,
        AgentMessage::Agent { .. } => TapeMessageRole::Agent,
        AgentMessage::System { .. } | AgentMessage::Context { .. } => TapeMessageRole::System,
        AgentMessage::ToolResult { .. } => TapeMessageRole::Tool,
    }
}

fn message_role_label(message: &AgentMessage) -> &'static str {
    match message_tape_role(message) {
        TapeMessageRole::User => "User",
        TapeMessageRole::Agent => "Agent",
        TapeMessageRole::Assistant => "Assistant",
        TapeMessageRole::System => "System",
        TapeMessageRole::Tool => "Tool",
    }
}

fn message_content(message: &AgentMessage) -> String {
    match message {
        AgentMessage::User { content } => extract_user_message_content(content),
        AgentMessage::Context { content, .. }
        | AgentMessage::Agent { content }
        | AgentMessage::Assistant { content, .. }
        | AgentMessage::ToolResult { content, .. } => extract_message_content(content),
        AgentMessage::System { content } => content.to_string(),
    }
}

fn tool_call_id(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::ToolResult { tool_call_id, .. } => Some(tool_call_id.to_string()),
        _ => None,
    }
}

fn tool_name(message: &AgentMessage) -> Option<String> {
    match message {
        AgentMessage::ToolResult { tool_name, .. } => Some(tool_name.to_string()),
        _ => None,
    }
}

fn truncate_content(content: &str, max_content_chars: Option<usize>) -> String {
    let Some(max) = max_content_chars else {
        return content.to_string();
    };
    let mut chars = content.chars();
    let prefix = chars.by_ref().take(max).collect::<String>();
    if chars.next().is_some() {
        format!("{prefix}...")
    } else {
        content.to_string()
    }
}

fn to_locale_time(timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .map(|time| time.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| timestamp.to_string())
}

fn push_graph_node(
    lines: &mut Vec<String>,
    ranked: &RankedMemoryGraphNode,
    entry: Option<&ContextFileEntry>,
    highlighted_paths: &HashSet<String>,
) {
    let selected = entry.is_some();
    let priority = entry
        .filter(|entry| {
            highlighted_paths.contains(&entry.absolute_path.to_string_lossy().to_string())
        })
        .map(|_| "high")
        .unwrap_or("normal");
    lines.push(format!(
        "<node id=\"{}\" kind=\"{}\" selected=\"{}\" priority=\"{}\" score=\"{:.2}\">",
        escape_xml(&ranked.node.id),
        graph_value(&ranked.node.kind),
        selected,
        priority,
        ranked.score,
    ));
    lines.push(escape_xml(&ranked.node.label));
    lines.push("</node>".to_string());
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn collect_markdown_files(root: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_markdown_files(&path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "md") {
            files.push(path);
        }
    }
    Ok(())
}

fn dedup(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn path_inside_or_equal(path: &Path, parent: &Path) -> bool {
    path == parent || path.starts_with(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_content_respects_unicode_character_boundaries() {
        assert_eq!(truncate_content("这是中文内容", Some(4)), "这是中文...");
        assert_eq!(truncate_content("这是中文", Some(4)), "这是中文");
    }

    #[test]
    fn activity_only_file_enters_graph_ranking_without_focus_lines() {
        let root = std::env::temp_dir().join(format!("phi-tape-activity-{}", uuid::Uuid::new_v4()));
        let memory_dir = root.join("memory");
        let active_file = root.join("active.rs");
        std::fs::create_dir_all(&memory_dir).unwrap();
        std::fs::write(&active_file, "fn active() {}\n").unwrap();

        let mut selector = MemoryFileSelector::new(
            &memory_dir,
            &WorkspaceMeta::discover(&root, None).unwrap(),
            Vec::new(),
            Vec::new(),
        );
        selector.project_graph = Some(MemoryGraph::new(
            vec![MemoryGraphNodeView {
                id: anchor_node_id("checkpoint"),
                kind: MemoryGraphNodeKind::Anchor,
                label: "checkpoint".to_string(),
                search_text: String::new(),
            }],
            Vec::new(),
        ));
        let active_path = active_file.to_string_lossy().to_string();
        let activity = HashMap::from([(
            active_path.clone(),
            MemoryPathStats {
                score: 30.0,
                anchor_scores: HashMap::from([("checkpoint".to_string(), 40.0)]),
                ..MemoryPathStats::default()
            },
        )]);
        let service = TapeService::default();
        let transcript = Transcript::new();
        let selected = selector.rank_by_intent_graph(
            std::slice::from_ref(&active_path),
            &activity,
            1,
            GraphRankContext {
                service: &service,
                transcript: &transcript,
                scan_hours: 72,
                prompt: Some("unrelated prompt"),
            },
        );
        let selection = selector.last_graph_selection.as_ref().unwrap();
        assert!(
            selection
                .edges
                .iter()
                .any(|edge| edge.kind == MemoryGraphEdgeKind::AccessedFile)
        );
        let injection = selector
            .build_context_from_files_with_highlights(selected.clone(), &selected, None)
            .unwrap()
            .unwrap();

        assert_eq!(selected, vec![active_path]);
        assert!(!injection.memory_contexts()[0].content().contains("<focus"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
