// The OpenWorker composer is progressively adapted to OPCOS data sources.
import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type ReactNode,
} from "react";
import { translate } from "../i18n";
import type { Attachment, SessionUsage } from "../types";
const readFile = async (file: File): Promise<Attachment | null> => {
  const text = await file.text();
  return { kind: "text", name: file.name, text };
};
const formatTokens = (value: number) => String(value);
const totalTokens = (value: SessionUsage) =>
  Object.values(value?.byModel ?? {}).reduce(
    (sum, item) => sum + item.input + item.output,
    0,
  );
import type { Option } from "./Dropdown";
import { Icon } from "./Icon";
import { Toggle } from "./Toggle";
import { expandSlashCommandValue } from "../slashCommands";
import { submissionRoute } from "../gui";
type DictationStatus = {
  recording?: boolean;
  supported?: boolean;
  model_verified?: boolean;
  test_passed?: boolean;
};
const isTauri = (): boolean => false;
const cancelDictation = async () => undefined;
const getDictationLevel = async () => 0;
const getDictationStatus = async (): Promise<DictationStatus | null> => null;
const startDictation = async (): Promise<DictationStatus | null> => null;
const stopDictation = async (): Promise<string | null> => null;

// Visual source: OpenWorker surfaces/gui/src/components/Composer.tsx:1-813.
// OPCOS keeps the composer shell, controls, and class names and adapts only
// invoke-backed session actions, harness selection, assets, and attachments.

// Plan + Custom hidden for this release (owner ask 2026-07-22): Plan's approval flow isn't
// polished enough to ship, and Custom (config.toml auto-allow rules) is a power-user mode
// with no in-app explanation. The server still honors both — a session already in one of
// those modes keeps working; the picker just doesn't offer them.
const PERMISSION_OPTIONS: Option[] = [
  {
    value: "discuss",
    label: "discuss",
    description: "discussDescription",
  },
  {
    value: "interactive",
    label: "askForApproval",
    description: "askForApprovalDescription",
  },
  {
    value: "auto",
    label: "fullAccess",
    description: "fullAccessDescription",
  },
];

// No hardcoded model fallback: until the server supplies the list (a few seconds after a
// cold app boot), the picker renders a disabled "Loading models…" chip. A baked-in list
// goes stale and silently offers ids the backend never confirmed (caught 2026-07-21).

// Drop the provider prefix for display (anthropic:claude-opus-4-8 → claude-opus-4-8); full id on hover.
const shortModel = (m: string) =>
  m.includes(":") ? m.split(":").slice(1).join(":") : m;

// Identify an attachment by name + payload size so duplicates (e.g. the same file picked twice,
// or a prefill applied twice) collapse to one chip.
const attKey = (a: Attachment) =>
  a.kind === "text"
    ? `t:${a.name}:${a.text?.length ?? 0}`
    : `${a.kind[0]}:${a.name}:${a.data_url?.length ?? 0}`;
const mergeAttachments = (
  cur: Attachment[],
  add: Attachment[],
): Attachment[] => {
  const seen = new Set(cur.map(attKey));
  return [...cur, ...add.filter((a) => !seen.has(attKey(a)))].slice(0, 8);
};

interface Props {
  mode: string;
  harness?: string;
  harnessOptions?: Array<{
    id: string;
    label: string;
    available: boolean;
    reason?: string;
  }>;
  model: string;
  models?: string[];
  modelLabels?: Record<string, string>; // curated display names (raw id when absent)
  // The model is FIXED once the session has history (§17): the picker renders ONLY on a fresh
  // session; after the first turn the fact lives in the topbar subtitle (§22) — no
  // interactive-then-disabled control.
  running: boolean;
  connected: boolean;
  // False when the default model's provider has no key — the composer shows a "connect a model"
  // banner and routes sends to setup (preserving the draft) instead of dropping them.
  modelReady?: boolean;
  onConnectModel?: () => void;
  onConfigureVoiceInput?: () => void;
  onSend: (text: string, attachments?: Attachment[]) => void | Promise<void>;
  onPendingQuestionAnswer?: (text: string) => void | Promise<void>;
  pendingQuestion?: boolean;
  onSteer?: (text: string, attachments?: Attachment[]) => void | Promise<void>;
  onInterrupt: () => void;
  assets?: Array<{ kind: string; title: string }>;
  secrets?: Array<{ name: string }>;
  slashCommands?: Array<{
    name: string;
    body?: string;
    description?: string;
    input?: { hint?: string };
    kind?: string;
    execution?: string;
  }>;
  acpMode?: {
    currentModeId?: string | null;
    availableModes: Array<{ id: string; name: string; description?: string }>;
  };
  acpConfigOptions?: Array<{
    id: string;
    name: string;
    description?: string;
    type: "select" | "boolean";
    currentValue: string | boolean;
    options?: Array<{ value: string; name: string; description?: string }>;
  }>;
  onAcpModeChange?: (modeId: string) => void;
  onAcpConfigOptionChange?: (configId: string, value: string | boolean) => void;
  onUploadFile?: (file: File) => Promise<string>;
  onModeChange?: (mode: string) => void;
  onHarnessChange?: (harness: string) => void;
  onModelChange: (model: string) => void;
  // When set (Code/Cowork), the Mode menu is shown. The folder/roots + branch controls left the
  // composer for the Session settings drawer (§22) — folder access is standing session config.
  workspace?: string;
  // Unattended / send-approvals-to-Inbox — folded into the Mode menu (§22): "who approves, and
  // when" is one mental model. Absent handler = no toggle (e.g. Chat).
  unattended?: boolean;
  onUnattendedChange?: (on: boolean) => void;
  progressiveToolDisclosure?: boolean;
  onProgressiveToolDisclosureChange?: (on: boolean) => void;
  approvalSlot?: ReactNode;
  interactionHeader?: ReactNode;
  // Push text + attachments into the composer (e.g. a start-panel task card). The `nonce` makes
  // repeated identical prefills re-apply; the user can still edit before sending.
  prefill?: { text: string; attachments?: Attachment[]; nonce: number };
  restoreDraft?: { text: string; nonce: number };
  // Changes when the active conversation changes; clears any unsent draft.
  resetKey?: string;
  // Surface-specific hint shown in the empty textarea.
  placeholder?: string;
  // Per-session token usage (OPE-42) — absent/empty hides the usage chip entirely
  // (older servers, backends that don't report usage, fresh sessions).
  usage?: SessionUsage;
  // Context-window size (tokens) of the ACTIVE model, from the curated matrix;
  // undefined hides the fill meter (unverified/custom models) but keeps the counts.
  contextWindow?: number;
}

export function Composer(props: Props) {
  const legacyOpenCode = props.harness === "opencode";
  const [text, setText] = useState("");
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [plusOpen, setPlusOpen] = useState(false);
  const [dictation, setDictation] = useState<DictationStatus | null>(null);
  const [dictationBusy, setDictationBusy] = useState<string | null>(null);
  const [dictationError, setDictationError] = useState<string | null>(null);
  const [recordingSeconds, setRecordingSeconds] = useState(0);
  const [attachNotice, setAttachNotice] = useState<string | null>(null);
  const [slashQuery, setSlashQuery] = useState<string | null>(null);
  const fileInput = useRef<HTMLInputElement | null>(null);
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);
  const noticeTimer = useRef<number | null>(null);

  // Rejected-attachment notice: visible ~8s, then clears (or on ✕).
  const showAttachNotice = (message: string) => {
    setAttachNotice(message);
    if (noticeTimer.current) window.clearTimeout(noticeTimer.current);
    noticeTimer.current = window.setTimeout(() => setAttachNotice(null), 8000);
  };

  useLayoutEffect(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    const max = parseFloat(getComputedStyle(el).lineHeight || "22") * 4;
    const next = Math.min(el.scrollHeight, max);
    el.style.height = `${Math.max(next, 24)}px`;
    el.style.overflowY = el.scrollHeight > max ? "auto" : "hidden";
  }, [text]);

  // Apply a prefill (text + attachments) pushed from outside, then focus the composer. Applied at
  // most once per nonce (a ref guards against StrictMode/re-render double-fires), and attachments
  // are de-duplicated so the same file never lands twice.
  const appliedNonce = useRef<number>(-1);
  useEffect(() => {
    const p = props.prefill;
    if (!p || p.nonce === appliedNonce.current) return;
    appliedNonce.current = p.nonce;
    setText(p.text);
    if (p.attachments?.length)
      setAttachments((cur) => mergeAttachments(cur, p.attachments!));
    textareaRef.current?.focus();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.prefill?.nonce]);

  // Clear the draft when the conversation changes, so a half-typed message / picked file doesn't
  // bleed from one session into another.
  useEffect(() => {
    setText("");
    setAttachments([]);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.resetKey]);

  useEffect(() => {
    const draft = props.restoreDraft;
    if (!draft || draft.nonce === appliedNonce.current) return;
    appliedNonce.current = draft.nonce;
    setText(draft.text);
    textareaRef.current?.focus();
  }, [props.restoreDraft?.nonce]);

  // Dictation is intentionally native-only: the browser/dev build remains a local server client
  // and never turns on the browser microphone or ships audio anywhere.
  useEffect(() => {
    if (!isTauri()) return;
    const refresh = (event?: Event) => {
      const supplied = (event as CustomEvent<DictationStatus> | undefined)
        ?.detail;
      if (supplied) {
        setDictation(supplied);
        return;
      }
      void getDictationStatus().then(
        (status) => status && setDictation(status),
      );
    };
    refresh();
    window.addEventListener("coworker:voice-input-changed", refresh);
    return () =>
      window.removeEventListener("coworker:voice-input-changed", refresh);
  }, []);

  useEffect(() => {
    if (!dictation?.recording) {
      setRecordingSeconds(0);
      return;
    }
    const started = Date.now();
    const timer = window.setInterval(() => {
      setRecordingSeconds(Math.floor((Date.now() - started) / 1000));
    }, 250);
    return () => window.clearInterval(timer);
  }, [dictation?.recording]);

  // Live waveform: poll mic loudness at ~10Hz while recording; the bars scroll left so the
  // trace reads as a real input meter (owner catch on DMG #28 — the first cut's bars were
  // decorative constants and read as fake).
  const [levels, setLevels] = useState<number[]>([]);
  useEffect(() => {
    if (!dictation?.recording) {
      setLevels([]);
      return;
    }
    const timer = window.setInterval(() => {
      getDictationLevel().then((level) => {
        if (typeof level === "number")
          setLevels((cur) => [...cur.slice(-13), level]);
      });
    }, 100);
    return () => window.clearInterval(timer);
  }, [dictation?.recording]);

  useEffect(() => {
    if (!dictation?.recording) return;
    const cancelOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      void cancelDictation()
        .catch(() => undefined)
        .finally(() => {
          void getDictationStatus().then(
            (status) => status && setDictation(status),
          );
        });
    };
    window.addEventListener("keydown", cancelOnEscape);
    return () => window.removeEventListener("keydown", cancelOnEscape);
  }, [dictation?.recording]);

  const voiceReady =
    !!dictation?.supported &&
    !!dictation?.model_verified &&
    !!dictation?.test_passed;
  const recordingTime = `${Math.floor(recordingSeconds / 60)}:${String(recordingSeconds % 60).padStart(2, "0")}`;

  // Attach-time PDF thresholds (Settings → Token savings): a PDF over the user's page or
  const addFiles = async (files: FileList | File[]) => {
    const read = (await Promise.all(Array.from(files).map(readFile))).filter(
      (attachment): attachment is Attachment => attachment !== null,
    );
    if (read.length) setAttachments((a) => mergeAttachments(a, read));
  };

  const needsModel = props.modelReady === false;

  const insertReference = (value: string) => {
    setText((current) =>
      current.trim() ? `${current.trimEnd()} ${value}` : value,
    );
    setPlusOpen(false);
    textareaRef.current?.focus();
  };

  const expandSlashCommand = (value: string) => {
    return expandSlashCommandValue(
      value,
      (props.slashCommands ?? []).map((command) => ({
        ...command,
        body: command.body || "",
        kind: command.kind || "custom",
      })),
    );
  };

  const uploadFile = async (file: File) => {
    if (!props.onUploadFile) return;
    if (file.size > 256 * 1024) {
      setAttachNotice("Text attachments are limited to 256 KiB.");
      return;
    }
    if (
      !file.type.startsWith("text/") &&
      !/\.(md|txt|json|ya?ml|csv|log|rs|ts|tsx|js|py|go|toml)$/i.test(file.name)
    ) {
      setAttachNotice("Only text attachments are supported.");
      return;
    }
    try {
      const path = await props.onUploadFile(file);
      setAttachments((current) => [
        ...current,
        { kind: "text", name: file.name, text: "" },
      ]);
      insertReference(`[Attached file: ${path}]`);
    } catch (error) {
      setAttachNotice(
        error instanceof Error ? error.message : "Attachment upload failed.",
      );
    }
  };

  const submit = async () => {
    const t = text.trim();
    if (
      (!t && attachments.length === 0) ||
      dictation?.recording ||
      dictationBusy
    )
      return;
    const draftText = text;
    const draftAttachments = attachments;
    const restoreDraft = () => {
      setText(draftText);
      setAttachments(draftAttachments);
      const match = draftText.match(/^\/([^\s]*)$/);
      setSlashQuery(match ? match[1].toLowerCase() : null);
    };
    const clearDraft = () => {
      setSlashQuery(null);
      setText("");
      setAttachments([]);
    };
    if (props.onPendingQuestionAnswer) {
      clearDraft();
      try {
        await props.onPendingQuestionAnswer(expandSlashCommand(t));
      } catch (error) {
        restoreDraft();
        showAttachNotice(
          error instanceof Error ? error.message : "Answer submission failed.",
        );
      }
      return;
    }
    const route = submissionRoute(props.running, Boolean(props.onSteer));
    if (route === "blocked") {
      showAttachNotice(
        "The session is still running; your message was not sent.",
      );
      return;
    }
    if (route === "steer") {
      clearDraft();
      try {
        await props.onSteer!(t, attachments);
      } catch (error) {
        restoreDraft();
        showAttachNotice(
          error instanceof Error ? error.message : "Message submission failed.",
        );
      }
      return;
    }
    // No model connected: keep the draft (don't drop it) and send the user to setup instead.
    if (needsModel) {
      props.onConnectModel?.();
      return;
    }
    clearDraft();
    try {
      await props.onSend(expandSlashCommand(t), attachments);
    } catch (error) {
      restoreDraft();
      showAttachNotice(
        error instanceof Error ? error.message : "Message submission failed.",
      );
    }
  };

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      submit();
    }
  };

  const onPaste = (e: React.ClipboardEvent) => {
    const imgs = Array.from(e.clipboardData.items)
      .filter((it) => it.kind === "file" && it.type.startsWith("image/"))
      .map((it) => it.getAsFile())
      .filter(Boolean) as File[];
    if (imgs.length) {
      e.preventDefault();
      addFiles(imgs);
    }
  };

  const toggleDictation = async () => {
    if (!isTauri() || dictationBusy) return;
    setDictationError(null);
    try {
      if (dictation?.recording) {
        setDictationBusy("Transcribing…");
        const transcript = await stopDictation();
        if (transcript === null)
          throw new Error("Could not transcribe your recording.");
        if (transcript.trim()) {
          setText((draft) =>
            draft.trim()
              ? `${draft.trimEnd()} ${transcript.trim()}`
              : transcript.trim(),
          );
        }
        setDictation(await getDictationStatus());
        textareaRef.current?.focus();
        return;
      }

      const status = dictation || (await getDictationStatus());
      if (!status) throw new Error("Voice dictation is unavailable.");
      if (!status.supported || !status.model_verified || !status.test_passed) {
        props.onConfigureVoiceInput?.();
        return;
      }
      setDictationBusy("Starting microphone…");
      const recording = await startDictation();
      if (!recording?.recording)
        throw new Error("Could not start the microphone.");
      setDictation(recording);
    } catch (error) {
      setDictationError(
        error instanceof Error
          ? error.message
          : "Voice dictation is unavailable.",
      );
      const status = await getDictationStatus();
      if (status) setDictation(status);
    } finally {
      setDictationBusy(null);
    }
  };

  const modelsLoaded = !!(props.models && props.models.length);
  const modelOptions: Option[] = Array.from(
    new Set([props.model, ...(props.models || [])]),
  ).map((m) => ({
    value: m,
    label: props.modelLabels?.[m] || shortModel(m),
  }));

  const iconBtn =
    "w-7 h-7 grid place-items-center rounded-md text-muted hover:text-ink hover:bg-paper shrink-0";

  // The send button is accent only when there's something to send — subtle grey otherwise, so the
  // composer isn't carrying a constant blue dot.
  const hasContent = text.trim().length > 0;

  return (
    <div className="composer-wrap">
      {props.approvalSlot}

      {dictationError && (
        <div
          className="max-w-3xl mx-auto mb-2 px-1 text-[12px] text-red-600"
          role="alert"
        >
          {dictationError}
        </div>
      )}

      {/* Rejected-attachment notice (PDF over the user's Token-savings thresholds). */}
      {attachNotice && (
        <div
          data-testid="attach-notice"
          className="max-w-3xl mx-auto mb-1.5 flex items-center gap-2 rounded-lg border border-warnInk/30 bg-warnSoft px-3 py-1.5 text-[12.5px] text-warnInk"
        >
          <span className="flex-1">{attachNotice}</span>
          <button
            className="shrink-0 opacity-60 hover:opacity-100"
            onClick={() => setAttachNotice(null)}
            title={translate("dismiss")}
          >
            ✕
          </button>
        </div>
      )}

      <div
        className={`composer-card${props.interactionHeader ? " composer-card-interaction" : ""}`}
      >
        {props.interactionHeader}
        {legacyOpenCode && (
          <div className="px-3.5 pt-3.5 text-[12px] text-muted">
            {translate("legacySessionReadonly")}
          </div>
        )}
        <textarea
          ref={textareaRef}
          className="w-full block px-3.5 pt-3.5 pb-1.5 text-[14.5px]"
          placeholder={
            props.placeholder ||
            (props.pendingQuestion
              ? translate("typeAnswerEllipsis")
              : translate("askOpcos"))
          }
          value={text}
          onChange={(e) => {
            const value = e.target.value;
            setText(value);
            const match = value.match(/^\/([^\s]*)$/);
            setSlashQuery(match ? match[1].toLowerCase() : null);
          }}
          onKeyDown={onKey}
          rows={1}
          disabled={legacyOpenCode}
        />
        {slashQuery !== null && props.slashCommands && (
          <div className="px-3 pb-2 flex flex-wrap gap-1.5">
            {props.slashCommands
              .filter((command) =>
                command.name.slice(1).toLowerCase().startsWith(slashQuery),
              )
              .slice(0, 8)
              .map((command) => (
                <button
                  className="pill"
                  key={command.name}
                  type="button"
                  onClick={() => {
                    setText(`${command.name} `);
                    setSlashQuery(null);
                    textareaRef.current?.focus();
                  }}
                >
                  <span>{command.name}</span>
                  <span
                    className={`ml-1 text-[10px] uppercase tracking-wide ${
                      command.execution === "action"
                        ? "text-emerald-300"
                        : "text-sky-300"
                    }`}
                  >
                    {command.execution === "action"
                      ? translate("action")
                      : translate("prompt")}
                  </span>
                </button>
              ))}
          </div>
        )}

        <div className="pending-files">
          {attachments.map((attachment, index) => (
            <span className="pill att-pill" key={`${attachment.name}-${index}`}>
              <span>{attachment.name}</span>
              <button
                className="pill-x"
                type="button"
                title={translate("removeAttachment")}
                onClick={() =>
                  setAttachments((current) =>
                    current.filter((_, itemIndex) => itemIndex !== index),
                  )
                }
              >
                ×
              </button>
            </span>
          ))}
        </div>

        {/* Mode, model, and send/steer controls are the real OPCOS composer actions. */}
        <div className="composer-row">
          <PlusMenu
            open={plusOpen}
            onOpenChange={setPlusOpen}
            onUpload={uploadFile}
            onInsert={insertReference}
            assets={props.assets}
            secrets={props.secrets}
          />
          {/* Listening replaces the quiet middle controls with a LIVE waveform (mic RMS,
              polled ~10Hz, scrolling left) + elapsed time (§37). */}
          {dictation?.recording ? (
            <div
              className="voice-wave-row flex-1 flex items-center gap-2 ml-1"
              aria-hidden="true"
            >
              <span className="voice-wave-line" />
              <span className="voice-wave-bars">
                {Array.from({ length: 14 }, (_, index) => {
                  const level = levels[levels.length - 14 + index] ?? 0;
                  return (
                    <i
                      key={index}
                      style={{ height: Math.round(4 + level * 24) }}
                    />
                  );
                })}
              </span>
              <span className="text-[12px] text-muted tabular-nums">
                {recordingTime}
              </span>
            </div>
          ) : props.workspace !== undefined && props.onModeChange ? (
            <ModeMenu
              mode={props.mode}
              onModeChange={props.onModeChange}
              unattended={props.unattended}
              onUnattendedChange={props.onUnattendedChange}
              progressiveToolDisclosure={props.progressiveToolDisclosure}
              onProgressiveToolDisclosureChange={
                props.onProgressiveToolDisclosureChange
              }
            />
          ) : null}
          {props.harness === "acp" &&
            props.acpConfigOptions?.map((option) =>
              option.type === "boolean" ? (
                <label className="chip flex items-center gap-1" key={option.id}>
                  <input
                    type="checkbox"
                    checked={option.currentValue === true}
                    onChange={(event) =>
                      props.onAcpConfigOptionChange?.(
                        option.id,
                        event.target.checked,
                      )
                    }
                  />
                  {option.name}
                </label>
              ) : (
                <select
                  className="chip"
                  key={option.id}
                  title={option.description || option.name}
                  value={String(option.currentValue)}
                  onChange={(event) =>
                    props.onAcpConfigOptionChange?.(
                      option.id,
                      event.target.value,
                    )
                  }
                >
                  {option.options?.map((item) => (
                    <option key={item.value} value={item.value}>
                      {item.name}
                    </option>
                  ))}
                </select>
              ),
            )}
          {props.harness === "acp" &&
            !props.acpConfigOptions?.length &&
            !!props.acpMode?.availableModes.length &&
            props.onAcpModeChange && (
              <select
                className="chip"
                title={translate("acpMode")}
                value={props.acpMode.currentModeId || ""}
                onChange={(event) =>
                  props.onAcpModeChange?.(event.target.value)
                }
              >
                {props.acpMode.availableModes.map((mode) => (
                  <option key={mode.id} value={mode.id}>
                    {mode.name}
                  </option>
                ))}
              </select>
            )}
          {!dictation?.recording &&
            props.harness &&
            props.onHarnessChange &&
            props.harnessOptions?.length && (
              <select
                className="chip"
                title={translate("harness")}
                value={props.harness}
                onChange={(event) =>
                  props.onHarnessChange?.(event.target.value)
                }
              >
                {(legacyOpenCode &&
                !props.harnessOptions.some((option) => option.id === "opencode")
                  ? [
                      ...props.harnessOptions,
                      {
                        id: "opencode",
                        label: "OpenCode (read-only)",
                        available: false,
                      },
                    ]
                  : props.harnessOptions
                ).map((option) => (
                  <option
                    key={option.id}
                    value={option.id}
                    disabled={!option.available}
                  >
                    {option.label}
                    {!option.available ? ` (${translate("unavailable")})` : ""}
                  </option>
                ))}
              </select>
            )}

          {dictationBusy === "Transcribing…" && (
            <span className="text-[11.5px] text-accent">
              {translate("transcribing")}
            </span>
          )}

          <span className="ml-auto" />

          {/* token usage (OPE-42) — a quiet meter+count chip; hidden until the server
              reports usage. Fill = context-window occupancy (bounded), count = session
              consumption (unbounded, so never a fill). */}
          {!dictation?.recording &&
            props.usage &&
            totalTokens(props.usage) > 0 && (
              <UsageChip
                usage={props.usage}
                contextWindow={props.contextWindow}
                model={props.model}
                modelLabels={props.modelLabels}
              />
            )}

          {/* model — a quiet chip, now for the session's whole life (§17 rev 2026-07-22:
              mid-session switching shipped, so the picker stays actionable; the topbar
              subtitle still states the current model). */}
          {!dictation?.recording &&
            (needsModel ? (
              <button
                className="pill model-warn chip"
                onClick={() => props.onConnectModel?.()}
                title={translate("connectModel")}
                aria-label={translate("connectModel")}
              >
                <span className="pill-label">{translate("noModel")}</span>
                <span className="model-warn-ico" aria-hidden>
                  ⚠
                </span>
              </button>
            ) : modelsLoaded ? (
              <select
                className="chip model-chip"
                title={translate("model")}
                value={props.model}
                onChange={(event) => props.onModelChange(event.target.value)}
              >
                {modelOptions.map((option) => (
                  <option key={option.value} value={option.value}>
                    {option.label}
                  </option>
                ))}
              </select>
            ) : (
              <button
                className="pill chip text-faint cursor-default"
                disabled
                data-testid="models-loading"
                title={translate("fetchingModelList")}
              >
                <span className="pill-label">{translate("loadingModels")}</span>
              </button>
            ))}

          {/* mic — immediately before send (owner call, DMG #28 walkthrough) */}
          {isTauri() && (
            <button
              className={
                iconBtn +
                (dictation?.recording
                  ? " bg-red-50 text-red-600 hover:bg-red-100"
                  : "") +
                (dictationBusy ? " opacity-60" : "") +
                (!voiceReady && !dictation?.recording ? " opacity-40" : "")
              }
              onClick={() => void toggleDictation()}
              disabled={!!dictationBusy}
              title={
                dictationBusy ||
                (dictation?.recording
                  ? translate("stopRecordingAndTranscribe")
                  : voiceReady
                    ? translate("startLocalVoiceDictation")
                    : translate("configureVoiceInput"))
              }
              aria-label={
                dictation?.recording
                  ? translate("stopDictation")
                  : voiceReady
                    ? translate("startDictation")
                    : translate("configureVoiceInput")
              }
              aria-disabled={!voiceReady && !dictation?.recording}
            >
              <Icon name={dictation?.recording ? "stop" : "mic"} size={16} />
            </button>
          )}

          {/* send / stop */}
          <SendButton
            running={props.running}
            disabled={
              !props.connected ||
              legacyOpenCode ||
              !!dictation?.recording ||
              !!dictationBusy ||
              (!hasContent && !props.running)
            }
            onSend={submit}
            onInterrupt={props.onInterrupt}
            title={needsModel ? translate("connectModelToSend") : undefined}
          />
        </div>
      </div>
      <span className="sr-only" role="status" aria-live="polite">
        {dictation?.recording
          ? `${translate("listening")}, ${recordingTime}`
          : dictationBusy || ""}
      </span>
    </div>
  );
}

export function SendButton({
  running,
  disabled,
  onSend,
  onInterrupt,
  title,
}: {
  running: boolean;
  disabled?: boolean;
  onSend: () => void;
  onInterrupt: () => void;
  title?: string;
}) {
  return (
    <button
      className={`send-btn${running ? " sending" : ""}`}
      type="button"
      onClick={running ? onInterrupt : onSend}
      disabled={disabled && !running}
      title={title}
      aria-label={running ? translate("stop") : translate("send")}
    >
      {running ? (
        <span aria-hidden="true">■</span>
      ) : (
        <svg
          width="15"
          height="15"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth={2}
          strokeLinecap="round"
          strokeLinejoin="round"
          aria-hidden="true"
        >
          <path d="M12 19V5M5 12l7-7 7 7" />
        </svg>
      )}
    </button>
  );
}

export function PlusMenu({
  open,
  onOpenChange,
  onUpload,
  onInsert,
  assets = [],
  secrets = [],
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onUpload?: (file: File) => void | Promise<void>;
  onInsert: (value: string) => void;
  assets?: Array<{ kind: string; title: string }>;
  secrets?: Array<{ name: string }>;
}) {
  const fileRef = useRef<HTMLInputElement>(null);
  const wrapRef = useRef<HTMLSpanElement>(null);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [menuStyle, setMenuStyle] = useState<{
    top: number;
    left: number;
    maxHeight: number;
  } | null>(null);
  const [openSubmenu, setOpenSubmenu] = useState<string | null>(null);
  const [submenuStyle, setSubmenuStyle] = useState<{
    top: number;
    left: number;
    maxHeight: number;
  } | null>(null);
  const submenuRef = useRef<HTMLDivElement>(null);
  const submenuTriggerRefs = useRef<Record<string, HTMLButtonElement | null>>(
    {},
  );
  const openSubmenuRef = useRef<string | null>(null);
  openSubmenuRef.current = openSubmenu;
  const closeMenu = (restoreFocus = true) => {
    onOpenChange(false);
    if (restoreFocus) triggerRef.current?.focus();
  };
  useEffect(() => {
    if (!open) return;
    const close = (event: MouseEvent) => {
      if (!wrapRef.current?.contains(event.target as Node)) closeMenu();
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        if (openSubmenuRef.current) {
          const submenu = openSubmenuRef.current;
          setOpenSubmenu(null);
          submenuTriggerRefs.current[submenu]?.focus();
        } else {
          closeMenu();
        }
      }
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", escape);
    };
  }, [open, openSubmenu]);
  useLayoutEffect(() => {
    if (!open || !menuRef.current || !triggerRef.current) return;
    const trigger = triggerRef.current.getBoundingClientRect();
    const menu = menuRef.current.getBoundingClientRect();
    const margin = 8;
    const maxHeight = Math.max(80, window.innerHeight - margin * 2);
    const height = Math.min(menu.height, maxHeight);
    const left = Math.min(
      Math.max(margin, trigger.left),
      Math.max(margin, window.innerWidth - menu.width - margin),
    );
    const above = trigger.top - menu.height - 6;
    const top =
      above >= margin
        ? above
        : Math.min(
            trigger.bottom + 6,
            Math.max(margin, window.innerHeight - height - margin),
          );
    setMenuStyle({ top, left, maxHeight });
    menuRef.current
      .querySelector<HTMLButtonElement>("button:not(:disabled)")
      ?.focus();
  }, [open, assets.length, secrets.length]);
  useEffect(() => {
    if (!openSubmenu) return;
    const trigger = submenuTriggerRefs.current[openSubmenu];
    if (trigger && submenuRef.current) {
      const triggerRect = trigger.getBoundingClientRect();
      const submenuRect = submenuRef.current.getBoundingClientRect();
      const margin = 8;
      const gap = 4;
      const maxHeight = Math.max(80, window.innerHeight - margin * 2);
      const height = Math.min(submenuRect.height, maxHeight);
      const left =
        triggerRect.right + gap + submenuRect.width <=
        window.innerWidth - margin
          ? triggerRect.right + gap
          : Math.max(margin, triggerRect.left - submenuRect.width - gap);
      const top = Math.min(
        Math.max(margin, triggerRect.top),
        Math.max(margin, window.innerHeight - height - margin),
      );
      setSubmenuStyle({ top, left, maxHeight });
    }
    submenuRef.current
      ?.querySelector<HTMLButtonElement>("button:not(:disabled)")
      ?.focus();
  }, [openSubmenu]);
  const upload = () => {
    closeMenu();
    fileRef.current?.click();
  };
  const assetItems = assets.filter((asset) =>
    ["agents", "knowledge", "playbook", "skill"].includes(asset.kind),
  );
  const categories = [
    {
      kind: "agents",
      label: "rules",
      reference: "@AGENTS.md",
      icon: "shield" as const,
    },
    {
      kind: "knowledge",
      label: "knowledge",
      reference: "@Knowledge",
      icon: "inbox" as const,
    },
    {
      kind: "playbook",
      label: "playbooks",
      reference: "@Playbook",
      icon: "board" as const,
    },
    {
      kind: "skill",
      label: "skills",
      reference: "@Skill",
      icon: "wrench" as const,
    },
  ];
  return (
    <span className="plus-menu-wrap" ref={wrapRef}>
      <input
        ref={fileRef}
        type="file"
        accept=".md,.txt,.json,.yaml,.yml,.csv,.log,.rs,.ts,.tsx,.js,.py,.go,.toml,text/*"
        hidden
        onChange={(event) => {
          const file = event.target.files?.[0];
          event.target.value = "";
          if (file) void onUpload?.(file);
        }}
      />
      <button
        ref={triggerRef}
        className="icon-btn"
        type="button"
        aria-label={translate("moreFeatures")}
        title={translate("moreFeatures")}
        aria-expanded={open}
        aria-haspopup="menu"
        onClick={() => onOpenChange(!open)}
      >
        +
      </button>
      {open && (
        <div
          ref={menuRef}
          className="plus-menu"
          role="menu"
          style={menuStyle ?? undefined}
        >
          {onUpload && (
            <button type="button" role="menuitem" onClick={upload}>
              <Icon name="file" size={15} className="pm-icon" />
              {translate("uploadAttachment")}
            </button>
          )}
          <div className="pm-divider" />
          {categories.map((category) => {
            const matching = assetItems.filter(
              (asset) => asset.kind === category.kind,
            );
            const hasSubmenu = matching.length > 0;
            return (
              <div className="pm-group" key={category.kind}>
                <button
                  ref={(element) => {
                    submenuTriggerRefs.current[category.kind] = element;
                  }}
                  type="button"
                  role="menuitem"
                  aria-haspopup={hasSubmenu ? "menu" : undefined}
                  aria-expanded={hasSubmenu && openSubmenu === category.kind}
                  onKeyDown={(event) => {
                    if (
                      hasSubmenu &&
                      (event.key === "Enter" || event.key === "ArrowRight")
                    ) {
                      event.preventDefault();
                      setOpenSubmenu(category.kind);
                    }
                  }}
                  onClick={() => {
                    if (hasSubmenu) {
                      setOpenSubmenu((value) =>
                        value === category.kind ? null : category.kind,
                      );
                    } else {
                      closeMenu(false);
                      onInsert(category.reference);
                    }
                  }}
                >
                  <Icon name={category.icon} size={15} className="pm-icon" />
                  <span>{translate(category.label)}</span>
                  {hasSubmenu && (
                    <Icon
                      name="chevronRight"
                      size={13}
                      className="pm-chevron"
                    />
                  )}
                </button>
                {hasSubmenu && openSubmenu === category.kind && (
                  <div
                    ref={submenuRef}
                    className="pm-submenu"
                    role="menu"
                    aria-label={translate("categoryItems", {
                      label: category.label,
                    })}
                    style={submenuStyle ?? undefined}
                    onKeyDown={(event) => {
                      if (event.key === "Escape" || event.key === "ArrowLeft") {
                        event.preventDefault();
                        event.stopPropagation();
                        setOpenSubmenu(null);
                        submenuTriggerRefs.current[category.kind]?.focus();
                      }
                    }}
                  >
                    {matching.map((asset) => (
                      <button
                        type="button"
                        role="menuitem"
                        key={`${asset.kind}:${asset.title}`}
                        onClick={() => {
                          closeMenu(false);
                          onInsert(`@${asset.kind}:${asset.title}`);
                        }}
                      >
                        <Icon name="file" size={14} className="pm-icon" />
                        {asset.title}
                      </button>
                    ))}
                  </div>
                )}
              </div>
            );
          })}
          <div className="pm-divider" />
          {secrets.length > 0 ? (
            <div className="pm-group">
              <button
                ref={(element) => {
                  submenuTriggerRefs.current.secrets = element;
                }}
                type="button"
                role="menuitem"
                aria-haspopup="menu"
                aria-expanded={openSubmenu === "secrets"}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === "ArrowRight") {
                    event.preventDefault();
                    setOpenSubmenu("secrets");
                  }
                }}
                onClick={() =>
                  setOpenSubmenu((value) =>
                    value === "secrets" ? null : "secrets",
                  )
                }
              >
                <Icon name="shield" size={15} className="pm-icon" />
                <span>{translate("secrets")}</span>
                <Icon name="chevronRight" size={13} className="pm-chevron" />
              </button>
              {openSubmenu === "secrets" && (
                <div
                  ref={submenuRef}
                  className="pm-submenu"
                  role="menu"
                  aria-label={translate("secretsItems")}
                  style={submenuStyle ?? undefined}
                  onKeyDown={(event) => {
                    if (event.key === "Escape" || event.key === "ArrowLeft") {
                      event.preventDefault();
                      event.stopPropagation();
                      setOpenSubmenu(null);
                      submenuTriggerRefs.current.secrets?.focus();
                    }
                  }}
                >
                  {secrets.map((secret) => (
                    <button
                      type="button"
                      role="menuitem"
                      key={secret.name}
                      onClick={() => {
                        closeMenu(false);
                        onInsert(`secret:session:${secret.name}`);
                      }}
                    >
                      <Icon name="shield" size={14} className="pm-icon" />
                      {secret.name}
                    </button>
                  ))}
                </div>
              )}
            </div>
          ) : (
            <button type="button" role="menuitem" disabled>
              <Icon name="shield" size={15} className="pm-icon" />
              {translate("secrets")}
            </button>
          )}
        </div>
      )}
    </span>
  );
}

// Token-usage chip + popover (OPE-42). Trigger: a tiny context-fill meter (only when the
// active model's window is known) + the session's total token count. Click → per-model
// breakdown. Tokens only, never dollars (true cost is unknowable client-side — discounted
// pricing, per-provider cache billing).
function UsageChip({
  usage,
  contextWindow,
  model,
  modelLabels,
}: {
  usage: SessionUsage;
  contextWindow?: number;
  model: string;
  modelLabels?: Record<string, string>;
}) {
  const [open, setOpen] = useState(false);
  const total = totalTokens(usage);
  const pct = contextWindow
    ? Math.min(100, Math.round((usage.context / contextWindow) * 100))
    : null;
  const labelFor = (id: string) =>
    id === "unknown" ? "Unknown model" : modelLabels?.[id] || shortModel(id);
  // One field per line, session-summed (owner ask 2026-07-28). Values are cumulative
  // across the whole session, never just the last turn; "Input" is the fresh
  // (uncached) share — the cached share sits in the cache rows at its own price.
  const stat = (label: string, value: number) => (
    <div className="flex items-baseline justify-between text-[11.5px] leading-snug">
      <span className="text-faint">{label}</span>
      <span className="text-ink tabular-nums">{formatTokens(value)}</span>
    </div>
  );
  return (
    <div className="relative">
      <button
        className="inline-flex items-center gap-1.5 px-2 py-1 rounded-lg text-[11.5px] text-muted hover:text-ink hover:bg-paper shrink-0"
        onClick={() => setOpen((v) => !v)}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={translate("tokenUsage")}
        title={
          pct !== null
            ? `${translate("tokenUsage")} — ${pct}% ${translate("contextWindow")}`
            : `${translate("tokenUsage")} — ${translate("sessionOnly")}`
        }
        data-testid="usage-chip"
      >
        {pct !== null && (
          <span
            className="w-7 h-1 rounded-full bg-line overflow-hidden"
            aria-hidden="true"
          >
            <span
              className="block h-full bg-accent transition-all"
              style={{ width: `${Math.max(pct, 4)}%` }}
            />
          </span>
        )}
        <span className="tabular-nums">{formatTokens(total)}</span>
      </button>
      {open && (
        <>
          <div className="fixed inset-0 z-30" onClick={() => setOpen(false)} />
          <div
            className="absolute z-40 bottom-full mb-1 right-0 w-[280px] rounded-xl border border-line bg-panel shadow-2xl p-3"
            role="menu"
            data-testid="usage-popover"
          >
            {contextWindow ? (
              <div className="mb-2.5">
                <div className="text-[10.5px] uppercase tracking-[0.06em] text-faint font-semibold mb-1">
                  {translate("contextWindow")}
                </div>
                <div className="h-1.5 rounded-full bg-line overflow-hidden">
                  <div
                    className="h-full bg-accent transition-all"
                    style={{ width: `${pct}%` }}
                  />
                </div>
                <div className="mt-1 text-[11.5px] text-muted tabular-nums">
                  {formatTokens(usage.context)} / {formatTokens(contextWindow)}{" "}
                  · {pct}%
                </div>
              </div>
            ) : usage.context > 0 ? (
              <div className="mb-2.5 text-[11.5px] text-muted tabular-nums">
                {translate("inContextNow", {
                  count: formatTokens(usage.context),
                })}
              </div>
            ) : null}
            <div className="text-[10.5px] uppercase tracking-[0.06em] text-faint font-semibold mb-1">
              {translate("sessionTotals")}
            </div>
            <div className="flex flex-col gap-1.5">
              {Object.entries(usage.byModel).map(([id, t]) => (
                <div key={id}>
                  <div
                    className="text-[12px] text-ink font-medium truncate"
                    title={id}
                  >
                    {labelFor(id)}
                  </div>
                  {/* Every row is a session sum. With a cache split, the input rows are
                      the three BILLING CLASSES of input (each priced differently) and
                      read as components: uncached + cache reads + cache writes = total.
                      Without one (Ollama, compat vendors), plain "Input" says it all. */}
                  <div className="mt-0.5 flex flex-col gap-0.5">
                    {t.cache_read + t.cache_write > 0 ? (
                      <>
                        {stat(translate("uncachedInput"), t.input)}
                        {stat(translate("cacheReads"), t.cache_read)}
                        {stat(translate("cacheWrites"), t.cache_write)}
                        {stat(
                          translate("totalInput"),
                          t.input + t.cache_read + t.cache_write,
                        )}
                      </>
                    ) : (
                      stat(translate("input"), t.input)
                    )}
                    {stat(translate("output"), t.output)}
                  </div>
                </div>
              ))}
            </div>
            <div className="mt-2 pt-2 border-t border-line flex items-baseline justify-between text-[11.5px]">
              <span className="text-faint">{translate("total")}</span>
              <span className="text-ink tabular-nums">
                {formatTokens(total)} {translate("tokens")}
              </span>
            </div>
            {model && !modelLabels?.[model] && contextWindow === undefined && (
              <div className="mt-1 text-[10.5px] text-faint leading-snug">
                {translate("customModelContextUnavailable")}
              </div>
            )}
          </div>
        </>
      )}
    </div>
  );
}

// The composer's Mode menu (§22): a quiet "Mode ⌄" chip opening the five permission options with
// the current one marked, plus — when the session supports it — the "Send approvals to Inbox"
// toggle at the bottom (the old standalone InboxControl, folded in).
function ModeMenu({
  mode,
  onModeChange,
  unattended,
  onUnattendedChange,
  progressiveToolDisclosure,
  onProgressiveToolDisclosureChange,
}: {
  mode: string;
  onModeChange: (mode: string) => void;
  unattended?: boolean;
  onUnattendedChange?: (on: boolean) => void;
  progressiveToolDisclosure?: boolean;
  onProgressiveToolDisclosureChange?: (on: boolean) => void;
}) {
  const [open, setOpen] = useState(false);
  const current = PERMISSION_OPTIONS.find((o) => o.value === mode);
  return (
    <div className="relative">
      {/* Borderless, and it names the CHOSEN mode (owner ask 2026-07-11, competitor composer
          comparison): "Ask for approval ⌄" not a generic "Mode ⌄" pill. aria-label stays
          "Mode" so the accessible name is stable across mode changes. */}
      <button
        className="inline-flex items-center gap-1 px-2 py-1 rounded-lg text-[12px] text-muted hover:text-ink hover:bg-paper shrink-0"
        onClick={() => setOpen((v) => !v)}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={translate("mode")}
        title={
          `Mode: ${current?.label || mode}` +
          (unattended ? " · approvals go to the Inbox" : "")
        }
      >
        {current?.label || mode}
        <Icon name="chevronDown" size={11} className="text-faint" />
      </button>
      {open && (
        <>
          <div className="fixed inset-0 z-30" onClick={() => setOpen(false)} />
          <div
            className="absolute z-40 bottom-full mb-1 left-0 w-[260px] rounded-xl border border-line bg-panel shadow-2xl p-1.5"
            role="menu"
            data-testid="mode-menu"
          >
            {PERMISSION_OPTIONS.map((o) => (
              <button
                key={o.value}
                className="w-full flex flex-col items-start px-2.5 py-1.5 rounded-lg text-left hover:bg-paper"
                onClick={() => {
                  onModeChange(o.value);
                  setOpen(false);
                }}
              >
                <span
                  className={
                    "text-[13px] " +
                    (o.value === mode ? "font-medium text-accent" : "text-ink")
                  }
                >
                  {translate(o.label)}
                  {o.value === mode && <span className="ml-1.5">✓</span>}
                </span>
                <span className="text-[11px] text-faint leading-snug">
                  {translate(o.description || "")}
                </span>
              </button>
            ))}
            {onUnattendedChange && (
              <>
                <div className="my-1 border-t border-line" />
                <div className="flex items-center gap-2 px-2.5 py-1.5">
                  <span className="flex-1 min-w-0">
                    <span className="block text-[13px] text-ink">
                      {translate("sendApprovalsInbox")}
                    </span>
                    <span className="block text-[11px] text-faint leading-snug">
                      {translate("approvalsQuestionsContinue")}
                    </span>
                  </span>
                  <Toggle
                    checked={!!unattended}
                    onChange={onUnattendedChange}
                    title={translate("sendApprovalsInbox")}
                  />
                </div>
              </>
            )}
            {onProgressiveToolDisclosureChange && (
              <>
                <div className="my-1 border-t border-line" />
                <div className="flex items-center gap-2 px-2.5 py-1.5">
                  <span className="flex-1 min-w-0">
                    <span className="block text-[13px] text-ink">
                      {translate("progressiveDisclosure")}
                    </span>
                    <span className="block text-[11px] text-faint leading-snug">
                      {translate("progressiveDisclosureDescription")}
                    </span>
                  </span>
                  <Toggle
                    checked={!!progressiveToolDisclosure}
                    onChange={onProgressiveToolDisclosureChange}
                    title={translate("progressiveDisclosure")}
                  />
                </div>
              </>
            )}
          </div>
        </>
      )}
    </div>
  );
}

// A row in the "+" attach menu.
function attachItem(
  icon: "image" | "file" | "fileCode",
  label: string,
  onClick: () => void,
) {
  return (
    <button
      className="w-full flex items-center gap-2.5 px-3 py-1.5 text-[13px] text-left hover:bg-paper"
      onClick={onClick}
    >
      <Icon name={icon} size={15} className="shrink-0 text-muted" /> {label}
    </button>
  );
}

function AttachChip({ a, onRemove }: { a: Attachment; onRemove: () => void }) {
  return (
    <div className={"attach-chip" + (a.kind === "image" ? " img" : "")}>
      {a.kind === "image" ? (
        <img src={a.data_url} alt={a.name} />
      ) : (
        <>
          <Icon name="file" size={13} />
          <span className="attach-name">{a.name}</span>
        </>
      )}
      <button
        className="attach-x"
        onClick={onRemove}
        title={translate("remove")}
      >
        ✕
      </button>
    </div>
  );
}
