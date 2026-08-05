import { describe, expect, it } from "vitest";
import { sessionStatusLabel } from "./sessionStatus";

describe("session status labels", () => {
  it.each([
    ["idle", "waiting_for_user", "等你回话"],
    ["idle", "waiting_for_approval", "等审批"],
    ["idle", "finished", "已完成"],
    ["error", "host_unavailable", "主机不可用"],
    ["error", "internal_error", "内部错误"],
    ["error", "max_iterations", "达到最大轮次"],
    ["running", "none", "运行中"],
  ])("%s/%s is distinguishable", (runState, stopReason, label) => {
    expect(sessionStatusLabel(runState, stopReason)).toBe(label);
  });

  it("safely degrades unknown values", () => {
    expect(sessionStatusLabel("future", "future_reason")).toBe("状态未知");
  });

  it("shows idle after a terminal event clears the run state", () => {
    expect(sessionStatusLabel("idle", "none")).toBe("空闲");
  });
});
