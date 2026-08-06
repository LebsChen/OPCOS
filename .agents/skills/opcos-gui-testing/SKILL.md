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
