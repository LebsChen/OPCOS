import { redactApproval } from "./gui";

export type TranscriptKind =
  "user" | "assistant" | "thinking" | "tool" | "notice" | "approval";

export type ToolState = "running" | "ok" | "error" | "pending";
export type ApprovalResolution = "allow" | "deny";

export type TranscriptViewItem = {
  id: string;
  kind: TranscriptKind;
  text?: string;
  reasoning?: string;
  toolName?: string;
  callId?: string;
  arguments?: unknown;
  result?: unknown;
  status?: ToolState;
  noticeKind?: string;
  approval?: boolean;
  resolved?: ApprovalResolution;
};

type RawItem = { kind: string; payload: Record<string, unknown> };

function textFromContent(value: unknown): string {
  if (typeof value === "string") return value;
  if (!Array.isArray(value)) return "";
  return value
    .map((part) => {
      if (typeof part === "string") return part;
      if (!part || typeof part !== "object") return "";
      const item = part as Record<string, unknown>;
      return typeof item.text === "string" ? item.text : "";
    })
    .filter(Boolean)
    .join("");
}

function payloadObject(payload: unknown): Record<string, unknown> {
  return payload && typeof payload === "object"
    ? (payload as Record<string, unknown>)
    : {};
}

function toolCalls(value: unknown): Array<Record<string, unknown>> {
  return Array.isArray(value)
    ? value.filter(
        (item): item is Record<string, unknown> =>
          !!item && typeof item === "object",
      )
    : [];
}

function stableId(prefix: string, index: number, value?: string): string {
  return value ? `${prefix}:${value}` : `${prefix}:${index}`;
}

export function normalizeTranscript(raw: RawItem[]): TranscriptViewItem[] {
  const output: TranscriptViewItem[] = [];
  const toolIndex = new Map<string, TranscriptViewItem>();
  raw.forEach((record, index) => {
    const payload = payloadObject(record.payload);
    const role = typeof payload.role === "string" ? payload.role : record.kind;
    if (role === "user") {
      output.push({
        id: stableId("user", index),
        kind: "user",
        text:
          typeof payload.text === "string"
            ? payload.text
            : textFromContent(payload.content),
      });
      return;
    }
    if (
      role === "notice" ||
      ["error", "interrupted", "compacted", "model_switch"].includes(
        record.kind,
      )
    ) {
      output.push({
        id: stableId("notice", index),
        kind: "notice",
        noticeKind:
          typeof payload.kind === "string" ? payload.kind : record.kind,
        text:
          typeof payload.text === "string"
            ? payload.text
            : typeof payload.message === "string"
              ? payload.message
              : textFromContent(payload.content),
      });
      return;
    }
    if (record.kind === "approval" || role === "approval") {
      const callId =
        typeof payload.call_id === "string"
          ? payload.call_id
          : `approval-${index}`;
      const existing = output.find(
        (item) => item.kind === "tool" && item.callId === callId,
      );
      if (existing?.kind === "tool") {
        existing.toolName =
          typeof payload.tool === "string" ? payload.tool : existing.toolName;
        existing.arguments = payload.arguments;
        existing.status = "pending";
        existing.approval = true;
      } else {
        output.push({
          id: `approval:${callId}`,
          kind: "tool",
          callId,
          toolName: typeof payload.tool === "string" ? payload.tool : "tool",
          arguments: payload.arguments,
          status: "pending",
          approval: true,
        });
      }
      return;
    }
    if (role === "assistant") {
      const calls = toolCalls(payload.tool_calls);
      if (payload.reasoning) {
        output.push({
          id: stableId("thinking", index),
          kind: "thinking",
          reasoning: String(payload.reasoning),
        });
      }
      output.push({
        id: stableId("assistant", index),
        kind: "assistant",
        text:
          typeof payload.content === "string"
            ? payload.content
            : textFromContent(payload.content),
      });
      calls.forEach((call, callIndex) => {
        const callId =
          typeof call.id === "string" ? call.id : `call-${index}-${callIndex}`;
        const item: TranscriptViewItem = {
          id: stableId("tool", index, callId),
          kind: "tool",
          callId,
          toolName: typeof call.name === "string" ? call.name : "tool",
          arguments: call.arguments,
          status: "pending",
          approval: true,
        };
        toolIndex.set(callId, item);
        output.push(item);
      });
      return;
    }
    if (role === "tool") {
      const parts = Array.isArray(payload.content) ? payload.content : [];
      const resultPart = payloadObject(parts[0]);
      const callId =
        typeof resultPart.tool_use_id === "string"
          ? resultPart.tool_use_id
          : undefined;
      const existing = callId ? toolIndex.get(callId) : undefined;
      if (existing) {
        existing.result =
          textFromContent(resultPart.content) || resultPart.content;
        existing.status =
          existing.result && String(existing.result).includes('"error"')
            ? "error"
            : "ok";
        existing.approval = false;
      } else {
        output.push({
          id: stableId("tool-result", index, callId),
          kind: "tool",
          callId,
          toolName: "tool",
          result: textFromContent(resultPart.content) || resultPart.content,
          status: "ok",
          approval: false,
        });
      }
    }
  });
  return output;
}

export function reduceStreamEvent(
  items: TranscriptViewItem[],
  event: { kind: string; payload: Record<string, unknown> },
): TranscriptViewItem[] {
  const next = items.map((item) => ({ ...item }));
  const payload = payloadObject(event.payload);
  if (event.kind === "message") {
    next.push({
      id: `event:message:${String(payload.id || next.length)}`,
      kind: "user",
      text:
        typeof payload.text === "string"
          ? payload.text
          : textFromContent(payload.content),
    });
    return next;
  }
  if (event.kind === "steering") {
    next.push({
      id: `event:steering:${next.length}`,
      kind: "user",
      text: String(payload.text || ""),
    });
    return next;
  }
  if (event.kind === "notice" || event.kind === "approval_resolved") {
    const noticeKind =
      typeof payload.kind === "string" ? payload.kind : event.kind;
    if (event.kind === "approval_resolved") {
      const callId = typeof payload.call_id === "string" ? payload.call_id : "";
      return next.map((item) =>
        item.kind === "tool" && item.callId === callId
          ? {
              ...item,
              status: payload.approve === true ? "ok" : "error",
              approval: false,
              resolved: payload.approve === true ? "allow" : "deny",
            }
          : item,
      );
    }
    next.push({
      id: `event:notice:${next.length}`,
      kind: "notice",
      noticeKind,
      text:
        typeof payload.text === "string"
          ? payload.text
          : String(payload.message || ""),
    });
    return next;
  }
  if (event.kind === "approval") {
    const callId =
      typeof payload.call_id === "string"
        ? payload.call_id
        : `approval:${next.length}`;
    const existing = next.find(
      (item) => item.kind === "tool" && item.callId === callId,
    );
    if (existing) {
      return next.map((item) =>
        item.callId === callId
          ? {
              ...item,
              toolName:
                typeof payload.tool === "string" ? payload.tool : item.toolName,
              arguments: payload.arguments,
              status: "pending",
              approval: true,
            }
          : item,
      );
    }
    next.push({
      id: `approval:${callId}`,
      kind: "tool",
      callId,
      toolName: typeof payload.tool === "string" ? payload.tool : "tool",
      arguments: payload.arguments,
      status: "pending",
      approval: true,
    });
    return next;
  }
  if (event.kind !== "stream") return next;
  const textDelta =
    typeof payload.text_delta === "string" ? payload.text_delta : "";
  const reasoningDelta =
    typeof payload.reasoning_delta === "string" ? payload.reasoning_delta : "";
  let live = next.find((item) => item.id === "stream:assistant");
  if ((textDelta || reasoningDelta) && !live) {
    live = {
      id: "stream:assistant",
      kind: "assistant",
      text: "",
      reasoning: "",
    };
    next.push(live);
  }
  if (live) {
    live.text = `${live.text || ""}${textDelta}`;
    live.reasoning = `${live.reasoning || ""}${reasoningDelta}`;
  }
  const delta = payloadObject(payload.tool_call_delta);
  if (Object.keys(delta).length > 0) {
    const index = typeof delta.index === "number" ? delta.index : 0;
    const id = typeof delta.id === "string" ? delta.id : `stream-tool-${index}`;
    const calls = next.filter(
      (item) => item.kind === "tool" && item.id.startsWith("stream:tool:"),
    );
    let tool = calls[index];
    if (!tool) {
      tool = {
        id: `stream:tool:${index}`,
        kind: "tool",
        callId: id,
        toolName: "tool",
        arguments: "",
        status: "running",
      };
      next.push(tool);
    }
    if (typeof delta.id === "string") tool.callId = delta.id;
    if (typeof delta.name === "string") tool.toolName = delta.name;
    if (typeof delta.arguments_fragment === "string")
      tool.arguments = `${String(tool.arguments || "")}${delta.arguments_fragment}`;
  }
  const turn = payloadObject(payload.turn);
  if (Object.keys(turn).length > 0) {
    const liveIndex = next.findIndex((item) => item.id === "stream:assistant");
    const calls = toolCalls(turn.tool_calls);
    const assistant: TranscriptViewItem = {
      id: `event:assistant:${next.length}`,
      kind: "assistant",
      text: typeof turn.text === "string" ? turn.text : live?.text || "",
    };
    if (typeof turn.reasoning === "string")
      assistant.reasoning = turn.reasoning;
    if (liveIndex >= 0) next.splice(liveIndex, 1, assistant);
    else next.push(assistant);
    calls.forEach((call, index) => {
      const callId =
        typeof call.id === "string" ? call.id : `call-${next.length}-${index}`;
      const existing = next.find(
        (item) => item.kind === "tool" && item.callId === callId,
      );
      if (existing && existing.kind === "tool") {
        existing.toolName =
          typeof call.name === "string" ? call.name : existing.toolName;
        existing.arguments = call.arguments;
        existing.status = "ok";
        return;
      }
      next.push({
        id: stableId("tool", next.length, callId),
        kind: "tool",
        callId,
        toolName: typeof call.name === "string" ? call.name : "tool",
        arguments: call.arguments,
        status: "pending",
        approval: true,
      });
    });
    const steeringIndex = next.findIndex(
      (item) => item.kind === "user" && item.id.startsWith("event:steering:"),
    );
    if (steeringIndex >= 0) {
      const [steering] = next.splice(steeringIndex, 1);
      const assistantIndex = next.indexOf(assistant);
      next.splice(assistantIndex, 0, steering);
    }
  }
  return next;
}

export function toolArgumentSummary(value: unknown): string {
  return redactApproval(value).slice(0, 180);
}
