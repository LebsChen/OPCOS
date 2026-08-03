# OPCOS 距「一人公司系统」终极目标的差距评审

> 评审对象：`dev` 分支 + 在途的 #35 / #36
> 终极目标：OPC = 一人公司，OPCOS = 一人公司系统。完全自动化、自主驱动 + 事件驱动的 agent 工作台，通过 Team 模板快速创建软件开发、跨境独立站、闲鱼/淘宝/拼多多/京东电商、自媒体等业务团队。

---

## 0. 一句话结论

**OPCOS 现在是一个相当完整的「本地 Devin」——一个 agent 框架。而「一人公司系统」不是一个更大的 agent 框架，它是一个业务运营系统。两者之间缺的不是更多的 agent，而是三个当前缺失或明显不足的层。**

用一个数字说明距离：`opcos-store` 已经持久化了 session、project、project agent、artifact、message、pending、inbox、compaction、usage、audit、secret 等实体，但这些仍然是 agent/项目执行过程的模型——**没有业务域实体**。没有商品、订单、客户、工单、素材、广告计划、账号。一人公司的日常恰恰全部发生在这些实体上。

按当前设计（agent 框架 + 业务 agent + LLM）继续走，软件开发场景能到 70–80%，但电商和自媒体场景会卡在 20–30% 并且很难再往上——原因见第 2 节。

---

## 1. 现状：已经建成的部分（这部分比我预期的扎实）

| 层                   | 状态   | 证据                                                                                                                                                                 |
| -------------------- | ------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Provider 中立        | 完成   | `opcos-provider` 3157 行，17 个 descriptor，OpenAI/Anthropic/Bedrock 独立 adapter                                                                                    |
| Agent 循环 / harness | 完成   | `opcos-engine` 6134 行；builtin + OpenCode + ACP（#35/#36）三种 harness                                                                                              |
| 执行环境抽象         | 完成   | `opcos-hosts`，Local / RVM，能力按声明协商，远程不可用显式失败                                                                                                       |
| 审批 / 策略 / 审计   | 完成   | `opcos-policy`（PermissionMode / ToolRisk / DurableGrant / redact）+ Inbox + unattended + `append_audit`                                                             |
| 凭据                 | 完成   | SecretStore + keyring，五层作用域解析                                                                                                                                |
| MCP                  | 完成   | `opcos-mcp` 1311 行，stdio/http/streamable/sse + 每工具审批                                                                                                          |
| 连接器               | 较完整 | connector catalog、身份探活和部分工具路径覆盖约 25 个平台/连接器种类（GitHub/Linear/Slack/Notion/Stripe/Gmail/Outlook/IMAP…）；engine 实际暴露的业务工具少于 catalog |
| 配置对象分层         | 完成   | global < project < repo < host < session，项目勾选 + 覆盖 + `.agents/*` 同步                                                                                         |
| 协同 runtime         | 部分   | `[[COORD]]` 信封、星型拓扑校验、熔断、BoardTask 租约/续租/验收                                                                                                       |
| 定时 + 本地触发      | 部分   | `scheduler.rs` 仅 `Every` / `MinuteModulo`；loopback HTTP callback；文件变化 watcher                                                                                 |

**这是一个合格的 agent 框架底座。** 后面所有的差距讨论，都建立在「底座本身不需要推倒重来」这个前提上。

---

## 2. 缺的三层（核心结论）

### 2.1 缺业务对象模型 —— 最大的结构性缺口

现在 OPCOS 的核心状态仍然围绕 session、project、project agent、artifact 等执行/项目模型组织。一个 agent 跑完一个 turn，主要产出的是 message、artifact 和 audit 记录。

但一人公司需要回答的问题是：**这个商品上架了吗？这个订单发货了吗？这个客户的问题回复了吗？这条广告计划这周花了多少、转化如何？**

没有业务状态持久化，会导致三个致命后果：

1. **无法增量**。每次都要让 agent 重新「看一遍」平台才知道现状。100 个商品就是 100 次全量扫描。
2. **无法幂等**。agent 中断重跑，会重复上架、重复回复、重复发货。当前的 durable resume 只能恢复 _会话_，恢复不了 _业务动作是否已生效_。
3. **无法对账**。系统不知道自己做过什么，也就无法自查、无法生成报表、无法「优化」。而用户明确要求了淘宝场景的「报表、优化」。

**这里的缺口不是“每个平台手写一套 schema 和业务逻辑”。** 更合适的是在 `opcos-store` 旁边增加平台无关的通用记录：实体、事件、来源/账号上下文、幂等键和结果状态。业务 agent 通过 MCP、API 或 computer-use 工具读取和写入这些记录；平台差异留在 actuator 和凭据配置里，而不是扩散成闲鱼/淘宝/Shopify 各自一套 Rust 模型。

但 actuator 能写入平台，并不等于 OPCOS 自动拥有事实层。当前仓库没有这组业务无关的实体/事件/幂等记录，也没有工具契约要求 agent 在每次外部动作前后写入它们。因此“agent 通过 MCP 发布了商品”仍可能只留下 transcript/tool call：系统无法查询 `SKU-123` 是否已经上架、无法用幂等键阻止重试，也无法从事件记录生成报表。这个状态层仍然绕不开，只是它应当被实现为所有平台共用的通用基础设施，而不是每平台手写业务能力。

### 2.2 缺自治循环 —— 「自主驱动」目前并不存在

用户要的是「自主驱动 + 事件驱动」。事件驱动有雏形（cron + loopback callback + 文件 watcher），**自主驱动完全没有**。

现在主要执行路径都是 turn-driven：

```
用户 prompt / schedule 到点 / [[COORD]] 消息  ->  queue_steering  ->  跑一个 turn  ->  结束
```

`scheduler.rs` 只支持「每 N 秒」和「分钟取模」，触发后发一个**预先写死的 prompt**。这是 cron，不是自主。没有任何机制让 agent 自己回答「现在该干什么」。

一人公司要的是：系统自己知道「昨天有 3 个待回复的客服工单、2 个订单待发货、广告 ROI 掉了 15% 需要调价、竞品上了新款需要跟进」，然后自己排期执行。这需要一个**规划器 + 持久化任务队列**，而不是更多的 cron 条目。

注意 `BoardTask` 已经有了 claim / renew / lease 的语义，且 `src-tauri/src/main.rs` 已经把 `coord_tasks`、`coord_messages`、任务依赖和认领状态持久化到 SQLite——**这是现成的地基**。但它仍然不是一个面向所有业务事件的通用跨 session 工作队列：缺少统一唤醒、规划、重试/补偿和业务状态驱动。

### 2.3 缺有状态的外部世界交互 —— 电商场景的硬门槛

浏览器现在的形态主要是 **surface relay**：暴露 VNC / CDP 通道（`WsKind::{Pty,Vnc,Cdp}`），`connector_browser_check` 也只是探测 host 是否暴露了 browser 能力；尚未看到更高层的确定性浏览器业务原语。

电商和自媒体需要的是在这些 surface 之上的程序化 computer-use 循环，以及：

- 登录态持久化与多账号隔离（一人公司会同时运营多个店铺/多个平台账号）
- 操作原语的幂等与重试（「发布商品」失败重试不能变成发两次）
- 风控应对（验证码、滑块、设备指纹、频率限制）
- 可重放、可断点续传的采集流水线
- 多账号登录态、profile 和凭据隔离

当前源码只确认了 surface relay 和能力探测：`WsKind::{Pty,Vnc,Cdp}`、`connector_browser_check`，以及 Host capability 中的 `cdp` / `browser` 项；尚未发现可供 agent 稳定调用的浏览器动作、截图/定位、会话 profile 管理或断点重放抽象。也就是说，真正的缺口是**程序化的 computer-use 循环 + 多账号登录态隔离**，不是“不能让 LLM 点击”。

LLM 可以参与 computer use，尤其是在异常分支和页面变化时；但要让它长期运行，仍需要把循环、状态、重试和账号边界做成框架能力，而不是把每次点击都当成一次无边界的临时 turn。

---

## 3. 对「agent 框架 + 业务 agent + LLM」这个设计的判断

**方向对，但“业务 agent”不能只是一段 system prompt；同时，LLM 的参与面需要*缩小*，实现手段不是为每个平台手写 Rust，而是让 agent 通过统一 actuator 工作，并把跑通的流程沉淀下来重放。**

用户提出的更合适分解是三层：

```
┌─ 业务 agent（LLM）────────── 判断：写文案 / 回客服 / 定价 / 看报表决定调什么 / 异常分支
├─ 统一 actuator ──────────── agent + MCP / agent + API / agent + Computer use
│   接新平台 = 配 MCP server 或凭据，不写平台专属代码
└─ Agent 框架（现有 + 待补）── engine / hosts / policy / store / mcp / connectors
                              待补：状态与幂等、程序化 computer use、自治循环
```

这不是“纯配置即可完成所有业务”。MCP/API/Computer use 负责触达外部平台，但它们默认是无状态调用；所有业务场景仍共同需要第 2 节的状态与幂等层、程序化 computer-use 循环和自治调度。平台接入的差异应主要落在 MCP server、API 凭据、computer-use profile 和 agent 配置，而不是复制一套平台专属业务代码。

### 3.1 当前 Skill / Playbook / Host 脚本能力

这三类能力已经有真实代码，但目前更接近“把经验和命令交给 agent 执行”，还不是完整的确定性流程重放系统：

- **Skill**：`crates/opcos-assets/src/lib.rs` 的 `discover` 会发现仓库中的 `.agents/skills/**/SKILL.md`，`parse_skill` 将其解析为 `SkillEntry`；`src-tauri/src/main.rs` 的 `append_session_config_assets` 会把选中的配置对象 skill 标记为 active，`engine_for` 通过 `AssetBundle::system_instructions` 注入 agent 的 system message，并由 `record_skill_usage` 写入 `skill_usage`。因此 Skill 当前主要是可选的提示/操作知识注入，不是独立的可调用程序。
- **Playbook**：源码中的配置对象内部称 `runbook`，仓库文件路径是 `.agents/playbooks/*.md`（`crates/opcos-assets/src/lib.rs` 的 `Playbook` / `parse_playbook` / `discover`）。`save_schedule` 将 schedule 绑定到 runbook，`run_schedule_for_inner` 读取 runbook 正文并直接作为一次 `engine.submit_text(prompt)` 的输入；它是可调度的流程提示，不是带步骤状态、补偿和幂等语义的执行引擎。
- **Host 脚本执行**：`crates/opcos-hosts/src/lib.rs` 的 `Host::exec` 及 LocalHost/RvmHost 实现能够在目标 Host 执行 shell 命令（RvmHost 内部调用 RVM 的 `exec_sync`）；`crates/opcos-engine/src/lib.rs` 的 `run_shell` 工具以及 Tauri 的 `RemoteExecutor` 将命令转发到 Host。`src-tauri/src/main.rs` 的 `execute_blueprint` / `run_lifecycle_stage` 还能执行 Blueprint 的 YAML 命令列表，并记录生命周期审计；但没有发现“从 Skill/Playbook 自动编译成可重放脚本”的统一机制。

所以“高频重复动作减少 LLM 参与面”的正确路径是：让 agent 在一次探索/调试后，把稳定步骤沉淀为 Skill、Playbook 或 Host 可执行脚本，之后由调度器/agent 重放；LLM 保留在需要判断、内容生成和异常处理的位置。**沉淀和重放本身仍需补状态、幂等、版本和失败恢复契约，不能把现有 Markdown 注入误称为已经完成的自动化流水线。**

判据应改成：**这个动作是否已经被验证为稳定流程？** 是 → 优先沉淀为 Skill/Playbook/脚本并重放；否，或涉及判断与异常分支 → 交给 LLM，在工具调用过程中记录业务状态。

“给商品写标题和详情”通常需要业务 agent 判断；“按已验证流程提交商品、记录结果并在失败时重试”则应由统一 actuator + 状态层 + 可重放步骤共同完成，而不是要求 LLM 每次从零点击。

---

## 4. 场景可达性评估

| 场景                                        | 当前可达度 | 主要卡点                                                                                                                                     |
| ------------------------------------------- | ---------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| 软件开发（全栈 AI 程序员）                  | **70–80%** | 已有 Git/PR/IDE/LSP/DAP/harness/Team 模板。缺自治循环和部署闭环                                                                              |
| 跨境独立站                                  | **35%**    | 有邮件 connector/config、浏览器 surface、Team 模板。缺建站/部署编排、SEO 与广告的业务对象、素材生成                                          |
| 闲鱼电商                                    | **15%**    | 未发现闲鱼 connector 或业务对象；平台 API/准入政策未在本仓库核验，账号隔离、风控、幂等层未发现                                               |
| 淘宝电商                                    | **15%**    | 未发现淘宝连接器或业务对象；且淘宝开放平台入驻要求企业认证支付宝（个人账号明确不支持）与营业执照，一人公司走不通官方 API［外部来源，见脚注］ |
| 拼多多 / 京东                               | **15%**    | 未发现对应 connector 或业务对象；平台 API/准入政策未在本仓库核验                                                                             |
| 自媒体（小红书/抖音/TikTok/YouTube/公众号） | **20%**    | 未发现这些平台的 first-class 连接器；“YouTube 有官方 API”属于外部事实，本仓库未核验。素材/视频生成流水线完全缺失                             |

---

## 5. 路线建议

### 阶段 0：先收口（1–2 周）

不做新功能，把已经开的口子闭上：

- 合入 #35 / #36
- **RVM IDE 的 `?tkn=` query token** 仍违反 `AGENTS.md` 硬约束 2；需要升级 RVM agent 使完整 IDE 流程支持 Bearer-only，再删掉客户端的 query 注入
- 拆 `src-tauri/src/main.rs`（16073 行）和 `web/src/App.tsx`（8968 行）。这不是洁癖问题：**每接一个新平台都要同时改 adapter / store / engine / UI 四层**，这个成本会随平台数量线性放大，而一人公司的目标恰恰是要接很多平台

### 阶段 1：补自治底座（重点，不要跳过）

第一步的 agent 框架已经基本建成；第二步不是为每个平台手写业务代码，而是让业务 agent 通过统一 actuator 触达平台，并一次性补齐所有场景共用的框架能力。按依赖顺序：

1. **平台无关的业务状态层**：在 `opcos-store` 旁边新增通用 `business_entity` + `business_event` + 来源/账号上下文 + 幂等键/结果状态；由 agent 通过 MCP/API/Computer use 相关工具读写，不为闲鱼、淘宝、Shopify 各建一套平台专属表。
2. **通用持久化任务队列**：在现有 SQLite `coord_tasks` / `coord_messages` / 依赖关系之上，补成面向业务事件的跨 session 真队列，并增加统一唤醒、重试和补偿语义。
3. **自治规划循环**：一个能回答“现在该干什么”的规划器——输入是业务状态、事件和目标，输出是入队任务，并选择合适的 MCP/API/Computer-use actuator。这是“自主驱动”的核心含义。
4. **事件总线**：把 cron / 文件 watcher / loopback callback / 出站轮询统一成一种 event 抽象，而不是四套各自为政的机制。
5. **程序化 Computer use + 账号隔离**：在现有 VNC/CDP surface 之上补动作循环、截图/定位、登录态/profile、凭据边界和可恢复执行；这应是所有需要浏览器的平台共用的能力，不是某个平台的业务实现。

上述第二步仍会反向要求第一步的框架补能力：MCP/API/Computer use 解决“怎么调用”，状态与幂等解决“调用后发生了什么”，自治循环解决“下一步做什么”。

### 阶段 2：打通**一个**业务闭环——建议选跨境独立站

**明确建议不要拿闲鱼/淘宝做第一个闭环。** 理由：

- 选择一个可以使用官方 API 或成熟 MCP 的链路，能先验证“统一 actuator + 状态层 + 自治循环”，避免第一轮就把主要精力消耗在平台风控和登录态对抗上。哪些平台、哪些 API/MCP 在当前账号和地区可用，仍需逐个平台做外部核验。
- 淘宝开放平台的准入资料属于外部事实（见脚注）；闲鱼/拼多多/京东的准入政策未逐个核验，不能把它们当作仓库内结论。
- 早期闭环仍应控制账号封禁和不可逆外部操作风险。

跨境独立站可以作为优先候选闭环；它适合用官方 API 或成熟 MCP 做 actuator 验证，下面列出的具体能力仍需要逐个平台外部核验，本仓库当前没有这些平台的实现：

```
Shopify / WooCommerce API  ->  商品与订单对象
Google Ads / Meta Ads API  ->  广告计划对象 + 报表 + 自动调价
IMAP / Gmail / Outlook 配置与验证（SMTP 未确认） -> 邮件客服工单对象
SEO                        ->  内容对象
```

跑通之后，“统一 actuator + 业务状态/幂等 + 自治循环”这套平台无关框架才会被真实验证。**这时候再去啃闲鱼/淘宝，仍需补对应平台的 MCP/API 配置、账号/风控适配和 Computer use；但不必再重新设计状态和自治底座。**

### 阶段 3：横向复制 + 啃硬骨头

- 有状态浏览器自动化层（账号隔离、登录态、风控、幂等重试）
- 素材生成流水线（图片/文案/视频）
- 闲鱼 → 淘宝 → 拼多多/京东 → 自媒体
- 每个平台落成一个 Team 模板 + 一组业务能力，而不是一堆 prompt

---

## 6. 需要「减少」的部分

用户明确问了要减少什么，这三条比较反直觉：

1. **减少 LLM 在高频重复动作上的参与面。** 见第 3 节：让 agent 把已验证流程沉淀为 Skill / Playbook / Host 脚本并重放，而不是每次从零推理和点击。LLM 仍负责判断、内容生成和异常分支。

2. **停止对齐 Devin 的 feature matrix。** `docs/10-reference-matrix.md`、`13-devin-behavior.md`、`14-devin-gap.md` 三份文档在持续拿 OPCOS 对标 Devin（Managed Devins、DeepWiki、Devin Review、stacked PRs…）。**OPCOS 的北极星已经和 Devin 分叉了**——Devin 是编码 agent，OPCOS 要做一人公司系统。继续按 Devin 的功能清单补齐，会把有限精力花在对目标无贡献的地方。建议保留这些文档作为历史参照，但从 roadmap 的驱动地位上移除。

3. **减少 `src-tauri` 的职责。** 它现在同时是 adapter、业务服务层和编排层。见阶段 0。

---

## 7. 一句话回答「还差多少」

- **框架层**：差 15%，主要是自治循环和工程债
- **业务层**：差 85%，因为业务对象模型、有状态平台交互、素材流水线三块基本从零开始
- **最关键的判断**：不要用「再写几个业务 agent」的方式去填业务层的 85%。先补第 2 节那三层，否则每个业务场景都会变成一次性的、不可复用的 prompt 工程。

---

## 8. 入库前事实校正

通读原始评审稿并对照当前源码后，做了以下校正：

1. **`opcos-store` 的实体范围**：原稿把它概括成只有 session/message/pending 等 agent 执行实体；实际还持久化了 `project`、`project_agents`、`artifacts` 等执行/项目实体。修正为“缺少业务域实体”，而不是“没有任何其他实体”。
2. **`BoardTask` 的持久化**：原稿称 BoardTask 只存在于内存；实际 `coord_tasks`、`coord_messages`、任务依赖和认领状态已经由 `src-tauri/src/main.rs` 持久化到 SQLite。修正为“已有协同地基，但还不是通用业务事件队列”。
3. **浏览器能力表述**：原稿将浏览器概括为“给人看的 surface relay”；修正为已暴露 VNC/CDP surface，但尚未发现更高层的确定性浏览器业务原语。
4. **连接器成熟度表述**：原稿的“约 25 个真实 SaaS 已接”改为“connector catalog、身份探活和部分工具路径覆盖多种连接器；engine 实际暴露的业务工具少于 catalog”，避免把 catalog 条目等同于完整业务集成。
5. **行数与外部平台政策**：#36 分支上的 `opcos-engine` 实际为 6134 行，原稿的 6499 行已修正。淘宝准入政策来自平台官方文档（见脚注），是有来源的外部事实，与仓库内可核验的代码事实分开标注；闲鱼/拼多多/京东的政策以及“YouTube 有官方 API”未经核验，已改为不作断言。
6. **邮件能力表述**：UI catalog 有邮件能力描述，但源码当前确认的是 Gmail/Outlook OAuth 配置和 IMAP 登录/身份验证路径，未找到对应的 engine 收发工具 dispatch；报告已不再把邮件收发写成已完成能力。
7. **统一 actuator 的架构修正**：用户提出并经本次评审接受：业务 agent 应通过 `agent + MCP`、`agent + API`、`agent + Computer use` 触达平台，取代原稿中容易被理解为“每个平台手写确定性业务能力层”的表述。原稿的状态层论证予以保留并改写为平台无关的通用实体、事件、账号上下文、幂等键和结果状态；它记录 agent actuator 已经造成的外部事实，不负责复制各平台业务 schema。

---

## 脚注：外部来源

- 淘宝开放平台开发者入驻要求：淘宝账号需绑定**企业认证**的支付宝账号，「个人账户不支持」；应用上线审核还需提供营业执照副本扫描件。来源：淘宝开放平台文档中心 <https://open.alitrip.com/docs/doc.htm?articleId=43&docType=1&treeId=803> 与 <https://developer.alibaba.com/docs/doc.htm?treeId=552&articleId=107424&docType=1>（核验于 2026-08）。
- 本文中其余涉及外部平台 API、准入与风控政策的说法均未核验，已在正文标注，不作为结论依据。
