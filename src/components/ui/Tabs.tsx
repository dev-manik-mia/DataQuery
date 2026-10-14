import type { ReactNode } from 'react';

export interface TabItem<T extends string | number> {
  id: T;
  label: ReactNode;
  count?: ReactNode;
  critical?: boolean;
}

/** Underline tab strip (result tabs, Messages). */
export function Tabs<T extends string | number>({
  label,
  items,
  value,
  onChange,
  trailing,
}: {
  label: string;
  items: TabItem<T>[];
  value: T;
  onChange: (id: T) => void;
  trailing?: ReactNode;
}) {
  return (
    <div className="dq-tabs" role="tablist" aria-label={label}>
      {items.map((t) => (
        <button
          key={t.id}
          type="button"
          role="tab"
          aria-selected={t.id === value}
          className={`dq-tabs__tab${t.critical ? ' dq-tabs__tab--critical' : ''}`}
          onClick={() => onChange(t.id)}
        >
          {t.label}
          {t.count !== undefined && <span className="dq-tabs__count">{t.count}</span>}
        </button>
      ))}
      {trailing && (
        <>
          <div style={{ flex: 1 }} />
          {trailing}
        </>
      )}
    </div>
  );
}
