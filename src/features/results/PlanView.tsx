import { useMemo, useState } from 'react';

import type { ExplainResult } from '@/bindings/ExplainResult';
import type { PlanNode } from '@/bindings/PlanNode';
import { Segmented } from '@/components/ui';
import { formatCount } from '@/lib/format';

import styles from './Results.module.css';

function maxTime(node: PlanNode): number {
  return Math.max(node.actual_ms ?? 0, ...node.children.map(maxTime));
}

function num(n: number | null, digits = 0): string {
  return n === null ? '' : n >= 1000 ? formatCount(Math.round(n)) : n.toFixed(digits);
}

function NodeRow({ node, depth, hottest }: { node: PlanNode; depth: number; hottest: number }) {
  const [open, setOpen] = useState(depth < 6);
  const [details, setDetails] = useState(false);
  const share = hottest > 0 && node.actual_ms !== null ? node.actual_ms / hottest : 0;
  const misestimate = node.rows !== null && node.actual_rows !== null && node.rows > 0 && (node.actual_rows / node.rows > 10 || node.rows / Math.max(1, node.actual_rows) > 10);
  return (
    <>
      <div className={styles.planRow} style={{ paddingLeft: 12 + depth * 18 }} data-hot={share > 0.5}>
        <button type="button" className={styles.planToggle} aria-label={open ? 'Collapse' : 'Expand'} disabled={!node.children.length} onClick={() => setOpen((o) => !o)}>
          {node.children.length ? (open ? '▾' : '▸') : '·'}
        </button>
        <button type="button" className={styles.planLabel} onClick={() => setDetails((d) => !d)} aria-expanded={details}>
          <b>{node.label}</b>
          {node.detail && <span className="mono ink-2"> {node.detail}</span>}
        </button>
        <span className="tabular ink-2" title="Estimated cost">{num(node.cost, 2)}</span>
        <span className="tabular ink-2" title="Estimated rows" data-warn={misestimate}>{num(node.rows)}</span>
        <span className="tabular" title="Actual rows">{num(node.actual_rows)}</span>
        <span className="tabular" title="Actual time (ms)">
          {node.actual_ms !== null ? `${node.actual_ms.toFixed(2)} ms` : ''}
          {node.loops !== null && node.loops > 1 ? <span className="ink-3"> ×{node.loops}</span> : null}
        </span>
      </div>
      {details && node.props.length > 0 && (
        <div className={styles.planProps} style={{ paddingLeft: 48 + depth * 18 }}>
          {node.props.map(([k, v]) => (
            <div key={k}>
              <span className="ink-3">{k}: </span>
              <span className="mono">{v}</span>
            </div>
          ))}
        </div>
      )}
      {open && node.children.map((c, i) => <NodeRow key={i} node={c} depth={depth + 1} hottest={hottest} />)}
    </>
  );
}

/** EXPLAIN output as a tree with cost, estimated vs actual rows and time. */
export function PlanView({ plan }: { plan: ExplainResult }) {
  const [mode, setMode] = useState<'tree' | 'raw'>(plan.plan ? 'tree' : 'raw');
  const hottest = useMemo(() => (plan.plan ? maxTime(plan.plan) : 0), [plan]);
  return (
    <div className={styles.plan}>
      <div className={styles.planBar}>
        <Segmented<'tree' | 'raw'> label="Plan view" value={mode} onChange={setMode} options={[{ value: 'tree', label: 'Tree', disabled: !plan.plan }, { value: 'raw', label: 'Raw' }]} />
        <span className="ink-2">
          {plan.analyzed ? 'EXPLAIN ANALYZE' : 'EXPLAIN'}
          {plan.planning_ms !== null && ` · planning ${plan.planning_ms.toFixed(2)} ms`}
          {plan.execution_ms !== null && ` · execution ${plan.execution_ms.toFixed(2)} ms`}
        </span>
      </div>
      {mode === 'tree' && plan.plan ? (
        <div className={styles.planTree} role="tree" aria-label="Query plan">
          <div className={`${styles.planRow} ${styles.planHead}`}>
            <span />
            <span>Operation</span>
            <span>Cost</span>
            <span>Est. rows</span>
            <span>Rows</span>
            <span>Time</span>
          </div>
          <NodeRow node={plan.plan} depth={0} hottest={hottest} />
        </div>
      ) : (
        <pre className={styles.planRaw}>{plan.raw}</pre>
      )}
    </div>
  );
}
