import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { RoutingPolicySection, parseLocations, policySummary } from './routing-policy-section';
import { familyLabels } from './providers-screen';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string) =>
  json({ type: 'about:blank', code, title: code, status, detail: '', instance: '', request_id: 'req-1' }, status);
const CATALOG = '/api/admin/v1/organizations/org-1/catalog';
const deployment = (id: string, location: string | null) => ({
  id, provider_account_id: 'a-1', adapter_instance_key: 'openai-1', family: 'openai', name: id, model: 'm', location, operations: [],
});
const unset = { providers: null, locations: null, version: 0, updated_at: null };

describe('routing policy', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let policy: unknown;
  let putResponse: (body: Record<string, unknown>) => Response;
  let pinnedVersion: string | null;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    policy = unset;
    pinnedVersion = null;
    putResponse = body => json({ ...body, version: 1, updated_at: '2026-11-14T00:00:00Z' }, 201);
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? 'GET';
      if (url.startsWith(`${CATALOG}/routing-policy`)) {
        if (method === 'PUT') return putResponse(JSON.parse(String(init!.body)));
        return json(policy);
      }
      if (url.endsWith('/config-versions/pin')) return json({ pinned_version_id: pinnedVersion, served_version_id: null, newest_version_id: null, version: 1 });
      if (url.startsWith(`${CATALOG}/deployments`)) {
        return json({ items: [deployment('d-1', 'us-east-1'), deployment('d-2', 'eu-west-1'), deployment('d-3', null)], next_page_token: null, release_id: 'r-1' });
      }
      return json({ items: [], next_page_token: null });
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
  const render = async (canChange = true) => {
    act(() => root.render(<RoutingPolicySection organizationId="org-1" canChange={canChange} familyLabels={familyLabels} />));
    await settle();
  };
  const option = (label: string) =>
    Array.from(container.querySelectorAll('.check-option')).find(node => node.textContent?.trim() === label)?.querySelector('input') as HTMLInputElement;
  const click = async (target: HTMLElement | null | undefined) => {
    if (!target) throw new Error('no such control');
    act(() => target.click());
    await settle();
  };
  const save = () => click(Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Save routing policy'));
  const type = (value: string) => {
    const input = container.querySelector('textarea') as HTMLTextAreaElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const puts = () => fetchMock.mock.calls.filter(([url, init]) => (init as RequestInit | undefined)?.method === 'PUT' && String(url).includes('/routing-policy'));

  it('parses and summarizes', () => {
    expect(parseLocations(' us-east-1\neu-west-1, us-east-1\n\n')).toEqual(['eu-west-1', 'us-east-1']);
    expect(policySummary(unset, familyLabels)).toBe('Requests may be sent to any provider, in any location.');
    expect(policySummary({ providers: ['bedrock'], locations: ['us-east-1'] }, familyLabels))
      .toBe('Requests may be sent to Amazon Bedrock, in us-east-1.');
  });

  it('restricts providers and locations, and says which deployments it leaves out', async () => {
    await render();
    expect(container.textContent).toContain('Requests may be sent to any provider, in any location.');
    await click(option('Only these providers'));
    await click(option('Amazon Bedrock'));
    await click(option('OpenAI'));
    await click(option('Only these locations'));
    expect(container.textContent).toContain('this catalog has eu-west-1, us-east-1');
    expect(container.textContent).toContain('1 deployment has no location');
    type('us-east-1\nus-east-1, eu-west-1');
    await save();
    const [put] = puts();
    expect(JSON.parse(String((put[1] as RequestInit).body))).toEqual({ providers: ['bedrock', 'openai'], locations: ['eu-west-1', 'us-east-1'] });
    expect(new Headers((put[1] as RequestInit).headers).get('If-Match')).toBe('*');
    expect(new Headers((put[1] as RequestInit).headers).get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('Requests may be sent to Amazon Bedrock, OpenAI, in eu-west-1, us-east-1.');
    expect(container.textContent).toContain('Saved.');
  });

  it('refuses an empty list before sending it, and sends the version it read', async () => {
    policy = { providers: ['openai'], locations: null, version: 4, updated_at: '2026-11-14T00:00:00Z' };
    await render();
    expect(option('Only these providers').checked).toBe(true);
    await click(option('OpenAI'));
    await save();
    expect(puts()).toHaveLength(0);
    expect(container.textContent).toContain('Choose at least one provider, or allow any provider.');
    await click(option('Only these providers'));
    await save();
    const [put] = puts();
    expect(JSON.parse(String((put[1] as RequestInit).body))).toEqual({ providers: null, locations: null });
    expect(new Headers((put[1] as RequestInit).headers).get('If-Match')).toBe('"v4"');
  });

  it('reloads after a conflict', async () => {
    putResponse = () => problem(409, 'version_conflict');
    await render();
    await click(option('Only these locations'));
    type('us-east-1');
    await save();
    expect(container.textContent).toContain('Someone else changed the routing policy; it has been reloaded.');
    expect(fetchMock.mock.calls.filter(([url, init]) => !(init as RequestInit | undefined)?.method && String(url).endsWith('/routing-policy'))).toHaveLength(2);
  });

  it('warns that a pinned organization keeps its pinned policy', async () => {
    await render();
    expect(container.textContent).not.toContain('pinned to a configuration version');
    act(() => root.unmount());
    root = createRoot(container);
    pinnedVersion = 'v-1';
    await render();
    expect(container.textContent).toContain('This organization is pinned to a configuration version');
  });

  it('is read-only without catalog write access', async () => {
    policy = { providers: ['bedrock'], locations: null, version: 1, updated_at: '2026-11-14T00:00:00Z' };
    await render(false);
    expect(container.textContent).toContain('Requests may be sent to Amazon Bedrock, in any location.');
    expect(option('Only these providers').disabled).toBe(true);
    expect(Array.from(container.querySelectorAll('button')).some(node => node.textContent === 'Save routing policy')).toBe(false);
  });
});
