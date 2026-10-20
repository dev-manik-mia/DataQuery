import { describe, expect, it } from 'vitest';

import type { Cell } from '@/bindings/Cell';
import type { ColumnMeta } from '@/bindings/ColumnMeta';

import { buildChart, defaultConfig, MAX_SLICES, measurableColumns, niceTicks } from './chart';

const col = (name: string, numeric: boolean): ColumnMeta => ({ name, type_name: numeric ? 'int4' : 'text', numeric }) as ColumnMeta;
const text = (value: string): Cell => ({ kind: 'text', value });
const num = (value: number): Cell => ({ kind: 'number', value: String(value) });
const NULL: Cell = { kind: 'null' };

const columns = [col('region', false), col('sales', true), col('orders', true)];
const rows: Cell[][] = [
  [text('EU'), num(10), num(1)],
  [text('US'), num(5), num(2)],
  [text('EU'), num(7), NULL],
  [NULL, num(1), num(1)],
];

describe('chart data', () => {
  it('picks the first label column for x and the measures for y', () => {
    expect(measurableColumns(columns, rows)).toEqual([1, 2]);
    expect(defaultConfig(columns, rows)).toEqual({ kind: 'bar', x: 0, ys: [1, 2], aggregate: 'sum' });
    expect(defaultConfig([col('name', false)], [[text('a')]])).toBeNull();
    // Key columns are not charted by default.
    expect(defaultConfig([col('id', true), ...columns], rows.map((r) => [num(1), ...r]))?.ys).toEqual([2, 3]);
  });

  it('treats text columns holding only numbers as measures', () => {
    expect(measurableColumns([col('n', false)], [[text('1.5')], [NULL], [text('2')]])).toEqual([0]);
    expect(measurableColumns([col('n', false)], [[text('1.5')], [text('x')]])).toEqual([]);
  });

  it('groups by x in first-appearance order and aggregates', () => {
    const sum = buildChart(columns, rows, { kind: 'bar', x: 0, ys: [1, 2], aggregate: 'sum' });
    expect(sum.categories).toEqual(['EU', 'US', 'NULL']);
    expect(sum.series.map((s) => s.values)).toEqual([
      [17, 5, 1],
      [1, 2, 1],
    ]);
    expect(buildChart(columns, rows, { kind: 'bar', x: 0, ys: [1], aggregate: 'avg' }).series[0]!.values).toEqual([8.5, 5, 1]);
    expect(buildChart(columns, rows, { kind: 'bar', x: 0, ys: [2], aggregate: 'count' }).series[0]!.values).toEqual([1, 1, 1]);
    expect(buildChart(columns, rows, { kind: 'bar', x: 0, ys: [2], aggregate: 'max' }).series[0]!.values).toEqual([1, 2, 1]);
  });

  it('folds small pie slices into Other', () => {
    const many = Array.from({ length: 10 }, (_, i) => [text(`k${i}`), num(i + 1)]);
    const pie = buildChart(columns.slice(0, 2), many, { kind: 'pie', x: 0, ys: [1], aggregate: 'sum' });
    expect(pie.categories).toHaveLength(MAX_SLICES);
    expect(pie.categories.slice(0, 2)).toEqual(['k9', 'k8']);
    expect(pie.categories.at(-1)).toBe('Other');
    expect(pie.series[0]!.values.reduce((a, b) => a! + b!, 0)).toBe(55);
    expect(pie.note).toMatch(/5 smaller slices/);
  });

  it('scatter keeps rows and numeric x', () => {
    const data = buildChart(columns, rows, { kind: 'scatter', x: 1, ys: [2], aggregate: 'sum' });
    expect(data.xs).toEqual([10, 5, 7, 1]);
    expect(data.series[0]!.values).toEqual([1, 2, null, 1]);
  });

  it('makes round ticks', () => {
    expect(niceTicks(0, 17)).toEqual([0, 5, 10, 15, 20]);
    expect(niceTicks(-3, 9)).toEqual([-5, 0, 5, 10]);
    expect(niceTicks(4, 4)).toEqual([0, 1, 2, 3, 4]);
    expect(niceTicks(0, 0)).toEqual([0, 1]);
  });
});
