import { useVirtualizer } from '@tanstack/react-virtual';
import { memo, useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from 'react';

import type { Cell } from '@/bindings/Cell';
import { Icon, Menu, type MenuEntry } from '@/components/ui';
import { cellText, isTruncated } from '@/lib/cells';
import { copyText, serializeRows, type CopyFormat } from '@/lib/clipboard';
import { isMac, shortcutLabel } from '@/lib/platform';
import { useUi } from '@/stores/ui';

import { CellInspector } from './CellInspector';
import { CellView } from './CellView';
import styles from './Grid.module.css';

export interface GridColumn {
  name: string;
  typeName: string;
  numeric: boolean;
  editable?: boolean;
}

export interface GridRow {
  key: string;
  cells: Cell[];
  status?: 'new' | 'deleted';
  /** Column indexes with pending edits. */
  edited?: ReadonlySet<number>;
  /** New-row columns that will take their DEFAULT. */
  defaults?: ReadonlySet<number>;
}

export interface CellPosition {
  row: number;
  col: number;
}

interface DataGridProps {
  label: string;
  columns: GridColumn[];
  rows: GridRow[];
  /** Added to the row number, for paged views. */
  rowOffset?: number;
  editable?: boolean;
  onEditCell?: (row: number, col: number, value: string | null) => void;
  selectedRows?: ReadonlySet<number>;
  onSelectRows?: (rows: Set<number>) => void;
  sort?: { column: string; descending: boolean } | null;
  onSort?: (column: string) => void;
  extraMenu?: (position: CellPosition) => MenuEntry[];
  onDeleteRows?: () => void;
  emptyText?: string;
}

const ROW_HEIGHT = 28;
const NUMBER_WIDTH = 52;
const MIN_WIDTH = 56;
const MAX_WIDTH = 1200;

function initialWidth(c: GridColumn): number {
  const type = c.typeName.toLowerCase();
  const byType = c.numeric ? (/int8|bigint|numeric|decimal/.test(type) ? 170 : 110) : /timestamp|datetime/.test(type) ? 230 : /json/.test(type) ? 240 : /bool/.test(type) ? 80 : /date|time|uuid/.test(type) ? 150 : 180;
  return Math.max(byType, Math.min(320, c.name.length * 8 + 28));
}

function canEdit(column: GridColumn | undefined, row: GridRow | undefined): boolean {
  return Boolean(column?.editable && row && row.status !== 'deleted');
}

/** Virtualised, keyboard-navigable grid for query results and table data. */
export function DataGrid({
  label,
  columns,
  rows,
  rowOffset = 0,
  editable = false,
  onEditCell,
  selectedRows: controlledSelection,
  onSelectRows,
  sort,
  onSort,
  extraMenu,
  onDeleteRows,
  emptyText = 'No rows.',
}: DataGridProps) {
  const toast = useUi((s) => s.toast);
  const scroller = useRef<HTMLDivElement>(null);
  const signature = columns.map((c) => c.name).join('\u0000');
  const [widths, setWidths] = useState<number[]>(() => columns.map(initialWidth));
  const [order, setOrder] = useState<number[]>(() => columns.map((_, i) => i));
  const [active, setActive] = useState<CellPosition | null>(null);
  const [editing, setEditing] = useState<(CellPosition & { value: string }) | null>(null);
  const [ownSelection, setOwnSelection] = useState<Set<number>>(new Set());
  const [anchor, setAnchor] = useState<number | null>(null);
  const [inspect, setInspect] = useState<CellPosition | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; at: CellPosition } | null>(null);
  const [dragOver, setDragOver] = useState<number | null>(null);

  const selection = controlledSelection ?? ownSelection;
  const setSelection = useCallback((next: Set<number>) => (onSelectRows ? onSelectRows(next) : setOwnSelection(next)), [onSelectRows]);

  // Reset layout when the column set changes (new result).
  useEffect(() => {
    setWidths(columns.map(initialWidth));
    setOrder(columns.map((_, i) => i));
    setActive(null);
    setEditing(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature]);

  const template = useMemo(() => `${NUMBER_WIDTH}px ${order.map((i) => `${widths[i] ?? 120}px`).join(' ')}`, [order, widths]);
  const totalWidth = NUMBER_WIDTH + order.reduce((sum, i) => sum + (widths[i] ?? 120), 0);

  const virtualizer = useVirtualizer({ count: rows.length, getScrollElement: () => scroller.current, estimateSize: () => ROW_HEIGHT, overscan: 16 });

  const headers = columns.map((c) => c.name);

  const copy = async (text: string, what: string) => {
    await copyText(text);
    toast(`${what} copied`);
  };

  const copyRows = (indexes: number[], format: CopyFormat) => {
    const picked = indexes.map((i) => order.map((c) => rows[i]?.cells[c] ?? ({ kind: 'null' } as Cell)));
    void copy(serializeRows(order.map((c) => headers[c] ?? ''), picked, format), indexes.length === 1 ? 'Row' : `${indexes.length} rows`);
  };

  const copyColumn = (col: number) => {
    const values = rows.map((r) => cellText(r.cells[col] ?? { kind: 'null' }) ?? '');
    void copy(values.join('\n'), 'Column');
  };

  const startEdit = (at: CellPosition) => {
    const row = rows[at.row];
    const column = columns[at.col];
    const cell = row?.cells[at.col];
    if (!editable || !onEditCell || !canEdit(column, row) || !cell) return false;
    if (row?.defaults?.has(at.col)) {
      setEditing({ ...at, value: '' });
      return true;
    }
    if (cell.kind === 'binary' || cell.kind === 'unsupported' || isTruncated(cell)) {
      toast('This value cannot be edited in the grid.');
      return false;
    }
    setEditing({ ...at, value: cellText(cell) ?? '' });
    return true;
  };

  const commitEdit = (value: string | null) => {
    if (!editing) return;
    const original = rows[editing.row]?.cells[editing.col];
    const isDefault = rows[editing.row]?.defaults?.has(editing.col) ?? false;
    const before = original && !isDefault ? cellText(original) : null;
    // An untouched editor on NULL shows '' — that is not an edit. Use "Set empty string" for that.
    const unchanged = before === null ? value === '' : value === before;
    if (original && !unchanged) onEditCell?.(editing.row, editing.col, value);
    setEditing(null);
    scroller.current?.focus();
  };

  const moveActive = (dRow: number, dCol: number) => {
    if (!rows.length || !columns.length) return;
    const current = active ?? { row: 0, col: order[0] ?? 0 };
    const pos = order.indexOf(current.col);
    const row = Math.max(0, Math.min(rows.length - 1, current.row + dRow));
    const col = order[Math.max(0, Math.min(order.length - 1, pos + dCol))] ?? current.col;
    setActive({ row, col });
    virtualizer.scrollToIndex(row, { align: 'auto' });
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (editing) return;
    const mod = isMac ? e.metaKey : e.ctrlKey;
    const keys: Record<string, () => void> = {
      ArrowDown: () => moveActive(1, 0),
      ArrowUp: () => moveActive(-1, 0),
      ArrowRight: () => moveActive(0, 1),
      ArrowLeft: () => moveActive(0, -1),
      PageDown: () => moveActive(20, 0),
      PageUp: () => moveActive(-20, 0),
      Home: () => moveActive(-rows.length, 0),
      End: () => moveActive(rows.length, 0),
    };
    if (keys[e.key] && !mod) {
      e.preventDefault();
      keys[e.key]?.();
    } else if ((e.key === 'Enter' || e.key === 'F2') && active) {
      e.preventDefault();
      if (!startEdit(active)) setInspect(active);
    } else if (e.key === ' ' && active) {
      e.preventDefault();
      setInspect(active);
    } else if (mod && e.key.toLowerCase() === 'c') {
      e.preventDefault();
      if (selection.size > 0) copyRows([...selection].sort((a, b) => a - b), 'tsv');
      else if (active) void copy(cellText(rows[active.row]?.cells[active.col] ?? { kind: 'null' }) ?? '', 'Value');
    } else if (mod && e.key.toLowerCase() === 'a') {
      e.preventDefault();
      setSelection(new Set(rows.map((_, i) => i)));
    } else if ((e.key === 'Delete' || e.key === 'Backspace') && editable && selection.size > 0) {
      e.preventDefault();
      onDeleteRows?.();
    } else if (e.key === 'Escape') {
      setSelection(new Set());
    }
  };

  const onRowNumberClick = (e: MouseEvent, index: number) => {
    const mod = isMac ? e.metaKey : e.ctrlKey;
    const next = new Set(mod ? selection : []);
    if (e.shiftKey && anchor !== null) {
      for (let i = Math.min(anchor, index); i <= Math.max(anchor, index); i += 1) next.add(i);
    } else if (mod && selection.has(index)) {
      next.delete(index);
    } else {
      next.add(index);
      setAnchor(index);
    }
    setSelection(next);
    setActive({ row: index, col: active?.col ?? order[0] ?? 0 });
  };

  const startResize = (e: React.PointerEvent, col: number) => {
    e.preventDefault();
    e.stopPropagation();
    const startX = e.clientX;
    const startWidth = widths[col] ?? 120;
    const move = (ev: PointerEvent) => {
      const width = Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, startWidth + ev.clientX - startX));
      setWidths((w) => w.map((v, i) => (i === col ? width : v)));
    };
    const up = () => {
      window.removeEventListener('pointermove', move);
      window.removeEventListener('pointerup', up);
    };
    window.addEventListener('pointermove', move);
    window.addEventListener('pointerup', up);
  };

  const autosize = (col: number) => {
    const sample = rows.slice(0, 300).map((r) => (cellText(r.cells[col] ?? { kind: 'null' }) ?? 'NULL').length);
    const longest = Math.max((columns[col]?.name.length ?? 4) + 2, ...sample);
    setWidths((w) => w.map((v, i) => (i === col ? Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, longest * 7.4 + 24)) : v)));
  };

  const dropColumn = (from: number, to: number) => {
    if (from === to) return;
    setOrder((o) => {
      const next = o.filter((c) => c !== from);
      next.splice(next.indexOf(to), 0, from);
      return next;
    });
  };

  const menuItems = (at: CellPosition): MenuEntry[] => {
    const row = rows[at.row];
    const cell = row?.cells[at.col] ?? ({ kind: 'null' } as Cell);
    const many = selection.size > 1 ? [...selection].sort((a, b) => a - b) : null;
    return [
      { label: 'Copy value', hint: shortcutLabel('mod+c'), onSelect: () => void copy(cellText(cell) ?? '', 'Value') },
      { label: 'Copy row', onSelect: () => copyRows([at.row], 'tsv') },
      { label: 'Copy row as JSON', onSelect: () => copyRows([at.row], 'json') },
      { label: 'Copy column', onSelect: () => copyColumn(at.col) },
      ...(many
        ? [
            { type: 'separator' as const },
            { label: `Copy ${many.length} rows as TSV`, onSelect: () => copyRows(many, 'tsv') },
            { label: `Copy ${many.length} rows as CSV`, onSelect: () => copyRows(many, 'csv') },
            { label: `Copy ${many.length} rows as JSON`, onSelect: () => copyRows(many, 'json') },
          ]
        : []),
      { type: 'separator' },
      { label: 'Inspect value…', hint: 'Space', onSelect: () => setInspect(at) },
      ...(extraMenu?.(at) ?? []),
    ];
  };

  if (!columns.length) return <div className={styles.empty}>{emptyText}</div>;

  const items = virtualizer.getVirtualItems();
  const inspected = inspect && rows[inspect.row]?.cells[inspect.col];

  return (
    <>
      <div
        ref={scroller}
        className={styles.scroller}
        tabIndex={0}
        role="grid"
        aria-label={label}
        aria-rowcount={rows.length + 1}
        aria-colcount={columns.length}
        onKeyDown={onKeyDown}
      >
        <div className={styles.header} style={{ gridTemplateColumns: template, width: totalWidth }} role="row">
          <div className={`${styles.headCell} ${styles.cornerCell}`} role="columnheader" aria-label="Row number" />
          {order.map((ci) => {
            const c = columns[ci]!;
            const sorted = sort?.column === c.name;
            return (
              <div
                key={c.name + ci}
                role="columnheader"
                aria-sort={sorted ? (sort?.descending ? 'descending' : 'ascending') : undefined}
                className={styles.headCell}
                data-numeric={c.numeric}
                data-sortable={Boolean(onSort)}
                data-dragover={dragOver === ci}
                draggable
                title={onSort ? `Sort by ${c.name}` : c.name}
                onClick={() => onSort?.(c.name)}
                onDragStart={(e) => e.dataTransfer.setData('text/x-rb-column', String(ci))}
                onDragOver={(e) => {
                  e.preventDefault();
                  setDragOver(ci);
                }}
                onDragLeave={() => setDragOver(null)}
                onDrop={(e) => {
                  e.preventDefault();
                  setDragOver(null);
                  dropColumn(Number(e.dataTransfer.getData('text/x-rb-column')), ci);
                }}
              >
                <span className={styles.headName}>
                  <span className="ellipsis">{c.name}</span>
                  {sorted && <Icon name={sort?.descending ? 'arrowDown' : 'arrowUp'} size={11} />}
                </span>
                <span className={styles.headType}>{c.typeName}</span>
                <span
                  className={styles.resizer}
                  role="separator"
                  aria-label={`Resize ${c.name}`}
                  onPointerDown={(e) => startResize(e, ci)}
                  onDoubleClick={(e) => {
                    e.stopPropagation();
                    autosize(ci);
                  }}
                  onClick={(e) => e.stopPropagation()}
                />
              </div>
            );
          })}
        </div>
        {rows.length === 0 ? (
          <div className={styles.empty}>{emptyText}</div>
        ) : (
          <div className={styles.body} style={{ height: virtualizer.getTotalSize(), width: totalWidth }}>
            {items.map((item) => (
              <GridRowView
                key={rows[item.index]!.key}
                index={item.index}
                top={item.start}
                row={rows[item.index]!}
                number={rowOffset + item.index + 1}
                order={order}
                columns={columns}
                template={template}
                width={totalWidth}
                selected={selection.has(item.index)}
                activeCol={active?.row === item.index ? active.col : null}
                editing={editing?.row === item.index ? editing : null}
                onCellMouseDown={(col) => {
                  setActive({ row: item.index, col });
                  if (selection.size) setSelection(new Set());
                }}
                onCellDoubleClick={(col) => {
                  const at = { row: item.index, col };
                  if (!startEdit(at)) setInspect(at);
                }}
                onCellContextMenu={(e, col) => {
                  e.preventDefault();
                  const at = { row: item.index, col };
                  setActive(at);
                  setMenu({ x: e.clientX, y: e.clientY, at });
                }}
                onNumberClick={(e) => onRowNumberClick(e, item.index)}
                onEditChange={(value) => setEditing((ed) => (ed ? { ...ed, value } : ed))}
                onEditCommit={commitEdit}
                onEditCancel={() => {
                  setEditing(null);
                  scroller.current?.focus();
                }}
                onEditTab={(value) => {
                  commitEdit(value);
                  moveActive(0, 1);
                }}
              />
            ))}
          </div>
        )}
      </div>
      {menu && <Menu x={menu.x} y={menu.y} items={menuItems(menu.at)} onClose={() => setMenu(null)} label="Cell actions" />}
      {inspect && inspected && (
        <CellInspector column={columns[inspect.col]?.name ?? ''} typeName={columns[inspect.col]?.typeName ?? ''} cell={inspected} onClose={() => setInspect(null)} />
      )}
    </>
  );
}

interface GridRowViewProps {
  index: number;
  top: number;
  row: GridRow;
  number: number;
  order: number[];
  columns: GridColumn[];
  template: string;
  width: number;
  selected: boolean;
  activeCol: number | null;
  editing: (CellPosition & { value: string }) | null;
  onCellMouseDown: (col: number) => void;
  onCellDoubleClick: (col: number) => void;
  onCellContextMenu: (e: MouseEvent, col: number) => void;
  onNumberClick: (e: MouseEvent) => void;
  onEditChange: (value: string) => void;
  onEditCommit: (value: string | null) => void;
  onEditCancel: () => void;
  onEditTab: (value: string) => void;
}

const marker = { new: '+', deleted: '−' } as const;

const GridRowView = memo(function GridRowView(p: GridRowViewProps) {
  return (
    <div
      className={styles.row}
      role="row"
      aria-rowindex={p.index + 2}
      aria-selected={p.selected}
      data-selected={p.selected}
      data-status={p.row.status}
      style={{ top: p.top, gridTemplateColumns: p.template, width: p.width }}
    >
      <div className={`${styles.cell} ${styles.rowNumber}`} role="rowheader" onClick={p.onNumberClick}>
        {p.row.status ? marker[p.row.status] : p.row.edited?.size ? '●' : p.number}
      </div>
      {p.order.map((ci) => {
        const column = p.columns[ci]!;
        const cell = p.row.cells[ci] ?? ({ kind: 'null' } as Cell);
        const isEditing = p.editing?.col === ci;
        return (
          <div
            key={ci}
            role="gridcell"
            className={styles.cell}
            data-numeric={column.numeric}
            data-active={p.activeCol === ci}
            data-edited={p.row.edited?.has(ci) ?? false}
            onMouseDown={() => p.onCellMouseDown(ci)}
            onDoubleClick={() => p.onCellDoubleClick(ci)}
            onContextMenu={(e) => p.onCellContextMenu(e, ci)}
          >
            {isEditing && p.editing ? (
              <input
                className={styles.editor}
                aria-label={`Edit ${column.name}`}
                autoFocus
                value={p.editing.value}
                onChange={(e) => p.onEditChange(e.target.value)}
                onBlur={(e) => p.onEditCommit(e.target.value)}
                onKeyDown={(e) => {
                  e.stopPropagation();
                  if (e.key === 'Enter') p.onEditCommit(e.currentTarget.value);
                  else if (e.key === 'Escape') p.onEditCancel();
                  else if (e.key === 'Tab') {
                    e.preventDefault();
                    p.onEditTab(e.currentTarget.value);
                  }
                }}
              />
            ) : p.row.defaults?.has(ci) ? (
              <span className={styles.defaultToken}>DEFAULT</span>
            ) : (
              <CellView cell={cell} />
            )}
          </div>
        );
      })}
    </div>
  );
});
