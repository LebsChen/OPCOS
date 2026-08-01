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
import { NewSessionModal } from "./components/NewSessionModal";
import { Transcript } from "./components/Transcript";
import { Composer } from "./components/Composer";
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
    const pending: string[] = [];
    const send = (data: string) => {
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
    const input = terminal.onData(send);
    terminal.onResize(({ cols, rows }) =>
      send(JSON.stringify({ type: "resize", cols, rows })),
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
        <div className="surface-toolbar">
          <span>{translate("Remote Web IDE")}</span>
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
            title={translate("Remote Web IDE")}
            src={`http://127.0.0.1:${idePort}/`}
            className="ide-frame"
          />
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
            <GitActions selected={selected} cwd={cwd} onError={onError} />
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
  const events = Array.isArray(worklog?.events) ? worklog.events : [];
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
      {events.map((event) => (
        <div className="timeline-row" key={JSON.stringify(event)}>
          <span className="timeline-dot" />
          <pre>{String(JSON.stringify(event, null, 2))}</pre>
        </div>
      ))}
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
  const [assetTitle, setAssetTitle] = useState("");
  const [assetBody, setAssetBody] = useState("");
  const [assetKind, setAssetKind] = useState<Asset["kind"]>("knowledge");
  const [assetTrigger, setAssetTrigger] = useState("");
  const [assetScope, setAssetScope] = useState("");
  const [editingAssetId, setEditingAssetId] = useState<string | null>(null);
  const [assetPending, setAssetPending] = useState<string | null>(null);
  const [assetFormOpen, setAssetFormOpen] = useState(false);
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
    agents: ["AGENTS.md", "Repository-wide operating guidance for the host."],
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
      ? "AGENTS.md"
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
        {tab === "provider" && (
          <div className="divide-y divide-line">
            {providers.map((descriptor) => {
              const config = providerConfigs.find(
                (item) => item.provider === descriptor.name,
              );
              const currentUrl =
                config?.base_url || descriptor.default_base_url || "";
              return (
                <div
                  className="py-4 first:pt-0 last:pb-0"
                  key={descriptor.name}
                >
                  <div className="settings-row">
                    <div>
                      <strong>{descriptor.title}</strong>
                      <small>
                        {config?.configured
                          ? "Configured securely."
                          : "Not configured yet."}
                      </small>
                    </div>
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
                      ).map((item) => ({ value: item.id, label: item.label }))}
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
                      value={currentUrl}
                      onChange={(event) =>
                        setProviderConfigs((items) =>
                          items.map((item) =>
                            item.provider === descriptor.name
                              ? { ...item, base_url: event.target.value }
                              : item,
                          ),
                        )
                      }
                    />
                  </div>
                  {descriptor.needs_key && (
                    <div className="settings-row">
                      <div>
                        <strong>{translate("Provider key")}</strong>
                        <small>
                          Stored securely and never returned to the UI.
                        </small>
                      </div>
                      <input
                        type="password"
                        value={providerKeys[descriptor.name] || ""}
                        onChange={(event) =>
                          setProviderKeys((items) => ({
                            ...items,
                            [descriptor.name]: event.target.value,
                          }))
                        }
                      />
                    </div>
                  )}
                  <div className="settings-row justify-end">
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
                      Save and validate
                    </Button>
                  </div>
                  {providerStatuses[descriptor.name] && (
                    <div
                      className={
                        providerStatuses[descriptor.name].includes("failed")
                          ? "failure"
                          : "success"
                      }
                    >
                      {providerStatuses[descriptor.name]}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
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
                        disabled={testingHostId === host.id}
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
                  "AGENTS.md",
                  "Repository-wide instructions loaded as the host's operating guidance.",
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
                  searchPlaceholder={`Search ${label}`}
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
                  columns={["Title", "Trigger", "Scope", "Status"]}
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
                                {asset.enabled ? "Enabled" : "Disabled"}
                              </span>
                            </div>
                            <p className="mt-2 text-[13px] text-muted line-clamp-2">
                              {asset.body}
                            </p>
                            <small className="mt-3 block text-muted">
                              {asset.trigger || "No trigger"}
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
                      New {label}
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
                                {asset.enabled ? "Enabled" : "Disabled"}
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
                                  ? "Saving…"
                                  : asset.enabled
                                    ? "Disable"
                                    : "Enable"}
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
                                }}
                              >
                                Edit
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
                                Delete
                              </Button>
                            </span>
                          </div>
                        ))}
                      {!assets.some((asset) => asset.kind === kind) && (
                        <p className="px-4 py-6 text-[13px] text-muted">
                          No {label} assets yet.
                        </p>
                      )}
                    </>
                  }
                  empty={`No ${label} assets yet.`}
                />
              ))}
            {assetFormOpen && (
              <div className="rounded-xl2 border border-line bg-panel p-5">
                <h2 className="text-[15px] font-semibold text-ink">
                  {editingAssetId ? "Edit asset" : "New asset"}
                </h2>
                <div className="form-grid mt-4">
                  <label className="field-label">
                    Title
                    <input
                      value={assetTitle}
                      onChange={(event) => setAssetTitle(event.target.value)}
                      placeholder={translate("Asset title")}
                    />
                  </label>
                  <label className="field-label">
                    Body
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
                    Scope
                    <input
                      value={assetScope}
                      onChange={(event) => setAssetScope(event.target.value)}
                      placeholder={translate("Optional scope")}
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
                    {editingAssetId ? "Save changes" : "Create asset"}
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
  const [search, setSearch] = useState("");
  useEffect(() => {
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
        filtered.length ? (
          <>
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
    "board" | "roles" | "tasks" | "messages" | "worklog" | "insights"
  >("board");
  const [worklog, setWorklog] = useState<Record<string, unknown> | null>(null);
  const [insights, setInsights] = useState<Record<string, unknown> | null>(
    null,
  );
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
              }}
            >
              <Icon
                name={
                  (
                    {
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

function SessionRightPanel({
  selected,
  onError,
  running,
}: {
  selected: Session;
  onError: (error: unknown) => void;
  running: boolean;
}) {
  const [panelTab, setPanelTab] = useState<
    | "info"
    | "terminal"
    | "desktop"
    | "ide"
    | "review"
    | "worklog"
    | "browser"
    | "board"
  >("info");
  const [collapsed, setCollapsed] = useState(false);
  const tabs: Array<{
    id: typeof panelTab;
    label: string;
    icon: Parameters<typeof Icon>[0]["name"];
  }> = [
    { id: "info", label: "Info", icon: "inbox" },
    { id: "terminal", label: "Shell", icon: "code" },
    { id: "desktop", label: "Desktop", icon: "image" },
    { id: "ide", label: "Web IDE", icon: "fileCode" },
    { id: "review", label: "Diff", icon: "search" },
    { id: "worklog", label: "Worklog", icon: "clock" },
    { id: "browser", label: "Browser", icon: "folder" },
    { id: "board", label: "Board", icon: "audit" },
  ];
  if (collapsed) {
    return (
      <aside className="right-rail right-rail-collapsed">
        <button
          className="rail-toggle"
          title={translate("Expand session panel")}
          onClick={() => setCollapsed(false)}
        >
          <Icon name="sidebarRight" />
        </button>
      </aside>
    );
  }
  return (
    <aside className="right-rail session-right-panel">
      <div className="session-panel-tabs">
        {tabs.map((item) => (
          <button
            key={item.id}
            className={panelTab === item.id ? "active" : ""}
            title={item.label}
            onClick={() => setPanelTab(item.id)}
          >
            <Icon name={item.icon} />
          </button>
        ))}
        <button
          className="panel-collapse"
          title={translate("Collapse session panel")}
          onClick={() => setCollapsed(true)}
        >
          <Icon name="sidebarRight" />
        </button>
      </div>
      <div className="session-panel-content">
        {panelTab === "info" && (
          <div className="p-4">
            <h2 className="text-[15px] font-semibold text-ink">
              {translate("Session")}
            </h2>
            <dl className="mt-4 space-y-3 text-[13px]">
              <div>
                <dt className="text-muted">{translate("Status")}</dt>
                <dd>{running ? "Running" : "Ready"}</dd>
              </div>
              <div>
                <dt className="text-muted">{translate("Host")}</dt>
                <dd>{selected.host_name}</dd>
              </div>
              <div>
                <dt className="text-muted">{translate("Workspace")}</dt>
                <dd>{selected.workspace || "Not set"}</dd>
              </div>
              <div>
                <dt className="text-muted">{translate("Model")}</dt>
                <dd>{selected.model}</dd>
              </div>
            </dl>
          </div>
        )}
        {panelTab !== "info" && panelTab !== "board" && (
          <SurfaceView
            tab={panelTab === "review" ? "review" : panelTab}
            selected={selected}
            onError={onError}
          />
        )}
        {panelTab === "board" && (
          <div className="p-4 text-[13px] text-muted">
            Coordination Board is available from Activity → Board.
          </div>
        )}
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
      if (payload.kind === "turn_done") setRunning(false);
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
    provider: string,
    mode: string,
    workspace: string,
  ) => {
    try {
      const next = await command<Session>("create_session", {
        title,
        hostId,
        model,
        provider: provider || null,
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
      if (item.kind === "tool" && item.approval)
        output.push({
          kind: "approval",
          callId: item.callId,
          name: item.toolName || "approval",
          args: item.arguments,
          reason: item.text || "Tool action requires approval",
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
          resolved: item.status === "ok" ? "once" : undefined,
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
  const steer = (text: string) => {
    if (!selected) return;
    void command("steering", { sessionId: selected.id, text }).catch(onError);
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
    <div
      className={`app ${surface === "session" ? "session-layout" : "surface-layout"}`}
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
                  Provider
                  <OpenWorkerSelectMenu
                    value={selected.provider || ""}
                    onChange={(provider) =>
                      command("change_provider", {
                        sessionId: selected.id,
                        provider: provider || null,
                      })
                        .then(() => setSelected({ ...selected, provider }))
                        .catch(onError)
                    }
                    options={[
                      { value: "", label: "Global default" },
                      ...providers.map((item) => ({
                        value: item.name,
                        label: item.title,
                      })),
                    ]}
                    ariaLabel="Provider"
                  />
                </label>
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
                  <input
                    key={selected.model}
                    defaultValue={selected.model}
                    placeholder={translate("customModel")}
                    onBlur={(event) => {
                      const model = event.target.value.trim();
                      if (!model || model === selected.model) return;
                      void command("change_model", {
                        sessionId: selected.id,
                        model,
                      })
                        .then(() => setSelected({ ...selected, model }))
                        .catch(onError);
                    }}
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
                  <Icon
                    name={item.icon as import("./components/Icon").IconName}
                  />
                  {item.label}
                </button>
              ))}
            </nav>
            <div className="main-content">
              {tab === "chat" ? (
                <>
                  <Transcript
                    items={transcriptItems}
                    running={running}
                    onApprove={(item, decision) => {
                      if (!item.callId) return;
                      void command("resolve_approval", {
                        sessionId: selected.id,
                        callId: item.callId,
                        approve: decision === "once",
                      }).catch(onError);
                    }}
                  />
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
                    resetKey={selected.id}
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
            <h1>{translate("startSession")}</h1>
            <p>{translate("chooseHost")}</p>
            <Button className="primary" onClick={() => setModal(true)}>
              <Icon name="plus" /> {translate("newSession")}
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
      {surface === "session" && selected && (
        <SessionRightPanel
          selected={selected}
          running={running}
          onError={onError}
        />
      )}
      {modal && (
        <NewSessionModal
          hosts={hosts}
          providers={providers}
          models={models}
          onClose={() => setModal(false)}
          onCreate={(title, hostId, model, provider, mode, workspace) =>
            void createSession(title, hostId, model, provider, mode, workspace)
          }
        />
      )}
    </div>
  );
}

export function App() {
  return (
    <AppErrorBoundary>
      <AppContent />
    </AppErrorBoundary>
  );
}
