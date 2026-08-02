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
import { Composer, PlusMenu, SendButton } from "./components/Composer";
import { SelectMenu as OpenWorkerSelectMenu } from "./components/SelectMenu";
import { SettingsView, type SettingsSection } from "./components/SettingsView";
import { Icon } from "./components/Icon";
import type { Item } from "./types";
import { CollectionPage } from "./components/CollectionPage";
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
  needs_key?: boolean;
  default_base_url?: string | null;
  recommended_model?: string | null;
};
type Asset = {
  id: string;
  kind: string;
  title: string;
  body: string;
  trigger: string;
  scope: string;
  scope_kind?: string;
  enabled: boolean;
};
type SecretMetadata = { name: string; scope: string; purpose: string };
type Schedule = {
  id: string;
  name: string;
  session_id: string;
  playbook_id: string;
  cron: string;
  enabled: boolean;
  last_run?: string;
  last_result?: string;
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
          }).catch(onError)
        }
      >
        Run {operation}
      </Button>
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
  onTestHost,
  onDeleteHost,
  hostName,
  setHostName,
  hostUrl,
  setHostUrl,
  hostToken,
  setHostToken,
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
  onTestHost: (hostId: string) => Promise<Host>;
  onDeleteHost: (hostId: string) => Promise<void>;
  hostName: string;
  setHostName: (value: string) => void;
  hostUrl: string;
  setHostUrl: (value: string) => void;
  hostToken: string;
  setHostToken: (value: string) => void;
}) {
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
  const sectionCopy: Record<SettingsSection, [string, string]> = {
    provider: [
      "Provider",
      "Choose a provider and validate its connection key.",
    ],
    hosts: ["Hosts", "Bind and test the remote hosts used by OPCOS sessions."],
    agents: ["规则", "仓库级运行规则（对应仓库中的 AGENTS.md 文件）。"],
    knowledge: ["Knowledge", "Reusable reference material added to context."],
    playbook: ["Playbook", "Repeatable workflows available to automation."],
    skill: ["Skill", "Focused capability and instruction bundles."],
    mcp: ["MCP", "Control the tools exposed by the selected remote host."],
    secrets: [
      "Secrets",
      "Inspect secret metadata without exposing secret values.",
    ],
    blueprint: ["Blueprint", "Read and manage the selected host blueprint."],
    appearance: [translate("general"), translate("appearanceDescription")],
  };
  const assetKinds = ["agents", "knowledge", "playbook", "skill"] as const;
  const assetTabKind = assetKinds.includes(tab as (typeof assetKinds)[number])
    ? (tab as Asset["kind"])
    : "knowledge";
  const assetLabel =
    assetTabKind === "agents"
      ? "规则"
      : assetTabKind[0].toUpperCase() + assetTabKind.slice(1);
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
    <section>
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
            <div className="grid grid-cols-2 xl:grid-cols-3 gap-2.5">
              {providers.map((descriptor) => {
                const config = providerConfigs.find(
                  (item) => item.provider === descriptor.name,
                );
                return (
                  <button
                    key={descriptor.name}
                    className="flex items-center gap-2.5 rounded-xl border border-line bg-panel px-3 py-2.5 text-left hover:border-lineStrong transition-colors"
                    onClick={() => setSelectedProvider(descriptor.name)}
                  >
                    <span className="rounded-lg border border-line grid place-items-center shrink-0 w-8 h-8 bg-paper">
                      <span className="text-[13px] font-semibold text-muted">
                        {descriptor.title.slice(0, 1)}
                      </span>
                    </span>
                    <span className="min-w-0 flex-1">
                      <span className="block text-[13px] font-semibold leading-tight truncate">
                        {descriptor.title}
                      </span>
                      <span className="block text-[11.5px] text-faint truncate">
                        {config?.configured
                          ? "✓ Configured securely."
                          : config?.base_url
                            ? config.base_url
                            : "Not configured yet."}
                      </span>
                    </span>
                    <span className="text-faint text-[14px]">›</span>
                  </button>
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
                options={providers.map((item) => ({
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
              <Button className="primary" onClick={() => setHostFormOpen(true)}>
                Add host
              </Button>
            }
            rows={
              hosts.length ? (
                <>
                  {hosts.map((host) => (
                    <div className="manage-row px-4" key={host.id}>
                      <span>
                        <strong>{host.name}</strong>
                        <small>
                          <span
                            className={
                              host.online === true
                                ? "status-online"
                                : host.online === false
                                  ? "status-offline"
                                  : "status-unknown"
                            }
                          >
                            {hostStatusLabel(host)}
                          </span>
                        </small>
                      </span>
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
                    </div>
                  ))}
                </>
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
                    placeholder={translate("Bearer token")}
                    type="password"
                    required
                  />
                  <Button type="submit" className="primary">
                    Add host
                  </Button>
                </form>
              ) : undefined
            }
          />
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
              .filter(([kind]) => kind === assetTabKind)
              .map(([kind, label, description]) => (
                <CollectionPage
                  key={kind}
                  search={assetSearch}
                  onSearch={setAssetSearch}
                  searchPlaceholder={
                    kind === "agents" ? "搜索规则" : `Search ${label}`
                  }
                  actions={
                    tab === "knowledge" ? (
                      <div className="inline-actions">
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
                                    { assetId: asset.id },
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
                                  void command("delete_asset", { id: asset.id })
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
                    {assetTabKind === "agents" ? "适用范围" : "Scope"}
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
                      disabled={assetScopeKind === "global"}
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
        {tab === "secrets" && (
          <CollectionPage
            search=""
            onSearch={() => undefined}
            searchPlaceholder={translate("searchSecretKeys")}
            primary={
              <Button className="primary">{translate("addSecret")}</Button>
            }
            rows={
              secrets.length ? (
                <>
                  {secrets.map((secret) => (
                    <div className="manage-row px-4" key={secret.name}>
                      <span>
                        <strong>{secret.name}</strong>
                        <small>
                          {secret.scope} · {secret.purpose}
                        </small>
                      </span>
                      <span className="muted">{translate("delete")}</span>
                    </div>
                  ))}
                </>
              ) : null
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
              placeholder={translate("Execute a blueprint command")}
            />
            <div className="inline-actions">
              <Button
                disabled={!selected}
                onClick={() =>
                  selected &&
                  command("execute_blueprint", {
                    sessionId: selected.id,
                    command: blueprintCommand,
                  }).catch(onError)
                }
              >
                Execute
              </Button>
              <Button
                disabled={!selected}
                onClick={() =>
                  selected &&
                  command("run_blueprint", { sessionId: selected.id }).catch(
                    onError,
                  )
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
  return (
    <CollectionPage
      search={search}
      onSearch={setSearch}
      searchPlaceholder={translate("searchMcp")}
      rows={
        filtered.length || servers.length ? (
          <>
            {servers
              .filter((server) =>
                String(server.name)
                  .toLowerCase()
                  .includes(search.toLowerCase()),
              )
              .map((server) => (
                <div className="manage-row px-4" key={String(server.id)}>
                  <span>
                    <strong>{String(server.name)}</strong>
                    <small>
                      {String(server.transport || "remote")} ·{" "}
                      {String(server.status || "configured")}
                    </small>
                  </span>
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
              ))}
            {filtered.map((tool) => (
              <div className="manage-row px-4" key={String(tool.name)}>
                <span>
                  <strong>{String(tool.name)}</strong>
                  <small>
                    {String(tool.transport || "remote")} ·{" "}
                    {String(tool.command || tool.url || "host-provided")} ·
                    Enabled
                  </small>
                </span>
                <Button
                  onClick={() =>
                    command("set_mcp_tool_enabled", {
                      sessionId: selected?.id,
                      name: String(tool.name),
                      enabled: true,
                    }).catch(onError)
                  }
                >
                  Enable
                </Button>
              </div>
            ))}
          </>
        ) : null
      }
      empty={
        selected
          ? "No MCP tools available."
          : "Select a session to inspect its host MCP tools."
      }
    />
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
                                {schedule.cron} ·{" "}
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
    <div className="page">
      <div className="flex min-h-full">
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
        <div className="flex-1 min-w-0 overflow-y-auto">
          <div className="w-full px-7 py-6">
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
    </div>
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
    { id: "worklog", label: "Worklog", icon: "list" },
    { id: "insights", label: "Insights", icon: "sparkle" },
  ];
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
                        <option key={item.name} value={item.name}>
                          {item.title}
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

function AppContent() {
  const NAV_COLLAPSED_KEY = "opcos:nav-collapsed:v1";
  const [windowMaximized, setWindowMaximized] = useState(false);
  const [hosts, setHosts] = useState<Host[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [selected, setSelected] = useState<Session | null>(null);
  const [transcript, setTranscript] = useState<TranscriptViewItem[]>([]);
  const [surface, setSurface] = useState<
    "session" | "automations" | "manage" | "activity"
  >("session");
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
    const [nextHosts, nextSessions, nextAssets, nextProviders, nextSecrets] =
      await Promise.all([
        command<Host[]>("list_hosts"),
        command<Session[]>("list_sessions"),
        command<Asset[]>("list_assets"),
        command<ProviderDescriptor[]>("provider_descriptors"),
        command<SecretMetadata[]>("list_secret_metadata"),
      ]);
    setHosts(nextHosts);
    setSessions(nextSessions);
    setAssets(nextAssets);
    setProviders(nextProviders);
    setSecrets(nextSecrets);
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
        setRunning(true);
        if (payload.payload.turn) setRunning(false);
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
        name: hostName,
        url: hostUrl,
        token: hostToken,
      });
      setHostName("");
      setHostUrl("");
      setHostToken("");
      await refresh();
    } catch (reason) {
      onError(submitFailureMessage(reason));
    }
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
        }))}
        agent="opcos"
        workspace={selected?.workspace || ""}
        activeSession={selected?.id || ""}
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
        {surface === "session" && selected ? (
          <>
            <header className="main-topbar session-header">
              <div>
                <h1>{selected.title}</h1>
                <p>
                  {selected.host_name} ·{" "}
                  {selected.workspace || "workspace not set"} · {selected.model}
                </p>
                <p className="surface-status muted">
                  {sessionStatusLabel(selected.run_state, selected.stop_reason)}
                </p>
              </div>
              <div className="main-topbar-actions">
                {secretBackend && (
                  <span className="backend-badge">
                    Secrets: {secretBackend}
                  </span>
                )}
                <button
                  className="icon-button"
                  title={translate("Toggle session panel")}
                  onClick={() => setDrawerCollapsed((value) => !value)}
                >
                  <Icon name="sidebarRight" />
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
                  model={selected.model}
                  models={models.map((item) => item.id)}
                  modelLabels={Object.fromEntries(
                    models.map((item) => [item.id, item.label]),
                  )}
                  connected={Boolean(selected)}
                  running={running}
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
              onTestHost={testHost}
              onDeleteHost={deleteHost}
              hostName={hostName}
              setHostName={setHostName}
              hostUrl={hostUrl}
              setHostUrl={setHostUrl}
              hostToken={hostToken}
              setHostToken={setHostToken}
            />
          </SettingsView>
        ) : surface === "automations" ? (
          <Automations sessions={sessions} assets={assets} onError={onError} />
        ) : surface === "activity" ? (
          <Activity selected={selected} onError={onError} />
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
                        <option key={provider.name} value={provider.name}>
                          {provider.title}
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
                      title="远程 workspace"
                      value={homeWorkspace}
                      onChange={(event) => setHomeWorkspace(event.target.value)}
                      placeholder="Workspace"
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
