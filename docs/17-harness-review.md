# OPCOS harness 评审、评分与优化计划

对照三份外部资料评审 OPCOS 现有 harness，给出逐维度评分和优化计划。

外部资料：

1. [deusyu/harness-engineering](https://github.com/deusyu/harness-engineering)（含 Anthropic/OpenAI 文章译档、Claude Code 逆向档案、Ralph orchestrator 笔记）
2. [lopopolo/harness-engineering](https://github.com/lopopolo/harness-engineering)（whole-job / just-in-time-context / tool-legibility / authority / feedback / effectiveness / evals）
3. [awesome-cc-harness](https://wanlanglin.github.io/-awesome-cc-harness/)（Claude Code 实现摘要，页面级摘要，非源码审计）

## 0. 读法与证据边界

- 「OPCOS 现状」一栏只写从当前 `dev` + 在审 MCP/steering 分支源码读到的事实，带文件位置。
- 「外部主张」一栏是资料的可检验主张，不是普适真理；三份资料内部有互相冲突之处，见 §4。
- 评分是本文作者按下述 rubric 的判断，不是外部资料给出的结论，也不是基准测试结果。任何标注「推断」的行判断没有运行时证据支撑。
- WanLanglin 页面提供的数字（26 hook events、43+ tools、5 permission modes 等）是页面自述，本文未独立审计，只当作设计方向参考。

评分 rubric（每维度 0–10）：

- 0–3：能力缺失或只有零散实现。
- 4–6：能力存在但形状不完整，模型或用户需要靠外部知识补齐。
- 7–8：能力完整且有测试，缺口是明确、有界的。
- 9–10：能力完整、有测试、且有运行时证据表明它在真实轨迹里起作用。

没有一项给到 9–10：OPCOS 目前缺少 trajectory 级评测（§3.13），因此没有任何维度具备「在真实轨迹里起作用」的量化证据。

## 1. 总分

| # | 维度 | 分数 | 一句话判断 |
|---|---|---|---|
| 1 | Agent loop 与终止条件 | 7 | 边界齐全（max_iterations / chunk-idle / interrupt / stop hook veto），但终止条件是计数与超时，不是可观察的完成判据 |
| 2 | 上下文管理与压缩 | 6 | 压缩、learned window、overflow retry 都在，但上下文是「一次性前置」而非按需检索 |
| 3 | 工具可发现性（tool legibility, discover） | 4 | 约 70+ 个工具定义每轮全量注入，没有目录/搜索/按需 schema |
| 4 | 工具结果与错误可修复性 | 5 | 输出有界、secret 已擦除，但错误多为 `{"error": string}`，不含违反的不变量与修复路线 |
| 5 | 文件编辑接口 | 8 | 唯一匹配 + 原子多段替换 + 外部变更检测 + diff artifact |
| 6 | Shell / 后台任务 / 持久会话 | 8 | 持久 shell、后台 job、有界输出 + offset 取回；远端 cwd/streaming 仍缺 |
| 7 | 子 agent、团队与协调 | 6 | envelope/lease/circuit breaker 完整，但没有 fan-out barrier，也没有独立 evaluator |
| 8 | Plan / todo / 长期工作记录 | 8 | plan 有 revision 历史、failed 不可静默变 done、压缩后仍注入 |
| 9 | 权限、审批与凭据 | 7 | 5 种模式 + durable grant + preflight + external action record；authority 维度不全，高后果动作没有分阶段 |
| 10 | Hooks 与生命周期扩展点 | 6 | Pre/PostToolUse、Stop、PostCompaction 有；缺 PreCompact / SessionStart / SessionEnd / PostToolUseFailure / PermissionRequest |
| 11 | 可中断性、steering 与恢复 | 6 | 每个 iteration 边界注入 steering、崩溃后 reconcile；但故障原因不可区分，没有 resume-from-last-event |
| 12 | 成本、caching 与反馈速度 | 5 | usage/cache 计数与逐轮 timing 齐全，但没有 prompt-cache 控制，也没把「四个时钟」分开记 |
| 13 | 可观测性与 eval harness | 3 | 事件流与 timeline 很强，但没有 dataset/grader/trajectory runner 与失败分类 |
| 14 | Provider 可移植性 | 8 | provider-neutral 请求模型 + 三套 adapter + 窗口来源分级 |

合计 **87 / 140 ≈ 6.2 / 10**。

形状判断：OPCOS 在「执行面」（编辑、shell、权限、plan、provider 适配）已经接近成熟 harness；短板集中在「模型的认知面」（工具怎么被发现、错误怎么被修复、上下文怎么按需取）和「工程的验证面」（trajectory 评测、恢复语义）。

## 2. 逐维度评审

### 2.1 Agent loop 与终止条件 — 7

外部主张：

- 把整个工作交给一个主 owner，约束放在结果与边界上，不预先规定所有状态转换（lopopolo `docs/whole-job`：“Delegate an outcome at the highest level the harness can safely support…”）。
- 循环到可观察的停止条件成立，而不是固定轮数（deusyu Anthropic dynamic workflows 译档：“循环派生智能体直到满足停止条件（没有新发现、或日志里不再有错误），而不是跑固定的轮数。”）。
- 每种工作流形状都要有自己的 barrier / 判定器。

OPCOS 现状：

- `run_loop` 按 `0..max_iterations` 迭代，达到上限记 safety-limit notice 并返回 `MaxIterations`（`crates/opcos-engine/src/lib.rs:1671-1680`、`2044-2054`）。
- `finish_turn` 把结果映射成 `run_state`/`stop_reason`，覆盖成功、approval pending、interrupted、provider/context/store/tool/usage-limit 与重复 turn（`crates/opcos-engine/src/lib.rs:1132-1189`）。
- `Stop` lifecycle hook 可以否决「没有 tool call 的最终回答」，veto 上限 3 次（`crates/opcos-engine/src/lib.rs:1884-1922`）。
- chunk-idle watchdog 把「有传输字节但没有解析出 chunk」变成 `ProviderError::ChunkIdleTimeout`（`crates/opcos-engine/src/lib.rs:2139-2199`）。

差距：

- 终止判据是「模型不再发 tool call」+ 计数上限；仓库里唯一的可观察判据是 Stop hook（外部命令），没有内建的「gate 全绿 / 日志无错误 / 无新发现」这类判定器。
- `local_gate_record` / `local_gate_status` 已经持久化 gate 合约（`crates/opcos-engine/src/lib.rs:4270-4271`），但它是模型自报，没有被接进终止条件。

### 2.2 上下文管理与压缩 — 6

外部主张：

- 模型看到的是组装后的上下文；根目录指南应是地图和路由表，深层规则按需加载（lopopolo `docs/just-in-time-context`：“The active window receives only the slice needed for the current decision…”）。
- 磁盘是长期状态，context 只是工作集（“disk is an infinite context sink.”）。
- 压缩既是长轨迹的基础设施，也会丢掉「不要做 X」这类边界约束。
- 为某个模型加的 context reset 会在更强模型上变成死重，必须按模型重新验证。

OPCOS 现状：

- `should_compact` 用已解析 context window 的 75% 作为 budget；无 usage 时按 JSON 字节数 / 4 估算（`crates/opcos-engine/src/lib.rs:2090-2101`）。
- 压缩会重组 system sections、runtime context、persisted plan 和内建指令，并断言排除 stale system context（`crates/opcos-engine/src/lib.rs:2977-3210`、测试 `7437-7510`）。
- context overflow 时把真实上限记为 `learned` 来源并在同一 turn 内压缩重试（`crates/opcos-engine/src/lib.rs:2014-2043`）。
- window 来源分级：gateway → matrix → probe → learned → user → assumed（`crates/opcos-engine/src/lib.rs:2104-2127`）。

差距：

- 首条 system message 把 plan、runtime context 和全部系统指令合并前置（`crates/opcos-engine/src/lib.rs:2933-2949`），这是「一次性前置」形状，不是地图 + 按需检索。
- 磁盘侧已有 `repo_index_*` 与 learned skills 可作检索面，但没有「压缩后按路径重新取回约束」的显式机制：压缩保留什么完全取决于摘要质量。
- 没有 forked context / fresh subagent 作为压缩的替代路径（协调层能派 worker，但不是为上下文隔离设计的）。

### 2.3 工具可发现性 — 4（最大短板之一）

外部主张：

- 能力可用的条件是：能在需要时发现、识别适用、正确调用、理解有界结果、失败后恢复、验证真实效果（lopopolo `docs/tool-legibility`）。
- 工具描述应短且可发现，详细 schema 与示例在选择后加载（“Use progressive disclosure for both.”）。
- 100+ 工具时应做 semantic tool search，只注入相关定义（WanLanglin 页面摘要）。
- 暴露能闭合工作的最小接口（“Expose the smallest interface that closes the job.”）。

OPCOS 现状：

- `tool_definitions()` 一次性返回全部 builtin 定义，再追加 action ledger、work queue、external ingress 和 coordination 工具（`crates/opcos-engine/src/lib.rs:4242-4311`）。
- 其中包含 15 个 browser 工具、4 个 background job 工具、3 个 repo index、3 个 LSP、4 个 plan、3 个 skill、以及 linear/github/telegram/discord/slack/notion/gitlab/jira/stripe 连接器共约 20 个（`crates/opcos-engine/src/lib.rs:4244-4302`）。
- MCP 工具在此之上追加，数量取决于服务端（GitHub MCP 单独就是 44 个工具）。
- 唯一的收窄手段是 per-session 工具开关和 MCP include/exclude 过滤，属于用户配置，不是模型侧发现。

差距（推断，无 token 实测）：连接一个中等规模 MCP 服务端后，每次 provider 请求都要携带 100+ 个工具定义。这同时抬高每轮成本（无法靠 prompt cache 抵消可变部分）和选择错误率。缺的是：紧凑目录（name / purpose / input shape / result shape / first useful call）+ 按需取 schema。

### 2.4 工具结果与错误可修复性 — 5

外部主张：

- 每个结果都是下一步的上下文：成功安静、结构有界稳定；失败给出违反的不变量、受影响目标、已知修复动作、被省略证据的取回路径，必要时提供 dry-run 与 postcondition/side-effect receipt（lopopolo `docs/tool-legibility`）。
- 错误信息应直接给修复路线（deusyu `concepts/02-mechanical-enforcement`：“lint 错误信息 = 修复指令”）。

OPCOS 现状：

- 结果在写给模型前 strip 内部字段并过 secret scrubber（`crates/opcos-engine/src/lib.rs:2803-2838`）。
- 输出有界：shell 事件截断到 4000 字符并带总字节数（`crates/opcos-engine/src/lib.rs:2855-2875`）；后台 job 输出可按 `offset/limit/tail` 取回（`crates/opcos-engine/src/lib.rs:4250`）；`repo_index_search` 最多 100 行并带 total marker（`src-tauri/src/main.rs:2354-2393`）。
- `edit_file` 在失败时不应用任何变更，并校验替换数量、唯一性、重叠与外部文件变化（`src-tauri/src/main.rs:2485-2797`）。
- 远端不支持的能力返回显式错误而不是静默降级（`crates/opcos-engine/src/lib.rs:4280-4282`、`src-tauri/src/main.rs:9051`、`16278`）。

差距：

- 拒绝与失败的载荷是扁平字符串：`{"error": reason, "_opcos_not_executed": true}`、`{"error":"tool call denied by policy",...}`、`{"error":"tool call interrupted"}`（`crates/opcos-engine/src/lib.rs:2205-2232`、`2462-2530`）。模型拿不到机器可读的 code、违反的不变量、可执行的修复路线，也分不清「不该重试」与「换个参数重试」。
- 没有 dry-run 模式，也没有统一的 postcondition/receipt：写文件后的验证要靠模型自己再读一遍。

### 2.5 文件编辑接口 — 8

外部主张：编辑接口是模型行为边界；精确唯一替换减少无关重写，全文写入要求重建整个文件、可能覆盖无关内容。

OPCOS 现状：`edit_file` 要求每个 `old_string` 在原文中恰好匹配一次、原子应用、保留换行；`write_file` 的描述明确要求修改已有文件优先用 `edit_file`（`crates/opcos-engine/src/lib.rs:4245-4246`）。执行前读取旧内容以生成 file-change artifact（`crates/opcos-engine/src/lib.rs:2490-2512`）。

差距：没有 patch/diff 形态的编辑接口（适合大范围结构性改动与审查），也没有 dry-run。

### 2.6 Shell / 后台任务 / 持久会话 — 8

外部主张：shell 覆盖面高但反馈必须有界可追溯；完整命令应跑到真实结束、保留完整输出与真实 exit status、返回有界结果 + 取回路径；引入 background shell 会让模型不愿意等阻塞式构建，因此要分开记四个时钟。

OPCOS 现状：`run_shell` 走持久 shell object，支持 cwd、secret-name 注入与输出 redaction（`crates/opcos-engine/src/lib.rs:4247`、`src-tauri/src/main.rs:3801-3834`）；后台 job 有 start/status/output/kill 四件套且输出有界（`crates/opcos-engine/src/lib.rs:4248-4251`）；本地 GUI streaming 复用同一 persistent session 并保留 cwd/env/exit code。

差距：远端 RVM 上 `cd` 不跨调用保留、没有 `terminal_update` 流式输出（`todos.md` 已列为 P1）；四个时钟只记了 worker 侧 timing，没有 human attention 与 accepted outcome。

### 2.7 子 agent、团队与协调 — 6

外部主张：subagent 是上下文防火墙；fan-out 后要有 barrier 合成；一条规则一个 validator，再加 skeptic 复核；worker 自评有 self-preferential bias，应用独立 evaluator；每个 subagent 可按复杂度路由模型与隔离方式。

OPCOS 现状：coordination `Envelope`（version/taskId/from/to/kind/msgId/replyTo/payload，`[[COORD]]…[[/COORD]]` 编码）、Role 状态机、`BoardPhase`（Open/Claimed/AwaitingApproval/Paused/AwaitingAcceptance/Done）、24 小时 lease、`require_acceptance`、dispatch limit 8 + circuit breaker、message id 去重与拓扑校验（`crates/opcos-engine/src/orchestration.rs:10-346`、`src-tauri/src/main.rs:20432-20580`）。builtin agent 模板有 Lead/Code/Review/Test/DevOps（`src-tauri/src/main.rs:5660-5745`）。

差距：

- 没有 fan-out/barrier：plan step 是串行 dispatch，没有「等待 N 个分支完成后合成」的原语。
- `AwaitingAcceptance` 的验收方是 Lead 会话本身，`Review` 模板也在同一 harness 内；没有强制的独立 evaluator/skeptic 角色，self-preferential bias 未被结构性抑制。
- 没有 per-role 模型路由（模型是会话级设置，不是 dispatch 参数）。

### 2.8 Plan / todo / 长期工作记录 — 8

外部主张：计划/spec 应是可重读的长期记录（Markdown、rubric、引用材料）。同时存在明确冲突：Ralph 主张计划可丢弃、用信号 steer；lopopolo 主张用静态分析 + 阻塞式 review 让偷工减料无法合并，从而不需要计划。

OPCOS 现状：`propose_plan` 持久化有序 plan，Plan 模式下进入 approval pending 而不是直接执行（`crates/opcos-engine/src/lib.rs:2233-2312`、`4276`）；`plan_update` 的 status enum 为 not_started/in_progress/done/failed/abandoned，abandoned 需要 reason，failed 不能静默变 done；`plan_revise` 保留 revision 历史、不物理删除步骤（`4277-4279`）；plan 会被并入压缩后的 system message（`2933-2949`）。

差距：plan 是结构化 DB 记录，不是仓库里的 Markdown spec，跨会话/跨 agent 的可重读性弱于「spec as product」的形态。

### 2.9 权限、审批与凭据 — 7

外部主张：capability 与 authority 分离（authority = 哪个 identity、对哪个 resource、在哪个 environment、多长 lifetime、什么 approval/recovery 合约）；可逆工作给宽 envelope；高后果动作拆成 assess → prepare → canary → approve → cut over → verify/rollback；credential custody 放在 trajectory 之外。

OPCOS 现状：`PermissionMode` 有 Discuss/Plan/Interactive/Auto/Custom（`src-tauri/src/main.rs:8981-8999`），当前模式写进 provider 可见的 runtime context；工具按 Read/Search/GitRead/Write/Execute/External 分级，未知名默认 External（`crates/opcos-engine/src/lib.rs:3510-3566`）；durable grant 带 key/target/expiry（`2214-2226`）；executor 可返回 `PreflightDecision::NeedsUser`；`git_push`、`github_create_pull_request` 明确要求 approval 与 external action record；approval payload 做 redaction（`src-tauri/src/main.rs:4889-5175`）。凭据只按 `secret_names` 在执行边界注入，模型上下文里不出现值（`crates/opcos-engine/src/lib.rs:4247`）——这一条与外部主张一致。

差距：

- grant 是 key + target + expiry，没有 environment（prod/staging）与 identity 维度，无法表达「同一个动作在 staging 可自动、在 prod 需分阶段」。
- 高后果动作是单点 approval，没有 canary → verify → cutover 的分阶段 authority；`local_gate_record` 是模型自报，不是授权前置条件。

### 2.10 Hooks 与生命周期扩展点 — 6

外部主张：hooks 覆盖 PreToolUse / PermissionRequest / PostToolUse / PostToolUseFailure / PreCompact / SessionStart / SessionEnd；成功静默、失败可行动。

OPCOS 现状：`PreToolUse`（可阻止，结果直接为 `{"error": reason}`）、`PostToolUse`（additional context 转下一条 user message）、`Stop`（可 veto ≤3 次）、`PostCompaction`（additional context 追加）；hook 受 `hook_permission_rules` 与 `ToolRisk::Execute` 决策约束，10 秒超时，输入经 redact，非法/空/超 64 KiB 的 stdout 被忽略（`crates/opcos-engine/src/lib.rs:772-935`、`1888-1922`）。

差距：缺 `PreCompact`（压缩前保住约束的最佳位置）、`SessionStart`/`SessionEnd`（注入/归档长期状态）、`PostToolUseFailure`（把重复失败升级为基础设施）、`PermissionRequest`。

### 2.11 可中断性、steering 与恢复 — 6

外部主张：steering 是信号不是脚本；harness bug、事件丢包、容器下线呈现同一症状，必须可区分；session 是追加写事件日志，harness 崩溃后用 `wake(sessionId)` + `getSession(id)` 从最后事件恢复。

OPCOS 现状：`queue_steering` 追加 source=`steering` 的 user message 并入队，`run_loop` 在每个 iteration 边界注入并发出 `steering_received`/`steering_applied`（在审分支）；`interrupt()` 通过 notify 立刻打断流；chunk-idle watchdog 收敛卡死；启动时把 crash-orphaned `running` 会话 reconcile 成 `interrupted (interrupted_by_crash)`。

差距：

- 症状仍不可区分：provider 静默、host 掉线、harness bug 目前都表现为 timeout 或 provider error，没有分类字段。
- 没有 resume-from-last-event：崩溃后 turn 被判为 interrupted，模型不会从事件日志的最后一个事件继续，需要用户重发。

### 2.12 成本、caching 与反馈速度 — 5

外部主张：成本杠杆是正交的（auto-compaction、subagent、forked context、MCP tool search）；应分开记四个时钟（worker feedback latency、worker wall-clock、synchronous human attention、time to accepted outcome）；token 榜不是价值。

OPCOS 现状：统一 `TokenUsage` 记 input/output/cache_read/cache_write（`crates/opcos-provider/src/lib.rs:25-37`）；OpenAI adapter 解析 `cached_tokens` 计入 cache_read，Bedrock 固定 0（`openai.rs:246-265`、`bedrock.rs:674-681`）；逐 iteration stats 含 tool calls、inference/tool/harness timing、retry/compaction 计数（`crates/opcos-engine/src/lib.rs:2057-2078`）。

差距：

- 只读不写：没有 prompt-cache 控制面（Anthropic `cache_control` 断点、稳定前缀排序），cache_read 目前是运气而不是设计。
- 四个时钟只有 worker 侧两个；human attention（approval 等待时长）与 time-to-accepted-outcome（AwaitingAcceptance → Done）虽然数据都在库里，但没有被聚合成指标。

### 2.13 可观测性与 eval harness — 3（最大短板）

外部主张：outcome / proof / architecture / trajectory cost 分开测；先人工审 20–50 条真实轨迹再自动化；Run / Trace / Thread 三层评测，多数团队从 Trace 开始；数据集要含正例与反例；失败要按 prompt / tool design / model limitation / tool failure / data gap 分类。

OPCOS 现状：working event 与 stream event 覆盖面很宽（`turn_finished`、`devin_message`、`tool_call_denied`、`approval_pending`、`${tool}_completed`、`shell_process_completed`、`tool_result`、context growth 等），timeline 按 event id 去重并分组（`web/src/timeline.ts:373-866`）；Rust 与前端单测覆盖 stop veto 上限、interrupted stream、chunk-idle、compaction、context overflow、steering、provider message 重建、orchestration envelope。

差距：本次读取的仓库里没有独立于单测之外、带 dataset / grader / trajectory runner 的 eval harness（`crates/opcos-engine/src/lib.rs`、`src-tauri/src/main.rs`、`web/src/*.test.ts`）。也就是说：所有 harness 改动目前只能靠单测 + 人工 UI 验证，没有回归轨迹集，没有失败分类，也没法回答「这次改动让完成率变好还是变差」。这直接限制了本文其它每一项的分数上限。

### 2.14 Provider 可移植性 — 8

外部主张：session / harness / sandbox 用小而稳定的接口连接，接口形状固定、背后实现可替换；adapter 替换时保留 action name、参数/结果、错误、approval、cancellation、lifecycle 语义；harness workaround 会随模型变化过时。

OPCOS 现状：`ProviderRequest`/`AssistantTurn`/`TokenUsage` 是 provider-neutral（`crates/opcos-provider/src/lib.rs:39-105`）；Anthropic/OpenAI/Bedrock 三套 adapter 各自映射 tool_use/tool_result 且对不支持形状返回显式 Protocol error（`bedrock.rs:409-554`）；`opcos-rvm` 不依赖 `opcos-engine`、`opcos-engine` 不依赖 Tauri（AGENTS.md 分层约束）。

差距：`ProviderDialect` 只有 OpenAi/Cloudflare 两个方言分支；换模型后重测 harness 假设的机制不存在（同样受 §2.13 限制）。

## 3. 优化计划

排序依据：先补「让模型能正确工作」的认知面，再补「让我们能证明改动有效」的验证面，最后是 authority 与协调形状。每项都写明外部依据、改动面与验收方式。

### P0-1 结构化工具错误信封（对应 §2.4，任务 #25）

- 依据：“Errors should identify the violated invariant and the likely repair.”（lopopolo tool-legibility）+ “lint 错误信息 = 修复指令”（deusyu）。
- 形状：所有工具失败返回稳定结构 `{"error": {"code", "invariant", "target", "repair", "retry": "no|same|adjusted", "retrieval"}}`，同时保留 `error` 的人类可读摘要以兼容现有 timeline 与测试。
- 覆盖：preflight denial、policy denial、interrupt、path rejection、`edit_file` 各类校验失败、远端 unsupported、host I/O、MCP transport/auth 失败。
- 验收：engine 单测断言每个失败分支的 code/repair 非空；`_opcos_*` 内部字段仍被 strip；scrubber 仍生效；timeline 显示摘要而非整包 JSON。
- 当前分类是跨 `ToolExecutor` 字符串边界的启发式规则；长期应让 `ToolExecutor` 错误类型直接携带 code（后续项，本次不做）。

### P0-2 渐进式工具披露 + 工具目录检索（对应 §2.3，任务 #26）

- 依据：“Use progressive disclosure for both. Advertise what and why compactly. Load detailed schemas… after selection.”（lopopolo）+ “100+ tools? → Semantic Tool Search → Only inject relevant defs”（WanLanglin 摘要）。
- 形状：保留高频核心工具全量注入（file/shell/plan/search/ask_user）；把 browser 15 件套、连接器约 20 件套和 MCP 工具改为目录条目（name / purpose / input shape / first useful call），新增 `tool_search(query)` 与 `tool_describe(name)` 取完整 schema；被 describe 过的工具在本 turn 内保持可直接调用。
- 风险与缓解：这会改变模型可见工具集，必须先有 P0-3 的轨迹回归；因此实现顺序为 P0-1 → P0-3 骨架 → P0-2，且由会话设置控制开关，默认保持现状直到轨迹集显示无退化。
- 验收：同一任务集下每轮工具定义 token 数下降且完成率不下降（需 P0-3 提供度量）。

### P0-3 Trajectory eval harness（对应 §2.13，任务 #27）

- 依据：Run / Trace / Thread 三层；正例 + 反例；失败分类；“A pull request, line count, plan, token total… None establishes value on its own.”
- 形状：`crates/opcos-eval`（或 `crates/opcos-engine/tests/trajectories`）提供 case 定义（初始 workspace fixture、prompt、脚本化 provider 响应或录制回放）、runner、grader（终态断言 + 事件断言 + 成本记录）与失败分类枚举（prompt / tool design / model limitation / tool failure / data gap）。
- 首批用例直接来自已修过的真实故障：嵌套目录写入、写失败不得产生 `Created` 行、工作区外写入必须拒绝、卡死 turn 必须收敛、运行中 steering 必须被消费、approval pending 后恢复、压缩后 plan 仍在 system message。
- 当前骨架验证的是 engine 侧编排不变量；工具行为由 fixture executor 替身提供，不等同于 `src-tauri` 的生产工具实现。要覆盖真实工具语义，后续需将 `src-tauri` 工具实现抽成独立 crate，本次不做。
- 验收：`cargo test -p opcos-eval` 在 CI 跑；每个 case 输出 outcome / proof / trajectory cost 三段结果。

### P0-4 分层失败轨迹导出

- `crates/opcos-trace` 将生产会话导出为 raw JSONL、确定性的 per-task analysis 和跨 run overview 三层产物；`src-tauri` 通过导出命令调用它。
- analysis 机械提取终态、工具序列、重复调用、iteration/token 统计和 P0-1 `error_details.code` 序列；签名的因果地位与 agent 机制维度保留为空位，后续由独立分析步骤填写，不在导出器中启发式猜测。
- 三层所有字段递归经过 scrubber；生产导出必须传入当前已知 secret 值做精确替换，启发式规则只作为第二层兜底。原始事件、分析报告和聚合总览都可直接作为模型输入或用户分享物。

### P1-4 恢复语义与故障分类（对应 §2.11）

- 依据：“harness 中的 bug、事件流中的丢包、容器下线，呈现出的都是同一个症状。”+ session log 之外恢复。
- 形状：给 provider/host/harness 失败加分类（`provider_silent` / `host_unavailable` / `harness_error` / `event_gap`）并写进 `stop_reason`；崩溃恢复时允许从事件日志最后一个事件继续当前 turn，而不是只标 interrupted。

### P1-5 Authority 模型（对应 §2.9）

- 依据：“Authority specifies which effect an identity may cause, to which resource, in which environment, for how long, and under what approval and recovery contract.” + assess → prepare → canary → approve → cut over → verify/rollback。
- 形状：durable grant 增加 identity（role）与 environment 维度；高后果动作（push 到保护分支、生产部署）声明 stage 序列，并把 `local_gate_status` 作为进入下一 stage 的前置条件而不是模型自述。

### P1-6 fan-out / barrier 与独立 evaluator（对应 §2.7）

- 依据：“合成步骤是一道屏障——它等待所有扇出的智能体完成”；“Claude 倾向于偏爱自己的结果或发现”。
- 形状：coordination 增加 fan-out group + barrier 语义（N 个 Claimed task 全部 Done 才解除）；`AwaitingAcceptance` 的验收方可配置为独立 evaluator 角色会话，禁止实现者自我验收。

### P1-7 prompt cache 控制与四个时钟（对应 §2.12）

- 形状：稳定前缀排序 + Anthropic `cache_control` 断点；把 approval 等待时长（human attention）与 AwaitingAcceptance → Done 时长（time to accepted outcome）聚合成会话指标，与 token 分开展示。

### P2-8 just-in-time 上下文（对应 §2.2）

- 形状：system message 收敛为「地图 + 路由表」，深层规则改为按路径检索（复用 `repo_index_*` 与 learned skills）；新增 `PreCompact` hook 用于压缩前固定「不要做 X」类约束。

### P2-9 hook 生命周期补全（对应 §2.10）

- 形状：新增 `PreCompact`、`SessionStart`、`SessionEnd`、`PostToolUseFailure`；`PostToolUseFailure` 的重复触发计数用于把反复失败升级为环境规则（“Turn repeated failure into infrastructure.”）。

### P2-10 编辑与验证接口补全（对应 §2.4/§2.5）

- 形状：patch/diff 形态编辑接口；写类工具的 dry-run 与 postcondition receipt。

## 4. 保留的冲突（不在本文调和）

这些冲突直接影响上面的优先级，选择需要真实轨迹数据而不是偏好：

1. **显式 Plan mode vs 静态强制**：Claude Code archive 的只读 Plan + 确认，vs lopopolo 的「outcome + 静态分析 + 阻塞式 review，因此不需要计划」。OPCOS 同时具备 Plan 模式与 `local_gate_record`，但两条路都不完整。
2. **压缩 vs fresh context**：lopopolo 把 auto-compaction 视为长 headless 轨迹的前提；Anthropic dynamic workflows 把压缩后 goal drift 作为改用新鲜 subagent 的理由。OPCOS 目前只有压缩这一条路（§2.2 差距）。
3. **前置完整手册 vs progressive disclosure**：与 P0-2/P2-8 直接相关；OPCOS 现状是前者。
4. **prescriptive instructions vs backpressure**：把品味编码进 CI/linter，还是删掉大部分系统提示让模型判断。
5. **单模型优化 vs model-independent harness**：harness workaround 会随模型过时（“那些重置变成了死重”），但评测又需要把模型当固定黑盒；这两件事不是同一命题。
6. **self-evaluation vs 独立 evaluator**：影响 P1-6 的强度。
7. **强约束 schema vs 简化工具描述**：影响 P0-2 中「目录条目要多薄」。

## 5. 可检验实验清单

在 P0-3 落地后可直接跑的对照实验：

- 全量工具注入 vs 目录 + `tool_search`：每轮工具 token、工具选择错误率、完成率。
- 扁平 `{"error": string}` vs 结构化错误信封：同一失败后的一次性修复率、重试次数。
- 单窗口压缩 vs fresh subagent：目标保持率与总 token。
- 自我验收 vs 独立 evaluator：误报通过率。
- 同步 shell vs 后台 shell：feedback latency 与 wall-clock。
