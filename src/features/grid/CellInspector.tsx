import { useEffect, useState } from 'react';

import type { Cell } from '@/bindings/Cell';
import { Alert, Button, Dialog, Segmented, Spinner } from '@/components/ui';
import { cellText, hexDump, localTime, prettyJson } from '@/lib/cells';
import { copyText } from '@/lib/clipboard';
import { formatBytes } from '@/lib/format';
import { useUi } from '@/stores/ui';

import styles from './Grid.module.css';

interface CellInspectorProps {
  column: string;
  typeName: string;
  /** The value as the grid holds it (possibly shortened). */
  cell: Cell;
  /** Fetches the full value from the server, when the grid knows the row. */
  load?: () => Promise<Cell>;
  onSaveFile?: () => void;
  /** Present when the value can be edited; the edit becomes a pending change. */
  onEdit?: (value: string | null) => void;
  onClose: () => void;
}

/** Full value viewer: text, JSON (editable), timestamps and a hex dump for binary. */
export function CellInspector({ column, typeName, cell: preview, load, onSaveFile, onEdit, onClose }: CellInspectorProps) {
  const toast = useUi((s) => s.toast);
  const [cell, setCell] = useState<Cell>(preview);
  const [loading, setLoading] = useState(false);
  const partial = (preview.kind === 'binary' && preview.head_hex.length < preview.length * 2) || ((preview.kind === 'text' || preview.kind === 'json') && Boolean(preview.truncated));
  const isJson = cell.kind === 'json' || /json/i.test(typeName);
  const editableText = onEdit && (cell.kind === 'text' || cell.kind === 'json' || cell.kind === 'null') && !(cell.kind !== 'null' && cell.truncated);
  const [mode, setMode] = useState<'view' | 'edit'>('view');
  const [draft, setDraft] = useState('');
  const [invalid, setInvalid] = useState<string | null>(null);

  useEffect(() => {
    if (!load || !partial) return;
    setLoading(true);
    load()
      .then(setCell)
      .catch((e) => toast(`Could not load the full value: ${String((e as Error).message ?? e)}`, 'critical'))
      .finally(() => setLoading(false));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const text = cell.kind === 'json' ? prettyJson(cell.value) : cellText(cell);
  const meta: [string, string][] = [
    ['Column', column],
    ['Type', typeName || cell.kind],
  ];
  if (cell.kind === 'temporal' && cell.zoned) meta.push(['Local time', localTime(cell.value) ?? '—']);
  if (cell.kind === 'binary') meta.push(['Size', `${formatBytes(cell.length)} (${cell.length} bytes)`], ['Shown', `first ${formatBytes(cell.head_hex.length / 2)}`]);
  if ((cell.kind === 'text' || cell.kind === 'json') && cell.truncated) meta.push(['Note', 'The value is larger than the viewer shows. Save it to a file to read all of it.']);
  if (cell.kind === 'text' || cell.kind === 'json') meta.push(['Length', `${cell.value.length} characters`]);

  const startEdit = () => {
    setDraft(cell.kind === 'null' ? '' : isJson ? prettyJson(cell.kind === 'json' || cell.kind === 'text' ? cell.value : '') : (cellText(cell) ?? ''));
    setInvalid(null);
    setMode('edit');
  };

  const apply = (value: string | null) => {
    if (value !== null && isJson) {
      try {
        // Keep the document compact, as stored; validation first.
        value = JSON.stringify(JSON.parse(value));
      } catch (e) {
        setInvalid((e as Error).message);
        return;
      }
    }
    onEdit?.(value);
    onClose();
  };

  return (
    <Dialog
      title={mode === 'edit' ? `Edit ${column}` : 'Value'}
      wide
      onClose={onClose}
      footer={
        mode === 'edit' ? (
          <>
            <Button onClick={() => apply(null)}>Set NULL</Button>
            <Button onClick={() => setMode('view')}>Back</Button>
            <Button variant="primary" onClick={() => apply(draft)}>
              Apply as pending change
            </Button>
          </>
        ) : (
          <>
            {onSaveFile && (cell.kind === 'binary' || partial) && <Button onClick={onSaveFile}>Save to file…</Button>}
            {editableText && <Button onClick={startEdit}>Edit{isJson ? ' JSON' : ''}…</Button>}
            <Button disabled={text === null} onClick={() => text !== null && void copyText(text).then(() => toast('Value copied'))}>
              Copy
            </Button>
            <Button variant="primary" onClick={onClose}>
              Close
            </Button>
          </>
        )
      }
    >
      <dl className={styles.inspectorMeta}>
        {meta.map(([k, v]) => (
          <div key={k} style={{ display: 'contents' }}>
            <dt>{k}</dt>
            <dd>{v}</dd>
          </div>
        ))}
      </dl>
      {loading && <Spinner label="Loading the full value" />}
      {mode === 'edit' ? (
        <>
          <textarea className={styles.jsonEditor} aria-label={`Value of ${column}`} value={draft} spellCheck={false} onChange={(e) => setDraft(e.target.value)} autoFocus />
          {invalid && <Alert tone="critical">Not valid JSON: {invalid}</Alert>}
        </>
      ) : cell.kind === 'binary' ? (
        <BinaryView hex={cell.head_hex} length={cell.length} />
      ) : (
        <pre className={styles.inspectorValue}>{text === null ? 'NULL' : text === '' ? '(empty string)' : text}</pre>
      )}
    </Dialog>
  );
}

function BinaryView({ hex, length }: { hex: string; length: number }) {
  const [as, setAs] = useState<'hex' | 'text'>('hex');
  const text = () => {
    const bytes = new Uint8Array(hex.length / 2);
    for (let i = 0; i < bytes.length; i += 1) bytes[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    return new TextDecoder('utf-8', { fatal: false }).decode(bytes);
  };
  return (
    <>
      <Segmented<'hex' | 'text'> label="Show as" value={as} onChange={setAs} options={[{ value: 'hex', label: 'Hex' }, { value: 'text', label: 'UTF-8 text' }]} />
      <pre className={styles.hexDump}>{as === 'hex' ? hexDump(hex) : text()}</pre>
      {hex.length / 2 < length && <p className="ink-3" style={{ fontSize: 12 }}>Showing the first {formatBytes(hex.length / 2)} of {formatBytes(length)}.</p>}
    </>
  );
}
