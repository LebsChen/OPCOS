# 01 数据模型

本文先记录 OPCOS 当前数据库事实，再记录目标态增量。参照系统的字段只作为设计依据，并保留来源标记；目标表不是现状。

## 1.1 OPCOS 现状：`opcos-store`

`SqliteStore::migrate` 当前创建 10 张表（`crates/opcos-store/src/lib.rs:461-539`）。

| 表                  | 字段                                                                                                                                                                | 用途                                                               |
| ------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------ |
| `schema_migrations` | `version`, `applied_at`                                                                                                                                             | 记录迁移版本。                                                     |
| `sessions`          | `session_id`, `workspace`, `model`, `mode`, `title`, `extra_roots`, `grants`, `pinned`, `archived`, `origin`, `origin_label`, `compaction`, `host_id`, `updated_at` | 会话元数据、工作区、模型、模式、额外 root、授权、归档和绑定 host。 |
| `messages`          | `session_id`, `sequence`, `role`, `content`, `display_only`                                                                                                         | 按 session 和 sequence 保存消息。                                  |
| `notices`           | `session_id`, `sequence`, `kind`, `content`                                                                                                                         | 保存结构化 notice。                                                |
| `tool_calls`        | `session_id`, `message_sequence`, `call_id`, `name`, `arguments`, `result`                                                                                          | 保存模型提出的工具调用及结果。                                     |
| `grants`            | `session_id`, `grant_key`, `grant_value`                                                                                                                            | 保存 session 级授权/standing grant。                               |
| `audit_events`      | `session_id`, `sequence`, `kind`, `payload`                                                                                                                         | 保存按 session 排序的审计事件。                                    |
| `compaction_state`  | `session_id`, `state`                                                                                                                                               | 保存 session 压缩状态。                                            |
| `pending`           | `session_id`, `call_id`, `tool`, `arguments`, `state`                                                                                                               | 保存等待审批或其它挂起的工具调用。                                 |
| `usage_events`      | `session_id`, `input_tokens`, `output_tokens`, `duration_ms`, `recorded_at`                                                                                         | 保存 token 用量和耗时。                                            |

`messages`、`notices`、`tool_calls`、`pending` 与 `audit_events` 都采用 session 维度的复合主键或序列键；原始字符串和 JSON 文本保留在 store 中，避免只存 UI 文案。

桌面 adapter 另有 SQLite 表，不属于上述 10 张 `opcos-store` 表：旧版 `main.rs` 初始化的桌面数据库包括 `hosts`、`sessions`、`transcript`、`asset_records`、`schedules`、`secret_records`、协调任务等（`src-tauri/src/main.rs:343-455`），实际没有桌面 `tool_calls` 表。重复的是桌面 `sessions`；桌面 `transcript` 与 store 的 `messages`、`notices`、`tool_calls` 在职责上重叠。P0-1 后桌面 `sessions`/`transcript` 删除，避免两套 session/tool 数据来源漂移。

## 1.2 目标态增量

`config_object`、`config_object_version`、`plugin`、`plugin_member` 的语义已经在 [06-capability-model.md](06-capability-model.md) 定义；本节只定义存储关系，不重复能力语义。

### `config_object` / `config_object_version`

| 表                      | 建议字段                                                                                                        | 约束                                                                                       |
| ----------------------- | --------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| `config_object`         | `id`, `kind`, `title`, `description`, `source_mode`, `status`, `current_version_id`, `created_at`, `updated_at` | `kind`、`source_mode`、`status` 使用 [06](06-capability-model.md) 的枚举；删除优先软删除。 |
| `config_object_version` | `id`, `config_object_id`, `version_number`, `created_at`, `created_via`, `payload_json`, `raw_source_text`      | 版本不可变；`config_object_id` 外键；`current_version_id` 指向版本。                       |

Den 的 config object 具有不可变版本、访问授权以及 archive/restore/delete 生命周期［OW文］；OPCOS 采用相同形状但先做本地持久化［推断］。

### `plugin` / `plugin_member`

字段沿用 [06](06-capability-model.md) 的 `id`, `name`, `description`, `status`, `manifest_json` 与 `plugin_id`, `config_object_id`。插件安装/卸载应在一个事务内完成；URL、MCP server 和成员关系必须从已存对象解析，不能由调用方在授权时临时替换［OW文］［推断］。

### `host`

| 字段                             | 用途                                                                                               |
| -------------------------------- | -------------------------------------------------------------------------------------------------- |
| `id`                             | 稳定 host 标识。                                                                                   |
| `name`                           | UI 显示名。                                                                                        |
| `kind`                           | `local`、`rvm`、`cloud_worker` 等实现类型；目标态枚举需与 [04](04-host-protocol.md) 一致［推断］。 |
| `base_url`                       | 非 secret 的 host 地址；RVM token 不放入此字段。                                                   |
| `enabled`                        | 是否允许新 session 绑定。                                                                          |
| `capabilities_json`              | 最近一次能力探测结果。                                                                             |
| `health_state`, `last_health_at` | 健康状态与时间。                                                                                   |
| `created_at`, `updated_at`       | 生命周期时间。                                                                                     |

当前桌面实现把 host 名存入 `hosts`，URL/token 分别进入加密 secret store（`main.rs:1013-1071`）；目标态应把非敏感 host 元数据统一到该表，并保留 secret store 分离。

### `artifact`

| 字段                                    | 用途                           |
| --------------------------------------- | ------------------------------ |
| `id`, `session_id`, `turn_id`           | 产物和会话/turn 关联。         |
| `host_id`, `remote_path`                | 产物所在 host 与远程路径。     |
| `kind`, `title`, `size_bytes`, `sha256` | 类型、显示名、大小、内容指纹。 |
| `source_tool_call_id`                   | 生成它的工具调用。             |
| `created_at`, `deleted_at`              | 生命周期。                     |

目标态只保存引用，不复制远程文件内容；读取必须重新通过 host 协议，失败显式返回［推断］。

### `automation`

| 字段                                        | 用途                                                                               |
| ------------------------------------------- | ---------------------------------------------------------------------------------- |
| `id`, `name`, `enabled`                     | 自动化身份和开关。                                                                 |
| `trigger_kind`, `trigger_json`              | `schedule`、`event`、`webhook` 等触发器及参数；具体枚举见 [07](07-automation.md)。 |
| `session_id`, `playbook_id`                 | 运行目标和执行内容。                                                               |
| `last_run_at`, `last_result`, `next_run_at` | 运行观察字段。                                                                     |
| `created_at`, `updated_at`                  | 生命周期。                                                                         |

当前已有 `schedules`，字段为 `id`, `name`, `session_id`, `playbook_id`, `cron`, `enabled`, `last_run`, `last_result`（`main.rs:2608-2660`）；迁移时先把它映射到 `automation`，再删除重复表。

## 1.3 Cloud-Dev 对照

Cloud-Dev 的 `src/db.ts:113-194` 有 18 张表：项目/删除项目、meta、accounts、account_orgs、account_switch_history、presets、settings、repo_bindings、repo_configs、usage_history、prompts、三类 scoped cache、coord_task、coord_role、coord_message、coord_cursor［CD码］。

OPCOS 需要吸收的概念：

- `session_cache_scoped` 的 account/org 维度可转化为 host/session 隔离，但 OPCOS 不需要 Devin account 语义［CD码］［推断］。
- `coord_task`、`coord_role`、`coord_message`、`coord_cursor` 对应 OPCOS 已有协调任务/消息模型，可继续统一 cursor 和幂等键［CD码］［推断］。
- attachment/file-diff cache 可作为 artifact 或短期缓存的实现参考，不应把凭据放进缓存［CD码］［推断］。

不直接照搬的云特有概念：

- `accounts`、`account_orgs`、`account_switch_history`：Devin 账号、组织和额度切换［CD码］。
- ACU、balance、quota、usage billing：Devin 云计量，不属于 OPCOS 本地执行模型［Devin文］［推断］。
- 云端 handoff、服务用户、组织授权：只有实现 OPCOS Cloud 控制面时才增加，不能污染离线本地 schema［CD码］［推断］。

## 1.4 迁移顺序

1. 新增迁移版本记录和幂等检查，不改现有 10 张表。
2. 建 `config_object`、`config_object_version`，把现有 `asset_records` 按 `kind` 建立初始版本。
3. 建 `host`，从桌面 `hosts` 导入元数据；URL/token 继续留在 secret store。
4. 建 `artifact`，先由 worklog、diff、附件导出引用，不复制文件。
5. 建 `automation`，从 `schedules` 双写一版后再切读路径。
6. 建 `plugin`、`plugin_member`，完成本地导入/导出后再考虑远程授权。
7. 最后合并桌面 adapter 与 `opcos-store` 的 session/tool 权威，保留回滚迁移。

迁移每一步都必须可重复、可回滚；外部参照系统的云端组织、ACU 和 handoff 不进入本地迁移［推断］。

## 1.5 字段与索引规则

现状表的 JSON 字段仍按原始字符串保存：

- `sessions.extra_roots`、`sessions.grants`、`sessions.compaction` 由 Rust store 解析和序列化。
- `messages.content`、`tool_calls.arguments`、`tool_calls.result`、`audit_events.payload` 不允许被 UI 直接拼接。
- `pending.arguments` 在返回 UI 前必须通过现有 approval redaction。
- `usage_events` 的 token 和时长只用于本地观察，不转换成 ACU 或云账单。

目标态建议的索引：

| 索引                                                     | 目的                         |
| -------------------------------------------------------- | ---------------------------- |
| `config_object(kind,status)`                             | 按资产类型和生命周期过滤。   |
| `config_object_version(config_object_id,version_number)` | 快速取得版本历史。           |
| `plugin_member(plugin_id,config_object_id)`              | 防止重复成员并支持安装事务。 |
| `host(kind,enabled)`                                     | host 选择器过滤。            |
| `artifact(session_id,created_at)`                        | 会话产物时间线。             |
| `automation(enabled,next_run_at)`                        | 调度器取可运行项。           |

这些索引是本地查询优化建议，不是外部系统事实［推断］。

## 1.6 约束、删除与隐私

- Secret value 永远不进入 `host`、`config_object_version.payload_json`、`artifact`、`audit_events` 或 transcript。
- 删除 host 前必须阻止仍有运行 session 的 host 删除，或者先将 session 标记为 host unavailable；不能自动改绑到另一个 host［推断］。
- 删除 config object 默认写 `status=deleted`，保留历史版本以支持审计；物理清除应是显式的数据清理操作。
- artifact 删除只删除引用记录；远程文件是否删除必须由单独的、受审批保护的工具决定。
- automation 删除不应删除它引用的 session 或 playbook。
- 多表迁移失败时必须事务回滚，不允许出现有 plugin_member 而无 plugin 的半状态。

## 1.7 与现有 API 的映射

| 现有入口                                                   | 目标表                                                    |
| ---------------------------------------------------------- | --------------------------------------------------------- |
| `list_assets` / `save_asset` / `delete_asset`              | `config_object` + current `config_object_version`         |
| `set_asset_enabled`                                        | session-object selection 关系表（目标态新增，名称未确认） |
| `save_host` / `list_hosts` / `delete_host`                 | `host`                                                    |
| `read_transcript` / `submit_turn`                          | `sessions`、`messages`、`tool_calls`、`pending`           |
| `audit_events`                                             | `audit_events`                                            |
| `save_schedule` / `list_schedules` / `run_schedule`        | `automation`                                              |
| `review_snapshot` / `review_file_diff` / attachment upload | `artifact` 或 worklog 引用                                |

目标态关系表的最终名称和迁移版本号尚未确定；实现前应先写 schema migration test［推断］。
