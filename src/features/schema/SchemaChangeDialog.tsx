import type { ReactNode } from 'react';

import type { SchemaChange } from '@/bindings/SchemaChange';
import { api } from '@/lib/ipc';
import { useWorkspace } from '@/stores/workspace';

import { SqlReviewDialog } from './SqlReviewDialog';

interface SchemaChangeDialogProps {
  title: string;
  /** The changes the form describes, or a message saying what is missing. */
  changes: SchemaChange[] | string;
  children?: ReactNode;
  onClose: () => void;
  onApplied: () => void;
  /** Skip the form step (drops): go straight to the reviewed SQL. */
  direct?: boolean;
}

/** Schema changes through the shared form → reviewed SQL → apply flow. */
export function SchemaChangeDialog({ title, changes, children, onClose, onApplied, direct }: SchemaChangeDialogProps) {
  const sid = { sessionId: useWorkspace((s) => s.session)!.session_id };
  const ready = Array.isArray(changes);
  return (
    <SqlReviewDialog
      title={title}
      missing={ready ? null : changes}
      preview={() => api.schemaChangePreview(sid, ready ? changes : [])}
      apply={(c) => api.applySchemaChanges(sid, ready ? changes : [], c)}
      done={(report) => `Applied ${report.statements} statement${report.statements === 1 ? '' : 's'}.`}
      onClose={onClose}
      onApplied={onApplied}
      direct={direct}
    >
      {children}
    </SqlReviewDialog>
  );
}
