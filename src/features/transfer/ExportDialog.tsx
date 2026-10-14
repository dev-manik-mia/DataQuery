import { save } from '@tauri-apps/plugin-dialog';
import { useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { ExportFormat } from '@/bindings/ExportFormat';
import type { ExportOptions } from '@/bindings/ExportOptions';
import type { TextEncoding } from '@/bindings/TextEncoding';
import { Button, Checkbox, Dialog, ErrorAlert, SelectField, TextField } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { formatBytes, formatCount, formatDuration } from '@/lib/format';
import { uid } from '@/lib/id';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { EXPORT_FILTERS, useExportRequest } from './exportData';
import { useTransferProgress } from './progress';

export const ENCODINGS: { value: TextEncoding; label: string }[] = [
  { value: 'utf8', label: 'UTF-8' },
  { value: 'utf8-bom', label: 'UTF-8 with BOM (Excel)' },
  { value: 'utf16le', label: 'UTF-16 LE' },
  { value: 'windows1252', label: 'Windows-1252' },
  { value: 'latin1', label: 'Latin-1 (ISO-8859-1)' },
];

const DEFAULTS: ExportOptions = {
  delimiter: ',',
  quote: '"',
  quote_all: false,
  header: true,
  null_text: '',
  crlf: false,
  encoding: 'utf8',
  sql_table: null,
  sql_batch_rows: 100,
};

/** Format and options for an export, then a streamed write with progress and Cancel. */
export function ExportDialog() {
  const { request, close } = useExportRequest();
  const session = useWorkspace((s) => s.session);
  const [format, setFormat] = useState<ExportFormat>(request?.format ?? 'csv');
  const [options, setOptions] = useState<ExportOptions>(DEFAULTS);
  const [jobId, setJobId] = useState<string | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const progress = useTransferProgress(jobId);
  if (!request || !session) return null;
  const set = (patch: Partial<ExportOptions>) => setOptions((o) => ({ ...o, ...patch }));
  const textual = format === 'csv' || format === 'sql_insert';

  const run = async () => {
    const path = await save({ defaultPath: `${request.baseName.replace(/[^\w.-]+/g, '_')}.${EXPORT_FILTERS[format].extensions[0]}`, filters: [EXPORT_FILTERS[format]] });
    if (!path) return;
    const job = uid('export');
    setJobId(job);
    setError(null);
    try {
      const report = await api.exportData({ sessionId: session.session_id }, request.source, format, options, path, job);
      useUi.getState().toast(`Exported ${formatCount(report.rows)} rows (${formatBytes(report.bytes)}) in ${formatDuration(report.duration_ms)}.`, 'positive');
      close();
    } catch (e) {
      const err = toAppError(e).info;
      if (err.kind === 'cancelled') useUi.getState().toast('Export cancelled. No file was left behind.');
      else setError(err);
    } finally {
      setJobId(null);
    }
  };

  return (
    <Dialog
      title={`Export ${request.baseName}`}
      onClose={() => (jobId ? undefined : close())}
      footer={
        jobId ? (
          <>
            <span className="ink-2 tabular" style={{ marginRight: 'auto' }}>
              {progress ? `${formatCount(progress.rows)} rows · ${formatBytes(progress.bytes)}` : 'Starting…'}
            </span>
            <Button onClick={() => void api.cancelTransfer(jobId)}>Cancel export</Button>
          </>
        ) : (
          <>
            <Button onClick={close}>Cancel</Button>
            <Button variant="primary" onClick={() => void run()}>
              Choose file and export
            </Button>
          </>
        )
      }
    >
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
        <SelectField
          label="Format"
          value={format}
          onChange={(e) => setFormat(e.target.value as ExportFormat)}
          options={[
            { value: 'csv', label: 'CSV' },
            { value: 'json', label: 'JSON' },
            { value: 'xlsx', label: 'Excel (XLSX)' },
            { value: 'sql_insert', label: 'SQL INSERT statements' },
          ]}
        />
        {textual && <SelectField label="Encoding" value={options.encoding} onChange={(e) => set({ encoding: e.target.value as TextEncoding })} options={ENCODINGS} />}
        {format === 'csv' && (
          <>
            <TextField label="Delimiter" mono maxLength={2} value={options.delimiter} hint="\t for tab" onChange={(e) => set({ delimiter: e.target.value || ',' })} />
            <TextField label="Quote" mono maxLength={1} value={options.quote} onChange={(e) => set({ quote: e.target.value || '"' })} />
            <TextField label="NULL as" mono placeholder="(empty)" value={options.null_text} hint="Written for NULL. Empty strings stay empty." onChange={(e) => set({ null_text: e.target.value })} />
            <div style={{ display: 'flex', flexDirection: 'column', gap: 8, paddingTop: 18 }}>
              <Checkbox label="Header row" checked={options.header} onChange={(v) => set({ header: v })} />
              <Checkbox label="Quote every field" checked={options.quote_all} onChange={(v) => set({ quote_all: v })} />
              <Checkbox label="Windows line endings (CRLF)" checked={options.crlf} onChange={(v) => set({ crlf: v })} />
            </div>
          </>
        )}
        {format === 'sql_insert' && (
          <>
            <TextField label="Target table" mono placeholder="same as the source" value={options.sql_table ?? ''} onChange={(e) => set({ sql_table: e.target.value || null })} />
            <TextField label="Rows per INSERT" mono inputMode="numeric" value={String(options.sql_batch_rows)} onChange={(e) => set({ sql_batch_rows: Number(e.target.value.replace(/\D/g, '')) || 1 })} />
          </>
        )}
      </div>
      {format === 'xlsx' && <p className="ink-2" style={{ fontSize: 12 }}>Numbers that a spreadsheet cannot hold exactly (more than 15 digits) are written as text. Sheets hold up to 1,048,576 rows.</p>}
      {error && (
        <div style={{ marginTop: 10 }}>
          <ErrorAlert error={error} title="Export failed" />
        </div>
      )}
    </Dialog>
  );
}
