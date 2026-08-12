import { translate, translateBackendError } from "./i18n";

export type ErrorPresentation = {
  summary: string;
  toast: string;
  detail: string;
};

export type StepStatusKind = "running" | "ok" | "failed";

export function classifyStepStatus(status: string): StepStatusKind {
  if (status === "running" || status === "…") return "running";
  if (status === "ok") return "ok";
  return "failed";
}

function friendlyErrorText(text: string): string {
  return text.replace(/[_-]+/g, " ").replace(/\s+/g, " ").trim().toLowerCase();
}

export function providerErrorPresentation(text: string): ErrorPresentation {
  const translatedText = translateBackendError(text);
  const translatedProviderError =
    /^(Provider request failed|提供商请求失败)(?::|：)/.test(
      translatedText.trim(),
    );
  let status: number | undefined;
  let message = text;
  try {
    const value = JSON.parse(text) as Record<string, unknown>;
    status =
      typeof value.status_code === "number" ? value.status_code : undefined;
    const error = value.error;
    if (error && typeof error === "object") {
      const candidate = (error as Record<string, unknown>).message;
      if (typeof candidate === "string") message = candidate;
    }
  } catch {
    // Keep the raw provider text for non-JSON errors.
  }
  const statusText = status ? ` — HTTP ${status}` : "";
  return {
    summary: translate("providerRequestFailed", { status: statusText }),
    toast: translatedProviderError
      ? translatedText
      : friendlyErrorText(message),
    detail: text,
  };
}

export function isErrorNotice(item: {
  kind: string;
  noticeKind?: string;
  text?: string;
}): boolean {
  return (
    item.kind === "notice" &&
    (/error|failed|interrupted|provider/i.test(item.noticeKind || "") ||
      /provider|HTTP\s+\d{3}|request failed/i.test(item.text || ""))
  );
}

export function humanizeActivity(value: string): string {
  return value.replace(/[_-]+/g, " ");
}

export function toolArgumentSummary(value: unknown): string {
  if (!value || typeof value !== "object") return "";
  return Object.entries(value as Record<string, unknown>)
    .map(([key, item]) => `${key}=${String(item)}`)
    .join(", ");
}
