import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { EmailSignIn } from './email-sign-in';
import { DomainsSection } from './domains-section';

const json = (body: unknown, status = 200) =>
  new Response(status === 204 ? null : JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }, status);
const options = { password: true, oidc: true, setup_required: false };
const pending = {
  id: 'd-1', domain: 'acme.example', status: 'pending', created_at: '2026-11-13T10:00:00Z', verified_at: null, last_checked_at: null,
  record: { type: 'TXT', name: '_gateway-verification.acme.example', value: 'gateway-domain-verification=abc' },
};

describe('email-first sign-in and domains', () => {
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
    localStorage.clear();
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
  const click = async (label: string) => {
    const button = Array.from(container.querySelectorAll('button')).find(b => b.textContent?.trim() === label);
    expect(button, label).toBeTruthy();
    await act(async () => { button!.click(); });
  };
  const body = (call: number) => JSON.parse(String((fetchMock.mock.calls[call][1] as RequestInit).body));
  const url = (call: number) => String(fetchMock.mock.calls[call][0]);

  it('sends an address at a verified domain to its identity provider', async () => {
    const navigate = vi.fn();
    fetchMock.mockResolvedValueOnce(json({ next: 'redirect', authorization_url: 'https://idp.example/authorize?login_hint=ada', password: false }));
    await render(<EmailSignIn options={options} pending={false} onSignInWith={vi.fn()} navigate={navigate} />);
    type('sign-in-start-email', ' ada@acme.example ');
    await click('Continue');
    expect(url(0)).toBe('/api/admin/v1/sign-in/start');
    expect(body(0)).toEqual({ email: 'ada@acme.example' });
    expect(navigate).toHaveBeenCalledWith('https://idp.example/authorize?login_hint=ada');
    expect(localStorage.getItem('gateway.sign-in.email')).toBe('ada@acme.example');
  });

  it('offers a password next to the identity provider when both are allowed', async () => {
    const navigate = vi.fn();
    fetchMock.mockResolvedValueOnce(json({ next: 'redirect', authorization_url: 'https://idp.example/authorize?login_hint=ada', password: true }));
    await render(<EmailSignIn options={options} pending={false} onSignInWith={vi.fn()} navigate={navigate} />);
    type('sign-in-start-email', 'ada@acme.example');
    await click('Continue');
    expect(navigate).not.toHaveBeenCalled();
    await click('Use a password instead');
    expect(container.querySelector('input[type="password"]')).not.toBeNull();
  });

  it('follows the identity provider when chosen', async () => {
    const navigate = vi.fn();
    fetchMock.mockResolvedValueOnce(json({ next: 'redirect', authorization_url: 'https://idp.example/authorize?login_hint=ada', password: true }));
    await render(<EmailSignIn options={options} pending={false} onSignInWith={vi.fn()} navigate={navigate} />);
    type('sign-in-start-email', 'ada@acme.example');
    await click('Continue');
    await click('Continue with single sign-on');
    expect(navigate).toHaveBeenCalledWith('https://idp.example/authorize?login_hint=ada');
  });

  it('asks for the password, with the address filled in, and goes back', async () => {
    const onSignedIn = vi.fn();
    fetchMock
      .mockResolvedValueOnce(json({ next: 'password' }))
      .mockResolvedValueOnce(json({ user_id: 'u-1' }));
    await render(<EmailSignIn options={options} pending={false} onSignInWith={vi.fn()} onSignedIn={onSignedIn} />);
    type('sign-in-start-email', 'ada@example.com');
    await click('Continue');
    expect(input('sign-in-email').value).toBe('ada@example.com');
    type('sign-in-password', 'correct horse battery staple');
    await click('Sign in');
    expect(body(1)).toEqual({ email: 'ada@example.com', password: 'correct horse battery staple' });
    expect(onSignedIn).toHaveBeenCalled();
    await click('Use a different email address');
    expect(input('sign-in-start-email')).not.toBeNull();
  });

  it('offers a password without routing, and only where passwords exist', async () => {
    fetchMock.mockResolvedValueOnce(json({ user_id: 'u-1' }));
    await render(<EmailSignIn options={options} pending={false} onSignInWith={vi.fn()} onSignedIn={vi.fn()} />);
    type('sign-in-start-email', 'ada@sso-only.example');
    await click('Sign in with a password');
    expect(fetchMock).not.toHaveBeenCalled();
    expect(input('sign-in-email').value).toBe('ada@sso-only.example');
    type('sign-in-password', 'correct horse battery staple');
    await click('Sign in');
    expect(body(0)).toEqual({ email: 'ada@sso-only.example', password: 'correct horse battery staple' });
    act(() => root.unmount());
    root = createRoot(container);
    await render(<EmailSignIn options={{ ...options, password: false }} pending={false} onSignInWith={vi.fn()} />);
    expect(Array.from(container.querySelectorAll('button')).some(b => b.textContent === 'Sign in with a password')).toBe(false);
  });

  it('keeps the short name as a fallback', async () => {
    const onSignInWith = vi.fn();
    await render(<EmailSignIn options={options} pending={false} onSignInWith={onSignInWith} />);
    expect(document.getElementById('sign-in-organization')).toBeNull();
    await click('Sign in with your organization’s short name instead');
    type('sign-in-organization', 'acme');
    await act(async () => { (container.querySelector('form[aria-label="Sign in to your organization"] button[type="submit"]') as HTMLButtonElement).click(); });
    expect(onSignInWith).toHaveBeenCalledWith('acme');
    await click('Sign in without an organization name');
    expect(onSignInWith).toHaveBeenLastCalledWith();
  });

  it('shows why an address cannot sign in', async () => {
    fetchMock.mockResolvedValueOnce(problem(503, 'sign_in_unavailable', 'this gateway has no way to sign in configured; ask its operator'));
    await render(<EmailSignIn options={{ ...options, password: false, oidc: false }} pending={false} onSignInWith={vi.fn()} />);
    type('sign-in-start-email', 'ada@example.com');
    await click('Continue');
    expect(container.textContent).toContain('This gateway has no way to sign in configured; ask its operator.');
  });

  it('adds, verifies and removes a domain', async () => {
    fetchMock
      .mockResolvedValueOnce(json({ items: [] }))
      .mockResolvedValueOnce(json(pending, 201))
      .mockResolvedValueOnce(json({ ...pending, last_checked_at: '2026-11-13T10:05:00Z' }))
      .mockResolvedValueOnce(json({ ...pending, status: 'verified', verified_at: '2026-11-13T10:10:00Z' }))
      .mockResolvedValueOnce(json(null, 204));
    await render(<DomainsSection organizationId="o-1" canWrite />);
    expect(url(0)).toBe('/api/admin/v1/organizations/o-1/domains');
    type('email-domain', 'acme.example');
    await click('Add domain');
    expect(body(1)).toEqual({ domain: 'acme.example' });
    expect(container.textContent).toContain('_gateway-verification.acme.example');
    expect(container.textContent).toContain('gateway-domain-verification=abc');
    await click('Verify');
    expect(url(2)).toBe('/api/admin/v1/organizations/o-1/domains/d-1/verify');
    expect(container.textContent).toContain('was not found yet');
    await click('Verify');
    expect(container.textContent).toContain('acme.example is verified.');
    expect(container.textContent).not.toContain('gateway-domain-verification=abc');
    await click('Remove');
    const confirm = Array.from(document.querySelectorAll('[role="alertdialog"] button')).find(b => b.textContent?.trim() === 'Remove') as HTMLButtonElement;
    await act(async () => { confirm.click(); });
    expect((fetchMock.mock.calls[4][1] as RequestInit).method).toBe('DELETE');
    expect(container.querySelector('.domain-item')).toBeNull();
  });

  it('lets an owner add a domain only once the list has loaded', async () => {
    let list!: (response: Response) => void;
    fetchMock.mockReturnValueOnce(new Promise<Response>(resolve => { list = resolve; }));
    await render(<DomainsSection organizationId="o-1" canWrite />);
    const add = () => Array.from(container.querySelectorAll('button')).find(b => b.textContent?.trim() === 'Add domain') as HTMLButtonElement;
    expect(add().disabled).toBe(true);
    await act(async () => { list(json({ items: [] })); });
    expect(add().disabled).toBe(false);
  });

  it('shows a domain claimed elsewhere, and only the list to non-owners', async () => {
    fetchMock
      .mockResolvedValueOnce(json({ items: [] }))
      .mockResolvedValueOnce(problem(409, 'domain_claimed', 'another organization on this gateway has verified this domain'));
    await render(<DomainsSection organizationId="o-1" canWrite />);
    type('email-domain', 'taken.example');
    await click('Add domain');
    expect(container.textContent).toContain('Another organization on this gateway has verified this domain.');
    act(() => root.unmount());
    root = createRoot(container);
    fetchMock.mockResolvedValueOnce(json({ items: [pending] }));
    await render(<DomainsSection organizationId="o-1" canWrite={false} />);
    expect(container.textContent).toContain('acme.example');
    expect(document.getElementById('email-domain')).toBeNull();
    expect(Array.from(container.querySelectorAll('button')).some(b => b.textContent === 'Verify')).toBe(false);
  });
});
