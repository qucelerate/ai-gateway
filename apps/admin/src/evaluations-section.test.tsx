import { StrictMode } from 'react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ProvidersScreen } from './providers-screen';
import { itemsProblem, parseItems, percent, setInput } from './evaluations-section';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string) =>
  new Response(JSON.stringify({ type: 'about:blank', title: code, status, code, detail: code }), { status, headers: { 'Content-Type': 'application/problem+json' } });

describe('evaluation set checks', () => {
  const item = (id: string, expected = 'Paris', match: 'exact' | 'contains' = 'exact') => ({ id, expected, match });

  it('takes a set the server takes, trimmed', () => {
    expect(setInput({ name: ' Capitals ', description: ' ', items: [item('q1')] })).toEqual({ input: { name: 'Capitals', items: [item('q1')] } });
    expect(setInput({ name: 'Capitals', description: 'Europe', items: [item('q1')] })).toEqual({ input: { name: 'Capitals', description: 'Europe', items: [item('q1')] } });
  });

  it('refuses what the server refuses', () => {
    const refused = setInput({ name: 'a\nb', description: 'x'.repeat(1001), items: [] });
    expect('errors' in refused && Object.keys(refused.errors).sort()).toEqual(['description', 'items', 'name']);
    expect(itemsProblem([item('q 1')])).toContain('Item 1: an id');
    expect(itemsProblem([item('q1'), item('q1')])).toContain('Item 2: the id q1 is used twice');
    expect(itemsProblem([item('q1', '   ')])).toContain('expected answer');
    expect(itemsProblem([item('q1', 'x'.repeat(1001))])).toContain('expected answer');
    expect(itemsProblem(Array.from({ length: 1001 }, (_, i) => item(`q${i}`)))).toContain('1 to 1,000 items');
    // Counted in code points, as the server counts them.
    expect(itemsProblem([item('q1', '😀'.repeat(1000))])).toBeNull();
  });

  it('reads pasted JSON: a list, or an object with items', () => {
    expect(parseItems('[{"id": "q1", "expected": "Paris", "match": "exact"}]')).toEqual({ items: [item('q1')] });
    expect(parseItems('{"name": "Capitals", "items": [{"id": "q2", "expected": "4", "match": "contains"}]}')).toEqual({ items: [item('q2', '4', 'contains')], name: 'Capitals' });
    expect(parseItems('not json')).toHaveProperty('error');
    expect(parseItems('{"items": 3}')).toHaveProperty('error');
    expect((parseItems('[{"id": "q1", "expected": "P", "match": "fuzzy"}]') as { error: string }).error).toContain('match is');
    expect((parseItems('[{"id": "q1", "expected": "P", "match": "exact", "weight": 2}]') as { error: string }).error).toContain('weight');
  });

  it('shows a share of the items', () => {
    expect(percent(1, 3)).toBe('33%');
    expect(percent(0, 0)).toBe('—');
  });
});

describe('Models → Evaluations', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn();
    history.replaceState(null, '', '/admin/models');
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
  const button = (text: string) => Array.from(document.querySelectorAll('button')).filter(node => node.textContent === text).pop() as HTMLButtonElement;
  const type = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement | HTMLTextAreaElement;
    const prototype = input instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    act(() => {
      Object.getOwnPropertyDescriptor(prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const calls = (method: string) => fetchMock.mock.calls.filter(([, init]) => ((init as RequestInit | undefined)?.method ?? 'GET') === method);
  const header = (call: unknown[], name: string) => new Headers((call[1] as RequestInit).headers).get(name);

  const SETS = '/api/admin/v1/organizations/org-1/catalog/evaluation-sets';
  const set = {
    id: '33333333-3333-3333-3333-333333333333', name: 'Capitals', description: 'Europe',
    items: [{ id: 'q1', expected: 'Paris', match: 'exact' }, { id: 'q2', expected: 'Rome', match: 'contains' }],
    created_at: '2026-11-10T00:00:00Z', created_by: { actor_type: 'user', id: 'u', name: 'Ada' }, version: 1,
  };
  const run = {
    id: 'r1', set_id: set.id, batch_id: '11111111-1111-1111-1111-111111111111', created_at: '2026-11-11T00:00:00Z', created_by: null,
    unmatched_lines: 1, results: [{ model: 'model-a', items: 2, answered: 2, correct: 1 }],
  };
  const batch = {
    id: 'batch_11111111-1111-1111-1111-111111111111', object: 'batch', endpoint: '/v1/chat/completions', status: 'completed',
    input_file_id: 'file-1', output_file_id: 'file-2', error_file_id: null, created_at: 1_790_000_000, expires_at: 1_790_086_400, completed_at: 1_790_000_600,
    request_counts: { total: 4, completed: 4, failed: 0 }, metadata: { name: 'capitals run' }, service_account_id: 'sa', team_id: null,
  };
  const embeddings = { ...batch, id: 'batch_22222222-2222-2222-2222-222222222222', endpoint: '/v1/embeddings' };

  const serve = (overrides: (url: string, init?: RequestInit) => Response | undefined = () => undefined) =>
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      const answer = overrides(url, init);
      if (answer) return answer;
      const method = init?.method ?? 'GET';
      if (url.endsWith('/runs') && method === 'POST') return json(run, 201);
      if (url.endsWith('/runs')) return json({ items: [run] });
      if (url === SETS && method === 'POST') return json({ ...set, id: '44444444-4444-4444-4444-444444444444', name: 'New', items: [set.items[0]] }, 201);
      if (url === `${SETS}/${set.id}` && method === 'DELETE') return new Response(null, { status: 204 });
      if (url === SETS) return json({ items: [set] });
      if (url.includes('/batches')) return json({ items: [batch, embeddings], next_page_token: null });
      return json({ items: [], next_page_token: null, release_id: null });
    });

  const open = async (role = 'organization_admin') => {
    act(() => root.render(<ProvidersScreen section="models" organizationId="org-1" role={role} />));
    await settle();
    openSection('models', 'evaluations');
    await settle();
  };

  it('lists sets, shows a set’s items and runs, and records a run from a completed chat batch', async () => {
    serve();
    await open();
    expect(container.textContent).toContain('Capitals');
    act(() => button('Open').click());
    await settle();
    const dialog = document.body.textContent ?? '';
    expect(dialog).toContain('Paris');
    expect(dialog).toContain('model-a');
    expect(dialog).toContain('1 of 2');
    expect(dialog).toContain('1 unmatched');
    // Only chat batches are offered.
    const options = Array.from((document.getElementById('evaluation-batch') as HTMLSelectElement).options).map(option => option.value);
    expect(options).toEqual([batch.id]);
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toContain('/api/admin/v1/organizations/org-1/batches?status=completed&page_size=100');
    act(() => button('Record run').click());
    await settle();
    const [post] = calls('POST');
    expect(post[0]).toBe(`${SETS}/${set.id}/runs`);
    expect(JSON.parse(String((post[1] as RequestInit).body))).toEqual({ batch_id: batch.id });
    expect(header(post, 'Idempotency-Key')).toBeTruthy();
    expect(document.body.textContent).toContain('Scored one model from the batch.');
  });

  it('takes a typed batch id when batches cannot be listed', async () => {
    serve(url => (url.includes('/batches') ? problem(403, 'forbidden') : undefined));
    await open();
    act(() => button('Open').click());
    await settle();
    expect(document.body.textContent).toContain('needs permission to read usage');
    type('evaluation-batch-id', ` ${batch.id} `);
    act(() => button('Record run').click());
    await settle();
    expect(JSON.parse(String((calls('POST')[0][1] as RequestInit).body))).toEqual({ batch_id: batch.id });
  });

  it('pages through completed batches and always takes a typed batch id', async () => {
    const later = { ...batch, id: 'batch_33333333-3333-3333-3333-333333333333', metadata: { name: 'older run' } };
    serve((url, init) => {
      if (init?.method) return undefined;
      if (url.includes('/batches') && url.includes('page_token=p2')) return json({ items: [later, batch], next_page_token: null });
      if (url.includes('/batches')) return json({ items: [batch, embeddings], next_page_token: 'p2' });
      return undefined;
    });
    await open();
    act(() => button('Open').click());
    await settle();
    const options = () => Array.from((document.getElementById('evaluation-batch') as HTMLSelectElement).options).map(option => option.value);
    expect(options()).toEqual([batch.id]);
    act(() => button('Load more').click());
    await settle();
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toContain('/api/admin/v1/organizations/org-1/batches?status=completed&page_size=100&page_token=p2');
    // Appended once each, and no more pages to load.
    expect(options()).toEqual([batch.id, later.id]);
    expect(button('Load more')).toBeUndefined();
    // The list worked, yet a typed id is taken, and checked first.
    type('evaluation-batch-id', 'batch_nope');
    act(() => button('Record run').click());
    await settle();
    expect(document.body.textContent).toContain('Enter the id of a completed chat batch');
    expect(calls('POST')).toHaveLength(0);
    const unlisted = 'batch_44444444-4444-4444-4444-444444444444';
    type('evaluation-batch-id', ` ${unlisted} `);
    act(() => button('Record run').click());
    await settle();
    expect(JSON.parse(String((calls('POST')[0][1] as RequestInit).body))).toEqual({ batch_id: unlisted });
  });

  it('keeps loaded pages when a first page from an earlier run of the effect answers late', async () => {
    // StrictMode sets the effect up twice: the first setup's request is
    // answered last, after another page was loaded.
    const later = { ...batch, id: 'batch_33333333-3333-3333-3333-333333333333', metadata: { name: 'older run' } };
    let release: (response: Response) => void = () => undefined;
    let first = true;
    serve((url, init) => {
      if (init?.method || !url.includes('/batches')) return undefined;
      if (url.includes('page_token=p2')) return json({ items: [later], next_page_token: null });
      return json({ items: [batch], next_page_token: 'p2' });
    });
    const served = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (!init?.method && url.includes('/batches') && !url.includes('page_token') && first) {
        first = false;
        return new Promise<Response>(resolve => { release = resolve; });
      }
      return served(url, init);
    });
    act(() => root.render(<StrictMode><ProvidersScreen section="models" organizationId="org-1" role="organization_admin" /></StrictMode>));
    await settle();
    openSection('models', 'evaluations');
    await settle();
    act(() => button('Open').click());
    await settle();
    const options = () => Array.from((document.getElementById('evaluation-batch') as HTMLSelectElement).options).map(option => option.value);
    act(() => button('Load more').click());
    await settle();
    expect(options()).toEqual([batch.id, later.id]);
    // The earlier setup's first page answers now, and changes nothing.
    await act(async () => { release(json({ items: [batch], next_page_token: 'p2' })); });
    await settle();
    expect(options()).toEqual([batch.id, later.id]);
    expect(button('Load more')).toBeUndefined();
  });

  it('offers a typed batch id when no completed chat batch is listed', async () => {
    serve(url => (url.includes('/batches') ? json({ items: [embeddings], next_page_token: null }) : undefined));
    await open();
    act(() => button('Open').click());
    await settle();
    expect(document.body.textContent).toContain('No completed chat batches.');
    expect(document.getElementById('evaluation-batch')).toBeNull();
    act(() => button('Record run').click());
    await settle();
    expect(document.body.textContent).toContain('Enter the id of a completed chat batch');
    type('evaluation-batch-id', batch.id);
    act(() => button('Record run').click());
    await settle();
    expect(JSON.parse(String((calls('POST')[0][1] as RequestInit).body))).toEqual({ batch_id: batch.id });
  });

  it('creates a set from the items editor, checking it first', async () => {
    serve();
    await open();
    act(() => button('New evaluation set').click());
    await settle();
    act(() => button('Create set').click());
    await settle();
    expect(document.body.textContent).toContain('Enter a name');
    expect(calls('POST')).toHaveLength(0);
    type('evaluation-name', 'New');
    const expected = document.querySelector<HTMLInputElement>('[aria-label="Item 1 expected answer"]')!;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(expected, 'Paris');
      expected.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => button('Create set').click());
    await settle();
    const [post] = calls('POST');
    expect(post[0]).toBe(SETS);
    expect(JSON.parse(String((post[1] as RequestInit).body))).toEqual({ name: 'New', items: [{ id: 'q1', expected: 'Paris', match: 'exact' }] });
    expect(document.body.textContent).toContain('Created New, with 1 items.');
  });

  it('creates a set from pasted JSON, and says what is wrong with it', async () => {
    serve();
    await open();
    act(() => button('New evaluation set').click());
    await settle();
    act(() => (document.getElementById('evaluation-mode-json') as HTMLInputElement).click());
    await settle();
    type('evaluation-json', '[{"id": "q1", "expected": "Paris"}]');
    type('evaluation-name', 'New');
    act(() => button('Create set').click());
    await settle();
    expect(document.body.textContent).toContain('match is "exact" or "contains"');
    expect(calls('POST')).toHaveLength(0);
    type('evaluation-json', '[{"id": "q1", "expected": "Paris", "match": "exact"}]');
    act(() => button('Create set').click());
    await settle();
    expect(JSON.parse(String((calls('POST')[0][1] as RequestInit).body)).items).toEqual([{ id: 'q1', expected: 'Paris', match: 'exact' }]);
  });

  it('deletes a set with its version, and one key per attempt', async () => {
    let refuse = true;
    serve((url, init) => {
      if (init?.method === 'DELETE' && refuse) {
        refuse = false;
        return problem(503, 'service_unavailable');
      }
      return undefined;
    });
    await open();
    act(() => button('Delete').click());
    await settle();
    expect(document.body.textContent).toContain('runs and scores are kept');
    act(() => button('Delete').click());
    await settle();
    act(() => button('Delete').click());
    await settle();
    const deletes = calls('DELETE');
    expect(deletes).toHaveLength(2);
    expect(deletes[0][0]).toBe(`${SETS}/${set.id}`);
    expect(header(deletes[0], 'If-Match')).toBe('"v1"');
    expect(header(deletes[1], 'Idempotency-Key')).toBe(header(deletes[0], 'Idempotency-Key'));
    expect(container.textContent).toContain('Deleted Capitals');
  });

  it('offers no changes to a read-only role', async () => {
    serve();
    await open('read_only_member');
    expect(button('New evaluation set')).toBeUndefined();
    expect(button('Delete')).toBeUndefined();
    act(() => button('Open').click());
    await settle();
    expect(button('Record run')).toBeUndefined();
    expect(button('Delete set')).toBeUndefined();
  });
});
