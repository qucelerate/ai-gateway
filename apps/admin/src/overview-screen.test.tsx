import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import type { Confidence } from './api';
import { OverviewScreen, exampleRequest, greeting, spendByCurrency, usableKey } from './overview-screen';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const total = (currency: string, confidence: Confidence, amount: number, requests: number) => ({
  currency, confidence, amount_minor: amount, request_count: requests, unpriced_request_count: 0, incomplete_request_count: 0,
  tokens: { input: 0, output: 0, cached_input: 0, reasoning: 0, other: 0 },
});

// The catalog the price-changes test adopts a published price into.
const RELEASE = 'rel';
const capability = { max_input_bytes: 4096, max_output_tokens: 32, max_tokens_per_byte: 1, tokenizer: 'configured', reservation_ttl_seconds: 17, chat_input_count_mode: 'unsupported' };
const manifestPrice = { input_minor_per_token: 250, output_minor_per_token: 1000, cached_input_minor_per_token: 0, reasoning_minor_per_token: 0, other_minor_per_token: 0, currency: 'USD', source: 'list', source_version: '2026-10', confidence: 'known' };
const manifest = {
  release: { version: '2026.10.2' },
  providers: [{ adapter_instance_key: 'openai-main', family: 'openai' }, { adapter_instance_key: 'bedrock-us', family: 'bedrock' }],
  deployments: [
    { name: 'gpt-5-prod', model: 'gpt-5', location: null, adapter_instance_key: 'openai-main', capabilities: [{ operation: 'chat', ...capability }, { operation: 'chat_stream', ...capability }] },
    { name: 'claude-us', model: 'claude', location: 'us-east-1', adapter_instance_key: 'bedrock-us', capabilities: [{ operation: 'chat', ...capability }] },
  ],
  prices: [
    { deployment: 'claude-us', operation: 'chat', ...manifestPrice },
    { deployment: 'gpt-5-prod', operation: 'chat', ...manifestPrice },
    { deployment: 'gpt-5-prod', operation: 'chat_stream', ...manifestPrice, input_minor_per_token: 300, output_minor_per_token: 1200 },
  ],
  routes: [
    { alias: 'chat-default', operation: 'chat', deployment: 'gpt-5-prod' },
    { alias: 'chat-default', operation: 'chat_stream', deployment: 'gpt-5-prod' },
    { alias: 'claude', operation: 'chat', deployment: 'claude-us' },
  ],
};
const drift = {
  release_id: RELEASE,
  sources: [],
  prices: [{
    deployment: 'gpt-5-prod', operation: 'chat', model: 'gpt-5', location: null, provider_family: 'openai', currency: 'USD', price_source: 'release',
    comparisons: [{ source: 'price-sheet:openai', snapshot_id: 's', status: 'differs', matched_by: 'model', observed_item: 'gpt-5', observed_region: null, observed_currencies: ['USD'], per_tokens: 1000000, dimensions: [
      { dimension: 'input', catalog: 250, observed: '125', published: '1.25', status: 'differs' },
      { dimension: 'output', catalog: 1000, observed: '1000', published: '10', status: 'matches' },
    ] }],
  }],
};

describe('overview helpers', () => {
  it('greets by first name and time of day', () => {
    expect(greeting('Ada Lovelace', new Date(2026, 9, 7, 9))).toBe('Good morning, Ada.');
    expect(greeting(null, new Date(2026, 9, 7, 14))).toBe('Good afternoon.');
    expect(greeting('  ', new Date(2026, 9, 7, 20))).toBe('Good evening.');
  });

  it('sums spend per currency across confidences, flagging estimates', () => {
    expect(spendByCurrency([total('USD', 'exact', 100, 2), total('USD', 'estimated', 50, 1), total('EUR', 'exact', 10, 1)])).toEqual([
      { currency: 'USD', amount: 150, requests: 3, estimated: true },
      { currency: 'EUR', amount: 10, requests: 1, estimated: false },
    ]);
  });
});

describe('overview screen', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  it('summarizes spend, open items, the catalog and recent activity, and links onward', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.endsWith('/me')) return json({ user_id: 'u', display_name: 'Ada Lovelace', email: null, photo_url: null });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [total('USD', 'exact', 1_250_000_000, 42)], items: [], next_page_token: 'x' });
      if (url.includes('/reconciliation-items')) return json({ items: [{ id: 'r' }, { id: 's' }], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [{ id: 'rel', version: '2026.10.2', active: true }], next_page_token: null });
      if (url.includes('/provider-accounts')) return json({ release_id: 'rel', items: [{ id: 'a', deployment_count: 3 }, { id: 'b', deployment_count: 2 }], next_page_token: null });
      if (url.includes('/audit-events')) return json({ items: [{ id: 'e', occurred_at: new Date().toISOString(), action: 'team.created', outcome: 'succeeded' }], next_page_token: null });
      // The audit screen reads its own collection; forbidden here to show the per-card message.
      return json({ code: 'forbidden', status: 403 }, 403);
    }));
    const onNavigate = vi.fn();
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={onNavigate} />));
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });

    expect(container.textContent).toMatch(/Good (morning|afternoon|evening), Ada\./);
    expect(container.textContent).toContain('$12.50');
    expect(container.textContent).toContain('42 requests');
    expect(container.textContent).toContain('Open reconciliation items');
    expect(container.textContent).toContain('5 deployments');
    expect(container.textContent).toContain('Release 2026.10.2');
    expect(container.textContent).toContain('Team created');
    act(() => (Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Models →') as HTMLButtonElement).click());
    expect(onNavigate).toHaveBeenCalledWith('models');
  });

  it('reads the whole catalog, naming the release the accounts came from', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.includes('/provider-accounts')) {
        if (url.includes('page_token') && !url.includes('release_id=rel-b')) return json({ code: 'page_token_query_mismatch', status: 400 }, 400);
        return url.includes('page_token')
          ? json({ release_id: 'rel-b', items: [{ id: 'c', deployment_count: 4 }], next_page_token: null })
          : json({ release_id: 'rel-b', items: [{ id: 'a', deployment_count: 1 }, { id: 'b', deployment_count: 2 }], next_page_token: 'p2' });
      }
      if (url.includes('/releases')) {
        // The list's own active flag is ignored: the accounts say which release they are.
        return url.includes('page_token')
          ? json({ items: [{ id: 'rel-b', version: '2026.09.1', active: false }], next_page_token: null })
          : json({ items: [{ id: 'rel-a', version: '2026.10.2', active: true }], next_page_token: 'r2' });
      }
      if (url.includes('/usage')) {
        return json({ from: '', to: '', group_by: 'day', items: [], next_page_token: null, totals: [
          total('USD', 'exact', 1_000_000_000, 10),
          { ...total('EUR', 'exact', 500_000_000, 3), unpriced_request_count: 1 },
        ] });
      }
      return json({ items: [], next_page_token: null });
    }));
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    expect(container.textContent).toContain('7 deployments');
    expect(container.textContent).toContain('Release 2026.09.1 · 3 provider accounts');
    // Each currency carries its own count and warning.
    const spend = Array.from(container.querySelectorAll('.spend-list li')).map(node => node.textContent);
    expect(spend[0]).toContain('10 requests');
    expect(spend[0]).not.toContain('includes');
    expect(spend[1]).toContain('3 requests · includes estimated, unpriced or incomplete usage');
  });

  it('retries a card that failed', async () => {
    let failing = true;
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.includes('/reconciliation-items')) {
        return failing ? json({ type: '', code: 'service_unavailable', title: '', status: 503, detail: '', instance: '', request_id: '' }, 503) : json({ items: [], next_page_token: null });
      }
      if (url.includes('/budgets')) return json({ items: [{ id: 'b', scope: 'organization' }], next_page_token: null });
      return json({ items: [], next_page_token: null, release_id: null, totals: [] });
    }));
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
    expect(container.textContent).toContain('Unavailable right now.');
    failing = false;
    act(() => (Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Try again') as HTMLButtonElement).click());
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
    expect(container.textContent).toContain('Every request settled automatically.');
  });

  it('explains a card the role cannot see without hiding the others', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.includes('/usage')) return json({ type: '', code: 'forbidden', title: '', status: 403, detail: '', instance: '', request_id: '' }, 403);
      return json({ items: [], next_page_token: null, release_id: null });
    }));
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
    expect(container.textContent).toContain('Your role cannot see this.');
    expect(container.textContent).toContain('No models yet');
  });

  it('lists the setup steps left, and says that no budget refuses every request', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.includes('/provider-accounts')) return json({ release_id: 'rel', items: [{ id: 'a', deployment_count: 1 }], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [{ id: 'rel', version: '1', active: true }], next_page_token: null });
      if (url.includes('/keys')) return json({ items: [{ id: 'k', status: 'revoked' }, { id: 'l', status: 'active' }], next_page_token: null });
      if (url.includes('/service-accounts')) return json({ items: [{ id: 'off', enabled: false }, { id: 'sa', enabled: true }], next_page_token: null });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    }));
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    const steps = Array.from(container.querySelectorAll('.setup-step')).map(node => [node.querySelector('strong')!.textContent, node.className.split(' ')[1]]);
    expect(steps).toEqual([
      ['Connect a provider and add a model', 'done'],
      ['Add a budget', 'todo'],
      ['Create an API key', 'done'],
      ['Send a first request', 'todo'],
    ]);
    expect(container.textContent).toContain('2 of 4 done');
    expect(container.querySelector('.setup-step.todo a')!.getAttribute('href')).toBe('/admin/policies/budgets');
    expect(container.textContent).toContain('No budget is set, so every request is refused.');
  });

  it('does not count a key of a disabled account as usable', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.includes('/keys')) return json({ items: [{ id: 'l', status: 'active', expires_at: null, last_used_at: null }], next_page_token: null });
      if (url.includes('/service-accounts')) return json({ items: [{ id: 'off', enabled: false }], next_page_token: null });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      return json({ items: [], next_page_token: null, release_id: null });
    }));
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    const key = Array.from(container.querySelectorAll('.setup-step')).find(node => node.textContent?.includes('Create an API key'))!;
    expect(key.className).toContain('todo');
  });

  it('counts a key only while it can call, and a first request only once one was served', async () => {
    const now = Date.parse('2026-11-12T12:00:00Z');
    expect(usableKey({ status: 'active', expires_at: null }, now)).toBe(true);
    expect(usableKey({ status: 'active', expires_at: '2026-11-12T11:59:59Z' }, now)).toBe(false);
    expect(usableKey({ status: 'revoked', expires_at: null }, now)).toBe(false);
    const fetchMock = vi.fn(async (url: string) => {
      if (url.includes('/provider-accounts')) return json({ release_id: 'rel', items: [{ id: 'a', deployment_count: 1 }], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [{ id: 'rel', version: '1', active: true }], next_page_token: null });
      if (url.includes('/budgets')) return json({ items: [{ id: 'b', scope: 'organization', starts_at: null, ends_at: null }], next_page_token: null });
      // An expired key, used once (refused for want of a budget, say): no
      // usable key, and no request served.
      if (url.includes('/keys')) return json({ items: [{ id: 'k', status: 'active', expires_at: '2020-01-01T00:00:00Z', last_used_at: '2019-12-01T00:00:00Z' }], next_page_token: null });
      if (url.includes('/service-accounts')) return json({ items: [{ id: 'sa', enabled: true }], next_page_token: null });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    });
    vi.stubGlobal('fetch', fetchMock);
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    const states = Object.fromEntries(Array.from(container.querySelectorAll('.setup-step')).map(node => [node.querySelector('strong')!.textContent, node.className.split(' ')[1]]));
    expect(states['Create an API key']).toBe('todo');
    expect(states['Send a first request']).toBe('todo');
    // Served is read over the longest window the report takes.
    const windows = fetchMock.mock.calls.map(([url]) => new URL(String(url), 'http://x').searchParams).filter(params => params.has('from'))
      .map(params => (Date.parse(params.get('to')!) - Date.parse(params.get('from')!)) / 86_400_000);
    expect(windows.map(Math.round)).toContain(366);
  });

  it('offers the price changes to adopt, and adopts one into a new release', async () => {
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/manifest')) return json(manifest);
      if (url.endsWith('/price-drift')) return json(drift);
      if (url.endsWith('/price-policy')) return json({ auto_adopt: false, version: 1, updated_at: '' });
      if (url.endsWith('/catalog/releases') && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        return json({ id: 'new', version: body.release.version, digest: 'c'.repeat(64), created_at: '', sealed_at: '', active: false, activated_at: null }, 201);
      }
      if (init?.method === 'POST') return json({});
      if (url.includes('/provider-accounts')) return json({ release_id: RELEASE, items: [{ id: 'a', deployment_count: 2 }], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [{ id: RELEASE, version: '2026.10.2', active: true }], next_page_token: null, activation_version: 2 });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    });
    vi.stubGlobal('fetch', fetchMock);
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    const card = container.querySelector('section[aria-labelledby="price-changes-title"]')!;
    expect(card.className).toContain('overview-card');
    expect(card.textContent).toContain('in $2.50 → $1.25');
    act(() => (card.querySelector('input[type="checkbox"]') as HTMLInputElement).click());
    act(() => (Array.from(card.querySelectorAll('button')).find(node => node.textContent === 'Adopt 1 and activate') as HTMLButtonElement).click());
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    const [[, created]] = fetchMock.mock.calls.filter(([url, request]) => String(url).endsWith('/catalog/releases') && (request as RequestInit | undefined)?.method === 'POST');
    expect(JSON.parse(String((created as RequestInit).body)).prices[1]).toMatchObject({ deployment: 'gpt-5-prod', operation: 'chat', input_minor_per_token: 125, source: 'price-sheet:openai' });
    expect(container.textContent).toContain('Adopted one published price');
  });

  it('lists only held prices while they are adopted automatically, and dismisses one', async () => {
    const claude = {
      ...drift.prices[0], deployment: 'claude-us', model: 'claude', location: 'us-east-1', provider_family: 'bedrock',
      comparisons: [{ ...drift.prices[0].comparisons[0], source: 'price-sheet:bedrock', observed_item: 'claude' }],
    };
    const hold = {
      id: 'h1', deployment: 'gpt-5-prod', operation: 'chat', model: 'gpt-5', currency: 'USD', source: 'price-sheet:openai', reason: 'large_decrease',
      // First found against an earlier release: still open.
      changes: [{ dimension: 'input', catalog: 250, observed: '125', proposed: 125 }], status: 'held', release_id: 'older', created_at: '', resolved_at: null, version: 1,
    };
    // Dismissed, and the source still publishes what was dismissed.
    const gemini = { ...hold, id: 'h2', deployment: 'gemini-prod', model: 'gemini', status: 'dismissed', resolved_at: '', version: 2 };
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/price-drift')) return json({ ...drift, prices: [drift.prices[0], claude, { ...drift.prices[0], deployment: 'gemini-prod', model: 'gemini' }] });
      if (url.endsWith('/price-policy')) return json({ auto_adopt: true, version: 0, updated_at: null });
      if (url.endsWith('/price-holds?all=true')) return json({ items: [hold, gemini] });
      if (url.endsWith('/price-holds/h1/dismiss')) return json({ ...hold, status: 'dismissed', version: 2 });
      if (url.includes('/provider-accounts')) return json({ release_id: RELEASE, items: [{ id: 'a', deployment_count: 2 }], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [{ id: RELEASE, version: '2026.10.2', active: true }], next_page_token: null, activation_version: 2 });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    });
    vi.stubGlobal('fetch', fetchMock);
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    const card = container.querySelector('section[aria-labelledby="price-changes-title"]')!;
    expect(card.textContent).toContain('held: drops below half');
    expect(card.textContent).not.toContain('claude ·');
    expect(card.textContent).toContain('One published price will be adopted automatically within the hour.');
    expect(card.textContent).toContain('One dismissed published price is left as it is');
    act(() => (Array.from(card.querySelectorAll('button')).find(node => node.textContent === 'Dismiss') as HTMLButtonElement).click());
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    const [[, dismissal]] = fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/dismiss'));
    expect(new Headers((dismissal as RequestInit).headers).get('If-Match')).toBe('"v1"');
    expect(container.textContent).toContain('gpt-5 keeps its price');
    expect(container.textContent).not.toContain('held: drops below half');
  });

  it('shows no price changes card when every price matches', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.endsWith('/price-drift')) return json({ release_id: RELEASE, sources: [], prices: [] });
      if (url.includes('/provider-accounts')) return json({ release_id: RELEASE, items: [{ id: 'a', deployment_count: 1 }], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [{ id: RELEASE, version: '1', active: true }], next_page_token: null });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    }));
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    expect(container.textContent).toContain('1 deployment');
    expect(container.textContent).not.toContain('Price changes');
  });

  it('offers a request to copy for the last step', () => {
    const text = exampleRequest('https://gw.example');
    expect(text).toContain('curl https://gw.example/api/inference/v1/chat/completions');
    expect(text).toContain('Idempotency-Key');
    expect(text).toContain('"model": "YOUR_ALIAS"');
  });

  it('hides the checklist once every step is done', async () => {
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.includes('/provider-accounts')) return json({ release_id: 'rel', items: [{ id: 'a', deployment_count: 1 }], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [{ id: 'rel', version: '1', active: true }], next_page_token: null });
      if (url.includes('/budgets')) return json({ items: [{ id: 'b', scope: 'organization' }], next_page_token: null });
      if (url.includes('/keys')) return json({ items: [{ id: 'l', status: 'active' }], next_page_token: null });
      if (url.includes('/service-accounts')) return json({ items: [{ id: 'sa', enabled: true }], next_page_token: null });
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [total('USD', 'exact', 1, 1)], items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    }));
    act(() => root.render(<OverviewScreen organizationId="org-1" onNavigate={() => undefined} />));
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
    expect(container.textContent).toContain('1 deployment');
    expect(container.querySelector('.setup-checklist')).toBeNull();
    expect(container.textContent).not.toContain('No budget is set');
  });
});
