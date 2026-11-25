import { describe, expect, it, vi } from 'vitest';
import { ApiError } from './api';
import { logoutSession, OrganizationContextManager } from './context-state';

const organizations = [
  { id: 'org-1', slug: 'one', name: 'One' },
  { id: 'org-2', slug: 'two', name: 'Two' },
];
const context = (organizationId: string) => ({ user_id: 'user-1', organization_id: organizationId, team_id: null, role: 'Admin' });

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

describe('organization context interaction state', () => {
  it('preserves the confirmed context and reverts the selector after failure', async () => {
    const request = vi.fn()
      .mockResolvedValueOnce(context('org-1'))
      .mockRejectedValueOnce(new ApiError(500, 'identity_store_unavailable'));
    const manager = new OrganizationContextManager(request);
    manager.setOrganizations(organizations);
    await manager.select('org-1');
    await manager.select('org-2');
    expect(manager.getSnapshot()).toMatchObject({
      confirmedOrganization: organizations[0],
      confirmedContext: context('org-1'),
      selectedOrganizationId: 'org-1',
      retryOrganizationId: 'org-2',
      state: 'error',
    });
  });

  it('ignores an older response that arrives after a newer selection', async () => {
    const first = deferred<ReturnType<typeof context>>();
    const second = deferred<ReturnType<typeof context>>();
    const request = vi.fn().mockReturnValueOnce(first.promise).mockReturnValueOnce(second.promise);
    const manager = new OrganizationContextManager(request);
    manager.setOrganizations(organizations);
    const firstSelection = manager.select('org-1');
    const secondSelection = manager.select('org-2');
    second.resolve(context('org-2'));
    await secondSelection;
    first.resolve(context('org-1'));
    await firstSelection;
    expect(manager.getSnapshot().confirmedOrganization).toEqual(organizations[1]);
  });

  it('transitions to sign-in when context returns 401', async () => {
    const manager = new OrganizationContextManager(vi.fn().mockRejectedValue(new ApiError(401, 'authentication_required')));
    manager.setOrganizations(organizations);
    await manager.select('org-1');
    expect(manager.getSnapshot().state).toBe('unauthenticated');
  });

  it('distinguishes context denial from a CSRF 403', async () => {
    const denied = new OrganizationContextManager(vi.fn().mockRejectedValue(new ApiError(403, 'organization_or_team_context_denied')));
    denied.setOrganizations(organizations);
    await denied.select('org-1');
    expect(denied.getSnapshot().state).toBe('permission');

    const csrf = new OrganizationContextManager(vi.fn().mockRejectedValue(new ApiError(403, 'csrf_origin_or_token_required')));
    csrf.setOrganizations(organizations);
    await csrf.select('org-1');
    expect(csrf.getSnapshot().state).toBe('error');
  });

  it('retries the rejected organization without changing confirmed context early', async () => {
    const request = vi.fn().mockRejectedValueOnce(new ApiError(503, 'temporary')).mockResolvedValueOnce(context('org-1'));
    const manager = new OrganizationContextManager(request);
    manager.setOrganizations(organizations);
    await manager.select('org-1');
    const retry = manager.retry();
    expect(manager.getSnapshot().state).toBe('pending');
    await retry;
    expect(manager.getSnapshot().confirmedOrganization).toEqual(organizations[0]);
  });

  it('reports logout success and normalized logout failure outcomes', async () => {
    await expect(logoutSession(async () => undefined)).resolves.toMatchObject({ ok: true, error: null });
    await expect(logoutSession(async () => { throw new Error('offline'); })).resolves.toMatchObject({ ok: false, error: { code: 'network_error' } });
  });
});
