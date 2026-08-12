import { translate } from "./i18n";

export function sessionStatusLabel(
  runState: string | undefined,
  stopReason: string | undefined,
  terminalCause?: string,
): string {
  if (terminalCause === "model_stopped") return translate("modelStopped");
  switch (stopReason) {
    case "waiting_for_user":
      return translate("waitingForUser");
    case "waiting_for_approval":
      return translate("waitingForApproval");
    case "finished":
      return translate("finished");
    case "interrupted_by_user":
      return translate("interrupted");
    case "interrupted_by_crash":
      return translate("interruptedByCrash");
    case "host_unavailable":
      return translate("hostUnavailable");
    case "provider_error":
      return translate("providerError");
    case "policy_denied":
      return translate("policyDenied");
    case "context_exhausted":
      return translate("contextExhausted");
    case "internal_error":
      return translate("internalError");
    case "tool_preflight_error":
      return translate("toolPreflightError");
    case "usage_limit":
      return translate("usageLimit");
    case "harness_error":
      return translate("harnessError");
    case "turn_already_running":
      return runState === "running"
        ? translate("running")
        : translate("alreadyRunning");
    case "max_iterations":
      return translate("maxIterations");
    case "none":
      if (runState === "running") return translate("running");
      if (runState === "error") return translate("runError");
      return translate("idle");
    default:
      return translate("unknownStatus");
  }
}

export function sessionRecoveryAction(
  runState: string | undefined,
  stopReason: string | undefined,
): "retry" | "restart" | null {
  if (runState !== "error") return null;
  if (stopReason === "provider_error" || stopReason === "host_unavailable")
    return "retry";
  if (stopReason === "harness_error") return "restart";
  return null;
}
