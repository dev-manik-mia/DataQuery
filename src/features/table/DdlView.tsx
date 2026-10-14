import { useEffect, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { SqlFlavor } from '@/bindings/SqlFlavor';
import { Button, Card, ErrorAlert, Skeleton } from '@/components/ui';
import { SqlViewer } from '@/features/editor/SqlViewer';
import { copyText } from '@/lib/clipboard';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { useUi } from '@/stores/ui';
import { useWorkspace } from '@/stores/workspace';

import styles from './Table.module.css';

function useDdl(object: ObjectRef) {
  const session = useWorkspace((s) => s.session)!;
  const [ddl, setDdl] = useState<string | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  useEffect(() => {
    let stale = false;
    setDdl(null);
    setError(null);
    api
      .tableDdl({ sessionId: session.session_id }, object)
      .then((d) => !stale && setDdl(d))
      .catch((e) => !stale && setError(toAppError(e).info));
    return () => {
      stale = true;
    };
  }, [object, session.session_id]);
  return { ddl, error };
}

/** Generated DDL with Copy and Open in editor. Never executed from here. */
export function DdlCard({ object, flavor, onOpenSql, full }: { object: ObjectRef; flavor: SqlFlavor; onOpenSql: (sql: string) => void; full?: boolean }) {
  const { ddl, error } = useDdl(object);
  const toast = useUi((s) => s.toast);
  return (
    <Card
      title="Generated DDL"
      className={full ? styles.ddlFull : styles.ddlCard}
      flush
      actions={
        <div style={{ display: 'flex', gap: 6 }}>
          <Button size="xs" disabled={!ddl} onClick={() => ddl && void copyText(ddl).then(() => toast('DDL copied'))}>
            Copy
          </Button>
          <Button size="xs" disabled={!ddl} onClick={() => ddl && onOpenSql(ddl)}>
            Open in editor
          </Button>
        </div>
      }
      footer="Read from the system catalog. Schema changes are previewed as SQL and never run without review."
    >
      <div className={styles.ddlBody}>
        {error ? (
          <div style={{ padding: 16 }}>
            <ErrorAlert error={error} />
          </div>
        ) : ddl === null ? (
          <div style={{ padding: 16, display: 'flex', flexDirection: 'column', gap: 8 }}>
            <Skeleton width="60%" />
            <Skeleton width="80%" />
            <Skeleton width="70%" />
          </div>
        ) : (
          <SqlViewer value={ddl} flavor={flavor} label={`DDL of ${object.name}`} />
        )}
      </div>
    </Card>
  );
}
