# Waku 正式仓库交接

日期：2026-09-06。当前项目已接续交接，一期规格已发布，处于任务拆分阶段。本文为当前入口；原始交接与设计稿中的历史状态以本文为准。

## 已有资料（引用，不重做）

- 原始设计交接：[原始设计交接](waku-steward-handoff.md)。
- 设计 v2：[设计 v2](waku-steward-design.md)。
- 当前一期澄清：[澄清记录](waku-steward-scope.md)，包含逐项已确认决策、ADR 与当前源码证据。
- 已发布规格：[GitHub Issue #1](https://github.com/kassol/waku/issues/1)；仓库副本见[一期规格](waku-steward-spec.md)。规格和测试边界已确认，尚未实施。
- 先读上述文件，再读取正式仓库及其父目录适用的 `AGENTS.md` / `CONTEXT.md`。迁移前已读取正式仓库根规范，并检查测试 App 隔离相关源码；期 1 源码复核仍待执行。

## 建仓时的核验记录（2026-09-06）

- 创建 fork：`https://github.com/kassol/waku`，GitHub API 确认 parent 为 `egoist/waku`。
- 克隆正式仓库到 `~/Workspace/waku`。
- `origin` 为 `https://github.com/kassol/waku.git`。
- `upstream` 为 `https://github.com/egoist/waku.git`。
- 交接前重新检查：HEAD `3395979`，工作区干净。本轮没有源代码或项目文档改动，没有安装 Rust、运行构建或测试。
- 临时克隆 `~/Workspace/temp/waku` 保留。原交接基于 `0988a1c`；正式仓库版本更新，原行号和行为判断需按当前源码复核。

## 用户明确约束

- **测试 App 必须与已安装的原版 Waku 隔离**（2026-09-06 新增）：不得覆盖、退出、升级或修改原版 App，不得写入原版配置、数据库、会话及工作目录，不得连接或控制原版 daemon。测试使用独立应用标识、数据目录、daemon 与测试工作区；首次启动前完成隔离核查。当前仅检查源码，尚未完成隔离实现或运行验证。Debug 已有独立应用标识与数据库路径，但 daemon 配置默认路径仍为 `~/.waku/settings.json`（`crates/waku-protocol/src/settings.rs` 的 `DaemonSettings::default_path`），`worktree::create` 仍使用 `~/.waku/worktrees`，`src/daemon.rs` 还支持通过环境变量连接外部 daemon。首次启动前需核查并补齐这些隔离边界。此约束不改变暂缓工具链安装与功能实现的状态。
- **只做单向同步**：仅从 `egoist/waku` 拉取并同步更新；开发提交只推送 `kassol/waku`。禁止向上游推送代码、创建 PR、Issue、评论或发送其他内容。
- 测试隔离的明确例外：用户允许共用原生 Claude/Codex 配置和会话目录，测试产生的原生会话可与现有环境共存；具体边界见 [ADR-0004](adr/0004-isolate-waku-and-share-native-harness-config.md)。
- **按产品方向融合上游**：逐项评估上游变更，选择采纳、适配、跳过或延期。每轮记录完整上游 SHA 与处理结论；流程和增量基线统一维护在[上游同步记录](upstream-sync.md)。
- 该约束已保存到 Nowledge Mem：`65a49358-e7c5-428f-aad8-4d1b4f61f145`。
- 当前仅记录了操作约束；未配置 upstream 的技术性推送禁用措施，Git remote 仍显示其正常 push URL。
- 延续原先暂缓安装 Rust、暂未实现的状态。此次 fork、clone 与 handoff 不代表授权开始功能实现或安装工具链。
- 中文简洁回复。遵循当前会话实际加载的用户规范及项目规范；原交接中观察到的偏好不能覆盖现行指令。

## 建议的下一步及授权边界

工程技能配置与 `/to-spec` 已完成，当前执行 `/to-tickets`；拆分确认后发布子任务并建立阻塞关系，随后才逐票 `/implement`。

1. 工程技能配置已完成：任务使用 `kassol/waku` GitHub Issues，保留默认 triage 标签，领域文档采用单一上下文；配置见 `docs/agents/`，无需重复初始化。
2. 一期规格已发布，使用已确认规格与 ADR 拆票；新增范围或对外行为决策仍需用户确认。
3. 拆分带阻塞关系的纵向任务。建议首个行为闭环：Claude 管家派生 Codex 子会话 → 查询完成状态 → 读取结果；构建基线是实现验收的前置条件。
4. 后续实现逐票完成必要回归检查与 Standards / Spec 两轴评审。

以下建仓时疑点已在本轮澄清中处理，详细决策和源码证据见[一期澄清记录](waku-steward-scope.md)，实现仍未开始：

- 用户确认历史保留、持久化失败停止、异常退出仅保证已确认保存的数据。
- 一期六个会话工具，观测 B/C/D 沿用四至六期。
- 当前快照确有整对象替换路径；父子关系必须覆盖全部保存及加载路径，不能只处理 stale merge。

## Suggested skills

下一 agent 按阶段调用 Skill 工具；工具不可用时读取对应 `SKILL.md`：

- `setup-matt-pocock-skills`：先核查工程流程前置配置；缺失技能时查找实际安装位置。
- `to-spec`：将设计与源码复核结果收敛为期 1 规格。
- `to-tickets`：规格确认后拆成独立可验收、声明阻塞关系的任务。
- `domain-modeling` / `writing-for-agents`：需要建立术语和代理文档规范时使用。
- `implement` / `tdd` / `code-review`：开始实现后使用，本次交接不自动开启实现。

Matt 流程产生仓库产物时，遵循用户的独立提交及默认 push 约定；push 目标只能是 origin，并先检查就近项目规范。交接与设计现已迁入 `docs/`，后续在项目内维护；迁移本身不代表授权开始功能实现。
