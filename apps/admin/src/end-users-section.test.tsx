import { readView } from './table-view';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { EndUsersSection } from './end-users-section';
import type { CreditGrant, EndUser, EndUserPlan, EndUserSettings, ServiceAccount } from './api';

const ORG = 'b6c2a7a4-6f3e-4ad8-8f43-2a9d1d4f5e01';
const SA = '9a1c6f0e-2b3d-4e5f-8a9b-0c1d2e3f4a5b';
const DOLLAR = 100_000_000;
const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
const problem = (status: number, code: string) =>
  json({ type: 'about:blank', code, title: code, status, detail: code, instance: '', request_id: 'req-1' }, status);
const account: ServiceAccount = {
  id: SA, organization_id: ORG, team_id: null, owner_user_id: 'u-1', name: 'chat-app', enabled: true,
  version: 1, created_at: '2026-11-01T00:00:00Z', updated_at: '2026-11-01T00:00:00Z',
} as unknown as ServiceAccount;
const plan = (name: string, allowance: number, extra: Partial<EndUserPlan> = {}): EndUserPlan => ({
  name, allowance_minor: allowance, period: 'monthly', period_days: null, requests_per_minute: null,
  tokens_per_minute: null, max_concurrent: null, currency: 'USD', users: 0, spent_minor: 0,
  period_start: '2026-11-01T00:00:00Z', period_end: '2026-12-01T00:00:00Z',
  version: 1, created_at: 'x', updated_at: '2026-11-01T00:00:00Z', ...extra,
});
const user = (id: string, plan: string | null): EndUser => ({
  id, plan, effective_plan: plan ?? 'free', currency: 'USD',
  period_start: '2026-11-01T00:00:00Z', period_end: '2026-12-01T00:00:00Z',
  allowance_minor: 5 * DOLLAR, allowance_spent_minor: 2 * DOLLAR, allowance_reserved_minor: 0,
  allowance_remaining_minor: 3 * DOLLAR, credit_balance_minor: 0, spent_minor: 2 * DOLLAR + DOLLAR / 4, spent_currency: 'USD', version: 0,
  created_at: '2026-11-02T00:00:00Z', updated_at: '2026-11-02T00:00:00Z',
});

describe('end users of a service account', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let settings: EndUserSettings | null;
  let plans: EndUserPlan[];
  let users: EndUser[];
  let grants: CreditGrant[];

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    settings = null;
    plans = [];
    users = [];
    grants = [];
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? 'GET';
      const path = url.split('?')[0];
      if (path.endsWith('/end-user-settings')) {
        if (method === 'PUT') {
          const body = JSON.parse(String(init!.body));
          const created = settings === null;
          settings = { service_account_id: SA, ...body, version: (settings?.version ?? 0) + 1, created_at: 'x', updated_at: 'x' };
          return json(settings, created ? 201 : 200, { etag: `"v${settings!.version}"` });
        }
        return settings ? json(settings) : problem(404, 'not_found');
      }
      if (path.endsWith('/end-user-plans')) return json({ data: plans });
      const planPath = path.match(/\/end-user-plans\/([^/]+)$/);
      if (planPath && method === 'PUT') {
        const body = JSON.parse(String(init!.body));
        const saved = plan(decodeURIComponent(planPath[1]), body.allowance_minor, { ...body, version: 1 });
        plans = [...plans.filter(item => item.name !== saved.name), saved];
        return json(saved, 201, { etag: '"v1"' });
      }
      if (path.endsWith('/end-users')) return json({ data: users, next_page_token: null });
      const userPath = path.match(/\/end-users\/([^/]+)$/);
      if (userPath && method === 'PUT') {
        const added = { ...user(decodeURIComponent(userPath[1]), JSON.parse(String(init!.body)).plan), version: 1 };
        users.push(added);
        return json(added, 201, { etag: '"v1"' });
      }
      const grantPath = path.match(/\/end-users\/([^/]+)\/credit-grants$/);
      if (grantPath && method === 'POST') {
        const body = JSON.parse(String(init!.body));
        const grant: CreditGrant = {
          id: 'g-1', end_user: decodeURIComponent(grantPath[1]), reference: body.reference, amount_minor: body.amount_minor,
          currency: 'USD', reversed_minor: 0, spent_minor: 0, reserved_minor: 0, remaining_minor: body.amount_minor,
          expires_at: body.expires_at, expired: false, granted_at: '2026-11-14T00:00:00Z',
        };
        grants.push(grant);
        return json(grant, 201);
      }
      if (grantPath) return json({ data: grants });
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
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const button = (text: string) =>
    Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement | undefined;
  const setInput = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const render = async () => {
    act(() => root.render(<EndUsersSection organizationId={ORG} account={account} />));
    await settle();
  };
  const writes = (method: string) =>
    fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === method) as [string, RequestInit][];

  it('turns end-user limits on for a service account that has none', async () => {
    await render();
    expect(container.textContent).toContain('Limit this app’s end users');
    expect(container.textContent).toContain('No end users yet');
    setInput('end-user-currency', 'eur');
    await act(async () => { button('Turn on')!.click(); });
    await settle();
    const [put] = writes('PUT');
    expect(put[0]).toContain(`/service-accounts/${SA}/end-user-settings`);
    expect(JSON.parse(String(put[1].body))).toEqual({ currency: 'EUR', requirement: 'optional', default_plan: null });
    const headers = new Headers(put[1].headers);
    expect(headers.get('If-Match')).toBe('*');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('End-user settings');
    expect(container.textContent).toContain('No plans');
  });

  it('shows each user’s plan, allowance left and credits, and grants credits by reference', async () => {
    settings = { service_account_id: SA, requirement: 'required', currency: 'USD', default_plan: 'free', version: 2, created_at: 'x', updated_at: 'x' };
    plans = [plan('free', 5 * DOLLAR, { users: 12, spent_minor: 31 * DOLLAR })];
    users = [user('user/42', null)];
    await render();
    const plansTable = container.querySelector('[aria-label="End-user plans"]')!;
    expect(plansTable.textContent).toContain('free');
    expect(plansTable.textContent).toContain('(default)');
    // Per-plan totals: users on it and their spend this period.
    expect(plansTable.textContent).toContain('$5.00 monthly');
    expect(plansTable.textContent).toContain('12');
    expect(plansTable.textContent).toContain('$31.00');
    expect(plansTable.textContent).toContain('None');
    const usersTable = container.querySelector('[aria-label="End users"]')!;
    expect(usersTable.textContent).toContain('user/42');
    expect(usersTable.textContent).toContain('$3.00 of $5.00');
    // This month's spend counts credits as well as the allowance.
    expect(usersTable.textContent).toContain('$2.25');
    // Each user links to their requests on the Traces page.
    const traces = usersTable.querySelector('a[aria-label="Traces of user/42"]') as HTMLAnchorElement;
    const link = new URL(traces.href, 'http://x');
    expect(link.pathname).toBe('/admin/traces');
    expect(readView(link.search, 'traces').categories).toEqual({ service_account: [SA], end_user: ['user/42'] });

    await act(async () => { button('Credits')!.click(); });
    await settle();
    expect(container.textContent).toContain('No credits');
    await act(async () => { button('Grant credits')!.click(); });
    setInput('grant-reference', 'order-981');
    setInput('grant-amount', '10.00');
    await act(async () => { button('Grant')!.click(); });
    await settle();
    const [post] = writes('POST');
    expect(post[0]).toContain('/end-users/user%2F42/credit-grants');
    expect(JSON.parse(String(post[1].body))).toEqual({ reference: 'order-981', amount_minor: 10 * DOLLAR, expires_at: null });
    // The reference makes the grant idempotent; the console sends no key.
    expect(new Headers(post[1].headers).get('Idempotency-Key')).toBeNull();
    expect(container.querySelector('[aria-label="Credit grants of user/42"]')!.textContent).toContain('order-981');
  });

  it('adds a user requests have not created yet, so they can get a plan before their first request', async () => {
    settings = { service_account_id: SA, requirement: 'optional', currency: 'USD', default_plan: null, version: 1, created_at: 'x', updated_at: 'x' };
    plans = [plan('pro', 20 * DOLLAR)];
    await render();
    await act(async () => { button('Add end user')!.click(); });
    setInput('new-end-user-id', 'new-user');
    const select = document.getElementById('new-end-user-plan') as HTMLSelectElement;
    act(() => {
      select.value = 'pro';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await act(async () => { button('Add')!.click(); });
    await settle();
    const [put] = writes('PUT');
    expect(put[0]).toContain('/end-users/new-user');
    expect(JSON.parse(String(put[1].body))).toEqual({ plan: 'pro' });
    expect(new Headers(put[1].headers).get('If-Match')).toBe('*');
    expect(container.querySelector('[aria-label="End users"]')!.textContent).toContain('new-user');
  });

  it('edits a plan’s period and per-user rate limits, and shows them with its totals', async () => {
    settings = { service_account_id: SA, requirement: 'optional', currency: 'USD', default_plan: null, version: 1, created_at: 'x', updated_at: 'x' };
    plans = [plan('pro', 20 * DOLLAR, { users: 3, spent_minor: 7 * DOLLAR })];
    await render();
    await act(async () => { button('Edit')!.click(); });
    const period = document.getElementById('plan-period') as HTMLSelectElement;
    expect(period.value).toBe('monthly');
    act(() => {
      period.value = 'days';
      period.dispatchEvent(new Event('change', { bubbles: true }));
    });
    setInput('plan-allowance', '2.50');
    // Days are required with an every-N-days period, and limits must be whole.
    const save = () => button('Save')!;
    expect(save().disabled).toBe(true);
    setInput('plan-period-days', '30');
    setInput('plan-requests-per-minute', '1.5');
    expect(save().disabled).toBe(true);
    expect(container.ownerDocument.body.textContent).toContain('Enter 1 to 1,000,000, or leave empty');
    setInput('plan-requests-per-minute', '60');
    setInput('plan-tokens-per-minute', '100000');
    setInput('plan-max-concurrent', '0');
    expect(save().disabled).toBe(true);
    setInput('plan-max-concurrent', '4');
    expect(save().disabled).toBe(false);
    await act(async () => { save().click(); });
    await settle();
    const [put] = writes('PUT');
    expect(put[0]).toContain('/end-user-plans/pro');
    expect(JSON.parse(String(put[1].body))).toEqual({
      allowance_minor: 2.5 * DOLLAR, period: 'days', period_days: 30, requests_per_minute: 60, tokens_per_minute: 100_000,
      max_concurrent: 4,
    });
    expect(new Headers(put[1].headers).get('If-Match')).toBe('"v1"');
    const plansTable = container.querySelector('[aria-label="End-user plans"]')!;
    expect(plansTable.textContent).toContain('$2.50 every 30 days');
    expect(plansTable.textContent).toContain('60 requests/min, 100,000 tokens/min, 4 in flight');
  });

  it('sends a plain plan as monthly with no limits', async () => {
    settings = { service_account_id: SA, requirement: 'optional', currency: 'USD', default_plan: null, version: 1, created_at: 'x', updated_at: 'x' };
    await render();
    await act(async () => { button('Add plan')!.click(); });
    setInput('plan-name', 'free');
    setInput('plan-allowance', '1.00');
    await act(async () => { button('Save')!.click(); });
    await settle();
    const [put] = writes('PUT');
    expect(JSON.parse(String(put[1].body))).toEqual({
      allowance_minor: DOLLAR, period: 'monthly', period_days: null, requests_per_minute: null, tokens_per_minute: null,
      max_concurrent: null,
    });
    expect(new Headers(put[1].headers).get('If-Match')).toBe('*');
  });
});
