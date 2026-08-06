# 04 Host 抽象与 dev-agent 线协议

## 4.1 统一 host 抽象

目标态由 `LocalHost`、`RvmHost`、`CloudWorker` 实现同一个 trait；当前 `LocalHost` 与
`RvmHost` 已落在 `opcos-hosts`，`CloudWorker` 仍是后续扩展［推断］。

```rust
trait Host {
    fn id(&self) -> &str;
    async fn capabilities(&self) -> Result<Capabilities, HostError>;
    async fn start(&self, request: StartRequest) -> Result<Handle, HostError>;
    async fn stop(&self, handle: &Handle) -> Result<(), HostError>;
    async fn health(&self) -> Result<Health, HostError>;
    async fn exec(&self, request: ExecRequest) -> Result<ExecResult, HostError>;
    async fn read(&self, path: &RemotePath) -> Result<FileRead, HostError>;
    async fn write(&self, path: &RemotePath, content: &[u8]) -> Result<FileWrite, HostError>;
    fn join(&self, base: &RemotePath, child: &str) -> Result<RemotePath, HostError>;
    fn contains(&self, root: &RemotePath, candidate: &RemotePath) -> bool;
}
```

`LocalHost` 可以使用本机 worker；`RvmHost` 只调用现有远程 RVM；`CloudWorker` 是 Den 风格的可选云目标。Den 的 worker 创建使用 `destination: local|cloud`，token API 另返回 owner/host/client token 与 connect 信息［OW文］；这说明执行位置应是同一抽象的参数，不应复制两套 session API。

硬约束：

1. RVM host 侧不修改，OPCOS 是 client-only。
2. token 只进入 `Authorization: Bearer` header，不进入 URL、日志、错误、transcript、fixture、UI。
3. 远程 host 不可用必须返回显式错误，禁止本地 fallback。
4. 远程路径使用远程路径代数和 containment check，禁止用本地 `Path::canonicalize`。
5. capability 探测结果必须标记来源和时间，不能把未探测能力伪造成可用。

当前 OPCOS 已有 `HttpRvmClient`、health/info、exec、read/write、worklog、MCP、Git 和路径保护；LocalHost、CloudWorker 的完整 runtime 尚未实现［推断］。

## 4.2 dev-agent HTTP 协议

Cloud-Dev agent 除 health 外由统一 auth middleware 检查 bearer token；失败返回 `401 {error:"unauthorized"}`［CD码］。

| 方法 | 路径                    | 请求                                                | 响应                                                                                | 认证   | 是否流式               |
| ---- | ----------------------- | --------------------------------------------------- | ----------------------------------------------------------------------------------- | ------ | ---------------------- |
| GET  | `/api/health`           | 无                                                  | `status,service,version,platform,host,workspace,vnc_port,ide_port,capabilities,pid` | 免认证 | 否                     |
| POST | `/api/exec`             | `cmd`/`command`、`cwd`、`timeout`、`session`、`env` | `status:"completed"`, `result`（stdout/stderr/exit_code）                           | bearer | 否；请求内等待         |
| POST | `/api/exec-sync`        | 同 `/api/exec`                                      | 同上                                                                                | bearer | 否；请求内等待         |
| POST | `/api/read`             | `path`                                              | `path,content,size`                                                                 | bearer | 否                     |
| POST | `/api/write`            | `path,content`                                      | `ok,path,bytes`                                                                     | bearer | 否                     |
| POST | `/api/ls`               | `path?`                                             | `path,items[{name,dir,size}]`                                                       | bearer | 否                     |
| GET  | `/api/info`             | 无                                                  | `hostname,platform,arch,cpus,memory_gb,uptime_hours,workspace,user,node`            | bearer | 否                     |
| POST | `/api/computer-use`     | `action` 及 action-specific 字段                    | computer result 或 `error`                                                          | bearer | 否                     |
| GET  | `/api/screenshot`       | 无                                                  | screenshot result/image 或 error                                                    | bearer | 否                     |
| GET  | `/api/ping`             | 无                                                  | `pong,time`                                                                         | bearer | 否                     |
| GET  | `/api/capabilities`     | 无                                                  | version/platform/arch/hostname 与 endpoint 分组                                     | bearer | 否                     |
| POST | `/api/upload`           | `path`、`content` 或 `url`、`encoding`              | storage upload result                                                               | bearer | 否                     |
| POST | `/api/download`         | `path`、`encoding`                                  | storage download result                                                             | bearer | 否                     |
| POST | `/api/identity`         | identity JSON                                       | `ok,identity`                                                                       | bearer | 否                     |
| POST | `/api/events/subscribe` | 当前 handler 未读取字段                             | `ok,message`                                                                        | bearer | 否；真实订阅语义未确认 |
| GET  | `/api/events/types`     | 无                                                  | `types`                                                                             | bearer | 否                     |
| ANY  | `/api/storage/*`        | operation-specific fields 未确认                    | storage response                                                                    | bearer | 未确认                 |
| ANY  | `/api/git/*`            | `cwd,path,ref,branch,name` 等按 operation           | git response                                                                        | bearer | 否                     |
| ANY  | `/api/repo/*`           | `path,url,name` 等按 operation                      | repo response                                                                       | bearer | 否                     |
| ANY  | `/api/deploy/*`         | operation-specific fields 未确认                    | deploy response                                                                     | bearer | 否                     |
| ANY  | `/api/vnc/*`            | status/start/stop 字段未确认                        | VNC response                                                                        | bearer | 否                     |
| POST | `/api/ide/start`        | `port,password,workspace`                           | `ok` 等 start result                                                                | bearer | 否；启动在请求内等待   |
| GET  | `/api/ide/status`       | 无                                                  | `running,port,password,url`                                                         | bearer | 否                     |
| POST | `/api/ide/stop`         | 无                                                  | `{ok:true}`                                                                         | bearer | 否                     |

核心 handler 见 `Cloud-Dev/tools/rvm/agent/core.js:396-503`；扩展分派见 `agent.js:507-673`［CD码］。

## 4.3 WebSocket 与代理

| 端点       | 请求/消息                                        | 响应/事件                  | 认证与流式语义             |
| ---------- | ------------------------------------------------ | -------------------------- | -------------------------- |
| `/vnc-ws`  | WebSocket upgrade，转 VNC TCP 字节               | 双向 desktop byte stream   | 长连接；统一 upgrade 路由  |
| `/pty-ws`  | id/session、cols、rows 等连接参数，双向 PTY 字节 | stdout/stderr bytes、exit  | 长连接；不是轮询           |
| `/cdp-ws`  | CDP WebSocket upgrade                            | CDP JSON/bytes             | browser bridge 长连接      |
| `/ide/*`   | document、static、`/vscode-remote-resource` 路径 | code-server proxy response | Web IDE unified token gate |
| `/novnc/*` | 静态资源路径                                     | noVNC assets               | 数据面走 `/vnc-ws`         |

实现位置为 `core.js:518-757,833-1188,1455-1494,1560-1591`；LSP、DAP 和 browser 也通过 agent MCP tool surface 暴露，参数 schema 来自 `lsp.toolSchema()`、`dap.toolSchema()`［CD码］。

OPCOS 不把 MCP/能力声明当成结构化 LSP transport。LSP 需要双向 stdio、独立 stderr、可靠的 `Content-Length` framing 和真实 process exit；RVM 目前只有 PTY/WebSocket 字节流，因此远程 LSP 必须显式报告不可用，不能用 PTY 输出拼接伪协议。

## 4.4 OPCOS 实现矩阵

| 能力                           | OPCOS 现状                                  | 目标                               |
| ------------------------------ | ------------------------------------------- | ---------------------------------- |
| health/info/capabilities       | 已有 RVM client 调用                        | 统一 `Host` trait                  |
| exec/exec-sync                 | 已有；远程不可用显式错误                    | 统一超时、session handle           |
| read/write/ls                  | 已有部分文件操作                            | 全部使用 `RemotePath` 代数         |
| PTY/VNC/CDP                    | Tauri surface relay 已有                    | capability gate、生命周期统一      |
| screenshot/computer-use        | 通过 RVM/engine 路径接入                    | 明确 capability 和结果类型         |
| LSP/DAP/browser                | MCP/协议层未形成统一 OPCOS trait            | 增加 capability-specific adapter   |
| Web IDE                        | IDE proxy 已有                              | 与 host 生命周期和 token gate 统一 |
| upload/download/storage/deploy | upload、asset export 等已有                 | artifact 引用和错误模型统一        |
| LocalHost                      | `opcos-hosts` 进程内实现 exec/read/write/ls | capability 探测与会话能力继续扩展  |
| CloudWorker                    | 未实现                                      | 仅在 cloud 形态启用后增加          |

Den 的 `activity-heartbeat` 是设计参照：worker 主动上报 `lastActiveAt`、`openSessionCount`、`isActiveRecently`，而不是控制面猜测健康［OW文］。OPCOS 本地 host 同样应保存最后主动健康时间［推断］。

## 4.5 能力对象

能力探测返回值建议至少包含：

```text
exec
exec_sync
read
write
ls
pty
vnc
cdp
browser
lsp
dap
screenshot
computer_use
ide
mcp
upload
download
```

每项应有 `available`、`version?`、`reason?`；不能因为服务端返回未知字段就默认 true。Cloud-Dev `/api/health` 返回的 `capabilities` 至少包含 `exec`、`pty`、`screenshot`、`computer_use`，有端口时加入 `vnc`、`code_server`［CD码］。

## 4.6 远程路径代数

`RemotePath` 不等同于本机 `PathBuf`：

1. 由 host 规定 separator、root 和相对路径规则。
2. `join(root, child)` 先拒绝绝对 child、空 segment 和 traversal。
3. `contains(root, candidate)` 在远程路径规范化后比较 segment，不解析本机 filesystem。
4. 文件读写前再次检查 containment；不能只在 UI 侧检查。
5. 错误中可以返回安全的 path label，但不能回显 token、secret 或完整 credential-bearing URL。

OPCOS 当前 `opcos-rvm::path_guard` 已提供远程路径保护；新的 LocalHost/CloudWorker 也必须实现同一语义［推断］。

## 4.7 执行与 session handle

`ExecRequest` 的核心字段是 command、cwd、timeout、session、env；Cloud-Dev agent 对应读取 `cmd|command`、`cwd`、`timeout`、`session`、`env`［CD码］。目标态应：

- 将 `timeout` 变成显式 `Duration` 并由 host 限制上限；
- 将 `session` 作为 host-side persistent shell handle，而不是 UI session id；
- 对 stdout/stderr/exit code 使用稳定结构；
- 对取消、超时、host disconnect 分别给出错误类别；
- 禁止把 env 中的 secret 值写入 audit/worklog。

`exec-sync` 适合短命令和 blueprint 阶段；持久 shell、PTY 和长任务应使用 handle/event 机制［推断］。

## 4.8 生命周期

Host 生命周期建议：

```text
configured
  -> probing
  -> ready
  -> degraded
  -> unavailable
  -> stopping
  -> stopped
```

`unavailable` 不代表自动切换 LocalHost；session 必须保留原 host_id 并显示原因。恢复时重新 probe 同一 host，除非用户明确选择新的 host［推断］。

## 4.9 参照端点补充

Cloud-Dev 的 `/api/ide/start` 将 connection token 传给 code-server，`/api/ide/status` 返回 running/port/password/url，IDE 静态资源和 management WebSocket 另经 token gate［CD码］。OPCOS 可复用“本地 relay + 远程能力”的形状，但 token 传输仍必须遵循 OPCOS header-only 约束。

Cloud-Dev MCP server 的 `lsp`、`dap`、`browser_navigate`、`browser_eval`、`browser_screenshot` 和 `browser_close` tool 说明了能力面如何由 schema 暴露［CD码］。OPCOS 应在 capability 不可用时返回 `unsupported_capability`，而不是把 tool 留在 UI 中。

Den 的 `destination: local|cloud` 和 activity-heartbeat 只作为 API/健康模型参照，不表示 OPCOS 已经有 Den worker API［OW文］。
