# Unified Memory Graph Design

This document defines the project-level `{project}__graph.json` materialized view and shared graph analysis engine.

## Goal

Graph semantics are the foundation of the memory subsystem. A single project graph connects:

- Markdown memory files;
- ordinary tape anchors;
- project files discovered through recent tool activity.

`graph.json` is a rebuildable materialized graph. Markdown and anchor JSONL are its canonical sources.

```text
Markdown files + anchors.jsonl
              |
              v
     MemoryGraphBuilder
              |
              v
          graph.json
              |
              v
     MemoryGraph query engine
              |
              v
 memory_search + smart context
```

## Design principles

1. **Sources own facts.** Markdown and anchor JSONL remain canonical.
2. **The graph owns materialized topology.** It stores graph-relevant projections and relations, not transcript content.
3. **Queries own transient ranking.** Prompt scores, recency, traversal distance, and final relevance are not persisted.
4. **One graph model.** Memory search, tape search, smart context use the same node IDs, edge kinds, and traversal engine.
5. **Rebuildable by default.** Corrupt or incompatible generated graph data can be reconstructed from sources.
6. **Deterministic output.** Identical canonical sources produce identical graph content.
7. **Keep the implementation compact.** Graph modules are separated by schema/projection, storage, and query ownership; no nested schema/builder/query hierarchy is needed.

## Canonical sources and ownership

| Source | Owns | Graph projection |
|---|---|---|
| `core/**/*.md` | memory content and frontmatter | `MemoryFile` nodes and explicit/derived memory relations |
| `{project}__anchors.jsonl` | anchor identity and metadata | `Anchor` nodes |
| session transcripts | conversation evidence referenced by anchors | optional derived anchor/file evidence; transcript content is never copied |

Prompt-time queries may extend this persisted read model with transient evidence. Recent file activity contributes temporary file nodes, seed scores, and `AccessedFile` relations. These additions participate in `MemoryGraph::select` but are never written to `graph.json`.

## Persisted graph model

Markdown metadata is stored in `MemoryFile` nodes, with no separate `memoryFiles` index section.

```rust
pub struct MemoryGraphRecord {
    pub sources: MemoryGraphSources,
    pub source_state: MemoryGraphSourceState,
    pub nodes: Vec<MemoryGraphNode>,
    pub edges: Vec<MemoryGraphEdge>,
}
```

### Sources

```rust
pub struct MemoryGraphSources {
    pub memory_root: String,
    pub anchors: String,
}

pub struct MemoryGraphSourceState {
    pub memory_fingerprint: String,
    pub anchors_fingerprint: Option<String>,
}
```

Source paths are project-memory-relative and validated against the project memory directory. Fingerprints describe canonical inputs, not the generated graph file.

### Nodes

```rust
pub enum MemoryGraphNode {
    MemoryFile {
        id: String,
        path: String,
        title: String,
        description: String,
        tags: Vec<String>,
        fingerprint: String,
    },
    Anchor {
        id: String,
        anchor_id: String,
        label: String,
        search_text: String,
        source: String,
    },
    ProjectFile {
        id: String,
        path: String,
    },
}
```

Stable ID namespaces:

```text
memory:<normalized-memory-path>
anchor:<anchor-id>
file:<normalized-project-path>
```

Normalization must reject empty IDs, escaped paths, kind/prefix mismatches, duplicate IDs, self-edges, and dangling endpoints.

### Edges

```rust
pub struct MemoryGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: MemoryGraphEdgeKind,
    pub origin: MemoryGraphEdgeOrigin,
    pub confidence: MemoryGraphConfidence,
    pub weight: f64,
    pub reason: String,
}

pub enum MemoryGraphEdgeOrigin {
    Source,
    Derived,
}
```

Relation vocabulary:

```rust
pub enum MemoryGraphEdgeKind {
    Continues,
    AccessedFile,
    Mentions,
    DependsOn,
    Supersedes,
    SameTopic,
    SharedContext,
}
```

- `Source` edges are direct projections of anchors or explicit Markdown metadata.
- `Derived` edges are reproducible results of the configured graph algorithm.
- Manual non-rebuildable relationships are out of scope until they have their own canonical source.

## Relation construction

Additional inferred relationships should use conservative, explainable evidence:

- chronological anchors in the same session produce `Continues`;
- matching normalized metadata keywords may produce bounded `SameTopic` edges;
- anchor text mentioning a memory path produces `Mentions`;
- shared explicit file or memory associations may produce `SharedContext`.

Do not connect every anchor sharing a common keyword. Apply per-node top-K limits, minimum confidence, deterministic tie-breaking, and hub suppression.

Transcript-derived file access is transient query evidence with bounded scan windows; it is not a canonical graph source.

### Memory files

Memory nodes store searchable metadata and participate in relations:

- explicit frontmatter references produce `DependsOn`, `Supersedes`, or `Mentions`;
- rare shared tags may produce bounded `SameTopic` edges;
- description/title lexical similarity may produce low-weight `SameTopic` edges;
- common anchor associations may produce `SharedContext`.

Relation construction uses explicit metadata and bounded lexical evidence. Embedding/model-dependent relations are outside this deterministic, provider-independent model.

## Persisted versus query-time data

Persisted graph data:

- stable node identity and source locators;
- graph-relevant labels and compact search text;
- explicit Markdown relations;
- deterministic bounded derived relations;
- relation origin, confidence, weight, and explanation.

Stable anchor, memory, and smart-context relations are computed during graph reconciliation and persisted. Runtime consumers load this projection instead of repeatedly reading all canonical sources and rebuilding equivalent relations.

Query-time data:

- prompt lexical/BM25 score;
- explicit seed score;
- recency and current-session boost;
- traversal distance and relation propagation score;
- hub penalties and final ranking;
- selected subgraph.

This keeps reads side-effect-free, avoids rewriting the graph for each prompt, and removes repeated runtime relation analysis. Runtime work is limited to prompt-dependent seed selection, bounded traversal, and ranking over the persisted topology.

## Build and reconciliation lifecycle

```text
MemoryService::create
        │
        └── attach optional TapeService without materializing graph.json

First graph consumer, conditional startup warmup, or explicit refresh
        │
        ├── fingerprint canonical sources
        ├── return cached graph when unchanged
        ├── load Markdown nodes
        ├── load anchor nodes
        ├── derive bounded relations
        ├── normalize and validate
        ├── best-effort save when a retrieval rebuilds a non-empty graph
        └── strictly persist/remove only for explicit refresh operations
```

Requirements:

1. Shell and Skill initialization never create `graph.json`.
2. `MemoryService` construction never materializes the graph or creates Memory directories, including when Tape is enabled. Empty-source reads and refreshes also leave missing directories untouched; directories are created only for persistence. Opening an existing Tape store acquires the project lock. Smart Tape may proactively become the first Graph consumer during background startup warmup.
3. Markdown and Anchor JSONL are optional and independently projectable, regardless of whether Tape is enabled.
5. Reconciliation sorts nodes and edges before serialization.
6. Empty graphs remain in memory and are not persisted; stale generated files are removed when all projected nodes disappear.
7. Unchanged sources must not rewrite `graph.json`.
8. Retrieval returns a successfully reconciled in-memory Graph even when optional materialization fails; explicit refresh/reset remains strict.
9. Store writes and removals need cross-process coordination; an instance-local mutex is insufficient.
10. Warmup retains activity evidence, not a frozen candidate list; prompt-time selection derives candidates from the current source-validated Graph.

## Query architecture

`memory/graph/query.rs` provides the shared Memory-wide traversal API:

```text
MemoryGraph
MemoryGraphNodeView
MemoryGraphRelation
MemoryGraphQuery
MemoryGraphSelection
```

The in-memory graph converts persisted nodes into lookup maps once, then provides:

```rust
impl MemoryGraph {
    pub fn select(&self, query: &MemoryGraphQuery) -> MemoryGraphSelection;
}
```

The same query engine serves:

- project/global memory search;
- tape anchor relation search;
- smart memory and project-file selection;

Relation traversal remains bounded, deterministic, relation-filtered, and hub-aware. Edge direction is preserved in results even when a query allows bidirectional traversal.

## Failure and recovery semantics

Because generated graph content is rebuildable:

- missing graph: reconcile lazily when a Graph consumer requests it;
- malformed graph: preserve or quarantine the bad file for diagnostics, then rebuild;
- missing source classes: build from any available Markdown or Anchor JSONL sources;
- dangling or inconsistent generated relation: reject the generated snapshot and rebuild;
- canonical source parse error: report the source path and retain the last valid graph instead of overwriting it with partial data.

A graph rebuild must never mutate Markdown, anchor JSONL, or transcripts.

## Compact code layout

Graph responsibilities are organized in these modules:

```text
memory/
├── graph/
│   ├── mod.rs           # compact public surface
│   ├── index.rs         # schema, source projection, relation materialization
│   ├── store.rs         # atomic persistence and cross-process locking
│   ├── query.rs         # bounded ranking and traversal
│   ├── bm25.rs          # Chinese/mixed-language Markdown BM25
│   └── activity.rs      # selected-file focus ranges only
├── service.rs           # lifecycle, cache, unified graph access
├── tools.rs             # tool adapters only
└── docs/unified-memory-graph-design.md
```

The graph folder is split at durable ownership boundaries, without additional schema/builder/query subdirectories. Memory repository initialization sets up Markdown sources; graph materialization belongs to `MemoryService` consumers.

## Invariants

- `graph.json` contains MemoryFile and Anchor nodes.
- Memory metadata search uses `MemoryFile` graph nodes.
- Tape relation search uses the same graph query engine.
- Smart context can select memory files through graph relations.
- Unchanged sources produce byte-identical graph output and no Git changes.
- Missing or corrupt generated graphs recover without losing canonical source data.
- Graph traversal remains bounded and deterministic.
- Graph construction, storage, and queries remain in the compact `memory/graph/` module layout.
