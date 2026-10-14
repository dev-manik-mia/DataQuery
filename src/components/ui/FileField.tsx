import { open } from '@tauri-apps/plugin-dialog';
import type { ReactNode } from 'react';

import { Button } from './Button';
import { FieldShell } from './Field';

interface FileFieldProps {
  label: ReactNode;
  value: string | null;
  onChange: (path: string | null) => void;
  hint?: ReactNode;
  error?: ReactNode;
  filters?: { name: string; extensions: string[] }[];
  className?: string;
}

/** A path chosen with the native file dialog. Only the path is kept, never the contents. */
export function FileField({ label, value, onChange, hint, error, filters, className }: FileFieldProps) {
  const browse = async () => {
    const chosen = await open({ multiple: false, directory: false, filters });
    if (typeof chosen === 'string') onChange(chosen);
  };
  return (
    <FieldShell label={label} hint={hint} error={error} className={className}>
      {(id, describedBy) => (
        <span style={{ display: 'flex', gap: 6 }}>
          <input id={id} className="dq-input dq-input--mono" readOnly value={value ?? ''} placeholder="No file" aria-describedby={describedBy} />
          <Button size="sm" onClick={() => void browse()}>
            Choose…
          </Button>
          {value && (
            <Button size="sm" onClick={() => onChange(null)}>
              Clear
            </Button>
          )}
        </span>
      )}
    </FieldShell>
  );
}
