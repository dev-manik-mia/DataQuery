import { useCallback, useEffect, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { Engine } from '@/bindings/Engine';
import type { MonitorSnapshot } from '@/bindings/MonitorSnapshot';
import type { ObjectSize } from '@/bindings/ObjectSize';
import type { ServerProcess } from '@/bindings/ServerProcess';
import type { ServerSetting } from '@/bindings/ServerSetting';
import { Alert, Button, Card, Checkbox, ErrorAlert, Input, PageHead, Segmented, Spinner } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { formatBytes, formatCount, formatDuration } from '@/lib/format';
import { api } from '@/lib/ipc';
import { objectLabel } from '@/lib/objects';
import { usePageVisible } from '@/lib/visibility';
import { guarded, useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Monitor.module.css';

const REFRESH_MS = 5000;
const LONG_RUNNING_SECS = 30;
const ALERT_RATIO = 0.8;

/** Used / max with a tick at the 80% alert threshold. */
function Gauge({ used, max }: { used: number; max: number }) {
  const ratio = max > 0 ? Math.min(1, used / max) : 0;
  return (
    <div
      className={styles.gauge}
      role="meter"
      aria-valuemin={0}
      aria-valuemax={max}
      aria-valuenow={used}
      aria-label="Connections in use"
    >
      <div className={styles.gaugeFill} style={{ width: `${ratio * 100}%` }} data-alert={ratio >= ALERT_RATIO} />
      <div className={styles.gaugeTick} style={{ left: `${ALERT_RATIO * 100}%` }} aria-hidden />
    </div>
  );
}

function Tile({
  label,
  value,
  help,
  children,
}: {
  label: string;
  value: string;
  help?: string;
  children?: React.ReactNode;
}) {
  return (
    <div className={styles.tile}>
      <span className="dq-eyebrow">{label}</span>
      <span className={styles.tileValue}>{value}</span>
      {children}
      {help && <span className={styles.tileHelp}>{help}</span>}
    </div>
  );
}

type Panel = 'activity' | 'locks' | 'sizes' | 'settings';

/** Server activity, running queries, locks, sizes and settings (screen 07). */
/** Where each engine's process list comes from. */
const PROCESS_SOURCE: Partial<Record<Engine, string>> = {
  postgres: 'Running queries from pg_stat_activity',
  cockroachdb: 'Sessions from crdb_internal.cluster_sessions',
  redshift: 'Sessions from stv_sessions, running statements from stv_recents',
  greenplum: 'Running queries from pg_stat_activity on the coordinator',
  sqlserver: 'Sessions from sys.dm_exec_sessions',
  oracle: 'Sessions from V$SESSION',
  clickhouse: 'Running queries from system.processes',
  vertica: 'Sessions from v_monitor.sessions',
  duckdb: 'The query editor connection (DuckDB runs inside DataQuery)',
  mongodb: 'Operations from currentOp',
  cassandra: 'Client connections from system_views.clients',
  scylladb: 'Client connections from system.clients',
  elasticsearch: 'Running tasks from _tasks',
  neo4j: 'Transactions from SHOW TRANSACTIONS',
};

export function MonitorView({ active: shown }: { active: boolean }) {
  // Poll only while the tab is on screen: not behind another page or in a hidden window.
  const onWorkspace = useUi((s) => s.view.name === 'workspace');
  const visible = usePageVisible();
  const active = shown && onWorkspace && visible;
  const session = useWorkspace((s) => s.session)!;
  const sid = { sessionId: session.session_id };
  const [snapshot, setSnapshot] = useState<MonitorSnapshot | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [updated, setUpdated] = useState<number | null>(null);
  const [auto, setAuto] = useState(true);
  const [panel, setPanel] = useState<Panel>('activity');
  const [idle, setIdle] = useState(false);
  const [sizes, setSizes] = useState<ObjectSize[] | null>(null);
  const [settings, setSettings] = useState<ServerSetting[] | null>(null);
  const [search, setSearch] = useState('');
  const [now, setNow] = useState(Date.now());

  const refresh = useCallback(async () => {
    try {
      setSnapshot(await api.monitorSnapshot(sid, LONG_RUNNING_SECS));
      setError(null);
      setUpdated(Date.now());
    } catch (e) {
      setError(toAppError(e).info);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.session_id]);

  useEffect(() => {
    if (!active) return;
    void refresh();
    if (!auto) return;
    const timer = window.setInterval(() => void refresh(), REFRESH_MS);
    const clock = window.setInterval(() => setNow(Date.now()), 1000);
    return () => {
      window.clearInterval(timer);
      window.clearInterval(clock);
    };
  }, [active, auto, refresh]);

  useEffect(() => {
    if (!active || panel !== 'sizes' || sizes) return;
    api
      .largestTables(sid, session.server.database, 50)
      .then(setSizes)
      .catch((e) => setError(toAppError(e).info));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, panel, sizes]);

  useEffect(() => {
    if (!active || panel !== 'settings') return;
    const timer = window.setTimeout(() => {
      api
        .serverSettings(sid, search.trim() || null)
        .then(setSettings)
        .catch((e) => setError(toAppError(e).info));
    }, 200);
    return () => window.clearTimeout(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, panel, search]);

  const kill = async (p: ServerProcess, terminate: boolean) => {
    try {
      const done = await guarded((c) => api.killProcess(sid, Number(p.pid), p.user, terminate, c));
      if (done === null) return;
      useUi
        .getState()
        .toast(
          done ? `${terminate ? 'Terminated' : 'Cancelled'} pid ${p.pid}.` : `pid ${p.pid} had already finished.`,
          done ? 'positive' : 'info',
        );
      void refresh();
    } catch (e) {
      useUi.getState().toastError(e);
    }
  };

  const s = snapshot;
  const processes = (s?.processes ?? []).filter((p) => idle || p.active);
  const blockers = new Set((s?.processes ?? []).flatMap((p) => p.blocked_by.map(Number)));

  return (
    <div className={styles.page} style={active ? undefined : { display: 'none' }}>
      <div className={styles.inner}>
        <PageHead
          eyebrow={`Diagnostics · ${s?.version ?? session.server.version}`}
          title="Monitoring"
          actions={
            <>
              <span className="ink-3" style={{ fontSize: 12 }}>
                {updated
                  ? `Refreshed ${Math.max(0, Math.round((now - updated) / 1000))} s ago${auto ? ' · every 5 s' : ''}`
                  : 'Loading…'}
              </span>
              <Checkbox label="Auto-refresh" checked={auto} onChange={setAuto} />
              <Button size="sm" onClick={() => void refresh()}>
                Refresh
              </Button>
            </>
          }
        />
        {session.connection.environment === 'Production' && (
          <Alert tone="caution">
            Production: cancel and terminate ask for confirmation and terminating needs the connection name.
          </Alert>
        )}
        {error && <ErrorAlert error={error} />}
        {!s && !error && <Spinner label="Reading server activity" />}
        {s && (
          <>
            <div className={styles.tiles}>
              {s.connections && (
                <Tile
                  label="Connections"
                  value={`${formatCount(s.connections.used)} / ${formatCount(s.connections.max)}`}
                  help="Tick marks the 80% alert threshold."
                >
                  <Gauge used={s.connections.used} max={s.connections.max} />
                </Tile>
              )}
              {s.active_queries !== null && (
                <Tile
                  label="Active queries"
                  value={formatCount(s.active_queries)}
                  help={s.long_running ? `${s.long_running} running longer than ${LONG_RUNNING_SECS} s` : undefined}
                />
              )}
              {s.blocked !== null && (
                <Tile
                  label="Blocked sessions"
                  value={formatCount(s.blocked)}
                  help={blockers.size ? `Waiting on pid ${[...blockers].join(', ')}` : undefined}
                />
              )}
              {s.replication && (
                s.replication.role === 'coordinator' ? (
                  // Greenplum: segment health in place of streaming replication.
                  <Tile label="Segments" value={s.replication.state?.split(' · ')[0] ?? s.replication.role} help={s.replication.state?.split(' · ').slice(1).join(' · ') || undefined} />
                ) : (
                  <Tile
                    label="Replication"
                    value={
                      s.replication.lag_secs !== null ? `${s.replication.lag_secs.toFixed(1)} s` : s.replication.role
                    }
                    help={`${s.replication.role}${s.replication.role === 'primary' ? ` · ${s.replication.replicas} replica${s.replication.replicas === 1 ? '' : 's'}` : ''}${s.replication.state ? ` · ${s.replication.state}` : ''}`}
                  />
                )
              )}
            </div>
            {s.unavailable.length > 0 && (
              <Alert tone="info" title="Some information is not available">
                {s.unavailable.map((u) => (
                  <div key={u}>{u}</div>
                ))}
              </Alert>
            )}
            <Segmented<Panel>
              label="Panel"
              value={panel}
              onChange={setPanel}
              options={[
                { value: 'activity', label: `Running queries (${processes.length})` },
                { value: 'locks', label: `Locks (${s.locks.length})` },
                { value: 'sizes', label: 'Table sizes' },
                { value: 'settings', label: 'Server settings' },
              ]}
            />
            {panel === 'activity' && (
              <Card
                title={PROCESS_SOURCE[session.server.engine] ?? 'Process list'}
                flush
                actions={<Checkbox label="Show idle sessions" checked={idle} onChange={setIdle} />}
                footer="Cancel and terminate open a confirmation naming the pid, user and connection. What is shown depends on server version and the account's privileges."
              >
                <div className={styles.scroll}>
                  <div className={`${styles.row} ${styles.head}`}>
                    <span>pid</span>
                    <span>User</span>
                    <span>State</span>
                    <span>Duration</span>
                    <span>Query</span>
                    <span />
                  </div>
                  {processes.length === 0 && <div className={styles.row}>No {idle ? '' : 'active '}sessions.</div>}
                  {processes.map((p) => (
                    <div
                      key={String(p.pid)}
                      className={styles.row}
                      data-blocked={p.blocked_by.length > 0}
                      data-blocker={blockers.has(Number(p.pid))}
                    >
                      <span className="mono">{String(p.pid)}</span>
                      <span className="ellipsis" title={p.client ?? undefined}>
                        {p.user ?? '—'}
                        {p.database ? <span className="ink-3"> · {p.database}</span> : null}
                      </span>
                      <span className="ellipsis" title={p.wait ?? undefined}>
                        {p.state ?? '—'}
                        {p.blocked_by.length > 0 && (
                          <span className={styles.blocked}> · waits on {p.blocked_by.join(', ')}</span>
                        )}
                      </span>
                      <span className="tabular" data-long={(p.duration_ms ?? 0) > LONG_RUNNING_SECS * 1000 && p.active}>
                        {p.duration_ms !== null ? formatDuration(Number(p.duration_ms)) : ''}
                      </span>
                      <span className="mono ellipsis" title={p.query ?? undefined}>
                        {p.query ?? <span className="ink-3">not visible</span>}
                      </span>
                      <span className={styles.actions}>
                        {p.active && (
                          <Button size="xs" onClick={() => void kill(p, false)}>
                            Cancel…
                          </Button>
                        )}
                        <Button size="xs" variant="danger" onClick={() => void kill(p, true)}>
                          Terminate…
                        </Button>
                      </span>
                    </div>
                  ))}
                </div>
              </Card>
            )}
            {panel === 'locks' && (
              <Card
                title="Locks and blocking"
                flush
                footer="Waiting locks first. Blocked sessions are highlighted in the process list."
              >
                <div className={styles.scroll}>
                  <div className={`${styles.lockRow} ${styles.head}`}>
                    <span>pid</span>
                    <span>Type</span>
                    <span>Mode</span>
                    <span>Object</span>
                    <span>Status</span>
                  </div>
                  {s.locks.length === 0 && <div className={styles.lockRow}>No locks of interest.</div>}
                  {s.locks.map((l, i) => (
                    <div key={i} className={styles.lockRow}>
                      <span className="mono">{String(l.pid)}</span>
                      <span>{l.lock_type}</span>
                      <span className="mono">{l.mode}</span>
                      <span className="mono ellipsis">{l.object ?? ''}</span>
                      <span style={{ color: l.granted ? undefined : 'var(--caution)' }}>
                        {l.granted ? 'granted' : 'waiting'}
                      </span>
                    </div>
                  ))}
                </div>
              </Card>
            )}
            {panel === 'sizes' && (
              <Card title={`Largest tables in ${session.server.database}`} flush>
                {!sizes && (
                  <div className={styles.row}>
                    <Spinner />
                  </div>
                )}
                {sizes?.map((t) => (
                  <div key={objectLabel(t.object)} className={styles.sizeRow}>
                    <span className="mono ellipsis">{objectLabel(t.object)}</span>
                    <span className="tabular">{formatBytes(t.total_bytes)}</span>
                    <span className="tabular ink-2">{formatBytes(t.table_bytes)} data</span>
                    <span className="tabular ink-2">{formatBytes(t.index_bytes)} indexes</span>
                    <span className="tabular ink-2">{t.rows !== null ? `≈${formatCount(t.rows)} rows` : ''}</span>
                  </div>
                ))}
              </Card>
            )}
            {panel === 'settings' && (
              <Card
                title="Server settings"
                flush
                actions={
                  <Input
                    label="Search settings"
                    placeholder="Search"
                    value={search}
                    onChange={(e) => setSearch(e.target.value)}
                  />
                }
              >
                {!settings && (
                  <div className={styles.row}>
                    <Spinner />
                  </div>
                )}
                {settings?.map((st) => (
                  <div key={st.name} className={styles.settingRow} title={st.description ?? undefined}>
                    <span className="mono ellipsis">{st.name}</span>
                    <span className="mono ellipsis">
                      {st.value}
                      {st.unit ? <span className="ink-3"> {st.unit}</span> : null}
                    </span>
                    <span className="ink-3 ellipsis">{st.category ?? ''}</span>
                  </div>
                ))}
              </Card>
            )}
          </>
        )}
      </div>
    </div>
  );
}
