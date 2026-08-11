import { describe, expect, it } from "vitest";
import liveEnvelopes from "../../fixtures/timeline/live-events.json";
import opcosEvents from "../../fixtures/timeline/opcos-events.json";
import opcosTodoCapture from "../../fixtures/timeline/opcos-todo-capture.json";
import planIterations from "../../fixtures/timeline/opcos-plan-iterations.json";
import terminalReplay from "../../fixtures/timeline/opcos-terminal-replay.json";
import parityEvents from "../../fixtures/timeline/opcos-devin-parity.json";
import toolCallOnlyIteration from "../../fixtures/timeline/tool-call-only-iteration.json";
import persisted from "../../fixtures/timeline/persisted-events.json";
import {
  buildTimeline,
  latestPlan,
  mergeEvents,
  optimisticUserMessageEvent,
  OPTIMISTIC_USER_EVENT_PREFIX,
  TRANSIENT_TIMELINE_EVENT_TYPES,
  lastActivityLabel,
  type TimelineEvent,
} from "./timeline";

const live = liveEnvelopes.map((entry) => entry.payload as TimelineEvent);
const saved = persisted as TimelineEvent[];
const opcos = opcosEvents as TimelineEvent[];

describe("single event-log timeline", () => {
  it("replaces an optimistic user message with the persisted event", () => {
    const optimistic = optimisticUserMessageEvent("session-1", "  Hello  ");
    const persisted: TimelineEvent = {
      type: "user_message",
      event_id: "persisted-user",
      created_at_ms: optimistic.created_at_ms + 1,
      session_id: "session-1",
      working_event: {
        event_type: "user_message",
        payload: { message: "Hello" },
      },
    };
    expect(optimistic.event_id).toContain(OPTIMISTIC_USER_EVENT_PREFIX);
    expect(mergeEvents([optimistic], persisted, true)).toEqual([persisted]);
  });

  it("does not replace an optimistic message from another session", () => {
    const optimistic = optimisticUserMessageEvent("session-1", "Hello");
    const persisted: TimelineEvent = {
      type: "user_message",
      event_id: "other-session-user",
      created_at_ms: optimistic.created_at_ms + 1,
      session_id: "session-2",
      working_event: {
        event_type: "user_message",
        payload: { message: "Hello" },
      },
    };
    expect(mergeEvents([optimistic], persisted, true)).toEqual([
      optimistic,
      persisted,
    ]);
  });

  it("retains an optimistic message when the persisted text differs", () => {
    const optimistic = optimisticUserMessageEvent("session-1", "Hello");
    const persisted: TimelineEvent = {
      type: "user_message",
      event_id: "different-text-user",
      created_at_ms: optimistic.created_at_ms + 1,
      session_id: "session-1",
      working_event: {
        event_type: "user_message",
        payload: { message: "Goodbye" },
      },
    };
    expect(mergeEvents([optimistic], persisted, true)).toEqual([
      optimistic,
      persisted,
    ]);
  });

  it("replaces an optimistic message with an initial user event", () => {
    const optimistic = optimisticUserMessageEvent("session-1", "Hello");
    const persisted: TimelineEvent = {
      type: "initial_user_message",
      event_id: "initial-user",
      created_at_ms: optimistic.created_at_ms + 1,
      session_id: "session-1",
      message: " Hello ",
    };
    expect(mergeEvents([optimistic], persisted, true)).toEqual([persisted]);
  });

  it("keeps a newer optimistic duplicate until its matching event arrives", () => {
    const optimistic = optimisticUserMessageEvent("session-1", "continue");
    const previous: TimelineEvent = {
      type: "user_message",
      event_id: "previous-user",
      created_at_ms: optimistic.created_at_ms - 1,
      session_id: "session-1",
      message: " continue ",
    };
    expect(mergeEvents([previous, optimistic], [], true)).toEqual([
      previous,
      optimistic,
    ]);
  });

  it("shows one live tail action while a turn is running", () => {
    const nodes = buildTimeline(
      [
        {
          type: "one_line_thoughts",
          event_id: "thought",
          created_at_ms: 1,
          short: "Designing recovery",
        },
      ] as TimelineEvent[],
      true,
    );
    expect(nodes.at(-1)).toEqual({
      kind: "tail_status",
      text: "OPCOS: Designing recovery",
    });
    expect(nodes.filter((node) => node.kind === "tail_status")).toHaveLength(1);
  });

  it("updates and clears a persisted provider wait row when streaming resumes", () => {
    const nodes = buildTimeline([
      {
        type: "provider_waiting",
        event_id: "provider-wait-1",
        created_at_ms: 3000,
        working_event: {
          event_type: "provider_waiting",
          payload: {
            elapsed_seconds: 3,
            message: "Waiting for provider response (3s)",
          },
        },
      },
      {
        type: "provider_waiting",
        event_id: "provider-wait-2",
        created_at_ms: 8000,
        working_event: {
          event_type: "provider_waiting",
          payload: {
            elapsed_seconds: 8,
            message: "Waiting for provider response (8s)",
          },
        },
      },
      {
        type: "provider_waiting_cleared",
        event_id: "provider-wait-cleared",
        created_at_ms: 9000,
        working_event: {
          event_type: "provider_waiting_cleared",
          payload: { message: "Provider response resumed" },
        },
      },
    ] as TimelineEvent[]);

    const rows = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows);
    expect(rows.filter((row) => row.providerWaiting)).toHaveLength(0);
  });

  it("replays captured tool results and terminal transitions", () => {
    const nodes = buildTimeline(live);
    const resultEvent = live.find((event) => event.type === "tool_result");
    const resultPayload = resultEvent?.tool_result as
      { call_id?: string; result?: unknown } | undefined;
    const resultCallId = String(resultPayload?.call_id);
    const resultRow = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows)
      .find((row) => row.callId === resultCallId);
    expect(resultPayload).toMatchObject({
      call_id: resultCallId,
      result: expect.anything(),
    });
    expect(resultRow).toMatchObject({
      callId: resultCallId,
      resultSummary: undefined,
    });
    expect(nodes).toContainEqual({
      kind: "tail_status",
      text: "Turn complete — waiting for your next instruction",
    });
    expect(buildTimeline(saved)).toEqual(nodes);
    const resumed = buildTimeline([
      ...live,
      {
        type: "user_message",
        event_id: "after-finished",
        created_at_ms: 1786040275969,
        message: "Continue",
      },
    ] as TimelineEvent[]);
    expect(resumed.some((node) => node.kind === "tail_status")).toBe(false);
  });

  it("derives the current task state from the timeline plan", () => {
    const events = [
      {
        type: "todo_update",
        event_id: "plan-start",
        created_at_ms: 1,
        working_event: {
          event_type: "todo_update",
          payload: {
            plan_id: "plan",
            steps: [
              { step_id: "one", content: "Inspect", status: "not_started" },
              { step_id: "two", content: "Fix", status: "not_started" },
            ],
          },
        },
      },
      {
        type: "todo_update",
        event_id: "plan-progress",
        created_at_ms: 2,
        working_event: {
          event_type: "todo_update",
          payload: {
            plan_id: "plan",
            steps: [
              { step_id: "one", content: "Inspect", status: "completed" },
              { step_id: "two", content: "Fix", status: "in_progress" },
            ],
          },
        },
      },
    ] as TimelineEvent[];
    expect(latestPlan(events)).toEqual([
      { content: "Inspect", status: "completed" },
      { content: "Fix", status: "in_progress" },
    ]);
  });
  it("renders the terminal replay fixture under one shell row", () => {
    const rows = buildTimeline(terminalReplay as TimelineEvent[])
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows);
    expect(rows).toContainEqual(
      expect.objectContaining({
        label: "cargo test",
        terminalOutput: "first\nsecond\n",
        terminalTruncated: true,
        terminalTotalBytes: 100,
      }),
    );
    expect(rows).toContainEqual(
      expect.objectContaining({
        label: "true",
        callId: "call-terminal-empty",
        terminalOutput: undefined,
        terminalTruncated: undefined,
        terminalTotalBytes: undefined,
      }),
    );
  });

  it("renders live and reloaded events identically", () => {
    expect(buildTimeline(live)).toEqual(buildTimeline(saved));
  });
  it("uses Devin-style group labels, attached thoughts, and shell results", () => {
    const nodes = buildTimeline(parityEvents as TimelineEvent[]);
    const work = nodes.find((node) => node.kind === "work");
    expect(work).toMatchObject({ label: "Worked for 2s" });
    expect(work?.rows).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          label: "Listing files",
          activityLabel: true,
        }),
        expect.objectContaining({
          label: "Thought for 1s",
          thoughtForCallId: "call-shell",
        }),
        expect.objectContaining({
          label: "cargo test",
          shellId: "shell-session",
          processId: "call-shell",
          exitCode: 0,
          durationMs: 500,
          isMajorAction: true,
        }),
      ]),
    );
    expect(work?.rows.some((row) => row.isMajorAction === false)).toBe(true);
  });
  it("keeps every one-line label without replacing the finished group header", () => {
    const nodes = buildTimeline([
      {
        type: "one_line_thoughts",
        event_id: "label-1",
        created_at_ms: 1000,
        working_event: {
          event_type: "one_line_thoughts",
          payload: { short: "Listing files" },
        },
      },
      {
        type: "browser_status_started",
        event_id: "minor-1",
        created_at_ms: 1100,
        working_event: {
          event_type: "browser_status_started",
          payload: { tool: "browser_status", is_major_action: false },
        },
      },
      {
        type: "one_line_thoughts",
        event_id: "label-2",
        created_at_ms: 2000,
        working_event: {
          event_type: "one_line_thoughts",
          payload: { short: "Running tests" },
        },
      },
      {
        type: "shell_process_started",
        event_id: "shell-label",
        created_at_ms: 3000,
        working_event: {
          event_type: "shell_process_started",
          payload: { call_id: "label-call", command: "cargo test" },
        },
      },
    ] as TimelineEvent[]);
    const work = nodes.find((node) => node.kind === "work");
    expect(work?.label).toBe("Worked for 2s");
    expect(work?.rows.map((row) => row.label)).toEqual([
      "Listing files",
      "Checked browser status",
      "Running tests",
      "cargo test",
    ]);
  });
  it("defaults legacy generic actions to major", () => {
    const [work] = buildTimeline([
      {
        type: "browser_status_started",
        event_id: "legacy-generic",
        created_at_ms: 1,
        working_event: {
          event_type: "browser_status_started",
          payload: { tool: "browser_status" },
        },
      },
    ] as TimelineEvent[]).filter((node) => node.kind === "work");
    expect(work?.rows[0]).toMatchObject({
      label: "Checked browser status",
      isMajorAction: true,
    });
  });
  it("shows the human-readable summary for structured tool errors", () => {
    const nodes = buildTimeline([
      {
        type: "edit_file_started",
        event_id: "structured-error-start",
        created_at_ms: 1,
        working_event: {
          event_type: "edit_file_started",
          payload: { call_id: "structured-error-call", tool: "edit_file" },
        },
      },
      {
        type: "tool_result",
        event_id: "structured-error-result",
        created_at_ms: 2,
        tool_result: {
          call_id: "structured-error-call",
          name: "edit_file",
          result: {
            error: "edit 0 old_string was not found",
            error_details: {
              code: "edit_anchor_not_found",
              invariant: "each edit anchor must occur exactly once",
              target: "src/lib.rs",
              repair:
                "read the file again and retry with an exact, longer anchor",
              retry: "adjusted",
            },
          },
        },
      },
    ] as TimelineEvent[]);
    const row = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows)
      .find((candidate) => candidate.callId === "structured-error-call");
    expect(row).toMatchObject({
      resultError: true,
      resultSummary: "edit 0 old_string was not found",
    });
    expect(JSON.stringify(row)).not.toContain("edit_anchor_not_found");
  });
  it("marks resolved approvals and denied calls without process completion", () => {
    const nodes = buildTimeline([
      {
        type: "approval_pending",
        event_id: "approval",
        created_at_ms: 1,
        working_event: {
          event_type: "approval_pending",
          payload: {
            call_id: "shell-denied",
            tool: "run_shell",
            arguments: { command: "rm -rf output" },
          },
        },
      },
      {
        type: "approval_resolved",
        event_id: "resolved",
        created_at_ms: 2,
        working_event: {
          event_type: "approval_resolved",
          payload: { call_id: "shell-denied", approved: false },
        },
      },
      {
        type: "tool_call_denied",
        event_id: "denied",
        created_at_ms: 3,
        working_event: {
          event_type: "tool_call_denied",
          payload: {
            call_id: "queued-shell",
            tool: "run_shell",
            reason: "canceled after approval denial",
          },
        },
      },
    ] as TimelineEvent[]);
    expect(nodes[0]).toMatchObject({
      kind: "approval",
      callId: "shell-denied",
      resolved: "deny",
    });
    const denied = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows)
      .find((row) => row.callId === "queued-shell");
    expect(denied).toMatchObject({
      label: "Not run: run_shell",
      denied: true,
    });
  });
  it("keeps pending questions out of the worklog and records their answer", () => {
    const nodes = buildTimeline([
      {
        type: "ask_user_pending",
        event_id: "question",
        created_at_ms: 1,
        working_event: {
          event_type: "ask_user_pending",
          payload: { call_id: "question-1", question: "Which option?" },
        },
      },
      {
        type: "user_question_answered",
        event_id: "answer",
        created_at_ms: 2,
        working_event: {
          event_type: "user_question_answered",
          payload: { call_id: "question-1", answer_type: "text" },
        },
      },
    ] as TimelineEvent[]);
    expect(nodes.some((node) => node.kind === "question")).toBe(false);
    expect(nodes).toContainEqual(
      expect.objectContaining({
        kind: "work",
        rows: [expect.objectContaining({ label: "Answered question" })],
      }),
    );
  });
  it("uses the persisted approval resolution field", () => {
    const nodes = buildTimeline([
      {
        type: "approval_pending",
        event_id: "approval",
        created_at_ms: 1,
        working_event: {
          event_type: "approval_pending",
          payload: {
            call_id: "approval-1",
            tool: "run_shell",
            arguments: { command: "echo ok" },
          },
        },
      },
      {
        type: "approval_resolved",
        event_id: "resolved",
        created_at_ms: 2,
        working_event: {
          event_type: "approval_resolved",
          payload: { call_id: "approval-1", approved: true },
        },
      },
    ] as TimelineEvent[]);
    expect(nodes[0]).toMatchObject({
      kind: "approval",
      resolved: "allow",
    });
  });
  it("renders in-flight deltas without making them reload history", () => {
    const live = mergeEvents(
      [],
      {
        type: "assistant_delta",
        event_id: "delta-1",
        created_at_ms: 1,
        working_event: {
          event_type: "assistant_delta",
          payload: { text_delta: "still " },
        },
      } as TimelineEvent,
      true,
    );
    const complete = mergeEvents(
      live,
      {
        type: "assistant_delta",
        event_id: "delta-2",
        created_at_ms: 2,
        working_event: {
          event_type: "assistant_delta",
          payload: { text_delta: "working" },
        },
      } as TimelineEvent,
      true,
    );
    expect(buildTimeline(complete)).toContainEqual(
      expect.objectContaining({
        kind: "work",
        rows: [
          expect.objectContaining({
            label: "Responding",
            detail: "still working",
          }),
        ],
      }),
    );
    expect(mergeEvents([], complete)).toEqual([]);
  });
  it("keeps legacy events without parity fields renderable", () => {
    const nodes = buildTimeline([
      {
        type: "shell_process_started",
        event_id: "legacy-start",
        created_at_ms: 1,
        working_event: {
          event_type: "shell_process_started",
          payload: { call_id: "legacy-call", command: "echo legacy" },
        },
      },
      {
        type: "shell_process_completed",
        event_id: "legacy-end",
        created_at_ms: 4,
        working_event: {
          event_type: "shell_process_completed",
          payload: { process_id: "legacy-call", exit_code: 0 },
        },
      },
    ] as TimelineEvent[]);
    expect(nodes).toContainEqual(
      expect.objectContaining({
        kind: "work",
        rows: [
          expect.objectContaining({
            label: "echo legacy",
            exitCode: 0,
            durationMs: 3,
          }),
        ],
      }),
    );
  });
  it("keeps thoughts visible when the next action starts a later work group", () => {
    const nodes = buildTimeline([
      {
        type: "devin_thoughts",
        event_id: "thought-before-message",
        created_at_ms: 1000,
        working_event: {
          event_type: "devin_thoughts",
          payload: {
            message: "I should inspect the repository first.",
            thinking_duration_ms: 3000,
          },
        },
      },
      {
        type: "devin_message",
        event_id: "message-between-groups",
        created_at_ms: 1100,
        working_event: {
          event_type: "devin_message",
          payload: { message: "I will inspect the repository." },
        },
      },
      {
        type: "shell_process_started",
        event_id: "action-later",
        created_at_ms: 1200,
        working_event: {
          event_type: "shell_process_started",
          payload: { call_id: "later-call", command: "ls" },
        },
      },
    ] as TimelineEvent[]);
    const thoughtRows = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) =>
        node.rows.filter((row) => row.label.startsWith("Thought for ")),
      );
    expect(thoughtRows).toHaveLength(1);
    expect(thoughtRows[0].thoughtForCallId).toBeUndefined();
    expect(
      nodes
        .filter((node) => node.kind === "work")
        .flatMap((node) => node.rows)
        .filter((row) => row.label.startsWith("Thought for ")),
    ).toHaveLength(1);
  });
  it("deduplicates replayed chunks across arbitrary reconnect boundaries", () => {
    let events: TimelineEvent[] = [];
    for (let i = 0; i < live.length; i += 3) {
      events = mergeEvents(events, live.slice(i, i + 3));
      events = mergeEvents(events, live.slice(i, i + 3));
    }
    expect(buildTimeline(events)).toEqual(buildTimeline(live));
  });
  it("matches bulk construction when delivered one event at a time", () => {
    let events: TimelineEvent[] = [];
    for (const event of live) events = mergeEvents(events, event);
    expect(buildTimeline(events)).toEqual(buildTimeline(live));
  });
  it("produces the same timeline when token deltas are omitted from persistence", () => {
    const persistedWithoutDeltas = live.filter(
      (event) =>
        !TRANSIENT_TIMELINE_EVENT_TYPES.includes(
          event.type as (typeof TRANSIENT_TIMELINE_EVENT_TYPES)[number],
        ),
    );
    expect(buildTimeline(mergeEvents([], live))).toEqual(
      buildTimeline(persistedWithoutDeltas),
    );
    expect(
      mergeEvents([], live).some((event) =>
        TRANSIENT_TIMELINE_EVENT_TYPES.includes(
          event.type as (typeof TRANSIENT_TIMELINE_EVENT_TYPES)[number],
        ),
      ),
    ).toBe(false);
  });
  it("shows steering receipt and application in work activity", () => {
    const nodes = buildTimeline([
      {
        type: "steering_received",
        event_id: "steering-received",
        created_at_ms: 10,
        working_event: {
          event_type: "steering_received",
          payload: { queued: true },
        },
      },
      {
        type: "steering_applied",
        event_id: "steering-applied",
        created_at_ms: 20,
        working_event: {
          event_type: "steering_applied",
          payload: { iteration: 2, count: 2 },
        },
      },
    ]);
    const rows = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows);
    expect(rows).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ label: "Steering received" }),
        expect.objectContaining({
          label: "Steering applied",
          detail: "Before iteration 2",
        }),
      ]),
    );
  });

  it("uses Devin wording for fixture-derived rows", () => {
    const nodes = buildTimeline(live);
    const work = nodes.filter((node) => node.kind === "work");
    const rows = work.flatMap((node) => node.rows.map((row) => row.label));
    expect(rows.some((label) => /^Thought for \d+s$/.test(label))).toBe(true);
    expect(rows).toContain("Edited alpha.txt");
    expect(rows).toContain("Created notes.md");
    expect(
      work
        .flatMap((node) => node.rows)
        .find((row) => row.label === "Edited alpha.txt"),
    ).toMatchObject({ additions: 1, deletions: undefined });
    expect(
      work
        .flatMap((node) => node.rows)
        .find((row) => row.label === "Created notes.md"),
    ).toMatchObject({ additions: 33, deletions: undefined });
    expect(rows).toContain("Wrote <temp-workspace>/notes.md");
    expect(rows).toContain("Edited <temp-workspace>/alpha.txt");
    const failedRead = work
      .flatMap((node) => node.rows)
      .find((row) => row.resultError);
    expect(failedRead).toMatchObject({
      label: "Read <temp-workspace>/session.sqlite",
      resultSummary: expect.stringContaining("valid UTF-8"),
    });
    expect(work.some((node) => /^Worked for \d+s$/.test(node.label))).toBe(
      true,
    );
    expect(work.reduce((sum, node) => sum + node.additions, 0)).toBe(34);
    expect(work.reduce((sum, node) => sum + node.deletions, 0)).toBe(0);
  });
  it("renders OPCOS-native persisted events as one work group", () => {
    const nodes = buildTimeline(opcos);
    expect(nodes).toContainEqual({
      kind: "user",
      text: "Implement the change.",
      ts: 1767225601,
      attachments: undefined,
    });
    expect(nodes).toContainEqual({
      kind: "assistant",
      text: "The requested change is complete.",
      ts: 1767225608,
    });
    const work = nodes.find((node) => node.kind === "work");
    expect(work).toMatchObject({
      label: "Worked for 6s",
      additions: 6,
      deletions: 1,
    });
    expect(work?.rows.map((row) => row.label)).toEqual([
      "Thought for 5s",
      "cargo test",
      "Created notes.md",
      "Edited lib.rs",
      "Created 4 Tasks",
      "1/4 #1 Implement the change",
      "Earlier context compacted",
    ]);
  });
  it("aggregates terminal chunks under their shell row and marks truncation", () => {
    const nodes = buildTimeline([
      {
        type: "shell_process_started",
        event_id: "shell",
        created_at_ms: 1,
        working_event: {
          event_type: "shell_process_started",
          payload: { call_id: "call-1", command: "cargo test" },
        },
      },
      {
        type: "terminal_update",
        event_id: "chunk-1",
        created_at_ms: 2,
        working_event: {
          event_type: "terminal_update",
          payload: { call_id: "call-1", contents: "first\n" },
        },
      },
      {
        type: "terminal_update",
        event_id: "chunk-2",
        created_at_ms: 2,
        working_event: {
          event_type: "terminal_update",
          payload: { call_id: "call-1", contents: "second\n" },
        },
      },
      {
        type: "shell_process_started",
        event_id: "shell-empty",
        created_at_ms: 3,
        working_event: {
          event_type: "shell_process_started",
          payload: { call_id: "call-empty", command: "true" },
        },
      },
      {
        type: "terminal_update",
        event_id: "chunk-3",
        created_at_ms: 4,
        working_event: {
          event_type: "terminal_update",
          payload: {
            call_id: "call-1",
            contents: "",
            truncated: true,
            total_bytes: 100,
          },
        },
      },
    ] as TimelineEvent[]);
    const rows = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows);
    expect(rows).toContainEqual(
      expect.objectContaining({
        label: "cargo test",
        callId: "call-1",
        terminalOutput: "first\nsecond\n",
        terminalTruncated: true,
        terminalTotalBytes: 100,
      }),
    );
    expect(rows).toContainEqual(
      expect.objectContaining({
        label: "true",
        callId: "call-empty",
        terminalOutput: undefined,
        terminalTruncated: undefined,
        terminalTotalBytes: undefined,
      }),
    );
  });
  it("keeps file change counts on file update rows", () => {
    const rows = buildTimeline([
      {
        type: "write_file_started",
        event_id: "write-started",
        created_at_ms: 1,
        working_event: {
          event_type: "write_file_started",
          payload: {
            call_id: "write-call",
            tool: "write_file",
            arguments: { path: "src/new.ts" },
          },
        },
      },
      {
        type: "multi_edit_result",
        event_id: "write-update",
        created_at_ms: 2,
        working_event: {
          event_type: "multi_edit_result",
          payload: {
            call_id: "write-call",
            file_updates: [
              {
                file_path: "src/new.ts",
                action_type: "create",
                lines_added: 4,
                lines_removed: 0,
              },
            ],
          },
        },
      },
      {
        type: "edit_file_started",
        event_id: "edit-started",
        created_at_ms: 3,
        working_event: {
          event_type: "edit_file_started",
          payload: {
            call_id: "edit-call",
            tool: "edit_file",
            arguments: { path: "src/existing.ts" },
          },
        },
      },
      {
        type: "multi_edit_result",
        event_id: "edit-update",
        created_at_ms: 4,
        working_event: {
          event_type: "multi_edit_result",
          payload: {
            call_id: "edit-call",
            file_updates: [
              {
                file_path: "src/existing.ts",
                action_type: "edit",
                lines_added: 2,
                lines_removed: 1,
              },
            ],
          },
        },
      },
    ] as TimelineEvent[])
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows);

    expect(rows).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          label: "Wrote src/new.ts",
        }),
        expect.objectContaining({
          label: "Edited src/existing.ts",
        }),
        expect.objectContaining({
          label: "Created new.ts",
          additions: 4,
          deletions: undefined,
        }),
        expect.objectContaining({
          label: "Edited existing.ts",
          additions: 2,
          deletions: 1,
        }),
      ]),
    );
  });
  it("renders terminal chunks that arrive before a shell row without a sequence field", () => {
    const nodes = buildTimeline([
      {
        type: "terminal_update",
        event_id: "chunk",
        created_at_ms: 1,
        working_event: {
          event_type: "terminal_update",
          payload: { call_id: "call-1", contents: "legacy output" },
        },
      },
      {
        type: "shell_process_started",
        event_id: "shell",
        created_at_ms: 2,
        working_event: {
          event_type: "shell_process_started",
          payload: { call_id: "call-1", command: "echo legacy" },
        },
      },
    ] as TimelineEvent[]);
    expect(nodes).toContainEqual(
      expect.objectContaining({
        kind: "work",
        rows: [
          expect.objectContaining({
            label: "echo legacy",
            terminalOutput: "legacy output",
          }),
        ],
      }),
    );
  });
  it("skips legacy bare working-event rows without a resolvable type", () => {
    expect(
      buildTimeline([
        {
          event_type: "shell_process_started",
          category: "tool",
          direction: "outgoing",
          timestamp: "2026-01-01T00:00:01Z",
          payload: { command: "legacy command" },
        } as unknown as TimelineEvent,
        ...opcos,
      ]),
    ).not.toContainEqual(
      expect.objectContaining({ kind: "work", label: "Worked for 0s" }),
    );
  });
  it("keeps plan progress across iteration messages", () => {
    const nodes = buildTimeline(planIterations as TimelineEvent[]);
    const rows = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows.map((row) => row.label));
    expect(rows.filter((label) => label === "Created 5 Tasks")).toHaveLength(1);
    expect(rows).toContain("1/5 #1 Create files");
    expect(rows).toContain("2/5 #2 Run tests");
    const planRow = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows)
      .find((row) => row.label === "Created 5 Tasks");
    expect(planRow?.plan?.steps).toHaveLength(5);
  });
  it("deduplicates repeated visible plan progress from a real session capture", () => {
    const rows = buildTimeline(opcosTodoCapture as TimelineEvent[])
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows.map((row) => row.label))
      .filter((label) => label.includes("#4"));
    expect(rows).toEqual([
      "3/4 #4 4. Finish by reporting every command run, each exit code, and whether the workspace changed (it should not).",
      "4/4 #4 4. Finish by reporting every command run, each exit code, and whether the workspace changed (it should not).",
    ]);
  });
  it("collapses identical progress separated by interleaved tool rows", () => {
    const rows = [
      { label: "0/4 #1 Implement", activityLabel: true },
      { label: "Read checkoutValidation.ts" },
      { label: "0/4 #1 Implement", activityLabel: true },
    ];
    expect(lastActivityLabel(rows)).toBe("0/4 #1 Implement");
  });
  it("points plan progress at the next unfinished step", () => {
    const rowsFor = (steps: Record<string, unknown>[]) =>
      buildTimeline([
        {
          type: "todo_update",
          event_id: "plan-progress",
          created_at_ms: 1,
          steps,
        },
        {
          type: "shell_process_started",
          event_id: "plan-action",
          created_at_ms: 2,
          command: "echo work",
          call_id: "plan-action-call",
        },
      ]).flatMap((node) =>
        node.kind === "work" ? node.rows.map((row) => row.label) : [],
      );

    expect(
      rowsFor([
        { step_id: "1", description: "First", status: "not_started" },
        { step_id: "2", description: "Second", status: "not_started" },
        { step_id: "3", description: "Third", status: "not_started" },
        { step_id: "4", description: "Fourth", status: "not_started" },
        { step_id: "5", description: "Fifth", status: "not_started" },
        { step_id: "6", description: "Sixth", status: "not_started" },
        { step_id: "7", description: "Seventh", status: "not_started" },
        { step_id: "8", description: "Eighth", status: "not_started" },
      ]),
    ).toContain("0/8 #1 First");
    expect(
      rowsFor([
        { step_id: "1", description: "First", status: "done" },
        { step_id: "2", description: "Second", status: "in_progress" },
        { step_id: "3", description: "Third", status: "not_started" },
      ]),
    ).toContain("1/3 #2 Second");
    expect(
      rowsFor([
        { step_id: "1", description: "First", status: "done" },
        { step_id: "2", description: "Second", status: "failed" },
        { step_id: "3", description: "Third", status: "abandoned" },
      ]),
    ).toContain("3/3 #3 Third");
  });
  it("renders control-action notices and skips empty notices", () => {
    const nodes = buildTimeline([
      {
        type: "mode_changed",
        event_id: "notice-mode",
        created_at_ms: 1,
        text: "Mode changed to Auto",
      },
      {
        type: "slash_help",
        event_id: "notice-help",
        created_at_ms: 2,
        payload: { text: "Actions: /compact, /help" },
      },
      {
        type: "compaction_summary_invalid",
        event_id: "notice-empty",
        created_at_ms: 3,
        payload: {},
      },
    ] as unknown as TimelineEvent[]);
    expect(nodes).toContainEqual(
      expect.objectContaining({ kind: "notice", text: "Mode changed to Auto" }),
    );
    expect(nodes).toContainEqual(
      expect.objectContaining({
        kind: "notice",
        text: "Actions: /compact, /help",
      }),
    );
    expect(nodes).not.toContainEqual(expect.objectContaining({ text: "" }));
  });

  it("renders nonblocking messages and operational blockers as distinct notices", () => {
    const nodes = buildTimeline([
      {
        type: "agent_message",
        event_id: "agent-message",
        created_at_ms: 1,
        working_event: {
          event_type: "agent_message",
          category: "message",
          direction: "outgoing",
          timestamp: new Date(1).toISOString(),
          payload: { message: "Progress update", kind: "progress" },
        },
      },
      {
        type: "operational_blocker",
        event_id: "operational-blocker",
        created_at_ms: 2,
        working_event: {
          event_type: "operational_blocker",
          category: "notice",
          direction: "outgoing",
          timestamp: new Date(2).toISOString(),
          payload: {
            severity: "hard",
            category: "host",
            summary: "Host unavailable",
          },
        },
      },
    ]);

    expect(nodes).toEqual([
      {
        kind: "notice",
        text: "Progress update",
        tone: "info",
        noticeKind: "agent_message",
      },
      {
        kind: "notice",
        text: "Hard blocker: Host unavailable",
        tone: "warn",
        noticeKind: "operational_blocker",
      },
    ]);
  });
  it("skips empty tool-call-only assistant iterations", () => {
    const nodes = buildTimeline(toolCallOnlyIteration as TimelineEvent[]);
    expect(nodes).toEqual([]);
  });
  it("attaches diff and screenshot references without creating empty attachment rows", () => {
    const nodes = buildTimeline([
      {
        type: "multi_edit_result",
        event_id: "diff",
        created_at_ms: 1,
        working_event: {
          payload: {
            file_updates: [
              {
                file_path: "src/lib.rs",
                action_type: "edit",
                lines_added: 2,
                lines_removed: 1,
                artifact_id: "artifact-diff",
              },
              {
                file_path: "src/empty.rs",
                action_type: "edit",
                lines_added: 0,
                lines_removed: 0,
              },
            ],
          },
        },
      },
      {
        type: "computer_use",
        event_id: "screenshot",
        created_at_ms: 2,
        working_event: {
          payload: { screenshot_keys: ["artifact-image"] },
        },
      },
      {
        type: "computer_use",
        event_id: "empty-screenshot",
        created_at_ms: 3,
        working_event: {
          payload: { screenshot_keys: [] },
        },
      },
    ] as unknown as TimelineEvent[]);
    const rows = nodes
      .filter((node) => node.kind === "work")
      .flatMap((node) => node.rows);
    expect(rows).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          label: "Edited lib.rs",
          additions: 2,
          deletions: 1,
          artifactId: "artifact-diff",
          artifactKind: "diff",
        }),
        expect.objectContaining({
          label: "Screenshot",
          artifactId: "artifact-image",
          artifactKind: "screenshot",
        }),
      ]),
    );
    expect(rows.filter((row) => row.artifactId)).toHaveLength(2);
  });
});
