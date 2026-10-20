import { open, save } from '@tauri-apps/plugin-dialog';
import { useEffect, useState } from 'react';

import type { BackupContent } from '@/bindings/BackupContent';
import type { BackupFormat } from '@/bindings/BackupFormat';
import type { BackupOptions } from '@/bindings/BackupOptions';
import type { BackupPlan } from '@/bindings/BackupPlan';
import type { BackupReport } from '@/bindings/BackupReport';
import type { DbError } from '@/bindings/DbError';
import type { RestoreOptions } from '@/bindings/RestoreOptions';
import { Alert, Button, Checkbox, Dialog, ErrorAlert, SelectField, Spinner, TextField } from '@/components/ui';
import { t } from '@/i18n';
import { toAppError } from '@/lib/errors';
import { formatBytes, formatCount, formatDuration } from '@/lib/format';
import { uid } from '@/lib/id';
import { api } from '@/lib/ipc';
import { guarded } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { FORMATS, backupFileName, defaultOptions, guessFormat, nameList, toolsFor, useBackupRequest } from './backup';
import { useTransferProgress } from './progress';

const CONTENT: { value: BackupContent; label: string }[] = [
  { value: 'all', label: 'Schema and data' },
  { value: 'schema_only', label: 'Schema only' },
  { value: 'data_only', label: 'Data only' },
];

const grid = { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 } as const;
const checks = { display: 'flex', flexDirection: 'column', gap: 6, paddingTop: 4 } as const;

/**
 * Backup and restore with the engine's dump tools (SQLite: its own backup API):
 * options, file, live progress with Cancel, then a summary. Restores are
 * confirmed by the backend like destructive statements.
 */
export function BackupDialog() {
  const { request, close } = useBackupRequest();
  const session = useWorkspace((s) => s.session);
  const [plan, setPlan] = useState<BackupPlan | null>(null);
  const [options, setOptions] = useState<BackupOptions | null>(null);
  const [restore, setRestore] = useState<RestoreOptions>({ database: request?.database ?? '', format: null, content: 'all', clean: false, no_owner: false, single_transaction: true });
  const [schemas, setSchemas] = useState('');
  const [tables, setTables] = useState('');
  const [path, setPath] = useState<string | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [report, setReport] = useState<BackupReport | null>(null);
  const progress = useTransferProgress(jobId);

  useEffect(() => {
    if (!request || !session) return;
    api
      .backupPlan({ sessionId: session.session_id })
      .then((p) => {
        setPlan(p);
        const initial = defaultOptions(p.family, p.formats[0]!, request.database, request.schema);
        setOptions(initial);
        setSchemas(initial.schemas.join(', '));
      })
      .catch((e) => setError(toAppError(e).info));
  }, [request, session]);

  if (!request || !session) return null;
  const backup = request.mode === 'backup';
  const family = plan?.family;
  const set = (patch: Partial<BackupOptions>) => setOptions((o) => (o ? { ...o, ...patch } : o));
  const setR = (patch: Partial<RestoreOptions>) => setRestore((o) => ({ ...o, ...patch }));
  const restoreFormat = family && path ? guessFormat(family, path) : null;
  const tools = family && options ? toolsFor(family, request.mode, backup ? options.format : restoreFormat) : [];
  const statuses = plan?.tools.filter((s) => tools.includes(s.tool)) ?? [];
  const missing = statuses.filter((s) => !s.path);
  const target = backup ? options?.database : restore.database;
  const title = `${backup ? 'Back up' : 'Restore into'} ${target || request.database}${request.schema && backup ? ` · ${request.schema}` : ''}`;
  const sid = { sessionId: session.session_id };

  const finish = (result: BackupReport | null) => {
    if (result) setReport(result);
  };
  const fail = (e: unknown) => {
    const err = toAppError(e).info;
    // Cancelled jobs say what was cleaned up; show that, not a failure.
    setError(err.kind === 'cancelled' ? { ...err, action: null } : err);
  };

  const runBackup = async () => {
    if (!options) return;
    const format = FORMATS[options.format];
    const chosen = await save({
      defaultPath: backupFileName(options.database, options.format, options.compress),
      filters: format.extension ? [{ name: format.label, extensions: [format.extension, ...(options.format === 'archive' ? ['gz'] : [])] }] : undefined,
    });
    if (!chosen) return;
    const job = uid('backup');
    setJobId(job);
    setError(null);
    setReport(null);
    try {
      finish(await api.backupDatabase(sid, { ...options, schemas: nameList(schemas), tables: nameList(tables) }, chosen, job));
    } catch (e) {
      fail(e);
    } finally {
      setJobId(null);
    }
  };

  const chooseFile = async (directory: boolean) => {
    const chosen = await open({ multiple: false, directory });
    if (typeof chosen === 'string') {
      setPath(chosen);
      setReport(null);
      setError(null);
    }
  };

  const runRestore = async () => {
    if (!path) return;
    const job = uid('restore');
    setError(null);
    setReport(null);
    try {
      setJobId(job);
      finish(await guarded((c) => api.restoreDatabase(sid, restore, path, job, c), { onAsk: () => setJobId(null), onConfirmed: () => setJobId(job) }));
    } catch (e) {
      fail(e);
    } finally {
      setJobId(null);
    }
  };

  const percent = progress?.total_bytes ? Math.min(100, Math.round((progress.bytes / progress.total_bytes) * 100)) : null;
  const status = progress
    ? [progress.rows > 0 ? `${formatCount(progress.rows)} objects` : null, progress.bytes > 0 ? formatBytes(progress.bytes) : null, percent !== null ? `${percent}%` : null]
        .filter(Boolean)
        .join(' · ') || 'Working…'
    : 'Starting…';

  return (
    <Dialog
      title={title}
      critical={!backup}
      onClose={() => (jobId ? undefined : close())}
      footer={
        jobId ? (
          <>
            <span className="ink-2 tabular" style={{ marginRight: 'auto' }}>
              {status}
            </span>
            <Button onClick={() => void api.cancelTransfer(jobId)}>{backup ? 'Cancel backup' : 'Cancel restore'}</Button>
          </>
        ) : report ? (
          <Button variant="primary" onClick={close}>
            {t('action.close')}
          </Button>
        ) : (
          <>
            <Button onClick={close}>{t('action.cancel')}</Button>
            {backup ? (
              <Button variant="primary" disabled={!options || missing.length > 0 || !options.database.trim()} onClick={() => void runBackup()}>
                Choose file and back up
              </Button>
            ) : (
              <Button variant="danger" disabled={!plan || !path || missing.length > 0 || (family !== 'sqlite' && !restore.database.trim())} onClick={() => void runRestore()}>
                Restore
              </Button>
            )}
          </>
        )
      }
    >
      {!plan && !error && <Spinner />}
      {plan && options && family && (
        <>
          <p className="ink-2" style={{ fontSize: 12, marginTop: 0 }}>
            {family === 'sqlite'
              ? 'Built in: SQLite copies the database page by page while it stays usable.'
              : statuses.map((s) => (
                  <span key={s.tool} title={s.path ?? undefined} style={{ marginRight: 12 }}>
                    <span className="mono">{s.tool}</span> {s.version ?? <b>not found</b>}
                    {s.custom ? ' (from Settings)' : ''}
                  </span>
                ))}
          </p>
          {missing.map((s) => (
            <Alert key={s.tool} tone="critical" title={`${s.tool} is needed`}>
              {s.error} Install the client tools for this database, or choose the program in Settings → Engines → Backup tools.
            </Alert>
          ))}
          {plan.warnings.map((w) => (
            <Alert key={w} tone="caution">
              {w}
            </Alert>
          ))}
          {backup ? (
            <div style={grid}>
              {family !== 'sqlite' && <TextField label="Database" mono value={options.database} onChange={(e) => set({ database: e.target.value })} />}
              <SelectField
                label="Format"
                value={options.format}
                onChange={(e) => set({ format: e.target.value as BackupFormat })}
                options={plan.formats.map((f) => ({ value: f, label: FORMATS[f].label }))}
                hint={FORMATS[options.format].hint}
              />
              {(family === 'postgres' || family === 'mysql') && (
                <SelectField label="Content" value={options.content} onChange={(e) => set({ content: e.target.value as BackupContent })} options={CONTENT} />
              )}
              {family === 'postgres' && (
                <TextField label="Schemas" mono placeholder="all" hint="Comma-separated." value={schemas} onChange={(e) => setSchemas(e.target.value)} />
              )}
              {family !== 'sqlite' && (
                <TextField
                  label={family === 'mongodb' ? 'Collection' : 'Tables'}
                  mono
                  placeholder={family === 'mongodb' ? 'all collections' : 'all tables'}
                  hint={family === 'postgres' ? 'Comma-separated; schema.table, * wildcards allowed.' : family === 'mysql' ? 'Comma-separated.' : 'One collection, or empty for all.'}
                  value={tables}
                  onChange={(e) => setTables(e.target.value)}
                />
              )}
              <div style={checks}>
                {family === 'postgres' && (
                  <>
                    <Checkbox label="No owner" help="Leave out ALTER … OWNER so another role can restore it." checked={options.no_owner} onChange={(v) => set({ no_owner: v })} />
                    {options.format === 'plain' && (
                      <Checkbox label="Drop objects first" help="DROP … IF EXISTS before each CREATE." checked={options.clean} onChange={(v) => set({ clean: v })} />
                    )}
                  </>
                )}
                {family === 'mysql' && (
                  <>
                    <Checkbox label="Single transaction" help="A consistent InnoDB snapshot without locking tables." checked={options.single_transaction} onChange={(v) => set({ single_transaction: v })} />
                    <Checkbox label="Routines" checked={options.routines} onChange={(v) => set({ routines: v })} />
                    <Checkbox label="Triggers" checked={options.triggers} onChange={(v) => set({ triggers: v })} />
                    <Checkbox label="Events" checked={options.events} onChange={(v) => set({ events: v })} />
                    <Checkbox label="DROP TABLE before CREATE" checked={options.clean} onChange={(v) => set({ clean: v })} />
                  </>
                )}
                {family === 'mongodb' && <Checkbox label="Compress (gzip)" checked={options.compress} onChange={(v) => set({ compress: v })} />}
              </div>
            </div>
          ) : (
            <>
              <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
                <Button onClick={() => void chooseFile(false)}>{path ? 'Choose another file…' : 'Choose backup file…'}</Button>
                {family === 'postgres' && <Button onClick={() => void chooseFile(true)}>Choose folder…</Button>}
                <span className="mono ellipsis" style={{ fontSize: 12 }}>
                  {path ?? 'No file selected'}
                </span>
              </div>
              <div style={{ ...grid, marginTop: 12 }}>
                <TextField
                  label="Restore into database"
                  mono
                  value={restore.database}
                  hint={family === 'sqlite' ? 'main, or an attached database.' : 'Must exist already.'}
                  onChange={(e) => setR({ database: e.target.value })}
                />
                {family === 'postgres' && restoreFormat !== 'plain' && (
                  <SelectField label="Content" value={restore.content} onChange={(e) => setR({ content: e.target.value as BackupContent })} options={CONTENT} />
                )}
                <div style={checks}>
                  {family === 'postgres' && restoreFormat !== 'plain' && (
                    <>
                      <Checkbox label="Drop objects first" help="pg_restore --clean --if-exists" checked={restore.clean} onChange={(v) => setR({ clean: v })} />
                      <Checkbox label="No owner" checked={restore.no_owner} onChange={(v) => setR({ no_owner: v })} />
                    </>
                  )}
                  {family === 'postgres' && (
                    <Checkbox label="All or nothing" help="One transaction: any error undoes the whole restore." checked={restore.single_transaction} onChange={(v) => setR({ single_transaction: v })} />
                  )}
                  {family === 'mongodb' && <Checkbox label="Drop collections first" help="mongorestore --drop" checked={restore.clean} onChange={(v) => setR({ clean: v })} />}
                </div>
              </div>
              <Alert tone="caution">
                {family === 'sqlite'
                  ? 'The whole database is replaced by the file. Cancelling leaves it unchanged.'
                  : family === 'mysql'
                    ? 'The script runs statement by statement; statements before an error stay applied.'
                    : 'Objects with the same names are changed or replaced. This cannot be undone; back up first if unsure.'}
              </Alert>
            </>
          )}
        </>
      )}
      {jobId && (
        <div style={{ marginTop: 12 }}>
          {percent !== null && <progress max={100} value={percent} style={{ width: '100%' }} />}
          <div className="mono ellipsis ink-2" style={{ fontSize: 12 }} title={progress?.detail ?? undefined}>
            {progress?.detail ?? (backup ? 'Starting the backup…' : 'Starting the restore…')}
          </div>
        </div>
      )}
      {report && (
        <Alert tone="positive" title={backup ? 'Backup complete' : 'Restore complete'}>
          <div className="mono ellipsis" title={report.path}>
            {report.path}
          </div>
          <div>
            {[report.objects > 0 ? `${formatCount(report.objects)} objects` : null, formatBytes(report.bytes), formatDuration(report.duration_ms), report.tool]
              .filter(Boolean)
              .join(' · ')}
          </div>
          {report.warnings.map((w) => (
            <div key={w} className="ink-2">
              {w}
            </div>
          ))}
        </Alert>
      )}
      {error &&
        (error.kind === 'cancelled' ? (
          <Alert tone="caution" title={error.message} />
        ) : (
          <ErrorAlert error={error} title={backup ? 'Backup failed' : 'Restore failed'} />
        ))}
    </Dialog>
  );
}
