import { useState } from "react";
import { Host } from "../gui";
import { Button, SelectMenu } from "./ui";

export function NewSessionModal({
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
    <div className="modal-backdrop fixed inset-0 z-50 grid place-items-center bg-black/30">
      <form
        className="modal w-[420px] rounded-xl2 border border-line bg-panel p-5 shadow-xl"
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
