import { useEffect, useState } from 'react';

import type { Snippet } from '@/bindings/Snippet';
import { Button, Card, EmptyState, TextAreaField, TextField } from '@/components/ui';
import { api } from '@/lib/ipc';
import { useCompletion } from '@/stores/completion';
import { useUi } from '@/stores/ui';

import styles from './History.module.css';

interface Draft {
  id: string | null;
  name: string;
  prefix: string;
  body: string;
}

const EMPTY: Draft = { id: null, name: '', prefix: '', body: '' };

/** Snippets: SQL templates inserted from autocomplete by typing their prefix. */
export function SnippetsPanel() {
  const snippets = useCompletion((s) => s.snippets);
  const { toastError, ask, toast } = useUi();
  const [draft, setDraft] = useState<Draft>(EMPTY);
  const [busy, setBusy] = useState(false);

  useEffect(() => void useCompletion.getState().loadSnippets(), []);

  const save = async () => {
    setBusy(true);
    try {
      await api.saveSnippet(draft);
      await useCompletion.getState().loadSnippets();
      setDraft(EMPTY);
      toast('Snippet saved', 'positive');
    } catch (e) {
      toastError(e);
    } finally {
      setBusy(false);
    }
  };

  const remove = async (s: Snippet) => {
    if (!(await ask({ title: `Delete snippet ${s.prefix}?`, body: 'It is removed from this computer.', confirmLabel: 'Delete', danger: true }))) return;
    await api.deleteSnippet(s.id).catch(toastError);
    await useCompletion.getState().loadSnippets();
  };

  return (
    <div style={{ padding: 'var(--space-4) var(--space-6)', display: 'flex', flexDirection: 'column', gap: 12 }}>
      <Card title={draft.id ? 'Edit snippet' : 'New snippet'}>
        <div style={{ display: 'grid', gridTemplateColumns: '1fr 140px', gap: 10 }}>
          <TextField label="Name" value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
          <TextField label="Prefix" mono placeholder="sel" value={draft.prefix} onChange={(e) => setDraft({ ...draft, prefix: e.target.value })} />
        </div>
        <TextAreaField label="Body" mono rows={5} placeholder={'SELECT ${1:columns}\nFROM ${2:table}\nWHERE ${3:condition};'} value={draft.body} onChange={(e) => setDraft({ ...draft, body: e.target.value })} />
        <div style={{ display: 'flex', gap: 8, marginTop: 10 }}>
          <Button variant="primary" busy={busy} disabled={!draft.name.trim() || !draft.prefix.trim() || !draft.body} onClick={() => void save()}>
            Save snippet
          </Button>
          {draft.id && <Button onClick={() => setDraft(EMPTY)}>New</Button>}
        </div>
      </Card>
      {snippets.length === 0 && <EmptyState title="No snippets yet">Save templates you type often; they appear in autocomplete.</EmptyState>}
      {snippets.map((s) => (
        <div key={s.id} className={styles.entry} style={{ cursor: 'default' }}>
          <span className={styles.entryHead}>
            <span className="mono">{s.prefix}</span>
            <span className={styles.entryConn}>{s.name}</span>
            <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
              <Button size="xs" onClick={() => setDraft({ id: s.id, name: s.name, prefix: s.prefix, body: s.body })}>
                Edit
              </Button>
              <Button size="xs" variant="plain" onClick={() => void remove(s)}>
                Delete
              </Button>
            </span>
          </span>
          <span className={styles.entrySql}>{s.body}</span>
        </div>
      ))}
    </div>
  );
}

export function SnippetHelp() {
  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: 8 }}>
      <h2 className={styles.detailTitle}>Snippets</h2>
      <p className="ink-2">Type a snippet's prefix in the SQL editor and pick it from autocomplete. Fields written as <span className="mono">{'${1:name}'}</span> become tab stops: press Tab to move to the next one.</p>
      <p className="ink-2">Snippets only insert text. Nothing runs until you press Run.</p>
    </div>
  );
}
