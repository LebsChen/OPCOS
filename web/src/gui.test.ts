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
} from "./gui";

describe("GUI boundary behavior", () => {
  it("does not offer local fallback for an offline remote host", () => {
    expect(hostFailureMessage({ id: "h", name: "Remote", online: false, reason: "connection refused" })).toBe("connection refused");
  });

  it("permits only the original host binding", () => {
    const session = { id: "s", title: "task", host_id: "remote-a", host_name: "A", model: "auto", mode: "Interactive" };
    expect(canRebindSession(session, "remote-a")).toBe(true);
    expect(canRebindSession(session, "remote-b")).toBe(false);
  });

  it("renders notices separately from regular messages", () => {
    expect(noticeClass("compacted")).toBe("notice");
    expect(noticeClass("assistant")).toBe("message");
  });

  it("redacts approval secrets before display", () => {
    expect(redactApproval({ token: "secret", command: "echo ok" })).toContain("[redacted]");
    expect(redactApproval({ token: "secret", command: "echo ok" })).not.toContain("secret");
  });

  it("shows missing provider configuration instead of an assistant success", () => {
    expect(submitFailureMessage("provider key is not configured; open Provider settings first")).toContain(
      "Provider key is not configured",
    );
  });

  it("requires a base URL when the registry has no default", () => {
    expect(providerBaseUrlError("", false)).toContain("base URL is not configured");
    expect(providerBaseUrlError("", true)).toBeNull();
  });

  it("does not contain the retired private gateway address", () => {
    const source = readFileSync(
      fileURLToPath(new URL("../../src-tauri/src/main.rs", import.meta.url)),
      "utf8",
    );
    expect(source).not.toContain(["ai", "yaoshen", "de5", "net"].join("."));
  });
});
