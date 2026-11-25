import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { listFilter } from './api';
import { formatMoney } from './money';
import { emptyView } from './table-view';
import { TracesScreen, traceGroupNote, traceQuery } from './traces-screen';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const trace = (id: string, model: string) => ({
  id, team_id: null, service_account_id: null, status: 'finalized', outcome: 'succeeded', model, deployment: `${model}-d`,
  provider_family: 'bedrock', location: 'us-east-1', currency: 'USD', amount_minor: 100, usage_confidence: 'exact',
  recorded_at: '2026-10-29T10:00:00Z', finalized_at: '2026-10-29T10:00:01Z', correlation_id: `c-${id}`, api_key_id: null, end_user: null,
  alias: null, operation: null, duration_ms: 1000, error_category: null, content_state: null,
});

describe('trace queries', () => {
  it('escapes commas and backslashes within listed values', () => {
    expect(listFilter(['a', 'east,us', 'c\\d'])).toBe('a,east\\,us,c\\\\d');
    expect(listFilter([])).toBeUndefined();
  });

  it('sends lists, a search and an inclusive time range', () => {
    const view = {
      ...emptyView,
      search: ' gpt ',
      categories: { model: ['a', 'b'], status: ['pending'], end_user: ['user,7'] },
      ranges: { recorded: { min: '2026-10-01T00:00:00Z', max: '2026-10-29T23:59:59.999Z' } },
    };
    expect(traceQuery(view)).toMatchObject({
      model: 'a,b',
      status: 'pending',
      end_user: 'user\\,7',
      q: 'gpt',
      from: '2026-10-01T00:00:00.000Z',
      // The API's end is exclusive.
      to: '2026-10-30T00:00:00.000Z',
    });
    expect(traceQuery(view, 'model').model).toBeUndefined();
    expect(traceQuery(view, 'recorded').from).toBeUndefined();
  });

  it('notes a group’s amounts per currency and its unpriced requests', () => {
    expect(traceGroupNote({
      values: {}, count: 3, unpriced_count: 1, first_recorded_at: '', last_recorded_at: '',
      amounts: [{ currency: 'EUR', amount_minor: 9 }, { currency: 'USD', amount_minor: 30 }],
    })).toBe(`${formatMoney(9, 'EUR')} · ${formatMoney(30, 'USD')} · 1 unpriced`);
  });
});

describe('traces table', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  const urls = () => fetchMock.mock.calls.map(([url]) => decodeURIComponent(String(url)));

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn(async (url: string) => {
      if (url.includes('/traces/summary')) {
        return json({ group_by: ['model'], total: 4, truncated: false, groups: [
          { values: { model: 'nova' }, count: 3, unpriced_count: 1, amounts: [{ currency: 'USD', amount_minor: 200 }], first_recorded_at: '2026-10-28T00:00:00Z', last_recorded_at: '2026-10-29T00:00:00Z' },
          { values: { model: 'gpt' }, count: 1, unpriced_count: 0, amounts: [{ currency: 'USD', amount_minor: 50 }], first_recorded_at: '2026-10-28T00:00:00Z', last_recorded_at: '2026-10-28T00:00:00Z' },
        ] });
      }
      if (/\/traces\/t-1$/.test(url)) return json({ ...trace('t-1', 'nova'), content_state: 'off', routing: null, provider_request_id: null, input_bound: 1, output_cap: 1, price_version: 'v1', pricing_confidence: 'known', reservation: { id: 'r', state: 'settled', reserved_minor: 100, expires_at: '2026-10-29T11:00:00Z' }, usage: null, reconciliation: null, provider_error: null });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      return json({ items: [trace('t-1', 'nova'), trace('t-2', 'gpt')], next_page_token: null });
    });
    vi.stubGlobal('fetch', fetchMock);
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
  const openTraces = async () => {
    act(() => root.render(<TracesScreen organizationId="org-1" />));
    await settle();
  };
  const traceUrls = () => urls().filter(url => url.includes('/traces'));

  it('queries the server with the view from the URL and opens a trace in place', async () => {
    history.replaceState(null, '', '/admin/traces?traces.q=gpt&traces.f.model=nova,gpt');
    await openTraces();
    expect(traceUrls()[0]).toContain('/traces?model=nova,gpt&q=gpt&page_size=25');
    expect(container.querySelectorAll('[aria-label="Traces"] > tbody > .data-row:not(.head)')).toHaveLength(2);
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label="Expand nova"]')!.click());
    await settle();
    expect(urls().some(url => url.endsWith('/traces/t-1'))).toBe(true);
    expect(container.textContent).toContain('Correlation ID');
    expect(container.textContent).toContain('Timing only');
    // The expanded trace links to its own page.
    const link = Array.from(container.querySelectorAll('a')).find(a => a.textContent === 'Open this request')!;
    expect(link.getAttribute('href')).toBe('/admin/traces/t-1');
    act(() => link.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, button: 0 })));
    await settle();
    expect(location.pathname).toBe('/admin/traces/t-1');
    expect(container.textContent).toContain('All requests');
    expect(container.querySelector('[aria-label="Traces"]')).toBeNull();
  });

  it('tells an attempt the request failed over from, and the attempt that replaced it', async () => {
    const left = { ...trace('t-1', 'nova'), outcome: 'failed-over', error_category: 'rate_limited', amount_minor: null, attempt: 1, failover_of: null };
    const next = { ...trace('t-2', 'nova'), attempt: 2, failover_of: 't-1' };
    const detail = { content_state: 'off', routing: null, provider_request_id: null, input_bound: 1, output_cap: 1, price_version: 'v1', pricing_confidence: 'known', reservation: { id: 'r', state: 'settled', reserved_minor: 100, expires_at: '2026-10-29T11:00:00Z' }, usage: null, reconciliation: null, provider_error: null };
    fetchMock.mockImplementation(async (url: string) => {
      if (/\/traces\/t-2$/.test(url)) return json({ ...next, ...detail });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      if (url.includes('/traces/summary')) return json({ group_by: [], total: 0, truncated: false, groups: [] });
      return json({ items: [left, next], next_page_token: null });
    });
    await openTraces();
    expect(container.textContent).toContain('Failed over · ');
    act(() => container.querySelectorAll<HTMLButtonElement>('button[aria-label="Expand nova"]')[1].click());
    await settle();
    expect(container.textContent).toContain('Attempt 2, after trace');
  });

  it('opens a trace page by its address and says when there is no such trace', async () => {
    fetchMock.mockImplementation(async () => new Response(JSON.stringify({ type: 'about:blank', code: 'not_found', title: 'Not found', status: 404, detail: 'no', instance: '', request_id: 'r' }), { status: 404, headers: { 'Content-Type': 'application/problem+json' } }));
    history.replaceState(null, '', '/admin/traces/t-9');
    await openTraces();
    expect(urls().some(url => url.endsWith('/traces/t-9'))).toBe(true);
    expect(container.textContent).toContain('No such trace');
  });

  it('groups on the server with amounts, reads no traces meanwhile, and drills in', async () => {
    history.replaceState(null, '', '/admin/traces?traces.g=model');
    await openTraces();
    expect(traceUrls().some(url => url.includes('/traces/summary?group_by=model'))).toBe(true);
    expect(traceUrls().some(url => url.includes('/traces?'))).toBe(false);
    expect(container.textContent).toContain(`3 rows · ${formatMoney(200, 'USD')} · 1 unpriced`);
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label="Show the rows of Model: nova"]')!.click());
    await settle();
    expect(new URLSearchParams(location.search).get('traces.f.model')).toBe('nova');
    expect(traceUrls()[traceUrls().length - 1]).toContain('/traces?model=nova');
  });

  it('cannot open a group whose value is missing, since it cannot be filtered for', async () => {
    fetchMock.mockImplementation(async (url: string) => json(url.includes('/traces/summary')
      ? { group_by: ['team'], total: 3, truncated: false, groups: [
          { values: { team: null }, count: 2, unpriced_count: 0, amounts: [], first_recorded_at: '2026-10-28T00:00:00Z', last_recorded_at: '2026-10-29T00:00:00Z' },
          { values: { team: '123e4567-e89b-12d3-a456-426614174000' }, count: 1, unpriced_count: 0, amounts: [], first_recorded_at: '2026-10-28T00:00:00Z', last_recorded_at: '2026-10-28T00:00:00Z' },
        ] }
      : { from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null }));
    history.replaceState(null, '', '/admin/traces?traces.g=team');
    await openTraces();
    const drills = container.querySelectorAll('button[aria-label^="Show the rows of Team"]');
    expect(drills).toHaveLength(1);
    expect(container.querySelectorAll('[aria-label="Traces"] .group-row')).toHaveLength(2);
  });

  it('explains a failed summary by its status and retries the grouped view', async () => {
    let fail = true;
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes('/traces/summary')) {
        if (fail) return new Response(JSON.stringify({ type: 'about:blank', code: 'internal', title: 'Error', status: 503, detail: 'down', instance: '', request_id: 'r' }), { status: 503, headers: { 'Content-Type': 'application/problem+json' } });
        return json({ group_by: ['model'], total: 1, truncated: false, groups: [{ values: { model: 'nova' }, count: 1, unpriced_count: 0, amounts: [], first_recorded_at: '2026-10-28T00:00:00Z', last_recorded_at: '2026-10-28T00:00:00Z' }] });
      }
      return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
    });
    history.replaceState(null, '', '/admin/traces?traces.g=model');
    await openTraces();
    expect(container.textContent).toContain('The trace groups could not be loaded.');
    fail = false;
    act(() => (Array.from(container.querySelectorAll('button')).find(button => button.textContent === 'Try again') as HTMLButtonElement).click());
    await settle();
    expect(container.textContent).not.toContain('could not be loaded');
    expect(container.querySelector('button[aria-label="Show the rows of Model: nova"]')).not.toBeNull();
    // Still grouped: retrying did not leave the grouped view.
    expect(new URLSearchParams(location.search).get('traces.g')).toBe('model');
  });
});

describe('traces id filters', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  const urls = () => fetchMock.mock.calls.map(([url]) => decodeURIComponent(String(url)));
  if (!Element.prototype.scrollIntoView) Element.prototype.scrollIntoView = () => {};

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn(async (url: string) => {
      if (url.includes('/usage/series')) {
        return json({ series: [
          { value: 'sa-1', label: 'ci-bot', other: false },
          { value: 'sa-2', label: 'nightly', other: false },
          { value: null, label: null, other: true },
        ] });
      }
      return json({ items: [trace('t-1', 'nova')], next_page_token: null });
    });
    vi.stubGlobal('fetch', fetchMock);
    history.replaceState(null, '', '/admin/traces?traces.f.model=nova&traces.f.status=pending');
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    history.replaceState(null, '', '/');
  });

  const settle = async () => {
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
  };

  it('picks a service account by name from the usage of the view', async () => {
    act(() => root.render(<TracesScreen organizationId="org-1" />));
    await settle();
    act(() => {
      const select = container.querySelector<HTMLSelectElement>('#traces-id-filter-dimension')!;
      select.value = 'service_account';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    const input = container.querySelector<HTMLInputElement>('#traces-id-filter-value')!;
    act(() => input.focus());
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'night');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await settle();
    const lookup = new URL(urls().find(url => url.includes('/usage/series'))!, 'http://x').searchParams;
    expect(lookup.get('split_by')).toBe('service_account');
    // The view's filters the usage API shares; trace status is not usage status.
    expect(lookup.get('model')).toBe('nova');
    expect(lookup.has('status')).toBe(false);
    expect(Date.parse(lookup.get('to')!) - Date.parse(lookup.get('from')!)).toBe(90 * 86_400_000);
    expect(Array.from(container.querySelectorAll('[role="option"]')).map(option => option.textContent)).toEqual(['nightly']);
    act(() => (container.querySelector('[role="option"]') as HTMLElement).click());
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(new URLSearchParams(location.search).get('traces.f.service_account')).toBe('sa-2');
    const traces = new URL(urls().filter(url => url.includes('/traces?')).at(-1)!, 'http://x').searchParams;
    expect(traces.get('service_account_id')).toBe('sa-2');
  });
});
