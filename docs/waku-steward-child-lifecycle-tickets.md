# 子会话生命周期实施任务

父规格：[Issue #23](https://github.com/kassol/waku/issues/23)。2026-09-07 用户确认以下 7 张纵向任务与依赖。每票包含自身公共接口、持久化、必要界面和回归；最后一票负责组合验收。

| 任务 | 交付 | 阻塞任务 |
| --- | --- | --- |
| [#24 主会话集中展示分工与进度](https://github.com/kassol/waku/issues/24) | 用户通过主会话查看执行分工、进度与必要详情，日常列表保持以主会话为入口。 | 无 |
| [#25 子会话请求管家决策并等待恢复](https://github.com/kassol/waku/issues/25) | 直属子会话提出普通澄清或方案请求，等待管家在已有授权内作出决定，接收后继续执行。 | 无 |
| [#26 超出授权的问题在主会话集中处理](https://github.com/kassol/waku/issues/26) | 管家将需要用户决定的事项集中转到主会话，用户答复沿原请求送回受阻子会话。 | [#25](https://github.com/kassol/waku/issues/25) |
| [#27 原生审批与提问接入管家决策](https://github.com/kassol/waku/issues/27) | Claude 与 Codex 原生审批或提问经真实接口交给管家处理，授权内代决，超出授权回主会话询问。 | [#26](https://github.com/kassol/waku/issues/26) |
| [#28 成果接管摘要保存与子会话归档](https://github.com/kassol/waku/issues/28) | 管家接管子任务成果或终止结果后，在主会话保留摘要与追溯入口，自动归档完成生命周期的子会话。 | [#27](https://github.com/kassol/waku/issues/27) |
| [#29 归档子任务的安全返工与继续](https://github.com/kassol/waku/issues/29) | 用户明确要求继续旧子任务时，管家重新核验执行条件，保留旧记录并安全恢复或重新委派。 | [#28](https://github.com/kassol/waku/issues/28) |
| [#30 分层协作完整流程与恢复验收](https://github.com/kassol/waku/issues/30) | 在隔离环境走通主会话统一交互、子会话双向决策、人工转问、成果接管、归档追溯及返工。 | [#24](https://github.com/kassol/waku/issues/24), [#29](https://github.com/kassol/waku/issues/29) |

## 执行与验收

发布时 #24、#25 为首批可执行任务；实现按阻塞关系推进，现已完成 #24–#30，提交与验收结果见[生命周期验收记录](waku-steward-child-lifecycle-acceptance.md)。不存在必须预先进行的独立重构，局部调整随所属行为票完成。实现 subagents 使用 Astra，共享源码修改按文件归属或隔离工作树协调。

每票自行完成公共接口回归与相关原生交互、协议和类型检查、Standards / Spec 复核。主要自动化边界继续使用真实 MCP/daemon socket、临时 Git/SQLite 与可控 provider，补充两种原生 harness 兼容性验证及独立签名 Debug 后台验收。日用 Steward、原版 Waku 及既有历史保持隔离。

## 规格覆盖

| 用户故事 | 主要任务 |
| --- | --- |
| 1–3、39：主会话分工、详情和响应性 | #24 |
| 4–12、16–19、22：双向请求、授权内决定及等待恢复 | #25 |
| 13–15、20–21、23：主会话转问、集中处理及嵌套转交 | #26 |
| 12–19：原生审批/提问的权限与真实响应 | #27 |
| 26–31、33–37：接管、摘要、归档与独立清理 | #28 |
| 32：安全返工与继续 | #29 |
| 24–25、38、40：恢复、退出、键盘及隔离验收 | 各票自身回归及 #30 组合验收 |

发布阶段保留父规格开放状态，子票标记 ready-for-agent，并通过原生子 Issue 与 blocking 关系关联。实施验收已完成；用户随后授权日用安装更新至 `ccc7c24`，见[安装记录](waku-steward-child-lifecycle-acceptance.md)。未自动整理历史子会话或删除既有数据。
