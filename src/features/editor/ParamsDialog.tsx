import { useRef, useState } from 'react';

import type { SqlFlavor } from '@/bindings/SqlFlavor';
import { Button, Checkbox, Dialog } from '@/components/ui';

interface ParamsDialogProps {
  count: number;
  flavor: SqlFlavor;
  initial: string[];
  sql: string;
  onRun: (values: (string | null)[]) => void;
  onClose: () => void;
}

/** Values for `$1…$n` (PostgreSQL) or `?` (MySQL). They are sent as bound parameters, never pasted into the SQL. */
export function ParamsDialog({ count, flavor, initial, sql, onRun, onClose }: ParamsDialogProps) {
  const [values, setValues] = useState(() => Array.from({ length: count }, (_, i) => initial[i] ?? ''));
  const [nulls, setNulls] = useState(() => Array.from({ length: count }, () => false));
  const first = useRef<HTMLInputElement>(null);
  const run = () => onRun(values.map((v, i) => (nulls[i] ? null : v)));
  return (
    <Dialog
      title={`Parameters (${count})`}
      onClose={onClose}
      initialFocus={first}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" onClick={run}>
            Run
          </Button>
        </>
      }
    >
      <pre className="mono ink-2" style={{ maxHeight: 90, overflow: 'auto', margin: '0 0 10px', fontSize: 12, whiteSpace: 'pre-wrap' }}>
        {sql.trim().slice(0, 600)}
      </pre>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          run();
        }}
        style={{ display: 'grid', gridTemplateColumns: 'auto 1fr auto', gap: '8px 10px', alignItems: 'center' }}
      >
        {values.map((v, i) => (
          <div key={i} style={{ display: 'contents' }}>
            <label className="mono" htmlFor={`dq-param-${i}`}>
              {flavor === 'postgres' ? `$${i + 1}` : `?${i + 1}`}
            </label>
            <input
              id={`dq-param-${i}`}
              ref={i === 0 ? first : undefined}
              className="dq-input dq-input--mono"
              value={v}
              disabled={nulls[i]}
              onChange={(e) => setValues((vs) => vs.map((x, j) => (j === i ? e.target.value : x)))}
            />
            <Checkbox label="NULL" checked={nulls[i] ?? false} onChange={(on) => setNulls((ns) => ns.map((x, j) => (j === i ? on : x)))} />
          </div>
        ))}
        <button type="submit" hidden />
      </form>
      <p className="ink-3" style={{ fontSize: 12, marginTop: 10 }}>
        Values are sent as text parameters; the server converts them to each placeholder's type.
      </p>
    </Dialog>
  );
}
