import type { KeyEntry } from '@/bindings/KeyEntry';
import type { KeyRef } from '@/bindings/KeyRef';
import type { KeyType } from '@/bindings/KeyType';
import type { ObjectRef } from '@/bindings/ObjectRef';
import type { RString } from '@/bindings/RString';

/** Short type badges, as in screen 12. */
export const TYPE_BADGE: Record<KeyType, string> = {
  string: 'STR',
  hash: 'HASH',
  list: 'LIST',
  set: 'SET',
  zset: 'ZSET',
  stream: 'STREAM',
  other: 'MOD',
};

export const TYPE_LABEL: Record<KeyType, string> = {
  string: 'String',
  hash: 'Hash',
  list: 'List',
  set: 'Set',
  zset: 'Sorted set',
  stream: 'Stream',
  other: 'Module type',
};

/** A key tab's object: `database` is the index, `parent` marks escaped binary names. */
export function keyObject(database: number, key: RString): ObjectRef {
  return { kind: 'key', database: String(database), schema: null, name: key.text, ...(key.binary ? { parent: 'binary' } : {}) };
}

export function keyRefOf(object: ObjectRef): KeyRef {
  return { database: Number(object.database), key: { text: object.name, binary: object.parent === 'binary' } };
}

/** `29m`, `2h 14m`, `3d 4h`; `—` without expiry. */
export function formatTtl(ms: number | null): string {
  if (ms === null) return '—';
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
  const d = Math.floor(h / 24);
  return h % 24 ? `${d}d ${h % 24}h` : `${d}d`;
}

/** Parses `90`, `90s`, `15m`, `2h`, `1d` into milliseconds; `null` when invalid or not positive. */
export function parseTtl(text: string): number | null {
  const m = /^\s*(\d+(?:\.\d+)?)\s*(ms|s|m|h|d)?\s*$/i.exec(text);
  if (!m) return null;
  const unit = { ms: 1, s: 1000, m: 60_000, h: 3_600_000, d: 86_400_000 }[(m[2] ?? 's').toLowerCase() as 'ms' | 's' | 'm' | 'h' | 'd'];
  const ms = Math.round(Number(m[1]) * unit);
  return ms > 0 ? ms : null;
}

export type KeyRow = { type: 'group'; prefix: string; count: number; open: boolean } | { type: 'key'; entry: KeyEntry; depth: number };

/**
 * Keys grouped by their first `:` segment, as in screen 12: a group row per
 * prefix shared by at least two keys, then its keys when the group is open.
 */
export function groupKeys(keys: KeyEntry[], closed: ReadonlySet<string>): KeyRow[] {
  const groups = new Map<string, KeyEntry[]>();
  const order: string[] = [];
  for (const k of keys) {
    const i = k.key.text.indexOf(':');
    const prefix = i > 0 ? k.key.text.slice(0, i + 1) : '';
    if (!groups.has(prefix)) {
      groups.set(prefix, []);
      order.push(prefix);
    }
    groups.get(prefix)!.push(k);
  }
  const rows: KeyRow[] = [];
  for (const prefix of order) {
    const members = groups.get(prefix)!;
    if (!prefix || members.length < 2) {
      for (const entry of members) rows.push({ type: 'key', entry, depth: 0 });
      continue;
    }
    const open = !closed.has(prefix);
    rows.push({ type: 'group', prefix, count: members.length, open });
    if (open) for (const entry of [...members].sort((a, b) => a.key.text.localeCompare(b.key.text))) rows.push({ type: 'key', entry, depth: 1 });
  }
  return rows;
}
