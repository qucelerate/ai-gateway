import { describe, expect, it } from 'vitest';
import { budgetInput, parseWhole, rateLimitInput, retentionInput, traceInput } from './policy-forms';

describe('policy forms', () => {
  it('reads whole numbers in range, empty as none', () => {
    expect(parseWhole(' 60 ', 1, 100)).toEqual({ value: 60 });
    expect(parseWhole('', 1, 100)).toEqual({ value: null });
    expect('error' in parseWhole('1.5', 1, 100)).toBe(true);
    expect('error' in parseWhole('0', 1, 100)).toBe(true);
  });

  it('needs at least one rate limit', () => {
    expect(rateLimitInput('600', '')).toEqual({ limits: { requests_per_minute: 600, tokens_per_minute: null, max_concurrent: null } });
    expect(rateLimitInput('', '', '50000')).toEqual({ limits: { requests_per_minute: null, tokens_per_minute: 50_000, max_concurrent: null } });
    expect(rateLimitInput('', '', '')).toEqual({ errors: { form: 'Set at least one limit, or remove the policy.' } });
    expect('errors' in rateLimitInput('2000000', '')).toBe(true);
    expect(rateLimitInput('', '', '1000000001')).toEqual({ errors: { tokens_per_minute: 'Enter 1 to 1,000,000,000.' } });
  });

  it('keeps traces no longer than usage, and only within the allowed periods', () => {
    expect(retentionInput({ trace_days: '30', usage_days: '365', audit_days: '' })).toEqual({ periods: { trace_days: 30, usage_days: 365, audit_days: null } });
    expect(retentionInput({ trace_days: '', usage_days: '', audit_days: '' })).toEqual({ periods: { trace_days: null, usage_days: null, audit_days: null } });
    expect(retentionInput({ trace_days: '', usage_days: '90', audit_days: '' })).toEqual({ errors: { trace_days: 'Set how long traces are kept when usage has a period.' } });
    expect(retentionInput({ trace_days: '400', usage_days: '90', audit_days: '' })).toEqual({ errors: { trace_days: 'Traces can’t be kept longer than usage.' } });
    expect('errors' in retentionInput({ trace_days: '3', usage_days: '', audit_days: '30' })).toBe(true);
  });

  it('reads a budget in dollars, monthly or for whole UTC days', () => {
    const form = { amount: '100', currency: 'usd', period: 'monthly' as const, fromDate: '', toDate: '' };
    expect(budgetInput(form)).toEqual({ budget: { currency: 'USD', limit_minor: 10_000_000_000, period: 'monthly' } });
    expect(budgetInput({ ...form, period: 'fixed', fromDate: '2026-10-01', toDate: '2026-10-01' })).toEqual({
      budget: { currency: 'USD', limit_minor: 10_000_000_000, period: 'fixed', starts_at: '2026-10-01T00:00:00.000Z', ends_at: '2026-10-02T00:00:00.000Z' },
    });
    expect('errors' in budgetInput({ ...form, amount: '-5' })).toBe(true);
    expect('errors' in budgetInput({ ...form, currency: 'dollars' })).toBe(true);
    expect('errors' in budgetInput({ ...form, period: 'fixed', fromDate: '2026-10-05', toDate: '2026-10-01' })).toBe(true);
  });

  it('traces selected teams and service accounts only when some are chosen', () => {
    const base = { mode: 'selected' as const, teamIds: ['b', 'a'], serviceAccountIds: [], content: true, days: '30' };
    expect(traceInput(base)).toEqual({ policy: { mode: 'selected', team_ids: ['a', 'b'], service_account_ids: [], content: true, content_retention_days: 30 } });
    expect(traceInput({ ...base, teamIds: [] })).toEqual({ errors: { selection: 'Choose at least one team or service account.' } });
    // Other modes send no selection, whatever the form still holds.
    expect(traceInput({ ...base, mode: 'organization' })).toEqual({ policy: { mode: 'organization', team_ids: [], service_account_ids: [], content: true, content_retention_days: 30 } });
    expect('errors' in traceInput({ ...base, days: '0' })).toBe(true);
    expect('errors' in traceInput({ ...base, days: '' })).toBe(true);
    // A selection over the limit no longer matters once another mode is chosen.
    const many = Array.from({ length: 201 }, (_, index) => `t-${index}`);
    expect('errors' in traceInput({ ...base, teamIds: many })).toBe(true);
    expect('policy' in traceInput({ ...base, mode: 'organization', teamIds: many })).toBe(true);
  });
});
