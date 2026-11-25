import { describe, expect, it } from 'vitest';
import type { PriceOverride, PriceSource } from './api';
import { billableFields, overrideInput, overridePrices, revocable, sourceStatus, type OverrideForm } from './pricing-model';

const source: PriceSource = {
  source: 'azure-retail:azure-openai', kind: 'azure-retail', provider_family: 'azure-openai',
  last_attempt_at: '2026-10-13T06:00:00Z', last_success_at: '2026-10-13T06:00:05Z', last_error_code: null,
  next_fetch_at: '2026-10-14T06:00:00Z',
  snapshot: { id: 's-1', source_version: null, fetched_at: '2026-10-10T06:00:00Z', confirmed_at: '2026-10-13T06:00:05Z', observations: 1200, unmapped: 1200 },
};
const form: OverrideForm = {
  deployment: 'openai-gpt-4o', operation: 'chat', currency: 'usd', prices: { input: '2.50', output: '10' },
  reason: '  Negotiated rate  ', from: '', until: '',
};

describe('price sources', () => {
  it('says how each source is doing, never where it is', () => {
    expect(sourceStatus(source)).toMatchObject({ badge: 'badge-success', label: 'OK' });
    expect(sourceStatus({ ...source, snapshot: null })).toMatchObject({ label: 'No prices yet' });
    const failing = sourceStatus({ ...source, last_error_code: 'timed_out' });
    expect(failing).toMatchObject({ badge: 'badge-danger', label: 'Failing' });
    expect(failing.text).toContain('did not answer in time');
    expect(failing.text).toContain('last prices fetched');
  });
});

describe('price overrides', () => {
  it('offers only the prices a family can bill', () => {
    expect(billableFields('anthropic', 'chat')).toEqual(['input', 'output', 'cached_input', 'other']);
    expect(billableFields('bedrock', 'chat')).toEqual(['input', 'output', 'cached_input', 'other']);
    expect(billableFields('openai', 'chat')).toEqual(['input', 'output', 'cached_input', 'reasoning', 'other']);
    expect(billableFields('openai', 'embeddings')).toEqual(['input', 'cached_input', 'reasoning', 'other']);
  });

  it('builds an exact override in money units per token', () => {
    expect(overrideInput(form, 'openai')).toEqual({
      input: {
        deployment: 'openai-gpt-4o', operation: 'chat', input_minor_per_token: 250, output_minor_per_token: 1000,
        currency: 'USD', confidence: 'known', reason: 'Negotiated rate',
      },
      rounded: false,
    });
  });

  it('rounds a finer price up and marks it conservative', () => {
    const built = overrideInput({ ...form, prices: { ...form.prices, cached_input: '0.0000001' } }, 'openai');
    expect(built).toMatchObject({ rounded: true, input: { cached_input_minor_per_token: 1, confidence: 'conservative' } });
  });

  it('ignores prices the family cannot bill', () => {
    const built = overrideInput({ ...form, prices: { ...form.prices, reasoning: '5' } }, 'anthropic');
    expect('input' in built && built.input.reasoning_minor_per_token).toBeUndefined();
  });

  it('needs a reason, prices, a currency and a window that ends after it starts', () => {
    expect(overrideInput({ ...form, deployment: '', currency: 'dollars', reason: ' ', prices: {} }, null)).toEqual({
      errors: {
        deployment: 'Choose a deployment.',
        currency: 'Enter a three-letter currency code, such as USD.',
        reason: 'Say why, for the audit log and whoever reads this later.',
        input: 'Enter a price per million tokens, such as 2.50.',
        output: 'Enter a price per million tokens, such as 2.50.',
      },
    });
    expect(overrideInput({ ...form, from: '2026-11-02T00:00', until: '2026-11-01T00:00' }, 'openai')).toEqual({ errors: { window: 'The end must be after the start.' } });
    expect(overrideInput({ ...form, prices: { input: 'free', output: '1' } }, 'openai')).toEqual({ errors: { input: 'Enter a price in USD per million tokens, such as 2.50.' } });
  });

  it('sends the window as UTC times', () => {
    const built = overrideInput({ ...form, from: '2026-11-01T09:00', until: '2026-12-01T09:00' }, 'openai');
    expect('input' in built && built.input.effective_from).toBe(new Date('2026-11-01T09:00').toISOString());
    expect('input' in built && built.input.effective_until).toBe(new Date('2026-12-01T09:00').toISOString());
  });

  it('shows prices per million and which overrides can still be revoked', () => {
    const override = {
      id: 'o-1', deployment: 'd', operation: 'chat', input_minor_per_token: 250, output_minor_per_token: 1000, cached_input_minor_per_token: 0,
      reasoning_minor_per_token: 0, other_minor_per_token: 0, currency: 'USD', status: 'active',
    } as PriceOverride;
    expect(overridePrices(override)).toBe('in $2.50 · out $10.00');
    expect(overridePrices({ ...override, operation: 'embeddings', cached_input_minor_per_token: 125 })).toBe('in $2.50 · cached $1.25');
    expect(revocable(override)).toBe(true);
    expect(revocable({ ...override, status: 'scheduled' })).toBe(true);
    expect(revocable({ ...override, status: 'expired' })).toBe(false);
    expect(revocable({ ...override, status: 'revoked' })).toBe(false);
  });
});
