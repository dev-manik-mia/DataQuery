import { useCallback, useEffect, useMemo, useState } from 'react';

import type { HistoryEntry } from '@/bindings/HistoryEntry';
import type { SavedScript } from '@/bindings/SavedScript';
import { Button, Checkbox, Chip, EmptyState, ErrorAlert, Eyebrow, Icon, Input, PromptDialog, Segmented, type ChipTone } from '@/components/ui';
import { SqlViewer } from '@/features/editor/SqlViewer';
import { copyText } from '@/lib/clipboard';
import { flavorOf } from '@/lib/engines';
import { formatCount, formatDuration, formatRelative, formatTimestamp, plural } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './History.module.css';
import { SnippetHelp, SnippetsPanel } from './Snippets';

type Mode = 'history' | 'scripts' | 'snippets';
type Range = 'all' | 'today' | 'week' | 'month';

const DAY = 86_400_000;

function rangeStart(range: Range): number | null {
  const now = Date.now();
  if (range === 'all') return null;
  if (range === 'today') return new Date(new Date().toDateString()).getTime();
  return now - (range === 'week' ? 7 : 30) * DAY;
}
const PAGE = 100;
const statusTone: Record<HistoryEntry['status'], ChipTone> = { ok: 'positive', error: 'critical', cancelled: 'caution' };

/** Opens SQL in a tab on its original connection. It never runs automatically. */
async function openInTab(sql: string, connectionId: string | null, title?: string, scriptId?: string) {
  const ws = useWorkspace.getState();
  const target = connectionId ?? ws.session?.connection.id ?? null;
  if (target && ws.session?.connection.id !== target) {
    if (!(await ws.connect(target))) {
      useUi.getState().toast('Could not connect to the original connection. Open it from Connections.', 'critical');
      return;
    }
  }
  if (!useWorkspace.getState().session) {
    useUi.getState().toast('Connect to a database first.', 'info');
    return;
  }
  useWorkspace.getState().openQueryTab({ sql, title, scriptId });
  useUi.getState().navigate({ name: 'workspace' });
}

export function HistoryPage() {
  const [mode, setMode] = useState<Mode>('history');
  const [search, setSearch] = useState('');
  const [entries, setEntries] = useState<HistoryEntry[]>([]);
  const [total, setTotal] = useState(0);
  const [scripts, setScripts] = useState<SavedScript[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [connectionId, setConnectionId] = useState('');
  const [status, setStatus] = useState<HistoryEntry['status'] | ''>('');
  const [range, setRange] = useState<Range>('all');
  const [folder, setFolder] = useState('');
  const [tag, setTag] = useState('');
  const connections = useConnections((s) => s.connections);
  const { ask, toast, toastError } = useUi();

  const load = useCallback(
    async (before: number | null) => {
      setLoading(true);
      try {
        const page = await api.listHistory({
          search: search || null,
          connection_id: connectionId || null,
          status: status || null,
          from_ms: rangeStart(range),
          to_ms: null,
          before_id: before,
          limit: PAGE,
        });
        setEntries((prev) => (before ? [...prev, ...page.entries] : page.entries));
        setTotal(page.total);
      } catch (e) {
        toastError(e);
      } finally {
        setLoading(false);
      }
    },
    [search, connectionId, status, range, toastError],
  );

  useEffect(() => {
    const t = window.setTimeout(() => void load(null), 150);
    return () => window.clearTimeout(t);
  }, [load]);

  const loadScripts = useCallback(() => api.listScripts().then(setScripts).catch(toastError), [toastError]);
  useEffect(() => void loadScripts(), [loadScripts]);

  const filteredScripts = useMemo(() => {
    const q = search.trim().toLowerCase();
    return scripts.filter(
      (s) =>
        (!q || s.name.toLowerCase().includes(q) || s.sql.toLowerCase().includes(q) || s.tags.some((t) => t.toLowerCase().includes(q))) &&
        (!folder || (folder === '\u0000' ? !s.folder : s.folder === folder)) &&
        (!tag || s.tags.includes(tag)),
    );
  }, [scripts, search, folder, tag]);
  const folders = useMemo(() => [...new Set(scripts.flatMap((s) => (s.folder ? [s.folder] : [])))].sort(), [scripts]);
  const tags = useMemo(() => [...new Set(scripts.flatMap((s) => s.tags))].sort(), [scripts]);

  const entry = mode === 'history' ? entries.find((e) => `h${e.id}` === selected) : undefined;
  const script = mode === 'scripts' ? filteredScripts.find((s) => `s${s.id}` === selected) : undefined;

  return (
    <div className={styles.page}>
      <section className={styles.listPane} aria-label={mode === 'history' ? 'Query history' : 'Saved scripts'}>
        <div className={styles.listHead}>
          <div className={styles.headRow}>
            <Segmented<Mode>
              label="Show"
              value={mode}
              onChange={(m) => {
                setMode(m);
                setSelected(null);
              }}
              options={[
                { value: 'history', label: 'History' },
                { value: 'scripts', label: 'Saved scripts', count: formatCount(scripts.length) },
                { value: 'snippets', label: 'Snippets' },
              ]}
            />
            <span className={styles.meta}>
              {mode === 'history' ? `Stored locally · ${plural(total, 'entry', 'entries')}` : 'Stored locally'}
              {mode === 'history' && total > 0 && (
                <Button
                  size="xs"
                  variant="plain"
                  onClick={async () => {
                    const name = connections.find((c) => c.id === connectionId)?.name;
                    if (
                      await ask({
                        title: name ? `Clear history of ${name}?` : 'Clear query history?',
                        body: name ? `History entries of ${name} on this computer are deleted.` : 'All history entries on this computer are deleted. Saved scripts are kept.',
                        confirmLabel: 'Clear history',
                        danger: true,
                      })
                    ) {
                      await api.clearHistory(connectionId || null).catch(toastError);
                      setSelected(null);
                      void load(null);
                    }
                  }}
                >
                  Clear
                </Button>
              )}
            </span>
          </div>
          {mode !== 'snippets' && (
            <Input label="Search" small={false} mono placeholder={mode === 'history' ? 'Search SQL or connection' : 'Search scripts and tags'} value={search} onChange={(e) => setSearch(e.target.value)} />
          )}
          {mode === 'history' && (
            <div className={styles.filters}>
              <select className="dq-input dq-input--sm" aria-label="Connection" value={connectionId} onChange={(e) => setConnectionId(e.target.value)}>
                <option value="">All connections</option>
                {connections.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </select>
              <select className="dq-input dq-input--sm" aria-label="Status" value={status} onChange={(e) => setStatus(e.target.value as HistoryEntry['status'] | '')}>
                <option value="">Any status</option>
                <option value="ok">OK</option>
                <option value="error">Error</option>
                <option value="cancelled">Cancelled</option>
              </select>
              <select className="dq-input dq-input--sm" aria-label="Date" value={range} onChange={(e) => setRange(e.target.value as Range)}>
                <option value="all">Any time</option>
                <option value="today">Today</option>
                <option value="week">Last 7 days</option>
                <option value="month">Last 30 days</option>
              </select>
            </div>
          )}
          {mode === 'scripts' && (folders.length > 0 || tags.length > 0) && (
            <div className={styles.filters}>
              <select className="dq-input dq-input--sm" aria-label="Folder" value={folder} onChange={(e) => setFolder(e.target.value)}>
                <option value="">All folders</option>
                <option value={'\u0000'}>No folder</option>
                {folders.map((f) => (
                  <option key={f} value={f}>
                    {f}
                  </option>
                ))}
              </select>
              <select className="dq-input dq-input--sm" aria-label="Tag" value={tag} onChange={(e) => setTag(e.target.value)}>
                <option value="">Any tag</option>
                {tags.map((t) => (
                  <option key={t} value={t}>
                    #{t}
                  </option>
                ))}
              </select>
            </div>
          )}
        </div>
        <div className={styles.list} role="listbox" aria-label="Entries">
          {mode === 'history' &&
            entries.map((e) => (
              <button key={e.id} type="button" role="option" aria-selected={selected === `h${e.id}`} className={styles.entry} onClick={() => setSelected(`h${e.id}`)}>
                <span className={styles.entryHead}>
                  <Chip tone={statusTone[e.status]} small>
                    {e.status === 'ok' ? 'OK' : e.status}
                  </Chip>
                  <span className={styles.entryConn}>{e.connection_name}</span>
                  <span className={styles.entryWhen}>{formatRelative(e.executed_at)}</span>
                  <span className={styles.entryDuration}>{formatDuration(e.duration_ms)}</span>
                </span>
                <span className={styles.entrySql}>{e.sql}</span>
              </button>
            ))}
          {mode === 'history' && entries.length < total && (
            <div className={styles.more}>
              <Button size="sm" busy={loading} onClick={() => void load(entries[entries.length - 1]?.id ?? null)}>
                Load older entries
              </Button>
            </div>
          )}
          {mode === 'history' && !loading && entries.length === 0 && <EmptyState title={search ? 'No matches' : 'No history yet'}>Statements you run are recorded here. Result rows are never stored.</EmptyState>}
          {mode === 'scripts' &&
            filteredScripts.map((s) => (
              <button key={s.id} type="button" role="option" aria-selected={selected === `s${s.id}`} className={styles.entry} onClick={() => setSelected(`s${s.id}`)}>
                <span className={styles.entryHead}>
                  {s.pinned && <Icon name="pin" size={11} title="Pinned" />}
                  <span className={styles.entryConn}>{s.name}</span>
                  {s.folder && <Chip tone="mute" small>{s.folder}</Chip>}
                  {s.tags.map((t) => (
                    <span key={t} className="ink-3" style={{ fontSize: 11 }}>
                      #{t}
                    </span>
                  ))}
                  <span className={styles.entryWhen}>updated {formatRelative(s.updated_at)}</span>
                </span>
                <span className={styles.entrySql}>{s.sql}</span>
              </button>
            ))}
          {mode === 'scripts' && filteredScripts.length === 0 && <EmptyState title="No saved scripts">Use File → Save as script in a query tab.</EmptyState>}
          {mode === 'snippets' && <SnippetsPanel />}
        </div>
      </section>
      <section className={styles.detail} aria-label="Details">
        {entry && <HistoryDetail entry={entry} onSaved={() => void loadScripts()} />}
        {script && (
          <ScriptDetail
            script={script}
            folders={folders}
            onChanged={() => void loadScripts()}
            onDelete={async () => {
              if (await ask({ title: `Delete ${script.name}?`, body: 'The saved script is removed from this computer.', confirmLabel: 'Delete', danger: true })) {
                await api.deleteScript(script.id).catch(toastError);
                setSelected(null);
                void loadScripts();
              }
            }}
            onCopy={() => void copyText(script.sql).then(() => toast('SQL copied'))}
          />
        )}
        {!entry && !script && mode !== 'snippets' && <EmptyState title="Select an entry">Details and actions appear here.</EmptyState>}
        {mode === 'snippets' && <SnippetHelp />}
      </section>
    </div>
  );
}

function HistoryDetail({ entry, onSaved }: { entry: HistoryEntry; onSaved: () => void }) {
  const toast = useUi((s) => s.toast);
  const connections = useConnections((s) => s.connections);
  const flavor = flavorOf(connections.find((c) => c.id === entry.connection_id)?.config.engine);
  const [naming, setNaming] = useState(false);
  const title =
    entry.status === 'ok'
      ? `${entry.rows !== null ? plural(entry.rows, 'row') : 'Done'} in ${formatDuration(entry.duration_ms)}`
      : entry.status === 'cancelled'
        ? `Cancelled after ${formatDuration(entry.duration_ms)}`
        : `Failed after ${formatDuration(entry.duration_ms)}`;
  return (
    <>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
        <Eyebrow>
          {entry.connection_name} · {entry.database} · {formatTimestamp(entry.executed_at)}
        </Eyebrow>
        <h2 className={styles.detailTitle}>{title}</h2>
      </div>
      {entry.error && <ErrorAlert error={{ kind: 'server', code: null, message: entry.error, detail: null, hint: null, action: null, position: null }} />}
      <div className={styles.sql}>
        <SqlViewer value={entry.sql} flavor={flavor} label="Statement" />
      </div>
      <div className={styles.buttons}>
        <Button variant="primary" onClick={() => void openInTab(entry.sql, entry.connection_id)}>
          Open in new tab
        </Button>
        <Button onClick={() => setNaming(true)}>Save as script</Button>
        <Button onClick={() => void copyText(entry.sql).then(() => toast('SQL copied'))}>Copy</Button>
      </div>
      <p className={styles.note}>Opening places the SQL in a tab on the original connection. It runs only when you press Run. Result rows are not stored in history.</p>
      {naming && (
        <PromptDialog
          title="Save as script"
          label="Script name"
          confirmLabel="Save script"
          onClose={() => setNaming(false)}
          onSubmit={async (name) => {
            await api.saveScript({ id: null, name, sql: entry.sql, connection_id: entry.connection_id, folder: null, tags: [], pinned: false });
            toast(`Saved script ${name}`, 'positive');
            onSaved();
          }}
        />
      )}
    </>
  );
}

function ScriptDetail({ script, folders, onChanged, onDelete, onCopy }: { script: SavedScript; folders: string[]; onChanged: () => void; onDelete: () => void; onCopy: () => void }) {
  const session = useWorkspace((s) => s.session);
  const toastError = useUi((s) => s.toastError);
  const [folder, setFolder] = useState(script.folder ?? '');
  const [tags, setTags] = useState(script.tags.join(', '));
  useEffect(() => {
    setFolder(script.folder ?? '');
    setTags(script.tags.join(', '));
  }, [script]);
  const saveMeta = async (patch: Partial<SavedScript>) => {
    try {
      await api.saveScript({
        id: script.id,
        name: script.name,
        sql: script.sql,
        connection_id: script.connection_id,
        folder: (patch.folder !== undefined ? patch.folder : script.folder) || null,
        tags: patch.tags ?? script.tags,
        pinned: patch.pinned ?? script.pinned,
      });
      onChanged();
    } catch (e) {
      toastError(e);
    }
  };
  return (
    <>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
        <Eyebrow>Saved script · updated {formatTimestamp(script.updated_at)}</Eyebrow>
        <h2 className={styles.detailTitle}>{script.name}</h2>
      </div>
      <div className={styles.metaFields}>
        <label className="dq-field__label" htmlFor="dq-script-folder">
          Folder
        </label>
        <input
          id="dq-script-folder"
          className="dq-input dq-input--sm"
          list="dq-script-folders"
          placeholder="No folder"
          value={folder}
          onChange={(e) => setFolder(e.target.value)}
          onBlur={() => folder !== (script.folder ?? '') && void saveMeta({ folder })}
        />
        <datalist id="dq-script-folders">
          {folders.map((f) => (
            <option key={f} value={f} />
          ))}
        </datalist>
        <label className="dq-field__label" htmlFor="dq-script-tags">
          Tags
        </label>
        <input
          id="dq-script-tags"
          className="dq-input dq-input--sm"
          placeholder="reports, billing"
          value={tags}
          onChange={(e) => setTags(e.target.value)}
          onBlur={() => {
            const next = tags.split(',').map((t) => t.trim()).filter(Boolean);
            if (next.join() !== script.tags.join()) void saveMeta({ tags: next });
          }}
        />
        <Checkbox label="Pinned to the top" checked={script.pinned} onChange={(pinned) => void saveMeta({ pinned })} />
      </div>
      <div className={styles.sql}>
        <SqlViewer value={script.sql} flavor={session?.flavor ?? 'postgres'} label="Script" />
      </div>
      <div className={styles.buttons}>
        <Button variant="primary" onClick={() => void openInTab(script.sql, script.connection_id, script.name, script.id)}>
          Open in new tab
        </Button>
        <Button onClick={onCopy}>Copy</Button>
        <Button variant="plain" onClick={onDelete}>
          Delete
        </Button>
      </div>
      <p className={styles.note}>Scripts open in a tab and run only when you press Run. With autosave on (Settings → History), edits in that tab are saved back to the script.</p>
    </>
  );
}
