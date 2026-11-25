import type { BudgetPolicy, ServiceAccount } from './api';

/** Whether a budget applies now: a budget with a start or an end covers
 * nothing outside them. */
export function budgetActive(budget: Pick<BudgetPolicy, 'starts_at' | 'ends_at'>, now = Date.now()): boolean {
  return (!budget.starts_at || Date.parse(budget.starts_at) <= now) && (!budget.ends_at || Date.parse(budget.ends_at) > now);
}

/**
 * Whether some budget covers a service account's requests now: the
 * organization's, its team's or its own, while it applies. A request no
 * budget covers is refused (`quota_exceeded`).
 */
export function budgetCovers(budgets: BudgetPolicy[], account: Pick<ServiceAccount, 'id' | 'team_id'>, now = Date.now()): boolean {
  return budgets.some(budget => appliesTo(budget, account, now));
}

function appliesTo(budget: BudgetPolicy, account: Pick<ServiceAccount, 'id' | 'team_id'>, now: number): boolean {
  return budgetActive(budget, now) && (
    budget.scope === 'organization'
    || (budget.scope === 'team' && budget.team_id !== null && budget.team_id === account.team_id)
    || (budget.scope === 'service-account' && budget.service_account_id === account.id));
}

/**
 * The currencies the account's requests would be refused in: a request is
 * charged against a budget in its model's price currency, so a budget in
 * another one does not cover it. With no priced model to go by, every
 * currency counts and only a missing budget is reported (as `['']`).
 */
export function uncoveredCurrencies(budgets: BudgetPolicy[], account: Pick<ServiceAccount, 'id' | 'team_id'>, priceCurrencies: string[], now = Date.now()): string[] {
  const applying = budgets.filter(budget => appliesTo(budget, account, now));
  if (priceCurrencies.length === 0) return applying.length ? [] : [''];
  const covered = new Set(applying.map(budget => budget.currency.toUpperCase()));
  return [...new Set(priceCurrencies.map(currency => currency.toUpperCase()))].filter(currency => !covered.has(currency)).sort();
}
