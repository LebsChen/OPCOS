import { useState } from "react";
import { Host } from "../gui";
import { Button, SelectMenu } from "./ui";
import { translate } from "../i18n";

type ProviderOption = { name: string; title: string };
type ModelOption = { id: string; label: string };

export function NewSessionModal({
  hosts,
  providers,
  models,
  onClose,
  onCreate,
}: {
  hosts: Host[];
  providers: ProviderOption[];
  models: ModelOption[];
  onClose: () => void;
  onCreate: (
    title: string,
    hostId: string,
    model: string,
    provider: string,
    mode: string,
    workspace: string,
  ) => void;
}) {
  const [title, setTitle] = useState("");
  const [hostId, setHostId] = useState(hosts[0]?.id || "");
  const [model, setModel] = useState("auto");
  const [provider, setProvider] = useState(providers[0]?.name || "");
  const [mode, setMode] = useState("Interactive");
  const [workspace, setWorkspace] = useState("");
  return (
    <div className="modal-backdrop fixed inset-0 z-50 grid place-items-center bg-black/30">
      <form
        className="modal w-[420px] rounded-xl2 border border-line bg-panel p-5 shadow-xl"
        onSubmit={(event) => {
          event.preventDefault();
          onCreate(
            title || translate("newSession"),
            hostId,
            model,
            provider,
            mode,
            workspace,
          );
        }}
      >
        <div className="modal-head">
          <h2>{translate("newSession")}</h2>
          <button type="button" className="close" onClick={onClose}>
            ×
          </button>
        </div>
        <label>
          {translate("title")}
          <input
            value={title}
            onChange={(event) => setTitle(event.target.value)}
            placeholder={translate("sessionPrompt")}
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
          {translate("provider")}
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
          Model
          <SelectMenu
            value={model}
            onChange={setModel}
            options={[
              { value: "auto", label: "Auto" },
              ...models
                .filter(
                  (item) =>
                    item.id.startsWith(`${provider}:`) ||
                    provider === "openai" ||
                    provider === "anthropic",
                )
                .map((item) => ({ value: item.id, label: item.label })),
            ]}
          />
          <input
            value={model}
            onChange={(event) => setModel(event.target.value)}
            placeholder={translate("customModel")}
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
            placeholder={translate("workspacePath")}
          />
        </label>
        <div className="modal-actions">
          <Button type="button" onClick={onClose}>
            {translate("cancel")}
          </Button>
          <Button className="primary" disabled={!hostId}>
            {translate("createSession")}
          </Button>
        </div>
      </form>
    </div>
  );
}
