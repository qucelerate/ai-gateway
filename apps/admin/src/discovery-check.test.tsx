import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { act } from 'react-dom/test-utils';
import { createRoot, type Root } from 'react-dom/client';
import { DiscoveryCheck } from './discovery-check';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const job = (status: string, extra: Record<string, unknown> = {}) => ({ id: 'job-1', adapter_instance_key: 'bedrock-main', status, error_code: null, ...extra });

describe('discovery check on an empty Deployed view', () => {
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
    vi.useRealTimers();
  });
  const settle = async () => { for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); }); };
  const render = async (onChecked = () => undefined) => {
    act(() => root.render(<DiscoveryCheck organizationId="org" accountKey="bedrock-main" onChecked={onChecked} />));
    await settle();
  };

  it('offers a first check when none has run, then follows it to the end', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
    let polls = 0;
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (init?.method === 'POST') return json(job('queued'), 202);
      if (url.includes('/sync-jobs/job-1')) return json(job(++polls > 1 ? 'succeeded' : 'running'));
      return json({ items: [], next_page_token: null });
    });
    vi.stubGlobal('fetch', fetchMock);
    const onChecked = vi.fn();
    await render(onChecked);
    expect(container.textContent).toContain('Not checked yet');
    act(() => (Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Check now') as HTMLButtonElement).click());
    await settle();
    expect(fetchMock.mock.calls.some(([url, init]) => String(url).endsWith('/catalog/sync-jobs') && init?.method === 'POST')).toBe(true);
    expect(container.textContent).toContain('Checking…');
    await act(async () => { vi.advanceTimersByTime(3_000); });
    await settle();
    expect(onChecked).not.toHaveBeenCalled();
    await act(async () => { vi.advanceTimersByTime(3_000); });
    await settle();
    expect(onChecked).toHaveBeenCalledTimes(1);
    expect(container.textContent).toContain('The last check found no model bedrock-main can serve.');
  });

  it('says the last check could not be read, rather than that none ran, and retries', async () => {
    let fail = true;
    vi.stubGlobal('fetch', vi.fn(async () => (fail ? json({ status: 500, code: 'internal', detail: 'boom' }, 500) : json({ items: [job('failed')], next_page_token: null }))));
    await render();
    expect(container.textContent).not.toContain('Not checked yet');
    expect(container.textContent).not.toContain('Check now');
    expect(container.querySelector('[role="alert"]:not([class*="counter-alert"])')).not.toBeNull();
    fail = false;
    act(() => (Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Try again') as HTMLButtonElement).click());
    await settle();
    expect(container.textContent).toContain('The last check failed');
  });

  it('says a check failed, with its code', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => json({ items: [job('failed', { error_code: 'provider_access_denied' })], next_page_token: null })));
    await render();
    expect(container.textContent).toContain('The last check failed');
    expect(container.textContent).toContain('(provider_access_denied)');
    expect(container.textContent).toContain('Check again');
  });

  it('stops following a check the gateway refuses to show, and says why', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'setTimeout', 'clearTimeout'] });
    const fetchMock = vi.fn(async (url: string) => {
      if (url.includes('/sync-jobs/job-1')) return json({ type: '', code: 'forbidden', title: '', status: 403, detail: 'no', instance: '', request_id: '' }, 403);
      return json({ items: [job('running')], next_page_token: null });
    });
    vi.stubGlobal('fetch', fetchMock);
    await render();
    expect(container.textContent).toContain('Checking…');
    await act(async () => { vi.advanceTimersByTime(3_000); });
    await settle();
    expect(container.textContent).toContain('Only organization owners and admins can follow this check.');
    const polls = fetchMock.mock.calls.filter(([url]) => String(url).includes('/sync-jobs/job-1')).length;
    await act(async () => { vi.advanceTimersByTime(30_000); });
    expect(fetchMock.mock.calls.filter(([url]) => String(url).includes('/sync-jobs/job-1')).length).toBe(polls);
  });

  it('says a check is taking a while instead of following it forever', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval', 'setTimeout', 'clearTimeout'] });
    vi.stubGlobal('fetch', vi.fn(async (url: string) => url.includes('/sync-jobs/job-1')
      ? json({ type: '', code: 'service_unavailable', title: '', status: 503, detail: 'down', instance: '', request_id: '' }, 503)
      : json({ items: [job('queued')], next_page_token: null })));
    await render();
    await act(async () => { vi.advanceTimersByTime(120_000); });
    await settle();
    expect(container.textContent).toContain('Still checking');
  });
});
