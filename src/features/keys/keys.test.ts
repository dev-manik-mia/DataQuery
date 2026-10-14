import { describe, expect, it } from 'vitest';

import type { KeyEntry } from '@/bindings/KeyEntry';

import { formatTtl, groupKeys, keyObject, keyRefOf, parseTtl } from './keys';

const entry = (key: string): KeyEntry => ({ key: { text: key, binary: false }, key_type: 'string', type_name: 'string', ttl_ms: null });

describe('keys helpers', () => {
  it('formats and parses TTLs', () => {
    expect(formatTtl(null)).toBe('—');
    expect(formatTtl(29 * 60_000)).toBe('29m');
    expect(formatTtl((2 * 60 + 14) * 60_000)).toBe('2h 14m');
    expect(formatTtl(3 * 86_400_000)).toBe('3d');
    expect(parseTtl('90')).toBe(90_000);
    expect(parseTtl('2h')).toBe(7_200_000);
    expect(parseTtl('0')).toBeNull();
    expect(parseTtl('soon')).toBeNull();
  });

  it('round-trips binary key names through tab objects', () => {
    const key = { text: 'a\\x00b', binary: true };
    expect(keyRefOf(keyObject(3, key))).toEqual({ database: 3, key });
    expect(keyObject(0, { text: 'user:1', binary: false }).parent).toBeUndefined();
  });

  it('groups keys by prefix', () => {
    const rows = groupKeys([entry('user:2'), entry('user:1'), entry('queue:emails'), entry('flag')], new Set());
    expect(rows.map((r) => (r.type === 'group' ? `${r.prefix}(${r.count})` : r.entry.key.text))).toEqual(['user:(2)', 'user:1', 'user:2', 'queue:emails', 'flag']);
    const closed = groupKeys([entry('user:2'), entry('user:1')], new Set(['user:']));
    expect(closed).toHaveLength(1);
  });
});
