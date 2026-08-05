import type { Attachment } from "./types";

export type TimelineEvent = {
  type: string;
  event_id: string;
  created_at_ms: number;
  [key: string]: unknown;
};

export const TRANSIENT_TIMELINE_EVENT_TYPES = [
  "assistant_delta",
  "reasoning_delta",
  "tool_call_delta",
] as const;

export type TimelineNode =
  | {
      kind: "user";
      text: string;
      ts?: number;
      attachments?: Attachment[];
    }
  | { kind: "assistant"; text: string; ts?: number }
  | {
      kind: "approval";
      callId: string;
      name: string;
      args: unknown;
      resolved?: "allow" | "deny";
    }
  | { kind: "question"; callId: string; text: string; options?: string[] }
  | {
      kind: "notice";
      text: string;
      noticeKind?: string;
      retriable?: boolean;
    }
  | {
      kind: "work";
      label: string;
      rows: Array<{ label: string; detail?: string }>;
      additions: number;
      deletions: number;
    };

function payload(event: TimelineEvent): Record<string, unknown> {
  const working = event.working_event;
  if (working && typeof working === "object") {
    const value = working as Record<string, unknown>;
    return value.payload && typeof value.payload === "object"
      ? (value.payload as Record<string, unknown>)
      : value;
  }
  return event;
}

function eventType(event: TimelineEvent): string {
  if (typeof event.type === "string") return event.type;
  const working = event.working_event;
  return working &&
    typeof working === "object" &&
    typeof (working as Record<string, unknown>).event_type === "string"
    ? String((working as Record<string, unknown>).event_type)
    : "";
}

export function mergeEvents(
  existing: TimelineEvent[],
  incoming: TimelineEvent | TimelineEvent[],
): TimelineEvent[] {
  const all = [
    ...existing,
    ...(Array.isArray(incoming) ? incoming : [incoming]),
  ];
  const seen = new Set<string>();
  const unique = all.filter((event) => {
    if (
      TRANSIENT_TIMELINE_EVENT_TYPES.includes(
        eventType(event) as (typeof TRANSIENT_TIMELINE_EVENT_TYPES)[number],
      )
    )
      return false;
    if (event.event_id && seen.has(event.event_id)) return false;
    if (event.event_id) seen.add(event.event_id);
    return true;
  });
  return unique
    .map((event, index) => ({ event, index }))
    .sort(
      (a, b) =>
        a.event.created_at_ms - b.event.created_at_ms || a.index - b.index,
    )
    .map(({ event }) => event);
}

export function buildTimeline(events: TimelineEvent[]): TimelineNode[] {
  const nodes: TimelineNode[] = [];
  let work: Extract<TimelineNode, { kind: "work" }> | null = null;
  let workStarted = 0;
  let workEnded = 0;
  const planSteps = new Map<string, Array<Record<string, unknown>>>();
  const flush = (endedAt = workEnded) => {
    if (!work) return;
    const seconds = Math.max(0, Math.round((endedAt - workStarted) / 1000));
    work.label = `Worked for ${seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${seconds % 60}s`}`;
    nodes.push(work);
    work = null;
  };
  for (const event of events) {
    const type = eventType(event);
    if (!type) continue;
    const data = payload(event);
    if (type === "user_message" || type === "initial_user_message") {
      flush(event.created_at_ms);
      nodes.push({
        kind: "user",
        text: String(data.message ?? data.text ?? ""),
        ts: Number.isFinite(Date.parse(String(event.timestamp)))
          ? Date.parse(String(event.timestamp)) / 1000
          : undefined,
        attachments: Array.isArray(data.attachments)
          ? (data.attachments as Attachment[])
          : undefined,
      });
      workStarted = 0;
      workEnded = 0;
    } else if (type === "devin_message") {
      flush(event.created_at_ms);
      nodes.push({
        kind: "assistant",
        text: String(data.message ?? ""),
        ts: Number.isFinite(Date.parse(String(event.timestamp)))
          ? Date.parse(String(event.timestamp)) / 1000
          : undefined,
      });
      workStarted = 0;
      workEnded = 0;
    } else if (type === "approval_pending" || type === "ask_user_pending") {
      flush(event.created_at_ms);
      const callId = String(data.call_id ?? "");
      if (type === "ask_user_pending") {
        nodes.push({
          kind: "question",
          callId,
          text: String(data.question ?? data.message ?? "Question"),
          options: Array.isArray(data.options)
            ? data.options.map(String)
            : undefined,
        });
      } else {
        nodes.push({
          kind: "approval",
          callId,
          name: String(data.tool ?? "tool"),
          args: data.arguments,
        });
      }
      workStarted = 0;
      workEnded = 0;
    } else if (type === "compacted") {
      if (!work)
        work = {
          kind: "work",
          label: "Worked for 0s",
          rows: [],
          additions: 0,
          deletions: 0,
        };
      if (!workStarted) workStarted = event.created_at_ms;
      workEnded = event.created_at_ms;
      work.rows.push({ label: "Earlier context compacted" });
    } else if (
      [
        "error",
        "interrupted",
        "provider_error",
        "compaction_summary_invalid",
        "mode_current",
        "mode_changed",
        "model_current",
        "model_switch",
        "session_list",
        "slash_help",
      ].includes(type)
    ) {
      flush(event.created_at_ms);
      const nested = data.payload;
      const text = String(
        data.message ??
          data.text ??
          (nested && typeof nested === "object"
            ? ((nested as Record<string, unknown>).message ??
              (nested as Record<string, unknown>).text)
            : ""),
      ).trim();
      if (text) {
        nodes.push({
          kind: "notice",
          text,
          noticeKind: type,
          retriable: type === "error" || type === "provider_error",
        });
      }
    } else if (
      [
        "iteration_stats",
        "context_growth_update",
        "simple_activity_update",
        "is_typing",
        "session_snapshot",
        "iteration_checkpoint",
        "status_update",
        "turn",
        "tool_result",
        ...TRANSIENT_TIMELINE_EVENT_TYPES,
        "stream_reset",
      ].includes(type)
    ) {
      continue;
    } else {
      if (!work)
        work = {
          kind: "work",
          label: "Worked for 0s",
          rows: [],
          additions: 0,
          deletions: 0,
        };
      if (!workStarted) workStarted = event.created_at_ms;
      workEnded = event.created_at_ms;
      if (type === "devin_thoughts") {
        const duration = Number(data.thinking_duration_ms ?? 0);
        work.rows.push({
          label: `Thought for ${Math.round(duration / 1000)}s`,
          detail: String(data.message ?? ""),
        });
      } else if (type === "shell_process_started") {
        work.rows.push({ label: String(data.command ?? "") });
      } else if (type === "multi_edit_result") {
        const updates = Array.isArray(data.file_updates)
          ? data.file_updates
          : [];
        for (const update of updates) {
          if (!update || typeof update !== "object") continue;
          const item = update as Record<string, unknown>;
          const added = Number(item.lines_added ?? 0);
          const removed = Number(item.lines_removed ?? 0);
          work.additions += added;
          work.deletions += removed;
          const basename =
            String(item.file_path ?? "")
              .split(/[\\/]/)
              .pop() ?? "";
          work.rows.push({
            label:
              item.action_type === "create"
                ? `Created ${basename} +${added}`
                : `Edited ${basename} +${added} −${removed}`,
          });
        }
      } else if (type === "todo_update") {
        const source = Array.isArray(data.steps) ? data.steps : data.todos;
        const todos: Record<string, unknown>[] = Array.isArray(source)
          ? source
              .filter(
                (todo): todo is Record<string, unknown> =>
                  !!todo && typeof todo === "object",
              )
              .map((todo): Record<string, unknown> => ({
                ...todo,
                content: todo.content ?? todo.description,
              }))
          : [];
        const planId = String(data.plan_id ?? "__legacy_plan__");
        const previousTodos = planSteps.get(planId);
        const completed = todos.filter((todo) =>
          ["done", "completed"].includes(String(todo.status)),
        ).length;
        if ((!previousTodos || todos.length > previousTodos.length) && work) {
          work.rows.push({ label: `Created ${todos.length} Tasks` });
        } else if (work && previousTodos) {
          const currentWork = work;
          todos.forEach((item, index) => {
            const previous = previousTodos[index];
            if (
              previous &&
              previous.step_id === item.step_id &&
              previous.status === item.status &&
              previous.description === item.description
            )
              return;
            currentWork.rows.push({
              label: `${completed}/${todos.length}#${index + 1} ${String(item.content ?? item.title ?? "")}`,
            });
          });
        }
        planSteps.set(planId, todos);
      } else if (
        type === "shell_process_completed" ||
        type === "read_file_started" ||
        type === "list_dir_started"
      ) {
        continue;
      } else if (
        type === "read_file_completed" ||
        type === "list_dir_completed"
      ) {
        const target = String(data.path ?? data.target ?? data.file_path ?? "");
        work.rows.push({
          label: target
            ? `${type.startsWith("read_file") ? "Read" : "Listed"} ${target}`
            : type.startsWith("read_file")
              ? "Read file"
              : "Listed directory",
        });
      } else if (type.endsWith("_started")) {
        if (
          ![
            "propose_plan_started",
            "plan_update_started",
            "plan_get_started",
            "plan_revise_started",
          ].includes(type) &&
          type !== "write_file_started" &&
          type !== "edit_file_started"
        ) {
          work.rows.push({ label: String(data.command ?? data.tool ?? type) });
        }
      }
    }
  }
  flush(workEnded || events.at(-1)?.created_at_ms || workStarted);
  return nodes;
}
