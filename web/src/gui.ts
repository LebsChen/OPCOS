export type Host = {
  id: string;
  name: string;
  builtin?: boolean;
  online?: boolean;
  reason?: string;
};

export type Session = {
  id: string;
  title: string;
  host_id: string;
  host_name: string;
  model: string;
  provider?: string | null;
  mode: string;
  harness: string;
  workspace?: string;
  run_state?: string;
  stop_reason?: string;
  terminal_cause?: string;
  provider_finish_reason?: string;
  archived?: boolean;
  project_id?: string | null;
  agent_id?: string | null;
};

export function normalizePermissionMode(mode: string): string {
  const normalized = mode.trim().toLowerCase();
  return ["auto", "custom", "discuss", "interactive", "plan"].includes(
    normalized,
  )
    ? normalized
    : mode;
}

export function normalizeSession(session: Session): Session {
  return { ...session, mode: normalizePermissionMode(session.mode) };
}

export type ProjectAgent = {
  id: string;
  project_id: string;
  template_id?: string | null;
  sort_order: number;
  name: string;
  role: string;
  session_id?: string | null;
  provider?: string | null;
  model: string;
  harness: string;
  mode: string;
  system_prompt: string;
  worktree_path: string;
  branch: string;
  state: string;
};

export type Project = {
  id: string;
  name: string;
  host_id: string;
  host_name: string;
  repo_url: string;
  repo_root: string;
  default_branch: string;
  workflow_json: string;
  board_id: string;
  archived: boolean;
  online?: boolean | null;
  agents: ProjectAgent[];
};

export type ProjectAgentRosterRow = {
  agent: ProjectAgent;
  session: Session | null;
};

export function projectAgentRosterValue(
  value: string | null | undefined,
): string {
  return value?.trim() || "Unknown";
}

export function projectAgentRosterHost(session: Session | null): string {
  return projectAgentRosterValue(session?.host_name);
}

export function projectAgentRosterRows(
  agents: ProjectAgent[],
  sessions: Session[],
): ProjectAgentRosterRow[] {
  return agents.map((agent) => ({
    agent,
    session:
      sessions.find((session) => session.id === agent.session_id) ||
      sessions.find(
        (session) =>
          session.project_id === agent.project_id &&
          session.agent_id === agent.id,
      ) ||
      null,
  }));
}

export type TranscriptItem = {
  kind: string;
  payload: Record<string, unknown>;
};

export type SurfaceTab =
  "chat" | "terminal" | "desktop" | "browser" | "ide" | "review" | "worklog";

export type PendingQuestionData = {
  callId: string;
  question: string;
  options?: string[];
  allowMultiple?: boolean;
};

export function pendingQuestionFromPayload(
  callId: string,
  payload: Record<string, unknown>,
): PendingQuestionData {
  const nestedQuestion =
    payload.question && typeof payload.question === "object"
      ? (payload.question as Record<string, unknown>)
      : {};
  const rawOptions =
    payload.options ?? payload.choices ?? nestedQuestion.options;
  const options = Array.isArray(rawOptions)
    ? rawOptions.filter(
        (option): option is string => typeof option === "string",
      )
    : [];
  return {
    callId,
    question:
      typeof payload.question === "string"
        ? payload.question
        : typeof nestedQuestion.question === "string"
          ? nestedQuestion.question
          : typeof payload.prompt === "string"
            ? payload.prompt
            : "Answer required",
    options: options.length > 0 ? options : undefined,
    allowMultiple:
      payload.allow_multiple === true ||
      payload.allowMultiple === true ||
      payload.multi === true ||
      nestedQuestion.allow_multiple === true ||
      nestedQuestion.multi === true,
  };
}

export function reconcileRunningState(
  current: boolean,
  event: {
    kind: string;
    runState?: string;
  },
): boolean {
  if (event.kind === "turn_done") return event.runState === "running";
  if (event.kind === "question_requested") return false;
  return current;
}

export function effectiveRunningState(
  hasPendingQuestion: boolean,
  backendRunState: string | undefined,
  localRunning: boolean,
): boolean {
  if (hasPendingQuestion) return false;
  return backendRunState ? backendRunState === "running" : localRunning;
}

export function selectedSessionFromList(
  sessions: Session[],
  selectedId: string | null | undefined,
): Session | null {
  return sessions.find((session) => session.id === selectedId) ?? null;
}

export function sessionViewSelection(
  selectedId: string | null,
  selected: Session | null,
  lastKnown: Session | null,
): Session | null {
  return selected ?? (lastKnown?.id === selectedId ? lastKnown : null);
}

export function reconcileSelectedIdAfterRefresh(
  selectedId: string | null,
  refreshed: Session[],
  optimisticIds: ReadonlySet<string>,
): string | null {
  if (
    !selectedId ||
    refreshed.some((session) => session.id === selectedId) ||
    optimisticIds.has(selectedId)
  )
    return selectedId;
  return null;
}

export function mergeSessionsPreservingOptimistic(
  current: Session[],
  refreshed: Session[],
  optimisticIds: ReadonlySet<string>,
): Session[] {
  const refreshedIds = new Set(refreshed.map((session) => session.id));
  const preserved = current.filter(
    (session) => optimisticIds.has(session.id) && !refreshedIds.has(session.id),
  );
  return preserved.length > 0 ? [...refreshed, ...preserved] : refreshed;
}

export function updateSessionRunState(
  sessions: Session[],
  sessionId: string,
  runState: string | undefined,
  stopReason: string | undefined,
): Session[] {
  return sessions.map((session) =>
    session.id === sessionId
      ? { ...session, run_state: runState, stop_reason: stopReason }
      : session,
  );
}

export function submissionRoute(
  running: boolean,
  canSteer: boolean,
): "send" | "steer" | "blocked" {
  if (!running) return "send";
  return canSteer ? "steer" : "blocked";
}

export function hostStatusLabel(host: Host): string {
  if (host.online === true) return "Online";
  if (host.online === false) return "Offline";
  return "Unknown";
}

export function groupSessionsByHost(
  sessions: Session[],
): Array<{ hostId: string; hostName: string; sessions: Session[] }> {
  const groups = new Map<
    string,
    { hostId: string; hostName: string; sessions: Session[] }
  >();
  sessions.forEach((session) => {
    const existing = groups.get(session.host_id);
    if (existing) existing.sessions.push(session);
    else
      groups.set(session.host_id, {
        hostId: session.host_id,
        hostName: session.host_name,
        sessions: [session],
      });
  });
  return Array.from(groups.values()).sort((a, b) =>
    a.hostName.localeCompare(b.hostName),
  );
}

export function filterSessions(sessions: Session[], query: string): Session[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return sessions;
  return sessions.filter((session) =>
    [session.title, session.host_name, session.model, session.mode].some(
      (value) => value.toLowerCase().includes(needle),
    ),
  );
}

export function canRebindSession(session: Session, hostId: string): boolean {
  return session.host_id === hostId;
}

export function hostFailureMessage(host: Host): string | null {
  return host.online === false
    ? host.reason || "Remote host unavailable"
    : null;
}

export function noticeClass(kind: string): string {
  return ["error", "interrupted", "compacted", "model_switch"].includes(kind)
    ? "notice"
    : "message";
}

export function redactApproval(value: unknown): string {
  const text = JSON.stringify(value) ?? String(value ?? "");
  return text
    .replace(/Bearer\s+[^\s"]+/gi, "Bearer [redacted]")
    .replace(
      /(token|api[_-]?key|password|secret)(["']?\s*[:=]\s*["']?)[^,"'}\s]+/gi,
      "$1$2[redacted]",
    );
}

export function submitFailureMessage(error: unknown): string {
  const message = errorMessage(error);
  return message.includes("provider key is not configured")
    ? "Provider key is not configured; open Provider settings first."
    : message;
}

export function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  if (typeof error === "string" && error.trim()) return error;
  if (
    error &&
    typeof error === "object" &&
    "message" in error &&
    typeof error.message === "string" &&
    error.message.trim()
  )
    return error.message;
  try {
    const serialized = JSON.stringify(error);
    if (serialized && serialized !== "{}") return serialized;
  } catch {
    // Fall through to the stable generic message.
  }
  return "The requested action could not be completed.";
}

export function providerBaseUrlError(
  baseUrl: string,
  hasRegistryDefault: boolean,
): string | null {
  return baseUrl.trim() || hasRegistryDefault
    ? null
    : "Provider base URL is not configured; open Provider settings first.";
}
