import { describe, expect, it } from "vitest";
import { sessionRecoveryAction, sessionStatusLabel } from "./sessionStatus";

describe("session status labels", () => {
  it.each([
    ["idle", "waiting_for_user", "Waiting for your reply"],
    ["idle", "waiting_for_approval", "Waiting for approval"],
    ["idle", "finished", "Finished"],
    ["error", "host_unavailable", "Host unavailable"],
    ["error", "internal_error", "Internal error"],
    ["error", "max_iterations", "Maximum iterations reached"],
    ["error", "tool_preflight_error", "Tool preflight check failed"],
    ["error", "usage_limit", "Usage limit reached"],
    ["error", "harness_error", "Agent runtime connection failed"],
    ["error", "turn_already_running", "Already running"],
    ["running", "none", "Running"],
    ["interrupted", "interrupted_by_crash", "Interrupted (application exited)"],
  ])("%s/%s is distinguishable", (runState, stopReason, label) => {
    expect(sessionStatusLabel(runState, stopReason)).toBe(label);
  });

  it("safely degrades unknown values", () => {
    expect(sessionStatusLabel("future", "future_reason")).toBe(
      "Unknown status",
    );
  });

  it("shows idle after a terminal event clears the run state", () => {
    expect(sessionStatusLabel("idle", "none")).toBe("Idle");
  });

  it("distinguishes a model stop from an interruption", () => {
    expect(sessionStatusLabel("idle", "finished", "model_stopped")).toBe(
      "Model stopped",
    );
  });

  it("offers recovery only for runtime failures", () => {
    expect(sessionRecoveryAction("error", "provider_error")).toBe("retry");
    expect(sessionRecoveryAction("error", "host_unavailable")).toBe("retry");
    expect(sessionRecoveryAction("error", "harness_error")).toBe("restart");
    expect(sessionRecoveryAction("error", "usage_limit")).toBeNull();
    expect(sessionRecoveryAction("error", "tool_preflight_error")).toBeNull();
    expect(
      sessionRecoveryAction("interrupted", "interrupted_by_user"),
    ).toBeNull();
  });
});
