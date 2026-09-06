# Waku 管家 Agent + MCP 设计稿 v2

> 设计记录，尚未实现。代码依据来自临时克隆 `0988a1c`，需按当前源码复核。当前约束、待复核矛盾与授权边界见[项目交接](waku-project-handoff.md)；本文保留原设计，不作为已完成的验收规格。

## 一句话方案

给 `waku-daemon` 加一个 `mcp` 子命令，它作为 stdio MCP server 启动、内部用 WebSocket 连回同一个 daemon；daemon 新增一条 `CreateSession` 命令与一张事件日志表，管家会话（跑在原生 Claude Code 上）通过 `--mcp-config` 挂上这个 shim，就能派生、驱动、观测子会话，且转录由 daemon 自己落库、App 关掉也不丢。

## harness 能力核实结果

| 能力 | 结论 | 证据 |
| --- | --- | --- |
| Claude Code 按会话挂 MCP | 支持。`--mcp-config <configs...>` 接受 JSON 文件路径**或**内联 JSON 字符串，空格分隔可多个 | 本机 `claude --help`：`--mcp-config <configs...> Load MCP servers from JSON files or strings (space-separated)` |
| Claude Code 隔离全局 MCP | 支持。`--strict-mcp-config` = 只用 `--mcp-config` 里的，忽略其它所有 MCP 配置 | 本机 `claude --help` |
| Claude Code 传输 | stdio / http / sse / websocket 均支持（`claude mcp add --transport http` 示例、`add-json` 说明 stdio、SSE、HTTP、WebSocket） | 本机 `claude mcp --help` |
| ACP `session/new` 的 mcpServers | 有。`mcpServers` 数组；stdio 为**必须支持**的传输（`name`/`command`/`args`/`env`）；http、sse 可选，需先在 initialize 的 `mcpCapabilities` 里确认 | agentclientprotocol.com/protocol/session-setup；waku 用 `agent-client-protocol` 2.0.0（Cargo.lock:133-135），当前 `NewSessionRequest::new(cwd)` 未传（acp.rs:709）。**Rust 侧字段名未核实**（本机无 cargo registry、无 rustc） |
| Pi 的 MCP | **未核实为不支持**：`pi --help` 全文无 `mcp` 字样，扩展机制是 `--extension` / `--skill`。MVP 管家不用 Pi | 本机 `pi --help` |
| 额外发现：Grok 已有按会话 MCP 注入通道 | 有。`config.toml` 的 `mcp_servers` 表注入 | support.rs:258-280 |

补充核实：`claude` 还有 `--session-id <uuid>`、`--append-system-prompt`，claude driver 已经在用 `--session-id`（claude.rs:212-216），加 `--mcp-config` 的插入点就在同一段。

## 架构（文字图）

```
用户 ──对话──> 管家会话（Waku 里一个普通 Claude Code 会话）
                      │  claude --mcp-config '<inline json>' --strict-mcp-config
                      v
              waku-daemon mcp --session <manager-uuid>      (stdio MCP server)
                      │  WebSocket + 同一个 DAEMON_TOKEN
                      v
              ┌─────────────── waku-daemon ───────────────┐
              │ Hub(事件/journal)  WakuBackend(task_state) │
              │ StateStore(SQLite) Scheduler(tick 线程)     │
              └───────────────────────────────────────────┘
                 │ start_local()          │ SaveTaskState / broadcast
                 v                        v
        子会话 driver 进程 × N        Waku 桌面端（订阅 TaskStateChanged）
        (Codex / Claude / …，各自 worktree)
```

要点：shim **不是** daemon 内的线程，是独立短命进程，跟着管家 harness 的生命周期走。它只说 waku 现有的 WebSocket 协议，所以不引入第二套鉴权和第二个网络面。

## Q1 MCP 传输：推荐 (a) stdio shim

| 维度 | (a) `waku-daemon mcp` stdio shim | (b) daemon 另起 HTTP 端口 |
| --- | --- | --- |
| harness 覆盖 | Claude ✓、ACP 系（Cursor/Fx/Kimi/Grok）stdio 是**强制**支持 ✓、Codex ✓（codex.rs:188-199 只支持 command/args/env，即只支持 stdio）、OpenCode ✓ | Claude ✓、ACP 需 `mcpCapabilities.http` 逐个探测、Codex ✗ |
| 打包 | 零新增二进制，release 已经签名分发 `waku-daemon` | 同 |
| 鉴权 | 复用 `WAKU_DAEMON_TOKEN`，由 shim 进程环境传入，不落磁盘 | 要新的 bearer/origin 策略，且 README:85-89 的 loopback-only 承诺要重新论证 |
| 并发子会话事件流 | 一条 WS 连接订阅全部子会话，journal 重放机制现成（server.rs:243-263） | 同，但要在 HTTP 上重造 SSE 分流 |
| 现实阻塞 | 无 | daemon 是裸 `TcpListener` + tungstenite（server.rs:464-504），同 listener 分流 HTTP 不现实，得再 bind 一个端口 |

Codex 那一栏是决定性的：`-c mcp_servers.<name>.command` 只能表达 stdio，选 (b) 就等于放弃 Codex 子会话被再次派生的能力。

管家启动时怎么拿到这段 `--mcp-config`：在 `DriverStartOptions` / `WireDriverStartOptions` 各加一个 `mcp_servers: Option<Value>` 字段（mod.rs:183-194、protocol.rs:270-281 现在都没有），claude.rs 在 `--session-id` 那段后面追加 `--mcp-config <json> --strict-mcp-config`；codex.rs 复用已有的 `-c mcp_servers.*` 写法；acp.rs 把它填进 `NewSessionRequest`。

## Q2 MCP 工具面

shim 启动参数带 `--session <manager-uuid>`，所以「我是谁」不靠模型自述，由进程参数固定。所有工具的 `parent_session_id` 都从这里取，模型无法伪造。

| 工具 | 入参 | 返回 | 幂等 |
| --- | --- | --- | --- |
| `waku_spawn_session` | `provider`(必填)、`model`、`workspace`: `"inherit"` \| `"local"` \| `"worktree"`（默认 `worktree`）、`prompt`(必填)、`title`、`runtime_mode` | `{session_id, workspace_path, branch}` | 否。带 `idempotency_key` 时同键返回同一个 session |
| `waku_prompt` | `session_id`、`prompt` | `{turn_id}` | 否 |
| `waku_status` | `session_ids[]`、`wait_ms`(0=轮询，>0=长轮询到状态变化或超时，上限 60000) | 每个会话 `{status, last_activity, turn_open, error}` | 是 |
| `waku_result` | `session_id`、`include_transcript`(默认 false)、`max_chars` | `{text, success, summary, transcript?}` | 是 |
| `waku_cancel` | `session_id` | `{cancelled}` | 是 |
| `waku_list_sessions` | `parent_only`(默认 true) | 会话摘要数组 | 是 |
| `waku_schedule_create` | `cron`(5 段) 或 `at`(RFC3339)、`provider`、`model`、`workspace`、`prompt`、`catch_up`(默认 true) | `{task_id, next_run_at}` | 带 `idempotency_key` 时是 |
| `waku_schedule_list` / `waku_schedule_delete` | — / `task_id` | 任务数组 / `{deleted}` | 是 |

`wait_ms` 用长轮询而不是 MCP 通知：Codex 与部分 ACP 实现对 server→client 通知的处理不一致，长轮询在所有 harness 上行为一致。

`waku_result` 的 `text` 由 daemon 从事件日志里取本轮最后一段连续 `TextDelta` 拼出，`success`/`summary` 取 `TurnFinished`（model.rs:1885-1888）。这是个 ~30 行的提取函数，不是完整 reducer。

## Q3/Q4 数据模型、daemon 侧簿记与落库

**新增命令**（protocol.rs `Command`）：

```rust
CreateSession {
    parent_session_id: Option<Uuid>,
    project_id: Uuid,
    provider: ProviderKind,
    model: Option<String>,
    workspace: SessionWorkspace,
    title: Option<String>,
    runtime_mode: RuntimeMode,
}
```

放在 daemon 侧而不是让 shim 自己拼 `AgentSession` + `SaveTaskState`：调度器到点起会话时没有 shim，两条路径必须共用同一段代码。daemon 内实现就是 `AgentSession::new` → 若 `workspace` 是 worktree 则同步走 `workspace::execute(CreateWorktree)`（workspace.rs:104-118）→ `state.push_session`（persistence.rs:318-321，自带 dirty 标记）→ `task_store.save`（persistence.rs:1253）→ `hub.task_state_changed(SENTINEL)`。

**客户端怎么知道有新会话**：不用新事件。`ServerMessage::TaskStateChanged { revision }` 已经是「daemon 拥有的项目/任务目录被别的客户端改了，去 `LoadTaskState`」的广播（protocol.rs:348-352，server.rs:314-322，客户端消费在 waku-client/src/client.rs:327）。daemon 自建会话复用它即可。唯一改动：`broadcast_task_state_changed` 的 `source_subscriber_id` 需要一个「非任何订阅者」的哨兵值（用 `u64::MAX`），让所有客户端都收到。

**schema 变更**（db/schema.ts → `bun run db:generate` → build.rs 内嵌 → `apply_migrations`）：

```ts
sessions: + parentSessionId: text("parent_session_id")        // 可空，自引用
          + originScheduleId: text("origin_schedule_id")      // 可空，标记「定时任务拉起的」

session_events (新表)   // daemon 自主落库的核心
  sessionId, runtimeId, epoch, sequence, event(text json), createdAt
  PRIMARY KEY (sessionId, runtimeId, epoch, sequence)
  index by (sessionId, createdAt)

scheduled_tasks (新表)
  id, parentSessionId(可空), projectId, provider, model,
  workspace(text json), prompt, cron(可空), runAt(可空),
  catchUp(int), enabled(int), lastRunAt, lastSessionId, nextRunAt, createdAt
```

**落库路径（关键取舍）**：不把 `handle_driver_event`（src/app/streaming.rs:234，约 560 行、`&mut self` 绑在 GPUI App 上）搬进 daemon。改为让 `Hub::emit` 在写内存 journal 的同时把同一条 `SequencedEvent` 追加进 `session_events`，客户端打开会话时把 `runtime_event_cursor` 之后的行重放进它**现有的** reducer。好处是转录语义只有一份实现，daemon 不需要理解 provider 语义。

`ponytail:` 写入用一个专门线程 + 有界 channel 批量提交，`Hub::emit` 持锁路径上只做 channel push；channel 满则丢弃并记一条 `overflow` 行，绝不阻塞事件广播。

**与客户端 SaveTaskState 快照的冲突**：现有防线已经够用，不要新加机制。`session_projection_precedes`（daemon.rs:816-841）用 `runtime_event_cursor` 的 `(runtime_id, epoch, sequence)` 判断新旧，落后的 incoming 走 `merge_stale_session_metadata`（daemon.rs:843-867）只合并显式字段表。真正需要动的只有两处：

1. `merge_stale_session_metadata` 的字段表要**排除** `parent_session_id` / `origin_schedule_id`（老客户端的快照里没有这两个字段，别让它们被 `None` 覆盖）。这是纯 daemon 拥有的字段，任何客户端快照都不该写。
2. `AgentSession` 的这两个新字段加 `#[serde(default, skip_serializing_if = "Option::is_none")]`，保证旧客户端往返不丢。

## Q5 调度器

落点：`waku-daemon/src/main.rs` 在 `waku_core::serve` 前起一个独立命名线程 `waku-daemon-scheduler`，30 秒 tick，共享同一个 `shutdown: Arc<AtomicBool>` 与 `WakuBackend`。**不要**挂进 `serve` 的 accept 循环（server.rs:478-504）——那个循环阻塞在 `listener.accept()` 上，掺 tick 会污染连接语义。

到点动作：读 `scheduled_tasks` 里 `nextRunAt <= now && enabled` 的行 → 走 Q3 的 `CreateSession` 内部路径 → 立刻 `Prompt` → 更新 `lastRunAt` / `lastSessionId` / `nextRunAt`。整条链和 shim 的 `waku_spawn_session` 完全同一段代码。

补跑：`catchUp=true` 时，启动后发现 `nextRunAt` 已过期就立刻跑**一次**（不按错过次数补 N 次）；`catchUp=false` 直接把 `nextRunAt` 推到下一个未来点。

与 `--parent-pid` 的关系：App 自己 spawn 的子 daemon 传了 `--parent-pid`，父死自杀（main.rs:31-44），定时任务在这个模式下**没有意义**。所以调度器只在**不带** `--parent-pid` 时启用，并在桌面端 Settings → Daemon 页加一个「安装后台服务」按钮，写一份 `~/Library/LaunchAgents/sh.waku.daemon.plist`（`RunAtLoad` + `KeepAlive`，`WAKU_DAEMON_TOKEN` 走 plist 的 `EnvironmentVariables`，固定端口），装完之后桌面端按 README:79-84 已有的 `WAKU_DAEMON_ADDRESS` / `WAKU_DAEMON_TOKEN` 路径连过去。代价要跟用户讲清楚：连外部 daemon 时文件夹选择器和 PTY 不可用（README:85-89）。

## Q6 观测四层分期

| 层 | 内容 | 最小改动 | 期次 |
| --- | --- | --- | --- |
| A 子会话拓扑 | 谁派生了谁 | `AgentSession.parent_session_id` + 侧边栏按父子缩进 | **随 MVP**，成本极低 |
| B harness 内部 subagent | Claude 带 `parent_tool_use_id` 的消息走 `forward_subagent_transcript`（claude.rs:1189-1234），只转发正文文本与 tool_use 的**标题**，然后 `return`（claude.rs:1413）跳过下面的 usage 累计。丢的是：subagent 的 token 用量、工具**输出**与 diff | 让 subagent 的 usage 计入会话；把 tool_use 的结果块也转发；打开 background_work.rs:162 里已存未显示的 `parent_id`（Codex 已经设置，codex.rs:1586-1622） | 期 3 |
| C 资源与成本 | `UsageUpdated` 只有 `context_tokens` + `context_window`（model.rs:1871-1876），无累计 token、无成本；codex 丢弃 `total.totalTokens`（codex.rs:1942-1946） | `UsageUpdated` 加 `input_tokens`/`output_tokens`/`cached_tokens` 累计字段 + 一张定价表；跨会话汇总突破 app.rs:1274 的按会话隔离 | 期 4 |
| D 实时动作流 | 全局「所有子会话正在做什么」的流 | 新面板消费 `session_events` 表 + 在线事件，按 A 层树分组 | 期 5 |

B/C 层要逐 provider 做（pi/acp/amp/opencode 目前完全没有 BackgroundWork），别指望一期覆盖全部。

## 分期改动文件清单

**期 1（MVP）：Claude 管家 + 派 Codex/Claude 子会话到 worktree + 树形侧边栏 + daemon 自主落库**

- `crates/waku-protocol/src/protocol.rs` — `Command::CreateSession`；`WireDriverStartOptions` 加 `mcp_servers`
- `crates/waku-protocol/src/model.rs` — `AgentSession` 加 `parent_session_id`
- `crates/waku-core/src/driver/mod.rs` — `DriverStartOptions.mcp_servers`
- `crates/waku-core/src/driver/claude.rs` — `--mcp-config` + `--strict-mcp-config`（插在 212-216 段后）
- `crates/waku-core/src/driver/codex.rs` — 复用现有 `-c mcp_servers.*` 通道注入 waku shim
- `crates/waku-core/src/daemon.rs` — `CreateSession` 处理；`merge_stale_session_metadata` 排除 `parent_session_id`
- `crates/waku-core/src/server.rs` — `Hub::emit` 旁路写 `session_events`；`task_state_changed` 哨兵 subscriber id
- `crates/waku-core/src/persistence.rs` — `session_events` 读写；重放查询
- `crates/waku-daemon/src/main.rs` + 新 `crates/waku-daemon/src/mcp.rs` — `mcp` 子命令、stdio MCP server、8 个工具
- `db/schema.ts` + `db/migrations/*` — `parent_session_id`、`session_events`
- `src/app/sessions.rs` / `src/ui/sidebar*` — 树形缩进；打开会话时重放 `session_events`

**期 2：定时任务** — `db/schema.ts`(`scheduled_tasks`)、新 `crates/waku-core/src/scheduler.rs`、`waku-daemon/src/main.rs`(线程)、`waku-daemon/src/mcp.rs`(3 个 schedule 工具)、`crates/waku-protocol/src/protocol.rs`(schedule 命令)

**期 3：独立常驻服务化** — `src/settings/daemon*`(安装按钮)、新 `resources/launchd/sh.waku.daemon.plist`、`docs/`(说明 PTY/picker 限制)

**期 4/5/6：观测 B / C / D** — 见上表

## 待拍板决策

1. **shim 是否复用 `waku-daemon` 二进制**。推荐**是**：同一个二进制加 `mcp` 子命令，release 已经签名分发它，不用新增打包目标。代价是 daemon crate 里多一个跟守护无关的模式。
2. **子会话默认 worktree 还是继承父目录**。推荐**默认 worktree**：管家会并行派多个子会话，共享一个工作目录必然互相踩。`workspace: "inherit"` 保留给「让子会话接着改我这儿的东西」。
3. **`session_events` 的保留策略**。推荐**按会话保留最近 20000 条 + 90 天，超出删旧行**。当前内存 journal 上限是 4096 条/会话（server.rs 的 `MAX_REPLAY_EVENTS_PER_SESSION`），落库后放宽但必须有上限，否则一个长跑会话能把 SQLite 撑爆。
4. **ACP 系（Cursor/Fx/Kimi/Grok）是否进期 1**。推荐**不进**：`agent-client-protocol` 2.0.0 的 Rust 字段名本机核实不到（无 cargo registry、无 rustc 工具链），协议层确认有 `mcpServers` 但 crate API 形状要等能编译时再确认。期 1 只做 Claude + Codex，两者的注入通道都已在代码里存在或有明确 CLI 依据。
