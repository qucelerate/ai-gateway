import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { IdentitiesScreen } from './identities-screen';

const ctx = { user_id: 'u-1', organization_id: 'org-1', team_id: null, role: 'organization_owner' };
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const account = {
  id: 'sa-1', organization_id: 'org-1', team_id: null, owner_user_id: 'u-1', name: 'used',
  enabled: true, created_at: '2026-10-01T00:00:00Z', version: 3,
};

const key = {
  id: 'k-1', organization_id: 'org-1', service_account_id: 'sa-1', successor_of: null, public_prefix: 'gw_live_ab',
  scopes: ['Inference'], status: 'active', expires_at: null, revoked_at: null, disabled_at: null,
  last_used_at: '2026-10-20T00:00:00Z', created_at: '2026-10-01T00:00:00Z', version: 1,
};
let keys: unknown[] = [];
let accounts: (typeof account)[] = [];
let budgets: unknown[] = [];
let rateLimits: unknown[] = [];
let deployments: unknown[] = [];
const priced = (currency: string) => ({
  id: `d-${currency}`, provider_account_id: 'pa', adapter_instance_key: 'aws-main', family: 'bedrock', name: 'm', model: 'm', location: null,
  operations: [{ operation: 'chat', capability: null, price: { id: 'p', currency, input_minor_per_token: 1, output_minor_per_token: 1, cached_input_minor_per_token: 0, reasoning_minor_per_token: 0, other_minor_per_token: 0, source: 's', source_version: '1', confidence: 'known' } }],
});
const orgBudget = (currency: string) => ({ id: `b-${currency}`, scope: 'organization', team_id: null, service_account_id: null, currency, limit_minor: 1, period: 'month', starts_at: null, ends_at: null, version: 1 });

describe('identities screen', () => {
  let container: HTMLDivElement;
  let root: Root;
  let current = account;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    current = account;
    accounts = [account, { ...account, id: 'sa-9', name: 'retired', enabled: false }];
    history.replaceState(null, '', '/admin/identities/service-accounts?account=sa-1');
    vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
      if (url.includes('/keys') && init?.method === 'POST') return json({ metadata: key, secret: 'gw_live_secret', replay: false }, 201);
      if (url.includes('/keys')) {
        const usable = new URL(url, 'http://x').searchParams.get('usable') === 'true';
        return json({ items: usable ? keys.filter(item => (item as typeof key).status === 'active') : keys, next_page_token: null });
      }
      if (url.includes('/budgets')) return json({ items: budgets, next_page_token: null });
      if (url.includes('/rate-limits')) return json({ items: rateLimits, next_page_token: null });
      if (url.includes('/catalog/deployments')) return json({ items: deployments, next_page_token: null, release_id: 'r-1' });
      if (init?.method === 'DELETE') {
        current = { ...account, enabled: false, version: 4 };
        return json({ deleted: false, disabled: true, reason: 'usage_history', service_account: current });
      }
      if (url.includes('/end-user-settings')) return json({ type: 'about:blank', code: 'not_found', title: 'Not found', status: 404, detail: 'none', instance: '', request_id: 'r' }, 404);
      if (url.includes('/end-user')) return json({ data: [], items: [], next_page_token: null });
      if (url.includes('/service-accounts') && init?.method === 'POST') {
        current = { ...account, id: 'sa-2', name: 'new-app', version: 1 };
        accounts = [...accounts, current];
        return json(current, 201);
      }
      if (/\/service-accounts\/sa-\d$/.test(url)) return json(url.endsWith(current.id) ? current : accounts.find(item => url.endsWith(item.id)));
      if (url.includes('/service-accounts')) {
        const enabled = new URL(url, 'http://x').searchParams.get('enabled');
        const listed = accounts.map(item => (item.id === current.id ? current : item));
        return json({ items: enabled === null ? listed : listed.filter(item => String(item.enabled) === enabled), next_page_token: null });
      }
      return json({}, 404);
    }));
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const buttons = (text: string) =>
    Array.from(document.querySelectorAll('button')).filter(node => node.textContent === text) as HTMLButtonElement[];

  it('filters an account’s keys by status and expands one to its lifecycle', async () => {
    keys = [key, { ...key, id: 'k-2', public_prefix: 'gw_live_cd', status: 'revoked', revoked_at: '2026-10-21T00:00:00Z', successor_of: 'k-1', scopes: ['Inference', 'Embeddings'] }];
    history.replaceState(null, '', '/admin/identities/service-accounts?account=sa-1&keys.f.status=revoked');
    act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
    await settle();
    // Only usable keys at first: the revoked one is behind the toggle.
    expect(container.querySelector('table[aria-label="API keys"]')?.textContent).not.toContain('gw_live_cd');
    act(() => (document.getElementById('show-retired-keys') as HTMLButtonElement).click());
    await settle();
    const table = container.querySelector('table[aria-label="API keys"]')!;
    expect(table.textContent).toContain('gw_live_cd');
    expect(table.textContent).not.toContain('gw_live_ab');
    act(() => (container.querySelector('button[aria-label="Expand gw_live_cd"]') as HTMLButtonElement).click());
    const detail = container.querySelector('[role="region"][aria-label="Details of gw_live_cd"]')!;
    expect(detail.textContent).toContain('k-2');
    expect(detail.textContent).toContain('Replacesk-1');
    expect(detail.textContent).toContain('ExpiresNever');
    keys = [];
  });

  it('shows each key’s own rate limit, or that its account’s apply', async () => {
    keys = [key, { ...key, id: 'k-2', public_prefix: 'gw_live_cd' }];
    rateLimits = [{
      id: 'p-1', scope: 'api-key', team_id: null, service_account_id: null, api_key_id: 'k-1', requests_per_minute: 60,
      tokens_per_minute: 50_000, max_concurrent: 2, version: 1, created_at: 'x', updated_at: 'x',
    }];
    act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
    await settle();
    const table = container.querySelector('table[aria-label="API keys"]')!;
    expect(table.textContent).toContain('Own rate limit');
    expect(table.textContent).toContain('60 req/min · 50,000 tok/min · 2 in flight');
    expect(table.textContent).toContain('Service account’s limits');
    act(() => (container.querySelector('button[aria-label="Expand gw_live_ab"]') as HTMLButtonElement).click());
    const detail = container.querySelector('[role="region"][aria-label="Details of gw_live_ab"]')!;
    expect(detail.textContent).toContain('Own rate limit60 req/min · 50,000 tok/min · 2 in flight Change');
    keys = [];
    rateLimits = [];
  });

  it('lists enabled service accounts, with disabled ones behind a toggle', async () => {
    history.replaceState(null, '', '/admin/identities/service-accounts');
    const calls = () => (fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.map(([url]) => String(url)).filter(url => url.includes('/service-accounts?'));
    act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
    await settle();
    const table = () => container.querySelector('table[aria-label="Service accounts"]')!;
    expect(calls().at(-1)).toContain('enabled=true');
    expect(table().textContent).toContain('used');
    expect(table().textContent).not.toContain('retired');
    act(() => (document.getElementById('show-disabled-accounts') as HTMLButtonElement).click());
    await settle();
    expect(calls().at(-1)).not.toContain('enabled=');
    expect(table().textContent).toContain('retired');
    // An account's name opens its page, in the address.
    act(() => (Array.from(table().querySelectorAll('a')).find(link => link.textContent === 'used') as HTMLAnchorElement).click());
    await settle();
    expect(location.search).toBe('?account=sa-1');
    expect(container.querySelector('h3')?.textContent).toBe('used');
    expect(container.querySelector('.detail-header')?.textContent).not.toContain('v3');
    act(() => (Array.from(container.querySelectorAll('a')).find(link => link.textContent === '← Service accounts') as HTMLAnchorElement).click());
    await settle();
    expect(location.search).toBe('');
    expect(table().textContent).toContain('used');
  });

  it('lists an app’s end users, the app picked from a list that narrows as you type', async () => {
    history.replaceState(null, '', '/admin/identities/end-users?app=sa-1');
    act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
    await settle();
    const urls = (fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.map(([url]) => String(url));
    // Only enabled accounts are offered.
    expect(urls.some(url => url.includes('/service-accounts?') && url.includes('enabled=true'))).toBe(true);
    expect((document.getElementById('end-users-app') as HTMLInputElement).value).toBe('used');
    expect(urls.some(url => url.includes('/service-accounts/sa-1/end-user'))).toBe(true);
  });

  it('says when the apps could not be loaded, and loads them again', async () => {
    history.replaceState(null, '', '/admin/identities/end-users');
    const fetchMock = fetch as unknown as ReturnType<typeof vi.fn>;
    const answer = fetchMock.getMockImplementation()!;
    fetchMock.mockImplementationOnce(async () => { throw new TypeError('network down'); });
    act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
    await settle();
    expect(container.textContent).not.toContain('Loading apps');
    expect(container.textContent).toContain('The apps could not be loaded.');
    fetchMock.mockImplementation(answer);
    act(() => buttons('Try again')[0].click());
    await settle();
    expect((document.getElementById('end-users-app') as HTMLInputElement).value).toBe('used');
  });

  it('opens a new service account on its page, ready for a key', async () => {
    history.replaceState(null, '', '/admin/identities/service-accounts');
    act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
    await settle();
    act(() => buttons('Create service account')[0].click());
    await settle();
    const input = document.getElementById('account-name') as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'new-app');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => (document.querySelector('.dialog form') as HTMLFormElement).requestSubmit());
    await settle();
    expect(location.search).toBe('?account=sa-2');
    expect(container.querySelector('h3')?.textContent).toBe('new-app');
    expect(buttons('Create key')).toHaveLength(1);
  });

  it('says when a delete disabled an account that has made requests, after the list reloads', async () => {
    act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
    await settle();
    act(() => buttons('Delete')[0].click());
    await settle();
    // The dialog's confirm button comes after the panel's.
    act(() => buttons('Delete')[1].click());
    await settle();

    expect(document.body.textContent).toContain('“used” has made requests, so it was disabled instead of deleted');
    expect(container.querySelector('.detail-header')?.textContent).toContain('disabled');
  });

  it('warns after a key is created when no budget covers its models’ currency', async () => {
    const created = async () => {
      act(() => root.render(<IdentitiesScreen organizationId="org-1" userId="u-1" context={ctx} />));
      await settle();
      act(() => buttons('Create key')[0].click());
      await settle();
      act(() => buttons('Create key')[1].click());
      await settle();
      return container.textContent ?? '';
    };
    deployments = [priced('USD')];
    budgets = [orgBudget('EUR')];
    expect(await created()).toContain('No budget in USD covers used');
    act(() => root.unmount());
    root = createRoot(container);
    budgets = [orgBudget('EUR'), orgBudget('USD')];
    expect(await created()).not.toContain('No budget');
    act(() => root.unmount());
    root = createRoot(container);
    deployments = [];
    budgets = [];
    expect(await created()).toContain('No budget covers used, so every request with this key is refused');
    budgets = [];
    deployments = [];
  });
});
