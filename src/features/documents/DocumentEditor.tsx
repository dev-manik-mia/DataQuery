import { useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import { Alert, Button, Dialog, ErrorAlert } from '@/components/ui';
import { CodeInput } from '@/features/editor/CodeInput';

/**
 * Insert or edit one document in the engine's own text form (MongoDB shell
 * syntax or DynamoDB JSON), so every type round-trips exactly. Saving is
 * confirmed by the backend when the connection needs it.
 */
export function DocumentEditor({
  title,
  initial,
  format,
  onSave,
  onClose,
}: {
  title: string;
  initial: string;
  format: string;
  onSave: (text: string) => Promise<DbError | null>;
  onClose: () => void;
}) {
  const [text, setText] = useState(initial);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<DbError | null>(null);

  const save = async () => {
    setSaving(true);
    setError(await onSave(text));
    setSaving(false);
  };

  return (
    <Dialog
      title={title}
      wide
      onClose={() => (saving ? undefined : onClose())}
      footer={
        <>
          <Button onClick={onClose} disabled={saving}>
            Cancel
          </Button>
          <Button variant="primary" busy={saving} shortcut="⌘↵" onClick={() => void save()}>
            Save
          </Button>
        </>
      }
    >
      <Alert tone="info">
        {format === 'dynamodb_json'
          ? 'DynamoDB JSON: every attribute names its type, e.g. { "pk": { "S": "a" }, "n": { "N": "1.50" } }.'
          : 'MongoDB shell syntax: ObjectId(\'…\'), ISODate(\'…\'), Long(\'…\'), Decimal128(\'…\') keep their exact types.'}
      </Alert>
      <CodeInput value={initial} onChange={setText} onSubmit={() => void save()} label={title} minHeight={260} />
      {error && <ErrorAlert error={error} title="Not saved" />}
    </Dialog>
  );
}
