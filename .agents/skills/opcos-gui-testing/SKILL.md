---
name: opcos-gui-testing
description: How to bring up and GUI-test the OPCOS desktop app (Rust + Tauri v2 + React) against a real remote RVM host, including launch quirks, UI navigation paths, secret injection, and the failure modes that keep recurring.
---

# Testing the OPCOS desktop app end to end

OPCOS is a local Devin client: Rust core + Tauri v2 shell + React frontend. It never executes
work locally — every session is bound to a remote RVM host running the unmodified Cloud-Dev Node
dev-agent. Meaningful acceptance therefore needs a real remote host and a real provider key.

## Bring-up order (matters)

1. Start Vite **first**; the Tauri `devUrl` points at it (`src-tauri/tauri.conf.json`, `http://localhost:1420`).
   ```bash
   cd web && rm -rf node_modules/.vite && npm run dev -- --port 1420
   ```
   Verify `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:1420/` returns 200. Kill stale
   Vite instances first — they squat on 1420/1421/1422 and the app will silently load an old bundle.
2. `cargo build -p opcos`
3. Launch the window:
   ```bash
   DISPLAY=:0 WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 \
     RUST_LOG=info ./target/debug/opcos
   ```
   **Launch it from a persistent/tty shell session.** Started from a short-lived non-interactive
   shell (even with `setsid`/`nohup`), the process is torn down before the window maps and
   `wmctrl -l` never shows `OPCOS`.
4. Maximize before recording: `DISPLAY=:0 wmctrl -r OPCOS -b add,maximized_vert,maximized_horz`
   (never `xdotool key super+Up` — it tiles instead of maximizing).
5. Sanity signal: startup prints `secret_backend=encrypted-file`. This box has no Secret Service
   (no gnome-keyring/kwallet), so the encrypted-file fallback is the expected backend; if a build
   regresses to keyring-only, every `client_for()` path (host Test, submit_turn, PTY/VNC/IDE,
   asset discovery) fails at once.

Testing must happen in the real Tauri window. A browser/CDP preview of :1420 has no `invoke`
bridge, so nothing real is exercised.

## Where things live in the UI

- Settings: sidebar bottom account row (`data-testid="account-row"`) → **Settings** → left sub-nav
  (General / Provider / Hosts / AGENTS.md / Knowledge / Playbook / Skill / MCP / Secrets / Blueprint).
- Provider: one card per provider; **Save and validate** produces exactly
  `Provider key validated successfully.` or `Provider key validation failed.` /
  `Provider validation failed: <error>` — good anchors for positive/negative assertions.
- Hosts: Add host form + per-row **Test** and **Delete → Confirm delete**.
- New session modal: Title / Bound host / Provider / Model (matrix dropdown **plus** a free-text
  custom-model input) / Mode / Workspace (remote path).
- Session page (from `423ea0b` onward): the main column is **Chat only** (topbar → transcript →
  composer); the old horizontal surface tabs are gone. All tools live in the right rail:
  a vertical icon rail (Info / Shell / Desktop / Web IDE / Diff / Worklog / Browser) + a drawer
  whose header shows the pane name and an `✕` collapse button. Model selection is a chip in the
  Composer, Provider is in the right-rail Info pane, Interrupt is the Composer `⏹ Stop`.
- **Rail buttons may be unreachable by mouse.** The main topbar can paint over the top of the icon
  rail, hiding the first few buttons (Info / Shell / Desktop) behind the topbar's
  "Toggle session panel" button. Workaround that works in the WebView: click a *visible* rail icon,
  press `Tab` once (focus moves to the next rail button), then `shift+ISO_Left_Tab` N times and
  `space` to activate the hidden one. Plain `shift+Tab` does **not** work under xdotool — you must
  send `shift+ISO_Left_Tab`.
- **Do not click the topbar "Toggle session panel" button** unless you are testing it: it toggles only
  the `.app` grid class, so the panel stays rendered at full width and overlaps/clips the chat
  column. Collapse with the drawer `✕` instead.

## Round-6 gotchas (right-rail layout)

- The Shell pane's 80-column xterm is much wider than the ~240 px drawer and is clipped **from the
  left**, so column 0 — where command output starts — is off-screen. To prove PTY output, pad it into
  the visible columns, e.g. `echo ("-" * 40 + "marker")` in PowerShell.
- Pane persistence (`display` switching) can be verified by: run a command in Shell → open Desktop →
  return to Shell → the scrollback must still be there, the toolbar must still read
  `Connected on <port>`, and a **new** command must still execute.
- The live transcript and the reloaded transcript differ a lot: assistant answers can end up nested
  inside a collapsed `› N steps` group and steer messages can be ordered below their answer in the
  live view but correct after reopening the session. Always check both.
- Web IDE: check the proxy root **and** the asset paths the returned HTML actually references
  (`/out/...`, `/resources/...`), then the corresponding upstream `/ide/out/...` with Bearer, without
  Bearer, and with the `Set-Cookie` values the upstream `/ide/` returns. A 403 on all three means the
  blocker is the upstream bootstrap, not OPCOS routing.
- 1024 px: test both drawer-open and drawer-collapsed; the clipping regression only shows with the
  drawer open.

## Secrets: inject without ever printing them

Click the password field with computer-use first, then type from stdin so the value never appears
in a command line, log, screenshot or DOM:

```bash
printf '%s' "$TOK" | DISPLAY=:0 xdotool type --clearmodifiers --delay 12 --file -
```
with the env binding done by reference (e.g. `{"TOK": "secret:session:RVM_WIN_TOKEN"}`).
Confirm the field shows dots before screenshotting. Finish every round with a counting-only leak
check (`grep -c -F`) over the app log, the vite log, `~/.config/com.opcos.desktop/opcos.db`,
**`opcos.db-wal` and `opcos.db-shm`** (transcript rows land in the WAL long before the main db file),
`secrets.enc`, and the IDE proxy HTML if that surface was opened; report only the counts.
One recording caveat: the remote Windows desktop may have the host's own RVM control window open,
which displays a `Token:` field. It shows up in the VNC surface. Do not zoom into it, and mention it
in the report — it is the host's UI, not something OPCOS leaks.

## Recurring failure modes to check first

- **Ported OpenWorker components vs OPCOS data shapes.** The React components were lifted from
  another product and are typed `props: any`. Whenever a session-scoped view is opened, verify the
  props actually match: `Sidebar` wants `session_id`/`agent`/`liveness`, `Composer` wants
  `model`/`mode`/`models`/`onSend`, `Transcript` wants `items`/`streamingText`/`onRetry`,
  `RightRail`/`AccessSection` want `sessionId`/`toolNames`/`personaId`/`branch`. A mismatch throws
  on first render (`x.includes is not a function`, `undefined is not an object`) and, without an
  error boundary, blanks the entire window. If the app renders empty, right-click → **Inspect
  Element** → Console: the WebKit devtools name the failing component directly. This is the fastest
  diagnostic available in a Tauri window.
- **Theme.** Confirm with pixels, not with the highlighted segment. A stylesheet imported after the
  palette (e.g. a legacy `style.css` hardcoding `:root{background:#…}`) silently defeats
  `html[data-theme]`, so the toggle looks alive while nothing changes. Check **every** surface, not
  just Settings: Settings has honoured dark while the ported Sidebar and the session view stayed
  white, and a cold start with dark persisted has painted the main pane light until the toggle was
  touched. Screenshot Settings + session view + sidebar in both themes, and always include a cold
  start.
- **Host liveness.** The dev-agent's `/api/health` answers 200 **without auth**, so a liveness check
  built on it reports `Online` for a completely invalid token. Always run a negative control: a
  second host with the same URL and a bogus token must go Offline (`/api/info` returns 401).
- **Custom models.** `crates/opcos-provider/src/matrix.rs` is a hardcoded catalogue; models the
  gateway actually serves may be missing from it. Check the gateway's `/v1/models` and use the
  free-text model input. On an OpenAI-compatible gateway, `auto` often auto-routes to the intended
  model while catalogue ids like `gpt-4o` come back as HTTP 400 `not in the catalog`.
- **Approvals.** In `Interactive` mode `write_file`/`run_shell` require approval; the backend emits
  an `approval` event and `read_transcript` replays pending records, so the Approve/Deny card should
  survive a restart. Prove suspension independently — while pending, the remote file must still be
  absent via the host's `/api/read`.
  There are **two** adapters between the engine and the card and they have disagreed before:
  `web/src/transcript.ts` turns events/records into `kind:"tool" + approval:true`, while
  `App.tsx`'s `transcriptItems` memo only emits the `kind:"approval"` Item that `Transcript` renders
  as `<ApprovalCard>`. If the UI shows a bare `Approval required before this tool can continue`
  notice and a pending tool row but no buttons, that mapping gap is the cause — check both files,
  not just the card component.
  **Workaround for blocked rounds:** create a second session with Mode `Auto` so writes/execs run
  without approval. That still proves the whole vertical slice (turn → tool → real remote file),
  and lets you keep testing surfaces and durable resume while approvals are broken.
  **Prove both directions against the host, not the UI:** Deny ⇒ `/api/read` content unchanged;
  Allow ⇒ `/api/read` shows the new content. Use a distinct marker string per round
  (`opcos-<round>-approve` / `-deny`) so a stale value can never be mistaken for a pass.
  **The `write_file` path is the reliable one to exercise approvals**; asking for a shell write
  often makes the model retry with odd Windows/bash-mixed commands that fail on their own.
- **Approvals raised on the `resolve_approval` continuation.** Completion events are emitted from
  the `submit_turn` paths; the continuation after an approval decision may emit nothing. Symptoms:
  after clicking Allow/Deny the turn really continues on the backend, but the header sticks at
  `Running` and any *second* approval shows up only as a red raw banner
  (`approval pending for tool call call_…`) with a tool row frozen at `running` and no card.
  Switching to another session and back re-reads the transcript and renders the card correctly —
  use that as a workaround, and report the difference as a live-event-delivery bug.
  Also check the decision mapping: if `App.tsx` computes `approve: decision === "once"`, then
  `Always allow` / `Allow every time` / `Always allow this command` all send `approve:false`,
  i.e. clicking an "allow" button silently denies. Click **`Allow` / `Allow once`** when you intend
  to approve, and test the other buttons explicitly.
- **Resolved approval cards may not change state** (no `declined`/`approved` chip, buttons stay).
  Do not use the card's appearance as evidence that a decision was or was not applied — use the
  transcript continuation plus `/api/read`.
- **Live vs. reloaded transcript.** The reloaded transcript (`read_transcript` after a restart) can
  be *more* complete than the live one: seen a session sit at `Running` with tool cards frozen at
  `running` forever, then reload after restart with all steps, thinking and the final answer. So
  never conclude "the turn hung" from the live view alone — restart and re-read before judging, and
  report a stuck live view as an event-delivery bug rather than an engine hang.
- **PTY input.** The xterm surface can render the remote PowerShell prompt (output path fine) while
  keystrokes never reach the shell. Always type a command **and** assert its echoed output; a
  rendered prompt alone proves nothing. Try clicking several spots inside the terminal and single
  `key` presses before concluding it is broken.
- **Web IDE.** The proxy can be healthy while the panel stays black. Separate the two: `curl` the
  local proxy port (find it with `ss -ltnp | grep opcos`) — HTTP 200 with VS Code workbench HTML
  means the failure is in the Tauri webview, not the proxy. **Do not stop at `/`** — grep the
  returned HTML for the asset paths it actually requests (`/out/...`, `/resources/...`) and curl
  those too. A 200 root with 502 assets means the proxy's upstream call fails; then curl the
  upstream directly (`$DEVBOX/ide/out/nls.messages.js` with the Bearer header). A 403 there means
  the RVM host wants some IDE-specific token that OPCOS is not forwarding — that is a backend bug,
  not a webview or a route-registration bug.
- **Layout at 1024 px.** Surface toolbar buttons (`Start terminal`, `Refresh`, `Open Web IDE`) sit
  under the right rail and are clipped; collapsing the rail does not reflow the main pane. Click the
  visible sliver (~x=795-810) rather than assuming the button is missing. After collapsing the rail
  the button can disappear entirely — take a zoom screenshot of the toolbar strip as evidence
  instead of relying on the full-window shot.
- **Layout debugging without devtools.** If a collapse/reflow class does not seem to take effect,
  reason from measurable pixels in a zoom screenshot: e.g. a right rail rendered ~330 px wide when
  the media query asks for 220 px means a child (`.session-panel-tabs`, 9 non-wrapping 30 px tabs)
  sets a larger min-content and overrides the grid track. Report the min-content culprit, not just
  "still clipped".
- **Steering.** Send the steer a few seconds after the turn starts while the header still shows the
  running state, then wait and scroll to the very bottom: the steered final answer can render
  *above* the steer user message, and a residual `Ran running` chip may linger. Judge the steer by
  searching for the injected keyword in the final answer, not by transcript ordering.
  The steer user message may also **not be persisted at all** — check the reloaded transcript for it
  before judging any ordering fix; if it is absent, ordering is simply not observable after reload.
- **PTY frame types.** Root cause of "prompt renders, nothing echoes" was frame type: the RVM host
  treats **binary frames as raw stdin and text frames as JSON control messages**, so
  `socket.send(string)` was silently discarded. If PTY input dies again, check that stdin goes out
  as `TextEncoder().encode(data)` and only resize goes out as JSON text. Prove PTY end to end by
  reading back a file the agent wrote earlier in the same round (`Get-Content <marker>.txt`).
- **Approval evidence discipline.** Use fresh per-round marker filenames (`r5-alpha.txt`,
  `r5-bravo.txt`) that are `ENOENT` before the run, and ask for **two** gated `write_file` calls in
  one prompt — that is the cheapest way to exercise the continuation-approval path (deny the first,
  allow the retry, allow the second) without switching sessions.

## Projects / team workspaces (P1-P4 surfaces)

- **No remote host needed for project testing.** Host `local` (`本机`) is built in (`main.rs:3777`) and
  `project_host_contains` restricts local project paths to **under `$HOME`** (`main.rs:2035`). So a
  throwaway `git init` repo under `~` (e.g. `~/opcos-test/demo-repo`, one commit, branch `main`) is a
  complete fixture — put its absolute path in the 新建项目 dialog's 仓库路径 field, leave 仓库 URL empty.
- UI path: sidebar 「项目」 row `+` → 新建项目 → board. Board has 添加成员 / per-card 启动会话·编辑·删除,
  then 项目配置 (规则/Knowledge/Playbook/MCP/Connectors/Blueprint + 项目 Secrets + 项目运行凭据), then
  「Workflow 与 Lead 指挥」 (启动全部 / 暂停 / 恢复 / 推进阶段 / Workflow 定义 / 任务 / 协同消息历史).
- Member semantics: `sort_order==0` ⇒ role must be `Lead`, worktree = repo root; others get
  `<repo>/worktrees/<agent-id>` and branch `agent/<role>-<n>` via a real `git worktree add`.
  **Always verify with `git -C <repo> worktree list` + `ls`** — the UI card is not evidence.

### The trap that wastes the most time: stale UI + WAL-invisible DB

- **The project board does not refresh when backend rows disappear.** It keeps rendering member cards
  from React state, with no error. Never conclude "the data is still there" from the board — kill the
  process and re-launch, or read the DB *after* a clean shutdown.
- `~/.config/com.opcos.desktop/opcos.db` is tiny; **everything lives in `opcos.db-wal`**. Reading the
  live DB (even `mode=ro`) returns stale/empty tables intermittently, and copying db/-wal/-shm while
  the app runs races. Reliable recipe: `pkill -f target/debug/opcos; sleep 4;` then open the db
  normally (shutdown checkpoints the WAL). There is no `sqlite3` CLI on the box — use
  `python3 -c "import sqlite3; ..."`.
- Symptom decoder: `启动全部` returning `coordination topology violation` usually means
  `load_project_agents` returned **zero** rows (`CoordinationRuntime::new` rejects an empty role set),
  i.e. your members are already gone — not a workflow-definition problem.

### Known-broken paths to re-check before writing them off as your own mistake

(The first three were fixed in `a69d6dc`; keep them as regression checks, they are cheap.)

- `保存 Workflow` (and any `update_project`: rename / default branch / archive) once **silently deleted
  every project member**: `save_project` used `INSERT OR REPLACE INTO projects` while
  `project_agents.project_id … ON DELETE CASCADE`, and rusqlite's bundled SQLite defaults
  `SQLITE_DEFAULT_FOREIGN_KEYS=1`. Correct form is `INSERT … ON CONFLICT(id) DO UPDATE SET …`.
  Always snapshot member count **in sqlite after a clean shutdown** before/after any project-level save.
- `启动会话` on a member card once failed with `command create_session missing required key hostId`.
  `host_id` is now `Option<String>` and the host/workspace are resolved from the project + member.
  If it breaks again, create a session from the **首页 New session composer** if you only need *a*
  session to test other surfaces.
- Project archive/delete live on the **project board header** (`归档项目` / `删除项目`), not the sidebar;
  both use `window.confirm`, and delete surfaces the raw backend error on the board with a
  `强制删除并回收 worktree` follow-up button. Grep `web/src` for the command name before assuming an entry
  point exists — e.g. rename / default-branch edit still have **no UI entry** (only archive calls
  `update_project`).
- Settings → Skill → **Browse** could report `暂无仓库 Skill` plus a banner
  `local host I/O failed: No such file or directory (os error 2)` even when a valid
  `.agents/skills/<x>/SKILL.md` existed in the session workspace (`join_remote_path` used `\` on
  POSIX; fixed in `4d4d5e0`). Verify with a real SKILL.md + `.cursor/rules/*.md` you dropped in the
  workspace yourself before concluding "the repo just has no skills", and check the listed paths use
  `/` separators.
- The session right rail's **Diff** panel (4th rail icon from the top → `Refresh`) lists changed files
  since `b287ac6`; since `9229c87` each row renders the real `change.path` plus `changeType` and
  `+additions/-deletions` (assert against `git diff --numstat HEAD`). Earlier builds rendered
  `JSON.stringify(file)` and passed that string as `path`, so unreadable `{"additions":1,…}` rows and
  a blank diff pane mean you are on a pre-`9229c87` frontend.
  Layout: since `fe29895` the panel uses a **container query** (`.review-panel { container-type:
  inline-size }` + `@container (min-width: 900px)`), so the narrow right rail renders single-column
  (file list on top, `Select a changed file.` placeholder / diff below, `.diff-view` scrolling on its
  own between 240-420px) and only wide containers get 2 columns. Between `b287ac6` and `a10e280` the
  rail clipped the diff to a ~10px sliver (`a10e280`'s `@media (min-width: 900px)` was a *viewport*
  query, useless for a ~180px rail) — if you see that sliver you are on an old frontend.
  The 放大（独立窗口打开）pop-out is still useful for wide 2-column viewing, but note it opens at
  ~575px, i.e. **single-column** until you maximize it past 900px.
  Long paths are CSS-ellipsized with the full path in
  `title` — hover to screenshot the tooltip, and check the diff header `diff --git a/<full path>` to
  prove the click sent the full, untruncated path.
  Since `a10e280` local `changeType` comes from `git diff --name-status --find-renames`
  (added / modified / deleted / renamed); before that everything was hardcoded `modified`. Good
  one-shot fixture to check the numstat↔name-status zip doesn't slip: in one repo state stage an
  added file, a modified file, a `git rm`-ed file and a `git mv`-ed file, then compare all four rows.
  File tree / Web IDE / Shell / Desktop / Browser tabs only exist for **non-local** hosts
  (`App.tsx:7133-7145`), so with no RVM they are untestable.
- (Fixed in `4d4d5e0`) Deleting a member/project now also runs `git branch -D <agent branch>`, so
  `git branch --list 'agent/*'` should be empty afterwards and the same repo can be reused. Before
  that fix a second project on the same repo failed with
  `fatal: 'agent/code-1' is already used by worktree`.
- (Fixed in `b287ac6`) `删除项目` used to be blocked as dirty for any project with a non-Lead member,
  because OPCOS's own untracked `<repo>/worktrees/` dirtied the Lead checkout. The Lead dirty check
  now filters that path and the empty container is `rmdir`-ed after cleanup. Regression recipe: with a
  **clean** fixture repo, create project + Lead + Code member, then delete without force — it must
  succeed and leave `git worktree list` at main only, `git branch --list 'agent/*'` empty, and no
  `<repo>/worktrees` dir. Counter-check: modify a **tracked** file first; the delete must still fail
  with `worktree has uncommitted changes; use force to remove it`, and force must not revert the edit.
- After deleting a project, the retained session's workspace points at the removed worktree; opening
  Settings → Skill then pops a red toast
  `本机 workspace 不可用: local host I/O failed: No such file or directory (os error 2)`. Expected
  fallout, not a Skill-discovery regression.
- Adding the **first** member with a non-Lead role fails with
  `store validation error: sort_order 0 project member must have Lead role` — always create the Lead
  first.
- (Fixed in `4d4d5e0`) Advancing past the last workflow stage now renders 当前阶段 as `已完成`
  (previously `未启动`). Save a **single-stage** workflow to reach that state in one click.
- `delete_project` keeps the member sessions and only nulls their `project_id`/`agent_id`; their
  `workspace` then points at a deleted worktree. Expected today — assert on nulled ownership, not on
  the session rows disappearing.
- Slash-command completion only works in the **session** composer; the 首页 New session composer is not
  passed `slashCommands`, so `/` there legitimately shows nothing.

## Template / preset market (Settings → 市场)

- Fresh config dir ⇒ 8 builtin templates are seeded (`main.rs:1322-1417`): agent Lead/Code/Review/
  Test/DevOps, teams `Lead + Code + Review` and `Lead + Code + Review + Test + DevOps`, plus one
  blueprint. Verify with
  `python3 -c "import sqlite3; …select id,kind,name,status from config_object where scope_kind='template'"`.
  Builtins render `内置模板只读。` + `另存为` only; there is **no** edit/delete UI at all
  (`delete_template` has no frontend call site), so "builtins can't be deleted" can only be proven
  at the UI level — say so rather than claiming the backend guard was exercised.
- **Repository import/export must resolve `.agents/templates/*` against `project.repo_root`**
  (`repository_path()`, added in `a658853`; local hosts concatenate `repo_root`, remote hosts derive
  a workspace-relative path). Regression symptom if it ever reverts to the host root: 从仓库导入
  aborts with a bare `"local host I/O failed: No such file or directory (os error 2)"` and imports
  nothing (`LocalHost::secure_path` canonicalizes the missing `$HOME/.agents` parent), or the export
  confirm dialog shows a `$HOME/.agents/...` target. **Always `rm -rf $HOME/.agents` before this
  test and re-`ls` it after each import/export assertion** — an existing `$HOME/.agents` silently
  masks the bug, and never create one as a fixture "workaround". Missing template directories are
  non-fatal (the `ls` error path just `continue`s), so a repo with only `.agents/knowledge` +
  `AGENTS.md` must still import those with an empty `rejected` list.
- Import semantics to assert (they are per-record, `main.rs:6892`): first run `imported`, unchanged
  re-run `unchanged` with empty `conflicts` (an all-`conflict` second run is the old bug), edited
  source `updated` + version bump on the market card, malformed YAML and name-less YAML rejected
  **individually** with full path + reason while the valid files still import.
- Project 配置模板 lifecycle (fixed in `a658853`; `list_project_configuration_templates` now selects
  `p.status`): checking copies the template into project scope and the box **stays ✓** across page
  switches; unchecking pops `将删除项目作用域配置「…」`, and OK flips the row to `status='deleted'`;
  re-checking revives it to `active` with the template's *current* content. If the box springs back
  to unchecked and re-clicking just re-copies, the `applied` column mapping has regressed.
  Verify state in sqlite: `config_object where scope_kind='project'`
  (object id is `project-<project_id>-<template_id>`).
- After the template source changes, a checked project copy keeps the old content and the row shows
  `· 已本地修改`; re-checking then pops `重新勾选将用模板当前内容覆盖本地修改` and OK pulls the new
  content. Good one-shot proof of copy-not-reference plus the overwrite warning.
- Team-template project creation writes the workflow and copies checked config templates in one
  command; assert `project_agents.sort_order=0` is Lead, `projects.workflow_json` stages, and the
  project-scope `config_object`. On disk expect **N-1 worktrees**: the Lead uses the repo root and
  gets the project's default branch (`main`) as of `a658853` — a Lead card showing `agent/lead-0`
  is a regression, since that branch is never created.
  Failure rollback works (non-git repo ⇒ explicit error, no project/member/worktree left).
- Save-As duplicate names error with `同名自定义模板已存在: <name>`; assert no orphans by snapshotting
  `count(config_object)` / `count(config_object_version)` before and after the failing retry.
  当前项目另存为 Team reuses existing templates by kind + content hash (builtin first) as of
  `a658853`, so it should add exactly **one** row (the Team template) — snapshot
  `count(config_object)`/`count(config_object_version)` around it and check
  `group by kind,name having count(*)>1` is empty.
- New-session Agent template prefill: there is no visible system-prompt field; the prompt is stored
  on session create as a session-scope `config_object` named `Agent template system prompt`
  (`session-<id>-agent-template`). Read it from sqlite to prove prefill + manual edits survived.

### Cheap, high-signal assertions for these surfaces

- Secrets isolation: store a project secret, **plus a global control secret**, then open global
  Settings → Secrets. An empty global list alone proves nothing — the control secret must be visible
  while the project one is not. Back it with `secret_records(name, project_id)` and a counting-only
  `grep -c` over `secrets.enc`.
- Scope switching: the Devin tab's 配置作用域 `SelectMenu` overlays the rows below it — press `Escape`
  and re-screenshot before clicking Computer use / Batch limit, or your clicks land on the dropdown.
- Session↔worktree binding: after 启动会话, check the right-rail Info panel (HOST / WORKSPACE) for the
  visual proof, and after a clean shutdown assert in sqlite that `sessions.workspace` equals the
  member's `project_agents.worktree_path`, `host_id` equals the project host, and the agent has
  exactly one session row. A duplicate-start attempt cannot be triggered from the UI (the button
  becomes 打开会话) — report it as untested rather than passed.
- Member delete: dirty the worktree first (`echo dirty > <wt>/dirty.txt`); the expected error is
  `worktree has uncommitted changes…` with a 强制删除 button, and force must remove the directory
  from disk, not just the card.

## Testing the working-event timeline (`buildTimeline`)

The React timeline is built by `web/src/timeline.ts::buildTimeline` from persisted `session_events`.
Its unit fixtures were originally captured from **Devin's** event stream, not OPCOS', so green unit
tests prove nothing about the real app. Always drive a real session.

### Replay `buildTimeline` over real events instead of counting rows by eye

This is the single highest-value technique for this surface — it gives an exact, complete row list
including things that are invisible in the UI (empty bubbles, zero-row groups):

```bash
# 1. dump the session's persisted events (after a clean shutdown; copy the WAL, see below)
python3 -c "
import sqlite3,json
c=sqlite3.connect('/tmp/snapshot.db')
rows=[json.loads(r[0]) for r in c.execute(
  'select event_json from session_events where session_id=? order by created_at_ms,sequence',(SID,))]
open('/tmp/ev.json','w').write(json.dumps(rows))"
# 2. replay the real implementation
cat > /tmp/replay.ts <<'EOF'
import { readFileSync } from "node:fs";
import { buildTimeline } from "/home/ubuntu/repos/OPCOS/web/src/timeline";
const nodes = buildTimeline(JSON.parse(readFileSync("/tmp/ev.json","utf8")) as any);
nodes.forEach((n: any, i: number) => {
  if (n.kind === "work") { console.log(`[${i}] WORK ${n.label} rows=${n.rows.length}`);
    n.rows.forEach((r: any) => console.log(`        - ${r.label}`)); }
  else console.log(`[${i}] ${n.kind.toUpperCase()} ${JSON.stringify(n.text ?? "").slice(0,70)}`);
});
EOF
(cd /home/ubuntu/repos/OPCOS/web && npx tsx /tmp/replay.ts)
```

Flag as **empty artifacts**: any `work` node with `rows.length === 0`, any `assistant`/`notice` node
whose text is empty/whitespace. The historical cause was the engine emitting `devin_message` with
`message: ""` on tool-call-only iterations, producing blank assistant bubbles and `Worked for 0s`
zero-row groups.

When empty-artifact guards are added, **prove nothing legitimate was dropped** as well as that the
empties are gone — a guard that is too aggressive looks identical to a passing test otherwise:

- rendered assistant-node count **==** persisted `devin_message` count with non-empty `message`
- at least one `Worked for 0s` group **that has rows** still renders (0s duration is legitimate)
- work-node count is unchanged apart from the genuinely empty ones

Beware a false positive when two consecutive assistant bubbles have identical short text (e.g. two
`4`s): check the persisted timestamps before calling it a rendering duplicate — the model does
sometimes answer twice across iterations.

### Composer and app-bring-up nits that cost time

- After an **action** slash command is submitted the autocomplete popup can stay open even though the
  textarea clears. The next click at the "usual" textarea position then lands on the popup.
  Re-screenshot after every send; the textarea is the line showing the `Ask OPCOS…` placeholder.
- The composer's vertical position moves as the transcript grows (roughly y≈645 vs y≈673 on a
  maximized 1024×768 window). Do not reuse coordinates between steps.
- The Home composer's **Workspace field is not cleared between session creations** — typing into it
  appends to the old value and silently produces a mangled path. Always `ctrl+a` first, then verify
  the resulting `sessions.workspace` in the DB.
- Pick the **model** explicitly on the Home composer; the default is `auto`, which fails the turn with
  `Provider request failed` and surfaces no error on the Home screen at all.
- Kill stray Vite servers before starting (`pkill -f vite`, then confirm with `ps`); if :1420 is taken
  Vite silently moves to :1421 and the Tauri window keeps loading the **old** server. `pkill` can need
  a follow-up `kill -9` on the `sh -c vite` wrapper.

### Force all row families in one prompt

> Plan this out as a task list first, then do it. In this workspace: 1) create `X.py` with two
> functions; 2) create `README_X.md` documenting them; 3) run `python3 X.py`; 4) add a `--flag` and
> re-run. Update the task list as you finish each step.

Step 4 is what produces an **edit** (`action_type:"edit"`); a task the model gets right first time
only ever yields `create` rows. Cross-check the numbers: sum
`multi_edit_result.file_updates[].lines_added - lines_removed` per file and compare with `wc -l` —
they have matched exactly every round, so drift is a real bug.

### Task rows: `steps`, not `todos`; and watch the reset scope

`todo_update` payloads are serialized `PlanRecord`s: `{plan_id, title, status, revision,
steps:[{step_id, position, description, status}]}`. There is **no `todos` key**.

Plan state must be keyed by `plan_id`, not reset at every work-group flush — when it was
work-group-scoped, the interleaved `devin_message` per iteration reset it and you got
`Created N Tasks` repeated once per group with **zero** `k/n#i` rows. Correct output looks like
`Created 5 Tasks` once followed by `0/5#1 …`, `1/5#1 …`, `1/5#2 …` … `5/5#5 …` (two rows per
`plan_update` is normal: "previous step done" + "next step in progress").

### Slash commands: action vs prompt

`builtin_control_slash_commands()` (`/compact`, `/mode`, `/model`, `/ls`, `/help`) are
`execution:"action"`; `builtin_slash_commands()` (`/implement`, `/plan`, `/review`, `/test`,
`/think-hard`, `/deploy`, `/pull-project`) are `execution:"prompt"`. The composer autocomplete labels
them `ACTION` / `PROMPT` — use that as the quick visual check. Test both kinds every round: an action
must produce **no user prose bubble** plus a backend effect and a rendered notice
(`session_list`, `mode_current`, `mode_changed`, `model_current`, `model_switch`, `slash_help`);
a prompt must expand into its body text in the user bubble.

**Composer click-target trap:** the slash autocomplete popup sits directly under the textarea and
shifts the layout, so clicking where the textarea "usually" is often hits the popup and silently
drops your typing. Screenshot first, click the `Ask OPCOS…` placeholder line specifically, and press
`Escape` to dismiss the popup before clicking send.

### Compaction

`/compact` on the Builtin harness calls `engine.compact_now()`. Expect exactly one persisted
`compacted` event, `Earlier context compacted` as a **row inside** a `Worked for …` group (not a
standalone notice), and a real non-empty summary in the `compaction_state` table:

```sql
select state from compaction_state where session_id=?;   -- JSON with a "summary" field
```

Assert `len(summary) > 0` — a `compaction_summary_invalid` event means the model's summary was
rejected (`response_too_large`) and compaction was lossy. Note the summary length varies a lot run to
run (1.9k–14k chars observed), so if you are testing a size cap, check the actual length before
claiming the cap was exercised.

### Stuck `Running` header

The header can stay `Running`/`Working` indefinitely while `sessions.run_state` is already `idle` —
**always cross-check the DB before believing the UI**:

```sql
select run_state, updated_at from sessions where session_id=?;
```

Several causes have been fixed over time (listener re-subscription on `selected?.id`; missing terminal
event after `/compact` itself; the session *list* not converging on the authoritative `run_state`).
Test this whole matrix separately every round — the sub-cases behave differently and a fix can land
for some and not others:

| Situation | Expected |
|---|---|
| Normal turn, no prior `/compact` | terminal unattended |
| Immediately after `/compact` | terminal unattended |
| Turn *following* a `/compact`, same mount | terminal unattended |
| Turn after navigating out and back in | terminal unattended |
| `⏹ Stop` mid-turn (incl. after a `/compact`) | clears to Ready / `Turn interrupted` |
| `⏹ Stop` on an already-stuck header | clears |

When the natural cases all pass and no header sticks, the last row is not reachable by normal use.
The way to manufacture the crash case is to `pkill -9` the app mid-turn and relaunch. Always confirm
the **precondition** in the DB between the kill and the relaunch (`run_state` should be `running`
/ `none` at that point) — otherwise a passing result after relaunch proves nothing, because you
cannot tell reconciliation from "the turn had already finished".

Status strings are the cheapest on-screen assertion here, and they are rendered in the **session
header subtitle** (`本机 · <workspace> · <model> · <status>`, `App.tsx` → `sessionStatusLabel`), *not*
in the Info pane, whose `STATUS` field reads only `run_state` and still shows `Ready` for interrupted
and error sessions. Zoom the subtitle line and assert the exact string:

| stop_reason | header subtitle |
|---|---|
| `interrupted_by_user` (⏹ Stop) | `已中断` |
| `interrupted_by_crash` (startup reconciliation) | `已中断（应用退出）` |

After any reconciliation change, also check the **negative** case: a session already terminal for
another reason must not be rewritten — e.g. `interrupted_by_user` must survive a restart rather than
becoming `interrupted_by_crash`. And check that the recovered session is *usable*: send a message in
it immediately, with no navigation or restart.

Historical note worth keeping: a `⏹ Stop` implementation that "clears the local flag and then
refreshes" cannot fix an orphaned row, because the refresh reads `running` straight back out of the
DB — the interrupt has to write the terminal state itself.

Two things that make this worth chasing rather than dismissing as cosmetic:

- **It can block the user.** While the flag is stuck the composer's send button is a Stop button, so
  Enter may do nothing and no `user_message` is persisted. Always try to send a follow-up message from
  a stuck header and check the DB — if the message never appears, report it as blocking, not cosmetic.
- **Localise which surface is stale.** The sidebar row's running dot, the Info-pane `STATUS` and the
  composer button read from different places. When the sidebar converges but the header does not, the
  session *list* refresh is working and the cached selected-session object is the culprit. Zoom the
  sidebar and the Info pane separately rather than reporting "the UI is stuck".

### Selection / refresh races (the `selected` derivation)

`selected` is derived from the `sessions` list by id. Any change to how that list is refreshed can
break *selection* rather than the transcript, and the symptom is always the same: **the app silently
falls back to the Home screen**. Because the agent keeps working in the background, this is easy to
miss unless you look. Check all of these after any selection/refresh change:

- create a session from the Home composer → it must navigate immediately **and still be selected
  30–60 s later**, while the turn is running (a race can hold it for only the first few seconds);
- with a session already open and idle, send a message → you must **stay** in the session when it
  transitions to `running`;
- click a **currently running** session's card on Home and its row in the sidebar → both must open it;
- issue an action command such as `/compact` → must not navigate away;
- switch between sessions, and delete one, watching for selection loss.

On session deletion: as of this branch the app exposes **no delete affordance at all** (the
`deleteQuestion` / `sessionActions` i18n keys are unused and there is no `delete_session` command in
the frontend or `src-tauri`). Don't waste time hunting for a menu. If you must exercise
deleted-session handling, delete the row from the live `sessions` table with `python3 -c` (there is no
`sqlite3` binary on the box, despite the blueprint) and then trigger a refresh — but label the result
as indicative, since it isn't a product path. Refresh is triggered by a terminal `turn_done`, by
`interrupt`, and on project/session creation — not by plain session switching.

Isolate with the DB: if `sessions.run_state='running'` and `session_events` is climbing while the UI
shows Home, the run is fine and only selection is broken. A useful discriminator is idle vs running —
if idle sessions open and running ones do not, the bug is in the list/derivation race, not in the
transcript.

Note this failure mode also **blocks most of the rest of the GUI matrix** (stuck-`Running`, `⏹ Stop`,
steering, blocked-submission notices all need a visible running session), so test selection first and
escalate immediately if it fails.

### Steering and blocked submission: the composer can eat your message

Sending while a turn is running is routed by `submissionRoute(running, canSteer)`. Test it and check
the DB, because the UI gives you almost nothing:

- A steer should render a **user bubble** within a couple of seconds and persist exactly **one**
  `user_message` event whose payload carries `"source": "steering"` (normal prompts carry no
  `source` key). Assert the count, not just presence — an earlier version appended the steer twice
  (once when queued, once when consumed mid-loop), so **two** events per steer is a regression.
- Always use a distinctive marker string (`STEER ONE:` / `POST CRASH:`) so DB greps are unambiguous,
  and always reconcile **messages you submitted through the GUI** against
  `select count(*) ... where type='user_message'`. That single number caught both a silent drop and
  a duplicate append in different rounds.
- Don't conclude "silently dropped" from a missing `user_message` alone: a steer can be delivered to
  the model without being persisted. Grep the whole event JSON — it shows up inside `turn` /
  `devin_thoughts` payloads if it really reached the model — and check whether the agent acted on it.
- The `blocked` branch and its notice (`The session is still running; your message was not sent.`)
  may be unreachable with the Builtin harness, since `canSteer` is true there. If you never see it,
  say so rather than marking it passed.

### Parity recipe: live == in-app re-read == cold restart

Replay dumps: dump the **whole `event_json` row**, not `working_event` — a large fraction of rows
(178 of 699 in one run) have `working_event: null` and `buildTimeline` throws on them.

When a round touches Rust, don't trust `cargo build` saying "Finished" or the binary mtime (a rebase
can leave the commit date *after* a perfectly current binary). Verify the change is in the artifact:
`strings target/debug/opcos | grep -c <new_symbol_or_literal>`, and also grep for a literal the
commit **removed** — seeing the new string and a zero count for the old one is conclusive.

Also check for a stray second app instance before recording (`wmctrl -l` showing `OPCOS <2>`): two
windows share one SQLite file and make every DB assertion ambiguous.

1. Capture the ordered row labels while the turn streams (expand every `<details>` group).
2. **`Ctrl+R` does not reload the Tauri webview.** Navigate to another view and back into the session
   to force a `read_transcript`.
3. `pkill -f target/debug/opcos`, relaunch, reopen the session.

Expand groups by clicking their summary rows **bottom-up** — expanding a group pushes everything
below it down, so top-down clicking hits the wrong targets.

### Context window resolution

The nextapi gateway reports no `context_length` for `glm-5.2` (`capabilities_known=false` in
`model_discovery_cache`), so a `context_growth_update` with `resolved_context_window=1000000` and
`context_window_source='matrix'` proves the matrix fallback rather than a gateway value. Assert over
**every** such event.

"No auto-compaction" is only decisive if `max(estimated_context_tokens)` actually exceeded the *old*
threshold of 24,000 (32k × ¾) — short runs stay under it anyway and prove nothing. A 6–9 step task
with file creation, edits, a CLI flag and lint/commit gates reaches ~32–40k tokens, which is enough;
a 4-step one peaks around 22–25k, which is not.

### Envelope integrity one-liner

After a clean shutdown, assert on every persisted event: non-empty `type`, non-empty `event_id`,
integer `created_at_ms`, unique ids, monotonic `created_at_ms`; plus zero empty `devin_thoughts`
bodies and no two consecutive identical thoughts.

### Provider bring-up on a gateway

Settings → Provider → OpenAI: base URL, paste key into the password field, Validate, pick the model.
After first configuring a provider the **home composer's model list stays on the built-in matrix
models until a full app restart** — restart before concluding the model is unavailable. Once
restarted the picker opens in <1 s; a multi-second `Loading models…` means something is probing every
model.

### Known-failing unit test (not a headless artifact)

`opcos-hosts::local_desktop::tests::x11_capture_and_input_are_real_when_display_is_available` fails
**even with `DISPLAY=:0` on a real X server that has XTEST**. The screenshot half passes; only
`Enigo::new()` / `computer_use(MouseMove …)` fails with
`local input unavailable: could not initialize the X11 input backend`. Treat it as an enigo backend /
feature-flag gap, not a headless artifact, and do not "fix" it by re-running with a display.

## Testing the local host's persistent shell (no remote host needed)

The built-in local host `本机` is enough to test `LocalHost::exec_persistent_streaming` end to end,
which is where `run_shell`/`exec` land when a session is bound to `本机`
(`src-tauri/src/main.rs`, `DesktopExecutor::Local::execute_streaming`). Fixture: any throwaway dir
**under `$HOME`** (containment is enforced), e.g. `~/opcos-shell-test/subdir/deeper`.

- Put the absolute path in the home composer's **Workspace** field, host `本机`, mode `Auto` (no
  approval cards), then send prompts of the form *"Use N separate run_shell calls, one command each,
  do not modify them, do not pass a cwd argument: …"*. Weaker models otherwise collapse the commands
  into one call with `&&`, or pass a `cwd` argument that silently resets the shell's cwd
  (`change_cwd`) and destroys any cwd-persistence assertion.
- The persistent shell session key is `opcos-local-<session_id>`, so **cwd/env persistence must be
  tested inside one session**; every tool row shows the same `shell-<id>` chip when it is reused.
- **Proving output streams incrementally:** run `for i in 1 .. 9; do echo "TICK-$i"; sleep 3; done`
  (keep total < `DEFAULT_EXEC_TIMEOUT_SECONDS = 30`, `crates/opcos-rvm/src/lib.rs`), then click the
  tool row's `Show output` toggle **while the turn is still running** and capture two snapshots a few
  seconds apart. A working streaming loop shows a strictly growing line count with no exit chip yet;
  a non-streaming regression shows an empty block until the row flips to `exit 0`.
- Two independent truncation limits exist and are easy to confuse. The **live** terminal block in the
  tool row is capped at 64 chunks × 2000 chars (`opcos-engine/src/lib.rs`), so a 50 000-line command
  renders only the first ~16 lines there. The **model-visible** result is capped at 64 KiB by
  `bounded_output_text` (`src-tauri/src/main.rs`), which prepends
  `[Output truncated: omitted N bytes; showing the last 64 KiB]` and keeps the *tail*. Assert the tail
  (e.g. line `50000`) from the assistant's reply, not from the row's short live block.
- Useful adversarial commands for this path: `sh -c 'exit 7'` (exit-code fidelity — a broken wrapper
  reports `1`), and
  `printf 'a\nb\nc\n' | grep -c b; echo "PIPESTATUS0=${PIPESTATUS[0]}"`. On POSIX the pipeline must
  return `exit 0`; `local host I/O failed: local shell exited` there means the shell died mid-command
  (`shell_exit_diagnostic`) and is always a bug.
- Cheap post-run checks: `grep -c "local shell exited"` in the app log (expect 0) and
  `ls /tmp/opcos-shell-output-*` (expect none — temp files should be cleaned up).
- The Windows PowerShell wrapper in the same file **cannot** be exercised from Linux. Say so
  explicitly rather than implying the fix is proven; it needs a Windows host — see
  "Testing the Windows PowerShell wrapper on a real Windows host" below.

### Testing the Windows PowerShell wrapper on a real Windows host

The `windows_persistent_command` / `windows_persistent_streaming_command` builders in
`crates/opcos-hosts/src/lib.rs` are `#[cfg_attr(not(windows), allow(dead_code))]`, so they **compile
and run on Linux** even though the code path they feed is Windows-only. That is the lever that makes
Windows testing possible without a Windows Rust toolchain.

Prefer `cargo test -p opcos-hosts` on the Windows host if you can, but **measure the host's download
throughput before committing to it**:

```powershell
$sw=[Diagnostics.Stopwatch]::StartNew(); & curl.exe -s -m 20 -o probe.bin https://static.crates.io/crates/tokio/tokio-1.40.0.crate; $sw.Stop()
```

On an RVM Windows dev-agent this measured ~80-110 KB/s, which makes a toolchain (~180 MB) plus a
mingw-w64 linker (~130 MB; these boxes typically have no MSVC, no gcc and no winget, and rustup does
**not** bundle a GNU linker driver) plus the crates.io tree (~200-300 MB) a 1.5-2 hour download
before anything compiles. Also note `Invoke-WebRequest` may hang at 0 bytes indefinitely on these
hosts — use the built-in `curl.exe` instead. Be aware that even a green Windows `cargo test` would
skip most of the persistent-shell runtime tests, because they are `#[cfg(not(windows))]`.

The practical route (validates the wrapper + marker protocol against real PowerShell, not the Rust
async plumbing — say so in the report):

1. Add a **temporary** `#[test]` in `mod tests` that calls the real builders with Windows-style paths
   (`PathBuf::from(r"C:\...")` — `display()` passes the string through on Linux) and writes
   `{index, marker, wrapper}` JSON to `/tmp`. Never hand-write the wrapper; revert the test after.
2. `/api/write` the JSON to the host (accepts ≥400 KB bodies; chunk + `tar` reassembly works for
   bigger payloads), then drive it with a PowerShell script that starts **one**
   `powershell.exe -NoProfile -NonInteractive -Command -` via `System.Diagnostics.ProcessStartInfo`
   with `StandardOutputEncoding = [Text.Encoding]::UTF8`, and per case: `WriteLine($wrapper)`,
   then read until a line containing `"<marker>:"` and split the remainder on `':'` with count 2
   (the cwd contains `C:`). Run all cases through the *same* child so "the session survived" means
   something.
3. Read the child's stdout **one char at a time** (`$out.ReadAsync($buf,0,1)` with `.Wait(ms)` for a
   timeout), not `ReadLine()`, so CR/LF bytes survive exactly as the Rust reader sees them. Never use
   `ReadToEndAsync()` — it only completes at EOF, so it hangs forever on a live persistent shell.
4. `/api/exec-sync` has a hard **30 s** timeout, so launch the driver detached
   (`Start-Process powershell.exe -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',… -WindowStyle Hidden`)
   and poll a log file it appends to.

Things that bit and are likely to bite again:

- **Output is CRLF on real Windows, LF under Linux/macOS `pwsh`.** The wrapper redirects with `>`
  (CRLF) and replays the file verbatim, and nothing in `opcos-hosts` strips `\r`. The repo's own
  `windows_persistent_wrapper_works_with_real_powershell` asserts `assert_eq!(output, "plain\n")`, so
  it is green in CI on Linux `pwsh` but would **fail on Windows**. Expect this class of
  LF-only-assertion bug in anything claiming Windows coverage; check whether such a test has ever run
  on Windows before trusting it.
- To generate the **pre-PR** wrapper for a differential ("was it really broken?") test, use
  `git worktree add` at the PR's code base. Old Windows-only branches guarded by `#[cfg(windows)]`
  (e.g. `persistent_env_prefix` returning `setlocal EnableDelayedExpansion && `) will not appear in a
  Linux build — temporarily flip `#[cfg(windows)]`→`#[cfg(all())]` and
  `#[cfg(not(windows))]`→`#[cfg(any())]` inside that one function in the throwaway worktree.
  Symptom of the old bug on PowerShell 5.1: `标记"&&"不是此版本中的有效语句分隔符` /
  `FullyQualifiedErrorId : InvalidEndOfLine` and, decisively, **no marker line at all** — which is
  what surfaces as `local host I/O failed: local shell exited` or a timeout.
- Parse errors emitted *before* the wrapper's `[Console]::OutputEncoding=UTF8` line runs come out in
  the console's ANSI code page (GBK on a zh-CN host) and look like mojibake if you decode UTF-8.
  Don't chase it; judge on the presence/absence of the marker.
- To prove incremental streaming on Windows, sample `<output_path>.working` (the staging file the
  streaming wrapper redirects into) every ~400 ms while the command runs, opening it with
  `[IO.File]::Open($p,'Open','Read','ReadWrite')` so the share mode does not conflict. A growing byte
  count before the marker arrives is the Windows-side precondition for live terminal output.
- Useful Windows-specific adversarial cases: `& cmd.exe /c exit 3` must report **3** and the *next*
  pure-cmdlet command must report **0** (no `$LASTEXITCODE` leak); `if (` must be a non-zero result
  rather than shell death; and `pwsh` being absent is a *feature* — it exercises
  `spawn_persistent_shell`'s `pwsh.exe` → `powershell.exe` fallback. You can only verify that
  fallback's premise without a toolchain (`[Diagnostics.Process]::Start("pwsh.exe",…)` throws
  `The system cannot find the file specified`); do not claim the Rust branch itself ran.
- `test` and `printf` resolve to the MSYS2 binaries bundled with git-for-windows, so bash-ish
  commands produce `/usr/bin/test: …` messages and pass through *their* exit code (e.g. 2), not a
  fixed 1.

### Cloudflare Workers AI as the provider

A usable free provider when no gateway key is around, using `CF_ID` + `CF_TOKEN`:

- Settings → Provider → **Cloudflare Workers AI**. Filling only *API token* + *Cloudflare account ID*
  makes **Save and validate** fail with
  `Provider validation failed: provider base URL is not configured; enter one in Provider settings`,
  because the descriptor has `default_base_url: None` (`crates/opcos-provider/src/registry.rs`) while
  only the session path derives the URL from the account id (`src-tauri/src/main.rs`). Workaround:
  type the base URL by hand — `https://api.cloudflare.com/client/v4/accounts/<account-id>/ai/v1` —
  then validate; you should get `Provider key validated successfully.`
- `@cf/zai-org/glm-5.2` (first Cloudflare row in `matrix.rs`) returns
  `not available on the Workers Free plan`. Use **`@cf/zai-org/glm-4.7-flash`**, which is free and
  does emit real `tool_calls`. Sanity-check any candidate model with one `curl` to
  `…/ai/v1/chat/completions` including a `tools` array before spending GUI time on it.
- Note that the account id (not the token) legitimately lands in `opcos.db` as
  `provider.account_id.cloudflare` and inside the stored base URL — expect a non-zero count for it in
  a leak check, and assert only that the **token** count is 0.

## Devin secrets needed

- `CF_ID` + `CF_TOKEN` — Cloudflare account id and API token for the Workers AI provider
  (free plan; use model `@cf/zai-org/glm-4.7-flash`). `CF_AI_TOKEN` fails authentication.
- `RVM_WIN_TOKEN` — valid for DevBox `https://devbox.windevos.com` only (Antec `win.windevos.com`
  answers 401 for everything except `/api/health`).
- `RVM_DEVBOX_URL` + `RVM_DEVBOX_TOKEN` — the Windows RVM dev-agent used for Windows PowerShell
  wrapper testing (`https://devbox.windevos.com`, Windows PowerShell **5.1**, has `git`, no `pwsh`,
  no Rust toolchain). Token goes only in an `Authorization: Bearer` header, never in a URL.
- `OPCOS_PROVIDER_KEY` — OpenAI-compatible gateway `https://ai.yaoshen.de5.net/v1`.
- `nextapi_token` — OpenAI-compatible gateway `https://api.nextapi.store/v1` (serves `glm-5.2`;
  reports no context length, which is what makes it useful for testing matrix fallback).
## GitHub Enterprise 实例 panel (project 运行凭据)

- Entry point: project board → scroll to the **bottom** section 项目运行凭据 → the card
  **GitHub Enterprise 实例**, right column next to *Connector token*. Mouse-wheel scrolling over the
  centre column can land inside the huge 全局预设 connector catalog (clicking a label there pops a
  `window.confirm` about removing a preset — Cancel it); scroll with the cursor over the **left
  sidebar** (x≈250) instead, it scrolls the page without hitting catalog controls.
- The instance list is **global**, not project-scoped (`list_github_enterprise_instances` takes no
  project id), but it is only rendered inside `ProjectConfigPanel`, so you need at least one project
  to reach it. Fastest fixture: sidebar 项目 `+` → name + 仓库路径 `~/opcos-test/demo-repo` → press
  `Return` in the name field (the 创建 button is below the fold and the dialog does not wheel-scroll).
- Validation errors from `save_github_enterprise_instance` surface in the app-level red
  `error-banner` at the bottom of the window (App.tsx `onError` → `setError`), not inside the card.
  Zoom into that strip to read them. Expected strings:
  `GitHub Enterprise API base host <x> does not match instance host <y>` and
  `github.com is always available and must not be registered as a GitHub Enterprise instance`.
- Persistence lives in `github_instances` (store migration 12). Verify with
  `pkill -f target/debug/opcos; sleep 4;` then
  `python3 -c "import sqlite3; ..."` on `~/.config/com.opcos.desktop/opcos.db` — the WAL is
  checkpointed on shutdown, so a live read is unreliable.
- Registering with an explicit but path-less API base (`https://<host>`) is a stronger check than an
  empty one: both must end up as `https://<host>/api/v3`.
- No GHES server exists on the box; do not attempt live Enterprise API calls — assert on
  registration/validation/persistence only and say so in the report.

## Testing a newly registered LLM provider (no remote host required)

A provider PR (a `descriptor(...)` in `crates/opcos-provider/src/registry.rs` + entries in
`matrix.rs`) can be fully accepted against the built-in `本机 (local)` host — you do **not** need an
RVM token. Recommended flow, all in the real Tauri window:

1. Settings → Provider → the new card. Assert the default Base URL, the presence of the
   `Provider key` password field (`needs_key`), and `Not configured yet.`
2. **Capture the pre-key state first**: with no key stored, `provider_models` fails and the card
   shows `来源：内置回退（provider key is not configured）` with only the `matrix.rs` models. That is
   the perfect "before" screenshot for the dynamic-discovery assertion.
3. Type the key from stdin (`printf '%s' "$TOK" | DISPLAY=:0 xdotool type --file -`), click
   `Test / Save` → `Provider key validated successfully.`
4. Click the card's `刷新` button, then open the `Model` select. **The discriminator between
   "real discovery" and "hardcoded list" is a model id the gateway serves but `matrix.rs` does not
   contain** — curl `<base>/v1/models` yourself first and pick one. The source line must flip to
   `来源：API 实时发现`. Non-chat models hide behind `显示全部模型`; unknown-capability models are
   suffixed `(能力未知)`.
5. Discovery is cached 300 s per (provider, base_url) in `model_discovery`; only `刷新`
   (`refresh: true`) bypasses it — a stale-looking list is usually the cache, not a bug.
6. Home composer chips, left→right: Agent template / Role / Harness / 绑定主机 / Provider / 模型 /
   模式 / Workspace. Pick 绑定主机 `本机`, the new Provider, a discovered model, 模式 `Auto`, and a
   workspace dir you created under `$HOME`; typing the prompt and pressing send creates the session
   *and* submits the turn in one go.

### Provider-surface gotchas found this way

- **The provider card's `Model` select is not persisted.** `save_provider_settings` takes only
  `provider` + `base_url` (`src-tauri/src/main.rs`), and `providerModels` in `App.tsx` is local React
  state, so after a restart the card always falls back to `descriptor.recommended_model`. Only the
  per-session model (`sessions.model`) survives. Assert persistence against the DB, and expect this
  to still be broken unless a PR adds a model parameter to `save_provider_settings`.
- **A failed validation still leaves `✓ Configured securely.`** and overwrites the previously stored
  good key, so always re-enter the real key after a negative test.
- Negative-key wording is `Provider validation failed: provider model discovery returned HTTP 401
  Unauthorized` — a good anchor, and it does not echo the key.
- **Tool step arguments render as `[object Object]`** in the transcript `raw` toggle, so you cannot
  assert tool arguments from the UI. Prove tool execution from disk instead (write a marker file and
  `cat` it), and report the rendering gap separately.
- **There is no usage UI**; usage only lands in `usage_events` (session_id, input/output tokens,
  duration). Read it after a clean shutdown.
- **There is no `delete_session` in `web/src`** — test sessions can only be cleaned up by deleting
  the `sessions` (+ `usage_events`) rows from sqlite after shutdown.
- `computer` `type` does not deliver CJK into the WebView — write prompts in ASCII/English, or paste
  via xdotool `--file -`. A silently-empty textarea after typing Chinese is this, not a focus bug.
- To make shell-only evidence visible in the recording, spawn `konsole` on `DISPLAY=:0` running the
  verification command (`ls`/`cat` of the marker file) and screenshot it next to the app window.

## Testing the working-event timeline (`buildTimeline`) — the surface that keeps regressing

The React timeline is built by `web/src/timeline.ts::buildTimeline` from persisted `session_events`.
Unit fixtures for it were originally captured from **Devin's** event stream, not OPCOS', so green
unit tests prove nothing about the real app. Always drive a real session.

### Force all row families in one prompt

A single prompt that reliably produces every row family:

> Plan this out as a task list first, then do it. In this workspace: 1) create `stats.py` with
> `mean(nums)` and `stdev(nums)`; 2) create `README_STATS.md` documenting them; 3) run
> `python3 stats.py`; 4) add a `--json` flag and re-run. Update the task list as you finish each step.

Step 4 is what produces an **edit** (`action_type:"edit"`); a task the model gets right first time
only ever yields `create` rows. Expect `Worked for Xs +N −M` groups containing `Thought for Ns`,
literal shell command rows, `Created <file> +N`, `Edited <file> +N −M`, and task rows.

### Cross-check the +N/−M numbers instead of eyeballing them

Sum `multi_edit_result.file_updates[].lines_added - lines_removed` per file from sqlite and compare
with `wc -l` on the real file. They matched exactly (52 / 76) in the last two rounds, so any drift is
a real bug, not rounding.

### Parity recipe: live == in-app re-read == cold restart

1. Capture the ordered row labels while the turn streams (expand every `<details>` group).
2. **`Ctrl+R` does not reload the Tauri webview.** Navigate to another view and back into the session
   to force a `read_transcript`.
3. `pkill -f target/debug/opcos`, relaunch, reopen the session.

Expand groups by clicking their summary rows **bottom-up** — expanding a group pushes everything
below it down, so top-down clicking hits the wrong targets.

### Task rows: `steps`, not `todos`

`todo_update` payloads are serialized `PlanRecord`s: `{plan_id, title, status, revision, steps:[{step_id,
position, description, status}]}`. There is **no `todos` key**. `buildTimeline` must read `steps[]` and
`description`, and count `done`/`completed` as complete.

Even after that is fixed, check the *progress* rows separately: `previousTodos` is reset to `[]` at
every `user_message`/`devin_message`/`approval_pending` flush, and this engine emits a `devin_message`
per iteration — so the `todos.length > previousTodos.length` branch fires every time and you get
`Created N Tasks` repeated once per group with **zero** `k/n#i <task>` rows. Verify by replaying the
same branch logic over the persisted events in Python rather than counting rows by eye.

### Slash commands: action vs prompt

`builtin_control_slash_commands()` (`/compact`, `/mode`, `/model`, `/ls`, `/help`) are declared
`execution:"action"`; `builtin_slash_commands()` (`/implement`, `/plan`, `/review`, …) are
`execution:"prompt"`. The composer autocomplete labels them `ACTION` / `PROMPT` — use that as the
quick visual check. Test both kinds every round: an action must produce **no user prose bubble** and
a backend effect (`/compact` ⇒ exactly one persisted `compacted` event), a prompt must expand into
its body text in the user bubble.

Caveat: actions other than `/compact` (`/ls`, `/mode`, `/model`, `/help`) emit a `notice` whose kind
is not in `timeline.ts`'s notice allow-list (`error`, `interrupted`, `provider_error`,
`compaction_summary_invalid`), so they run but show **nothing** in the UI. Do not read "no output" as
"the command did not run" — check sqlite/backend state.

**Composer click target:** the slash autocomplete popup sits directly under the textarea and shifts
the layout. Clicking where the textarea "usually" is often hits the popup and silently drops your
typing. Screenshot first, click the `Ask OPCOS…` placeholder line specifically, and press `Escape` to
dismiss the popup before clicking send.

### Compaction

`/compact` on the Builtin harness calls `engine.compact_now()`. Expect `Earlier context compacted` as
a **row inside** a `Worked for …` group (not a standalone notice), and re-check it after a cold
restart. Watch for `compaction_summary_invalid` — `glm-5.2` returned a 14k-char summary that the
engine rejected as `response_too_large`, so compaction succeeded but lost its summary. Note that this
event is emitted **twice**, and the second copy has no `message`/`text`, which renders as an *empty*
notice node — that violates the "no empty artifacts" rule and is easy to miss visually.

### Stuck `Running` header

The header can stay `Running`/`Working` indefinitely while `sessions.run_state` is already `idle` —
always cross-check the DB before believing the UI. One cause (event listener re-subscribing on
`selected?.id` change) has been fixed, but it still reproduced on a turn that followed a `/compact`
(which recreates the engine via `engine_for`). `⏹ Stop` clears the state reliably **during** a real
turn, but does not clear an already-stuck header; only navigating away and back does.

### Context window resolution

The nextapi gateway reports no `context_length` for `glm-5.2` (`capabilities_known=false` in
`model_discovery_cache`), so a `context_growth_update` with `resolved_context_window=1000000` and
`context_window_source='matrix'` proves the matrix fallback rather than a gateway value. Assert over
**every** such event, not just one. "No auto-compaction" alone is weak evidence — a short run stays
under the old 24k threshold anyway.

### Envelope integrity one-liner

After a clean shutdown, assert on every persisted event: non-empty `type`, non-empty `event_id`,
integer `created_at_ms`, unique ids, monotonic `created_at_ms`. Zero empty `devin_thoughts` bodies
and no two consecutive identical thoughts.

### Provider bring-up on a gateway

Settings → Provider → OpenAI: base URL, paste key into the password field, Validate, pick the model.
After configuring a provider the **home composer's model list stays on the built-in matrix models
until a full app restart** — restart before concluding the model is unavailable. Once restarted the
picker opens in <1 s; a multi-second `Loading models…` means something is probing every model.

## Local-host sessions are fully usable (and are the cheapest fixture)

The claim that "OPCOS never executes work locally" is stale: host `本机` (`local`) runs the real
Builtin harness with real tools. A one-commit `git init` repo under `$HOME`
(`~/opcos-test/demo-repo`) plus the home composer's Workspace chip is a complete fixture; no RVM
token is needed to test the timeline, artifacts, iteration stats, shell replay or compaction.
Right-rail panes on a local host are Info / Shell / Changes / Progress / Agents / Artifacts / PR /
Insights / Diff (Worklog, Desktop, Web IDE, Browser appear only for non-local hosts), and the Info
pane opens by default — the "rail buttons hidden behind the topbar" workaround was not needed.

### Which shell path a local `run_shell` really takes

For a local host, `run_shell`/`exec` are intercepted by
`DesktopExecutor::execute_streaming` (`src-tauri/src/main.rs:4300-4380`) and run through
`host.spawn(...)`, i.e. **a fresh child process per call** — `LocalHost::exec`'s persistent
shell/marker/temp-file protocol (`crates/opcos-hosts/src/lib.rs`, reached only from
`main.rs:4184`) is **not** used. Two cheap runtime probes to confirm this before crediting any
persistent-shell fix as "verified end to end":

- poll `ls /tmp/opcos-shell-output-*` for the whole run — the persistent protocol creates such
  files, the spawn path does not;
- ask for two calls: `OPCOS_PROBE=x; echo set` then `echo "probe=[$OPCOS_PROBE]"`. `probe=[]` means
  no shell state persists ⇒ spawn path. Cross-shell desync is impossible there, so report such a
  fix as "not exercised through the UI" rather than passed.

### Terminal replay: reconstruct chunk offsets, don't eyeball

`execute_tool_streaming` (`crates/opcos-engine/src/lib.rs:2312-2355`) splits each PTY read into
2000-char `terminal_update` events and caps at **64 events** (not 64x2000 chars): a 4096-byte read
becomes 2000+2000+96, so `seq 1 200000` yields chunk lengths `[2000,2000,96]x21…` and 88016 chars
total, ending mid-number. Anything else (all-2000 chunks, 128000 chars) means the read size or cap
changed. Recipe: `pkill -f target/debug/opcos; sleep 4`, then in python read
`session_events` ordered by `(created_at_ms, sequence)`, group `terminal_update` by `call_id`,
concatenate the non-truncated `contents`, and compare byte-for-byte with the real command output
prefix (`subprocess.run(["seq","1","200000"])`). Assert exactly one `{"contents":"","truncated":true}`
and that it is the **last** event for the call. Note the model sees a *tail*-bounded tool result
while the UI replay shows the *head* — different windows of the same command, easy to misread.

UI check for the truncation marker: the `pre` is ~21k lines, so expand the shell row's
**Show output** and scroll with a few `scroll_amount: 1500` wheel actions (~6 lines per click);
`[Output truncated]` sits right after the mid-line cut.

### Local `run_shell` now goes through one persistent `sh` per session (since `8b9aaf6`)

`DesktopExecutor::execute_streaming` calls `LocalHost::exec_persistent_streaming` with session
`opcos-local-<session_id>`, so a local GUI session keeps one long-lived `sh`. Verification probes that
distinguish working from broken:

- **Protocol on the path:** poll `ls -l /tmp/opcos-shell-output-*` every 100 ms during a slow command
  (e.g. `for i in $(seq 1 12); do echo tick=$i; sleep 0.5; done`). You must see
  `opcos-shell-output-<uuid>.working` **growing** and then *both* it and the non-`.working` snapshot
  removed after the run. If no file ever appears, the old `host.spawn` path is being used.
  Also poll `pgrep -P <opcos pid>`: the same `sh` PID must survive across shell calls.
- **State persistence:** call 1 `export OPCOS_PROBE=set; cd /tmp; echo first=done`, call 2
  `echo probe=[$OPCOS_PROBE] pwd=[$PWD]` must return `probe=[set] pwd=[/tmp]`. Remember this changes
  cwd for every later call in the session — put an explicit `cd <workspace>` in later commands.
- **Live streaming:** the store's `terminal_update` events for the slow command must have many
  distinct `created_at_ms` spanning the command duration (12 chunks over 5502 ms for the loop above),
  not one burst.
- **Background isolation:** `(sleep 0.05; printf late) & true` then `printf next` — no `late` may
  appear in any later `tool_result`.
- Chunking on this path is driven by 8192-byte file reads, so lengths are `[2000,2000,2000,2000,192]`
  repeating (~106304 chars for 64 events), not the old `[2000,2000,96]` PTY pattern. No TTY/ANSI on
  this path by design.
- Truncation event carries `total_bytes` (full command output size); the UI renders
  `[Output truncated: N bytes omitted; the model saw the tail]` with `N = total_bytes − displayed`,
  while the model-facing `stdout` starts with
  `[Output truncated: omitted M bytes; showing the last 64 KiB]` and `stdout_metadata.omitted_bytes = M`.
  Check `N + displayed == M + retained_tail == real output bytes`.

### Typing non-ASCII into the composer

Automation keyboard input silently drops CJK/emoji before they reach the Tauri window (the prompt echo
shows them missing). Test UTF-8 with ASCII-only source, e.g.
`python3 -c "print('\u4e2d\u6587 \u00fcn\u00efc\u00f8d\u00e9 \u2705 done')"`, and read the persisted
`tool_result` bytes.

### Reading the store: `session_events` shape

Columns are `session_id, event_id, event_json, created_at_ms, sequence` — there is no `event_type`
column. Type and payload live at `json['working_event']['event_type'|'payload']` (fall back to
`json['type']`). `tool_calls.result` is often empty; the authoritative model-facing result is the
`tool_result` field of the `tool_result` event in `session_events`.

### Iteration stats / artifacts anchors

- Info pane's `Iteration stats` card (`web/src/App.tsx:8880`) is fed by `read_session_events`;
  totals must equal the sum of the per-iteration `details`, and each iteration's input/output
  tokens must equal a row of the store's `usage_events` table — that is the fabrication check.
  On the nextapi gateway `input_tokens` legitimately swings (46697 → 203 → 42401) because it reports
  non-cached prompt tokens; don't read the total as context size.
- A file edit emits `multi_edit_result.file_updates[0].artifact_id`; the timeline row renders a
  **View diff** link, and the artifact lands at
  `~/.config/com.opcos.desktop/artifacts/<session>/<artifact-id>` (compare with `git diff --numstat`).
  For "no inline base64", scan `session_events` for strings ≥500 chars of the base64 charset and for
  the `"image":"<base64>"` form — both must be 0.
- `/compact` on Builtin persists exactly one `compacted` event with `source:"manual"`, the Info pane
  reads `1 (0 automatic, 1 manual)`, and the row renders inside a `Worked for …` group.

## Timeline-rendering verification (one-liners, thoughts, minor collapse)

Screenshots alone cannot prove "no row was dropped". Re-implement `web/src/timeline.ts` +
`Transcript.tsx renderRows` as a small Python simulator over the persisted `session_events`
(see `/home/ubuntu/opcos-test/sim_timeline.py` for a working version) and compare its counts with
what you see in the GUI. Key semantics to mirror, because they are where bugs hide:

- `devin_message` / `user_message` **flush the current work group**; the engine emits one group per
  iteration, so a long task produces dozens of groups (Devin shows one per user turn).
- a `devin_thoughts` row gets `thoughtForCallId` from the *next* action, and `Transcript.tsx` skips
  such a row at top level and re-renders it from a **per-group** `thoughtByCallId` map. If the target
  action lands in the *next* group (the common case, since thoughts are emitted at the end of an
  iteration) the thought renders **nowhere**. Always count `nested / standalone / lost` explicitly.
- one-liner rows come from `one_line_thoughts.short`; assert `events == rendered rows`.
- consecutive rows with `is_major_action === false` collapse into an `N minor actions` expander; check
  the contained rows' timestamps lie between the surrounding rendered rows.

Shell-row metadata gotchas observed on `dbcf337`:

- `shell_process_completed.exit_code` is derived from `result["exit_code"]`, but the desktop executor
  wraps results as `{"status":…,"result":{"exit_code":…}}`, so **every row renders `exit 0`**. Force a
  failure (`sh -c 'exit 3'`) and compare the row against the inner value in the store before believing
  any "non-zero exits look different" claim.
- `duration_ms` is absent from the payload; the UI falls back to the completed−started timestamp delta.
- `shell_id_for_session` = `shell-` + first 8 alphanumerics of the session id, and session ids look
  like `session-<digits>` → the id is the constant `shell-session1` for *every* session. Verify
  uniqueness across two sessions, not just presence.

## Remote RVM surfaces in the GUI

- Probe the host directly first (`/api/health`, then `/api/exec-sync` with
  `Authorization: Bearer <token>`) so an unreachable host or a rejected token is never mistaken for an
  OPCOS bug. `/api/screenshot` and `/api/computer-use` can fail host-side with
  `resize failed: … convert: not found` (ImageMagick missing) — that is a host gap; the OPCOS-side
  check is that the error text is surfaced and no local fallback happens.
- Add the host in Settings → Hosts → *Add host* (name / Remote URL / Bearer token). To keep the token
  out of your own transcript, click the masked field and type it from the shell with
  `xdotool type -- "$RVM_<HOST>_TOKEN"` using an env-bound secret. Press **Test** and require `Online`.
- Bind a **new session** to that host (host dropdown in the new-session form) and set the workspace to
  a remote path (e.g. `/home/ctyun`). Prove remoteness with `uname -a; whoami; pwd` — the remote user
  and hostname must appear, never this box's.
- Things that were broken on `dbcf337` and may still be: remote `cd` does **not** persist between
  `run_shell` calls (exported env does); the remote path emits **no `terminal_update` events** (no live
  streaming); the Desktop (VNC) and Editor (Web IDE) rail tabs are hardcoded `PlannedPane`
  placeholders in `App.tsx` (~9374-9389) even though the `SurfacePanel` VNC/`start_ide_proxy` code
  exists, so those tabs cannot work regardless of the host.
- Remote file tools do work; verify out of band with authenticated `/api/read` + `/api/ls` and confirm
  the file does not exist locally.

## Session model default

A newly created session's model selector defaults to `auto`, which gateways reject with
`Provider request failed`. Set the model explicitly (e.g. `glm-5.2`) before the first prompt, and note
that the `<select>` may advance one option per click under automation instead of opening a list.

## Permission modes and approval gating (`Interactive`)

The composer/new-session mode dropdown offers Discuss / Plan / Interactive / Auto / Custom
(`crates/opcos-policy/src/lib.rs` `classify`). Previous rounds only ever exercised **Auto**, where
nothing is ever asked; use **Interactive** when the question is "does the harness ask before a side
effect". In Interactive, every write and every shell call raises a modal card
(*Run a command* / *Use edit_file* → *Tool action requires approval*, Allow once / Deny) and the run
blocks; `approval_pending` is persisted per call.

Traps when driving an approval-gated run:

- **Resolved cards stay on screen** with live-looking buttons. Only the bottom-most card is real —
  always `scroll` to the bottom before clicking, or you will click a dead card and conclude the run hung.
- Cards say *"runs on the bound remote host"* even for the local host 本机.
- **Parallel tool calls in one turn are denied, not queued.** If the model emits N calls and one needs
  approval, the others come back `{"error":"tool call denied pending another approval"}`. This is how a
  real `propose_plan` call gets thrown away, leaving `plans`/`plan_steps`/`planning_rounds` empty and
  `plan_get` returning `{"plan":null}` — check the `tool_calls` table and the `tool` role messages before
  concluding "the model never plans".
- Budget wall-clock: each approval is a click plus a model round-trip, so a 5-part task can need ~15
  approvals.

## Keeping the agent out of your own test material

The agent will `grep -rn` over the workspace's *parent* directories when a requirement is not answerable
from the repo, and it will happily read your test plan and expected results. Keep task fixtures in a
directory that shares no ancestor with `/home/ubuntu/opcos-test` (e.g. `/home/ubuntu/sandbox/<fixture>`).
`read_file` is workspace-restricted, but `run_shell` is not. A good ambiguity probe is a requirement that
references something "we use elsewhere in our codebase" that does not exist.

## Environment flakiness that repeatedly derails runs (not product bugs, but budget for them)

- `run_shell` commands that use pipes plus `${PIPESTATUS[0]}` or that exit non-zero sometimes come back as
  `{"error":"local host I/O failed: local shell exited"}` — the local shell session dies. Retrying the
  same command without the pipe works. Prefer simple commands in probes so a shell teardown is not
  mistaken for a harness defect.
- `Provider request failed` can hit mid-session even with a small context and a reachable gateway
  (check with `curl -s -o /dev/null -w '%{http_code}' <base>/models` → 401 means reachable). Switching the
  composer model chip (e.g. `deepseek-v4-flash` → `glm-5.2`) and pressing **Retry** in the transcript is
  the fastest recovery; note the model used per session in the report.
- The app's shell python (pyenv) may lack `pytest` even though your own exec shell has it; a fixture whose
  task requires pytest can burn many turns. Verify `python3 -m pytest` from inside a session's shell before
  relying on it.
- Sending a message right after interacting with the composer Mode menu often does nothing (the menu
  overlay swallows the click). Click somewhere neutral first, re-type, then click the send arrow — and
  check the store/transcript that the message actually landed before waiting on it.


## Session-specific reliability notes (2026-08)

### Long project paths and GUI input

The project dialog can silently truncate long repository paths when text is entered character by
character. Prefer clipboard paste (`Ctrl+V`) or create a fixture repository under a short path
inside the containment-approved workspace. After creation, verify the exact `repo_root` and
member worktree paths in sqlite and with `git -C <repo> worktree list`; never treat the visible
field or project board alone as proof of the path that was submitted. Use ASCII prompts when
possible: computer-use typing can silently drop CJK text in the WebView.

### Workspace containment

Project and session paths must remain under the explicitly approved workspace or the user's home
for the built-in local host. Do not use `/tmp`, arbitrary host paths, or a path copied from an
untrusted transcript as a fixture. Before a run, confirm the project repository and session
workspace are the intended paths; after a run, verify that files and worktrees were created only
under that boundary.

### Diagnosing a hard freeze

A stuck spinner is not sufficient evidence of a UI problem. Capture process state and every thread
while the app is hung:

```bash
ps -o pid,stat,pcpu,wchan:32,cmd -p <pid>
gdb -q -p <pid> -batch -ex 'thread apply all bt' > /tmp/opcos-freeze.bt
```

Check whether the process is sleeping in `futex_wait_queue_me` (lock/deadlock) or consuming CPU.
For SQLite-related hangs, inspect stacks for a recursive `Mutex<rusqlite::Connection>::lock` and
trace the synchronous call chain. In particular, never hold a database mutex guard while calling
a state-taking helper that acquires the same mutex; finish the SQL work, drop the guard, then call
the helper. Reproduce against a clean revision before attributing a freeze to local changes.

### Approval verification discipline

For an approval continuation, record the exact sequence of persisted `approval_pending` and
`approval_resolved` events by `call_id`. The next pending call must be selected by the engine's
returned `next_call_id`, never by taking the first database row. A card that says Approved is not
engine evidence: verify the corresponding persisted resolution event and that the next tool or
approval actually starts. When several gated writes are needed, use fresh marker filenames and
verify the remote file state after each Allow/Deny decision.

## Testing the OPCOS MCP server and `mcp-serve` bridge

The MCP service runs inside the already-running desktop app on loopback at an ephemeral
`POST /mcp` endpoint. The app writes its discovery state to
`<config_dir>/com.opcos.desktop/mcp-server.json` with mode `0600`; it contains the loopback host,
port, and a generated bearer token. `opcos mcp-serve` is a thin stdio-to-HTTP bridge and must not
boot Tauri, open another window, or create another SQLite runtime. Verify this with `wmctrl -l`
and by checking `/proc/<pid>/fd` for database links: only the GUI process should hold the database.

Drive the bridge from an external MCP client. Hermes can provide a fast real-client discovery
check:

```bash
hermes mcp add opcos --command /path/to/target/debug/opcos --args mcp-serve
hermes mcp test opcos
```

Hermes currently needs the MCP SDK 1.x API (`mcp==1.28.1`); MCP 2.x changes
`CallToolResult.isError` and can crash the CLI. If an agent model turn hangs against the local
gateway, do not invent an agent-driven result. Use the official MCP SDK's stdio client from the
same environment, disclose that fallback, and test the bridge protocol directly. Codex and pi
may require `/v1/responses`, which a chat-completions-only local gateway does not provide.

The MCP bridge safety matrix should be repeatable and should always produce exit status 1, empty
stdout, and no extra window/process:

- app stopped with a stale state file → endpoint unreachable;
- discovery state removed → endpoint state unavailable;
- host changed to `0.0.0.0` → rejected as not loopback-only;
- malformed JSON → invalid endpoint-state error;
- port `0` → invalid endpoint-state error.

Never print the MCP bearer token. Inspect only mode, host, port, and token length. Perform
counting-only leakage checks over application logs, the database and its `-wal`/`-shm` files,
bridge traces, and screenshots. The MCP server itself needs no user-provided secret; remote-host
tests use the normal bearer-header-only RVM procedure described above.

When checking Devin-shaped MCP contracts, verify session objects use the expected identifier
shape, gather accepts the supported multi-session form, and tool failures are returned as MCP
errors with readable control-plane messages rather than internal prefixes. Compare settings
discovery with the actual Settings sidebar order, and use explicit probe kinds when testing
integration probing. Builtin assets must remain read-only: select an item reported as builtin and
verify update/delete returns a readable error.

## Testing the OPCOS ACP server and `acp-serve` bridge

The ACP service also runs inside the existing desktop app. `opcos acp-serve` is a thin
stdio-to-authenticated-loopback-WebSocket bridge dispatched before Tauri startup, so it must not
start a second app. It discovers `<config_dir>/com.opcos.desktop/acp-server.json` (mode `0600`,
keys `host`, `port`, and `token`) and connects to the loopback `/acp` endpoint with a bearer
header. The token is reused while the state file exists; the port may rotate on app restart.
Read only mode, host, port, and token length; never print the token.

There is usually no real ACP client installed. ACP *agent* CLIs such as `codex-acp`,
`claude-agent-acp`, `pi-acp`, `hermes acp`, and `opencode acp` are the wrong side of the
protocol: they are agents that connect to an ACP server, not clients that drive this server.
Use a scripted client that spawns `opcos acp-serve` and speaks newline-delimited JSON-RPC when no
real client is available. Keep monotonic timestamps in every trace; they are the evidence that
`session/update` notifications arrived before the `session/prompt` response. The reusable driver
is `/home/ubuntu/mcp-mock/acp_client_drive.py` and supports:

```text
init
roundtrip <cwd>
cancel notify|request <cwd> [seconds]
perm approve|deny|ignore-ui|drop <session_id> <marker>
turn <cwd> <text>
cwd <cwd>
```

Avoid `input()` in scripted scenarios because they normally run non-interactively and receive
EOF. Test initialization/version negotiation, session creation, streaming, both cancellation
forms, permission round trips, client cancellation, connection drops, and stdout purity.

### Deterministic ACP turns

Model turns through the local LiteLLM gateway may not complete deterministically. Point the test
session at the local fixture provider (`http://127.0.0.1:8899/v1`) and use its `RUNSHELL:<command>`
trigger to force a real `run_shell` call. Give each scenario a fresh placeholder marker path and
verify file existence out of band; transcript text alone does not prove execution.

The fixture's ordinary response may contain only one content delta. To prove live streaming,
configure the fixture to emit several delayed SSE content chunks (the fixture's stream helper
supports extra deltas), then require multiple ACP `agent_message_chunk` updates with timestamps
strictly before the prompt result. After changing the fixture, ensure the old process is gone,
verify `/v1/models` responds, and relaunch it from a persistent tty; stale fixture processes can
continue serving the old code after an apparent restart.

ACP-created sessions default to Auto, so approval scenarios must switch the mounted session to
Interactive / “Ask for approval” first. In Interactive mode, `run_shell` and writes raise an
approval card and persist a pending record. Auto will silently allow the same calls and is not a
valid approval test.

### Remote `cwd` matrix

`session/new` selects a host by matching the agent setting `default_platform` against registered
host names. Set it to the intended remote host, save, and verify the created session's persisted
`host_id`; do not infer host selection from the setting alone because it may be cached.

For a DevBox-style Windows host, test both missing and known-good remote paths. `/api/ls` returns
an HTTP 404 `ENOENT ... scandir` for a missing directory, while Linux-looking paths may be mapped
under the Windows root and therefore make useful negative cases. Keep a known-good path such as
`C:\Users\Admin` in the matrix so a validation fix cannot reject every path. Cross-check existence
out of band with authenticated remote `/api/ls` or `/api/exec-sync`; never use local filesystem
semantics to decide whether a remote path exists.

To simulate an unreachable host without changing its data, edit the host URL in Settings → Hosts
to a dead loopback port such as `http://127.0.0.1:9`, leaving the stored token untouched. Expect
an explicit `/api/health` connection error and no local fallback. Restore the original URL,
re-run a known-good remote path, and leave the `default_platform` setting at the requested value.

### ACP bridge safety and approval cleanup

Repeat these checks for ACP:

- state file mode is `0600`;
- the WebSocket binds loopback only;
- missing or wrong bearer credentials receive 401, a valid credential receives 101, and a LAN
  address cannot connect;
- absent, stale, malformed, non-loopback, and invalid-port state files make the bridge exit 1,
  print only an `ACP bridge unavailable: ...` diagnostic on stderr, leave stdout empty, and
  spawn no window/process;
- two bridges still show one OPCOS window and zero database links from bridge processes;
- token counts remain zero in logs, database files, client traces, and screenshots.

Before every approval scenario, verify there are no leftover rows in `pending` with
`state='pending'`. Clean leftovers through the UI/Inbox before starting the next scenario.
Overlapping permission runs can make an “exactly one request” assertion meaningless.

For UI-side resolution, bound the wait and record request count, prompt latency, marker existence,
and persisted pending state. For ACP-side approval or denial, verify the mounted view itself:
the card must disappear and status must leave Running without navigating away. If the database says
the session is idle/finished and the pending row is resolved but the card clears only after
navigating away and back, the backend turn is not stuck; this is a live frontend refresh gap.
Conversely, if remounting does not clear it, inspect engine state, persisted events, and fixture
request counts before attributing it to rendering.

## Agent execution surfaces

Use short, isolated turns and record both the user-visible result and persisted evidence. A turn
containing many tool calls can remain `Working` for minutes; one or two calls usually completes
quickly. If a turn stalls, interrupt it, then inspect the store before repeating mutations because
earlier calls may already have committed.

### Bring-up and model controls

1. Kill stale Vite processes before starting the app. A process left on port 1420 can make the new
   Vite instance move to 1421 while Tauri continues loading the old bundle.
2. Use a model known to work with the configured gateway (refresh Settings → Provider if needed).
   Treat a model claiming that a tool is unavailable as a separate model-behavior observation; do
   not conclude that the tool was unregistered until the actual request tool list is checked.
3. Record the provider, model, host, workspace, and mode with every scenario.

### SQLite evidence and WAL handling

Rows may be in `opcos.db-wal` rather than the main database file. Never diagnose persistence from
`cp opcos.db` alone:

```bash
cp opcos.db /tmp/opcos-check.db
cp opcos.db-wal /tmp/opcos-check.db-wal
cp opcos.db-shm /tmp/opcos-check.db-shm 2>/dev/null || true
```

Alternatively stop the app before copying. Snapshot immediately before and after a mutation, and
identify which tables the code path actually writes.

### Session rename and desktop surface

- For user rename, hover a sidebar row, open its visible `⋯` menu, choose **Rename**, submit the
  inline editor, then reload the app and verify the title remains changed. Confirm that opening the
  menu does not select a different session.
- For `desktop_show`, verify the persisted `desktop_view_requested` event and the visible right
  drawer. The event type is on the `WorkingEvent` envelope (`event_type`), not under
  `payload.event_type`.
- Test three cases: first request from Info, two requests in one turn, and a request after the
  drawer was collapsed with `✕`. The frontend request must carry a changing nonce/request ID, so
  the last case re-opens the same Desktop tab instead of being swallowed as unchanged state.

### Recording and session search

- Recording is a sampled screenshot timeline, not a video. Verify the `recording_manifest` artifact,
  frame count/slider, and annotation list. Use an unknown `test_start_id` as a fast negative case
  for assertion annotations.
- To prove `session_search` redaction, create a fresh short session and make the marker the first
  tool call, for example `R3PROBE --api-key=LEAKCANARY777`. Search it from another session with
  `content_scope: "tool_calls"`. The returned snippet must contain the marker while replacing or
  omitting the secret value. If the marker is absent too, the probe was outside the snippet window
  and the result is inconclusive. Raw tool-call storage may retain the value; redaction is expected
  on the search output path.

### Knowledge, playbook, and automation assets

- `config_asset_manage` requires `kind` on every action, including `get` and `rollback`. Only
  `knowledge` and `playbook` are valid; rejection of `permissions` or `provider` is structural.
  Verify rollback through both `config_object.current_version_id` /
  `config_object_version` and Settings → Knowledge → the asset editor body.
- `automation_manage` schedules use the agent automation path, not the legacy trigger-session path.
  After **Run now**, compare a work-queue count snapshot and inspect the new `ready` row,
  `payload.automation_depth`, and `schedules.last_run` / `last_result`.
  `schedule_runs` belongs to the legacy trigger path and is not evidence for agent automation.
  Confirm no new session was created and no `session_preferences.unattended != 0` row appeared.
  Disable live cron schedules or snapshot counts immediately before the run; otherwise background
  enqueues can make a non-empty queue result meaningless.

### Evidence checklist

For each scenario record: session ID, provider/model/host/workspace/mode, prompt, visible result,
event or artifact ID, relevant database query, and whether the app was restarted. Distinguish
“not observed” from “negative”; a missing marker, missing row, or missing tool claim is only useful
when the observation window and expected write path are known.

## External-agent bridge checklist

For any bridge test, prove the external process connects to the already-running app rather than
starting another Tauri/SQLite/background runtime. Capture protocol traces, process/window counts,
state-file permissions, loopback binding, and token-count-only leak results. Keep fixture files and
marker paths outside the repository's test material when possible so the agent cannot discover
expected answers by searching parent directories. Restore temporary host URLs and provider
fixtures before finishing, and record any intentionally changed agent setting without silently
“fixing” it.

## Local RVM fixture host: test capability gating / MCP / surfaces without a remote box

A remote RVM host is not always usable (expired token, no VNC password, gateway down). A small
aiohttp "dev-agent" fixture on `127.0.0.1:8899` covers almost every OPCOS host code path and is
fully controllable, so gating/approval/surface tests stop depending on an external service.
Endpoints OPCOS actually needs: `/api/health`, `/api/info`, `/api/capabilities`, `/api/exec-sync`,
`/api/read`, `/api/write`, `/api/ls`, `/api/screenshot`, `/api/computer-use`, `/api/storage/stat`,
`/api/storage/exists`, `/mcp`, `/pty-ws`, `/vnc-ws`. Wire-shape traps (read the Rust structs in
`crates/opcos-rvm/src/lib.rs` before guessing): exec-sync takes `cmd` + `timeout` (not
`command`/`timeout_seconds`), `/api/read` must return `size`, `/api/ls` must return `items`,
`/api/screenshot` must return a nonempty `image` **plus** `format`, `/api/computer-use` must return
`ok: true` (not `status`).

Useful fixture knobs (add them; they turn hard-to-stage cases into one curl):
`POST /fixture/capabilities {"capabilities":[...]}` to add/remove `vnc`/`pty`/`browser`/`cdp`/
`computer_use`, `POST /fixture/delays {"health_delay":4,"computer_use_delay":60}` to make the
optimistic-render window and the in-flight computer-use lease observable, `POST /fixture/tools` to
change host MCP tools. Bridge `/vnc-ws` to a local `x11vnc -display :0 -nopw` and `/pty-ws` to a
real `bash -i` pty so Desktop/Terminal surfaces are genuinely live.

**Fixture pty pitfall that looks exactly like an OPCOS bug.** If the pty reader uses
`loop.run_in_executor(None, os.read, master, 4096)`, every connection permanently consumes one
default-ThreadPoolExecutor thread; after a handful of Terminal opens/pop-outs new pty websockets
connect but never emit a byte, i.e. **black terminal panels and black pop-out windows**. Before
blaming OPCOS, connect to `/pty-ws` directly with an aiohttp client and see whether the *host* echoes
anything; fix the fixture with `loop.add_reader(master, ...)` + an asyncio queue. Also `pkill -f
"bash -i"` between rounds. Generally: for any black surface, first prove the host side alone.

## Provider / host credential fallbacks

- Gateways expire mid-session: a provider card that validated earlier can start returning
  `401 Invalid token`, which then surfaces as a mid-turn error and makes submit-failure tests
  ambiguous. Keep an alternate OpenAI-compatible base URL + key (`LLM_Baseurl` / `LLM_KEY`) and
  re-run Settings → Provider → Test/Save until it prints `Provider key validated successfully.`
  Also re-pick the **session model chip** — a session created earlier keeps the old model id
  (e.g. `glm-5.1`) and will fail with "model not found" even after the provider is fixed.
- To stage a *submit* failure deterministically, add a host pointing at a dead port
  (`http://127.0.0.1:9`) and send from a session bound to it; the error text is
  `rvm request failed …`, no provider involvement.
- Remote Desktop may be unreachable for a host-side reason. Probe the RFB handshake yourself before
  calling it an OPCOS bug: connect to `wss://<RVM_URL>/vnc-ws` with the Bearer header, reply
  `RFB 003.008\n`, and read the security-type list. `[1, 2]` on the wire means count=1, type=2
  (VncAuth) ⇒ the host really requires a VNC password and OPCOS correctly shows
  `Remote VNC requires a password; configure the host VNC password.` Ask for the host VNC password
  as a secret (there is a per-host field); `/api/info` does not expose it.

## Pop-out panes (`#/pane?session=…&tab=…`)

`StandalonePane` re-fetches `list_sessions` and renders the same `SurfaceView`, so a pop-out calls
`start_surface` again and gets its **own** bridge port. Desktop and Terminal pop-outs do paint live
VNC/pty content on a healthy host — if a pop-out is black, suspect the host/bridge (see the fixture
pty pitfall) before the pane route. `ide` pop-out shows only the backend reason when the session has
no workspace, which is expected, so set a workspace if you need to test the IDE pop-out.

## Capability gating specifics (App.tsx `remoteTabs`)

Desktop is hidden only when **both** `vnc` and `computer_use` are Unavailable; Terminal is hidden
when `pty` is Unavailable **or** the host is `local`; Browser is hidden when `browser` is
Unavailable; Editor is hidden when `ide` is Unavailable. All four keep their tab when that pane is
currently open, and show the backend `reason` text when the open pane's capability is unavailable.
So: to test the "reason instead of blank" path, open the pane first, then drop the capability and
force a re-probe by switching session away and back.

## Testing real idle sleep (`OPCOS_IDLE_SLEEP_SECONDS`)

- Launch with `DISPLAY=:0 OPCOS_IDLE_SLEEP_SECONDS=25 OPCOS_DEV_URL=http://127.0.0.1:1420 setsid
  nohup ./target/debug/opcos &` — the threshold is read **once at process start** and the scan tick
  is `clamp(threshold/4, 1s, 60s)`, so 20–30 s gives a ~6 s tick. Always use `setsid`: a plain
  backgrounded `nohup` dies with the exec-tool shell and the window never appears.
- `sqlite3` CLI is not installed on the box; read state with
  `python3 -c "import sqlite3; ..."` on `/home/ubuntu/.config/com.opcos.desktop/opcos.db`.
  Useful columns on `sessions`: `run_state`, `sleep_state`, `slept_at`, `last_active_at`.
- Sleep candidates require `archived=0 AND run_state='idle' AND sleep_state='awake' AND
  last_active_at < now-threshold`; `session_can_idle_sleep` additionally refuses when a turn is
  active or any `pending` row is still `pending`. A session waiting on an approval card has
  `run_state='idle'`, so that pending guard is the only thing keeping it awake — test it explicitly.
- These refresh `last_active_at`: `submit_turn`, `steering`, `resolve_approval`, `start_surface`,
  `ide_url`, `touch_session`/`wake_session`. Since the surface-activity fix, real user input in a
  panel (xterm `onData`, VNC `pointerdown`/`keydown`/`wheel`) also calls `touch_session`, but it is
  throttled to **once per 60 s per session**. Consequence for test design: with a 25 s threshold
  typing can never keep a session awake (the throttle swallows the touches) — use
  `OPCOS_IDLE_SLEEP_SECONDS=90` and type every ~35–40 s to prove the keep-alive, then stop typing
  and expect sleep ~threshold later. Merely having a panel open with no input does **not** renew.
- Verify sleep really released resources from the shell, not the UI: the relay listener disappears
  from `ss -ltnp | grep opcos` and the fixture log prints `pty-ws close pid=…`. Baseline (no surface
  open) on this box is 3 opcos listeners; one extra port per open surface.
- Re-selecting a session calls `touch_session`, but the frontend throttles it to **once per 60 s per
  session** (`lastTouchedSessionRef`). To test wake-by-reselect, wait >60 s after the last selection
  of that session, otherwise the click is silently swallowed and the session stays asleep.
- Sleep/wake lifecycle events arrive only on the single Tauri event `opcos://event` with a `kind`
  field (`fn emit` in `src-tauri/src/main.rs`); `kind` is `session-sleep` / `session-wake`. Anything
  that subscribes to `listen("session-sleep")` literally never fires — a past regression. The
  dispatch must also run *before* the handler's `payload.session_id !== selectedId` early return,
  otherwise a non-selected session's sidebar dot won't light up. Test both: watch the selected
  session sleep with zero interaction (subtitle + dot must change within a tick), and keep another
  session selected while a background session sleeps.
- After the surface-lifecycle fix a slept panel shows an explicit notice + Reconnect button instead
  of a stale frame: EN "This panel was disconnected because the session is asleep. Reconnect to wake
  it." / "Reconnect", ZH "会话休眠后此面板已断开。点击重连以唤醒会话。" / "重连"
  (`surfaceSleepingDescription` / `reconnectSurface`). Reconnect goes through normal `start_surface`,
  which wakes the session; verify with a *new* port in `ss -ltnp`, a fresh `pty-ws open pid=…` in the
  fixture log, `sleep_state='awake'` in sqlite, and a real command echoing in the terminal.
- `touch_session` throttling is bypassed when the session is already `asleep`, so "switch away and
  back" wakes reliably even inside the 60 s window.
- Intermittent first-connect trap seen twice (dev *and* production `vite preview` builds): React
  Strict Mode or a session/tab transition can invalidate or overlap the first `start_surface`
  attempt. SurfaceView now self-heals one invalidated/overlapped attempt when the panel is still
  mounted, awake, and has no port; a genuine `start_surface` error is not retried in a loop. If
  the panel remains unavailable, it must show the translated "Retry"/"重试" action. Check
  `ss -ltnp` + fixture log before blaming sleep code, and use that action to trigger a fresh
  `start_surface`.
- Denying an approval (or interrupting a turn) does not leave an orphan `pending` row: the session
  still sleeps ~threshold later. If a session refuses to sleep, query
  `select state from pending where session_id=… and state='pending'` first — an outstanding approval
  is the usual, correct reason.
- Fixture caveat: `/api/exec-sync` in the local fixture times out around 60 s, so a `sleep 60` probe
  fails host-side. Use `sleep 45` or shorter when you need a long-running turn to hold a session
  awake.

## Surface start failures: what the UI can and cannot show

`start_surface` (`src-tauri/src/main.rs`, `async fn start_surface`) binds a local `TcpListener` and
spawns `relay_surface` **before ever contacting the host**, then returns `Ok(port)`. A later relay
failure emits `surface-ended`; the frontend enters an explicit failed state, keeps the safe reason,
and stops automatic reconnect attempts until the user presses Retry or otherwise changes context.
The `shouldShowSurfaceRetry({port, sleeping, failed})` banner (i18n `surfaceUnavailable` /
`retrySurface`) must remain stable and mutually exclusive with the sleep banner.

- A genuinely unreachable/erroring host (connection refused, HTTP 503, session bound to a dead host)
  does not make `start_surface` fail synchronously. The relay later emits `surface-ended`; verify
  that the panel keeps the safe reason and stable Retry action instead of becoming a blank panel or
  opening another connection automatically.
- Ways to fake an unavailable host, cheapest first: point the session at a host whose URL is a dead
  port (a `DeadHost` entry with `http://127.0.0.1:9` is already in the local DB), stop the fixture
  (`kill` the `python3 agent.py` PID — `pkill -f fixture-agent/agent.py` does **not** match, the
  cmdline is just `python3 agent.py`), or run a logging stub on :8899 that 503s everything and appends
  each request to a log file so you can count attempts.
- Retry-storm check: count host-side requests. Use an untouched window of at least 25 s:
  `a=$(wc -l < /tmp/stub.log); sleep 25; …`, then compare the line count and inspect `ss -ltnp`
  for relay churn. A fixed failed state must produce one startup attempt and zero additional
  requests until the user presses Retry; if the count grows repeatedly or relay ports
  appear/disappear every second, the retry storm has regressed.
- If a failure loop is running fast, any "reason" banner it renders can flicker faster than a
  screenshot can catch — treat "no visible reason/Retry across several seconds of screenshots" as the
  user-visible truth, and back it up with request counts / port churn.
- From `70339e8` on, the surface has an explicit **failed state**: a `surface-ended` event or a
  `start_surface` error freezes the panel on the backend reason (e.g. `RVM websocket failed: HTTP
  error: 503 Service Unavailable`, `... IO error: Connection refused (os error 111)`, `Remote surface
  disconnected.`) plus the localized unavailable text + Retry, and auto-connect stops firing. Testing
  notes: one Retry click = exactly one host attempt; the failed state is cleared by clicking
  Retry/Reconnect, switching panel tab, switching session, or waking from sleep — so if you need the
  failed state to persist for measurement, do not touch tabs or the session list while timing.
- Watch out for the panel spontaneously reverting to a different surface tab (observed once flipping
  Terminal → Browser during an untouched idle wait). The Browser/CDP surface polls frames
  continuously, which keeps the session `awake` past the idle threshold and leaves an extra relay
  listener behind until the panel is closed — so before timing an idle-sleep test, screenshot the
  panel to confirm it is still on the tab you expect, and treat any unexplained "did not sleep" result
  as possibly a Browser-tab artifact.
- Sessions with `run_state='error'` are never slept, no matter how long they idle. Pick a session with
  `run_state='idle'` for sleep tests, or you will wait forever and mis-report a sleep regression.
- Right-rail surface icon positions shift depending on which panel is open and which capabilities the
  host reports (local-host sessions have no Terminal/Desktop icon at all). Always `zoom` the rail
  (region ~`[995, 25, 1024, 400]`) to locate the monitor (Desktop) and `>_` (Terminal) icons before
  clicking; clicking the already-active icon toggles the panel closed.

## Direct surface WebSockets (from PR #208, `surface_url`) — how to test without a relay

Terminal/Desktop no longer go through a local relay port: `surface_url(...)` returns
`{ url, vnc_password }` with `ws(s)://host/{pty,vnc,cdp}-ws?...&token=...` and the **webview** connects
directly (`new WebSocket(url)` / `new RFB(host, url)`). `start_surface`/`stop_surface`/`relay_surface`/
`surface-ended` are gone. This changes every piece of evidence you used to collect:

- **No more per-surface loopback listener.** `ss -ltnp | grep -c opcos` must stay at the app baseline
  (3 on this box) no matter how many surfaces are open. A count that grows by one per panel means the
  relay is back. Use this as the primary "is it really direct?" assertion.
- **Count webview sockets instead of listeners:**
  `ss -tnp | grep <host:port> | grep WebKit` — one ESTAB line per live direct surface, owned by
  `WebKitNetworkPr`. Note the local ephemeral port: if the same local port survives a UI transition,
  the socket was never torn down and the panel is reusing a stale connection.
- **Fixture must accept URL tokens and expose `/api/health`.** `surface_url` health-checks the host
  *before* returning, so unavailable hosts now fail synchronously (no relay to discover it later) — the
  reason string is the health error (`error sending request …`, `http 503 …`). Make the fixture's
  `/pty-ws` / `/vnc-ws` handlers ignore/accept `?token=`, and log `pty-ws open/close pid=` so you can
  prove the remote shell really died.
- **Host-side request counting still works** for storm checks, but point the *stub* at the same port and
  count `/api/health` + `/pty-ws` lines. Verified-good numbers: 0 requests over a 27 s untouched window
  in the failed state, exactly 1 new request per Retry click. A stronger no-storm proof with direct
  sockets: restart the host while the panel sits failed and confirm **no** `pty-ws open` appears until
  Retry is pressed.
- **Known defect to re-check (PR #208 head `eafaca5`): switching panel tabs does not close the direct
  socket.** Terminal → Desktop/Browser leaves the PTY socket ESTAB and the remote `/bin/bash -i`
  running (no `pty-ws close`), and coming back reuses the stale socket (no new `pty-ws open`, old
  scrollback still there). Panel close (X), session switch and idle sleep *do* close it. Likely cause:
  the terminal socket effect deps are `[selected.id, surfaceUrl, sleeping]` with `tab` missing, so a tab
  change never runs its cleanup. Test recipe: note `grep -c 'pty-ws close'` + WebKit conn count, click
  the other surface icon, wait ~8 s, re-count.
- **Browser tab no longer opens a CDP WebSocket** — it only polls `capture_remote_browser_frame`, and
  the polling *stops* after the first failure (measure: host request count flat for ≥24 s, one extra
  request per Retry). Consequence for sleep tests: the Browser tab no longer keeps a session awake, so
  a session can now sleep with Browser selected (confirmed: DB `asleep` + audit row while Browser open).
  The pane still renders "Waiting for a remote browser page target…" underneath the failure banner —
  cosmetic, but do not read it as "working".
- **Token exposure check:** the `?token=` query is sanctioned by `AGENTS.md` for surface WebSockets only.
  Verify the UI never renders it: the Browser footer must read `Remote browser preview (CDP)` (the old
  `CDP relay for an external client: ws://127.0.0.1:<port>` line is gone), and
  `grep -i token /tmp/opcos-*.log` must be empty. A fixture/stub access log containing
  `GET /pty-ws?...&token=...` is expected (that is the host side, not OPCOS UI/logs).

## Frontend i18n verification (zh/en sweeps) — the `{label}` / camelCase leak pattern

Language switching lives in **设置 → 通用 / Settings → General → 语言 / Language**; `setLocale()` writes
`localStorage["opcos.locale"]` and notifies `subscribeLocale` listeners (`web/src/i18n.ts`). Locale
survives a full app restart, so remember to reset it between runs or you will start a sweep in the
wrong language.

**Pick the right bundle first.** `OPCOS_DEV_URL` has no effect on a reused debug binary — the webview
always connects to `http://localhost:1420`. With several worktrees around it is very easy to test the
wrong branch. Always confirm before launching:

```bash
pid=$(ss -ltnp 2>/dev/null | grep ':1420' | grep -o 'pid=[0-9]*' | head -1 | cut -d= -f2)
ls -l /proc/$pid/cwd            # must point at the worktree under test
curl -s http://127.0.0.1:1420/src/i18n.ts | grep -c 'someKeyAddedByThisPR'
```

Grepping the *served* `src/i18n.ts` for a key the PR just added is the cheapest positive proof that the
running UI is the branch you think it is.

### The defect pattern that keeps recurring: dictionary keys leaking onto the screen

Fixes usually replace display text with a dictionary key inside a static table, e.g.

```tsx
const tabs = [["blueprints", "blueprints"], ...];
...
{label}                 // BUG: renders the key
{translate(label)}      // correct
```

Two consequences you must actively hunt for:

1. **`translate(variable)` escapes the repo's own i18n unit test**, which only scans
   `translate("literal")`. A green CI says nothing about these call sites. Pre-scan the diff with
   `rg '\{(label|tab\.label|option\.label|category\.label)\}'` and treat every hit as a GUI target.
2. **The visible symptom is a camelCase or all-lowercase English word** (`fullAccess`, `skills`,
   `blueprints`, `sharePromptsInPrs`). This is far more dangerous than leftover English prose because
   it reads like a normal English label and the eye skips it. **Sweep specifically for camelCase /
   lowercase single words in the zh UI** — in zh, *any* lowercase English word on a control is almost
   certainly a leaked key. Note that keys leak in **both** locales, so an en-only sweep will not find
   them; and the fallback chain (`zh[key] → en[key] → key`) means a missing key silently degrades to
   English before it degrades to the key itself.

### Verifying "switch is instant": screenshot the frame, not the settled state

Different components subscribe to locale independently (`SettingsView`, `Sidebar`, `AppContent`). A
component that is *not* subscribed still updates a few seconds later via unrelated background polling,
so **waiting before you screenshot will hide the bug**. Click the language option and take the
screenshot in the *same* `computer` action batch with no `wait`, then assert that the global sidebar,
the settings sub-nav and the body pane are all in one language in that single frame. Test both
directions (zh→en and en→zh) and from more than one settings section.

### Sweep checklist and scoping rules

- Cover: 19 settings sections, sidebar (incl. the grouping `≡` menu and collapsed state), Composer
  (collapsed permission chip **and** the expanded menu — the chip and the menu items are separate
  render paths), the `+` menu, Transcript notices/banners, right-rail panel tabs, project configuration
  tabs, and archive/delete confirm dialogs (good place to check `{name}` interpolation).
- Genuine protocol/technical identifiers stay English by agreement: MCP, ACP, IDE, Token, CDP, VNC,
  stdio, Outposts, Blueprint. Ordinary product words (Knowledge, Playbook, Environment, Skill) do not.
  If a dictionary deliberately keeps an English zh value, check the project's `zhEnglishKeyAllowlist`
  before filing it — it may be an intentional decision rather than a bug.
- Chinese strings that persist in the **en** UI (`本机`, `内置 · v1`, `Rust/TypeScript 项目准则`,
  `通用工程工作准则`) come from the Rust backend and seeded DB rows, not from `web/`. Report them as
  out-of-scope for a frontend-only i18n PR.
- Templated strings: confirm real values are substituted and no bare `{count}` / `{name}` / `{host}` /
  `{destination}` / `{title}` / `{operation}` appears. When a template's parameter can be missing,
  check that the whole line is suppressed rather than rendering an empty sentence or a bare
  placeholder. Also watch for **double rendering** (`Switched to model Switched to model gpt-5.5`),
  which happens when the parameter falls back to a field (`data.message`) that already holds the full
  sentence.
- Session status labels (`sessionStatus.ts`) are a good instant-switch probe because the label must
  change in place with no reload. Triggering `running` / `idle` / `finished` needs a provider whose
  model ids match the app's built-in list; on the usual gateway they return `model_not_found`, so
  budget for marking those `untested` and cover the `error` label instead.

### Devin secrets needed

Nothing beyond the usual GUI bring-up. To cover `running` / `idle` / `finished` status labels you need
a provider (`LLM_Baseurl` / `LLM_KEY`) that actually serves a model id present in the app's built-in
model list; otherwise those states are not reachable.
