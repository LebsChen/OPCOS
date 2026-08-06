import { describe, expect, it } from "vitest";
import liveEnvelopes from "../../fixtures/timeline/live-events.json";
import opcosEvents from "../../fixtures/timeline/opcos-events.json";
import planIterations from "../../fixtures/timeline/opcos-plan-iterations.json";
import terminalReplay from "../../fixtures/timeline/opcos-terminal-replay.json";
import toolCallOnlyIteration from "../../fixtures/timeline/tool-call-only-iteration.json";
import persisted from "../../fixtures/timeline/persisted-events.json";
import {
  buildTimeline,
  mergeEvents,
  TRANSIENT_TIMELINE_EVENT_TYPES,
  type TimelineEvent,
} from "./timeline";

const live = liveEnvelopes.map((entry) => entry.payload as TimelineEvent);
const saved = persisted as TimelineEvent[];
const opcos = opcosEvents as TimelineEvent[];

describe("single event-log timeline", () => {
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
    expect(rows).toContainEqual({
      label: "true",
      callId: "call-terminal-empty",
      terminalOutput: undefined,
      terminalTruncated: undefined,
      terminalTotalBytes: undefined,
    });
  });

  it("renders live and reloaded events identically", () => {
    expect(buildTimeline(live)).toEqual(buildTimeline(saved));
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
  it("uses Devin wording for fixture-derived rows", () => {
    const nodes = buildTimeline(live);
    const work = nodes.filter((node) => node.kind === "work");
    const rows = work.flatMap((node) => node.rows.map((row) => row.label));
    expect(rows.some((label) => /^Thought for \d+s$/.test(label))).toBe(true);
    expect(rows).toContain("Edited alpha.txt +1 −0");
    expect(rows).toContain("Created notes.md +69");
    expect(rows).not.toContain("write_file");
    expect(rows).not.toContain("edit_file");
    expect(work.some((node) => /^Worked for \d+s$/.test(node.label))).toBe(
      true,
    );
    expect(work.reduce((sum, node) => sum + node.additions, 0)).toBe(70);
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
      "Created notes.md +4",
      "Edited lib.rs +2 −1",
      "Created 4 Tasks",
      "1/4#1 Implement the change",
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
    expect(rows).toContainEqual({
      label: "true",
      callId: "call-empty",
      terminalOutput: undefined,
      terminalTruncated: undefined,
      terminalTotalBytes: undefined,
    });
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
    expect(rows).toContain("1/5#1 Create files");
    expect(rows).toContain("2/5#2 Run tests");
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
          artifactId: "artifact-diff",
          artifactKind: "diff",
        }),
        expect.objectContaining({
          artifactId: "artifact-image",
          artifactKind: "screenshot",
        }),
      ]),
    );
    expect(rows.filter((row) => row.artifactId)).toHaveLength(2);
  });
});
