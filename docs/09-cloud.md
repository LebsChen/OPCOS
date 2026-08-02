# 09 OPCOS Cloud

云端是**可选**的。断网、无账号、无云时 OPCOS 全部核心功能必须照常——这是定位，不是权衡项。

## 9.1 参照系统的云端边界

| 系统         | 云端做什么                                                                                                    |
| ------------ | ------------------------------------------------------------------------------------------------------------- |
| Devin        | 全部：agent 循环、执行、编排、知识、计量。Outposts 只把「执行」放回你的机器，循环仍在云上［Devin文］          |
| Tembo        | 全部；但提供完整 self-hosted（API + cron + agent workers + PostgreSQL 16 + Redis 7 + S3 兼容存储）［Tembo文］ |
| OpenWork/Den | 只做控制面：身份、RBAC、分发、托管 worker、托管模型、策略、分析［OW文］［OW界］                               |

OPCOS 照 Den。

## 9.2 必须照抄的三个设计

**1）单一 Cloud URL**［OW文］
桌面端只配置一个 `baseUrl`（Den 默认 `https://app.openworklabs.com`），REST 走 `/api/den/v1/...`，MCP 走 `/api/den/mcp/...`，全部从它派生。OPCOS 不要出现「API 地址」「MCP 地址」两个配置项——自托管时一改全改。

**2）双认证通道**［OW文］
用户会话用 `Authorization: Bearer <session-token>`；机器对机器用组织 API key（Den 用 `x-api-key`）。两者权限不同，审计里要能区分。API key **只存哈希**。

**3）worker 的 destination 是参数不是分支**［OW文］
Den 的 `POST /v1/workers` 带 `destination: local | cloud`，同一套 API 管本机 worker 和云 worker；`POST /v1/workers/{id}/tokens` 返回 `tokens.owner / host / client` 与 `connect` URL；`DELETE` 级联清理 token、runtime 记录和 provider 资源。OPCOS 的 host 抽象要长成同一个形状，见 [04](04-host-protocol.md)。

健康由 worker 主动心跳上报（Den 的 `activity-heartbeat`），不是中心轮询［OW文］。

## 9.3 形态演进

**A —— Broker（最小）**
只做托管 OAuth：Slack / Linear / Jira / Sentry / Notion 一键连接，token 通过回调表单回传本机 loopback，**云端不存连接器 token**（OpenWorker 本地实现就是这么做的［OWK码］）。手工粘贴 token 的路径永远保留。

**D —— 分发与授权**
把 [06](06-capability-model.md) 的配置对象和插件搬上云：不可变版本 + 授权到 member/team/org + 从 GitHub 仓库导入。即使只有一个人用，也解决多机器/多仓库复用同一套配置。

**B —— Relay（解决公网入站）**
本地只出站长连接；relay 提供稳定公网端点接 webhook（GitHub PR 事件、Sentry 告警、Slack 提及），把事件推给本机执行。等价于隧道 + 事件路由，但不暴露本机端口。

**C —— Fleet（云执行）**
待服务会话队列 + 出站认领，照 Outposts 的原子 claim + 租约超时回队［Devin文］。这是唯一引入云执行的形态，必须可完全关闭。

顺序：**A → D → B → C**。A/D/B 都不改变「循环在本地」。

## 9.4 远程 OAuth 的坑［OW文］

OpenWork 文档明确记录：OpenCode 跑在远程主机时，本机浏览器打不开远程 host 上的回调地址（`http://127.0.0.1:19876/mcp/oauth/callback`）。官方解法是 SSH 本地端口转发：

```sh
ssh -o ExitOnForwardFailure=yes -L 127.0.0.1:19876:127.0.0.1:19876 user@remote-host
```

并明确**不应**把回调绑到 `0.0.0.0`，也**不应**改成公网 redirect；没有 device-code 兜底。

OPCOS 面临同样的拓扑（本机 UI + 远程执行），设计 OAuth 时必须先确定回调落在哪一侧，并把隧道边界写进文档，不要指望用户自己想明白。

## 9.5 安全基线（如果真的做云端）［OW文］

- 全站 HTTPS；数据库、对象存储、备份、日志加密。
- 敏感列用 AES-256-GCM 单独加密，密钥独立于数据库凭据。
- API key、SCIM token 只存哈希；SSO 配置加密存储。
- 特权路由（SSO / SCIM / API keys / 角色 / 计费）要求**最近 15 分钟内创建的会话**。
- 审计负载剔除 bearer token、API key、SCIM token、SAML 证书。
- 至少保留两个 Owner；Admin 默认不自动获得安全配置权限。

## 9.6 P3 Cloud A：托管 OAuth broker（已实现本地流程）

Cloud A 是可选的 OAuth 回调 broker，不是云端 agent、事件 relay 或执行平面。Cloud 默认关闭；关闭时 OPCOS 的 Host、会话、工具、审批、Inbox、连接器手工 token 路径和本地存储全部照常工作。

实现位置：

- `crates/opcos-cloud-broker`：可自托管的 broker，使用：
  ```sh
  cargo run -p opcos-cloud-broker
  ```
- `src-tauri/src/main.rs`：桌面端 Cloud 开关、授权启动、轮询和本地 SecretStore 写入。
- Settings → Cloud：沿用现有 Settings sub-nav、`settings-row`、`form-grid` 和按钮 class；未打开开关时不会启动授权请求。

### 流程

1. 本机生成 OS CSPRNG PKCE verifier，并只向 broker 发送 S256 challenge。
2. broker 生成一次性 CSPRNG `session_code` 和 OAuth `state`，返回 authorize URL；客户端请求不携带 redirect URI，本地没有回调地址。
3. 浏览器访问 provider，provider 回调 broker 的 `/oauth/callback`。
4. 本机以指数退避向 `POST /v1/oauth/sessions/token` 发送 JSON body（`session_code` + `code_verifier`）；verifier 不进入 query string，broker 先验证 PKCE，再交换 provider code。
5. broker 将 token 仅作为一次性响应返回；本机立即写入本地 keyring/加密 SecretStore，broker 内存中的 session 随即删除。

Session code 有五分钟 TTL、一次性消费和常量时间比较。OAuth state 也使用常量时间匹配。broker 不把 access token、refresh token、verifier 或 client secret 写入日志、错误或 URL；provider client secret 只存在 broker 的运行时配置中。

由于 OAuth authorization-code exchange 需要 OAuth app 的 client secret，broker 在轮询完成时会短暂接收 verifier 以验证发起方并完成 exchange；verifier 不落库、不记录，且 token 不在 broker 侧持久化。这是“凭据不落云”的边界：用户 token 只落本地 SecretStore，broker 只托管 OAuth app 凭据和短生命周期交换状态。

OAuth authorization 和 token exchange 两次都使用 broker 计算出的同一个 callback URL，避免 provider 的 `redirect_uri_mismatch`。

本轮没有公网部署，也没有硬编码域名。自托管 broker 的 `public_base_url`、provider client id/secret、authorize URL 和 token URL 全部通过环境变量配置。回环测试覆盖：创建 session、state callback、正确 callback URL 的 token exchange、body 中的 PKCE verifier、token 一次性返回、过期 session 清扫和并发 session 超限（429）。

### 尚未实现

- Cloud B：事件 relay / webhook 公网入站；
- Cloud C：云端执行 fleet；
- Cloud D：配置对象分发、组织授权和云端同步；
- 生产公网 broker 部署、域名、TLS 和 provider-specific OAuth app 配置。

启动自托管 broker 时需要显式提供配置，不存在默认公网域名：

```sh
OPCOS_BROKER_BIND=127.0.0.1:8787 \
OPCOS_BROKER_PUBLIC_BASE_URL=https://broker.example \
OPCOS_BROKER_PROVIDER=linear \
OPCOS_OAUTH_CLIENT_ID=... \
OPCOS_OAUTH_CLIENT_SECRET=... \
OPCOS_OAUTH_AUTHORIZE_URL=https://provider.example/oauth/authorize \
OPCOS_OAUTH_TOKEN_URL=https://provider.example/oauth/token \
OPCOS_OAUTH_SCOPES='read write' \
cargo run -p opcos-cloud-broker
```

示例中的域名只是配置占位符，不属于 OPCOS 内置地址；client secret 只通过 broker 进程环境注入，不进入仓库或客户端。
