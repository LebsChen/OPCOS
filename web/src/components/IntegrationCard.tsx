import type { ReactNode } from "react";

export type IntegrationBadgeTone = "success" | "neutral" | "info";

export function IntegrationCard({
  icon,
  title,
  badge,
  description,
  onClick,
  disabled = false,
  actions,
}: {
  icon: ReactNode;
  title: ReactNode;
  badge?: {
    label: ReactNode;
    tone: IntegrationBadgeTone;
  };
  description?: ReactNode;
  onClick?: () => void;
  disabled?: boolean;
  actions?: ReactNode;
}) {
  const clickable = Boolean(onClick) && !disabled;
  return (
    <div
      className={`rounded-xl border border-line bg-panel px-3.5 py-3 transition-colors ${
        clickable ? "cursor-pointer hover:border-lineStrong" : ""
      } ${disabled ? "opacity-60" : ""}`}
      onClick={clickable ? onClick : undefined}
      onKeyDown={(event) => {
        if (clickable && (event.key === "Enter" || event.key === " ")) {
          event.preventDefault();
          onClick?.();
        }
      }}
      role={clickable ? "button" : undefined}
      tabIndex={clickable ? 0 : undefined}
      aria-disabled={disabled || undefined}
    >
      <div className="flex items-center gap-2.5 min-w-0">
        <span className="rounded-lg border border-line grid place-items-center shrink-0 w-8 h-8 bg-paper">
          <span className="text-[13px] font-semibold text-muted">{icon}</span>
        </span>
        <span className="min-w-0 flex-1">
          <span className="block text-[13px] font-semibold leading-tight truncate">
            {title}
          </span>
        </span>
        {badge && (
          <span
            className={`shrink-0 rounded-full px-2 py-0.5 text-[10.5px] font-medium ${
              badge.tone === "success"
                ? "bg-[#e8f7ee] text-[#23844d]"
                : badge.tone === "info"
                  ? "bg-[#e8f1ff] text-[#356bc2]"
                  : "bg-[#f0f1f3] text-[#68707d]"
            }`}
          >
            {badge.label}
          </span>
        )}
      </div>
      {description && (
        <div className="mt-2 line-clamp-2 text-[11.5px] leading-4 text-faint">
          {description}
        </div>
      )}
      {actions && (
        <div
          className="mt-2.5 flex flex-wrap items-center gap-2"
          onClick={(event) => event.stopPropagation()}
        >
          {actions}
        </div>
      )}
    </div>
  );
}
