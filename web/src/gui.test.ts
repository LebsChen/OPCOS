import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  canRebindSession,
  hostFailureMessage,
  noticeClass,
  redactApproval,
  submitFailureMessage,
  providerBaseUrlError,
  pendingQuestionFromPayload,
  reconcileRunningState,
  effectiveRunningState,
  mergeSessionsPreservingOptimistic,
  normalizePermissionMode,
  normalizeSession,
  reconcileSelectedIdAfterRefresh,
  selectedSessionFromList,
  sessionViewSelection,
  shouldRefreshForSessionLifecycleEvent,
  shouldRetrySurfaceStart,
  shouldResetSurfaceForSleep,
  shouldShowSurfaceReconnect,
  shouldShowSurfaceRetry,
  preserveSurfaceTabWhileSleeping,
  surfaceLifecycleEventMatches,
  surfaceNeedsConnection,
  updateSessionRunState,
  projectAgentRosterHost,
  projectAgentRosterValue,
  projectAgentRosterRows,
} from "./gui";

describe("GUI boundary behavior", () => {
  it("routes session lifecycle events through the shared Tauri event channel", () => {
    expect(
      shouldRefreshForSessionLifecycleEvent({ kind: "session-sleep" }),
    ).toBe(true);
    expect(
      shouldRefreshForSessionLifecycleEvent({ kind: "session-wake" }),
    ).toBe(true);
    expect(shouldRefreshForSessionLifecycleEvent({ kind: "turn_done" })).toBe(
      false,
    );
  });

  it("resets a surface only on the transition into sleep", () => {
    expect(shouldResetSurfaceForSleep("awake", "asleep")).toBe(true);
    expect(shouldResetSurfaceForSleep("asleep", "asleep")).toBe(false);
    expect(shouldResetSurfaceForSleep("asleep", "awake")).toBe(false);
    expect(shouldShowSurfaceReconnect("asleep")).toBe(true);
    expect(shouldShowSurfaceReconnect("awake")).toBe(false);
    expect(surfaceNeedsConnection("terminal", null, false)).toBe(true);
    expect(surfaceNeedsConnection("terminal", 1234, false)).toBe(false);
    expect(surfaceNeedsConnection("terminal", null, true)).toBe(false);
    expect(
      shouldRetrySurfaceStart({
        invalidated: true,
        port: null,
        sleeping: false,
        tab: "terminal",
      }),
    ).toBe(true);
    expect(
      shouldRetrySurfaceStart({
        invalidated: false,
        port: null,
        sleeping: false,
        tab: "terminal",
      }),
    ).toBe(false);
    expect(
      shouldShowSurfaceRetry({ busy: false, port: null, sleeping: false }),
    ).toBe(true);
    expect(
      shouldShowSurfaceRetry({ busy: false, port: null, sleeping: true }),
    ).toBe(false);
    expect(preserveSurfaceTabWhileSleeping("asleep")).toBe(true);
    expect(preserveSurfaceTabWhileSleeping("awake")).toBe(false);
    expect(
      surfaceLifecycleEventMatches({
        eventSessionId: "s",
        eventPort: 1234,
        currentSessionId: "s",
        currentPort: 1234,
      }),
    ).toBe(true);
    expect(
      surfaceLifecycleEventMatches({
        eventSessionId: "other",
        eventPort: 1234,
        currentSessionId: "s",
        currentPort: 1234,
      }),
    ).toBe(false);
    expect(
      surfaceLifecycleEventMatches({
        eventSessionId: "s",
        eventPort: 1235,
        currentSessionId: "s",
        currentPort: 1234,
      }),
    ).toBe(false);
  });

  it("normalizes persisted permission modes at the frontend boundary", () => {
    expect(normalizePermissionMode("Interactive")).toBe("interactive");
    expect(normalizePermissionMode("Auto")).toBe("auto");
    expect(normalizePermissionMode("Discuss")).toBe("discuss");
    expect(normalizePermissionMode("future-mode")).toBe("future-mode");
    expect(
      normalizeSession({
        id: "s",
        title: "task",
        host_id: "h",
        host_name: "Host",
        model: "auto",
        mode: "Interactive",
        harness: "builtin",
      }).mode,
    ).toBe("interactive");
  });

  it("matches project agents to existing sessions without inferring hierarchy", () => {
    const agents = [
      {
        id: "agent-1",
        project_id: "project-1",
        sort_order: 0,
        name: "Builder",
        role: "Code",
        session_id: null,
        model: "auto",
        harness: "builtin",
        mode: "Auto",
        system_prompt: "",
        worktree_path: "/repo",
        branch: "dev",
        state: "Active",
      },
    ];
    const session = {
      id: "session-1",
      title: "Builder",
      host_id: "host-1",
      host_name: "Remote",
      model: "auto",
      mode: "Auto",
      harness: "builtin",
      project_id: "project-1",
      agent_id: "agent-1",
      run_state: "idle",
      stop_reason: "finished",
    };
    expect(projectAgentRosterRows(agents, [session])[0].session).toEqual(
      session,
    );
    expect(projectAgentRosterHost(session)).toBe("Remote");
    expect(projectAgentRosterHost(session)).not.toBe("Viewing session host");
  });

  it("keeps unknown agent session fields absent instead of inventing defaults", () => {
    const rows = projectAgentRosterRows(
      [
        {
          id: "agent-1",
          project_id: "project-1",
          sort_order: 0,
          name: "Builder",
          role: "Code",
          session_id: null,
          model: "auto",
          harness: "builtin",
          mode: "Auto",
          system_prompt: "",
          worktree_path: "/repo",
          branch: "dev",
          state: "Active",
        },
      ],
      [],
    );
    expect(rows).toHaveLength(1);
    expect(rows[0].session).toBeNull();
    expect(projectAgentRosterValue("")).toBe("Unknown");
    expect(projectAgentRosterValue(null)).toBe("Unknown");
  });

  it("renders an empty project roster as an empty list", () => {
    expect(projectAgentRosterRows([], [])).toEqual([]);
  });

  it("does not offer local fallback for an offline remote host", () => {
    expect(
      hostFailureMessage({
        id: "h",
        name: "Remote",
        online: false,
        reason: "connection refused",
      }),
    ).toBe("connection refused");
  });

  it("permits only the original host binding", () => {
    const session = {
      id: "s",
      title: "task",
      host_id: "remote-a",
      host_name: "A",
      model: "auto",
      mode: "Interactive",
      harness: "builtin",
    };
    expect(canRebindSession(session, "remote-a")).toBe(true);
    expect(canRebindSession(session, "remote-b")).toBe(false);
  });

  it("renders notices separately from regular messages", () => {
    expect(noticeClass("compacted")).toBe("notice");
    expect(noticeClass("assistant")).toBe("message");
  });

  it("redacts approval secrets before display", () => {
    expect(redactApproval({ token: "secret", command: "echo ok" })).toContain(
      "[redacted]",
    );
    expect(
      redactApproval({ token: "secret", command: "echo ok" }),
    ).not.toContain("secret");
  });

  it("shows missing provider configuration instead of an assistant success", () => {
    expect(
      submitFailureMessage(
        "provider key is not configured; open Provider settings first",
      ),
    ).toContain("Provider key is not configured");
  });

  it("requires a base URL when the registry has no default", () => {
    expect(providerBaseUrlError("", false)).toContain(
      "base URL is not configured",
    );
    expect(providerBaseUrlError("", true)).toBeNull();
  });

  it("preserves ask_user options and multi-select metadata", () => {
    expect(
      pendingQuestionFromPayload("q-1", {
        question: "Choose a delivery format",
        options: ["A", "B", 3],
        allow_multiple: true,
      }),
    ).toEqual({
      callId: "q-1",
      question: "Choose a delivery format",
      options: ["A", "B"],
      allowMultiple: true,
    });
  });

  it("accepts legacy open-ended ask_user payloads", () => {
    expect(
      pendingQuestionFromPayload("q-2", { question: "Tell me more" }),
    ).toEqual({
      callId: "q-2",
      question: "Tell me more",
      options: undefined,
      allowMultiple: false,
    });
  });

  it("normalizes an ask_user approval event as a question payload", () => {
    expect(
      pendingQuestionFromPayload("q-3", {
        prompt: "Choose one",
        options: ["A", "B"],
        allowMultiple: true,
      }),
    ).toMatchObject({
      callId: "q-3",
      question: "Choose one",
      options: ["A", "B"],
      allowMultiple: true,
    });
  });

  it("clears running when turn_done arrives after a session switch", () => {
    expect(
      reconcileRunningState(true, {
        kind: "turn_done",
        runState: "idle",
      }),
    ).toBe(false);
  });

  it("keeps the composer enabled while a question awaits an answer", () => {
    expect(effectiveRunningState(true, "running", true)).toBe(false);
  });

  it("lets the authoritative idle state clear a stale local running flag", () => {
    expect(effectiveRunningState(false, "idle", true)).toBe(false);
  });

  it("keeps a completed session on the send path", () => {
    expect(effectiveRunningState(false, "idle", false)).toBe(false);
  });

  it("derives every selected surface from the refreshed session list", () => {
    const running = {
      id: "s",
      title: "task",
      host_id: "h",
      host_name: "host",
      model: "model",
      mode: "Auto",
      harness: "builtin",
      run_state: "running",
      stop_reason: "none",
    };
    expect(selectedSessionFromList([running], "s")?.run_state).toBe("running");
    const idle = { ...running, run_state: "idle" };
    expect(selectedSessionFromList([idle], "s")?.run_state).toBe("idle");
  });

  it("keeps an optimistic created session through a refresh race", () => {
    const created = {
      id: "new",
      title: "new task",
      host_id: "h",
      host_name: "host",
      model: "model",
      mode: "Auto",
      harness: "builtin",
      run_state: "running",
      stop_reason: "none",
    };
    const optimistic = new Set([created.id]);
    const afterMissingRefresh = mergeSessionsPreservingOptimistic(
      [created],
      [],
      optimistic,
    );
    expect(selectedSessionFromList(afterMissingRefresh, created.id)).toEqual(
      created,
    );
    const refreshed = { ...created, run_state: "idle" };
    expect(
      selectedSessionFromList(
        mergeSessionsPreservingOptimistic(
          afterMissingRefresh,
          [refreshed],
          optimistic,
        ),
        created.id,
      ),
    ).toEqual(refreshed);
  });

  it("updates a running session row without changing the selected id", () => {
    const session = {
      id: "s",
      title: "task",
      host_id: "h",
      host_name: "host",
      model: "model",
      mode: "Auto",
      harness: "builtin",
      run_state: "idle",
      stop_reason: "none",
    };
    const updated = updateSessionRunState(
      [session],
      session.id,
      "running",
      "none",
    );
    expect(selectedSessionFromList(updated, session.id)?.run_state).toBe(
      "running",
    );
    expect(selectedSessionFromList(updated, "other")).toBeNull();
  });

  it("keeps the session view target while a selected row is temporarily missing", () => {
    const lastKnown = {
      id: "s",
      title: "task",
      host_id: "h",
      host_name: "host",
      model: "model",
      mode: "Auto",
      harness: "builtin",
      run_state: "running",
      stop_reason: "none",
    };
    expect(sessionViewSelection("s", null, lastKnown)).toEqual(lastKnown);
    expect(sessionViewSelection("other", null, lastKnown)).toBeNull();
    expect(sessionViewSelection(null, null, lastKnown)).toBeNull();
  });

  it("clears selection when a non-optimistic selected session is deleted", () => {
    const session = {
      id: "s",
      title: "task",
      host_id: "h",
      host_name: "host",
      model: "model",
      mode: "Auto",
      harness: "builtin",
      run_state: "idle",
      stop_reason: "none",
    };
    expect(reconcileSelectedIdAfterRefresh("s", [], new Set())).toBeNull();
    expect(reconcileSelectedIdAfterRefresh("s", [], new Set(["s"]))).toBe("s");
    expect(reconcileSelectedIdAfterRefresh("s", [session], new Set())).toBe(
      "s",
    );
  });

  it("does not contain the retired private gateway address", () => {
    const source = readFileSync(
      fileURLToPath(new URL("../../src-tauri/src/main.rs", import.meta.url)),
      "utf8",
    );
    expect(source).not.toContain(["ai", "yaoshen", "de5", "net"].join("."));
  });

  it("routes desktop and editor tiles through the real surface machinery", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain(
      '{opened.includes("desktop") && panelTab === "desktop" && (',
    );
    expect(source).toContain('capabilities.vnc?.state === "Unavailable"');
    expect(source).toContain(
      '{opened.includes("ide") && panelTab === "ide" && (',
    );
    expect(source).toContain(
      '<SurfaceView tab="ide" selected={selected} onError={onError} />',
    );
    expect(source).not.toContain('<PlannedPane title="Desktop">');
    expect(source).not.toContain('<PlannedPane title="Editor">');
  });

  it("gates remote surfaces by session capabilities and keeps approvals keyed", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain('"session_capabilities"');
    expect(source).toContain('capabilities.browser?.state === "Unavailable"');
    expect(source).toContain("Object.values(pendingApprovals)");
    expect(source).toContain("delete next[callId]");
  });

  it("restores failed home submissions into the session composer", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain("setRestoredComposerDraft({ text, nonce:");
    expect(source).toContain(
      "restoreDraft={restoredComposerDraft || undefined}",
    );
    expect(source).toContain("setHomeInput(text)");
  });

  it("reconciles approvals from the backend when a turn ends", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain(
      '>("list_pending", { sessionId: payload.session_id })',
    );
    expect(source).toContain(
      'item.tool !== "ask_user" && item.state !== "resolved"',
    );
  });

  it("gates the editor by the remote IDE capability", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain('capabilities.ide?.state === "Unavailable"');
    expect(source).toContain('panelTab === "ide"');
  });

  it("submits the first message optimistically without blocking on refresh", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain("optimisticUserMessageEvent");
    expect(source).toContain("submittingSessionIdRef");
    expect(source).toContain("void refresh().catch(onError)");
    expect(source).toContain('command("submit_turn"');
  });

  it("uses the server-provided direct IDE URL", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain("ws://127.0.0.1:${port}");
    expect(source).toContain('command<string>("ide_url"');
    expect(source).toContain("Remote IDE workspace is not configured");
    expect(source).not.toContain(
      'folderUri: selected.workspace || "/workspace"',
    );
    expect(source).toContain("src={ideUrl}");
    expect(source).not.toMatch(/Authorization:\s*Bearer/i);
  });

  it("passes the configured VNC password to noVNC and surfaces handshake failures", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain('command<string | null>("vnc_password"');
    expect(source).toContain("credentials: vncPassword");
    expect(source).toContain('addEventListener("securityfailure"');
    expect(source).toContain('addEventListener("credentialsrequired"');
    expect(source).toContain('addEventListener("disconnect"');
    expect(source).toContain("configure the host VNC password");
  });

  it("preserves an existing host VNC password when editing host metadata", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain(
      'const password = await command<string | null>("vnc_password"',
    );
    expect(source).toContain('setHostVncPassword(password || "")');
    expect(source).toContain("vncPassword: hostVncPassword");
  });

  it("surfaces real sleep state and stops owned surfaces", () => {
    const source = readFileSync(
      fileURLToPath(new URL("./App.tsx", import.meta.url)),
      "utf8",
    );
    expect(source).toContain('session.sleep_state === "asleep"');
    expect(source).toContain('translate("sessionAsleep")');
    expect(source).toContain('command("stop_surface", { port: activePort })');
    expect(source).toContain(
      'command("touch_session", { sessionId: selectedId })',
    );
    expect(source).toContain("lastTouchedSessionRef");
    expect(source).toContain("60_000");
    expect(source).toContain('selected?.sleep_state === "asleep"');
    expect(source).toContain('listen<UiEvent>("opcos://event"');
    expect(source).toContain("shouldRefreshForSessionLifecycleEvent(payload)");
    expect(source).not.toContain('listen("session-sleep"');
    expect(source).not.toContain('listen("session-wake"');
    expect(source).toContain("surfaceSleepingDescription");
    expect(source).toContain("reconnectSurface");
    expect(source).toContain("surfaceUnavailable");
    expect(source).toContain("retrySurface");
    expect(source).toContain("surfaceRetryToken");
    expect(source).toContain("surface-ended");
    expect(source).toContain("preserveSurfaceTabWhileSleeping");
    expect(source).toContain("touchSessionActivity");
  });
});
