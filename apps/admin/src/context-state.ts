import { isContextDenied, normalizeApiError, type Context, type NormalizedError, type Organization } from './api';

export type ContextState = 'idle' | 'pending' | 'permission' | 'unauthenticated' | 'error';
export type ContextSnapshot = {
  confirmedOrganization: Organization | null;
  confirmedContext: Context | null;
  selectedOrganizationId: string;
  pendingOrganizationId: string | null;
  retryOrganizationId: string | null;
  state: ContextState;
  error: NormalizedError | null;
};

type Listener = (snapshot: ContextSnapshot) => void;
type ContextRequest = (organizationId: string) => Promise<Context>;

export async function logoutSession(logout: () => Promise<void>) {
  try {
    await logout();
    return { ok: true as const, error: null };
  } catch (caught) {
    return { ok: false as const, error: normalizeApiError(caught) };
  }
}

export class OrganizationContextManager {
  private sequence = 0;
  private organizations: Organization[] = [];
  private listeners: Listener[] = [];
  private snapshot: ContextSnapshot = {
    confirmedOrganization: null,
    confirmedContext: null,
    selectedOrganizationId: '',
    pendingOrganizationId: null,
    retryOrganizationId: null,
    state: 'idle',
    error: null,
  };

  constructor(private readonly request: ContextRequest) {}

  setOrganizations(organizations: Organization[]) {
    this.organizations = organizations;
  }

  subscribe(listener: Listener) {
    this.listeners.push(listener);
    return () => { this.listeners = this.listeners.filter(candidate => candidate !== listener); };
  }

  getSnapshot() { return this.snapshot; }

  async select(organizationId: string) {
    const organization = this.organizations.find(candidate => candidate.id === organizationId);
    if (!organization) return;
    const requestSequence = ++this.sequence;
    this.update({
      selectedOrganizationId: organizationId,
      pendingOrganizationId: organizationId,
      retryOrganizationId: null,
      state: 'pending',
      error: null,
    });
    try {
      const context = await this.request(organizationId);
      if (requestSequence !== this.sequence) return;
      this.update({
        confirmedOrganization: organization,
        confirmedContext: context,
        selectedOrganizationId: organizationId,
        pendingOrganizationId: null,
        retryOrganizationId: null,
        state: 'idle',
        error: null,
      });
    } catch (caught) {
      if (requestSequence !== this.sequence) return;
      const error = normalizeApiError(caught);
      this.update({
        selectedOrganizationId: this.snapshot.confirmedOrganization?.id ?? '',
        pendingOrganizationId: null,
        retryOrganizationId: organizationId,
        state: error.status === 401 ? 'unauthenticated' : isContextDenied(error) ? 'permission' : 'error',
        error,
      });
    }
  }

  retry() {
    if (this.snapshot.retryOrganizationId) return this.select(this.snapshot.retryOrganizationId);
  }

  private update(changes: Partial<ContextSnapshot>) {
    this.snapshot = { ...this.snapshot, ...changes };
    this.listeners.forEach(listener => listener(this.snapshot));
  }
}
