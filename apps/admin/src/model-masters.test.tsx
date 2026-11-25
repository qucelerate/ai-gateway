import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ModelMasters } from './model-masters';
import { ProvidersScreen } from './providers-screen';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string, detail = '') => new Response(JSON.stringify({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }), { status, headers: { 'Content-Type': 'application/problem+json' } });
const CATALOG = '/api/admin/v1/organizations/org-1/catalog';
const RELEASE = '11111111-1111-1111-1111-111111111111';
const price = (input: number) => ({
  currency: 'USD', input_minor_per_token: input, output_minor_per_token: 1500, cached_input_minor_per_token: 0, reasoning_minor_per_token: 0,
  other_minor_per_token: 0, billed_by: 'release', source: 'list', source_version: '1', confidence: 'known',
});
const chat = (tools: boolean, input = 300) => ({ operation: 'chat', max_input_bytes: 1_048_576, max_output_tokens: 64_000, tools, price: price(input) });
const masterDeployment = (deployment: string, extra: object = {}) => ({
  deployment, model: deployment, provider_family: 'anthropic', adapter_instance_key: 'anthropic-main', location: null,
  master: 'anthropic/claude-sonnet-4-6', resolved_by: 'card', card: 'anthropic/claude-sonnet-4-6', link: null,
  operations: [chat(true)], availability: { state: 'verified', reason: null, source: 'traffic', observed_at: '2026-11-18T00:00:00Z', verified_at: '2026-11-18T00:00:00Z' },
  routes: [{ alias: 'claude-sonnet-4-6', operation: 'chat' }], ...extra,
});
const sonnet = {
  id: 'anthropic/claude-sonnet-4-6', name: 'Claude Sonnet 4.6', kind: 'card', vendor: 'Anthropic', origin: 'builtin', modality: 'chat', version: null,
  deployments: [
    masterDeployment('claude-sonnet-4-6'),
    masterDeployment('bedrock-sonnet', { model: 'us.anthropic.claude-sonnet-4-6', provider_family: 'bedrock', adapter_instance_key: 'bedrock-main', location: 'us-east-1', operations: [chat(false, 330)], availability: null, routes: [] }),
  ],
  // The alias routes Anthropic's deployment, not Bedrock's.
  suggestions: [{ alias: 'claude-sonnet-4-6', operation: 'chat', deployment: 'bedrock-sonnet' }],
};
const embed = { id: 'openai/text-embedding-3-small', name: 'Text embedding 3 small', kind: 'card', vendor: 'OpenAI', origin: 'builtin', modality: 'embeddings', version: null, deployments: [] };
const qwen = { id: 'org/qwen', name: 'Qwen local', kind: 'organization', vendor: null, origin: null, modality: 'chat', version: 2, deployments: [] };
const unlinked = masterDeployment('vllm-qwen', { model: 'qwen3', provider_family: 'openai-compatible', adapter_instance_key: 'gpu', master: null, resolved_by: 'none', card: null, routes: [] });
const list = { release_id: RELEASE, items: [sonnet, embed, qwen], unlinked: [unlinked], links: [], deleted_records: [] as object[] };
const proposal = (alias = 'claude-sonnet-4-6') => ({
  master: sonnet.id, release_id: RELEASE, alias, alias_exists: alias === 'claude-sonnet-4-6', operations: ['chat'], tools: false,
  routes: [{ operation: 'chat', deployment: 'claude-sonnet-4-6', weight: 1, priority: 1 }, { operation: 'chat', deployment: 'bedrock-sonnet', weight: 1, priority: 1 }],
  excluded: [{ operation: 'chat', deployment: 'foundry-sonnet', reason: 'currency', detail: 'Priced in EUR; the route is priced in USD.' }],
  existing: alias === 'claude-sonnet-4-6' ? [{ operation: 'chat', deployment: 'claude-sonnet-4-6', weight: 1, priority: 1 }] : [],
  changes: [], problem: null,
});
const manifest = {
  release: { version: '2026.11.1' },
  providers: [],
  deployments: ['claude-sonnet-4-6', 'bedrock-sonnet', 'foundry-sonnet'].map(name => ({ name, model: name, location: null, adapter_instance_key: 'a', capabilities: [] })),
  prices: [],
  routes: [
    { alias: 'claude-sonnet-4-6', operation: 'chat', deployment: 'claude-sonnet-4-6' },
    { alias: 'claude-sonnet-4-6', operation: 'embeddings', deployment: 'claude-sonnet-4-6' },
  ],
};

describe('models across providers', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let linkResponse: () => Response;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    history.replaceState(null, '', '/');
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    linkResponse = () => json({ provider_family: 'openai-compatible', model: 'qwen3', master: 'org/qwen', version: 1, updated_at: '', updated_by: null }, 201);
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? 'GET';
      if (url === `${CATALOG}/model-masters`) return json(list);
      if (url.startsWith(`${CATALOG}/model-master-links/`)) return linkResponse();
      if (url.startsWith(`${CATALOG}/model-masters/anthropic%2Fclaude-sonnet-4-6/route-proposal`)) {
        const alias = new URL(url, 'http://x').searchParams.get('alias') ?? undefined;
        return json(proposal(alias));
      }
      if (url.startsWith(`${CATALOG}/model-masters/org%2F`) && method === 'DELETE') return problem(409, 'master_in_use', 'links name it');
      if (url.startsWith(`${CATALOG}/model-masters/org%2F`) && method === 'PUT') return json({ id: 'org/llama', name: 'Llama', modality: 'chat', version: 1, created_at: '', updated_at: '', updated_by: null }, 201);
      if (url.endsWith('/manifest')) return json(manifest);
      if (url.endsWith('/catalog/releases') && method === 'POST') {
        const body = JSON.parse(String(init!.body));
        return json({ id: 'new', version: body.release.version, digest: 'c'.repeat(64), created_at: '', sealed_at: '', active: false, activated_at: null }, 201);
      }
      if (url.endsWith('/activate')) return json({});
      if (url.includes('/catalog/releases')) return json({ items: [{ id: RELEASE, version: '2026.11.1', digest: 'a'.repeat(64), created_at: '', sealed_at: '', active: true, activated_at: '' }], next_page_token: null, activation_version: 4 });
      if (url.includes('/deployments')) return json({ release_id: RELEASE, items: [], next_page_token: null });
      if (url.includes('/routes')) return json({ release_id: RELEASE, items: [], next_page_token: null });
      if (url.includes('/model-cards')) return json({ release_id: RELEASE, items: [] });
      return json({}, 404);
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const byLabel = (label: string) => document.querySelector(`[aria-label="${label}"]`) as HTMLElement;
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement;
  const click = async (element: HTMLElement) => {
    act(() => element.click());
    await settle();
  };
  const choose = async (id: string, value: string) => {
    const select = document.getElementById(id) as HTMLSelectElement;
    act(() => {
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
  };
  const type = async (input: HTMLInputElement, value: string) => {
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    act(() => {
      setValue.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await settle();
  };
  const render = async (canChange = true) => {
    act(() => root.render(<ModelMasters organizationId="org-1" release={RELEASE} familyLabels={{ anthropic: 'Anthropic', bedrock: 'Amazon Bedrock' }} canChange={canChange} onReleaseChanged={() => undefined} />));
    await settle();
  };
  const calls = (fragment: string, method: string) => fetchMock.mock.calls.filter(([url, init]) => String(url).includes(fragment) && ((init as RequestInit | undefined)?.method ?? 'GET') === method);
  const header = (init: unknown, name: string) => new Headers((init as RequestInit).headers).get(name);

  it('shows each record with its deployments side by side, and the unlinked ones apart', async () => {
    await render();
    const record = byLabel('Claude Sonnet 4.6');
    expect(record.textContent).toContain('anthropic/claude-sonnet-4-6');
    const rows = Array.from(record.querySelectorAll('tbody tr')).map(row => row.textContent ?? '');
    expect(rows).toHaveLength(2);
    // Sorted by deployment: Bedrock first.
    expect(rows[0]).toContain('Amazon Bedrock');
    expect(rows[0]).toContain('bedrock-main');
    expect(rows[0]).toContain('us-east-1');
    expect(rows[0]).toContain('$3.30');
    expect(rows[0]).toContain('Not yet checked');
    expect(rows[1]).toContain('Tools');
    expect(rows[1]).toContain('Verified');
    expect(rows[1]).toContain('1.0 MiB in · 64,000 out');
    expect(rows[1]).toContain('claude-sonnet-4-6');
    expect(rows[1]).toContain('Card');
    // Records without deployments are not listed as models; the idle organization record is.
    expect(byLabel('Text embedding 3 small')).toBeNull();
    expect(byLabel('Deployments with no model record').textContent).toContain('vllm-qwen');
    expect(container.textContent).toContain('Organization records with no deployments');
    expect(container.textContent).toContain('org/qwen');
  });

  it('offers no change controls to a read-only role', async () => {
    await render(false);
    expect(button('Route across providers')).toBeUndefined();
    expect(button('Change link')).toBeUndefined();
    expect(button('New organization record')).toBeUndefined();
  });

  it('links a deployment to a record of its modality with "v0" and a new key per attempt', async () => {
    await render();
    await click(byLabel('Change the record of vllm-qwen'));
    const options = Array.from((document.getElementById('master-link') as HTMLSelectElement).options).map(option => option.value);
    expect(options).toContain('org/qwen');
    expect(options).toContain(sonnet.id);
    expect(options).not.toContain(embed.id);
    await choose('master-link', 'org/qwen');
    linkResponse = () => problem(422, 'modality_mismatch');
    await click(button('Save link'));
    expect(document.querySelector('.dialog')!.textContent).toContain('another modality');
    linkResponse = () => json({ provider_family: 'openai-compatible', model: 'qwen3', master: 'org/qwen', version: 1, updated_at: '', updated_by: null }, 201);
    await click(button('Save link'));
    const puts = calls('/model-master-links/', 'PUT');
    expect(puts).toHaveLength(2);
    expect(String(puts[1][0])).toBe(`${CATALOG}/model-master-links/openai-compatible/qwen3`);
    expect(JSON.parse(String((puts[1][1] as RequestInit).body))).toEqual({ master: 'org/qwen' });
    expect(header(puts[1][1], 'If-Match')).toBe('"v0"');
    // The server answered the first: the second is a new request.
    expect(header(puts[0][1], 'Idempotency-Key')).not.toBe(header(puts[1][1], 'Idempotency-Key'));
    expect(document.querySelector('.dialog')).toBeNull();
    expect(container.textContent).toContain('Linked qwen3 to Qwen local.');
  });

  it('clears a link against its version so the card decides again', async () => {
    const linked = { ...sonnet, deployments: [masterDeployment('claude-sonnet-4-6', { resolved_by: 'link', link: { master: sonnet.id, version: 3, cleared: false } })] };
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === `${CATALOG}/model-masters`) return json({ ...list, items: [linked] });
      if (url.includes('/model-master-links/') && init?.method === 'DELETE') return new Response(null, { status: 204 });
      return json({}, 404);
    });
    await render();
    await click(byLabel('Change the record of claude-sonnet-4-6'));
    await choose('master-link', '\u0000clear');
    await click(button('Clear link'));
    const [[url, init]] = calls('/model-master-links/', 'DELETE');
    expect(String(url)).toBe(`${CATALOG}/model-master-links/anthropic/claude-sonnet-4-6`);
    expect(header(init, 'If-Match')).toBe('"v3"');
  });

  it('creates an organization record and explains a refused delete', async () => {
    await render();
    await click(button('New organization record'));
    await type(document.getElementById('master-slug') as HTMLInputElement, 'llama');
    await type(document.getElementById('master-name') as HTMLInputElement, 'Llama');
    await click(button('Create record'));
    const [[url, init]] = calls('/model-masters/org%2Fllama', 'PUT');
    expect(String(url)).toBe(`${CATALOG}/model-masters/org%2Fllama`);
    expect(header(init, 'If-Match')).toBe('"v0"');
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ name: 'Llama', modality: 'chat' });
    await click(byLabel('Delete Qwen local'));
    await click(Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Delete') as HTMLButtonElement);
    const [[, deleteInit]] = calls('/model-masters/org%2Fqwen', 'DELETE');
    expect(header(deleteInit, 'If-Match')).toBe('"v2"');
    expect(document.querySelector('.dialog')!.textContent).toContain('Links still name this record');
  });

  it('proposes a route, shows the change to the existing alias, and publishes the edited routes', async () => {
    const published: string[] = [];
    act(() => root.render(<ModelMasters organizationId="org-1" release={RELEASE} familyLabels={{}} canChange onReleaseChanged={summary => published.push(summary)} />));
    await settle();
    await click(byLabel('Route Claude Sonnet 4.6 across providers'));
    const dialog = document.querySelector('.dialog') as HTMLElement;
    expect(String(calls('/route-proposal', 'GET')[0][0])).toBe(`${CATALOG}/model-masters/anthropic%2Fclaude-sonnet-4-6/route-proposal?operations=chat`);
    expect((document.getElementById('proposal-alias') as HTMLInputElement).value).toBe('claude-sonnet-4-6');
    expect(dialog.textContent).toContain('claude-sonnet-4-6 exists already');
    expect(byLabel('Deployments left out').textContent).toContain('Other currency');
    expect(byLabel('Deployments left out').textContent).toContain('Priced in EUR');
    const changes = () => Array.from(byLabel('Changes to claude-sonnet-4-6').querySelectorAll('tbody tr')).map(row => row.textContent);
    expect(changes()).toEqual([expect.stringContaining('Added'), expect.stringContaining('Unchanged')]);
    const publish = () => button('Publish and activate');
    // Nothing changes on the existing route yet: no confirmation needed.
    expect(publish().disabled).toBe(false);
    await type(byLabel('Weight of claude-sonnet-4-6 chat') as HTMLInputElement, '3');
    expect(changes()[1]).toContain('Changed');
    // Changing the alias's current route needs a word from the admin.
    expect(publish().disabled).toBe(true);
    const confirm = Array.from(dialog.querySelectorAll('input[type="checkbox"]')).find(input => input.parentElement?.textContent?.includes('Change the existing alias')) as HTMLInputElement;
    await click(confirm);
    expect(publish().disabled).toBe(false);
    await type(byLabel('Weight of claude-sonnet-4-6 chat') as HTMLInputElement, '0');
    expect(dialog.textContent).toContain('weight of claude-sonnet-4-6 is a whole number');
    expect(publish().disabled).toBe(true);
    await type(byLabel('Weight of claude-sonnet-4-6 chat') as HTMLInputElement, '3');
    // An excluded deployment can be added back.
    await choose('proposal-add', 'chat\u0000foundry-sonnet');
    await click(button('Add'));
    await click(byLabel('Remove bedrock-sonnet chat'));
    // Each edit asks again: the changes it confirmed are not the ones now.
    expect(confirm.checked).toBe(false);
    await click(confirm);
    await click(publish());
    const [[, create]] = calls('/catalog/releases', 'POST');
    const body = JSON.parse(String((create as RequestInit).body));
    expect(body.routes).toEqual([
      { alias: 'claude-sonnet-4-6', operation: 'embeddings', deployment: 'claude-sonnet-4-6' },
      { alias: 'claude-sonnet-4-6', operation: 'chat', deployment: 'claude-sonnet-4-6', weight: 3 },
      { alias: 'claude-sonnet-4-6', operation: 'chat', deployment: 'foundry-sonnet' },
    ]);
    expect(calls('/activate', 'POST')).toHaveLength(1);
    expect(document.querySelector('.dialog')).toBeNull();
    expect(published).toEqual([expect.stringMatching(/^Routed claude-sonnet-4-6 across providers; release .* is active\.$/)]);
  });

  it('locks the route editor while it publishes', async () => {
    const served = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    let finish: (response: Response) => void = () => undefined;
    fetchMock.mockImplementation((url: string, init?: RequestInit) => {
      if (String(url).endsWith('/catalog/releases') && init?.method === 'POST') return new Promise<Response>(resolve => { finish = resolve; });
      return served(url, init);
    });
    await render();
    await click(byLabel('Route Claude Sonnet 4.6 across providers'));
    const weight = byLabel('Weight of claude-sonnet-4-6 chat') as HTMLInputElement;
    expect(weight.closest('fieldset')?.disabled).toBe(false);
    await click(button('Publish and activate'));
    // The publish carries the routes as they were: none can change now.
    expect(weight.closest('fieldset')?.disabled).toBe(true);
    expect(byLabel('Remove bedrock-sonnet chat').closest('fieldset')?.disabled).toBe(true);
    finish(json({ id: 'new', version: 'v', digest: 'c'.repeat(64), created_at: '', sealed_at: '', active: false, activated_at: null }, 201));
    await settle();
    expect(document.querySelector('.dialog')).toBeNull();
  });

  it('suggests a deployment an alias of the model leaves out, and reviews that alias’s route', async () => {
    await render(false);
    const suggestions = byLabel('Suggestions for Claude Sonnet 4.6');
    expect(suggestions.textContent).toContain('claude-sonnet-4-6 routes chat to other deployments of this model, but not to bedrock-sonnet.');
    // Only a suggestion: a read-only role sees it, with nothing to act on.
    expect(suggestions.querySelector('button')).toBeNull();
    expect(byLabel('Suggestions for Text embedding 3 small')).toBeNull();
    await render();
    const review = byLabel('Review the route for claude-sonnet-4-6 chat');
    // Its accessible name starts with what it shows (WCAG 2.5.3).
    expect(review.getAttribute('aria-label')!.startsWith(review.textContent!)).toBe(true);
    await click(review);
    // The proposal is read for that alias and operation; nothing is written.
    expect(String(calls('/route-proposal', 'GET')[0][0])).toBe(`${CATALOG}/model-masters/anthropic%2Fclaude-sonnet-4-6/route-proposal?operations=chat&alias=claude-sonnet-4-6`);
    expect((document.getElementById('proposal-alias') as HTMLInputElement).value).toBe('claude-sonnet-4-6');
    expect(calls('/catalog/releases', 'POST')).toHaveLength(0);
  });

  it('offers no review for a record with nothing to propose a route for', async () => {
    // As Route across providers: no deployment of it serves an operation.
    const idle = { ...sonnet, deployments: sonnet.deployments.map(d => ({ ...d, operations: [] })) };
    answer(url => (url === `${CATALOG}/model-masters` ? json({ ...list, items: [idle, embed, qwen] }) : undefined));
    await render();
    expect(byLabel('Suggestions for Claude Sonnet 4.6').querySelector('button')).toBeNull();
    expect(byLabel('Route Claude Sonnet 4.6 across providers')).toBeNull();
  });

  it('proposes again for another alias before it can publish', async () => {
    await render();
    await click(byLabel('Route Claude Sonnet 4.6 across providers'));
    await type(document.getElementById('proposal-alias') as HTMLInputElement, 'sonnet');
    expect(button('Publish and activate').disabled).toBe(true);
    await click(button('Propose for this alias'));
    expect(String(calls('/route-proposal', 'GET').at(-1)![0])).toContain('alias=sonnet');
    expect(document.querySelector('.dialog')!.textContent).toContain('The alias sonnet is new.');
    expect(button('Publish and activate').disabled).toBe(false);
    for (const label of ['Remove claude-sonnet-4-6 chat', 'Remove bedrock-sonnet chat']) await click(byLabel(label));
    expect(button('Publish and activate').disabled).toBe(true);
  });

  // Answers `url` specially, else as beforeEach does.
  const answer = (special: (url: string, init?: RequestInit) => Response | Promise<Response> | undefined) => {
    const base = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => (await special(url, init)) ?? base(url, init));
  };

  it('sets a cleared link again at its version, not "v0"', async () => {
    const cleared = masterDeployment('claude-sonnet-4-6', { resolved_by: 'card', link: { master: null, version: 4, cleared: true } });
    answer(url => (url === `${CATALOG}/model-masters` ? json({ ...list, items: [{ ...sonnet, deployments: [cleared] }, embed, qwen] }) : undefined));
    await render();
    await click(byLabel('Change the record of claude-sonnet-4-6'));
    const select = document.getElementById('master-link') as HTMLSelectElement;
    // The card decides, as with no link: that is the current choice.
    expect(select.value).toBe('\u0000clear');
    expect(select.selectedOptions[0].textContent).toContain('current');
    await choose('master-link', 'org/qwen');
    await click(button('Save link'));
    const [[, init]] = calls('/model-master-links/', 'PUT');
    expect(header(init, 'If-Match')).toBe('"v4"');
  });

  it('shows a link to a deleted record as the current value, and clears it', async () => {
    const orphan = { ...unlinked, master: 'org/gone', resolved_by: 'link', link: { master: 'org/gone', version: 2, cleared: false } };
    answer((url, init) => {
      if (url === `${CATALOG}/model-masters`) return json({ ...list, unlinked: [orphan] });
      if (url.includes('/model-master-links/') && init?.method === 'DELETE') return new Response(null, { status: 204 });
      return undefined;
    });
    await render();
    await click(byLabel('Change the record of vllm-qwen'));
    const select = document.getElementById('master-link') as HTMLSelectElement;
    expect(select.value).toBe('org/gone');
    expect(select.selectedOptions[0].textContent).toBe('org/gone (deleted) · current');
    await choose('master-link', '\u0000clear');
    await click(button('Clear link'));
    const [[, init]] = calls('/model-master-links/', 'DELETE');
    expect(header(init, 'If-Match')).toBe('"v2"');
  });

  it('creates a deleted record again at its version', async () => {
    answer(url => (url === `${CATALOG}/model-masters`
      ? json({ ...list, deleted_records: [{ id: 'org/llama', name: 'Llama', modality: 'chat', version: 2, created_at: '', updated_at: '', updated_by: null, deleted: true }] })
      : undefined));
    await render();
    await click(button('New organization record'));
    expect(document.querySelector('.dialog legend')!.textContent).toBe('What the model does');
    await type(document.getElementById('master-slug') as HTMLInputElement, 'llama');
    expect(document.querySelector('.dialog')!.textContent).toContain('org/llama was deleted');
    await type(document.getElementById('master-name') as HTMLInputElement, 'Llama');
    await click(button('Create record'));
    const [[, init]] = calls('/model-masters/org%2Fllama', 'PUT');
    expect(header(init, 'If-Match')).toBe('"v2"');
    expect(container.textContent).toContain('Created again the record Llama');
  });

  it('gets the same proposal again, and keeps a stale one from publishing', async () => {
    answer(url => (url.includes('/route-proposal') && url.includes('tools=true') ? problem(503, 'service_unavailable') : undefined));
    await render();
    await click(byLabel('Route Claude Sonnet 4.6 across providers'));
    const reads = () => calls('/route-proposal', 'GET').length;
    expect(reads()).toBe(1);
    await click(button('Get the proposal again'));
    expect(reads()).toBe(2);
    expect(button('Publish and activate').disabled).toBe(false);
    // Reading it with tools fails: the proposal shown is without tools.
    const withTools = Array.from(document.querySelectorAll('.dialog input[type="checkbox"]')).find(input => input.parentElement?.textContent?.includes('With tools')) as HTMLInputElement;
    await click(withTools);
    expect(withTools.checked).toBe(true);
    expect(document.querySelector('.dialog')!.textContent).toContain('not the operations chosen above');
    expect(button('Publish and activate').disabled).toBe(true);
  });

  it('stops reading when the last operation is unticked mid-read', async () => {
    let release: (response: Response) => void = () => undefined;
    let first = true;
    answer(url => {
      if (!url.includes('/route-proposal')) return undefined;
      if (first) { first = false; return undefined; }
      return new Promise<Response>(resolve => { release = resolve; });
    });
    await render();
    await click(byLabel('Route Claude Sonnet 4.6 across providers'));
    const box = (text: string) => Array.from(document.querySelectorAll('.dialog input[type="checkbox"]')).find(input => input.parentElement?.textContent === text) as HTMLInputElement;
    await click(box('With tools (chat only)'));
    expect(document.querySelector('.dialog')!.textContent).toContain('Reading the proposal');
    await click(box('Chat'));
    expect(document.querySelector('.dialog')!.textContent).not.toContain('Reading the proposal');
    expect(document.querySelector('.dialog')!.textContent).toContain('Choose at least one operation.');
    release(json(proposal()));
    await settle();
    expect(document.querySelector('.dialog')!.textContent).not.toContain('Reading the proposal');
  });

  it('can keep an alias route to a deployment of another record', async () => {
    answer(url => (url.includes('/route-proposal')
      ? json({ ...proposal(), existing: [...proposal().existing, { operation: 'chat', deployment: 'gpt-luna', weight: 5, priority: 2 }] })
      : undefined));
    await render();
    await click(byLabel('Route Claude Sonnet 4.6 across providers'));
    const option = Array.from((document.getElementById('proposal-add') as HTMLSelectElement).options).find(item => item.value === 'chat\u0000gpt-luna');
    expect(option!.textContent).toContain('on the alias now, of another record');
    await choose('proposal-add', 'chat\u0000gpt-luna');
    await click(button('Add'));
    expect((byLabel('Weight of gpt-luna chat') as HTMLInputElement).value).toBe('5');
    expect((byLabel('Priority of gpt-luna chat') as HTMLInputElement).value).toBe('2');
  });

  it('has a page of its own under Models, apart from the deployments list', async () => {
    history.replaceState(null, '', '/admin/models/masters');
    act(() => root.render(<ProvidersScreen section="models" organizationId="org-1" role="admin" />));
    await settle();
    expect(container.textContent).toContain('Models across providers');
    expect(byLabel('Claude Sonnet 4.6')).not.toBeNull();
    history.replaceState(null, '', '/admin/models/models');
    act(() => root.render(<ProvidersScreen section="models" organizationId="org-1" role="admin" key="models" />));
    await settle();
    expect(container.textContent).not.toContain('Models across providers');
  });
});
