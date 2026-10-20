import { useEffect, useMemo, useState } from 'react';
import { useShallow } from 'zustand/react/shallow';

import type { HistoryEntry } from '@/bindings/HistoryEntry';
import type { SavedConnection } from '@/bindings/SavedConnection';
import {
  Button,
  EmptyState,
  EngineMark,
  EnvironmentChip,
  ErrorAlert,
  Eyebrow,
  Icon,
  Input,
  Menu,
  PageHead,
  PromptDialog,
  Segmented,
  Spinner,
  useContextMenu,
  type MenuEntry,
} from '@/components/ui';
import { exportConnections, importConnections } from '@/features/connections/transfer';
import { ImportAppsDialog } from '@/features/connections/ImportAppsDialog';
import { connectionAddress } from '@/lib/engines';
import { formatCount, formatDuration, formatRelative, plural } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Connections.module.css';

type Filter = 'all' | 'favorites' | 'recent';

interface Group {
  id: string | null;
  name: string;
  items: SavedConnection[];
}

export function ConnectionsHome() {
  const { connections, folders, engines, loaded, toggleFavorite, remove, duplicate, createFolder, renameFolder, deleteFolder } = useConnections(useShallow((s) => ({ connections: s.connections, folders: s.folders, engines: s.engines, loaded: s.loaded, toggleFavorite: s.toggleFavorite, remove: s.remove, duplicate: s.duplicate, createFolder: s.createFolder, renameFolder: s.renameFolder, deleteFolder: s.deleteFolder })));
  const navigate = useUi((s) => s.navigate);
  const ask = useUi((s) => s.ask);
  const { connect, connectingId, connectError } = useWorkspace(useShallow((s) => ({ connect: s.connect, connectingId: s.connectingId, connectError: s.connectError })));
  const [query, setQuery] = useState('');
  const [filter, setFilter] = useState<Filter>('all');
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>({});
  const [prompt, setPrompt] = useState<{ kind: 'new' } | { kind: 'rename'; id: string; name: string } | null>(null);
  const [failedId, setFailedId] = useState<string | null>(null);
  const [importingApps, setImportingApps] = useState(false);
  const rowMenu = useContextMenu<SavedConnection>();
  const folderMenu = useContextMenu<Group>();

  const groups = useMemo<Group[]>(() => {
    const q = query.trim().toLowerCase();
    let list = connections.filter(
      (c) => !q || c.name.toLowerCase().includes(q) || c.config.host.toLowerCase().includes(q) || c.config.database.toLowerCase().includes(q),
    );
    if (filter === 'favorites') list = list.filter((c) => c.favorite);
    if (filter === 'recent') {
      list = list.filter((c) => c.last_used_at).sort((a, b) => (b.last_used_at ?? 0) - (a.last_used_at ?? 0));
      return list.length ? [{ id: null, name: 'Recently used', items: list.slice(0, 20) }] : [];
    }
    const byFolder = folders.map((f) => ({ id: f.id, name: f.name, items: list.filter((c) => c.folder_id === f.id) }));
    const loose = list.filter((c) => !c.folder_id || !folders.some((f) => f.id === c.folder_id));
    const all = [...byFolder, { id: null, name: folders.length ? 'No folder' : 'Connections', items: loose }];
    return all.filter((g) => g.items.length > 0 || (g.id !== null && !q && filter === 'all'));
  }, [connections, folders, query, filter]);

  const open = async (c: SavedConnection) => {
    setFailedId(null);
    if (!(await connect(c.id))) setFailedId(c.id);
  };

  const engineOf = (c: SavedConnection) => engines.find((e) => e.engine === c.config.engine);
  const failed = connections.find((c) => c.id === failedId);

  const rowItems = (c: SavedConnection): MenuEntry[] => [
    { label: 'Connect', onSelect: () => void open(c) },
    { label: 'Edit', onSelect: () => navigate({ name: 'connection-form', engine: c.config.engine, connectionId: c.id }) },
    { label: 'Duplicate', onSelect: () => void duplicate(c.id) },
    { label: 'Export…', onSelect: () => void exportConnections([c.id]) },
    { label: c.favorite ? 'Remove from favorites' : 'Add to favorites', onSelect: () => void toggleFavorite(c.id) },
    ...(folders.length
      ? [
          { type: 'separator' as const },
          { type: 'label' as const, label: 'Move to folder' },
          ...folders.filter((f) => f.id !== c.folder_id).map((f) => ({ label: f.name, onSelect: () => void moveTo(c, f.id) })),
          ...(c.folder_id ? [{ label: 'No folder', onSelect: () => void moveTo(c, null) }] : []),
        ]
      : []),
    { type: 'separator' },
    {
      label: 'Delete…',
      danger: true,
      onSelect: async () => {
        const ok = await ask({ title: `Delete ${c.name}?`, body: 'The connection and its saved password are removed from this computer. Query history is kept.', confirmLabel: 'Delete', danger: true });
        if (ok) await remove(c.id);
      },
    },
  ];

  const moveTo = async (c: SavedConnection, folderId: string | null) => {
    await useConnections.getState().save({ id: c.id, name: c.name, folder_id: folderId, environment: c.environment, favorite: c.favorite, config: c.config, password: { action: 'keep' }, ssh_secrets: [] }).catch(useUi.getState().toastError);
  };

  return (
    <div className={styles.home}>
      <section className={styles.main} aria-label="Connections">
        <PageHead
          eyebrow="Workspace"
          title="Connections"
          actions={
            <>
              <Button onClick={() => setImportingApps(true)}>Import from another app…</Button>
              <Button onClick={() => void importConnections()}>Import…</Button>
              <Button disabled={connections.length === 0} onClick={() => void exportConnections(null)}>
                Export…
              </Button>
              <Button onClick={() => setPrompt({ kind: 'new' })}>New folder</Button>
              <Button variant="primary" onClick={() => navigate({ name: 'engines' })}>
                New connection
              </Button>
            </>
          }
        />
        <div className={styles.toolbar}>
          <Input label="Search connections" placeholder="Search connections" small={false} className={styles.search} value={query} onChange={(e) => setQuery(e.target.value)} />
          <Segmented<Filter>
            label="Show"
            value={filter}
            onChange={setFilter}
            options={[
              { value: 'all', label: 'All' },
              { value: 'favorites', label: 'Favorites' },
              { value: 'recent', label: 'Recent' },
            ]}
          />
        </div>

        {failed && connectError && (
          <div>
            <ErrorAlert error={connectError} title={`Could not connect to ${connectionAddress(failed.config)}`} />
            <div style={{ display: 'flex', gap: 8, marginTop: 8 }}>
              <Button size="sm" onClick={() => void open(failed)}>
                Try again
              </Button>
              <Button size="sm" onClick={() => navigate({ name: 'connection-form', engine: failed.config.engine, connectionId: failed.id })}>
                Edit connection
              </Button>
            </div>
          </div>
        )}

        {loaded && connections.length === 0 ? (
          <EmptyState title="No connections yet">
            Add a connection to PostgreSQL, MySQL, MariaDB, SQL Server, Oracle, CockroachDB, Amazon Redshift, Greenplum, ClickHouse, Snowflake, MongoDB, DynamoDB, Redis, Valkey, Dragonfly, Cassandra, ScyllaDB, Elasticsearch, Neo4j, libSQL (Turso), Cloudflare D1 or a DuckDB file to start. Passwords are stored in your system keychain.
            <div style={{ marginTop: 12, display: 'flex', gap: 8, justifyContent: 'center' }}>
              <Button variant="primary" onClick={() => navigate({ name: 'engines' })}>
                New connection
              </Button>
              <Button onClick={() => setImportingApps(true)}>Import from TablePlus, DBeaver, Sequel Ace or DataGrip…</Button>
            </div>
          </EmptyState>
        ) : groups.length === 0 && loaded ? (
          <EmptyState title="Nothing matches">Try a different search or filter.</EmptyState>
        ) : (
          <div className={styles.groups}>
            {groups.map((g) => {
              const key = g.id ?? 'none';
              const isCollapsed = collapsed[key] ?? false;
              return (
                <div key={key} className={styles.group}>
                  <button
                    type="button"
                    className={styles.groupHead}
                    aria-expanded={!isCollapsed}
                    onClick={() => setCollapsed({ ...collapsed, [key]: !isCollapsed })}
                    onContextMenu={(e) => g.id && folderMenu.open(e, g)}
                  >
                    <span className={styles.chevron}>
                      <Icon name={isCollapsed ? 'chevronRight' : 'chevronDown'} size={12} />
                    </span>
                    {g.name}
                    <span className={styles.count}>{formatCount(g.items.length)}</span>
                  </button>
                  {!isCollapsed &&
                    g.items.map((c) => {
                      const engine = engineOf(c);
                      return (
                        <div
                          key={c.id}
                          role="button"
                          tabIndex={0}
                          className={styles.row}
                          onClick={() => void open(c)}
                          onKeyDown={(e) => e.key === 'Enter' && void open(c)}
                          onContextMenu={(e) => rowMenu.open(e, c)}
                          aria-label={`Connect to ${c.name}`}
                        >
                          <button
                            type="button"
                            className={styles.star}
                            aria-label={c.favorite ? `Remove ${c.name} from favorites` : `Add ${c.name} to favorites`}
                            aria-pressed={c.favorite}
                            onClick={(e) => {
                              e.stopPropagation();
                              void toggleFavorite(c.id);
                            }}
                          >
                            <Icon name="star" filled={c.favorite} />
                          </button>
                          {engine ? <EngineMark engine={engine} /> : <span />}
                          <span className={`${styles.name} ellipsis`} title={`${c.name} · ${connectionAddress(c.config)}`}>
                            {c.name}
                          </span>
                          <span>
                            <EnvironmentChip environment={c.environment} />
                          </span>
                          <span className={`${styles.address} ellipsis`}>{connectionAddress(c.config)}</span>
                          <span className={styles.lastUsed}>{formatRelative(c.last_used_at)}</span>
                          <span className={styles.connect}>
                            {connectingId === c.id ? (
                              <>
                                <Spinner /> Connecting…
                              </>
                            ) : (
                              'Connect →'
                            )}
                          </span>
                        </div>
                      );
                    })}
                  {!isCollapsed && g.items.length === 0 && (
                    <div className={styles.note} style={{ padding: '10px 16px' }}>
                      Empty folder. Right-click a connection to move it here.
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
      </section>
      <RecentQueries />

      {rowMenu.menu && <Menu x={rowMenu.menu.x} y={rowMenu.menu.y} items={rowItems(rowMenu.menu.target)} onClose={rowMenu.close} label="Connection actions" />}
      {folderMenu.menu && (
        <Menu
          x={folderMenu.menu.x}
          y={folderMenu.menu.y}
          onClose={folderMenu.close}
          label="Folder actions"
          items={[
            { label: 'Rename…', onSelect: () => folderMenu.menu?.target.id && setPrompt({ kind: 'rename', id: folderMenu.menu.target.id, name: folderMenu.menu.target.name }) },
            { label: 'Delete folder', danger: true, onSelect: () => folderMenu.menu?.target.id && void deleteFolder(folderMenu.menu.target.id) },
          ]}
        />
      )}
      {importingApps && <ImportAppsDialog onClose={() => setImportingApps(false)} />}
      {prompt && (
        <PromptDialog
          title={prompt.kind === 'new' ? 'New folder' : 'Rename folder'}
          label="Folder name"
          initial={prompt.kind === 'rename' ? prompt.name : ''}
          confirmLabel={prompt.kind === 'new' ? 'Create folder' : 'Rename'}
          onClose={() => setPrompt(null)}
          onSubmit={async (name) => {
            if (prompt.kind === 'new') await createFolder(name);
            else await renameFolder(prompt.id, name);
          }}
        />
      )}
    </div>
  );
}

/** The last few statements from history. Opening one never runs it. */
function RecentQueries() {
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const connect = useWorkspace((s) => s.connect);

  useEffect(() => {
    api
      .listHistory({ search: null, connection_id: null, status: null, from_ms: null, to_ms: null, before_id: null, limit: 6 })
      .then((page) => setEntries(page.entries))
      .catch(() => setEntries([]));
  }, []);

  const open = async (entry: HistoryEntry) => {
    if (!entry.connection_id) return;
    if (await connect(entry.connection_id)) useWorkspace.getState().openQueryTab({ sql: entry.sql });
  };

  return (
    <aside className={styles.aside} aria-label="Recent queries">
      <Eyebrow>Recent queries</Eyebrow>
      {entries.length === 0 && <div className={styles.note}>Statements you run appear here.</div>}
      {entries.map((e) => (
        <button key={e.id} type="button" className={styles.recent} onClick={() => void open(e)} disabled={!e.connection_id}>
          <span className={styles.recentSql}>{e.sql}</span>
          <span className={styles.lastUsed}>
            {e.connection_name} · {e.status === 'ok' ? (e.rows !== null ? plural(e.rows, 'row') : 'done') : e.status} · {formatDuration(e.duration_ms)}
          </span>
        </button>
      ))}
      <div className={styles.note}>Opening a recent query places it in a new tab. Nothing runs until you press Run.</div>
    </aside>
  );
}
