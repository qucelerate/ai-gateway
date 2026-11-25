import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { CreateOwnOrganizationDialog, DeletionRequestSection, OrganizationsScreen, eventText } from './organizations-screen';

const json = (body: unknown, status = 200) =>
  new Response(status === 204 ? null : JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }, status);
const organization = (overrides: Record<string, unknown> = {}) => ({
  id: 'org-1', slug: 'acme', name: 'Acme', status: 'active', created_at: '2026-11-01T10:00:00Z', suspended_at: null,
  deleted_at: null, deletion_requested_at: null, members: 3, ...overrides,
});
const event = {
  id: 'e-1', occurred_at: '2026-11-13T10:00:00Z', actor_user_id: 'u-1', actor_email: 'ops@example.com',
  action: 'organization.suspended', organization_id: 'org-1', target_user_id: null, detail: { slug: 'acme', name: 'Acme' },
};

describe('organizations screen', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const render = async (element: React.ReactElement) => { await act(async () => { root.render(element); }); };
  const input = (id: string) => document.getElementById(id) as HTMLInputElement;
  const type = (id: string, value: string) => act(() => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input(id), value);
    input(id).dispatchEvent(new Event('input', { bubbles: true }));
  });
  const button = (label: string) => Array.from(document.querySelectorAll('button')).find(b => b.textContent?.trim() === label);
  const click = async (label: string) => {
    expect(button(label), label).toBeTruthy();
    await act(async () => { button(label)!.click(); });
  };
  const submit = async (label: string) => {
    const target = Array.from(document.querySelectorAll('form button')).filter(b => b.textContent?.trim() === label).pop() as HTMLButtonElement | undefined;
    expect(target, label).toBeTruthy();
    await act(async () => { target!.closest('form')!.requestSubmit(); });
  };
  const call = (n: number) => ({
    url: String(fetchMock.mock.calls[n][0]),
    method: (fetchMock.mock.calls[n][1] as RequestInit | undefined)?.method ?? 'GET',
    body: (fetchMock.mock.calls[n][1] as RequestInit | undefined)?.body,
  });
  const listing = (items: unknown[], selfServe = false) => {
    fetchMock.mockResolvedValueOnce(json({ items, self_serve: selfServe }));
    fetchMock.mockResolvedValueOnce(json({ items: [event] }));
  };

  it('lists organizations with their state and the deployment audit', async () => {
    listing([organization(), organization({ id: 'org-2', slug: 'beta', name: 'Beta', status: 'suspended', deletion_requested_at: '2026-11-12T10:00:00Z' })]);
    await render(<OrganizationsScreen />);
    expect(call(0).url).toBe('/api/admin/v1/deployment/organizations');
    expect(call(1).url).toContain('/api/admin/v1/deployment/audit');
    const text = container.textContent ?? '';
    expect(text).toContain('Acme');
    expect(text).toContain('Suspended');
    expect(text).toContain('deletion requested');
    expect(text).toContain('ops@example.com');
    expect(text).toContain('suspended “Acme”');
    expect(button('Suspend')).toBeTruthy();
    expect(button('Unsuspend')).toBeTruthy();
    expect(button('Delete')).toBeTruthy();
  });

  it('suspends an active organization and reloads', async () => {
    listing([organization()]);
    await render(<OrganizationsScreen />);
    fetchMock.mockResolvedValueOnce(json(organization({ status: 'suspended' })));
    listing([organization({ status: 'suspended' })]);
    await click('Suspend');
    expect(call(2)).toMatchObject({ url: '/api/admin/v1/deployment/organizations/org-1/suspend', method: 'POST' });
    expect(button('Unsuspend')).toBeTruthy();
  });

  it('deletes a suspended organization only once its short name is typed', async () => {
    const stored = { provider_credentials: 2, cloud_connections: 1, webhook_signing_secrets: 0, identity_provider_client_secret: true };
    listing([organization({ status: 'suspended', stored })]);
    await render(<OrganizationsScreen />);
    await click('Delete');
    const dialog = document.body.textContent ?? '';
    expect(dialog).toContain('2 provider credentials');
    expect(dialog).toContain('1 cloud connection');
    expect(dialog).not.toContain('webhook signing secret');
    expect(dialog).toContain('its SSO client secret');
    expect(dialog).toContain('These stay stored, encrypted, so the organization can be restored. If you won’t restore it, revoke them at each');
    expect(dialog).not.toContain('destroy');
    const confirm = Array.from(document.querySelectorAll('button')).filter(b => b.textContent?.trim() === 'Delete').pop()!;
    expect(confirm.disabled).toBe(true);
    type('delete-organization-confirm', 'acme');
    expect(confirm.disabled).toBe(false);
    fetchMock.mockResolvedValueOnce(json(organization({ status: 'deleted' })));
    listing([]);
    await act(async () => { confirm.closest('form')!.requestSubmit(); });
    expect(call(2)).toMatchObject({ url: '/api/admin/v1/deployment/organizations/org-1/delete', method: 'POST' });
    expect(JSON.parse(String(call(2).body))).toEqual({ confirm_slug: 'acme' });
  });

  it('asks for a new short name when restoring finds the old one taken', async () => {
    fetchMock.mockResolvedValueOnce(json({ items: [organization()], self_serve: false }));
    fetchMock.mockResolvedValueOnce(json({ items: [] }));
    await render(<OrganizationsScreen />);
    listing([organization({ status: 'deleted' })]);
    await act(async () => { input('show-deleted').click(); });
    expect(call(2).url).toBe('/api/admin/v1/deployment/organizations?deleted=true');
    await click('Restore');
    fetchMock.mockResolvedValueOnce(problem(409, 'slug_taken', 'Another organization uses acme.'));
    await submit('Restore');
    expect(input('restore-organization-slug')).toBeTruthy();
    type('restore-organization-slug', 'acme-old');
    fetchMock.mockResolvedValueOnce(json(organization({ slug: 'acme-old', status: 'suspended' })));
    listing([]);
    await submit('Restore');
    expect(JSON.parse(String(call(5).body))).toEqual({ slug: 'acme-old' });
  });

  it('creates an organization with an invited first owner and shows the link once', async () => {
    listing([]);
    await render(<OrganizationsScreen />);
    await click('Create organization');
    type('new-organization-name', 'Gamma Labs');
    expect(input('new-organization-slug').value).toBe('gamma-labs');
    type('new-organization-owner', 'grace@gamma.example');
    fetchMock.mockResolvedValueOnce(json({
      organization: organization({ id: 'org-3', slug: 'gamma-labs', name: 'Gamma Labs' }),
      owner_link: { kind: 'invitation', email: 'grace@gamma.example', link: 'http://localhost/invitations/accept#token=t', expires_at: '2026-11-20T10:00:00Z' },
    }, 201));
    listing([]);
    await submit('Create');
    expect(JSON.parse(String(call(2).body))).toEqual({
      name: 'Gamma Labs', slug: 'gamma-labs', owner: { email: 'grace@gamma.example', method: 'invitation' },
    });
    expect(document.body.textContent).toContain('http://localhost/invitations/accept#token=t');
    expect(document.body.textContent).toContain('not shown again');
  });

  it('lets an owner ask for deletion and withdraw it', async () => {
    fetchMock.mockResolvedValueOnce(json({ requested_at: null }));
    await render(<DeletionRequestSection organizationId="org-1" canWrite />);
    expect(call(0).url).toBe('/api/admin/v1/organizations/org-1/deletion-request');
    fetchMock.mockResolvedValueOnce(json({ requested_at: '2026-11-13T10:00:00Z' }));
    await click('Ask for deletion');
    expect(call(1).method).toBe('PUT');
    expect(container.textContent).toContain('You asked for its deletion');
    fetchMock.mockResolvedValueOnce(json({ requested_at: null }));
    await click('Withdraw the request');
    expect(call(2).method).toBe('DELETE');
  });

  it('keeps the list of the latest filter when an earlier load ends last', async () => {
    let finishFirst: (response: Response) => void = () => {};
    fetchMock.mockImplementationOnce(() => new Promise<Response>(resolve => { finishFirst = resolve; }));
    fetchMock.mockResolvedValueOnce(json({ items: [] }));
    await render(<OrganizationsScreen />);
    listing([organization({ status: 'deleted', name: 'Gone' })]);
    await act(async () => { input('show-deleted').click(); });
    expect(container.textContent).toContain('Gone');
    await act(async () => { finishFirst(json({ items: [organization({ name: 'Stale' })], self_serve: false })); });
    expect(container.textContent).toContain('Gone');
    expect(container.textContent).not.toContain('Stale');
  });

  it('shows what a deleted organization still stores, and says nothing when it stores nothing', async () => {
    listing([
      organization({ status: 'deleted', stored: { provider_credentials: 0, cloud_connections: 0, webhook_signing_secrets: 3, identity_provider_client_secret: false } }),
      organization({ id: 'org-2', slug: 'beta', name: 'Beta', status: 'deleted', stored: { provider_credentials: 0, cloud_connections: 0, webhook_signing_secrets: 0, identity_provider_client_secret: false } }),
    ]);
    await render(<OrganizationsScreen />);
    const hints = Array.from(container.querySelectorAll('.stored-secrets')).map(node => node.textContent);
    expect(hints).toHaveLength(1);
    expect(hints[0]).toContain('still stored, encrypted: 3 webhook signing secrets');
    expect(hints[0]).toContain('Revoke them at each provider');
  });

  it('explains that an organization session cannot create organizations', async () => {
    await render(<CreateOwnOrganizationDialog open organizationSession onClose={vi.fn()} onCreated={vi.fn()} />);
    expect(document.body.textContent).toContain('Sign in with this gateway’s own sign-in first.');
    expect(document.getElementById('own-organization-name')).toBeNull();
  });

  it('creates an organization of your own, and explains a refusal for an organization session', async () => {
    const onCreated = vi.fn();
    await render(<CreateOwnOrganizationDialog open organizationSession={false} onClose={vi.fn()} onCreated={onCreated} />);
    type('own-organization-name', 'My Lab');
    fetchMock.mockResolvedValueOnce(problem(403, 'deployment_sign_in_required', 'sign in with the gateway'));
    await submit('Create');
    expect(call(0)).toMatchObject({ url: '/api/admin/v1/organizations', method: 'POST' });
    expect(JSON.parse(String(call(0).body))).toEqual({ name: 'My Lab', slug: 'my-lab' });
    expect(document.body.textContent).toContain('Sign in with this gateway’s own sign-in first.');
    expect(onCreated).not.toHaveBeenCalled();
  });

  it('describes unknown audit actions by name', () => {
    expect(eventText({ ...event, action: 'something.else' })).toBe('something.else');
    expect(eventText({ ...event, action: 'organization.deleted' })).toBe('deleted “Acme”');
  });
});
