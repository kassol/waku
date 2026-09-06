# 管家一期验收记录

日期：2026-09-06。目标仓库 `kassol/waku`。一期实现、六工具真实闭环及原生交互验收完成。

## 隔离边界

使用唯一 watcher 构建签名 `Waku Debug.app`，标识 `sh.waku.dev`，数据与 worktree 位于本 checkout 的 `temp/`。验证只连接 Debug 自有的回环 daemon。

原版 `/Applications/Waku.app` 和 daemon 的 PID 24440/24444 保持运行；安装二进制、Info.plist、`~/.waku/settings.json` 的 SHA-256 与开工基线一致。共用原生 Claude/Codex 配置遵循 ADR-0004；验证没有修改配置或调用 OpenRouter。

## 已完成的行为证据

| 范围 | 验证 |
| --- | --- |
| 自主持久化与退出恢复 | 真实 Astra 会话结束、保存、正常退出及重开后，完整问答、父子关系和保存游标一致。故障保存与未确认退出由临时数据库和自有进程回归覆盖。 |
| MCP 创建与查询 | 真实 Claude（现有 fable）通过 MCP 创建 Astra 子会话，列表、状态、结果的实际工具输出与保存历史一致。 |
| 递归 | Astra → Claude → Astra 三级会话均完成，孙级 worktree 的指定文件正确；原生搜索中用方向键与 Enter 打开 Claude 子会话阅读结果。直属范围及审批权限由真实 stdio、临时 SQLite/Git 和可控 provider 回归覆盖。 |
| 幂等与目录 | 真实 Astra 使用 inherit 目录；同键同请求重复返回原 child/turn_id，首轮只执行一次。Debug daemon 实际重启后再次请求仍返回原结果。worktree/local、阶段失败和恢复不重发有自动回归。 |
| 树导航 | 后台原生 Shift+Tab 聚焦、Up/Down 导航、Left 折叠、Right 展开和 Enter 打开子会话通过；焦点有可见边框。 |
| 审批与回答 | 可控 Claude 进程的真实控制请求在原生 App 显示；Shift+Tab 聚焦允许、Enter 批准，随后 Shift+Tab 聚焦 Yes、Space 单次选择（含多选问题）、Tab 聚焦 Submit、Enter 提交，两轮均完成。禁用 Submit 跳过，焦点可见。 |
| 后续输入与取消 | 真实 Claude 通过 MCP 向 Astra 子会话提交后续输入，精确回复标记。下一轮实际执行 `sleep 60`；一次取消先返回 accepted=true/stopped=false，18 秒时确认 Interrupted，状态/结果返回同一轮，历史及原文件保留；签名 App 显示停止耗时，单次 Enter 展开、Space 折叠，保留预告和命令记录。 |

## 长历史与并发流

在同一签名 Debug 中创建 3 个独立可控 provider 会话，每个预存 1000 条历史，再以每秒 25 个增量输出 3000 个增量。原生窗口显示新输出，输出期间可滚动读取旧历史。结束后逐个读取 daemon：每个会话 1002 条消息、轮次完成、全部 3000 个编号增量存在、保存游标为 3005。三个自有 runtime 均关闭成功。

沿用 `docs/performance.md` 的 CPU 与 `sample` 方法。30 秒采样剔除 `sample` 运行及跨边界间隔，按累计 CPU 时间计算：App 平均 15.9%，daemon 平均 5.6%。主线程 410 次样本中，275 次等待 Metal drawable、110 次等待事件，其余主要为绘制/prepaint。App 观察到的 RSS 峰值约 138 MiB，daemon 约 76 MiB。

这些数值来自后台测试窗口和 Debug 构建，不是 release 或 120 Hz 帧率承诺。没有启用会主动触发重绘的 FPS 计数器；采样未覆盖所有可能的阻塞路径。减少动态效果采用 GPUI App 级回归，不修改用户的系统设置。

## 可复现检查

- `cargo test --workspace -j 2 -- --test-threads=2`
- `bun run --cwd packages/waku-client check`
- `bun run --cwd packages/waku-client test`
- `bun run protocol:check`
- `TOOLCHAINS=com.apple.dt.toolchain.Metal.32023.883 bun ./scripts/dev.ts`：仅在 watcher 确认不可用时恢复。

Rust 1.98 的全仓格式检查已有历史差异，本次不重排无关代码。完整 workspace 测试 919 项通过、26 项既有外部环境测试忽略；TypeScript 客户端 39 项、类型及生成协议检查通过。最终增量运行完整桌面测试 382 项通过，覆盖焦点动态启用、回答卡片顺序、禁用环境操作、中断提示、空输出中断的文件摘要及包含按键松开的单次激活；减少动态效果 GPUI 回归通过。逐票 Standards/Spec 复核发现的权限、保存、取消与键盘问题均纳入回归。

## 最终现场

四个开发 worktree 与临时分支、独立编译目录及八个模拟会话和六个临时项目已清理。真实 Claude/Codex 测试会话、独立 worktree 和已产出的文件保留。测试 provider 均已关闭；最终已由 daemon 确认保存，再停止本轮持有的 Debug watcher 与其自有进程，退出均已确认。原版文件摘要与进程在收尾核对仍与基线一致；证据保存在 checkout 的 `temp/steward-acceptance-evidence/`。
