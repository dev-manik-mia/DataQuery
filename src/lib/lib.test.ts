import { describe, expect, it } from 'vitest';

import type { StatementInfo } from '@/bindings/StatementInfo';

import { binaryLabel, cellText, splitTemporal } from './cells';
import { serializeRows } from './clipboard';
import { formatCount, formatDuration, formatTimer } from './format';
import { statementAt } from './sql';

describe('cells', () => {
  it('keeps NULL and empty distinct', () => {
    expect(cellText({ kind: 'null' })).toBeNull();
    expect(cellText({ kind: 'text', value: '' })).toBe('');
  });

  it('keeps int8 above 2^53 exact', () => {
    expect(cellText({ kind: 'number', value: '9007199254740993' })).toBe('9007199254740993');
    expect(formatCount('9007199254740993')).toBe('9,007,199,254,740,993');
  });

  it('splits zoned timestamps', () => {
    expect(splitTemporal('2026-10-14T07:59:00.123+00:00')).toEqual({ main: '2026-10-14 07:59:00.123', offset: '+00' });
    expect(splitTemporal('2026-10-14T07:59:00+05:30')).toEqual({ main: '2026-10-14 07:59:00', offset: '+05:30' });
    expect(splitTemporal('2026-10-14')).toEqual({ main: '2026-10-14', offset: '' });
  });

  it('labels binary values', () => {
    expect(binaryLabel(4300, '89504e470d0a1a0a')).toBe('\\x89504e47… · 4.2 KB');
  });
});

describe('clipboard', () => {
  const rows = [
    [{ kind: 'number', value: '9007199254740993' }, { kind: 'text', value: 'a,"b"' }, { kind: 'null' }],
    [{ kind: 'number', value: '2' }, { kind: 'json', value: '{"x":1}' }, { kind: 'bool', value: true }],
  ] as const;

  it('serialises CSV with quoting', () => {
    expect(serializeRows(['id', 'v', 'n'], rows.map((r) => [...r]), 'csv')).toBe(
      'id,v,n\n9007199254740993,"a,""b""",\n2,"{""x"":1}",true',
    );
  });

  it('serialises JSON with exact numbers', () => {
    const parsed = JSON.parse(serializeRows(['id', 'v', 'n'], rows.map((r) => [...r]), 'json'));
    expect(parsed[0].id).toBe('9007199254740993');
    expect(parsed[1].v).toEqual({ x: 1 });
    expect(parsed[0].n).toBeNull();
  });
});

describe('statementAt', () => {
  const s = (from: number, to: number) => ({ from, to }) as StatementInfo;
  const list = [s(0, 10), s(13, 20), s(30, 40)];

  it('finds the statement under the cursor', () => {
    expect(statementAt(list, 15)).toEqual(list[1]);
    expect(statementAt(list, 10)).toEqual(list[0]);
  });

  it('falls back to the previous statement', () => {
    expect(statementAt(list, 25)).toEqual(list[1]);
    expect(statementAt(list, 99)).toEqual(list[2]);
  });

  it('uses the first statement before any text', () => {
    expect(statementAt([s(5, 9)], 0)).toEqual({ from: 5, to: 9 });
  });
});

describe('format', () => {
  it('formats durations and timers', () => {
    expect(formatDuration(142)).toBe('142 ms');
    expect(formatDuration(1500)).toBe('1.50 s');
    expect(formatTimer(12_400)).toBe('00:12.4');
  });
});
