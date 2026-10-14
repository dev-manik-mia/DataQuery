import { useEffect, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { FrameworkMigrations } from '@/bindings/FrameworkMigrations';
import { Button, Card, EmptyState, ErrorAlert, PageHead, Spinner } from '@/components/ui';
import tableStyles from '@/features/table/Table.module.css';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { objectLabel } from '@/lib/objects';
import { useWorkspace, type ContainerTab } from '@/stores/workspace';

import styles from './Schema.module.css';

const STATUS_TONE: Record<string, string> = { failed: 'var(--critical)', dirty: 'var(--critical)', 'rolled back': 'var(--caution)' };

/** Applied migrations of Prisma, Laravel, Django, Rails, Flyway, Knex, Alembic, TypeORM, Sequelize and golang-migrate. */
export function MigrationsView({ tab, active }: { tab: ContainerTab; active: boolean }) {
  const session = useWorkspace((s) => s.session)!;
  const openTableTab = useWorkspace((s) => s.openTableTab);
  const [found, setFound] = useState<FrameworkMigrations[] | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [version, setVersion] = useState(0);

  useEffect(() => {
    let stale = false;
    setFound(null);
    setError(null);
    api
      .frameworkMigrations({ sessionId: session.session_id }, tab.database, tab.schema)
      .then((f) => !stale && setFound(f))
      .catch((e) => !stale && setError(toAppError(e).info));
    return () => {
      stale = true;
    };
  }, [tab.database, tab.schema, session.session_id, version]);

  return (
    <div className={styles.page} style={active ? undefined : { display: 'none' }}>
      <div className={styles.inner}>
        <PageHead
          eyebrow="Framework migrations"
          title={tab.schema ?? tab.database}
          actions={
            <Button size="sm" onClick={() => setVersion((v) => v + 1)}>
              Refresh
            </Button>
          }
        />
        {error && <ErrorAlert error={error} />}
        {!found && !error && <Spinner label="Looking for migration tables" />}
        {found?.length === 0 && (
          <EmptyState title="No migration tables found">
            Looked for Prisma, Laravel, Django, Rails, Flyway, Knex, Alembic, TypeORM, Sequelize and golang-migrate tables.
          </EmptyState>
        )}
        {found?.map((f) => (
          <Card
            key={objectLabel(f.table)}
            title={`${f.framework} · ${f.entries.length}${f.entries.length >= 500 ? '+' : ''} entries`}
            actions={
              <Button size="xs" onClick={() => openTableTab(f.table, 'data')}>
                Open {f.table.name}
              </Button>
            }
            flush
          >
            <div className={`${tableStyles.tableHead}`} style={{ display: 'grid', gridTemplateColumns: 'minmax(0,1.4fr) minmax(0,1fr) 180px 100px 70px', gap: 12 }}>
              <span>Version / migration</span>
              <span>Name / app</span>
              <span>Applied</span>
              <span>Status</span>
              <span>Batch</span>
            </div>
            {f.entries.map((e, i) => (
              <div key={i} className={tableStyles.tableRow} style={{ display: 'grid', gridTemplateColumns: 'minmax(0,1.4fr) minmax(0,1fr) 180px 100px 70px', gap: 12 }}>
                <span className="mono ellipsis">{e.version}</span>
                <span className="mono ellipsis ink-2">{e.name ?? ''}</span>
                <span className="mono ink-2">{e.applied_at ?? ''}</span>
                <span style={{ color: STATUS_TONE[e.status] }}>{e.status}</span>
                <span className="mono ink-2">{e.batch ?? ''}</span>
              </div>
            ))}
          </Card>
        ))}
      </div>
    </div>
  );
}
