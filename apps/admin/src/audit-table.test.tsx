import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { AuditScreen } from './audit-screen';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const event = (id: string, action: string, outcome: string) => ({
  id, occurred_at: '2026-10-29T10:00:00Z', organization_id: 'org-1', team_id: null,
  actor: { actor_type: 'user', user_id: '123e4567-e89b-12d3-a456-426614174000', service_account_id: null },
  action, target_type: 'api_key', target_id: '00000000-0000-0000-0000-00000000000' + id.slice(-1), outcome, request_id: null,
});

describe('audit table', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  const urls = () => fetchMock.mock.calls.map(([url]) => decodeURIComponent(String(url)));

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    history.replaceState(null, '', '/admin/audit');
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn(async (url: string) => {
      if (url.includes('/audit-events/summary')) {
        const groupBy = new URL(url, 'http://x').searchParams.get('group_by');
        if (groupBy === 'outcome') {
          return json({ group_by: ['outcome'], total: 7, truncated: false, groups: [
            { values: { outcome: 'succeeded' }, count: 5, first_occurred_at: '2026-10-28T00:00:00Z', last_occurred_at: '2026-10-29T00:00:00Z' },
            { values: { outcome: 'denied' }, count: 2, first_occurred_at: '2026-10-28T00:00:00Z', last_occurred_at: '2026-10-29T00:00:00Z' },
          ] });
        }
        return json({ group_by: ['action'], total: 3, truncated: true, groups: [
          { values: { action: 'api_key.created' }, count: 3, first_occurred_at: '2026-10-28T00:00:00Z', last_occurred_at: '2026-10-29T00:00:00Z' },
        ] });
      }
      if (url.includes('page_token=p2')) return json({ items: [event('e-3', 'api_key.revoked', 'succeeded')], next_page_token: null });
      return json({ items: [event('e-1', 'api_key.created', 'succeeded'), event('e-2', 'team.updated', 'denied')], next_page_token: 'p2' });
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    history.replaceState(null, '', '/admin/audit');
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const render = async () => {
    act(() => root.render(<AuditScreen organizationId="org-1" />));
    await settle();
  };
  const bodyRows = () => container.querySelectorAll('table > tbody > .data-row:not(.head):not(.group-row)');

  it('queries the server with the view from the URL, and loads more pages', async () => {
    history.replaceState(null, '', '/admin/audit?audit.q=api_key&audit.f.outcome=denied,failed');
    await render();
    expect(urls()[0]).toContain('/audit-events?page_size=25&outcome=denied,failed&q=api_key');
    expect(bodyRows()).toHaveLength(2);
    expect(container.textContent).toContain('2+ rows');
    act(() => (Array.from(container.querySelectorAll('button')).find(button => button.textContent === 'Load more') as HTMLButtonElement).click());
    await settle();
    expect(bodyRows()).toHaveLength(3);
    expect(urls().some(url => url.includes('page_token=p2'))).toBe(true);
  });

  it('counts a filter menu’s choices on the server when it opens, without its own filter', async () => {
    history.replaceState(null, '', '/admin/audit?audit.f.outcome=denied&audit.q=key');
    await render();
    const menu = Array.from(container.querySelectorAll('details')).find(details => details.querySelector('summary')?.textContent?.startsWith('Outcome'))!;
    act(() => {
      menu.open = true;
      menu.dispatchEvent(new Event('toggle'));
    });
    await settle();
    const summary = urls().find(url => url.includes('/summary'))!;
    expect(summary).toContain('group_by=outcome');
    expect(summary).toContain('q=key');
    expect(summary).not.toContain('outcome=denied');
    expect(menu.textContent).toContain('succeeded 5');
    expect(menu.textContent).toContain('denied 2');

    // Choosing another value narrows the events, not the menu's own counts:
    // its list stays and is not counted again.
    const summaries = urls().filter(url => url.includes('/summary')).length;
    const succeeded = Array.from(menu.querySelectorAll<HTMLInputElement>('input[type="checkbox"]')).find(input => input.parentElement?.textContent?.includes('succeeded'))!;
    act(() => succeeded.click());
    await settle();
    expect(new URLSearchParams(location.search).get('audit.f.outcome')).toBe('denied,succeeded');
    expect(menu.textContent).toContain('succeeded 5');
    expect(urls().filter(url => url.includes('/summary'))).toHaveLength(summaries);
  });

  it('reads no events while grouped', async () => {
    history.replaceState(null, '', '/admin/audit?audit.g=action');
    await render();
    expect(urls().some(url => url.includes('/summary?group_by=action'))).toBe(true);
    expect(urls().some(url => url.includes('/audit-events?'))).toBe(false);
  });

  it('keeps the table on invalid filters, so they can be corrected', async () => {
    fetchMock.mockImplementation(async () => new Response(JSON.stringify({ type: 'about:blank', code: 'invalid_request', title: 'Bad Request', status: 400, detail: 'occurred_after must be before occurred_before', instance: '', request_id: 'r' }), { status: 400, headers: { 'Content-Type': 'application/problem+json' } }));
    history.replaceState(null, '', '/admin/audit?audit.r.occurred=2026-10-29T00:00:00Z..2026-10-01T00:00:00Z');
    await render();
    expect(container.textContent).toContain('These filters are not valid');
    expect(container.querySelector('table')).not.toBeNull();
  });

  it('finds events by a typed actor ID', async () => {
    await render();
    const input = container.querySelector<HTMLInputElement>('#audit-actor')!;
    const setValue = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    act(() => {
      setValue.call(input, 'not-an-id');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => container.querySelector<HTMLFormElement>('form[aria-label="Find events by ID"]')!.requestSubmit());
    expect(container.textContent).toContain('Enter the actor as a UUID.');
    act(() => {
      setValue.call(input, '123e4567-e89b-12d3-a456-426614174000');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => container.querySelector<HTMLFormElement>('form[aria-label="Find events by ID"]')!.requestSubmit());
    await settle();
    expect(urls()[urls().length - 1]).toContain('actor=123e4567-e89b-12d3-a456-426614174000');

    // Emptying the field and finding again drops the actor filter.
    act(() => {
      setValue.call(input, '');
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    act(() => container.querySelector<HTMLFormElement>('form[aria-label="Find events by ID"]')!.requestSubmit());
    await settle();
    expect(new URLSearchParams(location.search).get('audit.f.actor')).toBeNull();
    expect(urls()[urls().length - 1]).not.toContain('actor=');
  });

  it('shows a day’s end to the millisecond in the time range', async () => {
    history.replaceState(null, '', '/admin/audit?audit.r.occurred=2026-10-29T00:00:00Z..2026-10-29T23:59:59.999Z');
    await render();
    const to = container.querySelector<HTMLInputElement>('[aria-label="Occurred range"] .form-field:nth-child(2) input')!;
    expect(to.value).toMatch(/:59:59\.999$/);
  });

  it('groups on the server and drills into a group', async () => {
    await render();
    const select = container.querySelector<HTMLSelectElement>('select[aria-label="Group by"]')!;
    act(() => {
      select.value = 'action';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    expect(urls().some(url => url.includes('/summary?group_by=action'))).toBe(true);
    expect(container.textContent).toContain('1+ groups · 3 rows');
    expect(container.textContent).toContain('Only the largest groups are shown');
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label="Show the rows of Action: API key created"]')!.click());
    await settle();
    expect(new URLSearchParams(location.search).get('audit.f.action')).toBe('api_key.created');
    expect(new URLSearchParams(location.search).get('audit.g')).toBeNull();
    expect(urls()[urls().length - 1]).toContain('/audit-events?page_size=25&action=api_key.created');
  });

  it('leaves out views unless asked to show them', async () => {
    await render();
    expect(urls()[urls().length - 1]).toContain('exclude_views=true');
    const toggle = () => Array.from(container.querySelectorAll('.check-option')).find(option => option.textContent?.includes('Show views'))!.querySelector('input')!;
    act(() => toggle().click());
    await settle();
    expect(urls()[urls().length - 1]).not.toContain('exclude_views');
    // Kept in the address, so a reload or shared link still shows them.
    expect(new URLSearchParams(location.search).get('audit.views')).toBe('1');
    act(() => root.unmount());
    root = createRoot(container);
    await render();
    expect(toggle().checked).toBe(true);
    expect(urls()[urls().length - 1]).not.toContain('exclude_views');
    act(() => toggle().click());
    await settle();
    expect(new URLSearchParams(location.search).has('audit.views')).toBe(false);
  });

  it('expands an event and narrows to everything by its actor', async () => {
    await render();
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label="Expand API key created"]')!.click());
    act(() => (Array.from(container.querySelectorAll('button')).find(button => button.textContent === 'Everything by this actor') as HTMLButtonElement).click());
    await settle();
    expect(urls()[urls().length - 1]).toContain('actor=123e4567-e89b-12d3-a456-426614174000');
    expect(container.textContent).toContain('Actor: 123e4567');
  });
});
