import { adminRequest } from './api';

/** What the sign-in page offers on this gateway. */
export type SignInOptions = {
  /** Email address and password (local accounts). */
  password: boolean;
  /** The gateway's own identity provider. */
  oidc: boolean;
  /** No organization exists yet: first-run setup is waiting. */
  setup_required: boolean;
};

export type InvitationOffer = {
  organization_name: string;
  role: string;
  /** May be accepted with an email address and password. */
  password: boolean;
  /** Is meant for an identity provider: accept it by signing in there. */
  sso: boolean;
};

export type SignInPolicy = {
  require_sso: boolean;
  identity_provider_enabled: boolean;
  passwords_allowed: boolean;
  local_accounts: boolean;
};

export type ResetLink = { link: string; expires_at: string };

/** Where an email address signs in. */
export type SignInNext =
  | { next: 'password' }
  /** `password`: a password is also accepted for this address's organization. */
  | { next: 'redirect'; authorization_url: string; password: boolean };

export type EmailDomain = {
  id: string;
  domain: string;
  status: 'pending' | 'verified';
  created_at: string;
  verified_at: string | null;
  last_checked_at: string | null;
  record: { type: 'TXT'; name: string; value: string };
};

const post = <T>(path: string, body: unknown) =>
  adminRequest<T>(path, { method: 'POST', body: JSON.stringify(body) }).then(result => result.data);

export const localAccounts = {
  signInOptions: () => adminRequest<SignInOptions>('/sign-in/options').then(result => result.data),
  startSignIn: (email: string) => post<SignInNext>('/sign-in/start', { email }),
  domains: (organizationId: string) =>
    adminRequest<{ items: EmailDomain[] }>(`/organizations/${encodeURIComponent(organizationId)}/domains`).then(result => result.data.items),
  addDomain: (organizationId: string, domain: string) =>
    post<EmailDomain>(`/organizations/${encodeURIComponent(organizationId)}/domains`, { domain }),
  verifyDomain: (organizationId: string, domainId: string) =>
    post<EmailDomain>(`/organizations/${encodeURIComponent(organizationId)}/domains/${encodeURIComponent(domainId)}/verify`, {}),
  removeDomain: (organizationId: string, domainId: string) =>
    adminRequest<void>(`/organizations/${encodeURIComponent(organizationId)}/domains/${encodeURIComponent(domainId)}`, { method: 'DELETE' }).then(() => undefined),
  signIn: (email: string, password: string) => post<{ user_id: string }>('/password/sign-in', { email, password }),
  setupCheck: (token: string) => post<{ valid: boolean }>('/setup/check', { token }),
  setup: (body: {
    token: string;
    organization_name: string;
    organization_slug: string;
    email: string;
    display_name?: string;
    password: string;
  }) => post<{ organization_id: string; user_id: string }>('/setup', body),
  changePassword: (currentPassword: string, newPassword: string) =>
    post<void>('/me/password', { current_password: currentPassword, new_password: newPassword }),
  resetCheck: (token: string) => post<{ valid: boolean; email?: string }>('/password/reset/check', { token }),
  reset: (token: string, password: string) => post<void>('/password/reset', { token, password }),
  inspectInvitation: (token: string) => post<InvitationOffer>('/invitations/inspect', { token }),
  acceptInvitation: (body: { token: string; email: string; password: string; display_name?: string }) =>
    post<{ user_id: string }>('/invitations/accept-password', body),
  memberResetLink: (organizationId: string, userId: string) =>
    post<ResetLink>(`/organizations/${encodeURIComponent(organizationId)}/members/${encodeURIComponent(userId)}/password-reset-link`, {}),
  signInPolicy: (organizationId: string) =>
    adminRequest<SignInPolicy>(`/organizations/${encodeURIComponent(organizationId)}/sign-in-policy`).then(result => result.data),
  setSignInPolicy: (organizationId: string, requireSso: boolean) =>
    adminRequest<SignInPolicy>(`/organizations/${encodeURIComponent(organizationId)}/sign-in-policy`, {
      method: 'PUT',
      body: JSON.stringify({ require_sso: requireSso }),
    }).then(result => result.data),
};

/** The SSO way to accept an invitation: the gateway's redirect to the
 * identity provider. */
export function invitationSsoUrl(token: string): string {
  return `/api/admin/v1/invitations/accept?token=${encodeURIComponent(token)}&method=sso`;
}

/** The `token` in a link's fragment (`#token=…`), where links put it so it
 * never reaches a server log. */
export function fragmentToken(hash: string): string {
  return new URLSearchParams(hash.replace(/^#/, '')).get('token') ?? '';
}

/** A short name from an organization's name: lowercase letters, digits and
 * single hyphens. */
export function slugFrom(name: string): string {
  return name
    .normalize('NFKD')
    .replace(/[̀-ͯ]/g, '')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 63)
    .replace(/-+$/, '');
}
