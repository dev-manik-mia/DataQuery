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

/** Yields to the event loop (a macrotask) so input and painting can run. */
const yieldToEventLoop = () => new Promise<void>((resolve) => setTimeout(resolve, 0));

/**
 * `serializeRows` for large blocks: same output, built `chunk` rows at a time
 * with a macrotask between chunks so the UI keeps responding.
 */
export async function serializeRowsChunked(headers: string[], rows: Cell[][], format: CopyFormat, withHeader = true, chunk = 2000): Promise<string> {
  if (rows.length <= chunk) return serializeRows(headers, rows, format, withHeader);
  const parts: string[] = [];
  for (let start = 0; start < rows.length; start += chunk) {
    if (start > 0) await yieldToEventLoop();
    const block = rows.slice(start, start + chunk);
    if (format === 'json') {
      // Each element as JSON.stringify(array, null, 2) would print it: one level deeper.
      for (const row of block) parts.push(JSON.stringify(Object.fromEntries(headers.map((h, i) => [h, jsonValue(row[i] ?? { kind: 'null' })])), null, 2).replace(/\n/g, '\n  '));
    } else {
      parts.push(serializeRows(headers, block, format, false));
    }
  }
  if (format === 'json') return `[\n  ${parts.join(',\n  ')}\n]`;
  const body = parts.join('\n');
  if (!withHeader) return body;
  const field = format === 'csv' ? csvField : tsvField;
  return `${headers.map((h) => field(h)).join(format === 'csv' ? ',' : '\t')}\n${body}`;
}

/**
 * Copies text. Text still being built is handed over as a promise: WebKit only
 * lets a page write the clipboard during the click, so the write starts now
 * and the clipboard waits for the data.
 */
export async function copyText(text: string | Promise<string>): Promise<void> {
  if (typeof text === 'string') return navigator.clipboard.writeText(text);
  if (typeof ClipboardItem === 'undefined') return navigator.clipboard.writeText(await text);
  const blob = text.then((t) => new Blob([t], { type: 'text/plain' }));
  await navigator.clipboard.write([new ClipboardItem({ 'text/plain': blob })]);
}
