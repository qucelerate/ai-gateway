import { beforeEach, describe, expect, it, vi } from 'vitest';
import { api, ApiProblemError, beginSignIn } from './api';

describe('admin session API', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    Object.defineProperty(document, 'cookie', {
      value: '__Host-gateway_csrf=token-123',
      configurable: true,
    });
  });

  it('uses same-origin credentials and csrf for context', async () => {
    const fetcher = vi
      .spyOn(globalThis, 'fetch')
      .mockResolvedValue(new Response(JSON.stringify({ role: 'Admin' }), { status: 200 }));
    await api.context('org-1');
    const [, options] = fetcher.mock.calls[0];
    expect(options).toEqual(expect.objectContaining({ credentials: 'same-origin', method: 'POST' }));
    expect((options?.headers as Headers).get('X-CSRF-Token')).toBe('token-123');
    expect(options?.body).toBe(JSON.stringify({ organization_id: 'org-1', team_id: null }));
  });

  it('sends csrf on logout and preserves a forbidden result', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(JSON.stringify({ error: 'csrf_origin_or_token_required' }), { status: 403 }),
    );
    await expect(api.logout()).rejects.toMatchObject({ status: 403, code: 'csrf_origin_or_token_required' });
    const [, options] = fetcher.mock.calls[0];
    expect(options).toEqual(expect.objectContaining({ credentials: 'same-origin', method: 'POST' }));
    expect((options?.headers as Headers).get('X-CSRF-Token')).toBe('token-123');
  });

  it('turns unauthorized responses into a useful API error', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(JSON.stringify({ error: 'authentication_required' }), { status: 401 }),
    );
    await expect(api.me()).rejects.toMatchObject({ status: 401, code: 'authentication_required' });
  });

  it('fetches the OIDC URL and redirects the browser to it', async () => {
    const navigate = vi.fn();
    const fetcher = vi
      .fn()
      .mockResolvedValue(
        new Response(JSON.stringify({ authorization_url: 'https://login.example.test/authorize?state=abc' }), { status: 200 }),
      );
    await beginSignIn(navigate, fetcher);
    expect(fetcher).toHaveBeenCalledWith('/api/admin/v1/oidc/start', expect.objectContaining({ credentials: 'same-origin' }));
    expect(navigate).toHaveBeenCalledWith('https://login.example.test/authorize?state=abc');
  });

  it('decodes the existing organization response shape', async () => {
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(JSON.stringify([{ id: 'org-1', slug: 'acme', name: 'Acme' }]), { status: 200 }),
    );
    await expect(api.organizations()).resolves.toEqual([{ id: 'org-1', slug: 'acme', name: 'Acme' }]);
  });
});

describe('admin collection and mutation API', () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    Object.defineProperty(document, 'cookie', {
      value: '__Host-gateway_csrf=token-123',
      configurable: true,
    });
  });

  it('lists teams with page_size and page_token', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({
          items: [{ id: 'team-1', organization_id: 'org-1', name: 'Platform', version: 1 }],
          next_page_token: 'token-2',
        }),
        { status: 200 },
      ),
    );
    const page = await api.listTeams('org-1', { page_size: 10, page_token: 'token-1' });
    expect(fetcher).toHaveBeenCalledWith(
      '/api/admin/v1/organizations/org-1/teams?page_size=10&page_token=token-1',
      expect.objectContaining({ credentials: 'same-origin' }),
    );
    expect(page.items).toHaveLength(1);
 expect(page.next_page_token).toBe('token-2');
 });

 it('lists audit events with opaque cursor and filters', async () => {
 const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(JSON.stringify({ items: [], next_page_token: 'opaque/cursor' }), { status: 200 }));
 await api.listAuditEvents('org-1', { page_size: 25, page_token: 'opaque/cursor', actor: 'user-1', action: 'team.updated', target_type: 'team', target_id: 'team-1', outcome: 'succeeded', occurred_after: '2026-01-01T00:00:00Z', occurred_before: '2026-01-02T00:00:00Z' });
 expect(fetcher).toHaveBeenCalledWith('/api/admin/v1/organizations/org-1/audit-events?page_size=25&page_token=opaque%2Fcursor&actor=user-1&action=team.updated&target_type=team&target_id=team-1&outcome=succeeded&occurred_after=2026-01-01T00%3A00%3A00Z&occurred_before=2026-01-02T00%3A00%3A00Z', expect.objectContaining({ credentials: 'same-origin' }));
 });

  it('creates a team with an Idempotency-Key header and body', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({ status: 'created', resource_id: 'team-1', version: 1, replayed: false }),
        { status: 201, headers: { 'x-request-id': 'req-1' } },
      ),
    );
    const result = await api.createTeam('org-1', 'Platform');
    const [, options] = fetcher.mock.calls[0];
    expect(options?.method).toBe('POST');
    expect(options?.body).toBe(JSON.stringify({ name: 'Platform' }));
    expect((options?.headers as Headers).get('Idempotency-Key')).toBeTruthy();
    expect((options?.headers as Headers).get('X-CSRF-Token')).toBe('token-123');
    expect(result.meta.requestId).toBe('req-1');
    expect(result.data.status).toBe('created');
  });

  it('updates a team with If-Match derived from the resource version', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({ status: 'updated', resource_id: 'team-1', version: 2, replayed: false }),
        { status: 200 },
      ),
    );
    await api.updateTeam('org-1', 'team-1', 'Platform Eng', 1);
    const [, options] = fetcher.mock.calls[0];
    expect(options?.method).toBe('PATCH');
    expect(options?.body).toBe(JSON.stringify({ name: 'Platform Eng' }));
    expect((options?.headers as Headers).get('If-Match')).toBe('"v1"');
  });

  it('deletes a team with If-Match and Idempotency-Key', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(new Response(null, { status: 204 }));
    await api.deleteTeam('org-1', 'team-1', 3);
    const [, options] = fetcher.mock.calls[0];
    expect(options?.method).toBe('DELETE');
    expect((options?.headers as Headers).get('If-Match')).toBe('"v3"');
    expect((options?.headers as Headers).get('Idempotency-Key')).toBeTruthy();
  });

  it('creates an org member using backend MembershipRole JSON vocabulary', async () => {
    for (const wire of ['Owner', 'Admin', 'Member', 'Auditor', 'ReadOnly'] as const) {
      vi.restoreAllMocks();
      const spy = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
        new Response(
          JSON.stringify({ status: 'created', resource_id: 'user-1', version: 1, replayed: false }),
          { status: 201 },
        ),
      );
      await api.createOrgMember('org-1', 'user-1', wire);
      const [, options] = spy.mock.calls[0];
      expect(JSON.parse(options?.body as string)).toEqual({ user_id: 'user-1', role: wire });
    }
  });

  it('updates an org member using backend MembershipRole JSON vocabulary', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({ status: 'updated', resource_id: 'user-1', version: 2, replayed: false }),
        { status: 200 },
      ),
    );
    await api.updateOrgMember('org-1', 'user-1', { role: 'ReadOnly' }, 1);
    const [, options] = fetcher.mock.calls[0];
    expect(JSON.parse(options?.body as string)).toEqual({ role: 'ReadOnly' });
  });

  it('creates and updates team members using backend MembershipRole JSON vocabulary', async () => {
    let fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({ status: 'created', resource_id: 'user-1', version: 1, replayed: false }),
        { status: 201 },
      ),
    );
    await api.createTeamMember('org-1', 'team-1', 'user-1', 'Auditor');
    let [, options] = fetcher.mock.calls[0];
    expect(JSON.parse(options?.body as string)).toEqual({ user_id: 'user-1', role: 'Auditor' });

    vi.restoreAllMocks();
    fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({ status: 'updated', resource_id: 'user-1', version: 2, replayed: false }),
        { status: 200 },
      ),
    );
    await api.updateTeamMember('org-1', 'team-1', 'user-1', { role: 'Admin' }, 1);
    [, options] = fetcher.mock.calls[0];
    expect(JSON.parse(options?.body as string)).toEqual({ role: 'Admin' });
  });

  it('creates an API key with scopes and expiry', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({
          metadata: {
            id: 'key-1',
            organization_id: 'org-1',
            service_account_id: 'sa-1',
            successor_of: null,
            public_prefix: 'gw_deadbeef',
            scopes: ['Inference', 'CatalogRead'],
            status: 'active',
            expires_at: '2026-12-31T23:59:59Z',
            revoked_at: null,
            disabled_at: null,
          },
          secret: 'gw_plaintext-secret',
          replay: false,
          error: null,
        }),
        { status: 201, headers: { 'x-request-id': 'req-key' } },
      ),
    );
    const result = await api.createApiKey('org-1', 'sa-1', {
      scopes: ['Inference', 'CatalogRead'],
      expires_at: '2026-12-31T23:59:59Z',
    });
    const [, options] = fetcher.mock.calls[0];
    expect(options?.method).toBe('POST');
    expect(options?.body).toBe(
      JSON.stringify({ scopes: ['Inference', 'CatalogRead'], expires_at: '2026-12-31T23:59:59Z' }),
    );
    expect((options?.headers as Headers).get('Idempotency-Key')).toBeTruthy();
    expect(result.data.secret).toBe('gw_plaintext-secret');
    expect(result.meta.requestId).toBe('req-key');
  });

  it('revokes an API key with If-Match', async () => {
    const fetcher = vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(
        JSON.stringify({
          id: 'key-1',
          status: 'revoked',
          version: 2,
        }),
        { status: 200 },
      ),
    );
    await api.revokeApiKey('org-1', 'sa-1', 'key-1', 1);
    const [, options] = fetcher.mock.calls[0];
    expect(options?.method).toBe('POST');
    expect((options?.headers as Headers).get('If-Match')).toBe('"v1"');
  });

  it('decodes application/problem+json and captures the request id', async () => {
    const problem = {
      type: 'https://gateway.invalid/problems/forbidden',
      code: 'forbidden',
      title: 'Forbidden',
      status: 403,
      detail: 'administrative permission denied',
      instance: '/api/admin/v1/organizations/org-1/teams',
      request_id: 'req-403',
    };
    vi.spyOn(globalThis, 'fetch').mockResolvedValue(
      new Response(JSON.stringify(problem), {
        status: 403,
        headers: {
          'content-type': 'application/problem+json',
          'x-request-id': 'req-403',
        },
      }),
    );
    await expect(api.listTeams('org-1')).rejects.toSatisfy((error: unknown) => {
      if (!(error instanceof ApiProblemError)) return false;
      expect(error.status).toBe(403);
      expect(error.problem.code).toBe('forbidden');
      expect(error.problem.detail).toBe('administrative permission denied');
      expect(error.problem.request_id).toBe('req-403');
      return true;
    });
  });

  it('decodes 409/412/428 problem bodies so callers can preserve form input', async () => {
    for (const status of [409, 412, 428]) {
      vi.restoreAllMocks();
      const problem = {
        type: 'about:blank',
        code: 'version_conflict',
        title: 'Conflict',
        status,
        detail: `status ${status}`,
        instance: '/api/admin/v1/organizations/org-1/teams/team-1',
        request_id: `req-${status}`,
      };
      vi.spyOn(globalThis, 'fetch').mockResolvedValue(
        new Response(JSON.stringify(problem), { status, headers: { 'content-type': 'application/problem+json' } }),
      );
      await expect(api.updateTeam('org-1', 'team-1', 'New', 1)).rejects.toSatisfy((error: unknown) => {
        if (!(error instanceof ApiProblemError)) return false;
        expect(error.status).toBe(status);
        expect(error.problem.code).toBe('version_conflict');
        return true;
      });
    }
  });
});
