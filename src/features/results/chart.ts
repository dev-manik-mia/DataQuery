import type { Cell } from '@/bindings/Cell';
import type { ColumnMeta } from '@/bindings/ColumnMeta';
import { cellText } from '@/lib/cells';

export type ChartKind = 'bar' | 'line' | 'pie' | 'scatter';
export type Aggregate = 'sum' | 'avg' | 'count' | 'min' | 'max' | 'first';

export interface ChartConfig {
  kind: ChartKind;
  /** Column index for the x axis (categories, slices). */
  x: number;
  /** Column indexes of the measured series. */
  ys: number[];
  aggregate: Aggregate;
}

export interface Series {
  column: number;
  name: string;
  values: (number | null)[];
}

export interface ChartData {
  /** Labels along x (categories, times, or slice names). */
  categories: string[];
  /** Numeric x positions for scatter; `null` where the x value is not a number. */
  xs: (number | null)[];
  series: Series[];
  /** Rows or groups folded into "Other" or left out, said under the chart. */
  note: string | null;
}

/** Categorical slots in fixed order (validated for CVD separation; see Chart.module.css). */
export const MAX_SERIES = 8;
/** Scatter compares every pair of colours, which only the first three slots clear. */
export const MAX_SCATTER_SERIES = 3;
export const MAX_BARS = 60;
export const MAX_SLICES = 6;
export const MAX_POINTS = 5000;

export function cellNumber(cell: Cell | undefined): number | null {
  if (!cell) return null;
  if (cell.kind === 'number' || cell.kind === 'text') {
    const n = Number(cell.value);
    return cell.value.trim() !== '' && Number.isFinite(n) ? n : null;
  }
  if (cell.kind === 'bool') return cell.value ? 1 : 0;
  return null;
}

/** Columns that can be measured: numeric by type, or whose loaded values are all numbers. */
export function measurableColumns(columns: ColumnMeta[], rows: Cell[][]): number[] {
  const sample = rows.slice(0, 200);
  return columns.flatMap((c, i) => (c.numeric || (sample.length > 0 && sample.every((r) => r[i]?.kind === 'null' || cellNumber(r[i]) !== null)) ? [i] : []));
}

/** A first chart: the first non-measured column along x, then up to three measures. */
export function defaultConfig(columns: ColumnMeta[], rows: Cell[][]): ChartConfig | null {
  const measured = measurableColumns(columns, rows);
  if (!measured.length) return null;
  const x = columns.findIndex((_, i) => !measured.includes(i));
  const xIndex = x >= 0 ? x : measured.length > 1 ? measured[0]! : -1;
  // Keys measure nothing; they stay selectable but are not charted by default.
  const isKey = (i: number) => /^(id|.*_id|.*Id)$/.test(columns[i]?.name ?? '');
  const candidates = measured.filter((i) => i !== xIndex);
  const ys = (candidates.some((i) => !isKey(i)) ? candidates.filter((i) => !isKey(i)) : candidates).slice(0, 3);
  if (!ys.length) return null;
  const temporal = xIndex >= 0 && rows.some((r) => r[xIndex]?.kind === 'temporal');
  return { kind: temporal ? 'line' : 'bar', x: Math.max(xIndex, 0), ys, aggregate: 'sum' };
}

function combine(values: number[], aggregate: Aggregate): number | null {
  if (aggregate === 'count') return values.length;
  if (!values.length) return null;
  switch (aggregate) {
    case 'sum':
      return values.reduce((a, b) => a + b, 0);
    case 'avg':
      return values.reduce((a, b) => a + b, 0) / values.length;
    case 'min':
      return Math.min(...values);
    case 'max':
      return Math.max(...values);
    default:
      return values[0]!;
  }
}

const label = (cell: Cell | undefined) => (cell ? (cellText(cell) ?? 'NULL') : 'NULL');

/** Groups rows by the x value (first appearance order; time order for temporal x) and aggregates each series. */
export function buildChart(columns: ColumnMeta[], rows: Cell[][], config: ChartConfig): ChartData {
  const ys = config.ys.filter((i) => i >= 0 && i < columns.length).slice(0, config.kind === 'scatter' ? MAX_SCATTER_SERIES : config.kind === 'pie' ? 1 : MAX_SERIES);
  const names = ys.map((i) => columns[i]?.name ?? '');

  if (config.kind === 'scatter') {
    const used = rows.slice(0, MAX_POINTS);
    return {
      categories: used.map((r) => label(r[config.x])),
      xs: used.map((r) => cellNumber(r[config.x])),
      series: ys.map((col, k) => ({ column: col, name: names[k]!, values: used.map((r) => cellNumber(r[col])) })),
      note: rows.length > MAX_POINTS ? `Showing the first ${MAX_POINTS.toLocaleString()} of ${rows.length.toLocaleString()} rows.` : null,
    };
  }

  const groups = new Map<string, Cell[][]>();
  for (const row of rows) {
    const key = label(row[config.x]);
    const group = groups.get(key);
    if (group) group.push(row);
    else groups.set(key, [row]);
  }
  let keys = [...groups.keys()];
  if (rows.some((r) => r[config.x]?.kind === 'temporal')) keys.sort();

  const value = (key: string, col: number) => combine((groups.get(key) ?? []).flatMap((r) => (cellNumber(r[col]) ?? []) as number[]), config.aggregate);
  let series = ys.map((col, k) => ({ column: col, name: names[k]!, values: keys.map((key) => value(key, col)) }));
  let note: string | null = null;

  const cap = config.kind === 'pie' ? MAX_SLICES : config.kind === 'bar' ? MAX_BARS : MAX_POINTS;
  if (keys.length > cap) {
    const additive = config.aggregate === 'sum' || config.aggregate === 'count';
    if (config.kind === 'pie' && additive) {
      // Largest slices first; the rest becomes one "Other" slice so the whole stays whole.
      const order = keys.map((_, i) => i).sort((a, b) => (series[0]!.values[b] ?? 0) - (series[0]!.values[a] ?? 0));
      const kept = order.slice(0, cap - 1);
      const rest = order.slice(cap - 1).reduce((sum, i) => sum + (series[0]!.values[i] ?? 0), 0);
      keys = [...kept.map((i) => keys[i]!), 'Other'];
      series = series.map((s) => ({ ...s, values: [...kept.map((i) => s.values[i] ?? null), rest] }));
      note = `${order.length - kept.length} smaller slices are combined into "Other".`;
    } else {
      note = `Showing the first ${cap.toLocaleString()} of ${keys.length.toLocaleString()} groups.`;
      keys = keys.slice(0, cap);
      series = series.map((s) => ({ ...s, values: s.values.slice(0, cap) }));
    }
  }
  return { categories: keys, xs: keys.map(() => null), series, note };
}

/** About `count` round tick values covering [min, max], always including zero for bars. */
export function niceTicks(min: number, max: number, count = 5): number[] {
  if (!Number.isFinite(min) || !Number.isFinite(max)) return [0];
  if (min === max) {
    if (min === 0) return [0, 1];
    [min, max] = min > 0 ? [0, min] : [min, 0];
  }
  const raw = (max - min) / count;
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  const step = ([1, 2, 5, 10].find((m) => m * magnitude >= raw) ?? 10) * magnitude;
  const ticks: number[] = [];
  for (let v = Math.floor(min / step) * step; v <= max + step * 1e-9; v += step) ticks.push(Number(v.toPrecision(12)));
  if (ticks[ticks.length - 1]! < max) ticks.push(Number((ticks[ticks.length - 1]! + step).toPrecision(12)));
  return ticks;
}

/** 1.2K, 3.4M; plain digits below 1,000. */
export function compactNumber(value: number): string {
  return new Intl.NumberFormat(undefined, { notation: Math.abs(value) >= 1000 ? 'compact' : 'standard', maximumFractionDigits: 2 }).format(value);
}
