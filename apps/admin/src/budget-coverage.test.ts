import { describe, expect, it } from 'vitest';
import type { BudgetPolicy } from './api';
import { budgetActive, budgetCovers, uncoveredCurrencies } from './budget-coverage';

const budget = (scope: BudgetPolicy['scope'], target: { team_id?: string | null; service_account_id?: string | null } = {}) =>
  ({ scope, currency: 'USD', team_id: null, service_account_id: null, ...target }) as BudgetPolicy;

describe('budget coverage', () => {
  const account = { id: 'sa', team_id: 'team' };
  it('is covered by an organization budget, its team’s, or its own', () => {
    expect(budgetCovers([budget('organization')], account)).toBe(true);
    expect(budgetCovers([budget('team', { team_id: 'team' })], account)).toBe(true);
    expect(budgetCovers([budget('service-account', { service_account_id: 'sa' })], account)).toBe(true);
  });
  it('is not covered by none, another team’s, or another account’s', () => {
    expect(budgetCovers([], account)).toBe(false);
    expect(budgetCovers([budget('team', { team_id: 'other' }), budget('service-account', { service_account_id: 'other' })], account)).toBe(false);
    expect(budgetCovers([budget('team')], { id: 'sa', team_id: null })).toBe(false);
  });
  it('is not covered by one key’s budget, which covers only that key', () => {
    expect(budgetCovers([{ ...budget('api-key'), api_key_id: 'key' }], account)).toBe(false);
  });
  it('is not covered by a budget that has ended or not yet started', () => {
    const now = Date.parse('2026-11-12T12:00:00Z');
    const window = (starts_at: string | null, ends_at: string | null) => ({ ...budget('organization'), starts_at, ends_at }) as BudgetPolicy;
    expect(budgetCovers([window('2026-01-01T00:00:00Z', '2026-11-01T00:00:00Z')], account, now)).toBe(false);
    expect(budgetCovers([window('2026-12-01T00:00:00Z', null)], account, now)).toBe(false);
    expect(budgetCovers([window('2026-11-01T00:00:00Z', '2026-12-01T00:00:00Z')], account, now)).toBe(true);
    expect(budgetActive({ starts_at: null, ends_at: null }, now)).toBe(true);
  });

  it('counts a budget only in its own currency', () => {
    const eur = { ...budget('organization'), currency: 'EUR' } as BudgetPolicy;
    expect(uncoveredCurrencies([eur], account, ['USD', 'usd'])).toEqual(['USD']);
    expect(uncoveredCurrencies([eur, budget('team', { team_id: 'team' })], account, ['USD', 'EUR'])).toEqual([]);
    expect(uncoveredCurrencies([eur], account, ['USD', 'GBP', 'EUR'])).toEqual(['GBP', 'USD']);
  });

  it('without priced models, reports only a missing budget', () => {
    expect(uncoveredCurrencies([], account, [])).toEqual(['']);
    expect(uncoveredCurrencies([{ ...budget('organization'), currency: 'EUR' } as BudgetPolicy], account, [])).toEqual([]);
  });
});
