import { describe, expect, it } from "vitest";
import { mcpPromptMessagesToDraft, mcpResourceSummary } from "./mcp";

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
});
