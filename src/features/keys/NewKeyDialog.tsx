import { useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { KeyType } from '@/bindings/KeyType';
import type { RString } from '@/bindings/RString';
import { Button, Dialog, ErrorAlert, SelectField, TextAreaField, TextField } from '@/components/ui';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { firstEdit, plain, writeKey } from './edits';
import { useKeyBrowser } from './KeyBrowser';
import { TYPE_LABEL, parseTtl } from './keys';

const TYPES: KeyType[] = ['string', 'hash', 'list', 'set', 'zset', 'stream'];

/** Creates a key with its first value; refuses to overwrite an existing key. */
export function NewKeyDialog({ database, onClose, onCreated }: { database: number; onClose: () => void; onCreated: (key: RString) => void }) {
  const session = useWorkspace((s) => s.session)!;
  const toast = useUi((s) => s.toast);
  const reload = useKeyBrowser((s) => s.reload);
  const [type, setType] = useState<KeyType>('string');
  const [name, setName] = useState('');
  const [values, setValues] = useState<Record<string, string>>({ field: '', value: '', score: '0' });
  const [ttl, setTtl] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<DbError | null>(null);
  const set = (k: string, v: string) => setValues({ ...values, [k]: v });
  const needsField = type === 'hash' || type === 'stream';

  const create = async () => {
    const edit = firstEdit(type, values);
    const ttlMs = ttl.trim() ? parseTtl(ttl) : null;
    if (!name) return setError({ kind: 'invalid_input', code: null, message: 'Enter a key name.', detail: null, hint: null, action: null, position: null });
    if (typeof edit === 'string') return setError({ kind: 'invalid_input', code: null, message: edit, detail: null, hint: null, action: null, position: null });
    if (ttl.trim() && ttlMs === null) return setError({ kind: 'invalid_input', code: null, message: 'Enter a TTL such as 90s, 30m or 2h.', detail: null, hint: null, action: null, position: null });
    setBusy(true);
    const key = { database, key: plain(name) };
    let result = await writeKey(session.session_id, key, edit, true);
    if (result.done && ttlMs !== null) result = await writeKey(session.session_id, key, { op: 'expire', ttl_ms: ttlMs });
    setBusy(false);
    if (result.error) return setError(result.error);
    if (!result.done) return;
    toast(`Created ${name}`, 'positive');
    reload();
    onCreated(plain(name));
    onClose();
  };

  return (
    <Dialog
      title={`New key in db ${database}`}
      onClose={() => (busy ? undefined : onClose())}
      footer={
        <>
          <Button onClick={onClose} disabled={busy}>
            Cancel
          </Button>
          <Button variant="primary" busy={busy} onClick={() => void create()}>
            Create
          </Button>
        </>
      }
    >
      <SelectField label="Type" value={type} onChange={(e) => setType(e.target.value as KeyType)} options={TYPES.map((t) => ({ value: t, label: TYPE_LABEL[t] }))} />
      <TextField label="Key" mono placeholder="user:1042" value={name} onChange={(e) => setName(e.target.value)} />
      {needsField && <TextField label="Field" mono value={values.field} onChange={(e) => set('field', e.target.value)} />}
      <TextAreaField label={type === 'set' || type === 'zset' ? 'Member' : type === 'list' ? 'First item' : 'Value'} mono rows={3} value={values.value} onChange={(e) => set('value', e.target.value)} />
      {type === 'zset' && <TextField label="Score" mono value={values.score} onChange={(e) => set('score', e.target.value)} />}
      <TextField label="TTL" mono placeholder="none (e.g. 30m, 2h, 1d)" value={ttl} onChange={(e) => setTtl(e.target.value)} />
      {error && <ErrorAlert error={error} title="Not created" />}
    </Dialog>
  );
}
