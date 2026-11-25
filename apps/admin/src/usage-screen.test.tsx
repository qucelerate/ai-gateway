import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { UsageScreen, reportWindow } from './usage-screen';
import { TraceContentSection, contentStateText, elapsed, providerErrorText, tokensText } from './traces-screen';
import type { ContentState } from './api';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

describe('usage report helpers', () => {
  it('ends the window now and starts it the chosen number of days earlier', () => {
    const now = new Date('2026-10-07T12:00:00Z');
    expect(reportWindow('7', now)).toEqual({ from: '2026-09-30T12:00:00.000Z', to: '2026-10-07T12:00:00.000Z' });
  });

  it('names why a provider request failed without any provider message', () => {
    expect(providerErrorText(null)).toBe('None');
    expect(providerErrorText({ category: 'authorization', http_status: 403, error_type: 'AccessDeniedException' }))
      .toBe('Permission refused · HTTP 403 · AccessDeniedException');
    expect(providerErrorText({ category: 'protocol', http_status: null, error_type: null })).toBe('Unreadable response');
  });
});

describe('usage screen', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn();
    // The dashboard has its own tests.
    history.replaceState(null, '', '/admin/usage/reconciliation');
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 5; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const requested = () => fetchMock.mock.calls.map(([url]) => String(url));

  it('resolves an open reconciliation item with the amount converted to money units', async () => {
    const item = {
      id: 'item-1', trace_id: 'trace-1', reason: 'provider-unknown', status: 'open', reservation_state: 'unknown',
      model: 'gpt-5', deployment: 'gpt-5-eu', currency: 'USD', reserved_minor: 2_000_000_000,
      created_at: '2026-10-06T00:00:00Z', resolved_at: null, resolution: null, resolved_amount_minor: null,
    };
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      if (init?.method === 'POST') return json({ ...item, status: 'resolved' });
      return json({ items: [item], next_page_token: null });
    });
    act(() => root.render(<UsageScreen organizationId="org-1" />));
    await settle();
    openSection('usage', 'reconciliation');
    await settle();
    expect(requested()).toContain('/api/admin/v1/organizations/org-1/reconciliation-items?status=open&page_size=25');

    const resolve = Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Resolve') as HTMLButtonElement;
    act(() => resolve.click());
    const input = document.getElementById('resolve-amount') as HTMLInputElement;
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    act(() => {
      setValue.call(input, '1.25');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();

    const post = fetchMock.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'POST')!;
    expect(post[0]).toBe('/api/admin/v1/organizations/org-1/reconciliation-items/item-1/resolve');
    const init = post[1] as RequestInit;
    expect(JSON.parse(String(init.body))).toEqual({ amount_minor: 125_000_000 });
    const headers = new Headers(init.headers);
    expect(headers.get('If-Match')).toBe('*');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
  });

  it('retries a resolution with the same idempotency key', async () => {
    const item = {
      id: 'item-1', trace_id: 'trace-1', reason: 'provider-unknown', status: 'open', reservation_state: 'unknown',
      model: 'gpt-5', deployment: 'gpt-5-eu', currency: 'USD', reserved_minor: 2_000_000_000,
      created_at: '2026-10-06T00:00:00Z', resolved_at: null, resolution: null, resolved_amount_minor: null,
    };
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.includes('/usage')) return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
      if (init?.method === 'POST') throw new TypeError('network error');
      return json({ items: [item], next_page_token: null });
    });
    act(() => root.render(<UsageScreen organizationId="org-1" />));
    await settle();
    openSection('usage', 'reconciliation');
    await settle();
    act(() => (Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Resolve') as HTMLButtonElement).click());
    const input = document.getElementById('resolve-amount') as HTMLInputElement;
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    const type = (value: string) => act(() => {
      setValue.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    const submit = async () => { act(() => (input.form as HTMLFormElement).requestSubmit()); await settle(); };
    type('1.25');
    await submit();
    await submit();
    type('2');
    await submit();
    const keys = fetchMock.mock.calls
      .filter(([, init]) => (init as RequestInit | undefined)?.method === 'POST')
      .map(([, init]) => new Headers((init as RequestInit).headers).get('Idempotency-Key'));
    expect(keys).toHaveLength(3);
    expect(keys[1]).toBe(keys[0]);
    expect(keys[2]).not.toBe(keys[0]);
  });
});

describe('trace content', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  const TRACE = '33333333-3333-3333-3333-333333333333';
  const stored = {
    trace_id: TRACE,
    state: 'stored',
    started_at: '2026-10-29T10:00:00.000Z',
    provider_started_at: '2026-10-29T10:00:00.020Z',
    first_event_at: '2026-10-29T10:00:00.400Z',
    finished_at: '2026-10-29T10:00:01.250Z',
    expired_at: null,
    request_bytes: 120,
    response_bytes: 80,
    truncated: { request: false, response: true },
    request: { messages: [{ role: 'system', content: 'Be brief.' }, { role: 'user', content: 'Hello?' }], temperature: 0.2 },
    response: { model: 'claude-x', choices: [{ index: 0, content: 'Hi.', finish_reason: 'stop' }] },
  };

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn();
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 5; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const show = async (state: ContentState | null = 'stored') => {
    act(() => root.render(<TraceContentSection organizationId="org-1" traceId={TRACE} state={state} />));
    await settle();
  };

  it('explains a trace without content and times what it kept', () => {
    expect(contentStateText('stored')).toBeNull();
    expect(contentStateText('expired')).toContain('expired');
    expect(contentStateText('pending')).toContain('still being recorded');
    expect(elapsed('2026-10-29T10:00:00.000Z', '2026-10-29T10:00:01.250Z')).toBe('1,250 ms');
    expect(elapsed(null, '2026-10-29T10:00:01.250Z')).toBeNull();
  });

  it('shows kept messages and response without being asked', async () => {
    fetchMock.mockResolvedValue(json(stored));
    await show();
    expect(String(fetchMock.mock.calls[0][0])).toContain(`/organizations/org-1/traces/${TRACE}/content`);
    const texts = Array.from(container.querySelectorAll('pre')).map(node => node.textContent);
    expect(texts).toEqual(['Be brief.', 'Hello?', 'Hi.']);
    expect(container.textContent).toContain('truncated at the size limit');
    expect(container.textContent).toContain('1,250 ms');
  });

  it('says why nothing is shown, reading nothing it knows was not kept', async () => {
    await show('off');
    expect(container.querySelectorAll('pre')).toHaveLength(0);
    expect(container.textContent).toContain('timing only');
    await show(null);
    expect(container.textContent).toContain('This request was not traced.');
    expect(container.textContent).not.toContain('audit log');
    // Every read is audited: none is made for a request without content.
    expect(fetchMock).not.toHaveBeenCalled();
  });

  it('shows an embeddings request’s inputs and the start of each vector', async () => {
    fetchMock.mockResolvedValue(json({
      ...stored,
      truncated: { request: false, response: false },
      request: { input: ['first text', 'second text'] },
      response: { model: 'embed-x', embeddings: [{ index: 0, dimensions: 1536, preview: [0.5, -0.25] }, { index: 1, dimensions: 2, preview: [1, 2] }] },
    }));
    await show();
    const texts = Array.from(container.querySelectorAll('pre')).map(node => node.textContent);
    expect(texts).toEqual(['first text', 'second text', '[0.5, -0.25, …]', '[1, 2]']);
    expect(container.textContent).toContain('embed-x · vector 0 · 1,536 dimensions');
  });

  it('marks a request too large to keep, and shows a failed check again', async () => {
    fetchMock.mockResolvedValueOnce(json({ ...stored, state: 'pending', request: null, response: null }));
    await show('writing');
    expect(container.textContent).toContain('still being recorded');
    fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({ status: 503, code: 'service_unavailable', title: 'Unavailable', detail: 'Trace content store unavailable.' }), { status: 503, headers: { 'Content-Type': 'application/problem+json' } }));
    const again = Array.from(container.querySelectorAll('button')).find(node => node.textContent === 'Check again') as HTMLButtonElement;
    act(() => again.click());
    await settle();
    expect(container.textContent).toContain('Trace content store unavailable.');

    act(() => root.unmount());
    root = createRoot(container);
    fetchMock.mockReset();
    fetchMock.mockResolvedValue(json({ ...stored, request: null, truncated: { request: true, response: false } }));
    await show();
    expect(container.textContent).toContain('Request · truncated at the size limit');
    expect(container.textContent).toContain('Too large to keep any of it.');
  });

  it('tells a role without the permission that it cannot read content', async () => {
    fetchMock.mockResolvedValue(new Response(JSON.stringify({ status: 403, code: 'forbidden', title: 'Forbidden', detail: 'administrative permission denied' }), { status: 403, headers: { 'Content-Type': 'application/problem+json' } }));
    await show();
    expect(container.textContent).toContain('Only owners and admins can read request content.');
    expect(container.querySelectorAll('button')).toHaveLength(0);
  });
});

describe('tokensText', () => {
  it('shows reasoning, cached and other tokens apart from output when reported', () => {
    // GPT spending its whole cap reasoning: no output, 16 reasoning.
    expect(tokensText({ input: 16, output: 0, cached_input: 0, reasoning: 16, other: null })).toBe('16 in / 0 out · 16 reasoning');
    expect(tokensText({ input: 1200, output: 300, cached_input: 800, reasoning: 50, other: 4 })).toBe('1,200 in / 300 out · 800 cached · 50 reasoning · 4 other');
    expect(tokensText({ input: 14, output: 1, cached_input: null, reasoning: null, other: null })).toBe('14 in / 1 out');
  });
});
