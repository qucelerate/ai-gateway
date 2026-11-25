import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { OrganizationScreen } from './organization-screen';
import { mcpUrl } from './agents-section';
import type { Agent, AgentRole, AgentToken } from './api';
import { openSection } from './test-route';

const ORG = 'b6c2a7a4-6f3e-4ad8-8f43-2a9d1d4f5e01';
const TOKEN = 'gwa_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde';
const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }, status);
const token = (id: string, status: AgentToken['status'], name = 'laptop'): AgentToken => ({
  id, name, public_prefix: `gwa_${id.padEnd(8, 'x')}`, status,
  created_at: '2026-10-10T10:00:00Z', created_by: 'u-1',
  expires_at: '2027-01-08T10:00:00Z', revoked_at: status === 'revoked' ? '2026-10-11T10:00:00Z' : null,
  last_used_at: null, service_account_id: null,
});
const agent = (id: string, name: string, enabled: boolean, tokens: AgentToken[], role: AgentRole = 'admin'): Agent => ({
  id, name, description: null, role, enabled,
  created_at: '2026-10-10T10:00:00Z', created_by: 'u-1',
  disabled_at: enabled ? null : '2026-10-12T10:00:00Z', tokens,
});
const context = { user_id: 'u-1', organization_id: ORG, team_id: null, role: 'Owner' };

describe('organization agents', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let agents: Agent[];
  let grantable: AgentRole[];
  let route: (url: string, init?: RequestInit) => Promise<Response>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    grantable = ['owner', 'admin', 'auditor', 'member', 'read-only'];
    agents = [
      agent('a-1', 'ci-deploy', true, [token('t-1', 'active'), token('t-2', 'revoked', 'old')]),
      agent('a-2', 'retired-bot', false, [token('t-3', 'revoked')], 'auditor'),
    ];
    route = async (url: string, init?: RequestInit) => {
      const method = init?.method ?? 'GET';
      if (url.endsWith('/agents') && method === 'POST') {
        const body = JSON.parse(String(init!.body));
        const created = agent('a-9', body.name, true, [], body.role);
        agents.push(created);
        return json(created, 201, { 'x-request-id': 'req-agent' });
      }
      const tokens = url.match(/\/agents\/([^/]+)\/tokens$/);
      if (tokens && method === 'POST') {
        const body = JSON.parse(String(init!.body));
        const minted = { ...token('t-9', 'active', body.name), expires_at: body.expires_in_days ? '2027-01-08T10:00:00Z' : null, service_account_id: body.service_account_id ?? null };
        agents.find(item => item.id === tokens[1])!.tokens.push(minted);
        return json({ ...minted, token: TOKEN, replay: false }, 201, { 'x-request-id': 'req-token' });
      }
      const revoke = url.match(/\/agents\/([^/]+)\/tokens\/([^/]+)$/);
      if (revoke && method === 'DELETE') {
        const owner = agents.find(item => item.id === revoke[1])!;
        owner.tokens = owner.tokens.map(item => (item.id === revoke[2] ? token(item.id, 'revoked', item.name) : item));
        return json(token(revoke[2], 'revoked'));
      }
      const disable = url.match(/\/agents\/([^/]+)$/);
      if (disable && method === 'DELETE') {
        agents = agents.map(item => (item.id === disable[1] ? { ...item, enabled: false } : item));
        return json(agents.find(item => item.id === disable[1]));
      }
      if (url.endsWith('/agents')) return json({ items: agents, grantable_roles: grantable });
      if (url.includes('/service-accounts')) return json({ items: [{ id: 'sa-1', name: 'chat-app', team_id: null, enabled: true }], next_page_token: null });
      if (url.includes(`/organizations/${ORG}/`)) return json({ items: [], next_page_token: null });
      return json({}, 404);
    };
    fetchMock = vi.fn(route);
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const tab = (name: string) =>
    Array.from(container.querySelectorAll('[role="tab"]')).find(node => node.textContent === name) as HTMLButtonElement | undefined;
  const buttons = (text: string) =>
    Array.from(document.querySelectorAll('button')).filter(node => node.textContent === text) as HTMLButtonElement[];
  const openAgents = async () => {
    act(() => root.render(<OrganizationScreen section="agents" organizationId={ORG} userId="u-1" context={context} />));
    await settle();
  };
  const rows = () => Array.from(container.querySelectorAll('[aria-label="Agents"] .data-row:not(.head)'));
  const tokenRows = (name: string) =>
    Array.from(container.querySelectorAll(`[aria-label="Tokens of ${name}"] .data-row:not(.head)`));
  const setInput = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const writes = (method: string) =>
    fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === method) as [string, RequestInit][];
  const key = (init: RequestInit) => new Headers(init.headers).get('Idempotency-Key');

  it('lists agents with their role, status and active tokens, and their tokens on request', async () => {
    await openAgents();
    expect(container.textContent).toContain('gwctl');
    expect(rows().map(row => row.querySelector('.badge')?.textContent)).toEqual(['active', 'disabled']);
    expect(rows()[0].textContent).toContain('Admin');
    expect(rows()[1].textContent).toContain('Auditor');
    // One active token of two; only an enabled agent can be disabled.
    expect(rows()[0].textContent).toContain('1');
    expect(rows().map(row => row.textContent?.includes('Disable'))).toEqual([true, false]);

    act(() => (rows()[0].querySelector('button') as HTMLButtonElement).click());
    expect(tokenRows('ci-deploy').map(row => row.querySelector('.badge')?.textContent)).toEqual(['active', 'revoked']);
    expect(tokenRows('ci-deploy').map(row => row.textContent?.includes('Revoke'))).toEqual([true, false]);
    expect(tokenRows('ci-deploy')[0].textContent).toContain('gwa_t-1');
  });

  it('creates an agent, then its first token, shown once with how to use it', async () => {
    await openAgents();
    act(() => buttons('Create agent')[0].click());
    const role = document.getElementById('agent-role') as HTMLSelectElement;
    expect(Array.from(role.options).map(option => option.value)).toEqual(grantable);
    expect(role.value).toBe('admin');
    setInput('agent-name', 'claude-code');
    act(() => {
      role.value = 'auditor';
      role.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => buttons('Create agent')[1].click());
    await settle();

    const [url, init] = writes('POST')[0];
    expect(url).toBe(`/api/admin/v1/organizations/${ORG}/agents`);
    expect(JSON.parse(String(init.body))).toEqual({ name: 'claude-code', role: 'auditor', description: null });
    expect(key(init)).toBeTruthy();

    // The token dialog opens for the new agent, with a name to start from.
    expect((document.getElementById('agent-token-name') as HTMLInputElement).value).toBe('default');
    expect((document.getElementById('agent-token-expiry') as HTMLSelectElement).value).toBe('90');
    act(() => buttons('Create token')[buttons('Create token').length - 1].click());
    await settle();
    const [tokenUrl, tokenInit] = writes('POST')[1];
    expect(tokenUrl).toBe(`/api/admin/v1/organizations/${ORG}/agents/a-9/tokens`);
    expect(JSON.parse(String(tokenInit.body))).toEqual({ name: 'default', expires_in_days: 90 });

    const panel = container.querySelector('.secret-panel')!;
    expect(panel.querySelector('[aria-label="Agent token"]')?.textContent).toBe(TOKEN);
    expect(panel.querySelector('[aria-label="Copy agent token"]')).not.toBeNull();
    expect(panel.textContent).toContain('GATEWAY_AGENT_TOKEN');
    expect(panel.textContent).toContain(mcpUrl());
    expect(panel.textContent).toContain('only time the token is shown');
    // The new agent is selected, with its token listed.
    expect(tokenRows('claude-code')).toHaveLength(1);

    act(() => buttons('Done')[0].click());
    expect(document.body.textContent).not.toContain(TOKEN);
  });

  it('mints a token that never expires, and keeps its key when the same dialog retries', async () => {
    let fail = true;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/tokens') && init?.method === 'POST' && fail) {
        fail = false;
        return problem(503, 'service_unavailable', 'agent store unavailable');
      }
      return route(url, init);
    });
    await openAgents();
    act(() => (rows()[0].querySelector('button') as HTMLButtonElement).click());
    act(() => buttons('Create token')[0].click());
    setInput('agent-token-name', 'github-actions');
    act(() => {
      const expiry = document.getElementById('agent-token-expiry') as HTMLSelectElement;
      expiry.value = 'never';
      expiry.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => buttons('Create token')[1].click());
    await settle();
    // The server's detail, as a sentence, with its code for whoever helps.
    expect(document.body.textContent).toContain('Agent store unavailable.');
    expect(document.body.textContent).toContain('Error code: service_unavailable');
    act(() => buttons('Create token')[1].click());
    await settle();

    const sent = writes('POST');
    expect(sent).toHaveLength(2);
    expect(key(sent[0][1])).toBe(key(sent[1][1]));
    expect(JSON.parse(String(sent[1][1].body))).toEqual({ name: 'github-actions', expires_in_days: null });
    expect(container.querySelector('.secret-panel')?.textContent).toContain('does not expire');
  });

  it('limits a token to one service account’s end users', async () => {
    await openAgents();
    act(() => (rows()[0].querySelector('button') as HTMLButtonElement).click());
    act(() => buttons('Create token')[0].click());
    setInput('agent-token-name', 'chat-backend');
    const scope = document.getElementById('agent-token-scope') as HTMLSelectElement;
    expect(Array.from(scope.options).map(option => option.textContent)).toEqual([
      'The whole organization, as the agent’s role allows',
      'End users of chat-app only',
    ]);
    act(() => {
      scope.value = 'sa-1';
      scope.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => buttons('Create token')[1].click());
    await settle();
    const sent = writes('POST');
    expect(JSON.parse(String(sent[0][1].body))).toMatchObject({ name: 'chat-backend', service_account_id: 'sa-1' });
    expect(tokenRows('ci-deploy').map(row => row.textContent).join(' ')).toContain('End users of chat-app');
  });

  it('says so when a replayed token cannot be shown again', async () => {
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/tokens') && init?.method === 'POST') {
        return json({ ...token('t-9', 'active'), token: null, replay: true });
      }
      return route(url, init);
    });
    await openAgents();
    act(() => (rows()[0].querySelector('button') as HTMLButtonElement).click());
    act(() => buttons('Create token')[0].click());
    setInput('agent-token-name', 'laptop');
    act(() => buttons('Create token')[1].click());
    await settle();
    expect(container.querySelector('.secret-panel')).toBeNull();
    expect(container.textContent).toContain('cannot be shown again');
  });

  it('revokes a token and disables an agent, each with its own key, and shows a refusal', async () => {
    await openAgents();
    act(() => (rows()[0].querySelector('button') as HTMLButtonElement).click());
    act(() => buttons('Revoke')[0].click());
    act(() => buttons('Revoke')[buttons('Revoke').length - 1].click());
    await settle();
    expect(writes('DELETE')[0][0]).toBe(`/api/admin/v1/organizations/${ORG}/agents/a-1/tokens/t-1`);
    expect(tokenRows('ci-deploy').map(row => row.querySelector('.badge')?.textContent)).toEqual(['revoked', 'revoked']);

    fetchMock.mockImplementationOnce(async () =>
      problem(409, 'last_owner_required', 'the organization needs another enabled owner first'));
    act(() => buttons('Disable')[0].click());
    act(() => buttons('Disable')[buttons('Disable').length - 1].click());
    await settle();
    expect(document.body.textContent).toContain('The organization needs at least one owner. Make someone else an owner first.');
    expect(document.body.textContent).not.toContain('needs another enabled owner');

    act(() => buttons('Disable')[buttons('Disable').length - 1].click());
    await settle();
    const deletes = writes('DELETE');
    expect(deletes.map(([url]) => url).slice(1)).toEqual([
      `/api/admin/v1/organizations/${ORG}/agents/a-1`,
      `/api/admin/v1/organizations/${ORG}/agents/a-1`,
    ]);
    // A retry of the same confirmation is the same request; the revoke was another.
    expect(key(deletes[1][1])).toBe(key(deletes[2][1]));
    expect(key(deletes[1][1])).not.toBe(key(deletes[0][1]));
    expect(rows().map(row => row.querySelector('.badge')?.textContent)).toEqual(['disabled', 'disabled']);
  });

  it('shows agents read-only to someone who may not change them', async () => {
    grantable = [];
    await openAgents();
    expect(rows()).toHaveLength(2);
    expect(buttons('Create agent')).toHaveLength(0);
    expect(buttons('Disable')).toHaveLength(0);
    act(() => (rows()[0].querySelector('button') as HTMLButtonElement).click());
    expect(tokenRows('ci-deploy')).toHaveLength(2);
    expect(buttons('Revoke')).toHaveLength(0);
    expect(buttons('Create token')).toHaveLength(0);
  });

  it('says so once the server refuses the list', async () => {
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) =>
      url.endsWith('/agents') ? problem(403, 'permission_denied', 'no') : route(url, init));
    await openAgents();
    expect(container.textContent).toContain('Agents aren’t shown to your role');
    expect(rows()).toHaveLength(0);
  });

  it('opens the first People section when the path names one that lives elsewhere', async () => {
    history.replaceState(null, '', '/admin/people/agents');
    act(() => root.render(<OrganizationScreen organizationId={ORG} userId="u-1" context={context} />));
    await settle();
    expect(container.querySelector('h3')?.textContent).toBe('Organization members');
    expect(container.querySelector('[aria-label="Agents"]')).toBeNull();
    history.replaceState(null, '', '/');
  });
});
