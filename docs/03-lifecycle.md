# 03 生命周期与状态机

所有状态枚举都要**在 store 里存原始字符串**，不要只存 UI 文案；参照系统的教训是状态一旦被 UI 化就无法审计和迁移。

## 3.1 参照系统的状态模型

**Devin**［Devin文］同时存在两套，按 API 版本区分，不可混用：

- v1 `status_enum`：`working` / `blocked` / `expired` / `finished` / `suspend_requested` / `suspend_requested_frontend` / `resume_requested` / `resume_requested_frontend` / `resumed`
- v3 `status`：`new` / `claimed` / `running` / `exit` / `error` / `suspended` / `resuming`
- v3 `status_detail`：`working` / `waiting_for_user` / `waiting_for_approval` / `finished` / `inactivity` / `user_request` / `usage_limit_exceeded` / `out_of_credits` / `out_of_quota` / `no_quota_allocation` / `payment_declined` / `org_usage_limit_exceeded` / `total_session_limit_exceeded` / `error`

关键洞察［推断］：Devin 把**运行态**（`status`）和**为什么停下**（`status_detail`）拆成两维。OPCOS 应当照抄这个二维模型——一维状态机在「等用户」「等审批」「跑完了」这三种都表现为「不在跑」时会丢信息。

**Outposts 队列**［Devin文］：`status.phase` = `pending` / `claimed`；`status.session_status` = `pending` / `running` / `suspended` / `terminated`；claim 是原子的，竞争失败返回 `409`，claim 过期自动回队列；watch 用 SSE，事件类型 `MODIFIED` / `DELETED`，**至少一次投递**，消费方按 `metadata.session_id` upsert。

**Den worker**［OW文］：worker 用签名 token 主动 `POST /v1/workers/{id}/activity-heartbeat` 上报 `lastActiveAt` / `openSessionCount` / `isActiveRecently`——**健康状态由 worker 推送而非中心轮询**。

## 3.2 OPCOS 会话状态

```
run_state:    idle | running | interrupted | error | sleeping
stop_reason:  none | waiting_for_user | waiting_for_approval | finished
              | interrupted_by_user | host_unavailable | provider_error
              | policy_denied | context_exhausted | internal_error
              | max_iterations | idle_timeout
terminal_cause: harness_error | host_unavailable | completed | user_interrupted
                | provider_silent | waiting_for_user | waiting_for_approval
                | provider_error | context_exhausted | internal_error
                | tool_preflight_error | max_iterations | usage_limit
                | policy_denied | model_stopped
```

约束：

- `run_state`、`stop_reason`、`terminal_cause` 和 `provider_finish_reason` 分开持久化；
  例如新建 session 可以暂时是 `idle/none`，而 setup 失败是 `error/harness_error`。
- `host_unavailable` 是**终止原因**，不是重试状态——远程不可用要显式暴露，见 [00](00-architecture.md) 硬约束 3。
- `sleeping` 表示 idle 断连后的运行时释放，不等同于错误；store 会将休眠会话设为
  `stop_reason=idle_timeout`。
- `terminal_cause` 保留更细的终止分类，例如 harness 初始化失败使用
  `harness_error`；provider 的协议细节另存于 `provider_finish_reason`。
  上面列出的是当前可达写入路径确认过的 `terminal_cause` 字面量：
  builtin engine 的终止状态会把对应的终止原因写入该列，ACP turn 还会写入
  `model_stopped` / `completed` / `user_interrupted`。`interrupted_by_user` 是
  `stop_reason` 的值，`idle_timeout` 也是 `stop_reason` 的值；休眠迁移会清空
  `terminal_cause`，因此二者不属于这里的枚举。

### 3.2.1 Idle 断连

桌面端的 idle maintenance 以 `session_activity` 而不是 `sessions.last_active_at`
单独选候选，并在候选真正 teardown 前重新加载 session。重新检查会话状态、active
turn、pending approval/question、surface lease 和 IDE lease；任一 guard 命中都不
释放运行时。

teardown 成功顺序固定为：

1. 释放 engine、capability/host、ACP 和 MCP runtime；
2. 成功后才写 `run_state='sleeping'`，记录 idle audit 并发出 `session-sleep`；
3. 每个 tick 最多处理八个 disconnect。

teardown 失败时写 `idle_teardown_failed` audit，payload 至少包含 `phase` 和
`detail`；不覆盖失败前的 session 状态，发出 `idle_teardown_retrying` structured
notice，并在后续 tick 重试。

### 3.2.2 Wake

`wake_session_inner()` 负责从 sleeping 清除休眠状态、刷新 activity 并发出
`session-wake`；需要实际 runtime 时统一走 `engine_for_with_context()`，不另建第二套
runtime builder。重建失败会释放半初始化 runtime map，确保
`state.engines[session_id]` 不残留，并恢复为 `sleeping`，发出
`wake_runtime_failed` structured notice，后续仍可重试。

### 3.2.3 首轮先接受

首轮入口先把 user message 写入 `messages`/`session_events` 并 emit 给前端，使
transcript 立即可见；随后才执行 wake、远程 host health、engine、capability/tool/
asset discovery。setup 失败保留这条 user message，设置 `terminal_cause=harness_error`
并发出 `turn_setup_failed`（远程 host 健康检查失败使用 `host_unavailable`）。
这一失败边界不创建 pending tool call、assistant tool call 或 dispatch marker，因此
恢复不会重放尚未 dispatch 的工具。

### 3.2.4 Recovery 分类

恢复 pending turn 时，先检查 durable dispatch marker：

- 已记录 dispatch 尝试但没有 durable result：`execution_state_unknown`，不重放；
- 未 dispatch 且工具是安全 read/search/git-read：允许安全重试；
- 未 dispatch 但工具会产生副作用：`side_effecting_unresolved`，写入 recovery
  结果并明确不重放；
- dispatch marker 在 executor 调用前持久化，避免崩溃窗口把可能已发出的副作用
  当成可安全重试。

## 3.3 Turn 生命周期

一个 turn = 用户输入 → 若干次「模型响应 + 工具执行」→ 终止事件。

发往前端的事件序列（现有实现的契约，改动必须同步 [02](02-ipc-contract.md)）：

```
turn_start
  ├─ assistant_delta*        流式文本增量
  ├─ tool_call               工具意图（含 callId）
  │    ├─ approval           需要审批时（callId 与 tool_call 一致）
  │    └─ tool_result        执行结果
  ├─ notice*                 结构化提示（风险、原因、策略拒绝）
  └─ turn_done               必须发，且必须是最后一个
```

已踩过的坑，作为回归约束：

- **审批延续**必须补 `turn_done`，否则前端永远显示 running。
- **steering** 必须有完成事件，且在事件顺序上排在当前 assistant 流之前。
- 同一个 `callId` 的重复记录要合并，不要产生两张卡片。
- pending approval 在 store 里由 `pending` 表保存，并与 `tool_calls` 按 `call_id` 合并；`read_transcript` 转成 `kind=approval`，前端负责渲染审批卡片。

### 3.3.1 全局 Instructions 与资产注入顺序

全局 Instructions 使用 P1-1 的 `config_object(kind='instructions')` 与不可变
`config_object_version`，作用域固定为 `scope_kind='global'`。会话创建/首次绑定时
固定当时的 active version；后续编辑只影响新会话，不改变进行中的会话。

系统提示的明确顺序为：

1. 全局 Instructions；
2. 工作区/仓库 Rules；
3. Knowledge；
4. Playbook；
5. Skill。

未设置全局 Instructions 时不生成该段，现有资产顺序与行为保持不变。

## 3.4 审批

判定顺序［推断，对齐 OpenWorker 的 standing rule 思路］：

1. 策略模式（只读 / 需审批 / 自动）
2. 已有 standing rule 命中 → 直接放行并记审计
3. 否则挂起，写 `pending` 与对应 `tool_calls`，发 `approval` 事件

决策只有两个值：`allow` / `deny`（不是 `once` 之类的字符串），映射到 `approve: true|false`。每次决策都要写 `audit_events`，且**审计负载不得包含任何 token 或密钥值**（Den 明文规定审计负载剔除凭据［OW文］）。

## 3.4.1 Inbox 挂起项与无人值守

审批、`ask_user` 提问、目录/工作区请求和 `propose_plan` 统一写入 durable
`pending` 表，使用 `kind`、`payload`、`created_at`、`state`、`resolution` 和
`resolved_at` 表达同一条挂起项。状态只能从 `pending` 进入 `resolved` 或 `expired`；
重复处理是幂等的，第一次解决结果保留。

会话的无人值守开关单独持久化。开启后，挂起项的可见性为 `inbox`，会话仍使用既有
二维状态 `idle / waiting_for_approval` 或 `idle / waiting_for_user`，不会引入新的
运行状态词。应用重启只重建内存 engine，不丢失 Inbox 项；处理仍调用现有的
`resolve_approval` 或 pending resume 路径。

投递、处理和过期均写入 `audit_events`。进入存储和 UI 前，载荷必须沿用既有审批
脱敏路径，禁止把 token、密码或其他凭据写入 Inbox。

## 3.5 产物（Artifacts）［推断］

三个参照系统都有产物面板（Devin / Tembo / OpenWork）。OPCOS 目前只有 Diff 与 Worklog。目标态：

- 产物 = 会话期间在 host 上创建/修改的文件 + 生成的 PR + 导出的报告。
- 来源是工具执行结果，不另建一套记录；产物表只存引用（host、路径、大小、hash、turn），**不复制文件内容**。
- 远程产物的读取走 host 协议 [04](04-host-protocol.md)，失败显式报错。

## 3.6 编排（多会话）

Cloud-Dev 的星型拓扑（Leader → Worker 单向派发、结构化消息信封、限流熔断）［CD码］已在 `opcos-engine/src/orchestration.rs` 有对应实现。演进方向照 Outposts 的队列模型［Devin文］：**待服务队列 + 原子 claim + 租约超时回队**，而不是中心直接指派——这是唯一能同时支撑「多设备」和「断线恢复」的形状。
