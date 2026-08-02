# 08 密钥、策略与审计

## 8.1 密钥存储

现状：优先系统 keyring，无 Secret Service 时回落到加密文件（启动打印 `secret_backend=encrypted-file`）。这条回落路径是真实环境里的常态（无 gnome-keyring/kwallet 的机器），不是异常分支——所有 `client_for()` 路径（host 测试、`submit_turn`、PTY/VNC/IDE、资产发现）都依赖它。

引用形式统一为 `secret:<scope>:<NAME>`，值只在使用点解引用，**不落 transcript、不落事件、不落 UI**。

参照：OpenWorker 把 OAuth token 放 SecretStore、**不写进 MCP 配置文件**［OWK码］；Den 把 API key 与 SCIM token 只存哈希、敏感列 AES-256-GCM 单独加密［OW文］。

## 8.2 Token 边界（硬约束）

来自 `AGENTS.md`，任何改动都不得违反：

1. RVM token 只出现在 `Authorization: Bearer` header。
2. token 不进 URL、日志、错误信息、transcript、fixture、UI、截图、录屏、报告、数据库明文。
3. 远程 host 不可用 → 显式报错，绝不静默 fallback 到本地执行。
4. 远程路径用远程路径代数与 containment 检查，不用本地 `Path::canonicalize`。

参照系统的同类做法：Cloud-Dev 的 Web IDE 用统一 token 网关保护文档、asset bridge、`/vscode-remote-resource` 和管理 WebSocket；合法的 query token 会被转成 HttpOnly cookie **并从 URL 移除**［CD码］。这是「不得不用 query 传 token」时的正确收尾方式。

Tembo 文档允许 trigger 用 `apiKey` query parameter［Tembo文］——**OPCOS 不采纳**。

## 8.3 权限与审批

策略层（`opcos-policy`）在工具执行前判定，顺序见 [03](03-lifecycle.md#34-审批)。要点：

- 决策只有 `allow` / `deny`。
- standing rule 是**会话级**授权，不跨会话继承（OpenWorker 的 standing rule 也是这个粒度［OWK码］）。
- 策略拒绝要发结构化 `notice`（含 risk、reason），不是一句纯文本。
- 低风险只读工具（读文件、搜索、git 查询）可并行；其余默认串行［OWK码］。

## 8.4 审计

`audit_events` 表已接通写入。必须覆盖的事件：

| 事件                 | 触发点         |
| -------------------- | -------------- |
| 审批 allow / deny    | 策略层决策     |
| host 增 / 删 / 改    | 设置页         |
| provider key 增 / 删 | 设置页         |
| 会话创建 / 中断      | 引擎           |
| 工具执行被策略拒绝   | 策略层         |
| 生命周期阶段失败     | Blueprint 执行 |

约束：

- **审计负载绝不包含 token、密钥值、Authorization header**（Den 明文规定审计负载剔除 bearer token、API key、SCIM token、SAML 证书［OW文］）。
- 审计只追加，不修改不删除。
- Activity 页面读真实审计数据，不允许展示构造出来的示例。

## 8.5 隐私姿态

Den 的 Analytics 页面明写「只采集事件元数据，不采集 prompt、代码或文件内容」［OW界］。OPCOS 默认更强：**没有任何遥测**。若未来加入可选遥测，必须默认关闭、可完全禁用，且采集范围公开列明。

这不是合规话术，是 OPCOS 相对云平台的核心差异之一（见 [00](00-architecture.md)）：代码、密钥、transcript 全部留在本机 SQLite 与加密存储里。

## 8.6 多身份（如果做云端）

本地单人使用不需要 RBAC。真要做云端时照 Den［OW文］：

- 三个默认角色 Owner / Admin / Member + 自定义角色；只有 Owner 能改角色。
- Teams 用于资源授权，Roles 用于组织管理权限，两者分离。
- 特权路由要求最近 15 分钟内创建的会话。
- 至少保留两个 Owner；Admin 默认不自动获得安全配置权限。

不要在本地版提前引入这套概念——它会污染单人使用的数据模型。
