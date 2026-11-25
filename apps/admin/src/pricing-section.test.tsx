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
const price = {
  id: 'p-1', currency: 'USD', input_minor_per_token: 250, output_minor_per_token: 1000, cached_input_minor_per_token: 0,
  reasoning_minor_per_token: 0, other_minor_per_token: 0, source: 'admin', source_version: '1', confidence: 'known',
};
const capability = { id: 'c-1', max_output_tokens: 4096 };
const azure = {
  id: 'd-1', provider_account_id: 'a-1', adapter_instance_key: 'azure-1', family: 'azure-openai', name: 'azure-1-gpt-4o', model: 'gpt-4o',
  location: 'eastus', operations: [{ operation: 'chat', capability, price }],
};
const openai = { ...azure, id: 'd-2', adapter_instance_key: 'openai-1', family: 'openai', name: 'openai-1-gpt-4o', location: null };
const sources = [
  {
    source: 'azure-retail:azure-openai', kind: 'azure-retail', provider_family: 'azure-openai', last_attempt_at: '2026-10-13T06:00:00Z',
    last_success_at: '2026-10-13T06:00:05Z', last_error_code: null, next_fetch_at: '2026-10-14T06:00:00Z',
    snapshot: { id: 's-1', source_version: null, fetched_at: '2026-10-10T06:00:00Z', confirmed_at: '2026-10-13T06:00:05Z', observations: 1200, unmapped: 1200 },
  },
  {
    source: 'price-sheet:openai', kind: 'price-sheet', provider_family: 'openai', last_attempt_at: '2026-10-13T06:00:00Z',
    last_success_at: null, last_error_code: 'http_status', next_fetch_at: '2026-10-13T06:15:00Z', snapshot: null,
  },
];
const mapped = {
  id: 'm-1', deployment: 'openai-1-gpt-4o', source: 'price-sheet:openai', item: 'gpt-4o-2024-08-06', region: null, version: 2,
  created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z',
};
const observation = {
  source: 'azure-retail:azure-openai', snapshot_id: 's-1', item: 'gpt-4o-0806-Inp-glbl', model: null, region: 'eastus', currency: 'USD',
  per_tokens: 1000000, input: '2.5', output: '10', cached_input: null, reasoning: null, other: null,
};
const active = {
  id: 'o-1', deployment: 'azure-1-gpt-4o', operation: 'chat', input_minor_per_token: 200, output_minor_per_token: 800, cached_input_minor_per_token: 0,
  reasoning_minor_per_token: 0, other_minor_per_token: 0, currency: 'USD', confidence: 'known', price_version: 'override:o-1', reason: 'Negotiated rate',
  effective_from: '2026-10-01T00:00:00Z', effective_until: null, status: 'active', created_by: 'u-1', created_at: '2026-10-01T00:00:00Z',
  revoked_at: null, revoked_by: null, revoke_reason: null, version: 3,
};
const revoked = { ...active, id: 'o-0', status: 'revoked', revoked_at: '2026-09-30T00:00:00Z', revoke_reason: 'Ended', version: 2 };

type Handler = (url: string, init?: RequestInit) => Response | Promise<Response> | undefined;

describe('pricing tab', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let overrides: unknown[];
  let sourceItems: unknown[];
  let override: Handler;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    history.replaceState(null, '', '/');
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    overrides = [active, revoked];
    sourceItems = sources;
    override = () => undefined;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const custom = await override(url, init);
      if (custom) return custom;
      const method = init?.method ?? 'GET';
      if (url.startsWith(`${CATALOG}/price-sources`)) return json({ items: sourceItems });
      if (url.startsWith(`${CATALOG}/price-policy`)) {
        if (method === 'PUT') return json({ ...JSON.parse(String(init!.body)), version: 1, updated_at: '2026-11-13T00:00:00Z' }, 201);
        return json({ auto_adopt: true, version: 0, updated_at: null });
      }
      if (url.startsWith(`${CATALOG}/deployments`)) return json({ items: [azure, openai], next_page_token: null, release_id: 'r-1' });
      if (url.startsWith(`${CATALOG}/price-mappings`)) {
        if (method === 'PUT') return json({ ...mapped, id: 'm-2', deployment: 'azure-1-gpt-4o', source: 'azure-retail:azure-openai', ...JSON.parse(String(init!.body)), version: 1 }, 201);
        if (method === 'DELETE') return new Response(null, { status: 204 });
        return json({ items: [mapped], next_page_token: null });
      }
      if (url.startsWith(`${CATALOG}/price-observations`)) return json({ items: [observation], next_page_token: null });
      if (url.startsWith(`${CATALOG}/price-overrides`)) {
        if (url.endsWith('/revoke')) return json({ ...active, status: 'revoked', revoke_reason: JSON.parse(String(init!.body)).reason, version: 4 });
        if (method === 'POST') return json({ ...active, id: 'o-2', ...JSON.parse(String(init!.body)), status: 'active', version: 1 }, 201);
        return json({ items: overrides, next_page_token: null });
      }
      if (url.startsWith(`${CATALOG}/price-drift`)) return json({ release_id: 'r-1', prices: [] });
      return json({ items: [], next_page_token: null, release_id: 'r-1' });
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement | undefined;
  const labelled = (label: string) => document.querySelector(`[aria-label="${label}"]`) as HTMLButtonElement | null;
  const click = async (target: HTMLElement | null | undefined) => {
    if (!target) throw new Error('no such control');
    act(() => target.click());
    await settle();
  };
  const dialogButton = (text: string) => Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === text) as HTMLButtonElement;
  const show = async (view: string) => click(Array.from(document.querySelectorAll<HTMLButtonElement>('[aria-label="Pricing views"] button')).find(node => node.textContent === view));
  /** Opens Pricing, and then one of its views when named. */
  const open = async (role = 'Owner', view?: 'Mappings' | 'Overrides') => {
    act(() => root.render(<ProvidersScreen organizationId="org-1" role={role} />));
    await settle();
    openSection('models', 'pricing');
    await settle();
    if (view) await show(view);
  };
  const type = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement | HTMLTextAreaElement;
    const proto = input instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    act(() => {
      Object.getOwnPropertyDescriptor(proto, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const choose = (id: string, value: string) => {
    const select = document.getElementById(id) as HTMLSelectElement;
    act(() => {
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
  };
  const calls = (method: string, fragment: string) =>
    fetchMock.mock.calls.filter(([url, init]) => ((init as RequestInit | undefined)?.method ?? 'GET') === method && String(url).includes(fragment));
  const headers = (call: unknown[]) => new Headers((call[1] as RequestInit).headers);
  const body = (call: unknown[]) => JSON.parse(String((call[1] as RequestInit).body));

  it('lists each price source by name, when it was fetched and how it is doing, without an address', async () => {
    await open();
    const table = container.querySelector('[aria-label="Price sources"]')!;
    expect(table.textContent).toContain('azure-retail:azure-openai');
    expect(table.textContent).toContain('OK');
    expect(table.textContent).toContain(new Date('2026-10-13T06:00:05Z').toLocaleString());
    expect(table.textContent).toContain('price-sheet:openai');
    expect(table.textContent).toContain('Failing');
    expect(table.textContent).toContain('Never');
    expect(container.textContent).not.toMatch(/https?:\/\//);
  });

  it('adopts published prices automatically unless an admin turns it off', async () => {
    await open();
    const setting = container.querySelector('.pricing-auto .check-option input[type="checkbox"]') as HTMLInputElement;
    expect(container.textContent).toContain('Automatic price updates');
    expect(setting.checked).toBe(true);
    await click(setting);
    const [put] = calls('PUT', '/price-policy');
    expect(body(put)).toEqual({ auto_adopt: false });
    expect(headers(put).get('If-Match')).toBe('*');
    expect(headers(put).get('Idempotency-Key')).toBeTruthy();
    expect((container.querySelector('.pricing-auto .check-option input[type="checkbox"]') as HTMLInputElement).checked).toBe(false);
    expect(container.textContent).toContain('an admin adopts each one');
  });

  it('fetches a price source now and shows when it is next fetched', async () => {
    const due = { ...sources[1], next_fetch_at: '2026-10-13T07:00:00Z' };
    override = (url, init) => (init?.method === 'POST' && url.endsWith('/fetch') ? json(due, 202) : undefined);
    await open();
    await click(labelled('Fetch price-sheet:openai now'));
    const [post] = calls('POST', '/price-sources/');
    expect(post[0]).toBe(`${CATALOG}/price-sources/price-sheet%3Aopenai/fetch`);
    expect(headers(post).get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('price-sheet:openai will be fetched within a minute');
    expect(container.querySelector('[aria-label="Price sources"]')!.textContent).toContain(new Date('2026-10-13T07:00:00Z').toLocaleString());
  });

  it('explains a fetch refused because the source was just fetched', async () => {
    override = (url, init) => (init?.method === 'POST' && url.endsWith('/fetch') ? problem(429, 'price_fetch_too_soon') : undefined);
    await open();
    await click(labelled('Fetch azure-retail:azure-openai now'));
    expect(container.querySelector('[role="alert"]:not([class*="counter-alert"])')!.textContent).toContain('fetched in the last 10 minutes');
  });

  it('shows what each active deployment is compared with at each source of its family', async () => {
    await open('Owner', 'Mappings');
    expect(container.textContent).toContain('needs to be told which list item matches each deployment');
    const rows = Array.from(container.querySelectorAll('[aria-label="Price mappings"] .data-row:not(.head)'));
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain('azure-retail:azure-openai');
    expect(rows[0].textContent).toContain('Not mapped: matched by model');
    expect(rows[1].textContent).toContain('price-sheet:openai');
    expect(rows[1].textContent).toContain('gpt-4o-2024-08-06');
  });

  it('maps a deployment to a published price found by search', async () => {
    await open('Owner', 'Mappings');
    await click(labelled('Map azure-1-gpt-4o at azure-retail:azure-openai'));
    type('mapping-search', 'gpt-4o-0806');
    act(() => (document.getElementById('mapping-search') as HTMLInputElement).form!.requestSubmit());
    await settle();
    const [search] = calls('GET', '/price-observations');
    expect(String(search[0])).toContain('q=gpt-4o-0806');
    expect(String(search[0])).toContain('source=azure-retail%3Aazure-openai');
    expect(document.querySelector('.dialog')!.textContent).toContain('in 2.5 USD · out 10 USD');
    await click(document.getElementById('price-result-0'));
    await click(dialogButton('Save mapping'));
    const [put] = calls('PUT', '/price-mappings');
    expect(put[0]).toBe(`${CATALOG}/price-mappings/azure-1-gpt-4o/azure-retail%3Aazure-openai`);
    expect(body(put)).toEqual({ item: 'gpt-4o-0806-Inp-glbl', region: 'eastus' });
    expect(headers(put).get('If-Match')).toBe('*');
    expect(headers(put).get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('azure-1-gpt-4o is now compared with gpt-4o-0806-Inp-glbl');
  });

  it('gives each published price its own choice when an item is published in several currencies', async () => {
    override = url => (url.includes('/price-observations') ? json({ items: [observation, { ...observation, currency: 'EUR' }], next_page_token: null }) : undefined);
    await open('Owner', 'Mappings');
    await click(labelled('Map azure-1-gpt-4o at azure-retail:azure-openai'));
    act(() => (document.getElementById('mapping-search') as HTMLInputElement).form!.requestSubmit());
    await settle();
    const radios = Array.from(document.querySelectorAll<HTMLInputElement>('.dialog input[name="mapping-item"]'));
    expect(new Set(radios.map(radio => radio.id)).size).toBe(2);
    await click(radios[1]);
    expect(radios.map(radio => radio.checked)).toEqual([false, true]);
    expect(document.querySelector(`label[for="${radios[1].id}"]`)!.textContent).toContain('EUR');
  });

  it('replaces a mapping against its ETag, and explains a conflict', async () => {
    override = (url, init) => (init?.method === 'PUT' ? problem(409, 'version_conflict', 'resource version conflict; current version is 3') : undefined);
    await open('Owner', 'Mappings');
    await click(labelled('Change the mapping of openai-1-gpt-4o at price-sheet:openai'));
    expect((document.getElementById('mapping-item') as HTMLInputElement).value).toBe('gpt-4o-2024-08-06');
    type('mapping-item', 'gpt-4o');
    await click(dialogButton('Save mapping'));
    const [put] = calls('PUT', '/price-mappings');
    expect(headers(put).get('If-Match')).toBe('"v2"');
    expect(document.querySelector('.dialog')!.textContent).toContain('Someone changed this mapping since the list loaded.');
    expect((document.getElementById('mapping-item') as HTMLInputElement).value).toBe('gpt-4o');
  });

  it('says when the source published new prices during a search', async () => {
    override = url => (url.includes('/price-observations') ? problem(409, 'page_token_stale') : undefined);
    await open('Owner', 'Mappings');
    await click(labelled('Map azure-1-gpt-4o at azure-retail:azure-openai'));
    act(() => (document.getElementById('mapping-search') as HTMLInputElement).form!.requestSubmit());
    await settle();
    expect(document.querySelector('.dialog')!.textContent).toContain('The source published new prices while you were looking.');
  });

  it('unmaps only after confirming, with the mapping’s ETag', async () => {
    await open('Owner', 'Mappings');
    await click(labelled('Unmap openai-1-gpt-4o at price-sheet:openai'));
    expect(document.querySelector('[role="alertdialog"]')!.textContent).toContain('Drift goes back to matching its model');
    expect(calls('DELETE', '/price-mappings')).toHaveLength(0);
    await click(dialogButton('Unmap'));
    const [del] = calls('DELETE', '/price-mappings');
    expect(del[0]).toBe(`${CATALOG}/price-mappings/openai-1-gpt-4o/price-sheet%3Aopenai`);
    expect(headers(del).get('If-Match')).toBe('"v2"');
    expect(container.textContent).toContain('openai-1-gpt-4o is matched by model again');
  });

  it('shows one view at a time, sources first, and keeps the chosen view in the URL', async () => {
    await open();
    expect(container.querySelector('[aria-label="Price sources"]')).not.toBeNull();
    expect(container.querySelector('[aria-label="Price overrides"]')).toBeNull();
    await show('Overrides');
    expect(container.querySelector('[aria-label="Price sources"]')).toBeNull();
    expect(container.querySelector('[aria-label="Price overrides"]')).not.toBeNull();
    expect(new URLSearchParams(location.search).get('pricing.view')).toBe('overrides');
    act(() => root.unmount());
    root = createRoot(container);
    await open();
    expect(container.querySelector('[aria-label="Price overrides"]')).not.toBeNull();
    expect(container.querySelector('[aria-label="Pricing views"] [aria-selected="true"]')!.textContent).toBe('Overrides');
  });

  it('lists overrides with their status, offering Revoke only while one applies', async () => {
    await open('Owner', 'Overrides');
    const rows = Array.from(container.querySelectorAll('[aria-label="Price overrides"] .data-row:not(.head)'));
    expect(rows[0].textContent).toContain('in $2.00 · out $8.00');
    expect(rows[0].textContent).toContain('active');
    expect(rows[0].textContent).toContain('Revoke');
    expect(rows[1].textContent).toContain('revoked');
    expect(rows[1].textContent).toContain('Ended');
    expect(rows[1].textContent).not.toContain('Revoke');
  });

  it('filters overrides by status from the URL and expands one to every price', async () => {
    // The revoked override is on a second page: the table must load every page.
    override = url => {
      if (!url.startsWith(`${CATALOG}/price-overrides`) || url.includes('/revoke')) return undefined;
      return url.includes('page_token=p2')
        ? json({ items: [revoked], next_page_token: null })
        : json({ items: [active], next_page_token: 'p2' });
    };
    history.replaceState(null, '', '/admin/providers?price-overrides.f.status=revoked');
    await open();
    expect(calls('GET', 'price-overrides?page_size=100&page_token=p2')).toHaveLength(1);
    const table = () => container.querySelector('table[aria-label="Price overrides"]')!;
    expect(table().querySelectorAll('.data-row:not(.head)')).toHaveLength(1);
    expect(table().textContent).toContain('revoked');
    expect(table().textContent).not.toContain('active');
    await click(table().querySelector<HTMLButtonElement>('button.cds--table-expand__button'));
    const detail = container.querySelector('[role="region"]')!;
    expect(detail.textContent).toContain('cached input');
    expect(detail.textContent).toContain('reasoning = output');
    expect(detail.textContent).toContain('Revoked');
    expect(detail.textContent).toContain('Ended');
    history.replaceState(null, '', '/admin/providers');
  });

  it('adds an override from prices per million tokens and a reason', async () => {
    await open('Owner', 'Overrides');
    await click(button('Add override'));
    choose('override-deployment', 'azure-1-gpt-4o');
    type('override-input', '2');
    type('override-output', '8.5');
    type('override-reason', 'Negotiated rate for Q4');
    await click(dialogButton('Add override'));
    const [post] = calls('POST', '/price-overrides');
    expect(post[0]).toBe(`${CATALOG}/price-overrides`);
    expect(body(post)).toEqual({
      deployment: 'azure-1-gpt-4o', operation: 'chat', input_minor_per_token: 200, output_minor_per_token: 850,
      currency: 'USD', confidence: 'known', reason: 'Negotiated rate for Q4',
    });
    expect(headers(post).get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('Override added for azure-1-gpt-4o');
  });

  it('requires a reason before adding an override', async () => {
    await open('Owner', 'Overrides');
    await click(button('Add override'));
    choose('override-deployment', 'azure-1-gpt-4o');
    type('override-input', '2');
    type('override-output', '8');
    await click(dialogButton('Add override'));
    expect(calls('POST', '/price-overrides')).toHaveLength(0);
    expect(document.querySelector('.dialog')!.textContent).toContain('Say why');
  });

  it('keeps the override form and explains a 403', async () => {
    override = (url, init) => (init?.method === 'POST' && url.endsWith('/price-overrides') ? problem(403, 'forbidden', 'administrative permission denied') : undefined);
    await open('Owner', 'Overrides');
    await click(button('Add override'));
    choose('override-deployment', 'azure-1-gpt-4o');
    type('override-input', '2');
    type('override-output', '8');
    type('override-reason', 'Negotiated');
    await click(dialogButton('Add override'));
    expect(document.querySelector('.dialog [role="alert"]:not([class*="counter-alert"])')!.textContent).toContain('Only organization owners and admins can add price overrides.');
    expect((document.getElementById('override-reason') as HTMLTextAreaElement).value).toBe('Negotiated');
  });

  it('shows the server’s detail when an override is refused', async () => {
    override = (url, init) => (init?.method === 'POST' && url.endsWith('/price-overrides')
      ? problem(422, 'invalid_price_override', 'cached_input price must be zero for anthropic') : undefined);
    await open('Owner', 'Overrides');
    await click(button('Add override'));
    choose('override-deployment', 'azure-1-gpt-4o');
    type('override-input', '2');
    type('override-output', '8');
    type('override-reason', 'Negotiated');
    await click(dialogButton('Add override'));
    expect(document.querySelector('.dialog')!.textContent).toContain('cached_input price must be zero for anthropic');
  });

  it('revokes an override with a reason, against its ETag', async () => {
    await open('Owner', 'Overrides');
    await click(labelled('Revoke the override of azure-1-gpt-4o Chat'));
    expect(document.querySelector('[role="alertdialog"]')!.textContent).toContain('Another override may still set the price; otherwise the catalog price applies.');
    await click(dialogButton('Revoke'));
    expect(calls('POST', '/revoke')).toHaveLength(0);
    expect(document.querySelector('.dialog')!.textContent).toContain('Say why it is revoked.');
    type('revoke-reason', 'Contract ended');
    await click(dialogButton('Revoke'));
    const [revoke] = calls('POST', '/revoke');
    expect(revoke[0]).toBe(`${CATALOG}/price-overrides/o-1/revoke`);
    expect(body(revoke)).toEqual({ reason: 'Contract ended' });
    expect(headers(revoke).get('If-Match')).toBe('"v3"');
    expect(container.textContent).toContain('Override for azure-1-gpt-4o revoked.');
  });

  it('shows the server’s detail for a 409 on revoke', async () => {
    override = url => (url.endsWith('/revoke') ? problem(409, 'already_revoked', 'the override was already revoked') : undefined);
    await open('Owner', 'Overrides');
    await click(labelled('Revoke the override of azure-1-gpt-4o Chat'));
    type('revoke-reason', 'Contract ended');
    await click(dialogButton('Revoke'));
    expect(document.querySelector('.dialog [role="alert"]:not([class*="counter-alert"])')!.textContent).toContain('the override was already revoked');
    expect(dialogButton('Reload the overrides')).toBeTruthy();
  });

  it('lets auditors and read-only members read, without change controls', async () => {
    for (const role of ['Auditor', 'ReadOnly']) {
      history.replaceState(null, '', '/');
      await open(role);
      expect(button('Fetch now')).toBeUndefined();
      expect((container.querySelector('.pricing-auto .check-option input[type="checkbox"]') as HTMLInputElement).disabled).toBe(true);
      await show('Mappings');
      expect(container.textContent).toContain('gpt-4o-2024-08-06');
      await show('Overrides');
      expect(container.textContent).toContain('Negotiated rate');
      for (const text of ['Add override', 'Map', 'Change', 'Unmap', 'Revoke', 'Fetch now']) expect(button(text)).toBeUndefined();
    }
  });

  it('says when there are no sources and no overrides', async () => {
    sourceItems = [];
    overrides = [];
    await open();
    expect(container.textContent).toContain('No price sources');
    await show('Mappings');
    expect(container.textContent).toContain('Nothing to map');
    await show('Overrides');
    expect(container.textContent).toContain('No price overrides');
  });

  it('explains a 403 when reading overrides', async () => {
    override = (url, init) => (!init?.method && url.startsWith(`${CATALOG}/price-overrides`) ? problem(403, 'forbidden') : undefined);
    await open('Owner', 'Overrides');
    expect(container.textContent).toContain('You do not have permission to view this organization’s price overrides.');
  });
});
