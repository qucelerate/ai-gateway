import type { AutoOffer, CatalogOperation, CatalogSyncJob, DiscoveredModel, OfferStatus } from './api';

// What discovery reports, in plain words, and how often to ask about a job
// in flight. A listing reports no prices, and a model's operations only where
// the provider makes them certain, so others stay unknown here. A listing
// changes the catalog only for an account opted in to automatic offers.

/** Whether the worker has yet to finish the job. */
export function isActive(job: Pick<CatalogSyncJob, 'status'>): boolean {
  return job.status === 'queued' || job.status === 'running';
}

/** How long to wait before the next look at a job in flight: 2, 4, 8, then
 * every 15 seconds. */
export function pollDelay(attempt: number): number {
  return Math.min(2000 * 2 ** Math.max(0, attempt), 15_000);
}

/** Stop polling after this long; the job keeps running on the server. */
export const POLL_LIMIT_MS = 10 * 60_000;

const errorTexts: Record<string, string> = {
  not_configured: 'No credential reaches this account. Add one under Credentials, then try again.',
  attempts_exhausted: 'The worker stopped three times while listing. Try again; if it keeps failing, tell the gateway operator.',
  authentication: 'The provider refused the credential. Replace it under Credentials.',
  authorization: 'The credential may not list models. Check its permissions with the provider.',
  rate_limited: 'The provider rate-limited the listing. Try again in a few minutes.',
  timeout: 'The provider did not answer in time. Try again.',
  unavailable: 'The provider was unavailable. Try again later.',
  provider: 'The provider returned an error. Try again later.',
  protocol: 'The provider answered in a way the gateway did not understand. Tell the gateway operator.',
};

/** Why a job failed, from its fixed error code. */
export function errorText(code: string | null): string {
  if (!code) return 'The listing failed.';
  return errorTexts[code] ?? `The listing failed (${code}).`;
}

const count = (value: number | null, one: string, many: string) => `${value ?? 0} ${value === 1 ? one : many}`;

/** A job's outcome, or what it is waiting for. */
export function jobSummary(job: CatalogSyncJob): string {
  switch (job.status) {
    case 'queued':
      return 'Queued. The worker starts it within about a minute.';
    case 'running':
      return 'Listing the provider’s models…';
    case 'succeeded':
      return `Listed ${count(job.models_listed, 'model', 'models')}: ${job.models_added ?? 0} new, ${job.models_missing ?? 0} no longer listed.`;
    case 'unsupported':
      return 'This provider cannot list its models, so it cannot be discovered.';
    default:
      return errorText(job.error_code);
  }
}

export const statusBadge: Record<CatalogSyncJob['status'], string> = {
  queued: 'badge-neutral',
  running: 'badge-neutral',
  succeeded: 'badge-success',
  failed: 'badge-danger',
  unsupported: 'badge-neutral',
};

/** Whether the provider still lists the model, or since when it does not.
 * A provider-wide catalog (Google's, on Agent Platform) cannot say whether
 * the account's location offers it. */
export function listingState(model: DiscoveredModel): { listed: boolean; text: string } {
  if (model.missing_since) return { listed: false, text: 'No longer listed' };
  if (model.availability === 'profile_only') return { listed: true, text: 'Listed; call it through an inference profile' };
  if (model.availability === 'endpoint_only') return { listed: true, text: 'Listed; an endpoint serving what the gateway does not call' };
  return { listed: true, text: model.availability === 'unknown' ? 'Listed; location not confirmed' : 'Listed' };
}

const operationNames: Record<CatalogOperation, string> = { chat: 'chat', chat_stream: 'streamed chat', embeddings: 'embeddings', chat_batch: 'chat batches', embeddings_batch: 'embedding batches' };

/** What is known of a discovered model's capabilities: the catalog's for a
 * deployed model, else what the listing reported, else unknown. Never
 * "supported" by guess. */
/** Listed operations, with a listing's `chat_stream` as chat (D-020). */
export function chatOperations<T extends string>(operations: T[]): T[] {
  return Array.from(new Set(operations.map(operation => (operation === 'chat_stream' ? 'chat' : operation) as T)));
}

export function capabilityText(model: DiscoveredModel): string {
  if (model.deployed) return 'As set in the catalog';
  if (!model.operations) return 'unknown';
  const listed = chatOperations(model.operations).map(operation => operationNames[operation]).join(', ');
  return `Listed for ${listed}`;
}

export const offerBadge: Record<OfferStatus, string> = {
  offered: 'badge-success',
  needs_price: 'badge-danger',
  unsupported: 'badge-neutral',
  withdrawn: 'badge-neutral',
  catalog_full: 'badge-danger',
  refused: 'badge-danger',
  requires_action: 'badge-danger',
  unchecked: 'badge-warning',
};

export const offerLabel: Record<OfferStatus, string> = {
  offered: 'Offered',
  needs_price: 'Needs a price',
  unsupported: 'Not offered',
  withdrawn: 'Withdrawn',
  catalog_full: 'No room',
  refused: 'Refused',
  requires_action: 'Needs action',
  unchecked: 'Not checked yet',
};

/** What a model's offer status means, and what to do about it. */
export function offerText(status: OfferStatus, deployment: string): string {
  switch (status) {
    case 'offered': return 'In the active release, priced by the source shown.';
    case 'needs_price': return `No price source prices it. Map ${deployment} to a published item under Pricing; the next run offers it.`;
    case 'unsupported': return 'Not a chat or embedding model this provider’s adapter can call, or the listing cannot say this account may call it.';
    case 'withdrawn': return 'A release activated since dropped it, so it is not offered again until automatic offers are set again.';
    case 'catalog_full': return 'The release has no room for another deployment.';
    case 'refused': return 'A call to it was refused for this account, so it is not offered. Its evidence under Discovery says why; it is offered again once a call succeeds.';
    case 'requires_action': return 'A check found something to do before this account can call it, so it is not offered. Its evidence under Discovery says what; it is offered again once a check or a call shows the account may call it.';
    case 'unchecked': return 'The listing could not check whether this account may call it (AWS throttled or refused the check), and a first call could subscribe the account, so it is not offered until a later listing checks it.';
  }
}

/** What the gateway has seen calling a model on this account, in plain
 * words; null when nothing yet. */
export function evidenceText(model: DiscoveredModel): string | null {
  const evidence = model.evidence;
  if (!evidence) return null;
  switch (evidence.state) {
    case 'verified': return 'A call succeeded';
    case 'entitled': return 'The account may call it';
    case 'requires_action': switch (evidence.reason) {
      case 'permission_missing': return 'Needs action: grant the gateway permission to call it';
      case 'agreement_required': return 'Needs action: accept the model’s offer terms with the provider';
      case 'not_entitled': return 'Needs action: the provider restricts it for this account';
      case 'quota_zero': return 'Needs action: request quota for it from the provider';
      case 'policy_denied': return 'Needs action: an organization policy forbids it';
      case 'enable_in_model_garden': return 'Needs action: enable it in Model Garden';
      default: return 'Needs action with the provider';
    }
    case 'refused': {
      const status = evidence.provider_code ?? (evidence.provider_status ? `HTTP ${evidence.provider_status}` : null);
      const why = evidence.reason === 'not_found'
        ? 'not found where the gateway calls it'
        : 'access denied for this account';
      return `Refused: ${why}${status ? ` (${status})` : ''}`;
    }
  }
}

/** Why the latest run changed nothing, in plain words. */
export function offerErrorText(code: string): string {
  switch (code) {
    case 'no_active_release': return 'The organization has no active release.';
    case 'account_not_declared': return 'The active release no longer declares this account.';
    case 'invalid_release': return 'The release it planned failed the catalog’s checks.';
    case 'incompatible_price_override': return 'A price override charges usage this provider never reports; revoke it first.';
    default: return `It failed (${code}).`;
  }
}

/** One line on an opt-in's latest run. */
export function offerSummary(offer: AutoOffer): string {
  if (!offer.applied_at) return 'Waiting for the account’s next listing, within about a minute.';
  if (offer.error_code) return `The latest run changed nothing: ${offerErrorText(offer.error_code)}`;
  const offered = offer.models.filter(model => model.status === 'offered').length;
  const unpriced = offer.models.filter(model => model.status === 'needs_price').length;
  return `${offered === 1 ? 'One model' : `${offered} models`} offered${unpriced ? `; ${unpriced === 1 ? 'one needs' : `${unpriced} need`} a price` : ''}.`;
}
