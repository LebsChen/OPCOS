import { redactApproval } from "../gui";
import { TranscriptViewItem, toolArgumentSummary } from "../transcript";
import { Button } from "./ui";

export function ToolCard({
  item,
  onApprove,
  onDeny,
  running,
}: {
  item: TranscriptViewItem;
  onApprove: (id: string) => void;
  onDeny: (id: string) => void;
  running: boolean;
}) {
  return (
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
              <Button onClick={() => onDeny(item.callId || "")}>Deny</Button>
            </div>
          </div>
        )}
      </div>
    </details>
  );
}
