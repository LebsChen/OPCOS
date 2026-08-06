export function sessionStatusLabel(
  runState: string | undefined,
  stopReason: string | undefined,
): string {
  switch (stopReason) {
    case "waiting_for_user":
      return "等你回话";
    case "waiting_for_approval":
      return "等审批";
    case "finished":
      return "已完成";
    case "interrupted_by_user":
      return "已中断";
    case "interrupted_by_crash":
      return "已中断（应用退出）";
    case "host_unavailable":
      return "主机不可用";
    case "provider_error":
      return "模型服务错误";
    case "policy_denied":
      return "策略拒绝";
    case "context_exhausted":
      return "上下文已耗尽";
    case "internal_error":
      return "内部错误";
    case "max_iterations":
      return "达到最大轮次";
    case "none":
      if (runState === "running") return "运行中";
      if (runState === "error") return "运行出错";
      return "空闲";
    default:
      return "状态未知";
  }
}
