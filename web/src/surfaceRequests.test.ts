import { describe, expect, it } from "vitest";
import {
  surfaceRequestForWorkingEvent,
  surfaceTabForWorkingEvent,
} from "./surfaceRequests";

describe("working event surface request contract", () => {
  it("reads event_type from the WorkingEvent envelope", () => {
    const workingEvent = {
      event_type: "desktop_view_requested",
      category: "surface",
      direction: "outgoing",
      timestamp: "2026-01-01T00:00:00Z",
      payload: {
        call_id: "call-1",
        reason: "inspect the desktop",
      },
    };

    expect(surfaceTabForWorkingEvent(workingEvent)).toBe("desktop");
  });

  it("does not accept the old mismatched payload.event_type shape", () => {
    expect(
      surfaceTabForWorkingEvent({
        category: "surface",
        payload: { event_type: "desktop_view_requested" },
      }),
    ).toBeNull();
  });

  it("gives repeated requests for the same tab distinct signals", () => {
    const event = { event_type: "desktop_view_requested", payload: {} };

    expect(surfaceRequestForWorkingEvent(event, 1)).toEqual({
      tab: "desktop",
      requestId: 1,
    });
    expect(surfaceRequestForWorkingEvent(event, 2)).toEqual({
      tab: "desktop",
      requestId: 2,
    });
  });
});
