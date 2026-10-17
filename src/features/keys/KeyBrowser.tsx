import { useVirtualizer } from '@tanstack/react-virtual';
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { create } from 'zustand';
import { useShallow } from 'zustand/react/shallow';

import type { DbError } from '@/bindings/DbError';
import type { KeyEntry } from '@/bindings/KeyEntry';
import type { KeyType } from '@/bindings/KeyType';
import type { ScanRequest } from '@/bindings/ScanRequest';
import { Button, Icon, IconButton, Input, Menu, SelectField, type MenuEntry } from '@/components/ui';
import explorer from '@/features/explorer/Explorer.module.css';
import { TREE_PADDING, TREE_ROW_HEIGHT } from '@/features/explorer/tree';
import { exportData } from '@/features/transfer/exportData';
import { toAppError } from '@/lib/errors';
import { formatCount } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useWorkspace } from '@/stores/workspace';

import { NewKeyDialog } from './NewKeyDialog';
import { TYPE_BADGE, TYPE_LABEL, formatTtl, groupKeys, keyObject } from './keys';
import styles from './Keys.module.css';

const SCAN_COUNT = 500;

/** Shared between the browser and key tabs: the selected database and a reload signal after writes. */
export const useKeyBrowser = create<{ database: number | null; version: number; setDatabase: (db: number) => void; reload: () => void }>((set) => ({
  database: null,
  version: 0,
  setDatabase: (database) => set({ database }),
  reload: () => set((s) => ({ version: s.version + 1 })),
}));

/** Screen 12: keys of one database, found with SCAN (never KEYS), grouped by prefix. */
export function KeyBrowser({ width }: { width: number }) {
  const session = useWorkspace((s) => s.session)!;
  const { openTableTab, openContainerTab } = useWorkspace(useShallow((s) => ({ openTableTab: s.openTableTab, openContainerTab: s.openContainerTab })));
  const features = session.keys!;
  const sid = useMemo(() => ({ sessionId: session.session_id }), [session.session_id]);
  const { database: chosen, version, setDatabase } = useKeyBrowser();
  const database = chosen ?? (Number(session.server.database) || 0);

  const [draft, setDraft] = useState('');
  const [pattern, setPattern] = useState('');
  const [keyType, setKeyType] = useState<KeyType | ''>('');
  const [keys, setKeys] = useState<KeyEntry[]>([]);
  const [cursor, setCursor] = useState<string | null>(null);
  const [scanned, setScanned] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<DbError | null>(null);
  const [total, setTotal] = useState<number | null>(null);
  const [closed, setClosed] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[] } | null>(null);

  const request = useCallback(
    (from: string | null): ScanRequest => ({ database, pattern, key_type: keyType || null, cursor: from, count: SCAN_COUNT }),
    [database, pattern, keyType],
  );

  /** One SCAN step; `from` null starts over. */
  const scan = useCallback(
    async (from: string | null) => {
      setLoading(true);
      setError(null);
      try {
        const page = await api.keysScan(sid, request(from));
        setKeys((k) => (from === null ? page.keys : [...k, ...page.keys]));
        setCursor(page.cursor);
        setScanned((n) => (from === null ? SCAN_COUNT : n + SCAN_COUNT));
      } catch (e) {
        setError(toAppError(e).info);
      } finally {
        setLoading(false);
      }
    },
    [sid, request],
  );

  useEffect(() => {
    void scan(null);
    api
      .keysCount(sid, database)
      .then(setTotal)
      .catch(() => setTotal(null));
  }, [scan, sid, database, version]);

  const rows = useMemo(() => groupKeys(keys, closed), [keys, closed]);
  const list = useRef<HTMLDivElement>(null);
  // Keys accumulate across "Scan more"; only rows near the viewport are rendered (all rows are one height).
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => list.current,
    estimateSize: () => TREE_ROW_HEIGHT,
    getItemKey: (i) => {
      const row = rows[i];
      return !row ? i : row.type === 'group' ? `g:${row.prefix}` : `k:${row.entry.key.text}`;
    },
    scrollMargin: TREE_PADDING,
    overscan: 12,
  });
  const open = (entry: KeyEntry) => {
    setSelected(entry.key.text);
    openTableTab(keyObject(database, entry.key), 'data');
  };
  const onPatternKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') setPattern(draft.trim());
  };
  const toggleGroup = (prefix: string) =>
    setClosed((c) => {
      const next = new Set(c);
      if (next.has(prefix)) next.delete(prefix);
      else next.add(prefix);
      return next;
    });

  const more = (e: React.MouseEvent) => {
    const r = e.currentTarget.getBoundingClientRect();
    setMenu({
      x: r.left,
      y: r.bottom + 4,
      items: [
        { label: 'Open console', onSelect: () => openContainerTab('console', String(database), null) },
        ...(session.server.read_only ? [] : [{ label: 'New key…', onSelect: () => setCreating(true) }]),
        { type: 'separator' },
        { label: 'Export matching keys…', onSelect: () => exportData({ type: 'keys', request: request(null) }, 'csv', `db${database}-keys`) },
      ],
    });
  };

  return (
    <aside className={explorer.explorer} style={{ width }} aria-label="Key browser">
      <div className={explorer.head}>
        <div className={explorer.headRow}>
          <span className="dq-eyebrow">Keys</span>
          <span className={styles.total}>{total === null ? '' : `${formatCount(total)} in db ${database}`}</span>
          <span style={{ display: 'flex', gap: 2 }}>
            <IconButton label="Rescan" onClick={() => void scan(null)}>
              <Icon name="refresh" size={12} />
            </IconButton>
            <IconButton label="Key actions" onClick={more}>
              <Icon name="more" size={12} />
            </IconButton>
          </span>
        </div>
        <div className={styles.filters}>
          {features.databases > 1 && (
            <SelectField
              label={<span className="sr-only">Database</span>}
              value={String(database)}
              onChange={(e) => setDatabase(Number(e.target.value))}
              options={Array.from({ length: features.databases }, (_, i) => ({ value: String(i), label: `db ${i}` }))}
            />
          )}
          <SelectField
            label={<span className="sr-only">Type</span>}
            value={keyType}
            onChange={(e) => setKeyType(e.target.value as KeyType | '')}
            options={[{ value: '', label: 'All types' }, ...(['string', 'hash', 'list', 'set', 'zset', 'stream'] as KeyType[]).map((t) => ({ value: t, label: TYPE_LABEL[t] }))]}
          />
        </div>
        <Input label="Key pattern" mono placeholder="user:*  (Enter to scan)" value={draft} onChange={(e) => setDraft(e.target.value)} onKeyDown={onPatternKey} />
        <span className={styles.hint}>Uses SCAN with COUNT {SCAN_COUNT}. KEYS is never sent.</span>
      </div>
      <div ref={list} className={explorer.tree} role="tree" aria-label="Keys">
        <div className={explorer.virtualBody} style={{ height: virtualizer.getTotalSize() }}>
          {virtualizer.getVirtualItems().map(({ index, key, start }) => {
            const row = rows[index]!;
            return (
              <div key={key} className={explorer.virtualRow} style={{ transform: `translateY(${start - TREE_PADDING}px)` }}>
                {row.type === 'group' ? (
                  <div role="treeitem" aria-expanded={row.open} aria-level={1} className={explorer.node} data-container onClick={() => toggleGroup(row.prefix)}>
                    <span className={explorer.chevron}>
                      <Icon name={row.open ? 'chevronDown' : 'chevronRight'} size={10} />
                    </span>
                    <span className={styles.badge} />
                    <span className={explorer.label}>{row.prefix}</span>
                    <span className={explorer.count}>{formatCount(row.count)}</span>
                  </div>
                ) : (
                  <div
                    role="treeitem"
                    aria-level={row.depth + 1}
                    aria-selected={selected === row.entry.key.text}
                    aria-label={`${TYPE_LABEL[row.entry.key_type]} ${row.entry.key.text}`}
                    className={explorer.node}
                    style={{ paddingLeft: 8 + row.depth * 16 }}
                    title={row.entry.key.binary ? 'Binary key name (escaped)' : row.entry.type_name}
                    onClick={() => open(row.entry)}
                  >
                    <span className={styles.badge} data-type={row.entry.key_type}>
                      {TYPE_BADGE[row.entry.key_type]}
                    </span>
                    <span className={`${explorer.label} mono`}>{row.depth ? row.entry.key.text.slice(row.entry.key.text.indexOf(':') + 1) : row.entry.key.text}</span>
                    <span className={explorer.count}>{formatTtl(row.entry.ttl_ms)}</span>
                  </div>
                )}
              </div>
            );
          })}
        </div>
        {!loading && !error && keys.length === 0 && <div className={explorer.empty}>{cursor ? 'No keys in this step; scan more.' : 'No keys match.'}</div>}
        {error && <div className={explorer.error}>{error.message}</div>}
      </div>
      <div className={styles.scanBar}>
        <span className="tabular">{cursor ? `Cursor ${cursor} · ${formatCount(scanned)} scanned` : `Scan complete · ${formatCount(keys.length)} keys`}</span>
        <Button size="xs" busy={loading} disabled={!cursor} onClick={() => void scan(cursor)}>
          Scan more
        </Button>
      </div>
      {menu && <Menu x={menu.x} y={menu.y} items={menu.items} onClose={() => setMenu(null)} />}
      {creating && <NewKeyDialog database={database} onClose={() => setCreating(false)} onCreated={(key) => openTableTab(keyObject(database, key), 'data')} />}
    </aside>
  );
}
