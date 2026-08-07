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

export function lastActivityLabel(
  rows: Array<{ activityLabel?: boolean; label: string }>,
) {
  return [...rows].reverse().find((row) => row.activityLabel)?.label;
}

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
  | { kind: "sleep"; text: string }
  | {
      kind: "notice";
      text: string;
      noticeKind?: string;
      retriable?: boolean;
    }
  | {
      kind: "work";
      label: string;
      startedAt?: number;
      rows: Array<{
        label: string;
        detail?: string;
        thoughtForCallId?: string;
        activityLabel?: boolean;
        callId?: string;
        terminalOutput?: string;
        terminalTruncated?: boolean;
        terminalTotalBytes?: number;
        shellId?: string;
        processId?: string;
        exitCode?: number;
        durationMs?: number;
        startedAt?: number;
        resultSummary?: string;
        resultError?: boolean;
        denied?: boolean;
        isMajorAction?: boolean;
        artifactId?: string;
        artifactKind?: string;
        artifactMime?: string;
        plan?: {
          steps: Array<{
            content: string;
            status?: string;
          }>;
        };
      }>;
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

function toolLabel(tool: string, args: unknown): string {
  const values =
    args && typeof args === "object"
      ? (args as Record<string, unknown>)
      : undefined;
  const target = String(
    values?.path ??
      values?.target ??
      values?.file_path ??
      values?.command ??
      values?.query ??
      values?.pattern ??
      values?.identifier ??
      values?.issue_id ??
      values?.repo ??
      "",
  ).trim();
  const verb =
    {
      read_file: "Read",
      list_dir: "Listed",
      write_file: "Wrote",
      edit_file: "Edited",
      git_status: "Checked git status",
      git_diff: "Reviewed git diff",
      git_log: "Viewed git log",
      git_rev_parse: "Resolved git revision",
      git_create_branch: "Created branch",
      git_stage_commit: "Created commit",
      git_push: "Pushed changes",
      github_create_issue: "Created GitHub issue",
      github_create_pull_request: "Created GitHub pull request",
      github_get_pull_request: "Read GitHub pull request",
      github_list_issues: "Listed GitHub issues",
      github_list_repositories: "Listed GitHub repositories",
      github_ci_status: "Checked GitHub CI",
      github_ci_failure_log: "Read GitHub CI failure log",
      linear_get_issue: "Read Linear issue",
      linear_list_my_issues: "Listed Linear issues",
      linear_comment_issue: "Commented on Linear issue",
      linear_update_issue_status: "Updated Linear issue",
      gitlab_list_projects: "Listed GitLab projects",
      gitlab_list_issues: "Listed GitLab issues",
      jira_search_issues: "Searched Jira issues",
      repo_index_find_symbol: "Found repository symbol",
      repo_index_glob: "Matched repository paths",
      repo_index_search: "Searched repository index",
      lsp_definition: "Found definition",
      lsp_references: "Found references",
      lsp_diagnostics: "Checked diagnostics",
      browser_navigate: "Navigated browser",
      browser_read: "Read browser",
      browser_measure: "Measured browser",
      browser_assert_geometry: "Verified browser geometry",
      browser_screenshot: "Captured browser screenshot",
      browser_status: "Checked browser status",
      browser_set_viewport: "Set browser viewport",
      browser_click: "Clicked browser",
      computer_use: "Used computer",
      run_shell: "Ran command",
      background_job_start: "Started background job",
      background_job_status: "Checked background job",
      background_job_output: "Read background job output",
      background_job_kill: "Stopped background job",
      propose_plan: "Created plan",
      plan_get: "Read plan",
      plan_update: "Updated plan",
      plan_revise: "Revised plan",
      secrets_list: "Listed secrets",
      skill_search_learned: "Searched learned skills",
      skill_get_learned: "Read learned skill",
      skill_save_learned: "Saved learned skill",
    }[tool] ??
    (tool.startsWith("slack_")
      ? "Used Slack"
      : tool.startsWith("discord_")
        ? "Used Discord"
        : tool.startsWith("telegram_")
          ? "Used Telegram"
          : tool.startsWith("notion_")
            ? "Searched Notion"
            : tool.startsWith("stripe_")
              ? "Read Stripe"
              : tool);
  return target ? `${verb} ${target}` : verb;
}

function resultSummary(raw: unknown): {
  summary?: string;
  error: boolean;
} {
  if (typeof raw === "string") {
    return { summary: raw.slice(0, 240), error: false };
  }
  if (!raw || typeof raw !== "object") {
    return { error: false };
  }
  const value = raw as Record<string, unknown>;
  const errorValue = value.error;
  const error =
    errorValue !== undefined || value.ok === false || value.success === false;
  for (const key of ["error", "message", "detail", "reason", "summary"]) {
    const candidate = value[key];
    if (typeof candidate === "string" && candidate.trim()) {
      return { summary: candidate.slice(0, 240), error };
    }
  }
  const content = value.content;
  if (typeof content === "string" && content.trim()) {
    return { summary: content.slice(0, 240), error };
  }
  if (Array.isArray(content)) {
    const text = content
      .map((item) =>
        item && typeof item === "object"
          ? (item as Record<string, unknown>).text
          : undefined,
      )
      .filter((item): item is string => typeof item === "string")
      .join(" ")
      .trim();
    if (text) return { summary: text.slice(0, 240), error };
  }
  return { error };
}

export function mergeEvents(
  existing: TimelineEvent[],
  incoming: TimelineEvent | TimelineEvent[],
  includeTransient = false,
): TimelineEvent[] {
  const all = [
    ...existing,
    ...(Array.isArray(incoming) ? incoming : [incoming]),
  ];
  const seen = new Set<string>();
  const unique = all.filter((event) => {
    if (
      !includeTransient &&
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
  let sleepPending = false;
  let pendingThought:
    | { row: Extract<TimelineNode, { kind: "work" }>["rows"][number] }
    | undefined;
  const planSteps = new Map<string, Array<Record<string, unknown>>>();
  const latestPlans = new Map<string, Array<Record<string, unknown>>>();
  const planRows = new Map<
    string,
    Extract<TimelineNode, { kind: "work" }>["rows"][number]
  >();
  const planProgressLabels = new Map<string, Set<string>>();
  const approvalNodes = new Map<
    string,
    Extract<TimelineNode, { kind: "approval" }>
  >();
  const pendingTerminal = new Map<
    string,
    { output: string; truncated: boolean; totalBytes?: number }
  >();
  const shellRows = new Map<
    string,
    {
      label: string;
      callId?: string;
      detail?: string;
      terminalOutput?: string;
      terminalTruncated?: boolean;
      terminalTotalBytes?: number;
      shellId?: string;
      processId?: string;
      exitCode?: number;
      durationMs?: number;
      denied?: boolean;
      isMajorAction?: boolean;
      startedAt?: number;
    }
  >();
  const genericRows = new Map<
    string,
    Extract<TimelineNode, { kind: "work" }>["rows"][number]
  >();
  const pendingShellCompletions = new Map<
    string,
    { processId?: string; exitCode?: number; durationMs?: number }
  >();
  const ensureWork = (startedAt: number) => {
    if (!work) {
      work = {
        kind: "work",
        label: "Worked for 0s",
        rows: [],
        additions: 0,
        deletions: 0,
        startedAt,
      };
    }
    if (!workStarted) workStarted = startedAt;
    workEnded = startedAt;
    return work;
  };
  const appendPlanProgress = (
    activeWork: Extract<TimelineNode, { kind: "work" }>,
  ) => {
    const plan = Array.from(latestPlans.values()).at(-1);
    if (!plan?.length) return;
    const completed = plan.filter((step) =>
      ["done", "completed", "failed", "abandoned"].includes(
        String(step.status),
      ),
    ).length;
    const inProgress = plan.findIndex(
      (step) => String(step.status) === "in_progress",
    );
    const nextNotStarted = plan.findIndex(
      (step) => String(step.status) === "not_started",
    );
    const currentIndex =
      inProgress >= 0
        ? inProgress
        : nextNotStarted >= 0
          ? nextNotStarted
          : plan.length - 1;
    const current = plan[currentIndex];
    if (!current) return;
    const label = `${completed}/${plan.length} #${currentIndex + 1} ${String(current.content ?? current.title ?? "")}`;
    if (lastActivityLabel(activeWork.rows) === label) return;
    activeWork.rows.push({
      label,
      activityLabel: true,
      isMajorAction: true,
    });
  };
  const flush = (endedAt = workEnded) => {
    if (!work) return;
    if (pendingThought) {
      pendingThought.row.thoughtForCallId = undefined;
      pendingThought = undefined;
    }
    if (work.rows.length === 0) {
      work = null;
      return;
    }
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
      sleepPending = false;
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
      const text = String(data.message ?? "").trim();
      if (text) {
        nodes.push({
          kind: "assistant",
          text,
          ts: Number.isFinite(Date.parse(String(event.timestamp)))
            ? Date.parse(String(event.timestamp)) / 1000
            : undefined,
        });
      }
      workStarted = 0;
      workEnded = 0;
    } else if (type === "approval_pending" || type === "ask_user_pending") {
      if (type === "ask_user_pending") {
        continue;
      }
      flush(event.created_at_ms);
      const callId = String(data.call_id ?? "");
      const node = {
        kind: "approval",
        callId,
        name: String(data.tool ?? "tool"),
        args: data.arguments,
      } as Extract<TimelineNode, { kind: "approval" }>;
      nodes.push(node);
      if (callId) approvalNodes.set(callId, node);
      workStarted = 0;
      workEnded = 0;
    } else if (type === "approval_resolved") {
      const callId = String(data.call_id ?? "");
      const node = approvalNodes.get(callId);
      if (node) node.resolved = data.approved === true ? "allow" : "deny";
    } else if (type === "user_question_answered") {
      const activeWork = ensureWork(event.created_at_ms);
      activeWork.rows.push({ label: "Answered question" });
    } else if (type === "compacted") {
      const activeWork = ensureWork(event.created_at_ms);
      activeWork.rows.push({ label: "Earlier context compacted" });
    } else if (type === "one_line_thoughts") {
      const activeWork = ensureWork(event.created_at_ms);
      const short = String(data.short ?? data.summary ?? "").trim();
      if (short) {
        activeWork.rows.push({
          label: short,
          activityLabel: true,
          isMajorAction: true,
        });
      }
    } else if (type === "turn_finished") {
      if (
        String(data.run_state ?? "").toLowerCase() === "idle" &&
        String(data.stop_reason ?? "").toLowerCase() === "finished"
      ) {
        flush(event.created_at_ms);
        sleepPending = true;
      }
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
        "turn",
        "stream_reset",
      ].includes(type)
    ) {
      continue;
    } else if (type === "assistant_delta" || type === "reasoning_delta") {
      const activeWork = ensureWork(event.created_at_ms);
      const text = String(
        type === "assistant_delta"
          ? (data.text_delta ?? data.text ?? "")
          : (data.reasoning_delta ?? data.text ?? ""),
      );
      if (text) {
        const label = type === "assistant_delta" ? "Responding" : "Thinking";
        const existing = activeWork.rows.find((row) => row.label === label);
        if (existing) existing.detail = `${existing.detail ?? ""}${text}`;
        else {
          activeWork.rows.push({
            label,
            detail: text,
            activityLabel: true,
            isMajorAction: true,
          });
        }
      }
    } else if (type === "tool_call_delta") {
      const activeWork = ensureWork(event.created_at_ms);
      const delta =
        data.tool_call_delta && typeof data.tool_call_delta === "object"
          ? (data.tool_call_delta as Record<string, unknown>)
          : data;
      const callId = String(delta.id ?? "");
      const name = String(delta.name ?? "Using tool");
      const existing = callId
        ? genericRows.get(callId)
        : activeWork.rows.find((row) => row.label === name);
      if (existing) {
        existing.detail = `${existing.detail ?? ""}${String(delta.arguments_fragment ?? "")}`;
      } else {
        const row = {
          label: name,
          callId: callId || undefined,
          detail: String(delta.arguments_fragment ?? ""),
          isMajorAction: true,
        };
        activeWork.rows.push(row);
        if (callId) genericRows.set(callId, row);
      }
    } else if (type === "tool_result") {
      const resultEvent =
        data.tool_result && typeof data.tool_result === "object"
          ? (data.tool_result as Record<string, unknown>)
          : data;
      const callId = String(
        resultEvent.call_id ??
          resultEvent.tool_use_id ??
          resultEvent.tool_call_id ??
          "",
      );
      const row = callId ? genericRows.get(callId) : undefined;
      if (row) {
        const raw =
          resultEvent.result ??
          resultEvent.output ??
          resultEvent.content ??
          resultEvent.message;
        const result = resultSummary(raw);
        const tool = String(resultEvent.name ?? "");
        if (tool) row.label = toolLabel(tool, resultEvent.arguments);
        row.resultSummary = result.summary;
        row.resultError = row.resultError === true || result.error;
        row.durationMs =
          typeof data.duration_ms === "number"
            ? data.duration_ms
            : row.startedAt === undefined
              ? undefined
              : Math.max(0, event.created_at_ms - row.startedAt);
      }
    } else {
      const activeWork = ensureWork(event.created_at_ms);
      if (type === "devin_thoughts") {
        const duration = Number(data.thinking_duration_ms ?? 0);
        const row = {
          label: `Thought for ${Math.round(duration / 1000)}s`,
          detail: String(data.message ?? ""),
          thoughtForCallId: undefined as string | undefined,
          isMajorAction: true,
        };
        activeWork.rows.push(row);
        pendingThought = { row };
      } else if (type === "shell_process_started") {
        const callId =
          typeof data.call_id === "string" ? data.call_id : undefined;
        const pending = callId ? pendingTerminal.get(callId) : undefined;
        const completion = callId
          ? pendingShellCompletions.get(callId)
          : undefined;
        const row = {
          label: String(data.command ?? ""),
          callId,
          detail: undefined as string | undefined,
          shellId:
            typeof data.shell_id === "string" ? data.shell_id : undefined,
          isMajorAction: data.is_major_action === false ? false : true,
          startedAt: event.created_at_ms,
          terminalOutput: pending?.output || undefined,
          terminalTruncated: pending?.truncated || undefined,
          terminalTotalBytes: pending?.totalBytes,
          ...completion,
        };
        if (pendingThought) {
          pendingThought.row.thoughtForCallId = callId;
          pendingThought = undefined;
        }
        appendPlanProgress(activeWork);
        activeWork.rows.push(row);
        if (callId) {
          shellRows.set(callId, row);
          pendingTerminal.delete(callId);
          pendingShellCompletions.delete(callId);
        }
      } else if (type === "shell_process_completed") {
        const callId =
          typeof data.process_id === "string" ? data.process_id : undefined;
        const exitCode =
          typeof data.exit_code === "number" ? data.exit_code : undefined;
        const durationMs =
          typeof data.duration_ms === "number" ? data.duration_ms : undefined;
        const row = callId ? shellRows.get(callId) : undefined;
        if (row) {
          row.processId = callId;
          row.exitCode = exitCode;
          row.durationMs =
            durationMs ??
            (row.startedAt === undefined
              ? undefined
              : Math.max(0, event.created_at_ms - row.startedAt));
        } else if (callId) {
          pendingShellCompletions.set(callId, {
            processId: callId,
            exitCode,
            durationMs,
          });
        }
      } else if (type === "tool_call_denied") {
        const callId =
          typeof data.call_id === "string" ? data.call_id : undefined;
        const shellRow = callId ? shellRows.get(callId) : undefined;
        if (shellRow) {
          shellRow.denied = true;
          shellRow.detail = String(data.reason ?? "Tool call was not run");
        } else {
          activeWork.rows.push({
            label: `Not run: ${String(data.tool ?? "tool")}`,
            detail: String(data.reason ?? "Tool call was not run"),
            callId,
            isMajorAction: true,
            denied: true,
          });
        }
      } else if (type === "terminal_update") {
        const callId = String(data.call_id ?? "");
        if (!callId) continue;
        const contents = String(data.contents ?? "");
        const truncated = data.truncated === true;
        const totalBytes =
          typeof data.total_bytes === "number" ? data.total_bytes : undefined;
        const row = shellRows.get(callId);
        if (row) {
          row.terminalOutput = `${row.terminalOutput ?? ""}${contents}`;
          if (truncated) row.terminalTruncated = true;
          if (totalBytes !== undefined) row.terminalTotalBytes = totalBytes;
        } else {
          const pending = pendingTerminal.get(callId) ?? {
            output: "",
            truncated: false,
          };
          pending.output += contents;
          pending.truncated ||= truncated;
          if (totalBytes !== undefined) pending.totalBytes = totalBytes;
          pendingTerminal.set(callId, pending);
        }
      } else if (type === "multi_edit_result") {
        const updates = Array.isArray(data.file_updates)
          ? data.file_updates
          : [];
        for (const update of updates) {
          if (!update || typeof update !== "object") continue;
          const item = update as Record<string, unknown>;
          const added = Number(item.lines_added ?? 0);
          const removed = Number(item.lines_removed ?? 0);
          activeWork.additions += added;
          activeWork.deletions += removed;
          const basename =
            String(item.file_path ?? "")
              .split(/[\\/]/)
              .pop() ?? "";
          appendPlanProgress(activeWork);
          activeWork.rows.push({
            callId: typeof data.call_id === "string" ? data.call_id : undefined,
            label:
              item.action_type === "create"
                ? `Created ${basename} +${added}`
                : `Edited ${basename} +${added} −${removed}`,
            isMajorAction: true,
            artifactId:
              typeof item.artifact_id === "string"
                ? item.artifact_id
                : undefined,
            artifactKind:
              typeof item.artifact_id === "string" ? "diff" : undefined,
            artifactMime:
              typeof item.artifact_id === "string" ? "text/x-diff" : undefined,
          });
          if (pendingThought) {
            pendingThought.row.thoughtForCallId =
              typeof data.call_id === "string" ? data.call_id : undefined;
          }
          pendingThought = undefined;
        }
      } else if (type === "computer_use") {
        const keys = Array.isArray(data.screenshot_keys)
          ? data.screenshot_keys.filter(
              (key): key is string => typeof key === "string",
            )
          : [];
        keys.forEach((artifactId) => {
          activeWork.rows.push({
            label: "Screenshot",
            artifactId,
            artifactKind: "screenshot",
            artifactMime: "image/png",
          });
        });
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
        if (!previousTodos || todos.length > previousTodos.length) {
          activeWork.rows.push({
            label: `Created ${todos.length} Tasks`,
            plan: {
              steps: todos.map((todo) => ({
                content: String(todo.content ?? todo.title ?? ""),
                status: String(todo.status ?? "not_started"),
              })),
            },
          });
          planRows.set(planId, activeWork.rows.at(-1)!);
        } else {
          const planRow = planRows.get(planId);
          if (planRow?.plan) {
            planRow.plan.steps = todos.map((todo) => ({
              content: String(todo.content ?? todo.title ?? ""),
              status: String(todo.status ?? "not_started"),
            }));
          }
          if (previousTodos) {
            const progressLabels =
              planProgressLabels.get(planId) ?? new Set<string>();
            planProgressLabels.set(planId, progressLabels);
            todos.forEach((item, index) => {
              const previous = previousTodos[index];
              if (
                previous &&
                previous.step_id === item.step_id &&
                previous.status === item.status &&
                String(previous.content ?? previous.description ?? "") ===
                  String(item.content ?? item.description ?? "")
              )
                return;
              const label = `${completed}/${todos.length} #${index + 1} ${String(item.content ?? item.title ?? "")}`;
              if (!progressLabels.has(label)) {
                activeWork.rows.push({ label });
                progressLabels.add(label);
              }
            });
          }
        }
        planSteps.set(planId, todos);
        latestPlans.set(planId, todos);
      } else if (
        type === "read_file_completed" ||
        type === "list_dir_completed"
      ) {
        const callId =
          typeof data.call_id === "string" ? data.call_id : undefined;
        const started = callId ? genericRows.get(callId) : undefined;
        if (started) {
          if (data.ok === false) {
            started.resultError = true;
            started.resultSummary ??= "Failed";
          }
          started.durationMs =
            typeof data.duration_ms === "number"
              ? data.duration_ms
              : started.startedAt === undefined
                ? undefined
                : Math.max(0, event.created_at_ms - started.startedAt);
          continue;
        }
        const target = String(data.path ?? data.target ?? data.file_path ?? "");
        appendPlanProgress(activeWork);
        activeWork.rows.push({
          label: target
            ? `${type.startsWith("read_file") ? "Read" : "Listed"} ${target}`
            : type.startsWith("read_file")
              ? "Read file"
              : "Listed directory",
          isMajorAction: true,
        });
      } else if (type.endsWith("_completed")) {
        const callId =
          typeof data.call_id === "string" ? data.call_id : undefined;
        const started = callId ? genericRows.get(callId) : undefined;
        if (started) {
          if (data.ok === false) {
            started.resultError = true;
            started.resultSummary ??= "Failed";
          }
          started.durationMs =
            typeof data.duration_ms === "number"
              ? data.duration_ms
              : started.startedAt === undefined
                ? undefined
                : Math.max(0, event.created_at_ms - started.startedAt);
        }
      } else if (type.endsWith("_started")) {
        if (
          ![
            "propose_plan_started",
            "plan_update_started",
            "plan_get_started",
            "plan_revise_started",
          ].includes(type)
        ) {
          const tool = String(data.tool ?? type.replace(/_started$/, ""));
          const row = {
            label: toolLabel(tool, data.arguments),
            callId: typeof data.call_id === "string" ? data.call_id : undefined,
            startedAt: event.created_at_ms,
            isMajorAction:
              typeof data.is_major_action === "boolean"
                ? data.is_major_action
                : true,
          };
          if (pendingThought) {
            pendingThought.row.thoughtForCallId =
              typeof data.call_id === "string" ? data.call_id : undefined;
            pendingThought = undefined;
          }
          appendPlanProgress(activeWork);
          activeWork.rows.push(row);
          if (row.callId) genericRows.set(row.callId, row);
        }
      }
    }
  }
  flush(workEnded || events.at(-1)?.created_at_ms || workStarted);
  if (sleepPending) nodes.push({ kind: "sleep", text: "OPCOS went to sleep" });
  return nodes;
}

export function latestPlan(
  events: TimelineEvent[],
): Array<{ content: string; status?: string }> | null {
  const plans = buildTimeline(events)
    .flatMap((node) => (node.kind === "work" ? node.rows : []))
    .map((row) => row.plan?.steps)
    .filter((steps): steps is Array<{ content: string; status?: string }> =>
      Boolean(steps),
    );
  return plans.at(-1) ?? null;
}
