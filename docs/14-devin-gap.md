# Devin 与 OPCOS 功能差距盘点（当前状态版）

> **说明**：本文把早期「差距清单」更新为当前真实状态。历史基线早于 #44–#53，
> 不能再直接当实现清单看。本文只保留已经核实过的事实；核实不了的地方明确标注
> **未核实**，不把猜测写成结论。
>
> 当前已存在 Git/PR 工具、GitHub Actions CI status/failure-log、background jobs、
> 精确 `edit_file`、tracked plans、learned skills、Leader-only coordination、
> Commands、`.agents/mcp` discovery、lifecycle hooks、声明式权限规则、以及一组
> 受 `BUILTIN_AGENT_INSTRUCTIONS` 约束的模型工具目录。它们仍有明确非等价边界：
> CI 没有 Devin 式自动修复闭环，background job 没有跨重启接管，LSP 只在 local
> host 挂载，Git push/PR verification 仍是 GitHub 绑定，ACP 不透传 builtin tool
> catalog，协同 worker 自述不等于交付证据。

基线来自 `docs/13-devin-behavior.md`；当前写法按「已关闭 / 仍开放 / 需修正文案」三类记录。

## 1. 已关闭的差距

以下条目都已经在 #71–#77 这一串 PR 里关掉了。每条都写了关闭方式和可追溯证据。

| 差距 | 关闭方式 | 证据 |
|---|---|---|
| 硬编码 12 步上限 | #71 改成预算驱动的循环，不再固定 12 步；真实观测 24 轮、65 次调用未触顶。 | PR #71；实测 24 轮 / 65 次调用。 |
| 瞬时 429/5xx 不重试 | #71 加了有界退避与重试，不再把一次瞬时失败当最终失败。 | PR #71。 |
| 压缩假摘要且会丢 system prompt | #71 改成真实 LLM 摘要，压缩后保留系统信息；记忆探针在压缩后零工具调用答对。 | PR #71；压缩后记忆探针实测。 |
| 无基础 agent 提示词 | #72 引入内置提示词，作为模型运行时的默认行为约束。 | PR #72。 |
| 多条 system 消息导致 Agnes 400 | #72 收束为单条 system 消息；真实观测 27 个出站请求全部保持单条。 | PR #72；27 个出站请求观测。 |
| 无运行时上下文 | #72 注入 host / 平台 / 时间 / 模型 / 集成等 runtime context；未观测到凭据泄漏。 | PR #72；凭据泄漏检查未见异常。 |
| 资产发现把技能附属文档全量注入、无长度上限 | #73 收紧资产发现和预算；DevinOS 实测 212 文件、约 1.25MB。 | PR #73；DevinOS 实测。 |
| 无声明式权限、无法表达 deny | #74 加入 allow / deny 规则。 | PR #74。 |
| 嵌套 `tool_use_id` 丢失导致 Agnes 多轮不可用 | #75 修复 Agnes 结果标准化，嵌套 `content[0].tool_use_id` 也会转成 `tool_call_id`。 | PR #75；相关测试。 |
| plan step id 展示与可用不一致，9/9 `plan_update` 全败 | #75 直接渲染真实 `step.id`，模型看到的标识符可直接用于 `plan_update`。 | PR #75；9/9 失败问题被消除。 |
| 无 lifecycle hooks | #76 落地 `PreToolUse` / `PostToolUse` / `PostCompaction` / `Stop` 四个事件，并把启用权收紧到本地未提交配置。 | PR #76。 |
| 提示词推荐的工具没进 `allowed_tools`、还引用不存在的 `repo_index_refresh` | #77 把推荐工具补进模型可见目录；`repo_index_*` 改成索引缺失/过期时自动刷新后重试；加了目录不变量测试。 | PR #77；`builtin_prompt_tools_are_present_in_local_tool_catalog_and_allowlist`。 |
| `edit_file` 契约不可用，逼出全文件重写 | #77 澄清 schema，补了 `edits` 数组契约和示例，并兼容合理的单编辑形状。 | PR #77；`exact_edit_accepts_single_replacement_compatibility_shape`。 |

## 2. 真实任务对比里仍然开放的行为差距

这几条是从真实对比里直接观察到的行为差异。前两条已经在 #77 的提示词里加了指引，但**提示词指引不等于行为已改变**；它们要等下一轮真实任务再验证，才能从这里移走。

| 行为差距 | 当前状态 | 备注 |
|---|---|---|
| 写测试前不先 smoke 跑真实行为 | 仍开放，但已加提示词指引 | Devin 会先跑真实 `add` / `list` / `export` 再看输出；OPCOS 目前只是在提示词里要求“先 smoke-run 再写断言”，还没有用真实任务验证这一行为已经稳定发生。 |
| 任务有歧义时不停下来问 | 仍开放，但已加提示词指引 | Devin 在任务描述有多种合理解读且选错代价高时会停下来问；OPCOS 已把这条合并进 `ask_user` 的边界描述，但还不能据此宣称行为已改变。 |
| 完成前没有“核对所有编辑点 / 检查引用”的自检步骤 | 仍开放 | 这是真实对比中观察到的缺口；当前没有证据表明已形成强制自检流程。 |

## 3. 架构级差距：逐条核对当前是否仍成立

下面是对旧版文档里那些架构级差距的复核结果。不是所有条目都能一次核实完，所以我把能确认的和不能确认的分开写；**未核实** 就是未核实，不当成已证实事实。

| 原条目 | 当前状态 | 备注 |
|---|---|---|
| CI 自动修复循环 | **部分成立 / 未核实到 Devin 式闭环** | 代码里已经有 CI repair / monitor / GitHub CI 查询工具，但没有核实到“失败后自动修到绿”的完整闭环。不能再直接写成“完全没有”，也不能写成“已经有 Devin 级自动修复”。 |
| background job 跨重启接管 | **仍成立** | 当前只核实到 background jobs 与持久化状态/输出，没看到可证明的跨重启接管语义。 |
| LSP 仅 LocalHost | **仍成立，但这是有意边界，不是缺陷** | local host 挂载 LSP，remote host 明确不暴露；这条现在应写成设计边界。 |
| git push / PR 校验仅 GitHub | **仍成立 / 未核实其他 forge** | 当前实现和验证路径仍围绕 GitHub；是否已经扩展到其它 forge，本文未核实。 |
| ACP 不透传 builtin tool catalog | **仍成立** | 这一点没有被这轮改动改变。ACP 仍按自己的能力面工作，不应假设会自动继承内置工具目录。 |
| worker 自报不等于交付证据 | **仍成立** | #77 仍保留了“看证据，不看口头完成”的约束；这条依旧成立。 |
| Ask / Agent 双模式 | **部分成立 / 需改写** | 现有 UI 已有模式切换与不同权限语义，但还不是 Devin 那种完整 Ask/Agent 产品切面。旧文案不能再写成“完全没有”，更适合写成“只具备局部对应”。 |
| managed 子会话 | **仍开放** | 现有 coordination / work queue 还不是 Devin 式 managed child sessions。 |
| DeepWiki | **仍开放** | 已有 repo index / symbol search，但没有 DeepWiki 式文档问答产品。 |
| 自动 Review | **仍开放** | 有手动 diff / review 能力，但没有 Devin 式自动评审流。 |
| session 派生资产 | **仍开放** | 有 playbook / skill / knowledge / schedule 等资产，但没有从会话自动生成这些资产的闭环。 |
| mentions / slash | **部分成立 / 需改写** | OPCOS 现在有部分 mention / command 入口，但仍缺 Devin 式完整 `@...` 与 `/...` 体系。 |
| 用量限额 | **部分成立 / 需改写** | 已有 token / usage / duration 统计与部分限制，但还不是 Devin 式完整组织级用量与额度产品面。 |
| 更多集成 | **仍开放** | GitHub / Linear / Slack / MCP 等已有部分实现，但 Bitbucket / Teams / 更完整的组织级集成仍未达到 Devin 级覆盖。 |

## 4. 逆向来源与方法

这一节记录“从哪来”以及“哪些东西刻意不采纳”。这些不是为了致敬，而是为了避免把不可信输入伪装成产品事实。

### 4.1 `x1xhlol/system-prompts-and-models-of-ai-tools/Devin AI`

- 约 402 行里，接近 250 行是 Devin 私有命令参考：`<str_replace>`、`<find_and_edit>`、部署/浏览器命令、pop quiz 等。
- **刻意不抄** 这些命令名和语法，因为抄进去等于向模型宣传不存在的工具，正好会加重 #77 修掉的那类“提示词里说有、工具目录里没有”的缺陷。
- 采纳的是剩下约 50 行的行为准则：诚实报告、别假装完成、别拿猜测当证据、先修真错因、守住边界。

### 4.2 `kenikiara/jail-break-ai-systems-/DEVIN`

- 这是 Devin 2.0 的 dump。
- 采纳的是沟通、根因、诚实性、代码规范、依赖核实、安全、git 纪律这些原则。
- **排除** 的是身份话术、提示词泄露指令、以及不支持的工具语法。

### 4.3 `kthgff/devin-toolkit`

- 采纳的是 lifecycle hooks 的 **8 事件模型** 和 **声明式权限配置** 这类结构性思路。
- **没有照搬** `.devin/config.json` 路径，而是改成 OPCOS 自己的 `.agents/` 约定。
- **更重要的是没有照搬其信任模型**：Devin toolkit 的项目级配置可以直接授权；在 OPCOS 里，这被判定为不可信输入给自己背书，所以改成只有本地未提交文件才能放松权限。这个偏离是故意的，原因是要堵住“仓库自我授权”漏洞。

### 4.4 `engadnan77/Devin`（DevinOS）

- 采纳的是 `.agents/` 资产约定。
- 也正是这个来源暴露了 #73 那类 context 爆炸问题：把附属文档和资产无上限地塞进提示词，实际会把上下文打爆。

## 5. 结论

当前文档的结论已经从“广泛差距清单”变成“当前状态记录”：

- 一批关键行为已经关闭，并且每个关闭点都有 PR 和实证可追溯。
- 仍开放的行为差距里，有两条已经在 #77 的提示词里加了指引，但**行为是否真的改变**还需要下一轮真实任务验证。
- 架构级差距里，几条旧文案需要改写成“部分成立”或“未核实”，不能再机械照抄旧清单。
- 逆向来源要只吸收行为原则，不吸收不可信项目里的“虚构工具名”或自授权模型。

## 6. Working 过程事件流对齐（当前实现）

### 已实现并已通过本地验证

Builtin engine 现在会把 working 过程作为结构化事件同时写入本地 audit store，
并通过既有 `opcos://event` 的 `stream` payload 向前端转发。事件具有：

- `event_type`、`category`、`direction`、`timestamp` 和结构化 `payload`；
- 每轮的 `status_update`、`simple_activity_update`、`context_growth_update`；
- 每回合聚合后的 provider reasoning 对应一条 `devin_thoughts`（最多 4000 字符）；
- 工具调用的 `<tool>_started` / `<tool>_completed`，完成事件只带参数 key、
  结果类型、字节数和成功标记，不复制原始敏感参数；
- `ToolExecutor::execute_streaming` 可选流式入口；engine 对输出按每次最多
  2000 字符、每次调用最多 64 条做限流，并持久化 `terminal_update`；
- 本地 Tauri `run_shell` 通过 host process 增量读取输出并转发
  `terminal_update`；远程 RVM 路径保持远端原生执行，不修改 host；
- provider usage 存在时的 `iteration_stats`，包括工具数量、耗时和 token 数；
- 本地 `session_worklog` 现在从 audit store 返回这些事件，沿用现有 Worklog
  时间线，不新增 UI 布局；Transcript 对 `devin_thoughts` 和
  `simple_activity_update` 沿用已有 thinking/notice 表面。

真实 Devin 事件样本的字段形状已依据
`/home/ubuntu/devin_session_events_full.txt` 核对，覆盖 status、shell、file、
search、mcp、todo、lifecycle、reasoning、iteration 和 context 事件。新增
engine example 的确定性事件断言、workspace 相关测试和 clippy 已通过；本地浏览器 UI
TypeScript、production build 和 format check 已通过。

### 尚未等价或未核实

- 通用 executor 的默认 streaming 实现仍回退到 `execute`，因此只有实现该可选
  入口的 executor 能提供真实增量；本地 DesktopExecutor 已实现，远程 RVM
  仍使用远端原生 worklog/执行流。
- MCP、search、git 和 todo 的 category 已在 fake engine/store/executor E2E 中
  分别覆盖并断言 started/completed 成对；尚未用真实模型分别触发每个类别。
- 远程 RVM worklog 仍使用远端原生事件；本次改动只补齐本地 builtin engine，
  没有改变 RVM host。
- Devin 的 `one_line_thoughts` 尚未单独生成；当前仅提供聚合的
  `devin_thoughts`。
