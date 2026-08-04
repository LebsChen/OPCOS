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
||||||| parent of 9750bee (Fix provider model configuration fallback)
        <div className="thinking-body" data-testid="thinking-body">
          {text}
        </div>
      )}
    </div>
  );
}

type ToolItem = Extract<Item, { kind: "tool" }>;
type ApprovalItem = Extract<Item, { kind: "approval" }>;
type AssistantItem = Extract<Item, { kind: "assistant" }>;
type TurnItem = ToolItem | ApprovalItem | AssistantItem;

// TurnGroup (§33, absorbs §7's StepGroup): the whole user-message → final-answer span collapses
// as ONE disclosure — "N steps" — with the agent's narration (assistant text followed by more
// activity in the same turn) and humanized one-line steps interleaved inside. The final assistant
// text renders as a normal bubble OUTSIDE the group (see the flush logic in Transcript below).
// Approvals fold into their tool's row as a chip; an approval with no executed call (typically
// declined) keeps its own "Wanted to …" row. Raw args+result stay one click away per row.

type TurnRow =
  | { type: "narr"; text: string }
  | { type: "step"; tool: ToolItem; approval?: ApprovalItem }
  | { type: "ask"; approval: ApprovalItem };

function buildRows(items: TurnItem[]): TurnRow[] {
  // First pass: tool rows in order; then pair each resolved approval with the nearest
  // same-name tool that doesn't have one yet (approvals may stream before or after their call).
  const rows: TurnRow[] = items
    .filter((it): it is ToolItem | AssistantItem => it.kind !== "approval")
    // Thinking-only assistant items (no text) carry nothing narratable — skip the row.
    .filter((it) => it.kind !== "assistant" || it.text)
    .map((it) =>
      it.kind === "assistant"
        ? { type: "narr" as const, text: it.text }
        : { type: "step" as const, tool: it },
    );
  const approvals = items.filter(
    (it): it is ApprovalItem => it.kind === "approval",
  );
  for (const ap of approvals) {
    const at = items.indexOf(ap);
    let bestRow: Extract<TurnRow, { type: "step" }> | null = null;
    let bestDist = Infinity;
    for (let i = 0; i < items.length; i++) {
      const it = items[i];
      if (it.kind !== "tool" || it.name !== ap.name) continue;
      const row = rows.find((r) => r.type === "step" && r.tool === it) as
        Extract<TurnRow, { type: "step" }> | undefined;
      if (!row || row.approval) continue;
      const dist = Math.abs(i - at);
      if (dist < bestDist) {
        bestRow = row;
        bestDist = dist;
      }
    }
    if (bestRow) bestRow.approval = ap;
    else {
      // No executed call to attach to (or it was declined) — the ask keeps its own row,
      // placed where the approval sat in the stream.
      const after = items
        .slice(0, at)
        .filter((it) => it.kind !== "approval").length;
      rows.splice(after, 0, { type: "ask", approval: ap });
    }
  }
  return rows;
}

function approvalChip(resolved: ApprovalDecision | undefined) {
  if (resolved === "deny")
    return (
      <span className="text-[10.5px] px-1.5 rounded-full bg-dangerSoft text-danger shrink-0">
        ✕ declined
      </span>
    );
  return (
    <span
      className="text-[10.5px] px-1.5 rounded-full bg-okSoft text-ok shrink-0"
      title={
        resolved ? `approved · ${resolved.replace(/_/g, " ")}` : "approved"
      }
    >
      ✓ approved
    </span>
  );
}

function LineText({ line }: { line: HumanLine }) {
  return (
    <span className="min-w-0 text-[13px] leading-relaxed">
      <span className="text-muted">{line.pre}</span>
      {line.obj && <span className="text-ink">{line.obj}</span>}
      {line.post && <span className="text-muted">{line.post}</span>}
    </span>
  );
}

function formatRawValue(value: unknown): string {
  if (typeof value === "string") return value;
  if (value === undefined) return "";
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function StepRow({
  tool,
  approval,
}: {
  tool: ToolItem;
  approval?: ApprovalItem;
}) {
  const [raw, setRaw] = useState(false);
  const resolution = approval?.resolved ?? tool.resolved;
  const statusKind = classifyStepStatus(tool.status);
  const running =
    statusKind === "running" && resolution !== "deny" && resolution !== "allow";
  const failed =
    statusKind === "failed" &&
    !running &&
    resolution !== "deny" &&
    resolution !== "allow";
  return (
    <div>
      <div
        className="group flex items-baseline gap-2 px-2 py-0.5 rounded-lg hover:bg-paper"
        data-testid="turn-step"
      >
        <span
          className={
            "w-3.5 text-center text-[10px] shrink-0 " +
            (failed ? "text-danger" : running ? "text-accent" : "text-ok")
          }
        >
          {running ? (
            <span className="spinner" data-testid="step-running" />
          ) : (
            "●"
          )}
        </span>
        <LineText line={humanizeTool(tool.name, tool.args)} />
        {resolution && approvalChip(resolution)}
        {!!tool.standingRule && (
          <span
            className="text-[10.5px] px-1.5 rounded-full bg-tealSoft text-tealInk shrink-0"
            data-testid="tool-standing-rule"
            title={`Auto-allowed by this automation's standing approval: ${tool.standingRule}. Revoke on its Automations page.`}
          >
            auto-allowed
          </span>
        )}
        {!!tool.hidden && (
          <span
            className="text-[11px] text-warnInk shrink-0"
            data-testid="tool-hidden-count"
            title={translate(
              "Removed by your privacy filters before the agent saw the results \u2014 agents get no trace of these.",
            )}
          >
            {tool.hidden} hidden
          </span>
        )}
        {failed && (
          <span className="text-[11px] text-danger shrink-0">
            {tool.status}
          </span>
        )}
        {!running && (
          <button
            className="ml-auto shrink-0 text-[11px] text-faint opacity-0 group-hover:opacity-100 cursor-pointer"
            onClick={() => setRaw((v) => !v)}
          >
            raw
          </button>
        )}
      </div>
      {raw && (
        <pre className="ml-8 mr-2 my-1 px-2.5 py-1.5 rounded-lg border border-line bg-paper font-mono text-[11.5px] leading-relaxed text-muted whitespace-pre-wrap break-words max-h-56 overflow-auto">
          {`${tool.name}  ${
            typeof tool.args === "object" && tool.args !== null
              ? shortArgs(tool.args)
              : formatRawValue(tool.args)
          }`}
          {tool.preview
            ? `\n→ ${
                formatRawValue(tool.preview).length > 1500
                  ? formatRawValue(tool.preview).slice(0, 1500) + "\n…"
                  : formatRawValue(tool.preview)
              }`
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
        const renderRow = (row: (typeof node.rows)[number], rowIndex: number) => (
          <div
            className={`transcript-item${row.denied ? " text-muted" : row.exitCode !== undefined && row.exitCode !== 0 ? " text-danger" : ""}`}
            key={rowIndex}
          >
            <span>{row.label}</span>
            {row.shellId && (
              <span className="ml-2 text-xs text-muted">
                {row.shellId}
              </span>
            )}
            {row.exitCode !== undefined && (
              <span className="ml-2 text-xs text-muted">
                exit {row.exitCode}
              </span>
            )}
            {row.denied && (
              <span className="ml-2 text-xs text-muted">
                not run{row.detail ? ` · ${row.detail}` : ""}
              </span>
            )}
            {row.durationMs !== undefined && (
              <span className="ml-2 text-xs text-muted">
                {formatDuration(row.durationMs)}
              </span>
            )}
            {row.detail && !row.thoughtForCallId && (
              <Thought text={row.detail} />
            )}
            {row.callId && thoughtByCallId.get(row.callId)?.detail && (
              <Thought text={thoughtByCallId.get(row.callId)?.detail ?? ""} />
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
                  <summary className="cursor-pointer text-xs text-muted">
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
          <details className="work-segment flex flex-col gap-2" open key={index}>
            <summary className="cursor-pointer text-muted">
              {node.label} {node.additions ? `+${node.additions}` : ""}{" "}
              {node.deletions ? `−${node.deletions}` : ""}
            </summary>
            <div className="max-h-[32rem] overflow-auto flex flex-col gap-2 text-ink">
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
