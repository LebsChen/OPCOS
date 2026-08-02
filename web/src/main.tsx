import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { FormEvent, useEffect, useState } from "react";
import { useRef } from "react";
import { createRoot } from "react-dom/client";
import { Terminal } from "@xterm/xterm";
import RFB from "@novnc/novnc";
import "@xterm/xterm/css/xterm.css";
import {
  Host,
  Session,
  TranscriptItem,
  hostFailureMessage,
  noticeClass,
  redactApproval,
  submitFailureMessage,
} from "./gui";
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

async function command<T>(
  name: string,
  args?: Record<string, unknown>,
): Promise<T> {
  return invoke<T>(name, args);
}

function App() {
  const [hosts, setHosts] = useState<Host[]>([]);
  const [sessions, setSessions] = useState<Session[]>([]);
  const [selected, setSelected] = useState<Session | null>(null);
  const [transcript, setTranscript] = useState<TranscriptItem[]>([]);
  const [hostName, setHostName] = useState("");
  const [hostUrl, setHostUrl] = useState("");
  const [hostToken, setHostToken] = useState("");
  const [sessionTitle, setSessionTitle] = useState("");
  const [selectedHost, setSelectedHost] = useState("");
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const [provider, setProvider] = useState("openai");
  const [providerKey, setProviderKey] = useState("");
  const [providers, setProviders] = useState<ProviderDescriptor[]>([]);
  const [baseUrl, setBaseUrl] = useState("");
  const [surfacePorts, setSurfacePorts] = useState<Record<string, number>>({});
  const [idePort, setIdePort] = useState<number | null>(null);
  const [assets, setAssets] = useState<Asset[]>([]);
  const [secretMetadata, setSecretMetadata] = useState<SecretMetadata[]>([]);
  const [review, setReview] = useState<Record<string, unknown> | null>(null);
  const [worklog, setWorklog] = useState<Record<string, unknown> | null>(null);
  const terminalHost = useRef<HTMLDivElement>(null);
  const vncHost = useRef<HTMLDivElement>(null);

  const refresh = async () => {
    setHosts(await command<Host[]>("list_hosts"));
    const next = await command<Session[]>("list_sessions");
    setSessions(next);
    if (selected) {
      const current = next.find((item) => item.id === selected.id);
      if (current) setSelected(current);
    }
  };

  useEffect(() => {
    void refresh().catch((reason: unknown) => setError(String(reason)));
    void command<ProviderDescriptor[]>("provider_descriptors")
      .then(setProviders)
      .catch((reason: unknown) => setError(String(reason)));
    void command<{ provider: string; base_url?: string }>("provider_settings")
      .then((settings) => {
        setProvider(settings.provider);
        setBaseUrl(settings.base_url || "");
      })
      .catch((reason: unknown) => setError(String(reason)));
    void command<Asset[]>("list_assets")
      .then(setAssets)
      .catch((reason: unknown) => setError(String(reason)));
    void command<SecretMetadata[]>("list_secret_metadata")
      .then(setSecretMetadata)
      .catch((reason: unknown) => setError(String(reason)));
    let active = true;
    const subscription = listen<UiEvent>("opcos://event", (event) => {
      if (!active || !selected) return;
      if (
        event.payload.session_id &&
        event.payload.session_id !== selected.id
      ) {
        return;
      }
      setTranscript((items) => [
        ...items,
        { kind: event.payload.kind || event.event, payload: event.payload.payload },
      ]);
    });
    return () => {
      active = false;
      void subscription.then((unlisten) => unlisten());
    };
  }, [selected?.id]);

  useEffect(() => {
    if (selected) {
      void command<TranscriptItem[]>("read_transcript", {
        sessionId: selected.id,
      })
        .then(setTranscript)
        .catch((reason: unknown) => setError(String(reason)));
    }
  }, [selected?.id]);

  const addHost = async (event: FormEvent) => {
    event.preventDefault();
    setError("");
    try {
      await command<Host>("save_host", {
        name: hostName,
        url: hostUrl,
        token: hostToken,
      });
      setHostName("");
      setHostUrl("");
      setHostToken("");
      await refresh();
    } catch (reason) {
      setError(submitFailureMessage(reason));
    }
  };

  const createSession = async (event: FormEvent) => {
    event.preventDefault();
    if (!selectedHost) return;
    try {
      const session = await command<Session>("create_session", {
        title: sessionTitle || "New session",
        hostId: selectedHost,
      });
      setSessions((items) => [session, ...items]);
      setSelected(session);
      setSessionTitle("");
    } catch (reason) {
      setError(String(reason));
    }
  };

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (!selected || !text.trim()) return;
    try {
      await command("submit_turn", {
        request: { session_id: selected.id, text },
      });
      setText("");
    } catch (reason) {
      setError(String(reason));
      await refresh();
    }
  };

  const testHost = async (host: Host) => {
    const result = await command<Host>("test_host", { hostId: host.id });
    setHosts((items) =>
      items.map((item) => (item.id === result.id ? result : item)),
    );
  };

  const startSurface = async (surface: string) => {
    if (!selected) return;
    try {
      const port = await command<number>("start_surface", {
        hostId: selected.host_id,
        surface,
        cols: 120,
        rows: 32,
      });
      setSurfacePorts((items) => ({ ...items, [surface]: port }));
      setError("");
    } catch (reason) {
      setError(String(reason));
    }
  };

  const startIde = async () => {
    if (!selected) return;
    try {
      const port = await command<number>("start_ide_proxy", {
        sessionId: selected.id,
        folderUri: `vscode-remote://${selected.host_name}/workspace`,
      });
      setIdePort(port);
    } catch (reason) {
      setError(String(reason));
    }
  };

  const refreshReview = async () => {
    if (!selected || !selected.workspace) return;
    try {
      setReview(
        await command<Record<string, unknown>>("review_snapshot", {
          sessionId: selected.id,
          cwd: selected.workspace,
          base: "HEAD",
        }),
      );
    } catch (reason) {
      setError(String(reason));
    }
  };

  const refreshWorklog = async () => {
    if (!selected) return;
    try {
      setWorklog(
        await command<Record<string, unknown>>("session_worklog", {
          sessionId: selected.id,
          afterId: "",
          limit: 200,
        }),
      );
    } catch (reason) {
      setError(String(reason));
    }
  };

  useEffect(() => {
    const port = surfacePorts.pty;
    if (!port || !terminalHost.current) return;
    const terminal = new Terminal({ convertEol: true, cursorBlink: true });
    terminal.open(terminalHost.current);
    const socket = new WebSocket(`ws://127.0.0.1:${port}`);
    socket.binaryType = "arraybuffer";
    socket.onmessage = (event) => {
      terminal.write(
        typeof event.data === "string"
          ? event.data
          : new Uint8Array(event.data as ArrayBuffer),
      );
    };
    const input = terminal.onData((data) => socket.send(data));
    terminal.onResize(({ cols, rows }) =>
      socket.send(JSON.stringify({ type: "resize", cols, rows })),
    );
    return () => {
      input.dispose();
      socket.close();
      terminal.dispose();
    };
  }, [surfacePorts.pty]);

  useEffect(() => {
    const port = surfacePorts.vnc;
    if (!port || !vncHost.current) return;
    const rfb = new RFB(vncHost.current, `ws://127.0.0.1:${port}`);
    rfb.scaleViewport = true;
    return () => rfb.disconnect();
  }, [surfacePorts.vnc]);

  return (
    <div className="app">
      <header>
        <strong>OPCOS</strong>
        <span>Local Devin client</span>
        <button onClick={() => void refresh()}>Refresh</button>
      </header>
      <div className="layout">
        <aside>
          <section>
            <h2>Workbench surfaces</h2>
            <p className="muted">
              RVM WebSockets terminate in Rust; the UI only receives loopback
              bridge ports.
            </p>
            {(["pty", "vnc", "cdp"] as const).map((surface) => (
              <div key={surface}>
                <button onClick={() => void startSurface(surface)}>
                  Start {surface.toUpperCase()}
                </button>
                {surfacePorts[surface] && (
                  <small>ws://127.0.0.1:{surfacePorts[surface]}</small>
                )}
              </div>
            ))}
            <button onClick={() => void startIde()}>Open Web IDE</button>
            {idePort && (
              <iframe
                title="Remote Web IDE"
                src={`http://127.0.0.1:${idePort}/`}
                className="ide-frame"
              />
            )}
            {surfacePorts.pty && <div ref={terminalHost} className="terminal" />}
            {surfacePorts.vnc && <div ref={vncHost} className="vnc" />}
          </section>
          <section>
            <h2>Hosts</h2>
            <form onSubmit={(event) => void addHost(event)}>
              <input
                value={hostName}
                onChange={(event) => setHostName(event.target.value)}
                placeholder="Name"
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
              <button>Add remote host</button>
            </form>
            {hosts.map((host) => (
              <div className="host" key={host.id}>
                <span>{host.name}</span>
                <button onClick={() => void testHost(host)}>Test</button>
                {host.online === false && (
                  <small className="failure">
                    {hostFailureMessage(host)} · Retry available
                  </small>
                )}
                {host.online === true && (
                  <small className="online">Online</small>
                )}
              </div>
            ))}
          </section>
          <section>
            <h2>Sessions</h2>
            <form onSubmit={(event) => void createSession(event)}>
              <input
                value={sessionTitle}
                onChange={(event) => setSessionTitle(event.target.value)}
                placeholder="Session title"
              />
              <select
                value={selectedHost}
                onChange={(event) => setSelectedHost(event.target.value)}
                required
              >
                <option value="">Select bound host</option>
                {hosts.map((host) => (
                  <option value={host.id} key={host.id}>
                    {host.name}
                  </option>
                ))}
              </select>
              <button>Create session</button>
            </form>
            {sessions.map((session) => (
              <button
                className={`session ${
                  selected?.id === session.id ? "selected" : ""
                }`}
                key={session.id}
                onClick={() => setSelected(session)}
              >
                {session.title}
                <small>
                  {session.host_name} · {session.mode}
                </small>
              </button>
            ))}
          </section>
        </aside>
        <main>
          {selected ? (
            <>
              <div className="session-header">
                <div>
                  <h1>{selected.title}</h1>
                  <small>
                    Bound permanently to {selected.host_name} ·{" "}
                    {selected.model}
                  </small>
                </div>
                <button
                  onClick={() =>
                    void command("interrupt", { sessionId: selected.id })
                  }
                >
                  Interrupt
                </button>
                <button onClick={() => void refreshReview()}>Review</button>
                <button onClick={() => void refreshWorklog()}>Worklog</button>
              </div>
              {review && (
                <details open>
                  <summary>Remote review</summary>
                  <pre>{JSON.stringify(review, null, 2)}</pre>
                </details>
              )}
              {worklog && (
                <details>
                  <summary>
                    Worklog timeline
                    {worklog.window_lost ? " · window lost" : ""}
                  </summary>
                  <pre>{JSON.stringify(worklog, null, 2)}</pre>
                </details>
              )}
              <div className="transcript">
                {transcript.map((item, index) => (
                  <article
                    className={noticeClass(item.kind)}
                    key={`${item.kind}-${index}`}
                  >
                    <label>{item.kind}</label>
                    <pre>
                      {(item.payload.text as string) ||
                        (item.payload.message as string) ||
                        redactApproval(item.payload)}
                    </pre>
                    {item.kind === "approval" && (
                      <div>
                        <button
                          onClick={() =>
                            void command("resolve_approval", {
                              sessionId: selected.id,
                              callId: item.payload.call_id,
                              approve: true,
                            })
                          }
                        >
                          Approve
                        </button>
                        <button
                          onClick={() =>
                            void command("resolve_approval", {
                              sessionId: selected.id,
                              callId: item.payload.call_id,
                              approve: false,
                            })
                          }
                        >
                          Deny
                        </button>
                      </div>
                    )}
                  </article>
                ))}
              </div>
              <form
                className="composer"
                onSubmit={(event) => void submit(event)}
              >
                <textarea
                  value={text}
                  onChange={(event) => setText(event.target.value)}
                  placeholder="Ask OPCOS to work on the bound host…"
                />
                <button>Send</button>
              </form>
            </>
          ) : (
            <div className="empty">
              <h1>Start a session</h1>
              <p>
                Select a remote host and create a permanently bound session.
              </p>
            </div>
          )}
          {error && <div className="error-banner">{error}</div>}
        </main>
        <aside className="settings">
          <h2>Assets</h2>
          <button
            disabled={!selected}
            onClick={() => {
              if (!selected) return;
              void command<unknown>("discover_remote_assets", {
                sessionId: selected.id,
              })
                .then(() => command<Asset[]>("list_assets"))
                .then(setAssets)
                .catch((reason: unknown) => setError(String(reason)));
            }}
          >
            Discover repository assets
          </button>
          {assets.map((asset) => (
            <div className="asset-row" key={asset.id}>
              <strong>{asset.title}</strong>
              <span className="muted">{asset.kind}</span>
            </div>
          ))}
          <p className="muted">
            {secretMetadata.length} configured secrets; values are never shown.
          </p>
          <h2>Provider</h2>
          <select
            value={provider}
            onChange={(event) => setProvider(event.target.value)}
          >
            {providers.map((item) => (
              <option value={item.name} key={item.name}>
                {item.title}
              </option>
            ))}
          </select>
          <input
            value={baseUrl}
            onChange={(event) => setBaseUrl(event.target.value)}
            placeholder="Provider base URL (optional when registry has a default)"
            type="url"
          />
          <input
            type="password"
            value={providerKey}
            onChange={(event) => setProviderKey(event.target.value)}
            placeholder="Provider key"
          />
          <button
            onClick={() => {
              void command("save_provider_settings", {
                provider,
                baseUrl: baseUrl || null,
              })
                .then(() =>
                  command("save_provider_key", {
                    provider,
                    key: providerKey,
                  }),
                )
                .then(() => command("validate_provider_key", { provider }))
                .then(() => {
                  setProviderKey("");
                  setError("");
                })
                .catch((reason: unknown) => setError(String(reason)));
            }}
          >
            Save and validate
          </button>
          <p className="muted">
            Keys are stored by the Rust SecretStore and are never returned to
            the UI.
          </p>
        </aside>
      </div>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
