export type Host = {
  id: string;
  name: string;
  online?: boolean;
  reason?: string;
};

export type Session = {
  id: string;
  title: string;
  host_id: string;
  host_name: string;
  model: string;
  mode: string;
};

export type TranscriptItem = {
  kind: string;
  payload: Record<string, unknown>;
};

export function canRebindSession(session: Session, hostId: string): boolean {
  return session.host_id === hostId;
}

export function hostFailureMessage(host: Host): string | null {
  return host.online === false ? host.reason || "Remote host unavailable" : null;
}

export function noticeClass(kind: string): string {
  return ["error", "interrupted", "compacted", "model_switch"].includes(kind)
    ? "notice"
    : "message";
}

export function redactApproval(value: unknown): string {
  const text = JSON.stringify(value);
  return text
    .replace(/Bearer\s+[^\s"]+/gi, "Bearer [redacted]")
    .replace(
      /(token|api[_-]?key|password|secret)(["']?\s*[:=]\s*["']?)[^,"'}\s]+/gi,
      "$1$2[redacted]",
    );
}

export function submitFailureMessage(error: unknown): string {
  const message = String(error);
  return message.includes("provider key is not configured")
    ? "Provider key is not configured; open Provider settings first."
    : message;
}
