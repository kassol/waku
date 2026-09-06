# 管家协作扩展验收记录

2026-09-07。合同为 [Issue #13](https://github.com/kassol/waku/issues/13) 的[协作规格](waku-steward-orchestration-spec.md)和 [#14–#22 纵向任务](waku-steward-orchestration-tickets.md)。本轮实现、公共组合流程、自动化检查及签名 Debug App 原生交互验收已通过。用户已明确授权验收后推送、关闭任务并更新安装 Waku Steward；本文更新时 #14–#22 待关闭，Steward 待安装，安装后结果另行补记。原版 Waku 的隔离保护继续有效。

## 检查边界

主审查基线为 `2ea0c7865a576ab28d9b3872df2f4b797892943d`，最终检查代码为 `c4316144a696858bb76e8356847287bc6d35913c`。证据目录为 `/tmp/waku-orchestration-implementation`；下表文件名均相对此目录。临时日志用于本机复核，测试源码提供可重复执行入口。

| 检查 | 当前已确认结果 | 证据 |
| --- | --- | --- |
| Rust workspace 全量回归 | 985 项通过，29 项忽略，0 失败 | `rust-full-20260907.log` |
| 共享 TypeScript 客户端 | 46 项通过，0 失败 | `client-full-20260907.log` |
| 原生 Codex、Claude tracked input | 2 项通过；各有 prompt 与 steer 确认，同一轮结束，回复含 steer 指定标记 | `native-tracked-final.log` |
| Rust 工作区历史快照 | 旧实现未恢复 revision 2，修复后 1 项通过 | `snapshot-red-rust.log`、`snapshot-green-rust.log` |
| TypeScript 工作区历史快照 | 旧实现回退至 revision 1，修复后文件内 20 项通过；同时纳入上述全量回归 | `snapshot-red-ts.log`、`snapshot-green-ts.log` |
| 即时交流长答复键盘滚动 | 旧实现 Down 后仍为 0 px，修复后 1 项通过 | `keyboard-red.log`、`keyboard-green.log` |
| 对话框焦点循环 | 旧实现第 7 次 Tab 逃出对话框，修复后回归通过，并完成原生 Tab、Shift+Tab、Enter 操作 | `focus-red-20260907.log`、`focus-green-20260907.log` |
| 类型与协议 | 客户端、Web、Mobile 类型检查及协议绑定检查通过 | `client-typecheck-20260907.log`、`web-typecheck-20260907.log`、`mobile-typecheck-20260907.log`；绑定补丁检查见 `/tmp/waku-consultation-display-bindings-check.log` |
| Debug 启停隔离 | 1 项通过，保护安装版 sentinel 并拒绝 release 身份 | `dev-process-tests.log` |

键盘回归使用 GPUI 测试窗口，验证方向键、PageUp/PageDown、Home/End 的滚动与重绘；签名 App 中的实际焦点和键盘操作另见下方原生验收。实现见 `src/app/consultation.rs` 的 `consultation_history` 和 `history_keyboard_tests`。

原生接收检查使用临时目录和新原生会话，复用现有认证；Codex 使用 `gpt-6-astra`，Claude 使用已配置模型。Codex 确认层级为 provider 接收，Claude 为传输接收；两者均不把确认解释为模型采纳。测试结束等待自有进程退出并移除临时目录，见 `crates/waku-core/src/driver/native_input_tests.rs`。

## 公共接口场景与复核修复

`crates/waku-core/src/steward_coordination_tests.rs` 的 `mcp_coordination_batches_reviews_feedback_wait_and_dependency_evidence` 使用真实 MCP、daemon socket、临时 Git/SQLite 和可控 provider。场景包含固定提交评审、集中 steer、独立只读讨论、明确指令因不支持 steer 而排队、父轮结束后自动接收指令、按新方向登记等待并结束、完成回调、依赖成果整合、组合验收、本地交付、自动清理及 daemon 重开。自动清理后比较 5 个会话的消息、轮次、活动、投递和工作区记录，并核查持久交付引用。带未跟踪文件的协调目录保留；两项已验收子任务资源及干净集成资源被清理。

组合回归经实际受限交流进程核对讨论不改变父任务消息、轮次、等待或投递记录；明确指令依次显示 `Queued`、`Received`，只提交一次，原等待撤销后由新等待恢复一次回调。后继任务按新要求生成 `dependent.txt`，清理重开后交流记录和指令投递仍一致。可控父进程仅在测试 provider 边界声明不支持 steer，投递和生命周期仍使用实际子进程；没有新增产品 provider。红绿日志为 `logs/combined-red.log` 与 `logs/combined-green.log`。更多竞态、权限与不确定恢复继续由 `consultation_tests.rs` 和 `steward_input_tests.rs` 的公共 socket 回归覆盖。

复核已补充以下可运行检查；最终通过状态以上方全量日志及各专项日志为准：

- 递归子管家在独立集成目录归集直属成果，运行目录保持原位；仅根任务可最终交付，交付后拒绝创建新子任务。见 `task_workspace_delivery_tests.rs`。
- 清理保留 `Queued`、`Accepted`、`Uncertain` 投递以及取消未确认的执行目录；重试重新检查实际资源。见 `task_cleanup_tests.rs`。
- 共享目录和路径别名的第二个 runtime 被拒绝，同时公开 `ReadTextFile` 和已提交版本 `CollectReviewDiff` 仍可读取，保留活动 runtime、HEAD、文件和工作区状态。见 `task_workspace_socket_tests.rs`。
- Rust 与 TypeScript 历史快照恢复工作区证据并保留较新 revision；协议版本提升至 15。见 `crates/waku-protocol/src/history.rs`、`protocol.rs` 和 `packages/waku-client/src/event-reducer.ts`。
- 即时交流明确指令通过可选 `display_content` 显示用户原文；provider 与审计保留完整上下文，空闲、steer、排队及重开沿用同一消息表示，旧记录继续兼容。见 `crates/waku-core/src/steward_input.rs`、`crates/waku-protocol/src/history.rs` 和 `src/app/transcript_view.rs`。补丁专项日志位于 `/tmp/waku-consultation-display-{core-green,protocol-green,ts-green,bindings-check}.log`。

## 效率对照口径

同一版本、同一 fixture 分别执行事件与集中结果策略、重复查询与重复检查策略。计数来源为实际 MCP 调用记录和可控 provider 的原生 RPC/检查记录。两组均使用 steer；检查要求两组取消次数相同，不声称取消次数降低。

对照记录无变化查询、重复结果读取、可避免的重复验证；必要独立评审复核和两项组合检查单独保留。测试驱动等待 fixture 屏障的观测不计作管家查询。原始 `coordination_metrics` 输出已归档于 `logs/combined-green.log`。

| 实际计数 | 事件与集中结果策略 | 重复查询与检查策略 |
| --- | ---: | ---: |
| 无变化查询 | 0 | 1 |
| 重复结果读取 | 0 | 1 |
| 可避免重复验证 | 0 | 1 |
| 反馈取消 / 原生 steer | 0 / 1 | 0 / 1 |
| 子任务检查 / 必要独立复核 | 4 / 1 | 5 / 1 |
| 组合检查 / 完成回调 | 2 / 1 | 2 / 1 |
| 用户讨论 / 排队指令提交 | 1 / 1 | 1 / 1 |

两组均观察到排队指令 `queued → received`。用户讨论与明确执行单独计数。

该对照没有运行旧版二进制，没有测量真实模型费用，也没有证明生产时延改善。fixture 用时仅描述单次测试，不将并行会话用时相加。可重复运行：

```sh
cargo test -p waku-core mcp_coordination_batches -- --nocapture
```

## 日用环境保护

安装前开发验收快照 `history-protection-final.json` 按本轮开始时的历史基线核对：

| 安装版 | 基线消息 | 当前消息 | 基线消息缺失或修改 | 基线会话缺失 | 安装文件哈希 |
| --- | ---: | ---: | --- | ---: | --- |
| Waku | 784 | 860 | 均为 0 | 0 | 保持不变 |
| Waku Steward | 146 | 146 | 均为 0 | 0 | 保持不变 |

Waku 当前消息数增加不影响“原有 784 条均保留且未修改”的核对结论；本记录不据此推断新增消息来源。该快照时未替换日用 `Waku Steward.app` 或原版 `Waku.app`。Debug 签名构建输出见 `dev-watcher-final.log`；`codesign --verify --deep --strict` 通过。实现临时 worktree 和分支已清理；最终验收 fixture 均已关闭，Debug provider 设置已恢复。首次辅助脚本实验遗留的干净工作树已安全清理，原提交通过 `refs/acceptance/retained/<ownid>` 保留；该失败实验不计入完整流程成功证据。

## 签名 Debug App 原生验收

2026-09-07 主会话通过后台 CUA 在真实签名 Debug App 完成操作复核。此前 `noWindowsAvailable` 及键盘无响应为历史阻塞，已由本次实际交互结果消除。

- 真实 Claude 返回只读长答复；`ui-before-discussion.json` 与 `ui-after-discussion.json` 的父子快照完全一致，消息、轮次和等待保持原状，未新增输入投递。长答复可滚动查看。
- `8f192e3` 修复显式焦点句柄及对话框 Tab 循环；原生 Tab、Shift+Tab 循环和 Enter 激活通过。`c431614` 的短指令气泡显示原文，原生界面可见 `Received` 回执，provider 与审计仍保留完整上下文。
- 最终任务 `e7fc2f0f-b46e-4324-8bf3-6ccab53e7da0` 完成两项依赖子任务的固定提交验收、整合和交付，交付提交为 `3fabaa2877b89010c7cc628c24dfa515316d83a9`。`ui-delivery-evidence.json` 记录 `accepted.txt` 精确为 `DIRECTION_UI_ACCEPTED\n`，`dependent.txt` 精确为 `Dependent acceptance passed\n`，父任务最终为 `idle`，历史保存游标追平运行游标。
- 成果 popover 经原生 Home/End 展示依赖提交、验收证据及全部 `Removed` 清理状态；清理后消息与成果历史仍可查看。该成功任务独立于首次辅助脚本失败实验。

实施与验收完成；Issue 关闭、Steward 安装及安装后历史保护核验的执行结果待补记。
