import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ProvidersScreen } from './providers-screen';
import { cardModelOf, cardPath } from './model-card';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const CATALOG = '/api/admin/v1/organizations/org-1/catalog';
const MODEL = 'us.amazon.nova-2-lite-v1:0';
const CARD_URL = `${CATALOG}/models/us.amazon.nova-2-lite-v1%3A0/card`;
const price = (input: number, billed_by = 'release') => ({
  currency: 'USD', input_minor_per_token: input, output_minor_per_token: 400, cached_input_minor_per_token: 0,
  reasoning_minor_per_token: 0, other_minor_per_token: 0, billed_by, source: billed_by === 'release' ? 'aws-price-list' : null,
  source_version: billed_by === 'release' ? '2026-11-01' : 'override:o-1', confidence: 'known',
});
const entry = (version: string, changes: Partial<{ added: string[]; changed: string[]; removed: string[] }>) => ({
  release_id: `r-${version}`, release_version: version, created_at: '2026-11-01T12:00:00Z',
  actor: { actor_type: 'user', id: 'u-1', name: 'Ada Admin' }, added: [], changed: [], removed: [], ...changes,
});
const fullCard = {
  schema: 'qucelerate.model-card.v1',
  model: MODEL,
  generated_at: '2026-11-13T09:00:00Z',
  release_id: 'r-2',
  overview: { display_name: 'Nova 2 Lite', provider_families: ['azure-ai-foundry', 'bedrock'], operations: ['chat', 'chat_stream'], locations: ['eastus2', 'us-east-1'], max_input_bytes: 1_048_576, max_output_tokens: 8192 },
  public_card: { status: 'not_on_file', data_handling: [] },
  deployments: [
    {
      deployment: 'bedrock-nova', adapter_instance_key: 'bedrock-main', provider_family: 'bedrock', location: 'us-east-1',
      routes: [{ alias: 'nova', operation: 'chat' }, { alias: 'nova', operation: 'chat_stream' }],
      operations: [{ operation: 'chat', max_input_bytes: 1_048_576, max_output_tokens: 8192, tools: false, price: price(30) }],
      availability: { state: 'verified', reason: null, source: 'traffic', observed_at: '2026-11-12T00:00:00Z', verified_at: '2026-11-12T00:00:00Z' },
      provider_listing: null,
    },
    {
      deployment: 'foundry-nova', adapter_instance_key: 'foundry-main', provider_family: 'azure-ai-foundry', location: 'eastus2',
      routes: [], operations: [{ operation: 'chat', max_input_bytes: 4096, max_output_tokens: 32, tools: false, price: price(50, 'override') }, { operation: 'embeddings', max_input_bytes: 4096, max_output_tokens: 0, tools: false, price: null }],
      availability: null,
      provider_listing: { format: 'Anthropic', model: 'claude-opus-5-5', version: '1', sku: 'GlobalStandard', lifecycle_status: 'Deprecating', retires_at: '2027-03-01T00:00:00Z', listed_at: '2026-11-12T00:00:00Z', content_filter: 'strict', content_filter_policy: { name: 'strict', base_policy: 'Microsoft.DefaultV2', mode: 'Default', filters: [{ name: 'Hate', source: 'Prompt', enabled: true, blocking: true, severity_threshold: 'Low' }] } },
    },
  ],
  usage: {
    windows: [7, 30, 90].map(days => ({
      days, from: '2026-08-15T09:00:00Z', to: '2026-11-13T09:00:00Z',
      totals: days === 7 ? [] : [{ currency: 'USD', confidence: 'exact', request_count: days * 10, amount_minor: 1_234_000_000, unpriced_request_count: 0, incomplete_request_count: 0, tokens: { input: 5000, output: 2500, cached_input: 1250, reasoning: 0, other: 0 } }],
    })),
    requests: {
      days: 30, from: '2026-10-14T09:00:00Z', to: '2026-11-13T09:00:00Z', request_count: 300, failed_count: 4,
      errors: [{ key: 'rate_limited', count: 3 }, { key: 'timeout', count: 1 }], timed_since: '2026-11-10T00:00:00Z', timed_request_count: 120,
      latency_p50_ms: 850, latency_p95_ms: 2400, first_event_p50_ms: 210, first_event_p95_ms: 600,
      finish_reasons: [{ reason: 'stop', count: 110, safety: false }, { reason: 'content_filter', count: 7, safety: true }, { reason: 'refusal', count: 3, safety: true }],
    },
  },
  governance: { first_release: entry('v1', { added: ['bedrock-nova'] }), history: [entry('v2', { added: ['foundry-nova'], changed: ['bedrock-nova'] }), entry('v1', { added: ['bedrock-nova'] })], truncated: true },
  organization_note: null,
  organization_evaluations: [{ set_id: 's-1', set_name: 'Support answers', run_id: 'r-1', batch_id: 'b-1', scored_at: '2026-11-16T00:00:00Z', items: 10, answered: 9, correct: 7 }],
  omitted: [],
};

describe('model card', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let card: unknown;
  let cardStatus: number;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    card = fullCard;
    cardStatus = 200;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url === CARD_URL) return json(card, cardStatus);
      if (url === `${CARD_URL}/note` && init?.method === 'PUT') {
        const { note } = JSON.parse(String(init.body)) as { note: string };
        return json({ note: note.trim(), version: 1, updated_at: '2026-11-17T00:00:00Z', updated_by: { actor_type: 'user', id: 'u-1', name: 'Ada Admin' } }, 201);
      }
      if (url === `${CARD_URL}/import` && !init?.method) return json({ type: 'about:blank', code: 'not_found', title: 'Not found', status: 404, detail: '', instance: '', request_id: 'r' }, 404);
      if (url === `${CARD_URL}/import` && init?.method === 'PUT') return json({ model: MODEL, repository: 'openai/gpt-oss-x', provider_repository: true, version: 1, imported_at: '2026-11-17T00:00:00Z', imported_by: null, card: {} }, 201);
      if (url === `${CATALOG}/model-cards`) {
        return json({ release_id: 'r-2', items: [{
          model: MODEL, provider_families: ['bedrock'], card: { id: 'amazon/nova-2-lite', name: 'Nova 2 Lite', vendor: 'Amazon', origin: 'builtin' },
          context_window_tokens: { value: 1_000_000, source_url: 'https://docs.aws.amazon.com/x', retrieved_at: '2026-11-17', reported_by: 'provider' },
          max_output_tokens: null,
          retirement: { value: 'No sooner than December 2, 2026', source_url: 'https://docs.aws.amazon.com/x', retrieved_at: '2026-11-17', reported_by: 'provider' },
          retires_at: null,
        }] });
      }
      if (url.startsWith(`${CATALOG}/deployments`)) {
        return json({
          items: [{ id: 'd-1', provider_account_id: 'a-1', adapter_instance_key: 'bedrock-main', family: 'bedrock', name: 'bedrock-nova', model: MODEL, location: 'us-east-1', operations: [] }],
          next_page_token: null, release_id: 'r-2',
        });
      }
      return json({ items: [], next_page_token: null, release_id: 'r-2' });
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    history.replaceState(null, '', '/');
  });

  const render = async (path: string) => {
    history.replaceState(null, '', path);
    await act(async () => {
      root.render(<ProvidersScreen organizationId="org-1" section="models" role="organization_owner" onSignIn={() => undefined} signInError="" signInPending={false} />);
    });
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 0)); });
  };

  it('keeps the model identifier in the path, encoded', () => {
    expect(cardPath(MODEL)).toBe('/admin/models/card/us.amazon.nova-2-lite-v1%3A0');
    expect(cardModelOf(cardPath('publishers/google/models/gemini-4'))).toBe('publishers/google/models/gemini-4');
    expect(cardModelOf('/admin/models/aliases')).toBeNull();
    expect(cardModelOf('/admin/connections/card/x')).toBeNull();
    expect(cardModelOf('/admin/models/card/%E0%A4%A')).toBeNull();
  });

  it('opens from a Models row', async () => {
    await render('/admin/models');
    const link = container.querySelector<HTMLAnchorElement>(`a[aria-label="Model card for ${MODEL}"]`);
    expect(link?.getAttribute('href')).toBe(cardPath(MODEL));
    await act(async () => { link!.click(); });
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 0)); });
    expect(location.pathname).toBe(cardPath(MODEL));
    expect(fetchMock).toHaveBeenCalledWith(CARD_URL, expect.anything());
    expect(container.querySelector('h2')?.textContent).toBe('Nova 2 Lite');
  });

  it('shows every section the caller can read', async () => {
    await render(cardPath(MODEL));
    const text = container.textContent ?? '';
    expect(text).toContain('No public card on file');
    expect(text).toContain('Azure AI Foundry, Amazon Bedrock');
    expect(text).toContain('Chat, Streaming');
    expect(text).toContain('1.0 MiB');
    // Deployments: aliases once each, prices per million, override and evidence.
    const rows = [...container.querySelectorAll('table[aria-label="Deployments serving this model"] tbody tr')];
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain('nova');
    expect(rows[0].textContent).not.toContain('nova, nova');
    expect(rows[0].textContent).toContain('$0.30 in / $4.00 out');
    expect(rows[0].textContent).toContain('Verified');
    expect(rows[1].textContent).toContain('Override');
    expect(rows[1].textContent).toContain('Not yet checked');
    expect(rows[1].textContent).toContain('Embeddings: not priced, so not routed');
    // Usage windows, including one with no usage.
    const usage = [...container.querySelectorAll('table[aria-label="Usage by window"] tbody tr')].map(row => row.textContent);
    expect(usage).toHaveLength(3);
    expect(usage[0]).toContain('Last 7 days0');
    expect(usage[1]).toContain('$12.34');
    expect(usage[1]).toContain('25%');
    expect(text).toContain('rate limited 3, timeout 1');
    expect(text).toContain('850 ms median, 2,400 ms p95');
    expect(text).toContain('210 ms median');
    expect(text).toMatch(/Stopped by safety\s*10/);
    expect(text).toContain('120 requests since');
    // Governance, newest first, with the audit log for older changes.
    const history = [...container.querySelectorAll('ol[aria-label="Catalog changes to this model"] li')].map(item => item.textContent);
    expect(history[0]).toContain('release v2 by Ada Admin added foundry-nova. changed bedrock-nova.');
    expect(text).toContain('First served by release v1');
    expect(container.querySelector('a[href="/admin/audit"]')).not.toBeNull();
  });

  it('shows a card file\'s facts with their sources, and data handling', async () => {
    const fact = (value: unknown, reported_by = 'provider') => ({ value, source_url: 'https://huggingface.co/openai/gpt-oss-120b', retrieved_at: '2026-11-14', reported_by });
    card = {
      ...fullCard,
      public_card: {
        status: 'on_file',
        card: {
          id: 'openai/gpt-oss-120b', name: 'gpt-oss-120b', vendor: 'OpenAI', origin: 'builtin', matched_deployments: ['bedrock-nova'],
          overview: { license: fact('apache-2.0'), open_weight: fact(true), context_window_tokens: fact(131072), parameters: fact('116.8B', 'independent') },
          evaluations: [{ name: 'MMLU', score: '90.0', evaluator: 'OpenAI', source_url: 'https://arxiv.org/abs/2508.10925', retrieved_at: '2026-11-14', reported_by: 'provider' }],
          safety: { model_card_url: { ...fact('https://arxiv.org/abs/2508.10925'), title: 'gpt-oss model card' } },
        },
        data_handling: [{ provider_family: 'bedrock', name: 'Amazon Bedrock', origin: 'builtin', deployments: ['bedrock-nova'], facts: { training_use: { value: 'Not used to improve base models.', source_url: 'https://aws.amazon.com/bedrock/faqs/', retrieved_at: '2026-11-14', reported_by: 'provider' } } }],
      },
      gateway_data_handling: { trace_mode: 'organization', trace_content: true, trace_content_retention_days: 30 },
    };
    await render(cardPath(MODEL));
    const text = container.textContent ?? '';
    expect(text).not.toContain('No public card on file');
    expect(text).toContain('openai/gpt-oss-120b');
    const facts = container.querySelector('dl[aria-label="Published facts"]')?.textContent ?? '';
    expect(facts).toContain('Context window (tokens)131,072');
    expect(facts).toContain('Open weightsYes');
    expect(facts).toContain('huggingface.co, read 2026-11-14, self-reported');
    expect(facts).toContain('116.8B (huggingface.co, read 2026-11-14, independent)');
    expect(container.querySelector('table[aria-label="Self-reported evaluations"]')?.textContent).toContain('MMLU90.0');
    expect(container.querySelector('table[aria-label="Independent evaluations"]')).toBeNull();
    expect(container.querySelector('dl[aria-label="Safety"] a')?.textContent).toBe('gpt-oss model card');
    expect(container.querySelector('dl[aria-label="Amazon Bedrock data handling"]')?.textContent).toContain('Training useNot used to improve base models.');
    expect(container.textContent).toContain('Covers bedrock-nova.');
    expect(text).toContain('Tracing is on for the whole organization, and the policy keeps traced content for up to 30 days. Content is stored only when the gateway has a trace store, and the retention policy can delete it sooner.');
  });

  it('says which sections the role cannot read', async () => {
    card = { ...fullCard, usage: undefined, governance: undefined, omitted: [{ section: 'usage', requires: 'usage_read' }, { section: 'governance', requires: 'audit_read' }] };
    await render(cardPath(MODEL));
    const text = container.textContent ?? '';
    expect(text).toContain('Usage is not shown: your role cannot read it.');
    expect(text).toContain('Catalog history is not shown');
    expect(text).toContain('No provider data-handling terms are on file');
    expect(text).toContain("This gateway's trace settings is not shown");
    expect(container.querySelector('table[aria-label="Usage by window"]')).toBeNull();
  });

  it('shows the card file context window and retirement in the Models table', async () => {
    await render('/admin/models');
    const text = container.textContent ?? '';
    expect(text).toContain('Context window');
    expect(text).toContain('1,000,000');
    expect(text).toContain('No sooner than December 2, 2026');
  });

  it('shows batch prices without output for batch embeddings, and listed retirement', async () => {
    card = {
      ...fullCard,
      deployments: [{
        ...fullCard.deployments[1],
        operations: [
          { operation: 'chat_batch', max_input_bytes: 4096, max_output_tokens: 32, tools: false, price: price(15) },
          { operation: 'embeddings_batch', max_input_bytes: 4096, max_output_tokens: 0, tools: false, price: price(1) },
        ],
      }],
    };
    await render(cardPath(MODEL));
    const row = container.querySelector('table[aria-label="Deployments serving this model"] tbody tr')!;
    expect(row.textContent).toContain('Batch chat: $0.15 in / $4.00 out');
    expect(row.textContent).toContain('Batch embeddings: $0.01 in');
    expect(row.textContent).not.toContain('$0.01 in / ');
    expect(row.textContent).toContain('Deprecating');
  });

  it('shows the organization evaluations as its own', async () => {
    await render(cardPath(MODEL));
    const table = container.querySelector('table[aria-label="Organization evaluations of this model"]');
    expect(table?.textContent).toContain('Support answers');
    expect(table?.textContent).toContain('7 of 10 (70%)');
    expect(container.textContent).toContain('not a benchmark');
  });

  it('shows each deployment content filter policy', async () => {
    await render(cardPath(MODEL));
    const filters = container.querySelector('table[aria-label="Content filters of foundry-nova"]');
    expect(filters?.textContent).toContain('Hate');
    expect(filters?.textContent).toContain('Low severity');
    expect(filters?.textContent).toContain('Blocks');
    expect(container.textContent).toContain('Based on Microsoft.DefaultV2.');
    expect(container.textContent).toContain('The other deployments (bedrock-nova) report no content filter policy.');
  });

  it('adds and shows the organization note', async () => {
    await render(cardPath(MODEL));
    expect(container.textContent).toContain('No note.');
    const add = [...container.querySelectorAll('button')].find(button => button.textContent === 'Add note')!;
    await act(async () => { add.click(); });
    const area = container.querySelector<HTMLTextAreaElement>('#model-card-note')!;
    await act(async () => {
      const setter = Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!;
      setter.call(area, 'Approved for internal tools.');
      area.dispatchEvent(new Event('input', { bubbles: true }));
    });
    const save = [...container.querySelectorAll('button')].find(button => button.textContent === 'Save note')!;
    await act(async () => { save.click(); await new Promise(resolve => setTimeout(resolve, 0)); });
    const put = fetchMock.mock.calls.find(([url, init]) => url === `${CARD_URL}/note` && (init as RequestInit)?.method === 'PUT');
    expect(put).toBeTruthy();
    // None ever set: only while none is, so two first notes cannot overwrite each other.
    expect(new Headers((put![1] as RequestInit).headers).get('If-Match')).toBe('"v0"');
    expect(container.textContent).toContain('Approved for internal tools.');
    expect(container.textContent).toContain('by Ada Admin');
  });

  const typeNote = async (text: string) => {
    const add = [...container.querySelectorAll('button')].find(button => button.textContent === 'Add note')!;
    await act(async () => { add.click(); });
    const area = container.querySelector<HTMLTextAreaElement>('#model-card-note')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value')!.set!.call(area, text);
      area.dispatchEvent(new Event('input', { bubbles: true }));
    });
    return [...container.querySelectorAll('button')].find(button => button.textContent === 'Save note')!;
  };

  it('shows a cleared note as none, and sets it again against its version', async () => {
    card = { ...fullCard, organization_note: { note: '', version: 3, updated_at: '2026-11-16T00:00:00Z', updated_by: null } };
    await render(cardPath(MODEL));
    expect(container.textContent).toContain('No note.');
    const save = await typeNote('Back in use.');
    await act(async () => { save.click(); await new Promise(resolve => setTimeout(resolve, 0)); });
    const put = fetchMock.mock.calls.find(([url, init]) => url === `${CARD_URL}/note` && (init as RequestInit)?.method === 'PUT');
    expect(new Headers((put![1] as RequestInit).headers).get('If-Match')).toBe('"v3"');
  });

  it('ignores a note save that finishes after another card opened', async () => {
    const OTHER = 'gemini-4';
    let finishSave: (response: Response) => void = () => undefined;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === CARD_URL) return json(fullCard);
      if (url === `${CARD_URL}/note` && init?.method === 'PUT') return new Promise<Response>(resolve => { finishSave = resolve; });
      if (url === `${CATALOG}/models/${OTHER}/card`) return json({ ...fullCard, model: OTHER, overview: { ...fullCard.overview, display_name: 'Gemini 4' } });
      return json({ items: [], next_page_token: null, release_id: 'r-2' });
    });
    await render(cardPath(MODEL));
    const save = await typeNote('Only for Nova.');
    await act(async () => { save.click(); });
    await act(async () => {
      history.pushState(null, '', cardPath(OTHER));
      dispatchEvent(new PopStateEvent('popstate'));
    });
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 0)); });
    await act(async () => {
      finishSave(json({ note: 'Only for Nova.', version: 1, updated_at: '2026-11-17T00:00:00Z', updated_by: null }, 201));
      await new Promise(resolve => setTimeout(resolve, 0));
    });
    expect(container.querySelector('h2')?.textContent).toBe('Gemini 4');
    expect(container.textContent).not.toContain('Only for Nova.');
  });

  it('retries an import with the same key and If-Match, without reading again', async () => {
    let puts = 0;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === CARD_URL) return json(fullCard);
      if (url === `${CARD_URL}/import` && !init?.method) return json({ model: MODEL, repository: 'openai/old', provider_repository: true, version: 2, imported_at: '2026-11-16T00:00:00Z', imported_by: null, card: {} });
      if (url === `${CARD_URL}/import` && init?.method === 'PUT') {
        puts += 1;
        if (puts === 1) return json({ type: 'about:blank', code: 'unavailable', title: 'Unavailable', status: 503, detail: 'try again', instance: '', request_id: 'r' }, 503);
        return json({ model: MODEL, repository: 'openai/gpt-oss-x', provider_repository: true, version: 3, imported_at: '2026-11-17T00:00:00Z', imported_by: null, card: {} });
      }
      return json({ items: [], next_page_token: null, release_id: 'r-2' });
    });
    card = fullCard;
    await render(cardPath(MODEL));
    const open = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import from Hugging Face')!;
    await act(async () => { open.click(); });
    const input = container.querySelector<HTMLInputElement>('#hub-repository')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'openai/gpt-oss-x');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    for (let i = 0; i < 2; i += 1) {
      const submit = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import')!;
      await act(async () => { submit.click(); await new Promise(resolve => setTimeout(resolve, 0)); });
    }
    const calls = fetchMock.mock.calls.filter(([url]) => url === `${CARD_URL}/import`);
    const reads = calls.filter(([, init]) => !(init as RequestInit | undefined)?.method);
    const writes = calls.filter(([, init]) => (init as RequestInit | undefined)?.method === 'PUT').map(([, init]) => new Headers((init as RequestInit).headers));
    expect(reads).toHaveLength(1);
    expect(writes).toHaveLength(2);
    expect(writes.map(headers => headers.get('If-Match'))).toEqual(['"v2"', '"v2"']);
    expect(writes[0].get('Idempotency-Key')).toBe(writes[1].get('Idempotency-Key'));
  });

  it('reads the import version again after a conflict', async () => {
    let puts = 0;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === CARD_URL) return json(fullCard);
      if (url === `${CARD_URL}/import` && !init?.method) return json({ model: MODEL, repository: 'openai/old', provider_repository: true, version: 2 + puts, imported_at: '2026-11-16T00:00:00Z', imported_by: null, card: {} });
      if (url === `${CARD_URL}/import` && init?.method === 'PUT') {
        puts += 1;
        if (puts === 1) return json({ type: 'about:blank', code: 'version_conflict', title: 'Conflict', status: 409, detail: 'changed', instance: '', request_id: 'r' }, 409);
        return json({ model: MODEL, repository: 'openai/gpt-oss-x', provider_repository: true, version: 4, imported_at: '2026-11-17T00:00:00Z', imported_by: null, card: {} });
      }
      return json({ items: [], next_page_token: null, release_id: 'r-2' });
    });
    await render(cardPath(MODEL));
    const open = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import from Hugging Face')!;
    await act(async () => { open.click(); });
    const input = container.querySelector<HTMLInputElement>('#hub-repository')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'openai/gpt-oss-x');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    for (let i = 0; i < 2; i += 1) {
      const submit = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import')!;
      await act(async () => { submit.click(); await new Promise(resolve => setTimeout(resolve, 0)); });
    }
    const writes = fetchMock.mock.calls
      .filter(([url, init]) => url === `${CARD_URL}/import` && (init as RequestInit | undefined)?.method === 'PUT')
      .map(([, init]) => new Headers((init as RequestInit).headers));
    expect(writes.map(headers => headers.get('If-Match'))).toEqual(['"v2"', '"v3"']);
    expect(writes[0].get('Idempotency-Key')).not.toBe(writes[1].get('Idempotency-Key'));
  });

  it('keeps the server explanation when imports are off', async () => {
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url === CARD_URL) return json(fullCard);
      if (url === `${CARD_URL}/import` && !init?.method) return json({ type: 'about:blank', code: 'not_found', title: 'Not found', status: 404, detail: '', instance: '', request_id: 'r' }, 404);
      if (url === `${CARD_URL}/import` && init?.method === 'PUT') return json({ type: 'about:blank', code: 'import_disabled', title: 'Conflict', status: 409, detail: 'Hugging Face import is off: set GATEWAY_MODEL_CARD_HUB_URL.', instance: '', request_id: 'r' }, 409);
      return json({ items: [], next_page_token: null, release_id: 'r-2' });
    });
    await render(cardPath(MODEL));
    const open = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import from Hugging Face')!;
    await act(async () => { open.click(); });
    const input = container.querySelector<HTMLInputElement>('#hub-repository')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'openai/gpt-oss-x');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    const submit = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import')!;
    await act(async () => { submit.click(); await new Promise(resolve => setTimeout(resolve, 0)); });
    expect(container.textContent).toContain('Hugging Face import is off');
    expect(container.textContent).not.toContain('Someone changed');
  });

  it('imports a Hugging Face card for a model without one', async () => {
    await render(cardPath(MODEL));
    const open = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import from Hugging Face')!;
    await act(async () => { open.click(); });
    const input = container.querySelector<HTMLInputElement>('#hub-repository')!;
    await act(async () => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, 'openai/gpt-oss-x');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    const submit = [...container.querySelectorAll('button')].find(button => button.textContent === 'Import')!;
    await act(async () => { submit.click(); await new Promise(resolve => setTimeout(resolve, 0)); });
    const put = fetchMock.mock.calls.find(([url, init]) => url === `${CARD_URL}/import` && (init as RequestInit)?.method === 'PUT');
    expect(JSON.parse(String((put![1] as RequestInit).body))).toEqual({ repository: 'openai/gpt-oss-x', provider_repository: true });
    expect(new Headers((put![1] as RequestInit).headers).get('If-Match')).toBe('*');
  });

  it('explains a model the active release does not serve', async () => {
    card = { type: 'about:blank', code: 'not_found', title: 'Not found', status: 404, detail: 'the active catalog release does not serve this model', instance: '', request_id: 'req-1' };
    cardStatus = 404;
    await render(cardPath(MODEL));
    expect(container.textContent).toContain(`The active catalog release does not serve ${MODEL}.`);
    expect(container.querySelector('a[href="/admin/models"]')).not.toBeNull();
  });

  it('reports a refused read as a permission problem', async () => {
    card = { type: 'about:blank', code: 'forbidden', title: 'Forbidden', status: 403, detail: '', instance: '', request_id: 'req-2' };
    cardStatus = 403;
    await render(cardPath(MODEL));
    expect(container.textContent).toContain('You do not have permission to view this organization’s model card.');
  });

  it('shows only the card of the model the path names now', async () => {
    const OTHER = 'gemini-4';
    let releaseFirst: (response: Response) => void = () => undefined;
    fetchMock.mockImplementation(async (url: string) => {
      if (url === CARD_URL) return new Promise<Response>(resolve => { releaseFirst = resolve; });
      if (url === `${CATALOG}/models/${OTHER}/card`) return json({ ...fullCard, model: OTHER, overview: { ...fullCard.overview, display_name: 'Gemini 4' } });
      return json({ items: [], next_page_token: null, release_id: 'r-2' });
    });
    await render(cardPath(MODEL));
    await act(async () => {
      history.pushState(null, '', cardPath(OTHER));
      dispatchEvent(new PopStateEvent('popstate'));
    });
    await act(async () => { await new Promise(resolve => setTimeout(resolve, 0)); });
    expect(container.querySelector('h2')?.textContent).toBe('Gemini 4');
    // The first model's card arrives last and is ignored.
    await act(async () => { releaseFirst(json(fullCard)); await new Promise(resolve => setTimeout(resolve, 0)); });
    expect(container.querySelector('h2')?.textContent).toBe('Gemini 4');
  });
});
