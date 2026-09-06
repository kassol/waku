# Handoff: waku fork「管家 Agent + MCP 调度多 harness」

日期：2026-09-06。历史记录，基于临时克隆 `0988a1c`。当前状态、约束与下一步以[项目交接](waku-project-handoff.md)为准。下文代码行号与环境结论均需按当前源码和环境复核。

## 目标

Fork egoist/waku（Rust daemon + GPUI 桌面端，GPL-3.0）自用，加一个「管家 Agent」形态：用户只跟一个跑在原生 Claude Code 上的 waku 会话对话，它通过 daemon 暴露的 MCP 工具派生 / 驱动 / 观测跑在其他 harness（Claude Code、Codex 等）上的子会话，并能创建定时任务。harness 本体一行不改。

## 已有产出（不要重做）

- 设计稿 v2（最终版）：[设计 v2](waku-steward-design.md)。含 harness 能力核实表、MCP 工具面、schema 变更、daemon 落库与调度方案、观测四层分期、各期改动文件清单、待拍板项及其结论。
- waku 源码克隆：`/Users/kassol/Workspace/temp/waku`，HEAD 为 2026-09-04 提交 `0988a1c`，未做任何修改。
- 长期记忆：`~/.claude/projects/-Users-kassol-Workspace-temp/memory/project_waku_orchestrator.md`，记录六点需求、决策演变、waku 代码事实。
- 被推翻的 v1 设计（固定三阶段流水线、编排放客户端）只在会话临时目录，已作废，不必找回。

## 用户已拍板的决策

1. 主形态是管家 + MCP，阶段由管家临场决定，取代固定流水线。
2. MCP 传输用 stdio shim：`waku-daemon` 加 `mcp` 子命令，内部用现有 WebSocket 协议连回同一 daemon、复用 token。选 stdio 的决定性理由：Codex 的 `-c mcp_servers.<name>.command` 只支持 stdio。
3. daemon 新增 `Command::CreateSession`（shim 与调度器共用）、`session_events` 表（`Hub::emit` 旁路落库，保留 20000 条/会话 + 90 天）、`AgentSession.parent_session_id`。
4. 子会话默认新建 worktree，可显式 `inherit`。
5. 期 1 只做 Claude 管家 + Claude/Codex 子会话 + 树形侧边栏 + daemon 自主落库；ACP 系（Cursor/Fx/Kimi/Grok）不进期 1。期 2 定时任务。期 3 launchd 服务化。期 4-6 观测 B/C/D 层。
6. 本机暂不安装 Rust 工具链，实现另择时机。

## 已交叉核实的关键代码事实

均已由主会话独立 read/grep 验证，可直接引用：

- 生产代码中 `SaveTaskState` 只由客户端发起（`crates/waku-client/src/persistence.rs:1024`），daemon 无自主落库路径，无客户端时 DriverEvent 只进 4096 条内存 journal（`crates/waku-core/src/server.rs:31, 213-238`）。这是期 1 必须补的硬缺口。
- `ForkProviderSession` 只能同 provider 内分叉（`crates/waku-protocol/src/provider_session.rs:10-42`），不能跨 harness 重放。
- `DriverStartOptions`（`crates/waku-core/src/driver/mod.rs:183-194`）无 MCP / system prompt 字段，需加 `mcp_servers`。
- Claude driver 的 `--session-id` 插入点在 `crates/waku-core/src/driver/claude.rs:212-216`，`--mcp-config` 追加在同一段。
- Claude 带 `parent_tool_use_id` 的 subagent 消息在 `claude.rs:1413` 经 `forward_subagent_transcript` 后 return，丢的是 usage、工具输出与 diff。
- `BackgroundWorkItem.parent_id` 只有 codex 设置（`codex.rs:1586-1622`），UI 存了不显示（`src/app/background_work.rs:162`）。
- `merge_stale_session_metadata`（`crates/waku-core/src/daemon.rs:843-867`）是显式字段表，新字段 `parent_session_id` 必须排除在外，否则被老客户端快照抹掉。
- 本机 `claude --help` 确认 `--mcp-config <configs...>` 接受内联 JSON，`--strict-mcp-config` 存在。`pi --help` 无 MCP 支持。

## 环境

- 本机（macOS）无 `cargo` / `rustc` / `rustup`。waku 要求 Rust ≥ 1.96（CONTRIBUTING.md:12），无 `rust-toolchain.toml`。brew stable 1.98.0 满足，但建议 `brew install rustup` 而非 `brew install rust`（GPUI 是 git 依赖，MSRV 会跟 zed 上游走）。
- 构建入口是 `bun install` + `bun run dev`（watcher 负责编译签名 Debug.app 和外挂 daemon），不要直接 `cargo run`。Bun 已有。
- 存储提醒已告知用户：GPUI 项目 debug `target/` 估计 10 到 30 GB，`~/.cargo/git` 里的 zed 检出数 GB。

## 未核实项

- `agent-client-protocol` 2.0.0 Rust crate 的 `NewSessionRequest` 是否有 `mcp_servers` 字段及其形状。协议层确认有 `mcpServers`，crate API 要等工具链装好后读 `~/.cargo/registry` 源码。期 1 不依赖。
- `WireDriverStartOptions` 与 `DriverStartOptions` 之间的映射函数位置未定位，加字段时需搜一次。

## 下一步（按序）

1. 用户决定开工时：装 rustup，`bun install && bun run dev` 验证 waku 在本机可编译运行。
2. 在 fork 里先写 AGENTS.md，定义术语：管家会话、子会话、shim、`session_events`、`CreateSession`，再动代码。
3. 按设计稿「期 1 改动文件清单」实现，顺序建议：schema + `parent_session_id` → `session_events` 落库 → `CreateSession` → `mcp` 子命令与工具 → Claude/Codex driver 注入 `--mcp-config` → 侧边栏树形。
4. 每一步写单元测试（daemon 侧用临时目录隔离 SQLite）。
5. 实现完跑 `bun run protocol:check` 确认 Rust 与 TS 类型同步。

## 当前工作规范

遵循项目根目录 [AGENTS.md](../AGENTS.md) 与当前会话指令。历史会话中的调度和提问偏好不作为现行规范。

## Suggested skills

- `domain-modeling`：开工第一步写 fork 的 AGENTS.md / CONTEXT.md，固定术语。
- `tdd`：期 1 每个 daemon 侧改动先写复现测试再实现。
- `codebase-design`：设计 `mcp` 子命令模块边界与 `session_events` 写入线程接口时使用。
- `code-review`：期 1 完成后按 Standards 与 Spec（对照设计稿）两轴评审。
- `run`：验证 `bun run dev` 起的 Debug.app 与外挂 daemon 能跑通管家派子会话。
