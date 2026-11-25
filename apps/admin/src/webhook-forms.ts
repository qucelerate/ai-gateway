import type { WebhookDelivery, WebhookEndpoint, WebhookEndpointInput, WebhookEndpointUpdate, WebhookEventType, WebhookSecretResponse } from './api';

// Client-side checks matching the webhooks contract, so the form explains a
// mistake before the server refuses it. The server still checks the host's
// addresses, which the browser cannot.

export const eventTypeHints: Record<WebhookEventType, string> = {
  'inference.finalized': 'A request finished and its usage and cost are final.',
  'reconciliation.opened': 'A request could not settle and needs reconciliation.',
  'reconciliation.updated': 'An open reconciliation item took the reason of a finalization that arrived later; it stays open.',
  'reconciliation.resolved': 'A reconciliation item was resolved.',
  'usage_export.failing': 'Writes to the usage export destination have failed for an hour; records stay queued.',
  'notification.created': 'A notification was raised in the organization: a budget past its threshold, a key or token near expiry, a webhook or price source failing, a price change held or adopted.',
};

export type WebhookFormValues = { url: string; description: string; eventTypes: WebhookEventType[]; enabled: boolean };
export type WebhookFormErrors = Partial<Record<'url' | 'description' | 'eventTypes', string>>;

export function formValues(endpoint: WebhookEndpoint | null): WebhookFormValues {
  return endpoint
    ? { url: endpoint.url, description: endpoint.description ?? '', eventTypes: [...endpoint.event_types], enabled: endpoint.enabled }
    : { url: '', description: '', eventTypes: [], enabled: true };
}

/** Why `value` cannot be a webhook URL, or null if it can. */
export function urlProblem(value: string): string | null {
  if (!value) return 'Enter the URL that receives the events.';
  if (value.length > 2048) return 'Use a URL of at most 2,048 characters.';
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return 'Enter a full URL, such as https://example.com/hooks/gateway.';
  }
  if (url.protocol !== 'https:' || !value.toLowerCase().startsWith('https://')) return 'Use an https:// URL.';
  if (url.username || url.password) return 'Remove the user name and password from the URL.';
  if (value.includes('#')) return 'Remove the #fragment from the URL.';
  const host = url.hostname.toLowerCase();
  if (host === 'localhost' || host.endsWith('.localhost') || (!host.includes('.') && !host.startsWith('['))) {
    return 'Use a public host name; local and single-label hosts are refused.';
  }
  return null;
}

/** The endpoint to create, or what to fix. */
export function webhookInput(values: WebhookFormValues): { input: WebhookEndpointInput } | { errors: WebhookFormErrors } {
  const url = values.url.trim();
  const description = values.description.trim();
  const errors: WebhookFormErrors = {};
  const problem = urlProblem(url);
  if (problem) errors.url = problem;
  if (description.length > 200) errors.description = 'Use at most 200 characters.';
  if (values.eventTypes.length === 0) errors.eventTypes = 'Choose at least one event.';
  if (Object.keys(errors).length) return { errors };
  return { input: { url, description: description || null, event_types: [...values.eventTypes], enabled: values.enabled } };
}

/** Only what changed from `endpoint`; null when nothing did. */
export function webhookUpdate(endpoint: WebhookEndpoint, input: WebhookEndpointInput): WebhookEndpointUpdate | null {
  const update: WebhookEndpointUpdate = {};
  if (input.url !== endpoint.url) update.url = input.url;
  // An empty description clears it.
  if ((input.description ?? '') !== (endpoint.description ?? '')) update.description = input.description ?? '';
  const sorted = (types: WebhookEventType[]) => [...types].sort().join(' ');
  if (sorted(input.event_types) !== sorted(endpoint.event_types)) update.event_types = input.event_types;
  if (input.enabled !== endpoint.enabled) update.enabled = input.enabled;
  return Object.keys(update).length ? update : null;
}

/** The secret a create or rotation returned, or that it was a replay and the
 * secret was already shown to whoever made the first attempt. */
export function secretOutcome(response: WebhookSecretResponse): { secret: string } | { replayed: true } {
  return response.secret ? { secret: response.secret } : { replayed: true };
}

/** What the receiver last answered. */
export function lastResponse(delivery: WebhookDelivery): string {
  if (delivery.last_status_code !== null) return `HTTP ${delivery.last_status_code}${delivery.last_error ? ` · ${delivery.last_error}` : ''}`;
  if (delivery.last_error) return delivery.last_error;
  return delivery.attempts === 0 ? 'Not attempted yet' : '—';
}

/** The time that matters for the delivery's status. */
export function deliveryTime(delivery: WebhookDelivery): { label: string; at: string | null } {
  if (delivery.status === 'delivered') return { label: 'Delivered', at: delivery.delivered_at };
  if (delivery.status === 'pending' && delivery.next_attempt_at) return { label: 'Next attempt', at: delivery.next_attempt_at };
  if (delivery.last_attempt_at) return { label: 'Last attempt', at: delivery.last_attempt_at };
  return { label: 'Created', at: delivery.created_at };
}

/** The form after a version conflict: fields the person changed keep their
 * edits; the others take the current endpoint's values, so saving does not
 * undo someone else's change to a field this person never touched. */
export function rebaseValues(values: WebhookFormValues, opened: WebhookEndpoint, current: WebhookEndpoint): WebhookFormValues {
  const before = formValues(opened);
  const now = formValues(current);
  const same = (a: unknown, b: unknown) => JSON.stringify(Array.isArray(a) ? [...a].sort() : a) === JSON.stringify(Array.isArray(b) ? [...b].sort() : b);
  return {
    url: same(values.url, before.url) ? now.url : values.url,
    description: same(values.description, before.description) ? now.description : values.description,
    eventTypes: same(values.eventTypes, before.eventTypes) ? now.eventTypes : values.eventTypes,
    enabled: values.enabled === before.enabled ? now.enabled : values.enabled,
  };
}
