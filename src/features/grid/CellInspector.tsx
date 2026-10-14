import type { Cell } from '@/bindings/Cell';
import { Button, Dialog } from '@/components/ui';
import { cellText, localTime, prettyJson } from '@/lib/cells';
import { copyText } from '@/lib/clipboard';
import { formatBytes } from '@/lib/format';
import { useUi } from '@/stores/ui';

import styles from './Grid.module.css';

/** Full value viewer for JSON, timestamps, long text and binary heads. */
export function CellInspector({ column, typeName, cell, onClose }: { column: string; typeName: string; cell: Cell; onClose: () => void }) {
  const toast = useUi((s) => s.toast);
  const text = cell.kind === 'json' ? prettyJson(cell.value) : cellText(cell);
  const meta: [string, string][] = [
    ['Column', column],
    ['Type', typeName || cell.kind],
  ];
  if (cell.kind === 'temporal' && cell.zoned) meta.push(['Local time', localTime(cell.value) ?? '—']);
  if (cell.kind === 'binary') meta.push(['Size', `${formatBytes(cell.length)} (${cell.length} bytes)`], ['Shown', `first ${cell.head_hex.length / 2} bytes`]);
  if ((cell.kind === 'text' || cell.kind === 'json') && cell.truncated) meta.push(['Note', 'Truncated in the grid at 256 KB. Export to read the full value.']);
  if (cell.kind === 'text' || cell.kind === 'json') meta.push(['Length', `${cell.value.length} characters`]);

  return (
    <Dialog
      title="Value"
      wide
      onClose={onClose}
      footer={
        <>
          <Button
            disabled={text === null}
            onClick={() => {
              if (text !== null) void copyText(text).then(() => toast('Value copied'));
            }}
          >
            Copy
          </Button>
          <Button variant="primary" onClick={onClose}>
            Close
          </Button>
        </>
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
      <pre className={styles.inspectorValue}>{text === null ? 'NULL' : text === '' ? '(empty string)' : text}</pre>
    </Dialog>
  );
}
