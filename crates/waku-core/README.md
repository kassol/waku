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
The daemon validates the existing parent and Git project, creates a worktree
beneath the task database directory, saves the child, and submits its first
prompt. Success follows the provider's durable first-turn acceptance and
returns the child ID, runtime ID, worktree path, and branch. Startup failures
retain the workspace and readable failed history. Runtime replacement and
removal are rejected while creation is in progress; no automatic retry or
workspace cleanup is performed.

The daemon assigns immutable `parent_session_id`. Client snapshots cannot
reparent existing sessions or create that relationship, and manual response
forks begin without a parent. Parent deletion preserves child history. Child
creation, runtime start, option changes, and saved permission changes enforce
the parent's permission limit. Across Claude and Codex, automatic approval
modes have different authority: an explicit Ask child or a FullAccess parent
provides the supported safe mapping.
