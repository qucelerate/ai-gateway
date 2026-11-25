import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ProvidersScreen, perMillion, reasoningPrice } from './providers-screen';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const RELEASE = '11111111-1111-1111-1111-111111111111';
const price = {
  id: 'p', currency: 'USD', input_minor_per_token: 250, output_minor_per_token: 1000,
  cached_input_minor_per_token: 0, reasoning_minor_per_token: 0, other_minor_per_token: 0,
  source: 'list', source_version: '2026-10', confidence: 'known',
};
const deployment = {
  id: 'd-1', provider_account_id: 'a-1', adapter_instance_key: 'openai-main', family: 'openai', name: 'gpt-5-prod',
  model: 'gpt-5', location: null,
  operations: [
    { operation: 'chat', capability: { id: 'c', max_output_tokens: 1 }, price },
    { operation: 'chat_stream', capability: { id: 'c', max_output_tokens: 1 }, price: { ...price, input_minor_per_token: 300, output_minor_per_token: 1200 } },
    { operation: 'embeddings', capability: null, price: null },
  ],
};
const accounts = [
  { id: 'a-1', release_id: RELEASE, adapter_instance_key: 'openai-main', family: 'openai', deployment_count: 1 },
  { id: 'a-2', release_id: RELEASE, adapter_instance_key: 'bedrock-us', family: 'bedrock', deployment_count: 2 },
];
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
const stored = { adapter_instance_key: 'openai-main', family: 'openai', key_id: '0123456789abcdef', version: 3, created_at: '2026-10-01T00:00:00Z', rotated_at: '2026-10-02T00:00:00Z' };

describe('catalog prices', () => {
  it('quotes per-token money units per million tokens', () => {
    expect(perMillion(250, 'USD')).toContain('2.50');
  });

  it('shows a zero reasoning price as charged at the output price, not free', () => {
    expect(reasoningPrice(0, 'USD')).toBe('= output');
    expect(reasoningPrice(900, 'USD')).toContain('9.00');
  });
});

describe('providers screen', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    // The open tab is in the path: each test starts on the screen's default.
    history.replaceState(null, '', '/');
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/manifest')) return json(manifest);
      if (url.endsWith('/price-drift')) return json(drift);
      if (url.endsWith('/catalog/releases') && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        return json({ id: 'new', version: body.release.version, digest: 'c'.repeat(64), created_at: '', sealed_at: '', active: false, activated_at: null }, 201);
      }
      if (init?.method === 'PUT') return json({ ...stored, adapter_instance_key: 'bedrock-us', family: 'bedrock', version: 1 }, 201);
      if (init?.method === 'DELETE') return new Response(null, { status: 204 });
      if (init?.method === 'POST') return json({});
      if (url.includes('/usage/series')) {
        const failedOver = url.includes('status=failed-over');
        const t = (requests: number, failed: number) => ({ requests, failed_requests: failed, pending_requests: 0, expired_requests: 0, unknown_requests: 0 });
        return json({ series: failedOver ? [] : [{ value: 'gpt-5-prod', other: false, totals: t(8, 4), points: [] }] });
      }
      if (url.endsWith('/catalog/model-masters')) return json({ release_id: RELEASE, items: [], unlinked: [], links: [], deleted_records: [] });
      if (url.includes('/deployments')) return json({ release_id: RELEASE, items: [deployment], next_page_token: null });
      if (url.includes('/routes')) return json({ release_id: RELEASE, items: [{ id: 'r', alias: 'chat-default', operation: 'chat', deployment_id: 'd-1', deployment_name: 'gpt-5-prod', model: 'gpt-5', family: 'openai' }], next_page_token: null });
      if (url.includes('/provider-accounts')) return json({ release_id: RELEASE, items: accounts, next_page_token: null });
      if (url.includes('/credentials')) return json({ items: [stored], next_page_token: null });
      if (url.includes('/releases')) return json({ items: [
        { id: RELEASE, version: '2026.10.2', digest: 'a'.repeat(64), created_at: '2026-10-02T00:00:00Z', sealed_at: '2026-10-02T00:00:00Z', active: true, activated_at: '2026-10-02T00:00:00Z' },
        { id: 'old', version: '2026.10.1', digest: 'b'.repeat(64), created_at: '2026-10-01T00:00:00Z', sealed_at: '2026-10-01T00:00:00Z', active: false, activated_at: null },
      ], next_page_token: null, activation_version: 2 });
      return json({}, 404);
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement;
  const openTab = async (name: string) => {
    const section = ['Connect a provider', 'Credentials', 'Discovery', 'Cloud trust'].includes(name) ? 'connections' : 'models';
    act(() => root.render(<ProvidersScreen section={section} organizationId="org-1" />));
    await settle();
    const key = { 'Connect a provider': 'connect', Credentials: 'credentials', Discovery: 'discovery', 'Cloud trust': 'trust', Models: 'models', 'Across providers': 'masters', Aliases: 'aliases', Availability: 'availability', Pricing: 'pricing', Releases: 'releases' }[name];
    if (!key) throw new Error(`no section “${name}”`);
    openSection(section, key);
    await settle();
  };
  const type = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement;
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    act(() => {
      setValue.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    return input;
  };
  const calls = (method: string) => fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === method);

  it('lists deployments with prices and routability, and their aliases', async () => {
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    expect(container.textContent).toContain('gpt-5');
    // A stored chat_stream is chat: one operation, and input and output
    // prices in columns of their own.
    const operations = Array.from(container.querySelectorAll('.operation-list li')).map(node => node.textContent);
    expect(operations).toEqual(['Chat', 'Embeddings']);
    const row = container.querySelector('tbody tr')!.textContent ?? '';
    expect(row).toContain('$2.50');
    expect(row).toContain('$10.00');
    expect(row).not.toContain('$3.00');
    expect(container.querySelector('[title="Missing a capability or price; not routable"]')?.textContent).toBe('Embeddings');
    // Aliases and price changes are elsewhere: their own tab, and the overview.
    expect(container.querySelector('[aria-label="Model aliases"]')).toBeNull();
    expect(container.textContent).not.toContain('Price changes');
    // The expanded rows' aliases come from the release the deployments resolved.
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toContain(`/api/admin/v1/organizations/org-1/catalog/routes?release_id=${RELEASE}&page_size=100`);
  });

  it('expands a deployment in place to its aliases and full prices, and filters by operation', async () => {
    history.replaceState(null, '', '/admin/providers');
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    act(() => (document.querySelector('button[aria-label="Expand gpt-5"]') as HTMLButtonElement).click());
    const detail = document.querySelector('[role="region"][aria-label="Details of gpt-5"]')!;
    expect(detail.textContent).toContain('gpt-5-prod');
    expect(detail.textContent).toContain('chat-default · Chat');
    expect(Array.from(detail.querySelectorAll('.detail-table tbody tr')).map(row => row.textContent)).toEqual([
      expect.stringContaining('$2.50'),
      expect.stringContaining('No capability'),
    ]);
    // Reasoning priced at zero is charged at the output price.
    expect(detail.querySelector('.detail-table tbody tr')!.textContent).toContain('= output');

    const filter = Array.from(document.querySelectorAll<HTMLInputElement>('[aria-label="Filter by type"] input'))
      .find(input => input.parentElement?.textContent?.startsWith('Embeddings'))!;
    act(() => filter.click());
    expect(new URLSearchParams(location.search).get('deployments.f.operations')).toBe('embeddings');
    // The deployment's name, shown under its model, is searchable.
    const search = document.querySelector<HTMLInputElement>('input[aria-label="Search deployments"]')!;
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    act(() => {
      setValue.call(search, 'gpt-5-prod');
      search.dispatchEvent(new Event('input', { bubbles: true }));
    });
    expect(container.textContent).toContain('1 row');
    history.replaceState(null, '', '/admin/providers');
  });

  it('says aliases are loading or failed rather than absent', async () => {
    history.replaceState(null, '', '/admin/providers');
    const base = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => (url.includes('/routes') ? json({ status: 500, code: 'internal' }, 500) : base(url, init)));
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    act(() => (document.querySelector('button[aria-label="Expand gpt-5"]') as HTMLButtonElement).click());
    const detail = document.querySelector('[role="region"][aria-label="Details of gpt-5"]')!;
    expect(detail.textContent).toContain('Aliases could not be loaded');
    expect(detail.textContent).not.toContain('cannot reach it');
  });

  it('says so when no release is active', async () => {
    fetchMock.mockImplementation(async () => json({ release_id: null, items: [], next_page_token: null }));
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    expect(container.textContent).toContain('No models yet');
  });

  it('adds a Bedrock role credential, sending only that mode’s fields', async () => {
    await openTab('Credentials');
    expect(container.textContent).toContain('Stored');
    expect(container.textContent).toContain('Gateway default');
    act(() => button('Add').click());
    const input = type('credential-role_arn', 'arn:aws:iam::123456789012:role/gateway');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();

    const [[url, init]] = calls('PUT');
    expect(url).toBe('/api/admin/v1/organizations/org-1/catalog/credentials/bedrock-us');
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ role_arn: 'arn:aws:iam::123456789012:role/gateway' });
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('*');
    expect(document.getElementById('credential-role_arn')).toBeNull();
  });

  it('replaces a stored key against its version, reusing the key on retry', async () => {
    await openTab('Credentials');
    act(() => button('Replace').click());
    const input = type('credential-api_key', 'sk-new');
    expect(input.type).toBe('password');
    fetchMock.mockImplementationOnce(async () => { throw new TypeError('network error'); });
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();

    const puts = calls('PUT').map(([, init]) => new Headers((init as RequestInit).headers));
    expect(puts).toHaveLength(2);
    expect(puts[0].get('If-Match')).toBe('"v3"');
    expect(puts[1].get('Idempotency-Key')).toBe(puts[0].get('Idempotency-Key'));
  });

  it('removes a stored credential against its version', async () => {
    await openTab('Credentials');
    act(() => button('Remove').click());
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Remove') as HTMLButtonElement).click());
    await settle();
    const [[url, init]] = calls('DELETE');
    expect(url).toBe('/api/admin/v1/organizations/org-1/catalog/credentials/openai-main');
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v3"');
  });

  it('warns when a stored credential was sealed for another provider family', async () => {
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) =>
      url.includes('/credentials') ? json({ items: [{ ...stored, adapter_instance_key: 'bedrock-us', family: 'openai' }], next_page_token: null }) : listing(url, init));
    await openTab('Credentials');
    expect(container.textContent).toContain('Stored for OpenAI: requests to this account fail until you replace it.');
    // It can still be replaced or removed.
    expect(button('Replace')).toBeTruthy();
    expect(button('Remove')).toBeTruthy();
  });

  it('refuses a stale activation and offers to reload', async () => {
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) =>
      init?.method === 'POST' ? json({ type: '', code: 'version_conflict', title: '', status: 409, detail: '', instance: '', request_id: '' }, 409) : listing(url, init));
    await openTab('Releases');
    act(() => button('Activate').click());
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Activate') as HTMLButtonElement).click());
    await settle();
    expect(document.body.textContent).toContain('Another release was activated since this list loaded.');
    act(() => button('Reload releases').click());
    await settle();
    expect(fetchMock.mock.calls.filter(([url]) => String(url).includes('/catalog/releases?')).length).toBe(2);
  });

  it('names the price override that blocks an activation', async () => {
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) =>
      init?.method === 'POST'
        ? json({ type: '', code: 'price_override_incompatible', title: '', status: 409, detail: 'price override 7d0c… sets a cached input price', instance: '', request_id: '' }, 409)
        : listing(url, init));
    await openTab('Releases');
    act(() => button('Activate').click());
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Activate') as HTMLButtonElement).click());
    await settle();
    const alert = document.querySelector('.dialog [role="alert"]:not([class*="counter-alert"])')!.textContent;
    expect(alert).toContain('price override 7d0c… sets a cached input price');
    expect(alert).toContain('Revoke that override under Pricing');
  });

  it('activates the first release only if nothing was activated since', async () => {
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (!init?.method && url.includes('/releases')) {
        return json({ items: [{ id: 'first', version: '2026.10.1', digest: 'b'.repeat(64), created_at: '2026-10-01T00:00:00Z', sealed_at: '2026-10-01T00:00:00Z', active: false, activated_at: null }], next_page_token: null, activation_version: 0 });
      }
      return listing(url, init);
    });
    await openTab('Releases');
    act(() => button('Activate').click());
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Activate') as HTMLButtonElement).click());
    await settle();
    const [[, init]] = calls('POST');
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v0"');
  });

  it('activates an older release', async () => {
    await openTab('Releases');
    act(() => button('Activate').click());
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Activate') as HTMLButtonElement).click());
    await settle();
    const [[url, init]] = calls('POST');
    expect(url).toBe('/api/admin/v1/organizations/org-1/catalog/releases/old/activate');
    const headers = new Headers((init as RequestInit).headers);
    // The activation version listed with the releases, so a newer switch conflicts.
    expect(headers.get('If-Match')).toBe('"v2"');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
  });

  const created = () => {
    const [[, init]] = fetchMock.mock.calls.filter(([url, request]) => String(url).endsWith('/catalog/releases') && (request as RequestInit | undefined)?.method === 'POST');
    return JSON.parse(String((init as RequestInit).body));
  };
  const activations = () => fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/activate'));

  it('edits a deployment’s limits and prices in a new release', async () => {
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    act(() => (document.querySelector('[aria-label="Edit gpt-5-prod"]') as HTMLButtonElement).click());
    await settle();
    expect((document.getElementById('edit-chat-input') as HTMLInputElement).value).toBe('2.5');
    type('edit-chat-input', '2');
    type('edit-chat-max-output', '64');
    // A stored chat_stream is folded into chat: there is nothing apart to edit.
    expect(document.getElementById('edit-chat_stream-ttl')).toBeNull();
    type('edit-chat-ttl', '0');
    expect(container.textContent).toContain('Reservation time is 1 to 86400 seconds.');
    expect(button('Save and activate').disabled).toBe(true);
    type('edit-chat-ttl', '300');
    act(() => button('Save and activate').click());
    await settle();
    const body = created();
    expect(body.deployments[0].capabilities).toEqual([
      { operation: 'chat', ...capability, max_output_tokens: 64, reservation_ttl_seconds: 300 },
    ]);
    expect(body.prices.some((price: { operation: string }) => price.operation === 'chat_stream')).toBe(false);
    expect(body.routes.some((route: { operation: string }) => route.operation === 'chat_stream')).toBe(false);
    expect(body.prices[1]).toMatchObject({ deployment: 'gpt-5-prod', operation: 'chat', input_minor_per_token: 200, source: 'admin', confidence: 'known' });
    // Unchanged prices keep their source.
    expect(body.prices[0]).toEqual(manifest.prices[0]);
    const [[url, init]] = activations();
    expect(url).toBe('/api/admin/v1/organizations/org-1/catalog/releases/new/activate');
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v2"');
    expect(container.textContent).toMatch(/Updated gpt-5; release .* ~ gpt-5-prod is active\./);
  });

  it('warns which aliases stop working before removing a deployment', async () => {
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    act(() => (document.querySelector('[aria-label="Remove gpt-5-prod"]') as HTMLButtonElement).click());
    await settle();
    expect(document.querySelector('.dialog')?.textContent).toContain('These requests will fail, as no other deployment serves them: chat-default (chat).');
    act(() => button('Remove and activate').click());
    await settle();
    const body = created();
    expect(body.deployments.map((item: { name: string }) => item.name)).toEqual(['claude-us']);
    expect(body.routes).toEqual([{ alias: 'claude', operation: 'chat', deployment: 'claude-us' }]);
    expect(activations()).toHaveLength(1);
    expect(container.textContent).toContain('Removed gpt-5');
  });

  it('shows batch prices in a column of their own, not in the input and output columns', async () => {
    const batched = { ...deployment, operations: [deployment.operations[0], { operation: 'chat_batch', capability: { id: 'b', max_output_tokens: 1 }, price: { ...price, input_minor_per_token: 125, output_minor_per_token: 500 } }] };
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes('/deployments')) return json({ release_id: RELEASE, items: [batched], next_page_token: null });
      if (url.includes('/routes')) return json({ release_id: RELEASE, items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    });
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    // A sortable header's text ends with its label.
    const headers = Array.from(container.querySelectorAll('thead th')).map(node => (node.textContent ?? '').replace(/^Click to sort rows by .* header in \w+ order/, ''));
    expect(headers).toContain('Batch per 1M');
    const cells = Array.from(container.querySelector('tbody tr')!.querySelectorAll('td')).map(node => node.textContent ?? '');
    const at = (label: string) => cells[headers.indexOf(label)];
    // Single requests' input and output stay as they were.
    expect(at('Input per 1M')).toBe('$2.50');
    expect(at('Output per 1M')).toBe('$10.00');
    expect(at('Batch per 1M')).toBe('$1.25 in · $5.00 out');
  });

  it('filters output price by what the column shows, leaving embeddings out', async () => {
    const embedded = { ...deployment, operations: [deployment.operations[0], { operation: 'embeddings', capability: { id: 'e', max_output_tokens: 1 }, price: { ...price, output_minor_per_token: 0 } }] };
    fetchMock.mockImplementation(async (url: string) => {
      if (url.includes('/deployments')) return json({ release_id: RELEASE, items: [embedded], next_page_token: null });
      if (url.includes('/routes')) return json({ release_id: RELEASE, items: [], next_page_token: null });
      return json({ items: [], next_page_token: null });
    });
    history.replaceState(null, '', '/admin/models/models?deployments.r.output=1..');
    act(() => root.render(<ProvidersScreen organizationId="org-1" />));
    await settle();
    // Chat output is $10 per 1M: above $1, so the row stays.
    expect(container.textContent).toContain('gpt-5');
    expect(container.textContent).toContain('1 row');
    history.replaceState(null, '', '/');
  });

  it('lists every deployment on Models, and the master records on a page of their own', async () => {
    await openTab('Models');
    expect(container.textContent).toContain('Deployments');
    expect(container.textContent).not.toContain('Models across providers');
    const masters = () => fetchMock.mock.calls.filter(([url]) => String(url).endsWith('/catalog/model-masters'));
    expect(masters()).toHaveLength(0);
    await openTab('Across providers');
    expect(container.querySelector('#model-masters')?.textContent).toBe('Models across providers');
    expect(container.textContent).not.toContain('Models in the active catalog release');
    expect(masters().length).toBeGreaterThan(0);
  });

  it('says so on Across providers when no release is active', async () => {
    fetchMock.mockImplementation(async () => json({ release_id: null, items: [], next_page_token: null }));
    await openTab('Across providers');
    expect(container.textContent).toContain('No models yet');
  });

  it('lists the aliases of the active release on their own tab', async () => {
    await openTab('Aliases');
    expect(container.querySelector('[aria-label="Model aliases"]')?.textContent).toContain('chat-default');
    expect(container.textContent).not.toContain('Deployments');
    // Each deployment's health over the last 15 minutes, from usage.
    expect(container.querySelector('[aria-label="Model aliases"]')?.textContent).toContain('Failing · 4/8');
    expect(container.textContent).toContain('Health as of');
    const urls = fetchMock.mock.calls.map(([url]) => String(url));
    expect(urls).toContain(`/api/admin/v1/organizations/org-1/catalog/routes?release_id=${RELEASE}&page_size=100`);
  });

  it('says so on the Aliases tab when no release is active', async () => {
    fetchMock.mockImplementation(async () => json({ release_id: null, items: [], next_page_token: null }));
    await openTab('Aliases');
    expect(container.textContent).toContain('No models yet');
  });

  it('removes a provider account with its deployments, keeping its credential', async () => {
    await openTab('Credentials');
    act(() => (document.querySelector('[aria-label="Remove bedrock-us from the catalog"]') as HTMLButtonElement).click());
    await settle();
    expect(document.querySelector('.dialog')?.textContent).toContain('claude (chat)');
    act(() => button('Remove and activate').click());
    await settle();
    const body = created();
    expect(body.providers).toEqual([{ adapter_instance_key: 'openai-main', family: 'openai' }]);
    expect(body.deployments.map((item: { name: string }) => item.name)).toEqual(['gpt-5-prod']);
    expect(calls('DELETE')).toHaveLength(0);
    expect(container.textContent).toContain('Removed bedrock-us');
  });
});
