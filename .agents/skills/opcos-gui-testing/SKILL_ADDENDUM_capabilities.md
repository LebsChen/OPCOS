---
name: opcos-harness-capability-verification
description: How to prove OPCOS harness capabilities (builtin tools, approval/permission rules, plan tools, remote surfaces, MCP tool exposure) actually execute at runtime in the real Tauri app against a remote RVM host, including which capabilities are structurally impossible to test on a given host.
---

# Verifying OPCOS harness capabilities for real

Companion to `opcos-gui-testing` (bring-up) and `opcos-mcp-runtime-testing` (MCP).
This file captures what makes a capability *provable* rather than merely rendered.

## Ground truth: always check side effects out-of-band

Never trust a transcript row. For every write/exec capability use unique markers and verify with
the host API directly (token in `Authorization: Bearer` only, never in a URL/log/screenshot):

```bash
curl -s -X POST -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  -d '{"command":"cat /home/ctyun/repos/Cloud-Dev/h1-alpha-<ts>.txt"}' "$U/api/exec-sync"
```

Marker convention `h1-<slug>-<epoch>`; verify ENOENT *before* the round so a stale file can't fake a pass.

## The SQLite database is the best evidence source

`~/.config/com.opcos.desktop/opcos.db` (read-only URI, app can stay running):

- `tool_calls(session_id, message_sequence, call_id, name, arguments, result)` — the real tool result
  JSON. Denials look like `{"error":"tool call denied by user","error_details":{"code":"approval_denied"...}}`
  and policy refusals like `{"code":"policy_denied"}`. This is how you distinguish a *real* deny from
  a silent skip, and how you catch a later re-approved retry that recreates a "denied" file.
- `pending(session_id, call_id, tool, state, arguments)` — pending vs resolved approvals.
- `mcp_session_tools(session_id, source, name, enabled)` — MCP tool disables are **per session**, not global.

## Capability gating you must check before calling something a failure

Host capabilities decide which builtin tools the model is even offered
(`builtin_tool_capability_requirements()` in `crates/opcos-engine/src/lib.rs`):

```bash
curl -s -H "Authorization: Bearer $T" "$U/api/health"   # -> capabilities: [exec, pty, screenshot, computer_use, vnc, code_server]
```

- `browser_*` tools require a `browser` capability. A host exposing only `/cdp-ws` does **not**
  advertise `browser`, so the model legitimately has no `browser_navigate` — mark browser tools
  `untested (host lacks browser capability)`, not failed. The Browser rail pane can still render a
  preview, so the pane rendering proves nothing.
- `tool_search`/`tool_describe` only exist when progressive tool disclosure is enabled; by default
  the model has neither, and `browser_*`/`mcp:` tools are progressive-catalog tools.

Ask the model directly ("if X is not among your tools reply exactly X_MISSING") — it is the cheapest
way to learn what the toolset really contains, and the answer is verifiable in the transcript.

## Things that are known-awkward (check whether still true)

- `computer_use` may reject the model's natural first call: `{"action":"screenshot"}` fails with
  `invalid type: string "screenshot", expected internally tagged enum ComputerUseAction`. The working
  shape is `{"action":{"action":"screenshot"}}`. Models often then double-encode
  (`{"action":"{\"action\": \"screenshot\"}"}`), which hangs as an unresolved pending call. Budget
  extra turns and expect approval cards for each retry.
- `/api/screenshot` resize can fail with `convert: not found` (ImageMagick missing) — an intermittent
  HTTP 500 that is a host dependency issue, not an OPCOS bug.
- Remote paths must be absolute; relative paths are rejected with
  `remote path rejected: path is outside the configured remote workspace`.
- Approval cards may not refresh live when a second call needs approval — switch to another session
  and back to force a re-render. Cross-check `pending` in SQLite before declaring the UI wrong.
- The Desktop pop-out window can render blank while the inline VNC pane works; test both.
- Plan and Custom permission modes are deliberately hidden in `Composer.tsx` ⇒ `untested (no UI path)`.
- Standing grants only appear in automation runs (`ApprovalCard.tsx` needs `runTask && item.standingTarget`)
  ⇒ ordinary sessions give `untested (no UI path)`.

## Remote Editor / IDE

Direct remote-IDE loading landed on PR #201; on older builds the Editor shows
`Command ide_url not found`. If the frontend and the Rust binary come from different commits you get
exactly that error — rebuild (`cargo build -p opcos`) and relaunch after any rebase.
Prove it is the *real* remote IDE by using the IDE's own terminal (prompt shows `ctyun@<remote-host>`)
and `Ctrl+P` to open a marker file created by another capability.
**The token appears in the IDE iframe URL on #201 — never capture a URL bar or query string in a
screenshot or annotation.**

## MCP

`POST $U/mcp` with `Accept: application/json, text/event-stream` and `params:{}` present
(`tools/list` returns empty without `params`). The host serves `shell_exec`, `read_file`, … .
Settings → MCP listing these as "remote · host-provided" reflects a real handshake, but that does
**not** mean the model can call them — verify by asking the model, and by checking whether a name
collides with a builtin (`read_file` exists as both; disabling the MCP one does not disable the builtin).

## Devin Secrets Needed

- `RVM_LINUX_URL`
- `RVM_LINUX_TOKEN`
