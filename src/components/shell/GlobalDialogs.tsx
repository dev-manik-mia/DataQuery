import { useRef, useState } from 'react';

import { Button, Dialog, TextField } from '@/components/ui';
import { ExportDialog } from '@/features/transfer/ExportDialog';
import { useExportRequest } from '@/features/transfer/exportData';
import { useUi } from '@/stores/ui';

import styles from './Shell.module.css';

/** The confirmation required by the backend for destructive and production writes. */
function ConfirmationDialog() {
  const pending = useUi((s) => s.confirmation);
  const [typed, setTyped] = useState('');
  const inputRef = useRef<HTMLInputElement>(null);
  if (!pending) return null;
  const { request, resolve } = pending;
  const needsName = request.require_typed_name;
  const matches = !needsName || typed.trim() === needsName;
  const production = request.environment === 'Production';
  const close = (ok: boolean) => {
    resolve(ok ? { acknowledged: true, typed_name: needsName ? typed.trim() : null } : null);
    setTyped('');
  };
  return (
    <Dialog
      title={request.title}
      critical={production || request.destructive}
      onClose={() => close(false)}
      initialFocus={needsName ? inputRef : undefined}
      footer={
        <>
          <Button onClick={() => close(false)}>Cancel</Button>
          <Button variant={request.destructive ? 'danger' : 'primary'} disabled={!matches} onClick={() => close(true)}>
            {request.destructive ? 'Run anyway' : 'Run on production'}
          </Button>
        </>
      }
    >
      <p>
        On <b className={production ? styles.confirmProduction : undefined}>{request.connection_name}</b>
        {production ? ' (Production)' : ''}:
      </p>
      <ul className={styles.confirmList}>
        {request.reasons.map((r) => (
          <li key={r}>{r}</li>
        ))}
      </ul>
      {needsName && (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            if (matches) close(true);
          }}
        >
          <TextField
            ref={inputRef}
            label={
              <>
                Type <span className={styles.confirmName}>{needsName}</span> to confirm
              </>
            }
            mono
            placeholder={needsName}
            value={typed}
            onChange={(e) => setTyped(e.target.value)}
          />
        </form>
      )}
    </Dialog>
  );
}

/** Simple yes/no question (closing a dirty tab, deleting a connection). */
function QuestionDialog() {
  const question = useUi((s) => s.question);
  if (!question) return null;
  return (
    <Dialog
      title={question.title}
      onClose={() => question.resolve(false)}
      footer={
        <>
          <Button onClick={() => question.resolve(false)}>Cancel</Button>
          <Button variant={question.danger ? 'danger' : 'primary'} onClick={() => question.resolve(true)}>
            {question.confirmLabel}
          </Button>
        </>
      }
    >
      <p>{question.body}</p>
    </Dialog>
  );
}

export function GlobalDialogs() {
  const exporting = useExportRequest((s) => s.request);
  return (
    <>
      {exporting && <ExportDialog key={exporting.baseName + exporting.format} />}
      <ConfirmationDialog />
      <QuestionDialog />
    </>
  );
}
