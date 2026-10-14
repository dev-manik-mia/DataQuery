import { useCallback, useEffect, useMemo, useState } from 'react';

import type { HistoryEntry } from '@/bindings/HistoryEntry';
import type { SavedScript } from '@/bindings/SavedScript';
import { Button, Chip, EmptyState, ErrorAlert, Eyebrow, Input, PromptDialog, Segmented, type ChipTone } from '@/components/ui';
import { SqlViewer } from '@/features/editor/SqlViewer';
import { copyText } from '@/lib/clipboard';
import { flavorOf } from '@/lib/engines';
import { formatCount, formatDuration, formatRelative, formatTimestamp, plural } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './History.module.css';

type Mode = 'history' | 'scripts';
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
  const { ask, toast, toastError } = useUi();

  const load = useCallback(
    async (before: number | null) => {
      setLoading(true);
      try {
        const page = await api.listHistory({ search: search || null, connection_id: null, before_id: before, limit: PAGE });
        setEntries((prev) => (before ? [...prev, ...page.entries] : page.entries));
        setTotal(page.total);
      } catch (e) {
        toastError(e);
      } finally {
        setLoading(false);
      }
    },
    [search, toastError],
  );

  useEffect(() => {
    const t = window.setTimeout(() => void load(null), 150);
    return () => window.clearTimeout(t);
  }, [load]);

  const loadScripts = useCallback(() => api.listScripts().then(setScripts).catch(toastError), [toastError]);
  useEffect(() => void loadScripts(), [loadScripts]);

  const filteredScripts = useMemo(() => {
    const q = search.trim().toLowerCase();
    return scripts.filter((s) => !q || s.name.toLowerCase().includes(q) || s.sql.toLowerCase().includes(q));
  }, [scripts, search]);

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
              ]}
            />
            <span className={styles.meta}>
              {mode === 'history' ? `Stored locally · ${plural(total, 'entry', 'entries')}` : 'Stored locally'}
              {mode === 'history' && total > 0 && (
                <Button
                  size="xs"
                  variant="plain"
                  onClick={async () => {
                    if (await ask({ title: 'Clear query history?', body: 'All history entries on this computer are deleted. Saved scripts are kept.', confirmLabel: 'Clear history', danger: true })) {
                      await api.clearHistory().catch(toastError);
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
          <Input label="Search" small={false} mono placeholder={mode === 'history' ? 'Search SQL or connection' : 'Search scripts'} value={search} onChange={(e) => setSearch(e.target.value)} />
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
                  <span className={styles.entryConn}>{s.name}</span>
                  <span className={styles.entryWhen}>updated {formatRelative(s.updated_at)}</span>
                </span>
                <span className={styles.entrySql}>{s.sql}</span>
              </button>
            ))}
          {mode === 'scripts' && filteredScripts.length === 0 && <EmptyState title="No saved scripts">Use File → Save as script in a query tab.</EmptyState>}
        </div>
      </section>
      <section className={styles.detail} aria-label="Details">
        {entry && <HistoryDetail entry={entry} onSaved={() => void loadScripts()} />}
        {script && (
          <ScriptDetail
            script={script}
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
        {!entry && !script && <EmptyState title="Select an entry">Details and actions appear here.</EmptyState>}
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
            await api.saveScript({ id: null, name, sql: entry.sql, connection_id: entry.connection_id });
            toast(`Saved script ${name}`, 'positive');
            onSaved();
          }}
        />
      )}
    </>
  );
}

function ScriptDetail({ script, onDelete, onCopy }: { script: SavedScript; onDelete: () => void; onCopy: () => void }) {
  const session = useWorkspace((s) => s.session);
  return (
    <>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 4 }}>
        <Eyebrow>Saved script · updated {formatTimestamp(script.updated_at)}</Eyebrow>
        <h2 className={styles.detailTitle}>{script.name}</h2>
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
      <p className={styles.note}>Scripts open in a tab and run only when you press Run.</p>
    </>
  );
}
