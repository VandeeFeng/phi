use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::index::{
    MemoryGraphConfidence as PersistedConfidence, MemoryGraphEdge as PersistedEdge,
    MemoryGraphEdgeKind as PersistedEdgeKind, MemoryGraphNode as PersistedNode, MemoryGraphRecord,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryGraphNodeKind {
    Anchor,
    MemoryFile,
    ProjectFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryGraphNodeView {
    pub id: String,
    pub kind: MemoryGraphNodeKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub search_text: String,
}

pub type MemoryGraphRelationKind = PersistedEdgeKind;
pub type MemoryGraphRelationConfidence = PersistedConfidence;
pub type MemoryGraphRelation = PersistedEdge;

#[derive(Debug, Clone, Default)]
pub struct MemoryGraph {
    nodes: BTreeMap<String, MemoryGraphNodeView>,
    edges: Vec<MemoryGraphRelation>,
}

#[derive(Debug, Clone)]
pub struct MemoryGraphQuery {
    pub prompt: String,
    pub seed_ids: Vec<String>,
    pub seed_scores: Vec<(String, f64)>,
    pub relation_filter: Vec<MemoryGraphRelationKind>,
    pub max_seeds: usize,
    pub max_depth: usize,
    pub hub_degree: Option<usize>,
}

impl Default for MemoryGraphQuery {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            seed_ids: Vec::new(),
            seed_scores: Vec::new(),
            relation_filter: Vec::new(),
            max_seeds: 3,
            max_depth: 2,
            hub_degree: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RankedMemoryGraphNode {
    pub node: MemoryGraphNodeView,
    pub score: f64,
    pub seed_score: f64,
    pub relation_score: f64,
    pub distance: usize,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryGraphSelection {
    pub seeds: Vec<String>,
    pub nodes: Vec<RankedMemoryGraphNode>,
    pub edges: Vec<MemoryGraphRelation>,
}

impl MemoryGraph {
    pub fn from_record(record: &MemoryGraphRecord) -> Self {
        let nodes = record.nodes.iter().map(project_node).collect();
        let edges = record.edges.clone();
        Self::new(nodes, edges)
    }

    pub fn new(nodes: Vec<MemoryGraphNodeView>, edges: Vec<MemoryGraphRelation>) -> Self {
        let nodes = nodes
            .into_iter()
            .filter_map(|node| {
                let id = normalize_id(&node.id);
                (!id.is_empty()).then(|| (id.clone(), MemoryGraphNodeView { id, ..node }))
            })
            .collect::<BTreeMap<_, _>>();
        let edges = normalize_edges(edges, &nodes);
        Self { nodes, edges }
    }

    pub fn nodes(&self) -> impl Iterator<Item = &MemoryGraphNodeView> {
        self.nodes.values()
    }

    pub fn edges(&self) -> &[MemoryGraphRelation] {
        &self.edges
    }

    pub fn select(&self, query: &MemoryGraphQuery) -> MemoryGraphSelection {
        let lexical_scores = self.seed_scores(&query.prompt);
        let weighted_seed_scores = self.weighted_seed_scores(&query.seed_scores);
        let seeds = self.pick_seeds(
            &lexical_scores,
            &query.seed_ids,
            &weighted_seed_scores,
            query.max_seeds,
        );
        if seeds.is_empty() {
            return MemoryGraphSelection::default();
        }

        let edges = self.filtered_edges(&query.relation_filter);
        let hub_degree = query.hub_degree.unwrap_or_else(|| self.hub_degree(&edges));
        let traversal = traverse(&seeds, &edges, query.max_depth, hub_degree);
        let mut nodes = traversal
            .distances
            .into_iter()
            .filter_map(|(id, distance)| {
                let node = self.nodes.get(&id)?.clone();
                let seed_score = lexical_scores.get(&id).copied().unwrap_or_default()
                    + weighted_seed_scores.get(&id).copied().unwrap_or_default();
                let relation_score = traversal
                    .relation_scores
                    .get(&id)
                    .copied()
                    .unwrap_or_default();
                Some(RankedMemoryGraphNode {
                    node,
                    score: seed_score + relation_score,
                    seed_score,
                    relation_score,
                    distance,
                })
            })
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| {
            right
                .score
                .total_cmp(&left.score)
                .then_with(|| left.distance.cmp(&right.distance))
                .then_with(|| left.node.label.cmp(&right.node.label))
                .then_with(|| left.node.id.cmp(&right.node.id))
        });

        MemoryGraphSelection {
            seeds,
            nodes,
            edges: traversal.edges,
        }
    }

    fn seed_scores(&self, prompt: &str) -> HashMap<String, f64> {
        let terms = search_terms(prompt);
        if terms.is_empty() {
            return HashMap::new();
        }
        let idf = self.inverse_document_frequency(&terms);
        self.nodes
            .values()
            .filter_map(|node| {
                let score = lexical_score(node, &terms, &idf);
                (score > 0.0).then(|| (node.id.clone(), score))
            })
            .collect()
    }

    fn inverse_document_frequency(&self, terms: &[String]) -> HashMap<String, f64> {
        let total = self.nodes.len() as f64;
        terms
            .iter()
            .map(|term| {
                let frequency = self
                    .nodes
                    .values()
                    .filter(|node| searchable_text(node).contains(term))
                    .count() as f64;
                (term.clone(), ((total + 1.0) / (frequency + 1.0)).ln() + 1.0)
            })
            .collect()
    }

    fn weighted_seed_scores(&self, seed_scores: &[(String, f64)]) -> HashMap<String, f64> {
        let mut scores = HashMap::new();
        for (id, score) in seed_scores {
            let id = normalize_id(id);
            if self.nodes.contains_key(&id) {
                scores
                    .entry(id)
                    .and_modify(|value: &mut f64| *value = value.max(*score))
                    .or_insert(*score);
            }
        }
        scores
    }

    fn pick_seeds(
        &self,
        scores: &HashMap<String, f64>,
        explicit_seed_ids: &[String],
        weighted_seed_scores: &HashMap<String, f64>,
        max_seeds: usize,
    ) -> Vec<String> {
        let limit = max_seeds.max(1);
        let mut seeds = Vec::new();
        for id in explicit_seed_ids.iter().map(|id| normalize_id(id)) {
            if self.nodes.contains_key(&id) && !seeds.contains(&id) {
                seeds.push(id);
            }
            if seeds.len() == limit {
                return seeds;
            }
        }

        let mut ranked = self
            .nodes
            .keys()
            .map(|id| {
                (
                    id,
                    scores.get(id).copied().unwrap_or_default()
                        + weighted_seed_scores.get(id).copied().unwrap_or_default(),
                )
            })
            .filter(|(_, score)| *score > 0.0)
            .collect::<Vec<_>>();
        ranked.sort_by(|(left_id, left_score), (right_id, right_score)| {
            right_score
                .total_cmp(left_score)
                .then_with(|| self.nodes[*left_id].label.cmp(&self.nodes[*right_id].label))
                .then_with(|| left_id.cmp(right_id))
        });
        for (id, _) in ranked {
            if !seeds.contains(id) {
                seeds.push(id.clone());
            }
            if seeds.len() == limit {
                break;
            }
        }
        seeds
    }

    fn filtered_edges(&self, kinds: &[MemoryGraphRelationKind]) -> Vec<MemoryGraphRelation> {
        if kinds.is_empty() {
            return self.edges.clone();
        }
        let allowed = kinds.iter().copied().collect::<BTreeSet<_>>();
        self.edges
            .iter()
            .filter(|edge| allowed.contains(&edge.kind))
            .cloned()
            .collect()
    }

    fn hub_degree(&self, edges: &[MemoryGraphRelation]) -> usize {
        let mut degrees = degree_map(edges).into_values().collect::<Vec<_>>();
        if degrees.is_empty() {
            return 50;
        }
        degrees.sort_unstable();
        let percentile = degrees[degrees.len() * 99 / 100];
        percentile.max(50)
    }
}

fn project_node(node: &PersistedNode) -> MemoryGraphNodeView {
    match node {
        PersistedNode::MemoryFile {
            id,
            path,
            title,
            description,
            tags,
            ..
        } => MemoryGraphNodeView {
            id: id.clone(),
            kind: MemoryGraphNodeKind::MemoryFile,
            label: title.clone(),
            search_text: format!("{path} {description} {}", tags.join(" ")),
        },
        PersistedNode::Anchor {
            id,
            label,
            search_text,
            ..
        } => MemoryGraphNodeView {
            id: id.clone(),
            kind: MemoryGraphNodeKind::Anchor,
            label: label.clone(),
            search_text: search_text.clone(),
        },
        PersistedNode::ProjectFile { id, path } => MemoryGraphNodeView {
            id: id.clone(),
            kind: MemoryGraphNodeKind::ProjectFile,
            label: path.clone(),
            search_text: String::new(),
        },
    }
}

fn normalize_edges(
    edges: Vec<MemoryGraphRelation>,
    nodes: &BTreeMap<String, MemoryGraphNodeView>,
) -> Vec<MemoryGraphRelation> {
    let mut seen = HashSet::new();
    let mut normalized = edges
        .into_iter()
        .filter_map(|edge| {
            let from = normalize_id(&edge.from);
            let to = normalize_id(&edge.to);
            if from.is_empty()
                || to.is_empty()
                || !nodes.contains_key(&from)
                || !nodes.contains_key(&to)
            {
                return None;
            }
            let key = (from.clone(), to.clone(), edge.kind);
            seen.insert(key)
                .then(|| MemoryGraphRelation { from, to, ..edge })
        })
        .collect::<Vec<_>>();
    normalized.sort_by(|left, right| {
        (
            &left.from,
            &left.to,
            left.kind,
            left.confidence,
            &left.reason,
        )
            .cmp(&(
                &right.from,
                &right.to,
                right.kind,
                right.confidence,
                &right.reason,
            ))
    });
    normalized
}

struct Traversal {
    distances: HashMap<String, usize>,
    relation_scores: HashMap<String, f64>,
    edges: Vec<MemoryGraphRelation>,
}

fn traverse(
    seeds: &[String],
    edges: &[MemoryGraphRelation],
    max_depth: usize,
    hub_degree: usize,
) -> Traversal {
    let degrees = degree_map(edges);
    let seed_set = seeds.iter().collect::<HashSet<_>>();
    let mut adjacency = HashMap::<&str, Vec<(&MemoryGraphRelation, &str)>>::new();
    for edge in edges {
        adjacency
            .entry(&edge.from)
            .or_default()
            .push((edge, &edge.to));
        adjacency
            .entry(&edge.to)
            .or_default()
            .push((edge, &edge.from));
    }

    let mut distances = seeds
        .iter()
        .cloned()
        .map(|id| (id, 0))
        .collect::<HashMap<_, _>>();
    let mut relation_scores = HashMap::<String, f64>::new();
    let mut queue = seeds.iter().cloned().collect::<VecDeque<_>>();
    let mut selected_edges = Vec::new();
    let mut selected_edge_keys = HashSet::new();

    while let Some(node_id) = queue.pop_front() {
        let distance = distances[&node_id];
        if distance == max_depth
            || (!seed_set.contains(&node_id)
                && degrees.get(&node_id).copied().unwrap_or_default() >= hub_degree)
        {
            continue;
        }
        for (edge, neighbor) in adjacency.get(node_id.as_str()).into_iter().flatten() {
            let next_distance = distance + 1;
            let contribution = relation_weight(edge) / next_distance as f64;
            let is_new = !distances.contains_key(*neighbor);
            if is_new {
                distances.insert((*neighbor).to_string(), next_distance);
                queue.push_back((*neighbor).to_string());
            }
            if distances.get(*neighbor) == Some(&next_distance) {
                relation_scores
                    .entry((*neighbor).to_string())
                    .and_modify(|score| *score = score.max(contribution))
                    .or_insert(contribution);
            }
            let key = (&edge.from, &edge.to, edge.kind);
            if selected_edge_keys.insert(key) {
                selected_edges.push((*edge).clone());
            }
        }
    }
    selected_edges.sort_by(|left, right| {
        (&left.from, &left.to, left.kind).cmp(&(&right.from, &right.to, right.kind))
    });
    Traversal {
        distances,
        relation_scores,
        edges: selected_edges,
    }
}

fn degree_map(edges: &[MemoryGraphRelation]) -> HashMap<String, usize> {
    let mut degrees = HashMap::new();
    for edge in edges {
        *degrees.entry(edge.from.clone()).or_default() += 1;
        *degrees.entry(edge.to.clone()).or_default() += 1;
    }
    degrees
}

fn relation_weight(edge: &MemoryGraphRelation) -> f64 {
    let relation = match edge.kind {
        MemoryGraphRelationKind::Continues => 80.0,
        MemoryGraphRelationKind::SharedContext => 35.0,
        MemoryGraphRelationKind::AccessedFile => 30.0,
        MemoryGraphRelationKind::DependsOn | MemoryGraphRelationKind::Supersedes => 25.0,
        MemoryGraphRelationKind::Mentions | MemoryGraphRelationKind::SameTopic => 8.0,
    };
    let confidence = match edge.confidence {
        MemoryGraphRelationConfidence::Extracted => 1.0,
        MemoryGraphRelationConfidence::Inferred => 0.7,
        MemoryGraphRelationConfidence::Ambiguous => 0.4,
    };
    relation * confidence * edge.weight.max(0.0)
}

fn lexical_score(node: &MemoryGraphNodeView, terms: &[String], idf: &HashMap<String, f64>) -> f64 {
    let label = normalize_text(&node.label);
    let text = searchable_text(node);
    let full_query = terms.join(" ");
    let mut score = if !full_query.is_empty() && label == full_query {
        100.0
    } else if !full_query.is_empty() && label.starts_with(&full_query) {
        50.0
    } else {
        0.0
    };
    let mut matched = 0;
    let mut tiered = 0.0;
    for term in terms {
        let weight = idf.get(term).copied().unwrap_or(1.0);
        if label == *term {
            tiered += 10.0 * weight;
            matched += 1;
        } else if label.starts_with(term) {
            tiered += 5.0 * weight;
            matched += 1;
        } else if text.contains(term) {
            score += weight;
            matched += 1;
        }
    }
    if tiered > 0.0 {
        score += tiered * (matched as f64 / terms.len() as f64).powi(2);
    }
    score
}

fn searchable_text(node: &MemoryGraphNodeView) -> String {
    normalize_text(&format!("{} {}", node.label, node.search_text))
}

fn normalize_id(value: &str) -> String {
    value.trim().to_string()
}

fn normalize_text(value: &str) -> String {
    value.trim().to_lowercase()
}

fn search_terms(value: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut word = String::new();
    for character in value.to_lowercase().chars() {
        if character.is_alphanumeric() || character == '_' {
            word.push(character);
        } else if !word.is_empty() {
            terms.push(std::mem::take(&mut word));
        }
    }
    if !word.is_empty() {
        terms.push(word);
    }
    terms.sort();
    terms.dedup();
    terms
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, label: &str) -> MemoryGraphNodeView {
        MemoryGraphNodeView {
            id: id.to_string(),
            kind: MemoryGraphNodeKind::Anchor,
            label: label.to_string(),
            search_text: String::new(),
        }
    }

    fn edge(from: &str, to: &str, kind: MemoryGraphRelationKind) -> MemoryGraphRelation {
        MemoryGraphRelation {
            from: from.to_string(),
            to: to.to_string(),
            kind,
            origin: crate::extension::memory::graph::MemoryGraphEdgeOrigin::Derived,
            confidence: MemoryGraphRelationConfidence::Extracted,
            weight: 1.0,
            reason: "test".to_string(),
        }
    }

    #[test]
    fn loads_persisted_memory_graph_relations() {
        let mut record = MemoryGraphRecord::default();
        record.nodes = vec![
            PersistedNode::Anchor {
                id: "anchor:a".to_string(),
                anchor_id: "a".to_string(),
                label: "graph".to_string(),
                search_text: String::new(),
                source: "anchors.jsonl".to_string(),
            },
            PersistedNode::Anchor {
                id: "anchor:t".to_string(),
                anchor_id: "t".to_string(),
                label: "memory".to_string(),
                search_text: String::new(),
                source: "anchors.jsonl".to_string(),
            },
        ];
        record
            .edges
            .push(crate::extension::memory::graph::MemoryGraphEdge {
                from: "anchor:a".to_string(),
                to: "anchor:t".to_string(),
                kind: PersistedEdgeKind::Continues,
                origin: crate::extension::memory::graph::MemoryGraphEdgeOrigin::Source,
                confidence: PersistedConfidence::Extracted,
                weight: 1.0,
                reason: "chronological anchors".to_string(),
            });

        let graph = MemoryGraph::from_record(&record);
        let selected = graph.select(&MemoryGraphQuery {
            seed_ids: vec!["anchor:a".to_string()],
            max_depth: 1,
            ..MemoryGraphQuery::default()
        });

        assert!(selected.nodes.iter().any(|node| node.node.id == "anchor:t"));
        assert_eq!(selected.edges[0].kind, PersistedEdgeKind::Continues);
    }

    #[test]
    fn normalizes_and_deduplicates_directed_edges() {
        let graph = MemoryGraph::new(
            vec![node("a", "A"), node("b", "B")],
            vec![
                edge(" a ", "b", MemoryGraphRelationKind::Continues),
                edge("a", "b", MemoryGraphRelationKind::Continues),
                edge("b", "a", MemoryGraphRelationKind::Continues),
                edge("a", "missing", MemoryGraphRelationKind::Continues),
            ],
        );

        assert_eq!(graph.edges().len(), 2);
        assert_eq!(graph.edges()[0].from, "a");
        assert_eq!(graph.edges()[1].from, "b");
    }

    #[test]
    fn ranks_full_prompt_match_before_partial_match() {
        let graph = MemoryGraph::new(
            vec![node("one", "tape relation engine"), node("two", "tape")],
            Vec::new(),
        );

        let result = graph.select(&MemoryGraphQuery {
            prompt: "tape relation engine".to_string(),
            ..MemoryGraphQuery::default()
        });

        assert_eq!(result.seeds[0], "one");
        assert!(result.seeds.contains(&"two".to_string()));
        assert_eq!(result.nodes[0].node.id, "one");
    }

    #[test]
    fn respects_the_seed_limit() {
        let graph = MemoryGraph::new(
            vec![node("tape", "tape"), node("engine", "relation engine")],
            Vec::new(),
        );

        let result = graph.select(&MemoryGraphQuery {
            prompt: "tape engine".to_string(),
            max_seeds: 1,
            ..MemoryGraphQuery::default()
        });

        assert_eq!(result.seeds, vec!["tape"]);
    }

    #[test]
    fn explicit_seed_survives_unrelated_prompt() {
        let graph = MemoryGraph::new(
            vec![node("head", "current task"), node("other", "database")],
            Vec::new(),
        );

        let result = graph.select(&MemoryGraphQuery {
            prompt: "database".to_string(),
            seed_ids: vec!["head".to_string()],
            ..MemoryGraphQuery::default()
        });

        assert!(result.seeds.contains(&"head".to_string()));
    }

    #[test]
    fn combines_behavior_seed_scores_with_prompt_scores() {
        let graph = MemoryGraph::new(
            vec![
                node("recent", "recent file"),
                node("relevant", "target file"),
            ],
            Vec::new(),
        );

        let result = graph.select(&MemoryGraphQuery {
            prompt: "target".to_string(),
            seed_scores: vec![("recent".to_string(), 10_000.0)],
            ..MemoryGraphQuery::default()
        });

        assert_eq!(result.nodes[0].node.id, "recent");
        assert!(result.nodes.iter().any(|node| node.node.id == "relevant"));
    }

    #[test]
    fn filters_relations_and_suppresses_hub_transit() {
        let graph = MemoryGraph::new(
            vec![
                node("seed", "target"),
                node("hub", "hub"),
                node("leaf", "leaf"),
                node("ignored", "ignored"),
            ],
            vec![
                edge("seed", "hub", MemoryGraphRelationKind::Continues),
                edge("hub", "leaf", MemoryGraphRelationKind::Continues),
                edge("seed", "ignored", MemoryGraphRelationKind::SameTopic),
            ],
        );

        let result = graph.select(&MemoryGraphQuery {
            prompt: "target".to_string(),
            relation_filter: vec![MemoryGraphRelationKind::Continues],
            max_depth: 3,
            hub_degree: Some(2),
            ..MemoryGraphQuery::default()
        });
        let selected = result
            .nodes
            .iter()
            .map(|item| item.node.id.as_str())
            .collect::<HashSet<_>>();

        assert!(selected.contains("hub"));
        assert!(!selected.contains("leaf"));
        assert!(!selected.contains("ignored"));
    }
}
