import { describe, expect, it } from "vitest";
import { normalizeTranscript, reduceStreamEvent } from "./transcript";

describe("OPCOS transcript folding", () => {
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
});
