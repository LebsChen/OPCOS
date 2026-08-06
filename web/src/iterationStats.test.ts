import { describe, expect, it } from "vitest";
import fixture from "../../fixtures/timeline/opcos-iteration-stats.json";
import { summarizeIterationStats } from "./iterationStats";
import { buildTimeline, type TimelineEvent } from "./timeline";

describe("iteration stats", () => {
  it("summarizes canonical stats and preserves unknown legacy fields", () => {
    const events = fixture as TimelineEvent[];
    const summary = summarizeIterationStats(events);
    expect(summary.totalInputTokens).toBeNull();
    expect(summary.totalOutputTokens).toBeNull();
    expect(summary.totalDurationMs).toBe(1700);
    expect(summary.totalRetries).toBeNull();
    expect(summary.totalCompactions).toBe(2);
    expect(summary.automaticCompactions).toBe(1);
    expect(summary.manualCompactions).toBe(1);
    expect(summary.iterations[0]).toMatchObject({
      inferenceMs: 700,
      toolExecMs: 400,
      harnessMs: 100,
      retryCount: 0,
      compactionCount: 1,
    });
    expect(summary.iterations[1]).toMatchObject({
      inferenceMs: null,
      toolExecMs: null,
      harnessMs: null,
      inputTokens: null,
    });
  });

  it("rebuilds the same session summary after canonical events are reread", () => {
    const live = summarizeIterationStats(fixture as TimelineEvent[]);
    const reread = summarizeIterationStats(
      JSON.parse(JSON.stringify(fixture)) as TimelineEvent[],
    );
    expect(reread).toEqual(live);
    expect(
      buildTimeline(fixture as TimelineEvent[]).some(
        (node) =>
          node.kind === "work" &&
          node.rows.some((row) => row.label.includes("iteration")),
      ),
    ).toBe(false);
  });
});
