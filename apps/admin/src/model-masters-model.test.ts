import { describe, expect, it } from 'vitest';
import type { CatalogManifest, MasterDeployment, MasterList, MasterRecord } from './api';
import {
  changesExisting,
  defaultOperations,
  deploymentAliases,
  deploymentModalities,
  groupMasters,
  linkChoices,
  linkIfMatch,
  linkInUse,
  masterOperations,
  proposalMatches,
  recordVersion,
  proposalEntries,
  proposalProblem,
  routeAcrossProviders,
  routeChanges,
  slugProblem,
  suggestionGroups,
} from './model-masters-model';

const now = new Date('2026-11-19T10:00:00Z');
const op = (operation: string, tools = false) => ({ operation, max_input_bytes: 4096, max_output_tokens: 32, tools, price: null });
const deployment = (name: string, extra: Partial<MasterDeployment> = {}): MasterDeployment => ({
  deployment: name, model: name, provider_family: 'bedrock', adapter_instance_key: 'bedrock-main', location: 'us-east-1',
  master: null, resolved_by: 'none', card: null, link: null, operations: [op('chat')], availability: null, routes: [], ...extra,
});
const record = (id: string, extra: Partial<MasterRecord> = {}): MasterRecord => ({
  id, name: id, kind: id.startsWith('org/') ? 'organization' : 'card', vendor: null, origin: null, modality: 'chat',
  version: id.startsWith('org/') ? 1 : null, deployments: [], ...extra,
});

describe('master links', () => {
  it('sends "v0" for a first link and the link version after', () => {
    expect(linkIfMatch(null)).toBe('"v0"');
    expect(linkIfMatch({ master: null, version: 3, cleared: false })).toBe('"v3"');
    expect(linkIfMatch({ master: 'org/qwen', version: 1, cleared: false })).toBe('"v1"');
    // A cleared link is kept: it is set again at its version.
    expect(linkIfMatch({ master: null, version: 4, cleared: true })).toBe('"v4"');
    expect(linkInUse({ master: null, version: 4, cleared: true })).toBeNull();
    expect(linkInUse({ master: null, version: 3, cleared: false })).toEqual({ master: null, version: 3, cleared: false });
    expect(linkInUse(null)).toBeNull();
  });

  it('creates a deleted record again at its version', () => {
    expect(recordVersion('org/llama', [{ id: 'org/llama', version: 2 }])).toBe(2);
    expect(recordVersion('org/new', [{ id: 'org/llama', version: 2 }])).toBeNull();
  });

  it('tells a proposal read for other settings', () => {
    expect(proposalMatches({ operations: ['chat', 'embeddings'], tools: false }, ['embeddings', 'chat'], false)).toBe(true);
    expect(proposalMatches({ operations: ['chat'], tools: false }, ['chat'], true)).toBe(false);
    expect(proposalMatches({ operations: ['chat'], tools: false }, ['chat', 'embeddings'], false)).toBe(false);
  });

  it('offers only records of a modality the deployment serves', () => {
    const items = [record('org/zed'), record('openai/text-embedding-3', { name: 'Embed', modality: 'embeddings' }), record('anthropic/claude', { name: 'Claude' })];
    expect(deploymentModalities(deployment('d', { operations: [op('chat_stream'), op('chat_batch')] }))).toEqual(['chat']);
    expect(linkChoices(items, deployment('d')).map(item => item.id)).toEqual(['anthropic/claude', 'org/zed']);
    expect(linkChoices(items, deployment('e', { operations: [op('embeddings')] })).map(item => item.id)).toEqual(['openai/text-embedding-3']);
    expect(linkChoices(items, deployment('both', { operations: [op('chat'), op('embeddings_batch')] }))).toHaveLength(3);
  });
});

describe('grouping', () => {
  it('lists records with deployments by name, idle organization records and unlinked deployments apart', () => {
    const list: MasterList = {
      release_id: 'r',
      items: [
        record('anthropic/sonnet', { name: 'Sonnet', deployments: [deployment('z-bedrock'), deployment('a-anthropic')] }),
        record('anthropic/haiku', { name: 'Haiku', deployments: [deployment('haiku')] }),
        record('anthropic/opus', { name: 'Opus' }),
        record('org/qwen', { name: 'Qwen' }),
      ],
      unlinked: [deployment('y'), deployment('x')],
      links: [],
      deleted_records: [],
    };
    const groups = groupMasters(list);
    expect(groups.linked.map(item => item.id)).toEqual(['anthropic/haiku', 'anthropic/sonnet']);
    expect(groups.linked[1].deployments.map(item => item.deployment)).toEqual(['a-anthropic', 'z-bedrock']);
    expect(groups.idleOrganization.map(item => item.id)).toEqual(['org/qwen']);
    expect(groups.unlinked.map(item => item.deployment)).toEqual(['x', 'y']);
  });

  it('folds streaming routes into chat per alias', () => {
    expect(deploymentAliases(deployment('d', { routes: [{ alias: 'b', operation: 'chat' }, { alias: 'b', operation: 'chat_stream' }, { alias: 'a', operation: 'embeddings' }] })))
      .toEqual([{ alias: 'a', operations: ['embeddings'] }, { alias: 'b', operations: ['chat'] }]);
  });

  it('proposes the operations the deployments serve, chat first', () => {
    const master = record('m', { deployments: [deployment('a', { operations: [op('chat_batch'), op('chat_stream')] }), deployment('b', { operations: [op('embeddings')] })] });
    expect(masterOperations(master)).toEqual(['chat', 'embeddings', 'chat_batch']);
    expect(defaultOperations(master)).toEqual(['chat']);
    expect(defaultOperations(record('e', { deployments: [deployment('e', { operations: [op('embeddings')] })] }))).toEqual(['embeddings']);
  });
});

describe('route proposals', () => {
  const existing = [{ operation: 'chat', deployment: 'old', weight: 1, priority: 1 }, { operation: 'chat', deployment: 'kept', weight: 2, priority: 1 }];

  it('diffs the edited routes against the alias’s current ones', () => {
    const entries = proposalEntries([{ operation: 'chat', deployment: 'kept', weight: 3, priority: 1 }, { operation: 'chat', deployment: 'new', weight: 1, priority: 2 }]);
    const changes = routeChanges(existing, entries);
    expect(changes.map(change => [change.deployment, change.change])).toEqual([['kept', 'changed'], ['new', 'added'], ['old', 'removed']]);
    expect(changes[0]).toMatchObject({ before: { weight: 2, priority: 1 }, after: { weight: 3, priority: 1 } });
    expect(changesExisting(changes)).toBe(true);
    expect(changesExisting(routeChanges([], entries))).toBe(false);
    expect(routeChanges(existing, proposalEntries(existing)).every(change => change.change === 'unchanged')).toBe(true);
  });

  it('checks weights, priorities, duplicates and the size of each route', () => {
    const entry = (deployment: string, weight = '1', priority = '1', operation = 'chat') => ({ operation, deployment, weight, priority });
    expect(proposalProblem([])).toMatch(/at least one/);
    expect(proposalProblem([entry('a', '0')])).toMatch(/weight of a/);
    expect(proposalProblem([entry('a', '1001')])).toMatch(/weight of a/);
    expect(proposalProblem([entry('a', '1', '11')])).toMatch(/priority of a/);
    expect(proposalProblem([entry('a'), entry('a')])).toMatch(/twice/);
    expect(proposalProblem([entry('a'), entry('a', '1', '1', 'embeddings')])).toBeNull();
    expect(proposalProblem(Array.from({ length: 9 }, (_, i) => entry(`d${i}`)))).toMatch(/at most 8/);
    expect(proposalProblem([entry('a', '1000', '10')])).toBeNull();
  });

  const base = (): CatalogManifest => ({
    release: { version: 'v1' },
    providers: [],
    deployments: ['old', 'kept', 'new', 'other'].map(name => ({ name, model: name, location: null, adapter_instance_key: 'a', capabilities: [] })),
    prices: [],
    routes: [
      { alias: 'sonnet', operation: 'chat', deployment: 'old' },
      { alias: 'sonnet', operation: 'chat', deployment: 'kept', weight: 2 },
      { alias: 'sonnet', operation: 'embeddings', deployment: 'other' },
      { alias: 'else', operation: 'chat', deployment: 'old' },
    ],
  });

  it('replaces only the alias’s routes of the proposal’s operations', () => {
    const proposal = { alias: 'sonnet', operations: ['chat'], existing };
    const entries = proposalEntries([{ operation: 'chat', deployment: 'kept', weight: 1, priority: 1 }, { operation: 'chat', deployment: 'new', weight: 5, priority: 2 }]);
    const result = routeAcrossProviders(proposal, entries, now)(base());
    if ('problem' in result) throw new Error(result.problem);
    expect(result.routes).toEqual([
      { alias: 'sonnet', operation: 'embeddings', deployment: 'other' },
      { alias: 'else', operation: 'chat', deployment: 'old' },
      // Weight and priority of 1 are left out, as the release stores them.
      { alias: 'sonnet', operation: 'chat', deployment: 'kept' },
      { alias: 'sonnet', operation: 'chat', deployment: 'new', weight: 5, priority: 2 },
    ]);
    expect(result.release.version).toBe('2026-11-19T10:00Z routes sonnet across providers');
    expect(result.deployments).toEqual(base().deployments);
  });

  it('refuses when the alias changed since the proposal, or a deployment is gone', () => {
    const entries = proposalEntries([{ operation: 'chat', deployment: 'new', weight: 1, priority: 1 }]);
    expect(routeAcrossProviders({ alias: 'sonnet', operations: ['chat'], existing: [] }, entries, now)(base())).toEqual({ problem: expect.stringMatching(/changed since the proposal/) });
    expect(routeAcrossProviders({ alias: 'sonnet', operations: ['chat'], existing }, proposalEntries([{ operation: 'chat', deployment: 'gone', weight: 1, priority: 1 }]), now)(base())).toEqual({ problem: 'gone is no longer in the catalog.' });
    expect(routeAcrossProviders({ alias: 'sonnet', operations: ['chat'], existing }, [], now)(base())).toEqual({ problem: expect.stringMatching(/at least one/) });
    expect(routeAcrossProviders({ alias: 'sonnet', operations: ['chat'], existing }, entries, now)(null)).toEqual({ problem: 'No catalog release is active.' });
  });

  it('adds a new alias next to the routes there are', () => {
    const result = routeAcrossProviders({ alias: 'fresh', operations: ['chat'], existing: [] }, proposalEntries([{ operation: 'chat', deployment: 'new', weight: 1, priority: 1 }]), now)(base());
    if ('problem' in result) throw new Error(result.problem);
    expect(result.routes).toHaveLength(5);
    expect(result.routes.at(-1)).toEqual({ alias: 'fresh', operation: 'chat', deployment: 'new' });
  });
});

describe('organization records', () => {
  it('takes the slugs the server takes', () => {
    expect(slugProblem('qwen-3.5_local')).toBeNull();
    expect(slugProblem('')).toMatch(/Enter/);
    expect(slugProblem('Qwen')).toMatch(/Lowercase/);
    expect(slugProblem('-qwen')).toMatch(/Lowercase/);
    expect(slugProblem('a/b')).toMatch(/Lowercase/);
    expect(slugProblem('a'.repeat(65))).toMatch(/64/);
  });
});

describe('suggestion groups', () => {
  it('groups suggestions by alias and operation, in the server’s order', () => {
    expect(suggestionGroups({})).toEqual([]);
    expect(suggestionGroups({ suggestions: [
      { alias: 'a', operation: 'chat', deployment: 'x' },
      { alias: 'a', operation: 'chat', deployment: 'y' },
      { alias: 'a', operation: 'embeddings', deployment: 'x' },
      { alias: 'b', operation: 'chat', deployment: 'z' },
    ] })).toEqual([
      { alias: 'a', operation: 'chat', deployments: ['x', 'y'] },
      { alias: 'a', operation: 'embeddings', deployments: ['x'] },
      { alias: 'b', operation: 'chat', deployments: ['z'] },
    ]);
  });
});
