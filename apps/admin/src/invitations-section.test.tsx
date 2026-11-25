import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { OrganizationScreen } from './organization-screen';
import type { Invitation, InvitationRole } from './api';
import { openSection } from './test-route';

const ORG = 'b6c2a7a4-6f3e-4ad8-8f43-2a9d1d4f5e01';
const LINK = 'https://gateway.example/api/admin/v1/invitations/accept?token=AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_abcde';
const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }, status);
const invitation = (id: string, status: Invitation['status'], role: InvitationRole = 'member'): Invitation => ({
  id, role, status,
  created_at: '2026-10-10T10:00:00Z', expires_at: '2026-10-13T10:00:00Z',
  created_by: 'u-1', accepted_at: null, accepted_by: null,
  revoked_at: status === 'revoked' ? '2026-10-11T10:00:00Z' : null,
});
const context = { user_id: 'u-1', organization_id: ORG, team_id: null, role: 'Owner' };

describe('organization invitations', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let listing: () => Response;
  let pages: Record<string, { items: Invitation[]; next_page_token: string | null }>;
  let grantable: InvitationRole[];
  let route: (url: string, init?: RequestInit) => Promise<Response>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    grantable = ['owner', 'admin', 'auditor', 'member', 'read-only'];
    pages = {
      first: { items: [invitation('i-1', 'pending'), invitation('i-2', 'accepted', 'admin'), invitation('i-3', 'revoked')], next_page_token: null },
    };
    listing = () => {
      return json({ ...pages.first, grantable_roles: grantable });
    };
    route = async (url: string, init?: RequestInit) => {
      if (url.includes('/invitations/i-1/revoke') && init?.method === 'POST') {
        pages.first.items[0] = invitation('i-1', 'revoked');
        return json({ ...invitation('i-1', 'revoked') }, 200, { 'x-request-id': 'req-revoke' });
      }
      if (url.endsWith('/invitations') && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        return json({
          invitation: { ...invitation('i-9', 'pending', body.role) },
          link: LINK, replay: false, error: null,
        }, 201, { 'x-request-id': 'req-create' });
      }
      if (url.includes('/invitations?') && url.includes('page_token=next')) return json({ ...pages.second, grantable_roles: grantable });
      if (url.includes('/invitations')) return listing();
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
  const render = async () => {
    act(() => root.render(<OrganizationScreen organizationId={ORG} userId="u-1" context={context} />));
    await settle();
  };
  const tab = (name: string) =>
    Array.from(container.querySelectorAll('[role="tab"]')).find(node => node.textContent === name) as HTMLButtonElement | undefined;
  const buttons = (text: string) =>
    Array.from(document.querySelectorAll('button')).filter(node => node.textContent === text) as HTMLButtonElement[];
  const openInvitations = async () => {
    await render();
    openSection('people', 'invitations');
    await settle();
  };
  const rows = () => Array.from(container.querySelectorAll('[aria-label="Invitations"] .data-row:not(.head)'));
  const posts = () => fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === 'POST');

  it('lists invitations with their status, and offers Revoke only on pending ones', async () => {
    await openInvitations();
    expect(rows().map(row => row.querySelector('.badge')?.textContent)).toEqual(['pending', 'accepted', 'revoked']);
    expect(rows()[1].textContent).toContain('Admin');
    expect(rows().map(row => row.textContent?.includes('Revoke'))).toEqual([true, false, false]);
    expect(container.textContent).toContain('you send the link to the person yourself');
  });

  it('creates a link in the chosen role and validity, and shows it once with a copy button', async () => {
    await openInvitations();
    act(() => buttons('Invite')[0].click());
    const role = document.getElementById('invitation-role') as HTMLSelectElement;
    expect(Array.from(role.options).map(option => option.value)).toEqual(grantable);
    expect(role.value).toBe('member');
    act(() => {
      role.value = 'auditor';
      role.dispatchEvent(new Event('change', { bubbles: true }));
      const validity = document.getElementById('invitation-validity') as HTMLSelectElement;
      validity.value = '168';
      validity.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => buttons('Create link')[0].click());
    await settle();

    const [url, init] = posts()[0] as [string, RequestInit];
    expect(url).toBe(`/api/admin/v1/organizations/${ORG}/invitations`);
    expect(JSON.parse(String(init.body))).toEqual({ role: 'auditor', valid_hours: 168 });
    expect(new Headers(init.headers).get('Idempotency-Key')).toBeTruthy();

    const panel = container.querySelector('.secret-panel')!;
    expect(panel.querySelector('[aria-label="Invitation link"]')?.textContent).toBe(LINK);
    expect(panel.querySelector('[aria-label="Copy invitation link"]')).not.toBeNull();
    expect(panel.textContent).toContain('Invitation link · Auditor');
    expect(panel.textContent).toContain('send it to the person you are inviting');
    expect(panel.textContent).toContain('The gateway does not send it for you');
    expect(panel.textContent).toContain('only time the link is shown');
    // The dialog closed and the list reloaded.
    expect(document.getElementById('invitation-role')).toBeNull();
    expect(fetchMock.mock.calls.filter(([u]) => String(u).includes('/invitations?')).length).toBeGreaterThan(1);

    // Once dismissed, it is gone from the page.
    act(() => buttons('Done')[0].click());
    expect(container.textContent).not.toContain(LINK);
    expect(document.body.textContent).not.toContain('AbCdEfGh');
  });

  it('keeps the same idempotency key when the same dialog retries', async () => {
    let fail = true;
    const base = route;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/invitations') && init?.method === 'POST' && fail) {
        fail = false;
        return problem(503, 'service_unavailable', 'Invitation store unavailable.');
      }
      return base(url, init);
    });
    await openInvitations();
    act(() => buttons('Invite')[0].click());
    act(() => buttons('Create link')[0].click());
    await settle();
    expect(document.body.textContent).toContain('Invitation store unavailable.');
    act(() => buttons('Create link')[0].click());
    await settle();
    const keys = posts().map(([, init]) => new Headers((init as RequestInit).headers).get('Idempotency-Key'));
    expect(keys).toHaveLength(2);
    expect(keys[0]).toBe(keys[1]);
    expect(container.querySelector('.secret-panel')?.textContent).toContain(LINK);
  });

  it('uses a new idempotency key once the role or validity changes after a failure', async () => {
    let fail = true;
    const base = route;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/invitations') && init?.method === 'POST' && fail) {
        fail = false;
        return problem(503, 'service_unavailable', 'Invitation store unavailable.');
      }
      return base(url, init);
    });
    await openInvitations();
    act(() => buttons('Invite')[0].click());
    act(() => buttons('Create link')[0].click());
    await settle();
    act(() => {
      const role = document.getElementById('invitation-role') as HTMLSelectElement;
      role.value = 'admin';
      role.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => buttons('Create link')[0].click());
    await settle();
    const sent = posts().map(([, init]) => ({
      key: new Headers((init as RequestInit).headers).get('Idempotency-Key'),
      body: JSON.parse(String((init as RequestInit).body)),
    }));
    expect(sent).toHaveLength(2);
    expect(sent[1].body.role).toBe('admin');
    expect(sent[1].key).not.toBe(sent[0].key);
    expect(container.querySelector('.secret-panel')?.textContent).toContain(LINK);
  });

  it('offers an admin only the roles at or below their own, and shows a refusal', async () => {
    grantable = ['admin', 'auditor', 'member', 'read-only'];
    await openInvitations();
    act(() => buttons('Invite')[0].click());
    const role = document.getElementById('invitation-role') as HTMLSelectElement;
    expect(Array.from(role.options).map(option => option.value)).not.toContain('owner');
    fetchMock.mockImplementationOnce(async () =>
      problem(403, 'escalation_denied', 'you can only invite people in a role at or below your own'));
    act(() => buttons('Create link')[0].click());
    await settle();
    expect(document.body.textContent).toContain('at or below your own');
    expect(container.querySelector('.secret-panel')).toBeNull();
  });

  it('revokes a pending invitation after confirming', async () => {
    await openInvitations();
    act(() => buttons('Revoke')[0].click());
    await settle();
    expect(document.body.textContent).toContain('Its link stops working at once');
    // Nothing is sent before the confirmation.
    expect(posts()).toHaveLength(0);
    // The dialog's confirm button comes after the row's.
    act(() => buttons('Revoke')[1].click());
    await settle();
    const [url, init] = posts()[0] as [string, RequestInit];
    expect(url).toBe(`/api/admin/v1/organizations/${ORG}/invitations/i-1/revoke`);
    expect(new Headers(init.headers).get('Idempotency-Key')).toBeTruthy();
    // The confirmation closed and the list reloaded with the new status.
    expect(document.body.textContent).not.toContain('Its link stops working at once');
    expect(rows()[0].querySelector('.badge')?.textContent).toBe('revoked');
    expect(buttons('Revoke')).toHaveLength(0);
  });

  it('keeps the same idempotency key when a revoke is retried', async () => {
    let fail = true;
    const base = route;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.includes('/revoke') && fail) {
        fail = false;
        return problem(503, 'service_unavailable', 'Invitation store unavailable.');
      }
      return base(url, init);
    });
    await openInvitations();
    act(() => buttons('Revoke')[0].click());
    act(() => buttons('Revoke')[1].click());
    await settle();
    expect(document.body.textContent).toContain('Invitation store unavailable.');
    act(() => buttons('Revoke')[1].click());
    await settle();
    const keys = posts().map(([, init]) => new Headers((init as RequestInit).headers).get('Idempotency-Key'));
    expect(keys).toHaveLength(2);
    expect(keys[0]).toBe(keys[1]);
    expect(rows()[0].querySelector('.badge')?.textContent).toBe('revoked');
  });

  it('keeps the section after a failed load, with a retry', async () => {
    let fail = true;
    listing = () => {
      if (fail) {
        fail = false;
        return problem(503, 'service_unavailable', 'Invitation store unavailable.');
      }
      return json({ ...pages.first, grantable_roles: grantable });
    };
    await openInvitations();
    expect(container.textContent).toContain('Invitation store unavailable.');
    act(() => buttons('Try again')[0].click());
    await settle();
    expect(rows()).toHaveLength(3);
    expect(buttons('Invite')).toHaveLength(1);
  });

  it('pages through invitations', async () => {
    pages.first = { items: [invitation('i-1', 'pending')], next_page_token: 'next' };
    pages.second = { items: [invitation('i-4', 'expired', 'read-only')], next_page_token: null };
    await openInvitations();
    expect(rows()).toHaveLength(1);
    act(() => buttons('Load more')[0].click());
    await settle();
    expect(rows().map(row => row.querySelector('.badge')?.textContent)).toEqual(['pending', 'expired']);
    expect(rows()[1].textContent).toContain('Read-only');
  });

  it('shows the list without Invite or Revoke to someone who may only read it', async () => {
    grantable = [];
    await openInvitations();
    expect(rows()).toHaveLength(3);
    expect(buttons('Invite')).toHaveLength(0);
    expect(buttons('Revoke')).toHaveLength(0);
  });

  it('hides the section from someone who may not read invitations', async () => {
    listing = () => problem(403, 'forbidden', 'administrative permission denied');
    await render();
    openSection('people', 'invitations');
    await settle();
    // The section falls back to Members.
    expect(container.querySelector('h3')?.textContent).toBe('Organization members');
    expect(container.querySelector('[aria-label="Invitations"]')).toBeNull();
    history.replaceState(null, '', '/');
  });
});
