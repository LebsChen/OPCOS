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

export function appendMcpPromptDraft(current: string, draft: string): string {
  return current ? `${current}\n\n${draft}` : draft;
}

export function mcpCatalogUpdateTargets(
  payload: { server_id?: string },
  serverId: string,
): boolean {
  return Boolean(payload.server_id && payload.server_id === serverId);
}

export function mcpResourceSummary(resource: Record<string, unknown>): string {
  return `${String(resource.uri || "")} · ${String(resource.mime_type || "unknown")}`;
}

export type McpTransport = "stdio" | "streamable-http" | "http-sse";

export function isUserMcpServer(server: Record<string, unknown>): boolean {
  return server.status !== "builtin";
}

export function mcpServerFormBody(fields: {
  transport: McpTransport;
  url: string;
  command: string;
  args: string;
  env: string;
  enabled: boolean;
  requiresApproval: boolean;
}): Record<string, unknown> {
  const env = Object.fromEntries(
    fields.env
      .split("\n")
      .map((line) => line.trim())
      .filter(Boolean)
      .map((line) => {
        const separator = line.indexOf("=");
        return separator < 1
          ? [line, ""]
          : [line.slice(0, separator).trim(), line.slice(separator + 1)];
      }),
  );
  return {
    transport: fields.transport,
    ...(fields.transport === "stdio"
      ? {
          command: fields.command,
          args: fields.args
            .split("\n")
            .map((value) => value.trim())
            .filter(Boolean),
          env,
        }
      : { url: fields.url.trim() }),
    enabled: fields.enabled,
    requires_approval: fields.requiresApproval,
  };
}
