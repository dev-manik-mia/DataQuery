import type { SchemaModel } from '@/bindings/SchemaModel';

export const BOX_WIDTH = 230;
export const HEADER_H = 28;
export const ROW_H = 18;
export const MAX_ROWS = 14;
const GAP_X = 90;
const GAP_Y = 50;

export interface Box {
  key: string;
  x: number;
  y: number;
  height: number;
  /** Visible column names, in order. */
  rows: string[];
  hidden: number;
}

export function tableKey(o: { schema: string | null; name: string }): string {
  return `${o.schema ?? ''}.${o.name}`;
}

/**
 * Places tables in a grid, ordered so that tables joined by foreign keys sit
 * near each other (breadth-first from the most connected table).
 */
export function layout(model: SchemaModel): Map<string, Box> {
  const keys = model.tables.map((t) => tableKey(t.object));
  const neighbours = new Map<string, Set<string>>(keys.map((k) => [k, new Set()]));
  for (const fk of model.foreign_keys) {
    const a = tableKey(fk.source);
    const b = tableKey(fk.target);
    neighbours.get(a)?.add(b);
    neighbours.get(b)?.add(a);
  }
  const order: string[] = [];
  const seen = new Set<string>();
  const byDegree = [...keys].sort((a, b) => (neighbours.get(b)?.size ?? 0) - (neighbours.get(a)?.size ?? 0));
  for (const start of byDegree) {
    if (seen.has(start)) continue;
    const queue = [start];
    seen.add(start);
    while (queue.length) {
      const k = queue.shift()!;
      order.push(k);
      for (const n of [...(neighbours.get(k) ?? [])].sort()) {
        if (!seen.has(n)) {
          seen.add(n);
          queue.push(n);
        }
      }
    }
  }
  const columns = Math.max(1, Math.ceil(Math.sqrt(order.length * 1.4)));
  const tables = new Map(model.tables.map((t) => [tableKey(t.object), t]));
  const boxes = new Map<string, Box>();
  let y = 0;
  for (let row = 0; row * columns < order.length; row += 1) {
    const slice = order.slice(row * columns, (row + 1) * columns);
    let rowHeight = 0;
    slice.forEach((key, i) => {
      const t = tables.get(key)!;
      const rows = t.columns.slice(0, MAX_ROWS).map((c) => c.name);
      const hidden = t.columns.length - rows.length;
      const height = HEADER_H + (rows.length + (hidden > 0 ? 1 : 0)) * ROW_H + 8;
      boxes.set(key, { key, x: i * (BOX_WIDTH + GAP_X), y, height, rows, hidden });
      rowHeight = Math.max(rowHeight, height);
    });
    y += rowHeight + GAP_Y;
  }
  return boxes;
}

/** Y of a column's row inside a box (its header when the column is hidden). */
export function rowY(box: Box, column: string | undefined): number {
  const i = column ? box.rows.indexOf(column) : -1;
  return box.y + (i < 0 ? HEADER_H / 2 : HEADER_H + i * ROW_H + ROW_H / 2 + 4);
}
