import { describe, expect, it } from "vitest";
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { backendValueKeys, messages } from "./i18n";

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
  "worktreeLabel",
  "panelAgents",
  "artifactCount",
  "artifactsLabel",
  "tokens",
  "hash",
  "cloneExample",
  "ciRepairLoop",
]);
const zhFreeTextKeyAllowlist = new Set([
  "jsonWorkflowExample",
  "jsonRolesExample",
  "jsonEnvelopeExample",
  "cloneExample",
  "ciRepairLoop",
  "workflowFormatHint",
  "environmentPerLine",
  "githubEnterpriseDescription",
  "setupExecutorDescription",
  "commandManagementDescription",
  "issueIdentifierExample",
  "ownerRepository",
]);
const zhEnglishWordAllowlist = new Set([
  "MCP",
  "ACP",
  "IDE",
  "CDP",
  "VNC",
  "SSH",
  "API",
  "URL",
  "JSON",
  "YAML",
  "stdio",
  "HTTP",
  "SSE",
  "Token",
  "Shell",
  "PTY",
  "Git",
  "Cron",
  "Blueprint",
  "Outposts",
  "Agent",
  "Agents",
  "Secret",
  "Persona",
  "Playbook",
  "OPCOS",
  "RVM",
  "DevBox",
  "Slack",
  "Linear",
  "Jira",
  "Dropbox",
  "GitHub",
  "Notion",
  "OpenAI",
  "Google",
  "Microsoft",
  "Telegram",
  "WhatsApp",
  "IMAP",
  "OAuth",
  "GraphQL",
  "RSS",
  "Atom",
  "OpenWorker",
  "CI",
  "SHA",
  "Hash",
  "PNG",
  "PR",
  "English",
  "Bearer",
  "Webhook",
  "Harness",
  "Rust",
  "Cloudflare",
  "Streamable",
  "server",
  "Account",
  "KEY",
  "VALUE",
  "task",
  "BETA",
  "Discord",
  "CRM",
  "Box",
  "Drive",
  "Cloud",
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

  it("does not mix unapproved English words into Chinese translations", () => {
    const violations: string[] = [];
    for (const [key, value] of Object.entries(messages.zh)) {
      if (zhFreeTextKeyAllowlist.has(key)) continue;
      const normalized = value
        .replace(/\{[^}]*\}/g, "")
        .replace(/https?:\/\/\S+/g, "")
        .replace(/(?:^|\s)\.?[\w.-]+\/[\w./-]+/g, " ")
        .replace(/(?:^|\s)[\w.-]+\.[A-Za-z0-9_-]+(?=\s|$|[),.;])/g, " ")
        .replace(/\/[\w./-]+/g, "");
      const words = normalized.match(/[A-Za-z]{3,}/g) || [];
      const unexpected = words.filter(
        (word) =>
          !zhEnglishWordAllowlist.has(word) &&
          !zhEnglishWordAllowlist.has(
            word[0].toUpperCase() + word.slice(1).toLowerCase(),
          ),
      );
      if (unexpected.length) violations.push(`${key}:${unexpected.join(",")}`);
    }
    expect(violations).toEqual([]);
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
    const categoryBlock =
      composerSource.match(/const categories = \[[\s\S]*?\n  \];/)?.[0] ?? "";
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
      ...categoryBlock.matchAll(/label:\s*"([^"]+)"/g),
    ].map((match) => match[1]);
    staticLabels.push("light", "dark", "auto");
    expect(staticLabels.filter((key) => !keys.has(key))).toEqual([]);
    expect(appSource).not.toMatch(/<strong>\{label\}<\/strong>/);
    expect(appSource).not.toMatch(/>\s*\{label\}\s*</);
    expect(composerSource).not.toMatch(/\{current\?\.label\s*\|\|\s*mode\}/);
    expect(appSource).not.toMatch(/`Search \$\{label\}`/);
    expect(appSource).not.toMatch(/searchPlaceholder="Repository index"/);
    expect(appSource).not.toMatch(
      /tool\.enabled === true \? "Enabled" : "Disabled"/,
    );
    expect(appSource).not.toMatch(
      /label:\s*"(Changes|Progress|Tasks|Insights|Artifacts|Agents|Terminal|Desktop)"/,
    );
    expect(appSource).not.toMatch(/String\(server\.url \|\| "configured"\)/);
    expect(appSource).not.toMatch(/` · Tasks: \$\{/);
  });

  it("translates every Activity tab label and subtitle", () => {
    const appSource = readFileSync(`${sourceRoot}App.tsx`, "utf8");
    expect(appSource).not.toMatch(
      /\[0\]\.toUpperCase\(\)\s*\+\s*\w+\.slice\(1\)/,
    );
    expect(appSource).not.toMatch(
      /(?:goals|board|tasks|messages|worklog):\s*"(?:Define|Start|Create|Send|Inspect)/,
    );
    for (const [tab, key] of [
      ["audit", "activityAudit"],
      ["actions", "activityActions"],
      ["queue", "activityQueue"],
      ["events", "activityEvents"],
      ["goals", "activityGoals"],
      ["board", "activityBoard"],
      ["roles", "activityRoles"],
      ["tasks", "activityTasks"],
      ["messages", "activityMessages"],
      ["worklog", "activityWorklog"],
      ["insights", "activityInsights"],
    ]) {
      expect(appSource).toContain(`${tab}: "${key}"`);
    }
    for (const key of [
      "reviewSecurityEvents",
      "reviewExternalActions",
      "reviewWorkQueue",
      "reviewEventRules",
      "reviewGoals",
      "reviewBoard",
      "reviewRoles",
      "reviewTasks",
      "reviewMessages",
      "reviewWorklog",
      "reviewInsights",
    ])
      expect(appSource).toContain(`translate("${key}")`);
  });

  it("keeps panel labels semantically distinct from settings keys", () => {
    const appSource = readFileSync(`${sourceRoot}App.tsx`, "utf8");
    expect(
      appSource.match(/<strong>\{translate\("panelAgents"\)\}<\/strong>/g),
    ).toHaveLength(2);
    expect(appSource).not.toMatch(
      /<strong>\{translate\("agents"\)\}<\/strong>/,
    );
  });

  it("keeps backend template source values stable and non-CJK", () => {
    const backendSource = readFileSync(
      `${sourceRoot}../../src-tauri/src/main.rs`,
      "utf8",
    );
    expect(backendSource).toMatch(/"source":\s*"project"/);
    expect(backendSource).toMatch(/"builtin"/);
    expect(backendSource).toMatch(/"repository"/);
    expect(backendSource).toMatch(/"custom"/);
    expect(backendSource).not.toMatch(/"source"\s*:\s*"[\u4e00-\u9fff]+"/);
    expect(backendSource).not.toMatch(
      /"source":\s*if[\s\S]{0,500}"[\u4e00-\u9fff]+"/,
    );
  });

  it("localizes composer errors and harness labels", () => {
    const guiSource = readFileSync(`${sourceRoot}gui.ts`, "utf8");
    const composerSource = readFileSync(
      `${sourceRoot}components/Composer.tsx`,
      "utf8",
    );
    expect(guiSource).toContain('translate("providerKeyNotConfigured")');
    expect(composerSource).toMatch(
      /option\.id === "builtin"[\s\S]*translate\("builtIn"\)/,
    );
  });

  it("renders insight fields individually with formatted token usage", () => {
    const appSource = readFileSync(`${sourceRoot}App.tsx`, "utf8");
    expect(appSource).toContain("function formatInsightValue");
    expect(appSource).toMatch(
      /key === "token_usage"[\s\S]*translate\("input"\)[\s\S]*translate\("output"\)/,
    );
    expect(appSource).not.toMatch(
      /<pre>\{JSON\.stringify\(insights,\s*null,\s*2\)\}<\/pre>/,
    );
    expect(appSource).toMatch(/formatInsightValue\(key, value\)/);
  });

  it("keeps MCP errors canonical and translated", () => {
    const guiSource = readFileSync(`${sourceRoot}gui.ts`, "utf8");
    const backendSource = readFileSync(
      `${sourceRoot}../../src-tauri/src/main.rs`,
      "utf8",
    );
    expect(guiSource).toContain("translateBackendError");
    expect(backendSource).toContain(
      "The local host does not provide remote MCP tools; bind a remote host.",
    );
    expect(backendSource).not.toContain("本机 host 不提供远程 MCP tools");
  });

  it("localizes environment status and keeps slash seeds language-consistent", () => {
    const appSource = readFileSync(`${sourceRoot}App.tsx`, "utf8");
    const dictionarySource = readFileSync(`${sourceRoot}i18n.ts`, "utf8");
    const backendSource = readFileSync(
      `${sourceRoot}../../src-tauri/src/main.rs`,
      "utf8",
    );
    expect(dictionarySource).toContain('outposts: "前哨"');
    expect(appSource).toContain('translate("localHost")');
    const slashSeeds = backendSource.match(
      /fn builtin_slash_commands\(\)[\s\S]*?fn builtin_control_slash_commands/,
    )?.[0];
    expect(slashSeeds).toBeTruthy();
    expect(slashSeeds).not.toMatch(
      /"\/(?:implement|plan|review|test|think-hard|deploy|pull-project)"[\s\S]*[\u4e00-\u9fff]/,
    );
  });

  it("maps backend status and enum values before rendering", () => {
    const appSource = readFileSync(`${sourceRoot}App.tsx`, "utf8");
    expect(appSource).not.toMatch(/translate\(String\(/);
    expect(appSource).not.toMatch(/\{indexStatus\.status\}/);
    expect(appSource).not.toMatch(
      /String\(server\.status\s*\|\|\s*"configured"\)/,
    );
    expect(appSource).not.toMatch(
      /\{workflow\.status\?\.trim\(\)\s*\|\|\s*translate\("unknownValue"\)\}/,
    );
    expect(appSource).toMatch(/translateBackendValue\(indexStatus\.status\)/);
    expect(appSource).toMatch(
      /translateBackendValue\(\s*server\.status\s*\|\|\s*"configured"\s*,?\s*\)/,
    );
    expect(appSource).toMatch(/translateBackendValue\(workflow\.status\)/);
    expect(appSource).toMatch(/insightFieldLabel\(key\)/);
    expect(appSource).not.toMatch(
      /translateBackendValue\(\s*projectAgentRosterHost/,
    );
    expect(appSource).not.toMatch(
      /translateBackendValue\(\s*(?:selected\.)?(?:host_name|workspace|model|name|branch|worktree|worktree_path)/,
    );
  });

  it("covers every backend-produced session insight field", () => {
    const appSource = readFileSync(`${sourceRoot}App.tsx`, "utf8");
    const backendSource = readFileSync(
      `${sourceRoot}../../src-tauri/src/main.rs`,
      "utf8",
    );
    const insightObject = appSource.match(
      /const insightFieldKeys:[\s\S]*?=\s*\{([\s\S]*?)\n\};/,
    );
    expect(insightObject).not.toBeNull();
    const mappedFields = new Set(
      [...(insightObject?.[1] ?? "").matchAll(/^\s+([a-z_]+):/gm)].map(
        (match) => match[1],
      ),
    );
    const insightsFunction = backendSource.match(
      /fn session_insights[\s\S]*?Ok\(json!\(\{([\s\S]*?)\}\)\)/,
    );
    expect(insightsFunction).not.toBeNull();
    const backendPayload = (insightsFunction?.[1] ?? "").replace(
      /^\s*"session_id":[^\n]*\n/m,
      "",
    );
    const backendFields = new Set(
      [...backendPayload.matchAll(/^\s{8}"([a-z_]+)"\s*:/gm)].map(
        (match) => match[1],
      ),
    );
    expect(backendFields.size).toBeGreaterThan(0);
    expect([...backendFields].filter((key) => !mappedFields.has(key))).toEqual(
      [],
    );
  });

  it("covers every seeded workflow stage in the dictionaries", () => {
    const seedSource = readFileSync(
      `${sourceRoot}../../src-tauri/src/main.rs`,
      "utf8",
    );
    const stages = new Set(
      [...seedSource.matchAll(/"stage"\s*:\s*"([^"]+)"/g)].map(
        (match) => match[1],
      ),
    );
    expect(stages.size).toBeGreaterThan(0);
    const missingMappings = [...stages].filter(
      (stage) => !(stage.toLowerCase() in backendValueKeys),
    );
    expect(missingMappings).toEqual([]);
    const missingEnglish = [...stages].filter((stage) => {
      const key = backendValueKeys[stage.toLowerCase()];
      return !key || !(key in messages.en);
    });
    const missingChinese = [...stages].filter((stage) => {
      const key = backendValueKeys[stage.toLowerCase()];
      return !key || !(key in messages.zh);
    });
    expect(missingEnglish).toEqual([]);
    expect(missingChinese).toEqual([]);
    expect(
      Object.values(backendValueKeys).filter(
        (key) => !(key in messages.en) || !(key in messages.zh),
      ),
    ).toEqual([]);
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
