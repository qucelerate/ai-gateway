import { describe, expect, it } from 'vitest';
import type { AutoOffer, CatalogSyncJob, DiscoveredModel } from './api';
import { capabilityText, errorText, evidenceText, isActive, jobSummary, listingState, offerErrorText, offerLabel, offerSummary, offerText, pollDelay } from './discovery-model';

const job: CatalogSyncJob = {
  id: 'j-1', adapter_instance_key: 'bedrock-1', provider_family: 'bedrock', trigger: 'manual', status: 'succeeded', attempts: 1,
  error_code: null, models_listed: 12, models_added: 2, models_missing: 1, requested_by: 'u-1',
  created_at: '2026-10-13T10:00:00Z', started_at: '2026-10-13T10:00:10Z', finished_at: '2026-10-13T10:00:20Z',
};
const model: DiscoveredModel = {
  adapter_instance_key: 'bedrock-1', provider_family: 'bedrock', model: 'anthropic.claude', display_name: 'Claude',
  first_seen_at: '2026-10-01T00:00:00Z', last_seen_at: '2026-10-13T00:00:00Z', missing_since: null, deployed: false,
  operations: null, availability: 'listed', evidence: null, deployment: null,
};

describe('discovery jobs', () => {
  it('knows which jobs are still in flight', () => {
    expect(isActive({ status: 'queued' })).toBe(true);
    expect(isActive({ status: 'running' })).toBe(true);
    for (const status of ['succeeded', 'failed', 'unsupported'] as const) expect(isActive({ status })).toBe(false);
  });

  it('backs off to every 15 seconds', () => {
    expect([0, 1, 2, 3, 4, 10].map(pollDelay)).toEqual([2000, 4000, 8000, 15_000, 15_000, 15_000]);
  });

  it('summarizes each outcome in plain words', () => {
    expect(jobSummary(job)).toBe('Listed 12 models: 2 new, 1 no longer listed.');
    expect(jobSummary({ ...job, models_listed: 1, models_added: 0, models_missing: 0 })).toBe('Listed 1 model: 0 new, 0 no longer listed.');
    expect(jobSummary({ ...job, status: 'queued' })).toContain('within about a minute');
    expect(jobSummary({ ...job, status: 'unsupported' })).toContain('cannot list its models');
    expect(jobSummary({ ...job, status: 'failed', error_code: 'not_configured' })).toContain('Add one under Credentials');
  });

  it('explains error codes, and names one it does not know', () => {
    expect(errorText('rate_limited')).toContain('Try again in a few minutes');
    expect(errorText('something_new')).toBe('The listing failed (something_new).');
    expect(errorText(null)).toBe('The listing failed.');
  });
});

describe('discovered models', () => {
  it('says whether the provider still lists a model', () => {
    expect(listingState(model)).toEqual({ listed: true, text: 'Listed' });
    expect(listingState({ ...model, missing_since: '2026-10-12T00:00:00Z' }).listed).toBe(false);
  });

  it('says when a listing cannot confirm the account’s location offers a model', () => {
    expect(listingState({ ...model, availability: 'unknown' })).toEqual({ listed: true, text: 'Listed; location not confirmed' });
    expect(listingState({ ...model, availability: 'profile_only' })).toEqual({ listed: true, text: 'Listed; call it through an inference profile' });
    expect(listingState({ ...model, availability: 'endpoint_only' })).toEqual({ listed: true, text: 'Listed; an endpoint serving what the gateway does not call' });
  });

  it('never presents unknown capabilities as supported', () => {
    expect(capabilityText(model)).toBe('unknown');
    expect(capabilityText({ ...model, deployed: true })).toBe('As set in the catalog');
  });

  it('shows the operations a listing reported', () => {
    expect(capabilityText({ ...model, operations: ['chat', 'chat_stream'] })).toBe('Listed for chat');
    expect(capabilityText({ ...model, operations: ['embeddings'] })).toBe('Listed for embeddings');
  });
});

describe('automatic offers', () => {
  const offer: AutoOffer = {
    adapter_instance_key: 'bedrock-1', location: 'us-east-1', created_at: '2026-10-27T10:00:00Z', updated_at: '2026-10-27T10:00:00Z',
    applied_at: '2026-10-27T10:01:00Z', applied_release_id: 'r-1', error_code: null, version: 1,
    models: [
      { model: 'a', deployment: 'bedrock-1-a', status: 'offered', price_source: 'price-sheet:bedrock', price_item: 'a', updated_at: '2026-10-27T10:01:00Z' },
      { model: 'b', deployment: 'bedrock-1-b', status: 'needs_price', price_source: null, price_item: null, updated_at: '2026-10-27T10:01:00Z' },
    ],
  };

  it('summarizes the latest run', () => {
    expect(offerSummary(offer)).toBe('One model offered; one needs a price.');
    expect(offerSummary({ ...offer, models: [] })).toBe('0 models offered.');
    expect(offerSummary({ ...offer, applied_at: null })).toContain('next listing');
    expect(offerSummary({ ...offer, error_code: 'no_active_release' })).toBe('The latest run changed nothing: The organization has no active release.');
  });

  it('says how to price a model', () => {
    expect(offerText('needs_price', 'bedrock-1-b')).toContain('Map bedrock-1-b to a published item');
    expect(offerErrorText('something_else')).toBe('It failed (something_else).');
  });
});

describe('model evidence', () => {
  const seen = { source: 'traffic' as const, deployment: 'bedrock-1-anthropic.claude', observed_at: '2026-11-03T00:00:00Z', verified_at: null };
  it('says nothing until a call is seen', () => {
    expect(evidenceText(model)).toBeNull();
  });
  it('names a success', () => {
    expect(evidenceText({ ...model, evidence: { ...seen, state: 'verified', reason: null, provider_status: null, provider_code: null } })).toBe('A call succeeded');
  });
  it('says why a model was refused, with the provider’s error type', () => {
    expect(evidenceText({ ...model, evidence: { ...seen, state: 'refused', reason: 'access_denied', provider_status: 403, provider_code: 'AccessDeniedException' } }))
      .toBe('Refused: access denied for this account (AccessDeniedException)');
    expect(evidenceText({ ...model, evidence: { ...seen, state: 'refused', reason: 'not_found', provider_status: 404, provider_code: null } }))
      .toBe('Refused: not found where the gateway calls it (HTTP 404)');
  });
  it('explains a refused offer', () => {
    expect(offerText('refused', 'd')).toContain('refused for this account');
  });
  it('explains an offer held back until the tenant acts', () => {
    expect(offerLabel.requires_action).toBe('Needs action');
    expect(offerText('requires_action', 'd')).toContain('not offered');
  });
});
