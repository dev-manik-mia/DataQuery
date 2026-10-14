/**
 * Pending grid edits for one table page, kept as plain data so they can be
 * previewed, counted and turned into `RowChange`s. Rows are identified by
 * their primary-key values, never by position.
 */
import type { Cell } from '@/bindings/Cell';
import type { ColumnInfo } from '@/bindings/ColumnInfo';
import type { ColumnValue } from '@/bindings/ColumnValue';
import type { RowChange } from '@/bindings/RowChange';
import { cellFromInput, cellText } from '@/lib/cells';
import { uid } from '@/lib/id';

export type Value = string | null;

export interface PendingInsert {
  id: string;
  values: Record<number, Value>;
}

export interface Edits {
  /** rowKey → column index → new value */
  updates: Record<string, Record<number, Value>>;
  inserts: PendingInsert[];
  deletes: string[];
}

export const NO_EDITS: Edits = { updates: {}, inserts: [], deletes: [] };

export interface EditCounts {
  cells: number;
  inserts: number;
  deletes: number;
  total: number;
}

export function rowKey(cells: Cell[], identity: number[]): string {
  return JSON.stringify(identity.map((i) => cellText(cells[i] ?? { kind: 'null' })));
}

export function counts(edits: Edits): EditCounts {
  const changedRows = Object.entries(edits.updates).filter(([k, v]) => !edits.deletes.includes(k) && Object.keys(v).length > 0);
  const cells = changedRows.reduce((n, [, v]) => n + Object.keys(v).length, 0);
  return { cells, inserts: edits.inserts.length, deletes: edits.deletes.length, total: changedRows.length + edits.inserts.length + edits.deletes.length };
}

export function setCell(edits: Edits, key: string, col: number, value: Value, original: Cell | undefined): Edits {
  const row = { ...(edits.updates[key] ?? {}) };
  const same = original !== undefined && (original.kind === 'null' ? value === null : value === cellText(original));
  if (same) delete row[col];
  else row[col] = value;
  const updates = { ...edits.updates, [key]: row };
  if (Object.keys(row).length === 0) delete updates[key];
  return { ...edits, updates };
}

export function setInsertCell(edits: Edits, id: string, col: number, value: Value): Edits {
  return { ...edits, inserts: edits.inserts.map((r) => (r.id === id ? { ...r, values: { ...r.values, [col]: value } } : r)) };
}

export function addInsert(edits: Edits, values: Record<number, Value> = {}): Edits {
  return { ...edits, inserts: [{ id: uid('new'), values }, ...edits.inserts] };
}

export function toggleDeletes(edits: Edits, keys: string[]): Edits {
  const allDeleted = keys.every((k) => edits.deletes.includes(k));
  const deletes = allDeleted ? edits.deletes.filter((k) => !keys.includes(k)) : [...new Set([...edits.deletes, ...keys])];
  return { ...edits, deletes };
}

export function removeInserts(edits: Edits, ids: string[]): Edits {
  return { ...edits, inserts: edits.inserts.filter((r) => !ids.includes(r.id)) };
}

/** Values for a duplicated row: every editable, non-generated column. */
export function duplicateValues(cells: Cell[], columns: ColumnInfo[]): Record<number, Value> {
  const values: Record<number, Value> = {};
  columns.forEach((c, i) => {
    const cell = cells[i];
    if (c.editable && !c.generated && !c.primary_key && cell && cell.kind !== 'binary' && cell.kind !== 'unsupported') values[i] = cellText(cell);
  });
  return values;
}

/** Converts pending edits into the changes sent to the backend. */
export function toChanges(edits: Edits, columns: ColumnInfo[], identity: number[], rows: Cell[][]): RowChange[] {
  const name = (i: number) => columns[i]?.name ?? '';
  const keyValues = (key: string): ColumnValue[] => {
    const values = JSON.parse(key) as Value[];
    return identity.map((col, i) => ({ column: name(col), value: values[i] ?? null }));
  };
  const known = new Set(rows.map((r) => rowKey(r, identity)));
  const changes: RowChange[] = [];
  for (const [key, cols] of Object.entries(edits.updates)) {
    if (edits.deletes.includes(key) || !known.has(key)) continue;
    const values = Object.entries(cols).map(([c, value]) => ({ column: name(Number(c)), value }));
    if (values.length) changes.push({ op: 'update', key: keyValues(key), values });
  }
  for (const insert of [...edits.inserts].reverse()) {
    changes.push({ op: 'insert', values: Object.entries(insert.values).map(([c, value]) => ({ column: name(Number(c)), value })) });
  }
  for (const key of edits.deletes) if (known.has(key)) changes.push({ op: 'delete', key: keyValues(key) });
  return changes;
}

/** Cells to display for a row with its pending edits applied. */
export function applyRow(cells: Cell[], changes: Record<number, Value> | undefined): Cell[] {
  if (!changes) return cells;
  return cells.map((cell, i) => (i in changes ? cellFromInput(changes[i] ?? null, cell) : cell));
}
