import { useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import { Button, Dialog, ErrorAlert, TextAreaField, TextField } from '@/components/ui';

export interface FieldSpec {
  name: string;
  label: string;
  initial?: string;
  multiline?: boolean;
  hint?: string;
  placeholder?: string;
}

/**
 * Asks for one or more values. Values are sent exactly as typed (no trimming):
 * Redis strings are binary-safe and spaces matter. `onSubmit` returns the error to show, if any.
 */
export function FieldsDialog({
  title,
  fields,
  confirmLabel,
  onSubmit,
  onClose,
}: {
  title: string;
  fields: FieldSpec[];
  confirmLabel: string;
  onSubmit: (values: Record<string, string>) => Promise<DbError | null>;
  onClose: () => void;
}) {
  const [values, setValues] = useState<Record<string, string>>(() => Object.fromEntries(fields.map((f) => [f.name, f.initial ?? ''])));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<DbError | null>(null);
  const submit = async () => {
    setBusy(true);
    const err = await onSubmit(values);
    setBusy(false);
    if (err) setError(err);
    else onClose();
  };
  return (
    <Dialog
      title={title}
      wide={fields.some((f) => f.multiline)}
      onClose={() => (busy ? undefined : onClose())}
      footer={
        <>
          <Button onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button variant="primary" busy={busy} onClick={() => void submit()}>
            {confirmLabel}
          </Button>
        </>
      }
    >
      {fields.map((f) =>
        f.multiline ? (
          <TextAreaField key={f.name} label={f.label} mono rows={8} hint={f.hint} placeholder={f.placeholder} value={values[f.name]} onChange={(e) => setValues({ ...values, [f.name]: e.target.value })} />
        ) : (
          <TextField
            key={f.name}
            label={f.label}
            mono
            hint={f.hint}
            placeholder={f.placeholder}
            value={values[f.name]}
            onChange={(e) => setValues({ ...values, [f.name]: e.target.value })}
            onKeyDown={(e) => e.key === 'Enter' && void submit()}
          />
        ),
      )}
      {error && <ErrorAlert error={error} title="Not saved" />}
    </Dialog>
  );
}
