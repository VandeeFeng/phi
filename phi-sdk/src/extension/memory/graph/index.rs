use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};

use anyhow::ensure;
use serde::{Deserialize, Serialize};

use crate::extension::memory::memory_core;
use crate::extension::memory::tape::{TapeAnchor, read_anchors};

const MAX_RELATION_GROUP_SIZE: usize = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryGraphRecord {
    pub sources: MemoryGraphSources,
    #[serde(default)]
    pub source_state: MemoryGraphSourceState,
    #[serde(default)]
    pub nodes: Vec<MemoryGraphNode>,
    #[serde(default)]
    pub edges: Vec<MemoryGraphEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryGraphSources {
    #[serde(default = "default_memory_root")]
    pub memory_root: String,
    #[serde(default)]
    pub anchors: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryGraphSourceState {
    #[serde(default)]
    pub memory_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchors_fingerprint: Option<String>,
}

impl Default for MemoryGraphSources {
    fn default() -> Self {
        Self {
            memory_root: default_memory_root(),
            anchors: String::new(),
        }
    }
}

impl Default for MemoryGraphRecord {
    fn default() -> Self {
        Self::new(MemoryGraphSources::default())
    }
}

impl MemoryGraphRecord {
    pub fn new(sources: MemoryGraphSources) -> Self {
        Self {
            sources,
            source_state: MemoryGraphSourceState::default(),
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    pub fn normalize(&mut self) -> anyhow::Result<()> {
        normalize_sources(&mut self.sources)?;
        normalize_nodes(&mut self.nodes)?;
        normalize_edges(&mut self.edges, &self.nodes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MemoryGraphNode {
    MemoryFile {
        id: String,
        path: String,
        title: String,
        description: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tags: Vec<String>,
        fingerprint: String,
    },
    Anchor {
        id: String,
        anchor_id: String,
        label: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        search_text: String,
        source: String,
    },
    ProjectFile {
        id: String,
        path: String,
    },
}

impl MemoryGraphNode {
    pub fn id(&self) -> &str {
        match self {
            Self::MemoryFile { id, .. }
            | Self::Anchor { id, .. }
            | Self::ProjectFile { id, .. } => id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::MemoryFile { .. } => "memory_file",
            Self::Anchor { .. } => "anchor",
            Self::ProjectFile { .. } => "project_file",
        }
    }

    pub fn label(&self) -> &str {
        match self {
            Self::MemoryFile { title, .. } => title,
            Self::Anchor { label, .. } => label,
            Self::ProjectFile { path, .. } => path,
        }
    }

    fn normalize(&mut self) -> anyhow::Result<()> {
        match self {
            Self::MemoryFile { id, path, .. } => normalize_node_path(id, path, "memory:"),
            Self::ProjectFile { id, path } => normalize_node_path(id, path, "file:"),
            Self::Anchor {
                id,
                anchor_id,
                source,
                ..
            } => normalize_source_node(id, anchor_id, source, "anchor:"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryGraphEdgeKind {
    Continues,
    AccessedFile,
    Mentions,
    DependsOn,
    Supersedes,
    SameTopic,
    SharedContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryGraphEdgeOrigin {
    Source,
    Derived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryGraphConfidence {
    Extracted,
    Inferred,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MemoryGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: MemoryGraphEdgeKind,
    pub origin: MemoryGraphEdgeOrigin,
    pub confidence: MemoryGraphConfidence,
    pub weight: f64,
    pub reason: String,
}

impl MemoryGraphEdge {
    pub(crate) fn extracted(
        from: impl Into<String>,
        to: impl Into<String>,
        kind: MemoryGraphEdgeKind,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            kind,
            origin: MemoryGraphEdgeOrigin::Source,
            confidence: MemoryGraphConfidence::Extracted,
            weight: 1.0,
            reason: reason.into(),
        }
    }

    pub(crate) fn derived(
        from: impl Into<String>,
        to: impl Into<String>,
        kind: MemoryGraphEdgeKind,
        weight: f64,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            kind,
            origin: MemoryGraphEdgeOrigin::Derived,
            confidence: MemoryGraphConfidence::Inferred,
            weight,
            reason: reason.into(),
        }
    }
}

pub fn memory_graph_sources(project_name: &str) -> MemoryGraphSources {
    MemoryGraphSources {
        memory_root: default_memory_root(),
        anchors: format!("{project_name}__anchors.jsonl"),
    }
}

pub fn memory_file_node_id(path: &str) -> String {
    format!("memory:{}", normalize_path(path))
}

pub fn project_file_node_id(path: &str) -> String {
    format!("file:{}", normalize_path(path))
}

pub fn anchor_node_id(id: &str) -> String {
    format!("anchor:{}", id.trim())
}

pub fn reconcile_memory_graph(
    sources: MemoryGraphSources,
    memory_dir: &Path,
) -> anyhow::Result<MemoryGraphRecord> {
    for _ in 0..2 {
        let before = current_source_state(memory_dir, &sources)?;
        let graph = build_memory_graph(sources.clone(), memory_dir)?;
        if before == graph.source_state {
            return Ok(graph);
        }
    }
    anyhow::bail!("Memory graph sources changed repeatedly during reconciliation")
}

fn build_memory_graph(
    sources: MemoryGraphSources,
    memory_dir: &Path,
) -> anyhow::Result<MemoryGraphRecord> {
    let mut graph = MemoryGraphRecord::new(sources);
    let mut nodes = BTreeMap::new();
    let memory_files = load_memory_nodes(memory_dir, &graph.sources.memory_root, &mut graph.edges)?;
    nodes.extend(
        memory_files
            .into_iter()
            .map(|node| (node.id().to_string(), node)),
    );
    let anchors = if graph.sources.anchors.trim().is_empty() {
        Vec::new()
    } else {
        read_anchors(&resolve_source(memory_dir, &graph.sources.anchors)?)?
    };
    project_anchors(
        &mut nodes,
        &mut graph.edges,
        &anchors,
        &graph.sources.anchors,
    );
    derive_anchor_topics(&mut graph.edges, &anchors);
    derive_memory_topics(&mut graph.edges, &nodes);
    derive_anchor_memory_mentions(&mut graph.edges, &nodes);
    derive_shared_context(&mut graph.edges);
    graph.source_state = current_source_state(memory_dir, &graph.sources)?;
    graph.nodes = nodes.into_values().collect();
    let node_ids = graph
        .nodes
        .iter()
        .map(|node| node.id().to_string())
        .collect::<HashSet<_>>();
    graph
        .edges
        .retain(|edge| node_ids.contains(&edge.from) && node_ids.contains(&edge.to));
    graph.normalize()?;
    Ok(graph)
}

fn load_memory_nodes(
    memory_dir: &Path,
    root: &str,
    edges: &mut Vec<MemoryGraphEdge>,
) -> anyhow::Result<Vec<MemoryGraphNode>> {
    let scan_dir = resolve_source(memory_dir, root)?;
    let mut files = memory_core::list_memory_files(&scan_dir)?;
    files.sort();
    files
        .into_iter()
        .map(|file| memory_node(memory_dir, &file, edges))
        .collect()
}

fn memory_node(
    memory_dir: &Path,
    file: &Path,
    edges: &mut Vec<MemoryGraphEdge>,
) -> anyhow::Result<MemoryGraphNode> {
    let path = relative_memory_path(file, memory_dir)?;
    let memory = memory_core::read_memory_file(file)?;
    for (targets, kind) in [
        (
            &memory.frontmatter.depends_on,
            MemoryGraphEdgeKind::DependsOn,
        ),
        (
            &memory.frontmatter.supersedes,
            MemoryGraphEdgeKind::Supersedes,
        ),
        (&memory.frontmatter.mentions, MemoryGraphEdgeKind::Mentions),
    ] {
        append_explicit_relations(edges, &path, targets, kind);
    }
    Ok(MemoryGraphNode::MemoryFile {
        id: memory_file_node_id(&path),
        title: memory_title(&path),
        path,
        description: memory.frontmatter.description,
        tags: memory.frontmatter.tags,
        fingerprint: file_fingerprint(file)?,
    })
}

fn project_anchors(
    nodes: &mut BTreeMap<String, MemoryGraphNode>,
    edges: &mut Vec<MemoryGraphEdge>,
    anchors: &[TapeAnchor],
    source: &str,
) {
    for anchor in anchors {
        let node = anchor_graph_node(anchor, source);
        nodes.insert(node.id().to_string(), node);
    }
    for anchors in anchors_by_session(anchors).into_values() {
        for pair in anchors.windows(2) {
            edges.push(MemoryGraphEdge::extracted(
                anchor_node_id(&pair[0].id),
                anchor_node_id(&pair[1].id),
                MemoryGraphEdgeKind::Continues,
                "chronological anchors in the same session",
            ));
        }
    }
}

fn anchor_graph_node(anchor: &TapeAnchor, source: &str) -> MemoryGraphNode {
    let meta = anchor.meta.as_ref();
    let search_text = [
        meta.and_then(|meta| meta.summary.as_deref())
            .unwrap_or_default(),
        meta.and_then(|meta| meta.purpose.as_deref())
            .unwrap_or_default(),
        &meta.map(|meta| meta.keywords.join(" ")).unwrap_or_default(),
    ]
    .join(" ");
    MemoryGraphNode::Anchor {
        id: anchor_node_id(&anchor.id),
        anchor_id: anchor.id.to_string(),
        label: anchor.name.to_string(),
        search_text,
        source: source.to_string(),
    }
}

fn anchors_by_session(anchors: &[TapeAnchor]) -> BTreeMap<&str, Vec<&TapeAnchor>> {
    let mut grouped = BTreeMap::<&str, Vec<&TapeAnchor>>::new();
    for anchor in anchors {
        grouped.entry(&anchor.session_id).or_default().push(anchor);
    }
    for anchors in grouped.values_mut() {
        anchors.sort_by(|left, right| {
            left.timestamp
                .cmp(&right.timestamp)
                .then_with(|| left.id.cmp(&right.id))
        });
    }
    grouped
}

fn derive_anchor_topics(edges: &mut Vec<MemoryGraphEdge>, anchors: &[TapeAnchor]) {
    let mut by_keyword = BTreeMap::<String, Vec<String>>::new();
    for anchor in anchors {
        for keyword in anchor
            .meta
            .as_ref()
            .map(|meta| meta.keywords.as_slice())
            .unwrap_or_default()
        {
            let keyword = keyword.trim().to_lowercase();
            if !keyword.is_empty() {
                by_keyword
                    .entry(keyword)
                    .or_default()
                    .push(anchor_node_id(&anchor.id));
            }
        }
    }
    for (keyword, ids) in by_keyword {
        append_relation_chain(
            edges,
            ids,
            MemoryGraphEdgeKind::SameTopic,
            0.6,
            format!("shared anchor keyword: {keyword}"),
        );
    }
}

fn derive_memory_topics(
    edges: &mut Vec<MemoryGraphEdge>,
    nodes: &BTreeMap<String, MemoryGraphNode>,
) {
    let mut by_tag = BTreeMap::<String, Vec<String>>::new();
    for node in nodes.values() {
        let MemoryGraphNode::MemoryFile { id, tags, .. } = node else {
            continue;
        };
        for tag in tags {
            let tag = tag.trim().to_lowercase();
            if !tag.is_empty() {
                by_tag.entry(tag).or_default().push(id.clone());
            }
        }
    }
    for (tag, ids) in by_tag {
        append_relation_chain(
            edges,
            ids,
            MemoryGraphEdgeKind::SameTopic,
            0.5,
            format!("shared memory tag: {tag}"),
        );
    }
}

fn append_relation_chain(
    edges: &mut Vec<MemoryGraphEdge>,
    mut ids: Vec<String>,
    kind: MemoryGraphEdgeKind,
    weight: f64,
    reason: String,
) {
    ids.sort();
    ids.dedup();
    if ids.len() < 2 || ids.len() > MAX_RELATION_GROUP_SIZE {
        return;
    }
    edges
        .extend(ids.windows(2).map(|pair| {
            MemoryGraphEdge::derived(&pair[0], &pair[1], kind, weight, reason.clone())
        }));
}

fn append_explicit_relations(
    edges: &mut Vec<MemoryGraphEdge>,
    source: &str,
    targets: &[String],
    kind: MemoryGraphEdgeKind,
) {
    edges.extend(targets.iter().filter_map(|target| {
        let target = normalize_path(target);
        (!target.is_empty()).then(|| {
            MemoryGraphEdge::extracted(
                memory_file_node_id(source),
                memory_file_node_id(&target),
                kind,
                "memory frontmatter relation",
            )
        })
    }));
}

fn derive_anchor_memory_mentions(
    edges: &mut Vec<MemoryGraphEdge>,
    nodes: &BTreeMap<String, MemoryGraphNode>,
) {
    let memories = nodes
        .values()
        .filter_map(|node| match node {
            MemoryGraphNode::MemoryFile { id, path, .. } => {
                Some((id.as_str(), path.to_lowercase()))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    for node in nodes.values() {
        let MemoryGraphNode::Anchor {
            id,
            label,
            search_text,
            ..
        } = node
        else {
            continue;
        };
        let text = format!("{label} {search_text}").to_lowercase();
        edges.extend(memories.iter().filter_map(|(memory_id, path)| {
            text.contains(path).then(|| {
                MemoryGraphEdge::derived(
                    id,
                    *memory_id,
                    MemoryGraphEdgeKind::Mentions,
                    0.8,
                    "anchor mentions memory path",
                )
            })
        }));
    }
}

fn derive_shared_context(edges: &mut Vec<MemoryGraphEdge>) {
    let mut by_memory = BTreeMap::<String, Vec<String>>::new();
    for edge in edges.iter() {
        if edge.kind == MemoryGraphEdgeKind::Mentions && edge.from.starts_with("anchor:") {
            by_memory
                .entry(edge.to.clone())
                .or_default()
                .push(edge.from.clone());
        }
    }
    for (memory_id, ids) in by_memory {
        append_relation_chain(
            edges,
            ids,
            MemoryGraphEdgeKind::SharedContext,
            0.7,
            format!("shared memory context: {memory_id}"),
        );
    }
}

pub fn current_source_state(
    dir: &Path,
    sources: &MemoryGraphSources,
) -> anyhow::Result<MemoryGraphSourceState> {
    Ok(MemoryGraphSourceState {
        memory_fingerprint: directory_fingerprint(&resolve_source(dir, &sources.memory_root)?)?,
        anchors_fingerprint: source_fingerprint(dir, &sources.anchors)?,
    })
}

fn source_fingerprint(dir: &Path, source: &str) -> anyhow::Result<Option<String>> {
    if source.trim().is_empty() {
        return Ok(None);
    }
    let path = resolve_source(dir, source)?;
    path.exists().then(|| file_fingerprint(&path)).transpose()
}

fn directory_fingerprint(dir: &Path) -> anyhow::Result<String> {
    let mut files = memory_core::list_memory_files(dir)?;
    files.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in files {
        file.strip_prefix(dir).unwrap_or(&file).hash(&mut hasher);
        fs::read(file)?.hash(&mut hasher);
    }
    Ok(format!("{:016x}", hasher.finish()))
}

fn normalize_sources(sources: &mut MemoryGraphSources) -> anyhow::Result<()> {
    sources.memory_root = normalize_path(&sources.memory_root);
    sources.anchors = normalize_path(&sources.anchors);
    validate_relative_source(&sources.memory_root)?;
    if !sources.anchors.is_empty() {
        validate_relative_source(&sources.anchors)?;
    }
    Ok(())
}

fn normalize_nodes(nodes: &mut Vec<MemoryGraphNode>) -> anyhow::Result<()> {
    let mut ids = HashSet::new();
    for node in nodes.iter_mut() {
        node.normalize()?;
        ensure!(
            ids.insert(node.id().to_string()),
            "Duplicate memory graph node: {}",
            node.id()
        );
    }
    nodes.sort_by(|left, right| left.id().cmp(right.id()));
    Ok(())
}

fn normalize_edges(
    edges: &mut Vec<MemoryGraphEdge>,
    nodes: &[MemoryGraphNode],
) -> anyhow::Result<()> {
    let node_ids = nodes.iter().map(|node| node.id()).collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    for edge in edges.iter_mut() {
        edge.from = edge.from.trim().to_string();
        edge.to = edge.to.trim().to_string();
        ensure!(
            edge.weight.is_finite() && edge.weight >= 0.0,
            "Invalid memory graph edge weight"
        );
    }
    edges.retain(|edge| {
        !edge.from.is_empty()
            && !edge.to.is_empty()
            && edge.from != edge.to
            && seen.insert((edge.from.clone(), edge.to.clone(), edge.kind))
    });
    edges.sort_by(|left, right| {
        (
            &left.from,
            &left.to,
            left.kind,
            left.origin,
            left.confidence,
            &left.reason,
        )
            .cmp(&(
                &right.from,
                &right.to,
                right.kind,
                right.origin,
                right.confidence,
                &right.reason,
            ))
    });
    for edge in edges {
        ensure!(
            node_ids.contains(edge.from.as_str()) && node_ids.contains(edge.to.as_str()),
            "Memory graph edge references a missing node: {} -> {}",
            edge.from,
            edge.to
        );
    }
    Ok(())
}

fn normalize_node_path(id: &mut String, path: &mut String, prefix: &str) -> anyhow::Result<()> {
    *path = normalize_path(path);
    validate_relative_source(path)?;
    let expected = format!("{prefix}{path}");
    ensure!(
        id.trim() == expected,
        "Memory graph node ID does not match its path: {id}"
    );
    *id = expected;
    Ok(())
}

fn normalize_source_node(
    id: &mut String,
    source_id: &mut String,
    source: &mut String,
    prefix: &str,
) -> anyhow::Result<()> {
    *source_id = source_id.trim().to_string();
    ensure!(
        !source_id.is_empty(),
        "Memory graph source ID cannot be empty"
    );
    *source = normalize_path(source);
    validate_relative_source(source)?;
    let expected = format!("{prefix}{source_id}");
    ensure!(
        id.trim() == expected,
        "Memory graph node ID does not match its source ID: {id}"
    );
    *id = expected;
    Ok(())
}

fn validate_relative_source(source: &str) -> anyhow::Result<()> {
    ensure!(
        !source.trim().is_empty(),
        "Memory graph source path cannot be empty"
    );
    let path = Path::new(source);
    ensure!(
        !path.is_absolute(),
        "Memory graph source path must be relative: {source}"
    );
    ensure!(
        path.components()
            .all(|part| matches!(part, Component::Normal(_))),
        "Memory graph source path cannot escape its project directory: {source}"
    );
    Ok(())
}

fn resolve_source(memory_dir: &Path, source: &str) -> anyhow::Result<PathBuf> {
    validate_relative_source(source)?;
    Ok(memory_dir.join(source))
}

fn relative_memory_path(path: &Path, memory_dir: &Path) -> anyhow::Result<String> {
    let memory_dir = canonical_or_original(memory_dir);
    let path = canonical_or_original(path);
    let relative = path.strip_prefix(&memory_dir).map_err(|_| {
        anyhow::anyhow!(
            "Memory file is outside the memory directory: {}",
            path.display()
        )
    })?;
    let value = normalize_path(&relative.to_string_lossy());
    validate_relative_source(&value)?;
    Ok(value)
}

fn canonical_or_original(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn memory_title(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path)
        .to_string()
}

fn file_fingerprint(path: &Path) -> anyhow::Result<String> {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    fs::read(path)?.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

fn normalize_path(path: &str) -> String {
    path.trim().replace('\\', "/")
}

fn default_memory_root() -> String {
    "core".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("phi-sdk-memory-graph-{}", uuid::Uuid::new_v4()))
    }

    fn memory_file(path: &Path, description: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            format!("---\ndescription: {description}\ntags:\n  - graph\n---\nMemory content"),
        )
        .unwrap();
    }

    #[test]
    fn rejects_kind_and_path_id_mismatches() {
        let mut graph = MemoryGraphRecord::default();
        graph.nodes.push(MemoryGraphNode::MemoryFile {
            id: "memory:core/wrong.md".to_string(),
            path: "core/design.md".to_string(),
            title: "design".to_string(),
            description: String::new(),
            tags: Vec::new(),
            fingerprint: String::new(),
        });

        assert!(graph.normalize().is_err());
    }

    #[test]
    fn sorts_nodes_and_deduplicates_edges() {
        let mut graph = MemoryGraphRecord::default();
        graph.nodes = vec![
            MemoryGraphNode::ProjectFile {
                id: "file:b.rs".to_string(),
                path: "b.rs".to_string(),
            },
            MemoryGraphNode::ProjectFile {
                id: "file:a.rs".to_string(),
                path: "a.rs".to_string(),
            },
        ];
        let edge = MemoryGraphEdge::extracted(
            "file:a.rs",
            "file:b.rs",
            MemoryGraphEdgeKind::DependsOn,
            "test",
        );
        graph.edges = vec![edge.clone(), edge];

        graph.normalize().unwrap();

        assert_eq!(graph.nodes[0].id(), "file:a.rs");
        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn reconciles_memory_and_anchor_nodes_deterministically() {
        let directory = temp_dir();
        memory_file(&directory.join("core/design.md"), "Graph design");
        let mut anchor = TapeAnchor::new(
            "graph/start",
            crate::extension::memory::tape::TapeAnchorType::Normal,
        );
        anchor.id = "a1".into();
        anchor.session_id = "session".into();
        anchor.session_entry_id = "entry".into();
        anchor.timestamp = "2026-01-01T00:00:00Z".into();
        fs::write(
            directory.join("project__anchors.jsonl"),
            format!("{}\n", serde_json::to_string(&anchor).unwrap()),
        )
        .unwrap();

        let sources = memory_graph_sources("project");
        let first = reconcile_memory_graph(sources.clone(), &directory).unwrap();
        let second = reconcile_memory_graph(sources, &directory).unwrap();

        assert_eq!(first, second);
        assert!(
            first
                .nodes
                .iter()
                .any(|node| node.id() == "memory:core/design.md")
        );
        assert!(first.nodes.iter().any(|node| node.id() == "anchor:a1"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_anchor_and_memory_topic_relations() {
        let directory = temp_dir();
        memory_file(&directory.join("core/one.md"), "One");
        memory_file(&directory.join("core/two.md"), "Two");
        let mut first = TapeAnchor::new(
            "first",
            crate::extension::memory::tape::TapeAnchorType::Normal,
        )
        .with_meta(crate::extension::memory::tape::TapeAnchorMeta {
            keywords: vec!["graph".to_string()],
            ..Default::default()
        });
        first = first.with_summary("core/one.md");
        first.id = "a1".into();
        first.session_id = "session".into();
        first.session_entry_id = "entry/1".into();
        let mut second = TapeAnchor::new(
            "second",
            crate::extension::memory::tape::TapeAnchorType::Normal,
        )
        .with_meta(crate::extension::memory::tape::TapeAnchorMeta {
            keywords: vec!["graph".to_string()],
            ..Default::default()
        });
        second = second.with_summary("core/one.md");
        second.id = "a2".into();
        second.session_id = "session".into();
        second.session_entry_id = "entry/2".into();
        fs::write(
            directory.join("project__anchors.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::to_string(&first).unwrap(),
                serde_json::to_string(&second).unwrap()
            ),
        )
        .unwrap();

        let graph = reconcile_memory_graph(memory_graph_sources("project"), &directory).unwrap();

        assert!(graph.edges.iter().any(|edge| {
            edge.from == "anchor:a1"
                && edge.to == "anchor:a2"
                && edge.kind == MemoryGraphEdgeKind::SameTopic
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "memory:core/one.md"
                && edge.to == "memory:core/two.md"
                && edge.kind == MemoryGraphEdgeKind::SameTopic
        }));
        assert!(graph.edges.iter().any(|edge| {
            edge.from == "anchor:a1"
                && edge.to == "anchor:a2"
                && edge.kind == MemoryGraphEdgeKind::SharedContext
        }));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn suppresses_dense_common_tag_hubs() {
        let directory = temp_dir();
        for index in 0..11 {
            memory_file(
                &directory.join(format!("core/{index}.md")),
                &format!("Memory {index}"),
            );
        }

        let graph = reconcile_memory_graph(memory_graph_sources("project"), &directory).unwrap();

        assert!(!graph.edges.iter().any(|edge| {
            edge.kind == MemoryGraphEdgeKind::SameTopic
                && edge.from.starts_with("memory:")
                && edge.to.starts_with("memory:")
        }));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn persists_explicit_memory_frontmatter_relations() {
        let directory = temp_dir();
        memory_file(&directory.join("core/two.md"), "Two");
        fs::write(
            directory.join("core/one.md"),
            "---\ndescription: One\ndepends_on:\n  - core/two.md\n---\nMemory",
        )
        .unwrap();

        let graph = reconcile_memory_graph(memory_graph_sources("project"), &directory).unwrap();

        assert!(graph.edges.iter().any(|edge| {
            edge.from == "memory:core/one.md"
                && edge.to == "memory:core/two.md"
                && edge.kind == MemoryGraphEdgeKind::DependsOn
        }));
        fs::remove_dir_all(directory).unwrap();
    }
}
