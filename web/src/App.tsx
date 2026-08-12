import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  Component,
  FormEvent,
  ReactNode,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type CSSProperties,
} from "react";
import { Terminal } from "@xterm/xterm";
import RFB from "@novnc/novnc";
import "@xterm/xterm/css/xterm.css";
import {
  Host,
  Session,
  Project,
  ProjectAgent,
  projectAgentRosterHost,
  projectAgentRosterValue,
  projectAgentRosterRows,
  SurfaceTab,
  hostFailureMessage,
  hostStatusLabel,
  errorMessage,
  effectiveRunningState,
  mergeSessionsPreservingOptimistic,
  normalizeSession,
  pendingQuestionFromPayload,
  reconcileRunningState,
  redactApproval,
  selectedSessionFromList,
  sessionViewSelection,
  submitFailureMessage,
  shouldRefreshForSessionLifecycleEvent,
  shouldResetSurfaceForSleep,
  shouldRetrySurfaceStart,
  shouldShowSurfaceReconnect,
  shouldShowSurfaceRetry,
  preserveSurfaceTabWhileSleeping,
  surfaceNeedsConnection,
  type PendingQuestionData,
  reconcileSelectedIdAfterRefresh,
  updateSessionRunState,
} from "./gui";
import { isErrorNotice, providerErrorPresentation } from "./transcript";
import {
  buildTimeline,
  latestPlan,
  mergeEvents,
  optimisticUserMessageEvent,
  optimisticUserMessageMatches,
  type TimelineEvent,
} from "./timeline";
import {
  appendMcpPromptDraft,
  isUserMcpServer,
  mcpCatalogUpdateTargets,
  mcpServerFormBody,
  mcpPromptMessagesToDraft,
  mcpResourceSummary,
  type McpTransport,
} from "./mcp";
import { summarizeIterationStats } from "./iterationStats";
import {
  surfaceRequestForWorkingEvent,
  type SurfaceRequestTab,
} from "./surfaceRequests";
import { Sidebar } from "./components/Sidebar";
import { sessionRecoveryAction, sessionStatusLabel } from "./sessionStatus";
import { Transcript } from "./components/Transcript";
import { ApprovalCard, PreviewBlock } from "./components/ApprovalCard";
import { Composer, PlusMenu, SendButton } from "./components/Composer";
import { SelectMenu as OpenWorkerSelectMenu } from "./components/SelectMenu";
import { SettingsView, type SettingsSection } from "./components/SettingsView";
import { Icon } from "./components/Icon";
import { CollectionPage } from "./components/CollectionPage";
import { IntegrationCard } from "./components/IntegrationCard";
import { getLocale, setLocale, subscribeLocale, translate } from "./i18n";
import type { Attachment } from "./types";
import "./openworker-tailwind.css";
import "./openworker-styles.css";
import "./style.css";

type UiEvent = {
  kind: string;
  session_id?: string;
  payload: Record<string, unknown>;
};
type ExternalIngressSource = {
  source_id: string;
  provider: "github" | "rss" | "atom" | string;
  config: Record<string, unknown>;
  enabled: boolean;
  cursor?: string | null;
  initialized: boolean;
  next_attempt_at?: string | null;
  consecutive_failures: number;
  circuit_open_until?: string | null;
  last_success_at?: string | null;
  last_error?: string | null;
};
type ProviderDescriptor = {
  name: string;
  title: string;
  available?: boolean;
  needs_key?: boolean;
  default_base_url?: string | null;
  recommended_model?: string | null;
  fields?: Array<{
    key: string;
    label: string;
    secret: boolean;
    required: boolean;
  }>;
};
type ProviderModelOption = {
  id: string;
  label: string;
  provider: string;
  capabilities: {
    tools: boolean;
    vision: boolean;
    pdf: boolean;
    parallel_tool_calls: boolean;
    streaming: boolean;
    context_window: number | null;
  };
  capabilities_known: boolean;
  likely_non_chat: boolean;
};
type ProviderModelsResponse = {
  models: ProviderModelOption[];
  source: "live" | "fallback";
  fallback_reason?: string | null;
  discovered_at: string;
  cache_hit: boolean;
};
type Asset = {
  id: string;
  kind: "agents" | "instructions" | "knowledge" | "playbook" | "skill" | string;
  title: string;
  body: string;
  trigger: string;
  scope: string;
  scope_kind?: string;
  enabled: boolean;
};
type SecretMetadata = {
  name: string;
  scope: string;
  purpose: string;
  project_id?: string | null;
};
type GitHubInstance = {
  host: string;
  api_base: string;
  token_secret?: string | null;
};
type ConnectorCatalogEntry = {
  name: string;
  description: string;
};
type TokenConnectorStatus = {
  connected: boolean;
  identity?: string;
};
type ConnectorField = {
  key: string;
  label: string;
  type?: "text" | "password" | "url";
  placeholder?: string;
};
type AgentSettings = {
  computer_use: boolean;
  default_agent: string;
  api_default_agent: string;
  default_platform: string;
  batch_limit: number;
  message_usage_limit: number;
  share_prompts_in_prs: boolean;
  require_agent_mention: boolean;
  auto_add_reviewer: boolean;
  reviewer: string;
  open_prs_as: "draft" | "ready";
  responding_to_bots: "ignore" | "respond";
};
type SlashCommand = {
  name: string;
  kind: "system" | "custom";
  body: string;
  scope: string;
  default_body?: string;
  description?: string;
  input?: { hint?: string };
};
type AcpCapabilities = {
  currentModeId?: string | null;
  availableModes: Array<{ id: string; name: string; description?: string }>;
  configOptions: Array<{
    id: string;
    name: string;
    description?: string;
    type: "select" | "boolean";
    currentValue: string | boolean;
    options?: Array<{ value: string; name: string; description?: string }>;
  }>;
  availableCommands: Array<{
    name: string;
    description?: string;
    input?: { hint?: string };
  }>;
};
type AcpSessionEventPayload =
  | {
      kind: "mode_update";
      currentModeId: string;
      availableModes: AcpCapabilities["availableModes"];
    }
  | {
      kind: "config_update";
      configOptions: AcpCapabilities["configOptions"];
    }
  | {
      kind: "commands_update";
      availableCommands: AcpCapabilities["availableCommands"];
    };
type SkillUsageDashboard = {
  skills: Array<{
    name: string;
    path: string;
    source: string;
    calls: number;
    sessions: number;
    last_used: string;
  }>;
  timeline: Array<{ date: string; calls: number }>;
};
type SkillRulesBrowse = {
  skills: Array<{
    name: string;
    path: string;
    content: string;
    source: string;
  }>;
  rules: Array<{ path: string; content: string; source: string }>;
};
type EnvironmentRepository = {
  position: number;
  repository: string;
  setup_command: string;
};
type BlueprintStatus = {
  source: "project" | "global" | "repository";
  content: string;
  value: Record<string, unknown>;
};
type LibraryEntry = {
  id: string;
  kind: string;
  name: string;
  status: "builtin" | "active";
  content: string;
  description: string;
  version: number;
  readonly: boolean;
  source?: string;
};
type ProjectConfigurationTemplate = LibraryEntry & {
  template_id: string;
  source: string;
  applied: boolean;
  overridden: boolean;
  modified: boolean;
};
const TOKEN_CONNECTOR_KINDS = [
  "github",
  "telegram",
  "discord",
  "slack",
  "notion",
  "gitlab",
  "stripe",
  "asana",
  "hubspot",
  "clickup",
  "pagerduty",
  "posthog",
  "apollo.io",
  "hunter",
  "close",
  "attio",
  "clay",
  "figma",
  "descript",
  "monday.com",
  "jira",
  "confluence",
  "zendesk",
  "datadog",
  "mixpanel",
  "amplitude",
] as const;
const OAUTH_CONNECTOR_KINDS = [
  "gmail",
  "google calendar",
  "google drive",
  "outlook",
  "salesforce",
  "quickbooks",
  "docusign",
  "canva",
  "dropbox",
  "box",
] as const;
const CONFIGURABLE_CONNECTOR_KINDS = new Set<string>([
  ...TOKEN_CONNECTOR_KINDS,
  ...OAUTH_CONNECTOR_KINDS,
  "whatsapp",
  "email (imap)",
  "browser",
]);
const CONNECTOR_FIELDS: Record<string, ConnectorField[]> = {
  github: [{ key: "token", label: "PAT", type: "password" }],
  telegram: [{ key: "token", label: "Bot token", type: "password" }],
  discord: [{ key: "token", label: "Bot token", type: "password" }],
  slack: [{ key: "token", label: "Bot token", type: "password" }],
  notion: [
    { key: "token", label: "Internal integration token", type: "password" },
  ],
  gitlab: [
    { key: "token", label: "PAT", type: "password" },
    {
      key: "base_url",
      label: "GitLab URL",
      type: "url",
      placeholder: "https://gitlab.com",
    },
  ],
  stripe: [{ key: "token", label: "Secret key", type: "password" }],
  asana: [{ key: "token", label: "PAT", type: "password" }],
  hubspot: [{ key: "token", label: "Private app token", type: "password" }],
  clickup: [{ key: "token", label: "API token", type: "password" }],
  pagerduty: [{ key: "token", label: "API token", type: "password" }],
  posthog: [
    { key: "token", label: "Personal API key", type: "password" },
    {
      key: "host",
      label: "PostHog host",
      type: "url",
      placeholder: "https://us.posthog.com",
    },
  ],
  "apollo.io": [{ key: "token", label: "API key", type: "password" }],
  hunter: [{ key: "token", label: "API key", type: "password" }],
  close: [{ key: "token", label: "API key", type: "password" }],
  attio: [{ key: "token", label: "API key", type: "password" }],
  clay: [{ key: "token", label: "API key", type: "password" }],
  figma: [{ key: "token", label: "Personal access token", type: "password" }],
  descript: [{ key: "token", label: "API token", type: "password" }],
  "monday.com": [
    { key: "token", label: "Personal API token", type: "password" },
  ],
  gmail: [
    { key: "client_id", label: "OAuth client ID" },
    { key: "client_secret", label: "OAuth client secret", type: "password" },
  ],
  "google calendar": [
    { key: "client_id", label: "OAuth client ID" },
    { key: "client_secret", label: "OAuth client secret", type: "password" },
  ],
  "google drive": [
    { key: "client_id", label: "OAuth client ID" },
    { key: "client_secret", label: "OAuth client secret", type: "password" },
  ],
  outlook: [
    { key: "client_id", label: "Microsoft application client ID" },
    {
      key: "client_secret",
      label: "Microsoft client secret",
      type: "password",
    },
  ],
  salesforce: [
    { key: "client_id", label: "Connected App client ID" },
    {
      key: "client_secret",
      label: "Connected App client secret",
      type: "password",
    },
  ],
  quickbooks: [
    { key: "client_id", label: "OAuth client ID" },
    { key: "client_secret", label: "OAuth client secret", type: "password" },
  ],
  docusign: [
    { key: "client_id", label: "Integration key" },
    { key: "client_secret", label: "Client secret", type: "password" },
  ],
  canva: [{ key: "client_id", label: "OAuth client ID" }],
  dropbox: [
    { key: "client_id", label: "App key" },
    { key: "client_secret", label: "App secret", type: "password" },
  ],
  box: [
    { key: "client_id", label: "OAuth client ID" },
    { key: "client_secret", label: "OAuth client secret", type: "password" },
  ],
  whatsapp: [
    { key: "access_token", label: "Cloud API access token", type: "password" },
    { key: "phone_number_id", label: "Phone number ID" },
    { key: "graph_version", label: "Graph API version", placeholder: "v20.0" },
  ],
  "email (imap)": [
    { key: "imap_host", label: "IMAP host" },
    { key: "imap_port", label: "IMAP port", placeholder: "993" },
    { key: "username", label: "Username" },
    { key: "password", label: "Password", type: "password" },
  ],
  jira: [
    {
      key: "site",
      label: "Atlassian site URL",
      type: "url",
      placeholder: "https://example.atlassian.net",
    },
    { key: "email", label: "Atlassian email", type: "text" },
    { key: "token", label: "API token", type: "password" },
  ],
  confluence: [
    {
      key: "site",
      label: "Atlassian site URL",
      type: "url",
      placeholder: "https://example.atlassian.net",
    },
    { key: "email", label: "Atlassian email", type: "text" },
    { key: "token", label: "API token", type: "password" },
  ],
  zendesk: [
    {
      key: "subdomain",
      label: "Subdomain",
      type: "text",
      placeholder: "your-subdomain",
    },
    { key: "email", label: "Account email", type: "text" },
    { key: "token", label: "API token", type: "password" },
  ],
  datadog: [
    {
      key: "site",
      label: "Datadog site",
      type: "text",
      placeholder: "datadoghq.com",
    },
    { key: "api_key", label: "API key", type: "password" },
    { key: "app_key", label: "Application key", type: "password" },
  ],
  mixpanel: [
    { key: "service_user", label: "Service account user", type: "text" },
    {
      key: "service_secret",
      label: "Service account secret",
      type: "password",
    },
  ],
  amplitude: [
    { key: "api_key", label: "API key", type: "password" },
    { key: "secret_key", label: "Secret key", type: "password" },
  ],
};
type InboxRecord = {
  session_id: string;
  call_id: string;
  kind: string;
  tool: string;
  payload: Record<string, unknown>;
  state: string;
  created_at: string;
  resolution?: string | null;
};
type PendingQuestion = PendingQuestionData;
type PendingApproval = {
  callId: string;
  name: string;
  args: Record<string, unknown>;
};

const OPENWORKER_CONNECTORS: ConnectorCatalogEntry[] = [
  { name: "Telegram", description: "Two-way messaging with a Telegram bot." },
  {
    name: "Slack",
    description:
      "Two-way messaging through a Slack app or managed workspace connection.",
  },
  {
    name: "Email (IMAP)",
    description: "Read, search, and send mail from an IMAP account.",
  },
  { name: "Gmail", description: "Search, summarize, draft, and send email." },
  {
    name: "Google Calendar",
    description: "Read availability, summarize schedules, and create events.",
  },
  {
    name: "Browser",
    description: "Navigate, read, and act on websites with approval.",
  },
  {
    name: "GitHub",
    description: "Work with issues, pull requests, files, and CI status.",
  },
  {
    name: "Outlook",
    description: "Manage Microsoft 365 mail and calendar.",
  },
  {
    name: "Jira",
    description: "Search, summarize, create, and update issues.",
  },
  {
    name: "monday.com",
    description: "Read boards and items, track work, and post updates.",
  },
  {
    name: "Confluence",
    description: "Search spaces, read pages, and draft documentation.",
  },
  {
    name: "Zendesk",
    description:
      "Search tickets, summarize customer context, and draft replies.",
  },
  {
    name: "Linear",
    description: "Search, read, and create Linear issues.",
  },
  {
    name: "GitLab",
    description: "Work with issues and merge requests.",
  },
  {
    name: "Discord",
    description: "Read channels and send messages through a Discord bot.",
  },
  {
    name: "Stripe",
    description: "Read customers, charges, and invoices.",
  },
  {
    name: "Asana",
    description: "Search, read, create, update, and comment on tasks.",
  },
  {
    name: "HubSpot",
    description: "Search CRM records and update notes and tasks.",
  },
  {
    name: "Dropbox",
    description: "Search, browse, and read files in Dropbox.",
  },
  {
    name: "Box",
    description: "Search, browse, and read files in Box.",
  },
  {
    name: "WhatsApp",
    description: "Send WhatsApp messages through the official Cloud API.",
  },
  {
    name: "QuickBooks",
    description: "Read customers, invoices, and financial reports.",
  },
  {
    name: "Datadog",
    description: "Pull firing alerts, monitors, and incident timelines.",
  },
  {
    name: "Salesforce",
    description: "Read and update cases, accounts, and opportunities.",
  },
  {
    name: "Docusign",
    description: "Track agreements and send documents for signature.",
  },
  {
    name: "ClickUp",
    description: "Search tasks and docs; create and update items.",
  },
  {
    name: "Google Drive",
    description: "Search, browse, and read files in Google Drive.",
  },
  {
    name: "Canva",
    description: "Browse, create, and export designs.",
  },
  {
    name: "Figma",
    description: "Read design files and comments; export assets.",
  },
  {
    name: "Descript",
    description: "Read and edit audio and video projects through transcripts.",
  },
  {
    name: "Clay",
    description: "Enrich people and companies for research workflows.",
  },
  {
    name: "Close",
    description: "Read and update leads, contacts, and opportunities.",
  },
  {
    name: "Notion",
    description:
      "Search pages, read content, query databases, and create pages.",
  },
  {
    name: "Attio",
    description: "Read CRM objects, records, and notes.",
  },
  {
    name: "PostHog",
    description: "Query product analytics, events, funnels, and insights.",
  },
  {
    name: "Mixpanel",
    description: "Query events and segmentation data.",
  },
  {
    name: "Amplitude",
    description: "Query product analytics and chart data.",
  },
  {
    name: "Apollo.io",
    description: "Enrich people and companies and search the B2B database.",
  },
  {
    name: "Hunter",
    description: "Find and verify professional email addresses.",
  },
  {
    name: "PagerDuty",
    description: "See on-call schedules and review active incidents.",
  },
];

function relativeTime(value: string): string {
  const elapsed = Date.now() - new Date(value).getTime();
  const minutes = Math.max(0, Math.round(elapsed / 60_000));
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.round(hours / 24)}d ago`;
}
type Schedule = {
  id: string;
  name: string;
  session_id: string;
  playbook_id: string;
  cron: string;
  enabled: boolean;
  last_run?: string;
  last_result?: string;
  trigger?: string;
};
type Coordination = {
  task_id: string;
  roles: Array<Record<string, unknown>>;
  tasks: Array<Record<string, unknown>>;
  messages: Array<Record<string, unknown>>;
};

type RailIconName =
  | "info"
  | "branch"
  | "list"
  | "sparkle"
  | "diff"
  | "terminal"
  | "shell"
  | "changes"
  | "progress"
  | "tasks"
  | "agents"
  | "monitor"
  | "code"
  | "grid"
  | "globe"
  | "file"
  | "refresh"
  | "back";

function RailIcon({
  name,
  size = 16,
  className,
}: {
  name: RailIconName;
  size?: number;
  className?: string;
}) {
  const s = {
    width: size,
    height: size,
    viewBox: "0 0 24 24",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 2,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    className,
    "aria-hidden": true,
  };

  switch (name) {
    case "info":
      return (
        <svg {...s}>
          <circle cx="12" cy="12" r="10" />
          <line x1="12" y1="16" x2="12" y2="12" />
          <line x1="12" y1="8" x2="12.01" y2="8" />
        </svg>
      );
    case "branch":
      return (
        <svg {...s}>
          <line x1="6" y1="3" x2="6" y2="15" />
          <circle cx="18" cy="6" r="3" />
          <circle cx="6" cy="18" r="3" />
          <path d="M18 9a9 9 0 0 1-9 9" />
        </svg>
      );
    case "list":
      return (
        <svg {...s}>
          <line x1="8" y1="6" x2="21" y2="6" />
          <line x1="8" y1="12" x2="21" y2="12" />
          <line x1="8" y1="18" x2="21" y2="18" />
          <line x1="3" y1="6" x2="3.01" y2="6" />
          <line x1="3" y1="12" x2="3.01" y2="12" />
          <line x1="3" y1="18" x2="3.01" y2="18" />
        </svg>
      );
    case "progress":
      return (
        <svg {...s}>
          <circle cx="12" cy="12" r="9" />
          <path d="m8 12 2.5 2.5L16 9" />
        </svg>
      );
    case "tasks":
      return (
        <svg {...s}>
          <rect x="4" y="4" width="16" height="16" rx="2" />
          <path d="m8 9 1.5 1.5L12 8" />
          <line x1="13.5" y1="9.5" x2="17" y2="9.5" />
          <path d="m8 14 1.5 1.5L12 13" />
          <line x1="13.5" y1="14.5" x2="17" y2="14.5" />
        </svg>
      );
    case "agents":
      return (
        <svg {...s}>
          <circle cx="9" cy="8" r="3" />
          <circle cx="17" cy="10" r="2.5" />
          <path d="M3.5 20a5.5 5.5 0 0 1 11 0" />
          <path d="M14 19a4 4 0 0 1 6.5-3" />
        </svg>
      );
    case "sparkle":
      return (
        <svg {...s}>
          <path d="M12 3l1.9 5.7a2 2 0 0 0 1.3 1.3L21 12l-5.8 1.9a2 2 0 0 0-1.3 1.3L12 21l-1.9-5.8a2 2 0 0 0-1.3-1.3L3 12l5.8-1.9a2 2 0 0 0 1.3-1.3z" />
        </svg>
      );
    case "diff":
      return (
        <svg {...s}>
          <line x1="12" y1="3" x2="12" y2="9" />
          <line x1="9" y1="6" x2="15" y2="6" />
          <line x1="9" y1="18" x2="15" y2="18" />
          <line x1="4" y1="12" x2="20" y2="12" />
        </svg>
      );
    case "terminal":
      return (
        <svg {...s}>
          <polyline points="4 17 10 11 4 5" />
          <line x1="12" y1="19" x2="20" y2="19" />
        </svg>
      );
    case "monitor":
      return (
        <svg {...s}>
          <rect x="2" y="3" width="20" height="14" rx="2" ry="2" />
          <line x1="8" y1="21" x2="16" y2="21" />
          <line x1="12" y1="17" x2="12" y2="21" />
        </svg>
      );
    case "code":
      return (
        <svg {...s}>
          <polyline points="16 18 22 12 16 6" />
          <polyline points="8 6 2 12 8 18" />
        </svg>
      );
    case "grid":
      return (
        <svg {...s}>
          <rect x="3" y="3" width="7" height="7" />
          <rect x="14" y="3" width="7" height="7" />
          <rect x="14" y="14" width="7" height="7" />
          <rect x="3" y="14" width="7" height="7" />
        </svg>
      );
    case "globe":
      return (
        <svg {...s}>
          <circle cx="12" cy="12" r="10" />
          <line x1="2" y1="12" x2="22" y2="12" />
          <path d="M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z" />
        </svg>
      );
    case "file":
      return (
        <svg {...s}>
          <path d="M5 3h9l5 5v13H5z" />
          <path d="M14 3v6h6" />
          <line x1="8" y1="13" x2="16" y2="13" />
          <line x1="8" y1="17" x2="16" y2="17" />
        </svg>
      );
    case "refresh":
      return (
        <svg {...s}>
          <path d="M20 11a8 8 0 0 0-14.5-4L4 9" />
          <path d="M4 4v5h5" />
          <path d="M4 13a8 8 0 0 0 14.5 4L20 15" />
          <path d="M20 20v-5h-5" />
        </svg>
      );
    case "back":
      return (
        <svg {...s}>
          <path d="m15 18-6-6 6-6" />
        </svg>
      );
  }
}

async function command<T>(
  name: string,
  args?: Record<string, unknown>,
): Promise<T> {
  if (
    !(window as Window & { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__
  ) {
    // Browser/CDP preview has no desktop command bridge. Keep preview effects
    // inert instead of surfacing an environment-only invoke error to users.
    return new Promise<T>(() => {});
  }
  return invoke<T>(name, args);
}

type HarnessOption = {
  id: string;
  label: string;
  available: boolean;
  reason?: string;
};

function ProjectDialog({
  hosts,
  onClose,
  onSubmit,
}: {
  hosts: Host[];
  onClose: () => void;
  onSubmit: (values: {
    name: string;
    hostId: string;
    repoUrl: string;
    repoRoot: string;
    defaultBranch: string;
    teamTemplateId: string;
  }) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [hostId, setHostId] = useState(hosts[0]?.id || "");
  const [repoUrl, setRepoUrl] = useState("");
  const [repoRoot, setRepoRoot] = useState("");
  const [defaultBranch, setDefaultBranch] = useState("main");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [teamTemplates, setTeamTemplates] = useState<LibraryEntry[]>([]);
  const [teamTemplateId, setTeamTemplateId] = useState("");
  useEffect(() => {
    void command<LibraryEntry[]>("list_configured_library", {
      kind: "team-template",
    }).then(setTeamTemplates);
  }, []);
  const selectedTeam = teamTemplates.find((item) => item.id === teamTemplateId);
  const selectedTeamContent = selectedTeam
    ? (() => {
        try {
          return JSON.parse(selectedTeam.content) as {
            agents?: Array<{ name?: string; role?: string }>;
            workflow?: unknown;
          };
        } catch {
          return null;
        }
      })()
    : null;
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!name.trim() || !hostId) {
      setError(translate("projectNameRequired"));
      return;
    }
    setSaving(true);
    setError("");
    try {
      await onSubmit({
        name: name.trim(),
        hostId,
        repoUrl: repoUrl.trim(),
        repoRoot: repoRoot.trim(),
        defaultBranch: defaultBranch.trim() || "main",
        teamTemplateId,
      });
    } catch (reason) {
      setError(errorMessage(reason));
      setSaving(false);
    }
  };
  return (
    <div className="fixed inset-0 z-50 grid place-items-center bg-black/30 p-4">
      <form
        className="flex max-h-[90vh] w-full max-w-lg flex-col rounded-xl border border-line bg-panel p-6 shadow-xl"
        onSubmit={submit}
      >
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-semibold text-ink">
            {translate("newProjectTitle")}
          </h2>
          <button type="button" className="btn" onClick={onClose}>
            {translate("close")}
          </button>
        </div>
        <div className="mt-5 min-h-0 flex-1 overflow-y-auto pr-1">
          <div className="grid gap-3">
            <label className="field-label">
              {translate("name")}
              <input
                autoFocus
                className="input"
                value={name}
                onChange={(event) => setName(event.target.value)}
                placeholder={translate("projectName")}
              />
            </label>
            <label className="field-label">
              {translate("host")}
              <select
                className="input"
                value={hostId}
                onChange={(event) => setHostId(event.target.value)}
              >
                {hosts.map((host) => (
                  <option key={host.id} value={host.id}>
                    {host.name}
                  </option>
                ))}
              </select>
            </label>
            <label className="field-label">
              {translate("repositoryUrlOptional")}
              <input
                className="input"
                value={repoUrl}
                onChange={(event) => setRepoUrl(event.target.value)}
                placeholder="https://github.com/org/repo.git"
              />
            </label>
            <label className="field-label">
              {translate("repositoryPathOptional")}
              <input
                className="input"
                value={repoRoot}
                onChange={(event) => setRepoRoot(event.target.value)}
                placeholder={translate("repositoryPathDefault")}
              />
            </label>
            <label className="field-label">
              {translate("defaultBranch")}
              <input
                className="input"
                value={defaultBranch}
                onChange={(event) => setDefaultBranch(event.target.value)}
              />
            </label>
            <label className="field-label">
              {translate("teamTemplateOptional")}
              <select
                className="input"
                value={teamTemplateId}
                onChange={(event) => setTeamTemplateId(event.target.value)}
              >
                <option value="">{translate("noTeamTemplate")}</option>
                {teamTemplates.map((template) => (
                  <option key={template.id} value={template.id}>
                    {template.name} ·{" "}
                    {template.status === "builtin"
                      ? translate("builtIn")
                      : translate("custom")}
                  </option>
                ))}
              </select>
            </label>
            {selectedTeamContent && (
              <div className="rounded-lg border border-line p-3 text-sm">
                <strong>{translate("membersToCreate")}</strong>
                <div className="mt-1">
                  {(selectedTeamContent.agents || [])
                    .map(
                      (agent) =>
                        `${agent.name || translate("member")}（${agent.role || translate("worker")}）`,
                    )
                    .join("、")}
                </div>
                <small className="text-muted">
                  {translate("workflow")}：{" "}
                  {JSON.stringify(selectedTeamContent.workflow)}
                </small>
              </div>
            )}
          </div>
        </div>
        {error && <p className="mt-3 text-sm text-danger">{error}</p>}
        <div className="sticky bottom-0 mt-6 flex justify-end gap-2 bg-panel pt-1">
          <button type="button" className="btn" onClick={onClose}>
            {translate("cancel")}
          </button>
          <button
            type="submit"
            className="btn approval-primary"
            disabled={saving}
          >
            {saving
              ? translate("creating")
              : translate("createProject")}
          </button>
        </div>
      </form>
    </div>
  );
}

function MemberDialog({
  mode,
  agent,
  providers,
  models,
  harnessOptions,
  saving,
  onClose,
  onSubmit,
}: {
  mode: "add" | "edit";
  agent?: ProjectAgent;
  providers: ProviderDescriptor[];
  models: Array<{ id: string; label: string }>;
  harnessOptions: HarnessOption[];
  saving: boolean;
  onClose: () => void;
  onSubmit: (values: {
    name: string;
    role: string;
    provider: string;
    model: string;
    harness: string;
    mode: string;
    branch: string;
    state: string;
  }) => Promise<void>;
}) {
  const [name, setName] = useState(agent?.name || "");
  const [role, setRole] = useState(agent?.role || "Code");
  const [provider, setProvider] = useState(agent?.provider || "");
  const [model, setModel] = useState(agent?.model || "auto");
  const [harness, setHarness] = useState(agent?.harness || "builtin");
  const [sessionMode, setSessionMode] = useState(agent?.mode || "Auto");
  const [branch, setBranch] = useState(agent?.branch || "");
  const [state, setState] = useState(agent?.state || "Active");
  const [error, setError] = useState("");
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!name.trim() || !role.trim()) {
      setError(translate("memberNameRoleRequired"));
      return;
    }
    setError("");
    try {
      await onSubmit({
        name: name.trim(),
        role: role.trim(),
        provider,
        model,
        harness,
        mode: sessionMode,
        branch: branch.trim(),
        state,
      });
    } catch (reason) {
      setError(errorMessage(reason));
    }
  };
  return (
    <div className="fixed inset-0 z-50 grid place-items-center bg-black/30 p-4">
      <form
        className="w-full max-w-lg rounded-xl border border-line bg-panel p-6 shadow-xl"
        onSubmit={submit}
      >
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-semibold text-ink">
            {mode === "add"
              ? translate("addingMember")
              : translate("editingMember")}
          </h2>
          <button type="button" className="btn" onClick={onClose}>
            {translate("close")}
          </button>
        </div>
        <div className="mt-5 grid gap-3">
          <label className="field-label">
            {translate("name")}
            <input
              autoFocus
              className="input"
              value={name}
              onChange={(event) => setName(event.target.value)}
            />
          </label>
          <label className="field-label">
            {translate("role")}
            <input
              className="input"
              list="project-agent-roles"
              value={role}
              onChange={(event) => setRole(event.target.value)}
            />
            <datalist id="project-agent-roles">
              {["Lead", "Code", "Review", "Test", "DevOps"].map((item) => (
                <option key={item} value={item} />
              ))}
            </datalist>
          </label>
          {mode === "add" ? (
            <>
              <label className="field-label">
                Provider
                <select
                  className="input"
                  value={provider}
                  onChange={(event) => setProvider(event.target.value)}
                >
                  <option value="">{translate("defaultValue")}</option>
                  {providers.map((item) => (
                    <option
                      key={item.name}
                      value={item.name}
                      disabled={item.available === false}
                    >
                      {item.title}
                    </option>
                  ))}
                </select>
              </label>
              <label className="field-label">
                {translate("modelLabel")}
                <select
                  className="input"
                  value={model}
                  onChange={(event) => setModel(event.target.value)}
                >
                  <option value="auto">{translate("auto")}</option>
                  {models.map((item) => (
                    <option key={item.id} value={item.id}>
                      {item.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="field-label">
                Harness
                <select
                  className="input"
                  value={harness}
                  onChange={(event) => setHarness(event.target.value)}
                >
                  {(harnessOptions.length
                    ? harnessOptions
                    : [{ id: "builtin", label: "Builtin", available: true }]
                  ).map((item) => (
                    <option
                      key={item.id}
                      value={item.id}
                      disabled={!item.available}
                    >
                      {item.label}
                    </option>
                  ))}
                </select>
              </label>
              <label className="field-label">
                {translate("modeLabel")}
                <select
                  className="input"
                  value={sessionMode}
                  onChange={(event) => setSessionMode(event.target.value)}
                >
                  <option value="Interactive">
                    {translate("interactive")}
                  </option>
                  <option value="Auto">{translate("auto")}</option>
                </select>
              </label>
              <label className="field-label">
                {translate("branchOptional")}
                <input
                  className="input"
                  value={branch}
                  onChange={(event) => setBranch(event.target.value)}
                  placeholder={translate("autoNameByRole")}
                />
              </label>
            </>
          ) : (
            <label className="field-label">
              {translate("state")}
              <select
                className="input"
                value={state}
                onChange={(event) => setState(event.target.value)}
              >
                <option value="Active">{translate("active")}</option>
                <option value="Sleep">{translate("sleep")}</option>
                <option value="Paused">{translate("paused")}</option>
              </select>
            </label>
          )}
        </div>
        {error && <p className="mt-3 text-sm text-danger">{error}</p>}
        <div className="mt-6 flex justify-end gap-2">
          <button type="button" className="btn" onClick={onClose}>
            {translate("cancel")}
          </button>
          <button
            type="submit"
            className="btn approval-primary"
            disabled={saving}
          >
            {saving ? translate("saving") : translate("save")}
          </button>
        </div>
      </form>
    </div>
  );
}

function ProjectConfigPanel({
  project,
  onError,
  onRefresh,
}: {
  project: Project;
  onError: (error: unknown) => void;
  onRefresh: () => Promise<void>;
}) {
  const [assets, setAssets] = useState<Asset[]>([]);
  const [secrets, setSecrets] = useState<SecretMetadata[]>([]);
  const [kind, setKind] = useState<
    | "agents"
    | "experts"
    | "teams"
    | "command"
    | "knowledge"
    | "playbook"
    | "mcp"
    | "acp-agent"
    | "connectors"
    | "blueprint"
  >("agents");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [editingId, setEditingId] = useState<string | null>(null);
  const [configurationTemplates, setConfigurationTemplates] = useState<
    ProjectConfigurationTemplate[]
  >([]);
  const [secretName, setSecretName] = useState("");
  const [secretPurpose, setSecretPurpose] = useState("");
  const [secretValue, setSecretValue] = useState("");
  const [secretFormOpen, setSecretFormOpen] = useState(false);
  const [providerName, setProviderName] = useState("");
  const [providerKey, setProviderKey] = useState("");
  const [mcpServerId, setMcpServerId] = useState("");
  const [mcpCredential, setMcpCredential] = useState("");
  const [connectorKind, setConnectorKind] = useState("");
  const [connectorToken, setConnectorToken] = useState("");
  const [githubInstances, setGithubInstances] = useState<GitHubInstance[]>([]);
  const [githubHost, setGithubHost] = useState("");
  const [githubApiBase, setGithubApiBase] = useState("");
  const loadGithubInstances = () =>
    command<GitHubInstance[]>("list_github_enterprise_instances")
      .then(setGithubInstances)
      .catch(onError);
  const load = async () => {
    const [nextAssets, nextSecrets] = await Promise.all([
      command<Asset[]>("list_assets", { projectId: project.id }),
      command<SecretMetadata[]>("list_secret_metadata", {
        projectId: project.id,
      }),
    ]);
    setAssets(nextAssets);
    setSecrets(nextSecrets);
    const nextTemplates = await command<ProjectConfigurationTemplate[]>(
      "list_project_configuration_templates",
      { projectId: project.id },
    );
    setConfigurationTemplates(nextTemplates);
  };
  useEffect(() => {
    void load().catch(onError);
    void loadGithubInstances();
  }, [project.id]);
  const reset = () => {
    setTitle("");
    setBody("");
    setEditingId(null);
  };
  const save = () => {
    void command("save_asset", {
      id: editingId || `project-${project.id}-${kind}-${Date.now()}`,
      kind,
      title: title.trim() || kind,
      body,
      trigger: null,
      scope: project.id,
      scopeKind: "project",
      projectId: project.id,
      enabled: true,
    })
      .then(load)
      .then(onRefresh)
      .then(reset)
      .catch(onError);
  };
  const toggleConfigurationTemplate = async (
    template: ProjectConfigurationTemplate,
    enabled: boolean,
  ) => {
    if (
      enabled &&
      template.modified &&
      !window.confirm(
        `${translate("locallyModified")}：${template.name}。${translate("confirmOverwriteTemplate")}`,
      )
    ) {
      return;
    }
    if (
      !enabled &&
      !window.confirm(
        `${translate("excludeGlobalPreset")}：${template.name}。`,
      )
    ) {
      return;
    }
    try {
      await command("set_project_configuration_template", {
        projectId: project.id,
        templateId: template.template_id,
        enabled,
      });
      await load();
      await onRefresh();
    } catch (reason) {
      onError(reason);
    }
  };
  return (
    <section className="mt-8 rounded-xl border border-line bg-panel p-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold text-ink">
            {translate("projectConfig")}
          </h2>
          <p className="mt-1 text-sm text-faint">
            {translate("projectConfigDescription")}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          {[
            ["agents", translate("rules")],
            ["experts", translate("experts")],
            ["teams", translate("teams")],
            ["command", "Command"],
            ["knowledge", "Knowledge"],
            ["playbook", "Playbook"],
            ["mcp", "MCP"],
            ["acp-agent", "ACP agents"],
            ["connectors", "Connectors"],
            ["blueprint", "Blueprint"],
          ].map(([value, label]) => (
            <button
              key={value}
              className={`btn ${kind === value ? "approval-primary" : ""}`}
              onClick={() => {
                setKind(value as typeof kind);
                reset();
              }}
            >
              {label}
            </button>
          ))}
        </div>
      </div>
      <div className="mt-5 grid gap-3">
        <fieldset className="rounded-lg border border-line p-3">
          <legend className="px-1 text-sm font-medium">
            {translate("globalPresets")}
          </legend>
          <div className="grid gap-2">
            {configurationTemplates
              .filter((template) => {
                const selectedKind = {
                  agents: "rules",
                  experts: "agent-template",
                  teams: "team-template",
                  command: "command",
                  knowledge: "knowledge",
                  playbook: "runbook",
                  mcp: "mcp",
                  "acp-agent": "acp-agent",
                  connectors: "connector",
                  blueprint: "blueprint",
                }[kind];
                return template.kind === selectedKind;
              })
              .map((template) => (
                <div
                  key={template.template_id}
                  className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-start gap-3 rounded-md border border-line/70 px-3 py-2 text-sm"
                >
                  <label className="flex items-center gap-2 pt-1">
                    <input
                      type="checkbox"
                      aria-label={translate("enableTemplate", {
                        name: template.name,
                      })}
                      style={{ width: 16, height: 16 }}
                      checked={template.applied}
                      onChange={(event) =>
                        void toggleConfigurationTemplate(
                          template,
                          event.target.checked,
                        )
                      }
                    />
                  </label>
                  <div className="min-w-0">
                    <strong className="block break-words text-ink">
                      {template.name}
                    </strong>
                    <small className="mt-1 block break-words text-faint">
                      {template.source} ·{" "}
                      {template.overridden
                        ? translate("projectOverridden")
                        : translate("inheritedGlobalPreset")}
                      {template.modified
                        ? ` · ${translate("locallyModified")}`
                        : ""}
                      {template.overridden ? ` · ${translate("editBelow")}` : ""}
                    </small>
                  </div>
                  <div className="flex shrink-0 gap-2">
                    {template.overridden && (
                      <button
                        type="button"
                        className="btn"
                        onClick={() => {
                          if (
                            window.confirm(
                              `将删除项目覆盖「${template.name}」，恢复继承全局预设。确定继续吗？`,
                            )
                          ) {
                            void command("restore_project_configuration", {
                              projectId: project.id,
                              templateId: template.template_id,
                            })
                              .then(load)
                              .then(onRefresh)
                              .catch(onError);
                          }
                        }}
                      >
                        {translate("restoreInheritance")}
                      </button>
                    )}
                    {!template.overridden && (
                      <button
                        type="button"
                        className="btn"
                        onClick={() => {
                          void command("override_project_configuration", {
                            projectId: project.id,
                            templateId: template.template_id,
                          })
                            .then(load)
                            .then(onRefresh)
                            .catch(onError);
                        }}
                      >
                        {translate("createProjectOverride")}
                      </button>
                    )}
                  </div>
                </div>
              ))}
            {!configurationTemplates.some((template) => {
              const selectedKind = {
                agents: "rules",
                experts: "agent-template",
                teams: "team-template",
                command: "command",
                knowledge: "knowledge",
                playbook: "runbook",
                mcp: "mcp",
                "acp-agent": "acp-agent",
                connectors: "connector",
                blueprint: "blueprint",
              }[kind];
              return template.kind === selectedKind;
            }) && (
              <span className="text-xs text-faint">
                {translate("noConfigTemplates")}
              </span>
            )}
          </div>
        </fieldset>
        {!["experts", "teams", "command"].includes(kind) &&
          assets
            .filter((asset) => asset.kind === kind)
            .map((asset) => (
              <div
                className="flex items-start justify-between gap-3 rounded-lg border border-line p-3"
                key={asset.id}
              >
                <div className="min-w-0">
                  <strong className="text-ink">{asset.title}</strong>
                  <p className="mt-1 whitespace-pre-wrap break-words text-xs text-faint">
                    {asset.body}
                  </p>
                </div>
                <div className="flex shrink-0 gap-2">
                  <button
                    className="btn"
                    onClick={() => {
                      setEditingId(asset.id);
                      setTitle(asset.title);
                      setBody(asset.body);
                    }}
                  >
                    {translate("edit")}
                  </button>
                  <button
                    className="btn"
                    onClick={() =>
                      command("delete_asset", { id: asset.id })
                        .then(load)
                        .catch(onError)
                    }
                  >
                    {translate("delete")}
                  </button>
                </div>
              </div>
            ))}
        {!["experts", "teams", "command"].includes(kind) && (
          <div className="grid gap-3 rounded-lg border border-line p-4">
            <label className="field-label">
              {translate("name")}
              <input
                value={title}
                onChange={(event) => setTitle(event.target.value)}
                placeholder={translate("configureName")}
              />
            </label>
            <label className="field-label">
              {translate("body")}
              <textarea
                value={body}
                onChange={(event) => setBody(event.target.value)}
                placeholder={
                  kind === "blueprint"
                    ? "clone:\n  - git fetch"
                    : translate("projectConfigContent")
                }
              />
            </label>
            <div>
              <button className="btn approval-primary" onClick={save}>
                {editingId
                  ? translate("saveChanges")
                  : translate("addConfig")}
              </button>
              {editingId && (
                <button className="btn ml-2" onClick={reset}>
                  {translate("cancelEdit")}
                </button>
              )}
            </div>
          </div>
        )}
      </div>
      <div className="mt-6 border-t border-line pt-5">
        <div className="flex items-center justify-between">
          <div>
            <h3 className="font-medium text-ink">
              {translate("projectSecrets")}
            </h3>
            <p className="mt-1 text-xs text-faint">
              {translate("projectSecretsDescription")}
            </p>
          </div>
          <button
            className="btn"
            onClick={() => setSecretFormOpen((value) => !value)}
          >
            {secretFormOpen ? translate("cancel") : translate("addSecret")}
          </button>
        </div>
        {secretFormOpen && (
          <div className="mt-3 grid gap-3 rounded-lg border border-line p-4">
            <input
              value={secretName}
              onChange={(event) => setSecretName(event.target.value)}
              placeholder={translate("secretName")}
            />
            <input
              value={secretPurpose}
              onChange={(event) => setSecretPurpose(event.target.value)}
              placeholder={translate("purpose")}
            />
            <input
              type="password"
              value={secretValue}
              onChange={(event) => setSecretValue(event.target.value)}
              placeholder={translate("secretValue")}
            />
            <button
              className="btn approval-primary"
              disabled={!secretName || !secretPurpose || !secretValue}
              onClick={() =>
                command("save_secret_metadata", {
                  name: secretName,
                  scope: `project:${project.id}`,
                  purpose: secretPurpose,
                  value: secretValue,
                  projectId: project.id,
                })
                  .then(load)
                  .then(onRefresh)
                  .then(() => {
                    setSecretName("");
                    setSecretPurpose("");
                    setSecretValue("");
                    setSecretFormOpen(false);
                  })
                  .catch(onError)
              }
            >
              {translate("saveSecret")}
            </button>
          </div>
        )}
        <div className="mt-3 grid gap-2">
          {secrets.map((secret) => (
            <div
              className="flex items-center justify-between rounded-lg border border-line p-3 text-sm"
              key={`${secret.project_id || "global"}:${secret.name}`}
            >
              <span>
                <strong>{secret.name}</strong>
                <span className="ml-2 text-xs text-faint">
                  {secret.project_id
                    ? secret.purpose
                    : `${secret.purpose} · 全局回退`}
                </span>
              </span>
              {secret.project_id === project.id && (
                <button
                  className="btn"
                  onClick={() =>
                    command("delete_secret_metadata", {
                      name: secret.name,
                      projectId: project.id,
                    })
                      .then(load)
                      .then(onRefresh)
                      .catch(onError)
                  }
                >
                  {translate("delete")}
                </button>
              )}
            </div>
          ))}
        </div>
      </div>
      <div className="mt-6 grid gap-3 border-t border-line pt-5">
        <h3 className="font-medium text-ink">
          {translate("projectCredentials")}
        </h3>
        <p className="text-xs text-faint">
          {translate("credentialsInputOnly")}
        </p>
        <div className="grid gap-3 md:grid-cols-2">
          <div className="grid gap-2 rounded-lg border border-line p-3">
            <strong className="text-sm text-ink">Provider key</strong>
            <input
              value={providerName}
              onChange={(event) => setProviderName(event.target.value)}
              placeholder="Provider ID"
            />
            <input
              type="password"
              value={providerKey}
              onChange={(event) => setProviderKey(event.target.value)}
              placeholder="Provider key"
            />
            <button
              className="btn"
              disabled={!providerName || !providerKey}
              onClick={() =>
                command("save_provider_key", {
                  provider: providerName,
                  key: providerKey,
                  projectId: project.id,
                })
                  .then(() => setProviderKey(""))
                  .catch(onError)
              }
            >
              {translate("saveProviderKey")}
            </button>
          </div>
          <div className="grid gap-2 rounded-lg border border-line p-3">
            <strong className="text-sm text-ink">MCP credential</strong>
            <input
              value={mcpServerId}
              onChange={(event) => setMcpServerId(event.target.value)}
              placeholder="MCP server ID"
            />
            <input
              type="password"
              value={mcpCredential}
              onChange={(event) => setMcpCredential(event.target.value)}
              placeholder="Credential JSON"
            />
            <button
              className="btn"
              disabled={!mcpServerId || !mcpCredential}
              onClick={() =>
                command("save_mcp_credential", {
                  serverId: mcpServerId,
                  value: mcpCredential,
                  projectId: project.id,
                })
                  .then(() => setMcpCredential(""))
                  .catch(onError)
              }
            >
              {translate("saveMcpCredential")}
            </button>
          </div>
          <div className="grid gap-2 rounded-lg border border-line p-3">
            <strong className="text-sm text-ink">Connector token</strong>
            <input
              value={connectorKind}
              onChange={(event) => setConnectorKind(event.target.value)}
              placeholder="Connector kind"
            />
            <input
              type="password"
              value={connectorToken}
              onChange={(event) => setConnectorToken(event.target.value)}
              placeholder="Token"
            />
            <button
              className="btn"
              disabled={!connectorKind || !connectorToken}
              onClick={() =>
                command("save_connector_token", {
                  kind: connectorKind,
                  value: connectorToken,
                  projectId: project.id,
                })
                  .then(() => setConnectorToken(""))
                  .catch(onError)
              }
            >
              {translate("saveConnectorToken")}
            </button>
          </div>
          <div className="grid gap-2 rounded-lg border border-line p-3">
            <strong className="text-sm text-ink">GitHub Enterprise 实例</strong>
            <span className="text-xs text-faint">
              github.com 默认可用。企业实例登记后 API base 归一化为
              https://&lt;host&gt;/api/v3，凭据使用 connector kind
              github@&lt;host&gt;。
            </span>
            <input
              value={githubHost}
              onChange={(event) => setGithubHost(event.target.value)}
              placeholder="ghe.example.com"
            />
            <input
              value={githubApiBase}
              onChange={(event) => setGithubApiBase(event.target.value)}
              placeholder="API base（可选，默认 https://host/api/v3）"
            />
            <button
              className="btn"
              disabled={!githubHost}
              onClick={() =>
                command("save_github_enterprise_instance", {
                  host: githubHost,
                  apiBase: githubApiBase || null,
                })
                  .then(() => {
                    setGithubHost("");
                    setGithubApiBase("");
                  })
                  .then(loadGithubInstances)
                  .catch(onError)
              }
            >
              登记企业实例
            </button>
            {githubInstances.map((instance) => (
              <div
                className="flex items-center justify-between gap-2 text-xs"
                key={instance.host}
              >
                <span className="min-w-0 break-all">
                  <strong className="text-ink">{instance.host}</strong>
                  <span className="ml-2 text-faint">{instance.api_base}</span>
                </span>
                <button
                  className="btn"
                  onClick={() =>
                    command("delete_github_enterprise_instance", {
                      host: instance.host,
                    })
                      .then(loadGithubInstances)
                      .catch(onError)
                  }
                >
                  删除
                </button>
              </div>
            ))}
          </div>
        </div>
      </div>
    </section>
  );
}

function ProjectBoard({
  project,
  sessions,
  providers,
  models,
  harnessOptions,
  onRefresh,
  onOpenSession,
  onError,
  onProjectDeleted,
}: {
  project: Project;
  sessions: Session[];
  providers: ProviderDescriptor[];
  models: Array<{ id: string; label: string }>;
  harnessOptions: HarnessOption[];
  onRefresh: () => Promise<void>;
  onOpenSession: (id: string) => void;
  onError: (error: unknown) => void;
  onProjectDeleted: () => void;
}) {
  const [memberForm, setMemberForm] = useState<{
    mode: "add" | "edit";
    agent?: ProjectAgent;
  } | null>(null);
  const [deleteError, setDeleteError] = useState<{
    agentId: string;
    message: string;
  } | null>(null);
  const [memberSaving, setMemberSaving] = useState(false);
  const [projectEditing, setProjectEditing] = useState(false);
  const [projectName, setProjectName] = useState(project.name);
  const [projectBranch, setProjectBranch] = useState(project.default_branch);
  const [projectActionError, setProjectActionError] = useState("");
  const [projectActionBusy, setProjectActionBusy] = useState(false);
  const submitMember = async (values: {
    name: string;
    role: string;
    provider: string;
    model: string;
    harness: string;
    mode: string;
    branch: string;
    state: string;
  }) => {
    setMemberSaving(true);
    try {
      if (memberForm?.mode === "add") {
        await command("create_project_agent", {
          projectId: project.id,
          name: values.name,
          role: values.role,
          provider: values.provider || null,
          model: values.model || "auto",
          harness: values.harness || "builtin",
          mode: values.mode || "Interactive",
          branch: values.branch || null,
        });
      } else if (memberForm?.agent) {
        await command("update_project_agent", {
          id: memberForm.agent.id,
          name: values.name,
          role: values.role,
          stateName: values.state,
        });
      }
      setMemberForm(null);
      await onRefresh();
    } catch (reason) {
      onError(reason);
    } finally {
      setMemberSaving(false);
    }
  };
  const deleteMember = async (agent: ProjectAgent, force = false) => {
    if (
      !force &&
      !window.confirm(`确定删除成员「${agent.name}」？其 worktree 将被回收。`)
    )
      return;
    try {
      const result = await command<{ warnings?: string[] }>(
        "delete_project_agent",
        {
          agentId: agent.id,
          force,
        },
      );
      setDeleteError(null);
      if (result.warnings?.length) {
        onError(result.warnings.join("\n"));
      }
      await onRefresh();
    } catch (reason) {
      setDeleteError({ agentId: agent.id, message: errorMessage(reason) });
    }
  };
  const archiveProject = async () => {
    if (
      !window.confirm(
        `确定归档项目「${project.name}」？归档不会删除 worktree。`,
      )
    )
      return;
    setProjectActionBusy(true);
    setProjectActionError("");
    try {
      await command("update_project", {
        id: project.id,
        archived: true,
      });
      await onRefresh();
      onProjectDeleted();
    } catch (reason) {
      setProjectActionError(errorMessage(reason));
    } finally {
      setProjectActionBusy(false);
    }
  };
  const saveProjectDetails = async () => {
    setProjectActionBusy(true);
    setProjectActionError("");
    try {
      await command("update_project", {
        id: project.id,
        name: projectName.trim(),
        defaultBranch: projectBranch.trim(),
      });
      await onRefresh();
      setProjectEditing(false);
    } catch (reason) {
      setProjectActionError(errorMessage(reason));
    } finally {
      setProjectActionBusy(false);
    }
  };
  const deleteProject = async (force = false) => {
    if (
      !force &&
      !window.confirm(
        `确定删除项目「${project.name}」？这会回收所有成员 worktree，且不可撤销。`,
      )
    )
      return;
    setProjectActionBusy(true);
    setProjectActionError("");
    try {
      const result = await command<{ warnings?: string[] }>("delete_project", {
        id: project.id,
        force,
      });
      await onRefresh();
      if (result.warnings?.length) {
        onError(result.warnings.join("\n"));
      }
      onProjectDeleted();
    } catch (reason) {
      setProjectActionError(errorMessage(reason));
    } finally {
      setProjectActionBusy(false);
    }
  };
  return (
    <main className="flex-1 overflow-y-auto p-8">
      <div className="max-w-6xl mx-auto">
        <div className="flex items-start justify-between gap-4 mb-8">
          <div>
            <h1 className="text-2xl font-semibold text-ink">{project.name}</h1>
            <p className="text-sm text-faint mt-2">
              {project.host_name} · {project.online === false ? "离线" : "在线"}{" "}
              · {project.repo_root}
            </p>
            <p className="text-sm text-faint mt-1">
              默认分支：{project.default_branch}
            </p>
          </div>
          <button
            className="btn approval-primary"
            onClick={() =>
              setMemberForm({
                mode: "add",
              })
            }
          >
            添加成员
          </button>
          <button
            className="btn"
            disabled={projectActionBusy}
            onClick={() => {
              setProjectName(project.name);
              setProjectBranch(project.default_branch);
              setProjectEditing((value) => !value);
            }}
          >
            编辑项目
          </button>
          <button
            className="btn"
            disabled={projectActionBusy}
            onClick={archiveProject}
          >
            归档项目
          </button>
          <button
            className="btn quiet-deny"
            disabled={projectActionBusy}
            onClick={() => void deleteProject()}
          >
            删除项目
          </button>
        </div>
        {projectActionError && (
          <div className="mt-3 rounded-lg bg-danger/10 p-3 text-sm text-danger">
            <p>{projectActionError}</p>
            <button
              className="btn mt-2"
              onClick={() => void deleteProject(true)}
            >
              强制删除并回收 worktree
            </button>
          </div>
        )}
        {projectEditing && (
          <div className="mt-4 grid gap-3 rounded-lg border border-line bg-panel p-4 md:grid-cols-[1fr_1fr_auto]">
            <label className="field-label">
              项目名称
              <input
                value={projectName}
                onChange={(event) => setProjectName(event.target.value)}
              />
            </label>
            <label className="field-label">
              默认分支
              <input
                value={projectBranch}
                onChange={(event) => setProjectBranch(event.target.value)}
              />
            </label>
            <div className="flex items-end gap-2">
              <button
                className="btn approval-primary"
                disabled={
                  projectActionBusy ||
                  !projectName.trim() ||
                  !projectBranch.trim()
                }
                onClick={() => void saveProjectDetails()}
              >
                保存
              </button>
              <button className="btn" onClick={() => setProjectEditing(false)}>
                取消
              </button>
            </div>
          </div>
        )}
        <div className="grid grid-cols-[repeat(auto-fill,minmax(240px,1fr))] gap-4">
          {project.agents.map((agent) => {
            const session = sessions.find(
              (item) => item.id === agent.session_id,
            );
            return (
              <div
                key={agent.id}
                className="rounded-xl border border-line bg-panel p-4 shadow-sm"
              >
                <div className="flex items-center justify-between gap-2">
                  <span className="text-xs rounded-full bg-faint/20 px-2 py-1 text-muted">
                    {agent.role}
                  </span>
                  <span className="text-xs text-faint">{agent.state}</span>
                </div>
                <h2 className="mt-4 font-medium text-ink">{agent.name}</h2>
                <p
                  className="mt-2 text-xs text-faint truncate"
                  title={agent.branch}
                >
                  分支：{agent.branch}
                </p>
                <p
                  className="mt-1 text-xs text-faint truncate"
                  title={agent.worktree_path}
                >
                  {agent.sort_order === 0 ? "主检出：" : "Worktree："}
                  {agent.worktree_path}
                </p>
                <div className="mt-4 flex items-center gap-2">
                  {session ? (
                    <button
                      className="btn approval-primary"
                      onClick={() => onOpenSession(session.id)}
                    >
                      打开会话
                    </button>
                  ) : (
                    <button
                      className="btn approval-primary"
                      onClick={() =>
                        command<Session>("create_session", {
                          title: agent.name,
                          projectId: project.id,
                          agentId: agent.id,
                          provider: agent.provider || null,
                          model: agent.model,
                          harness: agent.harness,
                          mode: agent.mode,
                        })
                          .then((next) => {
                            onOpenSession(next.id);
                            return onRefresh();
                          })
                          .catch(onError)
                      }
                    >
                      启动会话
                    </button>
                  )}
                  <button
                    className="btn"
                    onClick={() => setMemberForm({ mode: "edit", agent })}
                  >
                    编辑
                  </button>
                  <button
                    className="btn"
                    onClick={() =>
                      command("save_project_agent_as_template", {
                        projectId: project.id,
                        agentId: agent.id,
                      }).catch(onError)
                    }
                  >
                    另存 Agent
                  </button>
                  {agent.sort_order !== 0 && (
                    <button
                      className="btn"
                      onClick={() => void deleteMember(agent)}
                    >
                      删除
                    </button>
                  )}
                </div>
                {deleteError?.agentId === agent.id && (
                  <div className="mt-3 rounded-lg bg-danger/10 p-2 text-xs text-danger">
                    <p>{deleteError.message}</p>
                    <button
                      className="btn mt-2"
                      onClick={() => void deleteMember(agent, true)}
                    >
                      强制删除
                    </button>
                  </div>
                )}
              </div>
            );
          })}
        </div>
        <ProjectConfigPanel
          project={project}
          onError={onError}
          onRefresh={onRefresh}
        />
        <ProjectCoordinationPanel
          project={project}
          onError={onError}
          onRefresh={onRefresh}
        />
      </div>
      {memberForm && (
        <MemberDialog
          mode={memberForm.mode}
          agent={memberForm.agent}
          providers={providers}
          models={models}
          harnessOptions={harnessOptions}
          saving={memberSaving}
          onClose={() => setMemberForm(null)}
          onSubmit={submitMember}
        />
      )}
    </main>
  );
}

function ProjectCoordinationPanel({
  project,
  onError,
  onRefresh,
}: {
  project: Project;
  onError: (error: unknown) => void;
  onRefresh: () => Promise<void>;
}) {
  const [snapshot, setSnapshot] = useState<Record<string, any> | null>(null);
  const [taskTitle, setTaskTitle] = useState("");
  const [taskAssignee, setTaskAssignee] = useState("");
  const [loading, setLoading] = useState(false);
  const [workflowText, setWorkflowText] = useState("");
  const taskId = `project-board:${project.id}`;
  const load = async () => {
    const value = await command<Record<string, any>>(
      "project_workflow_snapshot",
      { projectId: project.id },
    );
    setSnapshot(value);
    if (value.workflow) {
      setWorkflowText(JSON.stringify(value.workflow, null, 2));
    }
  };
  useEffect(() => {
    void load().catch(onError);
  }, [project.id]);
  const setAllRoles = async (state: string) => {
    try {
      setLoading(true);
      await command("coordination_start_project", { projectId: project.id });
      for (const agent of project.agents) {
        await command("coordination_set_role_state", {
          taskId,
          roleId: agent.id,
          stateName: state,
        });
      }
      await load();
      await onRefresh();
    } catch (error) {
      onError(error);
    } finally {
      setLoading(false);
    }
  };
  const createTask = async () => {
    if (!taskTitle.trim()) return;
    try {
      const id = `task-${Date.now()}`;
      await command("coordination_create_task", {
        id,
        projectId: project.id,
        title: taskTitle.trim(),
        requireAcceptance: true,
        branch: null,
        pr: null,
      });
      if (taskAssignee) {
        await command("coordination_claim_task", {
          id,
          worker: taskAssignee,
        });
      }
      setTaskTitle("");
      setTaskAssignee("");
      await load();
      await onRefresh();
    } catch (error) {
      onError(error);
    }
  };
  return (
    <section className="mt-8 rounded-xl border border-line bg-panel p-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold text-ink">
            {translate("workflowLeadCommand")}
          </h2>
          <p className="mt-1 text-sm text-faint">
            {translate("currentStage")}：
            {snapshot?.status === "done"
              ? translate("completed")
              : snapshot?.workflow?.workflow?.[snapshot.stage_index]?.stage ||
                translate("notStarted")}
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            className="btn approval-primary"
            disabled={loading}
            onClick={() =>
              command("coordination_start_project", { projectId: project.id })
                .then(load)
                .then(onRefresh)
                .catch(onError)
            }
          >
            {translate("startAll")}
          </button>
          <button
            className="btn"
            disabled={loading}
            onClick={() => void setAllRoles("paused")}
          >
            {translate("pause")}
          </button>
          <button
            className="btn"
            disabled={loading}
            onClick={() => void setAllRoles("active")}
          >
            {translate("resume")}
          </button>
          <button
            className="btn"
            onClick={() =>
              command("project_workflow_advance", { projectId: project.id })
                .then(load)
                .then(onRefresh)
                .catch(onError)
            }
          >
            {translate("advanceStage")}
          </button>
        </div>
      </div>
      <div className="mt-5 grid gap-3">
        <div className="grid gap-2 rounded-lg border border-line p-4">
          <h3 className="font-medium text-ink">
            {translate("workflowDefinition")}
          </h3>
          <p className="text-xs text-faint">
            {translate("workflowFormatHint")}
          </p>
          <textarea
            value={workflowText}
            onChange={(event) => setWorkflowText(event.target.value)}
            placeholder='{"workflow":[{"stage":"plan","roles":["Lead"],"gate":"none"}],"serial":true}'
          />
          <button
            className="btn"
            onClick={() =>
              command("save_project_workflow", {
                projectId: project.id,
                workflowJson: workflowText,
              })
                .then(load)
                .then(onRefresh)
                .catch(onError)
            }
          >
            {translate("saveWorkflow")}
          </button>
        </div>
        <div className="grid gap-2 rounded-lg border border-line p-4">
          <h3 className="font-medium text-ink">{translate("task")}</h3>
          <div className="grid gap-2 md:grid-cols-[1fr_180px_auto]">
            <input
              value={taskTitle}
              onChange={(event) => setTaskTitle(event.target.value)}
              placeholder={translate("taskTitle")}
            />
            <input
              value={taskAssignee}
              onChange={(event) => setTaskAssignee(event.target.value)}
              placeholder={translate("assigneeRoleId")}
            />
            <button className="btn approval-primary" onClick={createTask}>
              {translate("createTask")}
            </button>
          </div>
          <div className="grid gap-2">
            {(snapshot?.tasks || []).map((task: any) => (
              <div
                className="flex flex-wrap items-center justify-between gap-2 rounded-lg border border-line p-3 text-sm"
                key={task.id}
              >
                <span>
                  <strong>{task.title}</strong>
                  <span className="ml-2 text-faint">
                    {task.phase} · {task.assignee || translate("unassigned")}
                  </span>
                </span>
                <div className="flex gap-2">
                  {taskAssignee && (
                    <button
                      className="btn"
                      onClick={() =>
                        command("coordination_claim_task", {
                          id: task.id,
                          worker: taskAssignee,
                        })
                          .then(load)
                          .catch(onError)
                      }
                    >
                      {translate("claimLease")}
                    </button>
                  )}
                  <button
                    className="btn"
                    onClick={() =>
                      command("coordination_ingest_session", {
                        sessionId: project.agents.find(
                          (agent) => agent.id === task.assignee,
                        )?.session_id,
                      })
                        .then(load)
                        .catch(onError)
                    }
                  >
                    {translate("syncReceipt")}
                  </button>
                </div>
              </div>
            ))}
          </div>
        </div>
        <div className="grid gap-2 rounded-lg border border-line p-4">
          <h3 className="font-medium text-ink">
            {translate("coordinationMessageHistory")}
          </h3>
          {(snapshot?.messages || []).map((message: any) => (
            <div
              className="rounded-lg border border-line p-3 text-xs"
              key={message.msg_id}
            >
              <strong>
                {message.from} → {message.to} · {message.kind}
              </strong>
              <pre className="mt-1 whitespace-pre-wrap text-faint">
                {JSON.stringify(message.payload)}
              </pre>
            </div>
          ))}
          {!snapshot?.messages?.length && (
            <p className="text-sm text-faint">
              {translate("noCoordinationMessages")}
            </p>
          )}
        </div>
      </div>
    </section>
  );
}

function Button({
  children,
  className = "",
  ...props
}: React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button className={`btn ${className}`} {...props}>
      {children}
    </button>
  );
}
function SelectMenu({
  value,
  onChange,
  options,
  ariaLabel = "Select option",
}: {
  value: string;
  onChange: (value: string) => void;
  options: Array<{ value: string; label: string }>;
  ariaLabel?: string;
}) {
  return (
    <OpenWorkerSelectMenu
      value={value}
      onChange={onChange}
      options={options}
      ariaLabel={ariaLabel}
    />
  );
}
function SurfaceView({
  tab,
  selected,
  onError,
  visible = true,
}: {
  tab: SurfaceTab | "pr";
  selected: Session;
  onError: (error: unknown) => void;
  visible?: boolean;
}) {
  const terminalHost = useRef<HTMLDivElement>(null);
  const vncHost = useRef<HTMLDivElement>(null);
  const [surfaceUrl, setSurfaceUrl] = useState("");
  const surfaceUrlRef = useRef("");
  const surfaceGenerationRef = useRef(0);
  const [vncPassword, setVncPassword] = useState<string | null>(null);
  const [surfaceError, setSurfaceError] = useState("");
  const [surfaceFailed, setSurfaceFailed] = useState(false);
  const surfaceFailedRef = useRef(false);
  const selectedSleepStateRef = useRef(selected.sleep_state);
  const [browserFrame, setBrowserFrame] = useState<{
    image: string;
    mime: string;
    target_url: string;
  } | null>(null);
  const [ideUrl, setIdeUrl] = useState("");
  const [review, setReview] = useState<Record<string, unknown> | null>(null);
  const [diff, setDiff] = useState<Record<string, unknown> | null>(null);
  const [worklog, setWorklog] = useState<Record<string, unknown> | null>(null);
  const [busy, setBusy] = useState(false);
  const [ideError, setIdeError] = useState("");
  const [sleeping, setSleeping] = useState(
    shouldShowSurfaceReconnect(selected.sleep_state),
  );
  const startInFlightRef = useRef(false);
  const retryAfterStartRef = useRef(false);
  const sleepingRef = useRef(sleeping);
  const surfaceTabRef = useRef(tab);
  const surfaceVisibleRef = useRef(visible);
  const [surfaceRetryToken, setSurfaceRetryToken] = useState(0);
  const markSurfaceFailed = (reason: string) => {
    surfaceFailedRef.current = true;
    setSurfaceFailed(true);
    setSurfaceError(reason);
  };
  const clearSurfaceFailure = () => {
    surfaceFailedRef.current = false;
    setSurfaceFailed(false);
    setSurfaceError("");
  };
  const lastTouchRef = useRef(0);
  sleepingRef.current = sleeping;
  surfaceTabRef.current = tab;
  surfaceVisibleRef.current = visible;
  const touchSessionActivity = () => {
    const now = Date.now();
    if (
      selected.sleep_state !== "asleep" &&
      now - lastTouchRef.current < 60_000
    )
      return;
    lastTouchRef.current = now;
    void command("touch_session", { sessionId: selected.id }).catch(onError);
  };
  const start = async (surface: string) => {
    if (surfaceFailedRef.current) return;
    if (startInFlightRef.current) {
      retryAfterStartRef.current = true;
      return;
    }
    const generation = surfaceGenerationRef.current;
    let invalidated = false;
    try {
      startInFlightRef.current = true;
      setBusy(true);
      clearSurfaceFailure();
      const connection = await command<{
        url: string;
        vnc_password: string | null;
      }>("surface_url", {
        sessionId: selected.id,
        hostId: selected.host_id,
        surface,
        cols: 100,
        rows: 30,
        cwd: selected.workspace || null,
        projectId: selected.project_id || null,
      });
      if (generation !== surfaceGenerationRef.current) {
        invalidated = true;
        surfaceUrlRef.current = "";
        setSurfaceUrl("");
        return;
      }
      surfaceUrlRef.current = connection.url;
      setSurfaceUrl(connection.url);
      setVncPassword(connection.vnc_password);
    } catch (error) {
      markSurfaceFailed(errorMessage(error));
      onError(error);
    } finally {
      startInFlightRef.current = false;
      setBusy(false);
      const retryRequested = invalidated || retryAfterStartRef.current;
      const shouldRetry = shouldRetrySurfaceStart({
        invalidated: retryRequested,
        connected: Boolean(surfaceUrlRef.current),
        sleeping: sleepingRef.current,
        tab: surfaceTabRef.current,
        failed: surfaceFailedRef.current,
        visible: surfaceVisibleRef.current,
      });
      retryAfterStartRef.current = false;
      if (shouldRetry) setSurfaceRetryToken((token) => token + 1);
    }
  };
  useEffect(() => {
    const previousSleepState = selectedSleepStateRef.current;
    selectedSleepStateRef.current = selected.sleep_state;
    if (shouldResetSurfaceForSleep(previousSleepState, selected.sleep_state)) {
      surfaceGenerationRef.current += 1;
      surfaceUrlRef.current = "";
      setSurfaceUrl("");
      setSleeping(true);
      clearSurfaceFailure();
    } else if (!shouldShowSurfaceReconnect(selected.sleep_state) && sleeping) {
      setSleeping(false);
      clearSurfaceFailure();
    }
  }, [selected.sleep_state, sleeping]);
  useEffect(() => {
    return () => {
      surfaceGenerationRef.current += 1;
      surfaceUrlRef.current = "";
    };
  }, [selected.id, tab]);
  useEffect(() => {
    surfaceUrlRef.current = "";
    setSurfaceUrl("");
    setVncPassword(null);
    surfaceFailedRef.current = false;
    setSurfaceFailed(false);
    setSurfaceError("");
    setBrowserFrame(null);
    setIdeUrl("");
    setReview(null);
    setDiff(null);
    setWorklog(null);
    setIdeError("");
    setSleeping(shouldShowSurfaceReconnect(selected.sleep_state));
    selectedSleepStateRef.current = selected.sleep_state;
  }, [selected.id]);
  useEffect(() => {
    clearSurfaceFailure();
  }, [tab]);
  useEffect(() => {
    if (
      visible &&
      (tab === "terminal" || tab === "desktop") &&
      surfaceNeedsConnection(
        tab,
        Boolean(surfaceUrl),
        sleeping,
        surfaceFailed,
        visible,
      )
    ) {
      void start(tab === "terminal" ? "pty" : "vnc");
    } else if (tab === "ide" && !ideUrl && !ideError) {
      if (!selected.workspace) {
        setIdeError("Remote IDE workspace is not configured for this session.");
        return;
      }
      setBusy(true);
      void command<string>("ide_url", {
        sessionId: selected.id,
        folderUri: selected.workspace,
      })
        .then(setIdeUrl)
        .catch((error) => {
          setIdeError(errorMessage(error));
          onError(error);
        })
        .finally(() => setBusy(false));
    }
  }, [
    tab,
    visible,
    selected.id,
    selected.host_id,
    selected.host_name,
    selected.workspace,
    surfaceUrl,
    ideUrl,
    ideError,
    sleeping,
    surfaceFailed,
    surfaceRetryToken,
  ]);
  useEffect(() => {
    if (tab !== "browser" || !visible || sleeping) return;
    let cancelled = false;
    let timer: number | undefined;
    const poll = async () => {
      let failed = false;
      try {
        const frame = await command<{
          image: string;
          mime: string;
          target_url: string;
        }>("capture_remote_browser_frame", { sessionId: selected.id });
        if (!cancelled) {
          setBrowserFrame(frame);
          if (!surfaceFailedRef.current) setSurfaceError("");
        }
      } catch (error) {
        if (!cancelled) {
          failed = true;
          markSurfaceFailed(errorMessage(error));
        }
      }
      if (!cancelled && !failed)
        timer = window.setTimeout(() => void poll(), 1000);
    };
    void poll();
    return () => {
      cancelled = true;
      if (timer !== undefined) window.clearTimeout(timer);
    };
  }, [tab, selected.id, sleeping, surfaceRetryToken, visible]);
  useEffect(() => {
    if (!visible || tab !== "terminal" || !surfaceUrl || !terminalHost.current)
      return;
    const terminal = new Terminal({
      convertEol: false,
      cursorBlink: true,
      theme: { background: "#11151d", foreground: "#d7dbe5" },
    });
    terminal.open(terminalHost.current);
    const socket = new WebSocket(surfaceUrl);
    let intentionallyClosed = false;
    socket.binaryType = "arraybuffer";
    const pending: Array<string | Uint8Array> = [];
    const send = (data: string | Uint8Array) => {
      if (socket.readyState === WebSocket.OPEN) socket.send(data);
      else pending.push(data);
    };
    socket.onopen = () => {
      clearSurfaceFailure();
      while (pending.length) socket.send(pending.shift()!);
    };
    const reportFailure = () => {
      if (intentionallyClosed) return;
      surfaceUrlRef.current = "";
      setSurfaceUrl("");
      if (!surfaceFailedRef.current)
        markSurfaceFailed(translate("surfaceDisconnected"));
    };
    socket.onerror = reportFailure;
    socket.onclose = reportFailure;
    socket.onmessage = (event) =>
      terminal.write(
        typeof event.data === "string"
          ? event.data
          : new Uint8Array(event.data as ArrayBuffer),
      );
    const encoder = new TextEncoder();
    const resize = () => {
      const width = terminalHost.current?.clientWidth || 0;
      const height = terminalHost.current?.clientHeight || 0;
      if (!width || !height) return;
      const cols = Math.max(20, Math.floor(width / 7.8));
      const rows = Math.max(5, Math.floor(height / 17));
      terminal.resize(cols, rows);
      send(JSON.stringify({ type: "resize", cols, rows }));
    };
    const input = terminal.onData((data) => {
      touchSessionActivity();
      send(encoder.encode(data));
    });
    terminal.onResize(({ cols, rows }) =>
      send(JSON.stringify({ type: "resize", cols, rows })),
    );
    const observer = new ResizeObserver(resize);
    observer.observe(terminalHost.current);
    requestAnimationFrame(resize);
    return () => {
      intentionallyClosed = true;
      observer.disconnect();
      input.dispose();
      socket.close();
      if (surfaceUrlRef.current === surfaceUrl) {
        surfaceUrlRef.current = "";
        setSurfaceUrl("");
      }
      terminal.dispose();
    };
  }, [selected.id, surfaceUrl, tab, sleeping, visible]);
  useEffect(() => {
    if (!visible || tab !== "desktop" || !surfaceUrl || !vncHost.current)
      return;
    const rfb = new RFB(vncHost.current, surfaceUrl, {
      credentials: vncPassword ? { password: vncPassword } : undefined,
    });
    rfb.scaleViewport = true;
    let connected = false;
    let intentionallyClosed = false;
    const report = (message: string) => {
      if (intentionallyClosed) return;
      surfaceUrlRef.current = "";
      setSurfaceUrl("");
      markSurfaceFailed(message);
    };
    rfb.addEventListener("connect", () => {
      connected = true;
      clearSurfaceFailure();
    });
    rfb.addEventListener("securityfailure", (event) => {
      report(
        `Remote VNC security negotiation failed: ${
          (event as CustomEvent<{ status?: string }>).detail?.status ||
          "authentication rejected"
        }`,
      );
    });
    rfb.addEventListener("credentialsrequired", () => {
      report(
        "Remote VNC requires a password; configure the host VNC password.",
      );
    });
    rfb.addEventListener("disconnect", (event) => {
      const detail = (event as CustomEvent<{ clean?: boolean }>).detail;
      if (!detail?.clean)
        report(
          connected
            ? "Remote VNC disconnected after the desktop handshake."
            : "Remote VNC disconnected before the desktop loaded.",
        );
    });
    const host = vncHost.current;
    const onUserInput = () => touchSessionActivity();
    host?.addEventListener("pointerdown", onUserInput);
    host?.addEventListener("keydown", onUserInput);
    host?.addEventListener("wheel", onUserInput, { passive: true });
    return () => {
      intentionallyClosed = true;
      host?.removeEventListener("pointerdown", onUserInput);
      host?.removeEventListener("keydown", onUserInput);
      host?.removeEventListener("wheel", onUserInput);
      rfb.disconnect();
      if (surfaceUrlRef.current === surfaceUrl) {
        surfaceUrlRef.current = "";
        setSurfaceUrl("");
      }
    };
  }, [selected.id, surfaceUrl, tab, vncPassword, sleeping, visible]);
  const reconnect = async () => {
    clearSurfaceFailure();
    if (tab === "browser") {
      try {
        await command("wake_session", { sessionId: selected.id });
        setSleeping(false);
        setSurfaceRetryToken((token) => token + 1);
      } catch (error) {
        markSurfaceFailed(errorMessage(error));
        onError(error);
      }
      return;
    }
    setSleeping(false);
    void start(tab === "terminal" ? "pty" : "vnc");
  };
  const retrySurface = () => {
    clearSurfaceFailure();
    setSurfaceRetryToken((token) => token + 1);
  };
  const showSurfaceRetry = shouldShowSurfaceRetry({
    connected: Boolean(surfaceUrl),
    sleeping,
    failed: surfaceFailed,
  });
  if (tab === "terminal" || tab === "desktop" || tab === "browser")
    return (
      <div className="surface-panel">
        {shouldShowSurfaceReconnect(selected.sleep_state) && sleeping && (
          <div className="surface-status warning">
            <p>{translate("surfaceSleepingDescription")}</p>
            <Button onClick={reconnect}>{translate("reconnectSurface")}</Button>
          </div>
        )}
        {busy && (
          <div className="surface-status muted">
            {translate("connectingRemoteHost")}
          </div>
        )}
        {surfaceError && !sleeping && !showSurfaceRetry && (
          <div className="surface-status error">{surfaceError}</div>
        )}
        {showSurfaceRetry && (
          <div className="surface-status warning">
            <p>{surfaceError || translate("surfaceUnavailable")}</p>
            <Button onClick={retrySurface}>{translate("retrySurface")}</Button>
          </div>
        )}
        {tab === "terminal" && (
          <div className="terminal-host" ref={terminalHost} />
        )}
        {tab === "desktop" && <div className="vnc-host" ref={vncHost} />}
        {tab === "browser" && (
          <div className="browser-preview">
            <div className="surface-status muted">
              {translate("remoteBrowserPreview")}
            </div>
            {browserFrame ? (
              <img
                className="browser-preview-image"
                src={`data:${browserFrame.mime};base64,${browserFrame.image}`}
                alt={translate("remoteBrowserPage")}
              />
            ) : !surfaceFailed ? (
              <div className="empty-surface">
                <Icon name="image" size={32} />
                <p>{translate("waitingBrowserTarget")}</p>
              </div>
            ) : null}
            <p className="surface-status muted">
              {translate("remoteBrowserCdp")}
            </p>
          </div>
        )}
      </div>
    );
  if (tab === "ide")
    return (
      <div className="surface-panel">
        {busy && (
          <div className="surface-status muted">
            {translate("connectingRemoteHost")}
          </div>
        )}
        {ideUrl && !ideError ? (
          <iframe
            title={translate("remoteWebIde")}
            src={ideUrl}
            className="ide-frame"
          />
        ) : ideError ? (
          <div className="empty-surface ide-error">
            <Icon name="code" size={32} />
            <p>{ideError}</p>
            <p className="muted">{translate("noLocalFallback")}</p>
          </div>
        ) : (
          <div className="empty-surface">
            <Icon name="code" size={32} />
            <p>{translate("startRemoteIde")}</p>
          </div>
        )}
      </div>
    );
  if (tab === "review")
    return (
      <ReviewView
        selected={selected}
        review={review}
        diff={diff}
        setReview={setReview}
        setDiff={setDiff}
        onError={onError}
      />
    );
  if (tab === "pr") return <PRView selected={selected} onError={onError} />;
  if (tab === "worklog")
    return (
      <WorklogView
        selected={selected}
        worklog={worklog}
        setWorklog={setWorklog}
        onError={onError}
      />
    );
  return null;
}

function ReviewView({
  selected,
  review,
  diff,
  setReview,
  setDiff,
  onError,
}: {
  selected: Session;
  review: Record<string, unknown> | null;
  diff: Record<string, unknown> | null;
  setReview: (value: Record<string, unknown>) => void;
  setDiff: (value: Record<string, unknown>) => void;
  onError: (error: unknown) => void;
}) {
  const [cwd, setCwd] = useState(selected.workspace || "/workspace");
  const [base, setBase] = useState("HEAD");
  const changes = Array.isArray(review?.changes)
    ? review.changes
    : review?.changes &&
        typeof review.changes === "object" &&
        Array.isArray((review.changes as { files?: unknown }).files)
      ? (review.changes as { files: unknown[] }).files
      : [];
  if (!selected.workspace) {
    return (
      <div className="surface-panel">
        <div className="warning">
          {translate("noWorkspaceReview")}
        </div>
      </div>
    );
  }
  return (
    <div className="surface-panel review-panel">
      <div className="surface-toolbar">
        <span>{translate("remoteReview")}</span>
        <div>
          <input value={cwd} onChange={(event) => setCwd(event.target.value)} />
          <input
            value={base}
            onChange={(event) => setBase(event.target.value)}
          />
          <Button
            onClick={() =>
              command<Record<string, unknown>>("review_snapshot", {
                sessionId: selected.id,
                cwd,
                base,
              })
                .then(setReview)
                .catch(onError)
            }
          >
            {translate("refresh")}
          </Button>
        </div>
      </div>
      {review ? (
        <div className="review-grid">
          <div>
            <h3>{translate("changedFiles")}</h3>
            {changes.map((change) => {
              const file =
                typeof change === "object" &&
                change !== null &&
                !Array.isArray(change)
                  ? (change as {
                      path?: unknown;
                      additions?: unknown;
                      deletions?: unknown;
                      changeType?: unknown;
                    })
                  : null;
              const path =
                typeof file?.path === "string"
                  ? file.path
                  : typeof change === "string"
                    ? change
                    : "";
              if (!path) return null;
              const additions =
                typeof file?.additions === "number" ? file.additions : null;
              const deletions =
                typeof file?.deletions === "number" ? file.deletions : null;
              const changeType =
                typeof file?.changeType === "string" ? file.changeType : null;
              return (
                <button
                  className="file-row"
                  key={path}
                  onClick={() =>
                    command<Record<string, unknown>>("review_file_diff", {
                      sessionId: selected.id,
                      cwd,
                      path,
                      base,
                    })
                      .then(setDiff)
                      .catch(onError)
                  }
                >
                  <span className="file-row-path" title={path}>
                    {path}
                  </span>
                  {(changeType || additions !== null || deletions !== null) && (
                    <span className="file-row-meta">
                      {changeType || ""}
                      {additions !== null && (
                        <span className="diff-add"> +{additions}</span>
                      )}
                      {deletions !== null && (
                        <span className="diff-del"> -{deletions}</span>
                      )}
                    </span>
                  )}
                </button>
              );
            })}
          </div>
          <DiffView diff={diff} />
        </div>
      ) : (
        <div className="empty-surface">
          <p>
            {translate("loadRemoteChanges")}
          </p>
        </div>
      )}
    </div>
  );
}

function PRView({
  selected,
  onError,
}: {
  selected: Session;
  onError: (error: unknown) => void;
}) {
  const [cwd, setCwd] = useState(selected.workspace || "/workspace");
  return (
    <div className="surface-panel">
      <div className="surface-toolbar">
        <span>{translate("gitWorkflowPullRequests")}</span>
        <input value={cwd} onChange={(event) => setCwd(event.target.value)} />
      </div>
      <GitActions selected={selected} cwd={cwd} onError={onError} />
    </div>
  );
}

function GitActions({
  selected,
  cwd,
  onError,
}: {
  selected: Session;
  cwd: string;
  onError: (error: unknown) => void;
}) {
  const [operation, setOperation] = useState("branch");
  const [value, setValue] = useState("");
  const [repo, setRepo] = useState("");
  const [pr, setPr] = useState("");
  const [commentPrUrl, setCommentPrUrl] = useState("");
  const [lifecycleResult, setLifecycleResult] = useState<unknown>(null);
  return (
    <div className="git-actions">
      <h3>{translate("gitWorkflow")}</h3>
      <SelectMenu
        value={operation}
        onChange={setOperation}
        options={["branch", "add", "commit", "push"].map((item) => ({
          value: item,
          label: item,
        }))}
      />
      <input
        value={value}
        onChange={(event) => setValue(event.target.value)}
        placeholder={translate("slug, files, or commit message")}
      />
      <Button
        onClick={() =>
          command("git_workflow", {
            sessionId: selected.id,
            operation,
            cwd,
            slug: operation === "branch" ? value : null,
            files:
              operation === "add"
                ? value.split(",").map((item) => item.trim())
                : null,
            message: operation === "commit" ? value : null,
          })
            .then(setLifecycleResult)
            .catch(onError)
        }
      >
        {translate("runOperation", { operation })}
      </Button>
      {lifecycleResult ? (
        <pre className="code-block">
          {JSON.stringify(lifecycleResult, null, 2)}
        </pre>
      ) : null}
      <details>
        <summary>{translate("createGithubPr")}</summary>
        <input
          value={repo}
          onChange={(event) => setRepo(event.target.value)}
          placeholder={translate("owner/repository")}
        />
        <input
          value={pr}
          onChange={(event) => setPr(event.target.value)}
          placeholder={translate("PR title")}
        />
        <Button
          onClick={() =>
            command("github_pull_request", {
              sessionId: selected.id,
              repo,
              title: pr,
              head: "HEAD",
              base: "main",
              body: "",
              tokenSecret: "github",
            }).catch(onError)
          }
        >
          {translate("createPr")}
        </Button>
        <p className="muted small">
          {translate("githubSecretNotice")}
        </p>
      </details>
      <CiMonitorPanel selected={selected} onError={onError} />
      <RunnerPanel selected={selected} onError={onError} />
      <details>
        <summary>{translate("processPrComments")}</summary>
        <input
          value={commentPrUrl}
          onChange={(event) => setCommentPrUrl(event.target.value)}
          placeholder="https://github.com/owner/repository/pull/123"
        />
        <Button
          onClick={() =>
            command("github_process_pull_request_comments", {
              sessionId: selected.id,
              prUrl: commentPrUrl,
              tokenSecret: "github",
            })
              .then(setLifecycleResult)
              .catch(onError)
          }
        >
          {translate("fetchAndProcessComments")}
        </Button>
        <p className="muted small">
          {translate("botCommentNotice")}
        </p>
      </details>
    </div>
  );
}

function CiMonitorPanel({
  selected,
  onError,
}: {
  selected: Session;
  onError: (error: unknown) => void;
}) {
  const [monitorId, setMonitorId] = useState("");
  const [repo, setRepo] = useState("");
  const [pullRequest, setPullRequest] = useState("");
  const [branch, setBranch] = useState("HEAD");
  const [enabled, setEnabled] = useState(false);
  const [monitor, setMonitor] = useState<unknown>(null);
  const [repairItems, setRepairItems] = useState<unknown[]>([]);
  const refresh = () => {
    if (!selected.project_id) return;
    Promise.all([
      invoke<unknown[]>("ci_monitors", { enabledOnly: false }),
      invoke<unknown[]>("ci_repair_status"),
    ])
      .then(([monitors, repairs]) => {
        setRepairItems(
          repairs.filter(
            (item) =>
              typeof item === "object" &&
              item !== null &&
              (item as { project_id?: string }).project_id ===
                selected.project_id,
          ),
        );
        const current = monitors.find(
          (item) =>
            typeof item === "object" &&
            item !== null &&
            (item as { project_id?: string }).project_id ===
              selected.project_id,
        );
        if (current) {
          setMonitor(current);
          setEnabled(Boolean((current as { enabled?: boolean }).enabled));
        }
      })
      .catch(onError);
  };
  useEffect(refresh, [selected.project_id]);
  const save = () =>
    invoke("save_ci_monitor", {
      monitorId,
      projectId: selected.project_id,
      repo,
      pullRequest: Number(pullRequest),
      branch,
      pollIntervalSeconds: 30,
    })
      .then(setMonitor)
      .then(refresh)
      .catch(onError);
  const toggle = () =>
    invoke("set_ci_monitor_enabled", {
      monitorId,
      enabled: !enabled,
    })
      .then((value) => {
        setMonitor(value);
        setEnabled(!enabled);
        refresh();
      })
      .catch(onError);
  return (
    <details>
      <summary>{translate("ciMonitorTitle")}</summary>
      <input
        value={monitorId}
        onChange={(event) => setMonitorId(event.target.value)}
        placeholder={translate("monitorId")}
      />
      <input
        value={repo}
        onChange={(event) => setRepo(event.target.value)}
        placeholder="owner/repository"
      />
      <input
        value={pullRequest}
        onChange={(event) => setPullRequest(event.target.value)}
        placeholder={translate("prNumber")}
      />
      <input
        value={branch}
        onChange={(event) => setBranch(event.target.value)}
        placeholder={translate("branch")}
      />
      <Button onClick={save}>{translate("saveMonitor")}</Button>
      {monitor ? (
        <Button onClick={toggle}>
          {enabled
            ? translate("disableMonitoring")
            : translate("enableMonitoring")}
        </Button>
      ) : null}
      <Button onClick={refresh}>{translate("refreshStatus")}</Button>
      <p className="muted small">
        {translate("ciAuthorizationNotice")}
      </p>
      {repairItems.length > 0 ? (
        <div className="stack">
          {repairItems.map((item) => {
            const record = item as {
              queue_id?: string;
              status?: string;
              attempts?: number;
              max_attempts?: number;
              run_after?: string;
              updated_at?: string;
              payload?: {
                phase?: string;
                repair_attempts?: number;
                max_repair_attempts?: number;
                poll_count?: number;
                max_polls?: number;
                deadline?: string;
                failure_signatures?: string[];
                stop_reason?: string;
                head_sha?: string;
                expected_head_sha?: string;
              };
              progress?: Record<string, unknown>;
            };
            const payload = record.payload ?? {};
            const state = {
              ...payload,
              ...(record.progress ?? {}),
            } as typeof payload;
            const signatures = state.failure_signatures ?? [];
            return (
              <article key={record.queue_id ?? JSON.stringify(item)}>
                <strong>{record.queue_id ?? "ci_repair_loop"}</strong>
                <div>{translate("status")}: {record.status ?? "unknown"}</div>
                <div>{translate("phase")}: {state.phase ?? "queued"}</div>
                <div>
                  {translate("attempts")}: {state.repair_attempts ?? 0} /{" "}
                  {state.max_repair_attempts ?? 3}
                </div>
                <div>
                  {translate("polls")}: {state.poll_count ?? 0} /{" "}
                  {state.max_polls ?? 20}
                </div>
                <div>{translate("deadline")}: {state.deadline ?? "not set"}</div>
                <div>{translate("currentSha")}: {state.head_sha ?? "not set"}</div>
                <div>
                  {translate("expectedSha")}: {state.expected_head_sha ?? "not set"}
                </div>
                <div>{translate("stopReason")}: {state.stop_reason ?? "none"}</div>
                <div>
                  {translate("signatures")}:{" "}
                  {signatures.length > 0 ? signatures.join(" → ") : "none"}
                </div>
              </article>
            );
          })}
        </div>
      ) : null}
    </details>
  );
}

function RunnerPanel({
  selected,
  onError,
}: {
  selected: Session;
  onError: (error: unknown) => void;
}) {
  const [enabled, setEnabled] = useState(false);
  const [hostId, setHostId] = useState("local");
  const [provider, setProvider] = useState("");
  const [model, setModel] = useState("");
  const [workspace, setWorkspace] = useState("");
  const [runnerEnabled, setRunnerEnabled] = useState(false);
  const [maxConcurrency, setMaxConcurrency] = useState("1");
  const refresh = () => {
    if (!selected.project_id) return;
    Promise.all([
      invoke<unknown>("runner_profile", { projectId: selected.project_id }),
      invoke<{ enabled: boolean; max_concurrency: number }>("runner_settings"),
    ])
      .then(([profile, settings]) => {
        if (profile && typeof profile === "object") {
          const value = profile as {
            enabled?: boolean;
            host_id?: string;
            provider?: string;
            model?: string;
            workspace?: string;
          };
          setEnabled(Boolean(value.enabled));
          setHostId(value.host_id ?? "local");
          setProvider(value.provider ?? "");
          setModel(value.model ?? "");
          setWorkspace(value.workspace ?? "");
        }
        setRunnerEnabled(settings.enabled);
        setMaxConcurrency(String(settings.max_concurrency));
      })
      .catch(onError);
  };
  useEffect(refresh, [selected.project_id]);
  const save = () =>
    invoke("save_runner_profile", {
      projectId: selected.project_id,
      hostId,
      provider,
      model,
      workspace,
      enabled,
    })
      .then(refresh)
      .catch(onError);
  const toggleRunner = () =>
    invoke("set_runner_settings", {
      enabled: !runnerEnabled,
      maxConcurrency: Number(maxConcurrency),
    })
      .then(() => setRunnerEnabled(!runnerEnabled))
      .catch(onError);
  return (
    <details>
      <summary>{translate("autonomousRunnerProfile")}</summary>
      <p className="muted small">
        {translate("runnerDescription")}
      </p>
      <input
        value={hostId}
        onChange={(event) => setHostId(event.target.value)}
        placeholder={translate("hostId")}
      />
      <input
        value={provider}
        onChange={(event) => setProvider(event.target.value)}
        placeholder={translate("provider")}
      />
      <input
        value={model}
        onChange={(event) => setModel(event.target.value)}
        placeholder={translate("model")}
      />
      <input
        value={workspace}
        onChange={(event) => setWorkspace(event.target.value)}
        placeholder={translate("workspace")}
      />
      <Button onClick={save}>{translate("saveRunnerProfile")}</Button>
      <label>
        <input
          type="checkbox"
          checked={enabled}
          onChange={(event) => setEnabled(event.target.checked)}
        />
        {translate("profileEnabled")}
      </label>
      <input
        value={maxConcurrency}
        onChange={(event) => setMaxConcurrency(event.target.value)}
        inputMode="numeric"
        placeholder={translate("maxConcurrency")}
      />
      <Button onClick={toggleRunner}>
        {runnerEnabled
          ? translate("disableBackgroundRunner")
          : translate("enableBackgroundRunner")}
      </Button>
      <Button onClick={refresh}>{translate("refreshRunnerSettings")}</Button>
    </details>
  );
}

function DiffView({ diff }: { diff: Record<string, unknown> | null }) {
  if (!diff)
    return (
      <div className="diff-view empty-surface">
        {translate("Select a changed file.")}
      </div>
    );
  const text =
    typeof diff.diff === "string" ? diff.diff : JSON.stringify(diff, null, 2);
  return (
    <pre className="diff-view">
      {text.split("\n").map((line) => (
        <span
          className={
            line.startsWith("+")
              ? "diff-add"
              : line.startsWith("-")
                ? "diff-del"
                : ""
          }
          key={`${line}:${text.indexOf(line)}`}
        >
          {line}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}
function WorklogView({
  selected,
  worklog,
  setWorklog,
  onError,
}: {
  selected: Session;
  worklog: Record<string, unknown> | null;
  setWorklog: (value: Record<string, unknown>) => void;
  onError: (error: unknown) => void;
}) {
  const load = () =>
    command<Record<string, unknown>>("session_worklog", {
      sessionId: selected.id,
      afterId: "",
      limit: 200,
    })
      .then(setWorklog)
      .catch(onError);
  useEffect(() => {
    load();
  }, [selected.id]);
  const events = Array.isArray(worklog?.events) ? worklog.events : [];
  const [expanded, setExpanded] = useState<Record<number, boolean>>({});
  const eventTitle = (event: unknown) => {
    if (!event || typeof event !== "object") return "Worklog event";
    const value = event as Record<string, unknown>;
    return String(
      value.title ??
        value.name ??
        value.type ??
        value.category ??
        "Worklog event",
    );
  };
  const eventTime = (event: unknown) => {
    const value = event as Record<string, unknown>;
    const raw = value.ts ?? value.timestamp ?? value.created_at;
    if (typeof raw !== "string" && typeof raw !== "number") return "";
    const date = new Date(raw);
    return Number.isNaN(date.valueOf())
      ? ""
      : date.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  };
  const eventDetail = (event: unknown) => {
    if (!event || typeof event !== "object") return String(event);
    const value = event as Record<string, unknown>;
    const command = value.command ?? value.cmd;
    const output = value.output ?? value.stdout ?? value.result;
    if (command || output) {
      return (
        <div className="worklog-detail-stack">
          {command ? (
            <pre className="output-pre">$ {String(command)}</pre>
          ) : null}
          {output ? <pre className="output-pre">{String(output)}</pre> : null}
        </div>
      );
    }
    const diffText = value.diff ?? value.patch;
    if (diffText) return <pre className="diff-view">{String(diffText)}</pre>;
    return (
      <dl className="worklog-fields">
        {Object.entries(value).map(([key, item]) => (
          <div key={key}>
            <dt>{key}</dt>
            <dd>{typeof item === "string" ? item : JSON.stringify(item)}</dd>
          </div>
        ))}
      </dl>
    );
  };
  return (
    <div className="surface-panel">
      <div className="surface-toolbar">
        <span>{translate("Worklog timeline")}</span>
        <Button onClick={load}>
          <Icon name="refresh" /> Reload
        </Button>
      </div>
      {Boolean(worklog?.window_lost) && (
        <div className="warning">
          The requested worklog window was lost. Reloaded from the current
          window.
        </div>
      )}
      {!worklog && (
        <div className="empty-surface">
          <p>{translate("Load the remote worklog for this session.")}</p>
          <Button onClick={load}>{translate("openWorklog")}</Button>
        </div>
      )}
      <div className="worklog-timeline">
        {events.map((event, index) => {
          const isExpanded = Boolean(expanded[index]);
          return (
            <div
              className={`worklog-entry${isExpanded ? " expanded" : ""}`}
              key={`${index}-${JSON.stringify(event)}`}
            >
              <span className="timeline-dot" aria-hidden="true" />
              <div className="worklog-entry-body">
                <button
                  className="worklog-entry-head"
                  onClick={() =>
                    setExpanded((items) => ({
                      ...items,
                      [index]: !items[index],
                    }))
                  }
                  aria-expanded={isExpanded}
                >
                  <Icon name="clock" size={14} />
                  <strong>{eventTitle(event)}</strong>
                  <time>{eventTime(event)}</time>
                  <Icon
                    name={isExpanded ? "chevronDown" : "chevronRight"}
                    size={14}
                  />
                </button>
                {isExpanded && (
                  <div className="worklog-entry-detail">
                    {eventDetail(event)}
                  </div>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function ManageSections({
  tab,
  hosts,
  assets,
  providers,
  secrets,
  projects,
  selected,
  onRefresh,
  onError,
  onAddHost,
  onEditHost,
  onTestHost,
  onDeleteHost,
  onPromptDraft,
  hostName,
  setHostName,
  hostUrl,
  setHostUrl,
  hostToken,
  setHostToken,
  vncPassword,
  setVncPassword,
  editingHostId,
  setEditingHostId,
}: {
  tab: SettingsSection;
  hosts: Host[];
  assets: Asset[];
  providers: ProviderDescriptor[];
  secrets: SecretMetadata[];
  projects: Project[];
  selected: Session | null;
  onRefresh: () => void;
  onError: (error: unknown) => void;
  onAddHost: (event: FormEvent) => void;
  onEditHost: (host: Host) => Promise<void>;
  onTestHost: (hostId: string) => Promise<Host>;
  onDeleteHost: (hostId: string) => Promise<void>;
  onPromptDraft: (draft: string) => void;
  hostName: string;
  setHostName: (value: string) => void;
  hostUrl: string;
  setHostUrl: (value: string) => void;
  hostToken: string;
  setHostToken: (value: string) => void;
  vncPassword: string;
  setVncPassword: (value: string) => void;
  editingHostId: string | null;
  setEditingHostId: (value: string | null) => void;
}) {
  // Body shell follows OpenWorker SettingsView.tsx:85-123. Asset-specific
  // rows use the existing CollectionPage/manage-row vocabulary because these
  // OPCOS configuration objects have no one-to-one reference component.
  const [provider, setProvider] = useState("openai");
  const [baseUrl, setBaseUrl] = useState("");
  const [key, setKey] = useState("");
  const [providerStatus, setProviderStatus] = useState("");
  const [providerConfigs, setProviderConfigs] = useState<
    Array<{
      provider: string;
      base_url?: string;
      account_id?: string;
      model?: string;
      configured: boolean;
    }>
  >([]);
  const [providerKeys, setProviderKeys] = useState<Record<string, string>>({});
  const [providerStatuses, setProviderStatuses] = useState<
    Record<string, string>
  >({});
  const [agentSettings, setAgentSettings] = useState<AgentSettings | null>(
    null,
  );
  const [settingsProjectId, setSettingsProjectId] = useState<string | null>(
    null,
  );
  const [agentSettingsStatus, setAgentSettingsStatus] = useState("");
  const [slashCommands, setSlashCommands] = useState<SlashCommand[]>([]);
  const [slashCommandName, setSlashCommandName] = useState("");
  const [slashCommandBody, setSlashCommandBody] = useState("");
  const [slashCommandKind, setSlashCommandKind] = useState<"system" | "custom">(
    "custom",
  );
  const [skillUsage, setSkillUsage] = useState<SkillUsageDashboard | null>(
    null,
  );
  const [skillBrowse, setSkillBrowse] = useState<SkillRulesBrowse | null>(null);
  const [libraryEntries, setLibraryEntries] = useState<LibraryEntry[]>([]);
  const [libraryKind, setLibraryKind] = useState<
    "agent-template" | "team-template" | "command"
  >("agent-template");
  const [templateDraftName, setTemplateDraftName] = useState("");
  const [templateDraftId, setTemplateDraftId] = useState<string | null>(null);
  const [templateDraftDescription, setTemplateDraftDescription] = useState("");
  const [templateDraftContent, setTemplateDraftContent] = useState("{}");
  const [templateDraftStatus, setTemplateDraftStatus] = useState("");
  const [templateEditorOpen, setTemplateEditorOpen] = useState(false);
  const [libraryProjectId, setLibraryProjectId] = useState("");
  const [environmentTab, setEnvironmentTab] = useState<
    "blueprints" | "snapshots" | "advanced" | "outposts"
  >("blueprints");
  const [blueprintStatus, setBlueprintStatus] =
    useState<BlueprintStatus | null>(null);
  const [blueprintDraft, setBlueprintDraft] = useState("");
  const [environmentRepositories, setEnvironmentRepositories] = useState<
    EnvironmentRepository[]
  >([]);
  const [environmentStatus, setEnvironmentStatus] = useState("");
  const [providerModelOptions, setProviderModelOptions] = useState<
    Record<string, ProviderModelsResponse>
  >({});
  const [showAllProviderModels, setShowAllProviderModels] = useState<
    Record<string, boolean>
  >({});
  const [providerModels, setProviderModels] = useState<Record<string, string>>(
    {},
  );
  const [selectedProvider, setSelectedProvider] = useState<string | null>(null);
  const [customProviderEditorOpen, setCustomProviderEditorOpen] =
    useState(false);
  const [customProviderId, setCustomProviderId] = useState<string | null>(null);
  const [customProviderName, setCustomProviderName] = useState("");
  const [customProviderDialect, setCustomProviderDialect] =
    useState("openai-compatible");
  const [customProviderBaseUrl, setCustomProviderBaseUrl] = useState("");
  const [customProviderKey, setCustomProviderKey] = useState("");
  const [customProviderModel, setCustomProviderModel] = useState("");
  const [customProviderStatus, setCustomProviderStatus] = useState("");
  const [assetTitle, setAssetTitle] = useState("");
  const [assetBody, setAssetBody] = useState("");
  const [instructionsDraft, setInstructionsDraft] = useState("");
  const [assetKind, setAssetKind] = useState<Asset["kind"]>("knowledge");
  const [assetTrigger, setAssetTrigger] = useState("");
  const [assetScope, setAssetScope] = useState("");
  const [assetScopeKind, setAssetScopeKind] = useState<"global" | "repo">(
    "global",
  );
  const [editingAssetId, setEditingAssetId] = useState<string | null>(null);
  const [assetPending, setAssetPending] = useState<string | null>(null);
  const [assetFormOpen, setAssetFormOpen] = useState(false);
  const [versionHistoryAsset, setVersionHistoryAsset] = useState<string | null>(
    null,
  );
  const [assetVersions, setAssetVersions] = useState<
    Array<Record<string, unknown>>
  >([]);
  const [compareVersionId, setCompareVersionId] = useState<string | null>(null);
  const [assetSearch, setAssetSearch] = useState("");
  const [assetStatus, setAssetStatus] = useState("All");
  const [discoveredAssets, setDiscoveredAssets] = useState<Asset[] | null>(
    null,
  );
  const [remoteAssetAction, setRemoteAssetAction] = useState<
    "discovering" | "importing" | "exporting" | null
  >(null);
  const [theme, setTheme] = useState<"light" | "dark" | "auto">(() => {
    const stored = localStorage.getItem("opcos.theme");
    return stored === "dark" || stored === "auto" ? stored : "light";
  });
  useEffect(() => {
    localStorage.setItem("opcos.theme", theme);
    const dark =
      theme === "dark" ||
      (theme === "auto" &&
        window.matchMedia("(prefers-color-scheme: dark)").matches);
    document.documentElement.dataset.theme = dark ? "dark" : "light";
  }, [theme]);
  const [locale, setCurrentLocale] = useState(getLocale());
  useEffect(() => subscribeLocale(() => setCurrentLocale(getLocale())), []);
  useEffect(() => {
    if (tab !== "connectors") return;
    for (const kind of [
      ...TOKEN_CONNECTOR_KINDS,
      ...OAUTH_CONNECTOR_KINDS,
      "whatsapp",
      "email (imap)",
    ]) {
      void command<TokenConnectorStatus>("connector_status", { kind })
        .then((status) =>
          setConnectorStatuses((items) => ({ ...items, [kind]: status })),
        )
        .catch(() => undefined);
    }
    let disposed = false;
    const subscription = listen<{ kind: string }>(
      "connector-oauth-complete",
      (event) => {
        if (disposed) return;
        const kind = event.payload.kind;
        void command<TokenConnectorStatus>("connector_status", { kind })
          .then((status) => {
            setConnectorStatuses((items) => ({ ...items, [kind]: status }));
            setConnectorMessages((items) => ({
              ...items,
              [kind]: `Connected as ${status.identity || "account"}.`,
            }));
          })
          .catch((error) => {
            setConnectorMessages((items) => ({
              ...items,
              [kind]: errorMessage(error),
            }));
          });
      },
    );
    return () => {
      disposed = true;
      void subscription.then((unlisten) => unlisten());
    };
  }, [tab, onError]);
  const [blueprint, setBlueprint] = useState<Record<string, unknown> | null>(
    null,
  );
  const [blueprintCommand, setBlueprintCommand] = useState("");
  const [testingHostId, setTestingHostId] = useState<string | null>(null);
  const [deletingHostId, setDeletingHostId] = useState<string | null>(null);
  const [confirmDeleteHostId, setConfirmDeleteHostId] = useState<string | null>(
    null,
  );
  const [hostFormOpen, setHostFormOpen] = useState(false);
  const [linearPat, setLinearPat] = useState("");
  const [linearStatus, setLinearStatus] = useState("");
  const [connectorTokens, setConnectorTokens] = useState<
    Record<string, string>
  >({});
  const [connectorConfigs, setConnectorConfigs] = useState<
    Record<string, Record<string, string>>
  >({});
  const [connectorStatuses, setConnectorStatuses] = useState<
    Record<string, TokenConnectorStatus>
  >({});
  const [connectorMessages, setConnectorMessages] = useState<
    Record<string, string>
  >({});
  const [openConnector, setOpenConnector] = useState<string | null>(null);
  const [ingressSources, setIngressSources] = useState<ExternalIngressSource[]>(
    [],
  );
  const [ingressProvider, setIngressProvider] = useState<"github" | "rss">(
    "github",
  );
  const [ingressTarget, setIngressTarget] = useState("");
  const [ingressInterval, setIngressInterval] = useState("60");
  const [editingIngressId, setEditingIngressId] = useState<string | null>(null);
  const [ingressStatus, setIngressStatus] = useState("");
  const loadIngress = () =>
    command<ExternalIngressSource[]>("external_ingress_sources", {
      enabledOnly: false,
    }).then(setIngressSources);
  useEffect(() => {
    if (tab !== "ingress") return;
    void loadIngress().catch(onError);
  }, [tab, onError]);
  const [secretFormOpen, setSecretFormOpen] = useState(false);
  const [secretName, setSecretName] = useState("");
  const [secretScope, setSecretScope] = useState("global");
  const [secretPurpose, setSecretPurpose] = useState("");
  const [secretValue, setSecretValue] = useState("");
  const [linearIssueId, setLinearIssueId] = useState("");
  const [linearIssue, setLinearIssue] = useState<Record<
    string,
    unknown
  > | null>(null);
  const [linearIssues, setLinearIssues] = useState<
    Array<Record<string, unknown>>
  >([]);
  const [indexStatus, setIndexStatus] = useState<{
    status: string;
    built_at?: string;
    file_count: number;
    symbol_count: number;
    truncated: boolean;
    reason?: string;
  } | null>(null);
  useEffect(() => {
    if (tab !== "index" || !selected) {
      setIndexStatus(null);
      return;
    }
    void command<typeof indexStatus>("repo_index_status", {
      sessionId: selected.id,
    })
      .then(setIndexStatus)
      .catch(onError);
  }, [tab, selected, onError]);
  const sectionCopy: Record<SettingsSection, [string, string]> = {
    provider: [
      "Provider",
      "Choose a provider and validate its connection key.",
    ],
    hosts: ["Hosts", "Bind and test the remote hosts used by OPCOS sessions."],
    agents: ["规则", "仓库级运行规则（对应仓库中的 AGENTS.md 文件）。"],
    instructions: ["全局指令", "应用于所有新会话的全局指令。"],
    knowledge: ["Knowledge", "Reusable reference material added to context."],
    playbook: ["Playbook", "Repeatable workflows available to automation."],
    skill: ["Skill", "Focused capability and instruction bundles."],
    mcp: ["MCP", "Control the tools exposed by the selected remote host."],
    connectors: [
      "Connectors",
      "Linear is connected locally with a Personal API Key. Other connectors are not integrated.",
    ],
    ingress: [
      "External events",
      "Poll GitHub and RSS/Atom sources that can wake OPCOS event rules.",
    ],
    index: [
      "Repository index",
      "Build a host-backed path and symbol index before asking the agent to change code.",
    ],
    secrets: [
      "Secrets",
      "Inspect secret metadata without exposing secret values.",
    ],
    blueprint: ["Blueprint", "Read and manage the selected host blueprint."],
    appearance: [translate("general"), translate("appearanceDescription")],
    agent: [
      "Agent defaults",
      "控制会话默认值、Computer use、用量上限和 Pull request 策略。",
    ],
    environment: [
      "Environment",
      "管理 Blueprint、固定环境说明、有序仓库 setup 和长期主机。",
    ],
    experts: ["专家", "管理可供项目启用的专家库。"],
    teams: ["团队", "管理可供项目启用的团队库。"],
    command: ["Command", "管理可供项目启用的命令库。"],
  };
  const assetKinds = [
    "agents",
    "instructions",
    "knowledge",
    "playbook",
    "skill",
  ] as const;
  const assetTabKind = assetKinds.includes(tab as (typeof assetKinds)[number])
    ? (tab as Asset["kind"])
    : "knowledge";
  const assetTabVisible =
    tab !== "skill" && assetKinds.includes(tab as (typeof assetKinds)[number]);
  const activeLibraryKind =
    tab === "experts"
      ? "agent-template"
      : tab === "teams"
        ? "team-template"
        : tab === "command"
          ? "command"
          : libraryKind;
  const assetLabel =
    assetTabKind === "agents"
      ? "规则"
      : assetTabKind[0].toUpperCase() + assetTabKind.slice(1);
  useEffect(() => {
    if (tab !== "instructions") return;
    setInstructionsDraft(
      assets.find((asset) => asset.kind === "instructions")?.body || "",
    );
  }, [assets, tab]);
  useEffect(() => {
    void command<Record<string, unknown>>("provider_settings")
      .then((value) => {
        setProvider(String(value.provider || "openai"));
        setBaseUrl(String(value.base_url || ""));
      })
      .catch(onError);
  }, []);
  useEffect(() => {
    if (tab !== "agent") return;
    void command<AgentSettings>("agent_settings", {
      projectId: settingsProjectId,
    })
      .then(setAgentSettings)
      .catch(onError);
    void command<SlashCommand[]>("list_slash_commands", {
      projectId: settingsProjectId,
    })
      .then(setSlashCommands)
      .catch(onError);
  }, [tab, settingsProjectId, onError]);
  useEffect(() => {
    if (tab !== "skill") return;
    void command<SkillUsageDashboard>("skill_usage_dashboard", {
      projectId: settingsProjectId,
    })
      .then(setSkillUsage)
      .catch(onError);
    if (!selected) {
      setSkillBrowse(null);
      return;
    }
    void command<SkillRulesBrowse>("browse_skill_rules", {
      sessionId: selected.id,
    })
      .then(setSkillBrowse)
      .catch(onError);
  }, [tab, settingsProjectId, selected, onError]);
  useEffect(() => {
    if (!["experts", "teams", "command"].includes(tab)) return;
    void command<LibraryEntry[]>("list_configured_library")
      .then(setLibraryEntries)
      .catch(onError);
  }, [tab, onError]);
  useEffect(() => {
    if (tab !== "environment") return;
    void command<EnvironmentRepository[]>("list_environment_repositories", {
      projectId: settingsProjectId,
    })
      .then(setEnvironmentRepositories)
      .catch(onError);
    if (!selected) {
      setBlueprintStatus(null);
      return;
    }
    void command<BlueprintStatus>("blueprint_status", {
      sessionId: selected.id,
    })
      .then((value) => {
        setBlueprintStatus(value);
        setBlueprintDraft(value.content);
      })
      .catch(onError);
  }, [tab, settingsProjectId, selected, onError]);
  useEffect(() => {
    void command<
      Array<{ provider: string; base_url?: string; configured: boolean }>
    >("provider_configurations")
      .then(setProviderConfigs)
      .catch(onError);
  }, []);
  useEffect(() => {
    if (!providers.length) return;
    void Promise.all(
      providers.map(
        async (item) =>
          [
            item.name,
            await command<ProviderModelsResponse>("provider_models", {
              provider: item.name,
            }),
          ] as const,
      ),
    ).then((entries) => setProviderModelOptions(Object.fromEntries(entries)));
  }, [providers]);
  return (
    <section className="settings-body">
      <header className="mb-5">
        <h1 className="text-[22px] font-semibold text-ink">
          {sectionCopy[tab][0]}
        </h1>
        <p className="text-[13px] text-muted mt-1">{sectionCopy[tab][1]}</p>
      </header>
      <div
        className={
          tab === "appearance" ||
          tab === "provider" ||
          tab === "blueprint" ||
          tab === "agent" ||
          tab === "experts" ||
          tab === "teams" ||
          tab === "command"
            ? "rounded-xl2 border border-line bg-panel p-5"
            : ""
        }
      >
        {tab === "appearance" && (
          <div className="divide-y divide-line">
            <div className="settings-row">
              <div>
                <strong>{translate("theme")}</strong>
                <small>{translate("themeDescription")}</small>
              </div>
              <div className="seg">
                {(["light", "dark", "auto"] as const).map((value) => (
                  <button
                    key={value}
                    className={theme === value ? "active" : ""}
                    onClick={() => setTheme(value)}
                    type="button"
                  >
                    {translate(value)}
                  </button>
                ))}
              </div>
            </div>
            <div className="settings-row">
              <div>
                <strong>{translate("language")}</strong>
                <small>{translate("languageDescription")}</small>
              </div>
              <SelectMenu
                value={locale}
                onChange={(value) => setLocale(value as "en" | "zh")}
                options={[
                  { value: "en", label: translate("english") },
                  { value: "zh", label: translate("chinese") },
                ]}
              />
            </div>
          </div>
        )}
        {tab === "environment" && (
          <div className="space-y-5">
            <div className="flex gap-2 border-b border-line pb-2">
              {(
                [
                  ["blueprints", "Blueprints"],
                  ["snapshots", "Snapshots"],
                  ["advanced", "Advanced"],
                  ["outposts", "Outposts"],
                ] as const
              ).map(([value, label]) => (
                <button
                  className={`px-3 py-1.5 rounded-md text-sm ${
                    environmentTab === value
                      ? "bg-paper text-accent font-medium"
                      : "text-muted"
                  }`}
                  key={value}
                  onClick={() => setEnvironmentTab(value)}
                  type="button"
                >
                  {label}
                </button>
              ))}
            </div>
            {environmentTab === "blueprints" && (
              <section className="rounded-lg border border-line p-4 space-y-3">
                <div>
                <strong>{translate("blueprints")}</strong>
                  <small className="block">
                    当前生效来源：
                    {blueprintStatus?.source === "project"
                      ? "项目 Blueprint"
                      : blueprintStatus?.source === "global"
                        ? "全局 Blueprint"
                        : blueprintStatus?.source === "repository"
                          ? "仓库 .devin/blueprint.yaml"
                          : "未读取"}
                  </small>
                </div>
                {!selected ? (
                  <div className="text-sm text-muted">
                    请先打开一个项目会话查看 Blueprint
                  </div>
                ) : (
                  <>
                    <textarea
                      className="w-full min-h-64 rounded-md border border-line bg-paper p-3 font-mono text-xs"
                      value={blueprintDraft}
                      onChange={(event) =>
                        setBlueprintDraft(event.target.value)
                      }
                      placeholder={translate("yamlBlueprint")}
                    />
                    <div className="flex justify-end gap-2">
                      <Button
                        onClick={() =>
                          command<BlueprintStatus>("blueprint_status", {
                            sessionId: selected.id,
                          })
                            .then((value) => {
                              setBlueprintStatus(value);
                              setBlueprintDraft(value.content);
                            })
                            .catch(onError)
                        }
                      >
                        重新读取
                      </Button>
                      <Button
                        className="primary"
                        onClick={() =>
                          command("save_asset", {
                            id: settingsProjectId
                              ? `project-blueprint-${settingsProjectId}`
                              : "global-blueprint",
                            kind: "blueprint",
                            title: "Blueprint",
                            body: blueprintDraft,
                            scopeKind: settingsProjectId ? "project" : "global",
                            projectId: settingsProjectId,
                            enabled: true,
                          })
                            .then(() =>
                              setEnvironmentStatus("Blueprint 已保存"),
                            )
                            .catch(onError)
                        }
                      >
                        保存当前作用域
                      </Button>
                    </div>
                    {environmentStatus && (
                      <small className="text-muted">{environmentStatus}</small>
                    )}
                  </>
                )}
              </section>
            )}
            {environmentTab === "snapshots" && (
              <section className="rounded-lg border border-line p-4">
                <strong>{translate("snapshots")}</strong>
                <p className="mt-2 text-sm text-muted">
                  本产品不适用：Local/RVM
                  是长期固定环境，不提供快照，也不伪造等价的快照能力。
                </p>
              </section>
            )}
            {environmentTab === "advanced" && (
              <section className="rounded-lg border border-line p-4 space-y-3">
                <div>
                  <strong>{translate("advancedRepositories")}</strong>
                  <small className="block">
                    setup executor 会按此列表顺序执行 clone 与 setup。
                  </small>
                </div>
                {environmentRepositories.map((item, index) => (
                  <div
                    className="grid grid-cols-[1fr_1fr_auto] gap-2"
                    key={index}
                  >
                    <input
                      value={item.repository}
                      placeholder={translate("repositoryUrl")}
                      onChange={(event) =>
                        setEnvironmentRepositories((current) =>
                          current.map((entry, entryIndex) =>
                            entryIndex === index
                              ? { ...entry, repository: event.target.value }
                              : entry,
                          ),
                        )
                      }
                    />
                    <input
                      value={item.setup_command}
                      placeholder={translate("setupCommand")}
                      onChange={(event) =>
                        setEnvironmentRepositories((current) =>
                          current.map((entry, entryIndex) =>
                            entryIndex === index
                              ? { ...entry, setup_command: event.target.value }
                              : entry,
                          ),
                        )
                      }
                    />
                    <div className="flex gap-1">
                      <Button
                        disabled={index === 0}
                        onClick={() =>
                          setEnvironmentRepositories((current) => {
                            const next = [...current];
                            [next[index - 1], next[index]] = [
                              next[index],
                              next[index - 1],
                            ];
                            return next;
                          })
                        }
                      >
                        ↑
                      </Button>
                      <Button
                        disabled={index === environmentRepositories.length - 1}
                        onClick={() =>
                          setEnvironmentRepositories((current) => {
                            const next = [...current];
                            [next[index], next[index + 1]] = [
                              next[index + 1],
                              next[index],
                            ];
                            return next;
                          })
                        }
                      >
                        ↓
                      </Button>
                      <Button
                        onClick={() =>
                          setEnvironmentRepositories((current) =>
                            current.filter(
                              (_, entryIndex) => entryIndex !== index,
                            ),
                          )
                        }
                      >
                        删除
                      </Button>
                    </div>
                  </div>
                ))}
                <div className="flex justify-between">
                  <Button
                    onClick={() =>
                      setEnvironmentRepositories((current) => [
                        ...current,
                        {
                          position: current.length,
                          repository: "",
                          setup_command: "",
                        },
                      ])
                    }
                  >
                    添加仓库
                  </Button>
                  <Button
                    className="primary"
                    onClick={() =>
                      command("save_environment_repositories", {
                        projectId: settingsProjectId,
                        repositories: environmentRepositories,
                      })
                        .then(() => setEnvironmentStatus("顺序与设置已保存"))
                        .catch(onError)
                    }
                  >
                    保存顺序
                  </Button>
                </div>
                {environmentStatus && (
                  <small className="text-muted">{environmentStatus}</small>
                )}
              </section>
            )}
            {environmentTab === "outposts" && (
              <section className="rounded-lg border border-line p-4">
                <strong>{translate("outposts")}</strong>
                <p className="mt-2 text-sm text-muted">
                  OPCOS 将 Outposts 映射为已登记的长期主机（Local/RVM）；
                  这里展示主机清单，不额外虚构独立 Outpost 资源。
                </p>
                <div className="mt-3 space-y-2">
                  {hosts.length === 0 ? (
                    <div className="text-sm text-muted">
                      {translate("noRegisteredHosts")}
                    </div>
                  ) : (
                    hosts.map((host) => (
                      <div
                        className="flex items-center justify-between rounded-md border border-line p-3"
                        key={host.id}
                      >
                        <span>{host.name}</span>
                        <span className="text-xs text-muted">
                          {host.id} · {host.online === false ? "离线" : "在线"}
                        </span>
                      </div>
                    ))
                  )}
                </div>
              </section>
            )}
          </div>
        )}
        {tab === "agent" && agentSettings && (
          <div className="divide-y divide-line">
            <label className="settings-row">
              <div>
                <strong>{translate("configurationScope")}</strong>
                <small>{translate("configurationScopeDescription")}</small>
              </div>
              <SelectMenu
                value={settingsProjectId || ""}
                onChange={(value) => setSettingsProjectId(value || null)}
                options={[
                  { value: "", label: "全局" },
                  ...projects.map((project) => ({
                    value: project.id,
                    label: project.name,
                  })),
                ]}
              />
            </label>
            <div className="settings-row">
              <div>
                <strong>{translate("computerUse")}</strong>
                <small>{translate("computerUseDescription")}</small>
              </div>
              <input
                type="checkbox"
                checked={agentSettings.computer_use}
                onChange={(event) =>
                  setAgentSettings({
                    ...agentSettings,
                    computer_use: event.target.checked,
                  })
                }
              />
            </div>
            {(
              [
                ["default_agent", "Default agent"],
                ["api_default_agent", "API default agent"],
                ["default_platform", "Default platform"],
              ] as const
            ).map(([keyName, label]) => (
              <label className="settings-row" key={keyName}>
                <div>
                  <strong>{label}</strong>
                  <small>{translate("newSessionDefaults")}</small>
                </div>
                <input
                  value={agentSettings[keyName]}
                  onChange={(event) =>
                    setAgentSettings({
                      ...agentSettings,
                      [keyName]: event.target.value,
                    })
                  }
                />
              </label>
            ))}
            <label className="settings-row">
              <div>
                <strong>{translate("batchLimit")}</strong>
                <small>{translate("batchLimitDescription")}</small>
              </div>
              <input
                type="number"
                min={1}
                max={500}
                value={agentSettings.batch_limit}
                onChange={(event) =>
                  setAgentSettings({
                    ...agentSettings,
                    batch_limit: Number(event.target.value),
                  })
                }
              />
            </label>
            <label className="settings-row">
              <div>
                <strong>{translate("messageUsageLimit")}</strong>
                <small>{translate("messageUsageLimitDescription")}</small>
              </div>
              <input
                type="number"
                min={0}
                value={agentSettings.message_usage_limit}
                onChange={(event) =>
                  setAgentSettings({
                    ...agentSettings,
                    message_usage_limit: Number(event.target.value),
                  })
                }
              />
            </label>
            {(
              [
                ["share_prompts_in_prs", "Share prompts in PRs"],
                ["require_agent_mention", "Require the agent to respond"],
                ["auto_add_reviewer", "Auto-add reviewer"],
              ] as const
            ).map(([keyName, label]) => (
              <label className="settings-row" key={keyName}>
                <div>
                  <strong>{label}</strong>
                  <small>{translate("pullRequestWorkflow")}</small>
                </div>
                <input
                  type="checkbox"
                  checked={agentSettings[keyName]}
                  onChange={(event) =>
                    setAgentSettings({
                      ...agentSettings,
                      [keyName]: event.target.checked,
                    })
                  }
                />
              </label>
            ))}
            <label className="settings-row">
              <div>
                <strong>{translate("reviewer")}</strong>
                <small>{translate("reviewerDescription")}</small>
              </div>
              <input
                value={agentSettings.reviewer}
                onChange={(event) =>
                  setAgentSettings({
                    ...agentSettings,
                    reviewer: event.target.value,
                  })
                }
              />
            </label>
            <label className="settings-row">
              <div>
                <strong>{translate("openPrsAs")}</strong>
                <small>{translate("openPrsAsDescription")}</small>
              </div>
              <SelectMenu
                value={agentSettings.open_prs_as}
                onChange={(value) =>
                  setAgentSettings({
                    ...agentSettings,
                    open_prs_as: value as "draft" | "ready",
                  })
                }
                options={[
                  { value: "ready", label: "Ready" },
                  { value: "draft", label: "Draft" },
                ]}
              />
            </label>
            <label className="settings-row">
              <div>
                <strong>{translate("respondingToBots")}</strong>
                <small>{translate("respondingToBotsDescription")}</small>
              </div>
              <SelectMenu
                value={agentSettings.responding_to_bots}
                onChange={(value) =>
                  setAgentSettings({
                    ...agentSettings,
                    responding_to_bots: value as "ignore" | "respond",
                  })
                }
                options={[
                  { value: "ignore", label: "Ignore" },
                  { value: "respond", label: "Respond" },
                ]}
              />
            </label>
            <div className="settings-row justify-end gap-3">
              {agentSettingsStatus && <small>{agentSettingsStatus}</small>}
              <Button
                className="primary"
                onClick={() =>
                  command("save_agent_settings", {
                    projectId: settingsProjectId,
                    value: agentSettings,
                  })
                    .then(() => setAgentSettingsStatus("已保存"))
                    .catch(onError)
                }
              >
                保存 Agent 设置
              </Button>
            </div>
            <div className="pt-5">
              <div className="flex items-center justify-between mb-3">
                <div>
                  <strong>{translate("manageCommands")}</strong>
                  <small className="block">
                    System 命令可覆盖或 Reset；Custom 命令可添加、编辑和删除。
                  </small>
                </div>
                <Button
                  onClick={() =>
                    command("reset_slash_commands", {
                      projectId: settingsProjectId,
                      name: null,
                    })
                      .then(() =>
                        command<SlashCommand[]>("list_slash_commands", {
                          projectId: settingsProjectId,
                        }),
                      )
                      .then(setSlashCommands)
                      .catch(onError)
                  }
                >
                  Reset system
                </Button>
              </div>
              <div className="space-y-2">
                {slashCommands.map((item) => (
                  <div
                    className="rounded-lg border border-line p-3"
                    key={item.name}
                  >
                    <div className="flex items-center gap-2">
                      <code className="text-accent">{item.name}</code>
                      <span className="text-[11px] text-muted">
                        {item.kind === "system" ? "System" : "Custom"}
                      </span>
                      <span className="ml-auto flex gap-2">
                        {item.kind === "system" && (
                          <Button
                            onClick={() =>
                              command("reset_slash_commands", {
                                projectId: settingsProjectId,
                                name: item.name,
                              })
                                .then(() =>
                                  command<SlashCommand[]>(
                                    "list_slash_commands",
                                    { projectId: settingsProjectId },
                                  ),
                                )
                                .then(setSlashCommands)
                                .catch(onError)
                            }
                          >
                            Reset
                          </Button>
                        )}
                        {item.kind === "custom" && (
                          <Button
                            onClick={() =>
                              command("delete_slash_command", {
                                projectId: settingsProjectId,
                                name: item.name,
                              })
                                .then(() =>
                                  command<SlashCommand[]>(
                                    "list_slash_commands",
                                    { projectId: settingsProjectId },
                                  ),
                                )
                                .then(setSlashCommands)
                                .catch(onError)
                            }
                          >
                            删除
                          </Button>
                        )}
                      </span>
                    </div>
                    <textarea
                      className="mt-2 w-full rounded-md border border-line bg-paper p-2 text-[13px]"
                      value={item.body}
                      onChange={(event) =>
                        setSlashCommands((current) =>
                          current.map((commandItem) =>
                            commandItem.name === item.name
                              ? { ...commandItem, body: event.target.value }
                              : commandItem,
                          ),
                        )
                      }
                    />
                    <div className="mt-2 flex justify-end">
                      <Button
                        className="primary"
                        onClick={() =>
                          command("save_slash_command", {
                            projectId: settingsProjectId,
                            name: item.name,
                            body: item.body,
                            kind: item.kind,
                          }).catch(onError)
                        }
                      >
                        保存
                      </Button>
                    </div>
                  </div>
                ))}
              </div>
              <div className="mt-4 rounded-lg border border-dashed border-line p-3">
                <strong>{translate("addCommand")}</strong>
                <div className="mt-2 grid gap-2">
                  <input
                    placeholder="/command"
                    value={slashCommandName}
                    onChange={(event) =>
                      setSlashCommandName(event.target.value)
                    }
                  />
                  <select
                    value={slashCommandKind}
                    onChange={(event) =>
                      setSlashCommandKind(
                        event.target.value as "system" | "custom",
                      )
                    }
                  >
                    <option value="custom">{translate("custom")}</option>
                    <option value="system">{translate("systemOverride")}</option>
                  </select>
                  <textarea
                    placeholder={translate("commandPromptTemplate")}
                    value={slashCommandBody}
                    onChange={(event) =>
                      setSlashCommandBody(event.target.value)
                    }
                  />
                  <Button
                    className="primary justify-self-end"
                    onClick={() =>
                      command("save_slash_command", {
                        projectId: settingsProjectId,
                        name: slashCommandName,
                        body: slashCommandBody,
                        kind: slashCommandKind,
                      })
                        .then(() =>
                          command<SlashCommand[]>("list_slash_commands", {
                            projectId: settingsProjectId,
                          }),
                        )
                        .then((items) => {
                          setSlashCommands(items);
                          setSlashCommandName("");
                          setSlashCommandBody("");
                        })
                        .catch(onError)
                    }
                  >
                    Add Command
                  </Button>
                </div>
              </div>
            </div>
          </div>
        )}
        {tab === "provider" &&
          (selectedProvider === null ? (
            <div className="space-y-3">
              <div className="flex justify-end">
                <Button
                  className="primary"
                  onClick={() => {
                    setCustomProviderId(null);
                    setCustomProviderName("");
                    setCustomProviderDialect("openai-compatible");
                    setCustomProviderBaseUrl("");
                    setCustomProviderKey("");
                    setCustomProviderModel("");
                    setCustomProviderStatus("");
                    setCustomProviderEditorOpen(true);
                  }}
                >
                  添加自定义 Provider
                </Button>
              </div>
              <div className="grid grid-cols-[repeat(auto-fill,minmax(min(100%,440px),440px))] gap-2.5">
                {providers.map((descriptor) => {
                  const config = providerConfigs.find(
                    (item) => item.provider === descriptor.name,
                  );
                  return (
                    <IntegrationCard
                      key={descriptor.name}
                      icon={descriptor.title.slice(0, 1)}
                      title={descriptor.title}
                      badge={{
                        label:
                          descriptor.available === false
                            ? "Not integrated"
                            : config?.configured
                              ? "Enabled"
                              : "Not configured",
                        tone:
                          descriptor.available === false || !config?.configured
                            ? "neutral"
                            : "success",
                      }}
                      description={
                        descriptor.available === false
                          ? "Not integrated."
                          : config?.configured
                            ? "Configured securely."
                            : config?.base_url || "Not configured yet."
                      }
                      disabled={descriptor.available === false}
                      onClick={() => {
                        if (descriptor.available === false) return;
                        if (descriptor.name.startsWith("custom-")) {
                          setCustomProviderId(descriptor.name);
                          setCustomProviderName(descriptor.title);
                          setCustomProviderDialect("openai-compatible");
                          setCustomProviderBaseUrl(
                            config?.base_url ||
                              descriptor.default_base_url ||
                              "",
                          );
                          setCustomProviderKey("");
                          setCustomProviderModel(
                            descriptor.recommended_model || "",
                          );
                          setCustomProviderStatus("");
                          setCustomProviderEditorOpen(true);
                        } else {
                          setSelectedProvider(descriptor.name);
                        }
                      }}
                      actions={
                        descriptor.available !== false ? (
                          <span className="ml-auto text-faint text-[14px]">
                            ›
                          </span>
                        ) : undefined
                      }
                    />
                  );
                })}
              </div>
            </div>
          ) : (
            (() => {
              const descriptor = providers.find(
                (item) => item.name === selectedProvider,
              );
              if (!descriptor) return null;
              const config = providerConfigs.find(
                (item) => item.provider === descriptor.name,
              );
              const currentUrl =
                config?.base_url || descriptor.default_base_url || "";
              const isCloudflare = descriptor.name === "cloudflare";
              const accountField = descriptor.fields?.find(
                (field) => field.key === "account_id",
              );
              return (
                <div>
                  <button
                    className="text-[12.5px] text-muted hover:text-ink"
                    onClick={() => setSelectedProvider(null)}
                  >
                    ‹ All providers
                  </button>
                  <div className="flex items-center gap-3 mt-3 mb-1">
                    <span className="rounded-lg border border-line grid place-items-center shrink-0 w-9 h-9 bg-paper">
                      <span className="text-[13px] font-semibold text-muted">
                        {descriptor.title.slice(0, 1)}
                      </span>
                    </span>
                    <span className="min-w-0">
                      <span className="block text-[15px] font-semibold leading-tight">
                        {descriptor.title}
                      </span>
                      <span className="block text-[11.5px] text-faint">
                        {config?.configured
                          ? "✓ Configured securely."
                          : "Not configured yet."}
                      </span>
                    </span>
                  </div>
                  <div className="form-grid mt-4">
                    <label>
                      {isCloudflare ? "Base URL (derived)" : "Base URL"}
                      <input
                        type="url"
                        value={currentUrl}
                        readOnly={isCloudflare}
                        onChange={
                          isCloudflare
                            ? undefined
                            : (event) =>
                                setProviderConfigs((items) => {
                                  const found = items.some(
                                    (item) => item.provider === descriptor.name,
                                  );
                                  return found
                                    ? items.map((item) =>
                                        item.provider === descriptor.name
                                          ? {
                                              ...item,
                                              base_url: event.target.value,
                                            }
                                          : item,
                                      )
                                    : [
                                        ...items,
                                        {
                                          provider: descriptor.name,
                                          base_url: event.target.value,
                                          configured: Boolean(
                                            config?.configured,
                                          ),
                                        },
                                      ];
                                })
                        }
                      />
                    </label>
                    {accountField && (
                      <label>
                        {accountField.label}
                        <input
                          value={config?.account_id || ""}
                          onChange={(event) =>
                            setProviderConfigs((items) =>
                              items.map((item) =>
                                item.provider === descriptor.name
                                  ? { ...item, account_id: event.target.value }
                                  : item,
                              ),
                            )
                          }
                        />
                      </label>
                    )}
                    {descriptor.needs_key && (
                      <label>
                        Provider key
                        <input
                          type="password"
                          value={providerKeys[descriptor.name] || ""}
                          placeholder={
                            config?.configured ? "Stored securely" : ""
                          }
                          onChange={(event) =>
                            setProviderKeys((items) => ({
                              ...items,
                              [descriptor.name]: event.target.value,
                            }))
                          }
                        />
                      </label>
                    )}
                    <label>
                      Model
                      <SelectMenu
                        value={
                          providerModels[descriptor.name] ||
                          descriptor.recommended_model ||
                          ""
                        }
                        onChange={(value) =>
                          setProviderModels((items) => ({
                            ...items,
                            [descriptor.name]: value,
                          }))
                        }
                        options={(
                          providerModelOptions[descriptor.name]?.models || []
                        )
                          .filter(
                            (item) =>
                              showAllProviderModels[descriptor.name] ||
                              !item.likely_non_chat,
                          )
                          .map((item) => ({
                            value: item.id,
                            label: `${item.label}${item.capabilities_known ? "" : " (能力未知)"}`,
                          }))}
                      />
                    </label>
                  </div>
                  {providerModelOptions[descriptor.name] && (
                    <div className="flex items-center gap-2 mt-2 text-[11.5px] text-faint">
                      <span>
                        来源：
                        {providerModelOptions[descriptor.name].source === "live"
                          ? "API 实时发现"
                          : `内置回退（${providerModelOptions[descriptor.name].fallback_reason || "未知原因"}）`}
                        ，上次刷新：
                        {providerModelOptions[descriptor.name].discovered_at}
                      </span>
                      {providerModelOptions[descriptor.name].models.some(
                        (item) => item.likely_non_chat,
                      ) && (
                        <Button
                          onClick={() =>
                            setShowAllProviderModels((items) => ({
                              ...items,
                              [descriptor.name]: !items[descriptor.name],
                            }))
                          }
                        >
                          {showAllProviderModels[descriptor.name]
                            ? "收起非对话模型"
                            : "显示全部模型"}
                        </Button>
                      )}
                      <Button
                        onClick={() =>
                          command<ProviderModelsResponse>("provider_models", {
                            provider: descriptor.name,
                            refresh: true,
                          }).then((value) =>
                            setProviderModelOptions((items) => ({
                              ...items,
                              [descriptor.name]: value,
                            })),
                          )
                        }
                      >
                        刷新
                      </Button>
                    </div>
                  )}
                  <div className="flex items-center gap-2 mt-4">
                    <Button
                      className="primary"
                      onClick={() =>
                        command("save_provider_settings", {
                          provider: descriptor.name,
                          baseUrl: currentUrl || null,
                          accountId: config?.account_id || null,
                          model: providerModels[descriptor.name] || null,
                        })
                          .then(() =>
                            providerKeys[descriptor.name]
                              ? command("save_provider_key", {
                                  provider: descriptor.name,
                                  key: providerKeys[descriptor.name],
                                  projectId: null,
                                })
                              : undefined,
                          )
                          .then(() =>
                            command<boolean>("validate_provider_key", {
                              provider: descriptor.name,
                            }),
                          )
                          .then((ok) => {
                            setProviderKeys((items) => ({
                              ...items,
                              [descriptor.name]: "",
                            }));
                            setProviderStatuses((items) => ({
                              ...items,
                              [descriptor.name]: ok
                                ? "Provider key validated successfully."
                                : "Provider key validation failed.",
                            }));
                            setProviderConfigs((items) =>
                              items.map((item) =>
                                item.provider === descriptor.name
                                  ? { ...item, configured: true }
                                  : item,
                              ),
                            );
                          })
                          .catch((error) =>
                            setProviderStatuses((items) => ({
                              ...items,
                              [descriptor.name]: `Provider validation failed: ${errorMessage(error)}`,
                            })),
                          )
                      }
                    >
                      Test / Save
                    </Button>
                    {config?.configured && (
                      <Button
                        onClick={() =>
                          command("delete_provider_key", {
                            provider: descriptor.name,
                          })
                            .then(() =>
                              setProviderConfigs((items) =>
                                items.map((item) =>
                                  item.provider === descriptor.name
                                    ? { ...item, configured: false }
                                    : item,
                                ),
                              ),
                            )
                            .catch(onError)
                        }
                      >
                        Clear key
                      </Button>
                    )}
                  </div>
                  {providerStatuses[descriptor.name] && (
                    <div
                      className={
                        providerStatuses[descriptor.name].includes("failed")
                          ? "failure mt-3"
                          : "success mt-3"
                      }
                    >
                      {providerStatuses[descriptor.name]}
                    </div>
                  )}
                </div>
              );
            })()
          ))}
        {false && tab === "provider" && (
          <div className="divide-y divide-line">
            <div className="settings-row">
              <div>
                <strong>{translate("provider")}</strong>
                <small>
                  {translate("providerDescription")}
                </small>
              </div>
              <SelectMenu
                value={provider}
                onChange={setProvider}
                options={providers
                  .filter((item) => item.available !== false)
                  .map((item) => ({
                    value: item.name,
                    label: item.title,
                  }))}
              />
            </div>
            <div className="settings-row">
              <div>
                <strong>{translate("baseUrl")}</strong>
                <small>
                  {translate("optionalProviderEndpoint")}
                </small>
              </div>
              <input
                type="url"
                value={baseUrl}
                onChange={(event) => setBaseUrl(event.target.value)}
              />
            </div>
            <div className="settings-row">
              <div>
                <strong>{translate("providerKey")}</strong>
                <small>
                  {translate("secureProviderKeyNotice")}
                </small>
              </div>
              <input
                type="password"
                value={key}
                onChange={(event) => setKey(event.target.value)}
              />
            </div>
            <div className="settings-row justify-end">
              <Button
                className="primary"
                onClick={() =>
                  command("save_provider_settings", {
                    provider,
                    baseUrl: baseUrl || null,
                  })
                    .then(() =>
                      command("save_provider_key", {
                        provider,
                        key,
                        projectId: null,
                      }),
                    )
                    .then(() =>
                      command<boolean>("validate_provider_key", { provider }),
                    )
                    .then((ok) => {
                      setKey("");
                      setProviderStatus(
                        ok
                          ? "Provider key validated successfully."
                          : "Provider key validation failed.",
                      );
                    })
                    .catch((error) => {
                      setKey("");
                      setProviderStatus(
                        `Provider validation failed: ${errorMessage(error)}`,
                      );
                    })
                }
              >
                Save and validate
              </Button>
            </div>
            {providerStatus && (
              <div
                className={
                  providerStatus.includes("failed") ? "failure" : "success"
                }
              >
                {providerStatus}
              </div>
            )}
          </div>
        )}
        {tab === "provider" && customProviderEditorOpen && (
          <div
            className="fixed inset-0 z-50 grid place-items-center bg-black/30 p-4"
            onClick={() => setCustomProviderEditorOpen(false)}
          >
            <section
              className="w-full max-w-2xl space-y-3 rounded-xl border border-line bg-panel p-5 shadow-xl"
              onClick={(event) => event.stopPropagation()}
            >
              <div className="flex items-center justify-between">
                <strong>
                  {customProviderId
                    ? "编辑自定义 Provider"
                    : "添加自定义 Provider"}
                </strong>
                <button
                  className="text-muted hover:text-ink"
                  onClick={() => setCustomProviderEditorOpen(false)}
                >
                  ×
                </button>
              </div>
              <div className="form-grid">
                <label>
                  Name
                  <input
                    className="input"
                    value={customProviderName}
                    onChange={(event) =>
                      setCustomProviderName(event.target.value)
                    }
                    placeholder="NextAPI"
                  />
                </label>
                <label>
                  API dialect
                  <select
                    className="input"
                    value={customProviderDialect}
                    onChange={(event) =>
                      setCustomProviderDialect(event.target.value)
                    }
                  >
                    <option value="openai-compatible">
                      {translate("openaiCompatible")}
                    </option>
                    <option value="cloudflare">{translate("cloudflare")}</option>
                  </select>
                </label>
                <label>
                  Base URL
                  <input
                    className="input"
                    type="url"
                    value={customProviderBaseUrl}
                    onChange={(event) =>
                      setCustomProviderBaseUrl(event.target.value)
                    }
                    placeholder="https://api.nextapi.store/v1"
                  />
                </label>
                <label>
                  Model
                  <input
                    className="input"
                    value={customProviderModel}
                    onChange={(event) =>
                      setCustomProviderModel(event.target.value)
                    }
                    placeholder="glm-5.2"
                  />
                </label>
                <label className="sm:col-span-2">
                  Key
                  <input
                    className="input"
                    type="password"
                    value={customProviderKey}
                    onChange={(event) =>
                      setCustomProviderKey(event.target.value)
                    }
                    placeholder={customProviderId ? "Stored securely" : ""}
                  />
                </label>
              </div>
              <div className="flex items-center justify-end gap-2">
                <Button onClick={() => setCustomProviderEditorOpen(false)}>
                  关闭
                </Button>
                <Button
                  className="primary"
                  onClick={() => {
                    setCustomProviderStatus("Validating…");
                    void command<string>("save_custom_provider", {
                      id: customProviderId,
                      name: customProviderName,
                      dialect: customProviderDialect,
                      baseUrl: customProviderBaseUrl,
                      model: customProviderModel,
                    })
                      .then((providerId) =>
                        command<boolean>("validate_provider_key", {
                          provider: providerId,
                          key: customProviderKey || null,
                        }).then((valid) => ({ providerId, valid })),
                      )
                      .then(({ providerId, valid }) => {
                        if (!valid) {
                          throw new Error("Provider key validation failed.");
                        }
                        return command("save_provider_settings", {
                          provider: providerId,
                          baseUrl: customProviderBaseUrl,
                          accountId: null,
                          model: customProviderModel,
                        })
                          .then(() =>
                            customProviderKey
                              ? command("save_provider_key", {
                                  provider: providerId,
                                  key: customProviderKey,
                                  projectId: null,
                                })
                              : undefined,
                          )
                          .then(() => onRefresh());
                      })
                      .then(() => {
                        setCustomProviderKey("");
                        setCustomProviderStatus(
                          "Provider key validated successfully.",
                        );
                        setCustomProviderEditorOpen(false);
                      })
                      .catch((error) =>
                        setCustomProviderStatus(errorMessage(error)),
                      );
                  }}
                >
                  验证并保存
                </Button>
              </div>
              {customProviderStatus && (
                <div
                  className={
                    customProviderStatus.includes("failed") ||
                    customProviderStatus.includes("Failed")
                      ? "failure"
                      : "text-muted text-sm"
                  }
                >
                  {customProviderStatus}
                </div>
              )}
            </section>
          </div>
        )}
        {tab === "hosts" && (
          <CollectionPage
            search=""
            onSearch={() => undefined}
            searchPlaceholder={translate("searchHosts")}
            primary={
              <Button
                className="primary"
                onClick={() => {
                  setEditingHostId(null);
                  setHostName("");
                  setHostUrl("");
                  setHostToken("");
                  setHostFormOpen(true);
                }}
              >
                Add host
              </Button>
            }
            bare
            rows={
              hosts.length ? (
                <div className="grid grid-cols-[repeat(auto-fill,minmax(min(100%,440px),440px))] gap-2.5">
                  {hosts.map((host) => (
                    <IntegrationCard
                      key={host.id}
                      icon={host.name.slice(0, 1).toUpperCase()}
                      title={host.name}
                      badge={{
                        label: hostStatusLabel(host),
                        tone: host.online === true ? "success" : "neutral",
                      }}
                      description={
                        host.online === false ? (
                          <span className="status-offline">
                            {translate("offline")}
                          </span>
                        ) : undefined
                      }
                      actions={
                        <>
                          {!host.builtin && (
                            <Button
                              onClick={() => {
                                void onEditHost(host)
                                  .then(() => setHostFormOpen(true))
                                  .catch(onError);
                              }}
                            >
                              Edit
                            </Button>
                          )}
                          <Button
                            disabled={host.builtin || testingHostId === host.id}
                            onClick={() => {
                              setTestingHostId(host.id);
                              void onTestHost(host.id)
                                .catch(onError)
                                .finally(() => setTestingHostId(null));
                            }}
                          >
                            {testingHostId === host.id ? "Testing…" : "Test"}
                          </Button>
                          <Button
                            className="danger"
                            disabled={host.builtin}
                            onClick={() => {
                              if (confirmDeleteHostId === host.id) {
                                void onDeleteHost(host.id)
                                  .then(() => setConfirmDeleteHostId(null))
                                  .catch(onError);
                              } else {
                                setConfirmDeleteHostId(host.id);
                              }
                            }}
                          >
                            {confirmDeleteHostId === host.id
                              ? "Confirm delete"
                              : "Delete"}
                          </Button>
                        </>
                      }
                    />
                  ))}
                </div>
              ) : null
            }
            empty={translate("noHosts")}
            form={
              hostFormOpen ? (
                <form
                  className="manage-card form-grid"
                  onSubmit={(event) => {
                    onAddHost(event);
                    setHostFormOpen(false);
                  }}
                >
                  <input
                    value={hostName}
                    onChange={(event) => setHostName(event.target.value)}
                    placeholder={translate("Host name")}
                    required
                  />
                  <input
                    value={hostUrl}
                    onChange={(event) => setHostUrl(event.target.value)}
                    placeholder={translate("Remote URL")}
                    type="url"
                    required
                  />
                  <input
                    value={hostToken}
                    onChange={(event) => setHostToken(event.target.value)}
                    placeholder={
                      editingHostId
                        ? "留空保持原 token"
                        : translate("Bearer token")
                    }
                    type="password"
                    required={!editingHostId}
                  />
                  <input
                    value={vncPassword}
                    onChange={(event) => setVncPassword(event.target.value)}
                      placeholder={translate("optionalVncPassword")}
                    type="password"
                  />
                  <Button type="submit" className="primary">
                    {editingHostId ? "Save" : "Add host"}
                  </Button>
                  <Button
                    type="button"
                    onClick={() => {
                      setEditingHostId(null);
                      setHostName("");
                      setHostUrl("");
                      setHostToken("");
                      setVncPassword("");
                      setHostFormOpen(false);
                    }}
                  >
                    Cancel
                  </Button>
                </form>
              ) : undefined
            }
          />
        )}
        {tab === "instructions" && (
          <div>
            {(() => {
              const instructions = assets.find(
                (asset) => asset.kind === "instructions",
              );
              const saveInstructions = () =>
                command("save_asset", {
                  id: instructions?.id || "global-instructions",
                  kind: "instructions",
                  title: instructions?.title || "全局指令",
                  body: instructionsDraft,
                  trigger: null,
                  scope: null,
                  scopeKind: "global",
                  enabled: true,
                })
                  .then(() => {
                    setInstructionsDraft("");
                    onRefresh();
                  })
                  .catch(onError);
              return (
                <>
                  <div className="rounded-xl2 border border-line bg-panel p-5">
                    <h2 className="text-[15px] font-semibold text-ink">
                      全局指令
                    </h2>
                    <p className="text-[13px] text-muted mt-1">
                      这里的内容会追加到所有会话的系统指令中。
                    </p>
                    <textarea
                      className="mt-4 min-h-[260px] w-full"
                      value={instructionsDraft}
                      onChange={(event) =>
                        setInstructionsDraft(event.target.value)
                      }
                      placeholder={translate("globalInstructionsPlaceholder")}
                    />
                    <div className="inline-actions">
                      <Button className="primary" onClick={saveInstructions}>
                        保存指令
                      </Button>
                      {instructions && (
                        <Button
                          className="bordered"
                          onClick={() => {
                            setVersionHistoryAsset(instructions.id);
                            setCompareVersionId(null);
                            void command<Array<Record<string, unknown>>>(
                              "list_asset_versions",
                              { assetId: instructions.id },
                            )
                              .then(setAssetVersions)
                              .catch(onError);
                          }}
                        >
                          版本历史
                        </Button>
                      )}
                    </div>
                  </div>
                  {versionHistoryAsset && (
                    <div className="manage-card mt-4">
                      <div className="flex items-center justify-between">
                        <strong>{translate("versionHistory")}</strong>
                        <Button
                          className="bordered"
                          onClick={() => {
                            setVersionHistoryAsset(null);
                            setAssetVersions([]);
                            setCompareVersionId(null);
                          }}
                        >
                          关闭
                        </Button>
                      </div>
                      {assetVersions.map((version) => {
                        const versionId = String(version.id);
                        const isCurrent =
                          instructions?.body === version.content;
                        return (
                          <div className="manage-row mt-2" key={versionId}>
                            <span>
                              <strong>
                                v{String(version.version)}
                                {isCurrent ? " · 当前" : ""}
                              </strong>
                              <small>{String(version.created_at)}</small>
                            </span>
                            <Button
                              className="bordered"
                              onClick={() =>
                                command("rollback_asset", {
                                  assetId: instructions!.id,
                                  versionId,
                                })
                                  .then(onRefresh)
                                  .catch(onError)
                              }
                            >
                              回滚
                            </Button>
                          </div>
                        );
                      })}
                    </div>
                  )}
                </>
              );
            })()}
          </div>
        )}
        {(tab === "experts" || tab === "teams" || tab === "command") && (
          <div className="space-y-4">
            {tab === "experts" && (
              <div className="flex flex-wrap items-center gap-2 rounded-lg border border-line p-3">
                <select
                  className="input"
                  value={libraryProjectId}
                  onChange={(event) => setLibraryProjectId(event.target.value)}
                >
                  <option value="">{translate("syncRepositoryProject")}</option>
                  {projects.map((project) => (
                    <option key={project.id} value={project.id}>
                      {project.name}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className="btn"
                  disabled={!libraryProjectId}
                  onClick={() =>
                    void command("import_repository_templates", {
                      projectId: libraryProjectId,
                    })
                      .then(() => setTemplateDraftStatus("已从仓库导入专家"))
                      .then(() =>
                        command<LibraryEntry[]>("list_configured_library").then(
                          setLibraryEntries,
                        ),
                      )
                      .catch(onError)
                  }
                >
                  从仓库导入专家
                </button>
              </div>
            )}
            {tab === "teams" && (
              <div className="flex flex-wrap items-center gap-2 rounded-lg border border-line p-3">
                <select
                  className="input"
                  value={libraryProjectId}
                  onChange={(event) => setLibraryProjectId(event.target.value)}
                >
                  <option value="">{translate("importExportTeamProject")}</option>
                  {projects.map((project) => (
                    <option key={project.id} value={project.id}>
                      {project.name}
                    </option>
                  ))}
                </select>
                <button
                  type="button"
                  className="btn"
                  disabled={!libraryProjectId}
                  onClick={() =>
                    void command("save_project_as_team_template", {
                      projectId: libraryProjectId,
                    })
                      .then(() =>
                        setTemplateDraftStatus("当前项目已另存为 Team 模板"),
                      )
                      .then(() =>
                        command<LibraryEntry[]>("list_configured_library").then(
                          setLibraryEntries,
                        ),
                      )
                      .catch(onError)
                  }
                >
                  当前项目另存为 Team
                </button>
              </div>
            )}
            <div className="flex justify-end">
              <button
                type="button"
                className="btn approval-primary"
                onClick={() => {
                  setTemplateDraftId(null);
                  setTemplateDraftName("");
                  setTemplateDraftDescription("");
                  setTemplateDraftContent("{}");
                  setTemplateDraftStatus("");
                  setTemplateEditorOpen(true);
                }}
              >
                添加
              </button>
            </div>
            {templateEditorOpen && (
              <div
                className="fixed inset-0 z-50 grid place-items-center bg-black/30 p-4"
                onClick={() => setTemplateEditorOpen(false)}
              >
                <section
                  className="w-full max-w-2xl space-y-3 rounded-xl border border-line bg-panel p-5 shadow-xl"
                  onClick={(event) => event.stopPropagation()}
                >
                  <div className="flex items-center justify-between">
                    <strong>
                      {templateDraftId
                        ? translate("editTemplate")
                        : translate("addTemplate")}
                    </strong>
                    <button
                      type="button"
                      className="btn"
                      onClick={() => setTemplateEditorOpen(false)}
                    >
                      关闭
                    </button>
                  </div>
                  <div className="grid gap-2 md:grid-cols-2">
                    <input
                      className="input"
                      value={templateDraftName}
                      onChange={(event) =>
                        setTemplateDraftName(event.target.value)
                      }
                      placeholder={translate("templateName")}
                    />
                    <input
                      className="input"
                      value={templateDraftDescription}
                      onChange={(event) =>
                        setTemplateDraftDescription(event.target.value)
                      }
                      placeholder={translate("description")}
                    />
                  </div>
                  <textarea
                    className="input min-h-28 font-mono text-xs"
                    value={templateDraftContent}
                    onChange={(event) =>
                      setTemplateDraftContent(event.target.value)
                    }
                    placeholder={
                      activeLibraryKind === "agent-template"
                        ? '{"role":"Code","model":"auto"}'
                        : activeLibraryKind === "team-template"
                          ? '{"workflow":{"workflow":[]},"agents":[]}'
                          : '{"name":"/review","body":"Review the current changes."}'
                    }
                  />
                  <div className="flex items-center justify-between">
                    <small className="text-muted">{templateDraftStatus}</small>
                    <button
                      type="button"
                      className="btn approval-primary"
                      disabled={!templateDraftName.trim()}
                      onClick={() => {
                        void command("save_template", {
                          id: templateDraftId,
                          kind: activeLibraryKind,
                          name: templateDraftName.trim(),
                          description: templateDraftDescription.trim(),
                          content: templateDraftContent,
                        })
                          .then(() => {
                            setTemplateDraftStatus("已保存");
                            setTemplateDraftName("");
                            setTemplateDraftId(null);
                            setTemplateEditorOpen(false);
                            return command<LibraryEntry[]>(
                              "list_configured_library",
                            );
                          })
                          .then(setLibraryEntries)
                          .catch(onError);
                      }}
                    >
                      保存
                    </button>
                  </div>
                </section>
              </div>
            )}
            <div className="grid gap-3 xl:grid-cols-2">
              {libraryEntries
                .filter((template) => template.kind === activeLibraryKind)
                .map((template) => (
                  <article
                    className="min-w-0 overflow-hidden rounded-lg border border-line p-4"
                    key={template.id}
                  >
                    <div className="flex items-start justify-between gap-2">
                      <div className="min-w-0">
                        <strong className="block break-words">
                          {template.name}
                        </strong>
                        <small className="block text-muted">
                          {template.source ||
                            (template.status === "builtin"
                              ? "内置"
                              : "自定义")}{" "}
                          · v{template.version}
                        </small>
                      </div>
                      <span className="max-w-[45%] break-words text-right text-xs text-muted">
                        {template.kind}
                      </span>
                    </div>
                    <p className="mt-2 break-words text-sm text-muted">
                      {template.description || "无描述"}
                    </p>
                    <pre className="mt-2 max-h-28 max-w-full overflow-auto whitespace-pre-wrap break-words text-xs text-muted">
                      {template.content}
                    </pre>
                    {template.readonly && (
                      <div className="mt-2 flex items-center justify-between">
                        <small className="text-muted">
                          {translate("builtInTemplateReadonly")}
                        </small>
                        <button
                          type="button"
                          className="btn"
                          onClick={() => {
                            setTemplateDraftName(`${template.name} 副本`);
                            setTemplateDraftId(null);
                            setTemplateDraftDescription(template.description);
                            setTemplateDraftContent(template.content);
                            setTemplateEditorOpen(true);
                          }}
                        >
                          另存为
                        </button>
                      </div>
                    )}
                    {!template.readonly &&
                      libraryProjectId &&
                      ["agent-template", "team-template"].includes(
                        template.kind,
                      ) && (
                        <button
                          type="button"
                          className="btn mt-2"
                          onClick={() =>
                            void command("export_template_to_repository", {
                              templateId: template.id,
                              projectId: libraryProjectId,
                            })
                              .then(() =>
                                setTemplateDraftStatus("已导出到仓库"),
                              )
                              .catch((reason) => {
                                const message = errorMessage(reason);
                                if (
                                  message.includes("confirm overwrite") &&
                                  window.confirm(
                                    `目标文件已有不同内容，确定覆盖吗？\n${message}`,
                                  )
                                ) {
                                  return command(
                                    "export_template_to_repository",
                                    {
                                      templateId: template.id,
                                      projectId: libraryProjectId,
                                      overwrite: true,
                                    },
                                  ).then(() =>
                                    setTemplateDraftStatus("已覆盖导出到仓库"),
                                  );
                                }
                                onError(reason);
                              })
                          }
                        >
                          导出到仓库
                        </button>
                      )}
                    {!template.readonly && (
                      <div className="mt-2 flex gap-2">
                        <button
                          type="button"
                          className="btn"
                          onClick={() => {
                            setLibraryKind(
                              template.kind as
                                "agent-template" | "team-template" | "command",
                            );
                            setTemplateDraftName(template.name);
                            setTemplateDraftId(template.id);
                            setTemplateDraftDescription(
                              template.description || "",
                            );
                            setTemplateDraftContent(template.content);
                            setTemplateEditorOpen(true);
                          }}
                        >
                          编辑
                        </button>
                        <button
                          type="button"
                          className="btn danger"
                          onClick={() => {
                            if (!window.confirm(`删除「${template.name}」？`))
                              return;
                            void command("delete_template", {
                              id: template.id,
                            })
                              .then(() =>
                                command<LibraryEntry[]>(
                                  "list_configured_library",
                                ),
                              )
                              .then(setLibraryEntries)
                              .catch(onError);
                          }}
                        >
                          删除
                        </button>
                      </div>
                    )}
                  </article>
                ))}
            </div>
            {!libraryEntries.some(
              (template) => template.kind === activeLibraryKind,
            ) && (
              <div className="py-8 text-sm text-muted">
                {translate("noTemplates")}
              </div>
            )}
          </div>
        )}
        {tab === "skill" && (
          <div className="space-y-6">
            <label className="settings-row">
              <div>
                <strong>{translate("skillsRulesScope")}</strong>
                <small>{translate("skillsRulesScopeDescription")}</small>
              </div>
              <SelectMenu
                value={settingsProjectId || ""}
                onChange={(value) => setSettingsProjectId(value || null)}
                options={[
                  { value: "", label: "全局" },
                  ...projects.map((project) => ({
                    value: project.id,
                    label: project.name,
                  })),
                ]}
              />
            </label>
            <section className="rounded-lg border border-line p-4">
              <div className="flex items-center justify-between mb-3">
                <div>
                  <strong>{translate("skillUsage")}</strong>
                  <small className="block">
                    数据来自技能实际注入会话上下文时的埋点；同一会话同一技能只计一次。
                  </small>
                </div>
              </div>
              {!skillUsage?.skills.length ? (
                <div className="text-sm text-muted py-6">
                  {translate("noSkillUsage")}
                </div>
              ) : (
                <>
                  <div className="grid grid-cols-3 gap-2 mb-4">
                    <div className="rounded-md bg-panel p-3">
                      <small>{translate("activationCount")}</small>
                      <strong className="block text-lg">
                        {skillUsage.skills.reduce(
                          (sum, item) => sum + item.calls,
                          0,
                        )}
                      </strong>
                    </div>
                    <div className="rounded-md bg-panel p-3">
                      <small>{translate("involvedSkills")}</small>
                      <strong className="block text-lg">
                        {skillUsage.skills.length}
                      </strong>
                    </div>
                    <div className="rounded-md bg-panel p-3">
                      <small>{translate("timeRange")}</small>
                      <strong className="block text-lg">
                        {skillUsage.timeline.length
                          ? `${skillUsage.timeline[0].date} – ${skillUsage.timeline.at(-1)?.date}`
                          : "—"}
                      </strong>
                    </div>
                  </div>
                  <div className="space-y-2">
                    {skillUsage.skills.map((item) => (
                      <div
                        className="flex items-center gap-3 rounded-md border border-line p-3"
                        key={`${item.source}:${item.path}`}
                      >
                        <code className="flex-1">{item.name}</code>
                        <span className="text-xs text-muted">
                          {item.calls} 次 · {item.sessions} 个会话 · 最近启用{" "}
                          {item.last_used}
                        </span>
                      </div>
                    ))}
                  </div>
                  {skillUsage.timeline.length > 0 && (
                    <div className="mt-4 text-xs text-muted">
                      随时间变化（启用）：
                      {skillUsage.timeline
                        .map((item) => ` ${item.date} (${item.calls})`)
                        .join(" · ")}
                    </div>
                  )}
                </>
              )}
            </section>
            <section className="rounded-lg border border-line p-4">
              <div className="flex items-center justify-between mb-3">
                <div>
                  <strong>{translate("browse")}</strong>
                  <small className="block">
                    浏览仓库发现的 .agents/skills 与规则文件；Skill 不在此创建。
                  </small>
                </div>
              </div>
              {!selected ? (
                <div className="text-sm text-muted py-6">
                  请先打开一个项目会话以浏览仓库技能和规则
                </div>
              ) : !skillBrowse ? (
                <div className="text-sm text-muted py-6">
                  {translate("readingRepositoryAssets")}
                </div>
              ) : (
                <div className="space-y-4">
                  <div>
                    <h4 className="font-medium mb-2">
                      {translate("skillsRepositorySource")}
                    </h4>
                    {!skillBrowse.skills.length ? (
                      <div className="text-sm text-muted">
                        {translate("noRepositorySkills")}
                      </div>
                    ) : (
                      skillBrowse.skills.map((item) => (
                        <details
                          className="border-b border-line py-2"
                          key={item.path}
                        >
                          <summary className="cursor-pointer">
                            {item.name}{" "}
                            <span className="text-xs text-muted">
                              {item.path}
                            </span>
                          </summary>
                          <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap text-xs">
                            {item.content}
                          </pre>
                        </details>
                      ))
                    )}
                  </div>
                  <div>
                    <h4 className="font-medium mb-2">
                      {translate("rulesRepositorySource")}
                    </h4>
                    {!skillBrowse.rules.length ? (
                      <div className="text-sm text-muted">
                        {translate("noRepositoryRules")}
                      </div>
                    ) : (
                      skillBrowse.rules.map((item) => (
                        <details
                          className="border-b border-line py-2"
                          key={item.path}
                        >
                          <summary className="cursor-pointer">
                            {item.path}
                          </summary>
                          <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap text-xs">
                            {item.content}
                          </pre>
                        </details>
                      ))
                    )}
                  </div>
                </div>
              )}
            </section>
          </div>
        )}
        {assetTabVisible && (
          <div>
            {(
              [
                [
                  "agents",
                  "规则",
                  "仓库级运行规则（对应仓库中的 AGENTS.md 文件）。",
                ],
                ["instructions", "全局指令", "应用于所有新会话的全局指令。"],
                [
                  "knowledge",
                  "Knowledge",
                  "Reusable reference material added to the knowledge context.",
                ],
                [
                  "playbook",
                  "Playbook",
                  "A repeatable workflow that can be run by an automation.",
                ],
                [
                  "skill",
                  "Skill",
                  "A focused capability or instruction bundle available to the agent.",
                ],
              ] as const
            )
              .filter(
                ([kind]) => kind === assetTabKind && kind !== "instructions",
              )
              .map(([kind, label, description]) => (
                <CollectionPage
                  key={kind}
                  search={assetSearch}
                  onSearch={setAssetSearch}
                  searchPlaceholder={
                    kind === "agents" ? "搜索规则" : `Search ${label}`
                  }
                  actions={
                    tab === "knowledge" || tab === "playbook" ? (
                      <div className="inline-actions">
                        {tab === "knowledge" && (
                          <>
                            <Button
                              className="bordered"
                              disabled={remoteAssetAction !== null || !selected}
                              onClick={() =>
                                (() => {
                                  setRemoteAssetAction("discovering");
                                  return command("discover_remote_assets", {
                                    sessionId: selected!.id,
                                  })
                                    .then((bundle) => {
                                      setDiscoveredAssets(bundle as Asset[]);
                                      return onRefresh();
                                    })
                                    .catch(onError)
                                    .finally(() => setRemoteAssetAction(null));
                                })()
                              }
                            >
                              {remoteAssetAction === "discovering"
                                ? "Discovering…"
                                : "Discover remote"}
                            </Button>
                            <Button
                              className="bordered"
                              disabled={remoteAssetAction !== null || !selected}
                              onClick={() =>
                                (() => {
                                  setRemoteAssetAction("importing");
                                  return command("import_assets", {
                                    sessionId: selected!.id,
                                  })
                                    .then((bundle) => {
                                      setDiscoveredAssets(bundle as Asset[]);
                                      return onRefresh();
                                    })
                                    .catch(onError)
                                    .finally(() => setRemoteAssetAction(null));
                                })()
                              }
                            >
                              {remoteAssetAction === "importing"
                                ? "Importing…"
                                : "Import"}
                            </Button>
                          </>
                        )}
                        <Button
                          className="bordered"
                          disabled={remoteAssetAction !== null || !selected}
                          onClick={() =>
                            (() => {
                              setRemoteAssetAction("exporting");
                              return command("export_assets", {
                                sessionId: selected!.id,
                                ids: assets.map((asset) => asset.id),
                              })
                                .then(() => setDiscoveredAssets([]))
                                .catch(onError)
                                .finally(() => setRemoteAssetAction(null));
                            })()
                          }
                        >
                          {remoteAssetAction === "exporting"
                            ? "Exporting…"
                            : "Export"}
                        </Button>
                      </div>
                    ) : undefined
                  }
                  columns={
                    kind === "agents"
                      ? ["标题", "触发条件", "范围", "状态"]
                      : ["Title", "Trigger", "Scope", "Status"]
                  }
                  renderCard={() => (
                    <>
                      {assets
                        .filter(
                          (asset) =>
                            asset.kind === kind &&
                            asset.title
                              .toLowerCase()
                              .includes(assetSearch.toLowerCase()) &&
                            (assetStatus === "All" ||
                              (assetStatus === "Enabled" && asset.enabled) ||
                              (assetStatus === "Disabled" && !asset.enabled)),
                        )
                        .map((asset) => (
                          <div
                            className="rounded-xl2 border border-line bg-panel p-4"
                            key={asset.id}
                          >
                            <div className="flex justify-between gap-2">
                              <strong>{asset.title}</strong>
                              <span className="text-[11px] text-muted">
                                {kind === "agents"
                                  ? asset.enabled
                                    ? "已启用"
                                    : "已禁用"
                                  : asset.enabled
                                    ? "Enabled"
                                    : "Disabled"}
                              </span>
                            </div>
                            <p className="mt-2 text-[13px] text-muted line-clamp-2">
                              {asset.body}
                            </p>
                            <small className="mt-3 block text-muted">
                              {asset.trigger ||
                                (kind === "agents"
                                  ? "未设置触发条件"
                                  : "No trigger")}
                            </small>
                          </div>
                        ))}
                    </>
                  )}
                  chips={["All", "Enabled", "Disabled"]}
                  activeChip={assetStatus}
                  onChip={setAssetStatus}
                  primary={
                    <Button
                      className="primary"
                      onClick={() => {
                        setEditingAssetId(null);
                        setAssetFormOpen(true);
                      }}
                    >
                      {kind === "agents" ? "新建规则" : `New ${label}`}
                    </Button>
                  }
                  rows={
                    <>
                      {assets
                        .filter(
                          (asset) =>
                            asset.kind === kind &&
                            (assetStatus === "All" ||
                              (assetStatus === "Enabled" && asset.enabled) ||
                              (assetStatus === "Disabled" && !asset.enabled)) &&
                            asset.title
                              .toLowerCase()
                              .includes(assetSearch.toLowerCase()),
                        )
                        .map((asset) => (
                          <div className="manage-row px-4" key={asset.id}>
                            <span>
                              <strong>{asset.title}</strong>
                              <small>
                                {kind === "agents"
                                  ? asset.enabled
                                    ? "已启用"
                                    : "已禁用"
                                  : asset.enabled
                                    ? "Enabled"
                                    : "Disabled"}
                                {asset.trigger ? ` · ${asset.trigger}` : ""}
                              </small>
                            </span>
                            <span className="inline-actions">
                              <Button
                                className="bordered"
                                disabled={assetPending === asset.id}
                                onClick={() => {
                                  if (!selected) {
                                    onError(
                                      "Select a session before changing asset access.",
                                    );
                                    return;
                                  }
                                  setAssetPending(asset.id);
                                  void command("set_asset_enabled", {
                                    sessionId: selected.id,
                                    assetId: asset.id,
                                    enabled: !asset.enabled,
                                  })
                                    .then(() => onRefresh())
                                    .catch(onError)
                                    .finally(() => setAssetPending(null));
                                }}
                              >
                                {assetPending === asset.id
                                  ? kind === "agents"
                                    ? "保存中…"
                                    : "Saving…"
                                  : asset.enabled
                                    ? kind === "agents"
                                      ? "停用"
                                      : "Disable"
                                    : kind === "agents"
                                      ? "启用"
                                      : "Enable"}
                              </Button>
                              <Button
                                className="bordered"
                                onClick={() => {
                                  setVersionHistoryAsset(asset.id);
                                  setCompareVersionId(null);
                                  void command<Array<Record<string, unknown>>>(
                                    "list_asset_versions",
                                    {
                                      assetId: asset.id,
                                    },
                                  )
                                    .then(setAssetVersions)
                                    .catch(onError);
                                }}
                              >
                                History
                              </Button>
                              <Button
                                className="bordered"
                                onClick={() => {
                                  setEditingAssetId(asset.id);
                                  setAssetFormOpen(true);
                                  setAssetKind(asset.kind);
                                  setAssetTitle(asset.title);
                                  setAssetBody(asset.body);
                                  setAssetTrigger(asset.trigger);
                                  setAssetScope(asset.scope);
                                  setAssetScopeKind(
                                    asset.scope_kind === "repo"
                                      ? "repo"
                                      : "global",
                                  );
                                }}
                              >
                                {kind === "agents" ? "编辑" : "Edit"}
                              </Button>
                              <Button
                                className="danger"
                                disabled={assetPending === asset.id}
                                onClick={() => {
                                  setAssetPending(asset.id);
                                  void command("delete_asset", {
                                    id: asset.id,
                                  })
                                    .then(onRefresh)
                                    .catch(onError)
                                    .finally(() => setAssetPending(null));
                                }}
                              >
                                {kind === "agents" ? "删除" : "Delete"}
                              </Button>
                            </span>
                          </div>
                        ))}
                      {!assets.some((asset) => asset.kind === kind) && (
                        <p className="px-4 py-6 text-[13px] text-muted">
                          {kind === "agents"
                            ? "暂无规则。"
                            : `No ${label} assets yet.`}
                        </p>
                      )}
                    </>
                  }
                  empty={
                    kind === "agents" ? "暂无规则。" : `No ${label} assets yet.`
                  }
                />
              ))}
            {versionHistoryAsset && (
              <div className="manage-card mt-4">
                <div className="flex items-center justify-between">
                  <strong>{translate("versionHistory")}</strong>
                  <Button
                    className="bordered"
                    onClick={() => {
                      setVersionHistoryAsset(null);
                      setAssetVersions([]);
                      setCompareVersionId(null);
                    }}
                  >
                    Close
                  </Button>
                </div>
                {assetVersions.map((version) => {
                  const versionId = String(version.id);
                  const isCurrent =
                    assets.find((asset) => asset.id === versionHistoryAsset)
                      ?.body === version.content;
                  return (
                    <div className="manage-row mt-2" key={versionId}>
                      <span>
                        <strong>
                          v{String(version.version)}
                          {isCurrent ? " · current" : ""}
                        </strong>
                        <small>{String(version.created_at)}</small>
                      </span>
                      <span className="inline-actions">
                        <Button
                          className="bordered"
                          onClick={() => {
                            setCompareVersionId(versionId);
                            const other = assetVersions.find(
                              (item) => String(item.id) !== versionId,
                            );
                            if (other) {
                              void command("compare_asset_versions", {
                                assetId: versionHistoryAsset,
                                leftVersionId: versionId,
                                rightVersionId: String(other.id),
                              })
                                .then((value) => {
                                  setCompareVersionId(
                                    JSON.stringify(value, null, 2),
                                  );
                                })
                                .catch(onError);
                            }
                          }}
                        >
                          Compare
                        </Button>
                        <Button
                          className="bordered"
                          onClick={() =>
                            command("rollback_asset", {
                              assetId: versionHistoryAsset,
                              versionId,
                            })
                              .then(onRefresh)
                              .catch(onError)
                          }
                        >
                          Roll back
                        </Button>
                      </span>
                    </div>
                  );
                })}
                {compareVersionId?.startsWith("{") && (
                  <pre className="code-block mt-3">{compareVersionId}</pre>
                )}
              </div>
            )}
            {assetFormOpen && (
              <div className="rounded-xl2 border border-line bg-panel p-5">
                <h2 className="text-[15px] font-semibold text-ink">
                  {assetTabKind === "agents"
                    ? editingAssetId
                      ? "编辑规则"
                      : "新建规则"
                    : assetTabKind === "instructions"
                      ? editingAssetId
                        ? "编辑全局指令"
                        : "新建全局指令"
                      : editingAssetId
                        ? "Edit asset"
                        : "New asset"}
                </h2>
                <div className="form-grid mt-4">
                  <label className="field-label">
                    {assetTabKind === "agents" ? "标题" : "Title"}
                    <input
                      value={assetTitle}
                      onChange={(event) => setAssetTitle(event.target.value)}
                      placeholder={translate("Asset title")}
                    />
                  </label>
                  <label className="field-label">
                    {assetTabKind === "agents" ? "内容" : "Body"}
                    <textarea
                      value={assetBody}
                      onChange={(event) => setAssetBody(event.target.value)}
                      placeholder={translate("Asset content")}
                    />
                  </label>
                  {(assetTabKind === "knowledge" ||
                    assetTabKind === "skill") && (
                    <label className="field-label">
                      Trigger
                      <input
                        value={assetTrigger}
                        onChange={(event) =>
                          setAssetTrigger(event.target.value)
                        }
                        placeholder={translate("Optional trigger")}
                      />
                    </label>
                  )}
                  <label className="field-label">
                    {assetTabKind === "agents" ||
                    assetTabKind === "instructions"
                      ? "适用范围"
                      : "Scope"}
                    <select
                      value={assetScopeKind}
                      onChange={(event) =>
                        setAssetScopeKind(
                          event.target.value === "repo" ? "repo" : "global",
                        )
                      }
                    >
                    <option value="global">{translate("global")}</option>
                    <option value="repo">{translate("repository")}</option>
                    </select>
                    <input
                      value={assetScope}
                      onChange={(event) => setAssetScope(event.target.value)}
                      placeholder={translate("Workspace path (absolute)")}
                      disabled={
                        assetScopeKind === "global" ||
                        assetTabKind === "instructions"
                      }
                    />
                  </label>
                  <Button
                    className="primary"
                    onClick={() =>
                      command("save_asset", {
                        id: editingAssetId || `asset-${Date.now()}`,
                        kind: assetTabKind,
                        title: assetTitle,
                        body: assetBody,
                        trigger: assetTrigger || null,
                        scope: assetScope || null,
                        scopeKind: assetScopeKind,
                        enabled: true,
                      })
                        .then(() => {
                          setAssetTitle("");
                          setAssetBody("");
                          setAssetTrigger("");
                          setAssetScope("");
                          setEditingAssetId(null);
                          setAssetFormOpen(false);
                          onRefresh();
                        })
                        .catch(onError)
                    }
                  >
                    {assetTabKind === "agents"
                      ? editingAssetId
                        ? "保存更改"
                        : "创建规则"
                      : assetTabKind === "instructions"
                        ? editingAssetId
                          ? "保存更改"
                          : "保存全局指令"
                        : editingAssetId
                          ? "Save changes"
                          : "Create asset"}
                  </Button>
                </div>
              </div>
            )}
          </div>
        )}
        {tab === "mcp" && (
          <McpManage
            selected={selected}
            onError={onError}
            onPromptDraft={onPromptDraft}
          />
        )}
        {tab === "ingress" && (
          <div className="space-y-5">
            <div className="rounded-xl2 border border-line bg-panel p-5">
              <h2 className="text-[15px] font-semibold text-ink">
                External event sources
              </h2>
              <p className="muted mt-1">
                Sources are disabled when created. Polling never exposes a
                public listener.
              </p>
              <p className="muted mt-1">
                GitHub repository events are best-effort polling: GitHub
                documents up to 300 events from the last 30 days and may delay
                delivery. Pull requests, comments, and issues are mapped.
                Check-run state is not exposed by this Events API source; use
                the later webhook/Checks integration for CI state triggers.
              </p>
              <div className="form-grid mt-4">
                <label className="field-label">
                  Provider
                  <select
                    value={ingressProvider}
                    onChange={(event) =>
                      setIngressProvider(event.target.value as "github" | "rss")
                    }
                  >
                    <option value="github">
                      {translate("githubRepositoryEvents")}
                    </option>
                    <option value="rss">{translate("rssAtomFeed")}</option>
                  </select>
                </label>
                <label className="field-label">
                  {ingressProvider === "github" ? "Repository" : "Feed URL"}
                  <input
                    value={ingressTarget}
                    onChange={(event) => setIngressTarget(event.target.value)}
                    placeholder={
                      ingressProvider === "github"
                        ? "owner/repository"
                        : "https://example.test/feed.xml"
                    }
                  />
                </label>
                <label className="field-label">
                  Poll interval
                  <select
                    value={ingressInterval}
                    onChange={(event) => setIngressInterval(event.target.value)}
                  >
                    <option value="30">{translate("seconds30")}</option>
                    <option value="60">{translate("minute1")}</option>
                    <option value="300">{translate("minutes5")}</option>
                    <option value="900">15 minutes</option>
                  </select>
                </label>
                <div className="flex items-end gap-2">
                  <Button
                    className="primary"
                    disabled={!ingressTarget.trim()}
                    onClick={() => {
                      const target = ingressTarget.trim();
                      const provider = ingressProvider;
                      const sourceId =
                        editingIngressId ||
                        `${provider}:${target.replace(/[^a-zA-Z0-9._/-]+/g, "-")}`;
                      const config =
                        provider === "github"
                          ? {
                              repo: target,
                              poll_interval_seconds: Number(ingressInterval),
                            }
                          : {
                              url: target,
                              poll_interval_seconds: Number(ingressInterval),
                            };
                      void command("save_external_ingress_source", {
                        sourceId,
                        provider,
                        config,
                      })
                        .then(loadIngress)
                        .then(() => {
                          setIngressTarget("");
                          setEditingIngressId(null);
                          setIngressStatus("Source saved disabled by default.");
                        })
                        .catch(onError);
                    }}
                  >
                    {editingIngressId ? "Save changes" : "Add source"}
                  </Button>
                  {editingIngressId && (
                    <Button
                      className="bordered"
                      onClick={() => {
                        setEditingIngressId(null);
                        setIngressTarget("");
                      }}
                    >
                      Cancel
                    </Button>
                  )}
                </div>
              </div>
              {ingressStatus && (
                <small className="success">{ingressStatus}</small>
              )}
            </div>
            <div className="manage-list">
              {ingressSources.length === 0 && (
                <div className="manage-card muted">
                  No external event sources configured.
                </div>
              )}
              {ingressSources.map((source) => {
                const target = String(
                  source.config.repo || source.config.url || "not configured",
                );
                const circuitOpen =
                  source.circuit_open_until &&
                  new Date(source.circuit_open_until).getTime() > Date.now();
                return (
                  <div className="manage-row px-4" key={source.source_id}>
                    <div className="min-w-0 flex-1">
                      <strong>{source.provider}</strong>
                      <div className="truncate text-sm text-faint">
                        {target}
                      </div>
                      <small>
                        {source.enabled ? "Enabled" : "Disabled"} · last success{" "}
                        {source.last_success_at || "never"} · failures{" "}
                        {source.consecutive_failures}
                      </small>
                      {circuitOpen && (
                        <div className="failure">
                          Circuit open until {source.circuit_open_until}
                        </div>
                      )}
                      {source.last_error && (
                        <div
                          className="failure truncate"
                          title={source.last_error}
                        >
                          {source.last_error}
                        </div>
                      )}
                    </div>
                    <div className="flex flex-wrap items-center justify-end gap-2">
                      <Button
                        className="bordered"
                        onClick={() => {
                          setIngressProvider(
                            source.provider === "github" ? "github" : "rss",
                          );
                          setIngressTarget(target);
                          setIngressInterval(
                            String(source.config.poll_interval_seconds || 60),
                          );
                          setEditingIngressId(source.source_id);
                        }}
                      >
                        Edit
                      </Button>
                      <Button
                        className="bordered"
                        onClick={() =>
                          void command("poll_external_ingress", {
                            sourceId: source.source_id,
                          })
                            .then(loadIngress)
                            .catch(onError)
                        }
                      >
                        Poll now
                      </Button>
                      <Button
                        className={source.enabled ? "bordered" : "primary"}
                        onClick={() =>
                          void command("set_external_ingress_enabled", {
                            sourceId: source.source_id,
                            enabled: !source.enabled,
                          })
                            .then(loadIngress)
                            .catch(onError)
                        }
                      >
                        {source.enabled ? "Disable" : "Enable"}
                      </Button>
                      <Button
                        className="bordered"
                        onClick={() => {
                          if (
                            !window.confirm(
                              "Delete this external event source?",
                            )
                          )
                            return;
                          void command("delete_external_ingress_source", {
                            sourceId: source.source_id,
                          })
                            .then(loadIngress)
                            .catch(onError);
                        }}
                      >
                        Delete
                      </Button>
                    </div>
                  </div>
                );
              })}
            </div>
          </div>
        )}
        {tab === "connectors" && (
          <div className="space-y-5">
            <div className="rounded-xl2 border border-line bg-panel p-5">
              {!openConnector && (
                <>
                  <h2 className="text-[15px] font-semibold text-ink">
                    OpenWorker connector directory
                  </h2>
                  <p className="muted mt-1">
                    Default connector catalog from OpenWorker. OPCOS only
                    enables integrations that are implemented locally.
                  </p>
                </>
              )}
              {!openConnector && (
                <div className="grid grid-cols-[repeat(auto-fill,minmax(min(100%,440px),440px))] gap-2.5 mt-4">
                  {OPENWORKER_CONNECTORS.map((connector) => {
                    const connectorKind = connector.name.toLowerCase();
                    const configurable =
                      CONFIGURABLE_CONNECTOR_KINDS.has(connectorKind);
                    const integrated =
                      configurable || connector.name === "Linear";
                    const tokenStatus = connectorStatuses[connectorKind];
                    const status = configurable
                      ? tokenStatus?.connected
                        ? `Connected as ${tokenStatus.identity || "bot"}`
                        : "Configurable"
                      : connector.name === "Linear"
                        ? linearStatus.includes("Connected")
                          ? "Connected"
                          : "Configurable"
                        : "Not integrated";
                    return (
                      <IntegrationCard
                        key={connector.name}
                        icon={connector.name.slice(0, 1)}
                        title={connector.name}
                        badge={{
                          label:
                            configurable && tokenStatus?.connected
                              ? "Connected"
                              : status === "Configurable"
                                ? "Configurable"
                                : status === "Connected"
                                  ? "Connected"
                                  : "Not integrated",
                          tone:
                            (configurable && tokenStatus?.connected) ||
                            status === "Connected"
                              ? "success"
                              : status === "Configurable"
                                ? "info"
                                : "neutral",
                        }}
                        description={connector.description}
                        disabled={!configurable}
                        onClick={
                          configurable
                            ? () =>
                                setOpenConnector((value) =>
                                  value === connectorKind
                                    ? null
                                    : connectorKind,
                                )
                            : undefined
                        }
                        actions={
                          configurable ? (
                            <Button
                              className="bordered"
                              onClick={() =>
                                setOpenConnector((value) =>
                                  value === connectorKind
                                    ? null
                                    : connectorKind,
                                )
                              }
                            >
                              {openConnector === connectorKind
                                ? "Close"
                                : "Configure"}
                            </Button>
                          ) : !integrated ? (
                            <Button className="bordered" disabled>
                              Unavailable
                            </Button>
                          ) : undefined
                        }
                      />
                    );
                  })}
                </div>
              )}
              {openConnector &&
                CONFIGURABLE_CONNECTOR_KINDS.has(openConnector) && (
                  <div className="form-grid mt-2">
                    <div className="col-span-full flex items-center gap-3 border-b border-line pb-3">
                      <Button
                        className="bordered"
                        onClick={() => setOpenConnector(null)}
                      >
                        ‹ All connectors
                      </Button>
                      <div>
                        <h3 className="text-[15px] font-semibold text-ink">
                          {openConnector}
                        </h3>
                        <p className="muted">
                          {
                            OPENWORKER_CONNECTORS.find(
                              (item) =>
                                item.name.toLowerCase() === openConnector,
                            )?.description
                          }
                        </p>
                      </div>
                      {connectorStatuses[openConnector]?.connected && (
                        <span className="status-success ml-auto">
                          Connected
                        </span>
                      )}
                    </div>
                    {OAUTH_CONNECTOR_KINDS.includes(
                      openConnector as (typeof OAUTH_CONNECTOR_KINDS)[number],
                    ) && (
                      <p className="muted col-span-full">
                        Use your own OAuth application credentials. OPCOS opens
                        the provider authorization page in your browser.
                      </p>
                    )}
                    {(CONNECTOR_FIELDS[openConnector] || []).map((field) => (
                      <label className="field-label" key={field.key}>
                        {field.label}
                        <input
                          type={field.type || "text"}
                          value={
                            field.key === "token"
                              ? connectorTokens[openConnector] || ""
                              : connectorConfigs[openConnector]?.[field.key] ||
                                ""
                          }
                          placeholder={
                            field.key === "token" &&
                            connectorStatuses[openConnector]?.connected
                              ? "Stored securely"
                              : field.placeholder
                          }
                          onChange={(event) => {
                            if (field.key === "token") {
                              setConnectorTokens((items) => ({
                                ...items,
                                [openConnector]: event.target.value,
                              }));
                            } else {
                              setConnectorConfigs((items) => ({
                                ...items,
                                [openConnector]: {
                                  ...items[openConnector],
                                  [field.key]: event.target.value,
                                },
                              }));
                            }
                          }}
                        />
                      </label>
                    ))}
                    <Button
                      className="primary self-end"
                      onClick={() => {
                        const config = {
                          ...connectorConfigs[openConnector],
                          ...(connectorTokens[openConnector]
                            ? { token: connectorTokens[openConnector] }
                            : {}),
                        };
                        const request = OAUTH_CONNECTOR_KINDS.includes(
                          openConnector as (typeof OAUTH_CONNECTOR_KINDS)[number],
                        )
                          ? command<void>("connector_oauth_start", {
                              kind: openConnector,
                              config,
                            })
                          : openConnector === "browser"
                            ? command<TokenConnectorStatus>(
                                "connector_browser_check",
                                { hostId: selected?.host_id || "local" },
                              )
                            : command<TokenConnectorStatus>("connector_save", {
                                kind: openConnector,
                                token: connectorTokens[openConnector] || null,
                                config,
                              });
                        void request
                          .then((value) => {
                            if (value) {
                              setConnectorStatuses((items) => ({
                                ...items,
                                [openConnector]: value,
                              }));
                            }
                            setConnectorTokens((items) => ({
                              ...items,
                              [openConnector]: "",
                            }));
                            setConnectorMessages((items) => ({
                              ...items,
                              [openConnector]: OAUTH_CONNECTOR_KINDS.includes(
                                openConnector as (typeof OAUTH_CONNECTOR_KINDS)[number],
                              )
                                ? "Authorization started in your browser."
                                : `Connected as ${value?.identity || "host capability"}.`,
                            }));
                          })
                          .catch((error) => {
                            setConnectorMessages((items) => ({
                              ...items,
                              [openConnector]: errorMessage(error),
                            }));
                          });
                      }}
                    >
                      {OAUTH_CONNECTOR_KINDS.includes(
                        openConnector as (typeof OAUTH_CONNECTOR_KINDS)[number],
                      )
                        ? "Connect"
                        : openConnector === "browser"
                          ? "Connect"
                          : "Save & verify"}
                    </Button>
                    {connectorMessages[openConnector] && (
                      <small
                        className={
                          connectorMessages[openConnector].startsWith(
                            "Connected",
                          )
                            ? "success"
                            : "failure"
                        }
                      >
                        {connectorMessages[openConnector]}
                      </small>
                    )}
                  </div>
                )}
            </div>
            <div className="rounded-xl2 border border-line bg-panel p-5">
              <h2 className="text-[15px] font-semibold text-ink">
                {translate("linear")}
              </h2>
              <p className="muted mt-1">
                Direct GraphQL integration using a Personal API Key stored in
                SecretStore. No OAuth callback or public listener is used.
              </p>
              <div className="form-grid mt-4">
                <label className="field-label">
                  Linear Personal API Key
                  <input
                    type="password"
                    value={linearPat}
                    onChange={(event) => setLinearPat(event.target.value)}
                    placeholder={translate("linearApiKey")}
                  />
                </label>
                <div className="flex gap-2 items-end">
                  <Button
                    className="primary"
                    onClick={() =>
                      command("save_secret_metadata", {
                        name: "linear-pat",
                        scope: "global",
                        purpose: "Linear connector Personal API Key",
                        value: linearPat,
                      })
                        .then(() => {
                          setLinearPat("");
                          setLinearStatus("Linear key saved in SecretStore.");
                        })
                        .catch(onError)
                    }
                  >
                    Save key
                  </Button>
                  <Button
                    className="bordered"
                    onClick={() =>
                      command<Record<string, unknown>>("linear_connection")
                        .then((value) => {
                          const viewer = value.viewer as
                            Record<string, unknown> | undefined;
                          setLinearStatus(
                            value.connected
                              ? `Connected as ${String(viewer?.name || "Linear user")}.`
                              : "Linear connection failed.",
                          );
                        })
                        .catch((error) => {
                          setLinearStatus(String(error));
                          onError(error);
                        })
                    }
                  >
                    Test connection
                  </Button>
                </div>
              </div>
              {linearStatus && <p className="mt-3 text-sm">{linearStatus}</p>}
            </div>
            <div className="rounded-xl2 border border-line bg-panel p-5">
              <h3 className="text-[14px] font-semibold text-ink">
                Issue tools
              </h3>
              <div className="flex gap-2 mt-3">
                <input
                  value={linearIssueId}
                  onChange={(event) => setLinearIssueId(event.target.value)}
                  placeholder={translate("issueIdentifierExample")}
                />
                <Button
                  className="bordered"
                  onClick={() =>
                    command<Record<string, unknown>>("linear_get_issue", {
                      identifier: linearIssueId,
                    })
                      .then(setLinearIssue)
                      .catch(onError)
                  }
                >
                  Read issue
                </Button>
                <Button
                  className="bordered"
                  onClick={() =>
                    command<Array<Record<string, unknown>>>(
                      "linear_list_my_issues",
                      { limit: 50 },
                    )
                      .then(setLinearIssues)
                      .catch(onError)
                  }
                >
                  List mine
                </Button>
              </div>
              {linearIssue && (
                <pre className="code-block mt-3">
                  {JSON.stringify(linearIssue, null, 2)}
                </pre>
              )}
              {linearIssues.length > 0 && (
                <div className="manage-list mt-3">
                  {linearIssues.map((issue) => (
                    <div className="manage-row" key={String(issue.id)}>
                      <span>
                        <strong>
                          {String(issue.identifier)} · {String(issue.title)}
                        </strong>
                        <small>{String(issue.url || "")}</small>
                      </span>
                    </div>
                  ))}
                </div>
              )}
            </div>
          </div>
        )}
        {tab === "index" && (
          <CollectionPage
            search=""
            onSearch={() => undefined}
            searchPlaceholder="Repository index"
            primary={
              <Button
                className="primary"
                disabled={!selected}
                onClick={() =>
                  selected &&
                  command<typeof indexStatus>("repo_index_refresh", {
                    sessionId: selected.id,
                  })
                    .then(setIndexStatus)
                    .catch(onError)
                }
              >
                {indexStatus?.status === "ready" ||
                indexStatus?.status === "limited"
                  ? "Refresh index"
                  : "Build index"}
              </Button>
            }
            rows={
              indexStatus ? (
                <div className="manage-row px-4">
                  <span>
                    <strong>{indexStatus.status}</strong>
                    <small>
                      {indexStatus.file_count} files ·{" "}
                      {indexStatus.symbol_count} symbols
                      {indexStatus.truncated ? " · limited by size" : ""}
                    </small>
                  </span>
                  <span className="muted">
                    {indexStatus.built_at
                      ? new Date(indexStatus.built_at).toLocaleString()
                      : "not built"}
                  </span>
                </div>
              ) : null
            }
            empty={
              selected
                ? "Repository index has not been built for this session host."
                : "Select a session to manage its repository index."
            }
          />
        )}
        {tab === "secrets" && (
          <CollectionPage
            search=""
            onSearch={() => undefined}
            searchPlaceholder={translate("searchSecretKeys")}
            primary={
              <Button
                className="primary"
                onClick={() => setSecretFormOpen((open) => !open)}
              >
                {secretFormOpen ? "Cancel" : translate("addSecret")}
              </Button>
            }
            rows={
              <>
                {secretFormOpen && (
                  <div className="manage-row px-4">
                    <div className="form-grid w-full">
                      <label className="field-label">
                        Name
                        <input
                          value={secretName}
                          onChange={(event) =>
                            setSecretName(event.target.value)
                          }
                        />
                      </label>
                      <label className="field-label">
                        Scope
                        <input
                          value={secretScope}
                          onChange={(event) =>
                            setSecretScope(event.target.value)
                          }
                        />
                      </label>
                      <label className="field-label">
                        Purpose
                        <input
                          value={secretPurpose}
                          onChange={(event) =>
                            setSecretPurpose(event.target.value)
                          }
                        />
                      </label>
                      <label className="field-label">
                        Value
                        <input
                          type="password"
                          value={secretValue}
                          onChange={(event) =>
                            setSecretValue(event.target.value)
                          }
                        />
                      </label>
                      <Button
                        className="primary"
                        onClick={() =>
                          command("save_secret_metadata", {
                            name: secretName,
                            scope: secretScope,
                            purpose: secretPurpose,
                            value: secretValue,
                          })
                            .then(() => {
                              setSecretName("");
                              setSecretPurpose("");
                              setSecretValue("");
                              setSecretFormOpen(false);
                              onRefresh();
                            })
                            .catch(onError)
                        }
                        disabled={
                          !secretName.trim() ||
                          !secretPurpose.trim() ||
                          !secretValue
                        }
                      >
                        Save secret
                      </Button>
                    </div>
                  </div>
                )}
                {secrets.length
                  ? secrets.map((secret) => (
                      <div className="manage-row px-4" key={secret.name}>
                        <span>
                          <strong>{secret.name}</strong>
                          <small>
                            {secret.scope} · {secret.purpose}
                          </small>
                        </span>
                        <Button
                          className="bordered"
                          onClick={() => {
                            if (
                              !window.confirm(
                                `Delete secret metadata "${secret.name}"?`,
                              )
                            )
                              return;
                            void command("delete_secret_metadata", {
                              name: secret.name,
                            })
                              .then(onRefresh)
                              .catch(onError);
                          }}
                        >
                          {translate("delete")}
                        </Button>
                      </div>
                    ))
                  : null}
              </>
            }
            empty={translate("noSecretMetadata")}
          />
        )}
        {tab === "blueprint" && (
          <div className="form-grid">
            <h2>{translate("remoteBlueprint")}</h2>
            <Button
              onClick={() =>
                selected &&
                command<Record<string, unknown>>("read_blueprint", {
                  sessionId: selected.id,
                })
                  .then(setBlueprint)
                  .catch(onError)
              }
            >
              Read blueprint
            </Button>
            {blueprint && (
              <pre className="code-block">
                {JSON.stringify(blueprint, null, 2)}
              </pre>
            )}
            <textarea
              value={blueprintCommand}
              onChange={(event) => setBlueprintCommand(event.target.value)}
              placeholder={translate("runRemoteCommand")}
            />
            <div className="inline-actions">
              <Button
                disabled={!selected}
                onClick={() =>
                  selected &&
                  command("execute_blueprint", {
                    sessionId: selected.id,
                    command: blueprintCommand,
                  })
                    .then((result) =>
                      setBlueprint(result as Record<string, unknown>),
                    )
                    .catch(onError)
                }
              >
                Run remote command
              </Button>
              <Button
                disabled={!selected}
                onClick={() =>
                  selected &&
                  command<Record<string, unknown>>("run_blueprint", {
                    sessionId: selected.id,
                  })
                    .then(setBlueprint)
                    .catch(onError)
                }
              >
                Run blueprint
              </Button>
            </div>
          </div>
        )}
      </div>
    </section>
  );
}
function McpManage({
  selected,
  onError,
  onPromptDraft,
}: {
  selected: Session | null;
  onError: (error: unknown) => void;
  onPromptDraft: (draft: string) => void;
}) {
  const [tools, setTools] = useState<Array<Record<string, unknown>>>([]);
  const [servers, setServers] = useState<Array<Record<string, unknown>>>([]);
  const [selectedServerId, setSelectedServerId] = useState("");
  const [resources, setResources] = useState<Array<Record<string, unknown>>>(
    [],
  );
  const [prompts, setPrompts] = useState<Array<Record<string, unknown>>>([]);
  const [promptArguments, setPromptArguments] = useState<
    Record<string, Record<string, string>>
  >({});
  const [resourcePreview, setResourcePreview] = useState<
    Array<Record<string, unknown>>
  >([]);
  const [contextResources, setContextResources] = useState<
    Array<Record<string, unknown>>
  >([]);
  const [search, setSearch] = useState("");
  const [editingServerId, setEditingServerId] = useState("");
  const [serverName, setServerName] = useState("");
  const [serverTransport, setServerTransport] = useState<
    "stdio" | "streamable-http" | "http-sse"
  >("streamable-http");
  const [serverUrl, setServerUrl] = useState("");
  const [serverCommand, setServerCommand] = useState("");
  const [serverArgs, setServerArgs] = useState("");
  const [serverEnv, setServerEnv] = useState("");
  const [serverToken, setServerToken] = useState("");
  const [serverEnabled, setServerEnabled] = useState(true);
  const [serverRequiresApproval, setServerRequiresApproval] = useState(true);
  useEffect(() => {
    void command<Array<Record<string, unknown>>>("list_mcp_servers")
      .then(setServers)
      .catch(onError);
    if (selected)
      void command<Array<Record<string, unknown>>>("mcp_tools", {
        sessionId: selected.id,
      })
        .then(setTools)
        .catch(onError);
  }, [selected?.id]);
  useEffect(() => {
    if (!selected) {
      setContextResources([]);
      return;
    }
    void command<Array<Record<string, unknown>>>("mcp_context_resources", {
      sessionId: selected.id,
    })
      .then(setContextResources)
      .catch(onError);
  }, [onError, selected?.id]);
  useEffect(() => {
    let active = true;
    if (
      !(window as Window & { __TAURI_INTERNALS__?: unknown })
        .__TAURI_INTERNALS__
    ) {
      return () => {
        active = false;
      };
    }
    const subscription = listen<{
      server_id?: string;
      version_id?: string;
      method?: string;
    }>("mcp-catalog-updated", (event) => {
      if (!active || !event.payload.server_id) return;
      void command<Array<Record<string, unknown>>>("list_mcp_servers")
        .then((nextServers) => {
          if (active) setServers(nextServers);
        })
        .catch(onError);
      if (
        mcpCatalogUpdateTargets(event.payload, selectedServerId) &&
        event.payload.version_id
      ) {
        void Promise.all([
          command<Array<Record<string, unknown>>>("mcp_resources", {
            serverId: event.payload.server_id,
            versionId: event.payload.version_id,
          }),
          command<Array<Record<string, unknown>>>("mcp_prompts", {
            serverId: event.payload.server_id,
            versionId: event.payload.version_id,
          }),
        ])
          .then(([nextResources, nextPrompts]) => {
            if (!active) return;
            setResources(nextResources);
            setPrompts(nextPrompts);
            setResourcePreview([]);
          })
          .catch(onError);
      }
    });
    return () => {
      active = false;
      void subscription.then((unsubscribe) => unsubscribe());
    };
  }, [onError, selectedServerId]);
  useEffect(() => {
    let active = true;
    if (
      !(window as Window & { __TAURI_INTERNALS__?: unknown })
        .__TAURI_INTERNALS__
    ) {
      return () => {
        active = false;
      };
    }
    const subscription = listen<{ session_id?: string; error?: string }>(
      "mcp-catalog-refresh-error",
      (event) => {
        if (
          active &&
          (!event.payload.session_id ||
            event.payload.session_id === selected?.id) &&
          event.payload.error
        ) {
          onError(event.payload.error);
        }
      },
    );
    return () => {
      active = false;
      void subscription.then((unsubscribe) => unsubscribe());
    };
  }, [onError, selected?.id]);
  const selectedServer = servers.find(
    (server) => String(server.id) === selectedServerId,
  );
  const resetServerForm = () => {
    setEditingServerId("");
    setServerName("");
    setServerTransport("streamable-http");
    setServerUrl("");
    setServerCommand("");
    setServerArgs("");
    setServerEnv("");
    setServerToken("");
    setServerEnabled(true);
    setServerRequiresApproval(true);
  };
  const editServer = (server: Record<string, unknown>) => {
    if (!isUserMcpServer(server)) return;
    setEditingServerId(String(server.id));
    setServerName(String(server.name || ""));
    setServerTransport(
      String(server.transport || "streamable-http") as McpTransport,
    );
    setServerUrl(String(server.url || ""));
    setServerCommand(String(server.command || ""));
    setServerArgs(
      Array.isArray(server.args) ? server.args.map(String).join("\n") : "",
    );
    setServerEnv(
      server.env && typeof server.env === "object"
        ? Object.entries(server.env as Record<string, unknown>)
            .map(([key, value]) => `${key}=${String(value)}`)
            .join("\n")
        : "",
    );
    setServerToken("");
    setServerEnabled(server.enabled !== false);
    setServerRequiresApproval(server.requires_approval !== false);
  };
  const saveServer = () => {
    const id = editingServerId || `mcp-${Date.now()}`;
    const credentialEnvKey = serverEnv
      .split("\n")
      .map((line) => line.trim().split("=", 1)[0])
      .find((key) =>
        /token|secret|password|authorization|client_secret/i.test(key),
      );
    if (credentialEnvKey) {
      onError(
        `Environment key "${credentialEnvKey}" looks sensitive. Store credentials in the bearer token field instead.`,
      );
      return;
    }
    const body = mcpServerFormBody({
      transport: serverTransport,
      url: serverUrl,
      command: serverCommand,
      args: serverArgs,
      env: serverEnv,
      enabled: serverEnabled,
      requiresApproval: serverRequiresApproval,
    });
    void command("save_asset", {
      id,
      kind: "mcp",
      title: serverName.trim(),
      body: JSON.stringify(body),
      trigger: null,
      scope: null,
      scopeKind: "global",
      enabled: serverEnabled,
      projectId: null,
    })
      .then(() =>
        serverToken.trim()
          ? command("save_mcp_credential", {
              serverId: id,
              value: JSON.stringify({ bearer_token: serverToken.trim() }),
              projectId: null,
            })
          : undefined,
      )
      .then(() =>
        serverEnabled
          ? command("retry_mcp_server", { serverId: id })
          : undefined,
      )
      .then(() => command<Array<Record<string, unknown>>>("list_mcp_servers"))
      .then((nextServers) => {
        setServers(nextServers);
        resetServerForm();
      })
      .catch(onError);
  };
  const removeServer = (server: Record<string, unknown>) => {
    if (!isUserMcpServer(server)) return;
    void command("delete_asset", { id: String(server.id) })
      .then(() => command<Array<Record<string, unknown>>>("list_mcp_servers"))
      .then(setServers)
      .catch(onError);
  };
  const loadServerCatalog = (server: Record<string, unknown>) => {
    const serverId = String(server.id);
    const versionId = String(server.version_id || "");
    setSelectedServerId(serverId);
    void Promise.all([
      command<Array<Record<string, unknown>>>("mcp_resources", {
        serverId,
        versionId,
      }),
      command<Array<Record<string, unknown>>>("mcp_prompts", {
        serverId,
        versionId,
      }),
    ])
      .then(([nextResources, nextPrompts]) => {
        setResources(nextResources);
        setPrompts(nextPrompts);
        setResourcePreview([]);
      })
      .catch(onError);
  };
  const previewResource = (resource: Record<string, unknown>) => {
    if (!selectedServer) return;
    void command<Array<Record<string, unknown>>>("mcp_read_resource", {
      serverId: String(selectedServer.id),
      uri: String(resource.uri),
    })
      .then(setResourcePreview)
      .catch(onError);
  };
  const filtered = tools.filter((tool) =>
    String(tool.name).toLowerCase().includes(search.toLowerCase()),
  );
  return (
    <>
      <section className="panel mb-4">
        <div className="flex items-center justify-between gap-2">
          <h2>
            {editingServerId
              ? translate("editMcpServer")
              : translate("addMcpServer")}
          </h2>
          {editingServerId && (
            <Button onClick={resetServerForm}>{translate("cancel")}</Button>
          )}
        </div>
        <div className="grid gap-2 md:grid-cols-2">
          <input
            value={serverName}
            onChange={(event) => setServerName(event.target.value)}
            placeholder="Server name"
          />
          <select
            value={serverTransport}
            onChange={(event) =>
              setServerTransport(
                event.target.value as "stdio" | "streamable-http" | "http-sse",
              )
            }
          >
            <option value="streamable-http">{translate("streamableHttp")}</option>
            <option value="http-sse">{translate("httpSse")}</option>
            <option value="stdio">{translate("stdio")}</option>
          </select>
          {serverTransport === "stdio" ? (
            <>
              <input
                value={serverCommand}
                onChange={(event) => setServerCommand(event.target.value)}
                placeholder="Command"
              />
              <textarea
                value={serverArgs}
                onChange={(event) => setServerArgs(event.target.value)}
                placeholder={translate("argumentsPerLine")}
                rows={3}
              />
              <textarea
                value={serverEnv}
                onChange={(event) => setServerEnv(event.target.value)}
                placeholder={translate("environmentPerLine")}
                rows={3}
              />
              <p className="text-xs text-slate-500 md:col-span-2">
                Use environment entries for non-sensitive runtime settings.
                Store bearer tokens in the secure token field below, not in
                environment variables.
              </p>
            </>
          ) : (
            <input
              className="md:col-span-2"
              value={serverUrl}
              onChange={(event) => setServerUrl(event.target.value)}
              placeholder="https://example.com/mcp"
            />
          )}
          <input
            type="password"
            value={serverToken}
            onChange={(event) => setServerToken(event.target.value)}
            placeholder="Bearer token (stored securely)"
          />
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={serverEnabled}
              onChange={(event) => setServerEnabled(event.target.checked)}
            />
            Enabled
          </label>
          <label className="flex items-center gap-2">
            <input
              type="checkbox"
              checked={serverRequiresApproval}
              onChange={(event) =>
                setServerRequiresApproval(event.target.checked)
              }
            />
            Require approval for tools
          </label>
        </div>
        <div className="mt-3 flex gap-2">
          <Button
            disabled={
              !serverName.trim() ||
              (serverTransport === "stdio"
                ? !serverCommand.trim()
                : !serverUrl.trim())
            }
            onClick={saveServer}
          >
            {editingServerId ? "Save and verify" : "Create and verify"}
          </Button>
        </div>
      </section>
      <CollectionPage
        search={search}
        onSearch={setSearch}
        searchPlaceholder={translate("searchMcp")}
        bare
        rows={
          filtered.length || servers.length ? (
            <div className="grid grid-cols-[repeat(auto-fill,minmax(min(100%,440px),440px))] gap-2.5">
              {servers
                .filter((server) =>
                  String(server.name)
                    .toLowerCase()
                    .includes(search.toLowerCase()),
                )
                .map((server) => (
                  <IntegrationCard
                    icon={String(server.name).slice(0, 1).toUpperCase()}
                    title={String(server.name)}
                    badge={{
                      label: String(server.status || "configured"),
                      tone:
                        String(server.status || "").toLowerCase() === "error"
                          ? "neutral"
                          : "success",
                    }}
                    description={
                      <>
                        {`${String(server.transport || "remote")} · ${String(server.url || "configured")}`}
                        {server.last_error
                          ? ` · ${String(server.last_error)}`
                          : ""}
                      </>
                    }
                    actions={
                      <div className="inline-actions">
                        {isUserMcpServer(server) && (
                          <>
                            <Button onClick={() => editServer(server)}>
                              Edit
                            </Button>
                            <Button onClick={() => removeServer(server)}>
                              Delete
                            </Button>
                          </>
                        )}
                        {String(server.status || "").toLowerCase() ===
                          "auth_required" && (
                          <Button
                            onClick={() =>
                              command("mcp_authorize", {
                                serverId: String(server.id),
                                versionId: String(server.version_id || ""),
                                resourceUrl: String(server.url || ""),
                              }).catch(onError)
                            }
                          >
                            Authorize
                          </Button>
                        )}
                        <Button onClick={() => loadServerCatalog(server)}>
                          Resources / prompts
                        </Button>
                        <Button
                          onClick={() =>
                            command("retry_mcp_server", {
                              serverId: String(server.id),
                            })
                              .then(() =>
                                command<Array<Record<string, unknown>>>(
                                  "list_mcp_servers",
                                ),
                              )
                              .then(setServers)
                              .catch(onError)
                          }
                        >
                          Retry
                        </Button>
                      </div>
                    }
                    key={String(server.id)}
                  />
                ))}
              {filtered.map((tool) => (
                <IntegrationCard
                  icon={String(tool.name).slice(0, 1).toUpperCase()}
                  title={String(tool.name)}
                  badge={{
                    label: tool.enabled === true ? "Enabled" : "Disabled",
                    tone: tool.enabled === true ? "success" : "neutral",
                  }}
                  description={`${String(tool.transport || "remote")} · ${String(tool.command || tool.url || "host-provided")}`}
                  actions={
                    <Button
                      disabled={!selected}
                      onClick={() =>
                        selected &&
                        command("set_mcp_tool_enabled", {
                          sessionId: selected.id,
                          name: String(tool.name),
                          enabled: tool.enabled !== true,
                        })
                          .then(() =>
                            command<Array<Record<string, unknown>>>(
                              "mcp_tools",
                              { sessionId: selected.id },
                            ),
                          )
                          .then(setTools)
                          .catch(onError)
                      }
                    >
                      {tool.enabled === true ? "Disable" : "Enable"}
                    </Button>
                  }
                  key={String(tool.name)}
                />
              ))}
            </div>
          ) : null
        }
        empty={
          selected
            ? "No MCP tools available."
            : "Select a session to inspect its host MCP tools."
        }
      />
      {selectedServer && (
        <section className="panel mt-4">
          <h2>
            {String(selectedServer.name)} resources ({resources.length}) ·
            prompts ({prompts.length}) · tools (
            {Number(selectedServer.tool_count || 0)})
          </h2>
          <div className="grid gap-2">
            {resources.map((resource) => (
              <div className="integration-card" key={String(resource.uri)}>
                <strong>{String(resource.title || resource.name)}</strong>
                <small>{mcpResourceSummary(resource)}</small>
                <div className="inline-actions">
                  <Button onClick={() => previewResource(resource)}>
                    Preview
                  </Button>
                  <Button
                    disabled={!selected}
                    onClick={() =>
                      selected &&
                      command("mcp_attach_resource", {
                        sessionId: selected.id,
                        serverId: String(selectedServer.id),
                        versionId: String(selectedServer.version_id || ""),
                        uri: String(resource.uri),
                      })
                        .then(() =>
                          command<Array<Record<string, unknown>>>(
                            "mcp_context_resources",
                            { sessionId: selected.id },
                          ),
                        )
                        .then(setContextResources)
                        .catch(onError)
                    }
                  >
                    Add to current context
                  </Button>
                </div>
              </div>
            ))}
            {prompts.map((prompt) => (
              <div className="integration-card" key={String(prompt.name)}>
                <strong>{String(prompt.title || prompt.name)}</strong>
                <small>{String(prompt.description || "MCP prompt")}</small>
                {Array.isArray(prompt.arguments) &&
                  prompt.arguments.map((argument) => {
                    const argumentName = String(
                      (argument as Record<string, unknown>).name || "",
                    );
                    return (
                      <input
                        key={argumentName}
                        placeholder={argumentName}
                        value={
                          promptArguments[String(prompt.name)]?.[
                            argumentName
                          ] || ""
                        }
                        onChange={(event) =>
                          setPromptArguments((current) => ({
                            ...current,
                            [String(prompt.name)]: {
                              ...current[String(prompt.name)],
                              [argumentName]: event.target.value,
                            },
                          }))
                        }
                      />
                    );
                  })}
                <Button
                  onClick={() =>
                    command("mcp_get_prompt", {
                      serverId: String(selectedServer.id),
                      name: String(prompt.name),
                      arguments: promptArguments[String(prompt.name)] || {},
                    })
                      .then((result) => {
                        const messages = (result as { messages?: unknown[] })
                          .messages;
                        if (messages?.length) {
                          onPromptDraft(mcpPromptMessagesToDraft(messages));
                        }
                      })
                      .catch(onError)
                  }
                >
                  Load into composer
                </Button>
              </div>
            ))}
            {resourcePreview.length > 0 && (
              <pre className="code-block">
                {JSON.stringify(resourcePreview, null, 2)}
              </pre>
            )}
            <h4>Current context resources ({contextResources.length})</h4>
            {contextResources.map((resource) => (
              <div
                className="inline-actions"
                key={`${String(resource.server_id)}:${String(resource.uri)}`}
              >
                <span>{String(resource.uri)}</span>
                <Button
                  onClick={() =>
                    selected &&
                    command("mcp_detach_resource", {
                      sessionId: selected.id,
                      serverId: String(resource.server_id),
                      versionId: String(resource.version_id),
                      uri: String(resource.uri),
                    })
                      .then(() =>
                        command<Array<Record<string, unknown>>>(
                          "mcp_context_resources",
                          { sessionId: selected.id },
                        ),
                      )
                      .then(setContextResources)
                      .catch(onError)
                  }
                >
                  Remove
                </Button>
              </div>
            ))}
          </div>
        </section>
      )}
    </>
  );
}

function Automations({
  sessions,
  assets,
  onError,
}: {
  sessions: Session[];
  assets: Asset[];
  onError: (error: unknown) => void;
}) {
  const [schedules, setSchedules] = useState<Schedule[]>([]);
  const [name, setName] = useState("");
  const [sessionId, setSessionId] = useState(sessions[0]?.id || "");
  const [playbookId, setPlaybookId] = useState(
    assets.find((asset) => asset.kind === "playbook")?.id || "",
  );
  const [cron, setCron] = useState("0 * * * *");
  const [trigger, setTrigger] = useState<"cron" | "filesystem">("cron");
  const [automationTab, setAutomationTab] = useState<"schedules" | "runs">(
    "schedules",
  );
  const [scheduleFormOpen, setScheduleFormOpen] = useState(false);
  const load = () =>
    command<Schedule[]>("list_schedules").then(setSchedules).catch(onError);
  useEffect(() => {
    void load();
  }, []);
  return (
    <div className="page">
      <div className="flex min-h-full">
        <nav className="page-subnav w-[208px] shrink-0 border-r border-line bg-panel/40 px-3 py-4">
          <div className="px-2 text-[13.5px] font-semibold mb-3 flex items-center gap-2">
            <Icon name="clock" size={16} /> Automations
          </div>
          {(["schedules", "runs"] as const).map((item) => (
            <button
              key={item}
              className={`w-full text-left px-2.5 py-2 rounded-lg text-[13px] flex items-center gap-2 ${
                automationTab === item
                  ? "bg-paper text-accent font-medium"
                  : "text-muted hover:bg-paper hover:text-ink"
              }`}
              onClick={() => setAutomationTab(item)}
            >
              <Icon
                name={item === "schedules" ? "refresh" : "code"}
                size={15}
              />
              {item === "schedules" ? "Schedules" : "Runs"}
            </button>
          ))}
        </nav>
        <div className="flex-1 min-w-0 overflow-y-auto">
          <div className="w-full px-7 py-6">
            <PageHeader
              title={automationTab === "schedules" ? "Schedules" : "Runs"}
              subtitle={
                automationTab === "schedules"
                  ? "Create and manage recurring OPCOS playbook runs."
                  : "Review the latest results reported by scheduled runs."
              }
            />
            {automationTab === "schedules" ? (
              <>
                <CollectionPage
                  search=""
                  onSearch={() => undefined}
                  searchPlaceholder={translate("searchSchedules")}
                  primary={
                    <Button
                      className="primary"
                      onClick={() => setScheduleFormOpen(true)}
                    >
                      New schedule
                    </Button>
                  }
                  rows={
                    schedules.length ? (
                      <>
                        {schedules.map((schedule) => (
                          <div className="manage-row px-4" key={schedule.id}>
                            <span>
                              <strong>{schedule.name}</strong>
                              <small>
                                {schedule.trigger || "cron"} · {schedule.cron} ·{" "}
                                {schedule.last_result || "never run"}
                              </small>
                            </span>
                            <Button
                              onClick={() =>
                                command("run_schedule", {
                                  scheduleId: schedule.id,
                                })
                                  .then(load)
                                  .catch(onError)
                              }
                            >
                              Run now
                            </Button>
                          </div>
                        ))}
                      </>
                    ) : null
                  }
                  empty={translate("noSchedules")}
                  form={
                    scheduleFormOpen ? (
                      <div className="manage-card form-grid">
                        <label>
                          Name
                          <input
                            value={name}
                            onChange={(event) => setName(event.target.value)}
                          />
                        </label>
                        <label>
                          Session
                          <SelectMenu
                            value={sessionId}
                            onChange={setSessionId}
                            options={sessions.map((session) => ({
                              value: session.id,
                              label: session.title,
                            }))}
                          />
                        </label>
                        <label>
                          Playbook
                          <SelectMenu
                            value={playbookId}
                            onChange={setPlaybookId}
                            options={assets
                              .filter((asset) => asset.kind === "playbook")
                              .map((asset) => ({
                                value: asset.id,
                                label: asset.title,
                              }))}
                          />
                        </label>
                        <label>
                          Trigger
                          <SelectMenu
                            value={trigger}
                            onChange={(value) =>
                              setTrigger(value as "cron" | "filesystem")
                            }
                            options={[
                              { value: "cron", label: "Cron" },
                              {
                                value: "filesystem",
                                label: "File changes (local only)",
                              },
                            ]}
                          />
                        </label>
                        <label>
                          Cron
                          <input
                            value={cron}
                            onChange={(event) => setCron(event.target.value)}
                          />
                        </label>
                        <Button
                          className="primary"
                          onClick={() =>
                            command("save_schedule", {
                              schedule: {
                                name,
                                sessionId,
                                playbookId,
                                cron,
                                trigger,
                                hostId:
                                  sessions.find((item) => item.id === sessionId)
                                    ?.host_id || "local",
                                workspace:
                                  sessions.find((item) => item.id === sessionId)
                                    ?.workspace || "",
                                harness:
                                  sessions.find((item) => item.id === sessionId)
                                    ?.harness || "builtin",
                                mode:
                                  sessions.find((item) => item.id === sessionId)
                                    ?.mode || "Interactive",
                                enabled: true,
                              },
                            })
                              .then(load)
                              .catch(onError)
                          }
                        >
                          Save automation
                        </Button>
                      </div>
                    ) : undefined
                  }
                />
              </>
            ) : (
              <CollectionPage
                search=""
                onSearch={() => undefined}
                searchPlaceholder={translate("searchRuns")}
                rows={
                  schedules.length ? (
                    <>
                      {schedules.map((schedule) => (
                        <div className="manage-row px-4" key={schedule.id}>
                          <span>
                            <strong>{schedule.name}</strong>
                            <small>
                              {schedule.last_result || "No run recorded yet"}
                            </small>
                          </span>
                        </div>
                      ))}
                    </>
                  ) : null
                }
                empty={translate("noRuns")}
              />
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

function Activity({
  selected,
  onError,
}: {
  selected: Session | null;
  onError: (error: unknown) => void;
}) {
  // Activity shell follows OpenWorker AuditView.tsx:20-64 and Cloud-Dev
  // RemotePanes.tsx:496-526 for timeline vocabulary; coordination controls
  // and durable command adapters are OPCOS-specific.
  const [taskId, setTaskId] = useState("");
  const [board, setBoard] = useState<Coordination | null>(null);
  const [roleId, setRoleId] = useState("");
  const [roleState, setRoleState] = useState("active");
  const [taskTitle, setTaskTitle] = useState("");
  const [taskFormOpen, setTaskFormOpen] = useState(false);
  const [worker, setWorker] = useState("");
  const [prUrl, setPrUrl] = useState("");
  const [message, setMessage] = useState("");
  const [messageFormOpen, setMessageFormOpen] = useState(false);
  const [rolesText, setRolesText] = useState(
    '[{"id":"leader","sort_order":0,"session_id":"","state":"Active"}]',
  );
  const [activityTab, setActivityTab] = useState<
    | "audit"
    | "actions"
    | "queue"
    | "events"
    | "goals"
    | "board"
    | "roles"
    | "tasks"
    | "messages"
    | "worklog"
    | "insights"
  >("board");
  const [worklog, setWorklog] = useState<Record<string, unknown> | null>(null);
  const [insights, setInsights] = useState<Record<string, unknown> | null>(
    null,
  );
  const [auditEvents, setAuditEvents] = useState<Record<string, unknown>[]>([]);
  const [actionLedger, setActionLedger] = useState<Record<string, unknown>[]>(
    [],
  );
  const [workQueue, setWorkQueue] = useState<Record<string, unknown>[]>([]);
  const [events, setEvents] = useState<Record<string, unknown>[]>([]);
  const [eventRules, setEventRules] = useState<Record<string, unknown>[]>([]);
  const [goals, setGoals] = useState<Record<string, unknown>[]>([]);
  const [planningHistory, setPlanningHistory] = useState<
    Record<string, unknown>[]
  >([]);
  const [currentPlan, setCurrentPlan] = useState<Record<
    string,
    unknown
  > | null>(null);
  const [goalDescription, setGoalDescription] = useState("");
  const [accountId, setAccountId] = useState("");
  const [accountHostId, setAccountHostId] = useState("");
  const [accountBindings, setAccountBindings] = useState<
    Record<string, unknown>[]
  >([]);
  const [loginProfilePath, setLoginProfilePath] = useState("");
  const [loginBackupDir, setLoginBackupDir] = useState("");
  const [loginProfile, setLoginProfile] = useState<Record<
    string,
    unknown
  > | null>(null);
  const [loginBackups, setLoginBackups] = useState<Record<string, unknown>[]>(
    [],
  );
  const [loginValidationUrl, setLoginValidationUrl] = useState("");
  const [loginExpectedSignal, setLoginExpectedSignal] = useState("");
  const [loginObservedSignal, setLoginObservedSignal] = useState("");
  const load = () =>
    command<Coordination>("coordination_snapshot", { taskId })
      .then(setBoard)
      .catch(onError);
  return (
    <main className="flex-1 min-w-0 min-h-0 flex bg-paper">
      <div className="flex flex-1 min-w-0 min-h-0">
        <nav className="page-subnav w-[208px] shrink-0 border-r border-line bg-panel/40 px-3 py-4">
          <div className="px-2 text-[13.5px] font-semibold mb-3 flex items-center gap-2">
            <Icon name="audit" size={16} /> Activity
          </div>
          {(
            [
              "audit",
              "actions",
              "queue",
              "events",
              "goals",
              "board",
              "roles",
              "tasks",
              "messages",
              "worklog",
              "insights",
            ] as const
          ).map((item) => (
            <button
              key={item}
              className={`w-full text-left px-2.5 py-2 rounded-lg text-[13px] flex items-center gap-2 ${activityTab === item ? "bg-paper text-accent font-medium" : "text-muted hover:bg-paper hover:text-ink"}`}
              onClick={() => {
                setActivityTab(item);
                if (item === "worklog" && selected)
                  void command<Record<string, unknown>>("session_worklog", {
                    sessionId: selected.id,
                    afterId: "",
                    limit: 200,
                  })
                    .then(setWorklog)
                    .catch(onError);
                if (item === "insights" && selected)
                  void command<Record<string, unknown>>("session_insights", {
                    sessionId: selected.id,
                  })
                    .then(setInsights)
                    .catch(onError);
                if (item === "audit")
                  void command<Record<string, unknown>[]>("audit_events", {
                    sessionId: selected?.id ?? null,
                  })
                    .then(setAuditEvents)
                    .catch(onError);
                if (item === "actions")
                  void command<Record<string, unknown>[]>(
                    "action_ledger_events",
                    {
                      limit: 200,
                    },
                  )
                    .then(setActionLedger)
                    .catch(onError);
                if (item === "queue")
                  void command<Record<string, unknown>[]>("work_queue_events", {
                    limit: 200,
                  })
                    .then(setWorkQueue)
                    .catch(onError);
                if (item === "events") {
                  void command<Record<string, unknown>[]>("event_stream", {
                    consumerId: "ui",
                    limit: 200,
                  })
                    .then(setEvents)
                    .catch(onError);
                  void command<Record<string, unknown>[]>("event_rules", {})
                    .then(setEventRules)
                    .catch(onError);
                }
                if (item === "goals") {
                  void command<Record<string, unknown>[]>(
                    "autonomous_goals",
                    {},
                  )
                    .then(setGoals)
                    .catch(onError);
                  void command<Record<string, unknown>[]>("planning_history", {
                    limit: 100,
                  })
                    .then(setPlanningHistory)
                    .catch(onError);
                  if (selected?.id) {
                    void command<Record<string, unknown> | null>(
                      "current_plan",
                      {
                        sessionId: selected.id,
                      },
                    )
                      .then(setCurrentPlan)
                      .catch(onError);
                  }
                  void command<Record<string, unknown>[]>(
                    "account_host_bindings",
                    {},
                  )
                    .then(setAccountBindings)
                    .catch(onError);
                  if (accountId.trim()) {
                    void command<Record<string, unknown> | null>(
                      "login_profile",
                      { accountId },
                    )
                      .then(setLoginProfile)
                      .catch(onError);
                    void command<Record<string, unknown>[]>(
                      "login_state_backups",
                      { accountId },
                    )
                      .then(setLoginBackups)
                      .catch(onError);
                  }
                }
              }}
            >
              <Icon
                name={
                  (
                    {
                      audit: "audit",
                      actions: "audit",
                      queue: "audit",
                      events: "audit",
                      goals: "sparkle",
                      board: "audit",
                      roles: "gear",
                      tasks: "code",
                      messages: "chat",
                      worklog: "clock",
                      insights: "sparkle",
                    } as const
                  )[item]
                }
                size={15}
              />
              {item[0].toUpperCase() + item.slice(1)}
            </button>
          ))}
        </nav>
        <div className="flex-1 min-w-0 overflow-y-auto hairline-scroll">
          <div className="activity-body w-full px-7 py-6">
            <header className="mb-5">
              <h1 className="text-[22px] font-semibold text-ink">
                {activityTab[0].toUpperCase() + activityTab.slice(1)}
              </h1>
              <p className="text-[13px] text-muted mt-1">
                {
                  (
                    {
                      audit:
                        "Review durable security and configuration events.",
                      actions:
                        "Review cross-session records of OPCOS external actions.",
                      queue:
                        "Review durable work items, retries, and dead-letter records.",
                      events:
                        "Review durable events, causal chains, and bounded effect rules.",
                      goals:
                        "Define bounded autonomous goals and review planning rounds.",
                      board: "Start or observe the active coordination board.",
                      roles: "Review board roles and their current state.",
                      tasks:
                        "Create, claim, complete, and verify coordination tasks.",
                      messages: "Send and review coordination messages.",
                      worklog: "Inspect the remote session worklog timeline.",
                      insights: "Review cross-session activity insights.",
                    } as const
                  )[activityTab]
                }
              </p>
            </header>
            {activityTab === "worklog" && (
              <CollectionPage
                search=""
                onSearch={() => undefined}
                searchPlaceholder={translate("filterWorklog")}
                primary={
                  <Button className="primary">
                    {translate("reloadWorklog")}
                  </Button>
                }
                rows={
                  selected && worklog ? (
                    <pre className="p-4">
                      {JSON.stringify(worklog, null, 2)}
                    </pre>
                  ) : null
                }
                empty={translate("selectSessionWorklog")}
              />
            )}
            {activityTab === "audit" && (
              <CollectionPage
                search=""
                onSearch={() => undefined}
                searchPlaceholder="Filter audit events"
                rows={
                  auditEvents.length ? (
                    <>
                      {auditEvents.map((event, index) => (
                        <div
                          className="manage-row px-4"
                          key={`${event.kind}-${index}`}
                        >
                          <span>
                            <strong>{String(event.kind)}</strong>
                            <small>{JSON.stringify(event.payload)}</small>
                          </span>
                        </div>
                      ))}
                    </>
                  ) : null
                }
                empty="No audit events recorded yet."
              />
            )}
            {activityTab === "actions" && (
              <CollectionPage
                search=""
                onSearch={() => undefined}
                searchPlaceholder="Filter action history"
                rows={
                  actionLedger.length ? (
                    <>
                      {actionLedger.map((action) => (
                        <div
                          className="manage-row px-4"
                          key={String(action.action_id)}
                        >
                          <span>
                            <strong>
                              {String(action.action_type)} ·{" "}
                              {String(action.platform)}
                            </strong>
                            <small>
                              {String(action.status)} ·{" "}
                              {String(action.account_id)} ·{" "}
                              {String(action.idempotency_key)}
                            </small>
                          </span>
                        </div>
                      ))}
                    </>
                  ) : null
                }
                empty="No action ledger records yet."
              />
            )}
            {activityTab === "queue" && (
              <CollectionPage
                search=""
                onSearch={() => undefined}
                searchPlaceholder="Filter durable work queue"
                rows={
                  workQueue.length ? (
                    <>
                      {workQueue.map((item) => (
                        <div
                          className="manage-row px-4"
                          key={String(item.queue_id)}
                        >
                          <span>
                            <strong>
                              {String(item.task_type)} · {String(item.status)}
                            </strong>
                            <small>
                              attempts {String(item.attempts)}/
                              {String(item.max_attempts)} ·{" "}
                              {String(item.queue_id)}
                              {item.status === "pending_approval" && (
                                <button
                                  className="ml-2 text-accent underline"
                                  onClick={() =>
                                    void command("approve_work_queue_item", {
                                      queueId: String(item.queue_id),
                                    })
                                      .then(() =>
                                        command<Record<string, unknown>[]>(
                                          "work_queue_events",
                                          { limit: 200 },
                                        ),
                                      )
                                      .then(setWorkQueue)
                                      .catch(onError)
                                  }
                                >
                                  approve
                                </button>
                              )}
                            </small>
                          </span>
                        </div>
                      ))}
                    </>
                  ) : null
                }
                empty="No durable work queue records yet."
              />
            )}
            {activityTab === "events" && (
              <div className="space-y-5">
                <CollectionPage
                  search=""
                  onSearch={() => undefined}
                  searchPlaceholder="Filter event stream"
                  rows={
                    events.length ? (
                      <>
                        {events.map((event) => (
                          <div
                            className="manage-row px-4"
                            key={String(event.event_id)}
                          >
                            <span>
                              <strong>
                                {String(event.kind)} · seq{" "}
                                {String(event.sequence)}
                              </strong>
                              <small>
                                {String(event.source)} · caused by{" "}
                                {String(event.caused_by ?? "none")} ·{" "}
                                {JSON.stringify(event.payload)}
                              </small>
                            </span>
                          </div>
                        ))}
                      </>
                    ) : null
                  }
                  empty="No durable events yet."
                />
                <CollectionPage
                  search=""
                  onSearch={() => undefined}
                  searchPlaceholder="Filter event rules"
                  rows={
                    eventRules.length ? (
                      <>
                        {eventRules.map((rule) => (
                          <div
                            className="manage-row px-4"
                            key={String(rule.rule_id)}
                          >
                            <span>
                              <strong>
                                {String(rule.kind_pattern)} →{" "}
                                {String(rule.effect_kind)}
                              </strong>
                              <small>
                                {rule.enabled ? "enabled" : "disabled"} ·{" "}
                                {String(rule.trigger_count)}/
                                {String(rule.max_triggers)} per{" "}
                                {String(rule.window_seconds)}s
                                <button
                                  className="ml-2 text-accent underline"
                                  onClick={() =>
                                    void command("set_event_rule_enabled", {
                                      ruleId: String(rule.rule_id),
                                      enabled: !rule.enabled,
                                    })
                                      .then(() =>
                                        command<Record<string, unknown>[]>(
                                          "event_rules",
                                          {},
                                        ),
                                      )
                                      .then(setEventRules)
                                      .catch(onError)
                                  }
                                >
                                  {rule.enabled ? "disable" : "enable"}
                                </button>
                              </small>
                            </span>
                          </div>
                        ))}
                      </>
                    ) : null
                  }
                  empty="No event rules configured."
                />
              </div>
            )}
            {activityTab === "goals" && (
              <div className="space-y-5">
                <div className="rounded-xl2 border border-line bg-panel p-5 space-y-3">
                  <h2 className="text-[15px] font-semibold">Current plan</h2>
                  {currentPlan ? (
                    <>
                      <div className="text-[13px]">
                        <strong>{String(currentPlan.title)}</strong>{" "}
                        <span className="text-muted">
                          revision {String(currentPlan.revision)}
                        </span>
                      </div>
                      <p className="text-[12px] text-muted">
                        {String(currentPlan.summary ?? "")}
                      </p>
                      <div className="space-y-1">
                        {(Array.isArray(currentPlan.steps)
                          ? currentPlan.steps
                          : []
                        ).map((step) => {
                          const item = step as Record<string, unknown>;
                          return (
                            <div
                              className="flex items-start justify-between gap-3 text-[12px]"
                              key={String(item.step_id)}
                            >
                              <span>
                                {Number(item.position ?? 0) + 1}.{" "}
                                {String(item.description)}
                              </span>
                              <span className="text-muted">
                                {String(item.status)}
                              </span>
                            </div>
                          );
                        })}
                      </div>
                    </>
                  ) : (
                    <p className="text-[12px] text-muted">
                      No tracked plan for the selected session.
                    </p>
                  )}
                </div>
                <div className="rounded-xl2 border border-line bg-panel p-5 space-y-3">
                  <h2 className="text-[15px] font-semibold">
                    Account host bindings
                  </h2>
                  <div className="grid grid-cols-2 gap-3">
                    <input
                      value={accountId}
                      onChange={(event) => setAccountId(event.target.value)}
                      placeholder="Account ID"
                    />
                    <input
                      value={accountHostId}
                      onChange={(event) => setAccountHostId(event.target.value)}
                      placeholder="Remote host ID"
                    />
                  </div>
                  <Button
                    className="primary"
                    onClick={() =>
                      void command("bind_account_host", {
                        accountId,
                        hostId: accountHostId,
                      })
                        .then(() =>
                          command<Record<string, unknown>[]>(
                            "account_host_bindings",
                            {},
                          ),
                        )
                        .then(setAccountBindings)
                        .catch(onError)
                    }
                    disabled={!accountId.trim() || !accountHostId.trim()}
                  >
                    Bind account to host
                  </Button>
                  <CollectionPage
                    search=""
                    onSearch={() => undefined}
                    searchPlaceholder="Filter bindings"
                    rows={
                      accountBindings.length ? (
                        <>
                          {accountBindings.map((binding) => (
                            <div
                              className="manage-row px-4"
                              key={String(binding.account_id)}
                            >
                              <span>
                                <strong>{String(binding.account_id)}</strong>
                                <small>host · {String(binding.host_id)}</small>
                              </span>
                            </div>
                          ))}
                        </>
                      ) : null
                    }
                    empty="No account host bindings."
                  />
                </div>
                <div className="rounded-xl2 border border-line bg-panel p-5 space-y-3">
                  <h2 className="text-[15px] font-semibold">Login state</h2>
                  <div className="grid grid-cols-2 gap-3">
                    <input
                      value={loginProfilePath}
                      onChange={(event) =>
                        setLoginProfilePath(event.target.value)
                      }
                      placeholder="Remote browser profile path"
                    />
                    <input
                      value={loginBackupDir}
                      onChange={(event) =>
                        setLoginBackupDir(event.target.value)
                      }
                      placeholder="Remote backup directory"
                    />
                  </div>
                  <div className="flex gap-2">
                    <Button
                      className="primary"
                      onClick={() =>
                        void command("save_login_profile", {
                          request: {
                            accountId,
                            profilePath: loginProfilePath,
                            backupDir: loginBackupDir,
                          },
                        })
                          .then((profile) =>
                            setLoginProfile(profile as Record<string, unknown>),
                          )
                          .catch(onError)
                      }
                      disabled={
                        !accountId.trim() ||
                        !loginProfilePath.trim() ||
                        !loginBackupDir.trim()
                      }
                    >
                      Save profile
                    </Button>
                    <Button
                      onClick={() =>
                        void command("backup_login_state", {
                          request: {
                            accountId,
                            idempotencyKey: `ui-login-backup-${Date.now()}`,
                          },
                        })
                          .then((backup) =>
                            setLoginBackups((items) => [
                              backup as Record<string, unknown>,
                              ...items,
                            ]),
                          )
                          .catch(onError)
                      }
                      disabled={!accountId.trim() || !loginProfile}
                    >
                      Backup
                    </Button>
                  </div>
                  {loginProfile ? (
                    <small className="text-muted">
                      validation ·{" "}
                      {String(
                        loginProfile.latest_validation_status ?? "not checked",
                      )}{" "}
                      {String(loginProfile.latest_validation_at ?? "")}
                    </small>
                  ) : null}
                  <div className="grid grid-cols-3 gap-3">
                    <input
                      value={loginValidationUrl}
                      onChange={(event) =>
                        setLoginValidationUrl(event.target.value)
                      }
                      placeholder="Validation URL"
                    />
                    <input
                      value={loginExpectedSignal}
                      onChange={(event) =>
                        setLoginExpectedSignal(event.target.value)
                      }
                      placeholder="Expected signal"
                    />
                    <input
                      value={loginObservedSignal}
                      onChange={(event) =>
                        setLoginObservedSignal(event.target.value)
                      }
                      placeholder="Observed signal (optional)"
                    />
                  </div>
                  <Button
                    onClick={() =>
                      void command("validate_login_state", {
                        request: {
                          accountId,
                          url: loginValidationUrl,
                          expectedSignal: loginExpectedSignal,
                          observedSignal: loginObservedSignal || null,
                        },
                      })
                        .then((status) =>
                          setLoginProfile((profile) =>
                            profile
                              ? {
                                  ...profile,
                                  latest_validation_status: status,
                                  latest_validation_at:
                                    new Date().toISOString(),
                                }
                              : profile,
                          ),
                        )
                        .catch(onError)
                    }
                    disabled={
                      !accountId.trim() ||
                      !loginValidationUrl.trim() ||
                      !loginExpectedSignal.trim()
                    }
                  >
                    Check login
                  </Button>
                  <CollectionPage
                    search=""
                    onSearch={() => undefined}
                    searchPlaceholder="Filter login backups"
                    rows={
                      loginBackups.length ? (
                        <>
                          {loginBackups.map((backup) => (
                            <div
                              className="manage-row px-4"
                              key={String(backup.backup_id)}
                            >
                              <span>
                                <strong>{String(backup.created_at)}</strong>
                                <small>
                                  {String(backup.size)} bytes · hash{" "}
                                  {String(backup.hash)}
                                </small>
                              </span>
                              <Button
                                onClick={() =>
                                  void command("restore_login_state", {
                                    request: {
                                      accountId,
                                      backupId: String(backup.backup_id),
                                      idempotencyKey: `ui-login-restore-${String(backup.backup_id)}-${Date.now()}`,
                                    },
                                  }).catch(onError)
                                }
                              >
                                Restore
                              </Button>
                            </div>
                          ))}
                        </>
                      ) : null
                    }
                    empty="No login-state backups."
                  />
                </div>
                <div className="rounded-xl2 border border-line bg-panel p-5 space-y-3">
                  <h2 className="text-[15px] font-semibold">New goal</h2>
                  <input
                    className="w-full"
                    value={goalDescription}
                    onChange={(event) => setGoalDescription(event.target.value)}
                    placeholder="Describe the outcome this company is pursuing"
                  />
                  <p className="text-[12px] text-muted">
                    New goals default to propose mode, one planning round per
                    hour, and a bounded queue.
                  </p>
                  <Button
                    className="primary"
                    onClick={() =>
                      void command("save_autonomous_goal", {
                        input: {
                          description: goalDescription,
                          sessionId: selected?.id ?? null,
                          projectId: selected?.project_id ?? null,
                          autonomyLevel: "propose",
                        },
                      })
                        .then(() => setGoalDescription(""))
                        .then(() =>
                          command<Record<string, unknown>[]>(
                            "autonomous_goals",
                            {},
                          ),
                        )
                        .then(setGoals)
                        .catch(onError)
                    }
                    disabled={!goalDescription.trim() || !selected}
                  >
                    Create goal
                  </Button>
                </div>
                <CollectionPage
                  search=""
                  onSearch={() => undefined}
                  searchPlaceholder="Filter goals"
                  rows={
                    goals.length ? (
                      <>
                        {goals.map((goal) => (
                          <div
                            className="manage-row px-4"
                            key={String(goal.goal_id)}
                          >
                            <span>
                              <strong>
                                {String(goal.description)} ·{" "}
                                {String(goal.status)}
                              </strong>
                              <small>
                                {String(goal.autonomy_level)} · failures{" "}
                                {String(goal.consecutive_failures)}/
                                {String(goal.failure_limit)}
                                <button
                                  className="ml-2 text-accent underline"
                                  onClick={() =>
                                    void command("run_autonomous_goal", {
                                      goalId: String(goal.goal_id),
                                    })
                                      .then(() =>
                                        command<Record<string, unknown>[]>(
                                          "planning_history",
                                          { goalId: String(goal.goal_id) },
                                        ),
                                      )
                                      .then(setPlanningHistory)
                                      .catch(onError)
                                  }
                                >
                                  plan now
                                </button>
                              </small>
                            </span>
                          </div>
                        ))}
                      </>
                    ) : null
                  }
                  empty="No autonomous goals yet."
                />
                <CollectionPage
                  search=""
                  onSearch={() => undefined}
                  searchPlaceholder="Filter planning rounds"
                  rows={
                    planningHistory.length ? (
                      <>
                        {planningHistory.map((round) => (
                          <div
                            className="manage-row px-4"
                            key={String(round.round_id)}
                          >
                            <span>
                              <strong>
                                {String(round.status)} · goal{" "}
                                {String(round.goal_id)}
                              </strong>
                              <small>
                                produced {String(round.produced_count)} ·{" "}
                                {String(round.reason ?? "completed")}
                              </small>
                            </span>
                          </div>
                        ))}
                      </>
                    ) : null
                  }
                  empty="No planning rounds yet."
                />
              </div>
            )}
            {activityTab === "insights" && (
              <div className="rounded-xl2 border border-line bg-panel p-5">
                {!selected ? (
                  <p className="empty-state">
                    Select a session to load insights.
                  </p>
                ) : (
                  <pre>{JSON.stringify(insights, null, 2)}</pre>
                )}
              </div>
            )}
            {activityTab !== "worklog" && activityTab !== "insights" && (
              <>
                {activityTab === "board" && (
                  <div className="rounded-xl2 border border-line bg-panel p-5 space-y-4">
                    <div>
                      <label className="field-label">
                        Coordination task ID
                      </label>
                      <input
                        value={taskId}
                        onChange={(event) => setTaskId(event.target.value)}
                        placeholder={translate("e.g. task-123")}
                      />
                      <p className="field-help">
                        The durable coordination board to observe or update.
                      </p>
                    </div>
                    <div>
                      <label className="field-label">
                        {translate("Initial roles")}
                      </label>
                      <textarea
                        value={rolesText}
                        onChange={(event) => setRolesText(event.target.value)}
                        placeholder='[{"id":"leader","sort_order":0,"session_id":"","state":"Active"}]'
                      />
                      <p className="field-help">
                        Use the JSON shape shown above when starting a new
                        board.
                      </p>
                    </div>
                    <Button
                      disabled={!taskId}
                      onClick={() => {
                        try {
                          const roles = JSON.parse(rolesText);
                          void command("coordination_start", {
                            input: { taskId, roles },
                          })
                            .then(() => {
                              setTaskFormOpen(false);
                              return load();
                            })
                            .catch(onError);
                        } catch {
                          onError("Roles must be valid JSON.");
                        }
                      }}
                    >
                      Start board
                    </Button>
                    <Button className="bordered" onClick={load}>
                      Observe
                    </Button>
                    <label className="field-label">
                      Role ID
                      <input
                        value={roleId}
                        onChange={(event) => setRoleId(event.target.value)}
                        placeholder={translate("leader")}
                      />
                    </label>
                    <SelectMenu
                      value={roleState}
                      onChange={setRoleState}
                      options={["active", "sleep", "paused"].map((value) => ({
                        value,
                        label: value,
                      }))}
                    />
                    <Button
                      className="bordered"
                      disabled={!taskId || !roleId}
                      onClick={() =>
                        command("coordination_set_role_state", {
                          taskId,
                          roleId,
                          stateName: roleState,
                        })
                          .then(load)
                          .catch(onError)
                      }
                    >
                      Set role
                    </Button>
                  </div>
                )}
                <div className="grid grid-cols-1 gap-4">
                  {activityTab === "roles" && (
                    <CollectionPage
                      search=""
                      onSearch={() => undefined}
                      searchPlaceholder={translate("searchRoles")}
                      rows={
                        board?.roles.length ? (
                          <>
                            {board.roles.map((role) => (
                              <div
                                className="manage-row px-4"
                                key={String(role.id)}
                              >
                                <span>
                                  <strong>{String(role.id)}</strong>
                                  <small>
                                    {String(role.state)} · Session{" "}
                                    {String(role.session_id)}
                                  </small>
                                </span>
                              </div>
                            ))}
                          </>
                        ) : null
                      }
                      empty={translate("noRoles")}
                    />
                  )}
                  {activityTab === "tasks" && (
                    <CollectionPage
                      search=""
                      onSearch={() => undefined}
                      searchPlaceholder={translate("searchTasks")}
                      primary={
                        <Button
                          className="primary"
                          onClick={() => setTaskFormOpen(true)}
                        >
                          New task
                        </Button>
                      }
                      rows={
                        board?.tasks.length ? (
                          <>
                            {board.tasks.map((task) => (
                              <div
                                className="manage-row px-4"
                                key={String(task.id || task.title)}
                              >
                                <span>
                                  <strong>
                                    {String(task.title || task.id)}
                                  </strong>
                                  <small>{String(task.phase)}</small>
                                </span>
                                <span className="inline-actions">
                                  <Button
                                    className="bordered"
                                    onClick={() =>
                                      command("coordination_claim_task", {
                                        id: task.id,
                                        worker,
                                      })
                                        .then(load)
                                        .catch(onError)
                                    }
                                  >
                                    Claim
                                  </Button>
                                  <Button
                                    className="bordered"
                                    onClick={() =>
                                      command("coordination_complete_task", {
                                        id: task.id,
                                        worker,
                                        verifiedPrUrl: prUrl || null,
                                      })
                                        .then(load)
                                        .catch(onError)
                                    }
                                  >
                                    Complete
                                  </Button>
                                </span>
                              </div>
                            ))}
                          </>
                        ) : null
                      }
                      empty={translate("noTasks")}
                      form={
                        taskFormOpen ? (
                          <div className="rounded-xl2 border border-line bg-panel p-5">
                            <div className="inline-actions">
                              <input
                                value={taskTitle}
                                onChange={(event) =>
                                  setTaskTitle(event.target.value)
                                }
                                placeholder={translate("New task")}
                              />
                              <input
                                value={worker}
                                onChange={(event) =>
                                  setWorker(event.target.value)
                                }
                                placeholder={translate("Worker / assignee")}
                              />
                              <input
                                value={prUrl}
                                onChange={(event) =>
                                  setPrUrl(event.target.value)
                                }
                                placeholder={translate("Verified PR URL")}
                              />
                              <Button
                                className="primary"
                                disabled={!taskId}
                                onClick={() =>
                                  command("coordination_create_task", {
                                    id: `task-${Date.now()}`,
                                    title: taskTitle,
                                    requireAcceptance: true,
                                  })
                                    .then(load)
                                    .catch(onError)
                                }
                              >
                                Create
                              </Button>
                            </div>
                          </div>
                        ) : undefined
                      }
                    />
                  )}
                  {activityTab === "messages" && (
                    <CollectionPage
                      search=""
                      onSearch={() => undefined}
                      searchPlaceholder={translate("searchMessages")}
                      primary={
                        <Button
                          className="primary"
                          onClick={() => setMessageFormOpen(true)}
                        >
                          New message
                        </Button>
                      }
                      rows={
                        board?.messages.length ? (
                          <>
                            {board.messages.map((item) => (
                              <div
                                className="manage-row px-4"
                                key={String(item.msg_id)}
                              >
                                <span>
                                  <strong>
                                    {String(item.from)} → {String(item.to)}
                                  </strong>
                                  <small>
                                    Kind: {String(item.kind)} · Message:{" "}
                                    {String(item.msg_id)}
                                  </small>
                                </span>
                              </div>
                            ))}
                          </>
                        ) : null
                      }
                      empty="No coordination messages yet."
                      form={
                        messageFormOpen ? (
                          <div className="rounded-xl2 border border-line bg-panel p-5">
                            <label className="field-label">
                              Message envelope
                            </label>
                            <textarea
                              value={message}
                              onChange={(event) =>
                                setMessage(event.target.value)
                              }
                              placeholder='{"kind":"status","payload":{}}'
                            />
                            <Button
                              className="bordered"
                              disabled={!taskId}
                              onClick={() => {
                                try {
                                  const envelope = JSON.parse(message);
                                  void command("coordination_message", {
                                    taskId,
                                    envelope,
                                  })
                                    .then(() => {
                                      setMessageFormOpen(false);
                                      return load();
                                    })
                                    .catch(onError);
                                } catch {
                                  onError("Message must be valid JSON.");
                                }
                              }}
                            >
                              Send message
                            </Button>
                          </div>
                        ) : undefined
                      }
                    />
                  )}
                  <section className="hidden">
                    <div className="inline-actions mb-3">
                      <input placeholder={translate("Search tasks")} />
                      <Button
                        className="primary"
                        onClick={() => setTaskFormOpen(true)}
                      >
                        New task
                      </Button>
                    </div>
                    {taskFormOpen && (
                      <div className="inline-actions">
                        <input
                          value={taskTitle}
                          onChange={(event) => setTaskTitle(event.target.value)}
                          placeholder={translate("New task")}
                        />
                        <input
                          value={worker}
                          onChange={(event) => setWorker(event.target.value)}
                          placeholder={translate("Worker / assignee")}
                        />
                        <input
                          value={prUrl}
                          onChange={(event) => setPrUrl(event.target.value)}
                          placeholder={translate("Verified PR URL")}
                        />
                        <Button
                          className="primary"
                          disabled={!taskId}
                          onClick={() =>
                            command("coordination_create_task", {
                              id: `task-${Date.now()}`,
                              title: taskTitle,
                              requireAcceptance: true,
                            })
                              .then(load)
                              .catch(onError)
                          }
                        >
                          Create
                        </Button>
                      </div>
                    )}
                    {board?.tasks.length ? (
                      board.tasks.map((task) => (
                        <div
                          className="board-card"
                          key={String(task.id || task.title)}
                        >
                          <strong>{String(task.title || task.id)}</strong>
                          <span>{String(task.phase)}</span>
                          <div className="inline-actions">
                            <Button
                              className="bordered"
                              onClick={() =>
                                command("coordination_claim_task", {
                                  id: task.id,
                                  worker,
                                })
                                  .then(load)
                                  .catch(onError)
                              }
                            >
                              Claim
                            </Button>
                            <Button
                              className="bordered"
                              onClick={() =>
                                command("coordination_complete_task", {
                                  id: task.id,
                                  worker,
                                  verifiedPrUrl: prUrl || null,
                                })
                                  .then(load)
                                  .catch(onError)
                              }
                            >
                              Complete
                            </Button>
                            <Button
                              className="bordered"
                              onClick={() => {
                                if (!task.verified_pr_url && !task.pr) {
                                  onError(
                                    "Cannot accept task without a verified PR URL.",
                                  );
                                  return;
                                }
                                void command("coordination_accept_task", {
                                  id: task.id,
                                })
                                  .then(load)
                                  .catch(onError);
                              }}
                            >
                              Accept
                            </Button>
                          </div>
                        </div>
                      ))
                    ) : (
                      <p className="empty-state">
                        {translate("No coordination tasks yet.")}
                      </p>
                    )}
                  </section>
                  <section className="hidden">
                    <div className="inline-actions mb-3">
                      <input placeholder={translate("Search messages")} />
                      <Button
                        className="primary"
                        onClick={() => setMessageFormOpen(true)}
                      >
                        New message
                      </Button>
                    </div>
                    {messageFormOpen && (
                      <>
                        <label className="field-label">
                          {translate("Message envelope")}
                        </label>
                        <textarea
                          value={message}
                          onChange={(event) => setMessage(event.target.value)}
                          placeholder='{"kind":"status","payload":{}}'
                        />
                        <p className="field-help">
                          Messages use the coordination envelope accepted by the
                          remote host.
                        </p>
                        <Button
                          className="bordered"
                          disabled={!taskId}
                          onClick={() => {
                            try {
                              const envelope = JSON.parse(message);
                              void command("coordination_message", {
                                taskId,
                                envelope,
                              })
                                .then(() => {
                                  setMessageFormOpen(false);
                                  return load();
                                })
                                .catch(onError);
                            } catch {
                              onError("Message must be valid JSON.");
                            }
                          }}
                        >
                          Send message
                        </Button>
                      </>
                    )}
                    {board?.messages.length ? (
                      board.messages.map((item) => (
                        <div className="board-card" key={String(item.msg_id)}>
                          <strong>
                            {String(item.from)} → {String(item.to)}
                          </strong>
                          <span>Kind: {String(item.kind)}</span>
                          <span>Message: {String(item.msg_id)}</span>
                          <span>Reply: {String(item.reply_to || "—")}</span>
                          <pre>{JSON.stringify(item.payload, null, 2)}</pre>
                        </div>
                      ))
                    ) : (
                      <p className="empty-state">
                        No coordination messages yet.
                      </p>
                    )}
                  </section>
                </div>
              </>
            )}
          </div>
        </div>
      </div>
    </main>
  );
}
function PageHeader({ title, subtitle }: { title: string; subtitle: string }) {
  return (
    <header className="page-header">
      <div>
        <h1 className="text-[22px] font-semibold text-ink">{title}</h1>
        <p>{subtitle}</p>
      </div>
    </header>
  );
}

function Field({ k, v }: { k: string; v: string }) {
  return (
    <div className="field">
      <label>{k}</label>
      <div className="value">{v}</div>
    </div>
  );
}

type PaneRoute = {
  sessionId: string;
  tab: PanelTab;
};

type PanelTab =
  | "info"
  | "artifacts"
  | "pr"
  | "terminal"
  | "shell"
  | "changes"
  | "progress"
  | "tasks"
  | "agents"
  | "desktop"
  | "ide"
  | "review"
  | "worklog"
  | "browser"
  | "insights";

function paneRoute(): PaneRoute | null {
  const hash = window.location.hash;
  if (!hash.startsWith("#/pane?")) return null;
  const params = new URLSearchParams(hash.slice("#/pane?".length));
  const sessionId = params.get("session");
  const tab = params.get("tab") as PanelTab | null;
  if (!sessionId || !tab) return null;
  const validTabs: PanelTab[] = [
    "info",
    "artifacts",
    "pr",
    "terminal",
    "shell",
    "changes",
    "progress",
    "tasks",
    "agents",
    "desktop",
    "ide",
    "review",
    "worklog",
    "browser",
    "insights",
  ];
  return validTabs.includes(tab) ? { sessionId, tab } : null;
}

function StandalonePane({ route }: { route: PaneRoute }) {
  const [selected, setSelected] = useState<Session | null>(null);
  const [providers, setProviders] = useState<ProviderDescriptor[]>([]);
  const [error, setError] = useState("");

  useEffect(() => {
    void Promise.all([
      command<Session[]>("list_sessions"),
      command<ProviderDescriptor[]>("provider_descriptors"),
    ])
      .then(([sessions, nextProviders]) => {
        setSelected(
          sessions.find((session) => session.id === route.sessionId) ?? null,
        );
        setProviders(nextProviders);
      })
      .catch((reason) => setError(errorMessage(reason)));
  }, [route.sessionId]);

  const close = () => {
    void import("@tauri-apps/api/window")
      .then(({ getCurrentWindow }) => getCurrentWindow().close())
      .catch(() => window.close());
  };

  return (
    <main className="standalone-pane">
      <header className="drawer-head">
        <strong className="drawer-title">{route.tab}</strong>
        <button
          className="drawer-action"
          title="关闭窗口"
          aria-label="关闭窗口"
          onClick={close}
        >
          <Icon name="x" />
        </button>
      </header>
      <div className="tab-body">
        {error && <div className="error-banner">{error}</div>}
        {!error && !selected && <div className="muted">Loading…</div>}
        {selected && route.tab === "info" && (
          <div className="info">
            <Field k={translate("Session ID")} v={selected.id} />
            <Field k={translate("Status")} v="Ready" />
            <Field k={translate("Host")} v={selected.host_name} />
            <Field
              k={translate("Workspace")}
              v={selected.workspace || translate("Not set")}
            />
            <Field k={translate("Model")} v={selected.model} />
            <div className="field">
              <label>Provider</label>
              <select
                value={selected.provider || ""}
                onChange={() => undefined}
                aria-label="Provider"
              >
                <option value="">Global default</option>
                {providers.map((provider) => (
                  <option key={provider.name} value={provider.name}>
                    {provider.title}
                  </option>
                ))}
              </select>
            </div>
          </div>
        )}
        {selected && route.tab === "insights" && (
          <StandaloneInsights
            selected={selected}
            onError={(reason) => setError(errorMessage(reason))}
          />
        )}
        {selected && route.tab === "artifacts" && (
          <ArtifactsPane selected={selected} />
        )}
        {selected &&
          route.tab !== "info" &&
          route.tab !== "insights" &&
          route.tab !== "artifacts" && (
            <SurfaceView
              tab={route.tab as SurfaceTab | "pr"}
              selected={selected}
              onError={(reason) => setError(errorMessage(reason))}
              visible
            />
          )}
      </div>
    </main>
  );
}

function StandaloneInsights({
  selected,
  onError,
}: {
  selected: Session;
  onError: (error: unknown) => void;
}) {
  const [insights, setInsights] = useState<Record<string, unknown> | null>(
    null,
  );
  useEffect(() => {
    void command<Record<string, unknown>>("session_insights", {
      sessionId: selected.id,
    })
      .then(setInsights)
      .catch(onError);
  }, [selected.id, onError]);
  if (!insights) return <div className="muted">Loading…</div>;
  return (
    <div className="info">
      {Object.entries(insights)
        .filter(([key]) => key !== "session_id")
        .map(([key, value]) => (
          <Field
            key={key}
            k={key}
            v={typeof value === "string" ? value : JSON.stringify(value)}
          />
        ))}
    </div>
  );
}

type ArtifactRecord = {
  id: string;
  path: string;
  kind: string;
  mime?: string | null;
  size_bytes?: number | null;
  sha256?: string | null;
  created_at: string;
};

function formatBytes(size: number | null | undefined) {
  if (size == null) return "Size unavailable";
  if (size < 1024) return `${size} B`;
  const units = ["KB", "MB", "GB"];
  let value = size;
  let unit = "B";
  for (const next of units) {
    value /= 1024;
    unit = next;
    if (value < 1024) break;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${unit}`;
}

function DiffPreview({ text }: { text: string }) {
  return (
    <pre className="artifact-code whitespace-pre-wrap">
      {text.split("\n").map((line, index) => (
        <span
          key={index}
          className={
            line.startsWith("+") && !line.startsWith("+++")
              ? "text-green-600"
              : line.startsWith("-") && !line.startsWith("---")
                ? "text-red-600"
                : undefined
          }
        >
          {line}
          {"\n"}
        </span>
      ))}
    </pre>
  );
}

function ArtifactsPane({ selected }: { selected: Session }) {
  const [artifacts, setArtifacts] = useState<ArtifactRecord[]>([]);
  const [opened, setOpened] = useState<ArtifactRecord | null>(null);
  const [content, setContent] = useState<Record<string, unknown> | null>(null);
  const [frameIndex, setFrameIndex] = useState(0);
  const [frameContent, setFrameContent] = useState<Record<
    string,
    unknown
  > | null>(null);
  const [loadError, setLoadError] = useState("");
  const refresh = () =>
    void command<ArtifactRecord[]>("list_artifacts", { sessionId: selected.id })
      .then((items) => {
        setLoadError("");
        setArtifacts(items);
      })
      .catch((error) => {
        setArtifacts([]);
        setLoadError(errorMessage(error));
      });
  useEffect(() => {
    setOpened(null);
    setContent(null);
    refresh();
  }, [selected.id]);
  useEffect(() => {
    if (!opened) return;
    setContent(null);
    setFrameIndex(0);
    setFrameContent(null);
    void command<Record<string, unknown>>("read_artifact", {
      sessionId: selected.id,
      artifactId: opened.id,
    })
      .then(setContent)
      .catch((error) => setContent({ error: errorMessage(error) }));
  }, [selected.id, opened?.id]);
  const manifest =
    opened?.kind === "recording_manifest" &&
    typeof content?.content === "string"
      ? (() => {
          try {
            return JSON.parse(content.content) as {
              frames?: Array<{
                timestamp_ms?: number;
                artifact_id?: string;
                reused?: boolean;
              }>;
              annotations?: Array<{
                annotation_type?: string;
                text?: string;
                result?: string;
                timestamp_ms?: number;
              }>;
              truncated?: boolean;
            };
          } catch {
            return null;
          }
        })()
      : null;
  const selectedFrame = manifest?.frames?.[frameIndex];
  useEffect(() => {
    if (!selectedFrame?.artifact_id) return;
    setFrameContent(null);
    void command<Record<string, unknown>>("read_artifact", {
      sessionId: selected.id,
      artifactId: selectedFrame.artifact_id,
    })
      .then(setFrameContent)
      .catch((error) => setFrameContent({ error: errorMessage(error) }));
  }, [selected.id, selectedFrame?.artifact_id]);
  if (opened) {
    return (
      <div className="artifact-viewer">
        <div className="artifact-head">
          <button
            className="artifact-icon-btn"
            onClick={() => setOpened(null)}
            aria-label="Back to artifacts"
            title="Back"
          >
            <RailIcon name="back" size={16} />
          </button>
          <div className="artifact-heading">
            <div className="artifact-title">
              <span>Artifacts</span>
              <span className="artifact-sep">/</span>
              <span>{opened.path.split(/[\\/]/).pop()}</span>
            </div>
            <div className="artifact-path">{opened.path}</div>
          </div>
        </div>
        <div className="artifact-preview">
          {!content ? (
            <div className="rail-muted">Loading…</div>
          ) : content.error ? (
            <div className="rail-error">{String(content.error)}</div>
          ) : typeof content.content_base64 === "string" ? (
            <img
              className="artifact-image max-w-full"
              src={`data:${String(content.mime ?? opened.mime ?? "image/png")};base64,${content.content_base64}`}
              alt={opened.path}
            />
          ) : opened.kind === "recording_manifest" && manifest ? (
            <div className="grid gap-3">
              <div className="flex items-center justify-between text-sm">
                <strong>Sampled screenshot timeline</strong>
                <span>
                  {manifest.frames?.length ?? 0} frames
                  {manifest.truncated ? " · truncated at limit" : ""}
                </span>
              </div>
              {selectedFrame &&
                typeof frameContent?.content_base64 === "string" && (
                  <img
                    className="artifact-image max-w-full"
                    src={`data:image/png;base64,${frameContent.content_base64}`}
                    alt={`Recording frame ${frameIndex + 1}`}
                  />
                )}
              <input
                type="range"
                min={0}
                max={Math.max(0, (manifest.frames?.length ?? 1) - 1)}
                value={frameIndex}
                onChange={(event) => setFrameIndex(Number(event.target.value))}
                disabled={!manifest.frames?.length}
              />
              <div className="grid gap-1 text-sm">
                {(manifest.annotations ?? []).map((annotation, index) => (
                  <button
                    className="text-left"
                    key={`${annotation.timestamp_ms ?? index}-${index}`}
                    onClick={() => {
                      const timestamp = annotation.timestamp_ms ?? 0;
                      const nearest =
                        manifest.frames?.findIndex(
                          (frame) => (frame.timestamp_ms ?? 0) >= timestamp,
                        ) ?? -1;
                      if (nearest >= 0) setFrameIndex(nearest);
                    }}
                  >
                    {annotation.annotation_type}: {annotation.text}
                    {annotation.result ? ` · ${annotation.result}` : ""}
                  </button>
                ))}
              </div>
            </div>
          ) : opened.kind === "diff" ? (
            <DiffPreview text={String(content.content ?? "")} />
          ) : (
            <pre className="artifact-code">{String(content.content ?? "")}</pre>
          )}
        </div>
      </div>
    );
  }
  return (
    <section className="rail-section">
      <div className="rail-section-head">
        <strong>
          Artifacts{artifacts.length ? ` (${artifacts.length})` : ""}
        </strong>
        <button
          className="rail-mini-btn"
          onClick={refresh}
          title="Refresh artifacts"
        >
          <RailIcon name="refresh" size={16} />
        </button>
      </div>
      <div className="rail-section-body">
        {loadError ? (
          <div className="rail-error">{loadError}</div>
        ) : artifacts.length === 0 ? (
          <div className="rail-muted">No artifacts yet.</div>
        ) : (
          <div className="artifact-list">
            {artifacts.slice(0, 16).map((artifact) => (
              <button
                className="artifact-row"
                key={artifact.id}
                onClick={() => setOpened(artifact)}
              >
                <span className="artifact-ico">
                  <RailIcon name="file" size={17} />
                </span>
                <span className="artifact-name">
                  {artifact.path.split(/[\\/]/).pop() || artifact.path}
                  <span className="artifact-row-meta">
                    {formatBytes(artifact.size_bytes)}
                    {artifact.sha256 ? "" : " · Hash not calculated"}
                    {" · "}
                    {new Date(artifact.created_at).toLocaleString()}
                  </span>
                </span>
                <span className="artifact-open">Open</span>
              </button>
            ))}
          </div>
        )}
      </div>
    </section>
  );
}

type ShellHistoryItem = {
  call_id: string;
  command: string;
  exit_code?: number | null;
  duration_ms?: number | null;
  output: string;
  output_truncated?: boolean;
};

function ShellHistoryPane({ selected }: { selected: Session }) {
  const [items, setItems] = useState<ShellHistoryItem[]>([]);
  const [error, setError] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const refresh = () =>
    void command<ShellHistoryItem[]>("session_shell_history", {
      sessionId: selected.id,
    })
      .then(setItems)
      .catch((reason) => setError(errorMessage(reason)));
  useEffect(() => {
    refresh();
  }, [selected.id]);
  return (
    <section className="rail-section">
      <div className="rail-section-head">
        <strong>Shell</strong>
        <button
          className="rail-mini-btn"
          onClick={refresh}
          title="Refresh shell history"
        >
          <RailIcon name="refresh" size={16} />
        </button>
      </div>
      <div className="rail-section-body">
        {error ? (
          <div className="rail-error">{error}</div>
        ) : items.length === 0 ? (
          <div className="rail-muted">No shell commands recorded yet.</div>
        ) : (
          <div className="rail-event-list">
            {items.map((item) => (
              <div className="rail-event-card" key={item.call_id}>
                <button
                  className="rail-event-head"
                  onClick={() =>
                    setOpen(open === item.call_id ? null : item.call_id)
                  }
                >
                  <code>{item.command || "(empty command)"}</code>
                  <span
                    className={item.exit_code === 0 ? "rail-ok" : "rail-muted"}
                  >
                    {item.exit_code == null
                      ? "running"
                      : `exit ${item.exit_code}`}
                  </span>
                </button>
                <div className="rail-event-meta">
                  {item.duration_ms == null ? "" : `${item.duration_ms} ms`}
                </div>
                {open === item.call_id &&
                  (item.output || item.output_truncated) && (
                    <pre className="rail-event-output">
                      {item.output}
                      {item.output_truncated ? "\n[Output truncated]" : ""}
                    </pre>
                  )}
              </div>
            ))}
          </div>
        )}
      </div>
    </section>
  );
}

function formatStat(value: number | null, suffix = "") {
  return value == null ? "Unknown" : `${value}${suffix}`;
}

function IterationStatsPane({ events }: { events: TimelineEvent[] }) {
  const summary = summarizeIterationStats(events);
  return (
    <section className="mt-4 rounded-lg border border-line p-3">
      <h3 className="text-sm font-semibold text-ink">Iteration stats</h3>
      <div className="mt-2 grid grid-cols-2 gap-2 text-xs">
        <Field k="Iterations" v={String(summary.iterations.length)} />
        <Field k="Input tokens" v={formatStat(summary.totalInputTokens)} />
        <Field k="Output tokens" v={formatStat(summary.totalOutputTokens)} />
        <Field
          k="Total duration"
          v={formatStat(summary.totalDurationMs, " ms")}
        />
        <Field k="Retries" v={formatStat(summary.totalRetries)} />
        <Field
          k="Compactions"
          v={`${summary.totalCompactions} (${summary.automaticCompactions} automatic, ${summary.manualCompactions} manual)`}
        />
      </div>
      {summary.iterations.length > 0 && (
        <div className="mt-3 flex flex-col gap-1">
          {summary.iterations.map((item) => (
            <details
              key={item.detailIndex}
              className="rounded border border-line"
            >
              <summary className="cursor-pointer px-2 py-1 text-xs text-muted">
                Iteration {item.iteration} · #{item.detailIndex} ·{" "}
                {item.toolCalls} tool calls
              </summary>
              <div className="grid grid-cols-2 gap-1 px-2 pb-2 text-xs">
                <Field k="Duration" v={formatStat(item.durationMs, " ms")} />
                <Field k="Inference" v={formatStat(item.inferenceMs, " ms")} />
                <Field
                  k="Tool execution"
                  v={formatStat(item.toolExecMs, " ms")}
                />
                <Field k="Harness" v={formatStat(item.harnessMs, " ms")} />
                <Field k="Input" v={formatStat(item.inputTokens)} />
                <Field k="Output" v={formatStat(item.outputTokens)} />
                <Field k="Retries" v={formatStat(item.retryCount)} />
                <Field k="Compactions" v={formatStat(item.compactionCount)} />
              </div>
            </details>
          ))}
        </div>
      )}
    </section>
  );
}

type FileChange = {
  path: string;
  edit_count: number;
  edits: Array<Record<string, unknown>>;
};

function LayoutSplitter({
  label,
  value,
  onChange,
  onCollapse,
  collapseThreshold,
  maxValue,
  className,
}: {
  label: string;
  value: number;
  onChange: (value: number) => void;
  onCollapse?: () => void;
  collapseThreshold?: number;
  maxValue?: number;
  className?: string;
}) {
  const applyValue = (next: number) => {
    const bounded =
      maxValue === undefined ? next : Math.min(next, Math.max(1, maxValue));
    if (collapseThreshold !== undefined && bounded <= collapseThreshold) {
      onCollapse?.();
      return true;
    }
    onChange(bounded);
    return false;
  };
  return (
    <div
      className={`layout-splitter${className ? ` ${className}` : ""}`}
      role="separator"
      aria-label={label}
      aria-orientation="vertical"
      aria-valuenow={Math.round(value)}
      tabIndex={0}
      onKeyDown={(event) => {
        if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
        event.preventDefault();
        applyValue(value + (event.key === "ArrowLeft" ? -16 : 16));
      }}
      onPointerDown={(event) => {
        event.preventDefault();
        try {
          event.currentTarget.setPointerCapture(event.pointerId);
        } catch {
          // Synthetic pointer events used by automated checks have no active pointer.
        }
        const startX = event.clientX;
        const startValue = value;
        const direction = label.includes("right") ? -1 : 1;
        let stop = () => {};
        const move = (moveEvent: PointerEvent) => {
          if (
            applyValue(startValue + direction * (moveEvent.clientX - startX))
          ) {
            stop();
          }
        };
        stop = () => {
          window.removeEventListener("pointermove", move);
          window.removeEventListener("pointerup", stop);
        };
        window.addEventListener("pointermove", move);
        window.addEventListener("pointerup", stop, { once: true });
      }}
    />
  );
}

function ChangesPane({ selected }: { selected: Session }) {
  const [items, setItems] = useState<FileChange[]>([]);
  const [gitDiff, setGitDiff] = useState<Record<string, unknown> | null>(null);
  const [error, setError] = useState("");
  const [open, setOpen] = useState<string | null>(null);
  const refresh = () => {
    setError("");
    void Promise.all([
      command<FileChange[]>("session_file_changes", { sessionId: selected.id }),
      selected.workspace
        ? command<Record<string, unknown>>("review_snapshot", {
            sessionId: selected.id,
            cwd: selected.workspace,
            base: "HEAD",
          })
        : Promise.resolve(null),
    ])
      .then(([changes, snapshot]) => {
        setItems(changes);
        setGitDiff(snapshot);
      })
      .catch((reason) => setError(errorMessage(reason)));
  };
  useEffect(() => {
    refresh();
  }, [selected.id, selected.workspace]);
  const changes = (gitDiff?.changes as Record<string, unknown> | undefined)
    ?.files;
  return (
    <section className="rail-section">
      <div className="rail-section-head">
        <strong>Changes</strong>
        <button
          className="rail-mini-btn"
          onClick={refresh}
          title="Refresh changes"
        >
          <RailIcon name="refresh" size={16} />
        </button>
      </div>
      <div className="rail-section-body">
        {error && <div className="rail-error">{error}</div>}
        {items.length === 0 ? (
          <div className="rail-muted">No file edits recorded yet.</div>
        ) : (
          <div className="rail-event-list">
            {items.map((item) => (
              <div className="rail-event-card" key={item.path}>
                <button
                  className="rail-event-head"
                  onClick={() => setOpen(open === item.path ? null : item.path)}
                >
                  <code>{item.path}</code>
                  <span className="rail-muted">{item.edit_count} edits</span>
                </button>
                {open === item.path &&
                  item.edits.map((edit, index) => (
                    <pre className="rail-event-output" key={index}>
                      {JSON.stringify(edit, null, 2)}
                    </pre>
                  ))}
              </div>
            ))}
          </div>
        )}
        {Array.isArray(changes) && changes.length > 0 && (
          <details className="rail-git-diff">
            <summary>Current git diff</summary>
            <pre>{JSON.stringify(changes, null, 2)}</pre>
          </details>
        )}
        {!gitDiff && selected.workspace && (
          <div className="rail-muted">
            Git diff unavailable for this host/workspace.
          </div>
        )}
      </div>
    </section>
  );
}

type ProgressEvent = {
  sequence: number;
  event_type?: string;
  category?: string;
  timestamp?: string;
  payload?: Record<string, unknown>;
};

function ProgressPane({ selected }: { selected: Session }) {
  const [events, setEvents] = useState<ProgressEvent[]>([]);
  const [category, setCategory] = useState("");
  const [error, setError] = useState("");
  const refresh = () =>
    void command<ProgressEvent[]>("session_progress", {
      sessionId: selected.id,
      category: category || null,
    })
      .then(setEvents)
      .catch((reason) => setError(errorMessage(reason)));
  useEffect(() => {
    refresh();
  }, [selected.id, category]);
  const categories = [
    ...new Set(events.map((event) => event.category).filter(Boolean)),
  ];
  return (
    <section className="rail-section">
      <div className="rail-section-head">
        <strong>Progress</strong>
        <select
          value={category}
          onChange={(event) => setCategory(event.target.value)}
        >
          <option value="">All</option>
          {categories.map((item) => (
            <option key={item} value={item}>
              {item}
            </option>
          ))}
        </select>
      </div>
      <div className="rail-section-body">
        {error ? (
          <div className="rail-error">{error}</div>
        ) : events.length === 0 ? (
          <div className="rail-muted">No progress events recorded yet.</div>
        ) : (
          <div className="rail-event-list">
            {events.map((event) => (
              <div
                className="rail-event-card"
                key={`${event.sequence}-${event.event_type}`}
              >
                <div className="rail-event-head">
                  <strong>{event.event_type || "working_event"}</strong>
                  <span className="rail-muted">{event.category}</span>
                </div>
                <div className="rail-event-meta">
                  {event.timestamp
                    ? new Date(event.timestamp).toLocaleString()
                    : ""}
                </div>
                <div className="rail-event-summary">
                  {JSON.stringify(event.payload || {})}
                </div>
              </div>
            ))}
          </div>
        )}
      </div>
    </section>
  );
}

function AgentRosterPane({
  selected,
  onError,
  onOpenSession,
}: {
  selected: Session;
  onError: (error: unknown) => void;
  onOpenSession: (session: Session) => void;
}) {
  const [agents, setAgents] = useState<ProjectAgent[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [workflow, setWorkflow] = useState<{
    stage_index?: number;
    status?: string;
    tasks?: unknown[];
  } | null>(null);
  const [error, setError] = useState("");
  const [busyAgentId, setBusyAgentId] = useState<string | null>(null);

  const refresh = () => {
    if (!selected.project_id) return;
    setError("");
    void Promise.all([
      command<ProjectAgent[]>("list_project_agents", {
        projectId: selected.project_id,
      }),
      command<Session[]>("list_sessions"),
      command<{
        stage_index?: number;
        status?: string;
        tasks?: unknown[];
      }>("project_workflow_snapshot", {
        projectId: selected.project_id,
      }),
    ])
      .then(([nextAgents, nextSessions, nextWorkflow]) => {
        setAgents(nextAgents);
        setSessions(nextSessions);
        setWorkflow(nextWorkflow);
      })
      .catch((reason) => {
        const message = errorMessage(reason);
        setError(message);
        onError(reason);
      });
  };

  useEffect(refresh, [selected.project_id]);

  if (!selected.project_id) {
    return (
      <section className="rail-section">
        <div className="rail-section-head">
          <strong>Agents</strong>
        </div>
        <div className="rail-section-body">
          <div className="rail-muted">
            This session is not associated with a project, so no project agent
            roster is available.
          </div>
        </div>
      </section>
    );
  }

  const rows = projectAgentRosterRows(agents, sessions);
  const startSession = async (agent: ProjectAgent) => {
    setBusyAgentId(agent.id);
    try {
      const session = await command<Session>("create_session", {
        title: agent.name,
        projectId: agent.project_id,
        agentId: agent.id,
        provider: agent.provider || null,
        model: agent.model,
        harness: agent.harness,
        mode: agent.mode,
      });
      onOpenSession(session);
      refresh();
    } catch (reason) {
      setError(errorMessage(reason));
      onError(reason);
    } finally {
      setBusyAgentId(null);
    }
  };

  return (
    <section className="rail-section">
      <div className="rail-section-head">
        <strong>Agents</strong>
        <button
          className="rail-mini-btn"
          onClick={refresh}
          title="Refresh agents"
          aria-label="Refresh agents"
        >
          <Icon name="refresh" size={16} />
        </button>
      </div>
      <div className="rail-section-body">
        {error && <div className="rail-error">{error}</div>}
        {workflow && (
          <div className="rail-muted agent-roster-workflow">
            Workflow stage:{" "}
            {typeof workflow.stage_index === "number"
              ? workflow.stage_index + 1
              : "Unknown"}{" "}
            · Status: {workflow.status?.trim() || "Unknown"}
            {Array.isArray(workflow.tasks) &&
              ` · Tasks: ${workflow.tasks.length}`}
          </div>
        )}
        {rows.length === 0 ? (
          <div className="rail-muted">This project has no agents.</div>
        ) : (
          <div className="rail-event-list">
            {rows.map(({ agent, session }) => {
              return (
                <div className="rail-event-card" key={agent.id}>
                  <div className="rail-event-head">
                    <strong>{projectAgentRosterValue(agent.name)}</strong>
                    <span className="rail-muted">
                      {projectAgentRosterValue(agent.state)}
                    </span>
                  </div>
                  <dl className="agent-roster-details">
                    <div>
                      <dt>Role</dt>
                      <dd>{projectAgentRosterValue(agent.role)}</dd>
                    </div>
                    <div>
                      <dt>Host</dt>
                      <dd>{projectAgentRosterHost(session)}</dd>
                    </div>
                    <div>
                      <dt>Branch</dt>
                      <dd title={agent.branch}>
                        {projectAgentRosterValue(agent.branch)}
                      </dd>
                    </div>
                    <div>
                      <dt>Worktree</dt>
                      <dd title={agent.worktree_path}>
                        {projectAgentRosterValue(agent.worktree_path)}
                      </dd>
                    </div>
                    <div>
                      <dt>Session</dt>
                      <dd>{session ? "Exists" : "None"}</dd>
                    </div>
                    <div>
                      <dt>Run state</dt>
                      <dd>{projectAgentRosterValue(session?.run_state)}</dd>
                    </div>
                    <div>
                      <dt>Stop reason</dt>
                      <dd>{projectAgentRosterValue(session?.stop_reason)}</dd>
                    </div>
                  </dl>
                  <div className="inline-actions">
                    {session ? (
                      <button
                        className="btn approval-primary"
                        onClick={() => onOpenSession(session)}
                      >
                        Open session
                      </button>
                    ) : (
                      <button
                        className="btn approval-primary"
                        disabled={busyAgentId === agent.id}
                        onClick={() => void startSession(agent)}
                      >
                        {busyAgentId === agent.id
                          ? "Starting…"
                          : "Start session"}
                      </button>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </section>
  );
}

function TasksPane({ events }: { events: TimelineEvent[] }) {
  const steps = latestPlan(events);
  if (!steps?.length) {
    return (
      <section className="tasks-pane">
        <div className="tasks-empty">No tasks yet.</div>
      </section>
    );
  }
  const completed = steps.filter((step) =>
    ["done", "completed", "failed", "abandoned"].includes(String(step.status)),
  ).length;
  return (
    <section className="tasks-pane">
      <div className="tasks-progress">
        {completed} / {steps.length} tasks completed
      </div>
      <div className="tasks-list">
        {steps.map((step, index) => {
          const status = String(step.status ?? "not_started");
          const complete = ["done", "completed"].includes(status);
          const active = status === "in_progress";
          return (
            <div className="tasks-row" key={`${step.content}-${index}`}>
              <span
                className={`tasks-status${complete ? " is-complete" : active ? " is-active" : ""}`}
                aria-hidden="true"
              >
                {complete ? "✓" : active ? "◌" : "○"}
              </span>
              <span>
                #{index + 1} {step.content}
              </span>
            </div>
          );
        })}
      </div>
    </section>
  );
}

function SessionRightPanel({
  selected,
  onError,
  running,
  collapsed,
  providers,
  onProviderChange,
  onCollapsedChange,
  width,
  onWidthChange,
  maxWidth,
  onCollapse,
  eventRefreshKey,
  transcript,
  focusTabRequest,
  onOpenSession,
}: {
  selected: Session;
  onError: (error: unknown) => void;
  running: boolean;
  collapsed: boolean;
  providers: ProviderDescriptor[];
  onProviderChange: (provider: string) => void;
  onCollapsedChange?: (collapsed: boolean) => void;
  width: number;
  onWidthChange: (width: number) => void;
  maxWidth: number;
  onCollapse: () => void;
  eventRefreshKey: string;
  transcript: TimelineEvent[];
  focusTabRequest?: { tab: PanelTab; requestId: number } | null;
  onOpenSession: (session: Session) => void;
}) {
  const [panelTab, setPanelTab] = useState<PanelTab>("info");
  const [opened, setOpened] = useState<PanelTab[]>(["info"]);
  const [insights, setInsights] = useState<Record<string, unknown> | null>(
    null,
  );
  const [iterationEvents, setIterationEvents] = useState<TimelineEvent[]>([]);
  const [capabilities, setCapabilities] = useState<
    Record<string, { state?: string; reason?: string }>
  >({});
  useEffect(() => {
    setCapabilities({});
    void command<{
      items?: Array<{ name: string; state: string; reason?: string | null }>;
    }>("session_capabilities", { sessionId: selected.id })
      .then((value) =>
        setCapabilities(
          Object.fromEntries(
            (value.items || []).map((item) => [
              item.name,
              { state: item.state, reason: item.reason || undefined },
            ]),
          ),
        ),
      )
      .catch(onError);
  }, [selected.id]);
  useEffect(() => {
    if (!focusTabRequest) return;
    setPanelTab(focusTabRequest.tab);
    setOpened((items) =>
      items.includes(focusTabRequest.tab)
        ? items
        : [...items, focusTabRequest.tab],
    );
    onCollapsedChange?.(false);
  }, [focusTabRequest, onCollapsedChange]);
  useEffect(() => {
    setInsights(null);
    void command<Record<string, unknown>>("session_insights", {
      sessionId: selected.id,
    })
      .then(setInsights)
      .catch(onError);
  }, [selected.id]);
  useEffect(() => {
    void command<TimelineEvent[]>("read_session_events", {
      sessionId: selected.id,
    })
      .then(setIterationEvents)
      .catch(onError);
  }, [selected.id, eventRefreshKey, running]);
  const informationTabs: Array<{
    id: typeof panelTab;
    label: string;
    icon: RailIconName;
  }> = [
    { id: "info", label: "Info", icon: "info" },
    { id: "shell", label: "Shell", icon: "terminal" },
    { id: "changes", label: "Changes", icon: "diff" },
    { id: "progress", label: "Progress", icon: "progress" },
    { id: "tasks", label: "Tasks", icon: "tasks" },
    { id: "agents", label: "Agents", icon: "agents" },
    { id: "artifacts", label: "Artifacts", icon: "file" },
    { id: "pr", label: "PR", icon: "branch" },
    { id: "insights", label: "Insights", icon: "sparkle" },
  ];
  if (selected.host_id !== "local") {
    informationTabs.splice(3, 0, {
      id: "worklog",
      label: "Worklog",
      icon: "progress",
    });
  }
  const workspaceTabs: Array<{
    id: typeof panelTab;
    label: string;
    icon: RailIconName;
  }> = [{ id: "review", label: "Diff", icon: "diff" }];
  const remoteTabs: Array<{
    id: typeof panelTab;
    label: string;
    icon: RailIconName;
  }> = [
    ...(capabilities.vnc?.state === "Unavailable" &&
    capabilities.computer_use?.state === "Unavailable" &&
    panelTab !== "desktop" &&
    !preserveSurfaceTabWhileSleeping(selected.sleep_state)
      ? []
      : [
          {
            id: "desktop" as const,
            label: "Desktop",
            icon: "monitor" as const,
          },
        ]),
    ...(capabilities.ide?.state === "Unavailable" &&
    panelTab !== "ide" &&
    !preserveSurfaceTabWhileSleeping(selected.sleep_state)
      ? []
      : [{ id: "ide" as const, label: "Editor", icon: "code" as const }]),
    ...(selected.host_id === "local" ||
    (capabilities.pty?.state === "Unavailable" &&
      panelTab !== "terminal" &&
      !preserveSurfaceTabWhileSleeping(selected.sleep_state))
      ? []
      : [
          {
            id: "terminal" as const,
            label: "Terminal",
            icon: "terminal" as const,
          },
        ]),
    ...(selected.host_id === "local" ||
    (capabilities.browser?.state === "Unavailable" &&
      panelTab !== "browser" &&
      !preserveSurfaceTabWhileSleeping(selected.sleep_state))
      ? []
      : [{ id: "browser" as const, label: "Browser", icon: "grid" as const }]),
  ];
  const tabs = [...informationTabs, ...workspaceTabs, ...remoteTabs];
  const unavailableCapability = preserveSurfaceTabWhileSleeping(
    selected.sleep_state,
  )
    ? null
    : panelTab === "browser" && capabilities.browser?.state === "Unavailable"
      ? capabilities.browser
      : panelTab === "desktop" &&
          capabilities.vnc?.state === "Unavailable" &&
          capabilities.computer_use?.state === "Unavailable"
        ? capabilities.vnc
        : panelTab === "ide" && capabilities.ide?.state === "Unavailable"
          ? capabilities.ide
          : null;
  const workspaceTabIds: PanelTab[] = [
    "review",
    "terminal",
    "desktop",
    "ide",
    "browser",
  ];
  const isWorkspaceTab = workspaceTabIds.includes(panelTab);
  const openTab = (id: PanelTab) => {
    if (!collapsed && panelTab === id) {
      onCollapsedChange?.(true);
      return;
    }
    setPanelTab(id);
    setOpened((items) => (items.includes(id) ? items : [...items, id]));
    onCollapsedChange?.(false);
  };
  const openStandalonePane = async () => {
    const url = new URL(window.location.href);
    url.search = "";
    url.hash = `/pane?session=${encodeURIComponent(selected.id)}&tab=${panelTab}`;
    const { WebviewWindow } = await import("@tauri-apps/api/webviewWindow");
    new WebviewWindow(`opcos-pane-${panelTab}-${Date.now()}`, {
      url: url.toString(),
      title: `${tabs.find((item) => item.id === panelTab)?.label || panelTab} · OPCOS`,
      width: 900,
      height: 700,
    });
  };
  return (
    <aside
      className={`right-shell right-rail session-right-panel${collapsed ? " drawer-collapsed" : ""}`}
    >
      {!collapsed && (
        <div className="right-panel session-panel-drawer">
          <div className="drawer-head">
            <strong className="drawer-title">
              {tabs.find((item) => item.id === panelTab)?.label}
            </strong>
            {running && <span className="live-pill">Live</span>}
            <div className="drawer-actions">
              {isWorkspaceTab && (
                <button
                  className="drawer-action"
                  title="放大（独立窗口打开）"
                  aria-label="放大（独立窗口打开）"
                  onClick={() => void openStandalonePane().catch(onError)}
                >
                  <Icon name="windowOpen" />
                </button>
              )}
              <button
                className="drawer-action"
                title={translate("Collapse session panel")}
                aria-label={translate("Collapse session panel")}
                onClick={() => {
                  onCollapsedChange?.(true);
                }}
              >
                <Icon name="x" />
              </button>
            </div>
          </div>
          <div className="session-panel-content">
            {opened.includes("info") && (
              <div
                className="session-pane"
                style={{ display: panelTab === "info" ? "block" : "none" }}
              >
                <div className="info">
                  <Field k={translate("Session ID")} v={selected.id} />
                  <Field
                    k={translate("Status")}
                    v={running ? "Running" : "Ready"}
                  />
                  <Field k={translate("Host")} v={selected.host_name} />
                  <Field
                    k={translate("Workspace")}
                    v={selected.workspace || translate("Not set")}
                  />
                  <Field k={translate("Model")} v={selected.model} />
                  <div className="field">
                    <label>Provider</label>
                    <select
                      value={selected.provider || ""}
                      onChange={(event) => onProviderChange(event.target.value)}
                      aria-label="Provider"
                    >
                      <option value="">Global default</option>
                      {providers.map((item) => (
                        <option
                          key={item.name}
                          value={item.name}
                          disabled={item.available === false}
                        >
                          {item.title}
                          {item.available === false ? " (unavailable)" : ""}
                        </option>
                      ))}
                    </select>
                  </div>
                  {running && (
                    <div className="actions">
                      <button
                        type="button"
                        onClick={() =>
                          void command("interrupt", {
                            sessionId: selected.id,
                          }).catch(onError)
                        }
                      >
                        Interrupt
                      </button>
                    </div>
                  )}
                  <IterationStatsPane events={iterationEvents} />
                </div>
              </div>
            )}
            {opened.includes("insights") && (
              <div
                className="session-pane"
                style={{ display: panelTab === "insights" ? "block" : "none" }}
              >
                <div className="p-4">
                  <h2 className="text-[15px] font-semibold text-ink">
                    Insights
                  </h2>
                  {insights ? (
                    <dl className="mt-4 space-y-3 text-[13px]">
                      {Object.entries(insights)
                        .filter(([key]) => key !== "session_id")
                        .map(([key, value]) => (
                          <div key={key}>
                            <dt className="text-muted">{key}</dt>
                            <dd>
                              {typeof value === "string"
                                ? value
                                : JSON.stringify(value)}
                            </dd>
                          </div>
                        ))}
                    </dl>
                  ) : (
                    <div className="muted">Loading insights…</div>
                  )}
                </div>
              </div>
            )}
            {opened.includes("artifacts") && (
              <div
                className="session-pane"
                style={{ display: panelTab === "artifacts" ? "block" : "none" }}
              >
                <ArtifactsPane selected={selected} />
              </div>
            )}
            {opened.includes("shell") && panelTab === "shell" && (
              <div className="session-pane">
                <ShellHistoryPane selected={selected} />
              </div>
            )}
            {opened.includes("changes") && panelTab === "changes" && (
              <div className="session-pane">
                <ChangesPane selected={selected} />
              </div>
            )}
            {opened.includes("progress") && panelTab === "progress" && (
              <div className="session-pane">
                <ProgressPane selected={selected} />
              </div>
            )}
            {opened.includes("tasks") && panelTab === "tasks" && (
              <div className="session-pane">
                <TasksPane events={transcript} />
              </div>
            )}
            {opened.includes("agents") && panelTab === "agents" && (
              <div className="session-pane">
                <AgentRosterPane
                  selected={selected}
                  onError={onError}
                  onOpenSession={onOpenSession}
                />
              </div>
            )}
            {opened.includes("desktop") && panelTab === "desktop" && (
              <div className="session-pane">
                {unavailableCapability ? (
                  <div className="rail-error">
                    {unavailableCapability.reason || "Capability unavailable."}
                  </div>
                ) : (
                  <SurfaceView
                    tab="desktop"
                    selected={selected}
                    onError={onError}
                    visible
                  />
                )}
              </div>
            )}
            {opened.includes("ide") && panelTab === "ide" && (
              <div className="session-pane">
                <SurfaceView
                  tab="ide"
                  selected={selected}
                  onError={onError}
                  visible
                />
              </div>
            )}
            {tabs
              .filter(
                (item) =>
                  item.id !== "info" &&
                  item.id !== "artifacts" &&
                  item.id !== "shell" &&
                  item.id !== "changes" &&
                  item.id !== "progress" &&
                  item.id !== "tasks" &&
                  item.id !== "agents" &&
                  item.id !== "desktop" &&
                  item.id !== "ide" &&
                  opened.includes(item.id),
              )
              .map((item) => (
                <div
                  className="session-pane"
                  key={item.id}
                  style={{ display: panelTab === item.id ? "flex" : "none" }}
                >
                  {item.id === "browser" &&
                  capabilities.browser?.state === "Unavailable" &&
                  !preserveSurfaceTabWhileSleeping(selected.sleep_state) ? (
                    <div className="rail-error">
                      {capabilities.browser.reason || "Capability unavailable."}
                    </div>
                  ) : (
                    <SurfaceView
                      tab={item.id as Exclude<SurfaceTab, "chat">}
                      selected={selected}
                      onError={onError}
                      visible={panelTab === item.id}
                    />
                  )}
                </div>
              ))}
          </div>
        </div>
      )}
      {!collapsed && (
        <LayoutSplitter
          label="Resize right session panel"
          value={width}
          collapseThreshold={260}
          maxValue={maxWidth}
          onCollapse={onCollapse}
          className="session-panel-resizer"
          onChange={onWidthChange}
        />
      )}
      <div className="icon-rail session-icon-rail">
        <div className="rail-group" aria-label="Information">
          {informationTabs.map((item) => (
            <button
              key={item.id}
              className={`rail-btn${panelTab === item.id ? " active" : ""}`}
              title={item.label}
              onClick={() => openTab(item.id)}
            >
              <RailIcon name={item.icon} />
            </button>
          ))}
        </div>
        <div className="rail-sep" />
        <div className="rail-group" aria-label="Workspace">
          {workspaceTabs.map((item) => (
            <button
              key={item.id}
              className={`rail-btn${panelTab === item.id ? " active" : ""}`}
              title={item.label}
              onClick={() => openTab(item.id)}
            >
              <RailIcon name={item.icon} />
            </button>
          ))}
        </div>
        <div className="rail-sep" />
        <div className="rail-group" aria-label="Remote host capabilities">
          {remoteTabs.map((item) => (
            <button
              key={item.id}
              className={`rail-btn${panelTab === item.id ? " active" : ""}`}
              title={item.label}
              onClick={() => openTab(item.id)}
            >
              <RailIcon name={item.icon} />
            </button>
          ))}
        </div>
      </div>
    </aside>
  );
}

class AppErrorBoundary extends Component<
  { children: ReactNode },
  { error: Error | null }
> {
  state = { error: null as Error | null };

  static getDerivedStateFromError(error: Error) {
    return { error };
  }

  render() {
    if (this.state.error) {
      return (
        <div className="error-boundary">
          <h1>{translate("workbenchError")}</h1>
          <p>{this.state.error.message}</p>
        </div>
      );
    }
    return this.props.children;
  }
}

function InboxPane({
  items,
  sessions,
  onResolve,
  onOpenSession,
}: {
  items: InboxRecord[];
  sessions: Session[];
  onResolve: (item: InboxRecord, resolution: string) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const [answers, setAnswers] = useState<Record<string, string>>({});
  const pending = items.filter((item) => item.state !== "resolved");
  const sessionTitle = (sessionId: string) =>
    sessions.find((session) => session.id === sessionId)?.title || sessionId;
  return (
    <div className="surface-panel p-4">
      <div className="flex items-center justify-between mb-4">
        <h2 className="text-base font-semibold">Inbox</h2>
        <span className="text-xs text-faint">{pending.length} pending</span>
      </div>
      <div className="space-y-3">
        {pending.length === 0 && <div className="muted">No pending items.</div>}
        {pending.map((item) => (
          <div
            key={`${item.session_id}:${item.call_id}`}
            className="approval rounded-xl border border-line p-3"
          >
            <div className="flex items-center justify-between gap-2">
              <strong>
                {item.kind === "question"
                  ? "Question"
                  : item.kind === "plan"
                    ? "Plan confirmation"
                    : "Approval"}
              </strong>
              <span className="text-xs text-faint">{item.tool}</span>
            </div>
            <div className="flex items-center gap-2 mt-2 text-xs text-faint">
              <button
                className="hover:text-ink underline-offset-2 hover:underline"
                onClick={() => onOpenSession(item.session_id)}
              >
                {sessionTitle(item.session_id)}
              </button>
              <span>·</span>
              <span>{relativeTime(item.created_at)}</span>
            </div>
            {item.kind === "approval" ? (
              <ApprovalCard
                item={{
                  kind: "approval",
                  callId: item.call_id,
                  name: item.tool,
                  args: item.payload,
                  reason: "requires approval",
                }}
                hostName={
                  sessions.find((session) => session.id === item.session_id)
                    ?.host_name
                }
                onApprove={(decision) => onResolve(item, decision)}
              />
            ) : (
              <>
                {item.kind === "question" && (
                  <div className="approval-with mt-3">
                    {String(
                      item.payload.question ||
                        item.payload.prompt ||
                        "Answer required",
                    )}
                  </div>
                )}
                {item.kind === "plan" &&
                  typeof item.payload.plan === "string" && (
                    <PreviewBlock text={item.payload.plan} mono={false} />
                  )}
                {item.kind === "directory" &&
                  typeof item.payload.path === "string" && (
                    <PreviewBlock text={item.payload.path} />
                  )}
                <div className="approval-btns mt-3">
                  {item.kind === "question" ? (
                    <>
                      <input
                        className="ob-input flex-1"
                        value={answers[item.call_id] || ""}
                        onChange={(event) =>
                          setAnswers((current) => ({
                            ...current,
                            [item.call_id]: event.target.value,
                          }))
                        }
                        placeholder="Type your answer"
                      />
                      <button
                        className="btn approval-primary"
                        disabled={!answers[item.call_id]?.trim()}
                        onClick={() => {
                          onResolve(item, answers[item.call_id].trim());
                          setAnswers((current) => {
                            const next = { ...current };
                            delete next[item.call_id];
                            return next;
                          });
                        }}
                      >
                        Answer
                      </button>
                    </>
                  ) : (
                    <>
                      <button
                        className="btn approval-primary"
                        onClick={() => onResolve(item, "allow")}
                      >
                        Allow
                      </button>
                      <button
                        className="btn quiet-deny"
                        onClick={() => onResolve(item, "deny")}
                      >
                        Deny
                      </button>
                    </>
                  )}
                </div>
              </>
            )}
          </div>
        ))}
      </div>
    </div>
  );
}

function QuestionCard({
  question,
  onCollapse,
  onAnswer,
}: {
  question: PendingQuestion;
  onCollapse: () => void;
  onAnswer: (answer: string) => Promise<void>;
}) {
  const [selectedOptions, setSelectedOptions] = useState<string[]>([]);
  const [submitting, setSubmitting] = useState(false);
  const options = question.options ?? [];
  const optionLabel = (index: number) =>
    String.fromCharCode("A".charCodeAt(0) + index);
  const answerOption = (answer: string) => {
    if (submitting) return;
    setSubmitting(true);
    void onAnswer(answer).catch(() => setSubmitting(false));
  };
  return (
    <div className="transcript-question-card">
      <div className="transcript-question-head">
        <strong>{question.question}</strong>
        <button
          className="transcript-question-close"
          type="button"
          aria-label="Collapse question"
          title="Collapse question"
          onClick={onCollapse}
        >
          ×
        </button>
      </div>
      {options.length > 0 && (
        <div className="approval-btns flex-wrap">
          {options.map((option, index) => {
            const selected = selectedOptions.includes(option);
            return (
              <button
                className={`approval-option-row${selected ? " selected" : ""}`}
                key={option}
                type="button"
                disabled={submitting}
                aria-pressed={question.allowMultiple ? selected : undefined}
                onClick={() => {
                  if (question.allowMultiple) {
                    setSelectedOptions((current) =>
                      selected
                        ? current.filter((item) => item !== option)
                        : [...current, option],
                    );
                  } else {
                    answerOption(option);
                  }
                }}
              >
                <span className="approval-option-key">
                  {optionLabel(index)}
                </span>
                <span>{option}</span>
              </button>
            );
          })}
          {question.allowMultiple && (
            <button
              className="approval-option-row"
              type="button"
              disabled={submitting || selectedOptions.length === 0}
              onClick={() => answerOption(JSON.stringify(selectedOptions))}
            >
              <span className="approval-option-key">↵</span>
              <span>{submitting ? "Sending…" : "Submit selection"}</span>
            </button>
          )}
        </div>
      )}
    </div>
  );
}

function AppContent() {
  const NAV_COLLAPSED_KEY = "opcos:nav-collapsed:v1";
  const [windowMaximized, setWindowMaximized] = useState(false);
  const [hosts, setHosts] = useState<Host[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const selectedIdRef = useRef<string | undefined>(undefined);
  const sessionsRef = useRef<Session[]>(sessions);
  const optimisticSessionIdsRef = useRef(new Set<string>());
  sessionsRef.current = sessions;
  const selectedFromList = useMemo(
    () => selectedSessionFromList(sessions, selectedId),
    [sessions, selectedId],
  );
  const lastSelectedSessionRef = useRef<Session | null>(null);
  if (selectedFromList) lastSelectedSessionRef.current = selectedFromList;
  const selected = sessionViewSelection(
    selectedId,
    selectedFromList,
    lastSelectedSessionRef.current,
  );
  const setSelected = (
    next: Session | null | ((current: Session | null) => Session | null),
  ) => {
    const rawResolved =
      typeof next === "function"
        ? next(
            selectedSessionFromList(
              sessionsRef.current,
              selectedIdRef.current,
            ) ?? lastSelectedSessionRef.current,
          )
        : next;
    const resolved = rawResolved ? normalizeSession(rawResolved) : null;
    if (!resolved) {
      if (typeof next === "function") return;
      selectedIdRef.current = undefined;
      setSelectedId(null);
      return;
    }
    selectedIdRef.current = resolved.id;
    setSelectedId(resolved.id);
    const exists = sessions.some((item) => item.id === resolved.id);
    if (!exists) optimisticSessionIdsRef.current.add(resolved.id);
    else optimisticSessionIdsRef.current.delete(resolved.id);
    setSessions((items) => {
      return exists
        ? items.map((item) =>
            item.id === resolved.id ? { ...item, ...resolved } : item,
          )
        : [...items, resolved];
    });
  };
  const [slashCommands, setSlashCommands] = useState<SlashCommand[]>([]);
  const [transcript, setTranscript] = useState<TimelineEvent[]>([]);
  const [acpCapabilities, setAcpCapabilities] =
    useState<AcpCapabilities | null>(null);
  const [liveTranscript, setLiveTranscript] = useState<TimelineEvent[]>([]);
  const transcriptScrollRef = useRef<HTMLDivElement>(null);
  const transcriptBottomRef = useRef<HTMLDivElement>(null);
  const transcriptAtBottomRef = useRef(true);
  const transcriptPreviousHeightRef = useRef(0);
  const [showTranscriptJump, setShowTranscriptJump] = useState(false);
  const [pendingQuestion, setPendingQuestion] =
    useState<PendingQuestion | null>(null);
  const [pendingQuestionCollapsed, setPendingQuestionCollapsed] =
    useState(false);
  const [pendingApprovals, setPendingApprovals] = useState<
    Record<string, PendingApproval>
  >({});
  const [restoredComposerDraft, setRestoredComposerDraft] = useState<{
    text: string;
    nonce: number;
  } | null>(null);
  useEffect(() => {
    setPendingQuestionCollapsed(false);
  }, [pendingQuestion?.callId]);
  const [surface, setSurface] = useState<
    "session" | "automations" | "manage" | "activity" | "inbox" | "project"
  >("session");
  const [projects, setProjects] = useState<Project[]>([]);
  const [selectedProject, setSelectedProject] = useState<Project | null>(null);
  const [projectDialogOpen, setProjectDialogOpen] = useState(false);
  const [inbox, setInbox] = useState<InboxRecord[]>([]);
  const [unattended, setUnattended] = useState(false);
  const [progressiveToolDisclosure, setProgressiveToolDisclosure] =
    useState(false);
  const [settingsTab, setSettingsTab] = useState<SettingsSection>("provider");
  const [query, setQuery] = useState("");
  const [error, setError] = useState("");
  const errorTimer = useRef<number | undefined>(undefined);
  const [running, setRunning] = useState(false);
  const [editingSessionTitle, setEditingSessionTitle] = useState(false);
  const [sessionTitleDraft, setSessionTitleDraft] = useState("");
  const titleEditingRef = useRef(false);
  const previousRunningRef = useRef(false);
  const submittingSessionIdRef = useRef<string | undefined>(undefined);
  const lastTouchedSessionRef = useRef<Record<string, number>>({});
  useEffect(() => {
    titleEditingRef.current = false;
    setEditingSessionTitle(false);
  }, [selected?.id]);
  useEffect(() => {
    selectedIdRef.current = selectedId ?? undefined;
    if (selectedId) {
      const now = Date.now();
      const lastTouched = lastTouchedSessionRef.current[selectedId] ?? 0;
      if (selected?.sleep_state === "asleep" || now - lastTouched >= 60_000) {
        lastTouchedSessionRef.current[selectedId] = now;
        void command("touch_session", { sessionId: selectedId });
      }
    }
  }, [selectedId]);
  const effectiveRunning = effectiveRunningState(
    pendingQuestion !== null,
    selected?.run_state,
    running,
  );
  const liveEvents = useMemo(
    () => mergeEvents(transcript, liveTranscript, true),
    [transcript, liveTranscript],
  );
  const updateTranscriptScrollState = () => {
    const element = transcriptScrollRef.current;
    if (!element) return;
    const atBottom =
      element.scrollHeight - element.scrollTop - element.clientHeight <= 12;
    transcriptAtBottomRef.current = atBottom;
    setShowTranscriptJump(!atBottom);
  };
  const jumpToTranscriptBottom = () => {
    const element = transcriptScrollRef.current;
    if (!element) return;
    transcriptAtBottomRef.current = true;
    setShowTranscriptJump(false);
    element.scrollTo({ top: element.scrollHeight, behavior: "smooth" });
  };
  useLayoutEffect(() => {
    const element = transcriptScrollRef.current;
    const content = transcriptBottomRef.current;
    if (!element || !content) return;
    transcriptAtBottomRef.current = true;
    setShowTranscriptJump(false);
    const followBottom = () => {
      if (!transcriptAtBottomRef.current) return;
      element.scrollTop = element.scrollHeight;
      transcriptPreviousHeightRef.current = element.scrollHeight;
    };
    const resizeObserver = new ResizeObserver(followBottom);
    resizeObserver.observe(content);
    followBottom();
    return () => resizeObserver.disconnect();
  }, [selectedId]);
  useEffect(() => {
    const element = transcriptScrollRef.current;
    if (!element) return;
    const wasAtBottom = transcriptAtBottomRef.current;
    const previousHeight = transcriptPreviousHeightRef.current;
    const nextHeight = element.scrollHeight;
    if (wasAtBottom) {
      element.scrollTop = nextHeight;
    } else if (nextHeight < previousHeight) {
      element.scrollTop = Math.min(element.scrollTop, nextHeight);
    }
    transcriptPreviousHeightRef.current = nextHeight;
    updateTranscriptScrollState();
  }, [transcript, effectiveRunning]);
  const [drawerCollapsed, setDrawerCollapsed] = useState(false);
  const [rightPanelWidth, setRightPanelWidth] = useState(() =>
    Math.max(
      1,
      Number(localStorage.getItem("opcos:session-right-width:v1")) ||
        Math.round(window.innerWidth * 0.3),
    ),
  );
  const [navWidth, setNavWidth] = useState(() =>
    Math.max(
      1,
      Number(localStorage.getItem("opcos:session-nav-width:v1")) || 276,
    ),
  );
  const [navCollapsed, setNavCollapsed] = useState(
    () => localStorage.getItem(NAV_COLLAPSED_KEY) === "1",
  );
  const [viewportWidth, setViewportWidth] = useState(() => window.innerWidth);
  const toggleNav = () => {
    const next = !navCollapsed;
    setNavCollapsed(next);
    localStorage.setItem(NAV_COLLAPSED_KEY, next ? "1" : "0");
  };
  useEffect(() => {
    localStorage.setItem(
      "opcos:session-right-width:v1",
      String(rightPanelWidth),
    );
    localStorage.setItem("opcos:session-nav-width:v1", String(navWidth));
  }, [navWidth, rightPanelWidth]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "b") {
        event.preventDefault();
        toggleNav();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });
  useEffect(() => {
    const onResize = () => setViewportWidth(window.innerWidth);
    window.addEventListener("resize", onResize);
    return () => window.removeEventListener("resize", onResize);
  }, []);
  const [hostName, setHostName] = useState("");
  const [hostUrl, setHostUrl] = useState("");
  const [hostToken, setHostToken] = useState("");
  const [hostVncPassword, setHostVncPassword] = useState("");
  const [editingHostId, setEditingHostId] = useState<string | null>(null);
  const [assets, setAssets] = useState<Asset[]>([]);
  const [providers, setProviders] = useState<ProviderDescriptor[]>([]);
  const [secrets, setSecrets] = useState<SecretMetadata[]>([]);
  const [models, setModels] = useState<ProviderModelOption[]>([]);
  const [showAllHomeModels, setShowAllHomeModels] = useState(false);
  const [homeInput, setHomeInput] = useState("");
  const [homePlusOpen, setHomePlusOpen] = useState(false);
  const [homeAttachment, setHomeAttachment] = useState<File | null>(null);
  const [homeHostId, setHomeHostId] = useState("");
  const [homeProvider, setHomeProvider] = useState("");
  const [homeModel, setHomeModel] = useState("auto");
  const [homeMode, setHomeMode] = useState("Auto");
  const [homeHarness, setHomeHarness] = useState("builtin");
  const [homeRole, setHomeRole] = useState("");
  const [homeSystemPrompt, setHomeSystemPrompt] = useState("");
  const [homeAgentTemplateId, setHomeAgentTemplateId] = useState("");
  const [homeAgentTemplates, setHomeAgentTemplates] = useState<LibraryEntry[]>(
    [],
  );
  const [harnessOptions, setHarnessOptions] = useState<
    Array<{ id: string; label: string; available: boolean; reason?: string }>
  >([]);
  const [selectedHarnessOptions, setSelectedHarnessOptions] = useState<
    Array<{ id: string; label: string; available: boolean; reason?: string }>
  >([]);
  useEffect(() => {
    if (!selected || selected.harness !== "acp") {
      setAcpCapabilities(null);
      return;
    }
    void command<AcpCapabilities>("acp_session_capabilities", {
      sessionId: selected.id,
    })
      .then(setAcpCapabilities)
      .catch(() => setAcpCapabilities(null));
  }, [selected?.id, selected?.harness]);
  useEffect(() => {
    const subscription = listen<{
      kind: "acp_session";
      session_id?: string;
      payload: AcpSessionEventPayload;
    }>("opcos://event", (event) => {
      if (
        !selected?.id ||
        event.payload.kind !== "acp_session" ||
        event.payload.session_id !== selected.id
      )
        return;
      const payload = event.payload.payload;
      setAcpCapabilities((current) => {
        if (!current) return current;
        if (payload.kind === "mode_update")
          return {
            ...current,
            currentModeId: payload.currentModeId,
            availableModes: payload.availableModes,
          };
        if (payload.kind === "config_update")
          return { ...current, configOptions: payload.configOptions };
        if (payload.kind === "commands_update")
          return {
            ...current,
            availableCommands: payload.availableCommands,
          };
        return current;
      });
    });
    return () => {
      void subscription.then((unlisten) => unlisten());
    };
  }, [selected?.id]);
  const [homeWorkspace, setHomeWorkspace] = useState("");
  const [secretBackend, setSecretBackend] = useState("");
  const [surfaceRequest, setSurfaceRequest] = useState<{
    sessionId: string;
    tab: SurfaceRequestTab;
    requestId: number;
  } | null>(null);
  const surfaceRequestId = useRef(0);
  const generation = useRef(0);
  const showErrorToast = (reason: unknown) => {
    const runtime = (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
    const message = errorMessage(reason);
    if (
      message.includes("Approval required before this tool can continue") ||
      message.includes(
        "Question requires an answer before this tool can continue",
      )
    )
      return;
    if (!runtime && /invoke|tauri/i.test(message)) return;
    const providerLike =
      /provider|HTTP\s+\d{3}|bad_response|overloaded|request failed/i.test(
        message,
      );
    const toast = providerLike
      ? providerErrorPresentation(redactApproval(message)).toast
      : redactApproval(message);
    setError(toast);
    if (errorTimer.current !== undefined)
      window.clearTimeout(errorTimer.current);
    errorTimer.current = window.setTimeout(() => {
      setError("");
      errorTimer.current = undefined;
    }, 6000);
  };
  useEffect(
    () => () => {
      if (errorTimer.current !== undefined)
        window.clearTimeout(errorTimer.current);
    },
    [],
  );
  useEffect(() => {
    void command<SlashCommand[]>("list_slash_commands", {
      projectId: selected?.project_id || null,
    })
      .then(setSlashCommands)
      .catch(() => undefined);
  }, [selected?.project_id]);
  useEffect(() => {
    if (
      !(window as Window & { __TAURI_INTERNALS__?: unknown })
        .__TAURI_INTERNALS__
    )
      return;
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void import("@tauri-apps/api/window")
      .then(async ({ getCurrentWindow }) => {
        const current = getCurrentWindow();
        const sync = async () => {
          const maximized = await current.isMaximized();
          if (!disposed) setWindowMaximized(maximized);
        };
        await sync();
        unlisten = await current.onResized(() => void sync());
      })
      .catch(() => {});
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);
  const refresh = async () => {
    const [
      nextHosts,
      rawNextSessions,
      nextAssets,
      nextProviders,
      nextSecrets,
      nextInbox,
    ] = await Promise.all([
      command<Host[]>("list_hosts"),
      command<Session[]>("list_sessions"),
      command<Asset[]>("list_assets"),
      command<ProviderDescriptor[]>("provider_descriptors"),
      command<SecretMetadata[]>("list_secret_metadata"),
      command<InboxRecord[]>("list_inbox"),
    ]);
    setHosts(nextHosts);
    const nextSessions = rawNextSessions.map(normalizeSession);
    const optimisticSessionIds = new Set(optimisticSessionIdsRef.current);
    const nextSelectedId = reconcileSelectedIdAfterRefresh(
      selectedIdRef.current ?? null,
      nextSessions,
      optimisticSessionIds,
    );
    if (nextSelectedId === null && selectedIdRef.current) {
      selectedIdRef.current = undefined;
      lastSelectedSessionRef.current = null;
      setSelectedId(null);
    }
    setSessions((current) =>
      mergeSessionsPreservingOptimistic(
        current,
        nextSessions,
        optimisticSessionIds,
      ),
    );
    setAssets(nextAssets);
    setProviders(nextProviders);
    setSecrets(nextSecrets);
    setInbox(nextInbox);
    for (const session of nextSessions)
      optimisticSessionIdsRef.current.delete(session.id);
    const nextProjects = await command<Project[]>("list_projects");
    setProjects(nextProjects);
    if (selectedProject) {
      setSelectedProject(
        nextProjects.find((item) => item.id === selectedProject.id) || null,
      );
    }
  };
  useEffect(() => {
    void refresh().catch((reason) => setError(errorMessage(reason)));
  }, []);
  useEffect(() => {
    if (!homeHostId && hosts[0]) setHomeHostId(hosts[0].id);
  }, [hosts, homeHostId]);
  useEffect(() => {
    if (!homeHostId) return;
    void command<
      Array<{ id: string; label: string; available: boolean; reason?: string }>
    >("harness_options", {
      hostId: homeHostId,
      workspace: homeWorkspace || null,
      projectId: null,
    })
      .then((options) => {
        setHarnessOptions(options);
        if (
          !options.some(
            (option) => option.id === homeHarness && option.available,
          )
        )
          setHomeHarness("builtin");
      })
      .catch(() => setHarnessOptions([]));
  }, [homeHostId, homeWorkspace]);
  useEffect(() => {
    if (!selected) return;
    void command<
      Array<{ id: string; label: string; available: boolean; reason?: string }>
    >("harness_options", {
      hostId: selected.host_id,
      workspace: selected.workspace || null,
      projectId: selected.project_id || null,
    })
      .then(setSelectedHarnessOptions)
      .catch(() => setSelectedHarnessOptions([]));
  }, [
    selected?.id,
    selected?.host_id,
    selected?.workspace,
    selected?.project_id,
  ]);
  useEffect(() => {
    if (!homeProvider && providers[0]) setHomeProvider(providers[0].name);
  }, [providers, homeProvider]);
  useEffect(() => {
    void command<LibraryEntry[]>("list_configured_library", {
      kind: "agent-template",
    })
      .then(setHomeAgentTemplates)
      .catch(() => setHomeAgentTemplates([]));
  }, []);
  useEffect(() => {
    void command<ProviderModelsResponse>("provider_models", {
      provider: homeProvider || "openai",
    })
      .then((response) => {
        setModels(response.models);
        setHomeModel((current) => {
          if (current !== "auto") return current;
          return (
            response.models.find((model) => !model.likely_non_chat)?.id ??
            response.models[0]?.id ??
            current
          );
        });
      })
      .catch((reason) => {
        if (
          (window as Window & { __TAURI_INTERNALS__?: unknown })
            .__TAURI_INTERNALS__
        )
          setError(errorMessage(reason));
      });
  }, [homeProvider]);
  useEffect(() => {
    const currentGeneration = ++generation.current;
    setTranscript([]);
    const submittingSelectedSession =
      submittingSessionIdRef.current === selected?.id;
    if (!submittingSelectedSession) setLiveTranscript([]);
    if (!submittingSelectedSession) setRestoredComposerDraft(null);
    setPendingQuestion(null);
    setPendingApprovals({});
    if (!submittingSelectedSession) setRunning(false);
    if (!selected) return;
    void Promise.all([
      command<TimelineEvent[]>("read_session_events", {
        sessionId: selected.id,
      }),
      command<InboxRecord[]>("list_inbox"),
      command<
        Array<{
          session_id: string;
          call_id: string;
          tool: string;
          arguments: Record<string, unknown>;
          state: string;
        }>
      >("list_pending", { sessionId: selected.id }),
    ])
      .then(([items, inboxItems, pendingItems]) => {
        if (generation.current !== currentGeneration) return;
        const pending = pendingItems.find(
          (item) => item.tool === "ask_user" && item.state !== "resolved",
        );
        const approvals = pendingItems
          .filter(
            (item) => item.tool !== "ask_user" && item.state !== "resolved",
          )
          .reduce<Record<string, PendingApproval>>((items, item) => {
            items[item.call_id] = {
              callId: item.call_id,
              name: item.tool,
              args: item.arguments,
            };
            return items;
          }, {});
        const inboxPending = inboxItems.find(
          (item) =>
            item.session_id === selected.id &&
            item.state === "pending" &&
            (item.kind === "question" || item.tool === "ask_user"),
        );
        if (pending) {
          setPendingQuestion(
            pendingQuestionFromPayload(pending.call_id, pending.arguments),
          );
        } else if (inboxPending) {
          setPendingQuestion(
            pendingQuestionFromPayload(
              inboxPending.call_id,
              inboxPending.payload,
            ),
          );
        }
        setPendingApprovals(approvals);
        setTranscript(mergeEvents([], items));
        if (submittingSessionIdRef.current !== selected.id)
          setLiveTranscript([]);
      })
      .catch((reason) => {
        if (generation.current === currentGeneration)
          setError(errorMessage(reason));
      });
  }, [selected?.id]);
  useEffect(() => {
    const wasRunning = previousRunningRef.current;
    previousRunningRef.current = running;
    if (!wasRunning || running || !selected) return;
    const currentGeneration = generation.current;
    void command<TimelineEvent[]>("read_session_events", {
      sessionId: selected.id,
    })
      .then((items) => {
        if (generation.current !== currentGeneration) return;
        setTranscript((current) => mergeEvents(current, items));
      })
      .catch((reason) => {
        if (generation.current === currentGeneration)
          setError(errorMessage(reason));
      });
  }, [running, selected?.id]);
  useEffect(() => {
    if (!selected) {
      setUnattended(false);
      setProgressiveToolDisclosure(false);
      return;
    }
    void command<boolean>("get_unattended", { sessionId: selected.id })
      .then(setUnattended)
      .catch((reason) => setError(errorMessage(reason)));
    void command<boolean>("get_progressive_tool_disclosure", {
      sessionId: selected.id,
    })
      .then(setProgressiveToolDisclosure)
      .catch((reason) => setError(errorMessage(reason)));
  }, [selected?.id]);
  useEffect(() => {
    let active = true;
    if (
      !(window as Window & { __TAURI_INTERNALS__?: unknown })
        .__TAURI_INTERNALS__
    ) {
      return () => {
        active = false;
      };
    }
    const subscription = listen<UiEvent>("opcos://event", (event) => {
      const payload = event.payload;
      if (!active) return;
      if (
        payload.kind === "system" &&
        typeof payload.payload.secret_backend === "string"
      ) {
        setSecretBackend(payload.payload.secret_backend);
      }
      if (shouldRefreshForSessionLifecycleEvent(payload)) {
        void refresh().catch(onError);
        return;
      }
      if (payload.session_id && payload.session_id !== selectedIdRef.current)
        return;
      if (payload.kind === "session_list_changed") {
        void refresh().catch(onError);
        return;
      }
      if (payload.kind === "stream") {
        const streamPayload = payload.payload;
        const workingEvent =
          streamPayload.working_event &&
          typeof streamPayload.working_event === "object"
            ? (streamPayload.working_event as Record<string, unknown>)
            : undefined;
        const workingPayload =
          workingEvent?.payload && typeof workingEvent.payload === "object"
            ? (workingEvent.payload as Record<string, unknown>)
            : undefined;
        const requestedSurface = surfaceRequestForWorkingEvent(
          workingEvent,
          ++surfaceRequestId.current,
        );
        if (requestedSurface) {
          setSurfaceRequest({
            sessionId: payload.session_id || "",
            ...requestedSurface,
          });
        }
        if (streamPayload.type === "user_question_answered") {
          const callId =
            typeof workingPayload?.call_id === "string"
              ? workingPayload.call_id
              : "";
          setPendingQuestion((current) =>
            current?.callId === callId ? null : current,
          );
        }
        if (streamPayload.type === "compacted") {
          setRunning(false);
        }
        const hasStreamingContent =
          typeof streamPayload.text_delta === "string" ||
          typeof streamPayload.reasoning_delta === "string" ||
          (streamPayload.tool_call_delta !== null &&
            typeof streamPayload.tool_call_delta === "object") ||
          (streamPayload.turn !== null &&
            typeof streamPayload.turn === "object" &&
            Object.keys(streamPayload.turn).length > 0);
        if (hasStreamingContent) {
          setRunning(true);
          if (payload.session_id) {
            setSessions((items) =>
              updateSessionRunState(
                items,
                payload.session_id!,
                "running",
                "none",
              ),
            );
          }
          if (streamPayload.turn) setRunning(false);
        }
      }
      if (payload.kind === "turn_done") {
        setLiveTranscript([]);
        if (submittingSessionIdRef.current === payload.session_id)
          submittingSessionIdRef.current = undefined;
        const runState =
          typeof payload.payload.run_state === "string"
            ? payload.payload.run_state
            : undefined;
        const stopReason =
          typeof payload.payload.stop_reason === "string"
            ? payload.payload.stop_reason
            : undefined;
        setRunning((previous) =>
          reconcileRunningState(previous, { kind: "turn_done", runState }),
        );
        if (
          payload.session_id &&
          payload.session_id === selectedIdRef.current
        ) {
          void command<
            Array<{
              session_id: string;
              call_id: string;
              tool: string;
              arguments: Record<string, unknown>;
              state: string;
            }>
          >("list_pending", { sessionId: payload.session_id })
            .then((items) => {
              setPendingApprovals(
                items
                  .filter(
                    (item) =>
                      item.tool !== "ask_user" && item.state !== "resolved",
                  )
                  .reduce<Record<string, PendingApproval>>((current, item) => {
                    current[item.call_id] = {
                      callId: item.call_id,
                      name: item.tool,
                      args: item.arguments,
                    };
                    return current;
                  }, {}),
              );
            })
            .catch(onError);
        }
        if (runState || stopReason) {
          setSessions((items) =>
            updateSessionRunState(
              items,
              payload.session_id!,
              runState,
              stopReason,
            ),
          );
          if (runState !== "running") {
            void refresh().catch(onError);
          }
        }
      }
      if (payload.kind === "question_requested") {
        const questionPayload = payload.payload;
        const args =
          questionPayload.arguments &&
          typeof questionPayload.arguments === "object"
            ? (questionPayload.arguments as Record<string, unknown>)
            : {};
        const callId =
          typeof questionPayload.call_id === "string"
            ? questionPayload.call_id
            : "";
        if (callId) {
          setPendingQuestion({
            ...pendingQuestionFromPayload(callId, args),
          });
          setRunning((previous) =>
            reconcileRunningState(previous, {
              kind: "question_requested",
            }),
          );
        }
      }
      if (payload.kind === "approval") {
        const approvalPayload = payload.payload;
        const callId =
          typeof approvalPayload.call_id === "string"
            ? approvalPayload.call_id
            : "";
        const tool =
          typeof approvalPayload.tool === "string"
            ? approvalPayload.tool
            : "tool";
        const args =
          approvalPayload.arguments &&
          typeof approvalPayload.arguments === "object"
            ? (approvalPayload.arguments as Record<string, unknown>)
            : {};
        if (callId && tool !== "ask_user") {
          setPendingApprovals((current) => ({
            ...current,
            [callId]: { callId, name: tool, args },
          }));
        }
      }
      if (payload.kind === "approval_resolved") {
        const callId =
          typeof payload.payload?.call_id === "string"
            ? payload.payload.call_id
            : "";
        setPendingApprovals((current) => {
          const next = { ...current };
          delete next[callId];
          return next;
        });
      }
      if (
        payload.kind === "approval_resolved" ||
        (payload.kind === "notice" &&
          String(payload.payload?.kind) === "approval_pending")
      ) {
        const selectedSessionId = selectedIdRef.current;
        if (selectedSessionId) {
          void command<TimelineEvent[]>("read_session_events", {
            sessionId: selectedSessionId,
          })
            .then((items) =>
              setTranscript((current) => mergeEvents(current, items)),
            )
            .catch(onError);
        }
      }
      if (payload.kind === "coordination_approval_pending") {
        void command<InboxRecord[]>("list_inbox").then(setInbox).catch(onError);
      }
      if (
        payload.kind === "approval_resolved" ||
        (payload.kind === "notice" &&
          String(payload.payload?.kind) === "approval_pending")
      )
        setError("");
      if (
        payload.kind === "notice" &&
        ["interrupted", "error", "approval_pending"].includes(
          String(payload.payload?.kind),
        )
      )
        setRunning(false);
      if (payload.kind === "notice") {
        const noticeText =
          typeof payload.payload?.text === "string"
            ? payload.payload.text
            : typeof payload.payload?.message === "string"
              ? payload.payload.message
              : "";
        if (
          isErrorNotice({
            kind: "notice",
            noticeKind: String(payload.payload?.kind || ""),
            text: noticeText,
          })
        )
          showErrorToast(noticeText);
      }
      if (
        payload.kind === "stream" &&
        typeof payload.payload.event_id === "string" &&
        typeof payload.payload.created_at_ms === "number" &&
        typeof payload.payload.type === "string"
      ) {
        const streamPayload = payload.payload as {
          working_event?: {
            payload?: {
              severity?: unknown;
              summary?: unknown;
            };
          };
        };
        if (
          payload.payload.type === "operational_blocker" &&
          streamPayload.working_event?.payload?.severity === "hard"
        ) {
          const summary =
            typeof streamPayload.working_event?.payload?.summary === "string"
              ? streamPayload.working_event.payload.summary
              : "An operational blocker was reported.";
          showErrorToast(`Hard blocker: ${summary}`);
        }
        setLiveTranscript((items) =>
          mergeEvents(items, payload.payload as unknown as TimelineEvent, true),
        );
      }
    });
    return () => {
      active = false;
      void subscription.then((unlisten) => unlisten());
    };
  }, []);
  const onError = (reason: unknown) => {
    showErrorToast(reason);
  };
  const addHost = async (event: FormEvent) => {
    event.preventDefault();
    try {
      await command("save_host", {
        id: editingHostId,
        name: hostName,
        url: hostUrl,
        token: hostToken,
        vncPassword: hostVncPassword,
      });
      setHostName("");
      setHostUrl("");
      setHostToken("");
      setHostVncPassword("");
      await refresh();
      setEditingHostId(null);
    } catch (reason) {
      onError(submitFailureMessage(reason));
    }
  };
  const editHost = async (host: Host) => {
    const url = await command<string>("host_binding", { hostId: host.id });
    const password = await command<string | null>("vnc_password", {
      hostId: host.id,
    });
    setHostName(host.name);
    setHostUrl(url);
    setHostToken("");
    setHostVncPassword(password || "");
    setEditingHostId(host.id);
  };
  const testHost = async (hostId: string) => {
    try {
      const next = await command<Host>("test_host", { hostId });
      setHosts((items) =>
        items.map((item) => (item.id === next.id ? next : item)),
      );
      if (next.online === false && next.reason) onError(next.reason);
      return next;
    } catch (reason) {
      const message = submitFailureMessage(reason);
      setHosts((items) =>
        items.map((item) =>
          item.id === hostId
            ? { ...item, online: false, reason: message }
            : item,
        ),
      );
      onError(message);
      throw reason;
    }
  };
  const deleteHost = async (hostId: string) => {
    await command("delete_host", { hostId });
    setHosts((items) => items.filter((item) => item.id !== hostId));
  };
  const openNewSessionHome = () => {
    setSelected(null);
    setTranscript([]);
    setRunning(false);
    setSurface("session");
    setHomeInput("");
  };
  const openPromptDraftHome = (draft: string) => {
    setSelected(null);
    setTranscript([]);
    setRunning(false);
    setSurface("session");
    setHomeInput((current) => appendMcpPromptDraft(current, draft));
  };
  const submitHome = async () => {
    const text = homeInput.trim();
    if (!text || !homeHostId || running) return;
    const title =
      text.split(/\r?\n/, 1)[0].trim().slice(0, 80) || "New session";
    let submittedSessionId: string | undefined;
    try {
      setRunning(true);
      const next = normalizeSession(
        await command<Session>("create_session", {
          title,
          hostId: homeHostId,
          model: homeModel || "auto",
          provider: homeProvider || null,
          mode: homeMode,
          harness: homeHarness,
          workspace: homeWorkspace || null,
          systemPrompt:
            [homeRole ? `你的角色是：${homeRole}` : "", homeSystemPrompt]
              .filter(Boolean)
              .join("\n\n") || null,
        }),
      );
      submittedSessionId = next.id;
      setSelected(next);
      setSurface("session");
      setHomeInput("");
      setRestoredComposerDraft(null);
      submittingSessionIdRef.current = next.id;
      setLiveTranscript((items) =>
        mergeEvents(items, optimisticUserMessageEvent(next.id, text), true),
      );
      setRunning(true);
      void refresh().catch(onError);
      let requestText = text;
      if (homeAttachment) {
        const path = await uploadTextAttachmentForSession(
          next.id,
          homeAttachment,
        );
        requestText = `${requestText}\n\n[Attached file: ${path}]`;
        setHomeAttachment(null);
      }
      await command("submit_turn", {
        request: { session_id: next.id, text: requestText },
      });
    } catch (reason) {
      if (
        submittedSessionId &&
        submittingSessionIdRef.current === submittedSessionId
      )
        submittingSessionIdRef.current = undefined;
      setLiveTranscript((items) =>
        items.filter(
          (event) =>
            !submittedSessionId ||
            !optimisticUserMessageMatches(event, submittedSessionId, text),
        ),
      );
      if (submittedSessionId) {
        setRestoredComposerDraft({ text, nonce: Date.now() });
      } else {
        setHomeInput(text);
      }
      setRunning(false);
      onError(submitFailureMessage(reason));
    }
  };
  const approvalPending = useMemo(
    () => buildTimeline(transcript).some((item) => item.kind === "approval"),
    [transcript],
  );
  const toggleAsset = (asset: Asset) => {
    if (!selected) return;
    void command("set_asset_enabled", {
      sessionId: selected.id,
      assetId: asset.id,
      enabled: !asset.enabled,
    })
      .then(() =>
        setAssets((items) =>
          items.map((item) =>
            item.id === asset.id ? { ...item, enabled: !item.enabled } : item,
          ),
        ),
      )
      .catch(onError);
  };
  const submit = async (
    text: string,
    attachments: Attachment[] = [],
  ): Promise<void> => {
    if (!selected) return;
    const sessionId = selected.id;
    submittingSessionIdRef.current = sessionId;
    setLiveTranscript((items) =>
      mergeEvents(
        items,
        optimisticUserMessageEvent(sessionId, text, attachments),
        true,
      ),
    );
    setRunning(true);
    try {
      await command("submit_turn", {
        request: { session_id: sessionId, text, attachments },
      });
    } catch (reason) {
      if (submittingSessionIdRef.current === sessionId)
        submittingSessionIdRef.current = undefined;
      setLiveTranscript((items) =>
        items.filter(
          (event) => !optimisticUserMessageMatches(event, sessionId, text),
        ),
      );
      setRunning(false);
      onError(submitFailureMessage(reason));
      throw new Error(submitFailureMessage(reason));
    }
  };
  const beginSessionTitleEdit = () => {
    if (!selected) return;
    titleEditingRef.current = true;
    setSessionTitleDraft(selected.title);
    setEditingSessionTitle(true);
  };
  const cancelSessionTitleEdit = () => {
    titleEditingRef.current = false;
    setEditingSessionTitle(false);
  };
  const commitSessionTitleEdit = async () => {
    if (!titleEditingRef.current || !selected) return;
    titleEditingRef.current = false;
    setEditingSessionTitle(false);
    const next = sessionTitleDraft.trim();
    if (!next || next === selected.title) return;
    try {
      await command("rename_session", {
        sessionId: selected.id,
        title: next,
      });
      await refresh();
    } catch (reason) {
      onError(reason);
    }
  };
  const uploadTextAttachmentForSession = async (
    sessionId: string,
    file: File,
  ) => {
    return command<string>("upload_text_attachment", {
      sessionId,
      fileName: file.name,
      content: await file.text(),
    });
  };
  const uploadTextAttachment = async (file: File) => {
    if (!selected) throw new Error("Select a session before uploading.");
    return uploadTextAttachmentForSession(selected.id, file);
  };
  const steer = async (text: string) => {
    if (!selected) return;
    await command("steering", { sessionId: selected.id, text });
  };
  const interrupt = async () => {
    if (!selected) return;
    try {
      await command("interrupt", { sessionId: selected.id });
    } catch (reason) {
      onError(reason);
    } finally {
      setRunning(false);
      void refresh().catch(onError);
    }
  };
  const answerPendingQuestion = async (answer: string) => {
    if (!selected || !pendingQuestion) return;
    setRunning(true);
    try {
      await command("resolve_inbox", {
        sessionId: selected.id,
        callId: pendingQuestion.callId,
        resolution: answer,
      });
    } catch (reason) {
      setRunning(false);
      throw reason;
    }
  };
  const readableConversationWidth = 360;
  const baseNavWidth = navCollapsed ? 56 : navWidth;
  const baseSplitterWidth = navCollapsed ? 0 : 6;
  const openDrawerConversationWidth =
    viewportWidth - baseNavWidth - baseSplitterWidth - rightPanelWidth;
  const responsiveDrawerCollapsed =
    !drawerCollapsed && openDrawerConversationWidth < readableConversationWidth;
  const effectiveDrawerCollapsed = drawerCollapsed || responsiveDrawerCollapsed;
  const effectiveDrawerWidth = effectiveDrawerCollapsed ? 44 : rightPanelWidth;
  const drawerYieldConversationWidth =
    viewportWidth - baseNavWidth - baseSplitterWidth - effectiveDrawerWidth;
  const responsiveNavCollapsed =
    !navCollapsed && drawerYieldConversationWidth < readableConversationWidth;
  const effectiveNavCollapsed = navCollapsed || responsiveNavCollapsed;
  return (
    <div
      className={`app ${surface === "session" && selected ? "session-layout" : "surface-layout"}${surface === "session" && selected && effectiveDrawerCollapsed ? " session-drawer-collapsed" : ""}${effectiveNavCollapsed ? " nav-collapsed" : ""}${windowMaximized ? " window-maximized" : ""}`}
      style={
        {
          "--right-panel-width": `${effectiveDrawerCollapsed ? 44 : rightPanelWidth}px`,
          "--nav-panel-width": `${effectiveNavCollapsed ? 56 : navWidth}px`,
        } as CSSProperties
      }
    >
      <Sidebar
        hosts={hosts}
        sessions={sessions.map((session) => ({
          session_id: session.id,
          title: session.title,
          workspace: session.workspace || "",
          agent: "opcos",
          model: session.model,
          mode: session.mode,
          updated_at: null,
          messages: 0,
          pinned: false,
          archived: session.archived ?? false,
          attention: 0,
          liveness:
            selected?.id === session.id && running
              ? "working"
              : session.sleep_state === "asleep"
                ? "sleeping"
                : "idle",
          stop_reason: session.stop_reason,
          project_id: session.project_id,
        }))}
        agent="opcos"
        workspace={selected?.workspace || ""}
        activeSession={selected?.id || ""}
        projectItems={projects}
        onOpenProject={(id) => {
          const project = projects.find((item) => item.id === id);
          if (project) {
            setSelectedProject(project);
            setSurface("project");
          }
        }}
        onCreateProject={() => {
          setProjectDialogOpen(true);
        }}
        selected={selected}
        query={query}
        onQuery={setQuery}
        onSelectSession={(id: string) => {
          const next = sessions.find((item) => item.id === id);
          if (!next) return;
          const project = next.project_id
            ? projects.find((item) => item.id === next.project_id)
            : undefined;
          if (project) setSelectedProject(project);
          setSelected(next);
          setSurface("session");
        }}
        onNew={openNewSessionHome}
        onRenameSession={(id, title) =>
          command("rename_session", { sessionId: id, title })
            .then(() => refresh())
            .catch(onError)
        }
        onTest={(host: Host) =>
          command<Host>("test_host", { hostId: host.id })
            .then((next) =>
              setHosts((items) =>
                items.map((item) => (item.id === next.id ? next : item)),
              ),
            )
            .catch(onError)
        }
        onSurface={setSurface}
        onManage={() => setSurface("manage")}
        onActivity={() => setSurface("activity")}
        onOpenInbox={() => {
          setSurface("inbox");
          void command<InboxRecord[]>("list_inbox")
            .then(setInbox)
            .catch(onError);
        }}
        inboxActive={surface === "inbox"}
        onAutomations={() => setSurface("automations")}
        onAddHost={addHost}
        hostName={hostName}
        setHostName={setHostName}
        hostUrl={hostUrl}
        setHostUrl={setHostUrl}
        hostToken={hostToken}
        setHostToken={setHostToken}
        collapsed={effectiveNavCollapsed}
        onCollapse={toggleNav}
      />
      {surface === "session" && selected && !effectiveNavCollapsed && (
        <LayoutSplitter
          label="Resize left session list"
          value={navWidth}
          collapseThreshold={180}
          maxValue={
            viewportWidth -
            (effectiveDrawerCollapsed ? 44 : rightPanelWidth) -
            6 -
            320
          }
          onCollapse={toggleNav}
          onChange={setNavWidth}
        />
      )}
      <main className="main">
        {surface === "project" && selectedProject ? (
          <ProjectBoard
            project={selectedProject}
            sessions={sessions}
            providers={providers}
            models={models}
            harnessOptions={harnessOptions}
            onRefresh={() => refresh().catch(onError)}
            onOpenSession={(id) => {
              const next = sessions.find((item) => item.id === id);
              if (next) {
                setSelected(next);
                setSurface("session");
              }
            }}
            onError={onError}
            onProjectDeleted={() => {
              setSelectedProject(null);
              setSurface("manage");
            }}
          />
        ) : surface === "session" && selectedId !== null ? (
          selected ? (
            <>
              {/* OpenWorker session topbar: surfaces/gui/src/App.tsx:1365-1442.
                Only the facts and Tauri panel action are adapted to OPCOS. */}
              <header className="main-topbar">
                <div className="main-title">
                  {editingSessionTitle ? (
                    <input
                      className="session-title-input"
                      value={sessionTitleDraft}
                      autoFocus
                      aria-label="Session title"
                      onChange={(event) =>
                        setSessionTitleDraft(event.target.value)
                      }
                      onBlur={() => void commitSessionTitleEdit()}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") {
                          event.preventDefault();
                          void commitSessionTitleEdit();
                        } else if (event.key === "Escape") {
                          event.preventDefault();
                          cancelSessionTitleEdit();
                        }
                      }}
                    />
                  ) : (
                    <button
                      className="main-title-text session-title-trigger"
                      type="button"
                      title={selected.title}
                      onClick={beginSessionTitleEdit}
                    >
                      {selected.title}
                    </button>
                  )}
                  <span className="title-sub" data-testid="session-subtitle">
                    {[
                      selected.host_name,
                      selected.workspace || "workspace not set",
                      selected.model,
                      sessionStatusLabel(
                        selected.run_state,
                        selected.stop_reason,
                        selected.terminal_cause,
                      ),
                    ].join(" · ")}
                    {selected.sleep_state === "asleep" && (
                      <span className="text-xs text-faint">
                        {" · "}
                        {translate("sessionAsleep")}
                      </span>
                    )}
                  </span>
                </div>
                <div className="main-topbar-side main-topbar-actions">
                  {secretBackend && (
                    <span className="backend-badge">
                      Secrets: {secretBackend}
                    </span>
                  )}
                  <button
                    className="topbar-icon-btn"
                    title={translate("Toggle session panel")}
                    onClick={() => setDrawerCollapsed((value) => !value)}
                  >
                    <Icon name="sidebarRight" size={16} />
                  </button>
                </div>
              </header>
              <div className="main-workspace">
                <div className="main-chat">
                  <div className="transcript-viewport">
                    <div
                      className="main-scroll"
                      ref={transcriptScrollRef}
                      onScroll={updateTranscriptScrollState}
                    >
                      <div ref={transcriptBottomRef}>
                        <Transcript
                          events={liveEvents}
                          sessionId={selected.id}
                          hostName={selected.host_name}
                          running={effectiveRunning}
                          onRetry={
                            sessionRecoveryAction(
                              selected.run_state,
                              selected.stop_reason,
                            ) === "restart"
                              ? () =>
                                  void command("restart_session_runtime", {
                                    sessionId: selected.id,
                                  }).catch(onError)
                              : sessionRecoveryAction(
                                    selected.run_state,
                                    selected.stop_reason,
                                  ) === "retry"
                                ? () =>
                                    void command("retry_session", {
                                      sessionId: selected.id,
                                    }).catch(onError)
                                : undefined
                          }
                          retryLabel={
                            sessionRecoveryAction(
                              selected.run_state,
                              selected.stop_reason,
                            ) === "restart"
                              ? "Restart"
                              : "Retry"
                          }
                          onQuestionAnswer={(callId, answer) => {
                            void command("resolve_inbox", {
                              sessionId: selected.id,
                              callId,
                              resolution: answer,
                            }).catch(onError);
                          }}
                        />
                      </div>
                    </div>
                    {showTranscriptJump && (
                      <button
                        className="transcript-jump-bottom"
                        type="button"
                        aria-label="Jump to latest conversation"
                        title="Jump to latest conversation"
                        onClick={jumpToTranscriptBottom}
                      >
                        <svg
                          aria-hidden="true"
                          width="16"
                          height="16"
                          viewBox="0 0 24 24"
                          fill="none"
                        >
                          <path
                            d="M6 9l6 6 6-6"
                            stroke="currentColor"
                            strokeWidth="1.8"
                            strokeLinecap="round"
                            strokeLinejoin="round"
                          />
                        </svg>
                      </button>
                    )}
                  </div>
                  {Object.values(pendingApprovals).map((pendingApproval) => (
                    <div
                      className="transcript-interaction-card"
                      key={pendingApproval.callId}
                    >
                      <ApprovalCard
                        item={{
                          kind: "approval",
                          callId: pendingApproval.callId,
                          name: pendingApproval.name,
                          args: pendingApproval.args,
                          reason: "Tool action requires approval",
                        }}
                        hostName={selected.host_name}
                        onApprove={(decision, optionId) => {
                          void command("resolve_approval", {
                            sessionId: selected.id,
                            callId: pendingApproval.callId,
                            approve: decision === "allow",
                            optionId,
                          }).catch(onError);
                        }}
                      />
                    </div>
                  ))}
                  {pendingQuestion && pendingQuestionCollapsed && (
                    <div className="transcript-question-collapsed">
                      <span className="transcript-question-collapsed-copy">
                        <strong>Question</strong>
                        <span>{pendingQuestion.question}</span>
                      </span>
                      <button
                        className="transcript-question-reopen"
                        type="button"
                        onClick={() => setPendingQuestionCollapsed(false)}
                      >
                        Answer
                      </button>
                    </div>
                  )}
                  <Composer
                    interactionHeader={
                      pendingQuestion && !pendingQuestionCollapsed ? (
                        <QuestionCard
                          question={pendingQuestion}
                          onCollapse={() => setPendingQuestionCollapsed(true)}
                          onAnswer={answerPendingQuestion}
                        />
                      ) : undefined
                    }
                    pendingQuestion={
                      Boolean(pendingQuestion) && !pendingQuestionCollapsed
                    }
                    onPendingQuestionAnswer={
                      pendingQuestion && !pendingQuestionCollapsed
                        ? answerPendingQuestion
                        : undefined
                    }
                    mode={selected.mode}
                    harness={selected.harness}
                    harnessOptions={selectedHarnessOptions}
                    model={selected.model}
                    models={models.map((item) => item.id)}
                    modelLabels={Object.fromEntries(
                      models.map((item) => [item.id, item.label]),
                    )}
                    connected={Boolean(selected)}
                    running={effectiveRunning}
                    workspace={selected.workspace}
                    onModeChange={(mode) => {
                      void command("change_mode", {
                        sessionId: selected.id,
                        mode,
                      })
                        .then(() => setSelected({ ...selected, mode }))
                        .catch(onError);
                    }}
                    onHarnessChange={(harness) => {
                      void command("change_harness", {
                        sessionId: selected.id,
                        harness,
                      })
                        .then(() => setSelected({ ...selected, harness }))
                        .catch(onError);
                    }}
                    unattended={unattended}
                    onUnattendedChange={(on) => {
                      void command("set_unattended", {
                        sessionId: selected.id,
                        unattended: on,
                      })
                        .then(() => setUnattended(on))
                        .catch(onError);
                    }}
                    progressiveToolDisclosure={progressiveToolDisclosure}
                    onProgressiveToolDisclosureChange={(on) => {
                      void command("set_progressive_tool_disclosure", {
                        sessionId: selected.id,
                        enabled: on,
                      })
                        .then(() => setProgressiveToolDisclosure(on))
                        .catch(onError);
                    }}
                    onSend={submit}
                    onSteer={steer}
                    onModelChange={(model) => {
                      void command("change_model", {
                        sessionId: selected.id,
                        model,
                      })
                        .then(() => setSelected({ ...selected, model }))
                        .catch(onError);
                    }}
                    onInterrupt={interrupt}
                    assets={assets}
                    secrets={secrets}
                    slashCommands={
                      selected.harness === "acp"
                        ? (acpCapabilities?.availableCommands || []).map(
                            (item) => ({
                              name: item.name.startsWith("/")
                                ? item.name
                                : `/${item.name}`,
                              body: "",
                              kind: "acp",
                              description: item.description,
                              input: item.input,
                            }),
                          )
                        : slashCommands
                    }
                    acpMode={
                      acpCapabilities
                        ? {
                            currentModeId: acpCapabilities.currentModeId,
                            availableModes: acpCapabilities.availableModes,
                          }
                        : undefined
                    }
                    acpConfigOptions={acpCapabilities?.configOptions}
                    onAcpModeChange={(modeId) =>
                      void command("acp_set_mode", {
                        sessionId: selected.id,
                        modeId,
                      }).catch(onError)
                    }
                    onAcpConfigOptionChange={(configId, value) =>
                      void command("acp_set_config_option", {
                        sessionId: selected.id,
                        configId,
                        value,
                      }).catch(onError)
                    }
                    onUploadFile={uploadTextAttachment}
                    restoreDraft={restoredComposerDraft || undefined}
                    resetKey={`${selected.id}:${pendingQuestion?.callId ?? "none"}`}
                  />
                </div>
              </div>
            </>
          ) : (
            <div className="session-loading">Loading session…</div>
          )
        ) : surface === "manage" ? (
          <SettingsView activeTab={settingsTab} onTabChange={setSettingsTab}>
            <ManageSections
              tab={settingsTab}
              hosts={hosts}
              assets={assets}
              providers={providers}
              secrets={secrets}
              projects={projects}
              selected={selected}
              onRefresh={() => refresh().catch(onError)}
              onError={onError}
              onAddHost={addHost}
              onEditHost={editHost}
              onTestHost={testHost}
              onDeleteHost={deleteHost}
              onPromptDraft={openPromptDraftHome}
              hostName={hostName}
              setHostName={setHostName}
              hostUrl={hostUrl}
              setHostUrl={setHostUrl}
              hostToken={hostToken}
              setHostToken={setHostToken}
              vncPassword={hostVncPassword}
              setVncPassword={setHostVncPassword}
              editingHostId={editingHostId}
              setEditingHostId={setEditingHostId}
            />
          </SettingsView>
        ) : surface === "automations" ? (
          <Automations sessions={sessions} assets={assets} onError={onError} />
        ) : surface === "activity" ? (
          <Activity selected={selected} onError={onError} />
        ) : surface === "inbox" ? (
          <InboxPane
            items={inbox}
            sessions={sessions}
            onOpenSession={(sessionId) => {
              const next = sessions.find((session) => session.id === sessionId);
              if (next) {
                setSelected(next);
                setSurface("session");
              }
            }}
            onResolve={(item, resolution) =>
              command("resolve_inbox", {
                sessionId: item.session_id,
                callId: item.call_id,
                resolution,
              })
                .then(() => command<InboxRecord[]>("list_inbox"))
                .then(setInbox)
                .catch(onError)
            }
          />
        ) : (
          <div className="home">
            <div className="home-inner">
              <div className="home-greeting">OPCOS</div>
              <div className="composer-wrap">
                <div className="composer-card hero">
                  <textarea
                    value={homeInput}
                    placeholder="告诉 OPCOS 你想完成什么…"
                    onChange={(event) => setHomeInput(event.target.value)}
                    onKeyDown={(event) => {
                      if (
                        event.key === "Enter" &&
                        (event.metaKey || event.ctrlKey)
                      ) {
                        event.preventDefault();
                        void submitHome();
                      }
                    }}
                  />
                  {homeAttachment && (
                    <div className="pending-files">
                      <span className="pill att-pill">
                        <span>{homeAttachment.name}</span>
                        <button
                          className="pill-x"
                          type="button"
                          title="移除附件"
                          onClick={() => setHomeAttachment(null)}
                        >
                          ×
                        </button>
                      </span>
                    </div>
                  )}
                  <div className="composer-row">
                    <PlusMenu
                      open={homePlusOpen}
                      onOpenChange={setHomePlusOpen}
                      onInsert={(value) =>
                        setHomeInput((current) =>
                          current.trim()
                            ? `${current.trimEnd()} ${value}`
                            : value,
                        )
                      }
                      assets={assets}
                      secrets={secrets}
                      onUpload={(file) => {
                        setHomeAttachment(file);
                        setHomePlusOpen(false);
                      }}
                    />
                    <select
                      className="chip"
                      title="Agent 模板"
                      value={homeAgentTemplateId}
                      onChange={(event) => {
                        const id = event.target.value;
                        setHomeAgentTemplateId(id);
                        const template = homeAgentTemplates.find(
                          (item) => item.id === id,
                        );
                        if (!template) {
                          setHomeRole("");
                          setHomeSystemPrompt("");
                          return;
                        }
                        try {
                          const content = JSON.parse(template.content) as {
                            role?: string;
                            provider?: string;
                            model?: string;
                            harness?: string;
                            mode?: string;
                            system_prompt?: string;
                          };
                          setHomeRole(content.role || "");
                          setHomeProvider(content.provider || "");
                          setHomeModel(
                            content.model && content.model !== "auto"
                              ? content.model
                              : (models.find((model) => !model.likely_non_chat)
                                  ?.id ??
                                  models[0]?.id ??
                                  "auto"),
                          );
                          setHomeHarness(content.harness || "builtin");
                          setHomeMode(content.mode || "Auto");
                          setHomeSystemPrompt(content.system_prompt || "");
                        } catch {
                          onError("Agent 模板内容不是有效 JSON");
                        }
                      }}
                    >
                      <option value="">Agent 模板</option>
                      {homeAgentTemplates.map((template) => (
                        <option key={template.id} value={template.id}>
                          {template.name}
                        </option>
                      ))}
                    </select>
                    <input
                      className="chip"
                      title="角色"
                      value={homeRole}
                      onChange={(event) => setHomeRole(event.target.value)}
                      placeholder="Role"
                    />
                    <select
                      className="chip"
                      title="Harness"
                      value={homeHarness}
                      onChange={(event) => setHomeHarness(event.target.value)}
                    >
                      {(harnessOptions.length
                        ? harnessOptions
                        : [{ id: "builtin", label: "Builtin", available: true }]
                      ).map((option) => (
                        <option
                          key={option.id}
                          value={option.id}
                          disabled={!option.available}
                        >
                          {option.label}
                          {!option.available ? " (unavailable)" : ""}
                        </option>
                      ))}
                    </select>
                    <select
                      className="chip"
                      title="绑定主机"
                      value={homeHostId}
                      onChange={(event) => setHomeHostId(event.target.value)}
                    >
                      <option value="">未选择</option>
                      {hosts.map((host) => (
                        <option key={host.id} value={host.id}>
                          {host.name}
                        </option>
                      ))}
                    </select>
                    <select
                      className="chip"
                      title="Provider"
                      value={homeProvider}
                      onChange={(event) => setHomeProvider(event.target.value)}
                    >
                      <option value="">默认</option>
                      {providers.map((provider) => (
                        <option
                          key={provider.name}
                          value={provider.name}
                          disabled={provider.available === false}
                        >
                          {provider.title}
                          {provider.available === false ? " (unavailable)" : ""}
                        </option>
                      ))}
                    </select>
                    <select
                      className="chip"
                      title="模型"
                      value={homeModel}
                      onChange={(event) => setHomeModel(event.target.value)}
                    >
                      <option value="auto">Auto</option>
                      {models
                        .filter(
                          (model) =>
                            showAllHomeModels || !model.likely_non_chat,
                        )
                        .map((model) => (
                          <option key={model.id} value={model.id}>
                            {model.label}
                            {model.likely_non_chat ? " (非对话模型)" : ""}
                          </option>
                        ))}
                    </select>
                    {models.some((model) => model.likely_non_chat) && (
                      <button
                        className="chip"
                        type="button"
                        onClick={() => setShowAllHomeModels((value) => !value)}
                      >
                        {showAllHomeModels ? "收起非对话" : "显示全部模型"}
                      </button>
                    )}
                    <select
                      className="chip"
                      title="模式"
                      value={homeMode}
                      onChange={(event) => setHomeMode(event.target.value)}
                    >
                      <option value="Interactive">Interactive</option>
                      <option value="Auto">Auto</option>
                    </select>
                    <input
                      className="chip workspace-chip"
                      title="留空时使用默认本地 workspace"
                      value={homeWorkspace}
                      onChange={(event) => setHomeWorkspace(event.target.value)}
                      placeholder="Workspace (默认 ~/OPCOS/workspaces/<id>)"
                    />
                    <span className="spacer" />
                    <SendButton
                      running={effectiveRunning}
                      disabled={!homeInput.trim() || !homeHostId}
                      onSend={() => void submitHome()}
                      onInterrupt={() => undefined}
                      title="发送并创建会话"
                    />
                  </div>
                </div>
              </div>
              {sessions.length > 0 && (
                <div className="recent">
                  <div className="recent-head">最近会话</div>
                  {sessions.slice(0, 8).map((item) => (
                    <button
                      className="recent-item"
                      key={item.id}
                      onClick={() => {
                        setSelected(item);
                        setSurface("session");
                      }}
                    >
                      <span className="recent-dot" />
                      <span className="recent-copy">
                        <strong>{item.title}</strong>
                        <small>
                          {item.host_name} · {item.model}
                        </small>
                      </span>
                    </button>
                  ))}
                </div>
              )}
            </div>
          </div>
        )}
        {error && (
          <div className="error-toast" role="alert" data-testid="error-toast">
            {error}
            <button
              aria-label="Dismiss notification"
              onClick={() => {
                setError("");
                if (errorTimer.current !== undefined)
                  window.clearTimeout(errorTimer.current);
                errorTimer.current = undefined;
              }}
            >
              ×
            </button>
          </div>
        )}
      </main>
      {projectDialogOpen && (
        <ProjectDialog
          hosts={hosts}
          onClose={() => setProjectDialogOpen(false)}
          onSubmit={async (values) => {
            const project = values.teamTemplateId
              ? await command<Project>("create_project_from_team_template", {
                  teamTemplateId: values.teamTemplateId,
                  name: values.name,
                  hostId: values.hostId,
                  repoUrl: values.repoUrl || null,
                  repoRoot: values.repoRoot || null,
                  defaultBranch: values.defaultBranch,
                })
              : await command<Project>("create_project", {
                  name: values.name,
                  hostId: values.hostId,
                  repoUrl: values.repoUrl || null,
                  repoRoot: values.repoRoot || null,
                  defaultBranch: values.defaultBranch,
                });
            await refresh();
            setSelectedProject(project);
            setProjectDialogOpen(false);
            setSurface("project");
          }}
        />
      )}
      {surface === "session" && selected && (
        <SessionRightPanel
          selected={selected}
          transcript={transcript}
          running={effectiveRunning}
          eventRefreshKey={`${transcript.length}:${transcript.at(-1)?.event_id ?? ""}`}
          collapsed={effectiveDrawerCollapsed}
          providers={providers}
          onProviderChange={(provider) =>
            command("change_provider", {
              sessionId: selected.id,
              provider: provider || null,
            })
              .then(() => setSelected({ ...selected, provider }))
              .catch(onError)
          }
          onError={onError}
          focusTabRequest={
            surfaceRequest?.sessionId === selected.id ? surfaceRequest : null
          }
          onOpenSession={(session) => {
            setSelected(session);
            setSurface("session");
          }}
          onCollapsedChange={setDrawerCollapsed}
          width={rightPanelWidth}
          onWidthChange={setRightPanelWidth}
          maxWidth={
            viewportWidth -
            (effectiveNavCollapsed ? 56 : navWidth) -
            (effectiveNavCollapsed ? 0 : 6) -
            320
          }
          onCollapse={() => setDrawerCollapsed(true)}
        />
      )}
    </div>
  );
}

export function App() {
  const route = paneRoute();
  return (
    <AppErrorBoundary>
      {route ? <StandalonePane route={route} /> : <AppContent />}
    </AppErrorBoundary>
  );
}
