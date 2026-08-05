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
  selectedSessionFromList,
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

  it("does not contain the retired private gateway address", () => {
    const source = readFileSync(
      fileURLToPath(new URL("../../src-tauri/src/main.rs", import.meta.url)),
      "utf8",
    );
    expect(source).not.toContain(["ai", "yaoshen", "de5", "net"].join("."));
  });
});
