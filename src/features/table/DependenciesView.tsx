import { useEffect, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { Dependencies } from '@/bindings/Dependencies';
import type { Dependency } from '@/bindings/Dependency';
import type { ObjectRef } from '@/bindings/ObjectRef';
import { Card, EmptyState, ErrorAlert, PageHead, Spinner } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { KIND_MARK, objectLabel } from '@/lib/objects';
import { useWorkspace } from '@/stores/workspace';

import styles from './Table.module.css';

function Edges({ title, edges, empty }: { title: string; edges: Dependency[]; empty: string }) {
  const openTableTab = useWorkspace((s) => s.openTableTab);
  return (
    <Card title={title} actions={<span className="ink-2" style={{ fontSize: 12 }}>{edges.length}</span>} flush>
      {edges.length === 0 && <div className={styles.tableRow}>{empty}</div>}
      {edges.map((d, i) => (
        <button key={i} type="button" className={`${styles.tableRow} ${styles.constraintsLayout} dq-row-button`} onClick={() => openTableTab(d.object, d.object.kind === 'table' || d.object.kind === 'view' ? 'structure' : 'ddl')}>
          <span className={styles.key}>{KIND_MARK[d.object.kind]}</span>
          <span className={`${styles.monoStrong} ellipsis`} style={{ fontWeight: 400 }}>
            {objectLabel(d.object)}
          </span>
          <span className="ink-2">{d.relation}</span>
        </button>
      ))}
    </Card>
  );
}

/** What a table or view depends on, and what depends on it. */
export function DependenciesView({ object }: { object: ObjectRef }) {
  const session = useWorkspace((s) => s.session)!;
  const [deps, setDeps] = useState<Dependencies | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  useEffect(() => {
    let stale = false;
    api
      .dependencies({ sessionId: session.session_id }, object)
      .then((d) => !stale && setDeps(d))
      .catch((e) => !stale && setError(toAppError(e).info));
    return () => {
      stale = true;
    };
  }, [object, session.session_id]);
  if (error) return <div style={{ padding: 24 }}><ErrorAlert error={error} /></div>;
  if (!deps) return <EmptyState title={<Spinner label="Loading" />}>Reading dependencies…</EmptyState>;
  return (
    <div className={styles.structure}>
      <PageHead eyebrow="Dependencies" title={objectLabel(object)} mono />
      <div className={styles.structureGrid}>
        <Edges title="Uses" edges={deps.uses} empty="Depends on nothing else." />
        <Edges title="Used by" edges={deps.used_by} empty="Nothing depends on it." />
      </div>
    </div>
  );
}
