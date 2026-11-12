# Git worktree and Agent execution directories

## Scope and principles

A worktree is an execution directory, not an Agent type, Session owner or delegation mode. Worktree preparation, task assignment and history storage are independent concerns.

Agents choose Git workflows through ordinary tools and user-customizable Skills. Phi enforces existing permissions, sandbox and path checks; it does not prescribe branch naming, create a Git scheduler or introduce worktree-specific authorization. Skills provide guidance, never authority.

Every Agent executes in its own AgentInstance, with its own Transcript. Manager coordinates delegation; application runtime owns preparation and Session persistence. See [parallel Agent design](design-parallel-agent.md) for those contracts.

## Prepare first, delegate second

```text
Agent -> permitted Git commands -> prepared directory
Agent -> delegation request -> Manager -> independent Instance in that directory
```

Creation selects an existing directory. Omission uses the immediate parent's execution directory; relative paths resolve against it, not process CWD. Invalid, missing or unauthorized targets fail without fallback or recreation. Plain directories and worktrees use the same admission path; repository-specific requirements belong to the task and ordinary Git operations.

Invocation uses the instance's existing directory and configuration. It cannot move the instance, change its specialization/Team or perform Git setup. Tool schema and canonical directory validation live in [Agent adapter](../../phi-sdk/src/agent/tools/agent.rs) and [Manager preparation](../../phi-sdk/src/harness/manager/preparation.rs).

Delegation failure does not roll back an independently prepared worktree. Completion, cancellation and detachment neither remove it nor merge its changes. Integration and cleanup are separate explicit operations.

## Execution security gate

```text
Agent intent + Skill guidance
  -> command/file tool
  -> permission + sandbox + resolved command/path validation
  -> permitted effects
  -> validated directory selection for delegation
```

The gate applies to actual effects, not Skill names, role labels or a textual `git worktree` match. Scripts and alternate shell forms remain subject to conservative command analysis; unverifiable behavior follows existing denial/approval policy. Git redirection and filesystem targets remain governed by resolved-path checks.

Directory validity is checked before startup and revalidated across preparation. Later execution must not silently switch to the main checkout or parent's directory if its configured directory disappears. New directory configuration does not expand delegated tool authority or bypass child-specific restrictions.

Removing a live instance's directory is not an implicit stop request. Active-directory removal protection would require an explicit concurrency/lease contract before implementation; current observations do not promise atomic exclusion or automatic stop-and-delete.

Implementation: [Bash authorization](../../phi-sdk/src/permission/shell.rs), [permission runtime](../../phi-sdk/src/permission/runtime.rs), [sandbox](../../phi-sdk/src/sandbox.rs), and [candidate/directory validation](../../phi-sdk/src/harness/manager/preparation.rs).

## Parallel execution and collaboration

Different instances can share a directory; one Team can span directories and different Teams can share one. Neither Team membership nor shared paths grant control authority or write isolation. Separate worktrees isolate Git working copies, not every external resource. Delegation adds no repository-wide lock or Git transaction.

An instance may receive successive invocations in the same directory. Completion preserves the instance and worktree. Child results return to the creator, independently of directory or Team membership. See [independent concepts](design-parallel-agent.md#concepts-and-independent-boundaries).

## Directed delegation

`@agent` submits original user text and a typed named-Agent constraint to the master, not a UI spawn request. The master formulates the task and selects a prepared directory through the asynchronous Agent tool.

Directed creation requires the constrained Agent call; retained assignment cannot substitute for it. Admission failure cannot trigger fallback Git commands, retries or a directory downgrade. Ordinary turns may prepare a directory before delegation. See [directed admission tests](../../phi-sdk/src/harness/tests/delegation.rs).

## Runtime and context boundaries

Application runtime resolves target-directory configuration/resources through the preparation seam. SDK construction validates the complete candidate and enforces definition, tool, permission, sandbox and delegated restrictions. Approval for one Git command does not authorize subsequent child tools or edits.

Automatic parent Memory/Tape injection and keyword authorization are not inherited by child startup. Delegated task text cannot manufacture trusted user consent; permitted read-only retrieval remains available. These are SDK context/security rules independent of worktrees; see [Memory and Tape](../design-sdk.md#memory-and-tape).

## Session storage and history

Each child has independent canonical history. Application-created children also have matching Session associations. Persisted files live in Phi history storage, not inside the worktree. Root and teammate journals use their working-directory `session_scope`. Ordinary subagents inherit their parent's storage association even when executing in another worktree: `<parent-session-id>/subagents/agent-<child-session-id>.jsonl`. Nested children extend the ancestry; follow-up retains the original file. See [ordinary sidechains and teammate history](design-agent-task.md#ordinary-sidechains-and-teammate-history).

Removing a worktree or parent Session does not erase child history or typed references. Recovery reads saved data without rerunning Git commands, delegation or delivery. Missing historical paths do not silently rebind execution. Exact save/retention rules belong to [application persistence](design-agent-runtime.md#application-session-and-persistence).

## Path terminology

`working_dir` determines execution and the default Session scope; ordinary subagent storage follows the parent association instead. Launching in a worktree subdirectory does not move execution to its Git root. `project_root` determines project resources/instructions and is the active Git/worktree root, or working directory outside Git. See [global path terminology](../design.md#path-terminology).

Filesystem tools resolve paths against the instance directory without changing process CWD. Authorization, previews, mutation locks, semantic diffs and effects must address the same resolved target. Symlink normalization and sandbox validation remain in their owning tools/security modules.

Memory directory-name scope retains its existing semantics; it is not a repository-wide identity or Session owner.

## Projection

Views are passive: inspecting a task or child history does not start, cancel, checkpoint or deliver work. Child interactions remain scoped regardless of directory or active view. Detailed rendering/navigation belongs to [application subagent presentation](../design-interactive.md#subagent-presentation); execution and access ownership belong to [Agent runtime design](design-agent-runtime.md).
