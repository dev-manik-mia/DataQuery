import type { ObjectRef } from '@/bindings/ObjectRef';

/** Short label: `schema.table` where the engine has schemas, else `table`. */
export function objectLabel(o: ObjectRef): string {
  return o.schema ? `${o.schema}.${o.name}` : o.name;
}

/** Full path for tooltips: `database.schema.table` or `database.table`. */
export function objectPath(o: ObjectRef): string {
  return [o.database, o.schema, o.name].filter(Boolean).join('.');
}
