import { describe, expect, it } from "vitest";
import liveEnvelopes from "../../fixtures/timeline/live-events.json";
import persisted from "../../fixtures/timeline/persisted-events.json";
import {
  buildTimeline,
  mergeEvents,
  TRANSIENT_TIMELINE_EVENT_TYPES,
  type TimelineEvent,
} from "./timeline";

const live = liveEnvelopes.map((entry) => entry.payload as TimelineEvent);
const saved = persisted as TimelineEvent[];

describe("single event-log timeline", () => {
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
});
