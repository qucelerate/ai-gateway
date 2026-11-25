import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import type { CatalogDeployment, CatalogRoute } from './api';
import { PlaygroundScreen, buildRequest, buildTools, codeRequest, playgroundModels, unansweredCalls } from './playground-screen';
import { languages, snippet } from './playground-code';
import { tokenize } from './code-highlight';

const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
const price = {
  id: 'p', currency: 'USD', input_minor_per_token: 1_000_000, output_minor_per_token: 2_000_000, cached_input_minor_per_token: 0,
  reasoning_minor_per_token: 0, other_minor_per_token: 0, source: 'list', source_version: '1', confidence: 'known' as const,
};
const deployments: CatalogDeployment[] = [{
  id: 'd1', provider_account_id: 'a', adapter_instance_key: 'openai-main', family: 'openai', name: 'gpt-prod', model: 'gpt-5', location: 'eastus2',
  operations: [
    { operation: 'chat', capability: { id: 'c', max_output_tokens: 1000 }, price },
    { operation: 'chat_stream', capability: { id: 'c2', max_output_tokens: 1000 }, price: { ...price, input_minor_per_token: 9 } },
  ],
}, {
  id: 'd2', provider_account_id: 'a', adapter_instance_key: 'openai-main', family: 'openai', name: 'big-prod', model: 'big', location: null,
  operations: [{ operation: 'chat', capability: { id: 'c3', max_output_tokens: 4096 }, price: { ...price, currency: 'EUR', input_minor_per_token: 50_000_000 } }],
}, {
  id: 'd3', provider_account_id: 'b', adapter_instance_key: 'bedrock-1', family: 'bedrock', name: 'titan', model: 'amazon.titan-embed-text-v2:0', location: 'us-east-1',
  operations: [{ operation: 'embeddings', capability: { id: 'c4', max_output_tokens: 1 }, price: { ...price, output_minor_per_token: 0 } }],
}];
const routes: CatalogRoute[] = [
  { id: 'r1', alias: 'chat-default', operation: 'chat', deployment_id: 'd1', deployment_name: 'gpt-prod', model: 'gpt-5', family: 'openai', weight: 1, priority: 1 },
  { id: 'r2', alias: 'chat-default', operation: 'chat_stream', deployment_id: 'd1', deployment_name: 'gpt-prod', model: 'gpt-5', family: 'openai', weight: 1, priority: 1 },
  { id: 'r4', alias: 'pricey', operation: 'chat', deployment_id: 'd2', deployment_name: 'big-prod', model: 'big', family: 'openai', weight: 1, priority: 1 },
  { id: 'r3', alias: 'embed', operation: 'embeddings', deployment_id: 'd3', deployment_name: 'titan', model: 'amazon.titan-embed-text-v2:0', family: 'bedrock', weight: 1, priority: 1 },
];
const usage = { input_tokens: 3, output_tokens: 2, cached_input_tokens: 0, reasoning_tokens: 0, other_tokens: 0 };
const apiKey = {
  id: 'k1', organization_id: 'org-1', service_account_id: 'sa-1', successor_of: null, public_prefix: 'gw_abcd', scopes: ['Inference'], status: 'active',
  expires_at: new Date(Date.now() + 3_600_000).toISOString(), revoked_at: null, disabled_at: null, last_used_at: null, created_at: new Date().toISOString(), version: 1,
};

function sse(...events: [string, unknown][]) {
  return new Response(events.map(([name, data]) => `event: ${name}\ndata: ${JSON.stringify(data)}\n\n`).join(''), {
    status: 200, headers: { 'Content-Type': 'text/event-stream', 'X-Request-Id': 'req-42' },
  });
}

describe('playground helpers', () => {
  it('lists each chat alias once, priced and capped by its chat operation', () => {
    expect(playgroundModels(routes, deployments)[0]).toEqual({
      alias: 'chat-default', model: 'gpt-5', deploymentName: 'gpt-prod', location: 'eastus2', deployments: 1, price, maxOutputTokens: 1000, tools: false,
    });
    expect(playgroundModels(routes, deployments).map(model => model.alias)).toEqual(['chat-default', 'pricey']);
  });

  it('counts the deployments an alias routes to', () => {
    const weighted = [...routes, { ...routes[0], id: 'r5', deployment_id: 'd2', deployment_name: 'big-prod', model: 'big', priority: 2 }];
    const [model] = playgroundModels(weighted, deployments);
    expect(model.deployments).toBe(2);
    // The preferred deployment (priority 1) still names the model.
    expect(model.deploymentName).toBe('gpt-prod');
  });

  it('builds the request from the instructions, the conversation and the composer, within the model’s cap', () => {
    const [model] = playgroundModels(routes, deployments);
    const form = {
      alias: 'chat-default', system: 'Be brief.', messages: [{ role: 'user' as const, content: 'Hi' }, { role: 'assistant' as const, content: 'Hello' }],
      draft: 'How are you?', maxTokens: '64', temperature: '0.5',
    };
    expect(buildRequest(form, model)).toEqual({
      request: {
        model: 'chat-default',
        messages: [{ role: 'system', content: 'Be brief.' }, { role: 'user', content: 'Hi' }, { role: 'assistant', content: 'Hello' }, { role: 'user', content: 'How are you?' }],
        max_tokens: 64,
        temperature: 0.5,
      },
      errors: {},
    });
    expect(buildRequest({ ...form, maxTokens: '1001', temperature: '3', draft: ' ' }, model).errors).toEqual({
      maxTokens: 'A whole number from 1 to 1000.', temperature: 'A number from 0 to 2.', messages: 'Write a message.',
    });
    expect(buildRequest({ ...form, system: '', messages: [], maxTokens: '', temperature: '' }, model).request)
      .toEqual({ model: 'chat-default', messages: [{ role: 'user', content: 'How are you?' }] });
  });

  it('gives the code an example question while the conversation is empty', () => {
    expect(codeRequest({ model: 'm', messages: [{ role: 'system', content: 'Be brief.' }] }).messages)
      .toEqual([{ role: 'system', content: 'Be brief.' }, { role: 'user', content: 'What can you help me with?' }]);
    const asked = { model: 'm', messages: [{ role: 'user' as const, content: 'Hi' }] };
    expect(codeRequest(asked)).toBe(asked);
  });

  it('takes tools only when every deployment of the alias does', () => {
    const withTools = deployments.map(deployment => (deployment.id === 'd1'
      ? { ...deployment, operations: deployment.operations.map(op => ({ ...op, capability: op.capability && { ...op.capability, tools: true } })) }
      : deployment));
    expect(playgroundModels(routes, withTools)[0].tools).toBe(true);
    const both = [...routes, { ...routes[0], id: 'r5', deployment_id: 'd2', deployment_name: 'big-prod', priority: 2 }];
    expect(playgroundModels(both, withTools)[0].tools).toBe(false);
  });

  it('checks the tools and the tool options, and sends them only when set', () => {
    const draft = (name: string, parameters = '') => ({ key: Math.random(), name, description: '', parameters });
    expect(buildTools([draft('get_weather', '{"type":"object","properties":{}}'), draft('now')]).tools).toEqual([
      { type: 'function', function: { name: 'get_weather', parameters: { type: 'object', properties: {} } } },
      { type: 'function', function: { name: 'now' } },
    ]);
    const bad = [draft('1st'), draft('a'), draft('a'), draft('b', '{'), draft('c', '[]'), draft('d', '{"type":"string"}')];
    const checked = buildTools(bad);
    expect([...checked.problems.values()]).toEqual([
      expect.stringMatching(/up to 64 letters/), 'Another tool is named a.', 'The parameters are not valid JSON.',
      expect.stringMatching(/"type" is "object"/), expect.stringMatching(/"type" is "object"/),
    ]);
    expect(checked.problem).toBe('5 tools need fixing.');

    const form = { alias: 'chat-default', system: '', messages: [], draft: 'Hi', maxTokens: '', temperature: '' };
    const tools = [draft('get_weather')];
    expect(buildRequest({ ...form, tools, toolChoice: 'auto', parallel: true }, undefined).request)
      .toEqual({ model: 'chat-default', messages: [{ role: 'user', content: 'Hi' }], tools: [{ type: 'function', function: { name: 'get_weather' } }] });
    expect(buildRequest({ ...form, tools, toolChoice: 'function:get_weather', parallel: false }, undefined).request).toMatchObject({
      tool_choice: { type: 'function', function: { name: 'get_weather' } }, parallel_tool_calls: false,
    });
    expect(buildRequest({ ...form, tools, toolChoice: 'required' }, undefined).request.tool_choice).toBe('required');
    expect(buildRequest({ ...form, tools, toolChoice: 'function:gone' }, undefined).errors.tools).toMatch(/No tool is named gone/);
    // Options without tools send nothing.
    expect(buildRequest({ ...form, tools: [], toolChoice: 'required', parallel: false }, undefined).request).toEqual({ model: 'chat-default', messages: [{ role: 'user', content: 'Hi' }] });
  });

  it('waits for every call’s result before the next message, and needs the tools the conversation used', () => {
    const call = (id: string) => ({ id, type: 'function' as const, function: { name: 'f', arguments: '{}' } });
    const asked = [{ role: 'user' as const, content: 'Hi' }, { role: 'assistant' as const, content: '', tool_calls: [call('a'), call('b')] }];
    expect(unansweredCalls(asked).map(c => c.id)).toEqual(['a', 'b']);
    expect(unansweredCalls([...asked, { role: 'tool', content: '1', tool_call_id: 'a' }]).map(c => c.id)).toEqual(['b']);
    expect(unansweredCalls([...asked, { role: 'tool', content: '1', tool_call_id: 'a' }, { role: 'tool', content: '2', tool_call_id: 'b' }])).toEqual([]);
    const form = { alias: 'm', system: '', messages: asked, draft: 'More', maxTokens: '', temperature: '', tools: [{ key: 1, name: 'f', description: '', parameters: '' }] };
    expect(buildRequest(form, undefined).errors.messages).toMatch(/results first/);
    expect(buildRequest({ ...form, continuing: true }, undefined).errors.messages).toMatch(/every tool call/);
    const answered = [...asked, { role: 'tool' as const, content: '1', tool_call_id: 'a' }, { role: 'tool' as const, content: '2', tool_call_id: 'b' }];
    const continued = buildRequest({ ...form, messages: answered, continuing: true }, undefined);
    expect(continued.errors).toEqual({});
    expect(continued.request.messages.at(-1)).toEqual({ role: 'tool', content: '2', tool_call_id: 'b' });
    expect(buildRequest({ ...form, messages: answered, tools: [] }, undefined).errors.tools).toMatch(/define its tools/);
    // The code fills in a placeholder for each result still missing.
    expect(codeRequest({ model: 'm', messages: asked }).messages).toEqual([
      ...asked,
      { role: 'tool', tool_call_id: 'a', content: 'The result of f.' },
      { role: 'tool', tool_call_id: 'b', content: 'The result of f.' },
    ]);
  });

  it('highlights every snippet without changing its text', () => {
    const request = { model: 'm', messages: [{ role: 'user' as const, content: 'Say "hi" — it\'s `ok` /* not */ # nope\nnext line' }] };
    for (const [language] of languages) {
      for (const style of ['client', 'raw'] as const) {
        for (const stream of [false, true]) {
          const code = snippet(language, style, request, { baseUrl: 'https://gw.example', stream });
          const tokens = tokenize(code.code, code.syntax);
          expect(tokens.map(token => token.text).join('')).toBe(code.code);
          expect(tokens.some(token => token.kind === 'string')).toBe(true);
        }
      }
    }
    expect(tokenize('import os # x', 'python')).toEqual([
      { kind: 'keyword', text: 'import' }, { kind: 'plain', text: ' ' }, { kind: 'plain', text: 'os' }, { kind: 'plain', text: ' ' }, { kind: 'comment', text: '# x' },
    ]);
  });
});

describe('playground screen', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  const settle = async () => {
    for (let i = 0; i < 12; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const setValue = (element: HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement, value: string) => {
    act(() => {
      const prototype = element instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
        : element instanceof HTMLSelectElement ? HTMLSelectElement.prototype : HTMLInputElement.prototype;
      Object.getOwnPropertyDescriptor(prototype, 'value')!.set!.call(element, value);
      element.dispatchEvent(new Event(element instanceof HTMLSelectElement ? 'change' : 'input', { bubbles: true }));
    });
  };
  const button = (text: string) =>
    Array.from(container.querySelectorAll('button')).find(candidate => candidate.textContent?.trim() === text || candidate.getAttribute('aria-label') === text) as HTMLButtonElement | undefined;
  const click = async (element: Element | undefined | null) => {
    expect(element).toBeTruthy();
    act(() => { (element as HTMLElement).click(); });
    await settle();
  };
  const draft = () => container.querySelector('#playground-draft') as HTMLTextAreaElement;
  const say = async (text: string) => {
    setValue(draft(), text);
    await click(button('Send'));
  };
  const usePastedKey = async (secret = 'gw_pasted_secret') => {
    setValue(container.querySelector('#playground-key-input') as HTMLInputElement, secret);
    await click(button('Use key'));
  };
  const inference = () => fetchMock.mock.calls.filter(([url]) => String(url).startsWith('/api/inference/')) as [string, RequestInit][];
  const writes = (method: string) => fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === method) as [string, RequestInit][];

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    history.replaceState(null, '', '/');
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.includes('/catalog/deployments')) return json({ release_id: 'rel', items: deployments, next_page_token: null });
      if (url.includes('/catalog/routes')) return json({ release_id: 'rel', items: routes, next_page_token: null });
      if (url.endsWith('/service-accounts') && init?.method === 'POST') {
        return json({ id: 'sa-1', organization_id: 'org-1', team_id: null, owner_user_id: 'user-1', name: 'playground-user-1', enabled: true, created_at: '', version: 1 }, 201);
      }
      if (url.includes('/service-accounts?')) return json({ items: [{ id: 'other', owner_user_id: 'user-2', name: 'playground-user-1', enabled: true }], next_page_token: null });
      if (url.endsWith('/service-accounts/sa-1/keys')) return json({ metadata: apiKey, secret: 'gw_temporary_secret', replay: false }, 201);
      if (url.endsWith('/keys/k1/revoke')) return json({ ...apiKey, status: 'revoked', version: 2 });
      if (url === '/api/inference/v1/chat/completions/stream') {
        const model = JSON.parse(String(init?.body)).model as string;
        return sse(
          ['response.started', { model }],
          ['choice.delta', { choice_index: 0, content_delta: model === 'pricey' ? 'Bonjour' : 'Hello ' }],
          ['choice.delta', { choice_index: 0, content_delta: model === 'pricey' ? '' : 'there' }],
          ['choice.finished', { choice_index: 0, finish_reason: 'stop' }],
          ['response.completed', { model, usage }],
        );
      }
      if (url === '/api/inference/v1/embeddings') {
        const { input } = JSON.parse(String(init?.body)) as { input: string[] };
        return json({
          object: 'list', model: 'embed',
          data: input.map((_, index) => ({ object: 'embedding', index, embedding: index === 0 ? [1, 0, 0, 0, 0.5] : [0.6, 0.8, 0, 0, 0] })),
          usage: { input_tokens: 4, output_tokens: 0 },
        }, 200, { 'X-Request-Id': 'req-embed' });
      }
      if (url === '/api/inference/v1/chat/completions') {
        return json({ code: 'rate_limited', detail: 'Too many requests.', status: 429 }, 429, { 'X-Request-Id': 'req-9' });
      }
      return json({ code: 'not_found', status: 404 }, 404);
    });
    vi.stubGlobal('fetch', fetchMock);
  });
  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    localStorage.clear();
    sessionStorage.clear();
  });

  it('sends the composer’s message with a pasted key and no session, and continues the conversation', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    expect((container.querySelector('#playground-model') as HTMLSelectElement).value).toBe('chat:chat-default');
    expect(container.querySelector('.playground-header h2')?.textContent).toBe('chat-default');
    expect(container.querySelector('.playground-model-picker')?.textContent).toContain('eastus2');
    expect(container.querySelector('.playground-model-picker')?.textContent).toContain('1000 tokens');
    // Square Carbon controls only: no pill tags on the page.
    expect(container.querySelector('.cds--tag')).toBeNull();
    expect((container.querySelector('#playground-parameters-body') as HTMLElement).hidden).toBe(false);
    expect((container.querySelector('#playground-advanced-body') as HTMLElement).hidden).toBe(true);
    expect(container.textContent).toContain('What do you want to chat about?');
    expect(container.textContent).toContain('Sending creates a temporary key');

    expect((container.querySelector('#playground-key-input') as HTMLInputElement).type).toBe('password');
    await usePastedKey();
    // The key section folds away once a key is chosen.
    expect((container.querySelector('#playground-key-body') as HTMLElement).hidden).toBe(true);
    expect(container.textContent).toContain('Using the key you pasted.');

    setValue(container.querySelector('#playground-system') as HTMLTextAreaElement, 'Be brief.');
    await say('Say hello');

    const [[url, init]] = inference();
    expect(url).toBe('/api/inference/v1/chat/completions/stream');
    expect(init.credentials).toBe('omit');
    expect(new Headers(init.headers).get('Authorization')).toBe('Bearer gw_pasted_secret');
    expect(JSON.parse(init.body as string)).toEqual({
      model: 'chat-default', messages: [{ role: 'system', content: 'Be brief.' }, { role: 'user', content: 'Say hello' }], max_tokens: 256,
    });
    expect(draft().value).toBe('');

    const lane = container.querySelector('.playground-reply')!;
    const turns = Array.from(lane.querySelectorAll('.playground-turn .playground-output')).map(node => node.textContent);
    expect(turns).toEqual(['Say hello', 'Hello there']);
    expect(lane.textContent).toContain('stop');
    expect(lane.textContent).toContain('3 in · 2 out');
    // At the chat price (one and two cents a token): 3·1 + 2·2 = 7 cents.
    expect(lane.textContent).toContain('$0.07');
    expect(lane.textContent).toContain('req-42');
    expect(lane.querySelector('a')?.getAttribute('href')).toBe('/admin/traces?traces.q=req-42');
    // The reply's facts show beside it without opening anything.
    expect(lane.querySelector('.playground-meta-list')?.textContent).toContain('$0.07');
    expect(lane.querySelector('.playground-meta-list a')?.textContent).toBe('View trace');

    // Choosing another model afterwards keeps the reply's own price.
    setValue(container.querySelector('#playground-model') as HTMLSelectElement, 'chat:pricey');
    expect(lane.textContent).toContain('$0.07');
    expect(lane.textContent).not.toContain('€');
    setValue(container.querySelector('#playground-model') as HTMLSelectElement, 'chat:chat-default');

    // Enter sends the next message, with the conversation before it.
    setValue(draft(), 'And again');
    act(() => { draft().dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true })); });
    await settle();
    expect(JSON.parse(inference()[1][1].body as string).messages).toEqual([
      { role: 'system', content: 'Be brief.' },
      { role: 'user', content: 'Say hello' },
      { role: 'assistant', content: 'Hello there' },
      { role: 'user', content: 'And again' },
    ]);
    // Kept for this tab only: never in storage other tabs or later visits read.
    expect(localStorage.length).toBe(0);
    expect(sessionStorage.length).toBe(1);

    // A new chat clears the conversation.
    await click(button('New chat'));
    expect(container.textContent).toContain('What do you want to chat about?');
  });

  it('creates a temporary inference-only key on the person’s own service account, and revokes it', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await click(button('Create temporary key'));

    const posts = writes('POST');
    // Someone else's same-named account is not reused.
    expect(posts[0][0]).toBe('/api/admin/v1/organizations/org-1/service-accounts');
    expect(JSON.parse(posts[0][1].body as string)).toEqual({ name: 'playground-user-1' });
    expect(posts[1][0]).toBe('/api/admin/v1/organizations/org-1/service-accounts/sa-1/keys');
    const body = JSON.parse(posts[1][1].body as string);
    // The key calls embedding models too.
    expect(body.scopes).toEqual(['Inference', 'Embeddings']);
    const expiresIn = Date.parse(body.expires_at) - Date.now();
    expect(expiresIn).toBeGreaterThan(3_500_000);
    expect(expiresIn).toBeLessThanOrEqual(3_600_000);
    expect(container.textContent).toContain('gw_abcd');
    expect(container.textContent).not.toContain('gw_temporary_secret');

    await say('Hi');
    expect(new Headers(inference()[0][1].headers).get('Authorization')).toBe('Bearer gw_temporary_secret');

    await click(button('API key'));
    await click(button('Revoke key'));
    expect(writes('POST').some(([url]) => url.endsWith('/service-accounts/sa-1/keys/k1/revoke'))).toBe(true);
    expect(button('Create temporary key')).toBeTruthy();
  });

  it('creates a temporary key on the first send when the page holds none', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await say('Hi');
    expect(writes('POST').some(([url]) => url.endsWith('/service-accounts/sa-1/keys'))).toBe(true);
    expect(new Headers(inference()[0][1].headers).get('Authorization')).toBe('Bearer gw_temporary_secret');
    expect(container.textContent).not.toContain('Sending creates a temporary key');
    // The next message reuses it.
    await say('Again');
    expect(writes('POST').filter(([url]) => url.endsWith('/service-accounts/sa-1/keys'))).toHaveLength(1);
  });

  it('creates one key for embeddings sent twice before it exists', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    setValue(container.querySelector('#playground-model') as HTMLSelectElement, 'embeddings:embed');
    setValue(container.querySelector('#playground-embed-input') as HTMLTextAreaElement, 'one');
    act(() => {
      button('Embed')!.click();
      button('Embed')!.click();
    });
    await settle();
    expect(writes('POST').filter(([url]) => url.endsWith('/service-accounts/sa-1/keys'))).toHaveLength(1);
    expect(inference()).toHaveLength(1);
  });

  /** Holds key creation until the returned function is called. */
  const holdKeyCreation = () => {
    let release = () => {};
    const gate = new Promise<void>(resolve => { release = resolve; });
    const answer = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/service-accounts/sa-1/keys')) await gate;
      return answer(url, init);
    });
    return async () => { release(); await settle(); };
  };

  it('sends a message once, however often it is submitted while its key is made', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    const release = holdKeyCreation();
    setValue(draft(), 'Hi');
    act(() => { draft().form!.requestSubmit(); });
    await settle();
    act(() => { draft().form!.requestSubmit(); });
    await release();
    expect(inference()).toHaveLength(1);
  });

  it('keeps what is typed while the first message waits for its key', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    const release = holdKeyCreation();
    setValue(draft(), 'Hi');
    act(() => { draft().form!.requestSubmit(); });
    await settle();
    expect(draft().value).toBe('');
    setValue(draft(), 'Next one');
    await release();
    expect(JSON.parse(inference()[0][1].body as string).messages).toEqual([{ role: 'user', content: 'Hi' }]);
    expect(draft().value).toBe('Next one');
  });

  it('revokes, and does not keep, a key made after the page was left', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    const release = holdKeyCreation();
    setValue(draft(), 'Hi');
    act(() => { draft().form!.requestSubmit(); });
    await settle();
    act(() => root.unmount());
    root = createRoot(container);
    await release();
    expect(sessionStorage.length).toBe(0);
    expect(writes('POST').some(([url]) => url.endsWith('/keys/k1/revoke'))).toBe(true);
  });

  it('drops a message that New chat discarded while its key was made', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    const release = holdKeyCreation();
    setValue(draft(), 'Hi');
    act(() => { draft().form!.requestSubmit(); });
    await settle();
    await click(button('New chat'));
    await release();
    expect(inference()).toHaveLength(0);
    expect(draft().value).toBe('');
    expect(container.textContent).not.toContain('Hello there');
  });

  it('drops embeddings whose pane closed while their key was made', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    setValue(container.querySelector('#playground-model') as HTMLSelectElement, 'embeddings:embed');
    setValue(container.querySelector('#playground-embed-input') as HTMLTextAreaElement, 'one');
    const release = holdKeyCreation();
    act(() => { button('Embed')!.click(); });
    await settle();
    act(() => root.unmount());
    root = createRoot(container);
    await release();
    expect(inference()).toHaveLength(0);
  });

  it('retries a failed message after its key was forgotten, with a new temporary key', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey('gw_x');
    await click(button('Advanced'));
    await click(container.querySelector('input[value="unary"]'));
    await say('Hi');
    await click(button('API key'));
    await click(button('Forget key'));
    await click(button('Retry'));
    expect(writes('POST').some(([url]) => url.endsWith('/service-accounts/sa-1/keys'))).toBe(true);
    expect(new Headers(inference()[1][1].headers).get('Authorization')).toBe('Bearer gw_temporary_secret');
  });

  it('keeps the key through a reload of the tab, for its organization only, and forgets it on request', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey();
    act(() => root.unmount());
    root = createRoot(container);
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    expect(container.textContent).toContain('Using the key you pasted.');
    await say('Hi');
    expect(new Headers(inference()[0][1].headers).get('Authorization')).toBe('Bearer gw_pasted_secret');

    act(() => root.unmount());
    root = createRoot(container);
    act(() => root.render(<PlaygroundScreen organizationId="org-2" userId="user-1" />));
    await settle();
    expect(container.textContent).not.toContain('Using the key you pasted.');

    act(() => root.unmount());
    root = createRoot(container);
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await click(button('API key'));
    await click(button('Forget key'));
    expect(sessionStorage.length).toBe(0);
  });

  it('explains a refused request with its request id, and retries it', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey('gw_x');
    await click(button('Advanced'));
    await click(container.querySelector('input[value="unary"]'));
    await say('Hi');
    const lane = container.querySelector('.playground-reply')!;
    expect(lane.textContent).toContain('A rate limit or budget stopped this request');
    expect(lane.textContent).toContain('rate_limited');
    expect(lane.textContent).toContain('req-9');
    // The message stays with its lane, to retry.
    expect(lane.querySelector('.playground-turn-user .playground-output')?.textContent).toBe('Hi');
    // The code shows the failed message, not the example question.
    await click(button('Continue in code'));
    const code = container.querySelector('.playground-snippet')!.textContent!;
    expect(code).toContain('"Hi"');
    expect(code).not.toContain('What can you help me with?');
    await click(button('Close'));

    // An invalid setting stops the retry and says why.
    setValue(container.querySelector('#playground-max-tokens') as HTMLInputElement, '0');
    await click(button('Retry'));
    expect(inference()).toHaveLength(1);
    expect(container.textContent).toContain('A whole number from 1 to 1000.');
    setValue(container.querySelector('#playground-max-tokens') as HTMLInputElement, '32');

    await click(button('Retry'));
    expect(inference()).toHaveLength(2);
    expect(JSON.parse(inference()[1][1].body as string)).toEqual({ model: 'chat-default', messages: [{ role: 'user', content: 'Hi' }], max_tokens: 32 });

    // With Parameters closed, a setting that blocks Send opens it to say why.
    const body = container.querySelector('#playground-parameters-body') as HTMLElement;
    await click(button('Parameters'));
    expect(body.hidden).toBe(true);
    setValue(container.querySelector('#playground-max-tokens') as HTMLInputElement, '0');
    await say('Again');
    expect(inference()).toHaveLength(2);
    expect(body.hidden).toBe(false);
    expect(container.textContent).toContain('A whole number from 1 to 1000.');
  });

  it('retries only the comparison lane that failed', async () => {
    let refusePricey = true;
    const base = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === '/api/inference/v1/chat/completions/stream' && JSON.parse(String(init?.body)).model === 'pricey' && refusePricey) {
        refusePricey = false;
        return json({ code: 'provider_unavailable', detail: 'Try again.', status: 503 }, 503);
      }
      return base(url, init);
    });
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey();
    await click(button('Compare models'));
    await say('Greet me');
    const lanes = () => Array.from(container.querySelectorAll('.playground-lane'));
    expect(lanes()[0].textContent).toContain('Hello there');
    expect(lanes()[1].textContent).toContain('Try again.');
    expect(draft().value).toBe('');
    expect(lanes()[0].querySelector('button')?.textContent).not.toBe('Retry');

    await click(Array.from(lanes()[1].querySelectorAll('button')).find(candidate => candidate.textContent === 'Retry'));
    const calls = inference();
    expect(calls).toHaveLength(3);
    expect(JSON.parse(calls[2][1].body as string).model).toBe('pricey');
    expect(lanes()[1].textContent).toContain('Bonjour');
    // The model that answered was not asked again.
    expect(lanes()[0].querySelectorAll('.playground-turn-assistant')).toHaveLength(1);
  });

  it('drops a reply that New chat discarded, and keeps Stop working for the next request', async () => {
    const base = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    const hanging: { resolve: (response: Response) => void; signal: AbortSignal }[] = [];
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.startsWith('/api/inference/')) {
        return new Promise<Response>((resolve, reject) => {
          const signal = init!.signal!;
          signal.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')));
          hanging.push({ resolve, signal });
        });
      }
      return base(url, init);
    });
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey();
    await say('First');
    expect(button('Stop')).toBeTruthy();
    await click(button('New chat'));
    // The discarded prompt does not come back into the new chat.
    expect(draft().value).toBe('');
    expect(container.textContent).toContain('What do you want to chat about?');
    expect(hanging[0].signal.aborted).toBe(true);

    await say('Second');
    expect(hanging).toHaveLength(2);
    await click(button('Stop'));
    expect(hanging[1].signal.aborted).toBe(true);
    // Stopped before any reply: the message goes back to the composer.
    expect(draft().value).toBe('Second');
  });

  it('shows the code for the conversation, and never puts the key in it, even revealed', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey();
    setValue(draft(), 'What is "2+2"?');
    await click(button('Continue in code'));
    const dialog = container.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain('Call chat-default in code');
    expect((dialog.querySelector('#playground-endpoint') as HTMLInputElement).value).toBe(`${location.origin}/api/inference/v1/chat/completions/stream`);
    const keyField = dialog.querySelector('#playground-code-key') as HTMLInputElement;
    expect(keyField.type).toBe('password');

    const snippetText = () => (dialog.querySelector('.playground-snippet') as HTMLElement).textContent! + (dialog.querySelector('.playground-setup') as HTMLElement).textContent!;
    expect(snippetText()).toContain('from qucelerate_ai_gateway import');
    expect(snippetText()).toContain('GATEWAY_API_KEY');
    expect(snippetText()).toContain('2+2');
    expect(snippetText()).not.toContain('gw_pasted_secret');
    expect(dialog.querySelectorAll('.playground-snippet .code-line').length).toBeGreaterThan(5);
    expect(dialog.querySelector('.playground-snippet .tok-string')).toBeTruthy();

    await click(button('Show key'));
    expect(keyField.type).toBe('text');
    expect(keyField.value).toBe('gw_pasted_secret');
    expect(snippetText()).not.toContain('gw_pasted_secret');

    setValue(dialog.querySelector('select[aria-label="Client"]') as HTMLSelectElement, 'raw');
    expect(snippetText()).toContain('urllib.request');
    setValue(dialog.querySelector('select[aria-label="Language"]') as HTMLSelectElement, 'javascript');
    expect(dialog.textContent).toContain('There is no JavaScript client yet');
    expect((dialog.querySelector('select[aria-label="Client"]') as HTMLSelectElement).disabled).toBe(true);
    expect(snippetText()).toContain('await fetch(');
    for (const [language] of languages) {
      setValue(dialog.querySelector('select[aria-label="Language"]') as HTMLSelectElement, language);
      expect(snippetText()).not.toContain('gw_pasted_secret');
    }

    await click(button('Close'));
    expect(container.querySelector('[role="dialog"]')).toBeNull();
    // Closing hides the key again.
    await click(button('Continue in code'));
    expect((container.querySelector('#playground-code-key') as HTMLInputElement).type).toBe('password');
  });

  it('shows a placeholder for the key in the code dialog when the page holds none', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await click(button('Continue in code'));
    expect((container.querySelector('#playground-code-key') as HTMLInputElement).value).toBe('GATEWAY_API_KEY');
    expect(button('Show key')).toBeUndefined();
    expect(container.querySelector('.playground-snippet')!.textContent).toContain('What can you help me with?');
  });

  it('compares models: one request per alias, each with its own idempotency key, usage and cost', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey();
    await click(button('Compare models'));
    const lanes = () => Array.from(container.querySelectorAll('.playground-lane'));
    expect(lanes()).toHaveLength(2);
    expect((lanes()[1].querySelector('select') as HTMLSelectElement).value).toBe('pricey');

    await say('Greet me');
    const calls = inference();
    expect(calls.map(([, init]) => JSON.parse(init.body as string).model).sort()).toEqual(['chat-default', 'pricey']);
    const keys = calls.map(([, init]) => new Headers(init.headers).get('Idempotency-Key'));
    expect(keys[0]).toBeTruthy();
    expect(keys[1]).toBeTruthy();
    expect(keys[0]).not.toBe(keys[1]);

    expect(lanes()[0].textContent).toContain('Hello there');
    expect(lanes()[0].textContent).toContain('$0.07');
    expect(lanes()[1].textContent).toContain('Bonjour');
    // 3 input tokens at 50 cents each, 2 output at 2 cents: €1.54.
    expect(lanes()[1].textContent).toContain('€1.54');

    await click(button('Remove pricey from the comparison'));
    expect(lanes()).toHaveLength(1);
  });

  it('has no tabs: the session’s stats sit in the conversation’s corner, linking the alias’s usage and traces', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    expect(container.querySelector('[role="tablist"]')).toBeNull();
    const stats = container.querySelector('.playground-stage-bar .playground-stats')!;
    expect(stats.textContent).toContain('Replies0');
    const links = Array.from(stats.querySelectorAll('a')).map(link => link.getAttribute('href'));
    expect(links[0]).toContain('/admin/usage?');
    expect(links[0]).toContain('chat-default');
    expect(links[1]).toBe('/admin/traces?traces.q=chat-default');
  });

  it('points to Connect a provider when nothing is routed', async () => {
    fetchMock.mockImplementation(async () => json({ release_id: null, items: [], next_page_token: null }));
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    expect(container.textContent).toContain('No models yet');
  });

  it('embeds one text per line with an embedding alias, shows vectors and similarity, and offers its code', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    const picker = container.querySelector('#playground-model') as HTMLSelectElement;
    expect(Array.from(picker.querySelectorAll('optgroup')).map(group => group.label)).toEqual(['Chat', 'Embeddings']);
    await click(button('Compare models'));
    expect(container.querySelectorAll('.playground-lane').length).toBe(2);
    setValue(picker, 'embeddings:embed');
    // The alias's usage and traces stay a click away.
    expect(Array.from(container.querySelectorAll('.playground-stage-bar .playground-stats a')).map(link => link.getAttribute('href'))[1]).toBe('/admin/traces?traces.q=embed');
    // Embeddings mode: one pane, no chat parameters or comparison.
    expect(container.querySelector('#playground-draft')).toBeNull();
    expect(container.querySelector('#playground-parameters-body')).toBeNull();
    expect(button('Compare models')?.disabled).toBe(true);
    expect(container.querySelector('.playground-model-picker')?.textContent).toContain('amazon.titan-embed-text-v2:0');
    expect(container.textContent).toContain('What do you want to embed?');
    expect(container.textContent).toContain('Sending creates a temporary key');

    await usePastedKey();
    const input = container.querySelector('#playground-embed-input') as HTMLTextAreaElement;
    setValue(input, '  \n');
    await click(button('Embed'));
    expect(container.textContent).toContain('Write at least one line to embed.');
    expect(inference()).toHaveLength(0);

    setValue(input, 'first text\n\nsecond text\n');
    await click(button('Embed'));
    const [[url, init]] = inference();
    expect(url).toBe('/api/inference/v1/embeddings');
    expect(init.credentials).toBe('omit');
    expect(new Headers(init.headers).get('Authorization')).toBe('Bearer gw_pasted_secret');
    expect(new Headers(init.headers).get('Idempotency-Key')).toBeTruthy();
    expect(JSON.parse(init.body as string)).toEqual({ model: 'embed', input: ['first text', 'second text'] });
    const pane = container.querySelector('[aria-label="Embeddings"]')!;
    expect(pane.textContent).toContain('2 vectors of 5 dimensions');
    expect(pane.textContent).toContain('4 tokens');
    // Four input tokens at one cent each.
    expect(pane.textContent).toContain('$0.04');
    expect(pane.textContent).toContain('req-embed');
    expect(pane.textContent).toContain('[1.0000, 0.0000, 0.0000, 0.0000, …]');
    // cos([1,0,0,0,.5], [.6,.8,0,0,0]) = .6 / √1.25 ≈ 0.537.
    const similarity = Array.from(pane.querySelectorAll('.playground-similarity td')).map(cell => cell.textContent);
    expect(similarity).toEqual(['1.000', '0.537', '0.537', '1.000']);

    await click(button('Continue in code'));
    const dialog = document.querySelector('.playground-code-dialog')!;
    expect((dialog.querySelector('#playground-endpoint') as HTMLInputElement).value).toBe(`${location.origin}/api/inference/v1/embeddings`);
    const languagesShown = Array.from(dialog.querySelectorAll('select[aria-label="Language"] option')).map(option => option.textContent);
    expect(languagesShown).toEqual(['curl', 'Python', 'JavaScript']);
    expect(dialog.textContent).toContain('client.embeddings("embed", inputs)');
    expect(dialog.textContent).toContain('"second text"');
  });

  it('uses an alias routed for chat and embeddings for either, and starts afresh on a new model', async () => {
    routes.push({ ...routes[3], id: 'r6', alias: 'chat-default' });
    try {
      act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
      await settle();
      const picker = container.querySelector('#playground-model') as HTMLSelectElement;
      expect(Array.from(picker.options).map(option => option.value))
        .toEqual(['chat:chat-default', 'chat:pricey', 'embeddings:chat-default', 'embeddings:embed']);
      expect(picker.value).toBe('chat:chat-default');
      expect(container.querySelector('#playground-draft')).not.toBeNull();

      setValue(picker, 'embeddings:embed');
      await usePastedKey();
      setValue(container.querySelector('#playground-embed-input') as HTMLTextAreaElement, 'first text');
      await click(button('Embed'));
      expect(container.querySelector('[aria-label="Embeddings"]')?.textContent).toContain('embed: 1 vector');

      setValue(picker, 'embeddings:chat-default');
      expect(picker.value).toBe('embeddings:chat-default');
      expect(container.querySelector('#playground-draft')).toBeNull();
      // The other model's vectors are gone.
      expect(container.querySelector('[aria-label="Embeddings"]')?.textContent).not.toContain('vector');

      setValue(picker, 'chat:chat-default');
      expect(container.querySelector('#playground-draft')).not.toBeNull();
      expect(container.querySelector('#playground-embed-input')).toBeNull();
    } finally {
      routes.pop();
    }
  });

  it('says a refused embeddings key needs the Embeddings scope', async () => {
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    setValue(container.querySelector('#playground-model') as HTMLSelectElement, 'embeddings:embed');
    await usePastedKey();
    const original = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => (url === '/api/inference/v1/embeddings'
      ? json({ code: 'forbidden', detail: 'Missing scope.', status: 403 }, 403)
      : original(url, init)));
    setValue(container.querySelector('#playground-embed-input') as HTMLTextAreaElement, 'hello');
    await click(button('Embed'));
    expect(container.textContent).toContain('It needs the Embeddings scope');
  });

  it('defines a tool, shows the call a reply makes, and continues with its result', async () => {
    const fallback = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    let continued = 0;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url !== '/api/inference/v1/chat/completions/stream') return fallback(url, init);
      const body = JSON.parse(String(init?.body)) as { messages: { role: string }[] };
      if (body.messages[body.messages.length - 1].role === 'tool') {
        // The first try fails; the conversation keeps the result to retry.
        continued += 1;
        if (continued === 1) return json({ code: 'provider_unavailable', detail: 'Try again.', status: 503 }, 503);
        return sse(
          ['response.started', { model: 'chat-default' }],
          ['choice.delta', { choice_index: 0, content_delta: 'It is 21 degrees.' }],
          ['choice.finished', { choice_index: 0, finish_reason: 'stop' }],
          ['response.completed', { model: 'chat-default', usage }],
        );
      }
      return sse(
        ['response.started', { model: 'chat-default' }],
        ['tool_call.delta', { choice_index: 0, tool_call_index: 0, id: 'call_1', name: 'get_weather', arguments_delta: '{"city":' }],
        ['tool_call.delta', { choice_index: 0, tool_call_index: 0, id: null, name: null, arguments_delta: '"Paris"}' }],
        ['choice.finished', { choice_index: 0, finish_reason: 'tool_calls' }],
        ['response.completed', { model: 'chat-default', usage }],
      );
    });
    act(() => root.render(<PlaygroundScreen organizationId="org-1" userId="user-1" />));
    await settle();
    await usePastedKey();
    expect((container.querySelector('#playground-tools-body') as HTMLElement).hidden).toBe(true);
    await click(button('Tools'));
    await click(button('Add tool'));
    const name = container.querySelector('input[id^="playground-tool-"][id$="-name"]') as HTMLInputElement;
    expect(name.value).toBe('get_weather');
    expect(container.querySelector('#playground-tools-heading')?.parentElement?.textContent).toContain('1 tool');

    await say('Weather in Paris?');
    const first = JSON.parse(inference()[0][1].body as string);
    expect(first.tools).toEqual([{
      type: 'function',
      function: { name: 'get_weather', description: 'The current weather in a city.', parameters: { type: 'object', properties: { city: { type: 'string', description: 'The city, such as Paris.' } }, required: ['city'] } },
    }]);
    expect(first.tool_choice).toBeUndefined();
    expect(first.parallel_tool_calls).toBeUndefined();
    const lane = container.querySelector('.playground-reply')!;
    expect(lane.querySelector('.playground-tool-calls')?.textContent).toContain('get_weather');
    expect(lane.querySelector('.playground-tool-arguments')?.textContent).toBe('{\n  "city": "Paris"\n}');
    expect(lane.textContent).toContain('tool_calls');

    // The next message waits for the result.
    await say('And tomorrow?');
    expect(inference()).toHaveLength(1);
    expect(container.textContent).toContain('Enter the tool calls’ results first');

    setValue(lane.querySelector('.playground-tool-results textarea') as HTMLTextAreaElement, '{"temperature":21}');
    await click(button('Send result'));
    expect(inference()).toHaveLength(2);
    expect(lane.textContent).toContain('Try again.');
    expect(lane.querySelector('.playground-tool-results')).toBeNull();
    await click(button('Retry'));
    expect(inference()).toHaveLength(3);
    expect(inference()[2][1].body).toBe(inference()[1][1].body);
    expect(JSON.parse(inference()[2][1].body as string).messages).toEqual([
      { role: 'user', content: 'Weather in Paris?' },
      { role: 'assistant', content: null, tool_calls: [{ id: 'call_1', type: 'function', function: { name: 'get_weather', arguments: '{"city":"Paris"}' } }] },
      { role: 'tool', content: '{"temperature":21}', tool_call_id: 'call_1' },
    ]);
    const turns = Array.from(lane.querySelectorAll('.playground-turn .playground-output')).map(node => node.textContent);
    expect(turns).toEqual(['Weather in Paris?', '{"temperature":21}', 'It is 21 degrees.']);
    expect(lane.querySelector('.playground-tool-results')).toBeNull();

    // The code sends the same conversation, its tools included.
    await click(button('Continue in code'));
    const dialog = container.querySelector('[role="dialog"]')!;
    expect(dialog.querySelector('.playground-snippet')?.textContent).toContain('get_weather');
    expect(dialog.querySelector('.playground-snippet')?.textContent).toContain('call_1');
  });
});
