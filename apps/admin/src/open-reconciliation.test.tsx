import { afterEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { reconciliationChanged, useOpenReconciliation } from './open-reconciliation';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

function Probe({ organizationId }: { organizationId: string }) {
  return <output>{useOpenReconciliation(organizationId, 0) ?? 'none'}</output>;
}

async function shown(response: () => Response, organizationId = 'org-1') {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  const fetchMock = vi.fn(async () => response());
  vi.stubGlobal('fetch', fetchMock);
  const container = document.createElement('div');
  const root = createRoot(container);
  act(() => root.render(<Probe organizationId={organizationId} />));
  for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
  const text = container.textContent;
  act(() => root.unmount());
  return { text, urls: fetchMock.mock.calls.map(call => String((call as unknown[])[0])) };
}

describe('open reconciliation count', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('counts the open items, and caps the count past 99', async () => {
    const two = await shown(() => json({ items: [{}, {}], next_page_token: null }));
    expect(two.text).toBe('2');
    expect(two.urls[0]).toContain('/organizations/org-1/reconciliation-items?status=open&page_size=100');
    expect((await shown(() => json({ items: Array(100).fill({}), next_page_token: 'more' }))).text).toBe('99+');
  });

  it('reads the count again once an item is resolved', async () => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    let open = 2;
    vi.stubGlobal('fetch', vi.fn(async () => json({ items: Array(open).fill({}), next_page_token: null })));
    const container = document.createElement('div');
    const root = createRoot(container);
    const settle = async () => { for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); }); };
    act(() => root.render(<Probe organizationId="org-1" />));
    await settle();
    expect(container.textContent).toBe('2');
    open = 0;
    act(() => { dispatchEvent(new Event(reconciliationChanged)); });
    await settle();
    expect(container.textContent).toBe('none');
    act(() => root.unmount());
  });

  it('shows nothing when none are open or the member may not read them', async () => {
    expect((await shown(() => json({ items: [], next_page_token: null }))).text).toBe('none');
    expect((await shown(() => json({ type: 'about:blank', code: 'forbidden', title: 'Forbidden', status: 403, detail: 'no', instance: '', request_id: 'r' }, 403))).text).toBe('none');
    expect((await shown(() => json({}), '')).urls).toEqual([]);
  });
});
