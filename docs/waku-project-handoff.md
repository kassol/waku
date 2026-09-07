# Waku 正式仓库交接

日期：2026-09-06。当前项目已接续交接，一期规格及 11 张实施任务已发布，#2–#12 实施任务已完成，一期六工具与原生交互验收通过，证据见[验收记录](waku-steward-acceptance.md)。本文为当前入口；原始交接与设计稿中的历史状态以本文为准。

## 新的已确认设计方向

2026-09-07，用户确认子会话作为临时执行单元，生命周期结束后归档隐藏，主会话保留摘要和追溯入口；问题由管家在已有授权内代决，超出授权统一回主会话询问。见 [ADR 0005](adr/0005-transient-child-sessions-and-manager-decisions.md)。[Issue #23](https://github.com/kassol/waku/issues/23) 及 [#24–#30](waku-steward-child-lifecycle-tickets.md) 已实现，公共 MCP／daemon、真实 harness、签名 Debug 后台交互及组合验收通过。Rust 全量 1046 通过、31 忽略、0 失败；见[生命周期验收记录](waku-steward-child-lifecycle-acceptance.md)。升级不自动处理历史子会话。用户随后授权更新安装；日用 Steward 现为 `ccc7c24` 的优化签名包，4 个会话、149 条消息及配置完整保留，安装与退出阻塞记录见同一验收文档。

## 当前扩展与交付边界

2026-09-07，管家协作扩展已按 [Issue #13](https://github.com/kassol/waku/issues/13) 的[协作规格](waku-steward-orchestration-spec.md)实现可靠 steer/反馈排队、独立即时交流、动态分工与任务集成分支、安全清理。`c431614` 的 Rust 全量 985 项通过、29 项忽略、0 失败，共享客户端 46 项及三端类型检查通过。9 张纵向任务 #14–#22 的公共组合流程和签名 Debug App 原生交互验收完成，#13–#22 已关闭；见[任务清单](waku-steward-orchestration-tickets.md)和[本轮验收记录](waku-steward-orchestration-acceptance.md)。原生复核覆盖真实 Claude 只读长答复、讨论前后父子快照不变、对话框键盘循环、明确指令原文与接收回执，以及两项依赖成果整合交付、清理状态和保留历史。

用户已批准新增 `waku_wait`，详见[管家等待与完成通知](waku-steward-wait.md)。一期六工具验收仍为历史基线。管家登记持久等待后结束当前轮；父轮成功结束且子轮次可处理时，由 daemon 的事件回调自动开启一次通知轮。即时交流查询保留等待。经可靠输入提交的用户 steer 在受理或待核实时暂停旧等待回调，接收确认后撤销同轮旧等待，明确失败允许旧等待继续；新计划按需登记新等待。普通新轮输入、取消及父轮失败或中断继续遵循原有撤销边界。

此前协作版本安装记录：用户已明确授权验收后推送、关闭任务并更新安装 `Waku Steward.app`。当时 Steward 已更新安装至 `/Applications/Waku Steward.app`，签名、后台启动、正常退出及重开验证通过；4 个既有会话、146 条消息和轮次完整保留。可重复安装包为 `~/Downloads/Waku-Steward-2026-09-07-orchestration.zip`。原版 `Waku.app` 继续保持隔离，不得替换或修改。以下历史构建、安装与验收记录不代表本次扩展的安装状态。

## 已有资料（引用，不重做）

- 原始设计交接：[原始设计交接](waku-steward-handoff.md)。
- 设计 v2：[设计 v2](waku-steward-design.md)。
- 当前一期澄清：[澄清记录](waku-steward-scope.md)，包含逐项已确认决策、ADR 与当前源码证据。
- 已发布规格：[GitHub Issue #1](https://github.com/kassol/waku/issues/1)；仓库副本见[一期规格](waku-steward-spec.md)。规格和测试边界已确认；本轮已获实现授权。
- 已发布任务：[11 张纵向任务清单](waku-steward-tickets.md)，对应 GitHub #2–#12，具备原生阻塞关系。已完成 [#2 测试 App 独立启动与资源隔离](https://github.com/kassol/waku/issues/2) 和 [#3 历史持久化](https://github.com/kassol/waku/issues/3)，[#4 Codex 子会话创建](https://github.com/kassol/waku/issues/4) 已验收；[#11 安全退出](https://github.com/kassol/waku/issues/11) 已验收。
- 先读上述文件，再读取正式仓库及其父目录适用的 `AGENTS.md` / `CONTEXT.md`。迁移前已读取正式仓库根规范，并检查测试 App 隔离相关源码；期 1 源码复核与逐票 Standards / Spec 评审已执行，最终证据见[验收记录](waku-steward-acceptance.md)。

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

工程技能配置、`/to-spec`、`/to-tickets` 及一期 `/implement` 已完成。后续新范围以新的 Issue 和实际授权为准。

1. 工程技能配置已完成：任务使用 `kassol/waku` GitHub Issues，保留默认 triage 标签，领域文档采用单一上下文；配置见 `docs/agents/`，无需重复初始化。
2. 一期规格和任务已发布，实施以对应 Issue 和 ADR 为准；新增范围或对外行为决策仍需用户确认。
3. 隔离、构建与历史持久化基线已通过；创建、MCP 六工具、递归、幂等重试、父子导航和安全退出已验收；最终 #12 集成检查已通过。本轮授权已覆盖安装、实现与隔离验证；完成构建及隔离检查后方可启动测试 App。
4. 后续实现逐票完成必要回归检查与 Standards / Spec 两轴评审，凭实际验收证据关闭对应 Issue。

以下建仓时疑点已在本轮澄清中处理，详细决策和源码证据见[一期澄清记录](waku-steward-scope.md)；以下决定已落实并纳入验证：

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
- 完整 Rust workspace 919 项通过、26 项既有外部环境测试忽略；最终增量完整桌面回归 382 项通过；共享 TypeScript 客户端类型检查、39 项测试与生成协议检查通过。
- Rust 1.98 的全仓格式检查存在既有差异；在临时目录提取未修改 HEAD 后复核，输出与当前工作区完全一致。本票未重排无关代码。


## 历史持久化实现基线

- #3：daemon 使用共享历史解释器，将有序事件和可读历史在 SQLite WAL FULL 事务中提交；成功提交后单独确认保存游标。客户端补取超出热窗口的持久事件，按游标去重；本阶段不清理持久事件。
- 桌面保存运行在后台，只清除已确认的修改版本；尾批停止后仍会到期保存。旧快照、晚到详情及回退保留较新状态，未决权限请求和提问随历史恢复。
- 已通过 Rust/TypeScript 相关回归、workspace 检查、生成协议检查及 Standards / Spec 复核。真实隔离 Astra 会话已完成，最新签名 Debug App 重开后显示完整问答和 History saved。
- Computer Use 使用指定 Debug App 的后台操作；macOS Debug 启动不主动激活窗口，避免 watcher 重建抢键盘焦点。原版 App 与设置文件校验值、原版 App/daemon PID 复核不变。
- #9 已完成实际停止确认；#11 已完成持续故障停止、可靠退出及崩溃恢复；#12 的多会话证据见[验收记录](waku-steward-acceptance.md)。

## 安全退出实现基线

- 2026-09-06 后续修复：daemon 终端关闭在发送 HUP 和等待子进程退出期间继续读取输出，避免 PTY 排空受阻；自有 shell 超过 2 秒仍未退出时发送 KILL，再停止输出读取。只处理持有的子进程。该次修复验证期间安装中的 Steward 保持不变。

- macOS 菜单退出先返回 `NSTerminateCancel`，让 GPUI 主队列继续完成保存与 daemon 排空；成功后经 `cx.quit()` 异步发起第二次退出，一次性允许 `NSTerminateNow`。禁止在此使用 `NSTerminateLater`，它的嵌套循环会阻塞主队列中的保存任务。`cargo run --example quit_bridge_probe` 在隐藏窗口验证真实退出桥、保存失败重试及最终退出；旧实现会在 5 秒看门狗处失败。
- 独立 Steward 优化安装包已验证原生应用菜单 Quit：窗口与自有 daemon 均正常退出，后台重开后历史消息与轮次数量一致。系统外部发起的退出与应用主队列发起的退出必须分别覆盖，前者成功不能证明后者正常。
- #11（6251fc6）：持久化故障保留未确认事件，停止受影响任务并拒绝新工作。正常退出先阻止新工作，排空运行时及尾部事件，确认保存，再等待自有 daemon 退出；保存或退出未确认时保留窗口并显示错误。外部 daemon 保持独立所有权。
- 已通过保存故障、尾部事件、未确认进程退出、共享运行时排空、崩溃恢复及创建/退出并发回归，桌面与 daemon 编译、共享客户端类型与协议生成检查、Standards / Spec 复核。
- 隔离 Debug 的原生系统 Quit 与重开验收通过。两个真实 Astra 会话的完整结果、父子关系及保存游标保持一致，界面显示子会话结果和 History saved；原版进程及文件摘要不变。
- #4（305b819）：真实子会话创建、执行、持久化及原生重开结果已验证。后台键盘接口恢复后，已通过会话搜索找到 Child creation E2E 并按 Enter 打开，完整问答与 Astra 模型正确；#4 已验收。

## 管家查询接口

- `waku_list_sessions`、`waku_status`、`waku_result` 复用 daemon 的 `StewardQuery` 命令；入口绑定父会话、项目及运行时，目标限直属子会话。查询不复用请求缓存；等待期间重新校验权限和运行时撤销。
- 状态分别返回会话状态、当前或最近轮次、开放轮次、权限审批与用户回答等待原因。`wait_ms` 默认 0，允许 0..60000；最多 128 个目标，变化或超时返回当前快照。后台线程等待，短期持锁，不占 UI 线程或退出工作锁。
- 失败原因保存在会话历史中，进程退出及重开后仍可查询；新轮提交、开始或成功结束清除旧原因。
- 结果按当前或最近轮次收集工具事件前后的全部助手回复；无轮次、运行中、失败和中断保留真实状态。可选历史按原生消息与活动顺序返回；`max_chars` 默认 20000，允许 1..100000，分别限制回复和历史的 Unicode 字符数，并分别标记截断。读取不改写历史。
- #6（3843107）：Rust 四包 517 项、真实 stdio 子进程/daemon/临时 Git/SQLite 查询回归、共享客户端 39 项、主 checkout 类型与编译检查通过；Standards / Spec 复核发现的锁序和失败原因问题均已修复。
- 唯一 watcher 重建签名 Debug 后，真实 Claude 各调用一次列表、状态、结果工具，实际工具输出确认直属 Astra 子会话已完成，回复与可选历史正确、无截断。父会话新轮和三个工具均完成，历史已保存；#6 已验收。

## MCP 创建实现基线

- #5（a740ece）：Claude 启动时注入独立 stdio MCP 配置，复用 daemon 二进制；按父会话、项目、运行时绑定可撤销权限。作用域连接仅接收获准响应，通用桌面接口、全局广播与跨身份缓存均隔离。保留共用原生 CLI 配置。
- Rust 四包 502 项、共享客户端 30 项、主 checkout 类型检查及生成协议检查通过；独立 Standards / Spec 复核通过。真实 stdio 子进程与临时 SQLite/Git 的权限、并发、项目变化和撤销回归通过。
- 真实 Claude Code 2.1.263（现有 fable 配置）经 MCP 创建 gpt-6-astra 子会话；父子轮次均结束，独立 worktree 中指定文件内容正确。后台原生 App 显示子会话、完整首轮结果和 History saved。#5 已关闭。
- #5 验收时公开创建工具；#6 查询接口及真实原生查询闭环已验收。#8 创建重试、工作目录选择及重启后幂等均已验收；#4 的后台键盘打开验收已完成。

## 父子导航与长期历史实现基线

- #10（ca347eb、7b1b8fc）：侧边栏按父子关系生成虚拟化树，支持展开、折叠、方向键导航与孤儿标记。历史仅清理已保存的事件前缀；超过回放窗口时从一致性快照恢复，大快照分块传输并在后台解码。
- 已通过深层树与环、并发清理、一致性快照、超过 48 MiB 的真实 WebSocket 回放/加载、并发用户修改保留回归。主 checkout 桌面与 daemon 编译、共享客户端 38 项测试、类型检查及生成协议检查通过。
- watcher 已成功重建签名 Debug App。后台原生展开、折叠及打开子会话通过；三个真实会话的历史结果、父子关系与已保存游标核验通过。后台键盘投递已恢复，#4 搜索打开通过；已修复窗口 Tab/Shift+Tab 焦点处理和菜单 Tab 焦点泄漏，GPUI 回归通过；签名 Debug 后台原生 Shift+Tab 聚焦树节点、方向键折叠/展开及 Enter 打开真实子会话全部通过，#10 已验收。

## Claude 子会话与递归委派

- `waku_spawn_session` 使用同一 daemon 创建 Claude/Codex 子会话。两种 provider 启动时均获得按会话、项目及运行时绑定的 MCP；Claude 使用 `--mcp-config`，Codex 使用进程级 `-c mcp_servers.waku=...`，不写共用原生配置。
- 每层 MCP 仅管理直属子会话；子会话可继续委派，父子关系沿用既有持久化。跨 provider 仅接受 Ask 或权限上限为 FullAccess 的父会话；其他无法安全映射的自动审批组合明确拒绝。同 provider 保持现有权限上限。
- 复用现有新会话入口，以说明文字提示委派能力；审批与回答仍由用户进入子会话处理。真实 Astra → Claude（fable）→ Astra 三级委派均完成，孙级独立 worktree 指定文件正确，三层历史已保存；原生搜索经方向键和 Enter 打开 Claude 子会话显示完整回复，#7 已验收。

## 创建重试与工作目录

- #8 创建记录在产生资源前持久化；相同管家、相同幂等键和请求返回保存结果，重启不重发首轮。重试重新验证项目、父会话及权限。
- 默认 worktree，inherit 使用父会话实际目录，local 使用项目普通检出目录；失败保留已产生资源并返回阶段及不确定状态。与 #7 整合后，Claude/Codex 共用此创建流程。
- 独立 Standards / Spec 复核、整合回归和真实 Astra inherit 创建通过；同键同请求及 Debug daemon 重启后均返回原 child/turn，首轮只执行一次，#8 已验收。

## 后续输入与取消实现边界

- `waku_prompt` 向直属 Claude/Codex 子会话投递：空闲时开始保存后的新轮，忙碌且支持 steer 时发送当前轮输入，明确不支持时持久排队。等待用户或取消未确认时拒绝普通输入。`delivery_id` 绑定调用者、目标与原文，同键重试返回原记录，`waku_prompt_status` 查询实际状态；投递及查询重验关系与权限。
- daemon 提交前保存受理记录；Codex 确认 provider 接收，Claude 确认传输接收。部分写入、断连、确认丢失或重启后的不确定输入不自动重发。持久队列按受理顺序消费，出队再次核对当前轮次、权限和用户等待；具体边界见[可靠输入说明](waku-steward-input.md)。这些扩展的实际验收及安装状态见[本轮验收记录](waku-steward-orchestration-acceptance.md)。
- `waku_cancel` 返回独立的 accepted 与 stopped。取消受理保留开放轮次；provider 中断、结束或进程退出确认后才显示 Interrupted。取消按当前轮次记录，保留历史和工作区；Codex 忽略旧轮次的迟到结束通知。Claude 仍有后台任务时请求关闭原生运行时，保持开放轮次直到进程退出确认。
- 桌面及共享 TypeScript 客户端沿用同一停止边界。真实 Claude 经 MCP 向 Astra 提交后续输入并精确返回标记；第二轮实际执行 sleep 60，取消先受理、18 秒时确认 Interrupted，状态/结果及保存历史一致，#9 已验收。自动回归使用临时数据库、Git 目录和可控子进程。

## 一期整体交付

六工具、两种原生 provider、递归、创建幂等与重启恢复、实际取消、后台键盘审批和回答、父子导航及保存退出均完成验证。最终修复包含禁用控件焦点、回答卡片顺序、中断轮次可见提示及空输出中断的修改摘要。三会话并发流与每会话千条历史验证、性能测量边界、隔离与清理记录见[一期验收](waku-steward-acceptance.md)。日常开发仍遵循唯一 watcher；测试身份不能替代原版安装。

## 独立日用包（2026-09-06）

用户授权安装 `Waku Steward.app` 供日常使用。运行 `sh scripts/bundle.sh steward`，使用继承 release 优化的 `steward` profile 和 `waku-protocol/steward` feature。产物为 `target/steward/Waku Steward.app`，安装到 `/Applications/Waku Steward.app`，不依赖开发 watcher。

- 应用身份 `sh.waku.steward`；helper 身份 `sh.waku.steward.computer-use`。
- 配置、projectless 工作区、worktree、helper 位于 `~/.waku-steward`。
- 数据库及附件位于 `~/Library/Application Support/Waku Steward`；模型缓存位于 `~/Library/Caches/Waku Steward`。
- 始终启动包内 daemon，忽略外部 daemon 地址、token 和可执行路径覆盖；关闭上游自动更新和上游分析上报。
- 启动时保持后台，避免抢占键盘焦点。用户通过 Dock 或 Finder 激活窗口。
- 不读取或迁移原版 Waku 数据；原生 Claude/Codex 登录配置继续按 ADR-0004 共用。
- 后续更新重新构建并替换 Steward 包，保留上述独立数据目录。普通 release 仍是上游身份，禁止用于此安装流程。

本机安装验证：优化构建成功；App、REPL、daemon 及嵌套 helper 签名检查通过；4 项身份、配置路径及导航回归检查通过。传入无效外部 daemon 地址和路径后，安装版仍启动自己的包内 daemon，并通过真实 WebSocket `loadTaskState` 读取独立数据库。初次启动自动创建 1 个空白会话。清除验证启动参数后，经 Launch Services 后台重启成功。原版 App、Info.plist、settings.json 的 SHA-256 和原版两个进程均保持不变。

本机使用现有 Apple Development 证书签名；该包用于本机使用，未做面向其他设备分发的公证。可重复安装包保存在 `~/Downloads/Waku-Steward-2026-09-06.zip`。

## Codex 生命周期修复（2026-09-06）

原生 Codex 子代理与主线程共用连接。子线程通知曾覆盖主轮次 ID，并提前结束主轮，导致最终回复后仍显示 Working，steer/cancel 缺少活动轮次。driver 现在按线程 ID 隔离通知；子代理审批和回答请求继续按 RPC ID 处理。

Codex 退出现在先请求 EOF，超过 2 秒仍未结束时终止自有进程组，再排空输出并确认退出。进程组主进程在管道排空前保持可等待，避免 PID 重用；覆盖忽略 EOF 和子进程继承输出管道两种阻塞。

完整 workspace library 回归 919 项通过、26 项忽略；独立真实 Codex 0.153.4 回归验证 Astra 原生子代理、主轮完成、后续输入、steer、cancel 与退出全部通过。可运行 `cargo test -p waku-core native_codex_child_followup_steer_cancel_and_drop -- --ignored --nocapture` 重验，该检查使用真实模型和临时目录。

包含 IME 候选窗坐标修复的优化包已更新本机 Steward，正常系统退出与后台重开通过，原会话历史和待发送消息保留。安装包为 `~/Downloads/Waku-Steward-2026-09-06-reliability-fix.zip`。搜狗实际候选窗交互仍待日常使用确认，输入法重绘前坐标回归已通过。
