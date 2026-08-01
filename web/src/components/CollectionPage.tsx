import { useEffect, useState, type ReactNode } from "react";

export type CollectionView = "list" | "grid";

export function CollectionPage({
  search,
  onSearch,
  searchPlaceholder,
  actions,
  primary,
  rows,
  empty,
  form,
  viewKey = "opcos.collection.view",
  renderCard,
  columns,
  chips,
  activeChip,
  onChip,
}: {
  search: string;
  onSearch: (value: string) => void;
  searchPlaceholder: string;
  actions?: ReactNode;
  primary?: ReactNode;
  rows: ReactNode;
  empty: string;
  form?: ReactNode;
  viewKey?: string;
  renderCard?: () => ReactNode;
  columns?: string[];
  chips?: string[];
  activeChip?: string;
  onChip?: (chip: string) => void;
}) {
  const [view, setView] = useState<CollectionView>(() => {
    const stored = localStorage.getItem(viewKey);
    return stored === "grid" ? "grid" : "list";
  });
  useEffect(() => localStorage.setItem(viewKey, view), [view, viewKey]);
  const hasRows = rows !== null && rows !== undefined && rows !== false;
  return (
    <>
      <div className="flex items-center gap-2 mb-3">
        <input
          className="input flex-1"
          placeholder={searchPlaceholder}
          value={search}
          onChange={(event) => onSearch(event.target.value)}
        />
        {chips?.map((chip) => (
          <button
            key={chip}
            type="button"
            className={`bordered px-2 py-1 text-[12px] ${activeChip === chip ? "text-accent" : ""}`}
            onClick={() => onChip?.(chip)}
          >
            {chip}
          </button>
        ))}
        {actions}
        <div className="inline-actions">
          <ButtonLike active={view === "list"} onClick={() => setView("list")}>
            List
          </ButtonLike>
          <ButtonLike active={view === "grid"} onClick={() => setView("grid")}>
            Grid
          </ButtonLike>
        </div>
        {primary ?? null}
      </div>
      {columns && view === "list" && hasRows && (
        <div className="grid grid-cols-[minmax(0,2fr)_minmax(0,1fr)_minmax(0,1fr)_auto] gap-3 rounded-t-xl2 border border-line border-b-0 bg-panel px-4 py-2 text-[11px] uppercase tracking-wide text-muted">
          {columns.map((column) => (
            <span key={column}>{column}</span>
          ))}
        </div>
      )}
      <div
        className={
          view === "grid"
            ? "grid grid-cols-1 md:grid-cols-2 gap-3"
            : "rounded-xl2 border border-line bg-panel divide-y divide-line"
        }
      >
        {hasRows ? (
          renderCard && view === "grid" ? (
            renderCard()
          ) : (
            rows
          )
        ) : (
          <div className="px-4 py-6 text-[13px] text-muted">{empty}</div>
        )}
      </div>
      {form && <div className="mt-4">{form}</div>}
    </>
  );
}

function ButtonLike({
  active,
  onClick,
  children,
}: {
  active: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      type="button"
      className={`bordered px-2 py-1 text-[12px] ${active ? "text-accent" : ""}`}
      onClick={onClick}
    >
      {children}
    </button>
  );
}
