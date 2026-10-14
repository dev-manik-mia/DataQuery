import type { Cell } from '@/bindings/Cell';

import { cellText } from './cells';

export type CopyFormat = 'tsv' | 'csv' | 'json';

function csvField(value: string | null): string {
  if (value === null) return '';
  return /[",\n\r]/.test(value) ? `"${value.replace(/"/g, '""')}"` : value;
}

function tsvField(value: string | null): string {
  if (value === null) return '';
  return value.replace(/[\t\n\r]/g, ' ');
}

function jsonValue(cell: Cell): unknown {
  if (cell.kind === 'null') return null;
  if (cell.kind === 'bool') return cell.value;
  if (cell.kind === 'json' && !cell.truncated) {
    try {
      return JSON.parse(cell.value);
    } catch {
      return cell.value;
    }
  }
  return cellText(cell);
}

/** Serialises a block of rows for the clipboard. Numbers stay exact strings. */
export function serializeRows(headers: string[], rows: Cell[][], format: CopyFormat, withHeader = true): string {
  if (format === 'json') {
    const objects = rows.map((row) => Object.fromEntries(headers.map((h, i) => [h, jsonValue(row[i] ?? { kind: 'null' })])));
    return JSON.stringify(objects.length === 1 ? objects[0] : objects, null, 2);
  }
  const field = format === 'csv' ? csvField : tsvField;
  const sep = format === 'csv' ? ',' : '\t';
  const lines = rows.map((row) => row.map((c) => field(cellText(c))).join(sep));
  if (withHeader) lines.unshift(headers.map((h) => field(h)).join(sep));
  return lines.join('\n');
}

export async function copyText(text: string): Promise<void> {
  await navigator.clipboard.writeText(text);
}
