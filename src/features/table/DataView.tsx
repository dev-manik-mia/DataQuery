import { useCallback, useEffect, useMemo, useState } from 'react';

import type { ObjectRef } from '@/bindings/ObjectRef';
import type { RowCount } from '@/bindings/RowCount';
import type { SortSpec } from '@/bindings/SortSpec';
import type { TablePage } from '@/bindings/TablePage';
import type { TableStructure } from '@/bindings/TableStructure';
import { Button, Dialog, ErrorAlert, Icon, IconButton, Menu, type MenuEntry } from '@/components/ui';
import { SqlViewer } from '@/features/editor/SqlViewer';
import { DataGrid, type GridColumn, type GridRow } from '@/features/grid/DataGrid';
import { exportData } from '@/features/transfer/exportData';
import { cellFromInput } from '@/lib/cells';
import { copyText } from '@/lib/clipboard';
import { toAppError } from '@/lib/errors';
import { formatCount, formatDuration, plural } from '@/lib/format';
import { api } from '@/lib/ipc';
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
import styles from './Table.module.css';

interface DataViewProps {
  object: ObjectRef;
  structure: TableStructure;
  onReloadStructure: () => void;
  /** Rendered first in the toolbar (the Data/Structure/DDL switcher). */
  leading?: React.ReactNode;
}

export function DataView({ object, structure, onReloadStructure, leading }: DataViewProps) {
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
  const [edits, setEdits] = useState<Edits>(NO_EDITS);
  const [selection, setSelection] = useState<Set<number>>(new Set());
  const [preview, setPreview] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [importing, setImporting] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[] } | null>(null);

  const editable = session.capabilities.grid_edit && structure.object.kind === 'table' && structure.row_identity.length > 0;
  const identity = useMemo(() => structure.row_identity.map((n) => structure.columns.findIndex((c) => c.name === n)), [structure]);
  const pending = counts(edits);
  const name = `${object.schema ? `${object.schema}.` : ''}${object.name}`;

  useEffect(() => {
    let stale = false;
    setLoading(true);
    setError(null);
    api
      .tablePage(sid, { object, filter, sort, offset, limit: pageSize })
      .then((p) => !stale && setPage(p))
      .catch((e) => !stale && setError(toAppError(e).info))
      .finally(() => !stale && setLoading(false));
    return () => {
      stale = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [object, filter, sort, offset, pageSize, reload, session.session_id]);

  useEffect(() => {
    let stale = false;
    setCount(null);
    api
      .rowCount(sid, object, filter)
      .then((c) => !stale && setCount(c))
      .catch(() => !stale && setCount(null));
    return () => {
      stale = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [object, filter, reload, session.session_id]);

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
    [pending.total, ask],
  );

  const columns = useMemo<GridColumn[]>(
    () => structure.columns.map((c) => ({ name: c.name, typeName: c.data_type, numeric: c.numeric, editable: editable && c.editable })),
    [structure, editable],
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

  const showPreview = async () => {
    try {
      setPreview(await api.previewChanges(sid, object, changes()));
    } catch (e) {
      toastError(e);
    }
  };

  const save = async () => {
    setSaving(true);
    try {
      const report = await guarded((c) => api.applyChanges(sid, object, changes(), c));
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
  const exportSource = { type: 'table' as const, object, filter, sort };

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
              { label: 'Export CSV…', onSelect: () => void exportData(exportSource, 'csv', object.name) },
              { label: 'Export JSON…', onSelect: () => void exportData(exportSource, 'json', object.name) },
              ...(structure.object.kind === 'table' && session.capabilities.import_export ? [{ type: 'separator' as const }, { label: 'Import CSV…', onSelect: () => setImporting(true) }] : []),
            ])
          }
        >
          Export / Import
        </Button>
        <IconButton label="Reload rows" onClick={() => void guardEdits(() => setReload((n) => n + 1))}>
          <Icon name="refresh" size={12} />
        </IconButton>
      </div>
      {!editable && structure.object.kind === 'table' && (
        <div className={styles.notice} role="status">
          This table has no primary key or NOT NULL unique key, so rows are read-only to avoid changing the wrong rows.
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
        emptyText={loading ? 'Loading…' : filter ? 'No rows match the filter.' : 'This table is empty.'}
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
          {structure.row_identity.length > 0 && (
            <>
              {' '}
              · row identity: {structure.columns.find((c) => c.name === structure.row_identity[0])?.primary_key ? 'primary key' : 'unique key'}{' '}
              <span className="mono" style={{ color: 'var(--ink)' }}>{structure.row_identity.join(', ')}</span>
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
            <span className="rb-eyebrow">Pending changes · not yet sent</span>
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
    </>
  );
}
