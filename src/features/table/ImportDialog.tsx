import { open } from '@tauri-apps/plugin-dialog';
import { useEffect, useMemo, useState } from 'react';

import type { CsvOptions } from '@/bindings/CsvOptions';
import type { CsvPreview } from '@/bindings/CsvPreview';
import type { DbError } from '@/bindings/DbError';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { TableStructure } from '@/bindings/TableStructure';
import { Alert, Button, Checkbox, Dialog, ErrorAlert, SelectField, TextField } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { formatCount, formatDuration } from '@/lib/format';
import { api } from '@/lib/ipc';
import { guarded, useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Table.module.css';

const SKIP = '';

/** CSV import: choose file, preview, map columns, validate, import in one transaction. */
export function ImportDialog({ object, structure, onClose, onImported }: { object: ObjectRef; structure: TableStructure; onClose: () => void; onImported: () => void }) {
  const session = useWorkspace((s) => s.session)!;
  const toast = useUi((s) => s.toast);
  const [path, setPath] = useState<string | null>(null);
  const [options, setOptions] = useState<CsvOptions>({ delimiter: ',', has_header: true, empty_as_null: true });
  const [preview, setPreview] = useState<CsvPreview | null>(null);
  const [mapping, setMapping] = useState<Record<string, string>>({});
  const [error, setError] = useState<DbError | null>(null);
  const [busy, setBusy] = useState(false);

  const targets = structure.columns.filter((c) => c.editable);

  useEffect(() => {
    if (!path) return;
    setError(null);
    api
      .csvPreview(path, options)
      .then((p) => {
        setPreview(p);
        // Auto-map columns whose header matches a column name.
        const auto: Record<string, string> = {};
        for (const c of targets) {
          const i = p.headers.findIndex((h) => h.trim().toLowerCase() === c.name.toLowerCase());
          auto[c.name] = i >= 0 ? String(i) : SKIP;
        }
        setMapping(auto);
      })
      .catch((e) => {
        setPreview(null);
        setError(toAppError(e).info);
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [path, options]);

  const missing = useMemo(
    () => targets.filter((c) => !c.nullable && c.default === null && !c.generated && !mapping[c.name]).map((c) => c.name),
    [targets, mapping],
  );
  const mapped = Object.entries(mapping).filter(([, v]) => v !== SKIP);

  const choose = async () => {
    const chosen = await open({ multiple: false, filters: [{ name: 'CSV', extensions: ['csv', 'tsv', 'txt'] }] });
    if (typeof chosen === 'string') {
      setPath(chosen);
      if (chosen.endsWith('.tsv')) setOptions((o) => ({ ...o, delimiter: '\\t' }));
    }
  };

  const run = async () => {
    if (!path) return;
    setBusy(true);
    setError(null);
    try {
      const report = await guarded((c) =>
        api.importCsv({ sessionId: session.session_id }, object, path, options, mapped.map(([column, field]) => ({ column, field: Number(field) })), c),
      );
      if (report) {
        toast(`Imported ${formatCount(report.rows)} rows in ${formatDuration(report.duration_ms)}.`, 'positive');
        onImported();
        onClose();
      }
    } catch (e) {
      setError(toAppError(e).info);
    } finally {
      setBusy(false);
    }
  };

  return (
    <Dialog
      title={`Import CSV into ${object.name}`}
      wide
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" busy={busy} disabled={!preview || !mapped.length || missing.length > 0} onClick={() => void run()}>
            Import
          </Button>
        </>
      }
    >
      <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
        <Button onClick={() => void choose()}>{path ? 'Choose another file…' : 'Choose CSV file…'}</Button>
        <span className="mono ellipsis" style={{ fontSize: 12 }}>
          {path ?? 'No file selected'}
        </span>
      </div>
      {path && (
        <div className={styles.importGrid}>
          <TextField label="Delimiter" mono value={options.delimiter} maxLength={2} onChange={(e) => setOptions({ ...options, delimiter: e.target.value || ',' })} hint="Use \t for tab" />
          <Checkbox label="First row is a header" checked={options.has_header} onChange={(v) => setOptions({ ...options, has_header: v })} />
          <Checkbox label="Empty fields are NULL" checked={options.empty_as_null} onChange={(v) => setOptions({ ...options, empty_as_null: v })} />
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
                      <td key={j}>{r[j] ?? ''}</td>
                    ))}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          <div className="rb-eyebrow">Column mapping</div>
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
                  options={[{ value: SKIP, label: c.default !== null || c.generated ? 'Skip (use default)' : c.nullable ? 'Skip (NULL)' : 'Skip' }, ...preview.headers.map((h, i) => ({ value: String(i), label: h }))]}
                />
              </div>
            ))}
          </div>
          {missing.length > 0 && <Alert tone="caution">Map the required columns: {missing.join(', ')}.</Alert>}
          <Alert tone="info">Rows are inserted in one transaction. If any row fails, nothing is imported and the failing rows are reported.</Alert>
        </>
      )}
      {error && <ErrorAlert error={error} title="Import failed — nothing was imported" />}
    </Dialog>
  );
}
