import React from "react";

export function Icon({ name, size = 16 }: { name: string; size?: number }) {
  const paths: Record<string, string> = {
    plus: "M8 3v10M3 8h10",
    search: "m11 11 4 4m-1.5-8A4.5 4.5 0 1 1 4.5 7 4.5 4.5 0 0 1 13.5 7Z",
    settings:
      "M6.5 2h3l.5 2 1.5.8 1.8-.9 2.1 2.1-.9 1.8.8 1.5 2 .5v3l-2 .5-.8 1.5.9 1.8-2.1 2.1-1.8-.9-1.5.8-.5 2h-3l-.5-2-1.5-.8-1.8.9-2.1-2.1.9-1.8-.8-1.5-2-.5v-3l2-.5.8-1.5-.9-1.8L3.2 4.7l1.8.9 1.5-.8.5-2Z M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z",
    activity: "M2 8h3l1.5-4L10 12l1.5-4H15",
    send: "m2 2 13 6-13 6 3-6-3-6Zm3 6h10",
    stop: "M4 4h8v8H4z",
    terminal: "m3 4 4 4-4 4m6 0h4",
    desktop: "M2 3h12v8H2zM6 14h4M8 11v3",
    browser: "M2 3h12v10H2zM2 6h12M4 4.5h.01M6 4.5h.01",
    code: "m5 4-4 4 4 4m6-8 4 4-4 4M9 2 7 14",
    refresh: "M13 5V2l-2 2a5 5 0 1 0 1.2 6",
  };
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.5"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d={paths[name] || paths.plus} />
    </svg>
  );
}

export function Button({
  children,
  className = "",
  ...props
}: React.ButtonHTMLAttributes<HTMLButtonElement>) {
  return (
    <button className={`btn ${className}`} {...props}>
      {children}
    </button>
  );
}

export function SelectMenu({
  value,
  onChange,
  options,
}: {
  value: string;
  onChange: (value: string) => void;
  options: Array<{ value: string; label: string }>;
}) {
  return (
    <select
      className="select"
      value={value}
      onChange={(event) => onChange(event.target.value)}
    >
      {options.map((option) => (
        <option value={option.value} key={option.value}>
          {option.label}
        </option>
      ))}
    </select>
  );
}

export function Toggle({
  checked,
  onChange,
}: {
  checked: boolean;
  onChange: (checked: boolean) => void;
}) {
  return (
    <input
      type="checkbox"
      checked={checked}
      onChange={(event) => onChange(event.target.checked)}
    />
  );
}

export function ManageTabs({
  tabs,
  active,
  onChange,
}: {
  tabs: string[];
  active: string;
  onChange: (tab: string) => void;
}) {
  return (
    <div className="manage-tabs">
      {tabs.map((tab) => (
        <button
          className={active === tab ? "active" : ""}
          key={tab}
          onClick={() => onChange(tab)}
        >
          {tab}
        </button>
      ))}
    </div>
  );
}
