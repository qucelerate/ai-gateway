import { describe, expect, it } from 'vitest';
import type { CatalogManifest, PriceDrift } from './api';
import { differences } from './catalog-actions';
import { foldStreaming, releaseVersion } from './catalog-change';
import {
  applyPublishedPrices,
  brokenRoutes,
  editDeployment,
  observedUnits,
  operationForms,
  operationProblem,
  removeDeployment,
  removeProvider,
  takesToolsFlag,
  editRoute,
  routableDeployments,
  routeEntries,
  routeProblem,
} from './catalog-edit';

const now = new Date('2026-10-12T19:30:00Z');
const capability = (operation: 'chat' | 'chat_stream' | 'embeddings') => ({
  operation, max_input_bytes: 4096, max_output_tokens: 32, max_tokens_per_byte: 1, tokenizer: 'configured' as const,
  reservation_ttl_seconds: 17, chat_input_count_mode: 'unsupported' as const,
});
const price = (deployment: string, operation: 'chat' | 'chat_stream' | 'embeddings', input: number, extra: object = {}) => ({
  deployment, operation, input_minor_per_token: input, output_minor_per_token: 1000, cached_input_minor_per_token: 0,
  reasoning_minor_per_token: 0, other_minor_per_token: 0, currency: 'USD', source: 'test', source_version: 'v1',
  confidence: 'known' as const, ...extra,
});

function manifest(): CatalogManifest {
  return {
    release: { version: 'v1' },
    providers: [
      { adapter_instance_key: 'openai-main', family: 'openai' },
      { adapter_instance_key: 'anthropic-main', family: 'anthropic' },
    ],
    deployments: [
      { name: 'gpt', model: 'gpt-4o', location: null, adapter_instance_key: 'openai-main', capabilities: [capability('chat'), capability('chat_stream')] },
      { name: 'embed', model: 'text-embedding-3-small', location: null, adapter_instance_key: 'openai-main', capabilities: [capability('embeddings')] },
      { name: 'claude', model: 'claude-sonnet', location: null, adapter_instance_key: 'anthropic-main', capabilities: [capability('chat')] },
    ],
    prices: [
      price('gpt', 'chat', 250, { cached_input_minor_per_token: 125 }),
      price('gpt', 'chat_stream', 250, { cached_input_minor_per_token: 125 }),
      price('embed', 'embeddings', 2, { output_minor_per_token: 0 }),
      price('claude', 'chat', 300),
    ],
    routes: [
      { alias: 'fast', operation: 'chat', deployment: 'gpt' },
      { alias: 'fast', operation: 'chat_stream', deployment: 'gpt' },
      { alias: 'smart', operation: 'chat', deployment: 'claude' },
      { alias: 'smart', operation: 'chat_stream', deployment: 'gpt' },
      { alias: 'embed', operation: 'embeddings', deployment: 'embed' },
    ],
  };
}

function applied(edit: (base: CatalogManifest | null) => CatalogManifest | { problem: string }): CatalogManifest {
  const result = edit(manifest());
  if ('problem' in result) throw new Error(result.problem);
  return result;
}

describe('catalog edits', () => {
  it('names a release by when and what', () => {
    expect(releaseVersion(now, '~ gpt')).toBe('2026-10-12T19:30Z ~ gpt');
    expect(releaseVersion(now, 'x'.repeat(200))).toHaveLength(128);
  });

  it('removes a deployment with its prices and routes, and says which aliases stop working', () => {
    const base = manifest();
    // Every route to a removed deployment breaks, per operation: `smart`
    // keeps chat (claude) but loses streaming.
    expect(brokenRoutes(base, new Set(['gpt']))).toEqual([
      { alias: 'fast', operations: ['chat', 'chat_stream'] },
      { alias: 'smart', operations: ['chat_stream'] },
    ]);
    expect(brokenRoutes(base, new Set(['claude']))).toEqual([{ alias: 'smart', operations: ['chat'] }]);
    const result = applied(removeDeployment('gpt', now));
    expect(result.release.version).toBe('2026-10-12T19:30Z - gpt');
    expect(result.deployments.map(item => item.name)).toEqual(['embed', 'claude']);
    expect(result.prices.map(item => item.deployment)).toEqual(['embed', 'claude']);
    expect(result.routes).toEqual([
      { alias: 'smart', operation: 'chat', deployment: 'claude' },
      { alias: 'embed', operation: 'embeddings', deployment: 'embed' },
    ]);
    expect(result.providers).toHaveLength(2);
    expect(removeDeployment('gone', now)(manifest())).toEqual({ problem: 'gone is no longer in the catalog.' });
  });

  it('removes a provider account with every deployment it serves', () => {
    const result = applied(removeProvider('openai-main', now));
    expect(result.providers).toEqual([{ adapter_instance_key: 'anthropic-main', family: 'anthropic' }]);
    expect(result.deployments.map(item => item.name)).toEqual(['claude']);
    expect(result.routes).toEqual([{ alias: 'smart', operation: 'chat', deployment: 'claude' }]);
    expect(removeProvider('nope', now)(manifest())).toEqual({ problem: 'nope is no longer in the catalog.' });
  });

  it('fills the edit form from the release, per million tokens', () => {
    expect(operationForms(manifest(), 'gpt')[0]).toEqual({
      operation: 'chat',
      currency: 'USD',
      limits: { maxInputBytes: '4096', maxOutputTokens: '32', reservationTtlSeconds: '17' },
      price: { input: '2.5', output: '10', cachedInput: '1.25' },
      tools: false,
    });
    expect(operationForms(manifest(), 'gone')).toEqual([]);
  });

  it('turns tool calling on and off for chat, leaving the flag out when off', () => {
    const original = operationForms(manifest(), 'gpt');
    const forms = structuredClone(original);
    forms[0].tools = true;
    const on = applied(editDeployment('gpt', original, forms, now));
    expect(on.deployments[0].capabilities[0]).toEqual({ ...capability('chat'), tools: true });
    expect(on.deployments[0].capabilities[1]).toEqual(capability('chat_stream'));
    expect(operationForms(on, 'gpt')[0].tools).toBe(true);
    const back = structuredClone(operationForms(on, 'gpt'));
    back[0].tools = false;
    const off = applied(editDeployment('gpt', operationForms(on, 'gpt'), back, now));
    expect(off.deployments[0].capabilities[0]).toEqual(capability('chat'));
    expect('tools' in off.deployments[0].capabilities[0]).toBe(false);
  });

  it('turns tool calling on for chat batches, which have their own flag', () => {
    const base = manifest();
    base.deployments[0].capabilities.push({ ...capability('chat'), operation: 'chat_batch' });
    base.prices.push({ ...price('gpt', 'chat', 125), operation: 'chat_batch' });
    const original = operationForms(base, 'gpt');
    expect(original.map(form => [form.operation, form.tools])).toEqual([['chat', false], ['chat_stream', false], ['chat_batch', false]]);
    const forms = structuredClone(original);
    forms[2].tools = true;
    const on = editDeployment('gpt', original, forms, now)(base);
    if ('problem' in on) throw new Error(on.problem);
    expect(on.deployments[0].capabilities.map(item => [item.operation, item.tools === true])).toEqual([['chat', false], ['chat_stream', false], ['chat_batch', true]]);
    expect(takesToolsFlag('chat_batch')).toBe(true);
    expect(takesToolsFlag('embeddings_batch')).toBe(false);
  });

  it('checks limits and prices', () => {
    const [form] = operationForms(manifest(), 'gpt');
    expect(operationProblem(form)).toBeNull();
    expect(operationProblem({ ...form, limits: { ...form.limits, maxInputBytes: '0' } })).toMatch(/Maximum input/);
    expect(operationProblem({ ...form, limits: { ...form.limits, maxOutputTokens: '1.5' } })).toMatch(/Maximum output/);
    expect(operationProblem({ ...form, limits: { ...form.limits, reservationTtlSeconds: '86401' } })).toMatch(/1 to 86400/);
    expect(operationProblem({ ...form, price: { ...form.price, cachedInput: '' } })).toMatch(/cached input/);
    // Embeddings have no output.
    const [embed] = operationForms(manifest(), 'embed');
    expect(operationProblem({ ...embed, price: { ...embed.price, output: '' } })).toBeNull();
  });

  it('edits limits, and re-prices only what changed', () => {
    const original = operationForms(manifest(), 'gpt');
    const forms = structuredClone(original);
    forms[0] = { ...forms[0], limits: { ...forms[0].limits, maxInputBytes: '8192', maxOutputTokens: '64' }, price: { ...forms[0].price, input: '3' } };
    const result = applied(editDeployment('gpt', original, forms, now));
    expect(result.release.version).toBe('2026-10-12T19:30Z ~ gpt');
    expect(result.deployments[0].capabilities[0]).toEqual({ ...capability('chat'), max_input_bytes: 8192, max_output_tokens: 64 });
    expect(result.deployments[0].capabilities[1]).toEqual(capability('chat_stream'));
    expect(result.prices[0]).toEqual(price('gpt', 'chat', 300, { cached_input_minor_per_token: 125, source: 'admin', source_version: '2026-10-12' }));
    // Unchanged: keeps its source.
    expect(result.prices[1]).toEqual(manifest().prices[1]);
    // Finer than a unit: rounded up, conservative.
    forms[0] = { ...forms[0], price: { ...forms[0].price, input: '0.075' } };
    expect(applied(editDeployment('gpt', original, forms, now)).prices[0]).toMatchObject({ input_minor_per_token: 8, confidence: 'conservative' });
    expect(editDeployment('gone', original, forms, now)(manifest())).toEqual({ problem: 'gone is no longer in the catalog.' });
    expect(editDeployment('gpt', original, [{ ...forms[0], limits: { ...forms[0].limits, maxInputBytes: 'x' } }], now)(manifest())).toEqual({ problem: 'chat: Maximum input is a whole number of bytes above 0.' });
    expect(editDeployment('gpt', original, original, now)(manifest())).toEqual({ problem: 'Nothing was changed.' });
  });

  it('keeps what someone else changed meanwhile', () => {
    const original = operationForms(manifest(), 'gpt');
    const forms = structuredClone(original);
    forms[0].limits.reservationTtlSeconds = '60';
    // Meanwhile another admin re-priced chat output and raised its input limit.
    const meanwhile = manifest();
    meanwhile.prices[0] = { ...meanwhile.prices[0], output_minor_per_token: 1500, source: 'other' };
    meanwhile.deployments[0].capabilities[0].max_input_bytes = 65536;
    const result = editDeployment('gpt', original, forms, now)(meanwhile);
    if ('problem' in result) throw new Error(result.problem);
    expect(result.prices[0]).toEqual(meanwhile.prices[0]);
    expect(result.deployments[0].capabilities[0]).toMatchObject({ max_input_bytes: 65536, reservation_ttl_seconds: 60 });
    // Re-pricing input keeps their output price.
    forms[0].price.input = '3';
    const repriced = editDeployment('gpt', original, forms, now)(structuredClone(meanwhile));
    if ('problem' in repriced) throw new Error(repriced.problem);
    expect(repriced.prices[0]).toMatchObject({ input_minor_per_token: 300, output_minor_per_token: 1500, source: 'admin' });
  });

  it('refuses to rebuild a price removed meanwhile', () => {
    const original = operationForms(manifest(), 'gpt');
    const forms = structuredClone(original);
    forms[0].price.input = '3';
    const meanwhile = manifest();
    meanwhile.prices = meanwhile.prices.filter(item => !(item.deployment === 'gpt' && item.operation === 'chat'));
    meanwhile.routes = meanwhile.routes.filter(item => !(item.deployment === 'gpt' && item.operation === 'chat'));
    expect(editDeployment('gpt', original, forms, now)(meanwhile)).toEqual({ problem: 'gpt chat is no longer priced; close and edit it again.' });
  });

  it('prices an unpriced operation from every field of the form', () => {
    const base = manifest();
    base.prices = base.prices.filter(item => !(item.deployment === 'gpt' && item.operation === 'chat'));
    base.routes = base.routes.filter(item => !(item.deployment === 'gpt' && item.operation === 'chat'));
    const original = operationForms(base, 'gpt');
    expect(original[0].price).toEqual({ input: '', output: '', cachedInput: '0' });
    const forms = structuredClone(original);
    forms[0].price = { input: '3', output: '12', cachedInput: '0' };
    const result = editDeployment('gpt', original, forms, now)(structuredClone(base));
    if ('problem' in result) throw new Error(result.problem);
    expect(result.prices.find(item => item.deployment === 'gpt' && item.operation === 'chat')).toMatchObject({ input_minor_per_token: 300, output_minor_per_token: 1200, cached_input_minor_per_token: 0, source: 'admin', confidence: 'known' });
  });

  it('refuses a price whose currency changed meanwhile', () => {
    const original = operationForms(manifest(), 'gpt');
    const forms = structuredClone(original);
    forms[0].price.input = '3';
    const meanwhile = manifest();
    meanwhile.prices[0] = { ...meanwhile.prices[0], currency: 'JPY' };
    expect(editDeployment('gpt', original, forms, now)(meanwhile)).toEqual({ problem: 'gpt is now billed in JPY, not USD; close and edit it again.' });
  });

  it('rounds published prices up to whole units', () => {
    expect(observedUnits('250')).toEqual({ units: 250, exact: true });
    expect(observedUnits('7.5')).toEqual({ units: 8, exact: false });
    expect(observedUnits('3.000')).toEqual({ units: 3, exact: true });
    expect(observedUnits('-1')).toBeNull();
  });

  it('adopts published prices, changing only what was published', () => {
    const result = applied(applyPublishedPrices([
      { deployment: 'gpt', operation: 'chat', currency: 'USD', source: 'price-sheet:openai', item: 'gpt-4o', observed: { input: '200', output: '800' } },
      { deployment: 'claude', operation: 'chat', currency: 'USD', source: 'price-sheet:anthropic', item: null, observed: { input: '2.5' } },
    ], now));
    expect(result.release.version).toBe('2026-10-12T19:30Z 2 prices');
    expect(result.prices[0]).toMatchObject({ input_minor_per_token: 200, output_minor_per_token: 800, cached_input_minor_per_token: 125, source: 'price-sheet:openai', source_version: 'gpt-4o', confidence: 'known' });
    expect(result.prices[3]).toMatchObject({ input_minor_per_token: 3, output_minor_per_token: 1000, source_version: '2026-10-12', confidence: 'conservative' });
    // Untouched prices stay as they were.
    expect(result.prices[1]).toEqual(manifest().prices[1]);
    expect(applyPublishedPrices([{ deployment: 'gpt', operation: 'chat', currency: 'EUR', source: 's', item: null, observed: { input: '1' } }], now)(manifest()))
      .toEqual({ problem: 'gpt is billed in USD, not EUR.' });
    expect(applyPublishedPrices([], now)(manifest())).toEqual({ problem: 'Choose at least one price.' });
  });

  it('offers the release prices that differ, not overridden ones', () => {
    const comparison = (status: string, dimensions: object[]) => ({ source: 'price-sheet:openai', snapshot_id: 's', status, matched_by: 'model', observed_item: 'gpt-4o', observed_region: null, observed_currencies: ['USD'], per_tokens: 1000000, dimensions });
    const entry = (deployment: string, price_source: string, comparisons: object[]) => ({ deployment, operation: 'chat', model: deployment, location: null, provider_family: 'openai', currency: 'USD', price_source, comparisons });
    const drift = {
      release_id: 'r',
      sources: [],
      prices: [
        entry('gpt', 'release', [comparison('differs', [
          { dimension: 'input', catalog: 250, observed: '200', published: '2', status: 'differs' },
          { dimension: 'output', catalog: 1000, observed: '1000', published: '10', status: 'matches' },
          { dimension: 'reasoning', catalog: 0, observed: null, published: null, status: 'unpublished' },
        ])]),
        entry('same', 'release', [comparison('matches', [])]),
        entry('over', 'override', [comparison('differs', [])]),
      ],
    } as unknown as PriceDrift;
    const { adoptable, overridden } = differences(drift);
    expect(overridden).toBe(1);
    expect(adoptable).toHaveLength(1);
    expect(adoptable[0]).toMatchObject({ deployment: 'gpt', observed: { input: '200' }, source: 'price-sheet:openai', item: 'gpt-4o' });
    expect(adoptable[0].changes).toEqual([{ dimension: 'input', catalog: 250, observed: '200' }]);
  });

  it('adopts a stored chat_stream price as chat, and the chat one for both', () => {
    const comparison = { source: 'price-sheet:openai', snapshot_id: 's', status: 'differs', matched_by: 'model', observed_item: 'gpt-4o', observed_region: null, observed_currencies: ['USD'], per_tokens: 1000000, dimensions: [{ dimension: 'input', catalog: 250, observed: '200', published: '2', status: 'differs' }] };
    const entry = (deployment: string, operation: string) => ({ deployment, operation, model: deployment, location: null, provider_family: 'openai', currency: 'USD', price_source: 'release', comparisons: [comparison] });
    const drift = { release_id: 'r', sources: [], prices: [entry('gpt', 'chat'), entry('gpt', 'chat_stream'), entry('lone', 'chat_stream')] } as unknown as PriceDrift;
    const { adoptable } = differences(drift);
    expect(adoptable.map(item => [item.deployment, item.operation])).toEqual([['gpt', 'chat'], ['lone', 'chat']]);
    // Applied to the folded release, where streaming is chat, it lands.
    const folded = foldStreaming(manifest());
    const result = applyPublishedPrices(adoptable.slice(0, 1), now)(structuredClone(folded));
    expect('problem' in result).toBe(false);
  });
});

describe('routes over several deployments', () => {
  it('routes an alias to weighted, prioritized deployments, leaving defaults out', () => {
    const result = editRoute('fast', 'chat', routeEntries(foldStreaming(manifest()), 'fast', 'chat'), [
      { deployment: 'gpt', weight: '3', priority: '1' },
      { deployment: 'claude', weight: '1', priority: '2' },
    ], now)(foldStreaming(manifest()));
    if ('problem' in result) throw new Error(result.problem);
    expect(result.routes.filter(route => route.alias === 'fast' && route.operation === 'chat')).toEqual([
      { alias: 'fast', operation: 'chat', deployment: 'gpt', weight: 3 },
      { alias: 'fast', operation: 'chat', deployment: 'claude', priority: 2 },
    ]);
    expect(routeEntries(result, 'fast', 'chat')).toEqual([
      { deployment: 'gpt', weight: '3', priority: '1' },
      { deployment: 'claude', weight: '1', priority: '2' },
    ]);
    // Other aliases and operations keep their routes.
    expect(result.routes.filter(route => route.alias === 'smart')).toEqual(foldStreaming(manifest()).routes.filter(route => route.alias === 'smart'));
  });

  it('offers only deployments priced for the operation and refuses what cannot be one route', () => {
    const base = foldStreaming(manifest());
    expect(routableDeployments(base, 'chat')).toEqual(['claude', 'gpt']);
    expect(routableDeployments(base, 'embeddings')).toEqual(['embed']);
    expect(routeProblem(base, 'chat', [])).toMatch(/at least one/);
    expect(routeProblem(base, 'chat', [{ deployment: 'gpt', weight: '0', priority: '1' }])).toMatch(/weight/);
    expect(routeProblem(base, 'chat', [{ deployment: 'gpt', weight: '1', priority: '11' }])).toMatch(/priority/);
    expect(routeProblem(base, 'chat', [{ deployment: 'gpt', weight: '1', priority: '1' }, { deployment: 'gpt', weight: '2', priority: '1' }])).toMatch(/once/);
    const euro = { ...base, prices: base.prices.map(item => (item.deployment === 'claude' ? { ...item, currency: 'EUR' } : item)) };
    expect(routeProblem(euro, 'chat', [{ deployment: 'gpt', weight: '1', priority: '1' }, { deployment: 'claude', weight: '1', priority: '1' }])).toMatch(/one currency/);
    const gone = editRoute('fast', 'embeddings', [], [{ deployment: 'gpt', weight: '1', priority: '1' }], now)(base);
    expect(gone).toEqual({ problem: 'gpt no longer serves this operation.' });
  });

  it('refuses a save over routing changed since the editor opened', () => {
    const base = foldStreaming(manifest());
    const opened = routeEntries(base, 'fast', 'chat');
    // Another administrator adds claude meanwhile.
    const changed = editRoute('fast', 'chat', opened, [...opened, { deployment: 'claude', weight: '1', priority: '2' }], now)(base);
    if ('problem' in changed) throw new Error(changed.problem);
    const stale = editRoute('fast', 'chat', opened, opened.map(entry => ({ ...entry, weight: '5' })), now)(changed);
    expect(stale).toEqual({ problem: 'The routing of fast changed since this dialog opened. Close it and open it again to see the change.' });
  });
});
