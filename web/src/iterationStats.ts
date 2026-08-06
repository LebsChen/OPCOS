import type { TimelineEvent } from "./timeline";

export type IterationStat = {
  iteration: number;
  toolCalls: number;
  durationMs: number | null;
  inferenceMs: number | null;
  toolExecMs: number | null;
  harnessMs: number | null;
  inputTokens: number | null;
  outputTokens: number | null;
  retryCount: number | null;
  compactionCount: number | null;
};

export type IterationStatsSummary = {
  iterations: IterationStat[];
  totalInputTokens: number | null;
  totalOutputTokens: number | null;
  totalDurationMs: number | null;
  totalRetries: number | null;
  totalCompactions: number;
  automaticCompactions: number;
  manualCompactions: number;
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

function numberOrNull(value: unknown): number | null {
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function sumKnown(values: Array<number | null>): number | null {
  return values.some((value) => value === null)
    ? null
    : values.reduce<number>((sum, value) => sum + (value ?? 0), 0);
}

export function summarizeIterationStats(
  events: TimelineEvent[],
): IterationStatsSummary {
  const iterations = events
    .filter((event) => eventType(event) === "iteration_stats")
    .map((event) => {
      const data = payload(event);
      return {
        iteration: numberOrNull(data.iteration) ?? 0,
        toolCalls: numberOrNull(data.num_tool_calls) ?? 0,
        durationMs: numberOrNull(data.duration_ms),
        inferenceMs: numberOrNull(data.inference_ms),
        toolExecMs: numberOrNull(data.tool_exec_ms),
        harnessMs: numberOrNull(data.harness_ms),
        inputTokens: numberOrNull(data.input_tokens),
        outputTokens: numberOrNull(data.output_tokens),
        retryCount: numberOrNull(data.retry_count),
        compactionCount: numberOrNull(data.compaction_count),
      };
    })
    .sort((a, b) => a.iteration - b.iteration);
  const compactions = events
    .filter((event) => eventType(event) === "compacted")
    .map((event) => String(payload(event).source ?? "automatic"));
  return {
    iterations,
    totalInputTokens: sumKnown(iterations.map((item) => item.inputTokens)),
    totalOutputTokens: sumKnown(iterations.map((item) => item.outputTokens)),
    totalDurationMs: sumKnown(iterations.map((item) => item.durationMs)),
    totalRetries: sumKnown(iterations.map((item) => item.retryCount)),
    totalCompactions: compactions.length,
    automaticCompactions: compactions.filter((source) => source === "automatic")
      .length,
    manualCompactions: compactions.filter((source) => source === "manual")
      .length,
  };
}
