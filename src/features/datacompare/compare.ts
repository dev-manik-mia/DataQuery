import type { ComparePlan } from '@/bindings/ComparePlan';
import type { CompareCounts } from '@/bindings/CompareCounts';
import type { DataCompareOptions } from '@/bindings/DataCompareOptions';
import type { FilterCondition } from '@/bindings/FilterCondition';
import type { SyncKinds } from '@/bindings/SyncKinds';

/** What the user chose on the page, before it becomes [`DataCompareOptions`]. */
export interface CompareChoices {
  /** Key columns; empty lets the backend use the primary or a unique key. */
  keys: string[];
  /** Compared non-key columns; `null` compares every common column. */
  columns: string[] | null;
  conditions: FilterCondition[];
  matchAny: boolean;
  ignoreWhitespace: boolean;
  ignoreCase: boolean;
  tolerance: string;
  maxExamples: number;
}

export const DEFAULT_CHOICES: CompareChoices = {
  keys: [],
  columns: null,
  conditions: [],
  matchAny: false,
  ignoreWhitespace: false,
  ignoreCase: false,
  tolerance: '',
  maxExamples: 1000,
};

/** Options for the backend. A blank or invalid tolerance compares exactly. */
export function compareOptions(c: CompareChoices): DataCompareOptions {
  const tolerance = Number(c.tolerance.trim() || '0');
  return {
    key_columns: c.keys,
    columns: c.columns ?? [],
    rows: { filter: null, matches: [], searches: [], conditions: c.conditions, match_any: c.matchAny },
    ignore_whitespace: c.ignoreWhitespace,
    ignore_case: c.ignoreCase,
    float_tolerance: Number.isFinite(tolerance) && tolerance > 0 ? tolerance : 0,
    max_examples: c.maxExamples,
  };
}

/** The choices that a newly loaded plan still allows: unknown columns are dropped. */
export function fitChoices(c: CompareChoices, plan: ComparePlan): CompareChoices {
  const names = new Set(plan.columns.map((col) => col.name));
  const keys = c.keys.filter((k) => names.has(k));
  return {
    ...c,
    keys: keys.length ? keys : plan.columns.filter((col) => col.key).map((col) => col.name),
    columns: c.columns?.filter((name) => names.has(name)) ?? null,
    conditions: c.conditions.filter((cond) => names.has(cond.column)),
  };
}

/** Rows the sync changes for each kind it includes. */
export function syncRows(counts: CompareCounts, kinds: SyncKinds): number {
  return (kinds.insert ? counts.only_in_source : 0) + (kinds.update ? counts.different : 0) + (kinds.delete ? counts.only_in_target : 0);
}

export function differences(counts: CompareCounts): number {
  return counts.only_in_source + counts.only_in_target + counts.different;
}
