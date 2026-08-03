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
- Settings → Skill → **Browse** can report `暂无仓库 Skill` plus a banner
  `local host I/O failed: No such file or directory (os error 2)` even when a valid
  `.agents/skills/<x>/SKILL.md` exists in the session workspace (`browse_skill_rules` →
  `opcos-assets::discover`). Verify with a real SKILL.md you dropped in the workspace yourself before
  concluding "the repo just has no skills".
- After deleting a project the `agent/*` **git branches survive** even though the worktrees are gone.
  Creating a second project against the same fixture repo then fails with
  `fatal: 'agent/code-1' is already used by worktree` — use a **fresh `git init` repo per project**
  in fixtures.
- Advancing past the last workflow stage renders 当前阶段 as `未启动` (falsy fallback in `App.tsx`), which
  looks like a regression but is only a label bug. Save a 2-stage workflow if you want a meaningful
  stage-advance assertion.
- `delete_project` keeps the member sessions and only nulls their `project_id`/`agent_id`; their
  `workspace` then points at a deleted worktree. Expected today — assert on nulled ownership, not on
  the session rows disappearing.
- Slash-command completion only works in the **session** composer; the 首页 New session composer is not
  passed `slashCommands`, so `/` there legitimately shows nothing.

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

## Devin secrets needed

- `RVM_WIN_TOKEN` — valid for DevBox `https://devbox.windevos.com` only (Antec `win.windevos.com`
  answers 401 for everything except `/api/health`).
- `OPCOS_PROVIDER_KEY` — OpenAI-compatible gateway `https://ai.yaoshen.de5.net/v1`.
