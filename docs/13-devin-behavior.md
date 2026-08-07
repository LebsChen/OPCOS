# Devin 真实行为基线（实地勘察）

来源：`app.devin.ai` 实地操作 + `docs.devin.ai` 全量文档。本文只记录**观察到的事实**，不含 OPCOS 设计决策。
勘察日期：2026-08-03。组织 `cloud-3301`，Linux 平台会话。

---

## 1. 全局信息架构

顶层是「组织应用」与「设置」两个模式，设置是**整页**（左侧分组导航 + 右侧正文），带 `← Back to app` 返回。

应用模式左栏（自上而下）：

| 区块 | 项 |
| --- | --- |
| 组织切换 | `C cloud-3301 ▾`、搜索、折叠侧栏 |
| 主导航 | `New session`（`Ctrl ⇧ O`）、`Automations`、`Security`、`Review`、`Wiki` |
| Recent | 会话列表，条目显示标题 + 状态（`Working`） |
| 底部 | `Upgrade`、`Settings`、下载客户端、`Help` |

`New session` 就是首页本身（`/org/<slug>`），不是弹窗。

## 2. 首页 = 新建会话页

居中单卡片，卡片右上是 `Agent | Ask` 胶囊切换。输入框占位符 `Ask Devin to build features, fix bugs, or work on your code`。

底部控件条（左→右）：

- `+` **Attach or mention**：`Upload attachment` / `Repositories` / `Codebase files` / `Skills` / `Devin sessions` / `Playbooks` / `Secrets` / `Actions`
- **Configuration**（滑杆图标）：`Virtual environment: Ubuntu` / `Notable repositories: 0` / `Manage MCP connectors`
- **Agent chip**：`Fusion`（可选 Devin / Fast / Fusion / Dana 等预设，不是裸模型名）
- 右侧：麦克风、`Send`、`More send options`

卡片下方：CLI 推广条 + `Get started 3 of 6` 引导清单（Connect to Git / Select repositories / Make your first session / Validate in Devin Review / Set up your wiki / Ask Devin about your codebase）。

**要点**：首页没有常驻的仓库/分支选择器，仓库通过 `+ → Repositories` 或 Configuration 进入。

### Ask vs Agent

- **Ask**：只读。代码问答（带引用）+ 规划；产出「Devin Prompt」后 `Send to Devin` 转 Agent 会话；Ask 会话内直接显示所派生 Agent 会话的状态。
- **Agent**：可写代码、跑命令、开 PR。
- 模式/agent **可在会话中途切换**，通过输入框旁的 chip，下一条消息生效。

### @ 提及与 / 命令

`@` 下拉：`@Repos` `@Files` `@Macros`(Knowledge 宏) `@Playbooks` `@Skills` `@Secrets` `@Sessions`。

`/` 下拉：内置 `/plan` `/review` `/test` `/think-hard` `/implement`；选中后在输入框内成为**可展开的 chip**（展开为完整模板再编辑）。组织可增删自定义命令。

## 3. 会话页

三段式：左侧栏（同上）+ 中间 transcript/composer + **右侧独立工作区**。

### 中间

- transcript：用户消息气泡 + Devin 正文 + 可折叠的「工作块」，标题形如 `Worked for 6m 42s`，内部逐条 `Thought for 13s ▸` / `Clicked at (886, 104)` / `Waited 3 s` / `Took screenshot`，末行是当前动作（`Navigating Devin UI`）。
- composer 占位符：`Guide Devin while it works, or press Ctrl ⏎ to queue` —— 工作中可**排队**消息。
- composer 左侧 `+`，中间 agent chip（`Fusion`），右侧麦克风与**停止**按钮。

### 右侧工作区

标签栏可增删、可全屏：`Progress` `Desktop` `Shell` `Editor` `Changes` `Tasks` `Agents`，末尾 `+` 添加面板、`⤢` 展开。

| 面板 | 观察到的内容 |
| --- | --- |
| Progress | 结构化进度/行动时间线 |
| Desktop | 交互式浏览器/整机桌面，可人工接管（登录、MFA、CAPTCHA），会话内 cookie 持久；原名 Browser |
| Shell | 命令历史 + 起始时间/时长 + 输出 + 终端 + `Go to live desktop` |
| Editor | 内嵌 VS Code（Explorer/搜索/源码管理/运行/扩展 + 集成终端，标题 `Devin IDE`），可暂停 Devin 后人工接管 |
| Changes | diff 视图 |
| Tasks | 结构化任务清单，`0/2 tasks completed`，条目 `#1 …` |
| Agents | 子会话（managed Devins）列表，空态 `No child sessions have been created yet.` |

### 会话菜单（topbar `⋯`）

`Rename` / `Folder ▸` / `Edit tags` / `Archive` / `More ▸` / `Give feedback` / `Session usage limits` / `Session insights`，页脚常驻用量：`On-demand usage: $10.14`、`User messages: 2`、`Session size: M`、`Platform: Linux`。

`More ▸`：`Reboot virtual machine` / `Update network config` / `Copy session link` / `Hide from team` / `Knowledge suggestions` / `Create automation from session` / `Create playbook from session` / `Analyze session`。

## 4. 设置（整页）

首页是**卡片索引**，与左栏分组一一对应。

| 分组 | 项 |
| --- | --- |
| Personal | Preferences、Connections |
| Organization | General、Connections、Plans、Invoices、Usage & Limits |
| Products | Devin、Review、DeepWiki、Schedules、Devin Desktop |
| Resources | Knowledge、Environment、Playbooks、Skills & Rules、Secrets |
| Administration | Repositories、Membership、Devin API、Analytics |

正文统一为「分节标题 + 描述 + 卡片内行式条目（右侧控件）」。

### Settings → Devin

- Sessions：`Enable native deployments`、`Computer use`（关闭则退回 legacy browser tools）
- Session agents：`Default agent`(Fusion)、`API default agent`、`Default platform`(Ubuntu)
- Commands：`/implement` `/plan` `/review` `/test` `/think-hard` 标 `System`，自定义命令可编辑/删除，含 `Reset` / `Add Command`
- Usage limits：`Batch limit`（Devin 每批可创建的会话数，1–500）、`Message usage limit`（每条消息的按需消耗上限）
- Pull requests：`Share prompts in PRs`、`Require @Devin to respond`、`Auto-add reviewer`、`Open PRs as`、`Responding to bots`

### Settings → Environment

四个 tab：`Blueprints` / `Snapshots` / `Advanced` / `Outposts`。
- 组织 blueprint（可为空，「Not set」）
- Repositories：**有序**列表，「按此顺序克隆并 setup」，可拖动排序、可搜索/筛选/添加，条目显示所有者、加入时间、连接状态告警。

### Settings → Skills & Rules

两个 tab：`Usage` / `Browse`。Usage 是分析看板：`Usage over time` 折线 + `Invocations / Skills used / Cloud sessions` 计数 + `Most invoked skills` + `Task types` 饼，下方表格（Skill、来源 repo、Invocations、Sessions、Users、Last used、View sessions）。
**Skill 不在 UI 里创建**——来源是仓库里的 `.agents/skills/<name>/SKILL.md`（Agent Skills 标准），Devin 跨所有已连接仓库自动发现。

### Knowledge

`Trigger Description`（触发检索的描述，必填）+ `Content` + `Macro`(`!name`) + **逐用户启用/禁用** + 文件夹（嵌套、批量开关、拖动、Devin 自动整理）+ pin 到 无 repo / 指定 repo / 所有 repo。会话中 Devin 会**自动生成 Knowledge 建议**，可编辑/驳回/让 Devin 按反馈重新生成，也可建议更新既有条目。企业版分 Organization / Suggestions / Enterprise 三个 tab，支持 `Promote to Enterprise`。

### Playbooks

结构化模板：`Overview` / `What's Needed From User` / `Procedure` / `Specifications` / `Advice and Pointers` / `Forbidden Actions`。支持 macro(`!name`)、**版本历史与回滚**、拖入 `<name>.devin.md` 附加、附加后在 composer 显示**蓝色 pill 且可内联编辑**、Team 库 + Community 库。

## 5. Automations

`All / Created by you` 计数 tab + 筛选 + 搜索 + `Create automation`。空态三卡：`Create manually`（自行配置 triggers / actions / limits）、`Start from template`、`Generate with Devin`（描述需求，Devin 在一个会话里帮你建）。
触发源：定时、Slack 消息、GitHub 事件、incoming webhook。

## 6. 高级能力（文档，任何会话内「直接说」即可）

- **Managed Devins**：拆分大任务 → 并行子会话（各自独立 VM），可带 prompt/playbook/tags/ACU 上限；协调者可给子会话发消息、监控 ACU、休眠/终止、给自己排定提醒；启动前需用户确认。
- **Analyze session**：分析历史会话为何成功/失败，产出改进后的 prompt。
- **从会话生成 playbook / knowledge 建议 / automation**。
- **Schedules**：cron 或一次性，开关、通知偏好、选择 agent。
- **DeepWiki**：仓库索引 + 文档 + 问答（Ask 可从 wiki 页发起并自动限定该 repo）。
- **Devin Review**：PR 自动评审。**Stacked PRs**。
- **Computer use**：鼠标/键盘/屏幕，可在设置里关闭退回 legacy browser tools。
- **测试与录屏**：Devin 可截图/录屏作为测试证据回传。
- **Session insights** / **Session usage limits**。
- 上述能力同时通过 **Devin MCP** 暴露给任何 MCP 客户端（会话/playbook/knowledge/schedule/integration/repo 文档管理）。
