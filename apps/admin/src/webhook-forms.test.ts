import { describe, expect, it } from 'vitest';
import type { WebhookDelivery, WebhookEndpoint } from './api';
import { deliveryTime, formValues, lastResponse, rebaseValues, secretOutcome, urlProblem, webhookInput, webhookUpdate } from './webhook-forms';

const endpoint: WebhookEndpoint = {
  id: 'e-1', url: 'https://hooks.example.com/gateway', description: 'Billing', event_types: ['inference.finalized'],
  enabled: true, created_at: '2026-10-01T00:00:00Z', updated_at: '2026-10-01T00:00:00Z', version: 3,
};
const delivery: WebhookDelivery = {
  id: 'd-1', event_id: 'ev-1', event_type: 'inference.finalized', status: 'parked', attempts: 24,
  next_attempt_at: null, last_attempt_at: '2026-10-02T00:00:00Z', last_status_code: 500, last_error: null,
  delivered_at: null, created_at: '2026-10-01T00:00:00Z',
};

describe('webhook URLs', () => {
  it('accepts an https URL on a public host', () => {
    expect(urlProblem('https://hooks.example.com/gateway?team=1')).toBeNull();
  });

  it('explains what the contract refuses', () => {
    expect(urlProblem('')).toContain('Enter the URL');
    expect(urlProblem('http://hooks.example.com')).toBe('Use an https:// URL.');
    expect(urlProblem('hooks.example.com')).toContain('full URL');
    expect(urlProblem('https://user:pass@hooks.example.com')).toContain('user name and password');
    expect(urlProblem('https://hooks.example.com/#top')).toContain('fragment');
    expect(urlProblem('https://localhost/hook')).toContain('public host');
    expect(urlProblem('https://intranet/hook')).toContain('public host');
    expect(urlProblem(`https://example.com/${'a'.repeat(2048)}`)).toContain('2,048');
  });
});

describe('webhook form', () => {
  it('builds an endpoint, dropping an empty description', () => {
    expect(webhookInput({ url: ' https://hooks.example.com/a ', description: ' ', eventTypes: ['reconciliation.updated'], enabled: false }))
      .toEqual({ input: { url: 'https://hooks.example.com/a', description: null, event_types: ['reconciliation.updated'], enabled: false } });
  });

  it('needs an event and a short description', () => {
    expect(webhookInput({ url: 'https://hooks.example.com', description: 'x'.repeat(201), eventTypes: [], enabled: true }))
      .toEqual({ errors: { description: 'Use at most 200 characters.', eventTypes: 'Choose at least one event.' } });
  });

  it('sends only what changed, clearing a description with an empty one', () => {
    const values = formValues(endpoint);
    const unchanged = webhookInput(values);
    expect('input' in unchanged && webhookUpdate(endpoint, unchanged.input)).toBeNull();
    const changed = webhookInput({ ...values, description: '', eventTypes: ['reconciliation.opened', 'inference.finalized'], enabled: false });
    expect('input' in changed && webhookUpdate(endpoint, changed.input)).toEqual({
      description: '', event_types: ['reconciliation.opened', 'inference.finalized'], enabled: false,
    });
  });

  it('after a conflict, keeps edited fields and takes the current values of the rest', () => {
    const current = { ...endpoint, description: 'Changed elsewhere', enabled: false, version: 5 };
    const edited = { ...formValues(endpoint), url: 'https://hooks.example.com/v2', eventTypes: ['reconciliation.opened' as const] };
    expect(rebaseValues(edited, endpoint, current)).toEqual({
      url: 'https://hooks.example.com/v2', description: 'Changed elsewhere', eventTypes: ['reconciliation.opened'], enabled: false,
    });
  });

  it('ignores the order of event types', () => {
    const both = { ...endpoint, event_types: ['inference.finalized', 'reconciliation.opened'] } as WebhookEndpoint;
    const built = webhookInput({ ...formValues(both), eventTypes: ['reconciliation.opened', 'inference.finalized'] });
    expect('input' in built && webhookUpdate(both, built.input)).toBeNull();
  });
});

describe('webhook secrets and deliveries', () => {
  it('tells a secret from a replay that has none', () => {
    expect(secretOutcome({ endpoint, secret: 'whsec_abc', replay: false, error: null })).toEqual({ secret: 'whsec_abc' });
    expect(secretOutcome({ endpoint, secret: null, replay: true, error: 'secret_unavailable_on_replay' })).toEqual({ replayed: true });
  });

  it('describes the last response', () => {
    expect(lastResponse(delivery)).toBe('HTTP 500');
    expect(lastResponse({ ...delivery, last_status_code: null, last_error: 'request timed out' })).toBe('request timed out');
    expect(lastResponse({ ...delivery, last_status_code: null, attempts: 0 })).toBe('Not attempted yet');
  });

  it('picks the time that matters for the status', () => {
    expect(deliveryTime(delivery)).toEqual({ label: 'Last attempt', at: '2026-10-02T00:00:00Z' });
    expect(deliveryTime({ ...delivery, status: 'delivered', delivered_at: '2026-10-03T00:00:00Z' }).label).toBe('Delivered');
    expect(deliveryTime({ ...delivery, status: 'pending', next_attempt_at: '2026-10-04T00:00:00Z' }).label).toBe('Next attempt');
    expect(deliveryTime({ ...delivery, last_attempt_at: null }).label).toBe('Created');
  });
});
