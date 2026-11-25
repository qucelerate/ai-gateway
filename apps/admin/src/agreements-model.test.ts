import { describe, expect, it } from 'vitest';
import type { AgreementModel, AgreementOffer } from './api';
import { acceptAllPairs, acceptPairs, acceptable, allRatesListed, offerFor, pruneOffers, rateSummary, statusText } from './agreements-model';
import { docsLink } from './header';
import { accessNote, cloudOf, needsAccessRequest } from './cloud-access';

const offer = (id: string, rates = 2): AgreementOffer => ({
  offer_id: id,
  rate_card: Array.from({ length: rates }, (_, index) => ({ dimension: `USE2_rate_${index}`, price: '0.001', unit: 'Units', description: null })),
  rate_card_region: 'USE2',
  rate_card_total: 570,
  legal_url: null,
  refund_policy: 'No refunds',
  agreement_duration: null,
});
const model = (name: string, overrides: Partial<AgreementModel> = {}): AgreementModel => ({
  model: name, unblocks: [name], deployments: [], agreement_required: true, offers: [offer(`offer-${name}`)], offers_error: null, agreement: null, ...overrides,
});
const agreement = (status: 'pending' | 'available' | 'verified' | 'refused' | 'failed') => ({
  status, reason: null, detail: null, provider_code: null, offer_id: 'o', accepted_by: null, accepted_at: '2026-11-19T00:00:00Z',
  checked_at: null, verified_at: null, probe: false, probed_model: null, checking: false,
});

describe('model agreements', () => {
  it('offers only models that need an agreement, have an offer, and are not accepted', () => {
    expect(acceptable(model('a'))).toBe(true);
    expect(acceptable(model('a', { offers: [] }))).toBe(false);
    expect(acceptable(model('a', { agreement_required: false }))).toBe(false);
    expect(acceptable(model('a', { agreement: agreement('pending') }))).toBe(false);
    expect(acceptable(model('a', { agreement: agreement('verified') }))).toBe(false);
    // A failed attempt may be tried again; a refused one already has its agreement.
    expect(acceptable(model('a', { agreement: agreement('failed') }))).toBe(true);
    expect(acceptable(model('a', { agreement: agreement('refused') }))).toBe(false);
  });

  it('pairs each picked model with its chosen offer, else its only one, and never guesses among several', () => {
    const models = [model('a', { offers: [offer('o-1'), offer('o-2')] }), model('b'), model('c', { offers: [] })];
    expect(acceptPairs(models, new Set(['a', 'b', 'c']), { a: 'o-2' })).toEqual([
      { model: 'a', offer_id: 'o-2' },
      { model: 'b', offer_id: 'offer-b' },
    ]);
    expect(acceptPairs(models, new Set(['a', 'b']), {})).toEqual([{ model: 'b', offer_id: 'offer-b' }]);
    expect(offerFor(models[0], {})).toBeNull();
    // Accept all: single readable offers, plus chosen ones; not unreadable ones.
    const unreadable = model('d', { offers: [], offers_error: 'agreement_permission_missing' });
    expect(acceptAllPairs([...models, unreadable], {})).toEqual([{ model: 'b', offer_id: 'offer-b' }]);
    expect(acceptAllPairs([...models, unreadable], { a: 'o-1' })).toEqual([{ model: 'a', offer_id: 'o-1' }, { model: 'b', offer_id: 'offer-b' }]);
    expect(pruneOffers(models, { a: 'o-9', b: 'offer-b', z: 'x' })).toEqual({ b: 'offer-b' });
  });

  it('summarizes the region’s rates, and says an available model was not yet called', () => {
    expect(rateSummary(offer('o', 5), 2)).toBe('USE2_rate_0: 0.001 Units; USE2_rate_1: 0.001 Units; and 3 more (USE2’s rates)');
    // No region: the card may have been cut to its first rates.
    const cut = { ...offer('o', 4), rate_card_region: null, rate_card_total: 570 };
    expect(rateSummary(cut, 2)).toBe('USE2_rate_0: 0.001 Units; USE2_rate_1: 0.001 Units; and 2 more (the first 4 of 570 rates)');
    expect(allRatesListed(cut)).toBe(false);
    expect(allRatesListed({ ...cut, rate_card_total: 4 })).toBe(true);
    expect(allRatesListed(offer('o', 4))).toBe(true);
    expect(rateSummary(offer('o', 0))).toBe('No rates listed');
    expect(statusText({ status: 'available', reason: 'not_verified' })).toBe('Available, not yet called');
    expect(statusText({ status: 'available', reason: null })).toBe('Available');
    expect(statusText({ status: 'verified', reason: null })).toBe('Verified');
  });

  it('notes per cloud that access may need a request to the provider', () => {
    expect(cloudOf('bedrock')).toBe('aws');
    expect(cloudOf('azure-openai')).toBe('azure');
    expect(cloudOf('gemini')).toBe('google');
    expect(accessNote('bedrock')).toContain('AWS grants on request');
    expect(accessNote('azure-ai-foundry')).toContain('Limited Access');
    expect(accessNote('gemini')).toContain('Model Garden');
    expect(accessNote('anthropic')).toBeNull();
    expect(needsAccessRequest({ state: 'requires_action', reason: 'agreement_required' })).toBe(true);
    expect(needsAccessRequest({ state: 'refused', reason: 'access_denied' })).toBe(true);
    expect(needsAccessRequest({ state: 'refused', reason: 'not_found' })).toBe(false);
    expect(needsAccessRequest({ state: 'requires_action', reason: 'permission_missing' })).toBe(false);
    expect(needsAccessRequest({ state: 'requires_action', reason: 'quota_zero' })).toBe(true);
    expect(needsAccessRequest({ state: 'requires_action', reason: 'enable_in_model_garden' })).toBe(true);
    expect(needsAccessRequest({ state: 'requires_action', reason: 'policy_denied' })).toBe(false);
    expect(needsAccessRequest(null)).toBe(false);
  });

  it('links the documentation site’s page when the build names one, else the repository’s file', () => {
    expect(docsLink('guide/providers#x', 'https://docs.example.com')).toBe('https://docs.example.com/guide/providers#x');
    expect(docsLink('guide/providers#x', null)).toBe('https://gateway.qucelerate.com/guide/providers#x');
  });
});
