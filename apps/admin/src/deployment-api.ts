import { adminRequest } from './api';

export type DeploymentOrganization = {
  id: string;
  slug: string;
  name: string;
  status: 'active' | 'suspended' | 'deleted';
  created_at: string;
  suspended_at: string | null;
  deleted_at: string | null;
  deletion_requested_at: string | null;
  members: number;
  /** Secrets it keeps stored, encrypted, through deletion: counts, never values. */
  stored?: StoredSecrets;
};

export type StoredSecrets = {
  provider_credentials: number;
  cloud_connections: number;
  webhook_signing_secrets: number;
  identity_provider_client_secret: boolean;
};

export type OwnerLink = { kind: 'invitation' | 'set_password'; email: string; link: string; expires_at: string };

export type DeploymentEvent = {
  id: string;
  occurred_at: string;
  actor_user_id: string | null;
  actor_email: string | null;
  action: string;
  organization_id: string | null;
  target_user_id: string | null;
  detail: Record<string, unknown>;
};

const base = '/deployment/organizations';
const post = <T>(path: string, body?: unknown) =>
  adminRequest<T>(path, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) }).then(result => result.data);
const at = (id: string, action: string) => `${base}/${encodeURIComponent(id)}/${action}`;

export const deployment = {
  organizations: (deleted: boolean) =>
    adminRequest<{ items: DeploymentOrganization[]; self_serve: boolean }>(`${base}${deleted ? '?deleted=true' : ''}`).then(result => result.data),
  create: (body: { name: string; slug: string; owner?: { email: string; method: 'invitation' | 'local_account' } }) =>
    post<{ organization: DeploymentOrganization; owner_link: OwnerLink | null }>(base, body),
  suspend: (id: string) => post<DeploymentOrganization>(at(id, 'suspend')),
  unsuspend: (id: string) => post<DeploymentOrganization>(at(id, 'unsuspend')),
  remove: (id: string, confirmSlug: string) => post<DeploymentOrganization>(at(id, 'delete'), { confirm_slug: confirmSlug }),
  restore: (id: string, slug?: string) => post<DeploymentOrganization>(at(id, 'restore'), slug ? { slug } : {}),
  /** Self-serve creation (`GATEWAY_SELF_SERVE_ORGANIZATIONS`): the caller becomes its owner. */
  createOwn: (body: { name: string; slug: string }) =>
    post<{ id: string; slug: string; name: string }>('/organizations', body),
  audit: () => adminRequest<{ items: DeploymentEvent[] }>('/deployment/audit?limit=50').then(result => result.data.items),
  deletionRequest: (organizationId: string) =>
    adminRequest<{ requested_at: string | null }>(`/organizations/${encodeURIComponent(organizationId)}/deletion-request`).then(result => result.data),
  setDeletionRequest: (organizationId: string, request: boolean) =>
    adminRequest<{ requested_at: string | null }>(`/organizations/${encodeURIComponent(organizationId)}/deletion-request`, {
      method: request ? 'PUT' : 'DELETE',
    }).then(result => result.data),
};
