import { useState, type ReactNode } from 'react';

import { Button } from './Button';
import { Dialog } from './Dialog';
import { TextField } from './Field';

interface PromptDialogProps {
  title: string;
  label: string;
  initial?: string;
  confirmLabel: string;
  hint?: ReactNode;
  mono?: boolean;
  onSubmit: (value: string) => Promise<void> | void;
  onClose: () => void;
}

/** Asks for a single value, e.g. a folder or script name. */
export function PromptDialog({ title, label, initial = '', confirmLabel, hint, mono, onSubmit, onClose }: PromptDialogProps) {
  const [value, setValue] = useState(initial);
  const [busy, setBusy] = useState(false);
  const submit = async () => {
    if (!value.trim()) return;
    setBusy(true);
    try {
      await onSubmit(value.trim());
      onClose();
    } finally {
      setBusy(false);
    }
  };
  return (
    <Dialog
      title={title}
      onClose={onClose}
      footer={
        <>
          <Button onClick={onClose}>Cancel</Button>
          <Button variant="primary" busy={busy} disabled={!value.trim()} onClick={() => void submit()}>
            {confirmLabel}
          </Button>
        </>
      }
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void submit();
        }}
      >
        <TextField label={label} hint={hint} mono={mono} value={value} onChange={(e) => setValue(e.target.value)} autoFocus />
      </form>
    </Dialog>
  );
}
