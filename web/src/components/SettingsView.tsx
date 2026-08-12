import { useEffect, useState, type ReactNode } from "react";
import { Icon, type IconName } from "./Icon";
import { subscribeLocale, translate } from "../i18n";

// Shell source: OpenWorker surfaces/gui/src/components/SettingsView.tsx:85-123.
// The body pages are OPCOS-only asset/host surfaces, so their data adapters stay
// local while the reference subnav, centered content width, and row vocabulary
// remain unchanged.

export type SettingsSection =
  | "appearance"
  | "agent"
  | "environment"
  | "experts"
  | "teams"
  | "command"
  | "provider"
  | "hosts"
  | "agents"
  | "instructions"
  | "knowledge"
  | "playbook"
  | "skill"
  | "mcp"
  | "connectors"
  | "ingress"
  | "index"
  | "secrets"
  | "blueprint";

const tabs: Array<{ key: SettingsSection; label: string; icon: IconName }> = [
  { key: "appearance", label: "general", icon: "sliders" },
  { key: "agent", label: "agentDefaults", icon: "sparkle" },
  { key: "environment", label: "environmentSection", icon: "folder" },
  { key: "experts", label: "experts", icon: "sparkle" },
  { key: "teams", label: "teams", icon: "board" },
  { key: "command", label: "commandSection", icon: "terminal" },
  { key: "provider", label: "provider", icon: "sparkle" },
  { key: "hosts", label: "hosts", icon: "folder" },
  { key: "agents", label: "rules", icon: "fileCode" },
  { key: "instructions", label: "instructions", icon: "fileCode" },
  { key: "knowledge", label: "knowledgeSection", icon: "file" },
  { key: "playbook", label: "playbookSection", icon: "table" },
  { key: "skill", label: "skillSection", icon: "sparkle" },
  { key: "mcp", label: "mcp", icon: "plug" },
  { key: "connectors", label: "connectors", icon: "globe" },
  { key: "ingress", label: "externalEvents", icon: "globe" },
  { key: "index", label: "index", icon: "search" },
  { key: "secrets", label: "secrets", icon: "shield" },
  { key: "blueprint", label: "blueprint", icon: "code" },
];

export function SettingsView({
  activeTab,
  onTabChange,
  children,
}: {
  activeTab: SettingsSection;
  onTabChange: (tab: SettingsSection) => void;
  children: ReactNode;
}) {
  const [, setLocaleVersion] = useState(0);
  useEffect(
    () => subscribeLocale(() => setLocaleVersion((value) => value + 1)),
    [],
  );
  return (
    <main className="flex-1 min-w-0 min-h-0 flex bg-paper">
      <nav className="page-subnav w-[208px] shrink-0 border-r border-line bg-panel/40 px-3 py-4">
        <div className="px-2 text-[13.5px] font-semibold mb-3 flex items-center gap-2">
          <Icon name="gear" size={16} /> {translate("settings")}
        </div>
        {tabs.map((tab) => (
          <button
            key={tab.key}
            className={
              "w-full text-left px-2.5 py-2 rounded-lg text-[13px] flex items-center gap-2 " +
              (activeTab === tab.key
                ? "bg-paper text-accent font-medium"
                : "text-muted hover:bg-paper hover:text-ink")
            }
            onClick={() => onTabChange(tab.key)}
          >
            <Icon name={tab.icon} size={15} />
            {translate(tab.label)}
          </button>
        ))}
      </nav>
      <div className="flex-1 min-w-0 overflow-y-auto hairline-scroll">
        <div className="w-full px-7 py-6">{children}</div>
      </div>
    </main>
  );
}
