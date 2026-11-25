import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import type { IdentityProvider, IdentityProviderConfiguration } from './api';
import { useMutationState } from './mutation-state';
import { OrganizationScreen } from './organization-screen';
import { openSection } from './test-route';
import {
  SignInSection,
  offersSignInChanges,
  parseScopes,
  signInTestMessage,
  testedOrganization,
} from './sign-in-section';

const ORG = 'b6c2a7a4-6f3e-4ad8-8f43-2a9d1d4f5e01';
const ISSUER = 'https://login.microsoftonline.com/tenant/v2.0';
const REDIRECT = 'https://gateway.example/api/admin/v1/oidc/callback';
const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }, status);
const configuration = (version: number, issuer = ISSUER): IdentityProviderConfiguration => ({
  configuration_version: version,
  issuer,
  client_id: 'client-a',
  client_secret_set: true,
  extra_scopes: ['groups'],
  created_at: '2026-10-10T10:00:00Z',
  created_by: 'u-1',
  tested_at: null,
});
const notConfigured: IdentityProvider = {
  status: 'not_configured', enabled: false, active: null, pending: null,
  last_tested_at: null, last_tested_by: null, redirect_uri: REDIRECT, version: 0,
};
const pending: IdentityProvider = { ...notConfigured, status: 'pending', pending: configuration(1), version: 1 };
const active: IdentityProvider = {
  ...notConfigured, status: 'active', enabled: true,
  active: { ...configuration(1), tested_at: '2026-10-11T10:00:00Z' },
  last_tested_at: '2026-10-11T10:00:00Z', last_tested_by: 'u-1', version: 2,
};

describe('the sign-in section', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let navigate: ReturnType<typeof vi.fn<(url: string) => void>>;
  let reloads: number;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    navigate = vi.fn<(url: string) => void>();
    reloads = 0;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/identity-provider/test') && init?.method === 'POST') {
        return json({ authorization_url: 'https://login.example/authorize?state=s' });
      }
      if (url.endsWith('/identity-provider') && init?.method === 'PUT') return json(pending, 201, { etag: '"v1"' });
      if (url.endsWith('/identity-provider') && init?.method === 'DELETE') return json({ ...notConfigured, version: 3 });
      return json({}, 404);
    });
    vi.stubGlobal('fetch', fetchMock);
    history.replaceState(null, '', '/admin/organization');
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  function Harness({ provider, role }: { provider: IdentityProvider; role: string }) {
    const mutation = useMutationState<unknown>();
    return (
      <SignInSection
        organizationId={ORG}
        provider={provider}
        loading={false}
        loadError={null}
        onReload={() => { reloads += 1; }}
        role={role}
        mutation={mutation}
        navigate={navigate}
      />
    );
  }
  const render = async (provider: IdentityProvider, role = 'Owner') => {
    act(() => root.render(<Harness provider={provider} role={role} />));
    await settle();
  };
  const input = (id: string) => document.getElementById(id) as HTMLInputElement | null;
  const type = (id: string, value: string) => {
    const field = input(id)!;
    act(() => {
      const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
      setter.call(field, value);
      field.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const buttons = (text: string) =>
    Array.from(document.querySelectorAll('button')).filter(node => node.textContent === text) as HTMLButtonElement[];
  const sent = (method: string) =>
    fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === method) as [string, RequestInit][];
  const submit = async () => {
    act(() => { buttons('Save')[0].click(); });
    await settle();
  };

  it('shows the status and the redirect URI to register, with a copy button', async () => {
    await render(active);
    expect(container.textContent).toContain('Active');
    expect(container.textContent).toContain(`${ISSUER} · client client-a`);
    expect(container.querySelector('code')?.textContent).toBe(REDIRECT);
    expect(container.querySelector('[aria-label="Copy redirect URI"]')).not.toBeNull();
    expect(container.textContent).toContain('Register exactly this');
  });

  it('saves a new provider, with its secret, as a change to test', async () => {
    await render(notConfigured);
    type('idp-issuer', ISSUER);
    type('idp-client-id', 'client-a');
    type('idp-client-secret', 's3cret');
    type('idp-scopes', 'groups, offline_access');
    expect(input('idp-client-secret')!.type).toBe('password');
    await submit();
    const [url, init] = sent('PUT')[0];
    expect(url).toBe(`/api/admin/v1/organizations/${ORG}/identity-provider`);
    expect(JSON.parse(String(init.body))).toEqual({
      issuer: ISSUER, client_id: 'client-a', client_secret: 's3cret', extra_scopes: ['groups', 'offline_access'],
    });
    const headers = new Headers(init.headers);
    expect(headers.get('If-Match')).toBe('*');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
    expect(reloads).toBe(1);
    // The secret is cleared from the form once saved.
    expect(input('idp-client-secret')?.value ?? '').toBe('');
    expect(container.textContent).toContain('Saving does not change how anyone signs in');
  });

  it('never shows the stored secret, and keeps it unless replaced', async () => {
    await render(pending);
    expect(input('idp-client-secret')).toBeNull();
    expect(container.textContent).toContain('A secret is stored. It is never shown.');
    expect(input('idp-issuer')!.value).toBe(ISSUER);
    type('idp-scopes', 'groups email-verified');
    await submit();
    let body = JSON.parse(String(sent('PUT')[0][1].body));
    expect(body).not.toHaveProperty('client_secret');
    expect(new Headers(sent('PUT')[0][1].headers).get('If-Match')).toBe('"v1"');

    // Replace secret asks for a new one, empty.
    act(() => buttons('Replace secret')[0].click());
    expect(input('idp-client-secret')!.value).toBe('');
    type('idp-client-secret', 'rotated');
    await submit();
    body = JSON.parse(String(sent('PUT')[1][1].body));
    expect(body.client_secret).toBe('rotated');
    expect(document.body.textContent).not.toContain('rotated');
  });

  it('asks for the secret again when the issuer or client changes', async () => {
    await render(pending);
    type('idp-issuer', 'https://idp.other.example.com');
    expect(input('idp-client-secret')!.required).toBe(true);
    expect(container.textContent).toContain('whenever the issuer or client ID changes');
  });

  it('keeps the form and shows the server explanation on a 409 or 403', async () => {
    fetchMock.mockImplementationOnce(async () =>
      problem(409, 'version_conflict', 'resource version conflict; current version is 2'));
    await render(pending);
    type('idp-client-id', 'client-b');
    type('idp-client-secret', 'new');
    await submit();
    expect(container.textContent).toContain('Someone changed this meanwhile');
    expect(input('idp-client-id')!.value).toBe('client-b');
    expect(input('idp-client-secret')!.value).toBe('new');
    expect(reloads).toBe(0);

    fetchMock.mockImplementationOnce(async () => problem(403, 'forbidden', 'administrative permission denied'));
    await submit();
    expect(container.textContent).toContain('Your role does not allow this');
    expect(input('idp-client-id')!.value).toBe('client-b');
  });

  it('shows an error when a save or disable cannot reach the gateway', async () => {
    fetchMock.mockImplementationOnce(async () => { throw new TypeError('Failed to fetch'); });
    await render(active);
    await submit();
    expect(container.textContent).toContain('The gateway could not be reached. The configuration may not have been saved.');
    expect(reloads).toBe(0);

    fetchMock.mockImplementationOnce(async () => json({ error: 'service_unavailable' }, 503));
    act(() => buttons('Disable')[0].click());
    await settle();
    act(() => buttons('Disable')[1].click());
    await settle();
    expect(document.body.textContent).toContain('Sign-in may not have been disabled.');
  });

  it('starts the test sign-in and sends the browser to the provider', async () => {
    await render(pending);
    expect(container.textContent).toContain('nobody else is signed in or added by the test');
    act(() => { buttons('Test and activate')[0].click(); });
    await settle();
    expect(sent('POST')[0][0]).toBe(`/api/admin/v1/organizations/${ORG}/identity-provider/test`);
    expect(navigate).toHaveBeenCalledWith('https://login.example/authorize?state=s');
  });

  it('explains a test that could not start, and keeps the page', async () => {
    fetchMock.mockImplementationOnce(async () => json({ error: 'oidc_provider_unavailable' }, 503));
    await render(pending);
    act(() => { buttons('Test and activate')[0].click(); });
    await settle();
    expect(navigate).not.toHaveBeenCalled();
    expect(container.textContent).toContain('does not name exactly the issuer you saved');
    expect(buttons('Test and activate')[0].disabled).toBe(false);
  });

  it('reports the outcome of a test sign-in once, from the URL', async () => {
    history.replaceState(null, '', `/admin/organization?organization=${ORG}&sign_in_test=activated`);
    expect(testedOrganization(location.search)).toBe(ORG);
    await render(active);
    expect(container.textContent).toContain('The test sign-in worked');
    expect(location.search).toBe('');
    expect(signInTestMessage('identity_provider_changed').ok).toBe(false);
    expect(signInTestMessage('identity_provider_changed').text).toContain('nothing was activated');
  });

  it('leaves another organization\'s test outcome for that organization', async () => {
    const other = 'c0ffee00-0000-4000-8000-000000000000';
    history.replaceState(null, '', `/admin/organization?organization=${other}&sign_in_test=activated`);
    await render(active);
    expect(container.textContent).not.toContain('The test sign-in worked');
    expect(location.search).toBe(`?organization=${other}&sign_in_test=activated`);
    expect(testedOrganization('?organization=x')).toBeNull();
  });

  it('locks the form while a save runs, so nothing typed meanwhile is lost', async () => {
    let finish: (response: Response) => void = () => {};
    fetchMock.mockImplementationOnce(() => new Promise<Response>(resolve => { finish = resolve; }));
    await render(pending);
    type('idp-client-id', 'client-b');
    type('idp-client-secret', 'new');
    act(() => { buttons('Save')[0].click(); });
    await settle();
    const fieldset = container.querySelector('fieldset')!;
    expect(fieldset.disabled).toBe(true);
    expect(input('idp-client-id')!.matches(':disabled')).toBe(true);
    await act(async () => { finish(json(pending, 200, { etag: '"v2"' })); });
    await settle();
    expect(container.querySelector('fieldset')!.disabled).toBe(false);
  });

  it('disables only after confirming', async () => {
    await render(active);
    act(() => buttons('Disable')[0].click());
    await settle();
    expect(document.body.textContent).toContain('Nobody\'s account or access is removed');
    expect(sent('DELETE')).toHaveLength(0);
    act(() => buttons('Disable')[1].click());
    await settle();
    const [url, init] = sent('DELETE')[0];
    expect(url).toBe(`/api/admin/v1/organizations/${ORG}/identity-provider`);
    const headers = new Headers(init.headers);
    expect(headers.get('If-Match')).toBe('"v2"');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
    expect(reloads).toBe(1);
  });

  it('shows the settings without controls to people who may only read them', async () => {
    await render(active, 'Admin');
    expect(container.textContent).toContain('Active');
    expect(container.querySelector('code')?.textContent).toBe(REDIRECT);
    expect(input('idp-issuer')).toBeNull();
    expect(buttons('Disable')).toHaveLength(0);
    expect(buttons('Test and activate')).toHaveLength(0);
    expect(offersSignInChanges('Auditor')).toBe(false);
    expect(offersSignInChanges('owner')).toBe(true);
    expect(offersSignInChanges(undefined)).toBe(true);
  });

  it('parses scopes separated by spaces or commas', () => {
    expect(parseScopes(' groups,offline_access  api://x/.default ')).toEqual(['groups', 'offline_access', 'api://x/.default']);
    expect(parseScopes('')).toEqual([]);
  });
});

describe('the sign-in tab', () => {
  let container: HTMLDivElement;
  let root: Root;
  let providerResponse: () => Response;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    providerResponse = () => json(active, 200, { etag: '"v2"' });
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url.endsWith('/identity-provider')) return providerResponse();
      if (url.includes('/invitations')) return json({ items: [], next_page_token: null, grantable_roles: [] });
      return json({ items: [], next_page_token: null });
    }));
    history.replaceState(null, '', '/admin/organization');
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const render = async (role = 'Owner') => {
    const context = { user_id: 'u-1', organization_id: ORG, team_id: null, role };
    act(() => root.render(<OrganizationScreen section="settings" organizationId={ORG} userId="u-1" context={context} />));
    await settle();
  };
  const tab = (name: string) =>
    Array.from(container.querySelectorAll('[role="tab"]')).find(node => node.textContent === name) as HTMLButtonElement | undefined;

  it('is offered to people who may read it', async () => {
    await render();
    openSection('settings', 'sign-in');
    await settle();
    expect(container.textContent).toContain(ISSUER);
    expect(document.getElementById('idp-issuer')).not.toBeNull();
  });

  it('keeps email domains for members who may not read the provider', async () => {
    for (const [status, code] of [[403, 'forbidden'], [404, 'not_found']] as const) {
      providerResponse = () => problem(status, code, 'no');
      await render('Member');
      openSection('settings', 'sign-in');
      await settle();
      expect(document.getElementById('idp-issuer')).toBeNull();
      expect(container.textContent).not.toContain(ISSUER);
      expect(container.textContent).toContain('Email domains');
      act(() => root.unmount());
      root = createRoot(container);
    }
  });

  it('opens on the sign-in tab when a test sign-in returns', async () => {
    history.replaceState(null, '', `/admin/settings?organization=${ORG}&sign_in_test=identity_provider_changed`);
    await render();
    expect(container.textContent).toContain('The configuration changed while you were testing it');
  });
});
