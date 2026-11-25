import type { CatalogManifest, CatalogOperation, ManifestCapability, PriceObservation, ProviderFamily } from './api';
import { parseMoney } from './money';

// Add provider: a new release that is the active one plus a provider
// account, a deployment per chosen model, its prices and its aliases. The
// catalog checks the whole release again when it is created; these checks
// only say what to fix before sending it.

/** One model to add, as the form holds it: prices are decimal major units
 * per million tokens, as price lists publish them. */
export type ModelChoice = {
  model: string;
  alias: string;
  operations: CatalogOperation[];
  currency: string;
  input: string;
  output: string;
  cachedInput: string;
  /** The cached input price is the input price standing in for one the
   * price list does not publish, until one is entered. */
  cachedAssumed: boolean;
  /** The price source the prices came from; null when typed. */
  source: { name: string; item: string } | null;
  /** Whether its chat and chat batch take tool definitions (D-032). */
  tools: boolean;
  /** OpenAI's chat reasoning price, if the admin enters one: empty or 0
   * charges reasoning tokens at the output price. */
  reasoning: string;
  /** Anthropic's five-minute cache write price: follows 1.25 times the
   * input price until the admin enters one (null). `derived` reads it. */
  cacheWrite: string | null;
  /** Batch prices that follow half the chat price until the admin enters
   * one (null). `derived` reads them. */
  batchInput: string | null;
  batchOutput: string | null;
  batchCachedInput: string | null;
};

// Limits for a new deployment; the Releases tab shows them per release.
const CAPABILITY: Omit<ManifestCapability, 'operation'> = {
  max_input_bytes: 1_048_576,
  max_output_tokens: 8192,
  max_tokens_per_byte: 1,
  tokenizer: 'configured',
  reservation_ttl_seconds: 300,
  // Accounting reserves against the byte bound and settles on reported
  // usage, which every family supports.
  chat_input_count_mode: 'unsupported',
};

/** The operations a new deployment of the family may offer here. A chat
 * batch is offered where the gateway runs the family's own batch API at a
 * price of its own; elsewhere the Pricing tab can add one later. */
export function offeredOperations(family: ProviderFamily): CatalogOperation[] {
  if (family === 'anthropic') return ['chat', 'chat_batch'];
  if (family === 'openai') return ['chat', 'embeddings', 'chat_batch'];
  return ['chat', 'embeddings'];
}

/** Whether a new deployment takes tools unless the admin says otherwise:
 * Anthropic's and OpenAI's APIs take them natively, for every model that
 * chats. Elsewhere tools depend on the model, so the admin opts in. */
export function toolsByDefault(family: ProviderFamily): boolean {
  return family === 'anthropic' || family === 'openai';
}

/** The operations whose capability may take tools. */
export function takesTools(operation: CatalogOperation): boolean {
  return operation === 'chat' || operation === 'chat_batch';
}

/** Whether the family's own price is entered here: these publish no price
 * list the gateway can read. */
export function publishesNoPriceList(family: ProviderFamily): boolean {
  return family === 'anthropic' || family === 'openai';
}

/** Whether the family's cache writes are priced apart: Anthropic reports
 * them (`cache_creation_input_tokens`), charged at the catalog's `other`
 * price, which Anthropic sets at 1.25 times input for a five-minute write. */
export function pricesCacheWrite(family: ProviderFamily): boolean {
  return family === 'anthropic';
}

/** Whether a reasoning price may be entered for the family. A price of
 * zero charges reasoning tokens at the output price (`reasoning_rate`),
 * which is how OpenAI bills them, and prices a response that reports no
 * reasoning count; a price of its own needs that count, else the request
 * cannot be settled. Anthropic and Bedrock report none, and must be zero. */
export function pricesReasoning(family: ProviderFamily): boolean {
  return family === 'openai';
}

/** Half a decimal price, exactly: "2.5" is "1.25". Null if not a price. */
export function half(value: string): string | null {
  return scaled(value, 5n, 1);
}

/** 1.25 times a decimal price, exactly: "3" is "3.75". Null if not a price. */
export function withQuarter(value: string): string | null {
  return scaled(value, 125n, 2);
}

/** `value` times `factor` / 10^`places`, exactly. */
function scaled(value: string, factor: bigint, places: number): string | null {
  const match = /^\s*(\d+)(?:\.(\d*))?\s*$/.exec(value);
  if (!match) return null;
  const fraction = match[2] ?? '';
  const digits = (BigInt(match[1] + fraction) * factor).toString().padStart(fraction.length + places + 1, '0');
  const point = digits.length - fraction.length - places;
  const whole = digits.slice(0, point).replace(/^0+(?=\d)/, '');
  const rest = digits.slice(point).replace(/0+$/, '');
  return rest ? `${whole}.${rest}` : whole;
}

/** The prices that follow others, as they stand: what the admin entered,
 * else what they follow (blank while that is not a price). */
export function derived(choice: ModelChoice): { batchInput: string; batchOutput: string; batchCachedInput: string; cacheWrite: string; batchCacheWrite: string } {
  const cacheWrite = choice.cacheWrite ?? withQuarter(choice.input) ?? '';
  return {
    batchInput: choice.batchInput ?? half(choice.input) ?? '',
    batchOutput: choice.batchOutput ?? half(choice.output) ?? '',
    batchCachedInput: choice.batchCachedInput ?? half(choice.cachedInput) ?? '',
    cacheWrite,
    // Anthropic's batch discount applies to cache writes too.
    batchCacheWrite: half(cacheWrite) ?? '',
  };
}

/** Embedding models embed; everything else chats (streamed or not: one operation). */
export function defaultOperations(family: ProviderFamily, model: string): CatalogOperation[] {
  if (/embed/i.test(model) && family !== 'anthropic') return ['embeddings'];
  return ['chat'];
}

/** The deployment a model gets under `key`. Deployment names appear in
 * URL paths, so anything but letters, digits, `.`, `_`, `:` and `-` is a
 * dash. */
export function deploymentName(key: string, model: string): string {
  return `${key}-${model.replace(/[^A-Za-z0-9._:-]+/g, '-')}`.slice(0, 255);
}

/** A published price restated per million tokens, exactly. */
export function perMillion(value: string | null, perTokens: PriceObservation['per_tokens']): string | null {
  if (value === null || !/^\d+(\.\d+)?$/.test(value)) return null;
  const places = perTokens === 1 ? 6 : perTokens === 1000 ? 3 : 0;
  const [integer, fraction = ''] = value.split('.');
  const digits = integer + fraction.padEnd(places, '0');
  const point = integer.length + places;
  const whole = digits.slice(0, point).replace(/^0+(?=\d)/, '');
  const rest = digits.slice(point).replace(/0+$/, '');
  return rest ? `${whole}.${rest}` : whole;
}

/** A per-million price as the catalog's money units per token. A price
 * finer than one unit per token is rounded up, never down, and is not
 * `exact`. */
export function perTokenPrice(value: string, currency: string): { units: number; exact: boolean } | null {
  const units = parseMoney(value, currency);
  if (units === null) return null;
  const perToken = Math.ceil(units / 1_000_000);
  return Number.isSafeInteger(perToken) ? { units: perToken, exact: perToken * 1_000_000 === units } : null;
}

/** The form's values from a published price. A price list may publish
 * only some prices: a missing input or output price stays blank, to be
 * entered, and missing cached input is charged as input, an upper bound. */
export function fromObservation(choice: ModelChoice, observation: PriceObservation): ModelChoice {
  const input = perMillion(observation.input, observation.per_tokens) ?? '';
  return {
    ...choice,
    currency: observation.currency,
    input,
    output: perMillion(observation.output, observation.per_tokens) ?? '',
    cachedInput: perMillion(observation.cached_input, observation.per_tokens) ?? input,
    cachedAssumed: perMillion(observation.cached_input, observation.per_tokens) === null,
    source: { name: observation.source, item: observation.item },
  };
}

export function newChoice(family: ProviderFamily, model: string): ModelChoice {
  return {
    model,
    alias: model,
    operations: defaultOperations(family, model),
    currency: 'USD',
    input: '',
    output: '',
    // Anthropic's cache read price differs by model (0.1, 0.05 or 0.025
    // times input), so it is entered, not assumed.
    cachedInput: family === 'anthropic' ? '' : '0',
    cachedAssumed: false,
    source: null,
    tools: toolsByDefault(family),
    reasoning: '',
    cacheWrite: null,
    batchInput: null,
    batchOutput: null,
    batchCachedInput: null,
  };
}

/** What is wrong with one choice, if anything. */
export function choiceProblem(family: ProviderFamily, choice: ModelChoice): string | null {
  if (!choice.alias.trim() || choice.alias !== choice.alias.trim() || choice.alias.length > 128) return 'Give it an alias of 1 to 128 characters, without surrounding spaces.';
  if (choice.operations.length === 0) return 'Choose at least one operation.';
  if (!/^[A-Z]{3}$/.test(choice.currency)) return 'The currency is a three-letter code, like USD.';
  const embeddingsOnly = choice.operations.every(operation => operation === 'embeddings');
  const chats = choice.operations.includes('chat');
  const batch = choice.operations.includes('chat_batch');
  const follows = derived(choice);
  const prices: [string, string, boolean][] = [
    ['input', choice.input, true],
    ['output', choice.output, !embeddingsOnly],
    ['cached input', choice.cachedInput, true],
    ['cache write', follows.cacheWrite, !embeddingsOnly && pricesCacheWrite(family)],
    // Optional: empty is zero, charged at the output price.
    ['reasoning', choice.reasoning, chats && pricesReasoning(family) && choice.reasoning.trim() !== ''],
    ['batch input', follows.batchInput, batch],
    ['batch output', follows.batchOutput, batch],
    ['batch cached input', follows.batchCachedInput, batch],
    ['batch cache write', follows.batchCacheWrite, batch && pricesCacheWrite(family)],
  ];
  for (const [label, value, asked] of prices) {
    if (asked && perTokenPrice(value, choice.currency) === null) return `Enter the ${label} price per million tokens, like 2.50.`;
  }
  return null;
}

export type Addition = { key: string; family: ProviderFamily; location: string | null; choices: ModelChoice[] };

/** The base release plus the addition, or why it cannot be added. */
export function addProvider(base: CatalogManifest | null, addition: Addition, now: Date): { manifest: CatalogManifest } | { problem: string } {
  const { key, family, location, choices } = addition;
  if (choices.length === 0) return { problem: 'Choose at least one model.' };
  const manifest: CatalogManifest = base
    ? structuredClone(base)
    : { release: { version: '' }, providers: [], deployments: [], prices: [], routes: [] };
  if (manifest.providers.some(provider => provider.adapter_instance_key === key)) {
    return { problem: `${key} is already in the catalog.` };
  }
  manifest.release.version = `${now.toISOString().slice(0, 16)}Z + ${key}`.slice(0, 128);
  manifest.providers.push({ adapter_instance_key: key, family });
  const routes = new Set(manifest.routes.map(route => `${route.alias}\u0000${route.operation}`));
  const date = now.toISOString().slice(0, 10);
  for (const choice of choices) {
    const problem = choiceProblem(family, choice);
    if (problem) return { problem: `${choice.model}: ${problem}` };
    const name = deploymentName(key, choice.model);
    if (manifest.deployments.some(deployment => deployment.name === name)) {
      return { problem: `${choice.model} is chosen twice.` };
    }
    manifest.deployments.push({
      name,
      model: choice.model,
      location,
      adapter_instance_key: key,
      capabilities: choice.operations.map(operation => ({ operation, ...CAPABILITY, ...(choice.tools && takesTools(operation) ? { tools: true } : {}) })),
    });
    const zero = { units: 0, exact: true };
    const price = (value: string) => perTokenPrice(value, choice.currency) ?? zero;
    const follows = derived(choice);
    for (const operation of choice.operations) {
      const batch = operation === 'chat_batch';
      const input = price(batch ? follows.batchInput : choice.input);
      const output = operation === 'embeddings' ? zero : price(batch ? follows.batchOutput : choice.output);
      // A reasoning price of zero charges reasoning at the output price;
      // only chat takes one the admin entered. Anthropic's cache writes
      // are the `other` tokens.
      const cached = price(batch ? follows.batchCachedInput : choice.cachedInput);
      const cacheWrite = operation !== 'embeddings' && pricesCacheWrite(family) ? price(batch ? follows.batchCacheWrite : follows.cacheWrite) : zero;
      const reasoning = operation === 'chat' && pricesReasoning(family) && choice.reasoning.trim() ? price(choice.reasoning) : zero;
      const alias = choice.alias;
      if (routes.has(`${alias}\u0000${operation}`)) {
        return { problem: `The alias ${alias} already routes ${operation}; choose another.` };
      }
      routes.add(`${alias}\u0000${operation}`);
      manifest.prices.push({
        deployment: name,
        operation,
        input_minor_per_token: input.units,
        output_minor_per_token: output.units,
        cached_input_minor_per_token: cached.units,
        reasoning_minor_per_token: reasoning.units,
        other_minor_per_token: cacheWrite.units,
        currency: choice.currency,
        source: choice.source ? choice.source.name.slice(0, 128) : 'admin',
        source_version: choice.source ? choice.source.item.slice(0, 128) : date,
        // Rounded up to whole units per token, or charging cached input as
        // input, a price is an upper bound.
        confidence: input.exact && output.exact && cached.exact && reasoning.exact && cacheWrite.exact && !choice.cachedAssumed ? 'known' : 'conservative',
      });
      manifest.routes.push({ alias, operation, deployment: name });
    }
  }
  return { manifest };
}
