import { useState } from 'react';

import type { FilterCondition } from '@/bindings/FilterCondition';
import type { FilterOp } from '@/bindings/FilterOp';
import { Button, Icon, IconButton, Segmented } from '@/components/ui';
import { withinFreeLimit } from '@/stores/license';

import styles from './Table.module.css';

export const FILTER_OPS: { value: FilterOp; label: string }[] = [
  { value: 'eq', label: '=' },
  { value: 'ne', label: '≠' },
  { value: 'gt', label: '>' },
  { value: 'gte', label: '≥' },
  { value: 'lt', label: '<' },
  { value: 'lte', label: '≤' },
  { value: 'contains', label: 'contains' },
  { value: 'not_contains', label: 'does not contain' },
  { value: 'starts_with', label: 'starts with' },
  { value: 'ends_with', label: 'ends with' },
  { value: 'in', label: 'in (a, b, c)' },
  { value: 'is_null', label: 'is NULL' },
  { value: 'is_not_null', label: 'is not NULL' },
];

export const isUnary = (op: FilterOp) => op === 'is_null' || op === 'is_not_null';

/** Conditions ready to send: a value is required except for `is NULL` / `is not NULL`. */
export function completeConditions(conditions: FilterCondition[]): FilterCondition[] {
  return conditions
    .filter((c) => c.column && (isUnary(c.op) || c.value.trim() !== ''))
    .map((c) => ({ ...c, value: isUnary(c.op) ? '' : c.value.trim() }));
}

/** Short text of applied conditions, e.g. `status = paid AND total > 100`. */
export function describeConditions(conditions: FilterCondition[], matchAny: boolean): string {
  const label = (op: FilterOp) => FILTER_OPS.find((o) => o.value === op)!.label.replace(' (a, b, c)', '');
  return conditions.map((c) => (isUnary(c.op) ? `${c.column} ${label(c.op)}` : `${c.column} ${label(c.op)} ${c.value}`)).join(matchAny ? ' OR ' : ' AND ');
}

export interface AppliedFilter {
  conditions: FilterCondition[];
  matchAny: boolean;
}

interface FilterBuilderProps {
  columns: string[];
  applied: AppliedFilter;
  onApply: (filter: AppliedFilter) => void;
  onClose: () => void;
}

/** Column · operator · value rows, joined with AND or OR. Values are sent as parameters, never as SQL. */
export function FilterBuilder({ columns, applied, onApply, onClose }: FilterBuilderProps) {
  const blank = (): FilterCondition => ({ column: columns[0] ?? '', op: 'eq', value: '' });
  const [rows, setRows] = useState<FilterCondition[]>(applied.conditions.length ? applied.conditions : [blank()]);
  const [matchAny, setMatchAny] = useState(applied.matchAny);
  const ready = completeConditions(rows);

  const change = (i: number, patch: Partial<FilterCondition>) => setRows((r) => r.map((c, j) => (j === i ? { ...c, ...patch } : c)));

  return (
    <form
      className={styles.filterBuilder}
      aria-label="Filter rows"
      onSubmit={(e) => {
        e.preventDefault();
        onApply({ conditions: ready, matchAny });
      }}
      onKeyDown={(e) => e.key === 'Escape' && onClose()}
    >
      {rows.length > 1 && (
        <Segmented
          label="Combine conditions"
          value={matchAny ? 'any' : 'all'}
          onChange={(v) => setMatchAny(v === 'any')}
          options={[
            { value: 'all', label: 'Match all (AND)' },
            { value: 'any', label: 'Match any (OR)' },
          ]}
        />
      )}
      {rows.map((c, i) => (
        <div key={i} className={styles.filterRow}>
          <select className="dq-input dq-input--sm dq-input--mono" aria-label={`Condition ${i + 1} column`} value={c.column} onChange={(e) => change(i, { column: e.target.value })}>
            {columns.map((name) => (
              <option key={name} value={name}>
                {name}
              </option>
            ))}
          </select>
          <select className="dq-input dq-input--sm" aria-label={`Condition ${i + 1} operator`} value={c.op} onChange={(e) => change(i, { op: e.target.value as FilterOp })}>
            {FILTER_OPS.map((o) => (
              <option key={o.value} value={o.value}>
                {o.label}
              </option>
            ))}
          </select>
          <input
            className="dq-input dq-input--sm dq-input--mono"
            aria-label={`Condition ${i + 1} value`}
            placeholder={isUnary(c.op) ? '' : c.op === 'in' ? 'a, b, c' : 'value'}
            disabled={isUnary(c.op)}
            spellCheck={false}
            value={isUnary(c.op) ? '' : c.value}
            onChange={(e) => change(i, { value: e.target.value })}
            autoFocus={i === rows.length - 1 && !c.value}
          />
          <IconButton label={`Remove condition ${i + 1}`} onClick={() => setRows((r) => (r.length > 1 ? r.filter((_, j) => j !== i) : [blank()]))}>
            <Icon name="close" size={12} />
          </IconButton>
        </div>
      ))}
      <div className={styles.filterActions}>
        <Button size="sm" onClick={() => withinFreeLimit('filters', rows.length) && setRows((r) => [...r, blank()])}>
          <Icon name="plus" size={12} /> Condition
        </Button>
        <span style={{ flex: 1 }} />
        <Button
          size="sm"
          disabled={!applied.conditions.length && rows.every((c) => !c.value)}
          onClick={() => {
            setRows([blank()]);
            onApply({ conditions: [], matchAny });
          }}
        >
          Clear
        </Button>
        <Button size="sm" variant="primary" type="submit" disabled={!ready.length && !applied.conditions.length}>
          Apply{ready.length ? ` ${ready.length} filter${ready.length === 1 ? '' : 's'}` : ''}
        </Button>
      </div>
    </form>
  );
}
