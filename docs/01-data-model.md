# 01 数据模型

本文先记录 OPCOS 当前数据库事实，再记录目标态增量。参照系统的字段只作为设计依据，并保留来源标记；目标表不是现状。

## 1.1 OPCOS 现状：`opcos-store`

`SqliteStore::migrate` 当前通过一组幂等的 `CREATE TABLE IF NOT EXISTS` 与兼容性
迁移建立 43 张表（`crates/opcos-store/src/lib.rs` 的 migration 实现）。表数量会
随迁移演进，因此下面按当前重要关系分组记录；字段以 migration 中的实际定义为准。

当前 `sessions` 行至少包含：

```text
session_id, workspace, model, mode, harness, title, extra_roots, grants,
pinned, archived, origin, origin_label, compaction, host_id, provider,
external_session_id, run_state, stop_reason, terminal_cause,
provider_finish_reason, created_at, updated_at, last_active_at, sleep_state,
slept_at, project_id, agent_id
```

核心表与用途如下：

| 表 | 用途 |
| --- | --- |
| `schema_migrations` | 记录迁移版本及应用时间。 |
| `sessions` / `projects` / `project_agents` | 保存会话、项目及项目 agent 的绑定、模型、harness、运行状态和生命周期字段。 |
| `messages` / `notices` / `tool_calls` / `pending` | 保存 transcript、结构化 notice、工具调用结果以及等待审批/提问等 durable pending 项。 |
| `grants` / `local_gate_records` / `audit_events` / `session_events` | 保存 session 授权、本地门禁、审计记录和有序工作事件。 |
| `compaction_state` / `usage_events` | 保存压缩状态及 token/耗时观察数据。 |
| `session_preferences` / `session_activity` | 保存会话偏好；`session_activity` 独立记录最近活动时间，供 idle candidate 查询。 |
| `action_ledger` / `work_queue` | 保存外部动作幂等状态，以及带 lease、重试和 dead-letter 的 durable queue。 |
| `account_host_bindings` / `login_profiles` / `login_state_backups` | 保存账号与 host 绑定、登录 profile 元数据及登录状态备份。 |
| `model_discovery_cache` / `learned_model_limits` | 缓存 provider 模型发现结果和按 provider/base URL/model 学到的限制。 |
| `learned_skills` / `learned_skill_provenance` / `automatic_memories` | 保存学习项、来源和自动记忆及其版本/冲突关系。 |
| `autonomous_goals` / `planning_rounds` / `plans` / `plan_steps` / `plan_revisions` | 保存自治目标、规划轮次及可追踪计划的步骤和修订。 |
| `events` / `event_cursors` / `event_rules` / `event_dispatches` | 保存事件总线、消费游标、规则和规则分发记录。 |
| `external_ingress_sources` / `work_queue_progress` / `ci_monitors` / `ci_monitor_states` | 保存外部入口、队列进度和 CI monitor 状态。 |
| `autonomous_runner_profiles` / `runner_settings` / `repair_loop_grants` / `github_instances` | 保存 runner 配置、runner 设置、修复循环授权和 GitHub 实例信息。 |

`session_activity` 的主键是 `session_id`，并通过 `ON DELETE CASCADE` 关联
`sessions`。store 用 upsert/max 语义刷新 `last_activity_at`；迁移会用
`COALESCE(NULLIF(sessions.last_active_at,''), sessions.created_at)` 回填已有
会话。idle 扫描只选择未归档、`run_state='idle'`、保持 awake 且活动时间早于阈值的
会话，详见 [03-lifecycle.md](03-lifecycle.md)。

`messages`、`notices`、`tool_calls`、`pending` 与 `audit_events` 都采用 session 维度的复合主键或序列键；原始字符串和 JSON 文本保留在 store 中，避免只存 UI 文案。

桌面 adapter 只初始化 `hosts`、`settings`、`asset_records`、`secret_records`、`mcp_session_tools`、`asset_session_selection`、`schedules`、`coord_tasks`；`sessions` 与 `transcript` 不再由桌面 schema 创建［推断］。启动时 `SqliteStore::open` 会识别旧桌面表，在同一个 SQLite transaction 中导入并删除旧表；失败会回滚并显式返回错误，避免静默丢失［推断］。

## 1.2 目标态增量

`config_object`、`config_object_version`、`plugin`、`plugin_member` 的语义已经在 [06-capability-model.md](06-capability-model.md) 定义；本节只定义存储关系，不重复能力语义。

### `config_object` / `config_object_version`

| 表                      | 建议字段                                                                                       | 约束                                                                                                                                                                |
| ----------------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `config_object`         | `id`, `kind`, `name`, `scope_kind`, `scope_key`, `status`, `created_at`, `current_version_id`  | `kind` 为 `rules / knowledge / runbook / skill / mcp`；scope 为 `global`（key 为 NULL）、`repo`（规范化 workspace）、`host`（host id）；删除使用 `status=deleted`。 |
| `config_object_version` | `id`, `object_id`, `version`, `content`, `content_hash`, `created_at`, `note`, `metadata_json` | 版本不可变；`object_id + version` 唯一；内容 hash 相同不新增版本；`current_version_id` 指向当前版本。                                                               |

`enabled`/`active` 是对象运行态，保存在 `config_object.status`，不产生新版本；知识库的 `trigger` 与 skill activation 元数据保存在对应 version 的 `metadata_json`，不得拼入正文。会话启动时把适用对象的具体 version id 写入 `session_config_versions`，旧会话不会随 current version 漂移。

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

| 字段                                    | 用途                                                 |
| --------------------------------------- | ---------------------------------------------------- |
| `id`, `session_id`, `turn_id`           | 产物和会话/turn 关联。                               |
| `host_id`, `path`                       | 产物所在 host 与路径引用。                           |
| `kind`, `title`, `size_bytes`, `sha256` | 类型、显示名、大小、内容指纹。                       |
| `source_tool_call_id`                   | 生成它的工具调用；`turn_id` 使用持久化工具消息序号。 |
| `created_at`, `deleted_at`              | 生命周期。                                           |

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

1. 新增迁移版本记录和幂等检查。
2. 建 `config_object`、`config_object_version`、`config_object_legacy_map`、`session_config_versions`，把现有 `asset_records` 按 `kind` 建立初始版本；旧表重命名为 `asset_records_legacy_p1_1` 并保留。
3. 建 `host`，从桌面 `hosts` 导入元数据；URL/token 继续留在 secret store。
4. 建 `artifact`，先由 worklog、diff、附件导出引用，不复制文件。
5. 建 `automation`，从 `schedules` 双写一版后再切读路径。
6. 建 `plugin`、`plugin_member`，完成本地导入/导出后再考虑远程授权。
7. P0-1 已将桌面 session/transcript 迁入 `opcos-store`；后续只允许通过 store 读写会话与 transcript，保留一次性旧表导入逻辑［推断］。

`mcp_session_tools` 继续保留为 session 级 MCP tool selection，不等同于 `kind=mcp` 的 server 配置对象；当前没有独立 MCP server 行时不伪造迁移数据。调度表引用 runbook object，实际每次运行的 version id 记录在 `schedule_runs`，以便复现而不冻结后续调度。

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
