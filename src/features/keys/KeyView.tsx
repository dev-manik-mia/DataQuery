import { useCallback, useEffect, useMemo, useState, type CSSProperties, type ReactNode } from 'react';
import { useShallow } from 'zustand/react/shallow';

import type { DbError } from '@/bindings/DbError';
import type { KeyEdit } from '@/bindings/KeyEdit';
import type { KeyValue } from '@/bindings/KeyValue';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { RString } from '@/bindings/RString';
import type { ValuePage } from '@/bindings/ValuePage';
import { Button, EmptyState, ErrorAlert, Icon, IconButton, Spinner, TextAreaField } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { formatBytes, formatCount } from '@/lib/format';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import { Console } from './Console';
import { FieldsDialog, type FieldSpec } from './dialogs';
import { plain, writeKey } from './edits';
import { useKeyBrowser } from './KeyBrowser';
import { TYPE_LABEL, formatTtl, keyObject, keyRefOf, parseTtl } from './keys';
import styles from './Keys.module.css';

const PAGE = 200;

const UNIT: Record<KeyValue['type'], [string, string]> = {
  string: ['byte', 'bytes'],
  hash: ['field', 'fields'],
  list: ['item', 'items'],
  set: ['member', 'members'],
  zset: ['member', 'members'],
  stream: ['entry', 'entries'],
  other: ['', ''],
};

/** Merges a following page of elements into the loaded value. */
function append(a: KeyValue, b: KeyValue): KeyValue {
  if (a.type === 'hash' && b.type === 'hash') return { ...a, entries: [...a.entries, ...b.entries] };
  if (a.type === 'list' && b.type === 'list') return { ...a, items: [...a.items, ...b.items] };
  if (a.type === 'set' && b.type === 'set') return { ...a, members: [...a.members, ...b.members] };
  if (a.type === 'zset' && b.type === 'zset') return { ...a, members: [...a.members, ...b.members] };
  if (a.type === 'stream' && b.type === 'stream') return { ...a, entries: [...a.entries, ...b.entries] };
  return b;
}

function Text({ value }: { value: RString }) {
  return <span className={value.binary ? styles.binary : undefined} title={value.binary ? 'Binary (escaped as \\xNN)' : undefined}>{value.text}</span>;
}

type Dialog = { title: string; fields: FieldSpec[]; confirm: string; run: (v: Record<string, string>) => KeyEdit | string | null };

/** Screen 12, main pane: one key's facts, its value (paged and editable) and the console. */
export function KeyView({ object }: { object: ObjectRef }) {
  const session = useWorkspace((s) => s.session)!;
  const { openTableTab, closeTab, tabs } = useWorkspace(useShallow((s) => ({ openTableTab: s.openTableTab, closeTab: s.closeTab, tabs: s.tabs })));
  const toast = useUi((s) => s.toast);
  const reloadBrowser = useKeyBrowser((s) => s.reload);
  const key = useMemo(() => keyRefOf(object), [object]);
  const writable = session.capabilities.grid_edit && !session.server.read_only;

  const [page, setPage] = useState<ValuePage | null>(null);
  const [value, setValue] = useState<KeyValue | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<DbError | null>(null);
  const [version, setVersion] = useState(0);
  const [dialog, setDialog] = useState<Dialog | null>(null);
  const [draft, setDraft] = useState<string | null>(null);

  const load = useCallback(
    async (cursor: string | null) => {
      setLoading(true);
      setError(null);
      try {
        const p = await api.keysValue({ sessionId: session.session_id }, { key, cursor, count: PAGE });
        setPage(p);
        setValue((v) => (cursor && v ? append(v, p.value) : p.value));
        if (!cursor) setDraft(null);
      } catch (e) {
        setError(toAppError(e).info);
      } finally {
        setLoading(false);
      }
    },
    [session.session_id, key],
  );

  useEffect(() => {
    void load(null);
  }, [load, version]);

  /** Applies a change; refreshes this key (and the browser for TTL, rename and delete). */
  const apply = async (edit: KeyEdit, done: string): Promise<DbError | null> => {
    const { error: err, done: ok } = await writeKey(session.session_id, key, edit);
    if (err) return err;
    if (!ok) return null;
    toast(done, 'positive');
    if (edit.op === 'delete' || edit.op === 'rename' || edit.op === 'expire') reloadBrowser();
    if (edit.op === 'delete') {
      const tab = tabs.find((t) => t.kind === 'table' && t.object === object);
      if (tab) void closeTab(tab.id);
      return null;
    }
    if (edit.op === 'rename') {
      openTableTab(keyObject(key.database, edit.new_key), 'data');
      return null;
    }
    setVersion((n) => n + 1);
    return null;
  };

  const ask = (d: Dialog) => setDialog(d);
  const act = (edit: KeyEdit, done: string) =>
    void apply(edit, done).then((err) => {
      if (err) useUi.getState().toastError(err);
    });

  if (error && !page) {
    return (
      <div style={{ padding: 24 }}>
        <ErrorAlert error={error} title={`Could not read ${object.name}`} />
      </div>
    );
  }
  if (!page || !value) return <EmptyState title={<Spinner label="Loading" />}>Reading the key…</EmptyState>;

  const d = page.details;
  const unit = UNIT[value.type];
  const rowActions = (children: ReactNode) => (writable ? <span className={styles.rowActions}>{children}</span> : <span />);
  const edit = (label: string, onClick: () => void) => (
    <IconButton label={label} bare onClick={onClick}>
      <Icon name="edit" size={12} />
    </IconButton>
  );
  const remove = (label: string, onClick: () => void) => (
    <IconButton label={label} bare onClick={onClick}>
      <Icon name="trash" size={12} />
    </IconButton>
  );
  const table = (cols: string, head: string[], rows: ReactNode, add?: ReactNode) => (
    <div className={styles.table} style={{ '--cols': cols } as CSSProperties}>
      <div className={`${styles.row} ${styles.rowHead}`}>
        {head.map((h) => (
          <span key={h}>{h}</span>
        ))}
      </div>
      {rows}
      {writable && add && <div className={styles.addRow}>{add}</div>}
    </div>
  );

  let body: ReactNode;
  switch (value.type) {
    case 'string': {
      const editable = writable && !value.truncated && !value.value.binary;
      body = (
        <>
          <TextAreaField
            label={value.truncated ? `Value (first ${formatBytes(value.value.text.length)}; too large to edit here)` : value.value.binary ? 'Value (binary, escaped as \\xNN; read-only)' : 'Value'}
            mono
            rows={12}
            readOnly={!editable}
            value={draft ?? value.value.text}
            onChange={(e) => setDraft(e.target.value)}
          />
          {editable && draft !== null && draft !== value.value.text && (
            <div style={{ display: 'flex', gap: 8, marginTop: 8 }}>
              <Button onClick={() => setDraft(null)}>Discard</Button>
              <Button variant="primary" onClick={() => act({ op: 'set_string', value: plain(draft) }, 'Value saved')}>
                Save
              </Button>
            </div>
          )}
        </>
      );
      break;
    }
    case 'hash':
      body = table(
        'minmax(0, 0.8fr) minmax(0, 2fr) 56px',
        ['Field', 'Value', ''],
        value.entries.map((e) => (
          <div key={e.field.text} className={styles.row}>
            <span className={`${styles.cell} ${styles.cellKey}`}>
              <Text value={e.field} />
            </span>
            <span className={styles.cell}>
              <Text value={e.value} />
            </span>
            {rowActions(
              <>
                {!e.value.binary && !e.field.binary && edit(`Edit ${e.field.text}`, () => ask({ title: `Edit ${e.field.text}`, confirm: 'Save', fields: [{ name: 'value', label: 'Value', initial: e.value.text, multiline: true }], run: (v) => ({ op: 'hash_set', field: e.field, value: plain(v.value ?? '') }) }))}
                {remove(`Delete field ${e.field.text}`, () => act({ op: 'hash_delete', field: e.field }, `Deleted ${e.field.text}`))}
              </>,
            )}
          </div>
        )),
        <Button size="xs" onClick={() => ask({ title: 'Add field', confirm: 'Add', fields: [{ name: 'field', label: 'Field' }, { name: 'value', label: 'Value', multiline: true }], run: (v) => (v.field ? { op: 'hash_set', field: plain(v.field), value: plain(v.value ?? '') } : 'Enter a field name.') })}>
          + Field
        </Button>,
      );
      break;
    case 'list':
      body = table(
        '70px minmax(0, 1fr) 56px',
        ['Index', 'Value', ''],
        value.items.map((it) => (
          <div key={it.index} className={styles.row}>
            <span className={`${styles.cell} tabular`}>{it.index}</span>
            <span className={styles.cell}>
              <Text value={it.value} />
            </span>
            {rowActions(
              <>
                {!it.value.binary && edit(`Edit item ${it.index}`, () => ask({ title: `Edit item ${it.index}`, confirm: 'Save', fields: [{ name: 'value', label: 'Value', initial: it.value.text, multiline: true }], run: (v) => ({ op: 'list_set', index: it.index, expected: it.value, value: plain(v.value ?? '') }) }))}
                {remove(`Remove item ${it.index}`, () => act({ op: 'list_remove', index: it.index, expected: it.value }, `Removed item ${it.index}`))}
              </>,
            )}
          </div>
        )),
        <>
          <Button size="xs" onClick={() => ask({ title: 'Push to head', confirm: 'Push', fields: [{ name: 'value', label: 'Value', multiline: true }], run: (v) => ({ op: 'list_push', value: plain(v.value ?? ''), head: true }) })}>
            + Head (LPUSH)
          </Button>
          <Button size="xs" onClick={() => ask({ title: 'Push to tail', confirm: 'Push', fields: [{ name: 'value', label: 'Value', multiline: true }], run: (v) => ({ op: 'list_push', value: plain(v.value ?? ''), head: false }) })}>
            + Tail (RPUSH)
          </Button>
        </>,
      );
      break;
    case 'set':
      body = table(
        'minmax(0, 1fr) 30px',
        ['Member', ''],
        value.members.map((m) => (
          <div key={m.text} className={styles.row}>
            <span className={styles.cell}>
              <Text value={m} />
            </span>
            {rowActions(remove(`Remove ${m.text}`, () => act({ op: 'set_remove', member: m }, `Removed ${m.text}`)))}
          </div>
        )),
        <Button size="xs" onClick={() => ask({ title: 'Add member', confirm: 'Add', fields: [{ name: 'member', label: 'Member' }], run: (v) => ({ op: 'set_add', member: plain(v.member ?? '') }) })}>
          + Member
        </Button>,
      );
      break;
    case 'zset':
      body = table(
        'minmax(0, 1fr) 140px 56px',
        ['Member', 'Score', ''],
        value.members.map((m) => (
          <div key={m.member.text} className={styles.row}>
            <span className={styles.cell}>
              <Text value={m.member} />
            </span>
            <span className={`${styles.cell} tabular`} style={{ textAlign: 'right' }}>
              {m.score}
            </span>
            {rowActions(
              <>
                {edit(`Change score of ${m.member.text}`, () => ask({ title: `Score of ${m.member.text}`, confirm: 'Save', fields: [{ name: 'score', label: 'Score', initial: m.score }], run: (v) => ({ op: 'zset_add', member: m.member, score: (v.score ?? '').trim() }) }))}
                {remove(`Remove ${m.member.text}`, () => act({ op: 'zset_remove', member: m.member }, `Removed ${m.member.text}`))}
              </>,
            )}
          </div>
        )),
        <Button size="xs" onClick={() => ask({ title: 'Add member', confirm: 'Add', fields: [{ name: 'member', label: 'Member' }, { name: 'score', label: 'Score', initial: '0' }], run: (v) => ({ op: 'zset_add', member: plain(v.member ?? ''), score: (v.score ?? '0').trim() }) })}>
          + Member
        </Button>,
      );
      break;
    case 'stream':
      body = table(
        '190px minmax(0, 1fr) 30px',
        ['ID', 'Fields', ''],
        value.entries.map((e) => (
          <div key={e.id} className={styles.row}>
            <span className={`${styles.cell} tabular`}>{e.id}</span>
            <span className={styles.cell}>
              {e.fields.map((f, i) => (
                <div key={i}>
                  <span className={styles.cellKey}>
                    <Text value={f.field} />
                  </span>{' '}
                  <Text value={f.value} />
                </div>
              ))}
            </span>
            {rowActions(remove(`Delete entry ${e.id}`, () => act({ op: 'stream_delete', id: e.id }, `Deleted ${e.id}`)))}
          </div>
        )),
        <Button
          size="xs"
          onClick={() =>
            ask({
              title: 'Add entry (XADD *)',
              confirm: 'Add',
              fields: [{ name: 'fields', label: 'Fields', multiline: true, placeholder: 'field value\nother value', hint: 'One “field value” pair per line; the value is the rest of the line.' }],
              run: (v) => {
                const pairs = (v.fields ?? '')
                  .split('\n')
                  .filter((l) => l.trim())
                  .map((l) => {
                    const at = l.indexOf(' ');
                    return [plain(at < 0 ? l : l.slice(0, at)), plain(at < 0 ? '' : l.slice(at + 1))] as [RString, RString];
                  });
                return pairs.length ? { op: 'stream_add', fields: pairs } : 'Enter at least one field.';
              },
            })
          }
        >
          + Entry
        </Button>,
      );
      break;
    case 'other':
      body = value.text ? <pre className={styles.cell}>{value.text}</pre> : <p className="ink-3">{d.type_name} values are shown in the console only.</p>;
      break;
  }

  return (
    <div className={styles.view}>
      <div className={styles.head}>
        <div style={{ minWidth: 0 }}>
          <div className={styles.eyebrow}>
            {TYPE_LABEL[d.key_type]}
            {d.length !== null && unit[0] && ` · ${formatCount(d.length)} ${d.length === 1 ? unit[0] : unit[1]}`}
          </div>
          <div className={styles.keyName}>
            <Text value={d.key} />
          </div>
        </div>
        <div className={styles.facts}>
          <span>
            TTL <strong>{formatTtl(d.ttl_ms)}</strong>
          </span>
          {d.memory_bytes !== null && (
            <span>
              Memory <strong>{formatBytes(d.memory_bytes)}</strong>
            </span>
          )}
          {d.encoding && (
            <span>
              Encoding <strong>{d.encoding}</strong>
            </span>
          )}
          {writable && (
            <>
              <Button size="xs" onClick={() => ask({ title: 'Time to live', confirm: 'Save', fields: [{ name: 'ttl', label: 'TTL', initial: '', placeholder: '90s, 30m, 2h, 1d — empty removes the expiry' }], run: (v) => (v.ttl?.trim() ? (parseTtl(v.ttl) === null ? 'Enter a TTL such as 90s, 30m or 2h.' : { op: 'expire', ttl_ms: parseTtl(v.ttl) }) : { op: 'expire', ttl_ms: null }) })}>
                Edit TTL
              </Button>
              <Button size="xs" onClick={() => ask({ title: 'Rename key', confirm: 'Rename', fields: [{ name: 'name', label: 'New name', initial: d.key.binary ? '' : d.key.text, hint: 'Refuses to overwrite an existing key (RENAMENX).' }], run: (v) => (v.name ? { op: 'rename', new_key: plain(v.name) } : 'Enter the new name.') })}>
                Rename
              </Button>
              <Button size="xs" variant="danger" onClick={() => act({ op: 'delete' }, `Deleted ${d.key.text}`)}>
                Delete…
              </Button>
            </>
          )}
          <IconButton label="Reload key" onClick={() => setVersion((n) => n + 1)}>
            <Icon name="refresh" size={12} />
          </IconButton>
        </div>
      </div>
      <div className={styles.body}>
        {error && <ErrorAlert error={error} title="Could not read more" />}
        {body}
        {page.next && (
          <div style={{ marginTop: 10 }}>
            <Button size="sm" busy={loading} onClick={() => void load(page.next)}>
              Load more {unit[1]}
            </Button>
          </div>
        )}
      </div>
      <Console />
      {dialog && (
        <FieldsDialog
          title={dialog.title}
          fields={dialog.fields}
          confirmLabel={dialog.confirm}
          onClose={() => setDialog(null)}
          onSubmit={async (v) => {
            const edit = dialog.run(v);
            if (edit === null) return null;
            if (typeof edit === 'string') return { kind: 'invalid_input', code: null, message: edit, detail: null, hint: null, action: null, position: null };
            return apply(edit, 'Saved');
          }}
        />
      )}
    </div>
  );
}
