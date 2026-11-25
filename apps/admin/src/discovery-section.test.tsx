import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ProvidersScreen } from './providers-screen';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string, detail = '') =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-9' }, status);
const CATALOG = '/api/admin/v1/organizations/org-1/catalog';
const account = { id: 'a-1', release_id: 'r-1', adapter_instance_key: 'bedrock-1', family: 'bedrock', deployment_count: 2 };
const job = {
  id: 'j-1', adapter_instance_key: 'bedrock-1', provider_family: 'bedrock', trigger: 'scheduled', status: 'succeeded', attempts: 1,
  error_code: null, models_listed: 3, models_added: 1, models_missing: 0, requested_by: null,
  created_at: '2026-10-13T06:00:00Z', started_at: '2026-10-13T06:00:05Z', finished_at: '2026-10-13T06:00:09Z',
};
const offered = {
  adapter_instance_key: 'bedrock-1', provider_family: 'bedrock', model: 'amazon.nova-pro-v1:0', display_name: 'Nova Pro',
  first_seen_at: '2026-10-10T00:00:00Z', last_seen_at: '2026-10-13T06:00:09Z', missing_since: null, deployed: false,
  operations: null, availability: 'listed' as const, evidence: null,
};
const deployed = { ...offered, model: 'anthropic.claude-3-5-haiku', display_name: 'Claude 3.5 Haiku', deployed: true };
const drift = {
  release_id: 'r-1',
  accounts: [{
    adapter_instance_key: 'bedrock-1', provider_family: 'bedrock', last_job: job, last_synced_at: '2026-10-13T06:00:09Z',
    undeployed_models: 1,
    unlisted_deployments: [{ deployment: 'bedrock-1/old-model', model: 'old-model', missing_since: '2026-10-12T00:00:00Z' }],
  }],
};

const offerOn = {
  adapter_instance_key: 'bedrock-1', location: 'us-east-1', created_at: '2026-10-27T10:00:00Z', updated_at: '2026-10-27T10:00:00Z',
  applied_at: '2026-10-27T10:01:00Z', applied_release_id: 'r-2', error_code: null, version: 1,
  models: [
    { model: 'amazon.nova-pro-v1:0', deployment: 'bedrock-1-amazon.nova-pro-v1:0', status: 'offered', price_source: 'price-sheet:bedrock', price_item: 'nova pro', updated_at: '2026-10-27T10:01:00Z' },
    { model: 'us.anthropic.claude-sonnet-4-6', deployment: 'bedrock-1-us.anthropic.claude-sonnet-4-6', status: 'needs_price', price_source: null, price_item: null, updated_at: '2026-10-27T10:01:00Z' },
  ],
};

type Handler = (url: string, init?: RequestInit) => Response | Promise<Response> | undefined;

describe('discovery tab', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let models: unknown[];
  let jobs: unknown[];
  let autoOffer: unknown;
  let override: Handler;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    history.replaceState(null, '', '/');
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    models = [offered, deployed];
    jobs = [job];
    autoOffer = null;
    override = () => undefined;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const custom = await override(url, init);
      if (custom) return custom;
      if (url.startsWith(`${CATALOG}/provider-accounts`)) return json({ items: [account], next_page_token: null, release_id: 'r-1' });
      if (url.startsWith(`${CATALOG}/auto-offers/`) && init?.method === 'PUT') {
        autoOffer = { ...offerOn, location: JSON.parse(String(init.body)).location, version: autoOffer ? 2 : 1 };
        return json(autoOffer, 201);
      }
      if (url.startsWith(`${CATALOG}/auto-offers/`) && init?.method === 'DELETE') return new Response(null, { status: 204 });
      if (url.startsWith(`${CATALOG}/auto-offers/`)) return autoOffer ? json(autoOffer) : problem(404, 'not_found');
      if (url.startsWith(`${CATALOG}/discovery-drift`)) return json(drift);
      if (url.startsWith(`${CATALOG}/price-drift`)) return json({ release_id: 'r-1', prices: [] });
      if (url.startsWith(`${CATALOG}/discovered-models`)) return json({ items: models, next_page_token: null });
      if (url.startsWith(`${CATALOG}/sync-jobs`) && init?.method === 'POST') return json({ ...job, id: 'j-2', trigger: 'manual', status: 'queued', models_listed: null }, 202);
      if (url.startsWith(`${CATALOG}/sync-jobs`)) return json({ items: jobs, next_page_token: null });
      return json({ items: [], next_page_token: null, release_id: 'r-1' });
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    vi.useRealTimers();
  });

  const settle = async () => {
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement | undefined;
  const click = async (target: HTMLElement | undefined) => {
    if (!target) throw new Error('no such button');
    act(() => target.click());
    await settle();
  };
  const show = async (view: string) => click(Array.from(document.querySelectorAll<HTMLButtonElement>('[aria-label="Discovery views"] button')).find(node => node.textContent === view));
  /** Opens Discovery, and then one of its views when named. */
  const open = async (role = 'Owner', view?: 'Discovered models' | 'Automatic offers' | 'Jobs') => {
    act(() => root.render(<ProvidersScreen section="connections" organizationId="org-1" role={role} />));
    await settle();
    openSection('connections', 'discovery');
    await settle();
    if (view) await show(view);
  };
  const urls = (fragment: string) => fetchMock.mock.calls.map(([url]) => String(url)).filter(url => url.includes(fragment));
  const posts = () => fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === 'POST');
  const advance = async (ms: number) => {
    await act(async () => { vi.advanceTimersByTime(ms); });
    await settle();
  };

  it('shows drift, discovered models and jobs, with unknown capabilities as unknown', async () => {
    await open();
    const driftTable = container.querySelector('[aria-label="Discovery drift by account"]')!;
    expect(driftTable.textContent).toContain('bedrock-1');
    expect(driftTable.textContent).toContain('1 model');
    expect(driftTable.textContent).toContain('1 deployment');
    expect(container.textContent).toContain('not listed since');
    await show('Discovered models');
    const rows = Array.from(container.querySelectorAll('[aria-label="Discovered models"] .data-row:not(.head)'));
    expect(rows[0].textContent).toContain('amazon.nova-pro-v1:0');
    expect(rows[0].textContent).toContain('Not in the catalog');
    expect(rows[0].querySelector('[data-label="Capabilities"]')!.textContent).toBe('unknown');
    expect(rows[1].textContent).toContain('In the catalog');
    expect(container.textContent).not.toMatch(/supported/i);
    await show('Jobs');
    expect(container.querySelector('[aria-label="Discovery jobs"]')?.textContent).toContain('Listed 3 models: 1 new, 0 no longer listed.');
  });

  it('filters discovered models, and shows what an account offers that is not in the catalog', async () => {
    await open();
    await click(container.querySelector('[aria-label^="Show the 1 models bedrock-1 offers"]') as HTMLButtonElement);
    const last = urls('/discovered-models').pop()!;
    expect(last).toContain('adapter_instance_key=bedrock-1');
    expect(last).toContain('state=listed');
    expect(last).toContain('deployed=false');
    expect((document.getElementById('discovered-deployed') as HTMLSelectElement).value).toBe('false');
    expect(document.activeElement?.textContent).toBe('Discovered models');
    expect(new URLSearchParams(location.search).get('discovery.view')).toBe('models');
  });

  it('shows only the drift row’s family when an account key was reused for another', async () => {
    models = [offered, { ...offered, provider_family: 'openai', model: 'gpt-4o', display_name: null }];
    await open();
    await click(container.querySelector('[aria-label^="Show the 1 models bedrock-1 offers"]') as HTMLButtonElement);
    const rows = Array.from(container.querySelectorAll('[aria-label="Discovered models"] .data-row:not(.head)'));
    expect(rows).toHaveLength(1);
    expect(rows[0].textContent).toContain('amazon.nova-pro-v1:0');
    // Choosing an account by hand shows every family again.
    const select = document.getElementById('discovered-account') as HTMLSelectElement;
    act(() => { select.value = 'bedrock-1'; select.dispatchEvent(new Event('change', { bubbles: true })); });
    await settle();
    expect(container.querySelectorAll('[aria-label="Discovered models"] .data-row:not(.head)')).toHaveLength(2);
  });

  it('keeps loading pages until the drift row’s family turns up', async () => {
    const other = { ...offered, provider_family: 'openai', model: 'gpt-4o', display_name: null };
    override = url => {
      if (!url.startsWith(`${CATALOG}/discovered-models`)) return undefined;
      return url.includes('page_token=p2') ? json({ items: [offered], next_page_token: null }) : json({ items: [other], next_page_token: 'p2' });
    };
    await open();
    await click(container.querySelector('[aria-label^="Show the 1 models bedrock-1 offers"]') as HTMLButtonElement);
    expect(container.textContent).not.toContain('No models match');
    const rows = Array.from(container.querySelectorAll('[aria-label="Discovered models"] .data-row:not(.head)'));
    expect(rows).toHaveLength(1);
    expect(rows[0].textContent).toContain('amazon.nova-pro-v1:0');
  });

  it('links an offered model to Add provider and an unlisted deployment to Models, without writing the catalog', async () => {
    await open('Owner', 'Discovered models');
    await click(container.querySelector('[aria-label="Add amazon.nova-pro-v1:0 through Connect a provider"]') as HTMLButtonElement);
    expect(location.pathname).toBe('/admin/connections/connect');
    openSection('connections', 'discovery');
    await settle();
    await show('Changes');
    await click(button('Open Models'));
    // Models is its own page now: the link goes there.
    expect(location.pathname).toBe('/admin/models/models');
    history.replaceState(null, '', '/');
    expect(urls('/catalog/releases').filter(url => url.endsWith('/releases'))).toHaveLength(0);
    expect(posts()).toHaveLength(0);
  });

  it('starts discovery and polls the job with backoff until it finishes', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] });
    const states = ['running', 'succeeded'];
    override = (url, init) => {
      if (!init?.method && url === `${CATALOG}/sync-jobs/j-2`) {
        const status = states.shift() ?? 'succeeded';
        return json({ ...job, id: 'j-2', trigger: 'manual', status, models_listed: status === 'succeeded' ? 4 : null, models_added: 1 });
      }
      return undefined;
    };
    await open('Owner', 'Discovered models');
    await click(button('Discover models'));
    const [[url, init]] = posts();
    expect(url).toBe(`${CATALOG}/sync-jobs`);
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ adapter_instance_key: 'bedrock-1' });
    expect(new Headers((init as RequestInit).headers).get('Idempotency-Key')).toBeTruthy();
    const status = () => container.querySelector('.job-status')!;
    expect(status().getAttribute('role')).toBe('status');
    expect(status().textContent).toContain('Queued');
    expect(button('Discover models')!.disabled).toBe(true);

    await advance(1999);
    expect(urls('/sync-jobs/j-2')).toHaveLength(0);
    await advance(1);
    expect(urls('/sync-jobs/j-2')).toHaveLength(1);
    expect(status().textContent).toContain('Listing the provider’s models');
    const modelLoads = urls('/discovered-models').length;

    await advance(3999);
    expect(urls('/sync-jobs/j-2')).toHaveLength(1);
    await advance(1);
    expect(urls('/sync-jobs/j-2')).toHaveLength(2);
    expect(status().textContent).toContain('Listed 4 models');
    // Finished: the lists reload with what it found, and polling stops.
    expect(urls('/discovered-models').length).toBeGreaterThan(modelLoads);
    await advance(60_000);
    expect(urls('/sync-jobs/j-2')).toHaveLength(2);
    expect(button('Discover models')!.disabled).toBe(false);
  });

  it('stops polling after ten minutes and offers to check again', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] });
    override = (url, init) => (!init?.method && url === `${CATALOG}/sync-jobs/j-2` ? json({ ...job, id: 'j-2', status: 'queued' }) : undefined);
    await open();
    await click(button('Discover models'));
    for (let i = 0; i < 50; i += 1) await advance(15_000);
    const calls = urls('/sync-jobs/j-2').length;
    expect(calls).toBeLessThan(50);
    expect(container.textContent).toContain('Still not finished after 10 minutes');
    await advance(60_000);
    expect(urls('/sync-jobs/j-2')).toHaveLength(calls);
    await click(button('Check again'));
    await advance(2000);
    expect(urls('/sync-jobs/j-2')).toHaveLength(calls + 1);
  });

  it('gives up after ten minutes even while a status request never answers', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout', 'Date'] });
    override = (url, init) => (!init?.method && url === `${CATALOG}/sync-jobs/j-2` ? new Promise<Response>(() => undefined) : undefined);
    await open();
    await click(button('Discover models'));
    await advance(2000);
    expect(urls('/sync-jobs/j-2')).toHaveLength(1);
    expect(container.textContent).not.toContain('Still not finished after 10 minutes');
    await advance(10 * 60_000);
    expect(container.textContent).toContain('Still not finished after 10 minutes');
    expect(urls('/sync-jobs/j-2')).toHaveLength(1);
  });

  it('follows the job already running when discovery is in progress', async () => {
    override = (url, init) => {
      if (init?.method === 'POST') return problem(409, 'catalog_sync_in_progress', 'the account already has a queued or running job');
      if (url.startsWith(`${CATALOG}/sync-jobs?adapter_instance_key=bedrock-1`)) return json({ items: [{ ...job, id: 'j-9', status: 'running' }], next_page_token: null });
      return undefined;
    };
    await open();
    await click(button('Discover models'));
    expect(container.textContent).toContain('A discovery of this account is already queued or running');
    expect(container.querySelector('.job-status')?.textContent).toContain('Listing the provider’s models');
  });

  it('retries with the same key after a server error, and a new one after a refusal', async () => {
    const answers = [problem(503, 'service_unavailable'), problem(422, 'unknown_provider_account')];
    override = (url, init) => (init?.method === 'POST' ? answers.shift() : undefined);
    await open();
    await click(button('Discover models'));
    await click(button('Discover models'));
    await click(button('Discover models'));
    const [first, second, third] = posts().map(([, init]) => new Headers((init as RequestInit).headers).get('Idempotency-Key'));
    expect(second).toBe(first);
    expect(third).not.toBe(second);
  });

  it('keeps the form and explains a 403', async () => {
    override = (url, init) => (init?.method === 'POST' ? problem(403, 'forbidden', 'administrative permission denied') : undefined);
    await open();
    await click(button('Discover models'));
    expect(container.querySelector('[role="alert"]:not([class*="counter-alert"])')?.textContent).toContain('Only organization owners and admins can start discovery.');
    expect((document.getElementById('discovery-account') as HTMLSelectElement).value).toBe('bedrock-1');
    expect(button('Discover models')!.disabled).toBe(false);
  });

  it('lets auditors and read-only members read, without starting discovery', async () => {
    for (const role of ['Auditor', 'ReadOnly']) {
      history.replaceState(null, '', '/');
      await open(role, 'Discovered models');
      expect(button('Discover models')).toBeUndefined();
      expect(container.textContent).toContain('Only organization owners and admins can start it.');
      expect(container.textContent).toContain('amazon.nova-pro-v1:0');
    }
  });

  it('says when nothing has been discovered and no job has run', async () => {
    models = [];
    jobs = [];
    await open('Owner', 'Discovered models');
    expect(container.textContent).toContain('No models discovered yet');
    await show('Jobs');
    expect(container.textContent).toContain('No discovery jobs');
  });

  it('says when there is no active release', async () => {
    override = url => (url.startsWith(`${CATALOG}/provider-accounts`) ? json({ items: [], next_page_token: null, release_id: null }) : undefined);
    await open();
    expect(container.textContent).toContain('No models yet');
    expect(button('Discover models')).toBeUndefined();
  });

  it('explains a 403 when reading discovered models', async () => {
    override = url => (url.startsWith(`${CATALOG}/discovered-models`) ? problem(403, 'forbidden') : undefined);
    await open('Owner', 'Discovered models');
    expect(container.textContent).toContain('You do not have permission to view this organization’s discovered models.');
  });

  it('turns automatic offers on and off for an account, and says which models need a price', async () => {
    await open('Owner', 'Automatic offers');
    expect(document.body.textContent).toContain('New models of bedrock-1 are only listed, not offered.');
    const location = document.getElementById('auto-offer-location') as HTMLInputElement;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      setter.call(location, 'us-east-1');
      location.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await click(button('Turn on'));
    const put = fetchMock.mock.calls.find(([url, init]) => String(url).includes('/auto-offers/bedrock-1') && (init as RequestInit).method === 'PUT');
    expect(put).toBeDefined();
    const headers = new Headers((put![1] as RequestInit).headers);
    expect(headers.get('If-Match')).toBe('*');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
    expect(JSON.parse(String((put![1] as RequestInit).body))).toEqual({ location: 'us-east-1' });
    expect(document.body.textContent).toContain('One model offered; one needs a price.');
    expect(document.body.textContent).toContain('Map bedrock-1-us.anthropic.claude-sonnet-4-6 to a published item under Pricing');
    expect(document.body.textContent).toContain('Priced by price-sheet:bedrock: nova pro');

    await click(button('Turn off'));
    const deleted = fetchMock.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'DELETE');
    expect(new Headers((deleted![1] as RequestInit).headers).get('If-Match')).toBe('"v1"');
    expect(document.body.textContent).toContain('only listed, not offered');
  });

  it('lets read-only roles see automatic offers without changing them', async () => {
    autoOffer = offerOn;
    await open('Auditor', 'Automatic offers');
    expect(document.body.textContent).toContain('One model offered; one needs a price.');
    expect(document.body.textContent).toContain('Only organization owners and admins can change automatic offers.');
    expect(button('Turn off')).toBeUndefined();
  });

  it('keeps the account while a change is pending, so an answer is never shown under another', async () => {
    let answer: (response: Response) => void = () => undefined;
    override = (url, init) => (url.includes('/auto-offers/') && init?.method === 'PUT'
      ? new Promise<Response>(resolve => { answer = resolve; })
      : undefined);
    await open('Owner', 'Automatic offers');
    await click(button('Turn on'));
    const select = document.getElementById('auto-offer-account') as HTMLSelectElement;
    expect(select.disabled).toBe(true);
    await act(async () => { answer(json(offerOn, 201)); });
    await settle();
    expect(select.disabled).toBe(false);
    expect(document.body.textContent).toContain('One model offered; one needs a price.');
  });
});
