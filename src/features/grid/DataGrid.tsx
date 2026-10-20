import { useVirtualizer } from '@tanstack/react-virtual';
import { memo, useCallback, useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from 'react';

import type { Cell } from '@/bindings/Cell';
import type { ExportFormat } from '@/bindings/ExportFormat';
import { Icon, Menu, type MenuEntry } from '@/components/ui';
import { cellText, isSensitiveColumn, isTruncated, MASK, parseClipboardGrid } from '@/lib/cells';
import { copyText, serializeRows, type CopyFormat } from '@/lib/clipboard';
import { isMac, shortcutLabel } from '@/lib/platform';
import { exportData } from '@/features/transfer/exportData';
import { useSettings } from '@/stores/settings';
import { useUi } from '@/stores/ui';

import { CellInspector } from './CellInspector';
import { CellView } from './CellView';
import { RowDetail } from './RowDetail';
import { columnOffsets, columnRange, columnSlots, revealScrollLeft } from './columnWindow';
import styles from './Grid.module.css';

export interface GridColumn {
  name: string;
  typeName: string;
  numeric: boolean;
  editable?: boolean;
  /** Key marker shown in the header, e.g. `PK` or `CK ↓` (CQL). */
  badge?: string;
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

/** A link from a foreign-key cell to the referenced row. */
export interface ForeignKeyLink {
  title: string;
  open: (row: number) => void;
}

interface DataGridProps {
  label: string;
  columns: GridColumn[];
  rows: GridRow[];
  /** Server-side per-column search (table view). Without it, column search filters the loaded rows. */
  onColumnSearch?: (searches: Record<string, string>) => void;
  /** Column index → link to the referenced row. */
  foreignKeys?: ReadonlyMap<number, ForeignKeyLink>;
  /** Full value of a cell for the viewer (the grid holds a bounded preview). */
  onLoadCell?: (row: number, col: number) => Promise<Cell>;
  /** Saves a cell's complete value to a file. */
  onSaveCell?: (row: number, col: number) => void;
  /** Pasted spreadsheet cells, starting at a position. */
  onPaste?: (at: CellPosition, values: string[][]) => void;
  onUndo?: () => void;
  onRedo?: () => void;
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
/** Wider results render only the columns near the viewport; narrower ones render every column. */
const WINDOW_MIN_COLUMNS = 40;
const COLUMN_OVERSCAN = 3;

function initialWidth(c: GridColumn): number {
  const type = c.typeName.toLowerCase();
  const byType = c.numeric ? (/int8|bigint|numeric|decimal/.test(type) ? 170 : 110) : /timestamp|datetime/.test(type) ? 230 : /json/.test(type) ? 240 : /bool/.test(type) ? 80 : /date|time|uuid/.test(type) ? 150 : 180;
  return Math.max(byType, Math.min(320, c.name.length * 8 + 28));
}

/** The row panel stays open for the next grid once the user opened it. */
let detailPreference = false;

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
  onColumnSearch,
  foreignKeys,
  onLoadCell,
  onSaveCell,
  onPaste,
  onUndo,
  onRedo,
}: DataGridProps) {
  const toast = useUi((s) => s.toast);
  const maskEnabled = useSettings((s) => s.settings.mask_columns);
  const maskPatterns = useSettings((s) => s.settings.mask_patterns);
  const scroller = useRef<HTMLDivElement>(null);
  const signature = columns.map((c) => c.name).join('\u0000');
  const [widths, setWidths] = useState<number[]>(() => columns.map(initialWidth));
  const [order, setOrder] = useState<number[]>(() => columns.map((_, i) => i));
  const [active, setActive] = useState<CellPosition | null>(null);
  const [editing, setEditing] = useState<(CellPosition & { value: string }) | null>(null);
  const [ownSelection, setOwnSelection] = useState<Set<number>>(new Set());
  const [anchor, setAnchor] = useState<number | null>(null);
  const [inspect, setInspect] = useState<CellPosition | null>(null);
  const [detail, setDetailState] = useState(detailPreference);
  const setDetail = (open: boolean) => {
    detailPreference = open;
    setDetailState(open);
  };
  const [menu, setMenu] = useState<{ x: number; y: number; at: CellPosition } | null>(null);
  const [dragOver, setDragOver] = useState<number | null>(null);
  const [hidden, setHidden] = useState<Set<number>>(new Set());
  const [pinned, setPinned] = useState<number[]>([]);
  const [revealed, setRevealed] = useState<Set<number>>(new Set());
  const [searchOpen, setSearchOpen] = useState(false);
  const [searches, setSearches] = useState<Record<number, string>>({});
  const [headMenu, setHeadMenu] = useState<{ x: number; y: number; col: number } | null>(null);

  const selection = controlledSelection ?? ownSelection;
  const setSelection = useCallback((next: Set<number>) => (onSelectRows ? onSelectRows(next) : setOwnSelection(next)), [onSelectRows]);

  // Reset layout when the column set changes (new result).
  useEffect(() => {
    setWidths(columns.map(initialWidth));
    setOrder(columns.map((_, i) => i));
    setActive(null);
    setEditing(null);
    setHidden(new Set());
    setPinned([]);
    setRevealed(new Set());
    setSearches({});
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [signature]);

  /** Sensitive columns show a mask until revealed (Settings → Security). */
  const masked = useMemo(
    () => new Set(maskEnabled ? columns.flatMap((c, i) => (isSensitiveColumn(c.name, maskPatterns) && !revealed.has(i) ? [i] : [])) : []),
    [columns, maskEnabled, maskPatterns, revealed],
  );

  /** On-screen order: pinned columns first, hidden ones left out. */
  const visible = useMemo(() => [...pinned.filter((c) => !hidden.has(c)), ...order.filter((c) => !pinned.includes(c) && !hidden.has(c))], [order, pinned, hidden]);

  /** Sticky offsets of pinned columns. */
  const pinLeft = useMemo(() => {
    const left = new Map<number, number>();
    let x = NUMBER_WIDTH;
    for (const c of visible) {
      if (!pinned.includes(c)) break;
      left.set(c, x);
      x += widths[c] ?? 120;
    }
    return left;
  }, [visible, pinned, widths]);

  // Client-side column search for results the grid already holds; deferred so typing stays responsive.
  const deferredSearches = useDeferredValue(searches);
  const shownRows = useMemo(() => {
    const active = Object.entries(deferredSearches)
      .map(([c, v]) => [Number(c), v.trim().toLowerCase()] as const)
      .filter(([, v]) => v);
    if (onColumnSearch || !active.length) return rows.map((_, i) => i);
    return rows.flatMap((r, i) =>
      active.every(([c, needle]) => (cellText(r.cells[c] ?? { kind: 'null' }) ?? 'null').toLowerCase().includes(needle)) ? [i] : [],
    );
  }, [rows, deferredSearches, onColumnSearch]);

  useEffect(() => {
    if (!onColumnSearch) return;
    const timer = window.setTimeout(() => onColumnSearch(Object.fromEntries(Object.entries(searches).map(([c, v]) => [columns[Number(c)]?.name ?? '', v]))), 350);
    return () => window.clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [searches]);

  const template = useMemo(() => `${NUMBER_WIDTH}px ${visible.map((i) => `${widths[i] ?? 120}px`).join(' ')}`, [visible, widths]);
  /** offsets[k] is the left edge of on-screen column k; the last entry is the total width. */
  const offsets = useMemo(() => columnOffsets(visible.map((i) => widths[i] ?? 120), NUMBER_WIDTH), [visible, widths]);
  const totalWidth = offsets[offsets.length - 1] ?? NUMBER_WIDTH;

  // Horizontal windowing. Every cell sits in its own grid track (grid-column), so the
  // template keeps all tracks and skipped columns leave empty space, not shifted cells.
  // The range is state that changes only when a column enters or leaves it, not per scroll pixel.
  const windowed = visible.length > WINDOW_MIN_COLUMNS;
  const [range, setRange] = useState<readonly [number, number]>([0, -1]);
  const offsetsRef = useRef(offsets);
  offsetsRef.current = offsets;
  const syncRange = useCallback(() => {
    const el = scroller.current;
    if (!el) return;
    const [first, last] = columnRange(offsetsRef.current, el.scrollLeft, el.scrollLeft + el.clientWidth, COLUMN_OVERSCAN);
    setRange((r) => (r[0] === first && r[1] === last ? r : [first, last]));
  }, []);
  const hasColumns = columns.length > 0;
  useLayoutEffect(() => {
    if (windowed) syncRange();
  }, [windowed, offsets, syncRange]);
  useEffect(() => {
    const el = scroller.current;
    if (!windowed || !el) return;
    const observer = new ResizeObserver(syncRange);
    observer.observe(el);
    return () => observer.disconnect();
  }, [windowed, hasColumns, syncRange]);
  const pinnedCount = pinLeft.size;
  /** The active and editing cells stay rendered (focus, editor) even when scrolled away. */
  const outside = (col: number | undefined) => {
    if (!windowed || col === undefined) return -1;
    const k = visible.indexOf(col);
    return k < pinnedCount || (k >= range[0] && k <= range[1]) ? -1 : k;
  };
  const activeExtra = outside(active?.col);
  const editingExtra = outside(editing?.col);
  /** Indexes into `visible` to render; a new array only when the rendered set changes. */
  const slots = useMemo(
    () => (windowed ? columnSlots(visible.length, pinnedCount, range[0], range[1], [activeExtra, editingExtra]) : visible.map((_, k) => k)),
    // eslint-disable-next-line react-hooks/exhaustive-deps -- `visible` matters only through its length here
    [windowed, visible.length, pinnedCount, range, activeExtra, editingExtra],
  );

  const virtualizer = useVirtualizer({ count: shownRows.length, getScrollElement: () => scroller.current, estimateSize: () => ROW_HEIGHT, overscan: 16 });

  const headers = columns.map((c) => c.name);

  const copy = async (text: string, what: string) => {
    await copyText(text);
    toast(`${what} copied`);
  };

  const maskCell = (cell: Cell, col: number): Cell => (masked.has(col) && cell.kind !== 'null' ? { kind: 'text', value: MASK } : cell);

  /** Cells of the given rows, in on-screen column order; masked values stay masked. */
  const pickRows = (indexes: number[], mask = true) =>
    indexes.map((i) => visible.map((c) => (mask ? maskCell : (x: Cell) => x)(rows[i]?.cells[c] ?? ({ kind: 'null' } as Cell), c)));

  const copyRows = (indexes: number[], format: CopyFormat) => {
    void copy(serializeRows(visible.map((c) => headers[c] ?? ''), pickRows(indexes), format), indexes.length === 1 ? 'Row' : `${indexes.length} rows`);
  };

  const exportRows = (indexes: number[], format: ExportFormat) => {
    const meta = visible.map((c) => ({ name: columns[c]?.name ?? '', type_name: columns[c]?.typeName ?? '', numeric: columns[c]?.numeric ?? false }));
    exportData({ type: 'rows', columns: meta, rows: pickRows(indexes, false) }, format, `${label} selection`);
  };

  const valueText = (row: number, col: number) => cellText(maskCell(rows[row]?.cells[col] ?? { kind: 'null' }, col));

  const copyColumn = (col: number) => {
    const values = shownRows.map((i) => valueText(i, col) ?? '');
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
    if (!shownRows.length || !visible.length) return;
    const current = active ?? { row: shownRows[0] ?? 0, col: visible[0] ?? 0 };
    const pos = visible.indexOf(current.col);
    const display = Math.max(0, Math.min(shownRows.length - 1, shownRows.indexOf(current.row) + dRow));
    const row = shownRows[display] ?? current.row;
    const col = visible[Math.max(0, Math.min(visible.length - 1, pos + dCol))] ?? current.col;
    setActive({ row, col });
    virtualizer.scrollToIndex(display, { align: 'auto' });
    // Bring the active column into view (pinned columns are always on screen).
    const k = visible.indexOf(col);
    const el = scroller.current;
    if (el && k >= pinLeft.size) {
      const left = revealScrollLeft(offsets[k] ?? 0, offsets[k + 1] ?? 0, el.scrollLeft, el.clientWidth, offsets[pinLeft.size] ?? NUMBER_WIDTH);
      if (left !== el.scrollLeft) el.scrollLeft = left;
    }
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
    } else if (e.key === ' ' && e.shiftKey && active) {
      e.preventDefault();
      setInspect(active);
    } else if (e.key === ' ') {
      e.preventDefault();
      if (!active && shownRows.length) setActive({ row: shownRows[0]!, col: visible[0] ?? 0 });
      setDetail(!detail);
    } else if (mod && e.key.toLowerCase() === 'c') {
      e.preventDefault();
      if (selection.size > 0) copyRows([...selection].sort((a, b) => a - b), 'tsv');
      else if (active) void copy(valueText(active.row, active.col) ?? '', 'Value');
    } else if (mod && e.key.toLowerCase() === 'z' && onUndo) {
      e.preventDefault();
      if (e.shiftKey) onRedo?.();
      else onUndo();
    } else if (mod && e.key.toLowerCase() === 'y' && onRedo) {
      e.preventDefault();
      onRedo();
    } else if (mod && e.key.toLowerCase() === 'a') {
      e.preventDefault();
      setSelection(new Set(shownRows));
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
            { label: `Export ${many.length} rows…`, onSelect: () => exportRows(many, 'csv') },
          ]
        : []),
      { type: 'separator' },
      { label: detail ? 'Hide row details' : 'Row details', hint: 'Space', onSelect: () => setDetail(!detail) },
      { label: 'Inspect value…', hint: shortcutLabel('shift+space'), onSelect: () => setInspect(at) },
      ...(foreignKeys?.has(at.col) && cell.kind !== 'null' ? [{ label: foreignKeys.get(at.col)!.title, onSelect: () => foreignKeys.get(at.col)!.open(at.row) }] : []),
      ...(onSaveCell && (cell.kind === 'binary' || isTruncated(cell)) ? [{ label: 'Save value to file…', onSelect: () => onSaveCell(at.row, at.col) }] : []),
      ...(extraMenu?.(at) ?? []),
    ];
  };

  const headActions = (col: number): MenuEntry[] => {
    const name = columns[col]?.name ?? '';
    const sensitive = maskEnabled && isSensitiveColumn(name, maskPatterns);
    return [
      { label: pinned.includes(col) ? 'Unpin column' : 'Pin column', onSelect: () => setPinned((p) => (p.includes(col) ? p.filter((c) => c !== col) : [...p, col])) },
      { label: 'Hide column', disabled: visible.length <= 1, onSelect: () => setHidden((h) => new Set([...h, col])) },
      ...(hidden.size ? [{ label: `Show all columns (${hidden.size} hidden)`, onSelect: () => setHidden(new Set()) }] : []),
      { type: 'separator' },
      { label: searchOpen ? 'Hide column search' : 'Search in columns', onSelect: () => setSearchOpen((o) => !o) },
      { label: 'Fit width to content', onSelect: () => autosize(col) },
      { label: 'Copy column', onSelect: () => copyColumn(col) },
      ...(sensitive ? [{ type: 'separator' as const }, { label: revealed.has(col) ? 'Mask values' : 'Reveal values', onSelect: () => setRevealed((r) => (r.has(col) ? new Set([...r].filter((c) => c !== col)) : new Set([...r, col]))) }] : []),
    ];
  };

  // Rows read the latest state through this ref, so they all share one stable handler object.
  const latest = useRef({ startEdit, commitEdit, moveActive, onRowNumberClick, masked, hasSelection: selection.size > 0, setSelection });
  latest.current = { startEdit, commitEdit, moveActive, onRowNumberClick, masked, hasSelection: selection.size > 0, setSelection };
  const rowHandlers = useMemo<RowHandlers>(
    () => ({
      cellMouseDown: (row, col) => {
        setActive({ row, col });
        if (latest.current.hasSelection) latest.current.setSelection(new Set());
      },
      cellDoubleClick: (row, col) => {
        const at = { row, col };
        if (latest.current.masked.has(col) || !latest.current.startEdit(at)) setInspect(at);
      },
      cellContextMenu: (e, row, col) => {
        e.preventDefault();
        const at = { row, col };
        setActive(at);
        setMenu({ x: e.clientX, y: e.clientY, at });
      },
      numberClick: (e, row) => latest.current.onRowNumberClick(e, row),
      editChange: (value) => setEditing((ed) => (ed ? { ...ed, value } : ed)),
      editCommit: (value) => latest.current.commitEdit(value),
      editCancel: () => {
        setEditing(null);
        scroller.current?.focus();
      },
      editTab: (value) => {
        latest.current.commitEdit(value);
        latest.current.moveActive(0, 1);
      },
    }),
    [],
  );

  if (!columns.length) return <div className={styles.empty}>{emptyText}</div>;

  const items = virtualizer.getVirtualItems();
  const inspected = inspect && rows[inspect.row]?.cells[inspect.col];
  const searching = Object.values(searches).some((v) => v.trim());

  const detailRow = detail && active ? rows[active.row] : undefined;

  return (
    <>
      <div className={styles.frame}>
        <div
          ref={scroller}
          className={styles.scroller}
          tabIndex={0}
          role="grid"
          aria-label={label}
          aria-rowcount={rows.length + 1}
          aria-colcount={visible.length + 1}
          onKeyDown={onKeyDown}
          onScroll={windowed ? syncRange : undefined}
          onPaste={(e) => {
            if (!onPaste || !active || editing) return;
            const text = e.clipboardData.getData('text/plain');
            if (!text) return;
            e.preventDefault();
            onPaste(active, parseClipboardGrid(text));
          }}
        >
          <div className={styles.header} style={{ gridTemplateColumns: template, width: totalWidth }} role="row">
            <div className={`${styles.headCell} ${styles.cornerCell}`} style={{ gridColumn: 1, gridRow: 1 }} role="columnheader" aria-colindex={1} aria-label="Row number" />
            {slots.map((k) => {
              const ci = visible[k]!;
              const c = columns[ci]!;
              const sorted = sort?.column === c.name;
              const left = pinLeft.get(ci);
              return (
                <div
                  key={c.name + ci}
                  role="columnheader"
                  aria-colindex={k + 2}
                  aria-sort={sorted ? (sort?.descending ? 'descending' : 'ascending') : undefined}
                  className={styles.headCell}
                  data-pinned={left !== undefined}
                  style={left !== undefined ? { gridColumn: k + 2, gridRow: 1, position: 'sticky', left, zIndex: 3 } : { gridColumn: k + 2, gridRow: 1 }}
                  onContextMenu={(e) => {
                    e.preventDefault();
                    setHeadMenu({ x: e.clientX, y: e.clientY, col: ci });
                  }}
                  data-numeric={c.numeric}
                  data-sortable={Boolean(onSort)}
                  data-dragover={dragOver === ci}
                  draggable
                  title={onSort ? `Sort by ${c.name}` : c.name}
                  onClick={() => onSort?.(c.name)}
                  onDragStart={(e) => e.dataTransfer.setData('text/x-dq-column', String(ci))}
                  onDragOver={(e) => {
                    e.preventDefault();
                    setDragOver(ci);
                  }}
                  onDragLeave={() => setDragOver(null)}
                  onDrop={(e) => {
                    e.preventDefault();
                    setDragOver(null);
                    dropColumn(Number(e.dataTransfer.getData('text/x-dq-column')), ci);
                  }}
                >
                  <span className={styles.headName}>
                    {pinned.includes(ci) && <Icon name="pin" size={10} />}
                    <span className="ellipsis">{c.name}</span>
                    {c.badge && (
                      <span className={styles.headBadge} data-kind={c.badge.startsWith('PK') ? 'partition' : 'clustering'}>
                        {c.badge}
                      </span>
                    )}
                    {sorted && <Icon name={sort?.descending ? 'arrowDown' : 'arrowUp'} size={11} />}
                    {masked.has(ci) && <Icon name="eyeOff" size={11} title="Masked: right-click to reveal" />}
                    {foreignKeys?.has(ci) && <Icon name="link" size={11} title={foreignKeys.get(ci)!.title} />}
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
            {searchOpen && (
              <>
                <div className={`${styles.searchCell} ${styles.cornerCell}`} style={{ gridColumn: 1, gridRow: 2 }}>
                  <Icon name="search" size={11} />
                </div>
                {slots.map((k) => {
                  const ci = visible[k]!;
                  const left = pinLeft.get(ci);
                  return (
                    <div
                      key={`search-${ci}`}
                      className={styles.searchCell}
                      style={left !== undefined ? { gridColumn: k + 2, gridRow: 2, position: 'sticky', left, zIndex: 3 } : { gridColumn: k + 2, gridRow: 2 }}
                    >
                      <input
                        className={styles.searchInput}
                        aria-label={`Search ${columns[ci]?.name}`}
                        placeholder="contains…"
                        value={searches[ci] ?? ''}
                        onChange={(e) => setSearches((s) => ({ ...s, [ci]: e.target.value }))}
                        onKeyDown={(e) => e.stopPropagation()}
                      />
                    </div>
                  );
                })}
              </>
            )}
          </div>
          {shownRows.length === 0 ? (
            <div className={styles.empty}>{searching && !onColumnSearch ? 'No loaded rows match the column search.' : emptyText}</div>
          ) : (
            <div className={styles.body} style={{ height: virtualizer.getTotalSize(), width: totalWidth }}>
              {items.map((item) => {
                const index = shownRows[item.index]!;
                return (
                <GridRowView
                  key={rows[index]!.key}
                  index={index}
                  top={item.start}
                  row={rows[index]!}
                  number={rowOffset + index + 1}
                  order={visible}
                  slots={slots}
                  pinLeft={pinLeft}
                  masked={masked}
                  foreignKeys={foreignKeys}
                  columns={columns}
                  template={template}
                  width={totalWidth}
                  selected={selection.has(index)}
                  activeCol={active?.row === index ? active.col : null}
                  editing={editing?.row === index ? editing : null}
                  handlers={rowHandlers}
                />
                );
              })}
            </div>
          )}
        </div>
        {detail &&
          (detailRow && active ? (
            <RowDetail
              columns={columns}
              row={detailRow}
              rowIndex={active.row}
              number={rowOffset + active.row + 1}
              order={visible}
              display={(col) => maskCell(detailRow.cells[col] ?? { kind: 'null' }, col)}
              masked={masked}
              foreignKeys={foreignKeys}
              activeCol={active.col}
              canEdit={(col) => editable && Boolean(onEditCell) && canEdit(columns[col], detailRow)}
              onEdit={(col, value) => onEditCell?.(active.row, col, value)}
              onInspect={(col) => setInspect({ row: active.row, col })}
              onFocusColumn={(col) => setActive({ row: active.row, col })}
              onClose={() => {
                setDetail(false);
                scroller.current?.focus();
              }}
            />
          ) : (
            <aside className={styles.detail} aria-label="Row details">
              <div className={styles.empty}>Select a row to see its fields.</div>
            </aside>
          ))}
      </div>
      {menu && <Menu x={menu.x} y={menu.y} items={menuItems(menu.at)} onClose={() => setMenu(null)} label="Cell actions" />}
      {headMenu && <Menu x={headMenu.x} y={headMenu.y} items={headActions(headMenu.col)} onClose={() => setHeadMenu(null)} label="Column actions" />}
      {inspect && inspected && (
        <CellInspector
          column={columns[inspect.col]?.name ?? ''}
          typeName={columns[inspect.col]?.typeName ?? ''}
          cell={maskCell(inspected, inspect.col)}
          load={onLoadCell && !masked.has(inspect.col) ? () => onLoadCell(inspect.row, inspect.col) : undefined}
          onSaveFile={onSaveCell ? () => onSaveCell(inspect.row, inspect.col) : undefined}
          onEdit={
            editable && onEditCell && canEdit(columns[inspect.col], rows[inspect.row]) && !masked.has(inspect.col)
              ? (value) => onEditCell(inspect.row, inspect.col, value)
              : undefined
          }
          onClose={() => setInspect(null)}
        />
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
  /** Indexes into `order` to render; each cell is placed in grid track index + 2. */
  slots: number[];
  pinLeft: ReadonlyMap<number, number>;
  masked: ReadonlySet<number>;
  foreignKeys?: ReadonlyMap<number, ForeignKeyLink>;
  columns: GridColumn[];
  template: string;
  width: number;
  selected: boolean;
  activeCol: number | null;
  editing: (CellPosition & { value: string }) | null;
  handlers: RowHandlers;
}

/** Row callbacks; one stable object for all rows so the row memo holds. */
interface RowHandlers {
  cellMouseDown: (row: number, col: number) => void;
  cellDoubleClick: (row: number, col: number) => void;
  cellContextMenu: (e: MouseEvent, row: number, col: number) => void;
  numberClick: (e: MouseEvent, row: number) => void;
  editChange: (value: string) => void;
  editCommit: (value: string | null) => void;
  editCancel: () => void;
  editTab: (value: string) => void;
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
      <div className={`${styles.cell} ${styles.rowNumber}`} style={{ gridColumn: 1 }} role="rowheader" aria-colindex={1} onClick={(e) => p.handlers.numberClick(e, p.index)} onMouseDown={(e) => e.preventDefault()}>
        {p.row.status ? marker[p.row.status] : p.row.edited?.size ? '●' : p.number}
      </div>
      {p.slots.map((k) => {
        const ci = p.order[k]!;
        const column = p.columns[ci]!;
        const cell = p.row.cells[ci] ?? ({ kind: 'null' } as Cell);
        const isEditing = p.editing?.col === ci;
        const left = p.pinLeft.get(ci);
        const fk = p.foreignKeys?.get(ci);
        return (
          <div
            key={ci}
            role="gridcell"
            aria-colindex={k + 2}
            className={styles.cell}
            data-pinned={left !== undefined}
            style={left !== undefined ? { gridColumn: k + 2, position: 'sticky', left, zIndex: 1 } : { gridColumn: k + 2 }}
            data-numeric={column.numeric}
            data-active={p.activeCol === ci}
            data-edited={p.row.edited?.has(ci) ?? false}
            onMouseDown={() => p.handlers.cellMouseDown(p.index, ci)}
            onDoubleClick={() => p.handlers.cellDoubleClick(p.index, ci)}
            onContextMenu={(e) => p.handlers.cellContextMenu(e, p.index, ci)}
          >
            {isEditing && p.editing ? (
              <input
                className={styles.editor}
                aria-label={`Edit ${column.name}`}
                autoFocus
                value={p.editing.value}
                onChange={(e) => p.handlers.editChange(e.target.value)}
                onBlur={(e) => p.handlers.editCommit(e.target.value)}
                onKeyDown={(e) => {
                  e.stopPropagation();
                  if (e.key === 'Enter') p.handlers.editCommit(e.currentTarget.value);
                  else if (e.key === 'Escape') p.handlers.editCancel();
                  else if (e.key === 'Tab') {
                    e.preventDefault();
                    p.handlers.editTab(e.currentTarget.value);
                  }
                }}
              />
            ) : p.row.defaults?.has(ci) ? (
              <span className={styles.defaultToken}>DEFAULT</span>
            ) : p.masked.has(ci) && cell.kind !== 'null' ? (
              <span className={styles.muted}>{MASK}</span>
            ) : (
              <>
                <CellView cell={cell} />
                {fk && cell.kind !== 'null' && p.row.status !== 'new' && (
                  <button
                    type="button"
                    className={styles.fkLink}
                    aria-label={fk.title}
                    title={fk.title}
                    onMouseDown={(e) => e.stopPropagation()}
                    onClick={(e) => {
                      e.stopPropagation();
                      fk.open(p.index);
                    }}
                  >
                    <Icon name="external" size={10} />
                  </button>
                )}
              </>
            )}
          </div>
        );
      })}
    </div>
  );
});
