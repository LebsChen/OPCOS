# 02 IPC 契约

前端只通过 Tauri `invoke` 与 event channel 访问内核；agent 事件序列的语义以 [03-lifecycle.md](03-lifecycle.md) 为准，本篇只列 channel 和 payload 形状。

## 2.1 Host、session 与 surface

| 命令              | 参数                                                             | 返回                                                                                   | 失败语义                                                                                                                                                                   |
| ----------------- | ---------------------------------------------------------------- | -------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `list_hosts`      | 无                                                               | `HostView[]`：`id`, `name`, `online`, `reason`                                         | 数据库错误转字符串。                                                                                                                                                       |
| `save_host`       | `id?`, `name`, `url`, `token`                                    | `HostView`                                                                             | 已存在 host 不允许修改；secret 写入失败回滚 host。token 只进入 secret store。                                                                                              |
| `test_host`       | `host_id`                                                        | `HostView`                                                                             | 认证失败与其它远程错误转为 `online=false, reason`，不是静默成功。                                                                                                          |
| `delete_host`     | `host_id`                                                        | `()`                                                                                   | host 不存在时报错，并删除关联 secrets。                                                                                                                                    |
| `start_surface`   | `host_id`, `surface`, `cols?`, `rows?`, `cwd?`                   | 本地 relay port `u16`                                                                  | `surface` 只接受 `pty` / `vnc` / `cdp`；绑定本地 relay 失败时报错。                                                                                                        |
| `ide_bootstrap`   | `session_id`, `folder_uri`                                       | `IdeBootstrap`                                                                         | `folder_uri` 必须以 `vscode-remote://` 开头；session/远程读取失败显式报错。                                                                                                |
| `start_ide_proxy` | `session_id`, `folder_uri`                                       | 本地 proxy port `u16`                                                                  | `folder_uri` 前缀、listener 或远端 proxy 失败显式报错。                                                                                                                    |
| `create_session`  | `title`, `host_id`, `model?`, `provider?`, `mode?`, `workspace?` | `SessionView`                                                                          | host 不存在时报 `remote host not found; session was not created`。                                                                                                         |
| `list_sessions`   | 无                                                               | `SessionView[]`                                                                        | 数据库错误转字符串。                                                                                                                                                       |
| `read_transcript` | `session_id`                                                     | `[{kind,payload}]`，pending approval 转为 `kind=approval`，tool call 按 `call_id` 合并 | store 读取失败报错；approval arguments 先脱敏；store 对无 result 且无 pending 的工具保留 `status=unresolved`，adapter 按活跃引擎覆盖为 `running`，否则返回 `interrupted`。 |

上述命令实现位于 `src-tauri/src/main.rs:989-1381`。`start_surface`、IDE proxy、`test_host` 是长任务/网络操作；它们返回建立结果，不把远程 host 不可用转换为本地执行。

## 2.2 Turn、审批、provider

| 命令                     | 参数                                       | 返回                                           | 失败语义                                                                                            |
| ------------------------ | ------------------------------------------ | ---------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| `submit_turn`            | `SubmitRequest`：至少 `session_id`, `text` | `()`                                           | 先 health；远程不可用返回 `remote host unavailable`；审批挂起和策略拒绝都返回显式错误并发完成事件。 |
| `upload_text_attachment` | `session_id`, `file_name`, `content`       | 远程路径字符串                                 | 文件名、长度、session、workspace、远程写入均校验；不使用本地 canonicalize。                         |
| `interrupt`              | `session_id`                               | `()`                                           | engine 获取/中断失败报错；写 `session_interrupted` audit。                                          |
| `steering`               | `session_id`, `text`                       | `()`                                           | engine 拒绝时报错；完成由异步 `turn_done` 表示。                                                    |
| `resolve_approval`       | `session_id`, `call_id`, `approve`         | `()`                                           | engine 拒绝时报错；写 allow/deny audit；若还有 pending approval 继续发 pending。                    |
| `change_model`           | `session_id`, `model`                      | `()`                                           | provider/engine 切换失败转字符串。                                                                  |
| `change_provider`        | `session_id`, `provider?`                  | `()`                                           | 未知 provider 报错；清理 session engine 以重新构建。                                                |
| `provider_descriptors`   | 无                                         | descriptor 数组                                | 无参数失败语义。                                                                                    |
| `provider_models`        | `provider`                                 | `ModelDescriptor[]`：`id`, `label`, `provider` | 未知 provider 返回对应空/矩阵结果。                                                                 |

实现见 `main.rs:1382-1715`。`submit_turn` 是长任务；文本增量不作为 invoke 返回值，而通过事件 channel 流出。

## 2.3 Assets、MCP、Blueprint、Git

| 命令                      | 参数                                                                             | 返回                                            | 失败语义                                                                 |
| ------------------------- | -------------------------------------------------------------------------------- | ----------------------------------------------- | ------------------------------------------------------------------------ |
| `list_assets`             | `kind?`                                                                          | `id,kind,title,body,trigger,scope,enabled` 数组 | DB 错误。                                                                |
| `save_asset`              | `id`, `kind`, `title`, `body`, `trigger?`, `scope?`, `enabled?`                  | `()`                                            | 仅允许 `knowledge` / `playbook` / `skill` / `agents`。                   |
| `delete_asset`            | `id`                                                                             | `()`                                            | DB 错误。                                                                |
| `set_asset_enabled`       | `session_id`, `asset_id`, `enabled`                                              | `()`                                            | DB 错误。                                                                |
| `export_assets`           | `session_id`, `ids`                                                              | 导出数量 `usize`                                | 远程 host unavailable 或写入失败显式报错。                               |
| `import_assets`           | `session_id`                                                                     | `AssetBundle`                                   | 远程读取/解析/DB 写入失败报错。                                          |
| `discover_remote_assets`  | `session_id`                                                                     | `AssetBundle`                                   | session/host/远程读取失败报错。                                          |
| `mcp_tools`               | `session_id`                                                                     | MCP `tools` 数组                                | RPC/远程错误报错。                                                       |
| `set_mcp_tool_enabled`    | `session_id`, `name`, `enabled`                                                  | `()`                                            | DB 错误。                                                                |
| `read_blueprint`          | `session_id`                                                                     | YAML 转换后的 JSON                              | 文件不存在或 YAML 无效报错。                                             |
| `execute_blueprint`       | `session_id`, `command`, `cwd?`                                                  | exec result JSON                                | 空命令、远程执行错误报错。                                               |
| `run_blueprint`           | `session_id`                                                                     | `{status:"ok", completed:[...]}`                | 任一 phase/command 非零退出即失败。                                      |
| `git_branch_name_command` | `slug`                                                                           | branch string                                   | branch 生成失败。                                                        |
| `git_workflow`            | `session_id`, `operation`, `cwd`, `slug?`, `files?`, `message?`, `secret_names?` | exec result JSON                                | 只允许已实现 operation；危险 git 操作、缺文件/secret、远程错误显式失败。 |

实现见 `main.rs:1699-2220`。export/import、blueprint、git workflow 是长任务；没有独立 streaming 返回。

## 2.4 Review、协调、自动化、审计和 secrets

| 命令                                                                                                 | 参数                                                    | 返回                                    | 失败语义                                                   |
| ---------------------------------------------------------------------------------------------------- | ------------------------------------------------------- | --------------------------------------- | ---------------------------------------------------------- |
| `github_pull_request`                                                                                | `repo`, `title`, `head`, `base`, `body`, `token_secret` | GitHub JSON                             | secret 未配置、字段泄漏、网络/API 错误显式失败。           |
| `review_snapshot`                                                                                    | `session_id`, `cwd`, `base`                             | `{status,changes}`                      | 远程 git 错误显式失败。                                    |
| `review_file_diff`                                                                                   | `session_id`, `cwd`, `path`, `base`                     | diff JSON                               | 远程错误显式失败。                                         |
| `session_worklog`                                                                                    | `session_id`, `after_id`, `limit?`                      | `{events,last_id,window_lost}`          | session/远程 worklog 错误显式失败。                        |
| `coordination_start`                                                                                 | `CoordinationStartInput {task_id,roles}`                | `{task_id,started:true}`                | roles 无法构造时报错。                                     |
| `coordination_message`                                                                               | `task_id`, `envelope`                                   | `{accepted,msg_id}`                     | envelope 无效或 task 未启动时报错。                        |
| `coordination_set_role_state`                                                                        | `task_id`, `role_id`, `state_name`                      | task/role/state JSON                    | 仅 `active` / `sleep` / `paused`；其它值报错。             |
| `coordination_snapshot`                                                                              | `task_id`                                               | `{task_id,roles,tasks,messages}`        | task 未启动时报错。                                        |
| `coordination_create_task`                                                                           | `id`, `title`, `require_acceptance`, `branch?`, `pr?`   | task JSON                               | DB 错误。                                                  |
| `coordination_claim_task` / `coordination_renew_task`                                                | task id、worker、lease 字段                             | task JSON                               | lease generation/归属错误显式失败。                        |
| `coordination_complete_task` / `coordination_accept_task`                                            | task id、worker/PR（complete）                          | task JSON                               | 状态不允许转换时报错。                                     |
| `save_schedule` / `list_schedules` / `run_schedule`                                                  | `ScheduleInput` 或 `schedule_id`                        | schedule JSON、数组或 `()`              | 缺失/禁用 schedule、playbook 或 engine 错误显式失败。      |
| `session_insights`                                                                                   | `session_id`                                            | counts、token_usage、duration           | DB 错误返回默认计数或错误，字段见 `main.rs:2720-2763`。    |
| `audit_events`                                                                                       | `session_id?`                                           | `session_id,sequence,kind,payload` 数组 | store 错误。                                               |
| `save_secret_metadata` / `list_secret_metadata`                                                      | name/scope/purpose/value 或无参数                       | `()` 或 metadata 数组                   | 空 secret、secret store/DB 错误；值不返回。                |
| `save_provider_key` / `delete_provider_key`                                                          | provider、key 或 provider                               | `()`                                    | 空 key、secret store 错误；audit payload 不含 key。        |
| `provider_settings` / `provider_configurations` / `save_provider_settings` / `validate_provider_key` | provider/base_url 等                                    | JSON、数组、bool                        | unknown provider、非法 URL、未配置 key、验证失败显式报错。 |

实现见 `main.rs:2222-3005`。

## 2.5 Event channels

公共 helper `emit` 发送事件名、可选 `session_id` 和 JSON payload（`main.rs:243-258`）。当前事件包括：

| channel             | payload                                                                          |
| ------------------- | -------------------------------------------------------------------------------- |
| `message`           | `{role:"user", text}` 或消息事件。                                               |
| `turn_start`        | turn 起始 JSON；完整序列见 [03](03-lifecycle.md)。                               |
| `assistant_delta`   | 文本增量 JSON。                                                                  |
| `tool_call`         | tool intent，含 `callId`/工具字段；具体 engine payload 以实现为准。              |
| `approval`          | `{call_id, tool, arguments, risk, reason}`，arguments 脱敏。                     |
| `approval_resolved` | `{call_id, approve}`。                                                           |
| `tool_result`       | 工具结果 payload。                                                               |
| `notice`            | `{kind,text}`，例如 `approval_pending`、`error`、`interrupted`、`model_switch`。 |
| `steering`          | `{text}`。                                                                       |
| `turn_done`         | `{run_state,stop_reason}`，必须是本 turn 最后事件；两字段是引擎产出的原始枚举。 |

事件序列和审批延续规则不在本篇重复，见 [03-lifecycle.md](03-lifecycle.md)。`submit_turn`、`resolve_approval`、`steering` 都会确保完成路径发 `turn_done`。

Cloud-Dev 的 PTY 做法是动态事件名 `term-data-{id}`（byte array）与 `term-exit-{id}`（空 payload）［CD码］。OPCOS 使用固定事件名 + session/call 字段，把路由维度放进 JSON，而不是为每个 session 创建新 channel［推断］；这样更适合审计、重连和 Tauri listener 管理。

## 2.6 错误与返回值约定

当前 Rust command 统一以 `Result<T,String>` 或 `Result<T, E>` 注册到 Tauri；错误最终表现为 rejected invoke promise。具体错误字符串来自 command body，不能依赖前端对字符串做稳定枚举匹配。

目标态建议把失败分成以下稳定类别，同时保留 human-readable detail：

| 类别                | 示例                                        | 前端处理                                      |
| ------------------- | ------------------------------------------- | --------------------------------------------- |
| `invalid_request`   | 空 command、非法 filename、unknown provider | 阻止提交并显示字段错误。                      |
| `not_found`         | session/host/playbook 不存在                | 返回列表刷新或跳转。                          |
| `host_unavailable`  | health、read、exec 连接失败                 | 显示远程不可用，不触发本地 fallback。         |
| `unauthorized`      | RVM 401                                     | 要求重新配置 host，不能显示 token。           |
| `policy_denied`     | 工具被 policy 拒绝                          | 显示 notice 并写 audit。                      |
| `approval_required` | pending tool call                           | 显示 approval card，等待 `resolve_approval`。 |
| `provider_error`    | 模型调用/验证失败                           | 保留 provider 名和脱敏 detail。               |
| `internal`          | DB lock、序列化、channel 错误               | 显示通用错误并记录本地日志。                  |

这些类别是 IPC 设计建议；当前代码仍主要返回字符串［推断］。

## 2.7 长任务与重入

- `submit_turn`、`run_schedule`、`run_blueprint`、`git_workflow`、`export_assets`、`import_assets` 和 `discover_remote_assets` 可能持续网络/执行时间；invoke 只返回最终状态，过程通过事件或 worklog 查询。
- `steering` 立即返回，但完成由后台 task 发 `turn_done`；前端不能把 invoke resolve 当作 turn 完成。
- `interrupt` 立即请求 engine 停止，停止完成仍由 engine/event 状态确认。
- `start_surface` 和 `start_ide_proxy` 只返回 relay port；真正 socket 生命周期由本地 relay task 管理。
- `mcp_tools` 是远程 `tools/list` 请求；若 server 长时间无响应，必须受 client timeout 约束，具体 timeout 当前未确认。

同一 session 的 `submit_turn`、`resolve_approval`、`interrupt` 必须串行化或由 engine 保证顺序；否则会出现审批已解决但旧 turn 又发 `turn_done` 的竞态［推断］。

## 2.8 IPC 版本化

command 名称是前端和 Rust 的 ABI；修改参数时应新增 command 或提供可选字段，不要静默改变旧字段含义。事件 payload 应包含稳定 `session_id`、`sequence` 或 `call_id`，以便前端去重和重连。

Cloud-Dev 动态 PTY event 名把 id 编进 channel 名［CD码］；OPCOS 固定 channel 的设计要求每个 payload 具备足够的路由字段。当前 `emit` 已接收 `session_id`，但需要继续核对所有事件是否都实际包含它［推断］。

## 2.9 与 [03] 的一致性检查表

1. turn 开始时发送 `turn_start`。
2. assistant 文本增量只发送 `assistant_delta`，不把完整 token 流塞进 invoke 返回。
3. 工具提出、审批、结果分别使用结构化事件。
4. policy denial 发送 `notice(kind=error)` 并写 `audit_events`。
5. approval pending 也必须发送 `turn_done`，见 [03](03-lifecycle.md)。
6. steering 的完成事件必须晚于当前 steering request。
7. 重连后 transcript 从 store 读取，event channel 只传增量。

所有新增 command 都应补对应 invoke 类型和失败测试。

命令参数以 Rust signature 为准；前端字段改名时必须同步更新 invoke 调用和本表。
