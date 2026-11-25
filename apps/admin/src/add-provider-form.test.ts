import { describe, expect, it } from 'vitest';
import type { CatalogManifest, PriceObservation } from './api';
import {
  addProvider,
  derived,
  half,
  withQuarter,
  offeredOperations,
  choiceProblem,
  defaultOperations,
  deploymentName,
  fromObservation,
  newChoice,
  perMillion,
  perTokenPrice,
  type ModelChoice,
} from './add-provider-form';

const now = new Date('2026-10-12T18:04:05Z');

function priced(model: string, values: Partial<ModelChoice> = {}): ModelChoice {
  return { ...newChoice('bedrock', model), input: '3', output: '15', ...values };
}

const base: CatalogManifest = {
  release: { version: 'v1' },
  providers: [{ adapter_instance_key: 'openai-main', family: 'openai' }],
  deployments: [{
    name: 'gpt',
    model: 'gpt-4o',
    location: null,
    adapter_instance_key: 'openai-main',
    capabilities: [{ operation: 'chat', max_input_bytes: 4096, max_output_tokens: 32, max_tokens_per_byte: 1, tokenizer: 'configured', reservation_ttl_seconds: 17, chat_input_count_mode: 'unsupported' }],
  }],
  prices: [{
    deployment: 'gpt', operation: 'chat', input_minor_per_token: 250, output_minor_per_token: 1000,
    cached_input_minor_per_token: 125, reasoning_minor_per_token: 0, other_minor_per_token: 0,
    currency: 'USD', source: 'test', source_version: 'v1', confidence: 'known',
  }],
  routes: [{ alias: 'smart', operation: 'chat', deployment: 'gpt' }],
};

describe('add provider form', () => {
  it('converts per-million prices to money units per token, rounding up', () => {
    // $2.50 per million: 250 money units (10⁻⁶ cent) per token.
    expect(perTokenPrice('2.50', 'USD')).toEqual({ units: 250, exact: true });
    expect(perTokenPrice('0', 'USD')).toEqual({ units: 0, exact: true });
    // $0.075 per million is 7.5 units: charged as 8, an upper bound.
    expect(perTokenPrice('0.075', 'USD')).toEqual({ units: 8, exact: false });
    expect(perTokenPrice('1.5', 'JPY')).toEqual({ units: 2, exact: false });
    expect(perTokenPrice('', 'USD')).toBeNull();
    expect(perTokenPrice('-1', 'USD')).toBeNull();
    expect(perTokenPrice('1e3', 'USD')).toBeNull();
  });

  it('restates published prices per million tokens exactly', () => {
    expect(perMillion('0.003', 1000)).toBe('3');
    expect(perMillion('0.00000025', 1)).toBe('0.25');
    expect(perMillion('2.5', 1000000)).toBe('2.5');
    expect(perMillion('0.0000375', 1000)).toBe('0.0375');
    expect(perMillion(null, 1000)).toBeNull();
    expect(perMillion('1e-3', 1000)).toBeNull();
  });

  it('fills a choice from a published price', () => {
    const observation: PriceObservation = {
      source: 'aws-price-list:bedrock', item: 'Claude 3 Haiku', model: null, region: 'us-east-2',
      currency: 'USD', per_tokens: 1000, input: '0.00025', output: '0.00125', cached_input: null,
    };
    expect(fromObservation(newChoice('bedrock', 'anthropic.claude-3-haiku'), observation)).toMatchObject({
      input: '0.25', output: '1.25', cachedInput: '0.25', currency: 'USD',
      source: { name: 'aws-price-list:bedrock', item: 'Claude 3 Haiku' },
    });
  });

  it('leaves unpublished prices to be entered, never free', () => {
    const partial: PriceObservation = {
      source: 'azure-retail:azure-openai', item: 'gpt-4o Inp regnl', model: null, region: 'eastus',
      currency: 'USD', per_tokens: 1000, input: '0.0025', output: null, cached_input: null,
    };
    const choice = fromObservation(newChoice('azure-openai', 'gpt-4o'), partial);
    expect(choice).toMatchObject({ input: '2.5', output: '' });
    // Cached input unpublished: charged as input, an upper bound.
    expect(choice.cachedInput).toBe('2.5');
    expect(choiceProblem('azure-openai', choice)).toMatch(/output price/);
  });

  it('marks a price with assumed cached input conservative', () => {
    const observation: PriceObservation = {
      source: 'aws-price-list:bedrock', item: 'Nova Micro', model: null, region: 'us-east-2',
      currency: 'USD', per_tokens: 1000, input: '0.00025', output: '0.00125', cached_input: null,
    };
    const add = (choice: ModelChoice) => {
      const result = addProvider(null, { key: 'bedrock-1', family: 'bedrock', location: null, choices: [choice] }, now);
      if (!('manifest' in result)) throw new Error(result.problem);
      return result.manifest.prices[0];
    };
    const assumed = fromObservation(newChoice('bedrock', 'amazon.nova-micro'), observation);
    expect(assumed.cachedAssumed).toBe(true);
    expect(add(assumed)).toMatchObject({ cached_input_minor_per_token: 25, confidence: 'conservative' });
    // Entered, or published, it is known.
    expect(add({ ...assumed, cachedInput: '0.03', cachedAssumed: false })).toMatchObject({ cached_input_minor_per_token: 3, confidence: 'known' });
    expect(fromObservation(newChoice('bedrock', 'm'), { ...observation, cached_input: '0.000025' }).cachedAssumed).toBe(false);
  });

  it('suggests operations and names a deployment per account and model', () => {
    expect(defaultOperations('azure-openai', 'text-embedding-3-small')).toEqual(['embeddings']);
    expect(defaultOperations('bedrock', 'amazon.titan-embed-text-v2:0')).toEqual(['embeddings']);
    expect(defaultOperations('anthropic', 'embed-like-name')).toEqual(['chat']);
    expect(defaultOperations('gemini', 'gemini-2.5-flash')).toEqual(['chat']);
    expect(deploymentName('bedrock-1234', 'anthropic.claude-3-haiku-20240307-v1:0')).toBe('bedrock-1234-anthropic.claude-3-haiku-20240307-v1:0');
    expect(deploymentName('gemini', 'publishers/google/models/x')).toBe('gemini-publishers-google-models-x');
  });

  it('says what a choice is missing', () => {
    expect(choiceProblem('bedrock', priced('m'))).toBeNull();
    expect(choiceProblem('bedrock', priced('m', { alias: ' m' }))).toMatch(/alias/);
    expect(choiceProblem('bedrock', priced('m', { operations: [] }))).toMatch(/operation/);
    expect(choiceProblem('bedrock', priced('m', { input: '' }))).toMatch(/input price/);
    expect(choiceProblem('bedrock', priced('m', { currency: 'usd' }))).toMatch(/currency/);
    // Anthropic's cache read and write prices are asked for too.
    expect(choiceProblem('anthropic', priced('m', { cachedInput: '' }))).toMatch(/cached input/);
    expect(choiceProblem('anthropic', priced('m', { cacheWrite: 'x' }))).toMatch(/cache write/);
    expect(choiceProblem('bedrock', priced('m', { cacheWrite: 'x' }))).toBeNull();
    expect(choiceProblem('gemini', priced('m', { cachedInput: '' }))).toMatch(/cached input/);
    // Embeddings have no output price.
    expect(choiceProblem('gemini', priced('m', { operations: ['embeddings'], output: '' }))).toBeNull();
  });

  it('adds the account, deployments, prices and aliases to the active release', () => {
    const result = addProvider(base, {
      key: 'bedrock-1234',
      family: 'bedrock',
      location: 'us-east-2',
      choices: [priced('anthropic.claude-3-haiku', { alias: 'haiku', input: '0.25', output: '1.25' }), priced('meta.llama3', { alias: 'llama', input: '0.075' })],
    }, now);
    if (!('manifest' in result)) throw new Error(result.problem);
    const { manifest } = result;
    expect(manifest.release.version).toBe('2026-10-12T18:04Z + bedrock-1234');
    // The base is kept, unchanged and first.
    expect(manifest.providers).toEqual([...base.providers, { adapter_instance_key: 'bedrock-1234', family: 'bedrock' }]);
    expect(manifest.deployments[0]).toEqual(base.deployments[0]);
    expect(manifest.routes.slice(0, 1)).toEqual(base.routes);
    expect(base.providers).toHaveLength(1);
    const haiku = manifest.deployments.find(deployment => deployment.model === 'anthropic.claude-3-haiku')!;
    expect(haiku).toMatchObject({ name: 'bedrock-1234-anthropic.claude-3-haiku', location: 'us-east-2', adapter_instance_key: 'bedrock-1234' });
    expect(haiku.capabilities.map(capability => capability.operation)).toEqual(['chat']);
    expect(manifest.prices.filter(price => price.deployment === haiku.name)).toEqual([
      expect.objectContaining({ operation: 'chat', input_minor_per_token: 25, output_minor_per_token: 125, reasoning_minor_per_token: 0, source: 'admin', source_version: '2026-10-12', confidence: 'known' }),
    ]);
    // A rounded price is conservative.
    expect(manifest.prices.find(price => price.deployment === 'bedrock-1234-meta.llama3')).toMatchObject({ input_minor_per_token: 8, confidence: 'conservative' });
    expect(manifest.routes.slice(1)).toEqual([
      { alias: 'haiku', operation: 'chat', deployment: haiku.name },
      { alias: 'llama', operation: 'chat', deployment: 'bedrock-1234-meta.llama3' },
    ]);
  });

  it('starts a first catalog and keeps embeddings output free', () => {
    const result = addProvider(null, {
      key: 'azure-acme',
      family: 'azure-openai',
      location: null,
      choices: [{ ...newChoice('azure-openai', 'text-embedding-3-small'), input: '0.02', output: '' }],
    }, now);
    if (!('manifest' in result)) throw new Error(result.problem);
    expect(result.manifest.providers).toEqual([{ adapter_instance_key: 'azure-acme', family: 'azure-openai' }]);
    expect(result.manifest.prices).toEqual([expect.objectContaining({ operation: 'embeddings', input_minor_per_token: 2, output_minor_per_token: 0 })]);
  });

  it('refuses what the catalog would', () => {
    const add = (choices: ModelChoice[], key = 'bedrock-1234') => addProvider(base, { key, family: 'bedrock', location: null, choices }, now);
    expect(add([])).toEqual({ problem: 'Choose at least one model.' });
    expect(add([priced('m')], 'openai-main')).toEqual({ problem: 'openai-main is already in the catalog.' });
    expect(add([priced('m', { alias: 'smart' })])).toEqual({ problem: 'The alias smart already routes chat; choose another.' });
    expect(add([priced('m', { alias: 'same' }), priced('n', { alias: 'same' })])).toEqual({ problem: 'The alias same already routes chat; choose another.' });
    expect(add([priced('m', { output: 'x' })])).toEqual({ problem: 'm: Enter the output price per million tokens, like 2.50.' });
  });

  it('halves prices exactly', () => {
    expect(half('3')).toBe('1.5');
    expect(half('2.50')).toBe('1.25');
    expect(half('0.075')).toBe('0.0375');
    expect(half('0')).toBe('0');
    expect(half('10')).toBe('5');
    expect(half('')).toBeNull();
    expect(half('x')).toBeNull();
  });

  it('offers chat batches where the family runs them here', () => {
    expect(offeredOperations('anthropic')).toEqual(['chat', 'chat_batch']);
    expect(offeredOperations('openai')).toEqual(['chat', 'embeddings', 'chat_batch']);
    expect(offeredOperations('bedrock')).toEqual(['chat', 'embeddings']);
    expect(offeredOperations('openai-compatible')).toEqual(['chat', 'embeddings']);
  });

  it('leaves OpenAI reasoning at zero, charged at the output price, unless one is entered', () => {
    const choice = { ...newChoice('openai', 'gpt-5'), input: '1.25', output: '10', cachedInput: '0.125' };
    expect(choice.reasoning).toBe('');
    // Empty is zero; an entered price must be one.
    expect(choiceProblem('openai', choice)).toBeNull();
    expect(choiceProblem('openai', { ...choice, reasoning: 'lots' })).toBe('Enter the reasoning price per million tokens, like 2.50.');
    const add = (value: ModelChoice) => {
      const result = addProvider(base, { key: 'openai-2', family: 'openai', location: null, choices: [value] }, now);
      if (!('manifest' in result)) throw new Error(result.problem);
      return result.manifest.prices.slice(1);
    };
    expect(add({ ...choice, operations: ['chat', 'chat_batch'] })).toEqual([
      expect.objectContaining({ operation: 'chat', input_minor_per_token: 125, output_minor_per_token: 1000, cached_input_minor_per_token: 13, reasoning_minor_per_token: 0, other_minor_per_token: 0 }),
      // A batch at half the price; its reasoning charged at the batch output price.
      expect.objectContaining({ operation: 'chat_batch', input_minor_per_token: 63, output_minor_per_token: 500, cached_input_minor_per_token: 7, reasoning_minor_per_token: 0, other_minor_per_token: 0 }),
    ]);
    // An entered price is the chat one; the batch keeps zero.
    expect(add({ ...choice, reasoning: '12', operations: ['chat', 'chat_batch'] }).map(price => [price.operation, price.reasoning_minor_per_token])).toEqual([['chat', 1200], ['chat_batch', 0]]);
    expect(add({ ...choice, reasoning: '0' })[0].reasoning_minor_per_token).toBe(0);
    // Embeddings report no reasoning.
    expect(add({ ...newChoice('openai', 'text-embedding-3-small'), input: '0.02', reasoning: '5' })[0]).toMatchObject({ operation: 'embeddings', output_minor_per_token: 0, reasoning_minor_per_token: 0 });
  });

  it('prices Anthropic cache reads and writes but never reasoning', () => {
    // A reasoning price the form held is not charged; the cache write
    // follows 1.25 times input, and half that in a batch.
    const choice = { ...newChoice('anthropic', 'claude-sonnet-4-5'), input: '3', output: '15', cachedInput: '0.3', reasoning: '15', operations: ['chat', 'chat_batch'] as ModelChoice['operations'] };
    expect(choiceProblem('anthropic', { ...choice, reasoning: 'x' })).toBeNull();
    expect(derived(choice)).toMatchObject({ cacheWrite: '3.75', batchCacheWrite: '1.875' });
    const result = addProvider(base, { key: 'anthropic-main', family: 'anthropic', location: null, choices: [choice] }, now);
    if (!('manifest' in result)) throw new Error(result.problem);
    expect(result.manifest.prices.slice(1).map(price => [price.operation, price.input_minor_per_token, price.output_minor_per_token, price.cached_input_minor_per_token, price.reasoning_minor_per_token, price.other_minor_per_token])).toEqual([
      ['chat', 300, 1500, 30, 0, 375],
      ['chat_batch', 150, 750, 15, 0, 188],
    ]);
    // An entered cache write price is the admin's.
    const entered = addProvider(base, { key: 'anthropic-main', family: 'anthropic', location: null, choices: [{ ...choice, cacheWrite: '6' }] }, now);
    if (!('manifest' in entered)) throw new Error(entered.problem);
    expect(entered.manifest.prices.slice(1).map(price => price.other_minor_per_token)).toEqual([600, 300]);
    // 1.25 times, exactly.
    expect(withQuarter('0.1')).toBe('0.125');
    expect(withQuarter('x')).toBeNull();
  });

  it('asks for batch prices that follow half the chat price until entered', () => {
    const choice = { ...newChoice('anthropic', 'claude'), input: '3', output: '15', cachedInput: '0.3', operations: ['chat', 'chat_batch'] as ModelChoice['operations'] };
    expect(derived(choice)).toMatchObject({ batchInput: '1.5', batchOutput: '7.5' });
    expect(derived({ ...choice, batchOutput: '8' })).toMatchObject({ batchInput: '1.5', batchOutput: '8' });
    expect(choiceProblem('anthropic', { ...choice, batchInput: 'x' })).toBe('Enter the batch input price per million tokens, like 2.50.');
    expect(choiceProblem('anthropic', { ...choice, operations: ['chat'], batchInput: 'x' })).toBeNull();
  });

  it('takes tools on chat and chat batch by default for Anthropic and OpenAI only', () => {
    const capabilities = (family: 'anthropic' | 'openai' | 'bedrock' | 'azure-openai', values: Partial<ModelChoice> = {}) => {
      const choice = { ...newChoice(family, 'm'), input: '1', output: '2', cachedInput: '0.1', operations: ['chat', 'chat_batch'] as ModelChoice['operations'], ...values };
      const result = addProvider(base, { key: `${family}-x`, family, location: null, choices: [choice] }, now);
      if (!('manifest' in result)) throw new Error(result.problem);
      return result.manifest.deployments.at(-1)!.capabilities.map(capability => [capability.operation, capability.tools ?? false]);
    };
    expect(capabilities('anthropic')).toEqual([['chat', true], ['chat_batch', true]]);
    expect(capabilities('openai')).toEqual([['chat', true], ['chat_batch', true]]);
    expect(capabilities('anthropic', { tools: false })).toEqual([['chat', false], ['chat_batch', false]]);
    // Elsewhere tools depend on the model: off unless the admin opts in, and
    // the capability leaves the flag out, as before.
    expect(newChoice('bedrock', 'm').tools).toBe(false);
    expect(capabilities('azure-openai', { operations: ['chat'] })).toEqual([['chat', false]]);
    expect(capabilities('azure-openai', { operations: ['chat'], tools: true })).toEqual([['chat', true]]);
    const bedrock = addProvider(base, { key: 'bedrock-x', family: 'bedrock', location: null, choices: [{ ...newChoice('bedrock', 'm'), input: '1', output: '2' }] }, now);
    if (!('manifest' in bedrock)) throw new Error(bedrock.problem);
    expect('tools' in bedrock.manifest.deployments.at(-1)!.capabilities[0]).toBe(false);
    // Embeddings never take tools.
    const embeddings = addProvider(base, { key: 'openai-x', family: 'openai', location: null, choices: [{ ...newChoice('openai', 'text-embedding-3-small'), input: '0.02' }] }, now);
    if (!('manifest' in embeddings)) throw new Error(embeddings.problem);
    expect(embeddings.manifest.deployments.at(-1)!.capabilities).toEqual([expect.not.objectContaining({ tools: true })]);
  });
});
