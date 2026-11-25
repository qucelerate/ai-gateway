import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { fragmentToken, invitationSsoUrl, slugFrom } from './local-accounts-api';
import {
  AcceptInvitationPage,
  ChangePasswordDialog,
  PasswordResetLinkDialog,
  PasswordSignIn,
  RequireSsoSetting,
  ResetPasswordPage,
  SetupPage,
} from './local-accounts';

const json = (body: unknown, status = 200) =>
  new Response(status === 204 ? null : JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }, status);

describe('link helpers', () => {
  it('reads the token from the fragment and makes short names', () => {
    expect(fragmentToken('#token=abc-123')).toBe('abc-123');
    expect(fragmentToken('')).toBe('');
    expect(slugFrom('  Ácme Rockets, Inc. ')).toBe('acme-rockets-inc');
    expect(slugFrom('---')).toBe('');
    expect(invitationSsoUrl('a b')).toBe('/api/admin/v1/invitations/accept?token=a%20b&method=sso');
  });
});

describe('local accounts', () => {
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

  const render = async (element: React.ReactElement) => {
    await act(async () => { root.render(element); });
  };
  const input = (id: string) => document.getElementById(id) as HTMLInputElement;
  const type = (id: string, value: string) => act(() => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input(id), value);
    input(id).dispatchEvent(new Event('input', { bubbles: true }));
  });
  const submit = async (label?: string) => {
    const buttons = Array.from(document.querySelectorAll<HTMLButtonElement>('button[type="submit"], button'));
    const button = label ? buttons.find(b => b.textContent?.trim() === label) : buttons.find(b => b.type === 'submit');
    await act(async () => { button!.click(); });
  };
  const body = (call: number) => JSON.parse(String((fetchMock.mock.calls[call][1] as RequestInit).body));
  const url = (call: number) => String(fetchMock.mock.calls[call][0]);

  it('signs in with an email address and password', async () => {
    const onSignedIn = vi.fn();
    fetchMock.mockResolvedValueOnce(json({ user_id: 'u-1' }));
    await render(<PasswordSignIn onSignedIn={onSignedIn} />);
    type('sign-in-email', ' ada@example.com ');
    type('sign-in-password', 'correct horse battery staple');
    await submit();
    expect(url(0)).toBe('/api/admin/v1/password/sign-in');
    expect(body(0)).toEqual({ email: 'ada@example.com', password: 'correct horse battery staple' });
    expect(onSignedIn).toHaveBeenCalled();
  });

  it('shows why a sign-in failed and clears the password', async () => {
    fetchMock.mockResolvedValueOnce(problem(429, 'too_many_attempts', 'too many failed sign-ins; try again in 30 seconds'));
    await render(<PasswordSignIn onSignedIn={vi.fn()} />);
    type('sign-in-email', 'ada@example.com');
    type('sign-in-password', 'wrong password here');
    await submit();
    expect(container.textContent).toContain('Too many failed sign-ins; try again in 30 seconds.');
    expect(input('sign-in-password').value).toBe('');
  });

  it('sets up the first organization and its owner', async () => {
    const onDone = vi.fn();
    fetchMock
      .mockResolvedValueOnce(json({ valid: true }))
      .mockResolvedValueOnce(json({ organization_id: 'o-1', user_id: 'u-1' }, 201));
    await render(<SetupPage token="setup-token" onDone={onDone} />);
    expect(body(0)).toEqual({ token: 'setup-token' });
    type('setup-organization-name', 'Acme Rockets');
    expect(input('setup-organization-slug').value).toBe('acme-rockets');
    type('setup-email', 'ada@example.com');
    type('setup-password', 'correct horse battery staple');
    type('setup-password-confirm', 'correct horse battery stapl');
    expect(container.textContent).toContain('The passwords do not match.');
    type('setup-password-confirm', 'correct horse battery staple');
    await submit('Create organization');
    expect(url(1)).toBe('/api/admin/v1/setup');
    expect(body(1)).toEqual({
      token: 'setup-token',
      organization_name: 'Acme Rockets',
      organization_slug: 'acme-rockets',
      email: 'ada@example.com',
      password: 'correct horse battery staple',
    });
    expect(onDone).toHaveBeenCalled();
  });

  it('says when the gateway is set up already', async () => {
    fetchMock.mockResolvedValueOnce(problem(409, 'setup_already_done', 'the gateway is already set up; sign in instead'));
    await render(<SetupPage token="old" onDone={vi.fn()} />);
    expect(container.textContent).toContain('This gateway is already set up.');
    await render(<SetupPage token="" onDone={vi.fn()} />);
  });

  it('accepts an invitation with a new password, or the existing one', async () => {
    const onDone = vi.fn();
    fetchMock
      .mockResolvedValueOnce(json({ organization_name: 'Acme', role: 'Admin', password: true, sso: true }))
      .mockResolvedValueOnce(problem(409, 'account_exists', 'an account with this email address was just created; enter its password'))
      .mockResolvedValueOnce(json({ user_id: 'u-2' }));
    await render(<AcceptInvitationPage token="invite" onDone={onDone} />);
    expect(container.querySelector('h1')?.textContent).toBe('Join Acme');
    expect(container.textContent).toContain('You are invited as an admin.');
    expect(container.querySelector('a')?.getAttribute('href')).toBe(invitationSsoUrl('invite'));
    type('invite-email', 'grace@example.com');
    type('invite-password', 'another long passphrase');
    type('invite-password-confirm', 'another long passphrase');
    await submit('Join Acme');
    expect(body(1)).toEqual({ token: 'invite', email: 'grace@example.com', password: 'another long passphrase' });
    // The account exists: the form asks for its password.
    expect(input('invite-current-password')).not.toBeNull();
    type('invite-current-password', 'her existing passphrase');
    await submit('Join Acme');
    expect(body(2)).toEqual({ token: 'invite', email: 'grace@example.com', password: 'her existing passphrase' });
    expect(onDone).toHaveBeenCalled();
  });

  it('refuses an invitation that no longer works', async () => {
    fetchMock.mockResolvedValueOnce(problem(404, 'invitation_invalid', 'no'));
    await render(<AcceptInvitationPage token="used" onDone={vi.fn()} />);
    expect(container.querySelector('h1')?.textContent).toBe('This invitation does not work');
  });

  it('resets a password with a link', async () => {
    fetchMock
      .mockResolvedValueOnce(json({ valid: true, email: 'ada@example.com' }))
      .mockResolvedValueOnce(problem(422, 'password_too_common', 'the password is too common; choose one that is harder to guess'))
      .mockResolvedValueOnce(json(null, 204));
    await render(<ResetPasswordPage token="reset" />);
    expect(container.textContent).toContain('ada@example.com');
    type('reset-password', 'Password2024!!');
    type('reset-password-confirm', 'Password2024!!');
    await submit('Set password');
    expect(container.textContent).toContain('The password is too common; choose one that is harder to guess.');
    type('reset-password', 'a much better passphrase');
    type('reset-password-confirm', 'a much better passphrase');
    await submit('Set password');
    expect(body(2)).toEqual({ token: 'reset', password: 'a much better passphrase' });
    expect(container.querySelector('h1')?.textContent).toBe('Your password is changed');
  });

  it('changes the signed-in password', async () => {
    fetchMock.mockResolvedValueOnce(json(null, 204));
    await render(<ChangePasswordDialog open onClose={vi.fn()} email="ada@example.com" />);
    type('change-current-password', 'old passphrase here');
    type('change-password', 'new passphrase here');
    type('change-password-confirm', 'new passphrase here');
    await submit('Change password');
    expect(url(0)).toBe('/api/admin/v1/me/password');
    expect(body(0)).toEqual({ current_password: 'old passphrase here', new_password: 'new passphrase here' });
    expect(container.ownerDocument.body.textContent).toContain('Your password is changed.');
  });

  it('makes a member a reset link once', async () => {
    fetchMock.mockResolvedValueOnce(json({ link: 'http://localhost:8080/admin/reset-password#token=t', expires_at: '2026-11-14T10:00:00Z' }, 201));
    await render(<PasswordResetLinkDialog organizationId="o-1" userId="u-2" onClose={vi.fn()} />);
    await submit('Make link');
    expect(url(0)).toBe('/api/admin/v1/organizations/o-1/members/u-2/password-reset-link');
    expect(document.body.textContent).toContain('http://localhost:8080/admin/reset-password#token=t');
  });

  it('requires SSO only once the identity provider is enabled', async () => {
    fetchMock.mockResolvedValueOnce(json({ require_sso: false, identity_provider_enabled: false, passwords_allowed: true, local_accounts: true }));
    await render(<RequireSsoSetting organizationId="o-1" canWrite />);
    expect(input('require-sso').disabled).toBe(true);
    act(() => root.unmount());
    root = createRoot(container);
    fetchMock
      .mockResolvedValueOnce(json({ require_sso: false, identity_provider_enabled: true, passwords_allowed: true, local_accounts: true }))
      .mockResolvedValueOnce(json({ require_sso: true, identity_provider_enabled: true, passwords_allowed: false, local_accounts: true }));
    await render(<RequireSsoSetting organizationId="o-1" canWrite />);
    expect(input('require-sso').disabled).toBe(false);
    await act(async () => { input('require-sso').click(); });
    expect((fetchMock.mock.calls[2][1] as RequestInit).method).toBe('PUT');
    expect(body(2)).toEqual({ require_sso: true });
    expect(container.textContent).toContain('Members must sign in with the organization’s identity provider.');
  });
});
