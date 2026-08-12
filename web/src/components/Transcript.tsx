import { invoke } from "@tauri-apps/api/core";
import {
  type ReactNode,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import {
  buildTimeline,
  type TimelineEvent,
  type TimelineNode,
} from "../timeline";
import { translate } from "../i18n";
import { Markdown } from "./Markdown";

const CHEVRON_PATH =
  "M9.167 5.086 C 8.866 5.235,8.711 5.578,8.778 5.947 C 8.802 6.084,9.120 6.415,11.752 9.050 L 14.700 12.000 11.772 14.930 C 10.134 16.569,8.825 17.910,8.802 17.973 C 8.636 18.420,8.947 18.937,9.415 18.989 C 9.793 19.032,9.659 19.151,13.093 15.725 C 14.849 13.973,16.331 12.463,16.387 12.369 C 16.526 12.135,16.526 11.865,16.387 11.631 C 16.232 11.370,9.974 5.137,9.791 5.062 C 9.585 4.976,9.371 4.985,9.167 5.086 ";

function TranscriptChevron({ className }: { className: string }) {
  return (
    <svg
      className={className}
      aria-hidden="true"
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
    >
      <path d={CHEVRON_PATH} fill="currentColor" />
    </svg>
  );
}

function formatDuration(durationMs: number): string {
  if (durationMs < 1000) return `${Math.round(durationMs)}ms`;
  const seconds = durationMs / 1000;
  return `${Number(seconds.toFixed(seconds < 10 ? 1 : 0))}s`;
}

function TranscriptRowLabel({
  label,
  additions,
  deletions,
}: {
  label: string;
  additions?: number;
  deletions?: number;
}) {
  return (
    <>
      <span>{label}</span>
      {(additions !== undefined || deletions !== undefined) && (
        <span className="transcript-diff-badges">
          {additions !== undefined && (
            <span className="transcript-additions">+{additions}</span>
          )}
          {deletions !== undefined && (
            <span className="transcript-deletions">−{deletions}</span>
          )}
        </span>
      )}
    </>
  );
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
          {copied ? translate("copied") : translate("copy")}
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

function TranscriptDisclosure({
  label,
  children,
  className = "",
  summaryClassName = "",
  summaryContent,
  bare = false,
  onToggle,
}: {
  label: string;
  children: ReactNode;
  className?: string;
  summaryClassName?: string;
  summaryContent?: ReactNode;
  bare?: boolean;
  onToggle?: (open: boolean) => void;
}) {
  return (
    <details
      className={`transcript-thought ${className}`}
      onToggle={(event) => onToggle?.(event.currentTarget.open)}
    >
      <summary
        className={`transcript-row-header ${bare ? "transcript-disclosure-bare" : ""} ${summaryClassName}`}
        aria-label={bare ? label : undefined}
      >
        {!bare && (summaryContent ?? <span>{label}</span>)}
        <TranscriptChevron className="transcript-thought-chevron" />
      </summary>
      <span className="transcript-disclosure-break" aria-hidden="true" />
      <div className="transcript-thought-body">{children}</div>
    </details>
  );
}

function Thought({
  text,
  label = "Thought details",
  bare = false,
  forceSummaryBreak = false,
}: {
  text: string;
  label?: string;
  bare?: boolean;
  forceSummaryBreak?: boolean;
}) {
  return (
    <TranscriptDisclosure
      label={label}
      bare={bare}
      summaryClassName={
        forceSummaryBreak ? "transcript-disclosure-summary-break" : ""
      }
    >
      {text}
    </TranscriptDisclosure>
  );
}

function thoughtLabel(label?: string): string {
  return label?.startsWith("Thought for ") ? label : "Thought details";
}

function QuestionCard({
  text,
  options,
  onAnswer,
}: {
  text: string;
  options?: string[];
  onAnswer?: (answer: string) => void;
}) {
  const [answer, setAnswer] = useState("");
  const optionLabel = (index: number) =>
    String.fromCharCode("A".charCodeAt(0) + index);
  return (
    <div className="approval transcript-question-card">
      <div className="transcript-question-head">
        <strong>{translate("question")}</strong>
        <span className="approval-with">{text}</span>
      </div>
      {options && options.length > 0 && (
        <div className="approval-btns flex-wrap">
          {options.map((option, index) => (
            <button
              className="approval-option-row"
              key={option}
              type="button"
              onClick={() => onAnswer?.(option)}
            >
              <span className="approval-option-key">{optionLabel(index)}</span>
              <span>{option}</span>
            </button>
          ))}
        </div>
      )}
      <div className="approval-btns">
        <input
          className="input"
          value={answer}
          placeholder={translate("typeAnswer")}
          onChange={(event) => setAnswer(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter" && answer.trim()) {
              onAnswer?.(answer.trim());
              setAnswer("");
            }
          }}
        />
        <button
          className="btn approval-primary"
          type="button"
          disabled={!answer.trim()}
          onClick={() => {
            onAnswer?.(answer.trim());
            setAnswer("");
          }}
        >
          {translate("send")}
        </button>
      </div>
    </div>
  );
}

function UserBubbleContent({
  attachments,
  text,
}: Pick<Extract<TimelineNode, { kind: "user" }>, "attachments" | "text">) {
  return (
    <>
      {attachments?.map((attachment) =>
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
      {text}
    </>
  );
}

const USER_BUBBLE_MAX_HEIGHT = 290;

function UserBubble({
  attachments,
  text,
}: Pick<Extract<TimelineNode, { kind: "user" }>, "attachments" | "text">) {
  const bubbleRef = useRef<HTMLElement | null>(null);
  const [overflowing, setOverflowing] = useState(false);
  const measureOverflow = useCallback(() => {
    const bubble = bubbleRef.current;
    if (!bubble) return;
    const clamped = bubble.classList.contains("transcript-user-bubble-clamped");
    if (clamped) bubble.classList.remove("transcript-user-bubble-clamped");
    const nextOverflowing = bubble.scrollHeight > USER_BUBBLE_MAX_HEIGHT;
    if (clamped) bubble.classList.add("transcript-user-bubble-clamped");
    setOverflowing(nextOverflowing);
  }, []);

  useEffect(() => {
    const bubble = bubbleRef.current;
    if (!bubble) return;
    const resizeObserver =
      typeof ResizeObserver === "undefined"
        ? undefined
        : new ResizeObserver(measureOverflow);
    resizeObserver?.observe(bubble);
    const mutationObserver = new MutationObserver(measureOverflow);
    mutationObserver.observe(bubble, {
      characterData: true,
      childList: true,
      subtree: true,
    });
    measureOverflow();
    return () => {
      resizeObserver?.disconnect();
      mutationObserver.disconnect();
    };
  }, [measureOverflow, overflowing]);

  const content = <UserBubbleContent attachments={attachments} text={text} />;
  const className = `bubble-user transcript-user-bubble${overflowing ? " transcript-user-bubble-clamped" : ""}`;
  if (!overflowing) {
    return (
      <div
        className={className}
        ref={(element) => {
          bubbleRef.current = element;
        }}
      >
        {content}
      </div>
    );
  }
  return (
    <details className="transcript-user-collapsible">
      <summary
        className={className}
        ref={(element) => {
          bubbleRef.current = element;
        }}
      >
        {content}
      </summary>
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
      <div className="transcript-plan-title">
        <svg
          className="transcript-plan-icon"
          aria-hidden="true"
          width="16"
          height="16"
          viewBox="0 0 24 24"
          fill="none"
        >
          <path
            d="M5 6h14M5 12h14M5 18h14"
            stroke="currentColor"
            strokeWidth="1.75"
            strokeLinecap="round"
          />
        </svg>
        <span>{translate("opcosExecutionPlan")}</span>
      </div>
      <div className="transcript-plan-steps">
        {steps.map((step, index) => {
          const complete = ["done", "completed"].includes(String(step.status));
          const active = step.status === "in_progress";
          return (
            <div
              className="transcript-plan-step"
              key={`${step.content}-${index}`}
            >
              <span className="transcript-plan-number">#{index + 1}</span>
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
  label,
  summaryContent,
  output,
  truncated,
  totalBytes,
}: {
  label: string;
  summaryContent?: ReactNode;
  output: string;
  truncated?: boolean;
  totalBytes?: number;
}) {
  const omittedBytes =
    totalBytes === undefined
      ? undefined
      : Math.max(0, totalBytes - new TextEncoder().encode(output).length);
  return (
    <TranscriptDisclosure
      label={label}
      summaryContent={summaryContent}
      className="transcript-output"
    >
      <pre className="artifact-code max-h-96 overflow-auto whitespace-pre-wrap break-words">
        {output}
        {truncated
          ? `\n[${translate("outputTruncatedWithDetails", {
              omitted:
                omittedBytes === undefined
                  ? translate("some")
                  : String(omittedBytes),
            })}]`
          : ""}
      </pre>
    </TranscriptDisclosure>
  );
}

function ArtifactRow({
  label,
  additions,
  deletions,
  sessionId,
  artifactId,
  kind,
  mime,
}: {
  label: string;
  additions?: number;
  deletions?: number;
  sessionId: string;
  artifactId: string;
  kind?: string;
  mime?: string;
}) {
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
    if (content || error) return;
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
    <>
      <TranscriptDisclosure
        label={label}
        summaryContent={
          <TranscriptRowLabel
            label={label}
            additions={additions}
            deletions={deletions}
          />
        }
        className="transcript-artifact"
        onToggle={(open) => {
          if (open) load();
        }}
      >
        {error ? (
          <span className="text-red-500">{error}</span>
        ) : image ? (
          <img
            className="max-w-full max-h-64 cursor-zoom-in"
            src={image}
            alt={translate("screenshotArtifact")}
            onClick={() => setLightboxOpen(true)}
          />
        ) : content ? (
          <pre className="artifact-code whitespace-pre-wrap">
            {diff ?? String(content.content ?? "")}
          </pre>
        ) : (
          <span className="text-muted">{translate("loading")}</span>
        )}
      </TranscriptDisclosure>
      {lightboxOpen && image && (
        <div
          className="fixed inset-0 z-50 flex items-center justify-center bg-black/80 p-6"
          role="presentation"
          onClick={() => setLightboxOpen(false)}
        >
          <img
            className="max-h-full max-w-full"
            src={image}
            alt={translate("screenshotArtifactEnlarged")}
            onClick={(event) => event.stopPropagation()}
          />
        </div>
      )}
    </>
  );
}

export function Transcript({
  events,
  sessionId,
  hostName,
  running,
  onRetry,
  retryLabel = "Retry",
  onQuestionAnswer,
}: {
  events: TimelineEvent[];
  sessionId: string;
  hostName?: string;
  running?: boolean;
  onRetry?: () => void;
  retryLabel?: string;
  onQuestionAnswer?: (callId: string, answer: string) => void;
}) {
  const nodes = buildTimeline(events, Boolean(running));
  const worklogOverrides = useRef(new Set<number>());
  const [worklogOpen, setWorklogOpen] = useState<Record<number, boolean>>({});
  const [clock, setClock] = useState(() => Date.now());
  useEffect(() => {
    if (!running) return;
    const interval = window.setInterval(() => setClock(Date.now()), 1000);
    return () => window.clearInterval(interval);
  }, [running]);
  const lastWorkIndex = nodes.reduce(
    (last, node, index) => (node.kind === "work" ? index : last),
    -1,
  );
  useEffect(() => {
    if (lastWorkIndex < 0 || worklogOverrides.current.has(lastWorkIndex))
      return;
    setWorklogOpen((current) => ({
      ...current,
      [lastWorkIndex]: Boolean(running),
    }));
  }, [lastWorkIndex, running]);
  return (
    <div className="transcript transcript-content">
      {nodes.map((node, index) => {
        if (node.kind === "user")
          return (
            <div className="group transcript-user-message self-end" key={index}>
              <UserBubble attachments={node.attachments} text={node.text} />
              <BubbleMeta text={node.text} ts={node.ts} />
            </div>
          );
        if (node.kind === "assistant")
          return (
            <div
              className="group bubble-assistant transcript-prose"
              key={index}
            >
              <Markdown text={node.text} />
              <BubbleMeta text={node.text} ts={node.ts} />
            </div>
          );
        if (node.kind === "approval")
          return node.resolved ? (
            <div className="transcript-resolved" key={index}>
              {translate("workedDetail")} ·{" "}
              {node.resolved === "allow"
                ? translate("allowedLabel")
                : translate("deniedLabel")}{" "}
              {node.name}
            </div>
          ) : null;
        if (node.kind === "question")
          return (
            <QuestionCard
              key={index}
              text={node.text}
              options={node.options}
              onAnswer={(answer) => onQuestionAnswer?.(node.callId, answer)}
            />
          );
        if (node.kind === "tail_status")
          return (
            <div className="transcript-sleep" key={index}>
              <span className="transcript-sleep-dot" />
              {node.text}
            </div>
          );
        if (node.kind === "notice")
          return (
            <div
              className={`notice ${
                node.annotationResult === "failed"
                  ? "error"
                  : node.tone === "info" || node.annotationType
                    ? "info"
                    : "warn"
              }`}
              key={index}
            >
              <span>{node.text}</span>
              {node.annotationResult && (
                <span className="ml-2 text-xs uppercase">
                  {node.annotationResult}
                </span>
              )}
              {node.retriable && onRetry && !running && (
                <button className="btn ml-2" onClick={onRetry}>
                  {retryLabel}
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
        ) => {
          const isThoughtRow =
            Boolean(row.detail) && row.label.startsWith("Thought for ");
          const hasTerminalOutput = Boolean(
            row.terminalOutput || row.terminalTruncated,
          );
          const terminalSummary = hasTerminalOutput ? (
            <>
              <span className="transcript-row-label">
                <TranscriptRowLabel
                  label={row.label}
                  additions={row.additions}
                  deletions={row.deletions}
                />
              </span>
              {row.shellId && (
                <span className="transcript-row-meta">{row.shellId}</span>
              )}
              {row.exitCode !== undefined && row.exitCode !== 0 && (
                <span className="transcript-row-meta">
                  {translate("exitLabel")} {row.exitCode}
                </span>
              )}
              {row.denied && (
                <span className="ml-2 text-xs text-muted">
                  {translate("notRunDetail")}
                  {row.detail ? ` · ${row.detail}` : ""}
                </span>
              )}
              {row.durationMs !== undefined && (
                <span className="transcript-row-duration">
                  {formatDuration(row.durationMs)}
                </span>
              )}
              {row.resultSummary && (
                <span
                  className={`transcript-row-result${row.resultError ? " text-danger" : ""}`}
                >
                  {row.resultError && `${translate("failed")} · `}
                  {row.resultSummary}
                </span>
              )}
            </>
          ) : undefined;
          return (
            <div
              className={`transcript-item transcript-row${row.shellId ? " transcript-shell-row" : ""}${row.denied ? " text-muted" : row.resultError || (row.exitCode !== undefined && row.exitCode !== 0) ? " text-danger" : ""}`}
              key={rowIndex}
            >
              {!isThoughtRow && !row.artifactId && !hasTerminalOutput && (
                <span className="transcript-row-label">
                  <TranscriptRowLabel
                    label={row.label}
                    additions={row.additions}
                    deletions={row.deletions}
                  />
                </span>
              )}
              {row.shellId && !hasTerminalOutput && (
                <span className="transcript-row-meta">{row.shellId}</span>
              )}
              {row.exitCode !== undefined &&
                row.exitCode !== 0 &&
                !hasTerminalOutput && (
                  <span className="transcript-row-meta">
                    {translate("exitLabel")} {row.exitCode}
                  </span>
                )}
              {row.denied && !hasTerminalOutput && (
                <span className="ml-2 text-xs text-muted">
                  {translate("notRunDetail")}
                  {row.detail ? ` · ${row.detail}` : ""}
                </span>
              )}
              {row.durationMs !== undefined && !hasTerminalOutput && (
                <span className="transcript-row-duration">
                  {formatDuration(row.durationMs)}
                </span>
              )}
              {row.resultSummary && !hasTerminalOutput && (
                <span
                  className={`transcript-row-result${row.resultError ? " text-danger" : ""}`}
                >
                  {row.resultError && `${translate("failed")} · `}
                  {row.resultSummary}
                </span>
              )}
              {(row.terminalOutput || row.terminalTruncated) && (
                <TerminalOutput
                  label={row.label}
                  summaryContent={terminalSummary}
                  output={row.terminalOutput ?? ""}
                  truncated={row.terminalTruncated}
                  totalBytes={row.terminalTotalBytes}
                />
              )}
              {row.artifactId && (
                <ArtifactRow
                  label={row.label}
                  additions={row.additions}
                  deletions={row.deletions}
                  sessionId={sessionId}
                  artifactId={row.artifactId}
                  kind={row.artifactKind}
                  mime={row.artifactMime}
                />
              )}
              {row.plan && <PlanCard steps={row.plan.steps} />}
              {row.detail && !row.thoughtForCallId && (
                <Thought text={row.detail} label={thoughtLabel(row.label)} />
              )}
              {row.callId && thoughtByCallId.get(row.callId)?.detail && (
                <Thought
                  text={thoughtByCallId.get(row.callId)?.detail ?? ""}
                  label={thoughtLabel(thoughtByCallId.get(row.callId)?.label)}
                />
              )}
            </div>
          );
        };
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
                    {minorRows.length} {translate("minorActions")}
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
          <details
            className="work-segment transcript-worklog"
            open={worklogOpen[index] ?? (running && index === lastWorkIndex)}
            onToggle={(event) => {
              const open = event.currentTarget.open;
              setWorklogOpen((current) => ({ ...current, [index]: open }));
            }}
            key={index}
          >
            <summary
              className="transcript-worklog-header"
              onClick={() => worklogOverrides.current.add(index)}
            >
              <TranscriptChevron className="transcript-chevron" />
              <span className="transcript-worklog-label">
                <span>
                  {running &&
                  index === lastWorkIndex &&
                  node.startedAt !== undefined
                    ? translate("workingFor", {
                        seconds: Math.max(
                          0,
                          Math.floor((clock - node.startedAt) / 1000),
                        ),
                      })
                    : node.label}
                </span>
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
          <span>{translate("working")}</span>
        </div>
      )}
    </div>
  );
}
