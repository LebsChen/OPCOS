# OPCOS 当前状态与下一步

本文只记录代码已经实现的能力、明确限制和未完成事项。`[x]` 表示当前
`origin/dev` 的代码已具备；`[ ]` 表示没有完成，不以文档目标代替实现。

## PR #93 P0/P1 状态（当前分支）

### P0 runtime batch

- [x] working-event envelope、Devin-style timeline rows、PlanRecord task progress、
  compaction row、空 artifact 清理、control slash actions、steering bubbles、
  live/re-read/cold-restart parity。
- [x] model-aware context/output limits：gateway → matrix → probe → learned → user →
  assumed；真实 gateway `glm-5.2` 通过 Local host 验证 1M window。
- [x] crash-orphaned `running` session startup reconciliation 和 authoritative Stop。

### PR #93 已完成

- [x] artifacts / attachments：截图和 diff artifact 持久化、timeline 展开读取，
  以及 artifact rail 的本地展示；recording、citation snippet 仍是明确 open gap。
- [x] terminal replay：`terminal_update` 按 `call_id` 聚合，64 chunks / 2000 chars
  上限，显式 truncated 收尾和 timeline 展示。
- [x] iteration stats：canonical timing 事件、Info pane 汇总和逐轮 timing 明细。
- [x] LocalHost persistent shell protocol：POSIX/Windows marker、临时文件、stdin
  隔离和 late background output regression。
- [x] Local GUI streaming shell：`DesktopExecutor::execute_streaming` 复用
  `opcos-local-<session_id>` persistent session，保留 cwd/env/exit code 和 live
  streaming；该路径不提供 PTY/ANSI 语义。
- [x] output-window alignment：model result 明示 tail 64 KiB 与 omitted bytes；
  terminal truncated event 带 total byte count，Transcript 说明 model saw the tail。

### PR #95 timeline parity 已完成

- [x] work-group one-line labels from persisted `one_line_thoughts`.
- [x] major/minor collapse in chronological position.
- [x] thought → action association with standalone fallback across flushed groups.
- [x] shell rows with stable per-session `shell_id`, real exit code, and duration.
- [x] bounded work-group and terminal output panes.

### Devin event-stream gap backlog（按下一轮优先级）

- [ ] P0：planning/todo surface。让真实 `PlanRecord`/`todo_update` 驱动 Devin
  式行内 `1/4 #1 ...` 进度行；PR95 的七段任务中 plans/plan_steps 仍为空。
- [ ] P0：clarifying question / side-effect approval behavior。对歧义任务先走
  `ask_user`，对安装依赖等副作用在执行前请求 approval；这是 harness 行为而非
  单纯 timeline 渲染。
- [ ] P0：合并同一 user turn 的 iteration work groups，避免像 PR95 一样产生
  37 iterations / 35 个小 groups，而 Devin 显示一个 aggregate Worked-for group。
- [ ] P1：remote `cd` persistence。RVM Linux 上 exported env 可持久化，但 cwd
  在下一次 `run_shell` 重置。
- [ ] P1：remote live terminal streaming。RVM stream 当前没有
  `terminal_update`，长命令只能在完成后看到输出。
- [ ] P1：remote Desktop/VNC 与 Editor/Web IDE rail surfaces。RVM 已广告
  `vnc_port`/`ide_port`，但 `App.tsx:9374-9389` 仍是 `PlannedPane`。
- [ ] P1：本地 context composition 计算 `per_source_context_bytes` 和
  `tool_aggregates`，不使用 provider usage 代替 context bar。
- [ ] P1：`multi_edit_result` 补 `total_lines`、open/edit 语义和完整 file snapshot
  `contents_key`；search 事件补 regex/path/result filenames。
- [ ] P1：`todo_update` 补 summary counts 与 `subagent_id`；增加 inline
  `subagent_started` / `subagent_finished` 的真实 coordination payload。
- [ ] P2：raw terminal bytes/base64 与 foreground/background completion 语义；
  不在当前 PR 伪造 Devin Cloud 的 binary/control-plane 字段。
- [ ] P2：每行 wall-clock timestamps，以及 Devin right-rail-only shell output。
- [ ] P2：MCP/connector 专用 timeline rows，以及 recording/test-mode/sidekick/
  route/ACU 等控制面事件（仅在 OPCOS 有真实来源时实现）。
- [ ] blocked/external：DevBox 401（缺 token）和 RVM screenshot/computer-use
  的 `convert: not found`（主机缺 ImageMagick），不当作 OPCOS bug。

### Completed smaller findings

- [x] Home model selector now chooses the first discovered chat-capable model instead
  of sending `auto` to gateways that reject it.
- [ ] `run_state=error` 的 session，Info pane 仍显示 `Ready`。
- [ ] Home composer 的 Workspace 不会在创建之间清空，可能拼接出错误路径。
- [ ] 没有 session-delete affordance：frontend 和 `src-tauri` 都没有
  `delete_session` command；`deleteQuestion` / `sessionActions` i18n keys 未使用。

Compaction summary cap 的提高尚未在真实运行中触发：四轮真实 summary 约
1.5–2.1k 字符，只有边界 unit test 覆盖该 cap。

## 已完成能力

### Agent runtime、Host 和安全边界

- [x] 会话 transcript、pending approval、Inbox、审计、暂停/恢复和 builtin
  TurnEngine（历史 #15–#17）。
- [x] LocalHost/RVM Host、能力探测、远程显式失败、远程路径 containment 和
  SecretStore token 边界（历史 #14–#17）。
- [x] OpenCode harness 和独立 ACP harness（历史 #15–#17）。
- [x] provider registry、动态模型发现和缓存（#44–#45）。

### 编码、Git、PR 和 CI

- [x] 文件读写、目录列举、shell 执行和 `edit_file` 精确原子编辑（#48）。
- [x] repository index 的 symbol/glob/search 查询（历史 #38）。
- [x] Git status/diff/log/rev-parse、建分支、显式文件 commit 和 GitHub-only
  push；GitHub PR 创建/读取/评论/reviewer 和 delivery verification（#45）。
- [x] GitHub Enterprise Server 实例身份：host 白名单、`/api/v3` 归一化、
  凭据按实例绑定（`github@<host>`）、canonical push/授权/事件身份带实例。
- [x] GitHub Actions status 和失败 job log 查询；输出和日志有界返回（#47）。
- [x] background jobs 的 start/status/output/kill，使用 job id 和截断元数据
  表达异步结果（#46）。
- [x] 本地 LSP definition/references/diagnostics（#50）。
- [x] 远程 LSP：远程主机在 `/mcp` 上暴露 `lsp` tool 时走主机自带的 LSP 服务，
  探测不到则不声明能力且不退回本地 language server。

### 计划、持久化和自治底座

- [x] action ledger：外部动作幂等键、in-flight/succeeded/failed 状态和结果
  摘要（#38）。
- [x] durable work queue：claim、lease、renew、bounded retry、dead-letter、
  cancel 和手工 requeue（#39）。
- [x] tracked plan：结构化 `propose_plan`、`plan_get/update/revise`（#49）。
- [x] autonomous goals、planning rounds、事件总线、event cursor/rule/dispatch
  持久化（#40–#43）。
- [x] learned skills：显式保存/检索、版本链、stale/conflict 标记和凭据拒绝；
  不注入 system prompt（#51）。
- [x] Leader-only coordination：已存在 Worker 派发、持久化预算、异步 status、
  Worker result 不自动完成（#52）。

### Assets、Commands 和连接器

- [x] Instructions/Agents/Knowledge/Playbooks/Skills/Blueprint/config object
  发现、版本和 builtin seed（#34）。
- [x] `.agents/commands`：严格 frontmatter、变量校验、required/default/unknown
  参数错误和纯文本展开；不执行动作、不进入 `system_instructions`（#53）。
- [x] `.agents/mcp`：JSON/YAML/YML discovery；默认 disabled，不自动连接；凭据
  只能引用 SecretStore（#53）。
- [x] 多种 provider/connector catalog、OAuth/Token 配置和少数已实现 agent
  connector tools；catalog 不等于完整业务工具覆盖（历史 #33–#45）。

## 已知限制

- 远程 LSP 的 language server 生命周期、文档同步和索引进度属于远程主机，
  OPCOS 拿不到 document version，也无法把还在索引的结果标为不完整。
- 远程原始 stdio 通道仍不存在；需要自己持有 language server 的场景（主机
  未注册的 server、交互式 DAP）不可用。
- background jobs 依赖 Host 的 `process_stream` 或 `pty`；job/进程生命周期
  没有跨应用重启接管契约，远程 PTY 进程也没有可靠的孤儿恢复语义。
- `git_push` credential path 只允许 `github.com` 和已登记的 GHES 实例；
  其他 forge 未实现。
- CI 查询只支持 GitHub Actions；没有通用 CI adapter，也没有自动修复 CI 到
  绿色的循环。
- ACP harness 不经过 builtin `tool_definitions`/`ToolExecutor`，因此不能使用
  coordination tools；协同也不会自动为 ACP/OpenCode 建立工具桥接。
- coordination 只接受当前 builtin Leader 派发到已有 Worker；不创建新 session、
  不递归派生。Worker 自述或 `[[COORD]] result` 不等于完成，必须走 branch、
  push、PR 和 GitHub API 核验。
- Commands 只展开文字；MCP 只发现不会自动启用/连接；两者都不是隐式执行后门。
- Browser/CDP/VNC/computer_use 依赖 Host capability，尚无通用确定性业务动作循环。
- connector catalog 中很多项目只有配置/身份探活，未提供完整 agent read/write tools。
- 不存在 Devin Cloud runtime、v3 endpoint、自动账号创建或自动切号。

## 未完成事项与阻塞原因

### [ ] 通用 CI provider 和 CI 修复循环

当前代码只有 GitHub Actions 查询和失败日志读取。没有抽象其他 CI provider，
也没有把“读取失败→编辑→重新运行→再次核实”实现为 durable 自动循环。阻塞点
是 provider/权限/重试/取消语义尚未统一，不能把现有两个 Read tool 写成已完成
的自治修复能力。

### [ ] 远程原始 stdio 通道

远程 LSP 已经通过主机自带的 `lsp` MCP tool 实现，但还没有原始双向 stdio 通道：
OPCOS 无法在远程主机上跑主机未注册的 language server，也无法自己持有 LSP/DAP
客户端。需要主机侧提供白名单 spawn + 持久化字节流 + 生命周期/退出码协议；PTY
不能代替，因为终端模式会改变字节。

### [ ] 可恢复的跨重启 background jobs

当前 job manager 能返回 job id、状态和有界输出，但没有跨重启的远程进程身份、
重新连接、孤儿回收和输出持久化协议。补做前不能宣称 durable background job。

### [ ] 非 GitHub forge 的安全 push/PR 闭环

push credential validation 只接受 `github.com` 和已登记的 GHES 实例，
PR/delivery verification 也使用 GitHub API。GitLab 等其他 forge 需要各自的
凭据、URL 校验、PR 模型和审计契约；当前没有实现。

### [ ] Agent-driven CI repair loop

CI 工具是 Read-only 观察面；它不会自动启动修复 turn、修改代码或重跑检查。
需要定义失败触发、预算、幂等、审批和停止条件，当前没有这些代码。

### [ ] ACP/外部 harness 协同工具桥

ACP session 走独立 harness，不经过 builtin tool catalog；需要单独的协议桥、
权限映射和 Leader/Worker 身份绑定。当前没有实现，也不能假设 ACP 能调用
`coordination_dispatch`。

### [ ] 完整 Computer-use 业务循环

当前只有 Host capability、VNC/CDP/browser surface 和部分交互入口，没有通用
截图→定位→动作→校验→重试的确定性 actuator，也没有完整账号/Host 生命周期
编排。缺少稳定的 Host capability contract 和业务动作安全模型。

### [ ] 完整 connector agent tools

大量 connector 目前停留在配置、OAuth 或身份验证；只有少数 connector 有
agent dispatch。需要逐 connector 定义 API schema、风险和审批，不以 catalog
条目数量代替完成。

### [ ] 发布与真机覆盖

仓库有本地 Tauri 构建和门禁命令，但每种 Host/provider/connector/harness
组合的真机端到端覆盖仍不完整；缺少证据的组合不应标记为已验证。
