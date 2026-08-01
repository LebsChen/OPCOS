import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { FormEvent, useEffect, useMemo, useRef, useState } from "react";
import { Terminal } from "@xterm/xterm";
import RFB from "@novnc/novnc";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import "@xterm/xterm/css/xterm.css";
import {
  Host,
  Session,
  SurfaceTab,
  filterSessions,
  groupSessionsByHost,
  hostFailureMessage,
  hostStatusLabel,
  errorMessage,
  noticeClass,
  redactApproval,
  submitFailureMessage,
} from "./gui";
import {
  TranscriptViewItem,
  normalizeTranscript,
  reduceStreamEvent,
  toolArgumentSummary,
} from "./transcript";
import { Sidebar } from "./components/Sidebar";
import { NewSessionModal } from "./components/NewSessionModal";
import { Transcript } from "./components/Transcript";
import { Composer } from "./components/Composer";
import { RightRail } from "./components/RightRail";
import { SelectMenu as OpenWorkerSelectMenu } from "./components/SelectMenu";
import { SettingsView, type SettingsSection } from "./components/SettingsView";
import "./openworker-tailwind.css";
import "./openworker-styles.css";
import "./style.css";

type UiEvent = {
  kind: string;
  session_id?: string;
  payload: Record<string, unknown>;
};
type ProviderDescriptor = { name: string; title: string };
type Asset = {
  id: string;
  kind: string;
  title: string;
  body: string;
  trigger: string;
  scope: string;
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

async function command<T>(
  name: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return invoke<T>(name, args);
}

function Icon({ name, size = 16 }: { name: string; size?: number }) {
  const paths: Record<string, string> = {
    plus: "M8 3v10M3 8h10",
    search: "m11 11 4 4m-1.5-8A4.5 4.5 0 1 1 4.5 7 4.5 4.5 0 0 1 13.5 7Z",
    settings:
      "M6.5 2h3l.5 2 1.5.8 1.8-.9 2.1 2.1-.9 1.8.8 1.5 2 .5v3l-2 .5-.8 1.5.9 1.8-2.1 2.1-1.8-.9-1.5.8-.5 2h-3l-.5-2-1.5-.8-1.8.9-2.1-2.1.9-1.8-.8-1.5-2-.5v-3l2-.5.8-1.5-.9-1.8L3.2 4.7l1.8.9 1.5-.8.5-2Z M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z",
    activity: "M2 8h3l1.5-4L10 12l1.5-4H15",
    send: "m2 2 13 6-13 6 3-6-3-6Zm3 6h10",
    stop: "M4 4h8v8H4z",
    terminal: "m3 4 4 4-4 4m6 0h4",
    desktop: "M2 3h12v8H2zM6 14h4M8 11v3",
    browser: "M2 3h12v10H2zM2 6h12M4 4.5h.01M6 4.5h.01",
    code: "m5 4-4 4 4 4m6-8 4 4-4 4M9 2 7 14",
    refresh: "M13 5V2l-2 2a5 5 0 1 0 1.2 6",
  };
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d={paths[name] || paths.plus} />
    </svg>
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
function LegacySidebar(props: {
  hosts: Host[];
  sessions: Session[];
  selected?: Session | null;
  query: string;
  onQuery: (value: string) => void;
  onSelect: (session: Session) => void;
  onNew: () => void;
  onTest: (host: Host) => void;
  onSurface: (surface: "manage" | "activity" | "automations") => void;
  onAddHost: (event: FormEvent) => void;
  hostName: string;
  setHostName: (value: string) => void;
  hostUrl: string;
  setHostUrl: (value: string) => void;
  hostToken: string;
  setHostToken: (value: string) => void;
}) {
  const groups = groupSessionsByHost(
    filterSessions(props.sessions, props.query),
  );
  return (
    <aside className="sidebar">
      <div className="brand">
        <span className="brand-mark">✦</span>
        <strong>OPCOS</strong>
        <span className="beta">M9</span>
      </div>
      <Button className="new-session" onClick={props.onNew}>
        <Icon name="plus" /> New session
      </Button>
      <label className="search">
        <Icon name="search" />
        <input
          value={props.query}
          onChange={(event) => props.onQuery(event.target.value)}
          placeholder="Search sessions"
        />
      </label>
      <div className="sidebar-scroll">
        <div className="sidebar-label">SESSIONS</div>
        {groups.length === 0 && <p className="muted small">No sessions yet.</p>}
        {groups.map((group) => (
          <section className="session-group" key={group.hostId}>
            <div className="group-title">
              <span className="status-dot" />
              {group.hostName}
            </div>
            {group.sessions.map((session) => (
              <button
                className={`session-row ${props.selected?.id === session.id ? "selected" : ""}`}
                key={session.id}
                onClick={() => props.onSelect(session)}
              >
                <span className="session-title">{session.title}</span>
                <span className="session-meta">
                  {session.host_name} · {session.model}
                </span>
              </button>
            ))}
          </section>
        ))}
        <div className="sidebar-label hosts-label">HOSTS</div>
        <form className="host-form" onSubmit={props.onAddHost}>
          <input
            value={props.hostName}
            onChange={(event) => props.setHostName(event.target.value)}
            placeholder="Host name"
            required
          />
          <input
            value={props.hostUrl}
            onChange={(event) => props.setHostUrl(event.target.value)}
            placeholder="Remote URL"
            type="url"
            required
          />
          <input
            value={props.hostToken}
            onChange={(event) => props.setHostToken(event.target.value)}
            placeholder="Bearer token"
            type="password"
            required
          />
          <Button type="submit">
            <Icon name="plus" /> Add host
          </Button>
        </form>
        {props.hosts.map((host) => (
          <div className="host-row" key={host.id}>
            <span>
              <span
                className={`status-dot ${host.online === false ? "offline" : host.online === true ? "online" : ""}`}
              />
              {host.name}
            </span>
            <Button className="tiny" onClick={() => props.onTest(host)}>
              Test
            </Button>
            {host.online === false && (
              <small className="failure">{hostFailureMessage(host)}</small>
            )}
          </div>
        ))}
      </div>
      <div className="sidebar-footer">
        <Button
          className="nav-button"
          onClick={() => props.onSurface("manage")}
        >
          <Icon name="settings" /> Manage
        </Button>
        <Button
          className="nav-button"
          onClick={() => props.onSurface("activity")}
        >
          <Icon name="activity" /> Activity
        </Button>
        <Button
          className="nav-button"
          onClick={() => props.onSurface("automations")}
        >
          <span>◷</span> Automations
        </Button>
      </div>
    </aside>
  );
}

function LegacyNewSessionModal({
  hosts,
  onClose,
  onCreate,
}: {
  hosts: Host[];
  onClose: () => void;
  onCreate: (
    title: string,
    hostId: string,
    model: string,
    mode: string,
    workspace: string,
  ) => void;
}) {
  const [title, setTitle] = useState("");
  const [hostId, setHostId] = useState(hosts[0]?.id || "");
  const [model, setModel] = useState("auto");
  const [mode, setMode] = useState("Interactive");
  const [workspace, setWorkspace] = useState("");
  return (
    <div className="modal-backdrop">
      <form
        className="modal"
        onSubmit={(event) => {
          event.preventDefault();
          onCreate(title || "New session", hostId, model, mode, workspace);
        }}
      >
        <div className="modal-head">
          <h2>New session</h2>
          <button type="button" className="close" onClick={onClose}>
            ×
          </button>
        </div>
        <label>
          Title
          <input
            value={title}
            onChange={(event) => setTitle(event.target.value)}
            placeholder="What are you working on?"
          />
        </label>
        <label>
          Bound host
          <SelectMenu
            value={hostId}
            onChange={setHostId}
            options={hosts.map((host) => ({
              value: host.id,
              label: host.name,
            }))}
          />
        </label>
        <label>
          Model
          <input
            value={model}
            onChange={(event) => setModel(event.target.value)}
          />
        </label>
        <label>
          Mode
          <SelectMenu
            value={mode}
            onChange={setMode}
            options={[
              { value: "Interactive", label: "Interactive" },
              { value: "Auto", label: "Auto" },
            ]}
          />
        </label>
        <label>
          Workspace <span className="muted">(remote path)</span>
          <input
            value={workspace}
            onChange={(event) => setWorkspace(event.target.value)}
            placeholder="/workspace"
          />
        </label>
        <div className="modal-actions">
          <Button type="button" onClick={onClose}>
            Cancel
          </Button>
          <Button className="primary" disabled={!hostId}>
            Create session
          </Button>
        </div>
      </form>
    </div>
  );
}

function LegacyTranscript({
  items,
  onApprove,
  onDeny,
  running,
}: {
  items: TranscriptViewItem[];
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  running: boolean;
}) {
  return (
    <div className="transcript">
      {items.map((item) => (
        <article
          className={`transcript-item ${item.kind} ${noticeClass(item.noticeKind || item.kind)}`}
          key={item.id}
        >
          {item.kind === "user" && (
            <>
              <div className="who">you</div>
              <div className="bubble user-bubble">{item.text}</div>
            </>
          )}
          {item.kind === "assistant" && (
            <>
              <div className="who">assistant</div>
              <div className="bubble assistant-bubble">
                <ReactMarkdown remarkPlugins={[remarkGfm]}>
                  {item.text || ""}
                </ReactMarkdown>
                {item.id === "stream:assistant" && (
                  <span className="stream-cursor">▍</span>
                )}
              </div>
            </>
          )}
          {item.kind === "thinking" && (
            <details className="thinking">
              <summary>Thinking</summary>
              <div>{item.reasoning}</div>
            </details>
          )}
          {item.kind === "notice" && (
            <div className="notice-card">
              <strong>{item.noticeKind || "notice"}</strong>
              <span>{item.text}</span>
            </div>
          )}
          {item.kind === "tool" && (
            <details
              className={`tool-card ${item.status || "running"}`}
              open={item.status === "pending"}
            >
              <summary>
                <span className="tool-icon">⌘</span>
                <strong>{item.toolName || "tool"}</strong>
                <span className="tool-state">{item.status}</span>
              </summary>
              <div className="tool-body">
                <div className="tool-label">Arguments</div>
                <code>{toolArgumentSummary(item.arguments)}</code>
                {item.result !== undefined && (
                  <>
                    <div className="tool-label">Output</div>
                    <code>{redactApproval(item.result)}</code>
                  </>
                )}
                {item.approval && (
                  <div className="approval-actions">
                    <strong>Approval required. The session is paused.</strong>
                    <div>
                      <Button
                        className="primary"
                        disabled={!running}
                        onClick={() => onApprove(item.callId || "")}
                      >
                        Approve
                      </Button>
                      <Button onClick={() => onDeny(item.callId || "")}>
                        Deny
                      </Button>
                    </div>
                  </div>
                )}
              </div>
            </details>
          )}
        </article>
      ))}
      {running && (
        <div className="waiting">
          <span className="spinner" /> Waiting for agent…
        </div>
      )}
    </div>
  );
}

function LegacyComposer({
  selected,
  running,
  onSubmit,
  onSteer,
  onInterrupt,
}: {
  selected: Session;
  running: boolean;
  onSubmit: (text: string) => void;
  onSteer: (text: string) => void;
  onInterrupt: () => void;
}) {
  const [text, setText] = useState("");
  const send = () => {
    const value = text.trim();
    if (!value) return;
    running ? onSteer(value) : onSubmit(value);
    setText("");
  };
  return (
    <div className="composer">
      <textarea
        value={text}
        onChange={(event) => setText(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === "Enter" && !event.shiftKey) {
            event.preventDefault();
            send();
          }
        }}
        placeholder={
          running
            ? "Turn in progress — type a steering instruction…"
            : `Ask OPCOS to work on ${selected.host_name}…`
        }
      />
      <div className="composer-bar">
        <span className="muted small">
          {running
            ? "Enter sends steering · Shift+Enter for newline"
            : "Enter sends · Shift+Enter for newline"}
        </span>
        {running ? (
          <Button onClick={onInterrupt}>
            <Icon name="stop" /> Interrupt
          </Button>
        ) : (
          <Button className="primary" onClick={send}>
            <Icon name="send" /> Send
          </Button>
        )}
      </div>
    </div>
  );
}

function SurfaceView({
  tab,
  selected,
  onError,
}: {
  tab: SurfaceTab;
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
  }, [selected.id, tab]);
  useEffect(() => {
    if (tab !== "terminal" || !port || !terminalHost.current) return;
    const terminal = new Terminal({
      convertEol: true,
      cursorBlink: true,
      theme: { background: "#11151d", foreground: "#d7dbe5" },
    });
    terminal.open(terminalHost.current);
    const socket = new WebSocket(`ws://127.0.0.1:${port}`);
    socket.binaryType = "arraybuffer";
    socket.onmessage = (event) =>
      terminal.write(
        typeof event.data === "string"
          ? event.data
          : new Uint8Array(event.data as ArrayBuffer),
      );
    const input = terminal.onData((data) => socket.send(data));
    terminal.onResize(({ cols, rows }) =>
      socket.send(JSON.stringify({ type: "resize", cols, rows })),
    );
    return () => {
      input.dispose();
      socket.close();
      terminal.dispose();
    };
  }, [tab, port]);
  useEffect(() => {
    if (tab !== "desktop" || !port || !vncHost.current) return;
    const rfb = new RFB(vncHost.current, `ws://127.0.0.1:${port}`);
    rfb.scaleViewport = true;
    return () => rfb.disconnect();
  }, [tab, port]);
  if (tab === "terminal" || tab === "desktop" || tab === "browser")
    return (
      <div className="surface-panel">
        <div className="surface-toolbar">
          <span>
            {tab === "terminal"
              ? "Remote PTY"
              : tab === "desktop"
                ? "Remote desktop"
                : "Browser/CDP surface"}
          </span>
          <Button
            disabled={busy || !!port}
            onClick={() =>
              void start(
                tab === "terminal" ? "pty" : tab === "desktop" ? "vnc" : "cdp",
              )
            }
          >
            {port ? `Connected on ${port}` : `Start ${tab}`}
          </Button>
        </div>
        {tab === "terminal" && (
          <div className="terminal-host" ref={terminalHost} />
        )}
        {tab === "desktop" && <div className="vnc-host" ref={vncHost} />}
        {tab === "browser" && (
          <div className="empty-surface">
            <Icon name="browser" size={32} />
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
        <div className="surface-toolbar">
          <span>Remote Web IDE</span>
          <Button
            disabled={!!idePort}
            onClick={() =>
              command<number>("start_ide_proxy", {
                sessionId: selected.id,
                folderUri: `vscode-remote://${selected.host_name}/${selected.workspace || "workspace"}`,
              })
                .then(setIdePort)
                .catch(onError)
            }
          >
            {idePort ? "Connected" : "Open IDE"}
          </Button>
        </div>
        {idePort ? (
          <iframe
            title="Remote Web IDE"
            src={`http://127.0.0.1:${idePort}/`}
            className="ide-frame"
          />
        ) : (
          <div className="empty-surface">
            <Icon name="code" size={32} />
            <p>Start the remote IDE for this bound session.</p>
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
        <span>Remote review</span>
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
            <h3>Changed files</h3>
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
            <GitActions selected={selected} cwd={cwd} onError={onError} />
          </div>
          <DiffView diff={diff} />
        </div>
      ) : (
        <div className="empty-surface">
          <p>Load the remote status and changes from the bound host.</p>
        </div>
      )}
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
      <h3>Git workflow</h3>
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
        placeholder="slug, files, or commit message"
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
        <summary>Create GitHub PR</summary>
        <input
          value={repo}
          onChange={(event) => setRepo(event.target.value)}
          placeholder="owner/repository"
        />
        <input
          value={pr}
          onChange={(event) => setPr(event.target.value)}
          placeholder="PR title"
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
      <div className="diff-view empty-surface">Select a changed file.</div>
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
  const events = Array.isArray(worklog?.events) ? worklog.events : [];
  return (
    <div className="surface-panel">
      <div className="surface-toolbar">
        <span>Worklog timeline</span>
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
          <p>Load the remote worklog for this session.</p>
          <Button onClick={load}>Load worklog</Button>
        </div>
      )}
      {events.map((event) => (
        <div className="timeline-row" key={JSON.stringify(event)}>
          <span className="timeline-dot" />
          <pre>{String(JSON.stringify(event, null, 2))}</pre>
        </div>
      ))}
    </div>
  );
}

function LegacyRightRail({
  selected,
  running,
  items,
  assets,
  onAsset,
  onMcp,
  onError,
}: {
  selected: Session | null;
  running: boolean;
  items: TranscriptViewItem[];
  assets: Asset[];
  onAsset: (asset: Asset) => void;
  onMcp: (name: string, enabled: boolean) => void;
  onError: (error: unknown) => void;
}) {
  const [insights, setInsights] = useState<Record<string, unknown> | null>(
    null,
  );
  const [tools, setTools] = useState<Array<Record<string, unknown>>>([]);
  useEffect(() => {
    if (!selected) return;
    void command<Record<string, unknown>>("session_insights", {
      sessionId: selected.id,
    })
      .then(setInsights)
      .catch(onError);
    void command<Array<Record<string, unknown>>>("mcp_tools", {
      sessionId: selected.id,
    })
      .then(setTools)
      .catch(onError);
  }, [selected?.id]);
  if (!selected)
    return (
      <aside className="right-rail">
        <div className="empty-surface">Select a session.</div>
      </aside>
    );
  const recentTools = items.filter((item) => item.kind === "tool").slice(-5);
  return (
    <aside className="right-rail">
      <RailSection title="Progress">
        <div className="progress-line">
          <span className={`status-dot ${running ? "busy" : "online"}`} />
          {running ? "Turn in progress" : "Ready"}
        </div>
        {recentTools.map((tool) => (
          <div className="rail-tool" key={tool.id}>
            <strong>{tool.toolName}</strong>
            <span>{tool.status}</span>
          </div>
        ))}
      </RailSection>
      <RailSection title="Insights">
        {insights ? (
          <div className="insights">
            <span>
              Messages <b>{String(insights.message_count ?? 0)}</b>
            </span>
            <span>
              Tool calls <b>{String(insights.tool_calls ?? 0)}</b>
            </span>
            <span>
              Approvals <b>{String(insights.approval_count ?? 0)}</b>
            </span>
            <span>
              Tokens{" "}
              <b>
                {String(
                  (insights.token_usage as Record<string, unknown> | undefined)
                    ?.input ?? 0,
                )}{" "}
                in /{" "}
                {String(
                  (insights.token_usage as Record<string, unknown> | undefined)
                    ?.output ?? 0,
                )}{" "}
                out
              </b>
            </span>
          </div>
        ) : (
          <span className="muted">Loading…</span>
        )}
      </RailSection>
      <RailSection title="Access">
        <dl className="access">
          <dt>Host</dt>
          <dd>{selected.host_name}</dd>
          <dt>Workspace</dt>
          <dd>{selected.workspace || "not configured"}</dd>
          <dt>Mode</dt>
          <dd>{selected.mode}</dd>
          <dt>Model</dt>
          <dd>{selected.model}</dd>
        </dl>
      </RailSection>
      <RailSection title="Assets">
        {assets.length === 0 && (
          <span className="muted">No assets configured.</span>
        )}
        {assets.map((asset) => (
          <label className="toggle-row" key={asset.id}>
            <span>
              {asset.title}
              <small>{asset.kind}</small>
            </span>
            <input
              type="checkbox"
              checked={asset.enabled}
              onChange={() => onAsset(asset)}
            />
          </label>
        ))}
      </RailSection>
      <RailSection title="MCP tools">
        {tools.map((tool) => {
          const name = String(tool.name || "tool");
          return (
            <label className="toggle-row" key={name}>
              <span>{name}</span>
              <input
                type="checkbox"
                defaultChecked
                onChange={(event) => onMcp(name, event.target.checked)}
              />
            </label>
          );
        })}
      </RailSection>
    </aside>
  );
}
function RailSection({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <section className="rail-section">
      <h3>{title}</h3>
      {children}
    </section>
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
  const [assetTitle, setAssetTitle] = useState("");
  const [assetBody, setAssetBody] = useState("");
  const [blueprint, setBlueprint] = useState<Record<string, unknown> | null>(
    null,
  );
  const [blueprintCommand, setBlueprintCommand] = useState("");
  const [testingHostId, setTestingHostId] = useState<string | null>(null);
  const [deletingHostId, setDeletingHostId] = useState<string | null>(null);
  const [confirmDeleteHostId, setConfirmDeleteHostId] = useState<string | null>(
    null,
  );
  const sectionCopy: Record<SettingsSection, [string, string]> = {
    provider: [
      "Provider",
      "Choose a provider and validate its connection key.",
    ],
    hosts: ["Hosts", "Bind and test the remote hosts used by OPCOS sessions."],
    assets: ["Assets", "Manage reusable knowledge and playbook assets."],
    mcp: ["MCP", "Control the tools exposed by the selected remote host."],
    secrets: [
      "Secrets",
      "Inspect secret metadata without exposing secret values.",
    ],
    blueprint: ["Blueprint", "Read and manage the selected host blueprint."],
  };
  useEffect(() => {
    void command<Record<string, unknown>>("provider_settings")
      .then((value) => {
        setProvider(String(value.provider || "openai"));
        setBaseUrl(String(value.base_url || ""));
      })
      .catch(onError);
  }, []);
  return (
    <section>
      <header className="mb-5">
        <h1 className="text-[22px] font-semibold text-ink">
          {sectionCopy[tab][0]}
        </h1>
        <p className="text-[13px] text-muted mt-1">{sectionCopy[tab][1]}</p>
      </header>
      <div className="rounded-xl2 border border-line bg-panel p-5">
        {tab === "provider" && (
          <div className="form-grid">
            <label>
              Provider
              <SelectMenu
                value={provider}
                onChange={setProvider}
                options={providers.map((item) => ({
                  value: item.name,
                  label: item.title,
                }))}
              />
            </label>
            <label>
              Base URL
              <input
                type="url"
                value={baseUrl}
                onChange={(event) => setBaseUrl(event.target.value)}
              />
            </label>
            <label>
              Provider key
              <input
                type="password"
                value={key}
                onChange={(event) => setKey(event.target.value)}
              />
            </label>
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
            {providerStatus && (
              <div
                className={
                  providerStatus.includes("failed") ? "failure" : "success"
                }
              >
                {providerStatus}
              </div>
            )}
            <p className="muted">
              Keys are stored by Rust and are never returned to the UI.
            </p>
          </div>
        )}
        {tab === "hosts" && (
          <div>
            <h2>Bound hosts</h2>
            <form className="host-form" onSubmit={onAddHost}>
              <input
                value={hostName}
                onChange={(event) => setHostName(event.target.value)}
                placeholder="Host name"
                required
              />
              <input
                value={hostUrl}
                onChange={(event) => setHostUrl(event.target.value)}
                placeholder="Remote URL"
                type="url"
                required
              />
              <input
                value={hostToken}
                onChange={(event) => setHostToken(event.target.value)}
                placeholder="Bearer token"
                type="password"
                required
              />
              <Button type="submit" className="primary">
                Add host
              </Button>
            </form>
            {hosts.map((host) => (
              <div className="manage-row" key={host.id}>
                <span>
                  <strong>{host.name}</strong>
                  <small
                    className={
                      host.online === true
                        ? "status-online"
                        : host.online === false
                          ? "status-offline"
                          : "status-unknown"
                    }
                  >
                    {hostStatusLabel(host)}
                    {host.reason ? ` · ${host.reason}` : ""}
                  </small>
                </span>
                <Button
                  disabled={testingHostId === host.id}
                  onClick={() =>
                    (() => {
                      setTestingHostId(host.id);
                      return onTestHost(host.id)
                        .catch(onError)
                        .finally(() => setTestingHostId(null));
                    })()
                  }
                >
                  {testingHostId === host.id ? "Testing…" : "Test"}
                </Button>
                {confirmDeleteHostId === host.id ? (
                  <>
                    <Button
                      className="danger"
                      disabled={deletingHostId === host.id}
                      onClick={() => {
                        setDeletingHostId(host.id);
                        void onDeleteHost(host.id)
                          .catch(onError)
                          .finally(() => {
                            setDeletingHostId(null);
                            setConfirmDeleteHostId(null);
                          });
                      }}
                    >
                      {deletingHostId === host.id ? "Deleting…" : "Confirm"}
                    </Button>
                    <Button onClick={() => setConfirmDeleteHostId(null)}>
                      Cancel
                    </Button>
                  </>
                ) : (
                  <Button onClick={() => setConfirmDeleteHostId(host.id)}>
                    Delete
                  </Button>
                )}
              </div>
            ))}
          </div>
        )}
        {tab === "assets" && (
          <div>
            <h2>Assets</h2>
            <div className="form-grid">
              <input
                value={assetTitle}
                onChange={(event) => setAssetTitle(event.target.value)}
                placeholder="Asset title"
              />
              <textarea
                value={assetBody}
                onChange={(event) => setAssetBody(event.target.value)}
                placeholder="Asset body"
              />
              <Button
                className="primary"
                onClick={() =>
                  command("save_asset", {
                    id: `asset-${Date.now()}`,
                    kind: "knowledge",
                    title: assetTitle,
                    body: assetBody,
                    enabled: true,
                  })
                    .then(() => {
                      setAssetTitle("");
                      setAssetBody("");
                      onRefresh();
                    })
                    .catch(onError)
                }
              >
                Save asset
              </Button>
            </div>
            {assets.map((asset) => (
              <div className="manage-row" key={asset.id}>
                <span>
                  {asset.title} <small>{asset.kind}</small>
                </span>
                <Button
                  onClick={() =>
                    command("delete_asset", { id: asset.id })
                      .then(onRefresh)
                      .catch(onError)
                  }
                >
                  Delete
                </Button>
              </div>
            ))}
            {selected && (
              <div className="inline-actions">
                <Button
                  onClick={() =>
                    command("discover_remote_assets", {
                      sessionId: selected.id,
                    })
                      .then(onRefresh)
                      .catch(onError)
                  }
                >
                  Discover remote
                </Button>
                <Button
                  onClick={() =>
                    command("import_assets", { sessionId: selected.id })
                      .then(onRefresh)
                      .catch(onError)
                  }
                >
                  Import
                </Button>
                <Button
                  onClick={() =>
                    command("export_assets", {
                      sessionId: selected.id,
                      ids: assets.map((asset) => asset.id),
                    }).catch(onError)
                  }
                >
                  Export
                </Button>
              </div>
            )}
          </div>
        )}
        {tab === "mcp" && <McpManage selected={selected} onError={onError} />}
        {tab === "secrets" && (
          <div>
            <h2>Secret metadata</h2>
            {secrets.map((secret) => (
              <div className="manage-row" key={secret.name}>
                <span>{secret.name}</span>
                <span className="muted">
                  {secret.scope} · {secret.purpose}
                </span>
              </div>
            ))}
            <p className="muted">Secret values are never shown.</p>
          </div>
        )}
        {tab === "blueprint" && (
          <div className="form-grid">
            <h2>Remote blueprint</h2>
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
              placeholder="Execute a blueprint command"
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
  useEffect(() => {
    if (selected)
      void command<Array<Record<string, unknown>>>("mcp_tools", {
        sessionId: selected.id,
      })
        .then(setTools)
        .catch(onError);
  }, [selected?.id]);
  return (
    <div>
      <h2>MCP tools</h2>
      {!selected && (
        <p className="muted">Select a session to inspect its host MCP tools.</p>
      )}
      {tools.map((tool) => (
        <div className="manage-row" key={String(tool.name)}>
          <span>{String(tool.name)}</span>
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
    </div>
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
  const load = () =>
    command<Schedule[]>("list_schedules").then(setSchedules).catch(onError);
  useEffect(() => {
    void load();
  }, []);
  return (
    <div className="page">
      <div className="max-w-3xl mx-auto px-7 py-6">
        <PageHeader
          title="Automations"
          subtitle="Run OPCOS playbooks on a schedule"
        />
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
                .map((asset) => ({ value: asset.id, label: asset.title }))}
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
                schedule: { name, sessionId, playbookId, cron, enabled: true },
              })
                .then(load)
                .catch(onError)
            }
          >
            Save automation
          </Button>
        </div>
        <div className="manage-card">
          {schedules.map((schedule) => (
            <div className="manage-row" key={schedule.id}>
              <span>
                <strong>{schedule.name}</strong>
                <small>
                  {schedule.cron} · {schedule.last_result || "never run"}
                </small>
              </span>
              <Button
                onClick={() =>
                  command("run_schedule", { scheduleId: schedule.id })
                    .then(load)
                    .catch(onError)
                }
              >
                Run now
              </Button>
            </div>
          ))}
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
  const [worker, setWorker] = useState("");
  const [prUrl, setPrUrl] = useState("");
  const [message, setMessage] = useState("");
  const [rolesText, setRolesText] = useState(
    '[{"id":"leader","sort_order":0,"session_id":"","state":"Active"}]',
  );
  const load = () =>
    command<Coordination>("coordination_snapshot", { taskId })
      .then(setBoard)
      .catch(onError);
  return (
    <div className="page">
      <div className="max-w-3xl mx-auto px-7 py-6">
        <PageHeader
          title="Activity"
          subtitle="Coordination board, worklog, and session insights"
        />
        <div className="rounded-xl2 border border-line bg-panel p-5 space-y-4">
          <div>
            <label className="field-label">Coordination task ID</label>
            <input
              value={taskId}
              onChange={(event) => setTaskId(event.target.value)}
              placeholder="e.g. task-123"
            />
            <p className="field-help">
              The durable coordination board to observe or update.
            </p>
          </div>
          <div>
            <label className="field-label">Initial roles</label>
            <textarea
              value={rolesText}
              onChange={(event) => setRolesText(event.target.value)}
              placeholder='[{"id":"leader","sort_order":0,"session_id":"","state":"Active"}]'
            />
            <p className="field-help">
              Use the JSON shape shown above when starting a new board.
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
                  .then(load)
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
              placeholder="leader"
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
        <div className="board-grid mt-5">
          <section className="board-column rounded-xl2 border border-line bg-panel p-4">
            <h2>Roles</h2>
            {board?.roles.length ? (
              board.roles.map((role) => (
                <div className="board-card" key={String(role.id)}>
                  <strong>{String(role.id)}</strong>
                  <span>Session: {String(role.session_id)}</span>
                  <span>Order: {String(role.sort_order)}</span>
                  <span>State: {String(role.state)}</span>
                </div>
              ))
            ) : (
              <p className="empty-state">
                No roles loaded yet. Start or observe a board.
              </p>
            )}
          </section>
          <section className="board-column rounded-xl2 border border-line bg-panel p-4">
            <h2>Tasks</h2>
            <div className="inline-actions">
              <input
                value={taskTitle}
                onChange={(event) => setTaskTitle(event.target.value)}
                placeholder="New task"
              />
              <input
                value={worker}
                onChange={(event) => setWorker(event.target.value)}
                placeholder="Worker / assignee"
              />
              <input
                value={prUrl}
                onChange={(event) => setPrUrl(event.target.value)}
                placeholder="Verified PR URL"
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
            {board?.tasks.length ? (
              board.tasks.map((task) => (
                <div className="board-card" key={String(task.id || task.title)}>
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
              <p className="empty-state">No coordination tasks yet.</p>
            )}
          </section>
          <section className="board-column rounded-xl2 border border-line bg-panel p-4">
            <h2>Messages</h2>
            <label className="field-label">Message envelope</label>
            <textarea
              value={message}
              onChange={(event) => setMessage(event.target.value)}
              placeholder='{"kind":"status","payload":{}}'
            />
            <p className="field-help">
              Messages use the coordination envelope accepted by the remote
              host.
            </p>
            <Button
              className="bordered"
              disabled={!taskId}
              onClick={() => {
                try {
                  const envelope = JSON.parse(message);
                  void command("coordination_message", { taskId, envelope })
                    .then(load)
                    .catch(onError);
                } catch {
                  onError("Message must be valid JSON.");
                }
              }}
            >
              Send message
            </Button>
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
              <p className="empty-state">No coordination messages yet.</p>
            )}
          </section>
        </div>
      </div>
    </div>
  );
}
function PageHeader({ title, subtitle }: { title: string; subtitle: string }) {
  return (
    <header className="page-header">
      <div>
        <h1>{title}</h1>
        <p>{subtitle}</p>
      </div>
    </header>
  );
}

export function App() {
  const [hosts, setHosts] = useState<Host[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [selected, setSelected] = useState<Session | null>(null);
  const [transcript, setTranscript] = useState<TranscriptViewItem[]>([]);
  const [surface, setSurface] = useState<
    "session" | "automations" | "manage" | "activity"
  >("session");
  const [settingsTab, setSettingsTab] = useState<SettingsSection>("provider");
  const [tab, setTab] = useState<SurfaceTab>("chat");
  const [query, setQuery] = useState("");
  const [error, setError] = useState("");
  const [running, setRunning] = useState(false);
  const [modal, setModal] = useState(false);
  const [hostName, setHostName] = useState("");
  const [hostUrl, setHostUrl] = useState("");
  const [hostToken, setHostToken] = useState("");
  const [assets, setAssets] = useState<Asset[]>([]);
  const [providers, setProviders] = useState<ProviderDescriptor[]>([]);
  const [secrets, setSecrets] = useState<SecretMetadata[]>([]);
  const [models, setModels] = useState<Array<{ id: string; label: string }>>(
    [],
  );
  const [secretBackend, setSecretBackend] = useState("");
  const generation = useRef(0);
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
      if (
        payload.kind === "notice" &&
        ["interrupted", "error"].includes(String(payload.payload?.kind))
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
  const createSession = async (
    title: string,
    hostId: string,
    model: string,
    mode: string,
    workspace: string,
  ) => {
    try {
      const next = await command<Session>("create_session", {
        title,
        hostId,
        model,
        mode,
        workspace: workspace || null,
      });
      setModal(false);
      await refresh();
      setSelected(next);
    } catch (reason) {
      onError(reason);
    }
  };
  const activeItems = useMemo(() => transcript, [transcript]);
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
  const steer = (text: string) => {
    if (!selected) return;
    void command("steering", { sessionId: selected.id, text }).catch(onError);
  };
  const approve = (callId: string, allow: boolean) => {
    if (!selected) return;
    void command("resolve_approval", {
      sessionId: selected.id,
      callId,
      approve: allow,
    }).catch(onError);
  };
  const tabs: Array<{ id: SurfaceTab; label: string; icon: string }> = [
    { id: "chat", label: "Chat", icon: "send" },
    { id: "terminal", label: "Terminal", icon: "terminal" },
    { id: "desktop", label: "Desktop", icon: "desktop" },
    { id: "browser", label: "Browser", icon: "browser" },
    { id: "ide", label: "IDE", icon: "code" },
    { id: "review", label: "Review", icon: "search" },
    { id: "worklog", label: "Worklog", icon: "activity" },
  ];
  return (
    <div className="app">
      <Sidebar
        hosts={hosts}
        sessions={sessions}
        selected={selected}
        query={query}
        onQuery={setQuery}
        onSelect={(session: Session) => {
          setSelected(session);
          setSurface("session");
          setTab("chat");
        }}
        onNew={() => setModal(true)}
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
      />
      <main className="main">
        {surface === "session" && selected ? (
          <>
            <header className="session-header">
              <div>
                <h1>{selected.title}</h1>
                <p>
                  Bound permanently to <strong>{selected.host_name}</strong> ·{" "}
                  {selected.workspace || "workspace not set"}
                </p>
              </div>
              <div className="header-actions">
                {secretBackend && (
                  <span className="backend-badge">
                    Secrets: {secretBackend}
                  </span>
                )}
                <label>
                  Model
                  <OpenWorkerSelectMenu
                    value={selected.model}
                    onChange={(model) =>
                      command("change_model", { sessionId: selected.id, model })
                        .then(() => setSelected({ ...selected, model }))
                        .catch(onError)
                    }
                    options={[
                      ...[
                        { id: selected.model, label: selected.model },
                        ...models,
                      ]
                        .filter(
                          (model, index, values) =>
                            values.findIndex((item) => item.id === model.id) ===
                            index,
                        )
                        .map((model) => ({
                          value: model.id,
                          label: model.label,
                        })),
                    ]}
                    ariaLabel="Model"
                  />
                </label>
                {running && (
                  <Button
                    onClick={() =>
                      command("interrupt", { sessionId: selected.id }).catch(
                        onError,
                      )
                    }
                  >
                    <Icon name="stop" /> Interrupt
                  </Button>
                )}
              </div>
            </header>
            <nav className="surface-tabs">
              {tabs.map((item) => (
                <button
                  className={tab === item.id ? "active" : ""}
                  key={item.id}
                  onClick={() => setTab(item.id)}
                >
                  <Icon name={item.icon} />
                  {item.label}
                </button>
              ))}
            </nav>
            <div className="main-content">
              {tab === "chat" ? (
                <>
                  <Transcript
                    items={activeItems}
                    running={running}
                    onApprove={(id: string) => approve(id, true)}
                    onDeny={(id: string) => approve(id, false)}
                  />
                  <Composer
                    selected={selected}
                    running={running}
                    approvalPending={approvalPending}
                    onSubmit={submit}
                    onSteer={steer}
                    onInterrupt={() =>
                      command("interrupt", { sessionId: selected.id }).catch(
                        onError,
                      )
                    }
                  />
                </>
              ) : (
                <SurfaceView tab={tab} selected={selected} onError={onError} />
              )}
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
          <div className="empty-main">
            <h1>Start a session</h1>
            <p>Choose a bound host and create an OPCOS workspace.</p>
            <Button className="primary" onClick={() => setModal(true)}>
              <Icon name="plus" /> New session
            </Button>
          </div>
        )}
        {error && (
          <div className="error-banner">
            {error}
            <button onClick={() => setError("")}>×</button>
          </div>
        )}
      </main>
      {surface === "session" && (
        <RightRail
          selected={selected}
          running={running}
          items={activeItems}
          assets={assets}
          onAsset={toggleAsset}
          onMcp={(name: string, enabled: boolean) =>
            selected &&
            command("set_mcp_tool_enabled", {
              sessionId: selected.id,
              name,
              enabled,
            }).catch(onError)
          }
          onError={onError}
        />
      )}
      {modal && (
        <NewSessionModal
          hosts={hosts}
          onClose={() => setModal(false)}
          onCreate={(title, hostId, model, mode, workspace) =>
            void createSession(title, hostId, model, mode, workspace)
          }
        />
      )}
    </div>
  );
}
