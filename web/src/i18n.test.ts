import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { messages } from "./i18n";

const sourceRoot = fileURLToPath(new URL(".", import.meta.url));
const allowlist = [
  // Brand name and technical identifiers are intentionally preserved.
  {
    pattern: /^(OPCOS|Terminal|Desktop|IDE|MCP|CDP|VNC|PR|ACP)$/,
    reason: "technical identifier",
  },
  // Examples teach users the expected format rather than naming a UI action.
  {
    pattern: /^https:\/\/github\.com\/org\/repo\.git$/,
    reason: "repository URL example",
  },
  { pattern: /^ghe\.example\.com$/, reason: "host example" },
  {
    pattern: /^https:\/\/github\.com\/owner\/repository\/pull\/123$/,
    reason: "pull request URL example",
  },
  { pattern: /^owner\/repository$/, reason: "repository slug example" },
  { pattern: /^\/command$/, reason: "slash command example" },
  { pattern: /^NextAPI$/, reason: "provider name example" },
  {
    pattern: /^https:\/\/api\.nextapi\.store\/v1$/,
    reason: "provider URL example",
  },
  { pattern: /^glm-5\.2$/, reason: "model ID example" },
  { pattern: /^https:\/\/example\.com\/mcp$/, reason: "MCP URL example" },
  {
    pattern: /^Describe the outcome this company is pursuing$/,
    reason: "goal prompt example",
  },
  { pattern: /^\{.*\}$/, reason: "JSON payload example" },
  {
    pattern: /^\[\{"id":"leader".*\}\]$/,
    reason: "coordination roles JSON example",
  },
  { pattern: /^(v|· v)$/, reason: "version notation" },
  { pattern: /^(Esc|A|O)$/, reason: "keyboard shortcut label" },
  { pattern: /^(per|s)$/, reason: "duration unit fragment" },
  { pattern: /^\(⌘B\)$/, reason: "keyboard shortcut notation" },
  {
    pattern: /^https:\/\/example\.test\/feed\.xml$/,
    reason: "feed URL example",
  },
];
const zhEnglishKeyAllowlist = new Set([
  "english",
  "mcp",
  "bearerToken",
  "workspacePath",
  "beta",
  "harness",
  "blueprints",
  "yamlBlueprint",
  "outposts",
  "openaiCompatible",
  "cloudflare",
  "linear",
  "streamableHttp",
  "httpSse",
  "stdio",
  "shell",
  "workingEvent",
  "ownerRepository",
  "baseUrl",
  "token",
  "providerId",
  "mcpServerId",
  "accountId",
  "secretsLabel",
  "connectorToken",
  "worktree",
  "worktreeLabel",
  "artifactCount",
  "artifactsLabel",
  "prompts",
  "tokens",
  "hash",
  "cloneExample",
  "ciRepairLoop",
]);
const zhFreeTextKeyAllowlist = new Set([
  "jsonWorkflowExample",
  "jsonRolesExample",
  "jsonEnvelopeExample",
]);

describe("i18n source coverage", () => {
  it("keeps English and Chinese dictionaries key-identical", () => {
    expect(Object.keys(messages.en).sort()).toEqual(
      Object.keys(messages.zh).sort(),
    );
  });

  it("keeps Chinese translations Chinese unless explicitly technical", () => {
    const zhWithoutChinese = Object.entries(messages.zh)
      .filter(
        ([key, value]) =>
          !zhEnglishKeyAllowlist.has(key) &&
          !zhFreeTextKeyAllowlist.has(key) &&
          !/[\u4e00-\u9fff]/.test(value),
      )
      .map(([key, value]) => `${key}:${value}`);
    const englishWithChinese = Object.entries(messages.en)
      .filter(
        ([key, value]) => key !== "chinese" && /[\u4e00-\u9fff]/.test(value),
      )
      .map(([key, value]) => `${key}:${value}`);
    expect(zhWithoutChinese).toEqual([]);
    expect(englishWithChinese).toEqual([]);
  });

  it("defines every literal translate key used by the frontend", () => {
    const keys = new Set(Object.keys(messages.en));
    const missing: string[] = [];
    for (const file of sourceFiles()) {
      const source = readFileSync(`${sourceRoot}${file}`, "utf8");
      for (const key of source.matchAll(/translate\("([^"]+)"/g)) {
        if (!keys.has(key[1])) missing.push(`${file}:${key[1]}`);
      }
    }
    expect(missing).toEqual([]);
  });

  it("keeps static label sources backed by dictionary keys", () => {
    const keys = new Set(Object.keys(messages.en));
    const settingsSource = readFileSync(
      `${sourceRoot}components/SettingsView.tsx`,
      "utf8",
    );
    const approvalSource = readFileSync(
      `${sourceRoot}components/ApprovalCard.tsx`,
      "utf8",
    );
    const composerSource = readFileSync(
      `${sourceRoot}components/Composer.tsx`,
      "utf8",
    );
    const appSource = readFileSync(`${sourceRoot}App.tsx`, "utf8");
    const connectorBlock =
      appSource.match(
        /const OPENWORKER_CONNECTORS:[\s\S]*?\n\];\n\nfunction relativeTime/,
      )?.[0] ?? "";
    const toolVerbBlock =
      approvalSource.match(
        /const TOOL_VERBS[\s\S]*?=\s*\{([\s\S]*?)\};/,
      )?.[1] ?? "";
    const staticLabels = [
      ...settingsSource.matchAll(/label:\s*"([^"]+)"/g),
      ...appSource.matchAll(
        /\["(?:agents|experts|teams|command|knowledge|playbook|mcp|acp-agent|connectors|blueprint|blueprints|snapshots|advanced|outposts)",\s*"([^"]+)"\]/g,
      ),
      ...toolVerbBlock.matchAll(/:\s*"([^"]+)"/g),
      ...connectorBlock.matchAll(/description:\s*"([^"]+)"/g),
    ].map((match) => match[1]);
    staticLabels.push("light", "dark", "auto");
    expect(staticLabels.filter((key) => !keys.has(key))).toEqual([]);
    expect(appSource).not.toMatch(/<strong>\{label\}<\/strong>/);
    expect(appSource).not.toMatch(/>\s*\{label\}\s*</);
    expect(composerSource).not.toMatch(/\{current\?\.label\s*\|\|\s*mode\}/);
  });

  it("does not leave bare product copy in JSX text or visible attributes", () => {
    const violations: string[] = [];
    for (const file of sourceFiles().filter((name) => name.endsWith(".tsx"))) {
      const source = readFileSync(`${sourceRoot}${file}`, "utf8");
      const tree = ts.createSourceFile(
        file,
        source,
        ts.ScriptTarget.Latest,
        true,
        ts.ScriptKind.TSX,
      );
      let currentNode: ts.Node | undefined;
      const visit = (node: ts.Node) => {
        currentNode = node;
        if (ts.isJsxText(node)) {
          checkText(node.getText(tree).trim());
        } else if (
          ts.isJsxAttribute(node) &&
          ts.isIdentifier(node.name) &&
          /^(title|placeholder|aria-label|alt)$/.test(node.name.text) &&
          node.initializer
        ) {
          if (ts.isStringLiteral(node.initializer)) {
            checkText(node.initializer.text.trim());
          } else if (ts.isJsxExpression(node.initializer)) {
            checkExpression(node.initializer.expression);
          }
        } else if (
          ts.isJsxExpression(node) &&
          (ts.isJsxElement(node.parent) || ts.isJsxFragment(node.parent))
        ) {
          checkExpression(node.expression);
        } else if (
          ts.isStringLiteralLike(node) &&
          /[\u4e00-\u9fff]/.test(node.text)
        ) {
          checkText(node.text.trim());
        }
        ts.forEachChild(node, visit);
      };
      const checkText = (text: string) => {
        if (
          text &&
          /[A-Za-z\u4e00-\u9fff]/.test(text) &&
          !allowlist.some((entry) => entry.pattern.test(text))
        ) {
          const position = tree.getLineAndCharacterOfPosition(
            (currentNode ?? tree).getStart(tree),
          );
          violations.push(`${file}:${position.line + 1}:${text}`);
        }
      };
      const checkExpression = (node: ts.Expression | undefined) => {
        if (!node) return;
        if (ts.isStringLiteralLike(node)) {
          checkText(node.text.trim());
        } else if (ts.isNoSubstitutionTemplateLiteral(node)) {
          checkText(node.text.trim());
        } else if (ts.isTemplateExpression(node)) {
          checkText(node.head.text.trim());
          for (const span of node.templateSpans) {
            checkText(span.literal.text.trim());
          }
        } else if (ts.isParenthesizedExpression(node)) {
          checkExpression(node.expression);
        } else if (ts.isConditionalExpression(node)) {
          checkExpression(node.whenTrue);
          checkExpression(node.whenFalse);
        } else if (
          ts.isBinaryExpression(node) &&
          (node.operatorToken.kind === ts.SyntaxKind.QuestionQuestionToken ||
            node.operatorToken.kind === ts.SyntaxKind.BarBarToken)
        ) {
          checkExpression(node.left);
          checkExpression(node.right);
        }
      };
      visit(tree);
    }
    expect(violations).toEqual([]);
  });
});

function sourceFiles(): string[] {
  const files: string[] = [];
  const visit = (directory: string) => {
    for (const entry of readdirSync(`${sourceRoot}${directory}`, {
      withFileTypes: true,
    })) {
      const relative = `${directory}${entry.name}`;
      if (entry.isDirectory()) {
        visit(`${relative}/`);
      } else if (
        /\.(?:tsx|ts)$/.test(entry.name) &&
        !entry.name.endsWith(".test.ts") &&
        !entry.name.endsWith(".test.tsx")
      ) {
        files.push(relative);
      }
    }
  };
  visit("");
  return files;
}
