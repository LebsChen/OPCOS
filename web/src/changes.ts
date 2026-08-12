export type DiffLineType = "add" | "del" | "context";

export type ParsedDiffLine = {
  type: DiffLineType;
  content: string;
  oldLine?: number;
  newLine?: number;
};

export type ParsedDiffHunk = {
  header: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  lines: ParsedDiffLine[];
};

export type ParsedUnifiedDiff = {
  hunks: ParsedDiffHunk[];
  isBinary: boolean;
};

const HUNK_HEADER = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: ?.*)$/;

export function parseUnifiedDiff(text: string): ParsedUnifiedDiff {
  const hunks: ParsedDiffHunk[] = [];
  let current: ParsedDiffHunk | null = null;
  let oldLine = 0;
  let newLine = 0;
  let isBinary = false;

  for (const rawLine of text.split("\n")) {
    if (rawLine.startsWith("Binary files ")) {
      isBinary = true;
      continue;
    }
    const header = rawLine.match(HUNK_HEADER);
    if (header) {
      current = {
        header: rawLine,
        oldStart: Number(header[1]),
        oldCount: Number(header[2] ?? 1),
        newStart: Number(header[3]),
        newCount: Number(header[4] ?? 1),
        lines: [],
      };
      oldLine = current.oldStart;
      newLine = current.newStart;
      hunks.push(current);
      continue;
    }
    if (
      !current ||
      rawLine === "" ||
      rawLine === "\\ No newline at end of file"
    )
      continue;

    const prefix = rawLine[0];
    const content = rawLine.slice(1);
    if (prefix === "+") {
      current.lines.push({ type: "add", content, newLine });
      newLine += 1;
    } else if (prefix === "-") {
      current.lines.push({ type: "del", content, oldLine });
      oldLine += 1;
    } else if (prefix === " ") {
      current.lines.push({
        type: "context",
        content,
        oldLine,
        newLine,
      });
      oldLine += 1;
      newLine += 1;
    }
  }

  return { hunks, isBinary };
}
