# OPCOS 项目 / 团队模型设计

取代原来「一个会话 = 一个 workspace」的扁平模型。**项目是顶层容器**，等价于一个团队；会话（agent）是团队成员。

参照物：Cloud-Dev 的项目隔离（`projects/<id>/project.db`）与 Teams 协同面板（Leader/Worker 星型拓扑、`[[COORD]]` 信封、Coord Board 租约与验收）。OPCOS 全在本地引擎内完成，不需要跨账号/跨组织的云端 API 派发。

---

## 1. 领域模型

```
Project「OPCOS」
├─ host_id              固定绑定一个 Local 或 RVM 主机
├─ repository           git URL + 主检出目录 + 默认分支
├─ workflow             有序阶段 + 每阶段负责角色 + 门禁
├─ 项目级配置            Rules / Knowledge / Playbook / Skills / MCP / Connectors / Secrets / Blueprint
└─ agents（成员）
   ├─ agent1  Lead     → session A   worktree …/lead      branch dev
   ├─ agent2  Code     → session B   worktree …/code      branch agent/code-1
   ├─ agent3  Review   → session C   worktree …/review    branch （跟随被审分支）
   ├─ agent4  Test     → session D   worktree …/test
   └─ agent5  DevOps   → session E   worktree …/devops
```

### 1.1 `projects`

| 字段 | 说明 |
| --- | --- |
| `id` | ULID |
| `name` | 显示名 |
| `host_id` | 项目的执行主机（Local / RVM），成员继承 |
| `repo_url` / `repo_root` / `default_branch` | 项目仓库；`repo_root` 是主检出目录（主机上的路径） |
| `workflow_json` | 工作流定义，见 §3 |
| `board_id` | 项目常驻协同看板 id（1:1） |
| `archived` / `created_at` / `updated_at` | |

### 1.2 `project_agents`

| 字段 | 说明 |
| --- | --- |
| `id` / `project_id` / `sort_order` | `sort_order = 0` 恒为 Lead，唯一 |
| `name` | 成员显示名 |
| `role` | `Lead` / `Code` / `Review` / `Test` / `DevOps` / 自定义 |
| `session_id` | 绑定的会话；未启动前为空，**不复用、不重建** |
| `provider` / `model` / `harness` / `mode` | 每个成员独立选择 |
| `system_prompt` | 角色提示词覆盖（在项目资产之后注入） |
| `worktree_path` / `branch` | 见 §2 |
| `state` | `Active` / `Sleep` / `Paused`（沿用 `orchestration::RoleState`） |

### 1.3 会话归属

`sessions` 增加 `project_id`、`agent_id`（均可空）。既有会话迁移为「未分组」，UI 单列一栏；新建会话默认要求选择项目。会话的 `host_id` 与 `workspace` 由项目 + 成员 worktree 解析后写入，不再由用户在首页手填。

---

## 2. Worktree

项目只 clone 一次到 `repo_root`；每个成员一个 worktree，互不干扰、可同时构建。

```
<host>/OPCOS/projects/<project-id>/
├─ repo/                     git clone（主检出，Lead 默认落在这里）
└─ worktrees/<agent-id>/     git worktree add，每成员一个
```

- 创建成员时执行 `git -C <repo> worktree add <path> -b <branch>`；分支缺省 `agent/<role>-<seq>`，可指定已有分支。
- Local host 直接执行；RVM 通过 host exec 在远端执行，路径走远端路径代数，不用 `Path::canonicalize`。
- 删除成员时 `git worktree remove`（有未提交改动则报错，需显式强制）。
- Review 角色可切到「跟随被审分支」模式：`git -C <worktree> checkout <target>`。

---

## 3. Workflow

项目级有序阶段，Lead 据此派发；每阶段声明负责角色与出口门禁。

```yaml
workflow:
  - stage: plan     roles: [Lead]                gate: none
  - stage: code     roles: [Code]                gate: build+test
  - stage: review   roles: [Review]              gate: accept
  - stage: test     roles: [Test]                gate: pass
  - stage: deploy   roles: [DevOps]              gate: accept
  serial: true      # 共享分支/单 PR 时串行；独立子任务可并行
```

阶段推进复用现有 `orchestration::BoardTask` 生命周期：`Open → Claimed → AwaitingAcceptance → Done`，门禁映射到 `require_acceptance` 与 verified PR 校验。

---

## 4. Lead 指挥

完全复用 `crates/opcos-engine/src/orchestration.rs` 已实现的部分，不重写：

- 星型拓扑：Lead↔Worker，Worker 之间不通信；
- `[[COORD]]` 信封（`request` / `result` / `status`，`msgId` 去重，`replyTo` 关联）；
- 熔断：每会话每分钟 ≤20 条、每任务 ≤200 条；
- 看板租约：`claim → renew(lease_generation) → complete → acceptance`；
- 完成判据是真实 branch + commit + push + PR，由 Lead 核实，口头声称不算。

需要新增的只有三件事：

1. `BoardTask` 与 `Role` 增加 `project_id`，看板与项目 1:1 常驻（不再每次新建 coordination task）；
2. Lead 的派发落到**本地会话消息队列**（复用现有 steering 队列），而不是 Cloud-Dev 那种对云端 `POST /sessions/<id>/messages`；
3. Lead 不直接改代码，这一条写进 Lead 角色的系统提示。

---

## 5. 配置作用域与全局预设

设置页里的全局配置本身就是可复用的系统预设，不再为 Rules、Knowledge、Playbook、Skills、MCP、Connectors、Blueprint 等配置维护平行的 `template` 存储层。市场页只是这些全局预设以及仓库导入预设的浏览入口；Agent/Team 预设也存放在全局作用域，但创建会话或成员时仍然实例化为具体实体。

### 5.1 作用域模型

`config_object.scope_kind` 使用以下作用域：

| 作用域 | 含义 |
| --- | --- |
| `global` | 设置页中的通用预设，默认对所有项目和会话继承 |
| `project` | 项目的新增配置或对全局预设的覆盖，`scope_key = project_id` |
| `repo` | 仓库/主机绑定的配置 |
| `host` | 执行主机绑定的配置 |
| `session` | 会话专属配置 |

项目选择不复制全局对象，而是记录在 `project_config_selection`：

- 没有记录：默认继承该全局预设；
- `enabled = 1`：项目显式启用该全局预设；
- `enabled = 0`：项目显式排除该全局预设；
- 新增全局预设自动对已有项目生效，除非该项目存在显式排除记录。

项目需要修改某条全局预设时，才创建同 `kind/name` 的 `project` 对象。项目页显示：

- `继承自全局预设`：没有有效项目覆盖；
- `项目已覆盖`：存在 active 的项目对象；
- `已本地修改`：项目覆盖的当前版本 `content_hash` 与全局预设当前版本的 `content_hash` 不同。

恢复继承会停用项目覆盖对象；它不会删除全局对象或历史版本。项目覆盖在项目配置页下方的既有资产编辑器中编辑，不另建一套编辑器。

### 5.2 运行时解析

运行时按以下顺序收集对象，后者覆盖前者：

```
global → project → repo → host → session binding（冻结版本）
```

对象按 `(kind, name)` 去重；同一名称在更高优先级作用域出现时，使用更高作用域的当前版本。全局对象先应用项目选择表的显式排除，项目覆盖和新增配置再参与解析；会话级 `asset_session_selection` 最后控制会话对象是否启用。绑定会话时保存所选对象及版本，保证运行中的会话使用冻结版本。

- **Rules / Knowledge / Playbook / Skills**：有效配置并入 `AssetBundle`，注入顺序为 Instructions → Rules → 仓库 AGENTS/rules → Knowledge → Playbook → Skills → 成员角色提示。
- **MCP / Connectors**：配置对象遵循同一作用域解析；成员会话的工具启用状态仍存 `mcp_session_tools`。
- **Secrets**：独立使用 `secret_records` 的显式全局/项目隔离 API；项目 Secret 优先于全局同名 Secret，值不进入配置对象或日志。
- **Blueprint**：优先使用有效项目覆盖，其次使用未被项目排除的全局 Blueprint，最后才读取仓库 `.devin/blueprint.yaml`；`pre-push` 作为 Lead 验收前门禁。

### 5.3 旧数据迁移

启动迁移 `p1-2-config-scope-model` 会：

1. 创建 `project_config_selection`；
2. 将已有 `scope_kind = 'template'` 的配置对象提升为 `global`，保留 `scope_key`、内容、版本和元数据，不覆盖用户数据；
3. 对旧的项目配置副本：
   - 副本内容仍等于来源预设时，停用副本并恢复全局继承；
   - 已删除的副本转换为 `enabled = 0` 的项目显式排除；
   - 内容已经被项目修改的副本保留为 `project` 覆盖；
4. 记录迁移版本，重复启动不会重复迁移。

配置对象的版本历史始终保留在 `config_object_version`；导入仓库模板也写入全局配置对象并保留仓库来源 scope，重复导入相同内容时保持不变，内容变化时追加新版本。

---

## 6. 项目看板（Teams 面板）

项目页布局：

- **顶部**：项目名、host 与在线状态、仓库/默认分支、workflow 当前阶段、`启动全部 / 暂停 / 恢复`。
- **成员网格**：每个 agent 一张卡片 —— 角色徽标、成员名、状态（Active/Sleep/Paused/无会话）、当前任务标题与阶段、分支与 PR 链接、最近一条消息、`打开会话`。
- **看板任务列表**：标题、阶段、负责成员、租约剩余、依赖、验收状态；Lead 可在此建/派/验收。
- **协同消息历史**：解析后的 `[[COORD]]` 条目（发送方→接收方、kind、payload 摘要），人类只读。
- 人类在 **Lead 会话**里对话下达目标；Worker 会话在面板中只读观察，也可单独打开接管。

---

## 7. 实施切片

| 切片 | 内容 |
| --- | --- |
| P1 | `projects` / `project_agents` 表、项目 CRUD、成员 CRUD、worktree 创建与回收、会话归属项目、项目看板只读版（状态 + 打开会话） |
| P2 | `project` 配置作用域：Rules / Knowledge / Playbook / Skills / MCP / Connectors / Secrets / Blueprint |
| P3 | workflow 定义与 Lead 指挥：项目常驻 board、阶段推进、门禁与验收、协同消息历史 |
| P4 | Devin 设置页具体功能项：slash 命令管理、Default agent / platform、Computer use 开关、Batch / Message usage limits、PR 策略五项、Environment 四 tab 与仓库有序 setup、Skills Usage 看板与 Browse、Knowledge 文件夹 / pin / macro / suggestions、Playbook 结构化章节与 `.devin.md` 附加 |

每个切片结束跑一遍本地门禁并做真机 Tauri 端到端验收。
