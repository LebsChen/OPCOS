import { useState } from "react";
import type { ApprovalDecision, Item } from "../types";
import { humanizeApprovalTitle, type HumanLine } from "../humanize";
import { translate } from "../i18n";
import { Icon } from "./Icon";

type ApprovalArgs = Record<string, unknown>;

export function shortArgs(args: ApprovalArgs | null | undefined): string {
  if (!args || typeof args !== "object") return "";
  return Object.entries(args)
    .map(([k, v]) => {
      let s = typeof v === "string" ? v : JSON.stringify(v);
      const text = s ?? "";
      if (text.length > 96) s = text.slice(0, 95) + "...";
      return `${k}=${(s ?? "").replace(/\n/g, " ")}`;
    })
    .join("  ");
}

// Human verbs kept for the §25 grant lines (the card title now comes from humanize.ts).
const TOOL_VERBS: Record<string, string> = {
  write_file: "writeFileVerb",
  replace_in_file: "editFileVerb",
  apply_patch: "applyPatchVerb",
  apply_unified_diff: "applyPatchVerb",
  run_shell: "runCommandVerb",
  send_message: "sendMessageVerb",
  send_file: "sendFileVerb",
};

// §35: routine workspace writes render as a compact ROW; everything else is a full card.
const FILE_WRITES = new Set([
  "write_file",
  "replace_in_file",
  "apply_patch",
  "apply_unified_diff",
]);
// Actions that leave the bound remote host get the warm border + destination note.
const EXTERNAL = new Set(["send_message", "send_file"]);

type ApprovalItem = Extract<Item, { kind: "approval" }>;

// A `permissions` proposal on the create_scheduled_task consent card (§25): reads are
// disclosure lines, writes are the standing grants the approval mints.
interface PermissionLine {
  tool: string;
  target: string;
  access: string;
}

function permissionLines(
  args: ApprovalArgs | null | undefined,
): PermissionLine[] {
  const raw = args?.permissions;
  if (!Array.isArray(raw)) return [];
  return raw
    .filter((p) => p && typeof p === "object" && p.tool && p.target)
    .map((p) => ({
      tool: String(p.tool),
      target: String(p.target),
      access: String(p.access || "read"),
    }));
}

export function TitleText({ line }: { line: HumanLine }) {
  return (
    <span className="approval-title">
      {line.pre}
      {line.obj && <b>{line.obj}</b>}
      {line.post}
    </span>
  );
}

// Plain-words scope note (replaces the "local action" badge): where does this act?
// Shared with the parked-approval card (InboxItemCard) so both dialects match (§35).
export function scopeNote(
  name: string,
  args: ApprovalArgs | null | undefined,
  category?: string,
  hostName?: string,
): { text: string; external: boolean } {
  if (category === "connector")
    return { text: translate("actsOnConnectedService"), external: true };
  if (EXTERNAL.has(name)) {
    const platform = String(args?.target ?? "").split(":")[0];
    const names: Record<string, string> = {
      slack: "Slack",
      telegram: "Telegram",
    };
    return {
      text: translate("leavesRemoteHost", {
        destination:
          names[platform] || platform || translate("connectedChat"),
      }),
      external: true,
    };
  }
  const overwrite = name === "write_file" && args?.overwrite;
  const location = hostName || translate("boundHost");
  return {
    text:
      translate("runsOnHost", { host: location }) +
      (overwrite ? ` · ${translate("overwritesExistingFile")}` : ""),
    external: false,
  };
}

// The proposed content/command, straight from the tool call's ARGS — the file/action
// doesn't exist yet, so no viewer could show it (§35; see UX-018 mock note).
// Clamps by CHARACTERS as well as lines: a one-paragraph Slack digest has no
// newlines at all and once ballooned the card to full-transcript height.
const PREVIEW_LINES = 5;
const PREVIEW_CHARS = 420;

export function PreviewBlock({
  text,
  mono = true,
}: {
  text: string;
  mono?: boolean;
}) {
  const [all, setAll] = useState(false);
  const lines = text.split("\n");
  const clipped = lines.length > PREVIEW_LINES || text.length > PREVIEW_CHARS;
  let shown = text;
  if (!all && clipped) {
    shown = lines.slice(0, PREVIEW_LINES).join("\n");
    if (shown.length > PREVIEW_CHARS)
      shown = shown.slice(0, PREVIEW_CHARS).trimEnd() + "…";
  }
  return (
    <div className={"approval-prev" + (mono ? "" : " prose")}>
      {shown}
      {clipped && (
        <button
          className="approval-prev-more"
          onClick={() => setAll((v) => !v)}
        >
          {all
            ? translate("showLess")
            : lines.length > PREVIEW_LINES
              ? translate("showAllLines", { count: lines.length })
              : translate("showFullMessage")}
        </button>
      )}
    </div>
  );
}

// Outbound message text: short one-liners keep the cozy inline quote; anything
// long (or multi-line) gets the clamped preview so the card stays card-sized.
function MessagePreview({ text, label }: { text: string; label?: string }) {
  if (text.length <= 220 && !text.includes("\n")) {
    return (
      <div className="approval-with">
        {label ? `${label}: ` : ""}“{text}”
      </div>
    );
  }
  return <PreviewBlock text={text} mono={false} />;
}

function Buttons({
  item,
  onApprove,
  runTask,
  primaryLabel,
}: {
  item: ApprovalItem;
  onApprove: (decision: ApprovalDecision, optionId?: string) => void;
  runTask?: { id: string; title: string } | null;
  primaryLabel: string;
}) {
  const switchModeOptions: Array<{ optionId: string; name?: string }> =
    item.args?.toolCall &&
    typeof item.args.toolCall === "object" &&
    (item.args.toolCall as { kind?: unknown }).kind === "switch_mode" &&
    Array.isArray(item.args.options)
      ? (item.args.options as unknown[]).filter(
          (option: unknown): option is { optionId: string; name?: string } =>
            !!option &&
            typeof option === "object" &&
            typeof (option as { optionId?: unknown }).optionId === "string",
        )
      : [];
  return (
    <div className="approval-btns">
      {switchModeOptions.length > 0 ? (
        switchModeOptions.map((option, index) => (
          <button
            className="approval-option-row"
            key={option.optionId}
            type="button"
            onClick={() => onApprove("allow", option.optionId)}
          >
            <span className="approval-option-key">
              {String.fromCharCode(65 + index)}
            </span>
            <span>{option.name || option.optionId}</span>
          </button>
        ))
      ) : (
        <button
          className="approval-option-row"
          type="button"
          onClick={() => onApprove("allow")}
        >
          <span className="approval-option-key">A</span>
          <span>{primaryLabel}</span>
        </button>
      )}
      <button
        className="approval-option-row"
        type="button"
        onClick={() => onApprove("deny")}
      >
        <span className="approval-option-key">
          {String.fromCharCode(
            65 + (switchModeOptions.length > 0 ? switchModeOptions.length : 1),
          )}
        </span>
        <span>{translate("deny")}</span>
      </button>
    </div>
  );
}

export function ApprovalCard({
  item,
  onApprove,
  runTask,
  compact = false,
  hostName,
}: {
  item: ApprovalItem;
  onApprove: (decision: ApprovalDecision, optionId?: string) => void;
  // Present when this approval was raised inside an automation run — unlocks the
  // task-persistent "Allow every time" (in-app only, §25).
  runTask?: { id: string; title: string } | null;
  compact?: boolean;
  hostName?: string;
}) {
  const [peek, setPeek] = useState(false);
  const title = humanizeApprovalTitle(item.name, item.args);
  const scope = scopeNote(item.name, item.args, item.category, hostName);
  const grants =
    item.name === "create_scheduled_task" ? permissionLines(item.args) : [];
  // "requires approval" is the engine's default boilerplate — only surface a real reason.
  const reason =
    item.reason && item.reason !== "requires approval" ? item.reason : "";
  const offerStanding = !!(runTask && item.standingTarget);
  const dock = compact ? " approval-dock" : "";

  // §35 compact row: routine workspace writes — one line, preview expands inline from the
  // tool args. Standing/grant flows keep the full card (they carry §25 consent weight).
  const content =
    typeof item.args?.content === "string" ? item.args.content : "";
  if (
    FILE_WRITES.has(item.name) &&
    !offerStanding &&
    !grants.length &&
    !item.resolved
  ) {
    return (
      <div
        className={"approval approval-row" + dock}
        data-testid="approval-row"
      >
        <div className="approval-row-line">
          <TitleText line={title} />
          {content && (
            <button
              className="approval-peek"
              onClick={() => setPeek((v) => !v)}
            >
              {translate("preview")} {peek ? "▴" : "▾"}
            </button>
          )}
          <span className="spacer" />
          <Buttons
            item={item}
            onApprove={onApprove}
            runTask={runTask}
            primaryLabel={translate("allow")}
          />
        </div>
        {peek && content && <PreviewBlock text={content} />}
        {reason && <div className="approval-reason">{reason}</div>}
      </div>
    );
  }

  return (
    <div
      className={
        "approval" + (scope.external ? " approval-external" : "") + dock
      }
    >
      <div className="approval-top">
        <div className="approval-heading">
          <span className="approval-ico" title={`Tool: ${item.name}`}>
            <Icon name="shield" size={15} />
          </span>
          <TitleText line={title} />
        </div>
        <span className={"approval-scope" + (scope.external ? " out" : "")}>
          {scope.text}
        </span>
      </div>

      {/* Tool-shaped previews — the proposal, not an args dump. */}
      {item.name === "run_shell" && item.args?.command && (
        <PreviewBlock text={String(item.args.command)} />
      )}
      {FILE_WRITES.has(item.name) && content && <PreviewBlock text={content} />}
      {item.name === "send_file" && (
        <>
          <span className="approval-filechip">
            <span className="ico">
              <Icon name="file" size={13} />
            </span>
            {String(item.args?.path ?? "")
              .split("/")
              .pop() || translate("file")}
            {item.args?.as_screenshot
              ? ` · ${translate("asPngScreenshot")}`
              : ""}
          </span>
          {item.args?.comment && (
            <MessagePreview
              text={String(item.args.comment)}
              label={translate("withMessage")}
            />
          )}
        </>
      )}
      {item.name === "send_message" && item.args?.text && (
        <MessagePreview text={String(item.args.text)} />
      )}

      {grants.length > 0 && (
        <div className="approval-grants" data-testid="approval-grants">
          {grants.map((g, i) => (
            <div className="approval-grant" key={i} data-access={g.access}>
              <span
                className={
                  "grant-mark" + (g.access === "write" ? " write" : "")
                }
              >
                {g.access === "write" ? "✓" : "·"}
              </span>
              <span className="grant-line">
                {translate(TOOL_VERBS[g.tool] || g.tool)}{" "}
                <code className="approval-tool">{g.target}</code>
                <span className="grant-note">
                  {g.access === "write"
                    ? ` — ${translate("alwaysAllowed")}`
                    : ` — ${translate("readOnly")}`}
                </span>
              </span>
            </div>
          ))}
        </div>
      )}
      {/* Long-tail tools: no bespoke preview — fall back to the compact args line. */}
      {!FILE_WRITES.has(item.name) &&
        !["run_shell", "send_message", "send_file"].includes(item.name) &&
        !grants.length &&
        shortArgs(item.args) && (
          <div className="approval-rest">{shortArgs(item.args)}</div>
        )}
      {reason && <div className="approval-reason">{reason}</div>}

      {item.resolved ? (
        <div className="resolved">
          {item.resolved === "allow"
            ? translate("approved")
            : translate("declined")}
        </div>
      ) : (
        <Buttons
          item={item}
          onApprove={onApprove}
          runTask={runTask}
          primaryLabel={translate("allowOnce")}
        />
      )}
    </div>
  );
}
