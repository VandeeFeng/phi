# Tape Design

Tape mode is an **anchor-indexed conversation and task-memory system**. Transcript entries are read-only evidence; local anchor indexes provide durable retrieval metadata.

## Design Philosophy
**"On-demand memory, intelligent retrieval"**

Each new agent conversation lacks continuous memory, and larger context windows do not automatically mean more effective context. Phi's memory subsystem therefore treats input quality, indexing, and deliberate retrieval as more important than dumping more history into the prompt.

The memory model is intentionally split like a computer system:
- **Context window as RAM**: fast, temporary, limited, and not always reliable.
- **Markdown memory files as disk**: persistent, grep-able, human-editable, and easier to organize.
- **Anchors and metadata as the index**: a compact entry point that helps both humans and agents locate the right details only when needed.

This is human-first as much as agent-first. The files should remain useful even without an agent: the user can grep tags, anchors, descriptions, and paths manually, build their own mental model, and then pass selected context to the agent. Agent retrieval should assist this workflow, not replace it.

Tape mode is inspired by:
- **LSTM memory** - Sequential context with checkpoint gates
- **Git workflow** - Anchors as commits, conversation as branches
- **Letta memory** - Explicit memory operations and tools

Tape mode stores anchors as points within Phi transcript entries, using them as the source of truth. Context injection then selects relevant memory files and recently active project files based on configured strategy. Recent file activity influences graph ranking internally; concrete historical line ranges are not injected. The active runtime creates anchors through `tape_anchor`; it does not automatically create session lifecycle anchors. When `mode: "manual"` is set, `tape_anchor` use the existing permission confirmation flow. Keyword detection independently authorizes only the matching `tape_anchor`; it does not require Manual confirmation. The agent may refuse a suggested keyword anchor when unnecessary. This combination of anchors and keywords balances the agent's autonomy with user control.

The design separates two memory horizons:
- **Immediate injection**: SDK prompt hooks prepare Memory/Tape context on demand for turn-scoped injection through `message-append`.
- **Durable memory**: the `memory-digest` skill turns recent tape anchors and selected session context into confirmed long-term Markdown memory updates.

Phi's session picker derives inline anchor labels from persisted anchors and their `sessionEntryId`; Tape does not mutate transcript nodes to store those labels. `/memory-review` provides a separate human-facing overlay for browsing, searching, deleting, and jumping to anchors without asking the agent to search for every anchor.

### Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│                     LLM Agent Layer                      │
│  - Uses tape tools to query session history              │
│  - Creates anchors for phase transitions                 │
│  - Decides what context to retrieve                      │
└─────────────────────────────────────────────────────────┘
                          ↓
┌─────────────────────────────────────────────────────────┐
│                   Tape Service Layer                     │
│  - Reads persisted Phi transcript files (JSONL)          │
│  - Maintains anchor stores (local JSONL)│
│  - Provides evidence reads, graph search, context selection │
└─────────────────────────────────────────────────────────┘
                          ↓
┌─────────────────────────────────────────────────────────┐
│                    Storage Layer                         │
│  - Session entries: Phi transcript file (read-only)     │
│  - Anchor stores: {projectMemoryDir}/        │
└─────────────────────────────────────────────────────────┘
```

## Unified Memory Graph

Tape retains dedicated canonical stores while participating in the project-wide
materialized Memory Graph.

```text
Transcript entries ── evidence for tape_read
        │
        ├── anchors.jsonl ── canonical anchor records
        └── graph.json ── rebuildable project-wide retrieval topology
```

`graph.json` projects anchors, Markdown memory files and their stable relations.
Canonical anchor writes are atomic and share an advisory lock on the project
Memory directory. Session identity records provenance, not transaction scope.

The shared query engine loads persisted graph topology, ranks lexical and
explicit seeds, and performs bounded relation-filtered traversal with a hub
cutoff. Prompt scores, current recency boosts, traversal distances, and selected
subgraphs remain query-time data; stable anchor, memory, and smart-mode
relations are materialized during graph reconciliation.

### Graph responsibilities

- `memory_search(mode="tape", query=..., includeRelations=true)` discovers ranked anchors and a bounded relation subgraph. Use `tape_read` afterwards to read
  transcript evidence.
- Smart memory selection queries persisted MemoryGraph relations and reads tape
  entries only to derive focus ranges for already selected files.
- Anchor deletion changes canonical storage; MemoryGraph reconciles from sources.

## Core Components

### 1. Session Reader (`tape/reader.rs`)

Reads entries directly from persisted Phi transcript files:

```rust
get_session_file_paths(cwd) -> Result<Vec<PathBuf>>
get_session_file_path(cwd, session_id) -> Result<Option<PathBuf>>
parse_session_file(path) -> Result<Option<ParsedSessionFile>>
```

**Entry types** (from `TranscriptEntryKind`):
- `message` - User, context, assistant, system, and tool-result messages
- `custom` - Phi transcript events
- `compaction` - Session compactions

### 2. Anchor Store (`tape/store.rs`)

Local store of anchor checkpoints:

```typescript
// Storage: {projectMemoryDir}/{projectName}__anchors.jsonl
interface TapeAnchor {
  id: string;             // Stable anchor id
  timestamp: string;      // ISO timestamp
  name: string;           // Anchor name (e.g., "task/begin", "task/complete")
  type: "normal";
  meta?: {
    trigger?: "direct" | "keyword" | "manual";
    keywords?: string[];
    summary?: string;
    purpose?: string; // 1-2 word label (e.g., "feature", "review", "deploy")
    git?: {
      headOid?: string;      // User HEAD when the anchor was created
      commitOid: string;     // Hidden Phi commit containing the workspace state
    };
    workspace?: string;      // git:main, git:worktrees/<name>, or directory:<root>
  };
  sessionId: string;      // Session ID
  sessionEntryId: string; // Related session entry ID
}
```

Anchor JSONL records serialize the Rust `TapeAnchor` fields: `id`, `name`, `type`, `sessionId`, `sessionEntryId`, `timestamp`, and optional `meta`. Finalization always records `meta.workspace`, including non-Git directories. In Git workspaces it additionally snapshots tracked and untracked non-ignored files through a temporary index, stores the hidden commit under `refs/phi/tape/`, and adds its OID to `meta.git`. It never changes the user's HEAD, Git branch, index, or worktree. Diffs remain derived data: compare any two Anchor `commitOid` values with Git instead of persisting patches.

**Key methods:**
- `append(entry)` - Add a new anchor to the JSONL store
- `remove_by_id(id)` - Delete an anchor and rewrite the JSONL store
- `get_all_anchors()` - Return all persisted anchors
- `scan(options)` - Filter by id/name/session/session-entry/type/time/meta fields
- `search(options)` - Apply user-facing limit handling over `scan`
- `clear()` - Remove all anchors from the active store

### 3. Tape Service (`tape/service.rs`)

Main service combining session reading and anchor management:

```rust
impl TapeService {
    fn record_anchor_at_entry(session_id, entry_id, anchor) -> Result<()>;
    fn delete_anchor(id) -> Result<bool>;
    fn delete_anchor_entries(ids) -> Result<Vec<TapeAnchor>>;

    fn scan_persisted_entries(options) -> Result<Vec<TranscriptEntry>>;
    fn search_anchors_with_fallback(options) -> Result<Vec<TapeAnchor>>;
    fn get_anchor_store() -> &AnchorStore;
}
```

### 4. Tape Context (`tape/context.rs`)

**ConversationSelector**: Helper for formatting/reducing session entries
- Token budget filtering (default: 1000 tokens, 40 entries)
- Can format selected session entries into compact context text
- Exists as an internal helper; current runtime injection is driven by `MemoryFileSelector`

**MemoryFileSelector**: Intelligently selects memory and project files
- **Smart strategy**: Combines persisted MemoryGraph evidence with transient `read` / `edit` / `write` activity nodes, behavior seed scores, and `AccessedFile` relations before one bounded graph selection
- **Activity scoring**: Reuses tool weights, recency decay, repeat dampening, tool-diversity bonus, and anchor-window boost without exposing line ranges in injected context
- **Keyword anchor helper**: Normalizes configured keywords and builds a hidden anchor instruction when a user prompt in the `[10, 300]` character range matches a keyword. During the active in-memory session, it suppresses a repeated match when the same keyword has the same nearest two context units on both sides.
- **Recent-only strategy**: Scans memory files and returns the most recently modified files

### 5. Tape Tools (`memory/tools.rs`)

Phi registers the shared memory tools plus `tape_anchor`, `tape_info`, `tape_read`, `tape_delete`, `tape_reset` when Tape is enabled.

Tape mutation decisions are centralized in `tape/gate.rs`:

- `TapeMutationGate` is constructed for the active runtime from resolved memory settings, shared session mutation state, and the current `PermissionRuntime`.
- `TapeAnchorTool` holds the shared gate for settings and permission decisions.
- `authorize_anchor(name)` first checks trusted active-turn keyword state; an exact match resolves to `keyword`, otherwise Manual mode uses the existing permission flow and auto mode resolves to `direct`.
- The staged-anchor finalizer remains session-scoped. Before Anchor persistence, `TapeService` creates its Git checkpoint when the working directory belongs to a repository. It consumes keyword state only after all durable persistence succeeds and preserves it when persistence fails so the same turn can retry.

## Tape Tools Reference

### tape_anchor - Create Anchor Checkpoint

```typescript
tape_anchor(
  name: string,        // Anchor name (e.g., "task/begin", "task/complete", "anchor")
  summary?: string,    // Brief intent summary, under 18 words
  purpose?: string     // 1-2 word label for the anchor purpose
)
```

**When to use:**
- Starting a new task or phase
- Completing a milestone
- Before major context shifts
- After important decisions

**Example:**
```typescript
// Phase transition
tape_anchor(name="task/begin", summary="Starting database migration")

// Keyword-authorized anchor generated from a hidden instruction
// The model only supplies name / summary / purpose.
tape_anchor(
  name="anchor/keyword-migration",
  summary="Continue the database migration work",
  purpose="migration"
)
```

---

### tape_delete - Delete Anchor Checkpoint

```typescript
tape_delete(
  id?: string,       // Single anchor id from tape-mode memory_search
  ids?: string[],    // Multiple exact anchor ids from tape-mode memory_search
)
```

**Use when:**
- removing an anchor and its derived session-picker label
- cleaning up stale normal anchors

---

### tape_info - Get Tape Statistics

```typescript
tape_info()
```

**Returns:**
```text
tape anchors: 3
```

---

### tape_read - Read Conversation History

```typescript
tape_read({
  sinceAnchor?: string,              // Read after this anchor name
  lastAnchor?: boolean,              // Read after last anchor
  betweenAnchors?: { start, end },   // Between two anchors
  betweenDates?: { start, end },     // ISO date range
  scan?: string,                     // Text scan over formatted entry JSON
  types?: ("message" | "custom" | "compaction")[],
  entryScope?: "session" | "project", // Default: project
  anchorScope?: "session" | "project", // Default: session
  limit?: number,                    // Max entries (default: 20, max: 100)
  maxContentChars?: number | null    // Default: 300, null returns full formatted content
})
```

**Common patterns:**
```typescript
// Everything since last anchor
tape_read({ lastAnchor: true })

// Context since task started
tape_read({ sinceAnchor: "task/begin" })

// Search messages
tape_read({ scan: "database schema", limit: 10 })

// Date range
tape_read({ betweenDates: { start: "2026-04-01", end: "2026-04-16" } })
```

---

### memory_search with mode=tape - Discover Anchors and Relations

```typescript
memory_search({
  mode: "tape",
  query?: string,              // Rank anchors
  session_id?: string,
  name_contains?: string,
  summary_contains?: string,
  purpose_contains?: string,
  limit?: number,              // Default 20, maximum 100
  includeRelations?: boolean,
  relationDepth?: number,      // Maximum 4
  include_orphans?: boolean,
})
```

When `includeRelations` is true, MemoryGraph returns a bounded ranked relation
subgraph over anchors and memory files. Use `tape_read` for transcript entries; tape-mode
`memory_search` does not scan entries itself.

**Example:**
```typescript
// Find anchors matching a task
memory_search({ mode: "tape", query: "bug" })

// Find normal anchors by metadata
memory_search({ mode: "tape", purpose_contains: "migration" })

// Find transcript entries after selecting anchors
// Use tape_read({ scan: "memory" })
```

---

### tape_reset - Reset Active Tape

```typescript
tape_reset()
```

**Behavior:** Clear canonical anchors and immediately reconcile MemoryGraph.
Markdown memory remains projected. Reset does not create a replacement anchor.

## Runtime Flow

Tape mode separates runtime setup from SDK-owned prompt preparation, shared by application and direct SDK hosts that install `MemoryHooks`.

```text
Prepared AgentInstance construction
       ↓
RuntimeSessionServices
  - create MemoryService
  - attach TapeService when the Tape gate passes
  - load persisted anchor store
       ↓
ToolRegistry construction
  - create shared TapeMutationGate
  - register memory_search and Tape tools
       ↓
Application Session boundary
  - emit SessionStart through matching SDK resources
  - run configured SessionStart Git actions (default pull; skipped for New/Resume)
  - optionally defer completion until lifecycle readiness
```

```text
AgentInstance input preparation
  - validate input and derive keyword candidates from original user-authored text
  - await generation lifecycle readiness
       ↓
UserPromptSubmit
  - run configured userPromptSubmit Git actions for non-delegated input (default none)
  - transform or handle input
  - strip /memory-inject when present
       ↓
Optional application command expansion
  - intercept through UserPromptExpansion
  - expand recognized Prompt/Skill commands
       ↓
PromptContext
  - attempt eligible automatic Memory injection once per registration lifetime
  - handle explicit /memory-inject through a separate repeatable handler
  - precompute Tape activity and build Memory/Graph context
    from transformed/expanded text on the blocking pool
  - contribute a separate instruction for an unseen keyword candidate
       ↓
Instance admission
  - validate merged tool policy
  - record keyword signatures and accept context/notices on Started or Queued
  - preserve keyword retry eligibility on handled, rejected or cancelled preparation
  - never restore a started automatic injection attempt
       ↓
Dispatch / canonical commit
  - activate trusted keyword state for the actual turn or Steer
  - append prompt/context Transcript entries
  - start or continue the agent run
```

`MemoryHooks` uses generation-scoped lifecycle readiness, not an AgentSession precompute cache. SessionStart is blocking by default; `background_startup()` starts owned work whose completion is awaited by prompt preparation or explicit readiness. SessionEnd does not await startup readiness; end hooks run within their own timeout and the shared SessionEnd budget. Startup and optional submission synchronization are independent [Git action lists](../../../../../docs/design-sdk.md#memory-and-tape); injection Once limits neither. Eligible prompt preparation selects from the current source-validated Graph and activity evidence, without a retained startup candidate list.

Subagents skip automatic Memory/Tape retrieval and injection through `message-append`; they never copy the parent's injected context. Read-only Memory/Tape tools remain available in the child's working directory; Tape mutations and mixed read/write `memory_sync` are denied. The master is the primary conversation Agent, not a privileged role. Original user-authored `@agent` input follows the master's normal injection and keyword authorization; generated child tasks and Agent definitions create neither child injection nor trusted keyword evidence.

Injection contributes typed `InjectedContext`, committed as context Transcript entries without another model request. Automatic and explicit injection use separate ordinary Rust handlers; only the automatic handler has `once: true`. Its lifetime follows [Hook Once](../../../../../docs/design-hook.md#once-and-registration-lifetime), not context acceptance or persisted markers. Tape keyword authorization retains separate admission accounting.

Only recognized transient Git fetch/clone/push failures retry, up to three total attempts. Parsing, precomputation and context building do not retry. Automatic Memory build failure may warn but its one-shot attempt remains consumed; explicit injection failure rejects input. See [SDK Memory](../../../../../docs/design-sdk.md#memory-and-tape) for the shared contract.

- Automatic injection requires `injection: "message-append"`; omitted or `false` disables it, not Memory tools, synchronization, or keyword authorization.
- `message-append`: the first eligible automatic handler execution consumes the [ordinary Hook one-shot allowance](../../../../../docs/design-hook.md#once-and-registration-lifetime). Empty output, error, timeout, cancellation after start or discarded/rejected preparation does not restore it.
- `/memory-inject [prompt]`: rebuilds through the same builder even when automatic injection is disabled or consumed. The automatic handler excludes these explicit requests, so manual injection does not consume its allowance. The optional transformed prompt drives selection; an omitted or blank argument provides no selection query or fabricated task. Preparation uses the owning Instance's directory and settings. Explicit injection adds a retrieval hint for `memory_search` and `tape_read`; automatic injection does not.
- An unseen keyword-context match adds a separate `ContextSource::TapeKeywordIntro` context on that matching turn and stores the same trusted instruction in the instance-bound Memory extension runtime for exact anchor authorization. Context signatures are process-local session state: they are not persisted or reconstructed when a session is resumed.

## Memory Context Injection

`MemoryContext` is one Static or Graph memory-tag section. `MemoryInjection` is the complete typed Memory or Tape contribution from one build operation; it does not encode whether injection was automatic or explicitly requested.

Tape mode changes **which memory files are selected**, not the injection mechanism itself. The injected content is assembled from the selected memory index/summary and Tape guidance.

```typescript
// memory.tape.context
{
  strategy: "smart",           // "smart" or "recent-only"
  fileLimit: 10,                // Max memory files
  memoryScan: [72, 168],        // Activity scan starts at 72h and expands toward 168h when samples are sparse
  whitelist: [],                // Always include these files or directories
  blacklist: [],                // Always exclude these files or directories; other paths still use rg/default ignore filtering
}

// Injection adds:
- Memory file list with descriptions/tags
- Persisted MemoryGraph selection over memory/project files using the latest user prompt plus recent anchor summary/purpose/keywords
- Files under the memory directory retain descriptions/tags in their graph-node search text
- Project files connected by persisted anchor relations or discovered through recent tool activity
- Recent `read` / `edit` / `write` activity contributes graph seed scores and transient `AccessedFile` relations
- No historical line ranges or tool activity details are included in the injected context
- Smart-mode filtering skips graph paths whose files no longer exist
- Hidden keyword-triggered anchor instruction when configured keywords match
- Optional `anchor.mode: "manual"` policy that asks through the existing permission UI before Tape mutations; keyword matches independently authorize only the matching `tape_anchor`
- Tape hint with tool usage instructions
```

### Injection behavior

| Injection mode | Tape behavior |
|---------------|---------------|
| `message-append` | Attempts automatic Tape-selected `InjectedContext` once per owning registration lifetime. Invocation start consumes the attempt, even without accepted content. Reset, compaction, reload, rewind and anchor navigation do not rearm a continuing handler; a new Instance starts fresh |
| `/memory-inject [prompt]` | Rebuilds Tape-selected context on every explicit submission, with no prompt when the argument is omitted |

Keyword-triggered anchor instructions are independent from the main memory payload. A matching master Agent prompt receives a separate turn-scoped `TapeKeywordIntro` context, so it does not trigger an extra model request. Delegated subagent task text is not a matching master prompt and cannot cause a second keyword instruction, even when it repeats the user's wording. A directed `@` user prompt is input to the master and its original user-authored text follows the same keyword matching, deduplication, `TapeKeywordIntro`, and exact-anchor authorization rules as any other user prompt to the master, including Manual-mode bypass only for the matching `tape_anchor`. The shortcut itself grants no authorization; expansion, Agent definitions, and model-generated text are never trusted keyword evidence. Repeated detection uses an exact `(keyword, left, right)` signature. Each side contains at most two units: every Han character and punctuation character is one unit, while a contiguous non-Han alphanumeric or underscore run is one unit; whitespace is ignored. If every matched occurrence was already seen in the active in-memory session, the keyword instruction is omitted. New and resumed sessions begin with empty detection state.

If `memory.tape.anchor.mode === "manual"`, `tape_anchor` uses the existing
permission choices: Allow once, Always, or Deny. An exact keyword-authorized
anchor bypasses Manual confirmation without granting other mutations.

Tape affects **selection**. Automatic and explicit injection share the same turn-scoped message mechanism; neither modifies the system prompt.

### Tape activation rules

Application `memory` and `memory.tape` settings follow the [feature-block rules](../../../../../README.md#settings-reference): empty blocks disable; options enable by default; explicit `enabled` wins. Tape also requires Memory enabled.

Tape runtime is enabled only when all of these checks pass:
- `memory.enabled` is true
- `memory.tape` is configured and `memory.tape.enabled !== false`
- current `cwd` does not match any absolute path in `memory.tape.excludeDirs`
- current `cwd` does not match the built-in system safety exclude list
- when `memory.tape.onlyGit !== false`, `git rev-parse --show-toplevel` resolves a Git root from `cwd`

If any check fails, tape is skipped completely for that turn/session startup: no tape injection, no tape keyword anchor message, and no anchor recording.

Tape context includes a concise instruction that Tape is enabled and that Tape tools may be used for anchors or history. In Manual mode it also explains that mutations require permission while an exact keyword match only authorizes its generated `tape_anchor`.

## Configuration

```json
{
  "memory": {
    "injection": "message-append",
    "tape": {
      "onlyGit": true,
      "excludeDirs": [
        "/absolute/path/to/sandbox"
      ],
      "context": {
        "strategy": "smart",
        "fileLimit": 10,
        "memoryScan": [72, 168],
        "whitelist": [],
        "blacklist": []
      },
      "anchor": {
        "labelPrefix": "⚓ ",
        "mode": "auto",
        "keywords": {
          "global": [],
          "project": []
        }
      }
    }
  }
}
```

- `injection: "message-append"` opts into automatic turn-scoped context; omitted or `false` leaves injection manual.
- `onlyGit` defaults to `true`. When enabled, tape runs only inside a Git repository; otherwise tape injection and anchor recording are skipped.
- `excludeDirs` is a list of absolute directory paths. If `cwd` is equal to or inside any excluded directory, tape is skipped.
- Built-in system safety excludes are also applied by default and merged with user-defined `excludeDirs`.

### Context Strategy

**Smart** (default):
- Loads memory and project-file candidates through `MemoryService`, lazily materializing `graph.json` when needed
- Filters stale or blacklisted paths before querying
- Builds intent text from the current prompt, latest user message, and recent anchor metadata
- Uses lexical and behavior seed scores plus bounded, relation-aware MemoryGraph traversal
- Reuses persisted anchor, memory, topic, and shared-context relations
- Adds recent activity-only files and anchor-linked `AccessedFile` relations as transient query evidence rather than canonical graph state
- Applies `write=30`, `edit=28`, and `read=20` event weights with 24-hour recency decay, repeat dampening, diversity bonus, and anchor-window boost
- Starts activity scanning at `memoryScan[0]` and expands by 24 hours toward `memoryScan[1]` until at least 5 access samples are available
- Falls back to directory scan only when neither graph nor activity provides candidates, except worktrees retain their isolated no-fallback behavior
- Does not inject concrete historical line ranges

**Recent-only**:
- Scans memory directory directly
- Sorts files by modification time (newest first)
- Returns the top N memory files
- Does not include project file paths from tape history
- Faster but less context-aware

### Anchor Types

| Type | Behavior |
|------|----------|
| `normal` | Independent anchors created through `tape_anchor` |

`memory.tape.anchor.labelPrefix` customizes derived anchor labels in Phi's session picker (default: `⚓ `).

## Usage Patterns

### Pattern 1: Task Phases

```typescript
// Start new task
tape_anchor(name="task/auth-api", summary="Implement authentication API")

// ... work on task ...

// Save checkpoint
tape_anchor(
  name="auth/api-endpoint",
  summary="Auth API endpoint checkpoint"
)

// Later: retrieve context
tape_read({ sinceAnchor: "task/auth-api" })
```

### Pattern 2: Debugging Sessions

```typescript
// Checkpoint before changes
tape_anchor(name="debug/before-fix", summary="Investigating timeout error")

// ... attempt fix ...

// Success or failure anchor
tape_anchor(name="debug/after-fix", summary="Fixed by increasing timeout")

// Review what happened
tape_read({ betweenAnchors: { start: "debug/before-fix", end: "debug/after-fix" } })
```

### Pattern 3: Context Switching

```typescript
// Save current state
tape_anchor(name="context/save", summary="Saving migration context")

// Switch task
tape_anchor(name="task/urgent-fix", summary="Hotfix for production")

// ... fix bug ...

// Return to previous task
tape_read({ sinceAnchor: "context/save" })
```

## Best Practices

### DO

✅ **Create anchors at meaningful transitions**
```typescript
tape_anchor(name="phase/design", summary="Moving to implementation")
```

✅ **Use descriptive, hierarchical names**
```typescript
// Good
tape_anchor(name="bug/auth/timeout-fix")
tape_anchor(name="task/api/phase2")

// Less useful
tape_anchor(name="checkpoint")
```

✅ **Store relevant metadata in anchors when it helps retrieval**
```typescript
tape_anchor(
  name="migration/checkpoint",
  summary="Users table migration checkpoint",
  purpose="migration"
)
```

✅ **Use targeted queries**
```typescript
// Specific and efficient
tape_read({ sinceAnchor: "task/api", types: ["message"], limit: 10 })

// Instead of everything
tape_read({})
```

### DON'T

❌ **Create anchors too frequently**
❌ **Use vague anchor names**
❌ **Query without filters in large sessions**

## Troubleshooting

### Issue: No entries returned

**Check:**
1. The persisted session file exists under `${PHI_SESSION_DIR:-~/.phi/projects}`
2. Anchor name exists: `memory_search({ mode: "tape" })`
3. Try without filters: `tape_read({ limit: 10 })`

### Issue: Keyword-triggered anchor not appearing

**Check:**
1. [Tape activation rules](#tape-activation-rules) pass
2. `memory.tape.anchor.keywords.global` or `project` contains the expected keyword
3. The user prompt length is between 10 and 300 characters
4. The keyword is actually present in the submitted user message
5. The same keyword and two-unit local context have not already been seen during the active in-memory session

### Issue: Memory files not injected

**Check:**
1. Memory is enabled; Tape selection also requires [Tape activation](#tape-activation-rules)
2. Memory repository initialized: `memory_check`
3. `core/` directory exists
4. Injection mode behavior matches expectations:
   - `injection: "message-append"` enables automatic injection; omitted or `false` disables it
   - `/memory-inject [prompt]` explicitly rebuilds and injects the main payload on demand
   - keyword anchor instructions may still be added on later matching turns as `TapeKeywordIntro` context

## File Structure

```
{localPath}/{projectName}/      # Project memory directory
├── {projectName}__graph.json          # optional lazy materialized view
├── {projectName}__anchors.jsonl       # optional canonical source
└── core/

${PHI_SESSION_DIR:-~/.phi/projects}/        # Phi session storage (read-only to Tape)
└── --{cwd-path}--/
    └── *.jsonl                 # Reader matches Phi metadata headers by session id
```

## Related Skills and Commands

- `memory-init` - Repository initialization through the skill workflow
- `memory-write` - Durable memory creation/update through Markdown files and frontmatter validation
- `memory-import` - Import durable knowledge from URLs, folders, or files
- `memory-digest` - Convert recent tape/session context into confirmed long-term memories
- `/memory-review` - Human-facing anchor timeline, search, jump, `ctrl+d` delete
- Native tools: `memory_check`, `memory_search`, and `memory_sync`

## Reference

- Session entry model: `crate::transcript::{TranscriptEntry, TranscriptEntryKind}`
- Tape systems: https://tape.systems
- https://bub.build/
- https://github.com/bubbuild/bub/tree/main/src/bub
