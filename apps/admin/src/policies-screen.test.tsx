import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { PoliciesScreen, exportRange, savedMatches, scopeLabel, shortens } from './policies-screen';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const TEAM = '22222222-2222-2222-2222-222222222222';
const orgPolicy = {
  id: 'p-1', scope: 'organization', team_id: null, service_account_id: null, api_key_id: null, requests_per_minute: 600, tokens_per_minute: null, max_concurrent: null,
  version: 4, created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-02T00:00:00Z',
};
const orgBudget = {
  id: 'b-1', scope: 'organization', team_id: null, service_account_id: null, currency: 'USD', limit_minor: 10_000_000_000,
  period: 'monthly', starts_at: null, ends_at: null, version: 1, created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z',
  current: { period_start: '2026-10-01T00:00:00Z', period_end: '2026-11-01T00:00:00Z', consumed_minor: 8_000_000_000, reserved_minor: 500_000_000 },
};
let budgetItems: unknown[] = [orgBudget];
const retention = {
  trace_days: 90, usage_days: 365, audit_days: null, version: 2, updated_at: '2026-10-01T00:00:00Z',
  last_purged_at: '2026-10-07T10:00:00Z', purged: { traces: 12, reservations: 3, audit_events: 0 },
};

let tracePolicy = {
  mode: 'off', team_ids: [] as string[], service_account_ids: [] as string[], content: true, content_retention_days: 30, version: 0, updated_at: null as string | null,
};

describe('policy helpers', () => {
  it('names what a policy applies to', () => {
    const names = { teams: new Map([[TEAM, 'Research']]), serviceAccounts: new Map() };
    expect(scopeLabel(orgPolicy as never, names)).toBe('Whole organization');
    expect(scopeLabel({ scope: 'team', team_id: TEAM, service_account_id: null }, names)).toBe('Team · Research');
    expect(scopeLabel({ scope: 'service-account', team_id: null, service_account_id: 'sa-9' }, names)).toBe('Service account · sa-9');
    const withKeys = { ...names, apiKeys: new Map([['key-1', 'gw_live_ab12 (billing)']]) };
    expect(scopeLabel({ scope: 'api-key', team_id: null, service_account_id: null, api_key_id: 'key-1' }, withKeys)).toBe('API key · gw_live_ab12 (billing)');
    expect(scopeLabel({ scope: 'api-key', team_id: null, service_account_id: null, api_key_id: 'key-2' }, withKeys)).toBe('API key · key-2');
  });

  it('knows when a retention change deletes data', () => {
    const current = { trace_days: 90, usage_days: 365, audit_days: null };
    expect(shortens(current, { trace_days: 30, usage_days: 365, audit_days: null })).toBe(true);
    expect(shortens(current, { trace_days: 90, usage_days: 365, audit_days: 365 })).toBe(true);
    expect(shortens(current, { trace_days: 180, usage_days: 365, audit_days: null })).toBe(false);
    expect(shortens(current, { trace_days: null, usage_days: null, audit_days: null })).toBe(false);
  });

  it('confirms a save only while the form holds what was sent', () => {
    const sent = JSON.stringify({ trace_days: 30, usage_days: 365, audit_days: null });
    expect(savedMatches(sent, { trace_days: '30', usage_days: '365', audit_days: '' })).toBe(true);
    expect(savedMatches(sent, { trace_days: '60', usage_days: '365', audit_days: '' })).toBe(false);
  });

  it('exports whole UTC days, at most 366', () => {
    expect(exportRange('2026-10-01', '2026-10-07')).toEqual({ from: '2026-10-01T00:00:00.000Z', to: '2026-10-08T00:00:00.000Z' });
    expect(exportRange('2026-10-07', '2026-10-01')).toEqual({ error: 'The end date must not be before the start date.' });
    expect('error' in exportRange('2025-01-01', '2026-10-01')).toBe(true);
  });
});

describe('policies screen', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.includes('/trace-policy')) {
        if (init?.method === 'PUT') return json({ ...tracePolicy, ...JSON.parse(String(init.body)), version: tracePolicy.version + 1, updated_at: '2026-10-29T00:00:00Z' }, tracePolicy.version ? 200 : 201);
        return json(tracePolicy);
      }
      if (init?.method === 'PUT' && url.includes('retention')) return json({ ...retention, ...JSON.parse(String(init.body)), version: 3 });
      if (url.includes('/budgets')) {
        if (init?.method === 'PUT') return json({ ...orgBudget, ...JSON.parse(String(init.body)), version: 2 });
        if (init?.method === 'DELETE') return new Response(null, { status: 204 });
        return json({ items: budgetItems, next_page_token: null });
      }
      if (init?.method === 'PUT') return json({ ...orgPolicy, id: 'p-2', scope: 'team', team_id: TEAM, version: 1 }, 201);
      if (url.includes('/rate-limits')) return json({ items: [orgPolicy], next_page_token: null });
      if (url.includes('/teams')) return json({ items: [{ id: TEAM, organization_id: 'org-1', name: 'Research', version: 1 }], next_page_token: null });
      if (url.includes('/service-accounts')) return json({ items: [], next_page_token: null });
      if (url.includes('/retention-policy')) return json(retention);
      if (url.includes('/exports')) return json({ items: [], next_page_token: null });
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
  const render = async () => {
    act(() => root.render(<PoliciesScreen organizationId="org-1" />));
    await settle();
  };
  const openTab = async (name: string) => {
    await render();
    const key = { Budgets: 'budgets', 'Rate limits': 'limits', Retention: 'retention', Tracing: 'tracing', Exports: 'exports', Webhooks: 'webhooks' }[name];
    if (!key) throw new Error(`no section “${name}”`);
    openSection('policies', key);
    await settle();
  };
  const setInput = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    return input;
  };
  const choose = (id: string, value: string) => {
    const select = document.getElementById(id) as HTMLSelectElement;
    act(() => {
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
  };
  const puts = () => fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === 'PUT');

  it('warns that requests are refused while no budget is set', async () => {
    budgetItems = [];
    await render();
    expect(container.textContent).toContain('No budget is set, so every inference request is refused.');
    budgetItems = [orgBudget];
  });

  it('filters budgets by scope and by share used, from the URL', async () => {
    const teamBudget = { ...orgBudget, id: 'b-team', scope: 'team', team_id: TEAM, current: { ...orgBudget.current!, consumed_minor: 0, reserved_minor: 0 } };
    budgetItems = [orgBudget, teamBudget];
    history.replaceState(null, '', '/admin/policies?budgets.f.scope=team');
    await render();
    const table = () => container.querySelector('table[aria-label="Budgets"]')!;
    expect(table().textContent).toContain('Team · Research');
    expect(table().textContent).not.toContain('Whole organization');
    act(() => (container.querySelector('[aria-label="Remove filter Scope: Team"]') as HTMLButtonElement).click());
    expect(table().textContent).toContain('Whole organization');

    history.replaceState(null, '', '/admin/policies?budgets.r.used=80..');
    act(() => root.unmount());
    root = createRoot(container);
    await render();
    expect(table().textContent).toContain('Whole organization');
    expect(table().textContent).not.toContain('Team · Research');
    history.replaceState(null, '', '/admin/policies');
    budgetItems = [orgBudget];
  });

  it('filters budgets by the share the cell shows, and names scopes', async () => {
    const zero = { ...orgBudget, id: 'b-zero', scope: 'service-account', service_account_id: 'sa-9', limit_minor: 0 };
    const over = { ...orgBudget, id: 'b-over', scope: 'team', team_id: TEAM, current: { ...orgBudget.current!, consumed_minor: 20_000_000_000 } };
    budgetItems = [zero, over];
    history.replaceState(null, '', '/admin/policies?budgets.r.used=100..100');
    await render();
    const rows = container.querySelectorAll('table[aria-label="Budgets"] .data-row:not(.head)');
    // A zero limit and an overspent budget both show, and filter as, 100%.
    expect(rows).toHaveLength(2);
    const scopes = Array.from(container.querySelectorAll('[aria-label="Filter by scope"] label')).map(label => label.textContent);
    expect(scopes).toEqual(expect.arrayContaining([expect.stringContaining('Service account'), expect.stringContaining('Team')]));
    history.replaceState(null, '', '/admin/policies');
    budgetItems = [orgBudget];
  });

  it('shows what each budget has used this period', async () => {
    await render();
    expect(container.textContent).toContain('Whole organization');
    expect(container.textContent).toContain('$100.00');
    expect(container.textContent).toContain('$85.00');
    expect(container.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('85');
    expect(container.querySelector('.budget-bar.high')).not.toBeNull();
  });

  it('adds a monthly team budget in dollars', async () => {
    budgetItems = [];
    await render();
    act(() => button('Add budget').click());
    choose('budget-scope', 'team');
    choose('budget-target', TEAM);
    const input = setInput('budget-amount', '250');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[url, init]] = puts();
    expect(url).toBe(`/api/admin/v1/organizations/org-1/budgets/teams/${TEAM}`);
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ currency: 'USD', limit_minor: 25_000_000_000, period: 'monthly' });
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('*');
    budgetItems = [orgBudget];
  });

  it('edits a budget against its version, with fixed UTC days', async () => {
    await render();
    act(() => button('Edit').click());
    expect((document.getElementById('budget-amount') as HTMLInputElement).value).toBe('100');
    choose('budget-period', 'fixed');
    setInput('budget-from', '2026-10-01');
    const input = setInput('budget-to', '2026-10-31');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[url, init]] = puts();
    expect(url).toBe('/api/admin/v1/organizations/org-1/budgets/organization');
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({
      currency: 'USD', limit_minor: 10_000_000_000, period: 'fixed',
      starts_at: '2026-10-01T00:00:00.000Z', ends_at: '2026-11-01T00:00:00.000Z',
    });
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v1"');
  });

  it('keeps a fixed window’s exact times when only the limit changes', async () => {
    const fixed = { ...orgBudget, limit_minor: 1, period: 'fixed', starts_at: '2026-10-01T12:00:00Z', ends_at: '2026-10-15T12:00:00Z' };
    budgetItems = [fixed];
    await render();
    act(() => button('Edit').click());
    expect((document.getElementById('budget-amount') as HTMLInputElement).value).toBe('0.00000001');
    const input = setInput('budget-amount', '50');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[, init]] = puts();
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({
      currency: 'USD', limit_minor: 5_000_000_000, period: 'fixed',
      starts_at: '2026-10-01T12:00:00Z', ends_at: '2026-10-15T12:00:00Z',
    });
    budgetItems = [orgBudget];
  });

  it('removes a budget against its version', async () => {
    await render();
    act(() => button('Remove').click());
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Remove') as HTMLButtonElement).click());
    await settle();
    const [[url, init]] = fetchMock.mock.calls.filter(([, request]) => (request as RequestInit | undefined)?.method === 'DELETE');
    expect(url).toBe('/api/admin/v1/organizations/org-1/budgets/organization');
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v1"');
  });

  it('adds a team rate limit as a new policy', async () => {
    await openTab('Rate limits');
    expect(container.textContent).toContain('Whole organization');
    act(() => button('Add rate limit').click());
    choose('limit-scope', 'team');
    choose('limit-target', TEAM);
    const input = setInput('limit-concurrent', '8');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[url, init]] = puts();
    expect(url).toBe(`/api/admin/v1/organizations/org-1/rate-limits/teams/${TEAM}`);
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ requests_per_minute: null, tokens_per_minute: null, max_concurrent: 8 });
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('*');
  });

  it('limits one active API key, tokens per minute too, as budgets can', async () => {
    const SA = '33333333-3333-3333-3333-333333333333';
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (!init?.method && url.includes('/keys')) {
        return json({ items: [
          { id: 'key-1', public_prefix: 'gw_live_ab12', status: 'active' },
          { id: 'key-2', public_prefix: 'gw_live_cd34', status: 'revoked' },
        ], next_page_token: null });
      }
      if (!init?.method && url.includes('/service-accounts')) return json({ items: [{ id: SA, name: 'billing' }], next_page_token: null });
      return listing(url, init);
    });
    await openTab('Rate limits');
    act(() => button('Add rate limit').click());
    choose('limit-scope', 'api-key');
    await settle();
    const options = Array.from((document.getElementById('limit-target') as HTMLSelectElement).options).map(option => option.textContent);
    expect(options).toEqual(['Choose…', 'gw_live_ab12 (billing)']);
    expect(document.body.textContent).toContain('Applies on top of the key’s service account, team and organization limits');
    choose('limit-target', 'key-1');
    const input = setInput('limit-tpm', '50000');
    setInput('limit-concurrent', '2');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[url, init]] = puts();
    expect(url).toBe('/api/admin/v1/organizations/org-1/rate-limits/api-keys/key-1');
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ requests_per_minute: null, tokens_per_minute: 50_000, max_concurrent: 2 });
  });

  it('replaces a target’s policy against its version even when it is not on the loaded page', async () => {
    const teamPolicy = { ...orgPolicy, id: 'p-7', scope: 'team', team_id: TEAM, version: 7 };
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (!init?.method && url.includes('/rate-limits')) {
        return url.includes('page_size=100')
          ? json({ items: [orgPolicy, teamPolicy], next_page_token: null })
          : json({ items: [orgPolicy], next_page_token: 'more' });
      }
      return listing(url, init);
    });
    await openTab('Rate limits');
    act(() => button('Add rate limit').click());
    choose('limit-scope', 'team');
    choose('limit-target', TEAM);
    const input = setInput('limit-rpm', '100');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[, init]] = puts();
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v7"');
  });

  it('retries a lost save with the same key and version, even if the policy changed since', async () => {
    let version = 7;
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (!init?.method && url.includes('/rate-limits')) {
        return json({ items: [orgPolicy, { ...orgPolicy, id: 'p-7', scope: 'team', team_id: TEAM, version }], next_page_token: null });
      }
      if (init?.method === 'PUT' && version === 7) {
        version = 8;
        throw new TypeError('response lost');
      }
      return listing(url, init);
    });
    await openTab('Rate limits');
    act(() => button('Add rate limit').click());
    choose('limit-scope', 'team');
    choose('limit-target', TEAM);
    const input = setInput('limit-rpm', '100');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [first, second] = puts().map(([, request]) => new Headers((request as RequestInit).headers));
    expect(second.get('Idempotency-Key')).toBe(first.get('Idempotency-Key'));
    expect([first.get('If-Match'), second.get('If-Match')]).toEqual(['"v7"', '"v7"']);
  });

  it('keeps the teams it loaded when service accounts fail, and offers a retry', async () => {
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) =>
      url.includes('/service-accounts') ? json({ code: 'service_unavailable', status: 503 }, 503) : listing(url, init));
    await openTab('Rate limits');
    act(() => button('Add rate limit').click());
    choose('limit-scope', 'team');
    expect(Array.from((document.getElementById('limit-target') as HTMLSelectElement).options).map(option => option.textContent)).toContain('Research');
    choose('limit-scope', 'service-account');
    expect(document.body.textContent).toContain('The service accounts could not be loaded.');
  });

  it('edits an existing policy against its version', async () => {
    await openTab('Rate limits');
    act(() => button('Edit').click());
    const input = setInput('limit-rpm', '1200');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[url, init]] = puts();
    expect(url).toBe('/api/admin/v1/organizations/org-1/rate-limits/organization');
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v4"');
  });

  it('sets tracing for chosen teams, creating the policy with If-Match *', async () => {
    await openTab('Tracing');
    expect(document.getElementById('trace-days')).toBeNull();
    choose('trace-mode', 'selected');
    const form = (document.getElementById('trace-mode') as HTMLSelectElement).form as HTMLFormElement;
    act(() => form.requestSubmit());
    await settle();
    expect(container.textContent).toContain('Choose at least one team or service account.');
    expect(puts()).toHaveLength(0);
    const research = Array.from(container.querySelectorAll('.check-option')).find(node => node.textContent === 'Research')!.querySelector('input')!;
    act(() => research.click());
    setInput('trace-days', '14');
    act(() => form.requestSubmit());
    await settle();
    const [[url, init]] = puts();
    expect(String(url)).toContain('/organizations/org-1/trace-policy');
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ mode: 'selected', team_ids: [TEAM], service_account_ids: [], content: true, content_retention_days: 14 });
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('*');
    expect(container.textContent).toContain('Tracing policy saved.');
  });

  it('asks before shortening how long content is kept, and keeps the saved period while it is hidden', async () => {
    tracePolicy = { ...tracePolicy, mode: 'organization', version: 2, updated_at: '2026-10-20T00:00:00Z' };
    await openTab('Tracing');
    const days = setInput('trace-days', '7');
    act(() => (days.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(puts()).toHaveLength(0);
    expect(container.textContent).toContain('older than 7 days');
    act(() => button('Save and delete older content').click());
    await settle();
    const [[, first]] = puts();
    expect(JSON.parse(String((first as RequestInit).body)).content_retention_days).toBe(7);
    expect(new Headers((first as RequestInit).headers).get('If-Match')).toBe('"v2"');

    // An emptied field that Off hides does not block saving.
    setInput('trace-days', '');
    choose('trace-mode', 'off');
    const form = (document.getElementById('trace-mode') as HTMLSelectElement).form as HTMLFormElement;
    act(() => form.requestSubmit());
    await settle();
    const second = puts()[1][1] as RequestInit;
    expect(JSON.parse(String(second.body))).toEqual({ mode: 'off', team_ids: [], service_account_ids: [], content: true, content_retention_days: 7 });
    tracePolicy = { ...tracePolicy, mode: 'off', version: 0, updated_at: null };
  });

  it('shows the tracing policy without changes to a read-only role', async () => {
    tracePolicy = { ...tracePolicy, mode: 'organization', version: 3, updated_at: '2026-10-20T00:00:00Z' };
    act(() => root.render(<PoliciesScreen organizationId="org-1" role="auditor" />));
    await settle();
    openSection('policies', 'tracing');
    await settle();
    expect((document.getElementById('trace-mode') as HTMLSelectElement).value).toBe('organization');
    expect(button('Save policy')).toBeUndefined();
    expect(container.textContent).toContain('Your role can view the tracing policy but not change it.');
    tracePolicy = { ...tracePolicy, mode: 'off', version: 0, updated_at: null };
  });

  it('asks before shortening retention, then saves against the version', async () => {
    await render();
    openSection('policies', 'retention');
    await settle();
    expect(container.textContent).toContain('12 traces');
    const input = setInput('retention-trace_days', '30');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(puts()).toHaveLength(0);
    act(() => button('Save and delete older data').click());
    await settle();
    const [[, init]] = puts();
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ trace_days: 30, usage_days: 365, audit_days: null });
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v2"');
    expect(container.textContent).toContain('Retention policy saved.');

    // Saving again is a new request against the new version, not a replay.
    const again = setInput('retention-trace_days', '60');
    act(() => (again.form as HTMLFormElement).requestSubmit());
    await settle();
    const [first, second] = puts().map(([, request]) => new Headers((request as RequestInit).headers));
    expect(second.get('If-Match')).toBe('"v3"');
    expect(second.get('Idempotency-Key')).not.toBe(first.get('Idempotency-Key'));
  });

  it('locks the retention fields while a save is in flight', async () => {
    let finish: (response: Response) => void = () => undefined;
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation((url: string, init?: RequestInit) =>
      init?.method === 'PUT' ? new Promise<Response>(resolve => { finish = resolve; }) : listing(url, init));
    await render();
    openSection('policies', 'retention');
    await settle();
    const input = setInput('retention-trace_days', '180');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect((document.getElementById('retention-trace_days') as HTMLInputElement).closest('fieldset')!.disabled).toBe(true);
    finish(json({ ...retention, trace_days: 180, version: 3 }));
    await settle();
    expect((document.getElementById('retention-trace_days') as HTMLInputElement).closest('fieldset')!.disabled).toBe(false);
    expect(container.textContent).toContain('Retention policy saved.');
    setInput('retention-trace_days', '200');
    expect(container.textContent).not.toContain('Retention policy saved.');
  });

  it('recovers from a retention version conflict without losing edits', async () => {
    let current: Omit<typeof retention, 'audit_days'> & { audit_days: number | null } = { ...retention };
    let conflicts = true;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.includes('/retention-policy') && init?.method === 'PUT') {
        if (conflicts) return json({ type: '', code: 'version_conflict', title: '', status: 409, detail: '', instance: '', request_id: '' }, 409);
        return json({ ...current, ...JSON.parse(String(init.body)), version: current.version + 1 });
      }
      if (url.includes('/retention-policy')) return json(current);
      return json({ items: [], next_page_token: null });
    });
    await render();
    openSection('policies', 'retention');
    await settle();
    // Another admin raises audit retention meanwhile.
    current = { ...retention, audit_days: 400, version: 3 };
    const input = setInput('retention-trace_days', '180');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(container.textContent).toContain('Someone changed the retention policy');
    act(() => button('Load the current policy').click());
    await settle();
    expect(container.textContent).toContain('audit events 400');
    expect((document.getElementById('retention-trace_days') as HTMLInputElement).value).toBe('180');
    conflicts = false;
    act(() => ((document.getElementById('retention-trace_days') as HTMLInputElement).form as HTMLFormElement).requestSubmit());
    await settle();
    const last = fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === 'PUT').pop()!;
    expect(new Headers((last[1] as RequestInit).headers).get('If-Match')).toBe('"v3"');
    expect(container.textContent).toContain('Retention policy saved.');
  });

  it('links the export download to the chosen data, format and days', async () => {
    await render();
    openSection('policies', 'exports');
    await settle();
    choose('export-dataset', 'audit-events');
    setInput('export-from', '2026-10-01');
    setInput('export-to', '2026-10-06');
    const link = Array.from(container.querySelectorAll('a')).find(node => node.textContent === 'Download') as HTMLAnchorElement;
    expect(link.getAttribute('href')).toBe('/api/admin/v1/organizations/org-1/exports/audit-events?format=csv&from=2026-10-01T00%3A00%3A00.000Z&to=2026-10-07T00%3A00%3A00.000Z');
  });

  it('shows the whole export history on the table, filtered and opened in place', async () => {
    const item = (id: string, status: string, rows: number) => ({
      id, dataset: 'usage', format: 'csv', from: '2026-10-01T00:00:00Z', to: '2026-10-08T00:00:00Z', actor_user_id: TEAM,
      status, rows, started_at: '2026-10-08T09:00:00Z', finished_at: '2026-10-08T09:00:04Z',
    });
    fetchMock.mockImplementation(async (url: string) => {
      if (!url.includes('/exports')) return json({ items: [], next_page_token: null });
      // Two pages, both loaded.
      return url.includes('page_token=p2')
        ? json({ items: [item('x-3', 'failed', 0)], next_page_token: null })
        : json({ items: [item('x-1', 'completed', 120), item('x-2', 'completed', 30)], next_page_token: 'p2' });
    });
    history.replaceState(null, '', '/?exports.f.status=completed');
    await openTab('Exports');
    const table = container.querySelector('[aria-label="Export history"]')!;
    expect(table.querySelectorAll('.data-row:not(.head)')).toHaveLength(2);
    expect(container.textContent).toContain('2 of 3 rows');
    act(() => (table.querySelector('button[aria-label^="Expand"]') as HTMLButtonElement).click());
    expect(table.textContent).toContain('Export ID');
    expect(table.textContent).toContain('x-1');
    expect(table.textContent).toContain('4 s');
    history.replaceState(null, '', '/');
  });

  it('lets exports by a removed user be filtered as Unknown', async () => {
    const item = (id: string, actor: string | null) => ({
      id, dataset: 'usage', format: 'csv', from: '2026-10-01T00:00:00Z', to: '2026-10-08T00:00:00Z', actor_user_id: actor,
      status: 'completed', rows: 1, started_at: '2026-10-08T09:00:00Z', finished_at: null,
    });
    fetchMock.mockImplementation(async (url: string) => json(url.includes('/exports')
      ? { items: [item('x-1', null), item('x-2', TEAM)], next_page_token: null }
      : { items: [], next_page_token: null }));
    history.replaceState(null, '', '/?exports.f.actor=unknown&exports.g=actor');
    await openTab('Exports');
    const table = container.querySelector('[aria-label="Export history"]')!;
    expect(container.textContent).toContain('1 of 2 rows');
    expect(table.querySelector('.group-row')!.textContent).toContain('Unknown');
    const menu = Array.from(container.querySelectorAll('details')).find(details => details.querySelector('summary')?.textContent?.startsWith('Exported by'))!;
    expect(menu.textContent).toContain('Unknown');
    history.replaceState(null, '', '/');
  });
});
