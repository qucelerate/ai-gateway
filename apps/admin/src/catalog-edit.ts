import type { CatalogManifest, CatalogOperation, ProviderFamily } from './api';
import { perTokenPrice } from './add-provider-form';
import { releaseVersion, type CatalogEdit } from './catalog-change';
import { toMajor } from './money';

// Changes to the active catalog, each a new release (see catalog-change).
// They are applied to the release active when they are sent, by name: a
// deployment or account removed meanwhile is reported, not recreated.

type Manifest = CatalogManifest;
type Price = Manifest['prices'][number];

/** Limits of one operation, as the form holds them. */
export type LimitsForm = { maxInputBytes: string; maxOutputTokens: string; reservationTtlSeconds: string };
/** A price per million tokens, as the form holds it. */
export type PriceForm = { input: string; output: string; cachedInput: string };
/** `tools`: whether the deployment takes tool definitions (D-032); chat operations only. */
export type OperationForm = { operation: CatalogOperation; currency: string; limits: LimitsForm; price: PriceForm; tools: boolean };

/** The operations with their own `tools` flag: chat (streaming is a mode of it) and chat batches. */
export const takesToolsFlag = (operation: CatalogOperation) => operation === 'chat' || operation === 'chat_batch';

const MAX_TTL_SECONDS = 86_400;
const MAX_U32 = 4_294_967_295;

function positiveInteger(value: string, max: number): number | null {
  if (!/^\d+$/.test(value.trim())) return null;
  const number = Number(value.trim());
  return number > 0 && number <= max && Number.isSafeInteger(number) ? number : null;
}

/** A price as the form shows it: per million tokens, in major units. */
export function perMillionInput(perToken: number, currency: string): string {
  return toMajor(perToken * 1_000_000, currency);
}

/** The form of each operation of `deployment` in `manifest`. */
export function operationForms(manifest: Manifest, deployment: string): OperationForm[] {
  const found = manifest.deployments.find(item => item.name === deployment);
  if (!found) return [];
  return found.capabilities.map(capability => {
    const price = manifest.prices.find(item => item.deployment === deployment && item.operation === capability.operation);
    const currency = price?.currency ?? 'USD';
    return {
      operation: capability.operation,
      currency,
      limits: {
        maxInputBytes: String(capability.max_input_bytes),
        maxOutputTokens: String(capability.max_output_tokens),
        reservationTtlSeconds: String(capability.reservation_ttl_seconds),
      },
      price: {
        input: price ? perMillionInput(price.input_minor_per_token, currency) : '',
        output: price ? perMillionInput(price.output_minor_per_token, currency) : '',
        cachedInput: price ? perMillionInput(price.cached_input_minor_per_token, currency) : '0',
      },
      tools: capability.tools === true,
    };
  });
}

/** What is wrong with one operation's form, if anything. */
export function operationProblem(form: OperationForm): string | null {
  if (positiveInteger(form.limits.maxInputBytes, Number.MAX_SAFE_INTEGER) === null) return 'Maximum input is a whole number of bytes above 0.';
  if (positiveInteger(form.limits.maxOutputTokens, MAX_U32) === null) return 'Maximum output is a whole number of tokens above 0.';
  if (positiveInteger(form.limits.reservationTtlSeconds, MAX_TTL_SECONDS) === null) return `Reservation time is 1 to ${MAX_TTL_SECONDS} seconds.`;
  const fields: [string, string][] = [['input', form.price.input]];
  if (form.operation !== 'embeddings') fields.push(['output', form.price.output]);
  fields.push(['cached input', form.price.cachedInput]);
  for (const [label, value] of fields) {
    if (perTokenPrice(value, form.currency) === null) return `Enter the ${label} price per million tokens, like 2.50.`;
  }
  return null;
}

function familyOf(manifest: Manifest, key: string): ProviderFamily | null {
  return manifest.providers.find(provider => provider.adapter_instance_key === key)?.family ?? null;
}

/** Routes that stop working when `deployments` go, by alias: a route is
 * unique per alias and operation, so no other deployment serves it. */
export function brokenRoutes(manifest: Manifest, deployments: Set<string>): { alias: string; operations: CatalogOperation[] }[] {
  const byAlias = new Map<string, CatalogOperation[]>();
  for (const route of manifest.routes) {
    if (!deployments.has(route.deployment)) continue;
    byAlias.set(route.alias, [...(byAlias.get(route.alias) ?? []), route.operation]);
  }
  return [...byAlias].sort(([a], [b]) => a.localeCompare(b)).map(([alias, operations]) => ({ alias, operations }));
}

function without(manifest: Manifest, deployments: Set<string>): Manifest {
  return {
    ...manifest,
    deployments: manifest.deployments.filter(item => !deployments.has(item.name)),
    prices: manifest.prices.filter(item => !deployments.has(item.deployment)),
    routes: manifest.routes.filter(item => !deployments.has(item.deployment)),
  };
}

/** Remove one deployment, with its prices and the routes to it. */
export function removeDeployment(name: string, now: Date): CatalogEdit {
  return base => {
    if (!base?.deployments.some(item => item.name === name)) return { problem: `${name} is no longer in the catalog.` };
    const manifest = without(base, new Set([name]));
    manifest.release.version = releaseVersion(now, `- ${name}`);
    return manifest;
  };
}

/** Remove a provider account with every deployment it serves. Its stored
 * credential is kept, for the account being added back. */
export function removeProvider(key: string, now: Date): CatalogEdit {
  return base => {
    if (!base?.providers.some(item => item.adapter_instance_key === key)) return { problem: `${key} is no longer in the catalog.` };
    const names = new Set(base.deployments.filter(item => item.adapter_instance_key === key).map(item => item.name));
    const manifest = without(base, names);
    manifest.providers = manifest.providers.filter(item => item.adapter_instance_key !== key);
    manifest.release.version = releaseVersion(now, `- ${key}`);
    return manifest;
  };
}

/** Set what the admin changed in a deployment's operations: each field is
 * compared with the form as it was opened (`original`), and only changed
 * ones are written, so a change someone else made meanwhile to another
 * field stays. A price whose currency changed meanwhile is refused. */
export function editDeployment(name: string, original: OperationForm[], forms: OperationForm[], now: Date): CatalogEdit {
  return base => {
    const deployment = base?.deployments.find(item => item.name === name);
    if (!base || !deployment) return { problem: `${name} is no longer in the catalog.` };
    if (!familyOf(base, deployment.adapter_instance_key)) return { problem: `${name} is no longer in the catalog.` };
    const date = now.toISOString().slice(0, 10);
    let changed = false;
    for (const form of forms) {
      const problem = operationProblem(form);
      if (problem) return { problem: `${form.operation}: ${problem}` };
      const before = original.find(item => item.operation === form.operation);
      if (!before) return { problem: `${form.operation} was not in the form opened.` };
      const capability = deployment.capabilities.find(item => item.operation === form.operation);
      if (!capability) return { problem: `${name} no longer offers ${form.operation}.` };
      const limits: [keyof LimitsForm, 'max_input_bytes' | 'max_output_tokens' | 'reservation_ttl_seconds'][] = [
        ['maxInputBytes', 'max_input_bytes'],
        ['maxOutputTokens', 'max_output_tokens'],
        ['reservationTtlSeconds', 'reservation_ttl_seconds'],
      ];
      for (const [field, column] of limits) {
        if (form.limits[field].trim() === before.limits[field].trim()) continue;
        capability[column] = Number(form.limits[field].trim());
        changed = true;
      }
      if (takesToolsFlag(form.operation) && form.tools !== before.tools) {
        // Absent means false, and keeps the release's digest as it was.
        if (form.tools) capability.tools = true;
        else delete capability.tools;
        changed = true;
      }
      const dimensions: [keyof PriceForm, 'input_minor_per_token' | 'output_minor_per_token' | 'cached_input_minor_per_token'][] = [
        ['input', 'input_minor_per_token'],
        ['output', 'output_minor_per_token'],
        ['cachedInput', 'cached_input_minor_per_token'],
      ];
      const edited = dimensions.filter(([field]) => form.price[field].trim() !== before.price[field].trim()
        && (field !== 'output' || form.operation !== 'embeddings'));
      if (edited.length === 0) continue;
      const index = base.prices.findIndex(item => item.deployment === name && item.operation === form.operation);
      const current: Price | undefined = base.prices[index];
      if (current && current.currency !== form.currency) {
        return { problem: `${name} is now billed in ${current.currency}, not ${form.currency}; close and edit it again.` };
      }
      // A price the form was opened with and that is gone now cannot be
      // rebuilt from the edited fields alone.
      if (!current && before.price.input.trim() !== '') {
        return { problem: `${name} ${form.operation} is no longer priced; close and edit it again.` };
      }
      const price: Price = current ? { ...current } : {
        deployment: name,
        operation: form.operation,
        input_minor_per_token: 0,
        output_minor_per_token: 0,
        cached_input_minor_per_token: 0,
        reasoning_minor_per_token: 0,
        other_minor_per_token: 0,
        currency: form.currency,
        source: 'admin',
        source_version: date,
        confidence: 'known',
      };
      let exact = true;
      // A new price takes every field of the form; an existing one only
      // the edited ones.
      for (const [field, column] of current ? edited : dimensions.filter(([field]) => field !== 'output' || form.operation !== 'embeddings')) {
        const units = perTokenPrice(form.price[field], form.currency) ?? { units: 0, exact: true };
        price[column] = units.units;
        exact &&= units.exact;
      }
      price.source = 'admin';
      price.source_version = date;
      // Known only if it was and what changed is exact.
      price.confidence = exact && (!current || current.confidence === 'known') ? 'known' : 'conservative';
      if (index >= 0) base.prices[index] = price;
      else base.prices.push(price);
      changed = true;
    }
    if (!changed) return { problem: 'Nothing was changed.' };
    base.release.version = releaseVersion(now, `~ ${name}`);
    return base;
  };
}

/** A published price to adopt: money units per token per dimension, as the
 * drift report states them (a fraction where finer than one unit). */
export type PublishedPrice = {
  deployment: string;
  operation: CatalogOperation;
  currency: string;
  source: string;
  item: string | null;
  observed: Partial<Record<'input' | 'output' | 'cached_input' | 'reasoning' | 'other', string>>;
};

/** Money units per token, rounded up: a published fraction is billed as the
 * next whole unit, an upper bound. */
export function observedUnits(value: string): { units: number; exact: boolean } | null {
  const match = /^(\d+)(?:\.(\d+))?$/.exec(value);
  if (!match) return null;
  const whole = Number(match[1]);
  const exact = !match[2] || /^0+$/.test(match[2]);
  const units = exact ? whole : whole + 1;
  return Number.isSafeInteger(units) ? { units, exact } : null;
}

/** Adopt published prices. Each changes only what its source publishes;
 * the rest of the price stays. */
export function applyPublishedPrices(prices: PublishedPrice[], now: Date): CatalogEdit {
  return base => {
    if (!base) return { problem: 'No release is active.' };
    if (prices.length === 0) return { problem: 'Choose at least one price.' };
    for (const published of prices) {
      const index = base.prices.findIndex(item => item.deployment === published.deployment && item.operation === published.operation);
      if (index < 0) return { problem: `${published.deployment} ${published.operation} is no longer priced in the catalog.` };
      const current = base.prices[index];
      if (current.currency !== published.currency) return { problem: `${published.deployment} is billed in ${current.currency}, not ${published.currency}.` };
      const next = { ...current };
      let exact = true;
      for (const [dimension, value] of Object.entries(published.observed)) {
        if (value === undefined) continue;
        const units = observedUnits(value);
        if (!units) return { problem: `${published.deployment}: ${dimension} price ${value} is not a price.` };
        exact &&= units.exact;
        next[`${dimension as keyof PublishedPrice['observed']}_minor_per_token`] = units.units;
      }
      next.source = published.source.slice(0, 128);
      next.source_version = (published.item ?? now.toISOString().slice(0, 10)).slice(0, 128);
      // Known only if it was and stays exact: a dimension the source does
      // not publish keeps whatever made the price an estimate.
      next.confidence = exact && current.confidence === 'known' ? 'known' : 'conservative';
      base.prices[index] = next;
    }
    base.release.version = releaseVersion(now, prices.length === 1 ? `price ${prices[0].deployment}` : `${prices.length} prices`);
    return base;
  };
}

/** One deployment of a route as the route editor holds it. */
export type RouteEntry = { deployment: string; weight: string; priority: string };

export const MAX_ROUTE_DEPLOYMENTS = 8;

/** The deployments `alias` routes `operation` to, for the route editor. */
export function routeEntries(manifest: Manifest, alias: string, operation: CatalogOperation): RouteEntry[] {
  return manifest.routes
    .filter(route => route.alias === alias && route.operation === operation)
    .sort((a, b) => (a.priority ?? 1) - (b.priority ?? 1) || a.deployment.localeCompare(b.deployment))
    .map(route => ({ deployment: route.deployment, weight: String(route.weight ?? 1), priority: String(route.priority ?? 1) }));
}

/** Deployments that can serve `operation`: those with its limits and price. */
export function routableDeployments(manifest: Manifest, operation: CatalogOperation): string[] {
  return manifest.deployments
    .filter(deployment => deployment.capabilities.some(capability => capability.operation === operation)
      && manifest.prices.some(price => price.deployment === deployment.name && price.operation === operation))
    .map(deployment => deployment.name)
    .sort((a, b) => a.localeCompare(b));
}

const wholeIn = (value: string, max: number) => /^\d+$/.test(value.trim()) && Number(value) >= 1 && Number(value) <= max;

/** Why these entries cannot be one route, or null. */
export function routeProblem(manifest: Manifest, operation: CatalogOperation, entries: RouteEntry[]): string | null {
  if (entries.length === 0) return 'Route the alias to at least one deployment, or remove the deployment it reaches instead.';
  if (entries.length > MAX_ROUTE_DEPLOYMENTS) return `An alias routes an operation to at most ${MAX_ROUTE_DEPLOYMENTS} deployments.`;
  const names = new Set(entries.map(entry => entry.deployment));
  if (names.size !== entries.length) return 'Each deployment appears once.';
  for (const entry of entries) {
    if (!wholeIn(entry.weight, 1000)) return `The weight of ${entry.deployment} is a whole number from 1 to 1000.`;
    if (!wholeIn(entry.priority, 10)) return `The priority of ${entry.deployment} is a whole number from 1 to 10.`;
  }
  const currencies = new Set(entries.flatMap(entry => manifest.prices.filter(price => price.deployment === entry.deployment && price.operation === operation).map(price => price.currency)));
  if (currencies.size > 1) return `Every deployment of a route is priced in one currency; these are priced in ${[...currencies].sort().join(' and ')}.`;
  return null;
}

const sameEntries = (a: RouteEntry[], b: RouteEntry[]) => {
  const key = (entries: RouteEntry[]) => JSON.stringify([...entries].sort((x, y) => x.deployment.localeCompare(y.deployment)).map(entry => [entry.deployment, Number(entry.weight), Number(entry.priority)]));
  return key(a) === key(b);
};

/** Route `alias`'s `operation` to exactly `entries`, replacing `original`,
 * the deployments it routed to when the editor opened. Refused when the
 * active release routes it otherwise by now, so a save never undoes
 * another administrator's change it did not show. */
export function editRoute(alias: string, operation: CatalogOperation, original: RouteEntry[], entries: RouteEntry[], now: Date): CatalogEdit {
  return base => {
    if (!base) return { problem: 'No catalog release is active.' };
    if (!sameEntries(routeEntries(base, alias, operation), original)) {
      return { problem: `The routing of ${alias} changed since this dialog opened. Close it and open it again to see the change.` };
    }
    for (const entry of entries) {
      if (!routableDeployments(base, operation).includes(entry.deployment)) return { problem: `${entry.deployment} no longer serves this operation.` };
    }
    const problem = routeProblem(base, operation, entries);
    if (problem) return { problem };
    const routes = base.routes.filter(route => !(route.alias === alias && route.operation === operation));
    for (const entry of entries) {
      const route: Manifest['routes'][number] = { alias, operation, deployment: entry.deployment };
      const weight = Number(entry.weight);
      const priority = Number(entry.priority);
      if (weight !== 1) route.weight = weight;
      if (priority !== 1) route.priority = priority;
      routes.push(route);
    }
    return { ...base, routes, release: { ...base.release, version: releaseVersion(now, `routes ${alias}`) } };
  };
}
