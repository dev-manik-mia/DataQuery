import type { KeyRole } from '@/bindings/KeyRole';
import type { NodeKind } from '@/bindings/NodeKind';
import type { ObjectRef } from '@/bindings/ObjectRef';

/** One- or two-letter marker per object kind, used in the explorer, tabs and search. */
export const KIND_MARK: Record<NodeKind, string> = {
  database: 'D',
  schema: 'S',
  table: 'T',
  view: 'V',
  materialized_view: 'M',
  index: 'I',
  sequence: 'Sq',
  trigger: 'Tr',
  function: 'F',
  procedure: 'P',
  key: 'K',
  type: 'U',
  group: '',
};

export const KIND_LABEL: Record<NodeKind, string> = {
  database: 'Database',
  schema: 'Schema',
  table: 'Table',
  view: 'View',
  materialized_view: 'Materialized view',
  index: 'Index',
  sequence: 'Sequence',
  trigger: 'Trigger',
  function: 'Function',
  procedure: 'Procedure',
  key: 'Key',
  type: 'Type',
  group: 'Folder',
};

const GROUP_LABEL: Record<string, string> = { sequences: 'Sequences', functions: 'Functions', procedures: 'Procedures', types: 'Types' };

export function isRelation(o: ObjectRef): boolean {
  return o.kind === 'table' || o.kind === 'view' || o.kind === 'materialized_view';
}

/** Name as shown in the tree: routines with their argument list, groups by title. */
export function objectName(o: ObjectRef): string {
  if (o.kind === 'group') return GROUP_LABEL[o.name] ?? o.name;
  if ((o.kind === 'function' || o.kind === 'procedure') && o.parent != null) return `${o.name}(${o.parent})`;
  return o.name;
}

/** Short label: `schema.table` where the engine has schemas, else `table`. */
export function objectLabel(o: ObjectRef): string {
  const name = objectName(o);
  return o.schema ? `${o.schema}.${name}` : name;
}

/** Full path for tooltips: `database.schema.table` or `database.table`. */
export function objectPath(o: ObjectRef): string {
  return [o.database, o.schema, o.name].filter(Boolean).join('.');
}

/** Header markers for wide-column keys: `PK` for partition columns, `CK ↑`/`CK ↓` for clustering columns. */
export function keyBadges(keys: readonly KeyRole[]): Map<string, string> {
  return new Map(keys.map((k) => [k.column, k.role === 'partition' ? 'PK' : k.descending ? 'CK ↓' : 'CK ↑']));
}
