import { describe, expect, it } from "vitest";
import { classifyStepStatus, providerErrorPresentation } from "./transcript";

describe("transcript presentation helpers", () => {
  it("shortens provider errors while retaining details", () => {
    const error = providerErrorPresentation(
      '{"status_code":503,"error":{"message":"system_cpu_overloaded"}}',
    );
    expect(error.summary).toContain("HTTP 503");
    expect(error.toast).toContain("system cpu overloaded");
    expect(error.detail).toContain("system_cpu_overloaded");
  });

  it("recognizes structured provider errors before toast humanization", () => {
    const error = providerErrorPresentation(
      'provider_request_failed {"detail":"upstream overloaded"}',
    );
    expect(error.toast).toBe("Provider request failed: upstream overloaded");
    expect(error.toast).not.toContain("provider_request_failed");
  });

  it.each([
    ["running", "running"],
    ["ok", "ok"],
    ["interrupted", "failed"],
    ["error", "failed"],
  ] as const)("classifies %s tool steps", (status, expected) => {
    expect(classifyStepStatus(status)).toBe(expected);
  });
});
