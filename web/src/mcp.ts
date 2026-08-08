export function mcpPromptMessagesToDraft(messages: unknown[]): string {
  return messages
    .map((message) => {
      if (!message || typeof message !== "object") return "";
      const content = (message as { content?: unknown }).content;
      if (typeof content === "string") return content;
      if (content && typeof content === "object") {
        const text = (content as { text?: unknown }).text;
        if (typeof text === "string") return text;
      }
      return "";
    })
    .filter(Boolean)
    .join("\n\n");
}

export function mcpResourceSummary(resource: Record<string, unknown>): string {
  return `${String(resource.uri || "")} · ${String(resource.mime_type || "unknown")}`;
}
