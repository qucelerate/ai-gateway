import { afterEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { HEALTH_REFRESH_MS, HEALTH_WINDOW_MS, HealthBadge, healthFor, healthOf, loadDeploymentHealth, useDeploymentHealth } from './route-health';

const totals = (requests: number, failed = 0, pending = 0, expired = 0, unknown = 0) => ({
  requests, failed_requests: failed, pending_requests: pending, expired_requests: expired, unknown_requests: unknown,
  tokens: {}, amounts: [], unpriced_requests: 0, no_usage_requests: 0, incomplete_requests: 0,
  duration: { average_ms: null, p50_ms: null, p95_ms: null },
});
const report = (series: [string, ReturnType<typeof totals>][]) => ({
  from: '', to: '', granularity: 'hour', split_by: 'deployment', rank_by: 'requests', buckets: [], totals: totals(0),
  previous: { from: '', to: '', totals: totals(0) }, truncated: false,
  series: series.map(([value, t]) => ({ value, label: value, other: false, totals: t, points: [] })),
});
const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { 'Content-Type': 'application/json' } });

describe('route health', () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  it('calls a deployment failing when at least half of three or more requests failed', () => {
    expect(healthOf(0, 0).state).toBe('idle');
    expect(healthOf(10, 0).state).toBe('healthy');
    expect(healthOf(2, 1).state).toBe('degraded');
    expect(healthOf(2, 2).state).toBe('degraded');
    expect(healthOf(10, 4).state).toBe('degraded');
    expect(healthOf(3, 2).state).toBe('failing');
    expect(healthOf(6, 3).state).toBe('failing');
  });

  it('counts failed and failed-over requests of the last 15 minutes, not pending ones', async () => {
    const now = Date.parse('2026-11-16T20:00:00Z');
    const fetchMock = vi.fn(async (url: string) => {
      const query = new URL(url, 'http://x').searchParams;
      expect(query.get('from')).toBe(new Date(now - HEALTH_WINDOW_MS).toISOString());
      expect(query.get('split_by')).toBe('deployment');
      expect(query.get('deployment')).toBe('a,b\\,c,idle');
      if (query.get('status') === 'failed-over') return json(report([['a', totals(2)]]));
      return json(report([['a', totals(12, 1, 2)], ['b,c', totals(9, 0, 0, 3, 1)], ['idle', totals(2, 0, 0, 2)]]));
    });
    vi.stubGlobal('fetch', fetchMock);
    const health = await loadDeploymentHealth('org-1', ['a', 'b,c', 'a', 'idle'], now);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    expect(health.get('a')).toEqual({ state: 'degraded', requests: 10, failed: 3 });
    // Expired and unknown requests have no provider result to count.
    expect(health.get('b,c')).toEqual({ state: 'healthy', requests: 5, failed: 0 });
    expect(health.get('idle')).toEqual({ state: 'idle', requests: 0, failed: 0 });
  });

  it('reads health again every minute, keeping what it read when a refresh fails', async () => {
    vi.useFakeTimers();
    let failures = 0;
    let broken = false;
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (broken) throw new Error('offline');
      if (url.includes('status=failed-over')) return json(report([]));
      return json(report([['a', totals(4, failures)]]));
    }));
    const seen: string[] = [];
    function Probe() {
      const { health } = useDeploymentHealth('org-1', ['a']);
      const entry = healthFor(health, 'a');
      seen.push(typeof entry === 'object' ? entry.state : String(entry));
      return null;
    }
    const root = createRoot(document.createElement('div'));
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    await act(async () => { root.render(<Probe />); });
    await act(async () => { await Promise.resolve(); });
    expect(seen.at(-1)).toBe('healthy');
    failures = 3;
    await act(async () => { await vi.advanceTimersByTimeAsync(HEALTH_REFRESH_MS); });
    expect(seen.at(-1)).toBe('failing');
    broken = true;
    await act(async () => { await vi.advanceTimersByTimeAsync(HEALTH_REFRESH_MS); });
    expect(seen.at(-1)).toBe('failing');
    // A slow read holds the next one back until it answers.
    broken = false;
    let answer: () => void = () => {};
    const calls = (fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.length;
    vi.stubGlobal('fetch', vi.fn(() => new Promise<Response>(resolve => { answer = () => resolve(json(report([['a', totals(4)]]))); })));
    await act(async () => { await vi.advanceTimersByTimeAsync(HEALTH_REFRESH_MS * 3); });
    expect((fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.length).toBe(2);
    expect(calls).toBeGreaterThan(0);
    await act(async () => { answer(); await Promise.resolve(); });
    act(() => root.unmount());
  });

  it('shows the counts behind a degraded or failing deployment', () => {
    const container = document.createElement('div');
    const root = createRoot(container);
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    act(() => root.render(<HealthBadge health={healthOf(6, 3)} />));
    expect(container.textContent).toBe('Failing · 3/6');
    expect(container.querySelector('[title]')?.getAttribute('title')).toBe('3 of 6 requests failed in the last 15 minutes.');
    act(() => root.render(<HealthBadge health="unavailable" />));
    expect(container.textContent).toBe('—');
    act(() => root.unmount());
  });
});

describe('route dialog health', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('reads health for a deployment added in the dialog', async () => {
    const { EditRouteDialog } = await import('./catalog-actions');
    const capability = { operation: 'embeddings', max_input_bytes: 1000, max_output_tokens: 1, max_tokens_per_byte: 1, tokenizer: 'configured', reservation_ttl_seconds: 300 };
    const price = (deployment: string) => ({ deployment, operation: 'embeddings', input_minor_per_token: 2, output_minor_per_token: 0, cached_input_minor_per_token: 0, reasoning_minor_per_token: 0, other_minor_per_token: 0, currency: 'USD', source: 'operator', source_version: 'v', confidence: 'conservative' });
    const manifest = {
      release: { version: 'r' },
      providers: [{ adapter_instance_key: 'acct', family: 'openai' }],
      deployments: ['one', 'two'].map(name => ({ name, model: name, location: null, adapter_instance_key: 'acct', capabilities: [capability] })),
      prices: [price('one'), price('two')],
      routes: [{ alias: 'embed', operation: 'embeddings', deployment: 'one' }],
    };
    const asked: string[] = [];
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.endsWith('/manifest')) return json(manifest);
      const query = new URL(url, 'http://x').searchParams;
      asked.push(query.get('deployment') ?? '');
      if (query.get('status') === 'failed-over') return json(report([]));
      return json(report([['one', totals(5)], ['two', totals(4, 4)]]));
    }));
    const container = document.createElement('div');
    document.body.appendChild(container);
    const root = createRoot(container);
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    const settle = async () => { for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); }); };
    await act(async () => { root.render(<EditRouteDialog organizationId="org-1" releaseId="rel" alias="embed" operation="embeddings" onClose={() => {}} onChanged={() => {}} />); });
    await settle();
    expect(document.body.textContent).toContain('Healthy');
    const select = document.querySelector('#route-add') as HTMLSelectElement;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(select, 'two');
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    const add = Array.from(document.querySelectorAll('button')).find(button => button.textContent === 'Add')!;
    await act(async () => { add.click(); });
    await settle();
    expect(asked).toContain('one,two');
    expect(document.body.textContent).toContain('Failing · 4/4');
    act(() => root.unmount());
    container.remove();
  });
});
