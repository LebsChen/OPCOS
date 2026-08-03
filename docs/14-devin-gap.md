# Devin 与 OPCOS 功能差距盘点

> **当前代码校正（origin/dev，2026-）：** 本文的历史盘点基线早于 #44–#53，
> 不能直接作为当前实现清单。当前已存在 Git/PR 工具、GitHub Actions CI
> status/failure-log、background jobs、精确 `edit_file`、tracked plans、
> learned skills、Leader-only coordination、Commands 和 `.agents/mcp`
> discovery。它们仍有明确非等价边界：CI 没有自动修复循环，background job
> 没有跨重启接管，LSP 只支持 LocalHost，Git push/PR verification 只支持
> GitHub，ACP 不经过 builtin tool catalog，协同 Worker 自述不构成完成证据。
> 本文后续表格保留历史证据；决定下一步时应以 `todos.md` 和当前源码为准。

基线来自 `docs/13-devin-behavior.md`，判定标准是「数据模型 + 后端行为 + UI 入口三者齐备才算『有』」。盘点时点 = `dev@fa78271`。

## A. 首页（新建会话页）

| 编号 | Devin 功能 | OPCOS 状态 | 证据（文件:行） | 差距说明 |
|---:|---|---|---|---|
| 1 | Agent / Ask 双模式切换；Ask 只读规划并可 Send to Devin | **部分** | `web/src/components/Composer.tsx:45-82,1038-1107`；`web/src/App.tsx:850-857` | 有 Discuss/Interactive/Auto 权限模式，其中 Discuss 接近只读，但没有独立 Ask 工作流，也没有“Send to Devin”转换。 |
| 2 | 会话中途切换 agent，下一条消息生效 | **部分** | `web/src/App.tsx:621-667`；`src-tauri/src/main.rs` 中注册 `change_model`、`change_mode`、`change_harness` 命令附近 | 可以切换模型、模式和 harness，但没有 Devin/Fast/Fusion/Dana 这类 agent preset；模型在已有历史会话中的可切换行为也不是完整的 Devin agent 切换。 |
| 3 | 一个或多个仓库及分支选择器 | **部分** | `web/src/App.tsx:785-864`；`crates/opcos-store/src/lib.rs:1104-1125` | 首页只能选择一个 host 并填写一个 workspace 路径；没有多仓库选择器，也没有首页分支选择器。 |
| 4 | Agent 选择器（Devin/Fast/Fusion/Dana 等 preset） | **部分** | `web/src/App.tsx:787-857`；`web/src/components/Composer.tsx:84-107` | 现有选择的是 harness、provider、model 和 permission mode，不是具有独立行为/配置的 agent preset。 |
| 5 | `@Repos` / `@Files` / `@Macros` / `@Playbooks` / `@Skills` / `@Secrets` / `@Sessions` | **部分** | `web/src/components/Composer.tsx:776-878` | 支持规则、Knowledge、Playbook、Skill 和 Secret 插入；没有仓库、文件、宏、会话的实际 mention picker，也不是完整的 `@Secrets` 语法。 |
| 6 | 附件上传 | **有** | `web/src/components/Composer.tsx:30-39,350-365,776-833`；`web/src/App.tsx:752-783,5768-5780` | 支持文本文件和 PDF 首页附件，上传到 Tauri 后端并插入会话引用。 |
| 7 | `/plan`、`/review`、`/test`、`/think-hard`、`/implement` 及组织自定义命令 | **无** | `web/src/components/Composer.tsx:350-400` 仅处理普通文本提交；`web/src/App.tsx:733-873` | 未发现 slash-command 解析、模板展开、chip 插入或组织自定义命令存储。 |
| 8 | Configuration 快捷入口 | **部分** | `web/src/components/SettingsView.tsx:10-39`；`web/src/App.tsx:5981-6004` | 有 Settings、hosts、MCP、connectors、blueprint 入口；没有 Devin 式虚拟环境/平台、notable repositories 等完整 Configuration 快捷入口。 |

## B. 会话页

| 编号 | Devin 功能 | OPCOS 状态 | 证据（文件:行） | 差距说明 |
|---:|---|---|---|---|
| 9 | Transcript + composer；工作中可排队消息 | **有** | `web/src/App.tsx:605-677`；`web/src/components/Composer.tsx:374-400`；`crates/opcos-engine/src/lib.rs:650-670`；`src-tauri/src/main.rs:4149-4170` | 工作中输入会进入 `steering` 队列，engine 在当前 turn 完成后处理。 |
| 10 | 右侧独立工作区，Progress/Desktop/Editor/Changes/Shell/Tasks/Agents，可增删 tab | **部分** | `web/src/App.tsx:4826-5170` | 有右侧 rail、独立窗口、Diff、Shell、Desktop、Web IDE、Browser、PR、Insights；没有 Progress/Tasks/Agents 这套 Devin tab 集合，也没有完整的 tab `+` 菜单和删除机制。 |
| 11 | Progress 结构化行动时间线和折叠条目 | **部分** | `web/src/App.tsx:1135-1186`；`web/src/App.tsx:3845-4040`；`src-tauri/src/main.rs:767-810` | 有 Worklog、audit 和 Activity 页面，但没有完整的 Thought/Clicked/Waited/Took screenshot 类型折叠条目及 Devin 的 “Worked for Xm” 进度呈现。 |
| 12 | Tasks 结构化任务清单，显示 `0/N completed` | **部分** | `web/src/App.tsx:4141-4428`；`crates/opcos-engine/src/orchestration.rs:24-150` | 有持久化 coordination task board，可创建、认领、完成和验收；不是会话内 Devin Tasks 面板，也没有 `0/N completed` 汇总。 |
| 13 | Agents 子会话/managed Devins 列表 | **无** | `web/src/App.tsx:3869-4100` 只有 roles/coordination；`crates/opcos-engine/src/orchestration.rs:115-290` | coordination roles 不是实际子会话列表；未发现 managed Devin 子会话生命周期或 Agent tab。 |
| 14 | Shell 命令历史、开始时间、时长、输出和终端 | **部分** | `web/src/App.tsx:692-850`；`src-tauri/src/main.rs:2868-2877`；`src-tauri/src/main.rs:7135-7238` | 有可交互 PTY 和生命周期 audit 中的 command/elapsed/output；没有 Devin 式独立 Shell history 列表与每条命令的完整终端记录 UI。 |
| 15 | 内嵌 IDE，可人工编辑和开终端 | **有** | `web/src/App.tsx:682-850`；`src-tauri/src/main.rs:2877-2948` | 通过 `vscode-remote://` 和 IDE proxy 加载远程 Web IDE；Shell 另有 PTY surface。 |
| 16 | Desktop 交互式浏览器/桌面，人工接管，cookie 持久 | **部分** | `web/src/App.tsx:721-820`；`src-tauri/src/main.rs:2848-2877`；`crates/opcos-hosts/src/lib.rs:513-515,1183-1210` | 有 VNC Desktop 和 CDP Browser surface；没有仓库级证据证明 cookie/session 持久化，也没有完整登录/验证码接管流程。 |
| 17 | Changes diff 视图 | **有** | `web/src/App.tsx:902-996,4867-4900`；`src-tauri/src/main.rs:7617-7700` | `review_snapshot` 和 `review_file_diff` 支持本地/远程 git diff，并在右侧 Diff 面板呈现。 |
| 18 | 会话菜单：Rename、Folder、Archive、tags、链接、VM/network、automation/playbook/analysis 等 | **部分** | `crates/opcos-store/src/lib.rs:1104-1125` 有 title/pinned/archived；`web/src/App.tsx:4867-4900` 仅有 Info/Artifacts/PR/Insights 等 tab | 存储层有部分 session 字段和 Insights，但没有 Devin 式统一会话菜单及大多数操作，例如复制链接、重启 VM、network config、从会话生成 playbook/automation。 |
| 19 | 会话用量：金额、用户消息数、session size、platform | **部分** | `web/src/components/Composer.tsx:888-980`；`src-tauri/src/main.rs:8542-8580` | 有 token/context usage 和 `message_count`、tool calls、duration；没有 on-demand 金额、session size、platform 的完整展示。 |

## C. 组织级导航

| 编号 | Devin 功能 | OPCOS 状态 | 证据（文件:行） | 差距说明 |
|---:|---|---|---|---|
| 20 | New session / Automations / Security / Review / Wiki / Recent / Settings / Help | **部分** | `web/src/App.tsx:5300-5355,5807-6005`；`web/src/components/Sidebar.tsx:1-110,760-967` | 有新建会话、Recent、Automations、Activity/Inbox、Settings；没有完整 Security、DeepWiki/Wiki、Help 和独立 Devin Review 导航。 |

## D. 设置

| 编号 | Devin 功能 | OPCOS 状态 | 证据（文件:行） | 差距说明 |
|---:|---|---|---|---|
| 21 | Personal: Preferences / Connections | **部分** | `web/src/components/SettingsView.tsx:10-39`；`web/src/App.tsx:1597-1937,2776-3033` | 有 appearance/provider/connectors，但没有 Devin 式 Personal 分组和 Preferences/Connections 页面结构。 |
| 22 | Organization: General / Connections / Plans / Invoices / Usage & Limits | **部分** | `web/src/components/SettingsView.tsx:25-39`；`src-tauri/src/main.rs:8542-8580` | 有本地 connectors 和 usage metrics，但没有组织 General、Plans、Invoices、Usage & Limits 管理体系。 |
| 23 | Products: Devin / Review / DeepWiki / Schedules / Devin Desktop | **部分** | `web/src/App.tsx:3617-3845,902-996`；`web/src/components/SettingsView.tsx:25-39` | 有 Schedules、手动 Review、Desktop surface；没有独立 Devin/DeepWiki 产品页，Review 也不是自动 Devin Review 产品。 |
| 24 | Resources: Knowledge / Environment / Playbooks / Skills & Rules / Secrets | **部分** | `web/src/components/SettingsView.tsx:25-39`；`web/src/App.tsx:2200-2780,3220-3425` | Knowledge、Playbook、Skill、Rules、Secrets、Blueprint/hosts 均有入口，但没有 Devin 式 Resources 分组和完整 Environment 页面。 |
| 25 | Administration: Repositories / Membership / Devin API / Analytics | **部分** | `web/src/App.tsx:1970-2140,3032-3058,3220-3425,4658-4700` | 有 hosts/repository index、Devin API key 和 session insights；没有 Membership、组织仓库权限管理和组织 Analytics。 |
| 26 | Devin 设置具体项：native deployments、Computer use、Default agent、API default agent、platform、自定义命令、Batch/message limits、PR 设置 | **部分** | `web/src/App.tsx:1565-1937`；`web/src/components/Composer.tsx:45-82`；`crates/opcos-hosts/src/lib.rs:513-515` | 有 provider/model/mode 和 host 配置，但没有这些 Devin 组织设置、custom slash commands、batch/message limits 或 PR policy 设置；Computer use 还是 capability 层的可用性。 |
| 27 | Environment：Blueprints / Snapshots / Advanced / Outposts；组织 blueprint；仓库可排序 | **部分** | `web/src/App.tsx:3220-3425`；`crates/opcos-assets/src/lib.rs:45-75`；`src-tauri/src/main.rs:7242-7285` | 只有单一 host blueprint 读取/执行和 lifecycle stages；没有 Snapshots、Advanced、Outposts、组织 blueprint 或仓库 clone/setup 排序。 |
| 28 | Skills Usage 分析 + Browse 仓库 `.agents/skills/SKILL.md` | **部分** | `web/src/App.tsx:2266-2745`；`crates/opcos-assets/src/lib.rs:80-105`；`src-tauri/src/repo_index.rs:1-205` | 有 Skill 配置对象、启用/停用和 repo index；没有 skill 调用次数/用户/任务类型统计，也没有专门浏览 `.agents/skills/SKILL.md` 的 Browse 页面。 |
| 29 | Knowledge trigger/content/macro、逐用户开关、嵌套文件夹、pin、suggestions | **部分** | `web/src/App.tsx:2266-2780`；`src-tauri/src/main.rs:4912-5018` | 有 body、trigger、enabled、global/repository scope 和版本历史；没有用户级启用、文件夹树/拖动/批量开关、pin 体系或自动 Knowledge suggestions。 |
| 30 | Playbooks 结构化章节、macro、版本历史回滚、`.devin.md` 附加 | **部分** | `web/src/App.tsx:2238-2780`；`web/src/App.tsx:610-800`；`src-tauri/src/main.rs:4536-4855` | 有 Playbook body、编辑、版本历史、compare 和 rollback；没有结构化章节字段、macro、`.devin.md` 拖入/蓝色 pill 内联编辑。 |
| 31 | Secrets 会话内引用且不泄漏值 | **部分** | `web/src/components/Composer.tsx:800-878`；`src-tauri/src/main.rs:82,566-923,6255-6490`；`crates/opcos-store/src/lib.rs:544-730` | SecretStore、redaction 和会话内 Secret 插入存在；UI 插入的是 `secret:session:<name>` 而非 Devin `@Secrets`，且没有完整的 Secrets mention picker。 |

## E. 高级能力

| 编号 | Devin 功能 | OPCOS 状态 | 证据（文件:行） | 差距说明 |
|---:|---|---|---|---|
| 32 | Managed Devins：并行子会话、prompt/playbook/tags/ACU、消息、监控、休眠/终止、提醒 | **部分** | `crates/opcos-engine/src/orchestration.rs:24-290`；`web/src/App.tsx:3845-4494` | 有 coordination board、roles、task claim/complete/message 和 active/sleep/paused 状态；没有真实子会话创建、并行 Devin、ACU 限额、子会话消息路由或终止生命周期。 |
| 33 | Analyze session，分析历史会话并产出结论 | **部分** | `src-tauri/src/main.rs:8542-8580`；`web/src/App.tsx:4658-4700,5014-5039` | `session_insights` 只聚合 message/tool/approval/token/duration 指标，不生成自然语言分析结论。 |
| 34 | 从会话生成 playbook、knowledge 建议、automation | **无** | `web/src/App.tsx:2540-2745,3617-3845` 只有手工资产和 schedule 管理；未发现 session-to-asset generation command | 没有从历史会话自动生成 playbook、Knowledge suggestion 或 automation 的实现。 |
| 35 | Schedules：cron/一次性、开关、通知偏好、agent 选择 | **部分** | `web/src/App.tsx:3617-3845`；`src-tauri/src/main.rs:7935-8180`；`src-tauri/src/scheduler.rs:1-80` | 有 cron、filesystem trigger、启用开关、prompt、host/workspace/harness/mode；没有一次性定时、通知偏好和 Devin agent 选择。 |
| 36 | Automations：事件驱动 | **部分** | `src-tauri/src/main.rs:126-130,7935-8537`；`web/src/App.tsx:3617-3845` | 有 cron、filesystem watcher、loopback HTTP callback、single-flight 和 audit；没有完整的 Devin 事件源/组织级 automation builder。 |
| 37 | DeepWiki：仓库索引 + 文档/问答 | **部分** | `src-tauri/src/repo_index.rs:1-205`；`crates/opcos-engine/src/lib.rs:1838-1840` | 有文件、符号和文本搜索索引；没有 DeepWiki 文档生成、知识图谱或面向仓库的自然语言问答。 |
| 38 | Devin Review：PR 自动评审 | **部分** | `web/src/App.tsx:902-996,997-1105`；`src-tauri/src/main.rs:7617-7700` | 有手动 Review/Diff 和 GitHub PR 创建；没有 PR webhook/自动触发、评论发布、自动评审策略或 Review bot。 |
| 39 | Stacked PRs | **无** | `src-tauri/src/main.rs:7245-7464` 仅有普通 git workflow；`crates/opcos-engine/src/orchestration.rs:140-146` 仅检测 branch/PR 冲突 | 没有 stacked branch/PR 依赖、排序、批量更新或合并编排。 |
| 40 | Computer use 与 legacy browser tools 切换 | **部分** | `web/src/App.tsx:721-820`；`src-tauri/src/main.rs:2848-2877`；`crates/opcos-hosts/src/lib.rs:513-515,1183-1210` | 有 VNC Desktop 和 CDP Browser 两类 surface；没有完整 Computer-use 鼠标/键盘工具选择器，LocalHost 明确不支持 computer use。 |
| 41 | 测试与录屏（testing-and-recordings） | **部分** | `web/src/App.tsx:2026-2038` 有 host Test；`web/src/components/Composer.tsx:166-284,420-450` 有语音录音/转写状态 | 有 host connectivity test 和语音录音状态，但没有 Devin testing-and-recordings 的桌面操作录屏、测试录像存档或回放。 |
| 42 | MCP marketplace / connectors | **部分** | `crates/opcos-mcp/src/lib.rs:31-115,740-1030`；`src-tauri/src/main.rs:84-85,9046-9050`；`web/src/App.tsx:293-453,2776-3033` | MCP server 生命周期、工具缓存和静态 connector catalog 存在；没有真正的 MCP marketplace/registry 浏览、安装、版本管理流程。 |
| 43 | GitHub / GitLab / Bitbucket / Jira / Linear / Slack / Teams 集成 | **部分** | `crates/opcos-engine/src/lib.rs:1826-1840`；`src-tauri/src/main.rs:5157-5205,6556-6695`；`web/src/App.tsx:293-453,2776-3033` | GitHub、Linear、Slack 有实际 API 路径，GitLab/Jira 主要停留在工具定义或有限 connector 配置；Bitbucket、Teams 未实现为同等可用集成。 |

## 结论

总体上，OPCOS 已经具备较完整的**单会话 Agent 工作台、远程 host/PTY/VNC/IDE、配置资产、MCP、审计、调度和部分连接器**，但与真实 Devin 的差距主要集中在：**Ask/Agent 产品模式、managed child Devins、组织级设置体系、DeepWiki、自动 Review、session-derived assets、完整 mention/slash command 系统、usage/limits、以及多平台集成产品化**。
