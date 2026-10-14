import { useState } from 'react';

import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { TableStructure } from '@/bindings/TableStructure';
import { Button, Card, Icon, IconButton, Menu, PageHead, useContextMenu, type MenuEntry } from '@/components/ui';
import { StructureEditDialog, type StructureEdit } from '@/features/schema/forms';
import { formatBytes, formatCount } from '@/lib/format';
import { objectLabel } from '@/lib/objects';
import { useWorkspace } from '@/stores/workspace';

import { DdlCard } from './DdlView';
import styles from './Table.module.css';

const CONSTRAINT_LABEL = { primary_key: 'PK', foreign_key: 'FK', unique: 'UNIQUE', check: 'CHECK', exclusion: 'EXCL' } as const;

/** Columns, constraints, indexes, sizes and generated DDL (screen 05), with schema-change forms. */
export function StructureView({
  structure,
  flavor,
  readOnly,
  onOpenSql,
  onChanged,
}: {
  structure: TableStructure;
  flavor: SqlFlavor;
  readOnly: boolean;
  onOpenSql: (sql: string) => void;
  onChanged: () => void;
}) {
  const { columns, constraints, indexes, object, size } = structure;
  const [edit, setEdit] = useState<StructureEdit | null>(null);
  const openContainerTab = useWorkspace((s) => s.openContainerTab);
  const openTableTab = useWorkspace((s) => s.openTableTab);
  const columnMenu = useContextMenu<TableStructure['columns'][number]>();
  const editable = object.kind === 'table' && !readOnly;
  const fkFor = (column: string) => structure.foreign_keys.find((f) => f.columns.includes(column));

  const columnActions = (c: TableStructure['columns'][number]): MenuEntry[] => [
    { label: 'Edit column…', onSelect: () => setEdit({ kind: 'alter-column', column: c }) },
    { label: 'Rename column…', onSelect: () => setEdit({ kind: 'rename-column', column: c }) },
    { type: 'separator' },
    { label: 'Drop column…', danger: true, onSelect: () => setEdit({ kind: 'drop-column', column: c }) },
  ];

  return (
    <div className={styles.structure}>
      <PageHead
        eyebrow={`${object.kind.replace('_', ' ')}${object.schema ? ` · ${object.schema}` : ''}`}
        title={object.name}
        mono
        actions={
          <>
            <Button size="sm" icon={<Icon name="diagram" />} onClick={() => openContainerTab('diagram', object.database, object.schema)}>
              ER diagram
            </Button>
            {editable && (
              <Button size="sm" onClick={() => setEdit({ kind: 'add-column' })}>
                Add column
              </Button>
            )}
          </>
        }
      />
      {structure.comment && <p className="ink-2">{structure.comment}</p>}
      {size && (
        <p className="ink-2" style={{ fontSize: 12 }}>
          {formatBytes(size.total_bytes)} total · {formatBytes(size.table_bytes)} data · {formatBytes(size.index_bytes)} indexes
          {structure.row_estimate !== null && ` · ≈${formatCount(structure.row_estimate)} rows`}
        </p>
      )}
      <div className={styles.structureGrid}>
        <div className={styles.stack}>
          <Card title="Columns" actions={<span className="ink-2" style={{ fontSize: 12 }}>{columns.length}</span>} flush>
            <div className={`${styles.tableHead} ${styles.columnsLayout}`}>
              <span>Name</span>
              <span>Type</span>
              <span>Null</span>
              <span>Default</span>
              <span>Key</span>
            </div>
            {columns.map((c) => {
              const fk = fkFor(c.name);
              return (
                <div
                  key={c.name}
                  className={`${styles.tableRow} ${styles.columnsLayout}`}
                  title={c.comment ?? undefined}
                  onContextMenu={(e) => editable && columnMenu.open(e, c)}
                  onDoubleClick={() => editable && setEdit({ kind: 'alter-column', column: c })}
                >
                  <span className={`${styles.monoStrong} ellipsis`}>{c.name}</span>
                  <span className={`${styles.monoMuted} ellipsis`}>{c.data_type}</span>
                  <span className="ink-2">{c.nullable ? 'yes' : 'no'}</span>
                  <span className={`${styles.monoMuted} ellipsis`}>{c.default ?? (c.identity ? 'identity' : c.generated ? 'generated' : '')}</span>
                  <span className={styles.key}>
                    {c.primary_key ? 'PK' : ''}
                    {fk && (
                      <button type="button" className="dq-status-link" title={`References ${objectLabel(fk.target)}`} onClick={() => openTableTab(fk.target, 'structure')}>
                        FK
                      </button>
                    )}
                  </span>
                </div>
              );
            })}
          </Card>
          <Card
            title="Constraints"
            flush
            actions={
              editable && (
                <span style={{ display: 'flex', gap: 6 }}>
                  <Button size="xs" onClick={() => setEdit({ kind: 'add-foreign-key' })}>
                    + FK
                  </Button>
                  <Button size="xs" onClick={() => setEdit({ kind: 'add-unique' })}>
                    + Unique
                  </Button>
                  <Button size="xs" onClick={() => setEdit({ kind: 'add-check' })}>
                    + Check
                  </Button>
                </span>
              )
            }
          >
            {constraints.length === 0 && <div className={styles.tableRow}>No constraints.</div>}
            {constraints.map((k) => (
              <div key={k.name} className={`${styles.tableRow} ${styles.constraintsLayout}`}>
                <span className={styles.key}>{CONSTRAINT_LABEL[k.kind]}</span>
                <span className={`${styles.monoStrong} ellipsis`} style={{ fontWeight: 400 }}>
                  {k.name}
                </span>
                <span className={styles.monoMuted} style={{ display: 'flex', gap: 6, alignItems: 'start' }}>
                  <span style={{ flex: 1 }}>{k.definition}</span>
                  {editable && (
                    <IconButton label={`Drop ${k.name}`} bare onClick={() => setEdit({ kind: 'drop-constraint', name: k.name, constraint: k.kind })}>
                      <Icon name="close" size={10} />
                    </IconButton>
                  )}
                </span>
              </div>
            ))}
          </Card>
          <Card
            title="Indexes"
            flush
            actions={
              editable && (
                <Button size="xs" onClick={() => setEdit({ kind: 'add-index' })}>
                  + Index
                </Button>
              )
            }
          >
            {indexes.length === 0 && <div className={styles.tableRow}>No indexes.</div>}
            {indexes.map((i) => (
              <div key={i.name} className={`${styles.tableRow} ${styles.indexesLayout}`}>
                <span className={`${styles.monoStrong} ellipsis`} style={{ fontWeight: 400 }}>
                  {i.name}
                  {i.primary ? ' · primary' : i.unique ? ' · unique' : ''}
                  {i.size_bytes !== null && <span className="ink-3"> · {formatBytes(i.size_bytes)}</span>}
                </span>
                <span className={styles.monoMuted} style={{ display: 'flex', gap: 6, alignItems: 'start' }}>
                  <span style={{ flex: 1 }}>{i.definition}</span>
                  {editable && !i.primary && !constraints.some((c) => c.name === i.name) && (
                    <IconButton label={`Drop ${i.name}`} bare onClick={() => setEdit({ kind: 'drop-index', name: i.name })}>
                      <Icon name="close" size={10} />
                    </IconButton>
                  )}
                </span>
              </div>
            ))}
          </Card>
        </div>
        <DdlCard object={object} flavor={flavor} onOpenSql={onOpenSql} />
      </div>
      {columnMenu.menu && <Menu x={columnMenu.menu.x} y={columnMenu.menu.y} items={columnActions(columnMenu.menu.target)} onClose={columnMenu.close} label="Column actions" />}
      {edit && <StructureEditDialog edit={edit} structure={structure} flavor={flavor} onClose={() => setEdit(null)} onApplied={onChanged} />}
    </div>
  );
}
