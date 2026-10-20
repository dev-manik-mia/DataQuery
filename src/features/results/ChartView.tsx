import { useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';

import type { Cell } from '@/bindings/Cell';
import type { ColumnMeta } from '@/bindings/ColumnMeta';
import { EmptyState, Segmented } from '@/components/ui';

import { buildChart, compactNumber, defaultConfig, MAX_SCATTER_SERIES, MAX_SERIES, measurableColumns, niceTicks, type Aggregate, type ChartConfig, type ChartData, type ChartKind } from './chart';
import styles from './Chart.module.css';

const color = (slot: number) => `var(--series-${(slot % MAX_SERIES) + 1})`;
const PAD = { top: 12, right: 16, bottom: 28 };

/** Room for the longest value label left of the plot. */
const leftPad = (ticks: number[]) => Math.max(...ticks.map((t) => compactNumber(t).length)) * 6.6 + 14;
const BAR_MAX = 24;
const GAP = 2;

interface Hover {
  x: number;
  y: number;
  title: string;
  rows: { slot: number; name: string; value: number | null }[];
  /** Category index under the pointer (bars, lines). */
  index?: number;
}

/** Bar, line, pie or scatter chart of a loaded result; the grid stays the table view. */
export function ChartView({ columns, rows }: { columns: ColumnMeta[]; rows: Cell[][] }) {
  const measured = useMemo(() => measurableColumns(columns, rows), [columns, rows]);
  const [config, setConfig] = useState<ChartConfig | null>(() => defaultConfig(columns, rows));
  const data = useMemo(() => (config ? buildChart(columns, rows, config) : null), [columns, rows, config]);

  if (!config || !data) {
    return (
      <EmptyState title="Nothing to chart">
        A chart needs at least one numeric column and one other column. Try a query such as <span className="mono">SELECT status, count(*) FROM orders GROUP BY status</span>.
      </EmptyState>
    );
  }

  const set = (patch: Partial<ChartConfig>) => setConfig({ ...config, ...patch });
  const maxSeries = config.kind === 'scatter' ? MAX_SCATTER_SERIES : config.kind === 'pie' ? 1 : MAX_SERIES;
  const toggleMeasure = (col: number) => {
    const on = config.ys.includes(col);
    if (on && config.ys.length === 1) return;
    set({ ys: on ? config.ys.filter((c) => c !== col) : config.kind === 'pie' ? [col] : [...config.ys, col].slice(-maxSeries) });
  };

  return (
    <div className={styles.chart}>
      <div className={styles.controls}>
        <Segmented<ChartKind>
          label="Chart type"
          value={config.kind}
          onChange={(kind) => set({ kind, ys: config.ys.slice(0, kind === 'scatter' ? MAX_SCATTER_SERIES : kind === 'pie' ? 1 : MAX_SERIES) })}
          options={[
            { value: 'bar', label: 'Bar' },
            { value: 'line', label: 'Line' },
            { value: 'pie', label: 'Pie' },
            { value: 'scatter', label: 'Scatter' },
          ]}
        />
        <label>
          {config.kind === 'pie' ? 'Slices' : 'X'}{' '}
          <select className="dq-input dq-input--sm" value={config.x} onChange={(e) => set({ x: Number(e.target.value) })}>
            {columns.map((c, i) => (
              <option key={i} value={i} disabled={config.kind === 'scatter' && !measured.includes(i)}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        {config.kind !== 'scatter' && (
          <label>
            Combine{' '}
            <select className="dq-input dq-input--sm" value={config.aggregate} onChange={(e) => set({ aggregate: e.target.value as Aggregate })}>
              <option value="sum">Sum</option>
              <option value="avg">Average</option>
              <option value="count">Count</option>
              <option value="min">Minimum</option>
              <option value="max">Maximum</option>
              <option value="first">First value</option>
            </select>
          </label>
        )}
        <div className={styles.measures} role="group" aria-label={`Measures (up to ${maxSeries})`}>
          {measured
            .filter((c) => c !== config.x)
            .map((c) => {
              const slot = config.ys.indexOf(c);
              return (
                <button key={c} type="button" className={styles.measure} aria-pressed={slot >= 0} onClick={() => toggleMeasure(c)}>
                  <span className={styles.swatch} style={{ background: slot >= 0 ? color(slot) : 'var(--track)' }} aria-hidden />
                  {columns[c]?.name}
                </button>
              );
            })}
        </div>
      </div>
      {data.series.length > 1 && (
        <div className={styles.legend} aria-label="Legend">
          {data.series.map((s, i) => (
            <span key={s.column} className={styles.legendItem}>
              <span className={styles.swatch} style={{ background: color(i) }} aria-hidden />
              {s.name}
            </span>
          ))}
        </div>
      )}
      <Plot data={data} kind={config.kind} xName={columns[config.x]?.name ?? ''} />
      {data.note && <div className={styles.note}>{data.note}</div>}
    </div>
  );
}

function Plot({ data, kind, xName }: { data: ChartData; kind: ChartKind; xName: string }) {
  const host = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [hover, setHover] = useState<Hover | null>(null);
  useLayoutEffect(() => {
    const el = host.current;
    if (!el) return;
    const observer = new ResizeObserver(([entry]) => entry && setSize({ width: entry.contentRect.width, height: entry.contentRect.height }));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  const { width, height } = size;
  let body: ReactNode = null;
  if (width > 0 && height > 0) {
    if (kind === 'pie') body = <Pie data={data} width={width} height={height} onHover={setHover} />;
    else if (kind === 'scatter') body = <Scatter data={data} width={width} height={height} xName={xName} onHover={setHover} />;
    else body = <Cartesian data={data} kind={kind} width={width} height={height} hover={hover} onHover={setHover} />;
  }

  return (
    <div ref={host} className={styles.plot} onMouseLeave={() => setHover(null)}>
      <svg width={width} height={height} role="img" aria-label={`${kind} chart of ${data.series.map((s) => s.name).join(', ')} by ${xName}`}>
        {body}
      </svg>
      {hover && (
        <div className={styles.tooltip} style={{ left: Math.min(hover.x + 12, Math.max(0, width - 200)), top: Math.max(0, hover.y - 12) }}>
          <div className={styles.tooltipTitle}>{hover.title}</div>
          {hover.rows.map((r) => (
            <div key={r.slot} className={styles.tooltipRow}>
              <span className={styles.swatch} style={{ background: color(r.slot) }} aria-hidden />
              <span className="ellipsis">{r.name}</span>
              <span className={styles.tooltipValue}>{r.value === null ? '—' : r.value.toLocaleString(undefined, { maximumFractionDigits: 4 })}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** Linear scale from a domain to a pixel range. */
const scale = (d0: number, d1: number, r0: number, r1: number) => (v: number) => (d1 === d0 ? r0 : r0 + ((v - d0) / (d1 - d0)) * (r1 - r0));

function valueTicks(data: ChartData, includeZero: boolean) {
  const values = data.series.flatMap((s) => s.values.filter((v): v is number => v !== null));
  const min = Math.min(includeZero ? 0 : Infinity, ...values);
  const max = Math.max(includeZero ? 0 : -Infinity, ...values);
  return niceTicks(values.length ? min : 0, values.length ? max : 1);
}

function YAxis({ ticks, y, width, left }: { ticks: number[]; y: (v: number) => number; width: number; left: number }) {
  return (
    <g>
      {ticks.map((t) => (
        <g key={t}>
          <line className={t === 0 ? styles.baseline : styles.grid} x1={left} x2={width - PAD.right} y1={Math.round(y(t)) + 0.5} y2={Math.round(y(t)) + 0.5} />
          <text className={styles.axis} x={left - 8} y={y(t)} textAnchor="end" dominantBaseline="middle">
            {compactNumber(t)}
          </text>
        </g>
      ))}
    </g>
  );
}

/** Category labels along x, thinned so they never collide. */
function XLabels({ labels, x, y, slot }: { labels: string[]; x: (i: number) => number; y: number; slot: number }) {
  const longest = Math.min(16, Math.max(...labels.map((l) => l.length), 1));
  const every = Math.max(1, Math.ceil((longest * 6.5 + 8) / Math.max(slot, 1)));
  return (
    <g>
      {labels.map((l, i) =>
        i % every === 0 ? (
          <text key={i} className={styles.axis} x={x(i)} y={y + 16} textAnchor="middle">
            {l.length > 16 ? `${l.slice(0, 15)}…` : l}
          </text>
        ) : null,
      )}
    </g>
  );
}

/** A bar whose data end is rounded (4px) and whose baseline end is square. */
function barPath(x: number, w: number, y0: number, y1: number): string {
  const r = Math.min(4, w / 2, Math.abs(y1 - y0));
  const up = y1 < y0;
  const end = up ? y1 + r : y1 - r;
  const sweep = up ? 1 : 0;
  return `M${x},${y0}V${end}A${r},${r} 0 0 ${sweep} ${x + r},${y1}H${x + w - r}A${r},${r} 0 0 ${sweep} ${x + w},${end}V${y0}Z`;
}

function Cartesian({ data, kind, width, height, hover, onHover }: { data: ChartData; kind: 'bar' | 'line'; width: number; height: number; hover: Hover | null; onHover: (h: Hover | null) => void }) {
  const ticks = valueTicks(data, kind === 'bar');
  const left = leftPad(ticks);
  const y = scale(ticks[0]!, ticks[ticks.length - 1]!, height - PAD.bottom, PAD.top);
  const n = Math.max(data.categories.length, 1);
  const band = (width - left - PAD.right) / n;
  const center = (i: number) => left + band * i + band / 2;
  const zero = y(Math.max(ticks[0]!, Math.min(0, ticks[ticks.length - 1]!)));

  const hoverAt = (e: React.MouseEvent<SVGRectElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    const i = Math.max(0, Math.min(n - 1, Math.floor((e.clientX - rect.left) / band)));
    onHover({
      x: center(i),
      y: e.clientY - rect.top + PAD.top,
      index: i,
      title: data.categories[i] ?? '',
      rows: data.series.map((s, slot) => ({ slot, name: s.name, value: s.values[i] ?? null })),
    });
  };

  const m = data.series.length;
  const barWidth = Math.max(1, Math.min(BAR_MAX, (band * 0.8 - GAP * (m - 1)) / m));
  const group = barWidth * m + GAP * (m - 1);

  return (
    <g>
      <YAxis ticks={ticks} y={y} width={width} left={left} />
      <XLabels labels={data.categories} x={center} y={height - PAD.bottom} slot={band} />
      {kind === 'bar'
        ? data.series.map((s, slot) => (
            <g key={s.column} fill={color(slot)}>
              {s.values.map((v, i) =>
                v === null ? null : (
                  <path key={i} d={barPath(center(i) - group / 2 + slot * (barWidth + GAP), barWidth, zero, y(v))} opacity={hover && hover.index !== i ? 0.55 : 1} />
                ),
              )}
            </g>
          ))
        : data.series.map((s, slot) => {
            // Nulls break the line instead of dropping to zero.
            const path = s.values.reduce((d, v, i) => (v === null ? d : `${d}${i === 0 || s.values[i - 1] === null ? 'M' : 'L'}${center(i)},${y(v)}`), '');
            return <path key={s.column} d={path} fill="none" stroke={color(slot)} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />;
          })}
      {kind === 'line' && hover?.index !== undefined && (
        <g>
          <line className={styles.crosshair} x1={center(hover.index)} x2={center(hover.index)} y1={PAD.top} y2={height - PAD.bottom} />
          {data.series.map((s, slot) => {
            const v = s.values[hover.index!];
            return v === null || v === undefined ? null : <circle key={s.column} cx={center(hover.index!)} cy={y(v)} r={4} fill={color(slot)} stroke="var(--surface)" strokeWidth={2} />;
          })}
        </g>
      )}
      <rect x={left} y={PAD.top} width={Math.max(0, width - left - PAD.right)} height={Math.max(0, height - PAD.top - PAD.bottom)} fill="transparent" onMouseMove={hoverAt} />
    </g>
  );
}

function Scatter({ data, width, height, xName, onHover }: { data: ChartData; width: number; height: number; xName: string; onHover: (h: Hover | null) => void }) {
  const xs = data.xs.filter((v): v is number => v !== null);
  const xTicks = niceTicks(xs.length ? Math.min(...xs) : 0, xs.length ? Math.max(...xs) : 1);
  const yTicks = valueTicks(data, false);
  const left = leftPad(yTicks);
  const x = scale(xTicks[0]!, xTicks[xTicks.length - 1]!, left, width - PAD.right);
  const y = scale(yTicks[0]!, yTicks[yTicks.length - 1]!, height - PAD.bottom, PAD.top);
  return (
    <g>
      <YAxis ticks={yTicks} y={y} width={width} left={left} />
      {xTicks.map((t) => (
        <text key={t} className={styles.axis} x={x(t)} y={height - PAD.bottom + 16} textAnchor="middle">
          {compactNumber(t)}
        </text>
      ))}
      {data.series.map((s, slot) => (
        <g key={s.column} fill={color(slot)} stroke="var(--surface)" strokeWidth={2}>
          {s.values.map((v, i) => {
            const xv = data.xs[i];
            if (v === null || xv === null || xv === undefined) return null;
            return (
              <circle
                key={i}
                cx={x(xv)}
                cy={y(v)}
                r={4}
                onMouseEnter={() => onHover({ x: x(xv), y: y(v), title: `${xName} ${xv.toLocaleString()}`, rows: [{ slot, name: s.name, value: v }] })}
              >
                <title>{`${s.name}: ${v}`}</title>
              </circle>
            );
          })}
        </g>
      ))}
    </g>
  );
}

function Pie({ data, width, height, onHover }: { data: ChartData; width: number; height: number; onHover: (h: Hover | null) => void }) {
  const series = data.series[0];
  if (!series) return null;
  const values = series.values.map((v) => Math.max(0, v ?? 0));
  const total = values.reduce((a, b) => a + b, 0);
  const cx = width / 2;
  const cy = height / 2;
  const r = Math.max(10, Math.min(width, height) / 2 - 16);
  const inner = r * 0.58;
  if (total <= 0) return <text className={styles.axis} x={cx} y={cy} textAnchor="middle">No positive values to show.</text>;
  let angle = -Math.PI / 2;
  const point = (a: number, radius: number) => `${cx + radius * Math.cos(a)},${cy + radius * Math.sin(a)}`;
  return (
    <g>
      {values.map((v, i) => {
        const start = angle;
        const sweep = (v / total) * Math.PI * 2;
        angle += sweep;
        if (v === 0) return null;
        const end = Math.min(start + sweep, start + Math.PI * 2 - 1e-4);
        const large = end - start > Math.PI ? 1 : 0;
        const d = `M${point(start, r)}A${r},${r} 0 ${large} 1 ${point(end, r)}L${point(end, inner)}A${inner},${inner} 0 ${large} 0 ${point(start, inner)}Z`;
        const mid = start + sweep / 2;
        return (
          <path
            key={i}
            d={d}
            fill={color(i)}
            stroke="var(--surface)"
            strokeWidth={GAP}
            onMouseEnter={() =>
              onHover({ x: cx + (r * 0.8) * Math.cos(mid), y: cy + (r * 0.8) * Math.sin(mid), title: `${data.categories[i]} · ${((v / total) * 100).toFixed(1)}%`, rows: [{ slot: i, name: series.name, value: v }] })
            }
          />
        );
      })}
      <text className={styles.axis} x={cx} y={cy - 6} textAnchor="middle">
        {series.name}
      </text>
      <text x={cx} y={cy + 12} textAnchor="middle" fill="var(--ink)" fontSize={15} fontWeight={600}>
        {compactNumber(total)}
      </text>
      <g>
        {data.categories.map((c, i) => (
          <g key={i} transform={`translate(8, ${PAD.top + i * 18})`}>
            <rect width={10} height={10} rx={3} fill={color(i)} />
            <text className={styles.axis} x={16} y={9}>
              {c.length > 24 ? `${c.slice(0, 23)}…` : c} · {((values[i]! / total) * 100).toFixed(1)}%
            </text>
          </g>
        ))}
      </g>
    </g>
  );
}
