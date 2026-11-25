import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import type { UsageMeasures, UsageSeriesReport } from './api';
import { formatMoney } from './money';
import { StackedColumns, assignSlots, changeOf, niceTicks } from './usage-chart';
import {
  UsageDashboard, chartSeries, dashboardWindow, defaultGranularity, metricValue, readDashboard, sliceQuery, valueLabel, writeDashboard,
  type DashboardState,
} from './usage-dashboard';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': status < 400 ? 'application/json' : 'application/problem+json' } });
const problem = (status: number) => json({ type: 'about:blank', code: 'x', title: 'x', status, detail: 'no', instance: '', request_id: 'r' }, status);

const measures = (requests: number, extra: Partial<UsageMeasures> = {}): UsageMeasures => ({
  requests, failed_requests: 0, pending_requests: 0, expired_requests: 0, unknown_requests: 0, tokens: { input: requests * 100, output: requests * 10, cached_input: 0, reasoning: 0, other: 0 },
  amounts: [{ currency: 'USD', amount_minor: requests * 1_000_000 }], unpriced_requests: 0, no_usage_requests: 0, incomplete_requests: 0,
  duration: { average_ms: 900, p50_ms: 800, p95_ms: 2400 }, ...extra,
});
const KEY = '123e4567-e89b-12d3-a456-426614174000';
const report: UsageSeriesReport = {
  from: '2026-11-06T00:00:00Z', to: '2026-11-08T00:00:00Z', granularity: 'day', split_by: 'model', rank_by: 'requests',
  buckets: ['2026-11-06T00:00:00Z', '2026-11-07T00:00:00Z'],
  totals: measures(6, { failed_requests: 1 }),
  previous: { from: '2026-11-04T00:00:00Z', to: '2026-11-06T00:00:00Z', totals: measures(3) },
  series: [
    { value: 'nova', label: null, other: false, totals: measures(4), points: [{ bucket: '2026-11-07T00:00:00Z', ...measures(4) }] },
    { value: 'gpt', label: null, other: false, totals: measures(1), points: [{ bucket: '2026-11-06T00:00:00Z', ...measures(1) }] },
    { value: null, label: null, other: true, totals: measures(1), points: [{ bucket: '2026-11-06T00:00:00Z', ...measures(1) }] },
  ],
  truncated: true,
};
const event = (id: string) => ({
  id, correlation_id: `c-${id}`, recorded_at: '2026-11-07T10:00:00Z', finalized_at: '2026-11-07T10:00:01Z', duration_ms: 1200,
  team_id: null, service_account_id: 'sa-1', api_key_id: KEY, user_id: null,
  labels: { team: null, service_account: 'ci-bot', api_key: 'deploy key', user: null },
  model: 'nova', alias: 'fast', deployment: 'nova-d', provider_family: 'bedrock', location: 'us-east-1', operation: 'chat',
  status: 'succeeded', outcome: 'succeeded', error_category: null,
  tokens: { input: 100, output: 10, cached_input: 0, reasoning: 0, other: 0 }, currency: 'USD', amount_minor: 1_000_000,
  usage_confidence: 'exact', content_state: 'stored', model_master: 'amazon/nova-pro',
});

describe('dashboard state', () => {
  it('round-trips the address, escaping commas in listed values', () => {
    const state: DashboardState = {
      range: 'custom', from: '2026-11-01', to: '2026-11-07', granularity: 'hour', split: 'api_key', metric: 'cost', currency: 'EUR',
      filters: { model: ['nova', 'east,us'], api_key: [KEY] },
    };
    const search = writeDashboard('?other=1', state);
    expect(new URLSearchParams(search).get('other')).toBe('1');
    expect(new URLSearchParams(search).get('u.f.model')).toBe('nova,east\\,us');
    expect(readDashboard(search)).toEqual(state);
  });

  it('defaults to the last 7 days of requests by model and keeps defaults out of the address', () => {
    expect(readDashboard('')).toEqual({ range: '7d', from: undefined, to: undefined, granularity: undefined, split: 'model', metric: 'requests', currency: undefined, filters: {} });
    expect(writeDashboard('', readDashboard(''))).toBe('');
    expect(readDashboard('?u.split=none&u.range=bogus').split).toBeNull();
    expect(readDashboard('?u.range=bogus').range).toBe('7d');
  });

  it('starts a preset on a whole bucket and ends a custom range after its last day', () => {
    const now = Date.parse('2026-11-13T15:30:00Z');
    expect(dashboardWindow({ ...readDashboard(''), range: '24h' }, now)).toEqual({ from: '2026-11-12T15:00:00.000Z', to: '2026-11-13T15:30:00.000Z', granularity: 'hour' });
    expect(dashboardWindow(readDashboard(''), now)).toEqual({ from: '2026-11-06T00:00:00.000Z', to: '2026-11-13T15:30:00.000Z', granularity: 'day' });
    // Weeks start on Monday: 2026-08-15 is a Saturday.
    expect(dashboardWindow({ ...readDashboard(''), range: '90d', granularity: 'week' }, now)!.from).toBe('2026-08-10T00:00:00.000Z');
    const custom = { ...readDashboard(''), range: 'custom' as const, from: '2026-11-01', to: '2026-11-01' };
    expect(dashboardWindow(custom, now)).toEqual({ from: '2026-11-01T00:00:00.000Z', to: '2026-11-02T00:00:00.000Z', granularity: 'hour' });
    // Without an end it runs to now; without either bound there is none.
    expect(dashboardWindow({ ...custom, to: undefined }, now)?.to).toBe(new Date(now).toISOString());
    expect(dashboardWindow({ ...custom, from: undefined, to: undefined }, now)).toBeNull();
    expect(dashboardWindow({ ...custom, to: '2026-10-01' }, now)).toBeNull();
    expect(defaultGranularity(200 * 86_400_000)).toBe('week');
  });

  it('keeps a second grouping only under a different split', () => {
    const state: DashboardState = { ...readDashboard(''), split: 'team', then: 'model' };
    expect(new URLSearchParams(writeDashboard('', state)).get('u.then')).toBe('model');
    expect(readDashboard(writeDashboard('', state))).toEqual(state);
    expect(readDashboard('?u.split=team&u.then=team').then).toBeUndefined();
    expect(readDashboard('?u.split=none&u.then=model').then).toBeUndefined();
    expect(readDashboard('?u.then=bogus').then).toBeUndefined();
  });

  it('opens a former Breakdown view as the dashboard’s, and drops it once changed', () => {
    const legacy = '?usage.g=team,model&usage.f.provider_family=bedrock&usage.r.day=2026-11-01T00:00..2026-11-07T00:00';
    const state = readDashboard(legacy);
    expect(state).toEqual({ range: 'custom', from: '2026-11-01T00:00', to: '2026-11-07T00:00', split: 'team', then: 'model', metric: 'requests', filters: { provider_family: ['bedrock'] } });
    // The same requests as Breakdown showed: its bounds kept to the minute,
    // in UTC, the end inclusive.
    expect(dashboardWindow(state, Date.parse('2026-11-13T15:30:00Z'))).toMatchObject({ from: '2026-11-01T00:00:00.000Z', to: '2026-11-07T00:00:00.001Z' });
    // An open end stays open: now, or 30 days before the end.
    const startOnly = readDashboard('?usage.r.day=2026-11-01T12:30..');
    expect(dashboardWindow(startOnly, Date.parse('2026-11-13T15:30:00Z'))).toMatchObject({ from: '2026-11-01T12:30:00.000Z', to: '2026-11-13T15:30:00.000Z' });
    const endOnly = readDashboard('?usage.r.day=..2026-11-07T00:00');
    expect(dashboardWindow(endOnly, Date.parse('2026-11-13T15:30:00Z'))?.from).toBe(new Date(Date.parse('2026-11-07T00:00:00.001Z') - 30 * 86_400_000).toISOString());
    expect(readDashboard('?usage.g=day').split).toBeNull();
    expect(readDashboard('?usage.f.model=nova').range).toBe('30d');
    // Its own parameters win, and writing the view drops the old ones.
    expect(readDashboard(`${legacy}&u.split=alias`).split).toBe('alias');
    expect(writeDashboard(legacy, state)).not.toContain('usage.');
  });

  it('filters by each dimension’s parameter, and by currency only for spend', () => {
    const state: DashboardState = { ...readDashboard(''), filters: { api_key: [KEY], user: ['u-1', 'u-2'], end_user: ['alice'], status: ['failed'] }, currency: 'USD' };
    const window = { from: 'a', to: 'b' };
    expect(sliceQuery(state, window)).toEqual({ from: 'a', to: 'b', api_key_id: KEY, user_id: 'u-1,u-2', end_user: 'alice', status: 'failed' });
    expect(sliceQuery({ ...state, metric: 'cost' }, window).currency).toBe('USD');
    // A batch is a filter, not a split: its lines, from the address.
    const batch = readDashboard('?u.f.batch=batch_1');
    expect(batch.filters).toEqual({ batch: ['batch_1'] });
    expect(sliceQuery(batch, window)).toEqual({ from: 'a', to: 'b', batch_id: 'batch_1' });
    // A master record is a dimension: split by it, or filter to some.
    const master = readDashboard('?u.split=model_master&u.f.model_master=anthropic%2Fclaude-sonnet-4-6,org%2Fqwen');
    expect(master.split).toBe('model_master');
    expect(master.filters).toEqual({ model_master: ['anthropic/claude-sonnet-4-6', 'org/qwen'] });
    expect(sliceQuery(master, window)).toEqual({ from: 'a', to: 'b', model_master: 'anthropic/claude-sonnet-4-6,org/qwen' });
  });

  it('places each series’ points in their buckets and reads the chosen metric', () => {
    const series = chartSeries(report, 'requests');
    expect(series.map(item => [item.label, item.values])).toEqual([['nova', [0, 4]], ['gpt', [1, 0]], ['Other', [1, 0]]]);
    expect(metricValue(measures(2), 'tokens')).toBe(220);
    expect(metricValue(measures(2), 'cost', 'USD')).toBe(2_000_000);
    expect(metricValue(measures(2), 'cost', 'EUR')).toBe(0);
  });

  it('names missing values and statuses', () => {
    expect(valueLabel('api_key', null)).toBe('No key recorded');
    expect(valueLabel('end_user', null)).toBe('No end user');
    expect(valueLabel('end_user', 'alice')).toBe('alice');
    expect(valueLabel('team', KEY)).toBe('123e4567…4000');
    expect(valueLabel('team', KEY, 'Platform')).toBe('Platform');
    expect(valueLabel('status', 'failed')).toBe('Failed');
    expect(valueLabel('status', 'failed-over')).toBe('Failed over');
    expect(valueLabel('status', 'expired')).toBe('Expired before the call');
    expect(valueLabel(null, null)).toBe('All requests');
    expect(valueLabel('model_master', null)).toBe('No master record');
    expect(valueLabel('model_master', 'org/qwen')).toBe('org/qwen');
  });
});

describe('chart helpers', () => {
  it('keeps a value’s color while it stays and gives a newcomer a free slot', () => {
    const slots = assignSlots(new Map(), ['a', 'b', 'c']);
    expect([...slots]).toEqual([['a', 0], ['b', 1], ['c', 2]]);
    assignSlots(slots, ['c', 'd']);
    expect(slots.get('c')).toBe(2);
    expect(slots.get('d')).toBe(0);
  });

  it('never gives two series on the chart one color when a key returns', () => {
    const slots = assignSlots(new Map(), ['a', 'b']);
    assignSlots(slots, ['b', 'c']);
    assignSlots(slots, ['a', 'c']);
    expect(slots.get('a')).not.toBe(slots.get('c'));
  });

  it('rounds axis ticks to 1, 2 or 5 steps that cover the maximum', () => {
    expect(niceTicks(0)).toEqual([0, 1]);
    expect(niceTicks(7)).toEqual([0, 2, 4, 6, 8]);
    expect(niceTicks(1000)).toEqual([0, 500, 1000]);
    const ticks = niceTicks(1234);
    expect(ticks[0]).toBe(0);
    expect(ticks[ticks.length - 1]).toBeGreaterThanOrEqual(1234);
  });

  it('compares with the previous window only when it had something', () => {
    expect(changeOf(6, 3)).toEqual({ direction: 'up', text: `+${(100).toLocaleString()}%` });
    expect(changeOf(3, 6)!.direction).toBe('down');
    expect(changeOf(3, 3)).toEqual({ direction: 'flat', text: 'No change' });
    expect(changeOf(3, 0)).toBeNull();
    expect(changeOf(null, 3)).toBeNull();
  });
});

describe('usage dashboard', () => {
  // The combo box scrolls its highlighted value into view; jsdom has no layout.
  if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  const urls = () => fetchMock.mock.calls.map(([url]) => decodeURIComponent(String(url)));
  const seriesUrls = () => urls().filter(url => url.includes('/usage/series'));
  const eventUrls = () => urls().filter(url => url.includes('/usage/events'));

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn(async (url: string) => {
      if (url.includes('/usage/series')) return json(report);
      if (url.includes('page_token=p2')) return json({ items: [event('t-3')], next_page_token: null });
      return json({ items: [event('t-1'), event('t-2')], next_page_token: 'p2' });
    });
    vi.stubGlobal('fetch', fetchMock);
    history.replaceState(null, '', '/admin/usage');
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    history.replaceState(null, '', '/');
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const render = async () => {
    act(() => root.render(<UsageDashboard organizationId="org-1" />));
    await settle();
  };
  const button = (text: string) => Array.from(container.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement;

  it('charts the slice, compares it with the previous window and lists its requests with trace links', async () => {
    await render();
    const series = new URL(seriesUrls()[0], 'http://x').searchParams;
    expect(series.get('granularity')).toBe('day');
    expect(series.get('split_by')).toBe('model');
    expect(series.get('limit')).toBe('7');
    expect(series.get('rank_by')).toBe('requests');
    // The requests cover the same window as the chart.
    const events = new URL(eventUrls()[0], 'http://x').searchParams;
    expect(events.get('from')).toBe(series.get('from'));
    expect(events.get('to')).toBe(series.get('to'));
    expect(events.get('page_size')).toBe('25');

    expect(container.textContent).toContain('Requests6');
    expect(container.textContent).toContain(`+${(100).toLocaleString()}% vs the previous window`);
    expect(container.textContent).toContain(formatMoney(6_000_000, 'USD'));
    // Two buckets, each a focusable column naming its values.
    const columns = container.querySelectorAll('.chart-column');
    expect(columns).toHaveLength(2);
    expect(columns[1].getAttribute('aria-label')).toContain('4');
    expect(container.querySelector('[aria-label="Legend"]')!.textContent).toContain('Other');
    expect(container.querySelector('.chart-table table')).not.toBeNull();

    const rows = container.querySelectorAll('[aria-label="Requests in this view"] > tbody > .data-row');
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain('deploy key');
    expect(rows[0].textContent).toContain('fast → nova');
    expect(rows[0].textContent).toContain('amazon/nova-pro');
    expect(rows[0].textContent).toContain('Content kept');
    const link = rows[0].querySelector('a')!;
    expect(link.getAttribute('href')).toBe('/admin/traces/t-1');
    act(() => link.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })));
    expect(location.pathname).toBe('/admin/traces/t-1');
  });

  it('pages through the requests and back', async () => {
    await render();
    act(() => button('Next').click());
    await settle();
    expect(eventUrls()[eventUrls().length - 1]).toContain('page_token=p2');
    expect(container.querySelectorAll('[aria-label="Requests in this view"] > tbody > .data-row')).toHaveLength(1);
    expect(container.textContent).toContain('Page 2');
    act(() => button('Previous').click());
    await settle();
    expect(eventUrls()[eventUrls().length - 1]).not.toContain('page_token');
    expect(container.textContent).toContain('Page 1');
  });

  it('narrows to a breakdown value, keeps it in the address and removes it again', async () => {
    await render();
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label="Filter to nova"]')!.click());
    await settle();
    expect(new URLSearchParams(location.search).get('u.f.model')).toBe('nova');
    expect(new URL(seriesUrls()[seriesUrls().length - 1], 'http://x').searchParams.get('model')).toBe('nova');
    expect(new URL(eventUrls()[eventUrls().length - 1], 'http://x').searchParams.get('model')).toBe('nova');
    // The folded tail and missing values cannot be filtered to.
    expect(container.querySelectorAll('.ranked-bar button')).toHaveLength(2);
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label="Remove Model: nova"]')!.click());
    await settle();
    expect(new URLSearchParams(location.search).has('u.f.model')).toBe(false);
  });

  it('narrows to only the clicked value when its dimension already lists several', async () => {
    history.replaceState(null, '', '/admin/usage?u.f.model=nova,gpt');
    await render();
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label="Filter to nova"]')!.click());
    await settle();
    expect(new URLSearchParams(location.search).get('u.f.model')).toBe('nova');
  });

  it('reads its view from the address and adds a typed filter', async () => {
    history.replaceState(null, '', `/admin/usage?u.split=api_key&u.metric=cost&u.cur=USD&u.f.team=${KEY}`);
    await render();
    const series = new URL(seriesUrls()[0], 'http://x').searchParams;
    expect(series.get('split_by')).toBe('api_key');
    expect(series.get('rank_by')).toBe('cost');
    expect(series.get('currency')).toBe('USD');
    expect(series.get('team_id')).toBe(KEY);
    expect(container.textContent).toContain('Team: 123e4567…4000');
    const value = container.querySelector<HTMLInputElement>('#usage-filter-value')!;
    act(() => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      setter.call(value, KEY);
      value.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => (value.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(new URL(seriesUrls()[seriesUrls().length - 1], 'http://x').searchParams.get('api_key_id')).toBe(KEY);
  });

  it('explains that listing requests needs trace access while the figures still show', async () => {
    fetchMock.mockImplementation(async (url: string) => (url.includes('/usage/series') ? json(report) : problem(403)));
    await render();
    expect(container.textContent).toContain('it needs trace access');
    expect(container.querySelectorAll('.chart-column')).toHaveLength(2);
  });

  it('keeps every currency on offer once spend filters to one', async () => {
    history.replaceState(null, '', '/admin/usage?u.metric=cost&u.cur=USD');
    const both = { ...report, totals: { ...report.totals, amounts: [{ currency: 'EUR', amount_minor: 5 }, { currency: 'USD', amount_minor: 9 }] } };
    const onlyUsd = { ...report, totals: { ...report.totals, amounts: [{ currency: 'USD', amount_minor: 9 }] } };
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes('/usage/series')) return json(url.includes('currency=') ? onlyUsd : both);
      return json({ items: [], next_page_token: null });
    });
    // A report without the currency filter showed both.
    history.replaceState(null, '', '/admin/usage');
    await render();
    act(() => {
      const select = container.querySelector<HTMLSelectElement>('#usage-metric')!;
      select.value = 'cost';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    const options = () => Array.from(container.querySelectorAll<HTMLOptionElement>('#usage-currency option')).map(option => option.value);
    expect(seriesUrls()[seriesUrls().length - 1]).toContain('currency=');
    expect(options()).toEqual(['EUR', 'USD']);
  });

  it('shows no breakdown of another split while the new one loads, and retries a failure', async () => {
    await render();
    let release: (value: Response) => void = () => {};
    fetchMock.mockImplementation((url: string) => (url.includes('/usage/series')
      ? new Promise<Response>(resolve => { release = resolve; })
      : Promise.resolve(json({ items: [], next_page_token: null }))));
    act(() => {
      const select = container.querySelector<HTMLSelectElement>('#usage-split')!;
      select.value = 'team';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    expect(container.querySelector('button[aria-label="Filter to nova"]')).toBeNull();
    expect(container.textContent).toContain('Loading usage');
    act(() => release(problem(503)));
    await settle();
    expect(container.querySelector('.ranked-bars')).toBeNull();
    const before = seriesUrls().length;
    fetchMock.mockImplementation(async (url: string) => json(url.includes('/usage/series') ? { ...report, split_by: 'team' } : { items: [], next_page_token: null }));
    act(() => button('Try again').click());
    await settle();
    expect(seriesUrls().length).toBe(before + 1);
    expect(container.querySelector('.ranked-bars')).not.toBeNull();
  });

  it('says which measure picks the top values when it is not the one shown, and orders by the one shown', async () => {
    const failing = { ...report, series: [
      { ...report.series[0], totals: measures(4, { failed_requests: 0 }) },
      { ...report.series[1], totals: measures(1, { failed_requests: 1 }) },
      report.series[2],
    ] };
    fetchMock.mockImplementation(async (url: string) => json(url.includes('/usage/series') ? failing : { items: [], next_page_token: null }));
    history.replaceState(null, '', '/admin/usage?u.metric=failed');
    await render();
    expect(container.textContent).toContain('the top 7 by requests');
    const names = Array.from(container.querySelectorAll('.ranked-name')).map(node => node.textContent);
    expect(names).toEqual(['gpt', 'nova', 'Other']);
  });

  it('drops a hovered column that a shorter report no longer has', async () => {
    const chart = (count: number) => (
      <StackedColumns label="c" buckets={Array.from({ length: count }, (_, i) => `2026-11-0${i + 1}T00:00:00Z`)} bucketLabel={b => b}
        series={[{ key: 'a', label: 'a', other: false, values: Array.from({ length: count }, () => 1) }]} colors={new Map([['a', 0]])} unit={{ kind: 'count' }} />
    );
    act(() => root.render(chart(8)));
    act(() => (container.querySelectorAll('.chart-column')[7] as SVGGElement).dispatchEvent(new FocusEvent('focusin', { bubbles: true })));
    expect(container.querySelector('.chart-tooltip')).not.toBeNull();
    act(() => root.render(chart(2)));
    expect(container.querySelectorAll('.chart-column')).toHaveLength(2);
    expect(container.querySelector('.chart-tooltip')).toBeNull();
  });

  it('splits each value again by a second dimension and narrows to a pair', async () => {
    const TEAM = '00000000-0000-0000-0000-00000000000a';
    const byTeam = (model: string): UsageSeriesReport => ({
      ...report, split_by: 'team', truncated: false,
      series: [
        { value: TEAM, label: 'Platform', other: false, totals: measures(model === 'nova' ? 3 : 1), points: [] },
        { value: null, label: null, other: false, totals: measures(model === 'nova' ? 1 : 0), points: [] },
      ],
    });
    fetchMock.mockImplementation(async (url: string) => {
      const params = new URL(url, 'http://x').searchParams;
      if (url.includes('/usage/series')) return json(params.get('split_by') === 'team' ? byTeam(params.get('model')!) : report);
      return json({ items: [], next_page_token: null });
    });
    history.replaceState(null, '', '/admin/usage?u.then=team');
    await render();
    expect(container.querySelector<HTMLSelectElement>('#usage-then')!.value).toBe('team');
    // One lookup per value that can be filtered to: not the folded tail.
    const second = seriesUrls().map(url => new URL(url, 'http://x').searchParams).filter(params => params.get('split_by') === 'team');
    expect(second.map(params => params.get('model')).sort()).toEqual(['gpt', 'nova']);
    expect(second[0].get('limit')).toBe('7');
    const table = container.querySelector('[aria-label="By model, then by team"]')!;
    expect(container.textContent).toContain('By model, then by team');
    const rows = Array.from(table.querySelectorAll('tbody tr')).map(row => row.textContent);
    expect(rows[0]).toContain('nova');
    expect(rows[1]).toContain('Platform');
    expect(rows[1]).toContain('75%');
    expect(rows[2]).toContain('No team');
    expect(rows.at(-1)).toContain('Not split further');
    // A missing value cannot be filtered to.
    expect(table.querySelector('button[aria-label="Filter to nova and No team"]')).toBeNull();
    act(() => table.querySelector<HTMLButtonElement>('button[aria-label="Filter to nova and Platform"]')!.click());
    await settle();
    expect(new URLSearchParams(location.search).get('u.f.model')).toBe('nova');
    expect(new URLSearchParams(location.search).get('u.f.team')).toBe(TEAM);
    expect(new URL(eventUrls().at(-1)!, 'http://x').searchParams.get('team_id')).toBe(TEAM);
    // Choosing the second dimension as the split drops the second grouping.
    act(() => {
      const select = container.querySelector<HTMLSelectElement>('#usage-split')!;
      select.value = 'team';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    expect(new URLSearchParams(location.search).has('u.then')).toBe(false);
  });

  it('offers the slice’s values of a filter’s dimension to pick from as you type', async () => {
    const keys: UsageSeriesReport = {
      ...report, split_by: 'api_key', truncated: false,
      series: [
        { value: KEY, label: 'deploy key', other: false, totals: measures(4), points: [] },
        { value: 'k-2', label: 'staging key', other: false, totals: measures(1), points: [] },
        { value: null, label: null, other: false, totals: measures(1), points: [] },
      ],
    };
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes('/usage/series')) return json(url.includes('split_by=api_key') ? keys : report);
      return json({ items: [], next_page_token: null });
    });
    history.replaceState(null, '', `/admin/usage?u.f.api_key=${KEY}`);
    await render();
    // Nothing is looked up until the box is used.
    expect(seriesUrls().filter(url => url.includes('split_by=api_key'))).toHaveLength(0);
    const input = container.querySelector<HTMLInputElement>('#usage-filter-value')!;
    expect(input.getAttribute('role')).toBe('combobox');
    act(() => input.focus());
    await settle();
    const lookup = new URL(seriesUrls().find(url => url.includes('split_by=api_key'))!, 'http://x').searchParams;
    expect(lookup.get('limit')).toBe('20');
    // Its own filter is left out, so the other keys are offered too.
    expect(lookup.has('api_key_id')).toBe(false);
    const type = (text: string) => act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, text);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    type('stag');
    await settle();
    const options = () => Array.from(container.querySelectorAll('[role="option"]')).map(option => option.textContent);
    expect(options()).toEqual(['staging key']);
    act(() => (container.querySelector('[role="option"]') as HTMLElement).click());
    await settle();
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(new URLSearchParams(location.search).get('u.f.api_key')).toBe(`${KEY},k-2`);
    // The chip names the key it was picked by.
    expect(container.textContent).toContain('API key: staging key');
  });

  it('filters by the value picked, when two values share a name', async () => {
    const keys: UsageSeriesReport = {
      ...report, split_by: 'api_key', truncated: false,
      series: [
        { value: 'k-1', label: 'same name', other: false, totals: measures(4), points: [] },
        { value: 'k-2', label: 'same name', other: false, totals: measures(1), points: [] },
      ],
    };
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes('/usage/series')) return json(url.includes('split_by=api_key') ? keys : report);
      return json({ items: [], next_page_token: null });
    });
    history.replaceState(null, '', '/admin/usage');
    await render();
    const input = container.querySelector<HTMLInputElement>('#usage-filter-value')!;
    act(() => input.focus());
    await settle();
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'same');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await settle();
    const second = container.querySelectorAll('[role="option"]')[1] as HTMLElement;
    act(() => second.click());
    await settle();
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(new URLSearchParams(location.search).get('u.f.api_key')).toBe('k-2');
  });

  it('offers the known statuses without looking them up', async () => {
    await render();
    act(() => {
      const select = container.querySelector<HTMLSelectElement>('#usage-filter-dimension')!;
      select.value = 'status';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    const before = seriesUrls().length;
    const input = container.querySelector<HTMLInputElement>('#usage-filter-value')!;
    act(() => input.focus());
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'fail');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await settle();
    expect(seriesUrls().length).toBe(before);
    expect(Array.from(container.querySelectorAll('[role="option"]')).map(option => option.textContent)).toEqual(['Failed', 'Failed over']);
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    // A typed text that names no listed value is taken as it is.
    expect(new URLSearchParams(location.search).get('u.f.status')).toBe('fail');
  });

  it('explains an invalid view instead of failing quietly', async () => {
    fetchMock.mockImplementation(async (url: string) => (url.includes('/usage/series') ? problem(400) : json({ items: [], next_page_token: null })));
    await render();
    expect(container.textContent).toContain('This view is not valid');
  });
});
