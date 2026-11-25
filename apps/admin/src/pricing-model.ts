import type { CatalogOperation, PriceOverride, PriceOverrideInput, PriceSource, PriceSourceErrorCode, ProviderFamily } from './api';
import { perTokenPrice } from './add-provider-form';
import { formatMoney } from './money';

// Pricing in plain words, and the override form's checks, matching the
// catalog contract so the form explains a mistake before the server refuses
// it. Amounts are money units per token; people type prices per million
// tokens, as providers quote them.

const sourceErrors: Record<PriceSourceErrorCode, string> = {
  invalid_url: 'The configured address is not a valid URL.',
  destination_not_allowed: 'The configured address is not allowed (not a public https host).',
  dns_failed: 'The host name could not be resolved.',
  connection_failed: 'The connection failed.',
  timed_out: 'The source did not answer in time.',
  http_status: 'The source answered with an error status.',
  response_too_large: 'The response was too large.',
  request_failed: 'The request failed.',
  invalid_price_sheet: 'The price sheet is not valid.',
  invalid_price_list: 'The price list could not be read.',
};

/** How a price source is doing, without its address. */
export function sourceStatus(source: PriceSource): { badge: 'badge-success' | 'badge-danger' | 'badge-neutral'; label: string; text: string } {
  if (source.last_error_code) {
    const why = sourceErrors[source.last_error_code] ?? `The fetch failed (${source.last_error_code}).`;
    const kept = source.snapshot ? ' Drift still compares with the last prices fetched.' : '';
    return { badge: 'badge-danger', label: 'Failing', text: `${why}${kept} The gateway operator can check the source’s configuration.` };
  }
  if (!source.snapshot) return { badge: 'badge-neutral', label: 'No prices yet', text: 'No fetch has succeeded yet.' };
  return { badge: 'badge-success', label: 'OK', text: `${source.snapshot.observations.toLocaleString()} prices published.` };
}

export type PriceField = 'input' | 'output' | 'cached_input' | 'reasoning' | 'other';
export const priceFieldLabels: Record<PriceField, string> = {
  input: 'Input',
  output: 'Output',
  cached_input: 'Cached input',
  reasoning: 'Reasoning',
  other: 'Other',
};

/** The prices an override may set: a family that never reports usage for a
 * dimension can only bill it at zero, so the form leaves it out. */
export function billableFields(family: ProviderFamily | null, operation: CatalogOperation): PriceField[] {
  const fields: PriceField[] = operation === 'embeddings' ? ['input'] : ['input', 'output'];
  fields.push('cached_input');
  // Anthropic and Bedrock count thinking as output.
  if (family !== 'anthropic' && family !== 'bedrock') fields.push('reasoning');
  fields.push('other');
  return fields;
}

export type OverrideForm = {
  deployment: string;
  operation: CatalogOperation;
  currency: string;
  prices: Partial<Record<PriceField, string>>;
  reason: string;
  /** `datetime-local` values, in the browser's time zone; empty for now / open-ended. */
  from: string;
  until: string;
};
export type OverrideErrors = Partial<Record<'deployment' | 'currency' | 'reason' | 'window' | PriceField, string>>;

/** The override to create, or what to fix. A price finer than one money
 * unit per token is rounded up, and the override is then `conservative`. */
export function overrideInput(form: OverrideForm, family: ProviderFamily | null): { input: PriceOverrideInput; rounded: boolean } | { errors: OverrideErrors } {
  const errors: OverrideErrors = {};
  const currency = form.currency.trim().toUpperCase();
  if (!form.deployment) errors.deployment = 'Choose a deployment.';
  if (!/^[A-Z]{3}$/.test(currency)) errors.currency = 'Enter a three-letter currency code, such as USD.';
  const reason = form.reason.trim();
  if (!reason) errors.reason = 'Say why, for the audit log and whoever reads this later.';
  else if (reason.length > 1024) errors.reason = 'Use at most 1,024 characters.';
  const units: Partial<Record<PriceField, number>> = {};
  let exact = true;
  for (const field of billableFields(family, form.operation)) {
    const text = (form.prices[field] ?? '').trim();
    if (!text) {
      if (field === 'input' || field === 'output') errors[field] = 'Enter a price per million tokens, such as 2.50.';
      continue;
    }
    const price = errors.currency ? null : perTokenPrice(text, currency);
    if (!price) {
      if (!errors.currency) errors[field] = `Enter a price in ${currency} per million tokens, such as 2.50.`;
      continue;
    }
    units[field] = price.units;
    exact &&= price.exact;
  }
  const from = form.from ? new Date(form.from) : null;
  const until = form.until ? new Date(form.until) : null;
  if ((from && Number.isNaN(from.getTime())) || (until && Number.isNaN(until.getTime()))) errors.window = 'Choose valid times.';
  else if (until && until.getTime() <= (from ?? new Date()).getTime()) errors.window = 'The end must be after the start.';
  if (Object.keys(errors).length) return { errors };
  const input: PriceOverrideInput = {
    deployment: form.deployment,
    operation: form.operation,
    input_minor_per_token: units.input ?? 0,
    output_minor_per_token: units.output ?? 0,
    currency,
    confidence: exact ? 'known' : 'conservative',
    reason,
  };
  if (units.cached_input !== undefined) input.cached_input_minor_per_token = units.cached_input;
  if (units.reasoning !== undefined) input.reasoning_minor_per_token = units.reasoning;
  if (units.other !== undefined) input.other_minor_per_token = units.other;
  if (from) input.effective_from = from.toISOString();
  if (until) input.effective_until = until.toISOString();
  return { input, rounded: !exact };
}

/** An override's non-zero prices per million tokens, input and output always. */
export function overridePrices(override: PriceOverride): string {
  const per = (units: number) => formatMoney(units * 1_000_000, override.currency);
  const parts = [`in ${per(override.input_minor_per_token)}`];
  if (override.operation !== 'embeddings') parts.push(`out ${per(override.output_minor_per_token)}`);
  if (override.cached_input_minor_per_token) parts.push(`cached ${per(override.cached_input_minor_per_token)}`);
  if (override.reasoning_minor_per_token) parts.push(`reasoning ${per(override.reasoning_minor_per_token)}`);
  if (override.other_minor_per_token) parts.push(`other ${per(override.other_minor_per_token)}`);
  return parts.join(' · ');
}

export const overrideBadge: Record<PriceOverride['status'], string> = {
  active: 'badge-success',
  scheduled: 'badge-neutral',
  expired: 'badge-neutral',
  revoked: 'badge-danger',
};

/** Whether revoking still changes anything. */
export function revocable(override: PriceOverride): boolean {
  return override.status === 'active' || override.status === 'scheduled';
}
