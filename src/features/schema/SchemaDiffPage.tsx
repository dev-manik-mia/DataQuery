import { useEffect, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { DiffSide } from '@/bindings/DiffSide';
import type { SchemaDiff } from '@/bindings/SchemaDiff';
import { Alert, Button, Card, EmptyState, ErrorAlert, PageHead, SelectField, TextField } from '@/components/ui';
import { SqlViewer } from '@/features/editor/SqlViewer';
import { copyText } from '@/lib/clipboard';
import { defaultSchema, flavorOf, hasSchemas } from '@/lib/engines';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { useConnections } from '@/stores/connections';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Schema.module.css';

function SidePicker({ label, value, onChange }: { label: string; value: DiffSide; onChange: (v: DiffSide) => void }) {
  const connections = useConnections((s) => s.connections);
  const connection = connections.find((c) => c.id === value.connection_id);
  const flavor = connection && flavorOf(connection.config.engine);
  const schemas = flavor && hasSchemas(flavor);
  return (
    <Card title={label}>
      <div style={{ display: 'flex', flexDirection: 'column', gap: 10 }}>
        <SelectField
          label="Connection"
          value={value.connection_id}
          onChange={(e) => {
            const c = connections.find((x) => x.id === e.target.value);
            onChange({ connection_id: e.target.value, database: c?.config.database ?? '', schema: c ? defaultSchema(flavorOf(c.config.engine), c.config.username) : null });
          }}
          options={[{ value: '', label: 'Choose…' }, ...connections.map((c) => ({ value: c.id, label: `${c.name} (${c.environment})` }))]}
        />
        <TextField label={flavor === 'cql' ? 'Keyspace' : 'Database'} mono value={value.database} onChange={(e) => onChange({ ...value, database: e.target.value })} />
        {schemas && <TextField label="Schema" mono value={value.schema ?? ''} onChange={(e) => onChange({ ...value, schema: e.target.value || null })} />}
      </div>
    </Card>
  );
}

const EMPTY: DiffSide = { connection_id: '', database: '', schema: null };

/** Compares two schemas (possibly on different connections) and shows the migration SQL for review. */
export function SchemaDiffPage() {
  const session = useWorkspace((s) => s.session);
  const [source, setSource] = useState<DiffSide>(EMPTY);
  const [target, setTarget] = useState<DiffSide>(EMPTY);
  const [diff, setDiff] = useState<SchemaDiff | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [busy, setBusy] = useState(false);
  const connections = useConnections((s) => s.connections);
  const targetConnection = connections.find((c) => c.id === target.connection_id);

  useEffect(() => {
    if (session && !source.connection_id) {
      const c = session.connection;
      setSource({ connection_id: c.id, database: session.server.database, schema: session.capabilities.schemas ? 'public' : null });
    }
  }, [session, source.connection_id]);

  const compare = async () => {
    setBusy(true);
    setError(null);
    setDiff(null);
    try {
      setDiff(await api.schemaDiff(source, target));
    } catch (e) {
      setError(toAppError(e).info);
    } finally {
      setBusy(false);
    }
  };

  const ready = source.connection_id && target.connection_id && source.database && target.database;
  const changeClass = { added: styles.added, removed: styles.removed, changed: styles.changed } as const;

  return (
    <div className={styles.page}>
      <div className={styles.inner}>
        <PageHead eyebrow="Schema" title="Schema diff" />
        <p className="ink-2">Plans the SQL that makes the target match the source. Nothing runs here: open the SQL in an editor connected to the target, review it, then run it.</p>
        <div className={styles.sides}>
          <SidePicker label="Source (desired)" value={source} onChange={setSource} />
          <Button
            onClick={() => {
              setSource(target);
              setTarget(source);
            }}
            aria-label="Swap source and target"
          >
            ⇄
          </Button>
          <SidePicker label="Target (to change)" value={target} onChange={setTarget} />
        </div>
        <div>
          <Button variant="primary" busy={busy} disabled={!ready} onClick={() => void compare()}>
            Compare
          </Button>
        </div>
        {error && <ErrorAlert error={error} />}
        {diff && diff.items.length === 0 && <EmptyState title="No differences">The target matches the source.</EmptyState>}
        {diff && diff.items.length > 0 && (
          <>
            {diff.warnings.map((w) => (
              <Alert key={w} tone="caution">
                {w}
              </Alert>
            ))}
            <Card title={`${diff.items.length} differences`} flush>
              {diff.items.map((item, i) => (
                <div key={i} className={styles.diffRow}>
                  <span className={changeClass[item.change]}>{item.change}</span>
                  <span className="mono ellipsis">{item.table}</span>
                  <span className="ink-2">{item.kind}</span>
                  <span className="mono ellipsis">{item.kind === 'table' ? '' : item.name}</span>
                  <span className="ink-2 ellipsis" title={item.detail}>
                    {item.detail}
                  </span>
                </div>
              ))}
            </Card>
            <Card
              title="Migration SQL"
              flush
              actions={
                <span style={{ display: 'flex', gap: 6 }}>
                  <Button size="xs" onClick={() => void copyText(diff.sql).then(() => useUi.getState().toast('SQL copied'))}>
                    Copy
                  </Button>
                  {session?.connection.id === target.connection_id && (
                    <Button
                      size="xs"
                      onClick={() => {
                        useWorkspace.getState().openQueryTab({ sql: diff.sql, title: 'schema-diff.sql' });
                        useUi.getState().navigate({ name: 'workspace' });
                      }}
                    >
                      Open in editor
                    </Button>
                  )}
                </span>
              }
              footer={session?.connection.id === target.connection_id ? undefined : `Connect to ${targetConnection?.name ?? 'the target'} to open this SQL in its editor.`}
            >
              <div style={{ height: 320 }}>
                <SqlViewer value={diff.sql} flavor={targetConnection ? flavorOf(targetConnection.config.engine) : 'postgres'} label="Migration SQL" />
              </div>
            </Card>
          </>
        )}
      </div>
    </div>
  );
}
