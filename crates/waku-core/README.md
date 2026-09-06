# waku-core

`waku-core` is Waku's daemon-only runtime. It contains the native session
drivers, provider discovery and model metadata, task persistence, attachment
storage, workspace filesystem and Git services, Computer Use process control,
and daemon-owned settings. It depends on the serializable contract in
[`waku-protocol`](../waku-protocol), but contains no desktop transport or UI.

The transport is an authenticated WebSocket (loopback by default). Requests
have stable UUIDs for idempotency; session events carry monotonically
increasing sequence numbers and runtime-generation IDs. The daemon commits
ordered events and the shared readable-history projection in one SQLite
transaction with WAL `synchronous=FULL`. Only a successful commit emits a
`HistoryPersistence` acknowledgment. Failed batches remain available for retry.
The server keeps a 4096-event hot replay window; `ReplayEvents` reads older
history in pages of at most 512 events, bounded by the transport byte budget.
No persisted event cleanup is enabled. Stale client snapshots can update
metadata without replacing the daemon-owned history.

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
