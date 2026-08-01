# OPCOS 开发文档

OPCOS 是本地优先的 one-person-company 工作台：Rust 内核 + Tauri v2 外壳 + React 前端，执行发生在本机或远程 RVM 主机上。

这套文档是**逆向五个参照系统**（Devin、Tembo、OpenWork/Den、OpenWorker、Cloud-Dev）的 Docs / API / MCP / 源码后，为 OPCOS 定的实现依据。写代码前先读对应章节；实现与文档不一致时，改文档或改代码，不要两边并存。

## 目录

| 文档                                             | 内容                                                      |
| ------------------------------------------------ | --------------------------------------------------------- |
| [00-architecture.md](00-architecture.md)         | 分层、边界、不可动摇的约束                                |
| [01-data-model.md](01-data-model.md)             | SQLite 数据模型（现状 + 目标态）                          |
| [02-ipc-contract.md](02-ipc-contract.md)         | Tauri command 与事件流契约                                |
| [03-lifecycle.md](03-lifecycle.md)               | 会话 / turn / 工具 / 审批 / 产物 的状态机                 |
| [04-host-protocol.md](04-host-protocol.md)       | 统一 host 抽象与 dev-agent 线协议                         |
| [05-mcp.md](05-mcp.md)                           | MCP client 与 OPCOS 作为 MCP server                       |
| [06-capability-model.md](06-capability-model.md) | Rules / 知识 / 运行手册 / 技能 / MCP → 统一配置对象与插件 |
| [07-automation.md](07-automation.md)             | 定时、事件、webhook 触发与生命周期 hooks                  |
| [08-security.md](08-security.md)                 | 密钥、token、策略、审计                                   |
| [09-cloud.md](09-cloud.md)                       | OPCOS Cloud 分层与形态演进                                |
| [10-reference-matrix.md](10-reference-matrix.md) | 五方对照与事实来源                                        |

## 事实来源标记

文档里的每条外部事实都带来源标记，不允许无标记的断言：

- **［Devin文］**`docs.devin.ai` 官方文档
- **［Tembo文］**`docs.tembo.io` 官方文档
- **［OW文］**`openworklabs.com/docs` 官方文档
- **［OW界］**已登录 `app.openworklabs.com` 的实际页面
- **［CD码］**`Cloud-Dev` 源码
- **［OWK码］**`openworker` 源码
- **［推断］**由上述事实推出的设计判断，不是外部事实

原始调研资料（抓取副本与端点全表）不在本仓库，位于调研目录：
`OPCOS-深度调研与本地化方案.md`、`openwork-api-summary.md`、`devin-api-summary.md`、`tembo-api-summary.md`、`clouddev-reverse.md`、`openworker-reverse.md`。
