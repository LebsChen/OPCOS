import { invoke } from "@tauri-apps/api/core";
import { type ReactNode, useEffect, useState } from "react";
import { type ApprovalDecision, type Item } from "../types";
import { buildTimeline, type TimelineEvent } from "../timeline";
import { ApprovalCard } from "./ApprovalCard";
import { Markdown } from "./Markdown";

function formatDuration(durationMs: number): string {
  if (durationMs < 1000) return `${Math.round(durationMs)}ms`;
  const seconds = durationMs / 1000;
  return `${Number(seconds.toFixed(seconds < 10 ? 1 : 0))}s`;
}

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

function Thought({
  text,
  label = "Thought details",
}: {
  text: string;
  label?: string;
}) {
  return (
    <details className="transcript-thought">
      <summary className="transcript-row-header">
        <span>{label}</span>
      </summary>
      <div className="transcript-thought-body">{text}</div>
    </details>
  );
}

function PlanCard({
  steps,
}: {
  steps: Array<{ content: string; status?: string }>;
}) {
  return (
    <div className="transcript-plan">
      <div className="transcript-plan-title">Devin&apos;s execution plan</div>
      <div className="transcript-plan-steps">
        {steps.map((step, index) => {
          const complete = ["done", "completed"].includes(String(step.status));
          const active = step.status === "in_progress";
          return (
            <div
              className="transcript-plan-step"
              key={`${step.content}-${index}`}
            >
              <span
                className={`transcript-plan-glyph${complete ? " is-complete" : active ? " is-active" : ""}`}
                aria-hidden="true"
              >
                {complete ? "✓" : active ? "◌" : "○"}
              </span>
              <span>{step.content}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

function TerminalOutput({
  output,
  truncated,
  totalBytes,
}: {
  output: string;
  truncated?: boolean;
  totalBytes?: number;
}) {
  const [open, setOpen] = useState(false);
  const omittedBytes =
    totalBytes === undefined
      ? undefined
      : Math.max(0, totalBytes - new TextEncoder().encode(output).length);
  return (
    <div className="mt-1">
      <button
        className="text-left underline text-xs text-muted"
        onClick={() => setOpen((value) => !value)}
      >
        {open ? "Hide output" : "Show output"}
      </button>
      {open && (
        <pre className="artifact-code max-h-96 overflow-auto whitespace-pre-wrap break-words">
          {output}
          {truncated
            ? `\n[Output truncated: ${
                omittedBytes === undefined ? "some" : omittedBytes
              } bytes omitted; the model saw the tail]`
            : ""}
        </pre>
      )}
    </div>
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
  const [lightboxOpen, setLightboxOpen] = useState(false);
  useEffect(() => {
    if (!lightboxOpen) return;
    const close = (event: KeyboardEvent) => {
      if (event.key === "Escape") setLightboxOpen(false);
    };
    window.addEventListener("keydown", close);
    return () => window.removeEventListener("keydown", close);
  }, [lightboxOpen]);
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
              onClick={() => setLightboxOpen(true)}
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
      {lightboxOpen && image && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/80 p-6"
          role="presentation"
          onClick={() => setLightboxOpen(false)}
        >
          <img
            className="max-h-full max-w-full"
            src={image}
            alt="Screenshot artifact enlarged"
            onClick={(event) => event.stopPropagation()}
          />
        </div>
      )}
    </div>
  );
}

export function Transcript({
  events,
  sessionId,
  hostName,
  running,
  onApprove,
  onRetry,
}: {
  events: TimelineEvent[];
  sessionId: string;
  hostName?: string;
  running?: boolean;
  onApprove?: (
    item: Extract<Item, { kind: "approval" }>,
    decision: ApprovalDecision,
  ) => void;
  onRetry?: () => void;
}) {
  const nodes = buildTimeline(events);
  return (
    <div className="transcript transcript-content">
      {nodes.map((node, index) => {
        if (node.kind === "user")
          return (
            <div className="group transcript-user-message self-end" key={index}>
              <div className="bubble-user transcript-user-bubble">
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
                resolved: node.resolved,
              }}
              hostName={hostName}
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
        const thoughtByCallId = new Map(
          node.rows
            .filter((row) => row.thoughtForCallId)
            .map((row) => [row.thoughtForCallId, row]),
        );
        const renderRow = (
          row: (typeof node.rows)[number],
          rowIndex: number,
        ) => (
          <div
            className={`transcript-item transcript-row${row.denied ? " text-muted" : row.exitCode !== undefined && row.exitCode !== 0 ? " text-danger" : ""}`}
            key={rowIndex}
          >
            <span className="transcript-row-label">{row.label}</span>
            {row.exitCode !== undefined && row.exitCode !== 0 && (
              <span className="text-xs text-muted">exit {row.exitCode}</span>
            )}
            {row.denied && (
              <span className="ml-2 text-xs text-muted">
                not run{row.detail ? ` · ${row.detail}` : ""}
              </span>
            )}
            {row.durationMs !== undefined && (
              <span className="transcript-row-duration">
                {formatDuration(row.durationMs)}
              </span>
            )}
            {row.detail && !row.thoughtForCallId && (
              <Thought text={row.detail} label={row.label} />
            )}
            {row.callId && thoughtByCallId.get(row.callId)?.detail && (
              <Thought
                text={thoughtByCallId.get(row.callId)?.detail ?? ""}
                label={thoughtByCallId.get(row.callId)?.label}
              />
            )}
            {(row.terminalOutput || row.terminalTruncated) && (
              <TerminalOutput
                output={row.terminalOutput ?? ""}
                truncated={row.terminalTruncated}
                totalBytes={row.terminalTotalBytes}
              />
            )}
            {row.artifactId && (
              <ArtifactRow
                sessionId={sessionId}
                artifactId={row.artifactId}
                kind={row.artifactKind}
                mime={row.artifactMime}
              />
            )}
            {row.plan && <PlanCard steps={row.plan.steps} />}
          </div>
        );
        const renderRows = (rows: typeof node.rows) => {
          const rendered: ReactNode[] = [];
          let rowIndex = 0;
          while (rowIndex < rows.length) {
            if (rows[rowIndex].isMajorAction === false) {
              const start = rowIndex;
              while (
                rowIndex < rows.length &&
                rows[rowIndex].isMajorAction === false
              ) {
                rowIndex += 1;
              }
              const minorRows = rows.slice(start, rowIndex);
              rendered.push(
                <details
                  className="rounded border border-line p-2"
                  key={`minor-${start}`}
                >
                  <summary className="transcript-minor-summary">
                    {minorRows.length} minor actions
                  </summary>
                  <div className="mt-2 flex flex-col gap-2">
                    {minorRows.map((row, offset) =>
                      renderRow(row, start + offset),
                    )}
                  </div>
                </details>,
              );
            } else {
              const row = rows[rowIndex];
              if (!row.thoughtForCallId) {
                rendered.push(renderRow(row, rowIndex));
              }
              rowIndex += 1;
            }
          }
          return rendered;
        };
        return (
          <details className="work-segment transcript-worklog" key={index}>
            <summary className="transcript-worklog-header">
              <span className="transcript-chevron" aria-hidden="true">
                ›
              </span>
              <span className="transcript-worklog-label">
                <span>{node.label}</span>
                {!!(node.additions || node.deletions) && (
                  <span className="transcript-diff-badges">
                    {node.additions ? (
                      <span className="transcript-additions">
                        +{node.additions}
                      </span>
                    ) : null}
                    {node.deletions ? (
                      <span className="transcript-deletions">
                        −{node.deletions}
                      </span>
                    ) : null}
                  </span>
                )}
              </span>
            </summary>
            <div className="transcript-worklog-body">
              {renderRows(node.rows)}
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
