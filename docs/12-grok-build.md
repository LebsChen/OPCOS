# grok-build 参照调研

定位：只读逆向参照，不是 OPCOS 的代码移植方案。本文事实来自
`/home/ubuntu/repos/grok-build` 当前 checkout 的 `crates/codegen` 源码，
统一使用来源标记 **［GB码］**；由事实形成的 OPCOS 取舍标记为
**［推断］**。不复制 grok-build 代码；未来移植前仍需单独做许可证、依赖和
安全审查。

## 1. 调研边界与结论

- grok-build 将 agent、会话 actor、工具注册、MCP、配置、hooks、sandbox、
  memory、workflow 拆成多个 Rust crate，而不是把所有循环塞进一个 runtime。
  ［GB码］
- 当前仓库存在 `xai-grok-agent`、`xai-chat-state`、`xai-grok-tools`、
  `xai-grok-mcp`、`xai-grok-config`、`xai-grok-hooks`、`xai-codebase-graph`
  等目标 crate；未发现名为 `xai-tool-protocol`、`xai-tool-runtime`、
  `xai-interjection-core` 或 `xai-circuit-breaker` 的同名目录。［GB码］
- 最值得 OPCOS 直接吸收的是：actor 化 session state、持久化边界、
  unresolved/dangling history 修复、可配置 compaction、工具注册器、
  MCP liveness/OAuth 去重、配置层叠、hook matcher/trust、网络策略快照。
  ［推断］
- 这些设计不能直接替换 OPCOS 已有的 `opcos-engine`、`opcos-store`、
  Tauri IPC 或 RVM 安全边界；应作为接口和测试参照。［推断］

## 2. P0：agent loop、会话状态与 prompt

### 2.1 `xai-grok-agent`

- `CompactionPolicy` 将自动压缩阈值、压缩模型、memory flush、墙钟预算和
  two-pass compaction 作为会话级策略；默认阈值为 85%，默认不启用
  memory flush/two-pass。［GB码］
- prompt 组装拆成 `agents_md`、skills、context、workspace user、
  subagent prompt、template、user message 等模块；system reminder 另有
  `ReminderPolicy`、TodoNudge 和可选 TodoGate。［GB码］
- TodoGate 在 content-only assistant message 后检查待办；若仍有未完成项，
  注入 system reminder 并触发额外 turn，同时有每 prompt 的 hard cap。［GB码］
- OPCOS 已有 system instructions、compaction、todo 工具和 turn loop，但
  prompt 组装与状态更新仍分散在 engine/adapter。［推断］
- 值得移植：把 prompt assembler、compaction policy、reminder policy
  定义为 engine 内的纯策略接口；代价中等，需补 provider context budget、
  测试和事件语义。［推断］

### 2.2 `xai-chat-state`

- `ChatStateActor` 独占可变会话状态，外部只持有 `ChatStateHandle`，通过
  mpsc command 发送 push message、token usage、turn timing、conversation
  replace、snapshot restore、repair history、flush 等命令。［GB码］
- `ChatStateSnapshot` 包含 conversation、sampling config、prompt index、
  token usage、edited paths、prompt text、stream/turn 时间、compaction
  boundary 和 opaque credentials。［GB码］
- actor 事件是协调信号，例如 `PromptIndexChanged`、`TokensUpdated`、
  `ConversationReset` 和 image budget；持久化由 actor 内部负责，而非由
  每个调用方自行写文件。［GB码］
- `ChatPersistence` 区分单条 append、整段 replace、flush，并支持需要
  persistence acknowledgement 的工作目录切换。［GB码］
- actor 初始化会去重重复 tool result，并将没有对应 result 的 dangling
  tool call 修复为 user-cancelled，避免进程崩溃后永久携带坏 transcript。
  ［GB码］
- OPCOS P0-1 已让 `opcos-store` 成为 transcript 权威；P0-2 应借鉴
  actor 的“命令串行化 + snapshot + event”边界，而不是另造第二个数据库。
  ［推断］
- 值得移植：在 `opcos-engine` 增加 session state coordinator，承载
  `run_state`、`stop_reason`、turn capture 和 restart recovery；代价较高，
  但与 P0-2 直接相关，优先级最高。［推断］

### 2.3 lifecycle、steering 与队列

- `xai-agent-lifecycle` 提供 host-agnostic 的 session/turn lifecycle
  contributors，以及 turn start/done/error/abort、input fragment 和
  command contributor 类型；贡献者只接收 data-only input，不拥有 loop
  控制权。［GB码］
- `xai-prompt-queue` 将排队消息建模为带 metadata 的 entry，提供 front/
  follower 合并门禁、文本拼接和 combined 标记。［GB码］
- 当前 checkout 未发现同名 `xai-interjection-core`；steering 相关能力由
  lifecycle send 类型和 agent/session 调用方承担。［GB码］
- OPCOS 已有 steering queue、interrupt 和 turn 事件，但没有独立的
  lifecycle contributor registry。［推断］
- 值得移植：先采用 data-only lifecycle event 与可测试 prompt queue，
  不复制完整 actor；代价中等，适合 P0-2/P1-4。［推断］

## 3. 工具、审批与持久化

### 3.1 `xai-grok-tools`

- `ToolRegistryBuilder` 注册工具类型、参数 schema、requirements、metadata、
  namespace/kind，并能 finalize 出 tool definitions、contract version、
  tool identity 和 cooperative cancellation 信息。［GB码］
- 工具调用前后有 normalization、参数 remap、retry、notification、
  reminder、persistence 等独立模块；retry 使用可配置 max retries、base
  delay 和 max delay 的 backoff。［GB码］
- 工具资源持久化提供 load/save、异步 enqueue-save-and-flush；registry
  也能按 prefix 或名称注销工具。［GB码］
- `pre_tool_use` 等策略不应隐含在工具实现中；工具定义、执行上下文和
  tool output 是分离类型。［推断］
- OPCOS 已有 `ToolExecutor`、policy、active call guard、tool_calls/pending
  持久化和逐工具审批；缺少统一 schema registry、contract version、重试
  语义和工具 metadata。［推断］
- 值得移植：先抽取 `ToolDefinition`/schema/identity/requirements，补充
  retry policy；代价中等。不要替换现有 `execute_tool` 单一路径。［推断］

### 3.2 审批与 unresolved 事实

- grok-build 的 chat state 会在启动时修复 dangling tool calls；这表明
  “没有 result”不能直接等同于“审批中”或“失败”。［GB码］
- OPCOS 当前 P0-1/P0-1 review 已将 store 的无 result 无 pending 状态记为
  `unresolved`，由 adapter 依据活跃 engine 覆盖为 `running` 或
  `interrupted`。这是与该参照相容的事实/显示分离。［推断］
- 值得移植：将 tool lifecycle 的 `pending/active/persisted/unresolved`
  测试矩阵作为回归套件；代价低，优先 P0-2。［推断］

## 4. P1-2：MCP transport、OAuth、liveness

### 4.1 `xai-grok-mcp`

- crate 明确隔离 MCP SDK 与 workspace 其他 reqwest 版本；servers 模块
  负责 Streamable HTTP、Tokio child process、tool invocation、错误分类和
  managed-MCP refresh。［GB码］
- credential store 位于 `$GROK_HOME/mcp_credentials.json`，按 server name
  与 URL 组合 key 保存 SDK 的 `StoredCredentials`；加载时尝试收紧 Unix
  权限到 `0600`，Debug 只显示 entry count。［GB码］
- OAuth 同时做跨进程文件锁和进程内 watch-channel 去重；后来的请求可以
  等待 leader 结果，force 请求可驱逐 stale entry。［GB码］
- liveness 单独建模连接/心跳状态，而不是把初始化中的每个 server 都
  当作全局 ready；MCP pool 以 `NotStarted`、`Starting`、`Finished`
  状态表达握手进度。［GB码］
- OPCOS 已有 MCP client 方向、SecretStore 设计和多个 transport 目标，
  但 P1-2 尚未完成 global/workspace 层叠、OAuth、liveness 和增量 refresh。
  ［推断］
- 值得移植：OAuth credential store + cross-process/in-process dedup、
  typed init progress、server diff refresh 和 liveness；代价中高，必须把
  token 放入 OPCOS SecretStore，不能照搬明文文件路径。［推断］

## 5. P1-1：配置层叠、override、managed

- `xai-grok-config-types` 用可选字段的 settings patch 表示局部 override；
  未设置字段继续向下一层 fallback，未知未来 key 可忽略。［GB码］
- `xai-grok-config` 分出 loader、config override、version override、
  managed cache、managed text transaction、validation、signed policy 和
  atomic filesystem helper。［GB码］
- managed text 有 source、format、transaction、validator；managed cache
  有 claim/test 逻辑，避免多个来源同时声称管理同一配置。［GB码］
- OPCOS 目标态已经定义 `config_object` 与不可变
  `config_object_version`，但 P1-1 尚未落地层叠解析、managed ownership
  和原子版本切换。［推断］
- 值得移植：采用“每字段 optional patch + source precedence +
  atomic write + validation report”，代价中等；不照搬 grok 的用户目录或
  远端配置命名。［推断］

## 6. P1-3：hooks、matcher 与 trust

- `xai-grok-hooks` 将 event envelope、event name、payload、matcher、
  hook spec、runner、result、dispatcher 和 trust 分开。［GB码］
- matcher 使用正则并按事件 payload 提供 match value；hook 配置可从多层
  config 解析，保留 hook origin。［GB码］
- `pre_tool_use` 逐 hook 顺序执行；明确 deny 会阻断，失败在该实现中
  fail-open 并保留 per-hook 结果供 UI scrollback。［GB码］
- trust 支持旧 trusted-projects 文件迁移、按项目禁用 hook、enable/disable；
  缺失文件表示空集合，其他读取错误不会伪装为空。［GB码］
- OPCOS 目前有 policy/audit 和 automation 方向，但没有独立 hook event
  envelope、matcher、trust migration、per-hook result。［推断］
- 值得移植：P1-3 先做 typed event/matcher/trust 和可配置 fail-open/closed
  策略；安全门禁默认应由 OPCOS policy 决定，代价中等。［推断］

## 7. P2-5：索引、memory 与上下文

- `xai-codebase-graph` 有后台 `IndexManager`、文件事件、scope graph、
  snapshot/cache、definition/reference/navigation 查询和 query version。
  ［GB码］
- `xai-grok-memory` 拆分 chunker、embedding、index、search、archive、
  watcher、query expansion、schema 和 storage；memory 还区分 dream/
  dream lock 等后台整理路径。［GB码］
- OPCOS 当前有 workspace/files 工具和知识资产，但没有增量符号图、
  embedding index 或语义检索。［推断］
- 值得移植：先移植 file-event/index-manager 接口和符号导航，再评估
  embedding/memory；代价高，P2-5，不能在 P0-2 前引入。［推断］

## 8. 安全与运行平台参照

- `xai-grok-sandbox` 将 child network policy、website origin allow/deny、
  profile/config、write-deny hook 和 canonical policy snapshot/hash 分开；
  snapshot 可序列化、校验 hash。［GB码］
- `xai-grok-secrets` 另有 secret sanitizer；工具层还提供 shell environment
  policy 和输出截断/脱敏辅助。［GB码］
- OPCOS 已有 policy、SecretStore、RVM path algebra 和 token redaction；
  不应把 grok 的本机 sandbox 当成远程 RVM 权限模型。［推断］
- 值得移植：网络策略快照/hash、输出 sanitizer 的测试思想；代价中等，
  P0-3/P1-3，必须保留 Authorization header-only 约束。［推断］

## 9. P2/P3 辅助 crate

- `xai-grok-plugin-marketplace` 对 marketplace relative path 做 containment
  校验，安装/更新提供 transactional result；插件 source、entry、scan
  和 keyword matcher 是独立类型。［GB码］
- `xai-grok-subagent-resolution` 将 context source、definition、effective
  runtime config、override intersection、resume identity validation 分开。
  ［GB码］
- `xai-workflow` 有 run params、pause kind、outcome、journal、request hash
  和 script validation；journal 可 replay、prune trailing host error。
  ［GB码］
- `xai-sqlite-journal` 按数据库路径选择 journal mode，并区分 network FS。
  ［GB码］
- `xai-token-estimation` 提供 token/character/image 估算、usage percentage、
  free tokens、threshold/headroom 判断。［GB码］
- OPCOS 已有 plugin/automation roadmap、coordination、SQLite store 和
  usage events；可参考这些 crate 的类型边界，但不要复制实现。［推断］
- 值得移植：P1-1/P2-4 的 containment + transactional install，P1-4 的
  subagent effective config/resume identity，P0-2 的 request hash/journal
  replay，代价分别为中/中/高。［推断］
- 当前 checkout 没有同名 `xai-circuit-breaker` crate；不能据此声称
  grok-build 已提供独立 circuit-breaker API。［GB码］

## 10. 可落地顺序

1. **P0-2**：采用 actor command/event、snapshot、dangling/unresolved 修复、
   request hash/journal replay 的测试边界；不改变 OPCOS 的
   `run_state × stop_reason` 词汇。［推断］
2. **P1-1**：实现 optional patch precedence、managed ownership、atomic
   version write 和 validation report。［推断］
3. **P1-2**：实现 MCP typed init progress、OAuth 去重、SecretStore-backed
   credential、liveness 与 config diff refresh。［推断］
4. **P1-3**：实现 typed hook envelope、matcher、trust migration、
   per-hook results 和明确 failure policy。［推断］
5. **P2-4/P2-5**：插件 transactional install/containment、增量 code graph，
   再评估 embedding memory。［推断］
