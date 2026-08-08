---
name: opcos-mcp-runtime-testing
description: How to prove OPCOS MCP client behaviour at runtime (which tools the model really receives, approval/deny paths, list_changed refresh) in the real Tauri app against a remote RVM host. Suggested additions to the existing opcos-gui-testing skill.
---

# Verifying OPCOS MCP behaviour at runtime

## Never trust the MCP settings label — observe the provider request

Settings → MCP shows `Enable`/`Disable` per host tool, but that label is derived from
`mcp_session_tools` and does **not** prove what the engine sends. Use a local
OpenAI-compatible fixture provider that echoes the received `tools` array:

- Serve `POST /v1/chat/completions` and `GET /v1/models` on `127.0.0.1:<port>`; support
  `stream: true` (SSE `data:` chunks + `[DONE]`) because OPCOS streams.
- Reply with text like `TOOLS_SENT=<n> | MCP_TOOLS=<comma list of names starting with mcp:>`
  so the answer is visible in the OPCOS transcript itself (good for recordings).
- To drive a real tool call, emit an OpenAI `tool_calls` response for `mcp:<tool>` when the
  latest user message contains a marker such as `SHELLCALL:<command>`. Only echo a returned
  tool result when the **last** message has `role: "tool"`, otherwise history from an earlier
  round short-circuits every later turn.
- Register it in Settings → Provider. Custom providers created there may not appear in the
  per-session provider dropdown; a working fallback is to point an existing built-in entry
  (e.g. OpenAI) at `http://127.0.0.1:<port>/v1` with any dummy key.

Host MCP tools reach the model as `mcp:<tool>`, and the engine only includes tools that have
an explicit `mcp_session_tools` row with `enabled=1`. Inspect state with:

```
sqlite3 ~/.config/com.opcos.desktop/opcos.db \
  "select session_id,source,name,enabled from mcp_session_tools"
```

Copy the db plus `-wal`/`-shm` before reading while the app runs.

A brand-new session has **no rows**, so the UI shows every tool as enabled while the model
receives none. To create an explicit enabled row, click `Disable` then `Enable`.

## Toggles may need an app restart

The desktop layer caches a `GuiEngine` per session in `state.engines` and applies the MCP
selection only when constructing a new engine, so toggling a tool mid-session may not affect
the running session. If a toggle appears not to work, restart the app
(`pkill -f target/debug/opcos`, relaunch, `wmctrl -a OPCOS` +
`wmctrl -r :ACTIVE: -b add,maximized_vert,maximized_horz`) and re-check before calling it a
regression; report both the live and post-restart behaviour.

## Approval path

Set the composer mode to “Ask for approval” (Interactive). `mcp:*` tools are `External` risk,
so they always go to approval in that mode. Prove the behaviour out-of-band, not from the
transcript alone: before approving, check the side effect is absent on the remote host via
`POST $RVM_DEVBOX_URL/api/exec-sync` (bearer header only), then Deny and re-check, then run a
fresh round with a new marker filename and Approve. Deny surfaces
`{"error":"tool call denied by user"}` as the tool result. Use a unique marker file per round
so a stale file cannot fake a pass.

## Registering an MCP server (no UI path today)

There is no UI to add a custom MCP server: all catalog entries are read-only `builtin`
`config_object`s and the template editor only handles agent/team/command kinds. For fixtures,
insert an active config object and restart the app:

```sql
INSERT INTO config_object (id,kind,name,server_key,scope_kind,scope_key,status,created_at,current_version_id)
VALUES ('template-custom-mocksse','mcp','MockSSE','mocksse01','global',NULL,'active',<now>,'template-custom-mocksse:v1');
INSERT INTO config_object_version (id,object_id,version,content,content_hash,created_at,note,metadata_json)
VALUES ('template-custom-mocksse:v1','template-custom-mocksse',1,
        '{"transport":"http-sse","url":"http://127.0.0.1:8765/sse","enabled":true,"requires_approval":true}',
        'hash',<now>,'created','{}');
```

`transport` values: `stdio`, `streamable-http`, `http-sse` (legacy). `initialize_mcp` only
loads `kind='mcp' AND status='active'`.

## `list_changed` refresh

Drive a fixture that pushes `notifications/tools/list_changed` on its SSE stream. Open
Settings → MCP → `Resources / prompts` on the server; the header
`<name> resources (R) · prompts (P) · tools (T)` is the surface that must update by itself
(the frontend listens on the `mcp-catalog-updated` Tauri event). Toggle the fixture's tool set
and watch T change within ~10 s with no clicks. A fixture whose `/bump` *toggles* a tool is
better than one that only adds, so the test is repeatable without restarting the fixture.

## Devin Secrets Needed

- `RVM_DEVBOX_URL`, `RVM_DEVBOX_TOKEN` — remote host; token only in `Authorization: Bearer`.
- Optionally a model provider key; not required if the fixture provider above is used.
