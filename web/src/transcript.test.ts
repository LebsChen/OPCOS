import { describe, expect, it } from "vitest";
import { humanizeTool } from "./humanize";
import {
  classifyStepStatus,
  providerErrorPresentation,
  normalizeTranscript,
  normalizeViewItems,
  reduceStreamEvent,
} from "./transcript";
import { groupWorkSegments } from "./workSegments";

describe("OPCOS transcript folding", () => {
  it("shortens provider JSON errors while retaining expandable details", () => {
    const error = providerErrorPresentation(
      '{"status_code":503,"error":{"message":"system_cpu_overloaded","request_id":"secret"}}',
    );
    expect(error.summary).toBe(
      "Provider request failed — HTTP 503 Service Unavailable",
    );
    expect(error.toast).toBe("system cpu overloaded");
    expect(error.detail).toContain("system_cpu_overloaded");
    expect(error.summary).not.toContain("{");
  });

  it("summarizes common tool actions as compact step lines", () => {
    expect(humanizeTool("run_shell", { command: "pytest -q" })).toMatchObject({
      pre: "Ran ",
      obj: "pytest -q",
    });
    expect(humanizeTool("edit_file", { path: "temperature.py" })).toMatchObject(
      {
        pre: "Edited ",
        obj: "temperature.py",
      },
    );
    expect(humanizeTool("read_file", { path: "src/orders.py" })).toMatchObject({
      pre: "Read ",
      obj: "orders.py",
    });
    expect(humanizeTool("propose_plan", { steps: [1, 2] })).toMatchObject({
      pre: "Proposed a plan (2 steps)",
    });
  });

  it.each([
    ["running", "running"],
    ["…", "running"],
    ["ok", "ok"],
    ["interrupted", "failed"],
    ["error", "failed"],
  ] as const)("classifies %s tool steps for rendering", (status, expected) => {
    expect(classifyStepStatus(status)).toBe(expected);
  });

  it("pairs persisted tool calls with their results", () => {
    const items = normalizeTranscript([
      {
        kind: "assistant",
        payload: {
          role: "assistant",
          content: "I will inspect the repository.",
          tool_calls: [
            {
              id: "call-1",
              name: "list_dir",
              arguments: { path: "/workspace" },
            },
          ],
        },
      },
      {
        kind: "tool",
        payload: {
          role: "tool",
          content: [
            {
              type: "tool_result",
              tool_use_id: "call-1",
              content: [{ type: "text", text: "ok" }],
            },
          ],
        },
      },
    ]);
    expect(items.filter((item) => item.kind === "tool")).toHaveLength(1);
    expect(items[1]).toMatchObject({
      callId: "call-1",
      status: "ok",
      result: "ok",
    });
  });

  it("renders interrupted store tool rows as failed tool items", () => {
    const items = normalizeTranscript([
      {
        kind: "tool",
        payload: {
          call_id: "call-interrupted",
          tool: "run_shell",
          arguments: { command: "echo hi" },
          result: null,
          status: "interrupted",
          approval: false,
        },
      },
    ]);
    expect(items).toContainEqual(
      expect.objectContaining({
        kind: "tool",
        callId: "call-interrupted",
        status: "interrupted",
      }),
    );
  });

  it("merges store tool rows into the assistant tool card by call id", () => {
    const items = normalizeTranscript([
      {
        kind: "assistant",
        payload: {
          role: "assistant",
          content: "",
          tool_calls: [
            {
              id: "call-merge",
              name: "run_shell",
              arguments: { command: "pwd" },
            },
          ],
        },
      },
      {
        kind: "tool",
        payload: {
          call_id: "call-merge",
          tool: "run_shell",
          arguments: { command: "pwd" },
          result: "ok",
          status: "ok",
          approval: false,
        },
      },
    ]);
    expect(items.filter((item) => item.kind === "tool")).toHaveLength(1);
    expect(items.find((item) => item.kind === "tool")).toMatchObject({
      callId: "call-merge",
      status: "ok",
      result: "ok",
    });
  });

  it("folds text deltas into one stable streaming item", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: { text_delta: "hel" },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: { text_delta: "lo" },
    });
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ id: "stream:assistant", text: "hello" });
  });

  it("deduplicates persisted pending approvals by call id", () => {
    const items = normalizeTranscript([
      {
        kind: "assistant",
        payload: {
          role: "assistant",
          content: "",
          tool_calls: [
            {
              id: "call-pending",
              name: "write_file",
              arguments: { path: "/workspace/a.txt" },
            },
          ],
        },
      },
      {
        kind: "approval",
        payload: {
          call_id: "call-pending",
          tool: "write_file",
          arguments: { path: "/workspace/a.txt" },
        },
      },
    ]);
    expect(
      items.filter(
        (item) => item.kind === "tool" && item.callId === "call-pending",
      ),
    ).toHaveLength(1);
  });

  it("keeps tool call deltas in a single card and finalizes the turn", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        tool_call_delta: {
          index: 0,
          id: "call-1",
          name: "run_shell",
          arguments_fragment: '{"cmd":',
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: { tool_call_delta: { index: 0, arguments_fragment: '"pwd"}' } },
    });
    expect(items.filter((item) => item.kind === "tool")).toHaveLength(1);
    expect(items[0]).toMatchObject({
      callId: "call-1",
      toolName: "run_shell",
      arguments: '{"cmd":"pwd"}',
    });
  });

  it("folds tool results into the matching streaming call", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        tool_call_delta: {
          index: 0,
          id: "call-result",
          name: "read_file",
          arguments_fragment: "{}",
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: {
        tool_result: {
          call_id: "call-result",
          result: { content: "ok" },
        },
      },
    });
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({
      callId: "call-result",
      result: { content: "ok" },
      status: "ok",
      approval: false,
    });
  });

  it("renders notices separately", () => {
    const items = reduceStreamEvent([], {
      kind: "notice",
      payload: { kind: "interrupted", text: "Turn interrupted" },
    });
    expect(items[0]).toMatchObject({
      kind: "notice",
      noticeKind: "interrupted",
    });
  });

  it("renders pending approval events and resolves their card", () => {
    let items = reduceStreamEvent([], {
      kind: "approval",
      payload: {
        call_id: "call-danger",
        tool: "run_shell",
        arguments: { cmd: "git status" },
        risk: "execute",
      },
    });
    expect(items[0]).toMatchObject({
      kind: "tool",
      callId: "call-danger",
      approval: true,
      status: "pending",
    });
    items = reduceStreamEvent(items, {
      kind: "approval_resolved",
      payload: { call_id: "call-danger", approve: true },
    });
    expect(items[0]).toMatchObject({
      callId: "call-danger",
      status: "ok",
      resolved: "allow",
    });
  });

  it("keeps a denied approval as a resolved historical item", () => {
    let items = reduceStreamEvent([], {
      kind: "approval",
      payload: { call_id: "call-denied", tool: "write_file", arguments: {} },
    });
    items = reduceStreamEvent(items, {
      kind: "approval_resolved",
      payload: { call_id: "call-denied", approve: false },
    });
    expect(items[0]).toMatchObject({
      callId: "call-denied",
      status: "error",
      resolved: "deny",
      approval: false,
    });
  });

  it("places live steering before the active assistant and finalizes running tools", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        tool_call_delta: {
          index: 0,
          id: "call-live",
          name: "run_shell",
          arguments_fragment: "{}",
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "steering",
      payload: { text: "use the safer command" },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: { text_delta: "done" },
    });
    expect(items.findIndex((item) => item.kind === "user")).toBeLessThan(
      items.findIndex((item) => item.id === "stream:assistant"),
    );
    items = reduceStreamEvent(items, {
      kind: "turn_done",
      payload: {},
    });
    expect(items.find((item) => item.kind === "tool")).toMatchObject({
      status: "ok",
    });
  });

  it("moves steering ahead when the assistant stream already started", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: { text_delta: "answer" },
    });
    items = reduceStreamEvent(items, {
      kind: "steering",
      payload: { text: "clarify first" },
    });
    expect(items.map((item) => item.kind)).toEqual(["user", "assistant"]);
    expect(items[0]).toMatchObject({ text: "clarify first" });
  });

  it("does not restore stale approval notices or pending bubbles", () => {
    const items = normalizeTranscript([
      {
        kind: "notice",
        payload: {
          role: "notice",
          text: "Approval required before this tool can continue",
        },
      },
      {
        kind: "assistant",
        payload: {
          role: "assistant",
          content: "Pending",
          tool_calls: [{ id: "call-2", name: "write_file", arguments: {} }],
        },
      },
    ]);
    expect(items.some((item) => item.kind === "notice")).toBe(false);
    expect(
      items.some(
        (item) => item.kind === "assistant" && item.text === "Pending",
      ),
    ).toBe(false);
  });

  it("deduplicates persisted tool rows and keeps denied state", () => {
    const items = normalizeTranscript([
      {
        kind: "assistant",
        payload: {
          role: "assistant",
          content: "",
          tool_calls: [{ id: "call-3", name: "write_file", arguments: {} }],
        },
      },
      {
        kind: "assistant",
        payload: {
          role: "assistant",
          content: "Pending",
          tool_calls: [{ id: "call-3", name: "write_file", arguments: {} }],
        },
      },
      {
        kind: "tool",
        payload: {
          role: "tool",
          content: [
            {
              tool_use_id: "call-3",
              content: [
                {
                  type: "text",
                  text: '{"error":"tool call denied by user"}',
                },
              ],
            },
          ],
        },
      },
    ]);
    expect(items.filter((item) => item.callId === "call-3")).toHaveLength(1);
    expect(items.find((item) => item.callId === "call-3")).toMatchObject({
      resolved: "deny",
      status: "ok",
    });
    expect(
      items.some(
        (item) => item.kind === "assistant" && item.text === "Pending",
      ),
    ).toBe(false);
  });

  it("deduplicates duplicate user messages from refresh and live merge", () => {
    const persisted = normalizeTranscript([
      { kind: "user", payload: { role: "user", text: "Fix the bug." } },
      { kind: "user", payload: { role: "user", text: "Fix the bug." } },
    ]);
    const merged = reduceStreamEvent(persisted, {
      kind: "message",
      payload: { text: "Fix the bug." },
    });

    expect(persisted.filter((item) => item.kind === "user")).toHaveLength(1);
    expect(merged.filter((item) => item.kind === "user")).toHaveLength(1);
  });

  it("resets live assistant deltas after a provider stream retry", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: { text_delta: "first", reasoning_delta: "thought one" },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: { stream_reset: true },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: { text_delta: "second", reasoning_delta: "thought two" },
    });

    expect(items).toContainEqual(
      expect.objectContaining({
        id: "stream:assistant",
        text: "second",
        reasoning: "thought two",
      }),
    );
  });

  it("keeps one current activity instead of accumulating status rows", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "status_update",
          payload: { enum: "working" },
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "simple_activity_update",
          payload: { enum: "deciding_action" },
        },
      },
    });

    expect(
      items.filter(
        (item) =>
          item.kind === "notice" &&
          (item.noticeKind === "status_update" ||
            item.noticeKind === "simple_activity_update"),
      ),
    ).toHaveLength(1);
    expect(items[0]).toMatchObject({
      text: "Deciding what to do next",
    });
  });

  it("does not create an empty thought item", () => {
    const items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "" },
        },
      },
    });
    expect(items).toHaveLength(0);
  });

  it("merges adjacent persisted thought items in chronological order", () => {
    const items = normalizeTranscript([
      {
        kind: "assistant",
        payload: { role: "assistant", reasoning: "Inspect the input." },
      },
      {
        kind: "assistant",
        payload: { role: "assistant", reasoning: "Trace the shared helper." },
      },
      {
        kind: "assistant",
        payload: { role: "assistant", content: "The fix is complete." },
      },
    ]);

    const thoughts = items.filter((item) => item.kind === "thinking");
    expect(thoughts).toHaveLength(1);
    expect(thoughts[0]).toMatchObject({
      reasoning: "Inspect the input.\n\n---\n\nTrace the shared helper.",
    });
  });

  it("merges adjacent live thought events into one block", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "First thought" },
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "Second thought" },
        },
      },
    });

    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({
      kind: "thinking",
      text: "First thought\n\n---\n\nSecond thought",
    });
  });

  it("merges thoughts separated by hidden activity notices", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "First thought" },
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "status_update",
          payload: { enum: "working" },
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "Second thought" },
        },
      },
    });

    expect(items.filter((item) => item.kind === "thinking")).toHaveLength(1);
    expect(items.find((item) => item.kind === "thinking")).toMatchObject({
      text: "First thought\n\n---\n\nSecond thought",
    });
  });

  it("merges persisted thoughts across hidden notices", () => {
    const items = normalizeTranscript([
      {
        kind: "assistant",
        payload: { role: "assistant", reasoning: "First persisted thought" },
      },
      {
        kind: "notice",
        payload: { kind: "iteration_stats", text: "internal stats" },
      },
      {
        kind: "assistant",
        payload: { role: "assistant", reasoning: "Second persisted thought" },
      },
    ]);

    expect(items.filter((item) => item.kind === "thinking")).toHaveLength(1);
    expect(items.find((item) => item.kind === "thinking")).toMatchObject({
      reasoning: "First persisted thought\n\n---\n\nSecond persisted thought",
    });
  });

  it("drops exact duplicate live thoughts", () => {
    const event = {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "Repeated thought" },
        },
      },
    };
    let items = reduceStreamEvent([], event);
    items = reduceStreamEvent(items, event);

    expect(items.filter((item) => item.kind === "thinking")).toHaveLength(1);
  });

  it("deduplicates thoughts replayed after a provider stream reset", () => {
    let items = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "Retry the provider request." },
        },
      },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: { stream_reset: true },
    });
    items = reduceStreamEvent(items, {
      kind: "stream",
      payload: {
        reasoning_delta: "Retry the provider request.",
      },
    });

    const rendered = normalizeViewItems(items);
    const thoughts = rendered.filter(
      (item) =>
        (item.kind === "thinking" ||
          (item.kind === "assistant" && !item.text?.trim())) &&
        (item.reasoning || item.text || "").trim() ===
          "Retry the provider request.",
    );
    expect(thoughts).toHaveLength(1);
  });

  it("normalizes live rendering to the persisted timeline shape", () => {
    let live = reduceStreamEvent([], {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "Inspect the repository." },
        },
      },
    });
    live = reduceStreamEvent(live, {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "status_update",
          payload: { enum: "working" },
        },
      },
    });
    live = reduceStreamEvent(live, {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "Inspect the repository." },
        },
      },
    });
    live = reduceStreamEvent(live, {
      kind: "stream",
      payload: {
        working_event: {
          event_type: "devin_thoughts",
          payload: { message: "Trace the shared helper." },
        },
      },
    });
    live = reduceStreamEvent(live, {
      kind: "stream",
      payload: {
        tool_call_delta: {
          index: 0,
          id: "call-1",
          name: "read_file",
          arguments_fragment: '{"path":"README.md"}',
        },
      },
    });

    const persisted = normalizeTranscript([
      {
        kind: "assistant",
        payload: { role: "assistant", reasoning: "Inspect the repository." },
      },
      {
        kind: "notice",
        payload: { kind: "status_update", text: "Working" },
      },
      {
        kind: "assistant",
        payload: { role: "assistant", reasoning: "Trace the shared helper." },
      },
      {
        kind: "assistant",
        payload: {
          role: "assistant",
          tool_calls: [
            {
              id: "call-1",
              name: "read_file",
              arguments: '{"path":"README.md"}',
            },
          ],
        },
      },
    ]);
    const shape = (items: typeof persisted) =>
      items.map(
        ({ id: _id, status: _status, approval: _approval, ...item }) => item,
      );
    expect(shape(normalizeViewItems(live))).toEqual(shape(persisted));
  });

  it("groups work items at user and final-answer boundaries", () => {
    const groups = groupWorkSegments(
      [
        { kind: "user", text: "Fix the bug." },
        {
          kind: "tool",
          id: "tool-1",
          name: "run_shell",
          args: { command: "sed -n '1,20p' README.md" },
          status: "ok",
        },
        {
          kind: "assistant",
          text: "The fix is complete.",
        },
        { kind: "user", text: "Run the tests too." },
        {
          kind: "tool",
          id: "tool-2",
          name: "run_shell",
          args: { command: "cargo test" },
          status: "ok",
        },
      ],
      false,
    );
    expect(groups).toHaveLength(5);
    expect(groups[1]).toMatchObject({
      items: [{ kind: "tool", name: "run_shell" }],
      active: false,
    });
    expect(groups[2]).toEqual({
      item: { kind: "assistant", text: "The fix is complete." },
    });
    expect(groups[4]).toMatchObject({
      items: [{ kind: "tool", name: "run_shell" }],
      active: false,
    });
  });

  it("does not retain a resolved ask_user placeholder notice", () => {
    const items = normalizeTranscript([
      {
        kind: "tool",
        payload: {
          role: "tool",
          tool: "ask_user",
          call_id: "ask-1",
          result: { answer: "Use JSON" },
        },
      },
      {
        kind: "notice",
        payload: {
          kind: "question_pending",
          text: "Question requires an answer before this tool can continue",
        },
      },
    ]);

    expect(items.some((item) => item.kind === "notice")).toBe(false);
    expect(items).toHaveLength(1);
    expect(items[0]).toMatchObject({ kind: "tool", toolName: "ask_user" });
  });

  it("skips question pending notices in the live timeline", () => {
    const items = reduceStreamEvent([], {
      kind: "notice",
      payload: {
        kind: "question_pending",
        text: "Question requires an answer before this tool can continue",
      },
    });

    expect(items).toHaveLength(0);
  });
});
