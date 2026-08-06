import { invoke } from "@tauri-apps/api/core";
import { useState } from "react";
import { type ApprovalDecision, type Item } from "../types";
import { buildTimeline, type TimelineEvent } from "../timeline";
import { ApprovalCard } from "./ApprovalCard";
import { Markdown } from "./Markdown";

function BubbleMeta({ text, ts }: { text: string; ts?: number }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    navigator.clipboard
      ?.writeText(text)
      .then(() => {
        setCopied(true);
        window.setTimeout(() => setCopied(false), 1200);
      })
      .catch(() => {});
  };
  return (
    <div className="relative h-0 select-none">
      <div className="absolute top-1 left-0 flex items-center gap-1.5 text-[10.5px] text-faint opacity-0 group-hover:opacity-100">
        <button onClick={copy} data-testid="bubble-copy">
          {copied ? "Copied" : "Copy"}
        </button>
        {typeof ts === "number" && (
          <span data-testid="bubble-ts">
            {new Date(ts * 1000).toLocaleTimeString([], {
              hour: "numeric",
              minute: "2-digit",
            })}
          </span>
        )}
      </div>
    </div>
  );
}

function Thought({ text }: { text: string }) {
  return (
    <details>
      <summary className="cursor-pointer text-xs text-muted">
        Thought details
      </summary>
      <div className="whitespace-pre-wrap">{text}</div>
    </details>
  );
}

function ArtifactRow({
  sessionId,
  artifactId,
  kind,
  mime,
}: {
  sessionId: string;
  artifactId: string;
  kind?: string;
  mime?: string;
}) {
  const [open, setOpen] = useState(false);
  const [content, setContent] = useState<Record<string, unknown> | null>(null);
  const [error, setError] = useState("");
  const load = () => {
    if (content || error) {
      setOpen((value) => !value);
      return;
    }
    setOpen(true);
    void invoke<Record<string, unknown>>("read_artifact", {
      sessionId,
      artifactId,
    })
      .then(setContent)
      .catch((reason) => setError(String(reason)));
  };
  const image =
    typeof content?.content_base64 === "string"
      ? `data:${String(content.mime ?? mime ?? "image/png")};base64,${content.content_base64}`
      : null;
  const diff =
    kind === "diff" && typeof content?.content === "string"
      ? String(content.content)
          .split("\n")
          .map((line, index) => (
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
          ))
      : null;
  return (
    <div className="artifact-inline">
      <button className="text-left underline" onClick={load}>
        {kind === "screenshot" ? "View screenshot" : "View diff"}
      </button>
      {open && (
        <div className="mt-1">
          {error ? (
            <span className="text-red-500">{error}</span>
          ) : image ? (
            <img
              className="max-w-full max-h-64 cursor-zoom-in"
              src={image}
              alt="Screenshot artifact"
              onClick={() =>
                window.open(image, "_blank", "noopener,noreferrer")
              }
            />
          ) : content ? (
            <pre className="artifact-code whitespace-pre-wrap">
              {diff ?? String(content.content ?? "")}
            </pre>
          ) : (
            <span className="text-muted">Loading…</span>
          )}
        </div>
      )}
    </div>
  );
}

export function Transcript({
  events,
  sessionId,
  running,
  onApprove,
  onRetry,
}: {
  events: TimelineEvent[];
  sessionId: string;
  running?: boolean;
  onApprove?: (
    item: Extract<Item, { kind: "approval" }>,
    decision: ApprovalDecision,
  ) => void;
  onRetry?: () => void;
}) {
  const nodes = buildTimeline(events);
  return (
    <div className="transcript">
      {nodes.map((node, index) => {
        if (node.kind === "user")
          return (
            <div
              className="group self-end max-w-[78%] flex flex-col items-end"
              key={index}
            >
              <div className="bubble-user px-3.5 py-2.5 rounded-[14px_14px_4px_14px] bg-solid text-onSolid text-[14.5px] leading-relaxed whitespace-pre-wrap">
                {node.attachments?.map((attachment) =>
                  attachment.kind === "image" ? (
                    <img
                      key={attachment.name}
                      className="msg-img"
                      src={attachment.data_url}
                      alt={attachment.name}
                    />
                  ) : (
                    <span key={attachment.name} className="msg-file">
                      📄 {attachment.name}
                    </span>
                  ),
                )}
                {node.text}
              </div>
              <BubbleMeta text={node.text} ts={node.ts} />
            </div>
          );
        if (node.kind === "assistant")
          return (
            <div className="group bubble-assistant" key={index}>
              <Markdown text={node.text} />
              <BubbleMeta text={node.text} ts={node.ts} />
            </div>
          );
        if (node.kind === "approval")
          return (
            <ApprovalCard
              key={index}
              item={{
                kind: "approval",
                callId: node.callId,
                name: node.name,
                args: node.args,
                reason: "Tool action requires approval",
              }}
              compact
              onApprove={(decision) =>
                onApprove?.(
                  {
                    kind: "approval",
                    callId: node.callId,
                    name: node.name,
                    args: node.args,
                    reason: "Tool action requires approval",
                  },
                  decision,
                )
              }
            />
          );
        if (node.kind === "question")
          return (
            <div className="notice" key={index}>
              {node.text}
            </div>
          );
        if (node.kind === "notice")
          return (
            <div className="notice warn" key={index}>
              {node.text}
              {node.retriable && onRetry && !running && (
                <button className="btn ml-2" onClick={onRetry}>
                  Retry
                </button>
              )}
            </div>
          );
        return (
          <details className="work-segment flex flex-col gap-2" key={index}>
            <summary className="cursor-pointer text-muted">
              {node.label} {node.additions ? `+${node.additions}` : ""}{" "}
              {node.deletions ? `−${node.deletions}` : ""}
            </summary>
            <div className="flex flex-col gap-2 text-ink">
              {node.rows.map((row, rowIndex) => (
                <div className="transcript-item" key={rowIndex}>
                  {row.label}
                  {row.detail && <Thought text={row.detail} />}
                  {row.artifactId && (
                    <ArtifactRow
                      sessionId={sessionId}
                      artifactId={row.artifactId}
                      kind={row.artifactKind}
                      mime={row.artifactMime}
                    />
                  )}
                </div>
              ))}
            </div>
          </details>
        );
      })}
      {running && (
        <div className="current-activity" data-testid="current-activity">
          <span className="spinner" />
          <span>Working</span>
        </div>
      )}
    </div>
  );
}
