# waku-core

`waku-core` is Waku's daemon-only runtime. It contains the native session
drivers, provider discovery and model metadata, task persistence, attachment
storage, workspace filesystem and Git services, Computer Use process control,
and daemon-owned settings. It depends on the serializable contract in
[`waku-protocol`](../waku-protocol), but contains no desktop transport or UI.

The transport is an authenticated WebSocket (loopback by default). Requests
have stable UUIDs for idempotency. Concurrent retransmissions share one
execution and response; completed responses use a bounded in-memory cache.
Session events carry monotonically increasing sequence numbers and
runtime-generation IDs. The daemon commits
ordered events and the shared readable-history projection in one SQLite
transaction with WAL `synchronous=FULL`. Only a successful commit emits a
`HistoryPersistence` acknowledgment. Failed batches remain available for retry.
The server keeps a 4096-event hot replay window; `ReplayEvents` reads older
history in pages of at most 512 events, bounded by the transport byte budget.
Saved redundant events retain at most 20,000 rows per session or 90 days from
reliable preservation. Unknown or unconfirmed prefixes remain unprunable.
A pruned `ReplayEvents` prefix returns `HistorySnapshot` with the complete
readable history and its saved cursor, read in one SQLite transaction.
Snapshot responses use ordered UTF-8 chunks below the wire frame limit;
clients validate their total byte length and cursor before replacing history.
Readable history remains until explicit deletion. Stale client snapshots can
update metadata without replacing daemon-owned history.

`DaemonClient` lives in [`waku-client`](../waku-client), which is what Waku
Desktop depends on. `serve` and `WakuBackend` are used by the `waku-daemon`
binary.

Configuration ownership is explicit:

- the desktop owns `~/.waku/app.json` in Release and checkout-local
  `temp/app.json` in Debug;
- the daemon owns `~/.waku/settings.json` in Release and checkout-local
  `temp/settings.json` in Debug.

Task SQLite rows and durable attachment materializations are daemon-owned as
well. Client-local attachment paths are upload inputs or caches only; provider
prompts and persisted messages use daemon-issued paths and references.
Projectless task directories are daemon-owned too and live beneath
`~/.waku/projects` in Release and checkout-local `temp/projects` in Debug.

The protocol types use Serde's tagged JSON representation and are exported by
`waku-protocol`, including checked-in TypeScript bindings.

`CreateSession` creates one Claude or Codex child from the request's parent session ID.
The daemon validates the parent and project, saves the child, starts the
provider and submits one first prompt. `workspace` defaults to `worktree`
beneath the task database directory; `inherit` uses the parent's actual
directory, and `local` uses the project's ordinary checkout. The response
includes the actual directory and an optional branch. Invalid worktree
requests fail without changing to another workspace mode.

An optional `idempotency_key` on the MCP tool (`idempotencyKey` on the wire)
is scoped to the parent session. SQLite records the request before creating
resources. Equal requests reuse the saved result across daemon restarts;
different requests with the same key fail. Retrying revalidates the parent,
project and permission limit. Different keys can create children concurrently.
Without a key, retries have no durable deduplication guarantee.

Creation failures return `SessionCreationFailed`, including the attempted
stage, allocated session ID, retained path and branch when known, and whether
first-prompt acceptance is uncertain. Provider failures are saved in the child
history. Restart converts an unfinished creation into a saved uncertain result;
it never resends its input. Runtime replacement is rejected while creation is
in progress. Workspaces are retained on failure for inspection; existing and
modified directories are never automatically removed.

The daemon assigns immutable `parent_session_id`. Client snapshots cannot
reparent existing sessions or create that relationship, and manual response
forks begin without a parent. Parent deletion preserves child history. Child
creation, runtime start, option changes, and saved permission changes enforce
the parent's permission limit. Across Claude and Codex, automatic approval
modes have different authority: an explicit Ask child or a FullAccess parent
provides the supported safe mapping.

Managed code tasks use the explicit `StewardWorkspace` command before the root
runtime starts. `begin` records the chosen target and commit, creates a separate
coordination worktree for the root runtime, and reserves an integration worktree
for daemon Git operations. Child worktrees start at a fixed integration commit.
`waku_workspace` exposes scoped inspection and fixed-commit acceptance; ordinary
sessions keep their existing workspace behavior. All current provider runtimes can acquire write access, so
managed resources reject another runtime at the same canonical execution path,
including `inherit` and symlink aliases. Static queries do not acquire a writer.
The daemon retains ownership through stale client saves and restarts. Failed
creation retains its recorded resources; it never adopts an unrelated branch.

`integrate` accepts a direct child's full commit ID and evidence naming the checks,
environment and reviewer. It records the result before merging into the fixed
integration target. Conflicts remain available for resolution; retries inspect
actual Git state. `CreateSession.dependencies` names accepted child commits, and
creation proceeds only when the selected integration commit contains them.

`deliver` accepts the combined commit with separate overall evidence. It checks
the selected local target's current commit, dirty files and active runtimes or
terminals before a fast-forward. Each attempt retains its record; each published
result has an immutable `refs/waku/tasks/<task-id>/deliveries/<commit>` reference.
After a failed attempt, a newly reviewed commit can be delivered without replacing
an earlier reference. A confirmed delivery is idempotent. Delivery does not push
or deploy. The UI distinguishes execution, pending acceptance, integration and
local delivery using saved records.

After confirmed delivery, the daemon cleans eligible task worktrees and branches.
It checks each recorded resource, shared session references, runtime and terminal
users, all untracked or ignored files, and commit containment in the durable
result reference. Idle provider processes are closed only when no open turn,
approval, queued input, persistent wait, unsaved history or background work remains.
The existing event worker retries resources deferred for active work. Other unsafe
resources keep a saved reason and can be retried with `waku_workspace cleanup`.
Cleanup uses ordinary Git removal, preserves history and delivery refs, and never
adopts legacy workspaces or deletes later work after a partial cleanup. The task
results popup shows saved commit evidence, dependencies and per-resource outcomes.
