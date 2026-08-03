# OPCOS

OPCOS 是一个 provider-neutral、local-first 的桌面 agent 工作台。它在本机运行
agent loop、会话状态、审批和 SQLite 持久化；通过统一 Host trait 在 LocalHost
或 RVM Host 上执行工作。OPCOS 不是任何单一 agent 云的客户端，也不运行
Devin 服务。

## 当前边界

- 本地 agent loop 是主要执行路径；OpenCode harness 也可通过 Host 启动。
- ACP 是独立 harness 路径，不经过 builtin `TurnEngine` 的工具目录和
  `ToolExecutor`，因此 ACP session 当前不能使用 OPCOS 协同工具。
- 远端 Host 不可用时返回明确错误，不回退到本机。
- RVM host 端不在本仓库修改；RVM token 只通过 SecretStore 注入
  `Authorization: Bearer` header。
- Git push 的凭据路径只允许 `github.com` remote；GitHub API/PR 与 CI 工具也
  只实现 GitHub 路径。

## Architecture

### Workspace crates

- `opcos-engine`：agent loop、harness、工具定义、审批和执行器抽象。
- `opcos-provider`：provider registry、模型发现和 provider 适配。
- `opcos-rvm`：RVM wire-protocol client。
- `opcos-hosts`：LocalHost/RVM Host、能力查询、文件、进程和路径边界。
- `opcos-mcp`：MCP server 生命周期、工具发现、缓存和 SecretStore 适配。
- `opcos-store`：SQLite 会话、消息、审批、审计、动作账本、队列、计划、
  learned skills、事件和项目实体。
- `opcos-assets`：Instructions、Agents、Knowledge、Playbooks、Skills、
  Commands 和 MCP 声明发现/解析。
- `opcos-policy`：工具风险、路径和审批策略。
- `src-tauri`：Tauri adapter、SQLite adapter、Host dispatch、GitHub/connector
  dispatch 和前端桥接。
- `web`：React/TypeScript/Vite UI。

分层约束：

- `opcos-rvm` 不依赖 `opcos-engine`；
- `opcos-engine` 不依赖 Tauri 或前端；
- `src-tauri` 是 adapter，不是另一套 agent runtime；
- 前端只通过 Tauri invoke/event channel 与桌面端通信。

## 已实现能力

### 会话、模型和资产

- builtin TurnEngine 会话、transcript、pending approval、Inbox、暂停/恢复和
  结构化事件；
- OpenCode harness；ACP harness 独立接入；
- provider registry 和 API 动态模型发现/缓存；
- global/project/repo/host/session 配置对象和 builtin preset；
- `.agents/commands/*.md` 参数化 prompt command：只做纯文本展开，不执行
  shell/Git/MCP，只有用户/UI/slash command 能显式触发，模型不能自行调用；
- `.agents/mcp/` 的 JSON/YAML/YML 发现；发现结果默认 disabled，不自动连接；
- learned skill 显式保存、检索、版本关系、stale/conflict 标记和凭据拒绝。

### Host 与代码工作流

- LocalHost 与 RVM Host；
- Host health/capability 查询；
- 文件读写、目录列举、shell 执行；
- `edit_file` 精确、原子、多替换编辑；
- repository index 的 symbol/glob/content 查询；
- 本地 LSP definition、references、diagnostics；
- Git status/diff/log/rev-parse、建分支、显式文件 commit、受限 push；
- GitHub PR 创建、读取、评论、reviewer 操作和交付核验；
- GitHub CI status 与失败 job log 读取；
- 本地和远程 background job，输出有界截断；
- Desktop/VNC/CDP surface 取决于绑定 Host capability。

### 计划、队列和协同

- durable `work_queue`：claim、lease、renew、bounded retry、dead-letter 和
  手工 requeue；
- tracked execution plan：`propose_plan`、`plan_get`、`plan_update`、
  `plan_revise`；
- autonomous goal/planning round 持久化和事件规则；
- action ledger：外部动作的幂等 key、状态、结果摘要和历史；
- `coordination_dispatch` / `coordination_status`：仅 builtin Leader session
  可派发给已存在 Worker；不创建 session、不递归派生；状态保持
  `worker_reported`、`awaiting_verification`、`verified_delivery`、
  `awaiting_acceptance`、`done` 等区别，Worker 自述不是完成证据。

### 外部连接器

Provider 和 connector catalog 覆盖多种 API/OAuth/IMAP 配置；agent tool 只对
少数已实现路径开放，例如 GitHub、Linear、Notion、GitLab、Jira 和 Stripe 的
部分读写操作。catalog、连接验证和完整 agent business tools 不是同一件事；
未列出的操作不能视为已经支持。

## 已知限制

- LSP 只在 LocalHost 上提供结构化客户端；远程主机即使声明 `lsp`，也因没有
  structured remote LSP channel 而明确不可用。
- background job 依赖当前 Host 的进程流/PTY capability；job 状态保存在当前
  adapter/job manager 路径，不能承诺跨应用重启恢复。远程进程受远程 PTY/进程
  流生命周期限制，不能承诺孤儿进程可被重新接管。
- Git push credential validation 只允许 `github.com`；其他 forge 不可用。
- CI 工具只查询 GitHub Actions；没有通用 CI provider，也没有“CI 挂了自动修
  到绿”的闭环。CI 工具返回状态和有界失败日志，后续修复仍由 agent loop
  再次编辑/验证。
- 协同工具只对 builtin TurnEngine 生效；ACP/OpenCode 不自动获得同一工具目录。
- coordination Worker result 不会自动推进 Done；真实交付必须经过 branch、
  push、PR repository/head 和 GitHub API 核验，再按 acceptance 规则收口。
- Commands 不执行动作；MCP repository discovery 不等于 enable/connect。
- Browser/Computer-use 不是通用确定性业务 actuator；必须依赖 Host 声明的
  capability，当前没有完整截图→定位→动作→校验业务循环。
- connector catalog 不代表每个 connector 都有完整读写 agent tools；OAuth
  application credentials 由用户提供。
- 没有 Devin Cloud v3 API、账号自动创建/切换或 Devin runtime dependency。

## 安全边界

- 凭据进入 SecretStore，不进入 URL、日志、transcript、工具返回或 UI 展示；
- remote→local 不做静默 fallback；
- remote path 使用 Host/RVM containment/path algebra，不使用本机
  `canonicalize` 绕过边界；
- 外部写操作按工具风险进入审批/Inbox；
- Commands 纯展开，展开后产生的 shell/Git/MCP 请求仍走普通工具与审批；
- MCP credentials 只能引用 SecretStore 名称，仓库声明的 server 默认不启用。

## 开发和验证

需要 stable Rust；不要固定 Rust 1.83。完整门禁：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test
cargo build
(cd web && npx tsc --noEmit && npm run build && npm run format:check)
git diff --check
```

Node/Vite 的版本提示或 chunk-size warning 不是成功构建的替代条件；应以命令
退出码为准。

## 发布

发布产物在本地构建，由维护者上传 GitHub Releases。GitHub Actions 目前只是
可查询的 CI 信号，不是发布路径。Linux 和 Windows 的 Tauri 构建命令以及产物
路径见 `docs/` 和 `AGENTS.md`。

## 文档状态

`todos.md` 是当前实现状态和下一步阻塞项的事实清单。旧的 gap/roadmap 文档
保留历史设计和对比，但每份相关文档开头都标注了当前代码事实，避免把目标态
误读为已实现能力。
