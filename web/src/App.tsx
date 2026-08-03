import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  Component,
  FormEvent,
  ReactNode,
  useEffect,
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
  SurfaceTab,
  hostFailureMessage,
  hostStatusLabel,
  errorMessage,
  redactApproval,
  submitFailureMessage,
} from "./gui";
import {
  TranscriptViewItem,
  normalizeTranscript,
  reduceStreamEvent,
} from "./transcript";
import { Sidebar } from "./components/Sidebar";
import { sessionStatusLabel } from "./sessionStatus";
import { Transcript } from "./components/Transcript";
import { ApprovalCard, PreviewBlock } from "./components/ApprovalCard";
import { Composer, PlusMenu, SendButton } from "./components/Composer";
import { SelectMenu as OpenWorkerSelectMenu } from "./components/SelectMenu";
import { SettingsView, type SettingsSection } from "./components/SettingsView";
import { Icon } from "./components/Icon";
import type { Item } from "./types";
import { CollectionPage } from "./components/CollectionPage";
import { IntegrationCard } from "./components/IntegrationCard";
import { getLocale, setLocale, subscribeLocale, translate } from "./i18n";
import "./openworker-tailwind.css";
import "./openworker-styles.css";
import "./style.css";

type UiEvent = {
  kind: string;
  session_id?: string;
  payload: Record<string, unknown>;
};
type ProviderDescriptor = {
  name: string;
  title: string;
  available?: boolean;
  needs_key?: boolean;
  default_base_url?: string | null;
  recommended_model?: string | null;
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
  {
    name: "Devin",
    description: "Import Devin Knowledge and Playbooks and connect Devin MCP.",
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
  }) => Promise<void>;
}) {
  const [name, setName] = useState("");
  const [hostId, setHostId] = useState(hosts[0]?.id || "");
  const [repoUrl, setRepoUrl] = useState("");
  const [repoRoot, setRepoRoot] = useState("");
  const [defaultBranch, setDefaultBranch] = useState("main");
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!name.trim() || !hostId) {
      setError("项目名称和主机不能为空");
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
      });
    } catch (reason) {
      setError(errorMessage(reason));
      setSaving(false);
    }
  };
  return (
    <div className="fixed inset-0 z-50 grid place-items-center bg-black/30 p-4">
      <form
        className="w-full max-w-lg rounded-xl border border-line bg-panel p-6 shadow-xl"
        onSubmit={submit}
      >
        <div className="flex items-center justify-between">
          <h2 className="text-lg font-semibold text-ink">新建项目</h2>
          <button type="button" className="btn" onClick={onClose}>
            关闭
          </button>
        </div>
        <div className="mt-5 grid gap-3">
          <label className="field-label">
            名称
            <input
              autoFocus
              className="input"
              value={name}
              onChange={(event) => setName(event.target.value)}
              placeholder="项目名称"
            />
          </label>
          <label className="field-label">
            主机
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
            仓库 URL（可留空）
            <input
              className="input"
              value={repoUrl}
              onChange={(event) => setRepoUrl(event.target.value)}
              placeholder="https://github.com/org/repo.git"
            />
          </label>
          <label className="field-label">
            仓库路径（可留空）
            <input
              className="input"
              value={repoRoot}
              onChange={(event) => setRepoRoot(event.target.value)}
              placeholder="按后端默认路径"
            />
          </label>
          <label className="field-label">
            默认分支
            <input
              className="input"
              value={defaultBranch}
              onChange={(event) => setDefaultBranch(event.target.value)}
            />
          </label>
        </div>
        {error && <p className="mt-3 text-sm text-danger">{error}</p>}
        <div className="mt-6 flex justify-end gap-2">
          <button type="button" className="btn" onClick={onClose}>
            取消
          </button>
          <button
            type="submit"
            className="btn approval-primary"
            disabled={saving}
          >
            {saving ? "创建中…" : "创建项目"}
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
  const [sessionMode, setSessionMode] = useState(agent?.mode || "Interactive");
  const [branch, setBranch] = useState(agent?.branch || "");
  const [state, setState] = useState(agent?.state || "Active");
  const [error, setError] = useState("");
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!name.trim() || !role.trim()) {
      setError("成员名称和角色不能为空");
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
            {mode === "add" ? "添加成员" : "编辑成员"}
          </h2>
          <button type="button" className="btn" onClick={onClose}>
            关闭
          </button>
        </div>
        <div className="mt-5 grid gap-3">
          <label className="field-label">
            名称
            <input
              autoFocus
              className="input"
              value={name}
              onChange={(event) => setName(event.target.value)}
            />
          </label>
          <label className="field-label">
            角色
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
                  <option value="">默认</option>
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
                Model
                <select
                  className="input"
                  value={model}
                  onChange={(event) => setModel(event.target.value)}
                >
                  <option value="auto">Auto</option>
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
                Mode
                <select
                  className="input"
                  value={sessionMode}
                  onChange={(event) => setSessionMode(event.target.value)}
                >
                  <option value="Interactive">Interactive</option>
                  <option value="Auto">Auto</option>
                </select>
              </label>
              <label className="field-label">
                分支（可留空）
                <input
                  className="input"
                  value={branch}
                  onChange={(event) => setBranch(event.target.value)}
                  placeholder="按角色自动命名"
                />
              </label>
            </>
          ) : (
            <label className="field-label">
              状态
              <select
                className="input"
                value={state}
                onChange={(event) => setState(event.target.value)}
              >
                <option value="Active">Active</option>
                <option value="Sleep">Sleep</option>
                <option value="Paused">Paused</option>
              </select>
            </label>
          )}
        </div>
        {error && <p className="mt-3 text-sm text-danger">{error}</p>}
        <div className="mt-6 flex justify-end gap-2">
          <button type="button" className="btn" onClick={onClose}>
            取消
          </button>
          <button
            type="submit"
            className="btn approval-primary"
            disabled={saving}
          >
            {saving ? "保存中…" : "保存"}
          </button>
        </div>
      </form>
    </div>
  );
}

function ProjectConfigPanel({
  project,
  onError,
}: {
  project: Project;
  onError: (error: unknown) => void;
}) {
  const [assets, setAssets] = useState<Asset[]>([]);
  const [secrets, setSecrets] = useState<SecretMetadata[]>([]);
  const [kind, setKind] = useState<
    "agents" | "knowledge" | "playbook" | "mcp" | "connectors" | "blueprint"
  >("agents");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [editingId, setEditingId] = useState<string | null>(null);
  const [secretName, setSecretName] = useState("");
  const [secretPurpose, setSecretPurpose] = useState("");
  const [secretValue, setSecretValue] = useState("");
  const [secretFormOpen, setSecretFormOpen] = useState(false);
  const load = async () => {
    const [nextAssets, nextSecrets] = await Promise.all([
      command<Asset[]>("list_assets", { projectId: project.id }),
      command<SecretMetadata[]>("list_secret_metadata", {
        projectId: project.id,
      }),
    ]);
    setAssets(nextAssets);
    setSecrets(nextSecrets);
  };
  useEffect(() => {
    void load().catch(onError);
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
      .then(reset)
      .catch(onError);
  };
  return (
    <section className="mt-8 rounded-xl border border-line bg-panel p-5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div>
          <h2 className="text-lg font-semibold text-ink">项目配置</h2>
          <p className="mt-1 text-sm text-faint">
            项目配置会继承到项目成员会话，全局配置仍可回退使用。
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          {[
            ["agents", "规则"],
            ["knowledge", "Knowledge"],
            ["playbook", "Playbook"],
            ["mcp", "MCP"],
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
        {assets
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
                  编辑
                </button>
                <button
                  className="btn"
                  onClick={() =>
                    command("delete_asset", { id: asset.id })
                      .then(load)
                      .catch(onError)
                  }
                >
                  删除
                </button>
              </div>
            </div>
          ))}
        <div className="grid gap-3 rounded-lg border border-line p-4">
          <label className="field-label">
            名称
            <input
              value={title}
              onChange={(event) => setTitle(event.target.value)}
              placeholder="配置名称"
            />
          </label>
          <label className="field-label">
            内容
            <textarea
              value={body}
              onChange={(event) => setBody(event.target.value)}
              placeholder={
                kind === "blueprint"
                  ? "clone:\n  - git fetch"
                  : "项目级配置内容"
              }
            />
          </label>
          <div>
            <button className="btn approval-primary" onClick={save}>
              {editingId ? "保存更改" : "新增配置"}
            </button>
            {editingId && (
              <button className="btn ml-2" onClick={reset}>
                取消编辑
              </button>
            )}
          </div>
        </div>
      </div>
      <div className="mt-6 border-t border-line pt-5">
        <div className="flex items-center justify-between">
          <div>
            <h3 className="font-medium text-ink">项目 Secrets</h3>
            <p className="mt-1 text-xs text-faint">
              项目 Secret 优先于全局同名 Secret，值不会显示。
            </p>
          </div>
          <button
            className="btn"
            onClick={() => setSecretFormOpen((value) => !value)}
          >
            {secretFormOpen ? "取消" : "新增 Secret"}
          </button>
        </div>
        {secretFormOpen && (
          <div className="mt-3 grid gap-3 rounded-lg border border-line p-4">
            <input
              value={secretName}
              onChange={(event) => setSecretName(event.target.value)}
              placeholder="Secret 名称"
            />
            <input
              value={secretPurpose}
              onChange={(event) => setSecretPurpose(event.target.value)}
              placeholder="用途"
            />
            <input
              type="password"
              value={secretValue}
              onChange={(event) => setSecretValue(event.target.value)}
              placeholder="Secret 值"
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
                  .then(() => {
                    setSecretName("");
                    setSecretPurpose("");
                    setSecretValue("");
                    setSecretFormOpen(false);
                  })
                  .catch(onError)
              }
            >
              保存 Secret
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
                      .catch(onError)
                  }
                >
                  删除
                </button>
              )}
            </div>
          ))}
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
}: {
  project: Project;
  sessions: Session[];
  providers: ProviderDescriptor[];
  models: Array<{ id: string; label: string }>;
  harnessOptions: HarnessOption[];
  onRefresh: () => Promise<void>;
  onOpenSession: (id: string) => void;
  onError: (error: unknown) => void;
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
      await command("delete_project_agent", {
        agentId: agent.id,
        force,
      });
      setDeleteError(null);
      await onRefresh();
    } catch (reason) {
      setDeleteError({ agentId: agent.id, message: errorMessage(reason) });
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
        </div>
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
        <ProjectConfigPanel project={project} onError={onError} />
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
}: {
  tab: SurfaceTab | "pr";
  selected: Session;
  onError: (error: unknown) => void;
}) {
  const terminalHost = useRef<HTMLDivElement>(null);
  const vncHost = useRef<HTMLDivElement>(null);
  const [port, setPort] = useState<number | null>(null);
  const [idePort, setIdePort] = useState<number | null>(null);
  const [review, setReview] = useState<Record<string, unknown> | null>(null);
  const [diff, setDiff] = useState<Record<string, unknown> | null>(null);
  const [worklog, setWorklog] = useState<Record<string, unknown> | null>(null);
  const [busy, setBusy] = useState(false);
  const [ideError, setIdeError] = useState("");
  const start = async (surface: string) => {
    try {
      setBusy(true);
      setPort(
        await command<number>("start_surface", {
          hostId: selected.host_id,
          surface,
          cols: 100,
          rows: 30,
          cwd: selected.workspace || null,
        }),
      );
    } catch (error) {
      onError(error);
    } finally {
      setBusy(false);
    }
  };
  useEffect(() => {
    setPort(null);
    setIdePort(null);
    setReview(null);
    setDiff(null);
    setWorklog(null);
    setIdeError("");
  }, [selected.id]);
  useEffect(() => {
    if (tab === "terminal" || tab === "desktop" || tab === "browser") {
      if (!port) {
        void start(
          tab === "terminal" ? "pty" : tab === "desktop" ? "vnc" : "cdp",
        );
      }
    } else if (tab === "ide" && !idePort && !ideError) {
      setBusy(true);
      void command<number>("start_ide_proxy", {
        sessionId: selected.id,
        folderUri: `vscode-remote://${selected.host_name}/${selected.workspace || "workspace"}`,
      })
        .then(setIdePort)
        .catch((error) => {
          setIdeError(errorMessage(error));
          onError(error);
        })
        .finally(() => setBusy(false));
    }
  }, [
    tab,
    selected.id,
    selected.host_id,
    selected.host_name,
    selected.workspace,
    port,
    idePort,
    ideError,
  ]);
  useEffect(() => {
    if (tab !== "terminal" || !port || !terminalHost.current) return;
    const terminal = new Terminal({
      convertEol: false,
      cursorBlink: true,
      theme: { background: "#11151d", foreground: "#d7dbe5" },
    });
    terminal.open(terminalHost.current);
    const socket = new WebSocket(`ws://127.0.0.1:${port}`);
    socket.binaryType = "arraybuffer";
    const pending: Array<string | Uint8Array> = [];
    const send = (data: string | Uint8Array) => {
      if (socket.readyState === WebSocket.OPEN) socket.send(data);
      else pending.push(data);
    };
    socket.onopen = () => {
      while (pending.length) socket.send(pending.shift()!);
    };
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
    const input = terminal.onData((data) => send(encoder.encode(data)));
    terminal.onResize(({ cols, rows }) =>
      send(JSON.stringify({ type: "resize", cols, rows })),
    );
    const observer = new ResizeObserver(resize);
    observer.observe(terminalHost.current);
    requestAnimationFrame(resize);
    return () => {
      observer.disconnect();
      input.dispose();
      socket.close();
      terminal.dispose();
    };
  }, [selected.id, port]);
  useEffect(() => {
    if (!port || !vncHost.current) return;
    const rfb = new RFB(vncHost.current, `ws://127.0.0.1:${port}`);
    rfb.scaleViewport = true;
    return () => rfb.disconnect();
  }, [selected.id, port]);
  if (tab === "terminal" || tab === "desktop" || tab === "browser")
    return (
      <div className="surface-panel">
        {busy && (
          <div className="surface-status muted">
            Connecting to the bound remote host…
          </div>
        )}
        {!busy && !port && (
          <div className="surface-status warning">
            Remote surface unavailable.
          </div>
        )}
        {tab === "terminal" && (
          <div className="terminal-host" ref={terminalHost} />
        )}
        {tab === "desktop" && <div className="vnc-host" ref={vncHost} />}
        {tab === "browser" && (
          <div className="empty-surface">
            <Icon name="image" size={32} />
            <p>
              CDP relay started on demand. Connect a browser client to{" "}
              <code>ws://127.0.0.1:{port || "…"}</code>.
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
            Connecting to the bound remote host…
          </div>
        )}
        {idePort && !ideError ? (
          <iframe
            title={translate("Remote Web IDE")}
            src={`http://127.0.0.1:${idePort}/`}
            className="ide-frame"
            onLoad={(event) => {
              const frame = event.currentTarget;
              try {
                const body = frame.contentDocument?.body?.textContent || "";
                if (
                  /bad gateway|upstream|forbidden|not available/i.test(body)
                ) {
                  setIdeError(
                    "Remote Web IDE bootstrap succeeded, but the bound host rejected its workbench assets.",
                  );
                }
              } catch {
                // Cross-origin frames cannot be inspected; browser errors remain visible.
              }
            }}
          />
        ) : ideError ? (
          <div className="empty-surface ide-error">
            <Icon name="code" size={32} />
            <p>{ideError}</p>
            <p className="muted">
              The host Web IDE is not authorized or not running. No local
              fallback is used.
            </p>
          </div>
        ) : (
          <div className="empty-surface">
            <Icon name="code" size={32} />
            <p>{translate("Start the remote IDE for this bound session.")}</p>
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
  const changes = Array.isArray(review?.changes) ? review.changes : [];
  if (!selected.workspace) {
    return (
      <div className="surface-panel">
        <div className="warning">
          This session has no workspace configured. Review is unavailable until
          the session is recreated with a remote workspace.
        </div>
      </div>
    );
  }
  return (
    <div className="surface-panel review-panel">
      <div className="surface-toolbar">
        <span>{translate("Remote review")}</span>
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
            Refresh
          </Button>
        </div>
      </div>
      {review ? (
        <div className="review-grid">
          <div>
            <h3>{translate("Changed files")}</h3>
            {changes.map((change) => {
              const value =
                typeof change === "string" ? change : JSON.stringify(change);
              return (
                <button
                  className="file-row"
                  key={value}
                  onClick={() =>
                    command<Record<string, unknown>>("review_file_diff", {
                      sessionId: selected.id,
                      cwd,
                      path: value,
                      base,
                    })
                      .then(setDiff)
                      .catch(onError)
                  }
                >
                  {value}
                </button>
              );
            })}
          </div>
          <DiffView diff={diff} />
        </div>
      ) : (
        <div className="empty-surface">
          <p>
            {translate(
              "Load the remote status and changes from the bound host.",
            )}
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
        <span>Git workflow and pull requests</span>
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
  const [lifecycleResult, setLifecycleResult] = useState<unknown>(null);
  return (
    <div className="git-actions">
      <h3>{translate("Git workflow")}</h3>
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
        Run {operation}
      </Button>
      {lifecycleResult ? (
        <pre className="code-block">
          {JSON.stringify(lifecycleResult, null, 2)}
        </pre>
      ) : null}
      <details>
        <summary>{translate("Create GitHub PR")}</summary>
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
          Create PR
        </Button>
        <p className="muted small">
          The configured GitHub secret is read only by Rust; it is never
          displayed.
        </p>
      </details>
    </div>
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
  selected,
  onRefresh,
  onError,
  onAddHost,
  onEditHost,
  onTestHost,
  onDeleteHost,
  hostName,
  setHostName,
  hostUrl,
  setHostUrl,
  hostToken,
  setHostToken,
  editingHostId,
  setEditingHostId,
}: {
  tab: SettingsSection;
  hosts: Host[];
  assets: Asset[];
  providers: ProviderDescriptor[];
  secrets: SecretMetadata[];
  selected: Session | null;
  onRefresh: () => void;
  onError: (error: unknown) => void;
  onAddHost: (event: FormEvent) => void;
  onEditHost: (host: Host) => Promise<void>;
  onTestHost: (hostId: string) => Promise<Host>;
  onDeleteHost: (hostId: string) => Promise<void>;
  hostName: string;
  setHostName: (value: string) => void;
  hostUrl: string;
  setHostUrl: (value: string) => void;
  hostToken: string;
  setHostToken: (value: string) => void;
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
    Array<{ provider: string; base_url?: string; configured: boolean }>
  >([]);
  const [providerKeys, setProviderKeys] = useState<Record<string, string>>({});
  const [providerStatuses, setProviderStatuses] = useState<
    Record<string, string>
  >({});
  const [providerModelOptions, setProviderModelOptions] = useState<
    Record<string, Array<{ id: string; label: string }>>
  >({});
  const [providerModels, setProviderModels] = useState<Record<string, string>>(
    {},
  );
  const [selectedProvider, setSelectedProvider] = useState<string | null>(null);
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
  const [devinKey, setDevinKey] = useState("");
  const [devinKeyConfigured, setDevinKeyConfigured] = useState(false);
  const [devinKeyStatus, setDevinKeyStatus] = useState("");
  const [devinAssetItems, setDevinAssetItems] = useState<
    Array<Record<string, unknown>>
  >([]);
  const [devinAssetOpen, setDevinAssetOpen] = useState(false);
  const [devinAssetLoading, setDevinAssetLoading] = useState(false);
  const [devinAssetImporting, setDevinAssetImporting] = useState<string | null>(
    null,
  );
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
    void command<Record<string, unknown>>("devin_integration_status")
      .then((status) => setDevinKeyConfigured(status.configured === true))
      .catch(onError);
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
  const assetLabel =
    assetTabKind === "agents"
      ? "规则"
      : assetTabKind[0].toUpperCase() + assetTabKind.slice(1);
  const loadDevinAssets = () => {
    const kind = assetTabKind === "playbook" ? "playbooks" : "knowledge";
    setDevinAssetLoading(true);
    void command<Array<Record<string, unknown>>>(
      kind === "playbooks" ? "devin_playbooks_list" : "devin_knowledge_list",
    )
      .then((items) => {
        setDevinAssetItems(items);
        setDevinAssetOpen(true);
      })
      .catch(onError)
      .finally(() => setDevinAssetLoading(false));
  };
  const importDevinAsset = (item: Record<string, unknown>) => {
    const sourceId = String(item.id || Date.now());
    const kind = assetTabKind;
    setDevinAssetImporting(sourceId);
    void command("save_asset", {
      id: `devin-${kind}-${sourceId}`,
      kind,
      title: String(item.title || item.name || "Devin asset"),
      body: String(item.body || ""),
      trigger: null,
      scope: null,
      scopeKind: "global",
      enabled: true,
    })
      .then(onRefresh)
      .catch(onError)
      .finally(() => setDevinAssetImporting(null));
  };
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
            await command<Array<{ id: string; label: string }>>(
              "provider_models",
              {
                provider: item.name,
              },
            ),
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
          tab === "appearance" || tab === "provider" || tab === "blueprint"
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
        {tab === "provider" &&
          (selectedProvider === null ? (
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
                    onClick={() =>
                      descriptor.available !== false &&
                      setSelectedProvider(descriptor.name)
                    }
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
                      Base URL
                      <input
                        type="url"
                        value={currentUrl}
                        onChange={(event) =>
                          setProviderConfigs((items) => {
                            const found = items.some(
                              (item) => item.provider === descriptor.name,
                            );
                            return found
                              ? items.map((item) =>
                                  item.provider === descriptor.name
                                    ? { ...item, base_url: event.target.value }
                                    : item,
                                )
                              : [
                                  ...items,
                                  {
                                    provider: descriptor.name,
                                    base_url: event.target.value,
                                    configured: Boolean(config?.configured),
                                  },
                                ];
                          })
                        }
                      />
                    </label>
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
                          providerModelOptions[descriptor.name] || []
                        ).map((item) => ({
                          value: item.id,
                          label: item.label,
                        }))}
                      />
                    </label>
                  </div>
                  <div className="flex items-center gap-2 mt-4">
                    <Button
                      className="primary"
                      onClick={() =>
                        command("save_provider_settings", {
                          provider: descriptor.name,
                          baseUrl: currentUrl || null,
                        })
                          .then(() =>
                            providerKeys[descriptor.name]
                              ? command("save_provider_key", {
                                  provider: descriptor.name,
                                  key: providerKeys[descriptor.name],
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
                <strong>{translate("Provider")}</strong>
                <small>
                  {translate("Choose the model provider for new sessions.")}
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
                <strong>{translate("Base URL")}</strong>
                <small>
                  {translate("Optional provider-compatible endpoint.")}
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
                <strong>{translate("Provider key")}</strong>
                <small>
                  {translate("Stored securely and never returned to the UI.")}
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
                    .then(() => command("save_provider_key", { provider, key }))
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
                          <span className="status-offline">Offline</span>
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
                      placeholder="输入全局指令内容"
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
                        <strong>版本历史</strong>
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
        {assetKinds.includes(tab as (typeof assetKinds)[number]) && (
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
                          disabled={devinAssetLoading}
                          onClick={loadDevinAssets}
                        >
                          {devinAssetLoading
                            ? "Loading Devin…"
                            : "从 Devin 导入"}
                        </Button>
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
            {devinAssetOpen && (tab === "knowledge" || tab === "playbook") && (
              <div className="manage-card mt-4">
                <div className="flex items-center justify-between">
                  <strong>
                    Devin{" "}
                    {assetTabKind === "playbook" ? "Playbooks" : "Knowledge"}
                  </strong>
                  <Button
                    className="bordered"
                    onClick={() => {
                      setDevinAssetOpen(false);
                      setDevinAssetItems([]);
                    }}
                  >
                    Close
                  </Button>
                </div>
                {devinAssetItems.map((item) => {
                  const itemId = String(item.id);
                  return (
                    <div className="manage-row mt-2" key={itemId}>
                      <span>
                        <strong>
                          {String(item.title || item.name || itemId)}
                        </strong>
                        <small>{String(item.body || "").slice(0, 160)}</small>
                      </span>
                      <Button
                        className="bordered"
                        disabled={devinAssetImporting === itemId}
                        onClick={() => importDevinAsset(item)}
                      >
                        {devinAssetImporting === itemId
                          ? "Importing…"
                          : "Import"}
                      </Button>
                    </div>
                  );
                })}
                {!devinAssetItems.length && (
                  <p className="px-4 py-6 text-[13px] text-muted">
                    No Devin assets found.
                  </p>
                )}
              </div>
            )}
            {versionHistoryAsset && (
              <div className="manage-card mt-4">
                <div className="flex items-center justify-between">
                  <strong>Version history</strong>
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
                      <option value="global">Global</option>
                      <option value="repo">Repository</option>
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
        {tab === "mcp" && <McpManage selected={selected} onError={onError} />}
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
                      configurable ||
                      connector.name === "Linear" ||
                      connector.name === "Devin";
                    const tokenStatus = connectorStatuses[connectorKind];
                    const status = configurable
                      ? tokenStatus?.connected
                        ? `Connected as ${tokenStatus.identity || "bot"}`
                        : "Configurable"
                      : connector.name === "Devin"
                        ? devinKeyConfigured
                          ? "Connected"
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
                Devin integrations
              </h2>
              <p className="muted mt-1">
                Store a Devin API key securely to import Knowledge and Playbooks
                and connect Devin MCP.
              </p>
              <div className="form-grid mt-4">
                <label className="field-label">
                  Devin API key
                  <input
                    type="password"
                    value={devinKey}
                    onChange={(event) => setDevinKey(event.target.value)}
                    placeholder={
                      devinKeyConfigured ? "Stored securely" : "devin_…"
                    }
                  />
                </label>
                <div className="flex gap-2 items-end">
                  <Button
                    className="primary"
                    onClick={() =>
                      command("devin_integration_save", { apiKey: devinKey })
                        .then(() => {
                          setDevinKey("");
                          setDevinKeyConfigured(true);
                          setDevinKeyStatus("Devin API key saved securely.");
                        })
                        .catch((error) => {
                          setDevinKeyStatus(String(error));
                          onError(error);
                        })
                    }
                  >
                    Save key
                  </Button>
                </div>
              </div>
              {devinKeyStatus && (
                <div
                  className={
                    devinKeyStatus.includes("failed") ||
                    devinKeyStatus.includes("cannot")
                      ? "failure mt-3"
                      : "success mt-3"
                  }
                >
                  {devinKeyStatus}
                </div>
              )}
              <div className="muted mt-3">
                {devinKeyConfigured
                  ? "Configured securely; the key value is not displayed."
                  : "Not configured."}
              </div>
            </div>
            <div className="rounded-xl2 border border-line bg-panel p-5">
              <h2 className="text-[15px] font-semibold text-ink">Linear</h2>
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
                    placeholder="lin_api_…"
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
                  placeholder="Issue identifier, e.g. ENG-123"
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
            <h2>{translate("Remote blueprint")}</h2>
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
              placeholder="Run remote command"
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
}: {
  selected: Session | null;
  onError: (error: unknown) => void;
}) {
  const [tools, setTools] = useState<Array<Record<string, unknown>>>([]);
  const [servers, setServers] = useState<Array<Record<string, unknown>>>([]);
  const [search, setSearch] = useState("");
  const [devinMcpSaving, setDevinMcpSaving] = useState(false);
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
  const filtered = tools.filter((tool) =>
    String(tool.name).toLowerCase().includes(search.toLowerCase()),
  );
  const addDevinMcp = () => {
    setDevinMcpSaving(true);
    void command("devin_mcp_configure")
      .then(() =>
        command("save_asset", {
          id: "devin-mcp",
          kind: "mcp",
          title: "Devin MCP",
          body: JSON.stringify({
            object_id: "devin-mcp",
            server_key: "devin",
            name: "Devin MCP",
            transport: "streamable-http",
            command: null,
            args: [],
            env: {},
            cwd: null,
            url: "https://mcp.devin.ai/mcp",
            headers: {},
            enabled: true,
            requires_approval: true,
          }),
          trigger: null,
          scope: null,
          scopeKind: "global",
          enabled: true,
        }),
      )
      .then(() => command<Array<Record<string, unknown>>>("list_mcp_servers"))
      .then(setServers)
      .catch(onError)
      .finally(() => setDevinMcpSaving(false));
  };
  return (
    <>
      <div className="grid grid-cols-[repeat(auto-fill,minmax(min(100%,440px),440px))] gap-2.5 mb-4">
        <IntegrationCard
          icon="D"
          title="Devin MCP"
          badge={{
            label: servers.some((server) => String(server.id) === "devin-mcp")
              ? "Enabled"
              : "Not configured",
            tone: servers.some((server) => String(server.id) === "devin-mcp")
              ? "success"
              : "neutral",
          }}
          description="https://mcp.devin.ai/mcp · Streamable HTTP"
          actions={
            <Button
              className="bordered"
              disabled={
                devinMcpSaving ||
                servers.some((server) => String(server.id) === "devin-mcp")
              }
              onClick={addDevinMcp}
            >
              {devinMcpSaving
                ? "Adding…"
                : servers.some((server) => String(server.id) === "devin-mcp")
                  ? "Added"
                  : "Add Devin MCP"}
            </Button>
          }
        />
      </div>
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
                    description={`${String(server.transport || "remote")} · ${String(server.url || "configured")}`}
                    actions={
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
                    }
                    key={String(server.id)}
                  />
                ))}
              {filtered.map((tool) => (
                <IntegrationCard
                  icon={String(tool.name).slice(0, 1).toUpperCase()}
                  title={String(tool.name)}
                  badge={{ label: "Enabled", tone: "success" }}
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
    "audit" | "board" | "roles" | "tasks" | "messages" | "worklog" | "insights"
  >("board");
  const [worklog, setWorklog] = useState<Record<string, unknown> | null>(null);
  const [insights, setInsights] = useState<Record<string, unknown> | null>(
    null,
  );
  const [auditEvents, setAuditEvents] = useState<Record<string, unknown>[]>([]);
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
              }}
            >
              <Icon
                name={
                  (
                    {
                      audit: "audit",
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

function ArtifactsPane({ selected }: { selected: Session }) {
  const [artifacts, setArtifacts] = useState<ArtifactRecord[]>([]);
  const [opened, setOpened] = useState<ArtifactRecord | null>(null);
  const [content, setContent] = useState<Record<string, unknown> | null>(null);
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
    void command<Record<string, unknown>>("read_artifact", {
      sessionId: selected.id,
      artifactId: opened.id,
    })
      .then(setContent)
      .catch((error) => setContent({ error: errorMessage(error) }));
  }, [selected.id, opened?.id]);
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
}) {
  const [panelTab, setPanelTab] = useState<PanelTab>("info");
  const [opened, setOpened] = useState<PanelTab[]>(["info"]);
  const [insights, setInsights] = useState<Record<string, unknown> | null>(
    null,
  );
  useEffect(() => {
    setInsights(null);
    void command<Record<string, unknown>>("session_insights", {
      sessionId: selected.id,
    })
      .then(setInsights)
      .catch(onError);
  }, [selected.id]);
  const informationTabs: Array<{
    id: typeof panelTab;
    label: string;
    icon: RailIconName;
  }> = [
    { id: "info", label: "Info", icon: "info" },
    { id: "artifacts", label: "Artifacts", icon: "file" },
    { id: "pr", label: "PR", icon: "branch" },
    { id: "insights", label: "Insights", icon: "sparkle" },
  ];
  if (selected.host_id !== "local") {
    informationTabs.splice(3, 0, {
      id: "worklog",
      label: "Worklog",
      icon: "list",
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
  }> =
    selected.host_id === "local"
      ? []
      : [
          { id: "terminal", label: "Shell", icon: "terminal" },
          { id: "desktop", label: "Desktop", icon: "monitor" },
          { id: "ide", label: "Web IDE", icon: "code" },
          { id: "browser", label: "Browser", icon: "grid" },
        ];
  const tabs = [...informationTabs, ...workspaceTabs, ...remoteTabs];
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
            {tabs
              .filter(
                (item) =>
                  item.id !== "info" &&
                  item.id !== "artifacts" &&
                  opened.includes(item.id),
              )
              .map((item) => (
                <div
                  className="session-pane"
                  key={item.id}
                  style={{ display: panelTab === item.id ? "flex" : "none" }}
                >
                  <SurfaceView
                    tab={item.id as Exclude<SurfaceTab, "chat">}
                    selected={selected}
                    onError={onError}
                  />
                </div>
              ))}
          </div>
        </div>
      )}
      {!collapsed && (
        <div
          className="session-panel-resizer"
          role="separator"
          aria-label="Resize session panel"
          onPointerDown={(event) => {
            event.currentTarget.setPointerCapture(event.pointerId);
            const startX = event.clientX;
            const startWidth = width;
            const move = (moveEvent: PointerEvent) => {
              const next = Math.min(
                460,
                Math.max(308, startWidth + startX - moveEvent.clientX),
              );
              onWidthChange(next);
            };
            const stop = () => {
              window.removeEventListener("pointermove", move);
              window.removeEventListener("pointerup", stop);
            };
            window.addEventListener("pointermove", move);
            window.addEventListener("pointerup", stop, { once: true });
          }}
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

function AppContent() {
  const NAV_COLLAPSED_KEY = "opcos:nav-collapsed:v1";
  const [windowMaximized, setWindowMaximized] = useState(false);
  const [hosts, setHosts] = useState<Host[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [selected, setSelected] = useState<Session | null>(null);
  const [transcript, setTranscript] = useState<TranscriptViewItem[]>([]);
  const [surface, setSurface] = useState<
    "session" | "automations" | "manage" | "activity" | "inbox" | "project"
  >("session");
  const [projects, setProjects] = useState<Project[]>([]);
  const [selectedProject, setSelectedProject] = useState<Project | null>(null);
  const [projectDialogOpen, setProjectDialogOpen] = useState(false);
  const [inbox, setInbox] = useState<InboxRecord[]>([]);
  const [unattended, setUnattended] = useState(false);
  const [settingsTab, setSettingsTab] = useState<SettingsSection>("provider");
  const [query, setQuery] = useState("");
  const [error, setError] = useState("");
  const [running, setRunning] = useState(false);
  const [drawerCollapsed, setDrawerCollapsed] = useState(false);
  const [rightPanelWidth, setRightPanelWidth] = useState(() =>
    Math.min(Math.round(window.innerWidth * 0.3), 460),
  );
  const [navCollapsed, setNavCollapsed] = useState(
    () => localStorage.getItem(NAV_COLLAPSED_KEY) === "1",
  );
  const toggleNav = () => {
    const next = !navCollapsed;
    setNavCollapsed(next);
    localStorage.setItem(NAV_COLLAPSED_KEY, next ? "1" : "0");
  };
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
  const [hostName, setHostName] = useState("");
  const [hostUrl, setHostUrl] = useState("");
  const [hostToken, setHostToken] = useState("");
  const [editingHostId, setEditingHostId] = useState<string | null>(null);
  const [assets, setAssets] = useState<Asset[]>([]);
  const [providers, setProviders] = useState<ProviderDescriptor[]>([]);
  const [secrets, setSecrets] = useState<SecretMetadata[]>([]);
  const [models, setModels] = useState<Array<{ id: string; label: string }>>(
    [],
  );
  const [homeInput, setHomeInput] = useState("");
  const [homePlusOpen, setHomePlusOpen] = useState(false);
  const [homeAttachment, setHomeAttachment] = useState<File | null>(null);
  const [homeHostId, setHomeHostId] = useState("");
  const [homeProvider, setHomeProvider] = useState("");
  const [homeModel, setHomeModel] = useState("auto");
  const [homeMode, setHomeMode] = useState("Interactive");
  const [homeHarness, setHomeHarness] = useState("builtin");
  const [harnessOptions, setHarnessOptions] = useState<
    Array<{ id: string; label: string; available: boolean; reason?: string }>
  >([]);
  const [selectedHarnessOptions, setSelectedHarnessOptions] = useState<
    Array<{ id: string; label: string; available: boolean; reason?: string }>
  >([]);
  const [homeWorkspace, setHomeWorkspace] = useState("");
  const [secretBackend, setSecretBackend] = useState("");
  const generation = useRef(0);
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
      nextSessions,
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
    setSessions(nextSessions);
    setAssets(nextAssets);
    setProviders(nextProviders);
    setSecrets(nextSecrets);
    setInbox(nextInbox);
    const nextProjects = await command<Project[]>("list_projects");
    setProjects(nextProjects);
    if (selectedProject) {
      setSelectedProject(
        nextProjects.find((item) => item.id === selectedProject.id) || null,
      );
    }
    if (selected) {
      const current = nextSessions.find((item) => item.id === selected.id);
      if (current) setSelected(current);
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
  }, [homeHostId]);
  useEffect(() => {
    if (!selected) return;
    void command<
      Array<{ id: string; label: string; available: boolean; reason?: string }>
    >("harness_options", {
      hostId: selected.host_id,
      workspace: selected.workspace || null,
    })
      .then(setSelectedHarnessOptions)
      .catch(() => setSelectedHarnessOptions([]));
  }, [selected?.id, selected?.host_id]);
  useEffect(() => {
    if (!homeProvider && providers[0]) setHomeProvider(providers[0].name);
  }, [providers, homeProvider]);
  useEffect(() => {
    void command<Array<{ id: string; label: string }>>("provider_models", {
      provider: "openai",
    })
      .then(setModels)
      .catch((reason) => {
        if (
          (window as Window & { __TAURI_INTERNALS__?: unknown })
            .__TAURI_INTERNALS__
        )
          setError(errorMessage(reason));
      });
  }, []);
  useEffect(() => {
    const currentGeneration = ++generation.current;
    setTranscript([]);
    setRunning(false);
    if (!selected) return;
    void command<Array<{ kind: string; payload: Record<string, unknown> }>>(
      "read_transcript",
      { sessionId: selected.id },
    )
      .then((items) => {
        if (generation.current === currentGeneration)
          setTranscript(normalizeTranscript(items));
      })
      .catch((reason) => {
        if (generation.current === currentGeneration)
          setError(errorMessage(reason));
      });
  }, [selected?.id]);
  useEffect(() => {
    if (!selected) {
      setUnattended(false);
      return;
    }
    void command<boolean>("get_unattended", { sessionId: selected.id })
      .then(setUnattended)
      .catch((reason) => setError(errorMessage(reason)));
  }, [selected?.id]);
  useEffect(() => {
    let active = true;
    const currentGeneration = generation.current;
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
      if (!active || currentGeneration !== generation.current) return;
      if (
        payload.kind === "system" &&
        typeof payload.payload.secret_backend === "string"
      ) {
        setSecretBackend(payload.payload.secret_backend);
      }
      if (payload.session_id && payload.session_id !== selected?.id) return;
      if (payload.kind === "stream") {
        const streamPayload = payload.payload;
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
          if (streamPayload.turn) setRunning(false);
        }
      }
      if (payload.kind === "turn_done") {
        setRunning(false);
        const runState =
          typeof payload.payload.run_state === "string"
            ? payload.payload.run_state
            : undefined;
        const stopReason =
          typeof payload.payload.stop_reason === "string"
            ? payload.payload.stop_reason
            : undefined;
        if (runState || stopReason) {
          setSessions((items) =>
            items.map((item) =>
              item.id === payload.session_id
                ? { ...item, run_state: runState, stop_reason: stopReason }
                : item,
            ),
          );
          setSelected((item) =>
            item && item.id === payload.session_id
              ? { ...item, run_state: runState, stop_reason: stopReason }
              : item,
          );
        }
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
      setTranscript((items) =>
        reduceStreamEvent(items, {
          kind: payload.kind,
          payload: payload.payload,
        }),
      );
    });
    return () => {
      active = false;
      void subscription.then((unlisten) => unlisten());
    };
  }, [selected?.id]);
  const onError = (reason: unknown) => {
    const runtime = (window as Window & { __TAURI_INTERNALS__?: unknown })
      .__TAURI_INTERNALS__;
    const message = errorMessage(reason);
    if (message.includes("Approval required before this tool can continue"))
      return;
    if (!runtime && /invoke|tauri/i.test(message)) return;
    setError(redactApproval(message));
  };
  const addHost = async (event: FormEvent) => {
    event.preventDefault();
    try {
      await command("save_host", {
        id: editingHostId,
        name: hostName,
        url: hostUrl,
        token: hostToken,
      });
      setHostName("");
      setHostUrl("");
      setHostToken("");
      await refresh();
      setEditingHostId(null);
    } catch (reason) {
      onError(submitFailureMessage(reason));
    }
  };
  const editHost = async (host: Host) => {
    const url = await command<string>("host_binding", { hostId: host.id });
    setHostName(host.name);
    setHostUrl(url);
    setHostToken("");
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
  const submitHome = async () => {
    const text = homeInput.trim();
    if (!text || !homeHostId || running) return;
    const title =
      text.split(/\r?\n/, 1)[0].trim().slice(0, 80) || "New session";
    try {
      setRunning(true);
      const next = await command<Session>("create_session", {
        title,
        hostId: homeHostId,
        model: homeModel || "auto",
        provider: homeProvider || null,
        mode: homeMode,
        harness: homeHarness,
        workspace: homeWorkspace || null,
      });
      setSelected(next);
      setSurface("session");
      setHomeInput("");
      await refresh();
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
      setRunning(false);
      onError(submitFailureMessage(reason));
    }
  };
  const activeItems = useMemo(() => transcript, [transcript]);
  const transcriptItems = useMemo<Item[]>(() => {
    const output: Item[] = [];
    activeItems.forEach((item) => {
      if (item.kind === "user")
        output.push({ kind: "user", text: item.text || "" });
      if (item.kind === "assistant")
        output.push({
          kind: "assistant",
          text: item.text || "",
          reasoning: item.reasoning,
        });
      if (item.kind === "thinking")
        output.push({
          kind: "assistant",
          text: "",
          reasoning: item.reasoning || item.text || "",
        });
      if (item.kind === "tool" && (item.approval || item.resolved))
        output.push({
          kind: "approval",
          callId: item.callId,
          name: item.toolName || "approval",
          args: item.arguments,
          reason: item.text || "Tool action requires approval",
          resolved: item.resolved,
        });
      else if (item.kind === "tool")
        output.push({
          kind: "tool",
          id: item.id,
          name: item.toolName || "tool",
          args: item.arguments,
          status: item.status || "ok",
          preview: item.result ? String(item.result) : undefined,
        });
      if (item.kind === "approval")
        output.push({
          kind: "approval",
          callId: item.callId,
          name: item.toolName || "approval",
          args: item.arguments,
          reason: item.text || "",
          resolved: item.status === "ok" ? "allow" : undefined,
        });
      if (item.kind === "notice")
        output.push({
          kind: "notice",
          tone: item.noticeKind === "error" ? "warn" : "info",
          text: item.text || "",
        });
    });
    return output;
  }, [activeItems]);
  const approvalPending = activeItems.some(
    (item) =>
      item.kind === "tool" && item.approval && item.status === "pending",
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
  const submit = (text: string) => {
    if (!selected) return;
    setRunning(true);
    void command("submit_turn", {
      request: { session_id: selected.id, text },
    }).catch((reason) => {
      setRunning(false);
      onError(submitFailureMessage(reason));
    });
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
  const steer = (text: string) => {
    if (!selected) return;
    void command("steering", { sessionId: selected.id, text }).catch(onError);
  };
  return (
    <div
      className={`app ${surface === "session" && selected ? "session-layout" : "surface-layout"}${surface === "session" && selected && drawerCollapsed ? " session-drawer-collapsed" : ""}${navCollapsed ? " nav-collapsed" : ""}${windowMaximized ? " window-maximized" : ""}`}
      style={
        {
          "--right-panel-width": `${drawerCollapsed ? 44 : rightPanelWidth}px`,
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
          archived: false,
          attention: 0,
          liveness: selected?.id === session.id && running ? "working" : "idle",
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
          setSelected(next);
          setSurface("session");
        }}
        onNew={openNewSessionHome}
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
        collapsed={navCollapsed}
        onCollapse={toggleNav}
      />
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
          />
        ) : surface === "session" && selected ? (
          <>
            {/* OpenWorker session topbar: surfaces/gui/src/App.tsx:1365-1442.
                Only the facts and Tauri panel action are adapted to OPCOS. */}
            <header className="main-topbar">
              <div className="main-topbar-side">
                {navCollapsed && (
                  <button
                    className="topbar-icon-btn"
                    onClick={toggleNav}
                    aria-label="Show sidebar"
                    title="Show sidebar"
                  >
                    <Icon name="sidebar" size={16} />
                  </button>
                )}
              </div>
              <div className="main-title">
                <span className="main-title-text" title={selected.title}>
                  {selected.title}
                </span>
                <span className="title-sub" data-testid="session-subtitle">
                  {[
                    selected.host_name,
                    selected.workspace || "workspace not set",
                    selected.model,
                    sessionStatusLabel(
                      selected.run_state,
                      selected.stop_reason,
                    ),
                  ].join(" · ")}
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
                <div className="main-scroll">
                  <Transcript
                    items={transcriptItems}
                    running={running}
                    onApprove={(item, decision) => {
                      if (!item.callId) return;
                      void command("resolve_approval", {
                        sessionId: selected.id,
                        callId: item.callId,
                        approve: decision === "allow",
                      }).catch(onError);
                    }}
                  />
                </div>
                <Composer
                  mode={selected.mode}
                  harness={selected.harness}
                  harnessOptions={selectedHarnessOptions}
                  model={selected.model}
                  models={models.map((item) => item.id)}
                  modelLabels={Object.fromEntries(
                    models.map((item) => [item.id, item.label]),
                  )}
                  connected={Boolean(selected)}
                  running={running}
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
                  onInterrupt={() =>
                    command("interrupt", { sessionId: selected.id }).catch(
                      onError,
                    )
                  }
                  assets={assets}
                  secrets={secrets}
                  onUploadFile={uploadTextAttachment}
                  resetKey={selected.id}
                />
              </div>
            </div>
          </>
        ) : surface === "manage" ? (
          <SettingsView activeTab={settingsTab} onTabChange={setSettingsTab}>
            <ManageSections
              tab={settingsTab}
              hosts={hosts}
              assets={assets}
              providers={providers}
              secrets={secrets}
              selected={selected}
              onRefresh={() => refresh().catch(onError)}
              onError={onError}
              onAddHost={addHost}
              onEditHost={editHost}
              onTestHost={testHost}
              onDeleteHost={deleteHost}
              hostName={hostName}
              setHostName={setHostName}
              hostUrl={hostUrl}
              setHostUrl={setHostUrl}
              hostToken={hostToken}
              setHostToken={setHostToken}
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
                      {models.map((model) => (
                        <option key={model.id} value={model.id}>
                          {model.label}
                        </option>
                      ))}
                    </select>
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
                      running={running}
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
          <div className="error-banner">
            {error}
            <button onClick={() => setError("")}>×</button>
          </div>
        )}
      </main>
      {projectDialogOpen && (
        <ProjectDialog
          hosts={hosts}
          onClose={() => setProjectDialogOpen(false)}
          onSubmit={async (values) => {
            const project = await command<Project>("create_project", {
              name: values.name,
              hostId: values.hostId,
              repoUrl: values.repoUrl || null,
              repoRoot: values.repoRoot || null,
              defaultBranch: values.defaultBranch,
            });
            setProjects((items) => [...items, project]);
            setSelectedProject(project);
            setProjectDialogOpen(false);
            setSurface("project");
          }}
        />
      )}
      {surface === "session" && selected && (
        <SessionRightPanel
          selected={selected}
          running={running}
          collapsed={drawerCollapsed}
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
          onCollapsedChange={setDrawerCollapsed}
          width={rightPanelWidth}
          onWidthChange={setRightPanelWidth}
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
