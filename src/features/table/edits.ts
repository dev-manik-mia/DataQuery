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
  const deleted = new Set(edits.deletes);
  const changedRows = Object.entries(edits.updates).filter(([k, v]) => !deleted.has(k) && Object.keys(v).length > 0);
  const cells = changedRows.reduce((n, [, v]) => n + Object.keys(v).length, 0);
  return { cells, inserts: edits.inserts.length, deletes: edits.deletes.length, total: changedRows.length + edits.inserts.length + edits.deletes.length };
}

/** Whether `value` is what the cell already holds, so there is nothing to change. */
function unchanged(original: Cell | undefined, value: Value): boolean {
  return original !== undefined && (original.kind === 'null' ? value === null : value === cellText(original));
}

export function setCell(edits: Edits, key: string, col: number, value: Value, original: Cell | undefined): Edits {
  const row = { ...(edits.updates[key] ?? {}) };
  if (unchanged(original, value)) delete row[col];
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
  const deleted = new Set(edits.deletes);
  const allDeleted = keys.every((k) => deleted.has(k));
  if (allDeleted) {
    const removed = new Set(keys);
    return { ...edits, deletes: edits.deletes.filter((k) => !removed.has(k)) };
  }
  for (const k of keys) deleted.add(k);
  return { ...edits, deletes: [...deleted] };
}

export function removeInserts(edits: Edits, ids: string[]): Edits {
  const removed = new Set(ids);
  return { ...edits, inserts: edits.inserts.filter((r) => !removed.has(r.id)) };
}

/** What a paste needs to know about the page: rows shown are the pending inserts, then the page rows. */
export interface PasteTarget {
  pageRows: Cell[][];
  /** Row keys of `pageRows`, in order. */
  pageKeys: string[];
  editableColumn: (col: number) => boolean;
}

/**
 * Spreadsheet cells pasted at a position, in one pass: existing rows are
 * edited, extra rows become new rows. Returns the edits and the number of
 * read-only cells skipped.
 */
export function applyPaste(
  edits: Edits,
  at: { row: number; col: number },
  values: string[][],
  target: PasteTarget,
): { edits: Edits; skipped: number } {
  const insertCount = edits.inserts.length;
  const rowCount = insertCount + target.pageRows.length;
  const inserts = edits.inserts.map((r) => ({ ...r, values: { ...r.values } }));
  const updates: Edits['updates'] = { ...edits.updates };
  const added: PendingInsert[] = [];
  let skipped = 0;
  values.forEach((line, r) => {
    const index = at.row + r;
    let insert: PendingInsert | null = null;
    if (index >= rowCount) added.push((insert = { id: uid('new'), values: {} }));
    else if (index < insertCount) insert = inserts[index]!;
    const pageIndex = index - insertCount;
    const key = insert ? '' : target.pageKeys[pageIndex]!;
    const row = insert ? insert.values : { ...(updates[key] ?? {}) };
    line.forEach((value, c) => {
      const col = at.col + c;
      if (!target.editableColumn(col)) {
        skipped += 1;
        return;
      }
      if (!insert && unchanged(target.pageRows[pageIndex]![col], value)) delete row[col];
      else row[col] = value;
    });
    if (insert) return;
    if (Object.keys(row).length) updates[key] = row;
    else delete updates[key];
  });
  // New rows go on top, newest first, as `addInsert` does.
  return { edits: { ...edits, updates, inserts: [...added.reverse(), ...inserts] }, skipped };
}

/** Values for a duplicated row: every editable, non-generated column. */
export function duplicateValues(cells: Cell[], columns: ColumnInfo[]): Record<number, Value> {
  const values: Record<number, Value> = {};
  columns.forEach((c, i) => {
    const cell = cells[i];
    if (c.editable && !c.generated && !c.primary_key && cell && cell.kind !== 'binary' && cell.kind !== 'unsupported')
      values[i] = cellText(cell);
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
  const deleted = new Set(edits.deletes);
  const changes: RowChange[] = [];
  for (const [key, cols] of Object.entries(edits.updates)) {
    if (deleted.has(key) || !known.has(key)) continue;
    const values = Object.entries(cols).map(([c, value]) => ({ column: name(Number(c)), value }));
    if (values.length) changes.push({ op: 'update', key: keyValues(key), values });
  }
  for (const insert of [...edits.inserts].reverse()) {
    changes.push({
      op: 'insert',
      values: Object.entries(insert.values).map(([c, value]) => ({ column: name(Number(c)), value })),
    });
  }
  for (const key of edits.deletes) if (known.has(key)) changes.push({ op: 'delete', key: keyValues(key) });
  return changes;
}

/** Cells to display for a row with its pending edits applied. */
export function applyRow(cells: Cell[], changes: Record<number, Value> | undefined): Cell[] {
  if (!changes) return cells;
  return cells.map((cell, i) => (i in changes ? cellFromInput(changes[i] ?? null, cell) : cell));
}
