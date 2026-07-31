import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { FormEvent, useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import {
  Host,
  Session,
  TranscriptItem,
  hostFailureMessage,
  noticeClass,
  redactApproval,
} from "./gui";
import "./style.css";

type UiEvent = {
  kind: string;
  session_id?: string;
  payload: Record<string, unknown>;
};
type ProviderDescriptor = { name: string; title: string };

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
      setError(String(reason));
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
              </div>
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
            type="password"
            value={providerKey}
            onChange={(event) => setProviderKey(event.target.value)}
            placeholder="Provider key"
          />
          <button
            onClick={() => {
              void command("save_provider_key", {
                provider,
                key: providerKey,
              })
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
