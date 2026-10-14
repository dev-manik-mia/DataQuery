import type { Cell } from '@/bindings/Cell';
import type { Document } from '@/bindings/Document';
import type { GridColumn, GridRow } from '@/features/grid/DataGrid';

/** Numeric type labels of MongoDB and DynamoDB (right-aligned in the table view). */
const NUMERIC = new Set(['Int32', 'Int64', 'Double', 'Decimal128', 'N']);

/**
 * Table view of documents: one column per top-level field in order of first
 * appearance, nested values as JSON cells. Mirrors `dq_core::document::to_grid`.
 */
export function documentsGrid(documents: Document[]): { columns: GridColumn[]; rows: GridRow[] } {
  const columns: GridColumn[] = [];
  const index = new Map<string, number>();
  for (const d of documents) {
    for (const f of d.fields) {
      if (!index.has(f.key)) {
        index.set(f.key, columns.length);
        columns.push({ name: f.key, typeName: f.type_label, numeric: NUMERIC.has(f.type_label) });
      }
    }
  }
  const rows = documents.map((d, i) => {
    const cells: Cell[] = columns.map(() => ({ kind: 'null' }));
    for (const f of d.fields) cells[index.get(f.key)!] = f.value;
    return { key: d.key || String(i), cells };
  });
  return { columns, rows };
}

/** One aggregation stage in the pipeline builder. */
export interface Stage {
  id: string;
  text: string;
  enabled: boolean;
}

/** The enabled stages as a pipeline array. */
export function pipelineText(stages: Stage[]): string {
  const body = stages.filter((s) => s.enabled && s.text.trim()).map((s) => s.text.trim());
  return body.length ? `[\n${body.map((s) => indent(s)).join(',\n')}\n]` : '[]';
}

function indent(text: string): string {
  return text
    .split('\n')
    .map((l) => `  ${l}`)
    .join('\n');
}

/** Stage operators offered when adding a stage, with a starting body. */
export const STAGE_TEMPLATES: { op: string; text: string }[] = [
  { op: '$match', text: '{ $match: { } }' },
  { op: '$project', text: '{ $project: { } }' },
  { op: '$group', text: '{ $group: { _id: "$field", count: { $sum: 1 } } }' },
  { op: '$sort', text: '{ $sort: { } }' },
  { op: '$limit', text: '{ $limit: 10 }' },
  { op: '$unwind', text: '{ $unwind: "$field" }' },
  { op: '$lookup', text: '{ $lookup: { from: "", localField: "", foreignField: "", as: "" } }' },
  { op: '$addFields', text: '{ $addFields: { } }' },
  { op: '$count', text: '{ $count: "count" }' },
];

/** Label for the "1–20 of 3,118" range, with `~` for estimates. */
export function rangeLabel(first: number, count: number, total: { count: number; exact: boolean } | null): string {
  if (count === 0) return 'No documents';
  const range = `${(first + 1).toLocaleString()}–${(first + count).toLocaleString()}`;
  if (!total) return range;
  return `${range} of ${total.exact ? '' : '~'}${total.count.toLocaleString()}`;
}
