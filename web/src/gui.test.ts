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
  selectedSessionFromList,
  sessionViewSelection,
  updateSessionRunState,
} from "./gui";

describe("GUI boundary behavior", () => {
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
    expect(sessionViewSelection(null, null, lastKnown)).toBeNull();
  });

  it("does not contain the retired private gateway address", () => {
    const source = readFileSync(
      fileURLToPath(new URL("../../src-tauri/src/main.rs", import.meta.url)),
      "utf8",
    );
    expect(source).not.toContain(["ai", "yaoshen", "de5", "net"].join("."));
  });
});
