import { useCallback, useEffect, useMemo, useState } from 'react';

import type { ObjectRef } from '@/bindings/ObjectRef';
import type { RowCount } from '@/bindings/RowCount';
import type { SortSpec } from '@/bindings/SortSpec';
import type { TablePage } from '@/bindings/TablePage';
import type { TableStructure } from '@/bindings/TableStructure';
import { save as saveDialog } from '@tauri-apps/plugin-dialog';

import type { ColumnInfo } from '@/bindings/ColumnInfo';
import type { ColumnSearch } from '@/bindings/ColumnSearch';
import type { ColumnValue } from '@/bindings/ColumnValue';
import { Button, Checkbox, Dialog, ErrorAlert, Icon, IconButton, Menu, TextField, type MenuEntry } from '@/components/ui';
import { SqlViewer } from '@/features/editor/SqlViewer';
import { DataGrid, type ForeignKeyLink, type GridColumn, type GridRow } from '@/features/grid/DataGrid';
import { exportData } from '@/features/transfer/exportData';
import { cellFromInput, cellText } from '@/lib/cells';
import { copyText } from '@/lib/clipboard';
import { toAppError } from '@/lib/errors';
import { formatBytes, formatCount, formatDuration, plural } from '@/lib/format';
import { api } from '@/lib/ipc';
import { objectLabel } from '@/lib/objects';
import { useSettings } from '@/stores/settings';
import { guarded, useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import type { DbError } from '@/bindings/DbError';

import {
  NO_EDITS,
  addInsert,
  applyRow,
  counts,
  duplicateValues,
  removeInserts,
  rowKey,
  setCell,
  setInsertCell,
  toChanges,
  toggleDeletes,
  type Edits,
  type Value,
} from './edits';
import { ImportDialog } from './ImportDialog';
import { useTableKeyBadges } from './useKeyBadges';
import styles from './Table.module.css';

interface DataViewProps {
  object: ObjectRef;
  structure: TableStructure;
  onReloadStructure: () => void;
  /** Rendered first in the toolbar (the Data/Structure/DDL switcher). */
  leading?: React.ReactNode;
  /** Exact-match filter from foreign-key navigation. */
  matches: ColumnValue[];
  onClearMatches: () => void;
}

const MAX_UNDO = 100;

export function DataView({ object, structure, onReloadStructure, leading, matches, onClearMatches }: DataViewProps) {
  const session = useWorkspace((s) => s.session)!;
  const sid = { sessionId: session.session_id };
  const pageSize = useSettings((s) => s.settings.page_size);
  const { ask, toast, toastError } = useUi();
  const [page, setPage] = useState<TablePage | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<DbError | null>(null);
  const [count, setCount] = useState<RowCount | null>(null);
  const [offset, setOffset] = useState(0);
  const [filterInput, setFilterInput] = useState('');
  const [filter, setFilter] = useState<string | null>(null);
  const [sort, setSort] = useState<SortSpec | null>(null);
  const [reload, setReload] = useState(0);
  const [history, setHistory] = useState<{ edits: Edits; past: Edits[]; future: Edits[] }>({ edits: NO_EDITS, past: [], future: [] });
  const edits = history.edits;
  const undo = history;
  const [searches, setSearches] = useState<ColumnSearch[]>([]);
  const [bulk, setBulk] = useState<{ col: number; rows: number[] } | null>(null);
  /** Every change to pending edits is undoable until they are saved or discarded. */
  const setEdits = useCallback((next: Edits | ((e: Edits) => Edits)) => {
    setHistory((h) => {
      const value = typeof next === 'function' ? next(h.edits) : next;
      if (value === h.edits) return h;
      if (value === NO_EDITS) return { edits: value, past: [], future: [] };
      return { edits: value, past: [...h.past, h.edits].slice(-MAX_UNDO), future: [] };
    });
  }, []);
  const undoEdit = () =>
    setHistory((h) => (h.past.length ? { edits: h.past[h.past.length - 1]!, past: h.past.slice(0, -1), future: [...h.future, h.edits] } : h));
  const redoEdit = () =>
    setHistory((h) => (h.future.length ? { edits: h.future[h.future.length - 1]!, past: [...h.past, h.edits], future: h.future.slice(0, -1) } : h));
  const [selection, setSelection] = useState<Set<number>>(new Set());
  const [preview, setPreview] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [importing, setImporting] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[] } | null>(null);

  // Tables without a key can be edited by matching every comparable column, after the user confirms.
  const [fullRow, setFullRow] = useState(false);
  const keyColumns = useMemo(
    () => (structure.row_identity.length > 0 ? structure.row_identity : fullRow ? structure.full_row_key : []),
    [structure, fullRow],
  );
  const editable = session.capabilities.grid_edit && structure.object.kind === 'table' && keyColumns.length > 0;
  const identity = useMemo(() => keyColumns.map((n) => structure.columns.findIndex((c) => c.name === n)), [structure, keyColumns]);
  const pending = counts(edits);
  const name = objectLabel(object);

  useEffect(() => {
    let stale = false;
    setLoading(true);
    setError(null);
    api
      .tablePage(sid, { object, filter, matches, searches, sort, offset, limit: pageSize })
      .then((p) => !stale && setPage(p))
      .catch((e) => !stale && setError(toAppError(e).info))
      .finally(() => !stale && setLoading(false));
    return () => {
      stale = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [object, filter, matches, searches, sort, offset, pageSize, reload, session.session_id]);

  useEffect(() => {
    let stale = false;
    setCount(null);
    api
      .rowCount(sid, object, { filter, matches, searches })
      .then((c) => !stale && setCount(c))
      .catch(() => !stale && setCount(null));
    return () => {
      stale = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [object, filter, matches, searches, reload, session.session_id]);

  /** Runs a navigation only after pending edits are saved or discarded. */
  const guardEdits = useCallback(
    async (go: () => void) => {
      if (pending.total > 0) {
        const ok = await ask({ title: `Discard ${plural(pending.total, 'pending change')}?`, body: 'They have not been sent to the database.', confirmLabel: 'Discard changes', danger: true });
        if (!ok) return;
        setEdits(NO_EDITS);
      }
      setSelection(new Set());
      go();
    },
    [pending.total, ask, setEdits],
  );

  const badges = useTableKeyBadges(structure.object);
  const columns = useMemo<GridColumn[]>(
    () => structure.columns.map((c) => ({ name: c.name, typeName: c.data_type, numeric: c.numeric, editable: editable && c.editable, badge: badges.get(c.name) })),
    [structure, editable, badges],
  );

  const pageRows = useMemo(() => page?.rows ?? [], [page]);
  const rows = useMemo<GridRow[]>(() => {
    const inserted = edits.inserts.map<GridRow>((r) => ({
      key: r.id,
      status: 'new',
      cells: structure.columns.map((_, i) => (i in r.values ? cellFromInput(r.values[i] ?? null, undefined) : { kind: 'null' })),
      edited: new Set(Object.keys(r.values).map(Number)),
      defaults: new Set(structure.columns.map((_, i) => i).filter((i) => !(i in r.values))),
    }));
    const existing = pageRows.map<GridRow>((cells, i) => {
      const key = editable ? rowKey(cells, identity) : String(i);
      const changes = edits.updates[key];
      return {
        key: `${key}#${i}`,
        cells: applyRow(cells, changes),
        status: edits.deletes.includes(key) ? 'deleted' : undefined,
        edited: changes ? new Set(Object.keys(changes).map(Number)) : undefined,
      };
    });
    return [...inserted, ...existing];
  }, [pageRows, edits, structure, editable, identity]);

  const insertCount = edits.inserts.length;
  const locate = (index: number) =>
    index < insertCount ? ({ type: 'insert', id: edits.inserts[index]!.id } as const) : ({ type: 'row', cells: pageRows[index - insertCount]! } as const);

  const editCell = (index: number, col: number, value: Value) => {
    const target = locate(index);
    if (target.type === 'insert') setEdits((e) => setInsertCell(e, target.id, col, value));
    else setEdits((e) => setCell(e, rowKey(target.cells, identity), col, value, target.cells[col]));
  };

  const selected = () => [...selection].sort((a, b) => a - b).map(locate);

  const deleteSelected = () => {
    const targets = selected();
    if (!targets.length) return toast('Select rows by clicking their numbers first.');
    const keys = targets.flatMap((t) => (t.type === 'row' ? [rowKey(t.cells, identity)] : []));
    const ids = targets.flatMap((t) => (t.type === 'insert' ? [t.id] : []));
    setEdits((e) => toggleDeletes(removeInserts(e, ids), keys));
    setSelection(new Set());
  };

  const duplicateSelected = (indexes = [...selection]) => {
    const rowsToCopy = indexes.map(locate).filter((t) => t.type === 'row');
    if (!rowsToCopy.length) return toast('Select a row to duplicate.');
    setEdits((e) => rowsToCopy.reduce((acc, t) => addInsert(acc, duplicateValues(t.type === 'row' ? t.cells : [], structure.columns)), e));
    setSelection(new Set());
  };

  const changes = () => toChanges(edits, structure.columns, identity, pageRows);

  const confirmFullRow = async () => {
    const ok = await ask({
      title: `Edit ${name} without a key?`,
      body: `Rows will be matched on ${structure.full_row_key.join(', ')}. A change is saved only if it matches exactly one row; identical duplicate rows cannot be edited. Adding a primary key is the safer fix.`,
      confirmLabel: 'Edit anyway',
      danger: true,
    });
    if (ok) setFullRow(true);
  };

  const showPreview = async () => {
    try {
      setPreview(await api.previewChanges(sid, object, changes(), fullRow));
    } catch (e) {
      toastError(e);
    }
  };

  const save = async () => {
    setSaving(true);
    try {
      const report = await guarded((c) => api.applyChanges(sid, object, changes(), fullRow, c));
      if (report) {
        toast(`Saved ${plural(report.updated + report.inserted + report.deleted, 'change')} in ${formatDuration(report.duration_ms)}.`, 'positive');
        setEdits(NO_EDITS);
        setSelection(new Set());
        setReload((n) => n + 1);
      }
    } catch (e) {
      toastError(e);
    } finally {
      setSaving(false);
    }
  };

  const onSort = (column: string) =>
    void guardEdits(() => {
      setOffset(0);
      setSort((s) => (s?.column !== column ? { column, descending: false } : s.descending ? null : { column, descending: true }));
    });

  const applyFilter = () =>
    void guardEdits(() => {
      setOffset(0);
      setFilter(filterInput.trim() || null);
    });

  const extraMenu = ({ row, col }: { row: number; col: number }): MenuEntry[] =>
    editable && columns[col]?.editable
      ? [
          { type: 'separator' },
          { label: 'Set NULL', onSelect: () => editCell(row, col, null) },
          { label: 'Set empty string', onSelect: () => editCell(row, col, '') },
          ...(selection.size > 1 ? [{ label: `Set ${columns[col]!.name} for ${selection.size} rows…`, onSelect: () => setBulk({ col, rows: [...selection].sort((a, b) => a - b) }) }] : []),
          { label: 'Duplicate row', onSelect: () => duplicateSelected([row]) },
          {
            label: 'Delete row',
            danger: true,
            onSelect: () => {
              const t = locate(row);
              setEdits((e) => (t.type === 'insert' ? removeInserts(e, [t.id]) : toggleDeletes(e, [rowKey(t.cells, identity)])));
            },
          },
        ]
      : [];

  const openMenu = (e: React.MouseEvent, items: MenuEntry[]) => {
    const r = e.currentTarget.getBoundingClientRect();
    setMenu({ x: r.left, y: r.bottom + 4, items });
  };

  const first = page && page.rows.length ? page.offset + 1 : 0;
  const last = page ? page.offset + page.rows.length : 0;
  const pages = count ? Math.max(1, Math.ceil(count.count / pageSize)) : null;
  const exportSource = { type: 'table' as const, object, filter, matches, searches, sort };

  /** Foreign-key columns link to the referenced row. */
  const foreignKeys = useMemo(() => {
    const links = new Map<number, ForeignKeyLink>();
    for (const fk of structure.foreign_keys) {
      const cols = fk.columns.map((c) => structure.columns.findIndex((x) => x.name === c));
      if (cols.some((c) => c < 0)) continue;
      for (const col of cols) {
        links.set(col, {
          title: `Open ${objectLabel(fk.target)} row`,
          open: (index) => {
            const cells = rows[index]?.cells;
            if (!cells) return;
            const values = cols.map((c, i) => ({ column: fk.target_columns[i] ?? '', value: cellText(cells[c] ?? { kind: 'null' }) }));
            useWorkspace.getState().openTableTab(fk.target, 'data', values);
          },
        });
      }
    }
    return links;
  }, [structure, rows]);

  /** Row key of an existing (saved) row, for fetching full values. */
  const keyOf = (index: number): ColumnValue[] | null => {
    if (index < insertCount || keyColumns.length === 0) return null;
    const cells = pageRows[index - insertCount];
    return cells ? keyColumns.map((c, i) => ({ column: c, value: cellText(cells[identity[i]!] ?? { kind: 'null' }) })) : null;
  };

  const loadCell = async (index: number, col: number) => {
    const key = keyOf(index);
    const column = structure.columns[col]?.name;
    if (!key || !column) return rows[index]!.cells[col]!;
    return api.cellValue(sid, object, key, column);
  };

  const saveCell = async (index: number, col: number) => {
    const key = keyOf(index);
    const column = structure.columns[col]?.name;
    if (!key || !column) return toast('Only saved rows with a key can be saved to a file.');
    const path = await saveDialog({ defaultPath: `${object.name}-${column}.bin` });
    if (!path) return;
    try {
      const bytes = await api.saveCell(sid, object, key, column, path);
      toast(`Saved ${formatBytes(bytes)}.`, 'positive');
    } catch (e) {
      toastError(e);
    }
  };

  /** Spreadsheet cells pasted at a position: existing rows are edited, extra rows become new rows. */
  const paste = (at: { row: number; col: number }, values: string[][]) => {
    if (!editable) return toast('This table cannot be edited.');
    let skipped = 0;
    {
      let next = edits;
      values.forEach((line, r) => {
        const index = at.row + r;
        let insertId: string | null = null;
        if (index >= rows.length) {
          next = addInsert(next);
          insertId = next.inserts[0]!.id;
        }
        line.forEach((value, c) => {
          const col = at.col + c;
          const info = structure.columns[col];
          if (!info?.editable) {
            skipped += 1;
            return;
          }
          if (insertId) next = setInsertCell(next, insertId, col, value);
          else if (index < insertCount) next = setInsertCell(next, next.inserts[index]!.id, col, value);
          else {
            const cells = pageRows[index - insertCount]!;
            next = setCell(next, rowKey(cells, identity), col, value, cells[col]);
          }
        });
      });
      setEdits(next);
    }
    toast(`Pasted ${values.length} row${values.length === 1 ? '' : 's'} as pending changes${skipped ? ` (${skipped} read-only cells skipped)` : ''}.`);
  };

  return (
    <>
      <div className={styles.toolbar} role="toolbar" aria-label="Table data">
        {leading}
        <form
          className={styles.filter}
          onSubmit={(e) => {
            e.preventDefault();
            applyFilter();
          }}
        >
          <span className={styles.filterLabel}>WHERE</span>
          <input
            className={styles.filterInput}
            aria-label="Filter rows (SQL expression)"
            placeholder="status = 'paid'"
            spellCheck={false}
            value={filterInput}
            onChange={(e) => setFilterInput(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape' && filterInput) {
                setFilterInput('');
                if (filter) void guardEdits(() => setFilter(null));
              }
            }}
          />
        </form>
        <span className={styles.sortInfo}>
          {sort ? (
            <>
              Sorted by <span className="mono" style={{ color: 'var(--ink)' }}>{`${sort.column} ${sort.descending ? '↓' : '↑'}`}</span>
            </>
          ) : (
            'Click a column to sort'
          )}
        </span>
        <span style={{ flex: 1 }} />
        {editable && (
          <>
            <IconButton label="Undo edit" disabled={!undo.past.length} onClick={undoEdit}>
              <Icon name="undo" size={12} />
            </IconButton>
            <IconButton label="Redo edit" disabled={!undo.future.length} onClick={redoEdit}>
              <Icon name="redo" size={12} />
            </IconButton>
            <Button size="sm" onClick={() => setEdits((e) => addInsert(e))}>
              + Row
            </Button>
            <Button size="sm" disabled={!selection.size} onClick={() => duplicateSelected()}>
              Duplicate
            </Button>
            <Button size="sm" disabled={!selection.size} onClick={deleteSelected}>
              − Delete
            </Button>
          </>
        )}
        <Button
          size="sm"
          onClick={(e) =>
            openMenu(e, [
              { label: 'Export…', onSelect: () => exportData(exportSource, 'csv', object.name) },
              ...(structure.object.kind === 'table' && session.capabilities.import_export && !session.server.read_only
                ? [{ type: 'separator' as const }, { label: 'Import CSV or JSON…', onSelect: () => setImporting(true) }]
                : []),
            ])
          }
        >
          Export / Import
        </Button>
        <IconButton label="Reload rows" onClick={() => void guardEdits(() => setReload((n) => n + 1))}>
          <Icon name="refresh" size={12} />
        </IconButton>
      </div>
      {matches.length > 0 && (
        <div className={styles.notice} role="status">
          Showing rows where{' '}
          <span className="mono">{matches.map((m) => `${m.column} = ${m.value === null ? 'NULL' : m.value}`).join(' AND ')}</span>
          <Button size="xs" onClick={() => void guardEdits(onClearMatches)}>
            Show all rows
          </Button>
        </div>
      )}
      {!editable && structure.object.kind === 'table' && !session.server.read_only && (
        <div className={styles.notice} role="status">
          This table has no primary key or NOT NULL unique key, so rows are read-only to avoid changing the wrong rows.
          {session.capabilities.grid_edit && structure.full_row_key.length > 0 && (
            <Button size="xs" onClick={() => void confirmFullRow()}>
              Edit anyway…
            </Button>
          )}
        </div>
      )}
      {fullRow && structure.row_identity.length === 0 && (
        <div className={styles.notice} role="status">
          Editing without a key: each change matches every column of the row, and nothing is saved unless it matches exactly one row.
        </div>
      )}
      {error && (
        <div className={styles.errorBar}>
          <ErrorAlert error={error} title={filter ? 'The filter could not be applied' : 'Could not load rows'} />
        </div>
      )}
      <DataGrid
        label={`Rows of ${name}`}
        columns={columns}
        rows={rows}
        rowOffset={(page?.offset ?? 0) - insertCount}
        editable={editable}
        onEditCell={editCell}
        selectedRows={selection}
        onSelectRows={setSelection}
        sort={sort}
        onSort={onSort}
        extraMenu={extraMenu}
        onDeleteRows={deleteSelected}
        emptyText={loading ? 'Loading…' : filter || matches.length || searches.length ? 'No rows match the filter.' : 'This table is empty.'}
        foreignKeys={foreignKeys}
        onColumnSearch={(s) =>
          void guardEdits(() => {
            setOffset(0);
            setSearches(Object.entries(s).filter(([, v]) => v.trim()).map(([column, contains]) => ({ column, contains: contains.trim() })));
          })
        }
        onLoadCell={loadCell}
        onSaveCell={(r, c) => void saveCell(r, c)}
        onPaste={editable ? paste : undefined}
        onUndo={editable ? undoEdit : undefined}
        onRedo={editable ? redoEdit : undefined}
      />
      <div className={styles.footer}>
        <span>
          Rows <span className="mono" style={{ color: 'var(--ink)' }}>{first ? `${formatCount(first)}–${formatCount(last)}` : '0'}</span>
          {count && (
            <>
              {' '}
              of <span className="mono" style={{ color: 'var(--ink)' }}>{`${count.exact ? '' : '≈'}${formatCount(count.count)}`}</span>
            </>
          )}
          {keyColumns.length > 0 && (
            <>
              {' '}
              · row identity: {structure.row_identity.length === 0 ? 'all columns' : structure.columns.find((c) => c.name === structure.row_identity[0])?.primary_key ? 'primary key' : 'unique key'}{' '}
              <span className="mono" style={{ color: 'var(--ink)' }}>{keyColumns.join(', ')}</span>
            </>
          )}
          {page && ` · ${formatDuration(page.duration_ms)}`}
        </span>
        <div className={styles.pager}>
          <Button size="xs" disabled={offset === 0 || loading} onClick={() => void guardEdits(() => setOffset(Math.max(0, offset - pageSize)))}>
            ← Prev
          </Button>
          <span className="mono">
            {Math.floor(offset / pageSize) + 1}
            {pages ? ` / ${formatCount(pages)}` : ''}
          </span>
          <Button size="xs" disabled={!page?.has_more || loading} onClick={() => void guardEdits(() => setOffset(offset + pageSize))}>
            Next →
          </Button>
        </div>
      </div>

      {pending.total > 0 && (
        <div className={styles.actionBar} role="region" aria-label="Pending changes">
          <div className={styles.actionText}>
            <span className="dq-eyebrow">Pending changes · not yet sent</span>
            <span>
              {[pending.cells && plural(pending.cells, 'edited cell'), pending.inserts && plural(pending.inserts, 'new row'), pending.deletes && plural(pending.deletes, 'deleted row')]
                .filter(Boolean)
                .join(' · ')}{' '}
              in <span className="mono">{name}</span>, applied in one transaction
            </span>
          </div>
          <div className={styles.actions}>
            <Button onClick={() => setEdits(NO_EDITS)}>Discard</Button>
            <Button onClick={() => void showPreview()}>Preview SQL</Button>
            <Button variant="primary" busy={saving} onClick={() => void save()}>
              Save {plural(pending.total, 'change')}
            </Button>
          </div>
        </div>
      )}

      {preview !== null && (
        <Dialog
          title="Preview SQL"
          wide
          onClose={() => setPreview(null)}
          footer={
            <>
              <Button onClick={() => void copyText(preview).then(() => toast('SQL copied'))}>Copy</Button>
              <Button onClick={() => setPreview(null)}>Close</Button>
              <Button
                variant="primary"
                busy={saving}
                onClick={() => {
                  setPreview(null);
                  void save();
                }}
              >
                Save {plural(pending.total, 'change')}
              </Button>
            </>
          }
        >
          <p>These statements run in one transaction. Values are sent as parameters; literals are shown here for review.</p>
          <div style={{ height: 260, border: '1px solid var(--line)', borderRadius: 4, overflow: 'hidden' }}>
            <SqlViewer value={preview} flavor={session.flavor} label="Change preview" />
          </div>
        </Dialog>
      )}
      {importing && (
        <ImportDialog
          object={object}
          structure={structure}
          onClose={() => setImporting(false)}
          onImported={() => {
            setReload((n) => n + 1);
            onReloadStructure();
          }}
        />
      )}
      {menu && <Menu x={menu.x} y={menu.y} items={menu.items} onClose={() => setMenu(null)} />}
      {bulk && (
        <BulkEditDialog
          column={structure.columns[bulk.col]!}
          count={bulk.rows.length}
          onClose={() => setBulk(null)}
          onApply={(value) => {
            setEdits((e) =>
              bulk.rows.reduce((acc, index) => {
                const t = locate(index);
                return t.type === 'insert' ? setInsertCell(acc, t.id, bulk.col, value) : setCell(acc, rowKey(t.cells, identity), bulk.col, value, t.cells[bulk.col]);
              }, e),
            );
            setBulk(null);
            toast(`Set ${structure.columns[bulk.col]!.name} on ${bulk.rows.length} rows as pending changes. Preview SQL before saving.`);
          }}
        />
      )}
    </>
  );
}

/** One value for one column across the selected rows; becomes pending changes. */
function BulkEditDialog({ column, count, onClose, onApply }: { column: ColumnInfo; count: number; onClose: () => void; onApply: (value: Value) => void }) {
  const [value, setValue] = useState('');
  const [isNull, setIsNull] = useState(false);
  return (
    <Dialog
      title={`Set ${column.name} for ${count} rows`}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={() => onApply(isNull ? null : value)}>
            Apply as pending changes
          </Button>
        </>
      }
    >
      <TextField label={`${column.name} (${column.data_type})`} mono value={value} disabled={isNull} onChange={(e) => setValue(e.target.value)} autoFocus />
      {column.nullable && (
        <div style={{ marginTop: 8 }}>
          <Checkbox label="NULL" checked={isNull} onChange={setIsNull} />
        </div>
      )}
      <p className="ink-3" style={{ fontSize: 12, marginTop: 8 }}>Nothing is written until you save; Preview SQL shows every statement first.</p>
    </Dialog>
  );
}
