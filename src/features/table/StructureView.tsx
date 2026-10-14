import type { SqlFlavor } from '@/bindings/SqlFlavor';
import type { TableStructure } from '@/bindings/TableStructure';
import { Card, PageHead, PlannedChip, Segmented } from '@/components/ui';

import { DdlCard } from './DdlView';
import styles from './Table.module.css';

const CONSTRAINT_LABEL = { primary_key: 'PK', foreign_key: 'FK', unique: 'UNIQUE', check: 'CHECK', exclusion: 'EXCL' } as const;

/** Columns, constraints, indexes and generated DDL (screen 05). */
export function StructureView({ structure, flavor, onOpenSql }: { structure: TableStructure; flavor: SqlFlavor; onOpenSql: (sql: string) => void }) {
  const { columns, constraints, indexes, object } = structure;
  return (
    <div className={styles.structure}>
      <PageHead
        eyebrow={`${object.kind.replace('_', ' ')}${object.schema ? ` · ${object.schema}` : ''}`}
        title={object.name}
        mono
        actions={
          <>
            <Segmented label="ER diagram" value="none" onChange={() => undefined} options={[{ value: 'er', label: 'ER diagram', disabled: true }]} />
            <PlannedChip priority="P1" />
          </>
        }
      />
      {structure.comment && <p className="ink-2">{structure.comment}</p>}
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
            {columns.map((c) => (
              <div key={c.name} className={`${styles.tableRow} ${styles.columnsLayout}`} title={c.comment ?? undefined}>
                <span className={`${styles.monoStrong} ellipsis`}>{c.name}</span>
                <span className={`${styles.monoMuted} ellipsis`}>{c.data_type}</span>
                <span className="ink-2">{c.nullable ? 'yes' : 'no'}</span>
                <span className={`${styles.monoMuted} ellipsis`}>{c.default ?? (c.generated ? 'generated' : '')}</span>
                <span className={styles.key}>{c.primary_key ? 'PK' : ''}</span>
              </div>
            ))}
          </Card>
          <Card title="Constraints" flush>
            {constraints.length === 0 && <div className={styles.tableRow}>No constraints.</div>}
            {constraints.map((k) => (
              <div key={k.name} className={`${styles.tableRow} ${styles.constraintsLayout}`}>
                <span className={styles.key}>{CONSTRAINT_LABEL[k.kind]}</span>
                <span className={`${styles.monoStrong} ellipsis`} style={{ fontWeight: 400 }}>
                  {k.name}
                </span>
                <span className={styles.monoMuted}>{k.definition}</span>
              </div>
            ))}
          </Card>
          {indexes.length > 0 && (
            <Card title="Indexes" flush>
              {indexes.map((i) => (
                <div key={i.name} className={`${styles.tableRow} ${styles.indexesLayout}`}>
                  <span className={`${styles.monoStrong} ellipsis`} style={{ fontWeight: 400 }}>
                    {i.name}
                    {i.primary ? ' · primary' : i.unique ? ' · unique' : ''}
                  </span>
                  <span className={styles.monoMuted}>{i.definition}</span>
                </div>
              ))}
            </Card>
          )}
        </div>
        <DdlCard object={object} flavor={flavor} onOpenSql={onOpenSql} />
      </div>
    </div>
  );
}
