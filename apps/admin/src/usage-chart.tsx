import React, { useEffect, useMemo, useRef, useState } from 'react';
import { formatMoney, minorDigits } from './money';

// Charts for the usage dashboard, drawn as plain SVG: a stacked column
// chart over time, ranked bars for a breakdown, and stat tiles.
//
// Colors come from CSS custom properties (`--series-1` … `--series-7`, and
// `--series-other` for the folded tail), stepped for the light and dark
// themes in styles.css and validated for colour-vision deficiency; the
// series' order is fixed, and a value keeps its color while it stays on the
// chart (`useSeriesColors`). Every value a tooltip shows is also in the
// chart's table view.

export const SERIES_SLOTS = 7;

/** One stacked series: its key, its name, and its value per bucket. */
export type ChartSeries = { key: string; label: string; other: boolean; values: number[] };

/** Gives each value a fixed slot while it stays on the chart, so a filter
 * that changes which values show never repaints the ones that stay. */
export function useSeriesColors(scope: string, keys: string[]): Map<string, number> {
  const assigned = useRef<{ scope: string; slots: Map<string, number> }>({ scope, slots: new Map() });
  if (assigned.current.scope !== scope) assigned.current = { scope, slots: new Map() };
  return useMemo(() => assignSlots(assigned.current.slots, keys), [keys.join('\u0000'), scope]); // eslint-disable-line react-hooks/exhaustive-deps
}

/** Assigns `keys` slots, keeping the slot each already had; a new key takes
 * the first slot no current key holds. Mutates and returns `slots`. */
export function assignSlots(slots: Map<string, number>, keys: string[]): Map<string, number> {
  // A key keeps its slot unless a key still on the chart holds it (its old
  // slot was given away while it was off the chart): then it takes a free one.
  const taken = new Set<number>();
  const placed = new Set<string>();
  for (const key of keys) {
    const slot = slots.get(key);
    if (slot !== undefined && !taken.has(slot)) { taken.add(slot); placed.add(key); }
  }
  for (const key of keys) {
    if (placed.has(key)) continue;
    let slot = 0;
    while (taken.has(slot) && slot < SERIES_SLOTS - 1) slot += 1;
    slots.set(key, slot);
    taken.add(slot);
  }
  return slots;
}

export const seriesColor = (slot: number | undefined, other: boolean) => (other || slot === undefined ? 'var(--series-other)' : `var(--series-${slot + 1})`);

/** What a chart plots: a count, or money in one currency (money units). */
export type ChartUnit = { kind: 'count' } | { kind: 'money'; currency: string };

export function formatValue(value: number, unit: ChartUnit, compact = false): string {
  if (unit.kind === 'money') return compact ? compactMoney(value, unit.currency) : formatMoney(value, unit.currency);
  return compact ? new Intl.NumberFormat(undefined, { notation: 'compact', maximumFractionDigits: 1 }).format(value) : value.toLocaleString();
}

function compactMoney(amount: number, currency: string): string {
  const major = amount / 10 ** (6 + minorDigits(currency));
  try {
    return new Intl.NumberFormat(undefined, { style: 'currency', currency, notation: 'compact', maximumFractionDigits: major !== 0 && Math.abs(major) < 1 ? 4 : 1 }).format(major);
  } catch {
    return `${major.toFixed(2)} ${currency}`;
  }
}

/** Round axis ticks from 0 to at least `max`: 0, 1, 2, 5 × 10ⁿ steps. */
export function niceTicks(max: number, count = 4): number[] {
  if (!(max > 0)) return [0, 1];
  const raw = max / count;
  const power = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 5, 10].map(factor => factor * power).find(candidate => candidate >= raw) ?? 10 * power;
  const ticks: number[] = [];
  for (let value = 0; value < max + step / 2; value += step) ticks.push(value);
  if (ticks[ticks.length - 1] < max) ticks.push(ticks[ticks.length - 1] + step);
  return ticks;
}

/** A column with only its top corners rounded: square at the baseline. */
function topRounded(x: number, y: number, width: number, height: number, radius: number): string {
  const r = Math.max(0, Math.min(radius, width / 2, height));
  return `M${x},${y + height}V${y + r}Q${x},${y} ${x + r},${y}H${x + width - r}Q${x + width},${y} ${x + width},${y + r}V${y + height}Z`;
}

/** The width the chart was laid out at, so text stays at its CSS size
 * instead of scaling with the drawing. */
function useWidth(fallback: number): [React.RefObject<HTMLDivElement | null>, number] {
  const ref = useRef<HTMLDivElement | null>(null);
  const [width, setWidth] = useState(fallback);
  useEffect(() => {
    const node = ref.current;
    if (!node || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(([entry]) => {
      const next = Math.round(entry.contentRect.width);
      if (next > 0) setWidth(next);
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  return [ref, width];
}
const HEIGHT = 240;
const MARGIN = { top: 8, right: 8, bottom: 28, left: 64 };

/**
 * Stacked columns, one per bucket, one segment per series. Hovering or
 * focusing a column shows every series' value at it.
 */
export function StackedColumns({ label, buckets, bucketLabel, series, colors, unit }: {
  label: string;
  buckets: string[];
  bucketLabel: (bucket: string, short: boolean) => string;
  series: ChartSeries[];
  colors: Map<string, number>;
  unit: ChartUnit;
}) {
  const [active, setActive] = useState<number | null>(null);
  const [ref, WIDTH] = useWidth(720);
  // A new report may have fewer buckets than the one hovered.
  const bucketKey = buckets.join(',');
  useEffect(() => setActive(null), [bucketKey]);
  const shown = active !== null && active < buckets.length ? active : null;
  const totals = buckets.map((_, index) => series.reduce((sum, item) => sum + (item.values[index] ?? 0), 0));
  const ticks = niceTicks(Math.max(0, ...totals));
  const top = ticks[ticks.length - 1] || 1;
  const plotWidth = WIDTH - MARGIN.left - MARGIN.right;
  const plotHeight = HEIGHT - MARGIN.top - MARGIN.bottom;
  const band = plotWidth / Math.max(1, buckets.length);
  // Columns at most 24 wide, with air between them where there is room.
  const barWidth = Math.max(1, Math.min(24, band > 6 ? band - Math.max(2, band * 0.3) : band - (band > 2 ? 1 : 0)));
  const y = (value: number) => MARGIN.top + plotHeight - (value / top) * plotHeight;
  const labelEvery = Math.max(1, Math.ceil(buckets.length / Math.max(2, Math.floor(plotWidth / 90))));
  const summary = `${label}: ${buckets.length} ${buckets.length === 1 ? 'period' : 'periods'}, highest ${formatValue(Math.max(0, ...totals), unit)}.`;
  return (
    <div ref={ref} className="usage-chart" onPointerLeave={() => setActive(null)}>
      <svg viewBox={`0 0 ${WIDTH} ${HEIGHT}`} role="group" aria-label={summary} className="usage-chart-svg">
        {ticks.map(tick => (
          <g key={tick}>
            <line x1={MARGIN.left} x2={WIDTH - MARGIN.right} y1={y(tick)} y2={y(tick)} className={tick === 0 ? 'chart-baseline' : 'chart-grid'} />
            <text x={MARGIN.left - 8} y={y(tick)} dy="0.32em" textAnchor="end" className="chart-tick">{formatValue(tick, unit, true)}</text>
          </g>
        ))}
        {buckets.map((bucket, index) => {
          const x = MARGIN.left + index * band + (band - barWidth) / 2;
          let base = 0;
          const segments = series.map(item => {
            const value = item.values[index] ?? 0;
            const segment = { item, value, from: base, to: base + value };
            base += value;
            return segment;
          }).filter(segment => segment.value > 0);
          return (
            <g
              key={bucket}
              tabIndex={0}
              className={`chart-column${shown === index ? ' active' : ''}`}
              aria-label={`${bucketLabel(bucket, false)}: ${formatValue(totals[index], unit)}${segments.length > 1 ? `; ${segments.map(segment => `${segment.item.label} ${formatValue(segment.value, unit)}`).join(', ')}` : ''}`}
              onPointerEnter={() => setActive(index)}
              onFocus={() => setActive(index)}
              onBlur={() => setActive(null)}
            >
              {/* The hit target is the whole band, not just the painted column. */}
              <rect x={MARGIN.left + index * band} y={MARGIN.top} width={band} height={plotHeight} className="chart-hit" />
              {segments.map((segment, position) => {
                const top = y(segment.to);
                // A 2px surface gap separates stacked segments.
                const height = Math.max(0, y(segment.from) - top - (position > 0 ? 2 : 0));
                const fill = seriesColor(colors.get(segment.item.key), segment.item.other);
                return position === segments.length - 1
                  ? <path key={segment.item.key} d={topRounded(x, top, barWidth, height, 4)} style={{ fill }} />
                  : <rect key={segment.item.key} x={x} y={top} width={barWidth} height={height} style={{ fill }} />;
              })}
            </g>
          );
        })}
        {buckets.map((bucket, index) => (index % labelEvery === 0 ? (
          <text key={bucket} x={MARGIN.left + index * band + band / 2} y={HEIGHT - 8} textAnchor="middle" className="chart-tick">{bucketLabel(bucket, true)}</text>
        ) : null))}
      </svg>
      {shown !== null && (
        <div className="chart-tooltip" aria-hidden="true" style={{ left: `${Math.min(80, Math.max(0, ((MARGIN.left + shown * band) / WIDTH) * 100))}%` }}>
          <div className="chart-tooltip-title">{bucketLabel(buckets[shown], false)}</div>
          <div className="chart-tooltip-row"><strong>{formatValue(totals[shown], unit)}</strong><span>Total</span></div>
          {series.filter(item => (item.values[shown] ?? 0) > 0).slice().reverse().map(item => (
            <div key={item.key} className="chart-tooltip-row">
              <span className="chart-key" style={{ background: seriesColor(colors.get(item.key), item.other) }} aria-hidden="true" />
              <strong>{formatValue(item.values[shown] ?? 0, unit)}</strong>
              <span>{item.label}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** The chart's legend: a swatch beside each series' name, in stack order. */
export function ChartLegend({ series, colors }: { series: ChartSeries[]; colors: Map<string, number> }) {
  if (series.length < 2) return null;
  return (
    <ul className="chart-legend" aria-label="Legend">
      {series.map(item => (
        <li key={item.key}><span className="chart-swatch" style={{ background: seriesColor(colors.get(item.key), item.other) }} aria-hidden="true" />{item.label}</li>
      ))}
    </ul>
  );
}

/** The chart's values as a table: every bucket, every series. */
export function ChartTable({ label, buckets, bucketLabel, series, unit }: {
  label: string;
  buckets: string[];
  bucketLabel: (bucket: string, short: boolean) => string;
  series: ChartSeries[];
  unit: ChartUnit;
}) {
  const rows = buckets.map((bucket, index) => ({ bucket, values: series.map(item => item.values[index] ?? 0) })).filter(row => row.values.some(value => value > 0));
  return (
    <details className="chart-table">
      <summary>Show the chart as a table</summary>
      <div className="table-scroll">
        <table aria-label={label}>
          <thead><tr><th scope="col">Period</th>{series.map(item => <th scope="col" key={item.key}>{item.label}</th>)}</tr></thead>
          <tbody>
            {rows.map(row => (
              <tr key={row.bucket}><th scope="row">{bucketLabel(row.bucket, false)}</th>{row.values.map((value, index) => <td key={series[index].key}>{formatValue(value, unit)}</td>)}</tr>
            ))}
          </tbody>
        </table>
      </div>
      {rows.length === 0 && <p className="meta">Nothing in this window.</p>}
    </details>
  );
}

/** Ranked horizontal bars, largest first, in one hue: each value's share of
 * the whole, with an action to narrow the dashboard to it. */
export function RankedBars({ label, rows, unit, onSelect, selectLabel }: {
  label: string;
  rows: { key: string; label: string; value: number; other: boolean; selectable: boolean }[];
  unit: ChartUnit;
  onSelect: (key: string) => void;
  selectLabel: (row: { label: string }) => string;
}) {
  const max = Math.max(0, ...rows.map(row => row.value));
  const total = rows.reduce((sum, row) => sum + row.value, 0);
  return (
    <ol className="ranked-bars" aria-label={label}>
      {rows.map(row => (
        <li key={row.key} className="ranked-bar">
          <span className="ranked-name" title={row.label}>{row.label}</span>
          <span className="ranked-track" aria-hidden="true">
            <span className={`ranked-fill${row.other ? ' other' : ''}`} style={{ width: `${max ? Math.max(row.value > 0 ? 1 : 0, (row.value / max) * 100) : 0}%` }} />
          </span>
          <span className="ranked-value">{formatValue(row.value, unit)}<span className="meta"> · {total ? Math.round((row.value / total) * 100) : 0}%</span></span>
          {row.selectable
            ? <button type="button" className="inline-action" onClick={() => onSelect(row.key)} aria-label={selectLabel(row)}>Filter</button>
            : <span />}
        </li>
      ))}
    </ol>
  );
}

/** A headline figure with its change against the previous window. `upIsBad`
 * colors a rise as a problem (failures, latency); otherwise change is
 * neutral. */
export function StatTile({ label, value, current, previous, upIsBad, note }: {
  label: string;
  value: string;
  current: number | null;
  previous: number | null;
  upIsBad?: boolean;
  note?: string;
}) {
  const change = changeOf(current, previous);
  const tone = change && upIsBad !== undefined && change.direction !== 'flat' ? ((change.direction === 'up') === upIsBad ? 'bad' : 'good') : 'neutral';
  return (
    <article className="cds--tile card stat-tile">
      <div className="card-top"><span>{label}</span></div>
      <p className="report-figure">{value}</p>
      <p className={`stat-delta ${tone}`}>
        {change
          ? <><span aria-hidden="true">{change.direction === 'up' ? '▲' : change.direction === 'down' ? '▼' : '■'}</span> {change.text} vs the previous window</>
          : 'No previous requests to compare'}
      </p>
      {note && <p className="meta">{note}</p>}
    </article>
  );
}

/** The change from `previous` to `current`, as a signed percentage. */
export function changeOf(current: number | null, previous: number | null): { direction: 'up' | 'down' | 'flat'; text: string } | null {
  if (current === null || previous === null || previous === 0) return null;
  const ratio = (current - previous) / previous;
  const percent = Math.round(ratio * 1000) / 10;
  if (percent === 0) return { direction: 'flat', text: 'No change' };
  return { direction: percent > 0 ? 'up' : 'down', text: `${percent > 0 ? '+' : ''}${percent.toLocaleString()}%` };
}
