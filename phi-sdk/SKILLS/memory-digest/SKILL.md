---
name: memory-digest
description: Digest recent tape anchors and session context into curated Phi memories when tape mode is enabled. Use only with tape mode enabled when the user asks to summarize recent work, extract learnings, preserve decisions, convert tape/session history into memory, or review recent intent for durable memory updates.
---

# memory-digest

Turn recent tape anchors and nearby session context into concise, durable memory. Prefer curation over transcript summarization.

## Core rule

Inspect recent intent first, read only relevant context, propose memory changes, then write only after user confirmation using the `memory-write` skill.

- Keep each proposed memory focused on one durable topic.
- Keep proposal details lightweight: explain only why each candidate should be created, updated, or skipped.
- For possible updates, identify the existing memory file and briefly state why it should change.
- Defer final description, tags, frontmatter shape, timestamp handling, and file creation/update mechanics to the `memory-write` skill.

## Workflow

1. Inspect recent anchors with `memory_search({ mode: "tape", limit: 20 })`.
2. Infer the user's recent intent from normal anchor names, summaries, purposes, keywords, timestamps, and nearby context.
3. Select the smallest useful tape range by combining anchor count and recency:
   - Use `memory_search({ mode: "tape", name_contains: "anchor", limit: 20 })` when anchor metadata identifies the active task.
   - Use `memory_search({ mode: "tape", query: "<active task>", includeRelations: true, limit: 10 })` when recent anchors do not capture the active task.
   - Use `tape_read({ betweenAnchors })` when a clear start/end pair exists.
4. Extract durable memory candidates only when they are useful across future sessions.
5. Compare candidates with existing memory using `memory_check({ directory })` or `memory_search` before proposing creates or updates. Then read the specific memory file if needed.
6. Apply the `memory-write` skill rules before showing proposed paths:
   - Treat `memory-write` as the single source of truth for placement, filename conventions, and frontmatter shape.
   - Validate every proposed relative path against `memory-write` rules.
   - Do not invent root folders or naming schemes outside `memory-write`.
   - If the correct path is uncertain, propose the candidate without a final path and ask the user to choose or confirm placement.
7. Present proposed changes and ask for confirmation.
8. After confirmation, use the `memory-write` skill to create or update memory files.
9. Ask before `memory_sync` unless the user explicitly requested sync.

## Save candidates

Save concise knowledge in these categories:

- user preferences
- project decisions
- architecture constraints
- reusable implementation patterns
- recurring bugs and fixes
- workflow habits
- unresolved questions or future directions

Do not save:

- raw command output
- temporary debugging noise
- one-off implementation details recoverable from git
- unconfirmed guesses
- large chat summaries
- secrets, credentials, tokens, or `.env` content

## Proposal format

Before writing, respond with:

```md
# Proposed memory updates

## Create
- `path/to/file.md`
  reason:

## Update
- `path/to/file.md`
  reason:

## Skip
- item:
  reason:
```

End by explicitly asking the user for confirmation with: `Proceed with these memory updates?`

## Related skills

- `memory-write`: Create or update memory files with valid frontmatter.
