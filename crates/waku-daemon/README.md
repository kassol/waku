# waku-daemon

`waku-daemon` is the standalone process that hosts Waku's provider sessions.
It defaults to a loopback-only listener, authenticates clients with
`WAKU_DAEMON_TOKEN`, and
prints one JSON readiness record to stdout containing its address, protocol
version, and process ID.

```text
WAKU_DAEMON_TOKEN=<secret> waku-daemon --bind 127.0.0.1:0 [--parent-pid PID] [--allow-origin ORIGIN]...
```

Waku Desktop supervises this process. Debug builds use the feature-gated
`waku-debug-daemon` target at `target/debug/waku-debug-daemon`, so rebuilding
provider code replaces only the daemon. Release distributions place the signed
`waku-daemon` binary beside the desktop executable.

The token is a full-control capability for a trusted Waku client, not a user or
workspace-scoped credential. Browser handshakes are rejected unless their exact
Origin was supplied with `--allow-origin`; native clients send no Origin. A
non-loopback bind is refused unless `--allow-non-loopback` is also present.
Waku Desktop adds that flag only after the user enables exposure in Settings →
Daemon. The daemon does not terminate TLS itself. For access outside a private
network, put a trusted TLS proxy or tunnel in front of it and use `wss://`. Do
not give the daemon token to untrusted page JavaScript.

## Session MCP transport

Claude sessions receive an inline `--mcp-config` when their daemon runtime
starts. The configuration launches the same daemon binary with the `mcp`
subcommand. It preserves native CLI configuration and does not write global
Claude settings. This transport implements newline-delimited JSON-RPC on
stdin/stdout; diagnostics use stderr.

The daemon supplies `WAKU_MCP_ADDRESS`, `WAKU_MCP_TOKEN`, `WAKU_MCP_SESSION`, and
`WAKU_MCP_RUNTIME` for this subprocess. The MCP token authorizes only the bound
session, project, and runtime. It cannot run desktop management commands or
receive global events, replay, or catalog notifications. Runtime replacement
or exit revokes it. Both completed responses and pending request waiters are
isolated by credential. The full-control daemon token is never included.

Currently `tools/list` exposes only `waku_spawn_session`: `provider` (`codex`)
and `prompt` are required; `model`, `title`, and `runtime_mode` are optional.
The daemon validates the parent's current project and permission ceiling,
creates a direct child in an isolated Git worktree, and returns `session_id`,
`workspace_path`, and `branch`. Unknown arguments fail. Other providers,
workspace choices, durable idempotency, and management tools are separate
implementation tasks. A lost response must not be retried automatically.

Protocol regression uses real WebSocket, SQLite and Git with fake providers:

```sh
cargo test -p waku-core steward_tests
cargo build -p waku-daemon
WAKU_TEST_MCP_BINARY="$PWD/target/debug/waku-daemon" cargo test -p waku-core mcp_stdio_creates_real_child
```

The last command additionally executes the daemon MCP subprocess over real
stdin/stdout pipes. It does not launch an app or call an external model.

References: [MCP stdio transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports),
[Claude session MCP configuration](https://code.claude.com/docs/en/mcp).
