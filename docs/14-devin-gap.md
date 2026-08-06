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

### 6.1 PR #91 / 七轮真实验证后的状态

以下行为已在真实 Tauri GUI、Local host、真实 gateway `glm-5.2` 上完成验证，
覆盖七轮 benchmark，并核对 live、重新读取和冷启动结果：

| 差距 | 当前状态 | 核实结果 |
|---|---|---|
| working events 到达 timeline | **已关闭** | canonical envelope 含 `type`、`event_id`、`created_at_ms`；631 个事件 envelope 完整。 |
| Devin-style 工作行 | **已关闭** | shell、Created、Edited（精确行数）、thoughts 和 `Worked for Xm Ys` 均可见。 |
| task rows | **已关闭** | 每个 plan 只显示一次 `Created N Tasks`，后续显示 `k/n#i <task>`；读取真实 `PlanRecord.steps`。 |
| compaction 行 | **已关闭** | `Earlier context compacted` 出现在 work group 内。 |
| 空 assistant / 空 work group | **已关闭** | 空 artifact 已移除；有行但耗时为 0 的合法 work group 仍保留。 |
| model-aware context / output limits | **已关闭** | 每字段按 gateway → matrix → probe → learned → user → assumed；`glm-5.2` 为 1M，来源写入 `context_growth_update`。 |
| control slash commands | **已关闭** | `/compact` 等作为 backend action 执行，输出可见且持久化。 |
| steering | **已关闭** | steer 作为 canonical `user_message` 持久化并渲染为 user bubble。 |
| live / re-read / cold restart parity | **已关闭** | 95-node row list byte-identical；真实会话可冷启动复现。 |

以下仍是下一批，不应写成已等价：

| 未关闭差距 | 当前状态 |
|---|---|
| attachments / artifacts timeline | **部分关闭**：截图和文件改动 diff 已作为 per-session artifacts 持久化并可在 timeline / artifact rail 按需展开；录屏、citation snippets 仍开放。截图 artifact 当前不会作为视觉输入发送给模型。 |
| terminal replay / `terminal_update` panel | **部分关闭**：`terminal_update` 已按 `call_id` 聚合到 timeline 的 shell 行下，并可展开查看；Shell rail 同样读取 canonical `session_events`。仍不提供 PTY 语义、ANSI 光标/控制序列或终端状态重放。 |
| iteration stats surfacing | **部分关闭**：canonical `iteration_stats` / `iteration_checkpoint` 仍被 timeline 忽略，但 Info pane 从持久化事件计算会话汇总和可折叠的逐轮 timing/tool 明细；更丰富的 Devin context/source/tool aggregates 仍开放。 |
| right rail Shell / Desktop / Web IDE panes | **未核实**：七轮没有 RVM token，因此没有真实远端 pane 验证。 |

### 已实现并已通过本地验证

Builtin engine 现在会把 working 过程作为结构化事件同时写入本地 audit store，
并通过既有 `opcos://event` 的 `stream` payload 向前端转发。事件具有：

- `event_type`、`category`、`direction`、`timestamp` 和结构化 `payload`；
- 每轮的 `status_update`、`simple_activity_update`、`context_growth_update`；
- 每回合聚合后的 provider reasoning 对应一条 `devin_thoughts`（最多 4000 字符）；
  payload 同时带 `thinking_duration_ms`；
- 工具调用的 `<tool>_started` / `<tool>_completed`，完成事件只带参数 key、
  结果类型、字节数和成功标记，不复制原始敏感参数；
- `ToolExecutor::execute_streaming` 可选流式入口；engine 对输出按每次最多
  2000 字符、每次调用最多 64 条做限流，并持久化 `terminal_update`；
- 本地 Tauri `run_shell` 通过 host process 增量读取输出并转发
  `terminal_update`；远程 RVM 路径保持远端原生执行，不修改 host；
- 每轮 `iteration_stats` 包括总耗时、provider streaming inference、工具执行、
  harness 剩余时间、可观测 context-overflow retry 次数、自动 compaction 次数和
  token 数；`iteration_checkpoint` 在能从 canonical incoming event 建立边界时带
  `last_processed_incoming_event_id`；
- timing 边界是从该轮 harness 开始到工具执行完成：`inference_ms` 包含
  `stream_turn` 的完整流式消费，`tool_exec_ms` 包含 `execute_tools` 的聚合耗时，
  `harness_ms = max(duration_ms - inference_ms - tool_exec_ms, 0)`；provider 内部
  未暴露的 retry 不计入 `retry_count`。
- 本地 `session_worklog` 现在从 audit store 返回这些事件，沿用现有 Worklog
  时间线，不新增 UI 布局；Transcript 渲染 `devin_thoughts`，但
  `simple_activity_update` / `status_update` 在 `web/src/timeline.ts:235-250`
  被作为控制事件忽略，尚无 Devin 式 status pill。
- `plan_update` / `plan_revise` 完成后发 `todo_update`，payload 是完整本地
  plan snapshot；pending question resolution 后发 `user_question_answered`。
- 本地资产注入保留 `note_used`，并额外为 rules/active skills 发
  `rules_injected` / `skill_activated`；不复制资产正文。
- compaction 完成后发本地 `session_snapshot`，正常 turn 收束发
  `iteration_checkpoint`，pending/restart recovery 发 `resuming_session`。
- Info pane 从 canonical `session_events` 重读 iteration stats，不改变 timeline
  行数；旧事件缺少拆分字段时显示 Unknown 而不是伪造 0ms。`usage_events` 保持
  兼容的旧 schema，统计字段只从 canonical iteration events 计算。
- 对照完整 Devin stream，`terminal_update` payload 使用 `contents` 而非
  `chunk`；OPCOS 保留 `call_id` 作为本地工具关联键，当前没有通用真实
  `shell_id` 或 gzip transport，因此不伪造这两个字段。

Artifacts / attachments 这一批已完成的范围是：

- 显式 computer-use / browser screenshot 结果会写入
  `<app_config_dir>/artifacts/<session_id>/<artifact_id>`，并注册现有
  `artifacts` 表；事件只保存 artifact id，不把图片 bytes 或 base64 内联进
  `session_events.event_json`。
- 截图事件使用 Devin 对齐的 `computer_use` 类型，payload 带
  `screenshot_keys: [artifact_id]`；截图 artifact 的单文件上限为 8 MiB。
- `write_file` / `edit_file` 的 `multi_edit_result.file_updates[]` 带 diff
  artifact id；diff 超过 5000 行，或 old/new 行数乘积超过约 4,000,000
  个 LCS 单元格时不生成 diff artifact，但仍保留
  `lines_added` / `lines_removed` 统计。
- timeline 和 artifact rail 只在用户展开/打开时读取 artifact 内容；图片以
  base64 data URL 展示，diff 的新增/删除行有区别显示。
- 当前没有自动 artifact 回收；目录按 session 分隔，可手工删除。预期单个
  session 的截图数量约等于进入工具 transcript 的显式 screenshot 结果数；
  computer-use loop 内部仅用于 before/after 验证的中间帧不持久化。
- 截图虽然可作为 artifact 查看，但当前不会转换成 provider 的
  `type: "image"` 输入 block，因此尚未进入模型的视觉输入；这是明确的后续
  open gap。OPCOS 仍没有 Devin `recording_stopped.clean_video_url` 或
  `citation_snippet.data.file_content` 对应能力。

Terminal replay 的当前边界：

- shell 输出以 `terminal_update` 增量事件持久化，前端按 `call_id` 按序拼接，
  作为对应 shell 命令行下的可展开输出，不为每个 chunk 生成独立 timeline 行。
- 每个 shell call 最多保留 64 个 chunk，每个 chunk 最多 2000 个字符；超出
  任一限制时会追加 `{"truncated": true}` 的收尾 `terminal_update`，timeline
  和 Shell rail 都显示 `Output truncated`。
- 旧事件没有 chunk 序号时仍可渲染。当前顺序依赖 canonical event 的
  `created_at_ms` + store `sequence` 排序；同一毫秒内由数据库 sequence 保持
  插入顺序，因此本批不新增序号字段。
- 当前没有 PTY、ANSI 控制序列/光标重放、终端尺寸或完整终端状态 replay；
  LocalHost 的 `pty` surface 仍不可用，右栏 PTY surface 保持 open gap。

真实 Devin 事件样本的字段形状已依据本次交付的权威输入
`/home/ubuntu/opcos-test/devin-events.txt` 和
`/home/ubuntu/opcos-test/devin-event-shapes.txt` 核对，覆盖 status、shell、file、
search、mcp、todo、lifecycle、reasoning、iteration 和 context 事件。下面的差距
表只比较本地 builtin engine 实际写入的 working event 与本地 timeline 实际读取的
字段，不把远端 RVM 原生 worklog 或模型 provider usage 当成本地事件。

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
- Devin 的 `computer_use`、subagent、test-mode、recording、sidekick、
  suspend/resume 控制面事件没有对应 OPCOS 子系统；除本地恢复生命周期
  `resuming_session` 外暂不伪造。
- PR 事件（`pr_created`、`pr_comment`、`pr_merge_conflict`）尚未增加专用
  集成事件；现有 git/PR 工具仍通过工具生命周期事件记录。

## 4. Devin event stream 对照（本地代码证据版）

### 4.1 OPCOS 实际发出的 working-event vocabulary

working event 的共同 envelope 在 `crates/opcos-engine/src/lib.rs:3103-3138`
创建，字段为 `event_type`、`category`、`direction`、`timestamp`、`payload`。
本地 Tauri 的资产注入在 `src-tauri/src/main.rs:9959-10009` 直接写同一 envelope。
按代码路径，OPCOS 当前可能发出以下类型：

| OPCOS 类型 | category | 当前 payload 字段 |
|---|---|---|
| `user_message` | message | `message`，可选 `source`；incoming |
| `status_update` | status | `enum`、`message` |
| `simple_activity_update` | status | `enum`、`iteration` |
| `context_growth_update` | other | `estimated_context_tokens`、`current_context_bytes`、`iteration_count`、`resolved_context_window`、`context_window_source` |
| `devin_thoughts` | other | `message`、`thinking_duration_ms` |
| `devin_message` | message | `message`、`tool_calls` |
| `iteration_stats` | other | `iteration`、`num_tool_calls`、`duration_ms`、`inference_ms`、`tool_exec_ms`、`harness_ms`、`retry_count`、`compaction_count`；有 provider usage 时另有 `input_tokens`、`output_tokens` |
| `iteration_checkpoint` | lifecycle | `iteration`、`num_tool_calls`，可选 `last_processed_incoming_event_id` |
| `session_snapshot` | lifecycle | `compaction_id`、`summary_chars`、`retained_messages` |
| `resuming_session` | lifecycle | `resume_reason` |
| `iteration_checkpoint` | lifecycle | `iteration`、`num_tool_calls`，可选 `last_processed_incoming_event_id`；在 Transcript timeline 中被显式忽略 |
| `shell_process_started` | shell | `call_id`、`command`、`starting_dir` |
| `terminal_update` | shell | `call_id`、`contents`、收尾时 `truncated`、`total_bytes` |
| `shell_process_completed` | shell | `process_id`、`exit_code`、`output_trunc` |
| `multi_edit_result` | file | `file_updates[]`：`file_path`、`action_type`、`start_line`、`end_line`、`lines_added`、`lines_removed`、可选 `artifact_id` |
| `computer_use` | computer_use | `call_id`、`screenshot_keys[]` |
| `todo_update` | todo | 当前完整 plan snapshot：`id/title/summary/status/revision/steps[]` 等 plan 字段 |
| `ask_user_pending` | message | `call_id`、`tool`、`options`、`allow_multiple` |
| `approval_pending` | message | `call_id`、`tool`、`arguments` |
| `user_question_answered` | message | `call_id`、`question_type`、`answer_type` |
| `note_used` | other | `knowledge_count`、`skills_count`、`commands_count` |
| `rules_injected` | other | `path` |
| `skill_activated` | other | `name`、`path` |
| `error`、`interrupted`、`usage_limit`、`model_switch`、`compacted` | notice | 通用 `message`；`compacted` 另有 `source`，`usage_limit` 由 message 携带限制值 |
| `compaction_summary_invalid` | notice | `message`、`reason`、`diagnostics` |
| `<tool>_started` | tool category | 除 `run_shell` 外，`call_id`、`tool`、`argument_keys` |
| `<tool>_completed` | tool category | 除 `run_shell` 外，`call_id`、`tool`、`ok`、`result_type`、`result_bytes` |

`<tool>` 是 builtin tool、已启用的 MCP qualified tool 或 coordination/action-ledger/
work-queue tool；具体目录见 `crates/opcos-engine/src/lib.rs:3847-4055`。`run_shell`
是特例，只发 `shell_process_started` / `shell_process_completed` 和
`terminal_update`，不发通用 `run_shell_started` / `run_shell_completed`。事件类型
列表中还会出现由 `notice()` 传入的错误/状态 kind，以及 src-tauri 控制面传入的
`mode_current`、`mode_changed`、`model_current`、`session_list`、`slash_help` 等
notice 类型；它们共用 `message`/可选 `payload`，不是 Devin 的 shell/file vocabulary。

### 4.2 OPCOS 实际消费与渲染的类型

`web/src/timeline.ts:235-250` 明确丢弃
`iteration_stats`、`context_growth_update`、`simple_activity_update`、
`status_update`、`session_snapshot`、`iteration_checkpoint`、`turn`、
`tool_result`、transient delta 和 `stream_reset`。实际生成 Transcript work rows
的分支在 `web/src/timeline.ts:262-404`：

- 渲染：`user_message` / `initial_user_message`、`devin_message`、审批/问题事件、
  `compacted`、`devin_thoughts`、`shell_process_started`、`terminal_update`、
  `multi_edit_result`、`computer_use`、`todo_update`、`read_file_completed`、
  `list_dir_completed`，以及所有未排除的 `*_started` 的通用标签。
- 通用但信息损失：`note_used`、`rules_injected`、`skill_activated` 和未特别处理的
  动态 tool started 只落成 event type/command 标签；动态 completed 通常没有 row。
- 已发但 Timeline 不展示：`shell_process_completed`、`user_question_answered`、
  `approval_pending`（在 pending 时会先被专门事件处理，但普通审批生命周期的
  completed 信息仍不形成 row）、`iteration_stats`、`context_growth_update`、
  `simple_activity_update`、`status_update`、`iteration_checkpoint`、
  `session_snapshot`、`resuming_session`、`tool_result`、大部分
  `<tool>_completed`。`iteration_stats` 另由 `web/src/iterationStats.ts:63-66`
  提供 Info pane 的 timing 汇总，不是 Transcript row。
- Shell history (`src-tauri/src/main.rs:18450-18520`) 只重建
  `terminal_update` 的 `call_id/contents/truncated`，不读取 `total_bytes`、
  `shell_id`、`process_id` 或 background 状态；右栏 Worklog
  (`web/src/App.tsx:3237-3343`) 则把所有 working payload 当作通用字段表，不理解
  Devin 的类型语义。

### 4.3 Devin 类型 → OPCOS 类型映射

| Devin type | OPCOS type / verdict | 结论 |
|---|---|---|
| `terminal_update` | `terminal_update` | **部分**：Devin `contents` 是 base64 原始 bytes；OPCOS 是 plain text `contents`，按 Rust `&str` 和前端 `String` 传递，不能保留 CRLF/ANSI/二进制。OPCOS 也缺少 `shell_id`/`process_id`。 |
| `shell_process_started` | `shell_process_started` | **部分**：都有 command/starting directory，但 OPCOS 只有 `call_id`，缺稳定短 `shell_id`、`process_id`、`acu_consumption`、`is_major_action`。 |
| `shell_process_completed` | `shell_process_completed` | **部分**：都有 exit/output 摘要，但 OPCOS 没有 `shell_id`、`timestamp` 级 process identity，也没有明确 total/output encoding 语义。 |
| `shell_process_completed_background` | 无 | **缺失**：OPCOS 的 `background_job_*` 工具生命周期不是 Devin 的 shell background completion event，Timeline 没有 background shell 行语义。 |
| `devin_thoughts` | `devin_thoughts` | **部分**：message/timing 已有；Devin stream 中与 work group 配套的 `one_line_thoughts` 仍无。 |
| `one_line_thoughts` | 无 | **缺失**：没有 `short` + `summary` 的工作组标题/活动标签。 |
| `simple_activity_update` | `simple_activity_update` | **部分**：`enum: deciding_action` 已发，但 Timeline 明确忽略，Transcript 没有 Devin 式状态 pill。 |
| `context_growth_update` | `context_growth_update` | **部分**：有粗略 `current_context_bytes`/估算 tokens/窗口来源；缺 `per_source_context_bytes`、`tool_aggregates`、total output/invocation/image bytes、main-chain growth。 |
| `iteration_stats` | `iteration_stats` | **等价（语义）**：timing 字段已覆盖 Devin 的 `total_ms`/`harness_ms`/`inference_ms`/`tool_exec_ms`/`num_tool_calls`；**偏离**：OPCOS 仍可附带 provider `input_tokens`/`output_tokens`，Devin representative payload 没有 token 字段。 |
| `multi_edit_result` | `multi_edit_result` | **部分**：有 file path/action/line range/diff artifact；缺 Devin `total_lines` 和 `contents_key` file snapshot，`action_type` 目前用 create/edit 而非 open/edit。 |
| `search_file_commands` | `<search_tool>_started` / `<search_tool>_completed` | **部分**：有 search category 和 argument keys，但不记录实际 regex/path，也没有 `search_commands[]` 与 `search_result_filenames[]` 的稳定事件。 |
| `todo_update` | `todo_update` | **部分**：有完整 plan steps/status/revision；缺 `total_count`、`pending_count`、`completed_count`、`in_progress_count` 和 `subagent_id`。 |
| `subagent_started` | `coordination_dispatch_started`（若工具被调用） | **部分**：Leader/Worker coordination 已有 dispatch/status 工具，但没有 inline task/title/profile/agent identity 的 Devin event。 |
| `subagent_finished` | `coordination_status_completed`（若工具被调用） | **部分**：状态可查询但不是 timeline inline 的 success/summary/finish event，且 Worker 自述不等于完成证据。 |
| `live_chain_update` | 无 | **缺失**：没有实时链路/主链 oid 事件。 |
| `simple_activity_update` / `status` | `status_update` / `simple_activity_update` | **部分**：状态事件有，但工作组标题仍只有按时长生成的 `Worked for ...`。 |
| `mcp_tool_call_started` | `<qualified_mcp>_started` | **部分**：类别和 started/completed 生命周期存在；缺 Devin 独立 MCP event payload/服务标识语义，Timeline 也不专门渲染。 |
| `mcp_tool_call` | `<qualified_mcp>_completed` / `tool_result` | **部分**：结果可持久化，但没有 Devin 的 MCP-specific call shape；completed row 不展示。 |
| `web_search` | 无 | **缺失**：OPCOS 的 browser/search/repo-index 工具没有 Devin `web_search` 类型和专用结果字段。 |
| `git_view_pr` | `github_get_pull_request_started/completed` | **部分**：GitHub PR read tool 有生命周期，但没有 Devin 专用 `git_view_pr` payload/major-action 语义。 |
| `skill_activated` | `skill_activated` | **部分**：本地 name/path 已有；没有 Devin stream 中更丰富的 activation context。 |
| `note_used` | `note_used` | **部分**：本地只记录知识/技能/命令数量，不记录单个 note/source。 |
| `environment_config_suggestion` | 无 | **缺失**：没有环境配置建议事件。 |
| `repo_setup_initialized` | 无 | **缺失**：没有仓库 setup 初始化事件。 |
| `scripted_tools_started` | 无 | **缺失**：没有 scripted-tools 控制面事件。 |
| `initialized` / `status_update` | `status_update` / notice | **部分**：有 generic working status，但没有 Devin 初始化 payload。 |
| `computer_use` | `computer_use` | **部分**：截图 artifact key 已有；缺 Devin 事件中更完整的动作/输入/验证过程语义，且目前不是 provider image input。 |
| `recording_started` / `recording_stopped` | 无 | **缺失**：没有 recording 控制面或 `clean_video_url`。 |
| `test_mode` (`enter_test_mode` / `exit_test_mode`) | 无 | **缺失**：没有 Devin test-mode event。 |
| `sidekick_stopped` | 无 | **缺失**：没有 Sidekick 生命周期事件。 |
| `auto_route_decision` | 无 | **缺失**：模型/provider routing 选择不进 working event。 |
| `acu_consumption_at_last_user_interaction` | 无 | **缺失**：OPCOS 没有 ACU 字段或计量事件。 |

### 4.4 按用户可见影响排序的 gap backlog

| 优先级 / gap | 证据（Devin 字段 + OPCOS 代码路径） | 用户可见影响 | 努力 |
|---|---|---|---|
| P0 原始 terminal bytes 与 stable shell identity | Devin `terminal_update.contents` 是 base64 raw bytes；OPCOS 在 `crates/opcos-engine/src/lib.rs:2336-2352` 写字符串，`web/src/timeline.ts:284-304` 用 `String` 拼接；started 只有 `call_id`（engine:2047-2055） | CRLF、ANSI、非 UTF-8 输出在 timeline/历史中损坏；并发/后台 shell 无法可靠归属，终端 replay 与真实命令不一致 | L |
| P0 shell lifecycle/background 语义 | Devin 有 `shell_id`+`process_id`、`starting_dir`、`shell_process_completed_background`；OPCOS 只有 `call_id`/`process_id` 且完成事件统一为 `shell_process_completed`（engine:2047-2055、2541-2549） | 用户看不到同一 shell 的连续身份，也无法区分后台进程和已完成前台命令；timeline collapse 不稳定 | M |
| P0 one-line work-group labels 与 major/minor | Devin 高频 `one_line_thoughts.short/summary`、`simple_activity_update.enum`，major events 带 `is_major_action`/`acu_consumption`；OPCOS only time label `Worked for ...`（`web/src/timeline.ts:252-266`），simple activity 在 `:235-250` 被丢弃 | 工作组标题无法像 Devin 一样概括“Listing files / Editing file”，用户只能展开 generic rows，长 session 可读性明显下降 | M |
| P1 context composition bar | Devin `current_context_bytes/tokens`、`per_source_context_bytes`、`tool_aggregates`；OPCOS context event 只有粗略 message JSON bytes 和窗口来源（engine:1493-1511） | context bar 不能解释增长来自 system/knowledge/tool output，provider token swings 无法用本地事实校准 | L |
| P1 file editor snapshot pane | Devin `file_updates[]` 有 `action_type`、line range、`total_lines`、`contents_key`；OPCOS 只有 diff `artifact_id`（engine:2433-2445，`web/src/timeline.ts:306-335`） | 用户能看 diff，但不能打开“编辑后的完整文件快照”，也缺 open/edit 语义和总行数 | M |
| P1 search activity fidelity | Devin 记录真实 regex/path 和 result filenames；OPCOS started 只保留 `argument_keys`（engine:2058-2067），Timeline 只生成通用 label | 用户无法从 timeline 回看“搜了什么、命中了哪些文件”，搜索活动不可审计 | M |
| P1 todo summary and delegation identity | Devin todo 有 full array + counts + `subagent_id`；OPCOS plan snapshot 没 counts/subagent（engine:2561-2567，`web/src/timeline.ts:351-384`） | 大计划的进度 pill 和 delegated work 归属不清；Lead/Worker 的贡献不能在 timeline 内联理解 | M |
| P1 subagent inline lifecycle | Devin `subagent_started/finished` 有 task/title/profile/success/summary；OPCOS coordination 只通过 tool started/completed/status，且完成需 branch/push/PR 核验（`crates/opcos-engine/src/lib.rs:3847-4055`） | 用户看不到“谁在做什么、是否成功、摘要是什么”的连续 delegated work row | M |
| P2 dedicated MCP and connector rows | Devin 有 `mcp_tool_call_started`/`mcp_tool_call`；OPCOS dynamic qualified MCP events 是 generic tool lifecycle，Timeline 不专门渲染 | MCP/connector 活动在长工作组中难以区分，调试成本上升 | S |
| P2 lifecycle/control-plane vocabulary | Devin `live_chain_update`、recording、test mode、sidekick、route/ACU/init events；OPCOS 只有部分 local recovery/lifecycle events（engine:1837、2837-2846；src-tauri:9959-10009） | 高级运行状态、录制和 delegated control 不可见；对普通 coding timeline 影响较低 | L |

推荐先做 P0：先改 terminal bytes/identity 和 major/minor grouping，再做 context/file/search/todo
的结构化 payload。不要为缺失的 Devin Cloud 控制面事件伪造字段；只有 OPCOS 具有真实来源
（本地 context composition、artifact snapshot、search result、coordination state）时才新增事件。
