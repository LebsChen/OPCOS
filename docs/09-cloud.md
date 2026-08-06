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
