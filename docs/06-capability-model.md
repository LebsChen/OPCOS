# 06 能力模型：配置对象与插件

## 6.1 问题

OPCOS 现在有五种「资产」：Rules（`AGENTS.md`）、知识、运行手册、技能、MCP 配置。它们各写了一套增删改查、一套 UI、一套存储，彼此不能组合、不能版本化、不能分发。三个参照系统都已经解决了这个问题，且解法收敛到同一个形状。

## 6.2 参照系统的解法

**Den（OpenWork Cloud）**［OW文］把一切都归约为两层：

- **config object**：`objectType` + `sourceMode` + **不可变版本**（`/versions`、`/versions/latest`、`/versions/{versionId}`）+ 访问授权（member / team / orgWide）+ `archive` / `restore` / `delete` 三态。
- **plugin**：config object 的集合，外加 MCP requirements、访问授权、发布到 marketplace。官方定义原文——插件是 **skills、hooks、MCP servers、agents、commands 的打包**［OW界］。

值得注意的细节：给 plugin 配置远程 MCP 时（`POST /v1/plugins/{pluginId}/mcp-connections`），**server name 和 URL 由 config object 推导，调用方不能传 URL**［OW文］——这防止了「授权一个插件，结果它指向别处」的提权。

**Tembo**［Tembo文］：技能目录按 harness 分（`.claude/` `.codex/` `.opencode/` `.cursor/`），主体文件恒为 `SKILL.md`；规则文件按固定优先级取第一个存在者：`tembo.md` → `AGENTS.md` → `CLAUDE.md` → `.cursorrules` → `.windsurfrules` → `.clinerules` → `.rules` → `AGENT.md` → `.github/copilot-instructions.md`；每个 session 开始前读取。

**Devin**［Devin文］：Knowledge 带触发条件注入；Blueprint 的 `knowledge` 条目字段为 `name` / `contents`，**不执行，只提供上下文**。

## 6.3 OPCOS 目标模型［推断］

统一为一种存储、一种生命周期：

```
config_object
  id, kind, title, description
  kind ∈ rule | knowledge | playbook | skill | mcp_server | blueprint | instruction
  source_mode ∈ local | repo_file | imported
  status ∈ active | archived | deleted
  current_version_id

config_object_version          -- 不可变
  id, config_object_id, created_at, created_via
  payload_json                 -- 归一化后的结构
  raw_source_text              -- 原始文本（Markdown / JSON），保留以便无损回写
```

规则：

- **版本不可变**。修改 = 追加新版本，`current_version_id` 前移。回滚 = 把旧版本指为 current，不删历史。
- `raw_source_text` 必须保留。Rules 的实体就是仓库里的 `AGENTS.md`，回写要保持原样。
- `kind` 决定装载语义，不决定存储结构。

### 装载语义

| kind          | 何时进入上下文                                                          |
| ------------- | ----------------------------------------------------------------------- |
| `rule`        | 每个 session 开始前全量注入。按 Tembo 的优先级链取仓库文件              |
| `instruction` | 全局追加到系统提示（PR 标题 / commit 文案规则等）                       |
| `knowledge`   | **按触发条件注入**（仓库、路径、关键词匹配），不是全量灌入              |
| `playbook`    | 用户显式选择，或按触发条件建议                                          |
| `skill`       | **按需装载**：先只给 agent 看清单（name + description），命中后再读全文 |
| `mcp_server`  | 会话建立时连接，见 [05](05-mcp.md)                                      |
| `blueprint`   | host 准备阶段执行，见 [07](07-automation.md)                            |

「先给清单、命中再读全文」是技能能规模化的唯一方式——全量注入会在几十个技能时就撑爆上下文。

## 6.4 插件（打包单元）［推断］

```
plugin
  id, name, description, status, manifest_json
plugin_member
  plugin_id, config_object_id
```

- 插件 = 配置对象的集合，安装/卸载是原子的。
- 支持从 GitHub 仓库导入（对标 Den 的 Sources［OW界］），也支持导入 Anthropic 兼容插件清单（Den 明确支持这种归一化［OW界］）。
- **单机也有价值**：多台机器、多个仓库复用同一套配置，不必手工同步。云端分发是它的延伸，不是前提。
- 沿用 Den 的安全约束：插件声明的 MCP server URL 只能来自它自己的 config object，不接受调用方传入。

## 6.5 迁移路径

1. 把现有五套资产收敛到 `config_object` + `config_object_version`（现有数据按 `kind` 落到 `active` 的初始版本）。
2. UI 上仍保留五个入口（用户认这个心智），但走同一套组件和同一条渲染路径。
3. 再加 `plugin` 表与导入/导出。
4. 最后才是云端分发与授权（[09](09-cloud.md) 形态 D）。

## 6.6 Harness 与 Host 进程流

Harness 层只消费跨实现的事实事件：助手文本、工具调用、工具结果、挂起的审批/提问、回合结束、错误和中断。`TurnEngine` 通过 `BuiltinHarness` 提供内置实现；外部 harness 尚未接入，本轮不展示 OpenCode 入口。

Host 另提供进程流能力：

```rust
async fn spawn(&self, request: SpawnRequest) -> Result<Box<dyn HostProcess>, HostError>;
```

`HostProcess` 交付增量 UTF-8 解码后的输出片段和退出事件，并支持 stdin 与 interrupt。Host 不承诺输出是干净的行，也不负责 ANSI、`\r`、PTY echo 清洗或 NDJSON 分帧；这些属于 harness 解析器。

- `LocalHost` 使用普通管道，避免没有必要的 PTY 噪声。
- `RvmHost` 使用已有 `/pty-ws` 双向 WebSocket；RVM 主机端不做修改。
- `/pty-ws` 是终端字节流，存在 echo、控制序列和窗口宽度导致换行污染风险。P2-2 接 OpenCode 时必须先验证 `--format json` 在该通道上的可靠性。
- 远程能力只有在主机能力表声明 `pty` 时才把 `process_stream` 标记为可用；不可用主机仍显式返回不可用，不代理到本机。
