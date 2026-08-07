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

## Testing transcript CSS/layout fixes (chevrons, row wrapping)

Style-only fixes in `web/src/style.css` are picked up by **Vite HMR** in the running Tauri window, so no
rebuild/restart is needed — but the app must have been started against a Vite instance serving the branch
checkout. A screenshot of the fixed state alone proves nothing; use a **broken control**:

1. Screenshot the fixed rows (full-resolution `zoom` on the row strip, window maximized).
2. Temporarily edit `style.css` back to the pre-fix values, wait ~3 s for HMR, screenshot again.
3. `git checkout -- web/src/style.css` and screenshot once more to show the fix returning.

Report the measured pixel geometry (gap label→chevron, chevron→row right edge) rather than "looks right".
All collapsible transcript rows come from one component, `TranscriptDisclosure`
(`web/src/components/Transcript.tsx`), rendering `<details class="transcript-thought">`: `Thought for Ns`
(labelled), and the *bare* variants `Show output` (shell rows) and `View diff` / `View screenshot`
(artifact rows). One prompt covers three of the four families:

> 1) run: `echo <150 identical-ish chars>` ; 2) use write_file to create
> `src/routes/deep/nested/categories.js` with some content ; 3) run: `ls -R src`

The long `echo` gives the wrapping-label case, the write gives the `View diff` chevron and doubles as the
nested-directory local-write test. `View screenshot` needs a browser/screenshot artifact — if none is
produced, report it as untested instead of assuming parity with `View diff`.

## Faking a "stuck running" session (steering / recovery fixes)

`sessions.run_state` is read straight from sqlite by `list_sessions`, but:

- A `running` value **does not survive a restart**: startup recovery rewrites it to
  `interrupted` / `interrupted_by_crash` (frontend label `已中断（应用退出）`).
- Editing the DB while the app runs works (WAL, cross-process), but the frontend keeps its cached session
  list; navigating between views does *not* refetch. What does refetch is a full `refresh()` — the cheapest
  UI trigger is **creating another session from the home composer** (`submitHome` awaits `refresh()`), or
  any `turn_done` with `runState !== "running"`.

Recipe: stop the app → set `run_state='running', stop_reason='none'` → relaunch → set it again while live →
create a throwaway session → reopen the target session; it now shows `STATUS Running` / `Working for Ns`
with no engine turn active. Typing and sending there routes through the `steering` command
(`gui.ts::submissionRoute` + `App.tsx` `steer`), which is the path to exercise.

## Cheap Lead-local / project-routing fixture

`automatic_project_routing_active` is true only for the project member with `sort_order == 0` **and** role
`Lead`. Fastest fixture (no remote host): `git init -b main ~/opcos-test/<repo>` with one commit → sidebar
项目 `+` → name + 仓库路径 → 添加成员 with 名称 + 角色 `Lead` + Provider/Model set explicitly → 保存 →
card `启动会话` (click it twice: the first click creates the session, the button then becomes `打开会话`).
The member dialog's Provider/Model default to 默认/Auto, which fails on a box with only Cloudflare
configured — always set them in the dialog.

## Judging "a write failed" from the transcript

A failed local write renders as a single tool row
`Wrote <path>  Nms  failed · local host path rejected: path is outside local workspace`.
The presence/absence of a **separate** `Created <path> +N` row (a `multi_edit_result` event) is the real
signal for `emit_file_change` regressions. Cross-check in sqlite after a clean shutdown: search
`session_events.event_json` for `"multi_edit_result"` and confirm none mentions the failed path (the
column is `event_json`, and `session_events` has no `kind` column — `audit_events` does).

## Testing the MCP client (panel, catalogs, credentials, transports)

Route reality check before planning:

- The **only** UI that creates an MCP server config is project board → 项目配置 → **MCP** tab (名称 + 内容
  JSON → 新增配置). Content is validated and credential-ish keys are rejected, so credentials must go
  through 项目运行凭据 → `MCP credential` (`MCP server ID` = the config object id, `Credential JSON` =
  `{"bearer_token": "…"}`; the field is `type=password` and shows dots).
- Settings → **MCP** panel (`McpManage`) lists *server cards* (status, `Authorize` when
  status == `auth_required`, `Resources / prompts`, `Retry`). The per-tool rows/toggles in the same panel
  come from `mcp_tools`, which **errors for local-host sessions** (`本机 host 不提供远程 MCP tools`) — you
  need a bound remote RVM host to test tool toggles/approval at all.
- Two different credential scopes exist: the panel's manager reads the **global** key
  `mcp-credential:<object_id>`, while project sessions build their own manager reading
  `project:<pid>/mcp-credential:<object_id>`. A UI-entered credential therefore fixes agent/session calls
  but may not fix the Settings panel; if you must, inject the global key into
  `~/.config/com.opcos.desktop/secrets.enc` (header `OCS1` + 12-byte nonce + AES-GCM, key =
  `sha256("opcos-secret-store\0" + /etc/machine-id)`) — never print the value.
- Credentials are only read **at connect time**. Deleting/adding a credential does not affect an already
  connected client; restart the app (or `Retry` for the panel's manager) to force a fresh connect.

Useful oracles (setup, not UI evidence): the UI prints no tool count, so read
`mcp_tool_cache` / `mcp_resource_cache` / `mcp_prompt_cache` / `mcp_session_resources` from
`~/.config/com.opcos.desktop/opcos.db`, and probe the endpoint with curl beforehand
(`initialize` → `mcp-session-id` header → `tools/list`) to know the expected counts. Remote catalogs drift
(mcp.devin.ai returned 18 tools where a doc said 17) — treat exact counts as soft expectations.

Real endpoints that work anonymously: `https://mcp.context7.com/mcp` (2 tools, 0 resources/prompts),
`https://mcp.devin.ai/mcp` (`/sse` is gone — do not use it). `https://api.githubcopilot.com/mcp/` returns
401 + `WWW-Authenticate` with resource metadata, which is the way to exercise `auth_required`.

Legacy `http-sse` needs a local mock; a minimal one is enough (`GET /sse` → `event: endpoint` +
`data: /message?s=<id>`, `POST /message` → 202 with the reply pushed on the stream, plus a `/flip` route
that pushes `notifications/tools/list_changed` and adds a tool). Gotchas:
- Restarting the mock leaves the session runtime with a dead client: every tool call returns
  `MCP server is disconnected` until the whole app restarts (`resources/read` still works because it goes
  through the global manager). Start the mock **before** the app and leave it alone.
- `notifications/*/list_changed` only *drops* the cached catalog. There is no frontend listener for
  `mcp-catalog-updated`, so the panel shows `MCP server is not connected; retry the connection` until you
  click `Retry`. Expect an error toast in the middle of that flow, not an auto-refresh.

`Load into composer` (prompt → draft) may look like a no-op: the draft lands in `homeInput`, but the only
navigation to the Home composer (`openNewSessionHome`) clears it. If a draft never appears, check that path
before blaming `prompts/get` (verify the request really happened via the mock log or a direct curl probe).

Attached resources: the transcript intentionally shows only a chip `MCP resource: <uri>` (mime + bytes);
the body goes to the model. To prove the model really received it, attach a resource with a unique marker
string in the body and ask the agent to quote its first line. Also ask the agent to list its `mcp__*` tools
— that is the cheapest UI-level proof that resources/prompts are **not** registered as tools.

Devin Secrets Needed: `GH_PAT` (GitHub MCP bearer), `Devin_MCP_COG` (mcp.devin.ai bearer),
`CF_TOKEN`/`CF_ID` for the Cloudflare provider the agent runs on.

## MCP round-9 findings (commit `7854e25` and later)

Fixture layout that has proven reusable (keep them out of the repo, e.g. `/home/ubuntu/mcp-mock/`):
- `sse_server.py` — legacy `http-sse` mock. Useful extras: per-method request counters exposed on
  `GET /stats` (the only oracle for "did the client really re-request?"), paginated `tools/list`
  (page 1 + `nextCursor`) to prove cursor aggregation, an explicit JSON-RPC `-32601` on
  `resources/templates/list` to prove `MethodNotFound` degrades to an empty set instead of failing the
  connection, and separate `/flip` (tools) and `/flip-resource` (resources) routes so a `list_changed`
  refresh is *visible* in the panel (the card shows resource/prompt counts but no tool count, so a
  tools-only change is unobservable in the UI).
- `oauth_server.py` — local AS + protected MCP endpoint on one port: 401 with
  `WWW-Authenticate: … resource_metadata=…`, RFC 9728 + RFC 8414 metadata with `registration_endpoint`
  and `code_challenge_methods_supported:["S256"]`, DCR, a token endpoint that verifies the PKCE S256
  challenge and the loopback `redirect_uri`, short `expires_in`, a `/expire` route to force a refresh,
  and a `/stats` route returning a redacted event log (`authorize s256=True state=True
  loopback_callback=True`, `token refresh_token accepted=True`, `mcp authenticated … gen=N`). Write the
  issued token values to a side file only, so the leak audit can grep for them without ever printing
  them. This is the only practical way to prove the OAuth chain — real ASes (github.com) advertise no
  `registration_endpoint`.

Behaviours to expect / watch for when testing MCP:
- `pkill -f 'sse_server.py'` kills your own shell (the pattern matches the shell's command line). Use
  `pkill -9 -f 'sse_serv[e]r.py'` and start mocks with `(setsid python3 … &)`.
- Agent-facing MCP tools only appear in sessions whose `project_id` matches the project that owns the
  MCP config (`effective_config_objects`). A scratch session with `project_id = NULL` will report "no
  such tool" — always use the project's session (e.g. its Lead member session).
- After the OAuth token exchange the card can stay `auth_required`; `Retry` may issue no request at all.
  Restarting the app makes the stored token take effect. Check `mcp-credential:<object_id>` in
  `secrets.enc` (keys only!) to tell "token stored but not applied" from "token exchange failed".
- A legacy-SSE server that is *fully down* (not just restarted) can make a tool call hang for many
  minutes with no explicit error, and afterwards the session may refuse to start any new turn
  (`session_events` gets no rows) until the app is restarted. Budget for an app restart in that scenario
  and prefer restart-the-mock (self-healing works) over kill-the-mock when you only need reconnect
  coverage.
- Submitting a message while an attached resource's MCP server is unreachable can silently drop the
  submission (composer clears, no turn, no error) — detach context resources before down-server tests.
- GitHub MCP resources are 0.8–1.1 MB HTML apps. Attaching one kills the turn with
  `Provider request failed`; there is no truncation on the attachment path. Use a small mock resource
  with a unique marker to prove the model really consumes attached context.
- `mcp_resource_templates` / `mcp_subscribe_resource` / `mcp_unsubscribe_resource` have no frontend
  caller (`grep web/src`), so templates and subscribe/unsubscribe cannot be tested through the UI —
  report them untested-by-design rather than hunting for a button.
