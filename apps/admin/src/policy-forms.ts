import type { BudgetInput, BudgetPeriod, RateLimitInput, RetentionPeriods, TraceMode, TracePolicyInput } from './api';
import { parseMoney } from './money';

// Client-side checks matching the policy contracts, so a form explains a
// mistake before the server refuses it.

type Parsed = { value: number | null } | { error: string };

/** A whole number in [min, max]; empty means none. */
export function parseWhole(text: string, min: number, max: number): Parsed {
  const value = text.trim();
  if (!value) return { value: null };
  if (!/^\d+$/.test(value)) return { error: 'Enter a whole number.' };
  const number = Number(value);
  if (number < min || number > max) return { error: `Enter ${min.toLocaleString()} to ${max.toLocaleString()}.` };
  return { value: number };
}

export function rateLimitInput(requestsPerMinute: string, maxConcurrent: string, tokensPerMinute = ''): { limits: RateLimitInput } | { errors: Partial<Record<keyof RateLimitInput | 'form', string>> } {
  const rpm = parseWhole(requestsPerMinute, 1, 1_000_000);
  const tpm = parseWhole(tokensPerMinute, 1, 1_000_000_000);
  const concurrent = parseWhole(maxConcurrent, 1, 1_000_000);
  const errors: Partial<Record<keyof RateLimitInput | 'form', string>> = {};
  if ('error' in rpm) errors.requests_per_minute = rpm.error;
  if ('error' in tpm) errors.tokens_per_minute = tpm.error;
  if ('error' in concurrent) errors.max_concurrent = concurrent.error;
  if (Object.keys(errors).length) return { errors };
  const value = (parsed: Parsed) => (parsed as { value: number | null }).value;
  const limits = { requests_per_minute: value(rpm), tokens_per_minute: value(tpm), max_concurrent: value(concurrent) };
  if (limits.requests_per_minute === null && limits.tokens_per_minute === null && limits.max_concurrent === null) return { errors: { form: 'Set at least one limit, or remove the policy.' } };
  return { limits };
}

export function retentionInput(values: Record<keyof RetentionPeriods, string>): { periods: RetentionPeriods } | { errors: Partial<Record<keyof RetentionPeriods, string>> } {
  const trace = parseWhole(values.trace_days, 7, 3650);
  const usage = parseWhole(values.usage_days, 30, 3650);
  const audit = parseWhole(values.audit_days, 90, 3650);
  const errors: Partial<Record<keyof RetentionPeriods, string>> = {};
  if ('error' in trace) errors.trace_days = trace.error;
  if ('error' in usage) errors.usage_days = usage.error;
  if ('error' in audit) errors.audit_days = audit.error;
  if (Object.keys(errors).length) return { errors };
  const periods = {
    trace_days: (trace as { value: number | null }).value,
    usage_days: (usage as { value: number | null }).value,
    audit_days: (audit as { value: number | null }).value,
  };
  // A trace is removed with its usage, so it can't outlive it.
  if (periods.usage_days !== null && periods.trace_days === null) return { errors: { trace_days: 'Set how long traces are kept when usage has a period.' } };
  if (periods.usage_days !== null && periods.trace_days !== null && periods.trace_days > periods.usage_days) return { errors: { trace_days: 'Traces can’t be kept longer than usage.' } };
  return { periods };
}

/** A budget from the form: an amount in major units (dollars), and for a
 * fixed budget the first and last UTC days it covers. */
export function budgetInput(values: { amount: string; currency: string; period: BudgetPeriod; fromDate: string; toDate: string }):
  { budget: BudgetInput } | { errors: Partial<Record<'amount' | 'currency' | 'dates', string>> } {
  const currency = values.currency.trim().toUpperCase();
  if (!/^[A-Z]{3}$/.test(currency)) return { errors: { currency: 'Enter a three-letter currency code, such as USD.' } };
  const limit = parseMoney(values.amount, currency);
  if (limit === null) return { errors: { amount: `Enter an amount in ${currency}, such as 100 or 12.50.` } };
  if (values.period === 'monthly') return { budget: { currency, limit_minor: limit, period: 'monthly' } };
  if (!values.fromDate || !values.toDate) return { errors: { dates: 'Choose the first and last day.' } };
  const starts = new Date(`${values.fromDate}T00:00:00Z`);
  const ends = new Date(new Date(`${values.toDate}T00:00:00Z`).getTime() + 86_400_000);
  if (Number.isNaN(starts.getTime()) || Number.isNaN(ends.getTime()) || ends <= starts) return { errors: { dates: 'The last day must not be before the first.' } };
  return { budget: { currency, limit_minor: limit, period: 'fixed', starts_at: starts.toISOString(), ends_at: ends.toISOString() } };
}


/** A tracing policy from the form. `selected` needs a team or service
 * account; the other modes send none. */
export function traceInput(values: { mode: TraceMode; teamIds: string[]; serviceAccountIds: string[]; content: boolean; days: string }):
  { policy: TracePolicyInput } | { errors: Partial<Record<'selection' | 'days', string>> } {
  const days = parseWhole(values.days, 1, 3650);
  if ('error' in days) return { errors: { days: days.error } };
  if (days.value === null) return { errors: { days: 'Enter how many days content is kept.' } };
  const selected = values.mode === 'selected';
  if (selected && !values.teamIds.length && !values.serviceAccountIds.length) return { errors: { selection: 'Choose at least one team or service account.' } };
  if (selected && (values.teamIds.length > 200 || values.serviceAccountIds.length > 200)) return { errors: { selection: 'Choose at most 200 teams and 200 service accounts.' } };
  return {
    policy: {
      mode: values.mode,
      team_ids: selected ? [...values.teamIds].sort() : [],
      service_account_ids: selected ? [...values.serviceAccountIds].sort() : [],
      content: values.content,
      content_retention_days: days.value,
    },
  };
}
