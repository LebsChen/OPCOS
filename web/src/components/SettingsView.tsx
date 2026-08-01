import type { ReactNode } from "react";
import { Icon, type IconName } from "./Icon";

export type SettingsSection =
  | "provider"
  | "hosts"
  | "agents"
  | "knowledge"
  | "playbook"
  | "skill"
  | "mcp"
  | "secrets"
  | "blueprint";

const tabs: Array<{ key: SettingsSection; label: string; icon: IconName }> = [
  { key: "provider", label: "Provider", icon: "sparkle" },
  { key: "hosts", label: "Hosts", icon: "folder" },
  { key: "agents", label: "AGENTS.md", icon: "fileCode" },
  { key: "knowledge", label: "Knowledge", icon: "file" },
  { key: "playbook", label: "Playbook", icon: "table" },
  { key: "skill", label: "Skill", icon: "sparkle" },
  { key: "mcp", label: "MCP", icon: "plug" },
  { key: "secrets", label: "Secrets", icon: "shield" },
  { key: "blueprint", label: "Blueprint", icon: "code" },
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
  return (
    <main className="flex-1 min-w-0 flex bg-paper">
      <nav className="page-subnav w-[208px] shrink-0 border-r border-line bg-panel/40 px-3 py-4">
        <div className="px-2 text-[13.5px] font-semibold mb-3 flex items-center gap-2">
          <Icon name="gear" size={16} /> Settings
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
            {tab.label}
          </button>
        ))}
      </nav>
      <div className="flex-1 min-w-0 overflow-y-auto hairline-scroll">
        <div className="w-full px-7 py-6">{children}</div>
      </div>
    </main>
  );
}
