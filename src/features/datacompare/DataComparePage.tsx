import { save } from '@tauri-apps/plugin-dialog';
import { Fragment, useEffect, useState } from 'react';

import type { CompareTable } from '@/bindings/CompareTable';
import type { ComparePlan } from '@/bindings/ComparePlan';
import type { DataCompareResult } from '@/bindings/DataCompareResult';
import type { DbError } from '@/bindings/DbError';
import type { RowDiff } from '@/bindings/RowDiff';
import type { SyncKinds } from '@/bindings/SyncKinds';
import { Alert, Button, Card, Checkbox, Chip, EmptyState, ErrorAlert, PageHead, TextField } from '@/components/ui';
import { SqlViewer } from '@/features/editor/SqlViewer';
import { CellView } from '@/features/grid/CellView';
import { SidePicker } from '@/features/schema/SchemaDiffPage';
import schemaStyles from '@/features/schema/Schema.module.css';
import { describeConditions, FilterBuilder } from '@/features/table/FilterBuilder';
import { useTransferProgress } from '@/features/transfer/progress';
import { copyText } from '@/lib/clipboard';
import { flavorOf } from '@/lib/engines';
import { toAppError } from '@/lib/errors';
import { formatCount, formatDuration, plural } from '@/lib/format';
import { uid } from '@/lib/id';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';
import { guarded, useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { compareOptions, DEFAULT_CHOICES, differences, fitChoices, syncRows, type CompareChoices } from './compare';
import styles from './DataCompare.module.css';

const EMPTY: CompareTable = { connection_id: '', database: '', schema: null, table: '' };
const ALL_KINDS: SyncKinds = { insert: true, update: true, delete: true };
const PAGE_ROWS = 50;

/**
 * Compares the rows of two tables (possibly on different connections and
 * engines) and builds the SQL that makes the target match the source.
 */
export function DataComparePage({ initialSource }: { initialSource?: CompareTable }) {
  const session = useWorkspace((s) => s.session);
  const connections = useConnections((s) => s.connections);
  const [source, setSource] = useState<CompareTable>(initialSource ?? EMPTY);
  const [target, setTarget] = useState<CompareTable>(EMPTY);
  const [plan, setPlan] = useState<ComparePlan | null>(null);
  const [choices, setChoices] = useState<CompareChoices>(DEFAULT_CHOICES);
  const [filtering, setFiltering] = useState(false);
  const [result, setResult] = useState<DataCompareResult | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [busy, setBusy] = useState<'plan' | 'compare' | 'apply' | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);
  const progress = useTransferProgress(jobId);
  const targetConnection = connections.find((c) => c.id === target.connection_id);

  useEffect(() => {
    if (session && !source.connection_id) {
      const c = session.connection;
      setSource((s) => ({ ...s, connection_id: c.id, database: session.server.database, schema: session.capabilities.schemas ? 'public' : null }));
    }
  }, [session, source.connection_id]);

  // The backend keeps a finished compare (its change journal) until it is replaced or the page closes.
  useEffect(() => {
    const id = result?.compare_id;
    return () => {
      if (id) void api.dataCompareDiscard(id).catch(() => undefined);
    };
  }, [result?.compare_id]);

  const changeSide = (set: (t: CompareTable) => void) => (value: CompareTable) => {
    set(value);
    setPlan(null);
    setResult(null);
  };

  const ready = Boolean(source.connection_id && target.connection_id && source.table.trim() && target.table.trim());

  const run = async <T,>(kind: 'plan' | 'compare' | 'apply', work: () => Promise<T>): Promise<T | null> => {
    setBusy(kind);
    setError(null);
    try {
      return await work();
    } catch (e) {
      setError(toAppError(e).info);
      return null;
    } finally {
      setBusy(null);
    }
  };

  const loadPlan = () =>
    run('plan', async () => {
      const loaded = await api.dataComparePlan(source, target, null);
      setPlan(loaded);
      setChoices((c) => fitChoices(c, loaded));
    });

  const compare = () =>
    run('compare', async () => {
      const id = uid('compare');
      setJobId(id);
      try {
        const done = await api.dataCompare(source, target, compareOptions(choices), id);
        setResult(done);
        setPlan(done.plan);
        return done;
      } finally {
        setJobId(null);
      }
    });

  return (
    <div className={schemaStyles.page}>
      <div className={schemaStyles.inner}>
        <PageHead eyebrow="Data" title="Data compare" />
        <p className="ink-2">
          Compares the rows of two tables by their key and builds the SQL that makes the target match the source. Both sides stream in key order, so large tables compare in bounded memory.
        </p>
        <div className={schemaStyles.sides}>
          <SidePicker label="Source (desired)" value={source} onChange={changeSide(setSource)}>
            <TextField label="Table" mono value={source.table} onChange={(e) => changeSide(setSource)({ ...source, table: e.target.value })} />
          </SidePicker>
          <Button
            onClick={() => {
              changeSide(setSource)(target);
              setTarget(source);
            }}
            aria-label="Swap source and target"
          >
            ⇄
          </Button>
          <SidePicker label="Target (to change)" value={target} onChange={changeSide(setTarget)}>
            <TextField label="Table" mono value={target.table} onChange={(e) => changeSide(setTarget)({ ...target, table: e.target.value })} />
          </SidePicker>
        </div>

        {plan ? (
          <OptionsCard plan={plan} choices={choices} onChange={setChoices} filtering={filtering} onFiltering={setFiltering} />
        ) : (
          <div>
            <Button busy={busy === 'plan'} disabled={!ready || busy != null} onClick={() => void loadPlan()}>
              Choose columns and options…
            </Button>
          </div>
        )}

        <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
          <Button variant="primary" busy={busy === 'compare'} disabled={!ready || (busy != null && busy !== 'compare')} onClick={() => void compare()}>
            Compare
          </Button>
          {jobId && (
            <>
              <span className="ink-2">{progress ? `${formatCount(progress.rows)} rows read…` : 'Starting…'}</span>
              <Button size="sm" onClick={() => void api.cancelTransfer(jobId)}>
                Cancel
              </Button>
            </>
          )}
        </div>
        {error && <ErrorAlert error={error} />}
        {result && (
          <Results
            key={result.compare_id}
            result={result}
            target={target}
            targetName={targetConnection?.name ?? 'the target'}
            readOnly={Boolean(targetConnection?.config.read_only)}
            flavor={targetConnection ? flavorOf(targetConnection.config.engine) : 'postgres'}
            onApplied={() => void compare()}
          />
        )}
      </div>
    </div>
  );
}

/** Key and compared columns, row filter and comparison options. */
function OptionsCard({
  plan,
  choices,
  onChange,
  filtering,
  onFiltering,
}: {
  plan: ComparePlan;
  choices: CompareChoices;
  onChange: (c: CompareChoices) => void;
  filtering: boolean;
  onFiltering: (open: boolean) => void;
}) {
  const names = plan.columns.map((c) => c.name);
  const compared = (name: string) => choices.keys.includes(name) || choices.columns == null || choices.columns.includes(name);
  const toggleKey = (name: string, on: boolean) => onChange({ ...choices, keys: on ? [...choices.keys, name] : choices.keys.filter((k) => k !== name) });
  const toggleColumn = (name: string, on: boolean) => {
    const current = choices.columns ?? names.filter((n) => !choices.keys.includes(n));
    onChange({ ...choices, columns: on ? [...current, name] : current.filter((c) => c !== name) });
  };
  const missing = [...plan.only_in_source.map((c) => `${c} (source only)`), ...plan.only_in_target.map((c) => `${c} (target only)`)];

  return (
    <Card title="Columns and options" footer={missing.length ? `Not compared: ${missing.join(', ')}` : undefined}>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 14 }}>
        <div className={styles.columns}>
          <span className={styles.columnsHead}>Column</span>
          <span className={styles.columnsHead}>Type (source → target)</span>
          <span className={styles.columnsHead}>Key</span>
          <span className={styles.columnsHead}>Compare</span>
          {plan.columns.map((c) => (
            <Fragment key={c.name}>
              <span className="mono ellipsis" title={c.target_name === c.name ? c.name : `${c.name} → ${c.target_name}`}>
                {c.name}
              </span>
              <span className="mono ellipsis ink-2">
                {c.source_type} → {c.target_type}{' '}
                {c.type_mismatch && <Chip tone="caution">types differ</Chip>} {!c.synced && <Chip tone="mute">not written</Chip>}
              </span>
              <Checkbox checked={choices.keys.includes(c.name)} onChange={(on) => toggleKey(c.name, on)} label={<span className="sr-only">Key {c.name}</span>} />
              <Checkbox
                checked={compared(c.name)}
                disabled={choices.keys.includes(c.name)}
                onChange={(on) => toggleColumn(c.name, on)}
                label={<span className="sr-only">Compare {c.name}</span>}
              />
            </Fragment>
          ))}
        </div>
        {choices.keys.length === 0 && <Alert tone="caution">Choose at least one key column: rows are matched on it.</Alert>}
        <div>
          {filtering ? (
            <FilterBuilder
              columns={names}
              applied={{ conditions: choices.conditions, matchAny: choices.matchAny }}
              onApply={(f) => {
                onChange({ ...choices, conditions: f.conditions, matchAny: f.matchAny });
                onFiltering(false);
              }}
              onClose={() => onFiltering(false)}
            />
          ) : (
            <span style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <Button size="sm" onClick={() => onFiltering(true)}>
                Filter rows…
              </Button>
              <span className="ink-2 mono ellipsis">{choices.conditions.length ? describeConditions(choices.conditions, choices.matchAny) : 'All rows, on both sides'}</span>
            </span>
          )}
        </div>
        <div className={styles.options}>
          <Checkbox checked={choices.ignoreWhitespace} onChange={(v) => onChange({ ...choices, ignoreWhitespace: v })} label="Ignore whitespace" help="Leading, trailing and repeated spaces" />
          <Checkbox checked={choices.ignoreCase} onChange={(v) => onChange({ ...choices, ignoreCase: v })} label="Ignore case" />
          <TextField label="Number tolerance" mono placeholder="0" value={choices.tolerance} onChange={(e) => onChange({ ...choices, tolerance: e.target.value })} />
          <TextField
            label="Differing rows to keep"
            mono
            type="number"
            min={0}
            max={10000}
            value={choices.maxExamples}
            onChange={(e) => onChange({ ...choices, maxExamples: Math.max(0, Math.min(10000, Number(e.target.value) || 0)) })}
          />
        </div>
      </div>
    </Card>
  );
}

const KIND_LABEL: Record<RowDiff['kind'], string> = { only_in_source: 'missing', only_in_target: 'extra', different: 'changed' };

/** Counts, the differing rows and the sync SQL with its actions. */
function Results({
  result,
  target,
  targetName,
  readOnly,
  flavor,
  onApplied,
}: {
  result: DataCompareResult;
  target: CompareTable;
  targetName: string;
  readOnly: boolean;
  flavor: ReturnType<typeof flavorOf>;
  onApplied: () => void;
}) {
  const { diff, compare_id: compareId } = result;
  const session = useWorkspace((s) => s.session);
  const { toast, toastError, ask } = useUi.getState();
  const [kinds, setKinds] = useState<SyncKinds>(ALL_KINDS);
  /** `undefined` while loading; `null` when too large to show. */
  const [sql, setSql] = useState<string | null | undefined>(undefined);
  const [page, setPage] = useState(0);
  const [applying, setApplying] = useState(false);
  const total = differences(diff.counts);
  const rows = syncRows(diff.counts, kinds);

  useEffect(() => {
    if (total === 0) return;
    let live = true;
    setSql(undefined);
    api.dataCompareSql(compareId, kinds).then(
      (text) => live && setSql(text),
      (e) => live && toastError(e),
    );
    return () => {
      live = false;
    };
  }, [compareId, kinds, total, toastError]);

  const saveToFile = async () => {
    const path = await save({ defaultPath: `${target.table.replace(/[^\w.-]+/g, '_')}-sync.sql`, filters: [{ name: 'SQL', extensions: ['sql'] }] });
    if (!path) return;
    try {
      const statements = await api.dataCompareSave(compareId, kinds, path);
      toast(`Saved ${plural(statements, 'statement')}.`, 'positive');
    } catch (e) {
      toastError(e);
    }
  };

  const apply = async () => {
    const ok = await ask({
      title: `Apply the sync to ${targetName}?`,
      body: `Changes ${plural(rows, 'row')} of ${target.table} in one transaction: if any statement fails, nothing changes.`,
      confirmLabel: 'Apply',
      danger: kinds.delete && diff.counts.only_in_target > 0,
    });
    if (!ok) return;
    setApplying(true);
    try {
      const report = await guarded((c) => api.dataCompareApply(compareId, kinds, uid('sync'), c));
      if (report) {
        toast(`Inserted ${formatCount(report.inserted)}, updated ${formatCount(report.updated)}, deleted ${formatCount(report.deleted)} rows in ${formatDuration(report.duration_ms)}.`, 'positive');
        onApplied();
      }
    } catch (e) {
      toastError(e);
    } finally {
      setApplying(false);
    }
  };

  const shown = diff.examples.slice(page * PAGE_ROWS, (page + 1) * PAGE_ROWS);
  const pages = Math.ceil(diff.examples.length / PAGE_ROWS);
  const onTarget = session?.connection.id === target.connection_id;

  return (
    <>
      <div className={styles.tiles}>
        <Tile label="Identical" value={diff.counts.identical} />
        <Tile label="Only in source (insert)" value={diff.counts.only_in_source} tone={styles.added} />
        <Tile label="Only in target (delete)" value={diff.counts.only_in_target} tone={styles.removed} />
        <Tile label="Different (update)" value={diff.counts.different} tone={styles.changed} />
      </div>
      <span className="ink-3">Compared in {formatDuration(diff.duration_ms)}.</span>
      {total === 0 ? (
        <EmptyState title="No differences">The compared rows of the target match the source.</EmptyState>
      ) : (
        <>
          <Card
            title={`${formatCount(total)} differing rows`}
            flush
            footer={diff.examples_truncated ? `Showing the first ${formatCount(diff.examples.length)}; the counts and the SQL cover every row.` : undefined}
          >
            <div className={styles.scroll}>
              <table className={styles.rows}>
                <thead>
                  <tr>
                    <th />
                    <th />
                    {diff.columns.map((c, i) => (
                      <th key={c} className="mono">
                        {c}
                        {i < diff.keys && <span className="ink-3"> key</span>}
                      </th>
                    ))}
                  </tr>
                </thead>
                <tbody>
                  {shown.map((row, i) => (
                    <DiffRows key={page * PAGE_ROWS + i} row={row} />
                  ))}
                </tbody>
              </table>
            </div>
            {pages > 1 && (
              <div className={styles.pager} style={{ padding: '6px 16px' }}>
                <Button size="xs" disabled={page === 0} onClick={() => setPage(page - 1)}>
                  ← Previous
                </Button>
                Page {page + 1} of {pages}
                <Button size="xs" disabled={page + 1 >= pages} onClick={() => setPage(page + 1)}>
                  Next →
                </Button>
              </div>
            )}
          </Card>
          <Card
            title="Sync SQL"
            flush
            actions={
              <span style={{ display: 'flex', gap: 6 }}>
                <Button size="xs" disabled={!sql} onClick={() => sql && void copyText(sql).then(() => toast('SQL copied'))}>
                  Copy
                </Button>
                <Button size="xs" disabled={rows === 0} onClick={() => void saveToFile()}>
                  Save to file…
                </Button>
                {onTarget && (
                  <Button
                    size="xs"
                    disabled={!sql}
                    onClick={() => {
                      if (!sql) return;
                      useWorkspace.getState().openQueryTab({ sql, title: `${target.table}-sync.sql` });
                      useUi.getState().navigate({ name: 'workspace' });
                    }}
                  >
                    Open in editor
                  </Button>
                )}
                <Button size="xs" variant="caution" busy={applying} disabled={rows === 0 || readOnly} onClick={() => void apply()}>
                  Apply to target…
                </Button>
              </span>
            }
            footer={readOnly ? `${targetName} is read-only: save or copy the SQL instead.` : 'Values are sent as parameters when applied; the SQL above shows them as literals for review.'}
          >
            <div className={styles.kinds}>
              <Checkbox checked={kinds.insert} onChange={(insert) => setKinds({ ...kinds, insert })} label={`Insert ${formatCount(diff.counts.only_in_source)} missing`} />
              <Checkbox checked={kinds.update} onChange={(update) => setKinds({ ...kinds, update })} label={`Update ${formatCount(diff.counts.different)} changed`} />
              <Checkbox checked={kinds.delete} onChange={(del) => setKinds({ ...kinds, delete: del })} label={`Delete ${formatCount(diff.counts.only_in_target)} extra`} />
            </div>
            {sql === null ? (
              <div style={{ padding: 16 }}>
                <Alert tone="info">The script is too large to show here. Save it to a file, or apply it directly.</Alert>
              </div>
            ) : (
              <div style={{ height: 320 }}>{sql !== undefined && <SqlViewer value={sql} flavor={flavor} label="Sync SQL" />}</div>
            )}
          </Card>
        </>
      )}
    </>
  );
}

function Tile({ label, value, tone }: { label: string; value: number; tone?: string }) {
  return (
    <div className={styles.tile}>
      {label}
      <span className={`${styles.tileValue} ${tone ?? ''}`}>{formatCount(value)}</span>
    </div>
  );
}

/** One differing row: a single line when it is on one side, source and target lines when it changed. */
function DiffRows({ row }: { row: RowDiff }) {
  const tone = row.kind === 'only_in_source' ? styles.added : row.kind === 'only_in_target' ? styles.removed : styles.changed;
  const label = <span className={tone}>{KIND_LABEL[row.kind]}</span>;
  if (row.kind !== 'different') {
    const cells = row.kind === 'only_in_source' ? row.source : row.target;
    return (
      <tr>
        <td>{label}</td>
        <td className={styles.side}>{row.kind === 'only_in_source' ? 'source' : 'target'}</td>
        {cells.map((cell, i) => (
          <td key={i}>
            <CellView cell={cell} />
          </td>
        ))}
      </tr>
    );
  }
  const line = (side: 'source' | 'target') => (
    <tr>
      <td>{side === 'source' ? label : null}</td>
      <td className={styles.side}>{side}</td>
      {row[side].map((cell, i) => (
        <td key={i} className={row.changed.includes(i) ? styles.changedCell : undefined}>
          <CellView cell={cell} />
        </td>
      ))}
    </tr>
  );
  return (
    <>
      {line('source')}
      {line('target')}
    </>
  );
}
