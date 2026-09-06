# 管家一期澄清记录

日期：2026-09-06。用户通过进入 `/to-spec` 确认本轮理解，逐项决策已定。本文作为后续规格输入；尚未开始实现、安装工具链或运行验证。

## 已确认行为

- 可读历史保留至主动删除；内部事件清理前必须保全历史。持久化持续失败时停止受影响任务并拒绝新任务，报告尚未保存内容。崩溃或断电后保证恢复已确认保存的数据，流式待保存内容需明确标示。见 [ADR-0001](adr/0001-preserve-readable-session-history.md)。
- 管家通过 MCP 仅管理直属子会话；子会话继承父会话权限上限，审批和提问交给用户。见 [ADR-0002](adr/0002-limit-manager-to-direct-children.md)。
- 一期正式退出 App 时停止任务并安全保存，后台运行保留到服务化阶段。见 [ADR-0003](adr/0003-stop-tasks-on-app-quit-in-phase-one.md)。
- 测试隔离 Waku 自有资源，允许共用原生 Claude/Codex 配置和会话目录。见 [ADR-0004](adr/0004-isolate-waku-and-share-native-harness-config.md)。

## 沿用既有范围

- 管家使用 Claude；子会话支持 Claude/Codex。子会话默认 worktree，可显式继承工作目录。ACP 留到后续。
- 一期六个工具：`waku_spawn_session`、`waku_prompt`、`waku_status`、`waku_result`、`waku_cancel`、`waku_list_sessions`。原设计文件清单中的“8 个工具”为计数矛盾，以工具表中六个非定时工具为准。
- 一期包含父子树展示和 daemon 自主持久化；二期增加三个定时工具；三期独立常驻服务；四、五、六期分别处理 harness 内部 subagent、资源成本和实时动作流。原观测表的三至五期编号不再适用。
- 复用 `waku-daemon` 的 stdio MCP 子命令方向保留；接口细节须结合当前源码与上述行为收敛。

## 源码核查与规格工作

以下为当前源码事实，行号基于 `5cf34ee`，已由主会话交叉核查：

| 事实 | 证据 | 规格需要覆盖 |
| --- | --- | --- |
| 事件只写内存 journal 并广播；可读快照由客户端发送保存 | `crates/waku-core/src/server.rs:214`；`crates/waku-client/src/persistence.rs:1011` | daemon 自主保存、重放与在线流顺序，以及事件安全清理条件 |
| 数据库可按需恢复已保存的会话详情与消息 | `crates/waku-core/src/persistence.rs:1179` | 复用现有历史表示，明确可靠保存边界；不能仅依赖尚未保存的客户端状态 |
| 非落后快照会整对象替换；当前只额外保留 daemon checkpoint | `crates/waku-core/src/daemon.rs:339` | 所有保存路径及加载路径均保留 daemon 管理的父子关系；serde 默认值和 stale 分支不足以保证这一点 |
| 客户端握手要求协议版本一致 | `crates/waku-client/src/client.rs:93` | 明确版本兼容策略，拒绝不兼容客户端，并测试同版本多客户端保存 |
| 正常退出调用保存，但保存请求采用 notify；daemon 停止超时后会被终止 | `src/app.rs:2479`；`crates/waku-client/src/persistence.rs:1019`；`crates/waku-client/src/process.rs:246` | 停止任务、等待可靠保存确认、失败报告及退出顺序 |
| daemon 默认配置及启动回写仍落在共用配置路径 | `crates/waku-protocol/src/settings.rs:35`；`crates/waku-daemon/src/main.rs:46`；`src/app.rs:1938`；`crates/waku-core/src/settings.rs:76` | 独立测试 daemon 配置，首次启动无原版配置读写副作用 |
| 环境变量可使 App 连接外部 daemon；worktree 与辅助程序目录仍位于共用根目录 | `src/daemon.rs:7`；`crates/waku-core/src/worktree.rs:20`；`crates/waku-core/src/computer_use.rs:267` | 测试启动限定自己的 daemon、工作区与辅助程序，不连接或控制原版实例 |

这些是规格和实现待办，不能作为已经修复的行为。后续 `/to-spec` 需补齐各工具的输入、状态、错误、重试语义及回归验收；涉及新的范围或对外行为决策时再确认。
