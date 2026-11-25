import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { EditDeploymentDialog } from './catalog-actions';

const ORG = '64bd1ec5-dbcc-4bbc-b0f4-9dcf3e1581f2';
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const capability = (operation: string) => ({
  operation, max_input_bytes: 4096, max_output_tokens: 32, max_tokens_per_byte: 1, tokenizer: 'configured',
  reservation_ttl_seconds: 17, chat_input_count_mode: 'unsupported',
});
const price = (operation: string) => ({
  deployment: 'gpt', operation, input_minor_per_token: 250, output_minor_per_token: 1000, cached_input_minor_per_token: 125,
  reasoning_minor_per_token: 0, other_minor_per_token: 0, currency: 'USD', source: 'test', source_version: 'v1', confidence: 'known',
});
const manifest = {
  release: { version: 'v1' },
  providers: [{ adapter_instance_key: 'openai-main', family: 'openai' }],
  deployments: [{ name: 'gpt', model: 'gpt-4o', location: null, adapter_instance_key: 'openai-main', capabilities: [capability('chat'), capability('chat_batch'), capability('embeddings')] }],
  prices: [price('chat'), price('chat_batch'), { ...price('embeddings'), output_minor_per_token: 0 }],
  routes: [{ alias: 'smart', operation: 'chat', deployment: 'gpt' }],
};

describe('edit a deployment', () => {
  let container: HTMLDivElement;
  let root: Root;
  let posted: { deployments: { capabilities: { operation: string; tools?: boolean }[] }[] }[];

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    posted = [];
    vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/catalog/releases/rel-1/manifest')) return json(manifest);
      if (url.endsWith('/activate')) return json({ release: { id: 'rel-2' }, activation_version: 4 });
      if (url.endsWith('/catalog/releases') && init?.method === 'POST') {
        const body = JSON.parse(String(init.body));
        posted.push(body);
        return json({ id: 'rel-2', version: body.release.version, digest: 'd', created_at: '', sealed_at: '', active: false, activated_at: null }, 201);
      }
      if (url.includes('/catalog/releases?')) return json({ items: [{ id: 'rel-1', version: 'v1', digest: 'd', created_at: '', sealed_at: '', active: true, activated_at: '' }], next_page_token: null, activation_version: 3 });
      return json({}, 404);
    }));
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };

  it('offers tool calling for chat batches as well as chat, and saves it', async () => {
    const onChanged = vi.fn();
    act(() => root.render(<EditDeploymentDialog organizationId={ORG} releaseId="rel-1" name="gpt" model="gpt-4o" onClose={() => {}} onChanged={onChanged} />));
    await settle();
    expect(document.getElementById('edit-chat-tools')).not.toBeNull();
    expect(document.getElementById('edit-embeddings-tools')).toBeNull();
    const batch = document.getElementById('edit-chat_batch-tools') as HTMLInputElement;
    expect(batch.checked).toBe(false);
    act(() => batch.click());
    act(() => (batch.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(posted).toHaveLength(1);
    expect(posted[0].deployments[0].capabilities.map(item => [item.operation, item.tools === true]))
      .toEqual([['chat', false], ['chat_batch', true], ['embeddings', false]]);
    expect(onChanged).toHaveBeenCalled();
  });
});
