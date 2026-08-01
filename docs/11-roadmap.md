# 11 开发路线与 Todos

定位：**Devin 为主（Cloud 形态与产品完整度对标 Devin），Tembo / OpenWork / OpenWorker / Cloud-Dev 为辅（Local 形态的实现参照）**。见 [README](README.md)。

每一项都写明：改哪里、验收标准。**验收标准是可执行的**——没有真机证据不算完成。所有项都受 [00](00-architecture.md) 硬约束与 [08](08-security.md) 的 token 边界限制。

状态图例：`[ ]` 未开始 · `[~]` 进行中 · `[x]` 完成

---

## P0 —— 收口现有实现（地基，不做完不往上盖）

### P0-1 单一存储权威

`opcos-store` 与桌面 adapter 各有一套 `sessions` schema；桌面 `transcript` 还与 store 的 `messages`、`notices`、`tool_calls` 职责重叠（见 [01](01-data-model.md) 1.1），会漂移。

- 改：`crates/opcos-store`、`src-tauri/src/main.rs`
- 做：桌面表只保留 adapter 专属（`hosts`、`secret_records`、`schedules`），会话/消息/工具/审批全部归 `opcos-store`；写一次性迁移。
- 验收：`cargo test` 通过；重启后 transcript、pending approval、审计完整；数据库中不再有两张 `sessions`。

### P0-2 会话状态二维化

- 改：`opcos-engine`、`opcos-store`、`web/src`
- 做：`run_state`（`idle|running|interrupted|error`）× `stop_reason`（见 [03](03-lifecycle.md) 3.2），持久化原始枚举；UI 用 `stop_reason` 区分「等你回话 / 等审批 / 跑完了 / 主机不可用」。
- 验收：四种 `stop_reason` 各能在 UI 上被区分出来；`host_unavailable` 不被显示成「已完成」。

### P0-3 Host trait 与本机 host

用户已多次提出「VM 默认 local 缺失」。

- 改：新增 `crates/opcos-hosts`，`opcos-rvm` 实现 `RvmHost`
- 做：按 [04](04-host-protocol.md) 4.1 落 `Host` trait；实现 `LocalHost`（真实进程生命周期：启动、停止、端口占用检测、崩溃恢复、能力探测、只绑定回环）；能力探测结果带来源与时间戳。
- 验收：设置页出现「本机」，能真起真停；端口被占用时报明确错误；远程不可用仍显式报错、不回落本机。

### P0-4 产物模型

- 改：`opcos-store`、右栏
- 做：`artifact` 表只存引用（host、路径、大小、hash、turn），不复制内容；工具执行结果落产物记录；右栏加 Artifacts pane（照 OpenWorker `surfaces/gui/src/components/RightRail.tsx` 的 Artifacts section/viewer 结构，沿用 OPCOS 现有 rail 样式）。
- 验收：一次真实 turn 后产物列表出现远端文件，点击能读回内容；远端不可读时显式报错。

---

## P1 —— 能力模型与协议

### P1-1 配置对象迁移

- 做：五套资产收敛到 `config_object` + 不可变 `config_object_version`（[06](06-capability-model.md)、[01](01-data-model.md)）；scope 使用 `scope_kind + scope_key`；会话钉具体 version，调度运行记录实际 version；UI 仍保留五个入口但走同一套 store API，并提供历史、比较和回滚；技能改「先给清单、命中再读全文」留待后续优化。
- P1-5 全局 Instructions：使用 `config_object(kind='instructions')` 的 global active version，按“全局 Instructions → Rules → Knowledge → Playbook → Skill”顺序注入；会话绑定版本后，中途编辑不影响进行中会话；Settings 沿用 Rules 编辑器。
- 验收：编辑资产产生新版本、旧版本可回滚；30 个技能时上下文不膨胀（只注入清单）。

### P1-2 MCP client 完整化

- 做：global（`~/.config/opcos/mcp.json`）与 workspace（`<ws>/.opcos/mcp.json`）配置层叠、同名 workspace 覆盖 global；transport `stdio` / `http` / `streamable-http` / `sse`；OAuth token 进 SecretStore **不写配置文件**；tool discovery + 逐 tool 审批（[05](05-mcp.md)）。
- 验收：接入一个真实 MCP server，工具可发现、可逐个审批；配置文件中无任何 token。
- 独立 server 使用 `config_object(kind='mcp')`，凭据仅进入 SecretStore；
- 常驻 MCP manager 负责 transport 生命周期，退出时 stdio 子进程 kill + wait；
- 状态显式区分 `disabled/starting/connected/disconnected/reconnecting/auth_required/failed`；
- tools/list runtime cache 按 `(server_object_id, config_version_id)` 失效；
- 不可用 server 保留在 UI 状态列表，但工具不进入 provider request；
- 工具名稳定为 `mcp__<server_key>__<tool_name>`，调用不自动 failover；
- 重连采用有限退避：立即、500ms、1s、2s、4s、8s、16s，30s 封顶。

### P1-3 生命周期阶段与 pre-push 门禁

- 做：按 [07](07-automation.md) 7.3 实现五个阶段与各自失败语义；补 `pre-push` 门禁（硬失败阻止 push）。
- 验收：故意让 `pre-push` 失败 → push 被阻止并显示失败命令与退出码；`maintenance` 失败不阻断。
- 阶段统一走 `Host` trait，local/remote 使用相同执行器；
- `clone` / `initialize` / `post-build` / `pre-push` 首个失败立即终止；
- `maintenance` 失败记录后继续后续命令；
- `initialize` / `post-build` 失败不留下成功缓存或可复用快照标记；
- 阶段开始、结束、失败以及每条命令的退出码和耗时写入 `audit_events`。
- 环境复用机制引入后，再补充 initialize 成功缓存、post-build 快照与环境就绪标记；当前没有缓存可失效。

### P1-4 Inbox 与无人值守

- 做：审批、提问、目录请求、计划确认统一停放 durable Inbox；会话可标记 unattended；断线重连后恢复挂起项（参照 OpenWorker 的模型）。
- 验收：unattended 会话产生的审批出现在 Inbox；重启 app 后仍在，处理后会话继续。
- 实现约束：挂起项统一复用 `pending` durable 表，状态为 `pending/resolved/expired`；
  处理必须幂等，投递/处理/过期写入 `audit_events`，载荷沿既有脱敏路径处理。

### P1-5 全局 Instructions

- 做：`kind = instruction` 的配置对象，追加到所有会话的系统提示（PR 标题、commit 文案等规则）。
- 验收：新会话的系统提示包含指令内容；关闭后不再包含。

---

## P2 —— 平台化

- **P2-2a Harness foundation**：
  - `Harness` 的启动、审批回复和问题回复返回异步 `TurnHandle`；完成结果只通过 `TurnFinished` 事件到达，句柄提供 `await_finished()` 便捷等待；
  - 外部 harness 的 HTTP/SSE 生命周期不能同步返回 `AssistantTurn`，因此不再伪造空结果或把“仍在运行”当作成功；
  - 审批事件的可批准形状必须包含完整工具名和完整参数；补全失败只产生显式 enrichment failure 事件并保持 pending，不创建审批卡片；
  - 会话记账通过可复用 `SessionRecorder` 访问状态、pending/Inbox、审计和产物记录；
  - `HostProcess` 支持显式 shutdown、drop 关停和生命周期 supervisor；本机子进程 drop 后必须终止，远程进程仍受 PTY 退出码限制；
  - sessions 独立保存 `external_session_id`，不把 OPCOS session ID 与外部 harness ID 混用。

- **P2-1 Harness 抽象与进程流**：
  - 已落地 harness-neutral trait、事实中心事件模型与 `BuiltinHarness`（现有 `TurnEngine` 的适配器），不改变内置行为；
  - 已落地 `Host::spawn` / `HostProcess`，本机走管道，远程走 RVM 现有 `/pty-ws`；
  - `HostProcess` 只交付增量 UTF-8 输出和生命周期事件，不承诺干净行，不在 Host 层清洗 ANSI、`\r`、echo 或做 NDJSON 分帧；
  - PTY 承载结构化输出存在 echo、控制序列、窗口宽度换行污染风险；
  - 远程 PTY 进程流没有退出码，当前 `Exited` 只能报告 `None`；P2-2 再评估通过 marker 回读退出码；
  - 本机使用无 echo 的普通管道，远程使用有终端噪声且无退出码的 PTY，两者语义一致但底层交付特性不同；
  - 外部 harness 尚未接入，UI 不提供 OpenCode 或其他假入口；
  - OpenCode CLI 模式因 permission/question 无法交回 OPCOS、会自动批准或拒绝而否决，不使用 `--auto` 降级。
- **P2-2 OpenCode server harness**：
  - 已通过 Host 启动 `opencode serve --hostname 127.0.0.1 --port 0`，使用 Host 上的 curl HTTP 请求和 `curl --no-buffer` SSE 事件流；
  - OpenCode 未安装、主机缺少 `process_stream` 或服务启动/端口回读失败时显式不可用，不创建假入口；
  - Basic 认证只引用受保护的 `OPENCODE_SERVER_PASSWORD` 环境变量，不进入 argv、URL 或 curl 命令文本；
  - permission 先按 `messageID`/`callID` 回查 tool part，补全失败只保留 pending 并发 `ApprovalEnrichmentFailed`；
  - 需要验证远程 `/pty-ws` 承载 NDJSON 是否可靠；若不可靠，考虑远程端口转发或等价 Host 通道；
  - 远程 HTTP 端口如何安全访问仍是未决设计，本轮不解决；
  - 已否决 `/api/expose-port` + cloudflared：公网暴露 agent 控制面会引入 URL 泄露、隧道生命周期和 cloudflared 可用性风险；目标方案为 Host 上的 `curl`（普通请求走 `Host::exec`，SSE 走 `Host::spawn`）；
  - 必须完整接入 Inbox、审批、二维状态、审计、产物登记和中断恢复后，才提供 OpenCode 入口。
- **P2-2 自动化三类触发**：定时（已有）+ 出站事件轮询（GitHub / Linear / Sentry）+ webhook（需 relay，见 P3）。payload 以结构化 event context 传入，不压成一句 prompt。
- **P2-3 连接器框架**：适配器接口 + OAuth（手工 token 路径永远保留）+ token 只进 SecretStore。先做一个真集成，不做空壳。
- **P2-4 插件打包**：`plugin` / `plugin_member`，支持从 GitHub 仓库导入导出；MCP server URL 只能来自插件自己的配置对象。
- **P2-5 仓库索引与语义检索**：`opcos-context`，知识按触发条件注入（对标 Devin DeepWiki）。

## P3 —— OPCOS Cloud（可完全关闭）

顺序 **A → D → B → C**，见 [09](09-cloud.md)。前三种都不改变「agent 循环在本地」。

- **A** 托管 OAuth broker（云端不存连接器 token）
- **D** 配置对象与插件分发、授权
- **B** 事件 relay（公网入站 webhook，本地只出站）
- **C** worker fleet（队列 + 原子 claim + 租约超时回队，照 Devin Outposts）

---

## 收尾项（贯穿）

- [ ] 剩余自造 UI（会话 topbar、Transcript、composer 外壳、Settings 正文页、Activity 看板）退回 OpenWork / Cloud-Dev 参照实现
- [ ] blueprint 写入 Tauri / Rust / 系统依赖
- [ ] 每个里程碑后跑真机 Tauri 端到端验收并录屏
- [ ] 发布产物：deb / rpm / AppImage / Windows NSIS + `SHA256SUMS.txt`（本地构建，GitHub Actions 只做 lint/test）
