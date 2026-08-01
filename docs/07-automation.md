# 07 自动化与生命周期 hooks

现状：OPCOS 只有本地 cron（`src-tauri/src/scheduler.rs` + `run_schedule` / `run_schedule_for`）。三个参照系统都做到了「定时 + 事件 + 外部 webhook」三类触发，且都把环境准备拆成了带明确失败语义的阶段。

## 7.1 触发器

| 类型        | Devin［Devin文］                 | Tembo［Tembo文］                                                      | OPCOS 目标态［推断］                      |
| ----------- | -------------------------------- | --------------------------------------------------------------------- | ----------------------------------------- |
| 定时        | Automations / 定时 snapshot 构建 | schedule trigger                                                      | 已有本地 cron                             |
| 事件        | 集成事件、Slack/Teams 提及       | event trigger                                                         | 本地集成轮询（出站）                      |
| Webhook     | —                                | `POST /agent/{keyOrId}/trigger`，任意 JSON payload 作为 event context | 需要公网入站，见 [09](09-cloud.md) 形态 B |
| 快捷键/别名 | —                                | macro key（`keyOrId` 可以是 UUID 或 macro key）                       | 短名触发，本地即可做                      |

Tembo 的关键设计［Tembo文］：**webhook 的整个 JSON payload 作为 event context 传给 agent，agent 的 instruction 可以引用 payload 字段**。这比「把事件转成一句话 prompt」强得多——payload 保持结构化，指令负责解释。OPCOS 照此做。

注意：Tembo 文档写明 trigger 也可以用 `apiKey` query parameter 认证［Tembo文］。**OPCOS 不采纳这一条**——token 不进 URL 是硬约束。

### 出站 vs 入站

本地优先的代价是没有公网入口。分两步：

1. **出站轮询**（本地即可）：定期拉 GitHub / Linear / Sentry 的增量。延迟秒到分钟级，够用，且不需要任何云端。

P2-4 先落地 Linear 的直接 GraphQL 连接器，而不是复用 Linear MCP：Linear 的 PAT
认证和 issue/comment/status 能力可以在本地明确建模，且不依赖 MCP server 的部署、
OAuth 回调或远端 transport。Linear webhook 需要公网入站，仍延后到 Cloud B；本地
事件继续使用 P2-3 的回环触发器和用户手动/低频定时拉取。Linear PAT 只存入
SecretStore，连接器工具缺少 PAT、网络不可达或权限不足时返回显式错误。
2. **入站 webhook**（需要 relay）：本地只出站长连接到 relay，relay 提供稳定公网端点接事件后推给本机。不暴露本机端口。

先做 1，不要因为 2 才好看就先做 2。

## 7.2 生命周期 hooks

**Devin Blueprint**［Devin文］顶层字段：`initialize`、`maintenance`、`knowledge`、`post-build`、`clone`。

- `initialize` / `maintenance` / `post-build` 支持 shell `run` 步骤和 GitHub Actions 风格 `uses` 步骤，字段可带 `name` / `run` / `uses` / `with` / `env`。
- `knowledge` 条目字段是 `name` / `contents`，**不执行**，只作为上下文。
- `clone` 支持 `path` / `ref` / `depth` / `tags`。
- `post-build` 只在 org/enterprise blueprint 可用，**非零退出使构建失败且不生成 snapshot**。

**Tembo `.tembo.json`**［Tembo文］：

```json
{
  "hooks": {
    "postClone": ["npm ci"],
    "prePush": ["npm test"]
  }
}
```

`postClone` 在 clone 后、agent 开工前跑；`prePush` 在改动后、push/开 PR 前跑。命令按顺序执行，**单条失败会记录但继续执行剩余 hook**，全部跑在同一个 sandbox 里，支持管道、重定向和 `&&`。

## 7.3 OPCOS 阶段模型［推断］

OPCOS 已有 Blueprint（`read_blueprint` / `execute_blueprint` / `run_blueprint`），缺的是 push 前门禁。目标阶段与失败语义：

| 阶段          | 何时                    | 失败语义                                   |
| ------------- | ----------------------- | ------------------------------------------ |
| `clone`       | 绑定仓库到 host 后      | 失败 = 会话不可用，硬失败                  |
| `initialize`  | 首次准备环境            | 硬失败，不缓存结果                         |
| `maintenance` | 复用已有环境时          | 软失败，记录并继续（环境可能仍可用）       |
| `post-build`  | 环境构建完成后的校验    | **硬失败，且不生成可复用快照**（照 Devin） |
| `pre-push`    | 改动完成、push/开 PR 前 | 硬失败，阻止 push；这是 OPCOS 当前缺的门禁 |

两条来自参照系统的分歧要明确选边：

- Tembo 的 hook「单条失败继续跑剩余」适合 `maintenance`，**不适合** `pre-push`——门禁失败就必须停。
- 阶段失败必须落 `audit_events`，并在 UI 上显式呈现失败的那一条命令与退出码，不做静默降级。

## 7.4 自动化的执行绑定

一条自动化必须显式绑定：**host + 仓库 + 模型/provider + 触发器 + 指令（或运行手册）**。缺任何一项就不允许保存——参照系统里最常见的用户困惑就是「自动化跑了，但不知道跑在哪台机器上、用的哪个模型」。

P1-3 将五个阶段统一为 Host-backed lifecycle executor：

```text
clone → initialize → maintenance → post-build → pre-push
```

阶段命令在绑定的 `Host` 上执行，本机和远端使用同一条执行路径，不会因为远端失败而静默回落本机。每条命令使用共享执行超时；Host 超时路径负责 kill + wait，避免遗留子进程。

- `clone`、`initialize`、`post-build`、`pre-push` 是硬失败；
- `maintenance` 是软失败，失败命令会审计并继续执行后续命令；
- `post-build` 失败不生成可复用快照或环境就绪标记；当前 OPCOS 尚无环境复用/快照机制，因此暂无缓存可失效；
- `initialize` 失败不缓存结果，下次仍重新执行；当前 OPCOS 尚无环境就绪缓存，因此该语义待环境复用机制引入后生效；
- `pre-push` 按顺序执行，首条非零命令立即阻止 push；
- 阶段开始、命令结束/失败、阶段结束/失败均写入 `audit_events`；
- audit/transcript/UI 输出沿用现有脱敏出口；
- pre-push 错误包含失败命令原文与退出码，UI 复用现有 row/card 和错误展示。
