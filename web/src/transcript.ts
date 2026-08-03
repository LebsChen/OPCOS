import { redactApproval } from "./gui";

export type TranscriptKind =
  "user" | "assistant" | "thinking" | "tool" | "notice" | "approval";

export type ToolState = "running" | "ok" | "error" | "pending" | "interrupted";
export type ApprovalResolution = "allow" | "deny";

export type StepStatusKind = "running" | "ok" | "failed";

export function classifyStepStatus(status: string): StepStatusKind {
  if (status === "running" || status === "…") return "running";
  if (status === "ok") return "ok";
  return "failed";
}

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
      const noticeText =
        typeof payload.text === "string"
          ? payload.text
          : typeof payload.message === "string"
            ? payload.message
            : textFromContent(payload.content);
      if (
        noticeText.includes("Approval required before this tool can continue")
      )
        return;
      output.push({
        id: stableId("notice", index),
        kind: "notice",
        noticeKind:
          typeof payload.kind === "string" ? payload.kind : record.kind,
        text: noticeText,
      });
      return;
    }
    if (
      record.kind === "tool" &&
      (typeof payload.call_id === "string" ||
        typeof payload.callId === "string" ||
        typeof payload.tool === "string" ||
        typeof payload.toolName === "string")
    ) {
      const callId =
        typeof payload.call_id === "string"
          ? payload.call_id
          : typeof payload.callId === "string"
            ? payload.callId
            : `tool-${index}`;
      const existing = output.find(
        (item) => item.kind === "tool" && item.callId === callId,
      );
      if (existing?.kind === "tool") {
        existing.toolName =
          typeof payload.toolName === "string"
            ? payload.toolName
            : typeof payload.tool === "string"
              ? payload.tool
              : existing.toolName;
        existing.arguments = payload.arguments ?? existing.arguments;
        existing.result = payload.result ?? existing.result;
        existing.status =
          typeof payload.status === "string"
            ? (payload.status as ToolState)
            : existing.status;
        existing.approval = false;
      } else {
        output.push({
          id: stableId("tool", index, callId),
          kind: "tool",
          callId,
          toolName:
            typeof payload.toolName === "string"
              ? payload.toolName
              : typeof payload.tool === "string"
                ? payload.tool
                : "tool",
          arguments: payload.arguments,
          result: payload.result,
          status:
            typeof payload.status === "string"
              ? (payload.status as ToolState)
              : "interrupted",
          approval: false,
        });
      }
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
      const assistantText =
        typeof payload.content === "string"
          ? payload.content
          : textFromContent(payload.content);
      if (
        assistantText.trim().toLowerCase() !== "pending" ||
        calls.length > 0
      ) {
        output.push({
          id: stableId("assistant", index),
          kind: "assistant",
          text:
            assistantText.trim().toLowerCase() === "pending"
              ? ""
              : assistantText,
        });
      }
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
        const resultText = String(existing.result || "");
        const denied = /tool call denied by user/i.test(resultText);
        const approvalTool =
          existing.toolName === "write_file" ||
          existing.toolName === "edit" ||
          existing.toolName === "run_shell" ||
          existing.toolName === "send_message" ||
          existing.toolName === "send_file";
        existing.status = denied
          ? "ok"
          : resultText.includes('"error"')
            ? "error"
            : "ok";
        existing.approval = false;
        if (denied) existing.resolved = "deny";
        else if (approvalTool && resultText.includes('"error"')) {
          existing.resolved = "allow";
          existing.status = "ok";
        }
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
  const byCall = new Map<string, TranscriptViewItem>();
  const deduped: TranscriptViewItem[] = [];
  for (const item of output) {
    if (item.kind !== "tool" || !item.callId) {
      deduped.push(item);
      continue;
    }
    const existing = byCall.get(item.callId);
    if (!existing) {
      byCall.set(item.callId, item);
      deduped.push(item);
      continue;
    }
    existing.toolName = item.toolName || existing.toolName;
    existing.arguments ??= item.arguments;
    existing.result ??= item.result;
    existing.resolved ??= item.resolved;
    existing.status =
      item.resolved === "deny" || existing.resolved === "deny"
        ? "ok"
        : item.status === "error" || existing.status === "error"
          ? "error"
          : item.status || existing.status;
    existing.approval = item.approval || existing.approval;
  }
  return deduped;
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
    const steering = {
      id: `event:steering:${next.length}`,
      kind: "user",
      text: String(payload.text || ""),
    } satisfies TranscriptViewItem;
    const liveAssistant = next.findIndex(
      (item) => item.id === "stream:assistant",
    );
    if (liveAssistant >= 0) next.splice(liveAssistant, 0, steering);
    else next.push(steering);
    return next;
  }
  if (event.kind === "turn_done") {
    return next.map((item) =>
      item.kind === "tool" && item.status === "running" && !item.approval
        ? { ...item, status: "ok" }
        : item,
    );
  }
  if (event.kind === "notice" || event.kind === "approval_resolved") {
    const noticeKind =
      typeof payload.kind === "string" ? payload.kind : event.kind;
    if (event.kind === "approval_resolved") {
      const callId = typeof payload.call_id === "string" ? payload.call_id : "";
      const resolved = next
        .filter(
          (item) =>
            !(
              item.kind === "notice" &&
              (item.noticeKind === "approval_pending" ||
                item.text?.includes(
                  "Approval required before this tool can continue",
                ))
            ),
        )
        .map((item) =>
          item.kind === "tool" && item.callId === callId
            ? {
                ...item,
                status: (payload.approve === true ? "ok" : "error") as
                  "ok" | "error",
                approval: false,
                resolved: (payload.approve === true ? "allow" : "deny") as
                  "allow" | "deny",
              }
            : item,
        );
      const seen = new Set<string>();
      return resolved.filter((item) => {
        if (item.kind !== "tool" || item.callId !== callId) return true;
        if (seen.has(callId)) return false;
        seen.add(callId);
        return true;
      });
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
    const steeringIndex = next.findIndex(
      (item) => item.kind === "user" && item.id.startsWith("event:steering:"),
    );
    if (steeringIndex >= 0) {
      const [steering] = next.splice(steeringIndex, 1);
      const assistantIndex = next.indexOf(live);
      next.splice(assistantIndex, 0, steering);
    }
  }
  const delta = payloadObject(payload.tool_call_delta);
  if (Object.keys(delta).length > 0) {
    const index = typeof delta.index === "number" ? delta.index : 0;
    const id = typeof delta.id === "string" ? delta.id : `stream-tool-${index}`;
    const calls = next.filter(
      (item) => item.kind === "tool" && item.id.startsWith("stream:tool:"),
    );
    let tool = next.find((item) => item.kind === "tool" && item.callId === id);
    if (!tool) tool = calls[index];
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
  const toolResult = payloadObject(payload.tool_result);
  if (Object.keys(toolResult).length > 0) {
    const callId =
      typeof toolResult.call_id === "string" ? toolResult.call_id : "";
    if (callId) {
      const tool = next.find(
        (item) => item.kind === "tool" && item.callId === callId,
      );
      if (tool && tool.kind === "tool") {
        tool.result = toolResult.result;
        tool.status = "ok";
        tool.approval = false;
      }
    }
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
