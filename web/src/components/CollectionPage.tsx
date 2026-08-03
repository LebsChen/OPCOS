import type { ReactNode } from "react";

export type CollectionView = "list";

export function CollectionPage({
  search,
  onSearch,
  searchPlaceholder,
  actions,
  primary,
  rows,
  empty,
  form,
  bare,
}: {
  search: string;
  onSearch: (value: string) => void;
  searchPlaceholder: string;
  actions?: ReactNode;
  primary?: ReactNode;
  rows: ReactNode;
  empty: string;
  form?: ReactNode;
  bare?: boolean;
  columns?: string[];
  chips?: string[];
  activeChip?: string;
  onChip?: (chip: string) => void;
  renderCard?: () => ReactNode;
  viewKey?: string;
}) {
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
        {actions}
        {primary ?? null}
      </div>
      {bare && hasRows ? (
        rows
      ) : (
        <div className="rounded-xl2 border border-line bg-panel divide-y divide-line">
          {hasRows ? (
            rows
          ) : (
            <div className="px-4 py-6 text-[13px] text-muted">{empty}</div>
          )}
        </div>
      )}
      {form && <div className="mt-4">{form}</div>}
    </>
  );
}
