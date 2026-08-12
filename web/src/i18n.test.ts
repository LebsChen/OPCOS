import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { messages } from "./i18n";

const sourceRoot = fileURLToPath(new URL(".", import.meta.url));

describe("i18n source coverage", () => {
  it("keeps English and Chinese dictionaries key-identical", () => {
    expect(Object.keys(messages.en).sort()).toEqual(
      Object.keys(messages.zh).sort(),
    );
  });

  it("defines every literal translate key used by the frontend", () => {
    const keys = new Set(Object.keys(messages.en));
    const missing: string[] = [];
    for (const file of ["App.tsx", ...tsxFiles()]) {
      const source = readFileSync(`${sourceRoot}${file}`, "utf8");
      for (const key of source.matchAll(/translate\("([^"]+)"/g)) {
        if (!keys.has(key[1])) missing.push(`${file}:${key[1]}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it("does not leave bare product copy in JSX text or visible attributes", () => {
    const allowlist = [
      { pattern: /^(OPCOS|Esc|A)$/, reason: "brand and keyboard labels" },
      {
        pattern: /^(command|revision|frames|edits|pending|void command)$/,
        reason: "dynamic protocol and artifact fields",
      },
      {
        pattern: /^exit$/,
        reason: "technical shell exit-code label",
      },
    ];
    expect(allowlist.every((entry) => entry.reason.length > 0)).toBe(true);
    const violations: string[] = [];
    for (const file of tsxFiles()) {
      const source = readFileSync(`${sourceRoot}${file}`, "utf8");
      const jsxText = source.replace(/\{[^{}]*\}/g, "");
      for (const match of jsxText.matchAll(
        />\s*([A-Za-z][A-Za-z ]{1,80})\s*</g,
      )) {
        const text = match[1].trim();
        if (
          text &&
          !allowlist.some((entry) => entry.pattern.test(text)) &&
          !/^[A-Z][a-z]+(?:\s+[A-Z][a-z]+)*$/.test(text)
        ) {
          violations.push(`${file}:${text}`);
        }
      }
    }
    expect(violations).toEqual([]);
  });
});

function tsxFiles(): string[] {
  return [
    "App.tsx",
    "components/ApprovalCard.tsx",
    "components/Composer.tsx",
    "components/Markdown.tsx",
    "components/SearchModal.tsx",
    "components/Sidebar.tsx",
    "components/Transcript.tsx",
  ];
}
