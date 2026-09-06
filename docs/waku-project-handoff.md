# Waku 正式仓库交接

日期：2026-09-06。当前项目已接续交接，一期规格及 11 张实施任务已发布，已完成 #2 隔离、#3 历史持久化、#5 MCP 创建与 #11 安全退出任务，继续按依赖实现后续任务。本文为当前入口；原始交接与设计稿中的历史状态以本文为准。

## 已有资料（引用，不重做）

- 原始设计交接：[原始设计交接](waku-steward-handoff.md)。
- 设计 v2：[设计 v2](waku-steward-design.md)。
- 当前一期澄清：[澄清记录](waku-steward-scope.md)，包含逐项已确认决策、ADR 与当前源码证据。
- 已发布规格：[GitHub Issue #1](https://github.com/kassol/waku/issues/1)；仓库副本见[一期规格](waku-steward-spec.md)。规格和测试边界已确认；本轮已获实现授权。
- 已发布任务：[11 张纵向任务清单](waku-steward-tickets.md)，对应 GitHub #2–#12，具备原生阻塞关系。已完成 [#2 测试 App 独立启动与资源隔离](https://github.com/kassol/waku/issues/2) 和 [#3 历史持久化](https://github.com/kassol/waku/issues/3)，继续推进 [#4 Codex 子会话创建](https://github.com/kassol/waku/issues/4) ；[#11 安全退出](https://github.com/kassol/waku/issues/11) 已验收。
- 先读上述文件，再读取正式仓库及其父目录适用的 `AGENTS.md` / `CONTEXT.md`。迁移前已读取正式仓库根规范，并检查测试 App 隔离相关源码；期 1 源码复核仍待执行。

## 建仓时的核验记录（2026-09-06）

- 创建 fork：`https://github.com/kassol/waku`，GitHub API 确认 parent 为 `egoist/waku`。
- 克隆正式仓库到 `~/Workspace/waku`。
- `origin` 为 `https://github.com/kassol/waku.git`。
- `upstream` 为 `https://github.com/egoist/waku.git`。
- 交接前重新检查：HEAD `3395979`，工作区干净。本轮没有源代码或项目文档改动，没有安装 Rust、运行构建或测试。
- 临时克隆 `~/Workspace/temp/waku` 保留。原交接基于 `0988a1c`；正式仓库版本更新，原行号和行为判断需按当前源码复核。

## 用户明确约束

- **测试 App 必须与已安装的原版 Waku 隔离**（2026-09-06 新增）：不得覆盖、退出、升级或修改原版 App，不得写入原版配置、数据库、会话及工作目录，不得连接或控制原版 daemon。测试使用独立应用标识、数据目录、daemon 与测试工作区；首次启动前完成隔离核查。本轮源码已将 Debug daemon 设置、projectless/worktree 与辅助程序放入 checkout 的 `temp/`，沿用独立数据库、缓存及应用身份。Debug 启动只使用自有 daemon，watcher 验证 `sh.waku.dev` 身份并按进程句柄停止；Debug 自动更新关闭。launcher、辅助程序、真实 daemon 保存/重开/停止及路径隔离回归通过；`cargo check --workspace` 和 watcher 构建通过，签名 App 已启动，原版进程 PID 和核对文件摘要保持不变。#2 已验收。
- **只做单向同步**：仅从 `egoist/waku` 拉取并同步更新；开发提交只推送 `kassol/waku`。禁止向上游推送代码、创建 PR、Issue、评论或发送其他内容。
- 测试隔离的明确例外：用户允许共用原生 Claude/Codex 配置和会话目录，测试产生的原生会话可与现有环境共存；具体边界见 [ADR-0004](adr/0004-isolate-waku-and-share-native-harness-config.md)。
- **按产品方向融合上游**：逐项评估上游变更，选择采纳、适配、跳过或延期。每轮记录完整上游 SHA 与处理结论；流程和增量基线统一维护在[上游同步记录](upstream-sync.md)。
- 该约束已保存到 Nowledge Mem：`65a49358-e7c5-428f-aad8-4d1b4f61f145`。
- 当前仅记录了操作约束；未配置 upstream 的技术性推送禁用措施，Git remote 仍显示其正常 push URL。
- 2026-09-06 用户已授权使用 Homebrew 安装工具链，当前已安装 Rust 1.98.0（Cargo、rustfmt、Clippy），已有 Bun 1.4.2。随后用户明确授权通过 subagents 完成全部 Issue 并进行隔离端到端验证。先前暂缓安装与实现的记录仅描述历史状态。
- 中文简洁回复。遵循当前会话实际加载的用户规范及项目规范；原交接中观察到的偏好不能覆盖现行指令。

## 建议的下一步及授权边界

工程技能配置、`/to-spec` 与 `/to-tickets` 已完成。后续按原生阻塞关系逐票 `/implement`，每次只领取前置任务已完成的票。

1. 工程技能配置已完成：任务使用 `kassol/waku` GitHub Issues，保留默认 triage 标签，领域文档采用单一上下文；配置见 `docs/agents/`，无需重复初始化。
2. 一期规格和任务已发布，实施以对应 Issue 和 ADR 为准；新增范围或对外行为决策仍需用户确认。
3. 隔离、构建与历史持久化基线已通过；从 #4 Codex 子会话创建继续推进，#11 保存故障与安全退出已完成，随后按依赖完成 MCP 委派与查询闭环。本轮授权已覆盖安装、实现与隔离验证；完成构建及隔离检查后方可启动测试 App。
4. 后续实现逐票完成必要回归检查与 Standards / Spec 两轴评审，凭实际验收证据关闭对应 Issue。

以下建仓时疑点已在本轮澄清中处理，详细决策和源码证据见[一期澄清记录](waku-steward-scope.md)；隔离任务已完成，以下功能仍待后续任务验证：

- 用户确认历史保留、持久化失败停止、异常退出仅保证已确认保存的数据。
- 一期六个会话工具，观测 B/C/D 沿用四至六期。
- 历史快照合并已覆盖父子关系及并发用户修改；保存、加载和回放均需保留同一关系。

## Suggested skills

下一 agent 按阶段调用 Skill 工具；工具不可用时读取对应 `SKILL.md`：

- `setup-matt-pocock-skills`：先核查工程流程前置配置；缺失技能时查找实际安装位置。
- `to-spec`：将设计与源码复核结果收敛为期 1 规格。
- `to-tickets`：规格确认后拆成独立可验收、声明阻塞关系的任务。
- `domain-modeling` / `writing-for-agents`：需要建立术语和代理文档规范时使用。
- `implement` / `tdd` / `code-review`：开始实现后使用，本次交接不自动开启实现。

Matt 流程产生仓库产物时，遵循用户的独立提交及默认 push 约定；push 目标只能是 origin，并先检查就近项目规范。交接与设计现已迁入 `docs/`，后续在项目内维护；迁移本身不代表授权开始功能实现。

## 当前验证基线

- 2026-09-06：隔离 Debug App 由唯一 `bun ./scripts/dev.ts` watcher 构建和启动；签名验证通过，身份 `sh.waku.dev`。
- Rust 隔离回归及共享 TypeScript 客户端类型检查、22 项测试通过。
- Rust 1.98 的全仓格式检查存在既有差异；在临时目录提取未修改 HEAD 后复核，输出与当前工作区完全一致。本票未重排无关代码。


## 历史持久化实现基线

- #3：daemon 使用共享历史解释器，将有序事件和可读历史在 SQLite WAL FULL 事务中提交；成功提交后单独确认保存游标。客户端补取超出热窗口的持久事件，按游标去重；本阶段不清理持久事件。
- 桌面保存运行在后台，只清除已确认的修改版本；尾批停止后仍会到期保存。旧快照、晚到详情及回退保留较新状态，未决权限请求和提问随历史恢复。
- 已通过 Rust/TypeScript 相关回归、workspace 检查、生成协议检查及 Standards / Spec 复核。真实隔离 Astra 会话已完成，最新签名 Debug App 重开后显示完整问答和 History saved。
- Computer Use 使用指定 Debug App 的后台操作；macOS Debug 启动不主动激活窗口，避免 watcher 重建抢键盘焦点。原版 App 与设置文件校验值、原版 App/daemon PID 复核不变。
- #9 仍负责实际停止确认；#11 已完成持续故障停止、可靠退出及崩溃恢复；#12 负责完整多会话原生验收。

## 安全退出实现基线

- #11（6251fc6）：持久化故障保留未确认事件，停止受影响任务并拒绝新工作。正常退出先阻止新工作，排空运行时及尾部事件，确认保存，再等待自有 daemon 退出；保存或退出未确认时保留窗口并显示错误。外部 daemon 保持独立所有权。
- 已通过保存故障、尾部事件、未确认进程退出、共享运行时排空、崩溃恢复及创建/退出并发回归，桌面与 daemon 编译、共享客户端类型与协议生成检查、Standards / Spec 复核。
- 隔离 Debug 的原生系统 Quit 与重开验收通过。两个真实 Astra 会话的完整结果、父子关系及保存游标保持一致，界面显示子会话结果和 History saved；原版进程及文件摘要不变。
- #4 实现已提交 305b819，真实子会话创建、执行、持久化及原生重开结果已验证。后台键盘操作验收仍受输入工具不稳定影响，保持 Issue 开放；MCP 代码和长期历史工作在独立 worktree 推进。

## MCP 创建实现基线

- #5（a740ece）：Claude 启动时注入独立 stdio MCP 配置，复用 daemon 二进制；按父会话、项目、运行时绑定可撤销权限。作用域连接仅接收获准响应，通用桌面接口、全局广播与跨身份缓存均隔离。保留共用原生 CLI 配置。
- Rust 四包 502 项、共享客户端 30 项、主 checkout 类型检查及生成协议检查通过；独立 Standards / Spec 复核通过。真实 stdio 子进程与临时 SQLite/Git 的权限、并发、项目变化和撤销回归通过。
- 真实 Claude Code 2.1.263（现有 fable 配置）经 MCP 创建 gpt-6-astra 子会话；父子轮次均结束，独立 worktree 中指定文件内容正确。后台原生 App 显示子会话、完整首轮结果和 History saved。#5 已关闭。
- 本阶段只公开创建工具。#6 查询闭环、#8 创建重试与工作目录选择、#10 父子导航与长期历史继续推进；#4 的后台键盘验收仍保留开放状态。


## 父子导航与长期历史实现基线

- #10（ca347eb、7b1b8fc）：侧边栏按父子关系生成虚拟化树，支持展开、折叠、方向键导航与孤儿标记。历史仅清理已保存的事件前缀；超过回放窗口时从一致性快照恢复，大快照分块传输并在后台解码。
- 已通过深层树与环、并发清理、一致性快照、超过 48 MiB 的真实 WebSocket 回放/加载、并发用户修改保留回归。主 checkout 桌面与 daemon 编译、共享客户端 38 项测试、类型检查及生成协议检查通过。
- watcher 已成功重建签名 Debug App。后台原生展开、折叠及打开子会话通过；三个真实会话的历史结果、父子关系与已保存游标核验通过。后台键盘投递仍不稳定，#4 与 #10 保持开放，待完整键盘验收。
