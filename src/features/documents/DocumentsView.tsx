import { useCallback, useEffect, useMemo, useState, type KeyboardEvent, type MouseEvent } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { Document } from '@/bindings/Document';
import type { DocumentChange } from '@/bindings/DocumentChange';
import type { DocumentPage } from '@/bindings/DocumentPage';
import type { FindRequest } from '@/bindings/FindRequest';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { RowCount } from '@/bindings/RowCount';
import type { TableStructure } from '@/bindings/TableStructure';
import { Button, ErrorAlert, Icon, IconButton, Menu, Segmented, SelectField, Spinner, Tabs, type MenuEntry } from '@/components/ui';
import { DataGrid } from '@/features/grid/DataGrid';
import { ImportDialog } from '@/features/table/ImportDialog';
import { exportData } from '@/features/transfer/exportData';
import { copyText } from '@/lib/clipboard';
import { toAppError } from '@/lib/errors';
import { formatDuration } from '@/lib/format';
import { api, objectRef } from '@/lib/ipc';
import { guarded, useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { AggregationPanel, IndexesPanel, ValidationPanel } from './Panels';
import { DocumentEditor } from './DocumentEditor';
import { DocumentTree } from './DocumentTree';
import { documentsGrid, rangeLabel } from './docs';
import styles from './Documents.module.css';

type DocTab = 'documents' | 'aggregation' | 'indexes' | 'validation';
type Layout = 'tree' | 'table' | 'json';

interface Query {
  filter: string;
  projection: string;
  sort: string;
}

const EMPTY: Query = { filter: '', projection: '', sort: '' };
const PAGE_SIZES = [20, 50, 100, 500];
const PROJECTION_LABEL: Record<string, string> = { PartiQL: 'Attributes', 'Query DSL': 'Source fields', Cypher: 'Return' };
const SORT_EXAMPLE: Record<string, string> = { MQL: '{ createdAt: -1 }', 'Query DSL': '{ "created_at": "desc" }', Cypher: 'n.name DESC' };

/** Screen 13: a collection's documents, aggregation pipeline, indexes and validation rules. */
export function DocumentsView({ object }: { object: ObjectRef }) {
  const session = useWorkspace((s) => s.session)!;
  const openQueryTab = useWorkspace((s) => s.openQueryTab);
  const toast = useUi((s) => s.toast);
  const features = session.documents!;
  const sid = useMemo(() => ({ sessionId: session.session_id }), [session.session_id]);
  const writable = object.kind === 'table' && session.capabilities.grid_edit && !session.server.read_only;
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[] } | null>(null);
  const openMenu = (e: MouseEvent, items: MenuEntry[]) => {
    const r = e.currentTarget.getBoundingClientRect();
    setMenu({ x: r.left, y: r.bottom + 4, items });
  };

  const [tab, setTab] = useState<DocTab>('documents');
  const [layout, setLayout] = useState<Layout>('tree');
  const [draft, setDraft] = useState<Query>(EMPTY);
  const [query, setQuery] = useState<Query>(EMPTY);
  const [pageSize, setPageSize] = useState(20);
  const [cursors, setCursors] = useState<(string | null)[]>([null]);
  const [pageIndex, setPageIndex] = useState(0);
  const [page, setPage] = useState<DocumentPage | null>(null);
  const [total, setTotal] = useState<RowCount | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<DbError | null>(null);
  const [reload, setReload] = useState(0);
  const [editing, setEditing] = useState<{ document: Document | null } | null>(null);
  const [importing, setImporting] = useState<TableStructure | null>(null);
  const [indexCount, setIndexCount] = useState<number | null>(null);

  const request = useCallback(
    (cursor: string | null): FindRequest => ({ object: objectRef(object), ...query, cursor, limit: pageSize }),
    [object, query, pageSize],
  );

  // The current page.
  useEffect(() => {
    let stale = false;
    setLoading(true);
    setError(null);
    api
      .documentsFind(sid, request(cursors[pageIndex] ?? null))
      .then((p) => !stale && setPage(p))
      .catch((e) => {
        if (stale) return;
        setPage(null);
        setError(toAppError(e).info);
      })
      .finally(() => !stale && setLoading(false));
    return () => {
      stale = true;
    };
  }, [sid, request, cursors, pageIndex, reload]);

  // The total for the applied filter (an estimate when the engine cannot count with it).
  useEffect(() => {
    let stale = false;
    setTotal(null);
    if (query.filter.trim() && !features.filtered_count) return;
    api
      .documentsCount(sid, object, query.filter)
      .then((c) => !stale && setTotal(c))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [sid, object, query.filter, features.filtered_count, reload]);

  useEffect(() => {
    api
      .documentsIndexes(sid, object)
      .then((i) => setIndexCount(i.length))
      .catch(() => setIndexCount(null));
  }, [sid, object]);

  const find = () => {
    setQuery({ ...draft });
    setCursors([null]);
    setPageIndex(0);
    setReload((n) => n + 1);
  };
  const onQueryKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      find();
    }
  };
  const next = () => {
    if (!page?.next) return;
    setCursors((c) => [...c.slice(0, pageIndex + 1), page.next]);
    setPageIndex(pageIndex + 1);
  };

  /** Runs one change through the backend's confirmation rules; returns the error to show, if any. */
  const write = async (change: DocumentChange, done: string): Promise<DbError | null> => {
    try {
      const result = await guarded((c) => api.documentsWrite(sid, object, change, c));
      if (result === null) return null;
      toast(done, 'positive');
      setEditing(null);
      setReload((n) => n + 1);
      return null;
    } catch (e) {
      return toAppError(e).info;
    }
  };

  const remove = async (d: Document) => {
    const err = await write({ op: 'delete', key: d.key, label: d.label }, 'Document deleted');
    if (err) useUi.getState().toastError(err);
  };

  const openInEditor = async () => {
    try {
      openQueryTab({ sql: await api.documentsStatement(sid, request(null)), title: `${object.name}.query` });
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };

  const startImport = async () => {
    try {
      setImporting(await api.tableStructure(sid, object));
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };

  const documents = useMemo(() => page?.documents ?? [], [page]);
  const first = pageIndex * pageSize;
  const grid = useMemo(() => (layout === 'table' ? documentsGrid(documents) : null), [layout, documents]);
  const tabs = [
    { id: 'documents' as const, label: 'Documents' },
    ...(features.aggregation ? [{ id: 'aggregation' as const, label: 'Aggregation' }] : []),
    { id: 'indexes' as const, label: 'Indexes', count: indexCount ?? undefined },
    ...(features.validation ? [{ id: 'validation' as const, label: 'Validation' }] : []),
  ];

  return (
    <div className={styles.view}>
      <div className={styles.tabs}>
        <Tabs label="Collection" items={tabs} value={tab} onChange={setTab} />
      </div>
      {/* Kept mounted so the pipeline survives switching tabs. */}
      {features.aggregation && (
        <div className={styles.view} style={tab === 'aggregation' ? undefined : { display: 'none' }}>
          <AggregationPanel object={object} />
        </div>
      )}
      {tab === 'indexes' && <IndexesPanel object={object} />}
      {tab === 'validation' && <ValidationPanel object={object} />}
      {tab === 'documents' && (
        <>
          <div className={`${styles.queryBar} ${features.sort ? '' : styles.noSort}`} role="search">
            <QueryField label="Filter" value={draft.filter} placeholder={features.filter_example} onChange={(filter) => setDraft({ ...draft, filter })} onKeyDown={onQueryKey} />
            <QueryField
              label={PROJECTION_LABEL[features.language] ?? 'Project'}
              value={draft.projection}
              placeholder={features.projection_example}
              onChange={(projection) => setDraft({ ...draft, projection })}
              onKeyDown={onQueryKey}
            />
            {features.sort && <QueryField label="Sort" value={draft.sort} placeholder={SORT_EXAMPLE[features.language] ?? '{ createdAt: -1 }'} onChange={(sort) => setDraft({ ...draft, sort })} onKeyDown={onQueryKey} />}
            <Button variant="primary" size="sm" busy={loading} onClick={find}>
              Find
            </Button>
          </div>
          <div className={styles.status}>
            <span className="tabular">
              Documents {rangeLabel(first, documents.length, total)}
              {page && ` · ${formatDuration(page.duration_ms)}`}
            </span>
            <span className={styles.spacer} />
            <Segmented<Layout>
              label="Layout"
              value={layout}
              onChange={setLayout}
              options={[
                { value: 'tree', label: 'Tree' },
                { value: 'table', label: 'Table' },
                { value: 'json', label: features.text_format === 'dynamodb_json' ? 'JSON' : 'Shell' },
              ]}
            />
            {writable && (
              <Button size="sm" icon={<Icon name="plus" size={12} />} onClick={() => setEditing({ document: null })}>
                Insert
              </Button>
            )}
            <Button
              size="sm"
              onClick={(e) =>
                openMenu(e, [
                  { label: 'Open in editor', onSelect: () => void openInEditor() },
                  { type: 'separator' },
                  { label: 'Export…', onSelect: () => exportData({ type: 'documents', request: request(null) }, 'json', object.name) },
                  ...(writable && session.capabilities.import_export ? [{ label: 'Import CSV or JSON…', onSelect: () => void startImport() }] : []),
                ])
              }
            >
              More
            </Button>
            <IconButton label="Reload documents" onClick={() => setReload((n) => n + 1)}>
              <Icon name="refresh" size={12} />
            </IconButton>
          </div>
          {error && (
            <div style={{ padding: 12 }}>
              <ErrorAlert error={error} title="Could not read documents" />
            </div>
          )}
          {layout === 'table' && grid ? (
            <div className={styles.gridBody}>
              <DataGrid label={`${object.name} documents`} columns={grid.columns} rows={grid.rows} rowOffset={first} emptyText="No documents match." />
            </div>
          ) : (
            <div className={styles.body}>
              {!page && loading && <Spinner label="Reading documents" />}
              {page && documents.length === 0 && <p className="ink-3">No documents match.</p>}
              {layout === 'tree' && (
                <DocumentTree
                  documents={documents}
                  onCopy={(d) => void copyText(d.text).then(() => toast('Document copied'))}
                  // A projection shows partial documents: saving one would drop the hidden fields.
                  onEdit={writable && !query.projection.trim() ? (d) => setEditing({ document: d }) : undefined}
                  onDelete={writable ? (d) => void remove(d) : undefined}
                />
              )}
              {layout === 'json' && (
                <pre className={styles.json}>{documents.map((d) => (d.truncated ? `// ${d.label}: too large to show here` : d.text)).join('\n\n')}</pre>
              )}
            </div>
          )}
          <div className={styles.status} style={{ borderTop: '1px solid var(--line)', borderBottom: 'none' }}>
            <SelectField
              label={<span className="sr-only">Page size</span>}
              value={String(pageSize)}
              onChange={(e) => {
                setPageSize(Number(e.target.value));
                setCursors([null]);
                setPageIndex(0);
              }}
              options={PAGE_SIZES.map((n) => ({ value: String(n), label: `${n} per page` }))}
            />
            <span className={styles.spacer} />
            <Button size="xs" disabled={pageIndex === 0 || loading} onClick={() => setPageIndex(pageIndex - 1)}>
              ‹ Previous
            </Button>
            <Button size="xs" disabled={!page?.next || loading} onClick={next}>
              Next ›
            </Button>
          </div>
        </>
      )}
      {editing && (
        <DocumentEditor
          title={editing.document ? `Edit ${editing.document.label}` : `Insert into ${object.name}`}
          initial={editing.document?.text ?? features.new_document}
          format={features.text_format}
          onClose={() => setEditing(null)}
          onSave={(text) =>
            editing.document
              ? write({ op: 'replace', key: editing.document.key, text }, 'Document saved')
              : write({ op: 'insert', text }, 'Document inserted')
          }
        />
      )}
      {importing && <ImportDialog object={object} structure={importing} onClose={() => setImporting(null)} onImported={() => setReload((n) => n + 1)} />}
      {menu && <Menu x={menu.x} y={menu.y} items={menu.items} onClose={() => setMenu(null)} />}
    </div>
  );
}

function QueryField({
  label,
  value,
  placeholder,
  onChange,
  onKeyDown,
}: {
  label: string;
  value: string;
  placeholder: string;
  onChange: (v: string) => void;
  onKeyDown: (e: KeyboardEvent<HTMLInputElement>) => void;
}) {
  return (
    <label className={styles.queryField}>
      <span className={styles.queryLabel}>{label}</span>
      <input className={styles.queryInput} value={value} placeholder={placeholder} spellCheck={false} onChange={(e) => onChange(e.target.value)} onKeyDown={onKeyDown} />
    </label>
  );
}
