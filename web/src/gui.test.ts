import { describe, expect, it } from "vitest";
import { canRebindSession, hostFailureMessage, noticeClass, redactApproval } from "./gui";

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
});
