export type SurfaceRequestTab = "desktop";

export function surfaceTabForWorkingEvent(
  event: unknown,
): SurfaceRequestTab | null {
  if (!event || typeof event !== "object") return null;
  const eventType = (event as { event_type?: unknown }).event_type;
  return eventType === "desktop_view_requested" ? "desktop" : null;
}
