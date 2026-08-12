import { describe, expect, it } from "vitest";
import { parseUnifiedDiff } from "./changes";

describe("parseUnifiedDiff", () => {
  it("parses hunk headers and advances old and new line numbers", () => {
    const result = parseUnifiedDiff(
      [
        "diff --git a/src/a.ts b/src/a.ts",
        "--- a/src/a.ts",
        "+++ b/src/a.ts",
        "@@ -2,3 +2,4 @@ function main()",
        " keep",
        "-removed",
        "+added",
        "+another",
        " tail",
      ].join("\n"),
    );

    expect(result.hunks).toHaveLength(1);
    expect(result.hunks[0]).toMatchObject({
      oldStart: 2,
      oldCount: 3,
      newStart: 2,
      newCount: 4,
    });
    expect(result.hunks[0].lines).toEqual([
      { type: "context", content: "keep", oldLine: 2, newLine: 2 },
      { type: "del", content: "removed", oldLine: 3 },
      { type: "add", content: "added", newLine: 3 },
      { type: "add", content: "another", newLine: 4 },
      { type: "context", content: "tail", oldLine: 4, newLine: 5 },
    ]);
  });

  it("handles renamed, deleted, empty, and no-newline diffs", () => {
    const renamed = parseUnifiedDiff(
      [
        "diff --git a/old.txt b/new.txt",
        "similarity index 100%",
        "rename from old.txt",
        "rename to new.txt",
      ].join("\n"),
    );
    const deleted = parseUnifiedDiff(
      [
        "--- a/deleted.txt",
        "+++ /dev/null",
        "@@ -1 +0,0 @@",
        "-gone",
        "\\ No newline at end of file",
      ].join("\n"),
    );

    expect(renamed.hunks).toEqual([]);
    expect(deleted.hunks[0].lines).toEqual([
      { type: "del", content: "gone", oldLine: 1 },
    ]);
    expect(parseUnifiedDiff("").hunks).toEqual([]);
  });

  it("marks binary patches without treating metadata as source lines", () => {
    expect(
      parseUnifiedDiff(
        "diff --git a/image.png b/image.png\nBinary files a/image.png and b/image.png differ",
      ),
    ).toMatchObject({ isBinary: true, hunks: [] });
  });
});
