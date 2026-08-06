# 00 架构与边界

## 0.1 定位

OPCOS = **本地 agent runtime + 远程执行目标 + 可选云端控制面**。

**Devin 是主参照**：产品完整度对标 Devin，OPCOS 要做到同等能力。Tembo / OpenWork / OpenWorker / Cloud-Dev 是辅参照，提供本地形态的实现路径。五者是互不相同的独立项目，见 [README](README.md)。

各系统选了不同的分割点：

| 系统      | agent 循环在哪                                             | 执行在哪                        | 云端职责                                                |
| --------- | ---------------------------------------------------------- | ------------------------------- | ------------------------------------------------------- |
| Devin     | 云端（Outposts 也是「循环在云、执行在你机器」）［Devin文］ | 云 VM 或你的 Outpost            | 全部产品                                                |
| Tembo     | 云沙箱                                                     | 云沙箱 / self-hosted［Tembo文］ | 全部产品                                                |
| OpenWork  | 本地桌面（OpenCode）［OW文］                               | 本机 workspace 或 hosted worker | 身份、RBAC、分发、托管 worker、托管模型［OW文］［OW界］ |
| **OPCOS** | **本地 Rust 内核**                                         | **本机 host / 远程 RVM host**   | **可选，且可完全关闭**                                  |

OPCOS 的能力目标是 Devin，边界取 OpenWork：**本地是完整产品，云端只做本地做不到的事**。这带来三个结构性优势——隐私不出本机、零平台抽成、交互零往返延迟——都是相对云平台的真实差异点，不要在演进中丢掉。

## 0.2 分层

```
web/                React UI。只通过 Tauri invoke + event 与内核通信，不含业务逻辑
src-tauri/          桌面 adapter：命令注册、窗口、事件转发。不是 agent runtime
crates/
  opcos-engine      agent 循环 · turn · 审批 · steering · 压缩 · 编排
  opcos-provider    模型适配：OpenAI 兼容 / Anthropic / Bedrock / Vertex / 本地
  opcos-policy      权限模式 · 审批判定 · 审计写入
  opcos-rvm         远程 host 客户端（dev-agent 线协议）
  opcos-store       SQLite 持久化
  opcos-assets      资产（Rules / 知识 / 运行手册 / 技能 / MCP 配置 / 蓝图）
  opcos-mcp         MCP client
```

### 硬约束（来自 `AGENTS.md`，不可违反）

1. RVM host 侧代码不改；OPCOS 只做 client。
2. RVM token 只出现在 `Authorization: Bearer` header，不进 URL、日志、错误、transcript、fixture、UI、截图。
3. 远程 host 不可用时**显式报错**，绝不静默 fallback 到本地执行。
4. 远程路径用远程路径代数与 containment 检查，不用本地 `Path::canonicalize`。
5. `opcos-rvm` 不依赖 `opcos-engine`；`opcos-engine` 不依赖 Tauri/前端；跨层用 trait。

## 0.3 计划中的新增层［推断］

| crate              | 职责                                                                           | 参照                                                            |
| ------------------ | ------------------------------------------------------------------------------ | --------------------------------------------------------------- |
| `opcos-harness`    | agent harness 可插拔：内置 / Claude Code / Codex / OpenCode 子进程，统一事件流 | Tembo 的 6 种 harness［Tembo文］                                |
| `opcos-hosts`      | host 抽象：`LocalHost` / `RvmHost` / `CloudWorker`，同一套能力探测与生命周期   | Den `POST /v1/workers` 的 `destination: local \| cloud`［OW文］ |
| `opcos-context`    | 仓库索引 + 语义检索 + 知识触发注入 + 技能按需装载                              | Devin DeepWiki［Devin文］                                       |
| `opcos-automation` | 定时 + 事件 + webhook 触发；生命周期 hooks                                     | Tembo triggers / `.tembo.json` hooks［Tembo文］                 |

## 0.4 三条设计原则

1. **不做假入口**：没有后端支撑的能力不出现在 UI。列表页只展示条目，创建表单点开后单独出现。
2. **执行位置是一等公民**：本机 / 远程 RVM / 云 worker 走同一套 host 抽象和同一套能力探测，UI 上是同一个选择器。
3. **一切可配置的东西都是「配置对象」**：Rules、知识、运行手册、技能、MCP 连接统一为版本化对象，见 [06](06-capability-model.md)。这是 Den 的 config-object 模型［OW文］，也是让 OPCOS 未来能做分发的前提。

## 0.5 反面清单

不做（因为与定位冲突或已被证明是坏路）：

- 不引入 Python HTTP sidecar（OpenWorker 走的是这条路，OPCOS 明确不走）。
- 不做 ACU/额度这类计量抽成概念。
- 不把 agent 循环搬到云端——那样 OPCOS 就变成又一个 Devin。
- 不自造 UI 布局：参照 OpenWork 与 Cloud-Dev 的既有实现。
