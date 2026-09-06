# waku-client

`waku-client` is the Rust client for `waku-daemon`. It owns the authenticated
WebSocket handshake, request correlation, subscriptions, event sequence
deduplication, replay cursors, local-daemon supervision, and disposable client
preview caches. It depends on `waku-protocol`, never on `waku-core`.

Both bare socket addresses and complete `ws://` or `wss://` URLs are accepted.
Dropping a connection to an externally managed daemon does not stop it.

`subscribe_after` starts live delivery before filling any missing persisted
prefix on a background thread. Replay and live events share cursor deduplication.
Unsubscribed buffers prune only acknowledged events beyond the 4096-entry hot
window; opening a session recovers that prefix through paged `ReplayEvents`
or a full `HistorySnapshot` when durable events were pruned. The snapshot
replaces old history, then delivery resumes strictly after its saved cursor.
Storage acknowledgments remain separate from event receipt.
