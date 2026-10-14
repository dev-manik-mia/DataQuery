import { open } from '@tauri-apps/plugin-dialog';
import { useEffect, useMemo, useState } from 'react';

import type { CsvOptions } from '@/bindings/CsvOptions';
import type { DbError } from '@/bindings/DbError';
import type { ImportOptions } from '@/bindings/ImportOptions';
import type { ImportPreview } from '@/bindings/ImportPreview';
import type { ImportReport } from '@/bindings/ImportReport';
import type { ImportSource } from '@/bindings/ImportSource';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { TableStructure } from '@/bindings/TableStructure';
import type { TextEncoding } from '@/bindings/TextEncoding';
import { Alert, Button, Checkbox, Dialog, ErrorAlert, SelectField, TextField } from '@/components/ui';
import { ENCODINGS } from '@/features/transfer/ExportDialog';
import { useTransferProgress } from '@/features/transfer/progress';
import { toAppError } from '@/lib/errors';
import { formatCount, formatDuration } from '@/lib/format';
import { uid } from '@/lib/id';
import { api } from '@/lib/ipc';
import { guarded, useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Table.module.css';

const SKIP = '';

const CSV_DEFAULTS: CsvOptions = { delimiter: ',', quote: '"', has_header: true, empty_as_null: true, null_text: '', encoding: 'utf8' };
const IMPORT_DEFAULTS: ImportOptions = { batch_rows: 250, commit_rows: 0, skip_invalid: false };

/**
 * CSV or JSON import: choose file, preview, map columns, then validate every
 * row before anything is written. Bad rows are reported by number.
 */
export function ImportDialog({ object, structure, onClose, onImported }: { object: ObjectRef; structure: TableStructure; onClose: () => void; onImported: () => void }) {
  const session = useWorkspace((s) => s.session)!;
  const toast = useUi((s) => s.toast);
  const [path, setPath] = useState<string | null>(null);
  const [json, setJson] = useState(false);
  const [csv, setCsv] = useState<CsvOptions>(CSV_DEFAULTS);
  const [options, setOptions] = useState<ImportOptions>(IMPORT_DEFAULTS);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [mapping, setMapping] = useState<Record<string, string>>({});
  const [error, setError] = useState<DbError | null>(null);
  const [report, setReport] = useState<ImportReport | null>(null);
  const [jobId, setJobId] = useState<string | null>(null);
  const progress = useTransferProgress(jobId);

  const targets = structure.columns.filter((c) => c.editable);
  const source = useMemo<ImportSource | null>(() => (path ? (json ? { type: 'json', path } : { type: 'csv', path, options: csv }) : null), [path, json, csv]);

  useEffect(() => {
    if (!source) return;
    setError(null);
    setReport(null);
    api
      .importPreview(source)
      .then((p) => {
        setPreview(p);
        // Map columns whose header or key matches a column name.
        const auto: Record<string, string> = {};
        for (const c of targets) {
          const i = p.headers.findIndex((h) => h.trim().toLowerCase() === c.name.toLowerCase());
          auto[c.name] = i >= 0 ? (json ? p.headers[i]! : String(i)) : SKIP;
        }
        setMapping(auto);
      })
      .catch((e) => {
        setPreview(null);
        setError(toAppError(e).info);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [source]);

  const missing = useMemo(() => targets.filter((c) => !c.nullable && c.default === null && !c.generated && !mapping[c.name]).map((c) => c.name), [targets, mapping]);
  const mapped = Object.entries(mapping).filter(([, v]) => v !== SKIP);

  const choose = async () => {
    const chosen = await open({ multiple: false, filters: [{ name: 'CSV or JSON', extensions: ['csv', 'tsv', 'txt', 'json', 'jsonl', 'ndjson'] }] });
    if (typeof chosen !== 'string') return;
    setPath(chosen);
    setJson(/\.(json|jsonl|ndjson)$/i.test(chosen));
    if (chosen.endsWith('.tsv')) setCsv((o) => ({ ...o, delimiter: '\\t' }));
  };

  const run = async () => {
    if (!source) return;
    const job = uid('import');
    setJobId(job);
    setError(null);
    setReport(null);
    try {
      const result = await guarded((c) =>
        api.importData(
          { sessionId: session.session_id },
          object,
          source,
          mapped.map(([column, field]) => (json ? { column, field: null, key: field } : { column, field: Number(field), key: null })),
          options,
          job,
          c,
        ),
      );
      if (!result) return;
      if (result.stopped || result.error_count > 0) {
        setReport(result);
        if (result.rows > 0) onImported();
      } else {
        toast(`Imported ${formatCount(result.rows)} rows in ${formatDuration(result.duration_ms)}.`, 'positive');
        onImported();
        onClose();
      }
    } catch (e) {
      setError(toAppError(e).info);
    } finally {
      setJobId(null);
    }
  };

  const setCsvOption = (patch: Partial<CsvOptions>) => setCsv((o) => ({ ...o, ...patch }));

  return (
    <Dialog
      title={`Import into ${object.name}`}
      wide
      onClose={() => (jobId ? undefined : onClose())}
      footer={
        jobId ? (
          <>
            <span className="ink-2 tabular" style={{ marginRight: 'auto' }}>
              {progress ? (progress.rows > 0 ? `${formatCount(progress.rows)} rows written` : `Checking rows… ${formatCount(progress.bytes)}`) : 'Starting…'}
            </span>
            <Button onClick={() => void api.cancelTransfer(jobId)}>Cancel import</Button>
          </>
        ) : (
          <>
            <Button onClick={onClose}>Close</Button>
            <Button variant="primary" disabled={!preview || !mapped.length || missing.length > 0} onClick={() => void run()}>
              Import
            </Button>
          </>
        )
      }
    >
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <Button onClick={() => void choose()}>{path ? 'Choose another file…' : 'Choose CSV or JSON file…'}</Button>
        <span className="mono ellipsis" style={{ fontSize: 12 }}>
          {path ?? 'No file selected'}
        </span>
      </div>
      {path && (
        <div className={styles.importGrid}>
          <SelectField label="File type" value={json ? 'json' : 'csv'} onChange={(e) => setJson(e.target.value === 'json')} options={[{ value: 'csv', label: 'CSV' }, { value: 'json', label: 'JSON (array or one object per line)' }]} />
          {!json && (
            <>
              <TextField label="Delimiter" mono value={csv.delimiter} maxLength={2} onChange={(e) => setCsvOption({ delimiter: e.target.value || ',' })} hint="\t for tab" />
              <TextField label="Quote" mono value={csv.quote} maxLength={1} onChange={(e) => setCsvOption({ quote: e.target.value || '"' })} />
              <SelectField label="Encoding" value={csv.encoding} onChange={(e) => setCsvOption({ encoding: e.target.value as TextEncoding })} options={ENCODINGS.filter((x) => x.value !== 'utf8-bom')} hint="A byte-order mark in the file wins." />
              <TextField label="NULL text" mono placeholder="e.g. NULL or \N" value={csv.null_text} onChange={(e) => setCsvOption({ null_text: e.target.value })} />
              <div style={{ display: 'flex', flexDirection: 'column', gap: 6, paddingTop: 18 }}>
                <Checkbox label="First row is a header" checked={csv.has_header} onChange={(v) => setCsvOption({ has_header: v })} />
                <Checkbox label="Empty fields are NULL" checked={csv.empty_as_null} onChange={(v) => setCsvOption({ empty_as_null: v })} />
              </div>
            </>
          )}
        </div>
      )}
      {preview && (
        <>
          <div className={styles.preview}>
            <table>
              <thead>
                <tr>
                  {preview.headers.map((h, i) => (
                    <th key={i}>{h}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {preview.rows.map((r, i) => (
                  <tr key={i}>
                    {preview.headers.map((_, j) => (
                      <td key={j}>{r[j] ?? <span className="muted">NULL</span>}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div className="dq-eyebrow">Column mapping</div>
          <div className={styles.mapping}>
            {targets.map((c) => (
              <div key={c.name} style={{ display: 'contents' }}>
                <span className={styles.mappingName}>
                  {c.name} <span className="muted">{c.data_type}</span>
                  {missing.includes(c.name) && <span className={styles.required}> · required</span>}
                </span>
                <SelectField
                  label={<span className="sr-only">Source for {c.name}</span>}
                  value={mapping[c.name] ?? SKIP}
                  onChange={(e) => setMapping({ ...mapping, [c.name]: e.target.value })}
                  options={[
                    { value: SKIP, label: c.default !== null || c.generated ? 'Skip (use default)' : c.nullable ? 'Skip (NULL)' : 'Skip' },
                    ...preview.headers.map((h, i) => ({ value: json ? h : String(i), label: h })),
                  ]}
                />
              </div>
            ))}
          </div>
          {missing.length > 0 && <Alert tone="caution">Map the required columns: {missing.join(', ')}.</Alert>}
          <div className={styles.importGrid}>
            <TextField label="Rows per INSERT" mono inputMode="numeric" value={String(options.batch_rows)} onChange={(e) => setOptions({ ...options, batch_rows: Number(e.target.value.replace(/\D/g, '')) || 1 })} />
            <TextField
              label="Commit every"
              mono
              inputMode="numeric"
              suffix="rows"
              hint="0 = one transaction for the whole file."
              value={String(options.commit_rows)}
              onChange={(e) => setOptions({ ...options, commit_rows: Number(e.target.value.replace(/\D/g, '')) || 0 })}
            />
            <div style={{ paddingTop: 18 }}>
              <Checkbox label="Skip invalid rows" help="Import the valid rows and report the others." checked={options.skip_invalid} onChange={(v) => setOptions({ ...options, skip_invalid: v })} />
            </div>
          </div>
          <Alert tone="info">
            Every row is checked first (types, NULLs, lengths). {options.commit_rows ? `Rows are committed every ${formatCount(options.commit_rows)} rows.` : 'Rows are inserted in one transaction: if any fails, nothing is imported.'}
          </Alert>
        </>
      )}
      {report && (
        <Alert tone={report.rows > 0 ? 'caution' : 'critical'} title={report.rows > 0 ? `Imported ${formatCount(report.rows)} rows` : 'Nothing was imported'}>
          {report.message && <div>{report.message}</div>}
          {report.skipped > 0 && <div>{formatCount(report.skipped)} invalid rows were skipped.</div>}
          {report.errors.length > 0 && (
            <div className={styles.errorReport}>
              {report.errors.map((e) => (
                <div key={e.row}>
                  <span className="mono">Row {formatCount(e.row)}</span> {e.message}
                </div>
              ))}
              {report.error_count > report.errors.length && <div className="ink-3">… and {formatCount(report.error_count - report.errors.length)} more</div>}
            </div>
          )}
        </Alert>
      )}
      {error && <ErrorAlert error={error} title="Import failed" />}
    </Dialog>
  );
}
