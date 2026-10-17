import { memo, useEffect, useMemo, useRef, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { SchemaModel } from '@/bindings/SchemaModel';
import { Button, EmptyState, ErrorAlert, Spinner } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { useWorkspace, type ContainerTab } from '@/stores/workspace';

import { BOX_WIDTH, HEADER_H, ROW_H, layout, rowY, tableKey } from './layout';
import styles from './Schema.module.css';

/** ER diagram of a schema (or MySQL database): tables, columns and foreign keys. */
export function DiagramView({ tab, active }: { tab: ContainerTab; active: boolean }) {
  const session = useWorkspace((s) => s.session)!;
  const openTableTab = useWorkspace((s) => s.openTableTab);
  const [model, setModel] = useState<SchemaModel | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [view, setView] = useState({ x: 24, y: 24, scale: 1 });
  const [hover, setHover] = useState<string | null>(null);
  const drag = useRef<{ x: number; y: number } | null>(null);
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let stale = false;
    setModel(null);
    setError(null);
    api
      .schemaModel({ sessionId: session.session_id }, tab.database, tab.schema)
      .then((m) => !stale && setModel(m))
      .catch((e) => !stale && setError(toAppError(e).info));
    return () => {
      stale = true;
    };
  }, [tab.database, tab.schema, session.session_id, version]);

  const boxes = useMemo(() => (model ? layout(model) : new Map()), [model]);

  if (!active) return <div style={{ display: 'none' }} />;
  if (error) return <div style={{ padding: 24 }}><ErrorAlert error={error} title="Could not read the schema" /></div>;
  if (!model) return <EmptyState title={<Spinner label="Loading" />}>Reading tables and foreign keys…</EmptyState>;
  if (model.tables.length === 0) return <EmptyState title="No tables">This {tab.schema ? 'schema' : 'database'} has no tables or views.</EmptyState>;

  const zoom = (factor: number) => setView((v) => ({ ...v, scale: Math.min(2.5, Math.max(0.2, v.scale * factor)) }));

  return (
    <div className={styles.diagram}>
      <div className={styles.toolbar} role="toolbar" aria-label="Diagram">
        <span className="ink-2">
          {model.tables.length} tables · {model.foreign_keys.length} foreign keys{model.truncated ? ' · showing the first tables only' : ''}
        </span>
        <span style={{ flex: 1 }} />
        <Button size="sm" onClick={() => zoom(1 / 1.2)} aria-label="Zoom out">
          −
        </Button>
        <span className="ink-2 tabular" style={{ width: 44, textAlign: 'center' }}>
          {Math.round(view.scale * 100)}%
        </span>
        <Button size="sm" onClick={() => zoom(1.2)} aria-label="Zoom in">
          +
        </Button>
        <Button size="sm" onClick={() => setView({ x: 24, y: 24, scale: 1 })}>
          Reset
        </Button>
        <Button size="sm" onClick={() => setVersion((v) => v + 1)}>
          Refresh
        </Button>
      </div>
      <svg
        className={styles.canvas}
        role="img"
        aria-label={`ER diagram of ${tab.schema ?? tab.database}`}
        onWheel={(e) => {
          if (e.ctrlKey || e.metaKey) zoom(e.deltaY < 0 ? 1.1 : 1 / 1.1);
          else setView((v) => ({ ...v, x: v.x - e.deltaX, y: v.y - e.deltaY }));
        }}
        onMouseDown={(e) => {
          drag.current = { x: e.clientX, y: e.clientY };
        }}
        onMouseMove={(e) => {
          if (!drag.current) return;
          const dx = e.clientX - drag.current.x;
          const dy = e.clientY - drag.current.y;
          drag.current = { x: e.clientX, y: e.clientY };
          setView((v) => ({ ...v, x: v.x + dx, y: v.y + dy }));
        }}
        onMouseUp={() => (drag.current = null)}
        onMouseLeave={() => (drag.current = null)}
      >
        <g transform={`translate(${view.x} ${view.y}) scale(${view.scale})`}>
          <DiagramContent model={model} boxes={boxes} hover={hover} onHover={setHover} onOpen={openTableTab} />
        </g>
      </svg>
      <p className={styles.hint}>Drag or scroll to pan, ⌘/Ctrl + scroll to zoom, double-click a table to open its structure.</p>
    </div>
  );
}

type Boxes = ReturnType<typeof layout>;

/**
 * Edges and table boxes. Memoized, so panning and zooming only move the
 * outer transform; per-table lookups are built once per model.
 */
const DiagramContent = memo(function DiagramContent({
  model,
  boxes,
  hover,
  onHover,
  onOpen,
}: {
  model: SchemaModel;
  boxes: Boxes;
  hover: string | null;
  onHover: (key: string | null) => void;
  onOpen: (object: ObjectRef, view: 'structure') => void;
}) {
  const { fkColumnsByTable, columnsByTable } = useMemo(() => {
    const fkColumnsByTable = new Map<string, Set<string>>();
    for (const f of model.foreign_keys) {
      const key = tableKey(f.source);
      const set = fkColumnsByTable.get(key) ?? new Set<string>();
      for (const c of f.columns) set.add(c);
      fkColumnsByTable.set(key, set);
    }
    const columnsByTable = new Map(
      model.tables.map((t) => [tableKey(t.object), new Map(t.columns.map((c) => [c.name, c]))]),
    );
    return { fkColumnsByTable, columnsByTable };
  }, [model]);
  return (
    <>
      {model.foreign_keys.map((fk, i) => {
        const from = boxes.get(tableKey(fk.source));
        const to = boxes.get(tableKey(fk.target));
        if (!from || !to) return null;
        const leftToRight = from.x <= to.x;
        const x1 = leftToRight ? from.x + BOX_WIDTH : from.x;
        const x2 = from === to ? from.x + BOX_WIDTH : leftToRight ? to.x : to.x + BOX_WIDTH;
        const y1 = rowY(from, fk.columns[0]);
        const y2 = rowY(to, fk.target_columns[0]);
        const bend = Math.max(40, Math.abs(x2 - x1) / 2) * (leftToRight ? 1 : -1);
        const lit = hover === from.key || hover === to.key;
        return (
          <path
            key={i}
            d={
              from === to
                ? `M${x1} ${y1} C${x1 + 50} ${y1} ${x2 + 50} ${y2} ${x2} ${y2}`
                : `M${x1} ${y1} C${x1 + bend} ${y1} ${x2 - bend} ${y2} ${x2} ${y2}`
            }
            className={styles.edge}
            data-lit={lit}
          >
            <title>{`${fk.name}: ${fk.source.name}(${fk.columns.join(', ')}) → ${fk.target.name}(${fk.target_columns.join(', ')})`}</title>
          </path>
        );
      })}
      {model.tables.map((t) => {
        const key = tableKey(t.object);
        const box = boxes.get(key);
        if (!box) return null;
        const fkColumns = fkColumnsByTable.get(key);
        const columns = columnsByTable.get(key)!;
        return (
          <g
            key={key}
            transform={`translate(${box.x} ${box.y})`}
            className={styles.box}
            onMouseEnter={() => onHover(key)}
            onMouseLeave={() => onHover(null)}
            onDoubleClick={() => onOpen(t.object, 'structure')}
            role="button"
            tabIndex={0}
            aria-label={`Table ${t.object.name}, ${t.columns.length} columns. Press Enter to open.`}
            onKeyDown={(e) => e.key === 'Enter' && onOpen(t.object, 'structure')}
          >
            <rect width={BOX_WIDTH} height={box.height} rx={6} className={styles.boxBody} data-lit={hover === key} />
            <rect width={BOX_WIDTH} height={HEADER_H} rx={6} className={styles.boxHead} />
            <text x={10} y={18} className={styles.boxTitle}>
              {t.object.kind === 'table' ? '' : 'view · '}
              {t.object.name}
            </text>
            {box.rows.map((name: string, i: number) => {
              const c = columns.get(name)!;
              return (
                <g key={name} transform={`translate(0 ${HEADER_H + i * ROW_H + 4})`}>
                  <text x={10} y={13} className={styles.colName}>
                    {c.primary_key ? 'PK ' : fkColumns?.has(c.name) ? 'FK ' : ''}
                    {c.name}
                  </text>
                  <text x={BOX_WIDTH - 10} y={13} textAnchor="end" className={styles.colType}>
                    {c.data_type.length > 16 ? `${c.data_type.slice(0, 15)}…` : c.data_type}
                  </text>
                </g>
              );
            })}
            {box.hidden > 0 && (
              <text x={10} y={HEADER_H + box.rows.length * ROW_H + 17} className={styles.colType}>
                … {box.hidden} more
              </text>
            )}
          </g>
        );
      })}
    </>
  );
});
