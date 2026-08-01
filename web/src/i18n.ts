export type Locale = "en" | "zh";

const storageKey = "opcos.locale";
let locale: Locale = localStorage.getItem(storageKey) === "zh" ? "zh" : "en";
const listeners = new Set<() => void>();

const messages: Record<Locale, Record<string, string>> = {
  en: {
    general: "General",
    appearanceDescription:
      "Set the appearance and language of the OPCOS workbench.",
    theme: "Theme",
    themeDescription: "Choose the light, dark, or system appearance.",
    language: "Language",
    languageDescription: "Choose the language used by the workbench.",
    light: "Light",
    dark: "Dark",
    auto: "Auto",
    english: "English",
    chinese: "中文",
  },
  zh: {
    general: "通用",
    appearanceDescription: "设置 OPCOS 工作台的外观和语言。",
    theme: "主题",
    themeDescription: "选择浅色、深色或跟随系统。",
    language: "语言",
    languageDescription: "选择工作台使用的语言。",
    light: "浅色",
    dark: "深色",
    auto: "自动",
    english: "English",
    chinese: "中文",
  },
};

export function getLocale(): Locale {
  return locale;
}

export function setLocale(next: Locale): void {
  locale = next;
  localStorage.setItem(storageKey, next);
  document.documentElement.lang = next;
  listeners.forEach((listener) => listener());
}

export function subscribeLocale(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function translate(key: string): string {
  return messages[locale][key] || messages.en[key] || key;
}

setLocale(locale);
