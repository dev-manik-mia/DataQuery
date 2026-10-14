import { useCallback, useEffect, useState } from 'react';

import type { DbError } from '@/bindings/DbError';
import type { TableStructure } from '@/bindings/TableStructure';
import { EmptyState, ErrorAlert, Segmented, Spinner } from '@/components/ui';
import { toAppError } from '@/lib/errors';
import { api } from '@/lib/ipc';
import { useWorkspace, type TableTab, type TableView } from '@/stores/workspace';

import { KIND_LABEL, isRelation, objectLabel } from '@/lib/objects';

import { DataView } from './DataView';
import { DependenciesView } from './DependenciesView';
import { DdlCard } from './DdlView';
import { StructureView } from './StructureView';
import styles from './Table.module.css';

/** Data / Structure / DDL for one table or view. */
export function TableTabView({ tab, active }: { tab: TableTab; active: boolean }) {
  const session = useWorkspace((s) => s.session)!;
  const { setTableView, openQueryTab } = useWorkspace();
  const [structure, setStructure] = useState<TableStructure | null>(null);
  const [error, setError] = useState<DbError | null>(null);
  const [version, setVersion] = useState(0);

  const relation = isRelation(tab.object);

  useEffect(() => {
    if (!relation) return;
    let stale = false;
    setError(null);
    api
      .tableStructure({ sessionId: session.session_id }, tab.object)
      .then((s) => !stale && setStructure(s))
      .catch((e) => !stale && setError(toAppError(e).info));
    return () => {
      stale = true;
    };
  }, [tab.object, session.session_id, version, relation]);

  const reloadStructure = useCallback(() => setVersion((v) => v + 1), []);
  const openSql = (sql: string) => openQueryTab({ sql, title: `${tab.object.name}.sql` });

  const switcher = (
    <Segmented<TableView>
      label="View"
      value={tab.view}
      onChange={(v) => setTableView(tab.id, v)}
      options={[
        { value: 'data', label: 'Data' },
        { value: 'structure', label: 'Structure' },
        { value: 'ddl', label: 'DDL' },
        { value: 'dependencies', label: 'Dependencies' },
      ]}
    />
  );

  if (!relation) {
    // Indexes, sequences, triggers and routines: their definition only.
    return (
      <div className={styles.tab} style={active ? undefined : { display: 'none' }}>
        <DdlCard object={tab.object} flavor={session.flavor} onOpenSql={openSql} full title={`${KIND_LABEL[tab.object.kind]} ${objectLabel(tab.object)}`} />
      </div>
    );
  }

  return (
    <div className={styles.tab} style={active ? undefined : { display: 'none' }}>
      {tab.view !== 'data' && (
        <div className={styles.toolbar} role="toolbar" aria-label="Table">
          {switcher}
        </div>
      )}
      {error ? (
        <div style={{ padding: 24 }}>
          <ErrorAlert error={error} title={`Could not open ${tab.object.name}`} />
        </div>
      ) : !structure ? (
        <EmptyState title={<Spinner label="Loading" />}>Reading the catalog…</EmptyState>
      ) : tab.view === 'data' ? (
        <DataView
          object={tab.object}
          structure={structure}
          onReloadStructure={reloadStructure}
          leading={switcher}
          matches={tab.matches ?? []}
          onClearMatches={() => useWorkspace.getState().setTableMatches(tab.id, undefined)}
        />
      ) : tab.view === 'structure' ? (
        <StructureView structure={structure} flavor={session.flavor} readOnly={session.server.read_only} onOpenSql={openSql} onChanged={reloadStructure} />
      ) : tab.view === 'dependencies' ? (
        <DependenciesView object={tab.object} />
      ) : (
        <DdlCard object={tab.object} flavor={session.flavor} onOpenSql={openSql} full />
      )}
    </div>
  );
}

