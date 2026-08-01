# 05 MCP

## 5.1 OPCOS 作为 MCP client

P1-2 uses `config_object(kind='mcp')` as the single source of truth. Non-sensitive
transport configuration is stored in the immutable version content; bearer/OAuth
credentials are stored only in SecretStore. Runtime discovery is kept separately
in `mcp_tool_cache`, keyed by `(server_object_id, config_version_id)`, so changing
an object version invalidates the discovery result without mutating history.

The desktop owns one long-lived `McpManager`. Enabled servers are initialized at
application startup and are shut down on application exit. Stdio shutdown always
kills and waits for the child process. Server state remains visible even when
disconnected:

```text
disabled → starting → connected
                    ↘ disconnected → reconnecting → connected
                    ↘ auth_required / failed
```

Reconnect delays are immediate, 500ms, 1s, 2s, 4s, 8s, 16s, then capped at
30s. A stable connection resets the consecutive-failure counter. Unavailable
servers retain their configuration and UI status, but their tools are excluded
from provider requests; there is no automatic failover.

Independent server tools use stable provider names:

```text
mcp__<server_key>__<tool_name>
```

`server_key` is immutable and generated from the object ID. Tool names are
sanitized to provider-safe characters, with deterministic collision handling.
Calls resolve the qualified name back to exactly one configured server.

配置格式对齐 OpenWorker 的 `mcpServers` JSON。OpenWorker 使用全局 `~/.config/coworker/mcp.json` 与 workspace `<workspace>/.coworker/mcp.json`，后者覆盖前者同名 server［OWK码］。OPCOS 应采用同样的合并规则，但配置文件只保存非敏感配置。

```json
{
  "mcpServers": {
    "server-name": {
      "type": "stdio",
      "command": "program",
      "args": [],
      "env": {},
      "cwd": null,
      "enabled": true,
      "include_tools": [],
      "exclude_tools": [],
      "requires_approval": true,
      "auth": null
    }
  }
}
```

字段名来自 OpenWorker `MCPServerDef`：`name,transport,command,args,env,cwd,url,headers,enabled,include_tools,exclude_tools,requires_approval,auth`［OWK码］。OPCOS 的配置对象/版本化关系见 [06](06-capability-model.md)；MCP 连接配置可以作为 `mcp_server` 的 payload。

### Transport

| transport         | OPCOS 目标语义                                                  |
| ----------------- | --------------------------------------------------------------- |
| `stdio`           | 启动本机 MCP subprocess，通过 stdin/stdout 通讯。               |
| `http`            | 远程 HTTP MCP；URL 只能来自已授权配置对象。                     |
| `streamable-http` | Streamable HTTP；与 `http` 同属 HTTP client，但保留原始声明值。 |
| `sse`             | SSE MCP；仅在 client library 明确支持时启用，不得伪称已支持。   |

OpenWorker 当前实际 client 使用 `stdio_client` 和 `streamablehttp_client`，配置识别 `http`、`https`、`sse`、`streamable-http`、`streamable_http`［OWK码］。Tembo 文档明确列出 `stdio`、`http`、`sse` 三种 transport［Tembo文］。

### Secret 与 OAuth

- `${VAR}` 可以在加载时解析，但解析后的值只进入内存/SecretStore，不回写配置文件［OWK码］。
- OAuth server 使用 browser OAuth 2.1 + PKCE + Dynamic Client Registration；token 只存 SecretStore，不写 MCP 配置文件［OWK码］。
- OPCOS 的 RVM token 约束更严格：只允许 `Authorization: Bearer` header，绝不把 token 放入 MCP URL、日志、错误、transcript 或 UI。
- 远程 OAuth 的 callback 拓扑要按 [09](09-cloud.md) 处理；不能把 callback 绑定到公网地址［OW文］。

### Discovery、过滤和审批

连接成功后执行 `tools/list`，保存 tool name、description、input schema 和来源 server；不保存 secret。OpenWorker 的 `MCPManager` 提供 `ensure/tools/call/aclose`，并将发现的 tools 转为 agent tools［OWK码］。

每个 tool 经过三层判定：

1. server `enabled` 与 include/exclude filter；
2. tool policy 和 session 级 `requires_approval`；
3. engine approval/standing rule，并写审计。

配置中的 `requires_approval` 只影响默认策略，不能越过 OPCOS 全局安全策略［推断］。

## 5.2 OPCOS 作为 MCP server（设计，未实现）

当前 OPCOS 尚未提供独立 MCP server endpoint；`opcos-mcp` crate 与 RVM MCP 调用属于 client/adapter 能力，不能宣称已有 server［推断］。

目标 server 应只暴露已经存在、可审计、可绑定 session/host 的工具：

| tool                                 | 目标参数             | 约束                                          |
| ------------------------------------ | -------------------- | --------------------------------------------- |
| `session_list`                       | filter/limit         | 只返回非 secret metadata。                    |
| `session_read`                       | `session_id`         | 只读 transcript，approval arguments 脱敏。    |
| `session_send`                       | `session_id`, `text` | 必须绑定已存在 session 和 host。              |
| `session_interrupt`                  | `session_id`         | 写 audit。                                    |
| `approval_list` / `approval_resolve` | session/call/approve | 二次 policy 检查，写 allow/deny audit。       |
| `host_list` / `host_health`          | host id?             | 返回 capability/health，不返回 token/secret。 |
| `asset_list` / `artifact_list`       | session/kind         | 返回引用和 metadata。                         |
| `worklog_list`                       | session/cursor       | 支持 cursor，不输出凭据。                     |

刻意不暴露：

- `save_host`、`delete_host`、secret/provider key 写入；
- 任意 shell、任意远程 path、任意 GitHub token/PR credential；
- `raw_token_get`、secret metadata value、MCP server headers；
- 未绑定 session 的 `exec`、`write`、`computer-use`；
- 改变全局 policy、cloud owner、组织授权的管理工具；
- 可把插件 MCP URL 临时替换为任意 URL 的参数。

最后一项沿用 Den 的安全事实：plugin MCP connection 的 server name 和 URL 由 config object 推导，调用方不能传 URL［OW文］。不安全的工具即使后端存在，也不加入 server catalog［推断］。

## 5.3 参照系统对照

### Devin MCP

Devin MCP 使用 `https://mcp.devin.ai/mcp` 的 Streamable HTTP，并使用 bearer 认证［Devin文］。已在调研中确认的 13 个 tool 名为：

```text
read_wiki_structure
read_wiki_contents
ask_question
list_available_repos
devin_session_create
devin_session_search
devin_session_interact
devin_session_events
devin_session_gather
devin_playbook_manage
devin_knowledge_manage
devin_schedule_manage
list_integrations
```

tool 参数以 Devin MCP schema 为准；本篇不复制未在 OPCOS 中实现的 provider/org 参数，也不记录任何 credential［Devin文］。

### Tembo MCP

Tembo 支持 `stdio`、`http`、`sse`，并支持 OAuth-based remote MCP［Tembo文］。tool 清单在当前公开抓取文档中未形成可核实的完整固定列表，OPCOS 不编造名称。

### OpenWork/Den MCP

Den 控制面通过单一 Cloud URL 派生 `/api/den/mcp/...`；托管 MCP 另有 OAuth MCP endpoint，具体 server/tool catalog 以 OpenWork 文档和实际页面为准［OW文］［OW界］。OAuth、RBAC 和组织授权不能由本地 OPCOS client 伪造。

### Cloud-Dev agent MCP

Cloud-Dev agent 的 `mcp.js:32-56` 暴露 23 个 tool：shell/file、computer、Git、upload/download、desktop/IDE URL、system_info、LSP、DAP、browser；协议是 `POST /mcp` JSON-RPC Streamable HTTP［CD码］。它适合 OPCOS client 的协议兼容测试，但不应直接复制其 token-in-IDE-URL 边界到 OPCOS RVM API。

## 5.4 实施顺序

1. 先实现 global/workspace `mcpServers` 读取、schema 校验和 secret redaction。
2. 接入 stdio 与 Streamable HTTP；sse 保留显式 capability，未实现时返回 unsupported。
3. 实现 tools/list、include/exclude、逐 tool approval 和 audit。
4. 加入 OAuth callback/SecretStore 流程，先本地 callback，再处理远程拓扑。
5. 最后实现受限 OPCOS MCP server，并对每个 tool 做 session/host/policy 绑定。

## 5.5 MCP 与配置对象的关系

MCP server 配置属于 `config_object.kind=mcp_server`；server 的原始 JSON 保存于 `config_object_version.raw_source_text`，归一化字段保存于 `payload_json`，关系见 [01](01-data-model.md) 和 [06](06-capability-model.md)［推断］。

安装 plugin 时：

1. 校验 plugin manifest；
2. 解析其 `plugin_member`；
3. 对 MCP member 做 schema 校验和 URL/source binding；
4. 生成 session 可见 tool catalog；
5. 逐 tool 走 policy/approval；
6. 在卸载时撤销 session selection，不删除共享 config object。

插件 URL 不应成为任意调用参数；Den 的 config-object 绑定约束正是防止授权后指向另一服务［OW文］。

## 5.6 tool schema 与结果

每个发现的 tool 记录：

```text
server_name
tool_name
description
input_schema
requires_approval
enabled
discovered_at
```

`input_schema` 只作为参数校验，不代表允许访问任意本地/远程资源。执行结果必须保留 `is_error`、结构化 content 和 call id；大结果转 artifact/reference，不把完整二进制塞入 transcript［推断］。

MCP discovery 失败时：

- server unavailable：保留配置，标记 disconnected；
- schema invalid：不加入 tool catalog；
- OAuth required：生成 pending connection 状态；
- tool policy denied：不调用 server，写 audit；
- tool call timeout：返回显式 timeout，不自动换 server。

## 5.7 认证边界

OPCOS RVM agent 的 `/mcp` 由远端 agent 的统一 bearer auth 保护；OPCOS client 只负责把 token 放在 HTTP Authorization header。Cloud-Dev agent 的 MCP 处理是 JSON-RPC `initialize`、`tools/list` 和 tool call［CD码］。

OPCOS 未来 server 的认证应至少区分：

- 本地 Tauri loopback：仅由已启动应用建立的随机 session binding；
- 远程 RVM client：明确 host token；
- Cloud control plane：独立 session/API key/OAuth，不复用 RVM token。

具体 listener、端口和 server-side auth 尚未实现，均为未确认［推断］。
