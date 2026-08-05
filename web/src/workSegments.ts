import type { Item } from "./types";

export type WorkSegment = {
  items: Item[];
  active: boolean;
};

function isBoundary(item: Item): boolean {
  if (
    item.kind === "notice" &&
    [
      "status_update",
      "simple_activity_update",
      "context_growth",
      "iteration_stats",
      "context_compacted",
      "iteration_checkpoint",
    ].includes(item.noticeKind || "")
  )
    return false;
  return (
    item.kind === "user" ||
    item.kind === "connector" ||
    item.kind === "question" ||
    item.kind === "dirreq" ||
    item.kind === "planreq" ||
    (item.kind === "approval" && !item.resolved) ||
    item.kind === "notice"
  );
}

function isWorkItem(item: Item): boolean {
  return (
    item.kind === "assistant" ||
    item.kind === "tool" ||
    item.kind === "approval"
  );
}

export function groupWorkSegments(
  items: Item[],
  running = false,
): Array<WorkSegment | { item: Item }> {
  const output: Array<WorkSegment | { item: Item }> = [];
  let segment: Item[] = [];
  const flush = (active = false) => {
    if (segment.length > 0) output.push({ items: segment, active });
    segment = [];
  };
  for (const item of items) {
    if (
      item.kind === "notice" &&
      [
        "status_update",
        "simple_activity_update",
        "context_growth",
        "iteration_stats",
        "context_compacted",
        "iteration_checkpoint",
      ].includes(item.noticeKind || "")
    )
      continue;
    if (isBoundary(item)) {
      flush(false);
      output.push({ item });
      continue;
    }
    if (!isWorkItem(item)) {
      flush(false);
      output.push({ item });
      continue;
    }
    if (
      item.kind === "assistant" &&
      item.text.trim() &&
      segment.some((entry) => entry.kind === "tool")
    ) {
      flush(false);
      output.push({ item });
      continue;
    }
    segment.push(item);
  }
  flush(running);
  return output;
}

export function workSegmentDuration(items: Item[]): number | undefined {
  const timestamps = items.flatMap((item) => {
    if (item.kind === "tool" && item.ts !== undefined) return [item.ts];
    if (item.kind === "assistant" && item.ts !== undefined) return [item.ts];
    return [];
  });
  if (timestamps.length < 2) return undefined;
  return Math.max(
    0,
    Math.round(Math.max(...timestamps) - Math.min(...timestamps)),
  );
}

export function workSegmentDiff(items: Item[]): {
  additions?: number;
  deletions?: number;
} {
  let additions = 0;
  let deletions = 0;
  let hasAdditions = false;
  let hasDeletions = false;
  for (const item of items) {
    if (item.kind !== "tool" || !item.diff) continue;
    if (typeof item.diff.additions === "number") {
      additions += item.diff.additions;
      hasAdditions = true;
    }
    if (typeof item.diff.deletions === "number") {
      deletions += item.diff.deletions;
      hasDeletions = true;
    }
  }
  return {
    additions: hasAdditions ? additions : undefined,
    deletions: hasDeletions ? deletions : undefined,
  };
}
