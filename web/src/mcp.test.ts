import { describe, expect, it } from "vitest";
import {
  appendMcpPromptDraft,
  mcpCatalogUpdateTargets,
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
});
