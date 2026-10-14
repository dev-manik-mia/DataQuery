/**
 * Display helpers for the value rendering contract (feature list §5.1).
 * Shared by the grid, the cell inspector, copy and edit.
 */
import type { Cell } from '@/bindings/Cell';

import { formatBytes } from './format';

/** Text used for copying and as the initial value when editing. `null` is SQL NULL. */
export function cellText(cell: Cell): string | null {
  switch (cell.kind) {
    case 'null':
      return null;
    case 'bool':
      return String(cell.value);
    case 'binary':
      return `\\x${cell.head_hex}`;
    case 'unsupported':
      return cell.value ?? `<${cell.type_name}>`;
    default:
      return cell.value;
  }
}

/** Splits a zoned timestamp into its server value and its offset. */
export function splitTemporal(value: string): { main: string; offset: string } {
  const display = value.replace('T', ' ');
  const match = /^(.*?)([+-]\d{2}(?::\d{2}){0,2}|Z)$/.exec(display);
  if (!match || !/\d{2}:\d{2}/.test(match[1] ?? '')) return { main: display, offset: '' };
  const offset = match[2] === 'Z' ? '+00' : (match[2] ?? '').replace(/:00$/, '');
  return { main: match[1] ?? display, offset };
}

/** Local-time rendering for the tooltip of a zoned timestamp. */
export function localTime(value: string): string | null {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? null : date.toString();
}

/** `\x89504e47… · 4.2 KB` */
export function binaryLabel(lengthBytes: number, headHex: string, headBytes = 4): string {
  const head = headHex.slice(0, headBytes * 2);
  const ellipsis = lengthBytes > headBytes ? '…' : '';
  return `\\x${head}${ellipsis} · ${formatBytes(lengthBytes)}`;
}

/** Single-line preview for long text and JSON. */
export function preview(value: string, max = 200): string {
  const flat = value.replace(/\s+/g, ' ');
  return flat.length > max ? `${flat.slice(0, max)}…` : flat;
}

/** Pretty-prints JSON when it parses; otherwise returns the input. */
export function prettyJson(value: string): string {
  try {
    return JSON.stringify(JSON.parse(value), null, 2);
  } catch {
    return value;
  }
}

export function isTruncated(cell: Cell): boolean {
  return (cell.kind === 'text' || cell.kind === 'json') && cell.truncated === true;
}

/** Builds a cell from user input when editing, keeping the column's kind. */
export function cellFromInput(value: string | null, like: Cell | undefined): Cell {
  if (value === null) return { kind: 'null' };
  switch (like?.kind) {
    case 'number':
      return { kind: 'number', value };
    case 'json':
      return { kind: 'json', value };
    case 'temporal':
      return { kind: 'temporal', value, zoned: like.zoned };
    case 'bool':
      return /^(t|true|1|yes|on)$/i.test(value) ? { kind: 'bool', value: true } : { kind: 'bool', value: false };
    default:
      return { kind: 'text', value };
  }
}
