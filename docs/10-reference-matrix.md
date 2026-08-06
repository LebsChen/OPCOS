# 10 五方参考矩阵

标记含义见 [README.md](README.md)。本表只记录已抓取文档或源码可核实事实；“未确认”表示当前资料没有足够证据。

## 10.1 能力矩阵

| 能力域              | Devin                                      | Tembo                                   | OpenWork + Den                                      | OpenWorker                                        | Cloud-Dev                                            | OPCOS 现状/差距                                                         |
| ------------------- | ------------------------------------------ | --------------------------------------- | --------------------------------------------------- | ------------------------------------------------- | ---------------------------------------------------- | ----------------------------------------------------------------------- |
| Agent loop          | 云端 session loop［Devin文］               | 云 sandbox agent［Tembo文］             | 桌面 OpenCode 本地 loop；Den 控制面［OW文］         | `TurnEngine` model↔tool loop［OWK码］             | agent/desktop shell 分层［CD码］                     | Rust `opcos-engine` 已有 turn/审批；需继续统一事件和恢复。              |
| Execution target    | VM/Outpost，Outpost claim queue［Devin文］ | sandbox/self-hosted［Tembo文］          | worker `destination: local \| cloud`［OW文］        | 本地 workspace/roots［OWK码］                     | RVM dev-agent、PTY/VNC/CDP［CD码］                   | RVM client 已有；LocalHost/CloudWorker 未形成统一 trait［推断］。       |
| Host health         | Outpost phase/session status［Devin文］    | sandbox 状态［Tembo文］                 | worker `activity-heartbeat`［OW文］                 | server/session manager［OWK码］                   | `/api/health` 与 `/api/info`［CD码］                 | `test_host`/health 已有；需 capability 与主动 heartbeat 模型。          |
| Exec                | session tools［Devin文］                   | sandbox commands［Tembo文］             | worker execution［OW文］                            | tool executor + approval［OWK码］                 | `/api/exec`, `/api/exec-sync`［CD码］                | exec-sync 已有；需统一长任务句柄和错误类型。                            |
| File/path           | workspace/context［Devin文］               | sandbox workspace［Tembo文］            | worker workspace［OW文］                            | `RootDir` canonical roots［OWK码］                | `/api/read/write/ls`［CD码］                         | read/write 与 remote path guard 已有；需 artifact 引用表。              |
| PTY/Desktop/Browser | Outpost execution surface［Devin文］       | sandbox surface，细节未确认             | hosted worker 连接［OW文］                          | browser endpoints［OWK码］                        | `/pty-ws`, `/vnc-ws`, `/cdp-ws`［CD码］              | Tauri surface relay 已有；协议 capability 仍需统一。                    |
| Web IDE             | Devin environment surface［Devin文］       | 未在抓取文档中确认                      | OpenCode/worker 连接［OW文］                        | 未确认独立 IDE bridge                             | `/ide/*` token gate［CD码］                          | IDE proxy 已有；需与 host trait/安全文档统一。                          |
| Model/provider      | Devin provider/model API［Devin文］        | harness/model 配置［Tembo文］           | managed/custom LLM provider［OW文］                 | ProviderClient + matrix + Bedrock/Vertex［OWK码］ | agent MCP 不负责模型［CD码］                         | OpenAI/Anthropic/Bedrock/Vertex 已接线；需 capability matrix 对齐。     |
| MCP client          | Devin hosted MCP［Devin文］                | stdio/http/sse［Tembo文］               | Den OAuth MCP［OW文］                               | global/workspace config + stdio/http［OWK码］     | agent `/mcp` server［CD码］                          | `opcos-mcp` client 已有基础；需 config layering、OAuth、tool approval。 |
| MCP server          | 13 platform tools［Devin文］               | tool 清单未确认                         | Den managed MCP［OW文］                             | server endpoints 未确认                           | 23 agent tools［CD码］                               | OPCOS server 未实现，需受限 session/host tool catalog［推断］。         |
| Config objects      | Knowledge/Playbook/Blueprint［Devin文］    | Skills/rules/hooks［Tembo文］           | config object/version/plugin［OW文］                | Persona/MCP/connector config［OWK码］             | capability endpoint/agent modules［CD码］            | `asset_records` 仍非版本对象；需按 [06] 迁移。                          |
| Plugin/skills       | Playbooks/Knowledge/Blueprint［Devin文］   | 6 harness + `SKILL.md`［Tembo文］       | plugins 包 skills/hooks/MCP/agents/commands［OW界］ | Persona skills/MCP recommendations［OWK码］       | MCP tools/modules［CD码］                            | asset CRUD 已有；plugin/version/member 未有。                           |
| Session lifecycle   | v1/v3 status + status_detail［Devin文］    | agent/session 生命周期［Tembo文］       | session/worker cloud control［OW文］                | WS session + durable `.jsonl`［OWK码］            | worklog/PTY session［CD码］                          | 目标二维状态见 [03]；需统一 store 权威。                                |
| Approval/policy     | session approval/tool states［Devin文］    | sandbox policy 未确认                   | desktop policies/RBAC［OW文］                       | standing rule、Inbox、permission events［OWK码］  | agent auth，不是 engine approval［CD码］             | approval/pending/audit 已有；Inbox/unattended 仍缺。                    |
| Audit/usage         | Usage/ACU/Audit［Devin文］                 | audit 细节未确认                        | audit/RBAC［OW文］                                  | SQLite AuditStore［OWK码］                        | worklog，token gate logs 受约束［CD码］              | `audit_events` 已接通；不引入 ACU 计量。                                |
| Automation          | schedules/events/integrations［Devin文］   | schedule/event/webhook/macro［Tembo文］ | cloud automation/control plane［OW文］              | automations API［OWK码］                          | events subscribe handler，真实 broker 未确认［CD码］ | `schedules`/run 已有；统一 automation 目标表。                          |
| Connectors          | Slack/integrations［Devin文］              | 连接能力未确认                          | managed/user connectors［OW文］                     | gateway/OAuth/SecretStore［OWK码］                | 无完整 connector catalog                             | OPCOS 移除无后端假入口；connector framework 未完成。                    |
| Durable artifacts   | session artifacts［Devin文］               | snapshots/artifacts［Tembo文］          | worker/workspace resources［OW文］                  | artifacts/read/reveal［OWK码］                    | storage/download［CD码］                             | diff/worklog 已有；artifact 引用模型缺失。                              |
| Cloud boundary      | loop 与执行均由 Devin 管理［Devin文］      | self-host 全组件［Tembo文］             | Den 控制面，桌面本地优先［OW文］                    | Python local server［OWK码］                      | embedded/standalone RVM parity［CD码］               | OPCOS 维持 local-first；云端可关闭［推断］。                            |
| Security            | Bearer/API key/RBAC［Devin文］             | Bearer API key/RBAC［Tembo文］          | session/API key/OAuth/SSO［OW文］                   | WebSocket subprotocol + SecretStore［OWK码］      | unified token gate［CD码］                           | RVM token header-only；远程失败显式报错。                               |

## 10.2 OPCOS 差距优先级

### P0：已有基础，需收口

1. 合并 desktop DB 的重复 `sessions`；desktop `transcript` 与 store 的 `messages`/`notices`/`tool_calls` 是职责重叠，不是桌面 `tool_calls` 表。
2. IPC command/event payload 统一，确保审批延续和 `turn_done` 顺序符合 [03](03-lifecycle.md)。
3. host capability、health、remote path algebra 统一到 trait。
4. MCP client 的 global/workspace 配置、tool discovery、approval、SecretStore。
5. artifact 引用、automation 统一表和迁移。

### P1：可直接借鉴但不引入云计量

- OpenWorker 的 Inbox/unattended/standing rule［OWK码］。
- Cloud-Dev 的 PTY/VNC/CDP/IDE relay［CD码］。
- Den 的 worker destination 和 activity-heartbeat［OW文］。
- Devin 的二维 session status 与 Outpost claim lease［Devin文］。
- Tembo 的 harness、Skills 目录和 hooks［Tembo文］。

### 明确不做

- Devin 的 ACU、quota、org billing 不进入 OPCOS 本地模型［Devin文］［推断］。
- 不把 Python sidecar 或 OpenWorker REST server 引入 OPCOS；分层约束见 [00](00-architecture.md)。
- 不做没有后端的 Slack/Jira/Sentry 等假入口；Linear 已有本地 GraphQL + SecretStore 真连接器。
- 不提供未绑定 host/session 的任意 shell 或 secret 读取 MCP tool［推断］。

## 10.3 调研产物与规模

| 产物                                            | 规模                                                                                           |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| `/home/ubuntu/research/openwork-api-summary.md` | OpenWork API 抓取 269 页；摘要记录约 202 个端点操作［OW文］。                                  |
| `/home/ubuntu/research/devin-api-summary.md`    | Devin 抓取 552 页；解析 269 个 API 操作［Devin文］。                                           |
| `/home/ubuntu/research/tembo-api-summary.md`    | Tembo 抓取 70 页；解析 12 个 REST 操作［Tembo文］。                                            |
| `/home/ubuntu/research/clouddev-reverse.md`     | Cloud-Dev 本地源码逆向；RVM、MCP、Tauri、SQLite、编排和安全［CD码］。                          |
| `/home/ubuntu/research/openworker-reverse.md`   | OpenWorker 本地源码逆向；FastAPI、TurnEngine、Provider、持久化、Connector、MCP、GUI［OWK码］。 |
| `/home/ubuntu/research/openwork-api/`           | OpenWork API 页面抓取副本；约 269 个页面［OW文］。                                             |
| `/home/ubuntu/research/devin-docs/`             | Devin `.md` 文档抓取副本；552 个页面［Devin文］。                                              |
| `/home/ubuntu/research/tembo-docs/`             | Tembo `.md` 文档抓取副本；70 个页面［Tembo文］。                                               |

端点/操作数量来自抓取脚本和摘要索引；不同文档可能对同一资源重复描述，不能将条目数直接视作唯一 URL 数［推断］。

## 10.4 设计取舍记录

### 保留本地优先

OpenWork 的桌面形态把 OpenCode 放在本地，Den 负责控制面；Cloud-Dev 则把 Tauri shell、Node RVM agent 和远端能力组合起来［OW文］［CD码］。OPCOS 保留 Rust engine 在本地，host 只承担明确绑定的执行；这不是把 Devin 的云端 loop 搬到本地的临时方案，而是产品边界［推断］。

### 保留真实 host

Devin Outposts 的原子 claim、租约和状态 watch 适合未来 fleet；Den 的 worker destination 与 heartbeat 适合统一 local/cloud worker API［Devin文］［OW文］。OPCOS 当前先实现单机/远程 RVM host，并把 claim/lease 留在可选 cloud 层，避免本地 schema 引入组织、ACU 和云账户［推断］。

### 保留真实安全

Cloud-Dev Web IDE 有统一 token gate，OpenWorker MCP OAuth token 进入 SecretStore，OpenWork 文档也区分用户 session 与组织 API key［CD码］［OWK码］［OW文］。OPCOS 的更严格约束是 RVM token 只能进 Authorization header，因而不复制 Cloud-Dev IDE URL 的 query token 便利路径［推断］。

### 保留可审计工具循环

OpenWorker 的 `TOOL_PROPOSED`、`PERMISSION_REQUIRED`、`TOOL_STARTED`、`TOOL_FINISHED`、`COMPACTED` 事件和 AuditStore 提供完整工具生命周期［OWK码］。OPCOS 已有 pending、approval、audit_events 和 turn event 基础，下一步是使 command、event、store 三者共享 call id/sequence［推断］。

### 保留配置对象而非假入口

Den plugin/config-object 版本与授权、Tembo skills/rules/harness 约定、Devin Knowledge/Playbook/Blueprint 都说明配置需要可组合、可版本化、可按触发加载［OW文］［Tembo文］［Devin文］。OPCOS 先做本地 config object/version，再做 plugin/member 和云端分发；没有后端的 connector 不加入 UI［推断］。

## 10.5 未确认项清单

- OPCOS `LocalHost` 的真实执行 worker、崩溃恢复和端口生命周期：未实现，未确认。
- OPCOS `CloudWorker` 的实际 API、token lifecycle 和 heartbeat endpoint：未实现，未确认。
- OPCOS 作为 MCP server 的 listener、认证 handshake、完整 tool catalog：未实现，未确认。
- OpenWork/Den 托管 MCP 的完整 tool 列表和所有 OAuth callback 字段：公开抓取资料未形成可核实的固定列表，未确认［OW文］。
- Tembo REST OpenAPI：公开探测返回异常/未授权，不能据此补充未抓到的 operation，未确认［Tembo文］。
- Cloud-Dev `/api/events/subscribe` 是否连接真实 broker：当前 handler 只返回 registered，未确认［CD码］。
- Cloud-Dev agent 子模块 storage/git/repo/deploy 每个 operation 的完整参数：逆向摘要只列已确认分派和核心字段，其余未确认［CD码］。
- OpenWorker 动态 manager response 的所有字段：未在每个方法逐字段展开，未确认［OWK码］。

这些项目必须在实现前通过对应源码、协议 fixture 或集成测试补证，不得用推测替代。

## 10.6 交叉引用

| 主题              | 规范文档                                         | 本篇作用                                                        |
| ----------------- | ------------------------------------------------ | --------------------------------------------------------------- |
| 分层与本地优先    | [00-architecture.md](00-architecture.md)         | 约束 agent runtime、desktop adapter、host 和可选 cloud 的边界。 |
| session/turn 状态 | [03-lifecycle.md](03-lifecycle.md)               | 约束 run state、stop reason、approval 和事件顺序。              |
| 能力对象          | [06-capability-model.md](06-capability-model.md) | 约束 config object、version、plugin、skill、MCP 和 blueprint。  |
| automation        | [07-automation.md](07-automation.md)             | 目标态触发器和执行阶段，本文只列差距。                          |
| 安全              | [08-security.md](08-security.md)                 | token、path、secret、审计边界。                                 |
| cloud             | [09-cloud.md](09-cloud.md)                       | 控制面、worker destination、API auth 和 cloud 演进顺序。        |
| store schema      | [01-data-model.md](01-data-model.md)             | 当前 10 张表和目标迁移关系。                                    |
| IPC               | [02-ipc-contract.md](02-ipc-contract.md)         | command 参数、返回、失败和 event channel。                      |
| host protocol     | [04-host-protocol.md](04-host-protocol.md)       | LocalHost/RvmHost/CloudWorker 和 dev-agent route matrix。       |
| MCP               | [05-mcp.md](05-mcp.md)                           | client/server 边界、transport、tool approval。                  |

## 10.7 阅读与实现顺序

新实现建议按以下顺序阅读：

1. 先读 [00](00-architecture.md)，确认代码应位于哪个 crate/adapter。
2. 读 [03](03-lifecycle.md) 和 [02](02-ipc-contract.md)，确认状态和事件。
3. 读 [04](04-host-protocol.md)，确认远程能力和错误语义。
4. 读 [06](06-capability-model.md)、[01](01-data-model.md)，确认持久化对象。
5. 读 [05](05-mcp.md)、[08](08-security.md)，确认第三方 tool 和 secret boundary。
6. 最后读 [09](09-cloud.md)，只在明确需要控制面时引入 cloud 依赖。

此顺序避免先照搬参照系统的 UI/API，再发现违反 OPCOS 的 client-only、header-only 和 no-fallback 约束［推断］。

## 10.8 验收证据要求

每个差距进入实现前，应有可复核证据：

- schema：迁移测试、旧数据导入测试和回滚测试；
- IPC：Rust command signature、前端 invoke、成功/失败 fixture；
- host：health、unauthorized、disconnect、path traversal 测试；
- MCP：tools/list、schema invalid、approval、OAuth secret redaction 测试；
- artifact：远程路径、删除引用、host unavailable 测试；
- automation：disabled、重复触发、失败重试和审计测试；
- cloud：显式 opt-in、API auth、无 cloud 时的离线运行测试。

证据不足时，状态写为“未确认”，不能用 UI 已有按钮、类型声明或命名推断后端能力［推断］。

## 10.9 维护规则

新增参照系统事实时，应更新对应 research 摘要和本表来源标记；新增 OPCOS 目标设计时，应标记［推断］并链接规范文档。禁止把调研摘要中的 token、API key、Cookie 或 secret value 复制到本仓库［推断］。

审阅阶段不提交、不推送，先由维护者确认术语和差距优先级。
