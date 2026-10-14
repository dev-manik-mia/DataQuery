import { useEffect, useMemo, useState } from 'react';

import { Button, Icon, Menu, Skeleton, Spinner, Tabs, type MenuEntry, type TabItem } from '@/components/ui';
import { DataGrid, type GridColumn, type GridRow } from '@/features/grid/DataGrid';
import { exportData } from '@/features/transfer/exportData';
import { copyText, serializeRows, type CopyFormat } from '@/lib/clipboard';
import { formatCount, formatDuration, formatTimer } from '@/lib/format';
import { api } from '@/lib/ipc';
import { keyBadges } from '@/lib/objects';
import { useSettings } from '@/stores/settings';
import { useUi } from '@/stores/ui';
import { useWorkspace, type QueryTab, type ResultSet, type ResultsTab, type RunMessage } from '@/stores/workspace';

import { PlanView } from './PlanView';
import styles from './Results.module.css';

/** Result tabs, Messages, bounded-fetch banner and running state for a query tab. */
export function ResultsPanel({ tab }: { tab: QueryTab }) {
  const { run } = tab;
  const setActive = useWorkspace((s) => s.setActiveResult);
  const fetchMore = useWorkspace((s) => s.fetchMore);
  const rowLimit = useSettings((s) => s.settings.row_limit);
  const result = typeof run.active === 'number' ? run.results[run.active] : undefined;
  const hasError = run.messages.some((m) => m.kind === 'error');

  const items: TabItem<ResultsTab>[] = [
    ...run.results.map((r, i) => ({ id: i, label: `Result ${i + 1}`, count: `${formatCount(r.rows.length)}${r.hasMore ? '+' : ''}` })),
    ...(run.plan ? [{ id: 'plan' as const, label: 'Plan' }] : []),
    { id: 'messages', label: 'Messages', critical: hasError, count: run.messages.length ? run.messages.length : undefined },
  ];

  return (
    <section className={styles.panel} aria-label="Results">
      <Tabs label="Results" items={items} value={run.active} onChange={(id) => setActive(tab.id, id)} trailing={result && <ResultActions result={result} index={run.active as number} />} />
      {run.status === 'running' && <RunningBar startedAt={run.startedAt} />}
      {run.status === 'running' && !result ? (
        <RunningSkeleton />
      ) : run.active === 'plan' && run.plan ? (
        <PlanView plan={run.plan} />
      ) : run.active === 'messages' || run.active === 'plan' ? (
        <MessagesView messages={run.messages} idle={run.status === 'idle'} />
      ) : result ? (
        <ResultGrid result={result} />
      ) : (
        <div className={styles.idle}>Run a statement to see results.</div>
      )}
      {result?.hasMore && typeof run.active === 'number' && (
        <div className={styles.banner} role="status">
          <span>
            Showing first {formatCount(result.rows.length)} rows.{' '}
            {result.canFetchMore ? 'More rows are available.' : 'Run the query again to read further — a newer statement closed this result.'}
          </span>
          {result.canFetchMore && (
            <Button size="xs" variant="caution" busy={run.fetching} onClick={() => void fetchMore(tab.id)}>
              Fetch next {formatCount(rowLimit)}
            </Button>
          )}
        </div>
      )}
    </section>
  );
}

function ResultGrid({ result }: { result: ResultSet }) {
  const badges = useResultKeyBadges(result.sql);
  const columns = useMemo<GridColumn[]>(
    () => result.columns.map((c) => ({ name: c.name, typeName: c.type_name, numeric: c.numeric, badge: badges.get(c.name) })),
    [result.columns, badges],
  );
  const rows = useMemo<GridRow[]>(() => result.rows.map((cells, i) => ({ key: String(i), cells })), [result.rows]);
  return <DataGrid label="Query result" columns={columns} rows={rows} emptyText="The statement returned no rows." />;
}

function ResultActions({ result, index }: { result: ResultSet; index: number }) {
  const toast = useUi((s) => s.toast);
  const [menu, setMenu] = useState<{ x: number; y: number; items: MenuEntry[] } | null>(null);
  const headers = result.columns.map((c) => c.name);

  const copyAll = (format: CopyFormat) =>
    void copyText(serializeRows(headers, result.rows, format)).then(() => toast(`${formatCount(result.rows.length)} rows copied as ${format.toUpperCase()}`));

  const open = (e: React.MouseEvent, items: MenuEntry[]) => {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    setMenu({ x: rect.left, y: rect.bottom + 4, items });
  };

  const base = `result-${index + 1}`;
  return (
    <div className={styles.tabActions}>
      <Button
        size="xs"
        onClick={(e) =>
          open(e, [
            { label: 'Copy as TSV', onSelect: () => copyAll('tsv') },
            { label: 'Copy as CSV', onSelect: () => copyAll('csv') },
            { label: 'Copy as JSON', onSelect: () => copyAll('json') },
          ])
        }
      >
        Copy
      </Button>
      <Button
        size="xs"
        title="Re-runs the statement on a read-only connection and streams every row to disk"
        onClick={() => exportData({ type: 'query', sql: result.sql }, 'csv', base)}
      >
        Export…
      </Button>
      {menu && <Menu x={menu.x} y={menu.y} items={menu.items} onClose={() => setMenu(null)} />}
    </div>
  );
}

function RunningBar({ startedAt }: { startedAt: number | null }) {
  const cancel = useWorkspace((s) => s.cancel);
  const [now, setNow] = useState(Date.now());
  useEffect(() => {
    const t = window.setInterval(() => setNow(Date.now()), 100);
    return () => window.clearInterval(t);
  }, []);
  return (
    <div className={styles.running} role="status">
      <Spinner />
      <span>
        Running · <span className="mono">{formatTimer(Math.max(0, now - (startedAt ?? now)))}</span>
      </span>
      <span style={{ flex: 1 }} />
      <Button size="sm" onClick={() => void cancel()}>
        Cancel query
      </Button>
    </div>
  );
}

function RunningSkeleton() {
  return (
    <div className={styles.skeleton} aria-hidden>
      {['80%', '62%', '90%', '70%', '55%', '84%', '66%'].map((w, i) => (
        <div key={i} className={styles.skeletonRow}>
          <Skeleton />
          <Skeleton />
          <Skeleton width={w} />
          <Skeleton />
        </div>
      ))}
      <div className={styles.hint}>The editor and explorer stay usable while the query runs.</div>
    </div>
  );
}

const messageIcon = {
  ok: <Icon name="check" style={{ color: 'var(--positive)' }} />,
  error: <Icon name="warning" style={{ color: 'var(--critical)' }} />,
  notice: <Icon name="more" style={{ color: 'var(--ink-3)' }} />,
  info: <Icon name="more" style={{ color: 'var(--ink-3)' }} />,
};

function MessagesView({ messages, idle }: { messages: RunMessage[]; idle: boolean }) {
  if (!messages.length) return <div className={styles.idle}>{idle ? 'Run a statement to see messages.' : 'No messages.'}</div>;
  return (
    <div className={styles.messages}>
      {messages.map((m) => (
        <div key={m.id} className={styles.message}>
          <span className={styles.messageIcon}>{messageIcon[m.kind]}</span>
          <div className={styles.messageBody}>
            {m.error ? (
              <>
                <div className={styles.messageError}>{m.error.message}</div>
                <div className={styles.messageMeta}>
                  {m.error.code ? `ERROR ${m.error.code}` : m.error.kind.toUpperCase()}
                  {m.statement !== null ? ` · statement ${m.statement + 1}` : ''}
                  {m.error.position !== null ? ` · position ${m.error.position}` : ''}
                </div>
                {m.error.detail && <div className={styles.messageNote}>{m.error.detail}</div>}
                {m.error.hint && <div className={styles.messageNote}>Hint from server: {m.error.hint}</div>}
                {m.error.action && <div className={styles.messageNote}>{m.error.action}</div>}
                <div className={styles.messageNote}>Previous result tabs are kept.</div>
              </>
            ) : (
              <span className={m.kind === 'notice' ? 'ink-2' : undefined}>{m.text}</span>
            )}
          </div>
          {m.durationMs !== undefined && <span className={styles.duration}>{formatDuration(m.durationMs)}</span>}
        </div>
      ))}
    </div>
  );
}

const NO_BADGES = new Map<string, string>();

/** CQL: `PK` / `CK` markers for the columns of the table a SELECT read. */
function useResultKeyBadges(sql: string): Map<string, string> {
  const session = useWorkspace((s) => s.session);
  const [badges, setBadges] = useState(NO_BADGES);
  useEffect(() => {
    if (session?.flavor !== 'cql') return;
    let stale = false;
    api
      .queryAnalysis({ sessionId: session.session_id }, sql)
      .then((a) => !stale && setBadges(a ? keyBadges(a.keys) : NO_BADGES))
      .catch(() => undefined);
    return () => {
      stale = true;
    };
  }, [session?.flavor, session?.session_id, sql]);
  return badges;
}
