# 管家协作任务清单

父规格：[Issue #13](https://github.com/kassol/waku/issues/13)。2026-09-06 用户确认以下 9 张纵向任务及依赖，发布时均为 ready-for-agent。父规格未修改。2026-09-07，#14–#22 实施及验收全部完成，包含公共组合流程和签名 Debug App 原生交互；当前均待关闭，详见[验收记录](waku-steward-orchestration-acceptance.md)。

每票具有自身可验证的完整行为，包含相关公共接口、持久化、必要界面和回归。依赖采用 GitHub 原生阻塞关系，正文同时保留链接。无独立预重构票；复用已有设施，按实际需要在所属票内调整。

| 编号 | 任务 | 阻塞任务 |
| --- | --- | --- |
| T01 | [#14 可靠原生 steer 与输入投递状态](https://github.com/kassol/waku/issues/14) | 无 |
| T02 | [#15 不支持 steer 时持久排队并自动续办](https://github.com/kassol/waku/issues/15) | [#14](https://github.com/kassol/waku/issues/14) |
| T03 | [#16 执行期间的独立只读即时交流](https://github.com/kassol/waku/issues/16) | 无 |
| T04 | [#17 即时交流指令投递与任务回调协调](https://github.com/kassol/waku/issues/17) | [#15](https://github.com/kassol/waku/issues/15), [#16](https://github.com/kassol/waku/issues/16) |
| T05 | [#18 任务集成分支与工作区归属](https://github.com/kassol/waku/issues/18) | 无 |
| T06 | [#19 固定成果验收整合与依赖放行](https://github.com/kassol/waku/issues/19) | [#18](https://github.com/kassol/waku/issues/18) |
| T07 | [#20 交付后安全清理与历史追溯](https://github.com/kassol/waku/issues/20) | [#19](https://github.com/kassol/waku/issues/19) |
| T08 | [#21 动态分工与集中反馈交接](https://github.com/kassol/waku/issues/21) | [#15](https://github.com/kassol/waku/issues/15), [#19](https://github.com/kassol/waku/issues/19) |
| T09 | [#22 管家协作完整流程与效率验收](https://github.com/kassol/waku/issues/22) | [#17](https://github.com/kassol/waku/issues/17), [#20](https://github.com/kassol/waku/issues/20), [#21](https://github.com/kassol/waku/issues/21) |

发布时初始可执行任务为 #14、#16、#18。实施采用隔离工作树协调共享源码修改；产品的受管理 worktree 能力在 #18–#20 中实现，实施顺序遵循阻塞关系。

## 规格覆盖

| 任务 | 用户故事 |
| --- | --- |
| #14 | 9、11、13、14、15、16、5、6 |
| #15 | 10、12 |
| #16 | 1、2、3、18 |
| #17 | 4、5、6、7、8、19 |
| #18 | 23、28、29、30、31 |
| #19 | 21、24、27、30、32、33、34、35 |
| #20 | 36、37、38、39 |
| #21 | 17、20、21、22、25、26、27 |
| #22 | 40 |

## 验收与边界

- 每票使用临时资源及现有 MCP／daemon 公共边界，完成相关类型、协议和 Standards / Spec 检查。实现 subagents 使用 Astra。
- 输入投递覆盖空闲及运行中状态；不确定提交不能自动重发。改方向覆盖相关任务分流、必要暂停与实际停止确认。
- 成果整合包含按已有授权交付回目标分支，清理前确认成果所在持久引用、目录干净与无人使用。
- 即时交流、任务状态及必要原生控件的隔离交互验收属于各票；末票负责完整组合流程和效率对照，不承担前置票遗留的基本验收。
- 已核对 40 条用户故事覆盖、9 票正文和标签、10 条原生阻塞关系及父规格保持不变。最终 Rust 全量 985 项通过、29 项忽略、0 失败，共享客户端 46 项及三端类型检查通过；原生 Codex、Claude tracked input 与真实界面流程通过。
- 用户已明确授权验收后推送、关闭任务并更新安装 Waku Steward。当前任务待关闭、Steward 待安装；原版 Waku 隔离保护继续有效。
