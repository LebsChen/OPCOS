# OPCOS

OPCOS 是一个本地化的 OPC（Open/Personal Computer）工作台：
桌面端负责会话、界面、凭据和远程主机连接，远程执行仍由 Cloud-Dev 现有的
`dev-agent` 提供。OPCOS 不替换或部署 RVM host，而是一个 client-only 的
wire-protocol client。

项目由 Rust workspace、Tauri v2 桌面壳和 React/TypeScript 前端组成。

## Architecture

### Workspace crates

- `opcos-engine`：agent loop、会话状态、工具调用、审批和执行器抽象。
- `opcos-provider`：Provider registry、模型矩阵以及 OpenAI-compatible、
  Anthropic、Bedrock 等 Provider 适配。
- `opcos-rvm`：Cloud-Dev RVM HTTP/WebSocket wire protocol client。
- `opcos-hosts`：本机 Host 和远程 RVM Host 的统一 Host trait、能力查询及执行。
- `opcos-mcp`：MCP server 生命周期、连接、工具发现和缓存。
- `opcos-store`：SQLite 会话、消息、资产、审计和设置存储，以及 SecretStore
  接口。
- `opcos-assets`：Agents、Instructions、Knowledge、Playbooks、Skills 和
  Blueprint 资产发现、解析与版本管理。
- `opcos-policy`：路径、工具、审批和 unattended execution 的安全策略。
- `src-tauri`：Tauri 桌面适配层，负责命令注册、窗口事件、本地数据库、
  SecretStore、RVM client 和前端桥接。
- `web`：React/TypeScript/Vite 用户界面。

分层约束：

- `opcos-rvm` 不依赖 `opcos-engine`；
- `opcos-engine` 不依赖 Tauri 或前端；
- 跨层行为通过 trait 表达；
- `src-tauri` 是桌面适配层，不是独立的 agent runtime；
- RVM host 端保持 Cloud-Dev `dev-agent` 不变。

## Current capabilities

以下清单以当前 `dev@92e9409` 代码为准，不代表尚未实现的产品愿景。

### Sessions and automation

- 会话创建、恢复、消息流和持久化 transcript；
- builtin agent loop 与 OpenCode harness；
- 工具调用、执行结果、暂停/恢复和审批；
- interactive、discuss、auto 等权限模式；
- Schedules、Automations 和 trigger callback；
- Repository index 构建、刷新和查询；
- Blueprint 读取、生命周期执行，以及明确标注的远程命令执行入口。

### Hosts

- 本机 Host；
- 远程 RVM Host；
- Host health 和 capability 查询；
- 远程 Host 不可用时返回明确错误，不静默回退到本机；
- Hosts 卡片网格、编辑、测试和删除；
- RVM token 通过 SecretStore 保存，不在界面回显。

### Providers

当前 registry 中有实际代码路径的 Provider 包括：

- OpenAI；
- Anthropic；
- Gemini；
- AWS Bedrock；
- DeepSeek；
- Together AI；
- Kimi / Moonshot；
- MiniMax；
- Qwen / DashScope；
- xAI；
- Mistral；
- Meta；
- Fireworks AI；
- OpenRouter；
- Z AI；
- Ollama。

Vertex AI 当前保持 `unavailable`，不伪造可用状态。Provider 页面支持
目录、模型矩阵、base URL、密钥保存和密钥验证；Provider 目录可用不等于用户
已经配置了对应凭据或默认模型一定适合其账户。

### Connectors

#### Token and direct API connectors

以下 connector 支持 token、PAT、API key 或服务账号等直接配置方式，并通过
真实身份/验证请求确认连接状态：

- GitHub；
- Telegram；
- Discord；
- Slack；
- Linear；
- Notion；
- GitLab；
- Stripe；
- Asana；
- HubSpot；
- ClickUp；
- PagerDuty；
- PostHog；
- Apollo.io；
- Hunter；
- Close；
- Attio；
- Clay；
- Figma；
- Descript；
- monday.com；
- Jira；
- Confluence；
- Zendesk；
- Datadog；
- Mixpanel；
- Amplitude。

部分 connector 需要额外字段，例如 Jira/Confluence 的 site、email 和 API
token，Zendesk 的 subdomain、email 和 API token，Datadog 的 site、API key
和 application key。

当前只为少数低风险读操作提供 agent tools：

- Notion search；
- GitLab projects/issues；
- Jira JQL issue search；
- Stripe charges list。

其余本批 connector 目前只提供连接和身份验证，不宣称已经提供完整的 agent
读写工具。

#### OAuth and other integrations

- Gmail、Google Calendar、Google Drive：共享 Google OAuth 授权码流程；
- Outlook：Microsoft Graph OAuth；
- Salesforce：OAuth，并保存授权返回的 instance URL；
- QuickBooks：OAuth，并保存 realm ID；
- Docusign：OAuth；
- Canva：OAuth 2.0 + PKCE；
- Dropbox：OAuth offline access；
- Box：OAuth；
- WhatsApp：Cloud API Bearer token、phone number ID 和 Graph version；
- Email (IMAP)：真实 IMAP LOGIN 验证，可配置 TLS、host、port、username
  和 password；
- Browser：检查当前绑定 Host 的 browser/CDP capability，通过主机能力使用，
  不伪造独立浏览器 runtime。

OAuth connector 使用本机随机端口的一次性 callback listener、随机 state 和
PKCE（适用时），并在授权完成后调用身份端点。用户需要为 OAuth connector
自行创建并填写 OAuth application credentials。

### MCP

- 手工配置任意 HTTP、Streamable HTTP 或 stdio MCP server；
- MCP server 状态、重试、工具发现和缓存；
- MCP 工具 Enable/Disable 状态切换；
- SecretStore 凭据注入，不把 bearer token 放进 MCP 配置 JSON。

### Secrets

Secrets 页面支持新增和删除 secret metadata 及对应 SecretStore 值。secret
值使用密码输入，不在列表或状态响应中回显。

## Security boundaries

- RVM token 只通过 `Authorization: Bearer <token>` header 发送；
- connector token、OAuth access/refresh token、IMAP password 和其他凭据进入
  SecretStore，不进入 URL、日志、transcript、错误字符串、命令返回值或 UI
  展示值；
- OAuth state、PKCE verifier 和临时授权数据只在授权流程中使用；
- 远程 Host 不可用会明确报错，绝不静默回退到本机执行；
- 前端通过 Tauri `invoke` 和 event channel 与桌面端通信，不使用 Python HTTP
  sidecar；
- 远程路径使用 Host/RVM 的路径约束，不通过本机路径 canonicalization 绕过
  containment check。

## Development and local verification

需要 stable Rust；不要将 Rust 固定到 1.83。安装前端依赖后，可在仓库根目录
运行完整门禁：

```bash
cargo fmt --all -- --check
cargo build
cargo test
cargo clippy --workspace --all-targets -- -D warnings

cd web
npm install
npx tsc --noEmit
npm run build
npm run format:check
cd ..

git diff --check
```

也可以使用 `AGENTS.md` 中的组合命令。Node.js 版本应满足当前 Vite 要求；
构建时若出现 chunk size warning，它是性能提示而不是构建失败。

## Local release process

Release 产物在本地构建，再由维护者按发布流程上传到 GitHub Releases；GitHub
Actions 只作为 lint/test 信号，不是当前发布路径。

### Linux

安装 Tauri CLI：

```bash
cargo install tauri-cli --locked
```

安装系统所需的 WebKitGTK/AppIndicator 开发包，然后从仓库根目录运行：

```bash
npm --prefix web ci
APPIMAGE_EXTRACT_AND_RUN=1 cargo tauri build --bundles deb,appimage
```

产物路径：

```text
target/release/opcos
target/release/bundle/deb/OPCOS_0.1.0_amd64.deb
target/release/bundle/appimage/OPCOS_0.1.0_amd64.AppImage
```

### Windows x64

在 Linux 环境使用 `cargo-xwin` 交叉构建：

```bash
cargo tauri build \
  --runner cargo-xwin \
  --target x86_64-pc-windows-msvc \
  --bundles nsis
```

产物路径：

```text
target/x86_64-pc-windows-msvc/release/opcos.exe
target/x86_64-pc-windows-msvc/release/bundle/nsis/OPCOS_0.1.0_x64-setup.exe
```

发布前应为产物生成 SHA-256 校验和，并将本地生成的 payload 上传到对应的
GitHub Release。当前开发版 Release：

[v0.1.0-dev.1](https://github.com/LebsChen/OPCOS/releases/tag/v0.1.0-dev.1)

构建提示可能包括 Node.js 版本建议、Vite chunk warning、Windows
`LNK4099` PDB 缺失警告、未签名安装器和 cargo-xwin experimental warning；
只要命令退出成功，这些提示不会改变产物是否生成。

## Development status and roadmap

- Vertex AI 仍未接入；
- OAuth connector 需要用户自备 OAuth application credentials；
- OAuth connector 当前重点是授权、token 刷新、身份和连接状态，不代表已
  有完整的 agent tools；
- Browser connector 依赖当前绑定 Host 提供 CDP/browser capability；
- IMAP 当前只做连接和 LOGIN 验证，完整收发 agent tools 后续再做；
- GitHub Actions 仅作 lint/test 信号，不作为 release 发布路径；
- 后续可继续扩展 connector agent tools、OAuth 应用管理和更多 Provider 的
  账户级验证。
