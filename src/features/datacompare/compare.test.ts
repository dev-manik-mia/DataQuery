import { describe, expect, it } from 'vitest';

import type { ComparePlan } from '@/bindings/ComparePlan';

import { comparesData } from './entry';
import { compareOptions, DEFAULT_CHOICES, differences, fitChoices, syncRows } from './compare';

const column = (name: string, key = false) => ({ name, target_name: name, source_type: 'int', target_type: 'int', key, compared: true, type_mismatch: false, synced: true });
const plan: ComparePlan = { columns: [column('id', true), column('name'), column('total')], key_source: 'primary', only_in_source: [], only_in_target: [] };

describe('data compare', () => {
  it('builds backend options from the page choices', () => {
    const options = compareOptions({ ...DEFAULT_CHOICES, tolerance: ' 0.01 ', conditions: [{ column: 'total', op: 'gt', value: '5' }], matchAny: true });
    expect(options.float_tolerance).toBe(0.01);
    expect(options.rows.conditions).toHaveLength(1);
    expect(options.rows.match_any).toBe(true);
    expect(options.columns).toEqual([]);
    expect(compareOptions({ ...DEFAULT_CHOICES, tolerance: 'abc' }).float_tolerance).toBe(0);
    expect(compareOptions({ ...DEFAULT_CHOICES, tolerance: '-1' }).float_tolerance).toBe(0);
  });

  it('keeps only choices the loaded plan allows', () => {
    const fitted = fitChoices({ ...DEFAULT_CHOICES, columns: ['name', 'gone'], conditions: [{ column: 'gone', op: 'eq', value: 'x' }] }, plan);
    expect(fitted.keys).toEqual(['id']);
    expect(fitted.columns).toEqual(['name']);
    expect(fitted.conditions).toEqual([]);
    expect(fitChoices({ ...DEFAULT_CHOICES, keys: ['name'] }, plan).keys).toEqual(['name']);
  });

  it('counts rows per sync kind', () => {
    const counts = { identical: 5, only_in_source: 2, only_in_target: 3, different: 4 };
    expect(differences(counts)).toBe(9);
    expect(syncRows(counts, { insert: true, update: false, delete: true })).toBe(5);
  });

  it('compares SQL engines only', () => {
    expect(comparesData('postgres')).toBe(true);
    expect(comparesData('sqlite')).toBe(true);
    expect(comparesData('cql')).toBe(false);
    expect(comparesData('mongodb')).toBe(false);
    expect(comparesData(undefined)).toBe(false);
  });
});
