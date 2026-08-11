import { describe, expect, it } from "vitest";
import { sessionRecoveryAction, sessionStatusLabel } from "./sessionStatus";

describe("session status labels", () => {
  it.each([
    ["idle", "waiting_for_user", "等你回话"],
    ["idle", "waiting_for_approval", "等审批"],
    ["idle", "finished", "已完成"],
    ["error", "host_unavailable", "主机不可用"],
    ["error", "internal_error", "内部错误"],
    ["error", "max_iterations", "达到最大轮次"],
    ["error", "tool_preflight_error", "工具执行前检查失败"],
    ["error", "usage_limit", "已达到用量限制"],
    ["error", "harness_error", "Agent 运行时连接失败"],
    ["error", "turn_already_running", "正在运行"],
    ["running", "none", "运行中"],
    ["interrupted", "interrupted_by_crash", "已中断（应用退出）"],
  ])("%s/%s is distinguishable", (runState, stopReason, label) => {
    expect(sessionStatusLabel(runState, stopReason)).toBe(label);
  });

  it("safely degrades unknown values", () => {
    expect(sessionStatusLabel("future", "future_reason")).toBe("状态未知");
  });

  it("shows idle after a terminal event clears the run state", () => {
    expect(sessionStatusLabel("idle", "none")).toBe("空闲");
  });

  it("distinguishes a model stop from an interruption", () => {
    expect(sessionStatusLabel("idle", "finished", "model_stopped")).toBe(
      "模型主动结束",
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
