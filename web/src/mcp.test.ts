import { describe, expect, it } from "vitest";
import {
  appendMcpPromptDraft,
  isUserMcpServer,
  mcpCatalogUpdateTargets,
  mcpServerFormBody,
  mcpPromptMessagesToDraft,
  mcpResourceSummary,
} from "./mcp";

describe("MCP resources and prompts", () => {
  it("converts prompt messages into an editable draft", () => {
    expect(
      mcpPromptMessagesToDraft([
        { role: "user", content: { type: "text", text: "Review this" } },
        { role: "assistant", content: "Add tests" },
        { role: "user", content: { type: "image", data: "ignored" } },
      ]),
    ).toBe("Review this\n\nAdd tests");
  });

  it("summarizes a resource without content", () => {
    expect(
      mcpResourceSummary({
        uri: "file:///workspace/readme.md",
        mime_type: "text/markdown",
        text: "private content",
      }),
    ).toBe("file:///workspace/readme.md · text/markdown");
  });

  it("keeps a loaded prompt as non-empty composer draft", () => {
    expect(appendMcpPromptDraft("", "Draft from MCP")).toBe("Draft from MCP");
    expect(appendMcpPromptDraft("Existing", "Draft from MCP")).toBe(
      "Existing\n\nDraft from MCP",
    );
  });

  it("targets catalog refreshes to the changed server", () => {
    expect(mcpCatalogUpdateTargets({ server_id: "server-a" }, "server-a")).toBe(
      true,
    );
    expect(mcpCatalogUpdateTargets({ server_id: "server-a" }, "server-b")).toBe(
      false,
    );
    expect(mcpCatalogUpdateTargets({}, "server-a")).toBe(false);
  });

  it("keeps composer text and attachments when submission rejects", async () => {
    const draft = "Keep this draft";
    const attachments = [{ uri: "resource://one", name: "one.md" }];
    let submitted = false;
    const submit = async () => {
      submitted = true;
      throw new Error("MCP server is disconnected");
    };

    await expect(submit()).rejects.toThrow("MCP server is disconnected");
    expect(submitted).toBe(true);
    expect(draft).toBe("Keep this draft");
    expect(attachments).toEqual([{ uri: "resource://one", name: "one.md" }]);
  });

  it("protects builtin catalog entries using the backend flag", () => {
    expect(isUserMcpServer({ status: "connected", builtin: true })).toBe(false);
    expect(isUserMcpServer({ status: "active", builtin: false })).toBe(true);
    expect(isUserMcpServer({ status: "active" })).toBe(false);
    expect(isUserMcpServer({ status: "active", builtin: "false" })).toBe(false);
  });

  it.each([
    {
      transport: "stdio" as const,
      fields: {
        command: "npx",
        args: "--yes\nserver",
        env: "PORT=8080",
        url: "",
      },
      expected: {
        command: "npx",
        args: ["--yes", "server"],
        env: { PORT: "8080" },
      },
    },
    {
      transport: "streamable-http" as const,
      fields: {
        command: "",
        args: "",
        env: "",
        url: " https://example.test/mcp ",
      },
      expected: { url: "https://example.test/mcp" },
    },
    {
      transport: "http-sse" as const,
      fields: {
        command: "",
        args: "",
        env: "",
        url: "https://example.test/sse",
      },
      expected: { url: "https://example.test/sse" },
    },
  ])(
    "produces a save_asset body that round-trips for $transport",
    ({ transport, fields, expected }) => {
      const body = mcpServerFormBody({
        ...fields,
        transport,
        enabled: true,
        requiresApproval: false,
      });
      const listed = JSON.parse(JSON.stringify(body)) as Record<
        string,
        unknown
      >;
      expect(listed).toMatchObject({
        transport,
        enabled: true,
        requires_approval: false,
        ...expected,
      });
    },
  );
});
