import React, { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  api,
  type ApiProblem,
  type Granularity,
  listFilter,
  type Page,
  type RankBy,
  type UsageDimension,
  type UsageEvent,
  type UsageMeasures,
  type UsageSeriesQuery,
  type UsageSeriesReport,
  type UsageSliceQuery,
} from './api';
import { ChartLegend, ChartTable, RankedBars, StackedColumns, StatTile, formatValue, useSeriesColors, type ChartSeries, type ChartUnit } from './usage-chart';
import { ConsoleLink, contentStateLabel, errorCategoryLabel, formatDuration, traceHref } from './traces-screen';
import { ReportError, loadFailed, type SignIn } from './screen-feedback';
import { formatCount, formatMoney } from './money';
import { readView } from './table-view';
import { ValueFilterForm } from './value-combo';
import { Button, DataCell, DataGrid, EmptyState, FormField, Message, Select, TableBody, TableHead, TableHeader, TableRow, TextInput, formatDate, uuidHint } from './ui';

// The usage dashboard: a slice of the organization's requests (a window and
// filters), charted over time and split by one dimension, its headline
// figures against the previous window, a breakdown of the split's values
// (optionally each split again by a second dimension), and the requests
// themselves, each linking to its trace. The slice lives in the address
// (`u.*`), so a view can be reloaded and shared.

export type RangeKey = '24h' | '7d' | '30d' | '90d' | 'custom';
export const ranges: [RangeKey, string][] = [['24h', 'Last 24 hours'], ['7d', 'Last 7 days'], ['30d', 'Last 30 days'], ['90d', 'Last 90 days'], ['custom', 'Custom range']];
const rangeDays: Record<Exclude<RangeKey, 'custom'>, number> = { '24h': 1, '7d': 7, '30d': 30, '90d': 90 };

export type Metric = 'requests' | 'tokens' | 'input' | 'output' | 'cost' | 'failed';
const metrics: [Metric, string][] = [['requests', 'Requests'], ['tokens', 'Tokens (in + out)'], ['input', 'Input tokens'], ['output', 'Output tokens'], ['cost', 'Spend'], ['failed', 'Failed requests']];

export const dimensions: [UsageDimension, string][] = [
  ['model', 'Model'], ['alias', 'Alias'], ['api_key', 'API key'], ['service_account', 'Service account'], ['team', 'Team'],
  ['user', 'Owner'], ['end_user', 'End user'], ['deployment', 'Deployment'], ['provider_family', 'Provider'], ['location', 'Location'],
  ['operation', 'Operation'], ['status', 'Status'], ['outcome', 'Outcome'], ['error_category', 'Error'],
  ['model_master', 'Model across providers'],
];
const dimensionLabel = Object.fromEntries(dimensions) as Record<UsageDimension, string>;
/** What the slice filters by: every dimension, and the batch a request was
 * a line of (a filter only: requests are not split by batch). */
export type FilterKey = UsageDimension | 'batch';
const filterKeys: FilterKey[] = [...dimensions.map(([key]) => key), 'batch'];
const filterLabel: Record<FilterKey, string> = { ...dimensionLabel, batch: 'Batch' };
/** The query parameter each filter sets. */
const filterParam: Record<FilterKey, keyof UsageSliceQuery> = {
  batch: 'batch_id',
  team: 'team_id', service_account: 'service_account_id', api_key: 'api_key_id', user: 'user_id', end_user: 'end_user', model: 'model', alias: 'alias',
  deployment: 'deployment', provider_family: 'provider_family', location: 'location', operation: 'operation', status: 'status',
  outcome: 'outcome', error_category: 'error_category', model_master: 'model_master',
};
const identities: UsageDimension[] = ['team', 'service_account', 'api_key', 'user'];
const SERIES_LIMIT = 7;
const EVENTS_PAGE = 25;

export type DashboardState = {
  range: RangeKey;
  /** Custom range: UTC dates, both inclusive. */
  from?: string;
  to?: string;
  granularity?: Granularity;
  split: UsageDimension | null;
  /** Each split value split again by this, in the breakdown only. */
  then?: UsageDimension;
  metric: Metric;
  currency?: string;
  filters: Partial<Record<FilterKey, string[]>>;
};

const PREFIX = 'u.';

/** A list as the address keeps it: comma-separated, `\,` and `\\` escaped. */
function splitList(value: string): string[] {
  const values = [''];
  for (let index = 0; index < value.length; index += 1) {
    const character = value[index];
    if (character === '\\' && index + 1 < value.length) values[values.length - 1] += value[++index];
    else if (character === ',') values.push('');
    else values[values.length - 1] += character;
  }
  return values.filter(Boolean);
}

/** The dimensions of the Breakdown page the dashboard replaced. */
const breakdownDimensions: UsageDimension[] = ['team', 'service_account', 'model', 'deployment', 'provider_family', 'location'];

/** A view of the former Breakdown page (`usage.*`: grouped by up to two
 * columns, filtered by lists and a day range) as the dashboard's. */
function fromBreakdown(search: string): DashboardState {
  const view = readView(search, 'usage');
  const [split, then] = view.group.filter((id): id is UsageDimension => breakdownDimensions.includes(id as UsageDimension));
  const filters: DashboardState['filters'] = {};
  for (const dimension of breakdownDimensions) if (view.categories[dimension]?.length) filters[dimension] = view.categories[dimension];
  // Its bounds as they were (dates or instants, the end inclusive); an open
  // end stays open (see `dashboardWindow`).
  const day = view.ranges.day;
  const custom = day?.min || day?.max ? { range: 'custom' as const, from: day.min, to: day.max } : { range: '30d' as const };
  return {
    ...custom,
    // Breakdown grouped by day unless told otherwise: the chart's own axis.
    split: split ?? null,
    ...(split && then ? { then } : {}),
    metric: 'requests',
    filters,
  };
}

export function readDashboard(search: string): DashboardState {
  const params = new URLSearchParams(search);
  const names = [...params.keys()];
  if (!names.some(name => name.startsWith(PREFIX)) && names.some(name => name.startsWith('usage.'))) return fromBreakdown(search);
  const get = (key: string) => params.get(`${PREFIX}${key}`) ?? undefined;
  const range = ranges.some(([key]) => key === get('range')) ? (get('range') as RangeKey) : '7d';
  const split = get('split');
  const filters: DashboardState['filters'] = {};
  for (const dimension of filterKeys) {
    const value = get(`f.${dimension}`);
    if (value) filters[dimension] = splitList(value);
  }
  const chosen = split === 'none' ? null : dimensions.some(([key]) => key === split) ? (split as UsageDimension) : 'model';
  const then = dimensions.find(([key]) => key === get('then') && key !== chosen)?.[0];
  return {
    range,
    from: get('from'),
    to: get('to'),
    granularity: (['hour', 'day', 'week'] as const).find(value => value === get('gran')),
    split: chosen,
    ...(chosen && then ? { then } : {}),
    metric: metrics.some(([key]) => key === get('metric')) ? (get('metric') as Metric) : 'requests',
    currency: get('cur'),
    filters,
  };
}

export function writeDashboard(search: string, state: DashboardState): string {
  const params = new URLSearchParams(search);
  // A former Breakdown view, read once, gives way to the dashboard's own.
  for (const name of [...params.keys()]) if (name.startsWith(PREFIX) || name.startsWith('usage.')) params.delete(name);
  const set = (key: string, value: string | undefined) => { if (value) params.set(`${PREFIX}${key}`, value); };
  if (state.range !== '7d') set('range', state.range);
  if (state.range === 'custom') { set('from', state.from); set('to', state.to); }
  set('gran', state.granularity);
  if (state.split !== 'model') set('split', state.split ?? 'none');
  if (state.split && state.then !== state.split) set('then', state.then);
  if (state.metric !== 'requests') set('metric', state.metric);
  if (state.metric === 'cost') set('cur', state.currency);
  for (const [dimension, values] of Object.entries(state.filters)) set(`f.${dimension}`, listFilter(values));
  const text = params.toString();
  return text ? `?${text}` : '';
}

function useDashboardState(): [DashboardState, (next: DashboardState) => void] {
  const [state, setState] = useState(() => readDashboard(location.search));
  useEffect(() => {
    const reread = () => setState(readDashboard(location.search));
    addEventListener('popstate', reread);
    return () => removeEventListener('popstate', reread);
  }, []);
  const update = useCallback((next: DashboardState) => {
    setState(next);
    history.replaceState(history.state, '', `${location.pathname}${writeDashboard(location.search, next)}${location.hash}`);
  }, []);
  return [state, update];
}

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

const floorTo = (at: number, granularity: Granularity) => {
  const day = Math.floor(at / DAY) * DAY;
  if (granularity === 'hour') return Math.floor(at / HOUR) * HOUR;
  if (granularity === 'day') return day;
  // Weeks start on Monday (1970-01-01 was a Thursday).
  const weekday = (new Date(day).getUTCDay() + 6) % 7;
  return day - weekday * DAY;
};

/** The granularity a window reads best at. */
export function defaultGranularity(spanMs: number): Granularity {
  if (spanMs <= 2 * DAY) return 'hour';
  if (spanMs <= 120 * DAY) return 'day';
  return 'week';
}

/** An instant without a zone (`2026-11-01T00:00`, as Breakdown wrote them)
 * is UTC. */
function utcInstant(value: string): number {
  return Date.parse(/(z|[+-]\d\d:?\d\d)$/i.test(value) ? value : `${value}Z`);
}

/** The window a state asks for, at `now`: a preset ends now and starts at
 * its granularity's bucket boundary, so the first bucket is whole; a custom
 * range covers its whole UTC days. Null for an incomplete custom range. */
export function dashboardWindow(state: DashboardState, now: number): { from: string; to: string; granularity: Granularity } | null {
  let from: number;
  let to: number;
  if (state.range === 'custom') {
    if (!state.from && !state.to) return null;
    // A date covers its whole UTC day; an instant (from an old Breakdown
    // link) is exact, the end inclusive. An open end is now; an open start,
    // 30 days before the end, as Breakdown read them.
    to = !state.to ? now : state.to.includes('T') ? utcInstant(state.to) + 1 : Date.parse(`${state.to}T00:00:00Z`) + DAY;
    from = !state.from ? to - 30 * DAY : state.from.includes('T') ? utcInstant(state.from) : Date.parse(`${state.from}T00:00:00Z`);
    if (!(from < to)) return null;
  } else {
    to = now;
    from = now - rangeDays[state.range] * DAY;
  }
  const granularity = state.granularity ?? defaultGranularity(to - from);
  if (state.range !== 'custom') from = floorTo(from, granularity);
  return { from: new Date(from).toISOString(), to: new Date(to).toISOString(), granularity };
}

/** The slice's API filters. */
export function sliceQuery(state: DashboardState, window: { from: string; to: string }): UsageSliceQuery {
  const query: UsageSliceQuery = { from: window.from, to: window.to };
  for (const [dimension, values] of Object.entries(state.filters) as [FilterKey, string[]][]) {
    const value = listFilter(values);
    if (value) (query as Record<string, string>)[filterParam[dimension]] = value;
  }
  if (state.metric === 'cost' && state.currency) query.currency = state.currency;
  return query;
}

// The API ranks by requests, tokens or spend; input, output and failures
// are ranked by the nearest of those, and the page says so.
const rankBy: Record<Metric, RankBy> = { requests: 'requests', tokens: 'tokens', input: 'tokens', output: 'tokens', cost: 'cost', failed: 'requests' };
const rankNames: Record<RankBy, string> = { requests: 'requests', tokens: 'tokens (in + out)', cost: 'spend' };
/** Whether the top values are chosen by a measure other than the one shown. */
export const rankedByOther = (metric: Metric) => metric === 'input' || metric === 'output' || metric === 'failed';

/** The plotted value of a set of requests. */
export function metricValue(measures: UsageMeasures, metric: Metric, currency?: string): number {
  switch (metric) {
    case 'requests': return measures.requests;
    case 'tokens': return measures.tokens.input + measures.tokens.output;
    case 'input': return measures.tokens.input;
    case 'output': return measures.tokens.output;
    case 'failed': return measures.failed_requests;
    case 'cost': return measures.amounts.find(amount => amount.currency === currency)?.amount_minor ?? 0;
  }
}

const statusLabels: Record<string, string> = { succeeded: 'Succeeded', failed: 'Failed', pending: 'Pending', expired: 'Expired before the call', unknown: 'Result unknown', 'failed-over': 'Failed over' };

/** How a dimension's value reads. `null` is a request without one. */
export function valueLabel(dimension: UsageDimension | null, value: string | null, label?: string | null): string {
  if (label) return label;
  if (dimension === null) return 'All requests';
  if (value === null) {
    switch (dimension) {
      case 'team': return 'No team';
      case 'api_key': return 'No key recorded';
      case 'user': return 'No owner';
      case 'end_user': return 'No end user';
      case 'outcome': return 'Not settled';
      case 'error_category': return 'No error';
      case 'service_account': return 'No service account';
      case 'model_master': return 'No master record';
      default: return 'No catalog provenance';
    }
  }
  if (dimension === 'status') return statusLabels[value] ?? value;
  if (dimension === 'error_category') return errorCategoryLabel(value);
  if (identities.includes(dimension)) return uuidHint(value) ?? value;
  return value;
}

/** A bucket's start as the chart labels it. */
export function bucketLabel(bucket: string, granularity: Granularity, short: boolean): string {
  const date = new Date(bucket);
  if (granularity === 'hour') {
    return short
      ? date.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', timeZone: 'UTC' })
      : `${date.toLocaleString(undefined, { month: 'short', day: 'numeric', hour: '2-digit', minute: '2-digit', timeZone: 'UTC' })} UTC`;
  }
  const day = date.toLocaleDateString(undefined, { month: 'short', day: 'numeric', timeZone: 'UTC' });
  return granularity === 'week' && !short ? `Week of ${day}` : day;
}

/** The chart's series: each split value's metric per bucket. */
export function chartSeries(report: UsageSeriesReport, metric: Metric, currency?: string): ChartSeries[] {
  const index = new Map(report.buckets.map((bucket, position) => [Date.parse(bucket), position]));
  return report.series.map(series => {
    const values = report.buckets.map(() => 0);
    for (const point of series.points) {
      const position = index.get(Date.parse(point.bucket));
      if (position !== undefined) values[position] = metricValue(point, metric, currency);
    }
    return {
      key: series.other ? '\u0000other' : series.value ?? '\u0000none',
      label: series.other ? 'Other' : valueLabel(report.split_by, series.value, series.label),
      other: series.other,
      values,
    };
  });
}

const utcTime = (at: string) => new Date(at).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short', timeZone: 'UTC' });

const percent = (part: number, whole: number) => (whole ? `${(Math.round((part / whole) * 1000) / 10).toLocaleString()}%` : '—');

export function UsageDashboard({ organizationId, ...signIn }: { organizationId: string } & SignIn) {
  const [state, setState] = useDashboardState();
  // A preset window ends when it was chosen, so the chart and the request
  // list ask for the same window until refreshed.
  const [now, setNow] = useState(() => Date.now());
  const window = useMemo(() => dashboardWindow(state, now), [state, now]);
  const slice = useMemo(() => (window ? sliceQuery(state, window) : null), [state, window]);
  const metricCurrency = state.metric === 'cost' ? state.currency : undefined;
  const seriesQuery = useMemo<UsageSeriesQuery | null>(() => slice && window && {
    ...slice,
    granularity: window.granularity,
    split_by: state.split ?? undefined,
    limit: SERIES_LIMIT,
    rank_by: state.metric === 'cost' && !slice.currency ? 'requests' : rankBy[state.metric],
  }, [slice, window, state.split, state.metric]);
  // The report with the query and view that produced it: a report shows
  // only for its own query, so its labels and actions never meet another
  // split's.
  const [loaded, setLoaded] = useState<{ key: string; report: UsageSeriesReport } | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [loading, setLoading] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const seriesKey = JSON.stringify(seriesQuery);
  useEffect(() => {
    if (!seriesQuery) return;
    let current = true;
    setLoading(true);
    setProblem(null);
    api.usageSeries(organizationId, seriesQuery).then(
      value => { if (current) setLoaded({ key: seriesKey, report: value }); },
      caught => { if (current) { setLoaded(null); setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The usage could not be loaded.')); } },
    ).finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [organizationId, seriesKey, attempt]); // eslint-disable-line react-hooks/exhaustive-deps
  const report = loaded && loaded.key === seriesKey ? loaded.report : null;

  // Names seen for identity values, for filter chips.
  const names = useRef(new Map<string, string>());
  for (const series of report?.series ?? []) if (series.value && series.label) names.current.set(`${report!.split_by}:${series.value}`, series.label);

  // The currencies a slice has, remembered across the currency filter
  // (spend asks for one currency's requests only) until the window or the
  // other filters change.
  const currencyScope = slice ? JSON.stringify({ ...slice, currency: undefined }) : '';
  const seenCurrencies = useRef<{ scope: string; codes: Set<string> }>({ scope: '', codes: new Set() });
  if (seenCurrencies.current.scope !== currencyScope) seenCurrencies.current = { scope: currencyScope, codes: new Set() };
  for (const amount of [...(report?.totals.amounts ?? []), ...(report?.previous.totals.amounts ?? [])]) seenCurrencies.current.codes.add(amount.currency);
  const seenKey = [...seenCurrencies.current.codes].sort().join(',');
  const currencies = useMemo(() => {
    const seen = new Set(seenKey ? seenKey.split(',') : []);
    if (state.currency) seen.add(state.currency);
    return [...seen].sort();
  }, [seenKey, state.currency]);
  // Spend charts one currency: the chosen one, else the first there is.
  useEffect(() => {
    if (state.metric === 'cost' && !state.currency && currencies.length) setState({ ...state, currency: currencies[0] });
  }, [state, currencies, setState]);

  const update = (patch: Partial<DashboardState>) => setState({ ...state, ...patch });
  const addFilter = (dimension: UsageDimension, value: string, label?: string) => {
    if (label && label !== value) names.current.set(`${dimension}:${value}`, label);
    const values = state.filters[dimension] ?? [];
    if (!values.includes(value)) update({ filters: { ...state.filters, [dimension]: [...values, value] } });
  };
  /** A breakdown click: only that value of the dimension, whatever was chosen before. */
  const narrowTo = (dimension: UsageDimension, value: string) => update({ filters: { ...state.filters, [dimension]: [value] } });
  /** A second-grouping click: only that pair of values. */
  const narrowToPair = (first: UsageDimension, value: string, second: UsageDimension, other: string) =>
    update({ filters: { ...state.filters, [first]: [value], [second]: [other] } });
  // A filter's values are looked up in the slice without its own filter, in
  // every currency.
  const valuesQuery = useCallback((dimension: UsageDimension): UsageSliceQuery | null => {
    if (!slice) return null;
    const query: Record<string, string | undefined> = { ...slice };
    delete query[filterParam[dimension]];
    delete query.currency;
    return query as UsageSliceQuery;
  }, [slice]);
  const removeFilter = (dimension: FilterKey, value: string) => {
    const values = (state.filters[dimension] ?? []).filter(item => item !== value);
    const filters = { ...state.filters };
    if (values.length) filters[dimension] = values; else delete filters[dimension];
    update({ filters });
  };

  const span = window ? Date.parse(window.to) - Date.parse(window.from) : 0;
  const unit: ChartUnit = state.metric === 'cost' && metricCurrency ? { kind: 'money', currency: metricCurrency } : { kind: 'count' };
  const series = report ? chartSeries(report, state.metric, metricCurrency) : [];
  const colors = useSeriesColors(`${state.split}`, series.filter(item => !item.other).map(item => item.key));
  const granularity = report?.granularity ?? window?.granularity ?? 'day';
  const labelOf = (bucket: string, short: boolean) => bucketLabel(bucket, granularity, short);
  const metricName = metrics.find(([key]) => key === state.metric)![1];
  const chartLabel = `${metricName}${unit.kind === 'money' ? ` in ${unit.currency}` : ''} by ${granularity}${state.split ? `, by ${dimensionLabel[state.split].toLowerCase()}` : ''}`;

  return (
    <div className="usage-dashboard">
      <div className="report-controls dashboard-controls" aria-label="Usage filters">
        <div className="audit-filter"><FormField label="Time range" htmlFor="usage-range">
          <Select id="usage-range" value={state.range} onChange={event => {
            const range = event.target.value as RangeKey;
            const today = new Date().toISOString().slice(0, 10);
            setNow(Date.now());
            update({ range, granularity: undefined, ...(range === 'custom' && !state.from ? { from: new Date(Date.now() - 6 * DAY).toISOString().slice(0, 10), to: today } : {}) });
          }}>
            {ranges.map(([key, label]) => <option key={key} value={key}>{label}</option>)}
          </Select>
        </FormField></div>
        {state.range === 'custom' && (
          <>
            <div className="audit-filter"><FormField label="From (UTC)" htmlFor="usage-from">
              <TextInput id="usage-from" type="date" value={state.from?.slice(0, 10) ?? ''} max={state.to?.slice(0, 10)} onChange={event => update({ from: event.target.value })} />
            </FormField></div>
            <div className="audit-filter"><FormField label="To (UTC, inclusive)" htmlFor="usage-to">
              <TextInput id="usage-to" type="date" value={state.to?.slice(0, 10) ?? ''} min={state.from?.slice(0, 10)} onChange={event => update({ to: event.target.value })} />
            </FormField></div>
          </>
        )}
        <div className="audit-filter"><FormField label="Interval" htmlFor="usage-granularity">
          <Select id="usage-granularity" value={window?.granularity ?? 'day'} onChange={event => update({ granularity: event.target.value as Granularity })}>
            <option value="hour" disabled={span > 31 * DAY}>Hour</option>
            <option value="day">Day</option>
            <option value="week">Week</option>
          </Select>
        </FormField></div>
        <div className="audit-filter"><FormField label="Show" htmlFor="usage-metric">
          <Select id="usage-metric" value={state.metric} onChange={event => update({ metric: event.target.value as Metric })}>
            {metrics.map(([key, label]) => <option key={key} value={key}>{label}</option>)}
          </Select>
        </FormField></div>
        {state.metric === 'cost' && (
          <div className="audit-filter"><FormField label="Currency" htmlFor="usage-currency">
            <Select id="usage-currency" value={state.currency ?? ''} onChange={event => update({ currency: event.target.value })}>
              {currencies.length === 0 && <option value="">No priced requests</option>}
              {currencies.map(currency => <option key={currency} value={currency}>{currency}</option>)}
            </Select>
          </FormField></div>
        )}
        <div className="audit-filter"><FormField label="Split by" htmlFor="usage-split">
          <Select id="usage-split" value={state.split ?? 'none'} onChange={event => {
            const split = event.target.value === 'none' ? null : event.target.value as UsageDimension;
            update({ split, then: split && state.then !== split ? state.then : undefined });
          }}>
            <option value="none">Nothing</option>
            {dimensions.map(([key, label]) => <option key={key} value={key}>{label}</option>)}
          </Select>
        </FormField></div>
        {state.split && (
          <div className="audit-filter"><FormField label="Then by" htmlFor="usage-then">
            <Select id="usage-then" value={state.then ?? 'none'} onChange={event => update({ then: event.target.value === 'none' ? undefined : event.target.value as UsageDimension })}>
              <option value="none">Nothing</option>
              {dimensions.filter(([key]) => key !== state.split).map(([key, label]) => <option key={key} value={key}>{label}</option>)}
            </Select>
          </FormField></div>
        )}
        {state.range !== 'custom' && <Button variant="ghost" onClick={() => setNow(Date.now())}>Refresh</Button>}
      </div>
      <FilterChips state={state} names={names.current} onRemove={removeFilter} onClear={() => update({ filters: {} })} />
      <ValueFilterForm
        organizationId={organizationId}
        idPrefix="usage-filter"
        dimensions={dimensions}
        initial="api_key"
        queryFor={valuesQuery}
        labelOf={valueLabel}
        onAdd={addFilter}
      />

      {!window && <Message kind="error">Choose a start and an end date, the start no later than the end.</Message>}
      {problem && (problem.status === 400
        ? <Message kind="error">This view is not valid: {problem.detail ?? 'check the filters.'} Choose a shorter range, a coarser interval, or change the filters.</Message>
        : <ReportError problem={problem} what="usage" onRetry={() => { setNow(Date.now()); setAttempt(value => value + 1); }} {...signIn} />)}
      {loading && !report && <Message kind="info">Loading usage…</Message>}

      {report && (
        <div className={loading ? 'refreshing' : undefined} aria-busy={loading}>
          <Tiles report={report} />
          <section className="cds--tile dashboard-panel" aria-labelledby="usage-chart-title">
            <div className="panel-head">
              <h2 id="usage-chart-title">{chartLabel}</h2>
              <p className="meta">{utcTime(report.from)} to {utcTime(report.to)} UTC{state.metric === 'cost' ? ' · only requests charged in this currency' : ''}{report.truncated ? ` · the top ${SERIES_LIMIT} by ${rankNames[report.rank_by]}, the rest as Other` : ''}</p>
            </div>
            {report.totals.requests === 0
              ? <EmptyState title="No requests" text="Nothing matches this window and these filters." />
              : (
                <>
                  <ChartLegend series={series} colors={colors} />
                  <StackedColumns label={chartLabel} buckets={report.buckets} bucketLabel={labelOf} series={series} colors={colors} unit={unit} />
                  <ChartTable label={`${chartLabel}, as a table`} buckets={report.buckets} bucketLabel={labelOf} series={series} unit={unit} />
                </>
              )}
          </section>
          {state.split && report.series.length > 0 && (
            <section className="cds--tile dashboard-panel" aria-labelledby="usage-breakdown-title">
              <div className="panel-head">
                <h2 id="usage-breakdown-title">By {dimensionLabel[state.split].toLowerCase()}{state.then ? `, then by ${dimensionLabel[state.then].toLowerCase()}` : ''}</h2>
                <p className="meta">{metricName}{unit.kind === 'money' ? ` in ${unit.currency}` : ''} over the window, largest first{report.truncated && rankedByOther(state.metric) ? `; the values shown are the top ${SERIES_LIMIT} by ${rankNames[report.rank_by]}` : ''}. Filter to a value to see only its requests.</p>
              </div>
              {state.then && seriesQuery ? (
                <ThenByTable
                  organizationId={organizationId}
                  base={seriesQuery}
                  report={report}
                  labels={series.map(item => item.label)}
                  split={state.split}
                  then={state.then}
                  metric={state.metric}
                  currency={metricCurrency}
                  unit={unit}
                  onNarrow={value => narrowTo(state.split!, value)}
                  onNarrowPair={(value, other) => narrowToPair(state.split!, value, state.then!, other)}
                  {...signIn}
                />
              ) : <RankedBars
                label={`${metricName} by ${dimensionLabel[state.split].toLowerCase()}`}
                unit={unit}
                rows={report.series.map((item, index) => ({
                  key: item.value ?? `\u0000${index}`,
                  label: series[index].label,
                  value: metricValue(item.totals, state.metric, metricCurrency),
                  other: item.other,
                  selectable: !item.other && item.value !== null,
                })).sort((a, b) => Number(a.other) - Number(b.other) || b.value - a.value)}
                onSelect={value => narrowTo(state.split!, value)}
                selectLabel={row => `Filter to ${row.label}`}
              />}
            </section>
          )}
        </div>
      )}
      {slice && <Events organizationId={organizationId} slice={slice} {...signIn} />}
    </div>
  );
}

function Tiles({ report }: { report: UsageSeriesReport }) {
  const { totals, previous } = report;
  const prior = previous.totals;
  const had = prior.requests > 0;
  const failureRate = (measures: UsageMeasures) => (measures.requests ? measures.failed_requests / measures.requests : null);
  const caveats = [
    totals.unpriced_requests ? `${formatCount(totals.unpriced_requests)} unpriced` : '',
    totals.no_usage_requests ? `${formatCount(totals.no_usage_requests)} without usage` : '',
    totals.incomplete_requests ? `${formatCount(totals.incomplete_requests)} incomplete` : '',
  ].filter(Boolean).join(' · ');
  return (
    <div className="cards report-totals stat-tiles">
      <StatTile label="Requests" value={formatCount(totals.requests)} current={totals.requests} previous={had ? prior.requests : null} note={[
        totals.pending_requests ? `${formatCount(totals.pending_requests)} still running` : '',
        totals.expired_requests ? `${formatCount(totals.expired_requests)} expired before the call` : '',
        totals.unknown_requests ? `${formatCount(totals.unknown_requests)} with an unknown result` : '',
      ].filter(Boolean).join(' · ') || undefined} />
      <StatTile label="Input tokens" value={formatCount(totals.tokens.input)} current={totals.tokens.input} previous={had ? prior.tokens.input : null} note={totals.tokens.cached_input ? `${formatCount(totals.tokens.cached_input)} cached` : undefined} />
      <StatTile label="Output tokens" value={formatCount(totals.tokens.output)} current={totals.tokens.output} previous={had ? prior.tokens.output : null} note={totals.tokens.reasoning ? `${formatCount(totals.tokens.reasoning)} reasoning` : undefined} />
      {totals.amounts.length === 0 && <StatTile label="Spend" value="—" current={null} previous={null} note={caveats || 'No priced requests'} />}
      {totals.amounts.map(amount => {
        const before = prior.amounts.find(item => item.currency === amount.currency)?.amount_minor ?? null;
        return <StatTile key={amount.currency} label={`Spend · ${amount.currency}`} value={formatMoney(amount.amount_minor, amount.currency)} current={amount.amount_minor} previous={had ? before : null} note={caveats || undefined} />;
      })}
      <StatTile label="Failed" value={percent(totals.failed_requests, totals.requests)} current={failureRate(totals)} previous={had ? failureRate(prior) : null} upIsBad note={`${formatCount(totals.failed_requests)} of ${formatCount(totals.requests)}`} />
      <StatTile label="Duration, 95th percentile" value={formatDuration(totals.duration.p95_ms)} current={totals.duration.p95_ms} previous={had ? prior.duration.p95_ms : null} upIsBad note={`Median ${formatDuration(totals.duration.p50_ms)} · average ${formatDuration(totals.duration.average_ms)}`} />
    </div>
  );
}

function FilterChips({ state, names, onRemove, onClear }: { state: DashboardState; names: Map<string, string>; onRemove: (dimension: FilterKey, value: string) => void; onClear: () => void }) {
  const chips = (Object.entries(state.filters) as [FilterKey, string[]][]).flatMap(([dimension, values]) => values.map(value => ({ dimension, value })));
  if (!chips.length) return null;
  return (
    <div className="filter-chips" aria-label="Active filters">
      {chips.map(({ dimension, value }) => {
        const text = `${filterLabel[dimension]}: ${dimension === 'batch' ? value : valueLabel(dimension, value, names.get(`${dimension}:${value}`))}`;
        return (
          <span key={`${dimension}:${value}`} className="chip">
            {text}
            <button type="button" className="chip-remove" aria-label={`Remove ${text}`} onClick={() => onRemove(dimension, value)}>×</button>
          </span>
        );
      })}
      <button type="button" className="inline-action" onClick={onClear}>Clear filters</button>
    </div>
  );
}

/** The breakdown with each split value split again: one series report per
 * value (the API splits by one dimension at a time), each value's rows
 * filterable to the pair. The folded tail and missing values cannot be
 * filtered to, so they are not split again. */
function ThenByTable({ organizationId, base, report, labels, split, then, metric, currency, unit, onNarrow, onNarrowPair, ...signIn }: {
  organizationId: string;
  base: UsageSeriesQuery;
  report: UsageSeriesReport;
  /** Each series' name, in the report's order. */
  labels: string[];
  split: UsageDimension;
  then: UsageDimension;
  metric: Metric;
  currency?: string;
  unit: ChartUnit;
  onNarrow: (value: string) => void;
  onNarrowPair: (value: string, other: string) => void;
} & SignIn) {
  const parents = report.series
    .map((item, index) => ({ item, label: labels[index], total: metricValue(item.totals, metric, currency) }))
    .sort((a, b) => Number(a.item.other) - Number(b.item.other) || b.total - a.total);
  const queries = parents.map(({ item }) => (item.other || item.value === null
    ? null
    : { ...base, [filterParam[split]]: listFilter([item.value]), split_by: then } as UsageSeriesQuery));
  const key = JSON.stringify([organizationId, queries]);
  const [loaded, setLoaded] = useState<{ key: string; reports: (UsageSeriesReport | null)[] } | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setProblem(null);
    Promise.all(queries.map(query => (query ? api.usageSeries(organizationId, query) : Promise.resolve(null)))).then(
      reports => { if (current) setLoaded({ key, reports }); },
      caught => { if (current) setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The second grouping could not be loaded.')); },
    );
    return () => { current = false; };
  }, [key, attempt]); // eslint-disable-line react-hooks/exhaustive-deps
  const reports = loaded && loaded.key === key ? loaded.reports : null;
  const grand = parents.reduce((sum, parent) => sum + parent.total, 0);
  const splitName = dimensionLabel[split];
  const thenName = dimensionLabel[then];
  if (problem) return <ReportError problem={problem} what="second grouping" onRetry={() => setAttempt(value => value + 1)} {...signIn} />;
  return (
    <>
      {!reports && <Message kind="info">Loading the second grouping…</Message>}
      <DataGrid aria-label={`By ${splitName.toLowerCase()}, then by ${thenName.toLowerCase()}`} className="then-by-table">
        <TableHead>
          <TableRow className="data-row head">
            <TableHeader>{splitName}</TableHeader>
            <TableHeader>{thenName}</TableHeader>
            <TableHeader>Value</TableHeader>
            <TableHeader>Share</TableHeader>
            <TableHeader className="row-actions">Action</TableHeader>
          </TableRow>
        </TableHead>
        <TableBody>
          {parents.map(({ item, label, total }, index) => {
            const children = reports?.[index];
            return (
              <React.Fragment key={item.other ? '\u0000other' : item.value ?? '\u0000none'}>
                <TableRow className="data-row group-row">
                  <DataCell label={splitName} className="cell-primary">{label}</DataCell>
                  <DataCell label={thenName}><span className="muted">{item.other || item.value === null ? 'Not split further' : 'All'}</span></DataCell>
                  <DataCell label="Value">{formatValue(total, unit)}</DataCell>
                  <DataCell label="Share">{percent(total, grand)}</DataCell>
                  <DataCell label="Action">
                    {!item.other && item.value !== null
                      ? <button type="button" className="inline-action" onClick={() => onNarrow(item.value!)} aria-label={`Filter to ${label}`}>Filter</button>
                      : '—'}
                  </DataCell>
                </TableRow>
                {children && children.series
                  .map(child => ({ child, childLabel: child.other ? 'Other' : valueLabel(then, child.value, child.label), value: metricValue(child.totals, metric, currency) }))
                  .sort((a, b) => Number(a.child.other) - Number(b.child.other) || b.value - a.value)
                  .map(({ child, childLabel, value }) => (
                    <TableRow className="data-row then-row" key={child.other ? '\u0000other' : child.value ?? '\u0000none'}>
                      <DataCell label={splitName}><span className="meta">{label}</span></DataCell>
                      <DataCell label={thenName}>{childLabel}</DataCell>
                      <DataCell label="Value">{formatValue(value, unit)}</DataCell>
                      <DataCell label="Share">{percent(value, total)}<span className="meta"> of {label}</span></DataCell>
                      <DataCell label="Action">
                        {!child.other && child.value !== null
                          ? <button type="button" className="inline-action" onClick={() => onNarrowPair(item.value!, child.value!)} aria-label={`Filter to ${label} and ${childLabel}`}>Filter</button>
                          : '—'}
                      </DataCell>
                    </TableRow>
                  ))}
              </React.Fragment>
            );
          })}
        </TableBody>
      </DataGrid>
    </>
  );
}

const eventStatus = (event: UsageEvent) =>
  event.status === 'failed' ? `Failed${event.error_category ? ` · ${errorCategoryLabel(event.error_category)}` : ''}` : statusLabels[event.status];

/** The slice's requests, a page at a time, each linking to its trace. */
function Events({ organizationId, slice, ...signIn }: { organizationId: string; slice: UsageSliceQuery } & SignIn) {
  const sliceKey = JSON.stringify(slice);
  // The tokens of the pages before this one, so Previous can go back.
  const [tokens, setTokens] = useState<(string | undefined)[]>([undefined]);
  const [page, setPage] = useState<Page<UsageEvent> | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [loading, setLoading] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => { setTokens([undefined]); }, [organizationId, sliceKey]);
  const token = tokens[tokens.length - 1];
  useEffect(() => {
    let current = true;
    setLoading(true);
    setProblem(null);
    api.usageEvents(organizationId, slice, { page_size: EVENTS_PAGE, page_token: token }).then(
      value => { if (current) setPage(value); },
      caught => { if (current) { setPage(null); setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The requests could not be loaded.')); } },
    ).finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [organizationId, sliceKey, token, attempt]); // eslint-disable-line react-hooks/exhaustive-deps

  return (
    <section className="dashboard-events" aria-labelledby="usage-events-title">
      <div className="section-header"><div>
        <h2 id="usage-events-title">Requests</h2>
        <p className="muted">The requests in this view, newest first. Open one for its trace: routing, timing and, where tracing kept it, its content.</p>
      </div></div>
      {problem?.status === 403 && <Message kind="info">Listing requests is reading traces: it needs trace access (owners, admins, operators and auditors; team admins for their teams). The figures above still cover them.</Message>}
      {problem && problem.status !== 403 && <ReportError problem={problem} what="requests" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
      {loading && !page && <Message kind="info">Loading requests…</Message>}
      {page && page.items.length === 0 && tokens.length === 1 && <EmptyState title="No requests" text="Nothing matches this window and these filters." />}
      {page && page.items.length > 0 && (
        <div className={loading ? 'refreshing' : undefined} aria-busy={loading}>
          <DataGrid aria-label="Requests in this view">
            <TableHead>
              <TableRow className="data-row head">
                <TableHeader>Time</TableHeader>
                <TableHeader>Key · account</TableHeader>
                <TableHeader>Model</TableHeader>
                <TableHeader>Tokens in / out</TableHeader>
                <TableHeader>Cost</TableHeader>
                <TableHeader>Status</TableHeader>
                <TableHeader>Duration</TableHeader>
                <TableHeader>Trace</TableHeader>
              </TableRow>
            </TableHead>
            <TableBody>
              {page.items.map(event => (
                <TableRow className="data-row" key={event.id}>
                  <DataCell label="Time" title={event.recorded_at}>{formatDate(event.recorded_at)}</DataCell>
                  <DataCell label="Key · account" title={event.api_key_id ?? undefined}>
                    {event.labels.api_key ?? (event.api_key_id ? uuidHint(event.api_key_id) : 'No key recorded')}
                    <span className="meta"> · {event.labels.service_account ?? (event.service_account_id ? uuidHint(event.service_account_id) : '—')}</span>
                    {event.batch_id && <span className="meta" title={event.batch_id}> · batch line {event.custom_id}</span>}
                  </DataCell>
                  <DataCell label="Model" className="cell-primary" title={event.deployment}>
                    {event.alias && event.alias !== event.model ? `${event.alias} → ${event.model}` : event.model}
                    {event.model_master && <span className="meta" title="Its master record when served"> · {event.model_master}</span>}
                  </DataCell>
                  <DataCell label="Tokens in / out">{formatCount(event.tokens.input)} / {formatCount(event.tokens.output)}</DataCell>
                  <DataCell label="Cost">{formatMoney(event.amount_minor, event.currency)}</DataCell>
                  <DataCell label="Status">{eventStatus(event)}</DataCell>
                  <DataCell label="Duration">{formatDuration(event.duration_ms)}</DataCell>
                  <DataCell label="Trace">
                    <ConsoleLink href={traceHref(event.id)} aria-label={`Open the trace of the request at ${formatDate(event.recorded_at)}`}>Open trace</ConsoleLink>
                    <span className="meta"> · {contentStateLabel(event.content_state)}</span>
                  </DataCell>
                </TableRow>
              ))}
            </TableBody>
          </DataGrid>
        </div>
      )}
      {(tokens.length > 1 || page?.next_page_token) && (
        <nav className="pager" aria-label="Request pages">
          <Button variant="ghost" disabled={tokens.length === 1 || loading} onClick={() => setTokens(tokens.slice(0, -1))}>Previous</Button>
          <span className="meta">Page {tokens.length}</span>
          <Button variant="ghost" disabled={!page?.next_page_token || loading} onClick={() => setTokens([...tokens, page!.next_page_token!])}>Next</Button>
        </nav>
      )}
    </section>
  );
}
