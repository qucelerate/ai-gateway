import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import type { Notification } from './api';
import { NotificationBell, NotificationsScreen, RecentNotifications, applyRead, covers, kindLabel, micros } from './notifications';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

// `n5` is newer than `n4`: a numbered id is that many minutes past a fixed hour ago.
const createdAt = (id: string) => {
  const n = Number(/\d+$/.exec(id)?.[0] ?? 50);
  return new Date(Date.now() - 3_600_000 + n * 60_000).toISOString();
};

const notification = (id: string, overrides: Partial<Notification> = {}): Notification => ({
  id,
  kind: 'price_source.failing',
  severity: 'warning',
  subject: 'price-sheet:openai',
  title: `Price source ${id} is failing`,
  body: 'The latest fetch failed.',
  link: '/admin/models/pricing',
  data: {},
  created_at: createdAt(id),
  updated_at: createdAt(id),
  resolved_at: null,
  read: false,
  ...overrides,
});

type Call = { url: string; method: string; body: unknown };

/** A fetch that records each call and answers from `route`. */
function stubFetch(route: (url: string, method: string, body: unknown) => Response | Promise<Response>) {
  const calls: Call[] = [];
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    const method = init?.method ?? 'GET';
    const body = init?.body ? JSON.parse(String(init.body)) : undefined;
    calls.push({ url, method, body });
    return route(url, method, body);
  }));
  return calls;
}

const last = <T,>(items: T[]): T => items[items.length - 1];

async function settle(rounds = 6) {
  for (let i = 0; i < rounds; i += 1) await act(async () => { await Promise.resolve(); });
}

function button(container: HTMLElement, text: string | RegExp): HTMLButtonElement {
  const found = Array.from(container.querySelectorAll('button')).find(node =>
    typeof text === 'string' ? node.textContent === text : text.test(node.textContent ?? '') || text.test(node.getAttribute('aria-label') ?? ''));
  if (!found) throw new Error(`no button ${text}`);
  return found as HTMLButtonElement;
}

describe('notification helpers', () => {
  it('names kinds', () => {
    expect(kindLabel('price.held')).toBe('Price change held');
    expect(kindLabel('something.new')).toBe('something.new');
  });

  it('marks read what a mark covers, by creation then id', () => {
    const at = '2026-11-13T10:00:00.000Z';
    const through = { id: 'b', created_at: at };
    expect(covers({ through }, { id: 'a', created_at: at })).toBe(true);
    expect(covers({ through }, { id: 'c', created_at: at })).toBe(false);
    expect(covers({ through }, { id: 'z', created_at: '2026-11-13T09:59:59.000Z' })).toBe(true);
    expect(covers({ ids: ['x'] }, { id: 'x', created_at: at })).toBe(true);
    // Within one millisecond, the microseconds order them, not the id.
    expect(micros('2026-11-13T10:00:00.123457Z') - micros('2026-11-13T10:00:00.123456Z')).toBe(1);
    expect(micros('2026-11-13T10:00:00Z')).toBe(Date.parse('2026-11-13T10:00:00Z') * 1000);
    const newest = { id: '00000000-0000-0000-0000-000000000001', created_at: '2026-11-13T10:00:00.123457Z' };
    expect(covers({ through: newest }, { id: 'ffffffff-ffff-ffff-ffff-ffffffffffff', created_at: '2026-11-13T10:00:00.123456Z' })).toBe(true);
    expect(covers({ through: { ...newest, created_at: '2026-11-13T10:00:00.123455Z' } }, { id: '0', created_at: '2026-11-13T10:00:00.123456Z' })).toBe(false);
    const items = [notification('n2'), notification('n1', { read: true })];
    expect(applyRead(items, { ids: ['n2'] }).map(item => item.read)).toEqual([true, true]);
  });
});

describe('notifications', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    vi.useRealTimers();
    history.replaceState(null, '', '/');
  });

  it('shows a dot while anything is unread, the count in its label, and reads it again on focus and every minute', async () => {
    vi.useFakeTimers({ toFake: ['setInterval', 'clearInterval'] });
    let unread = 3;
    const calls = stubFetch(url => url.endsWith('/unread-count') ? json({ unread }) : json({ items: [], next_page_token: null }));
    act(() => root.render(<NotificationBell organizationId="org-1" />));
    await settle();
    expect(container.querySelector('.bell-dot')).toBeTruthy();
    expect(button(container, /Notifications, 3 unread/)).toBeTruthy();

    unread = 12;
    act(() => { dispatchEvent(new Event('focus')); });
    await settle();
    expect(container.querySelector('.bell-dot')).toBeTruthy();
    expect(button(container, /Notifications, 12 unread/)).toBeTruthy();

    unread = 0;
    act(() => { vi.advanceTimersByTime(60_000); });
    await settle();
    expect(container.querySelector('.bell-dot')).toBeNull();
    expect(calls.filter(call => call.url.endsWith('/organizations/org-1/notifications/unread-count'))).toHaveLength(3);
  });

  it('opens the latest five, marks them all read through the newest, and links to the page', async () => {
    const items = [notification('n5'), notification('n4', { read: true }), notification('n3')];
    let unread = 2;
    const calls = stubFetch((url, method) => {
      if (url.endsWith('/unread-count')) return json({ unread });
      if (method === 'POST') { unread = 0; return json({ unread: 0 }); }
      return json({ items, next_page_token: 'more' });
    });
    act(() => root.render(<NotificationBell organizationId="org-1" />));
    await settle();
    act(() => button(container, /Notifications, 2 unread/).click());
    await settle();
    const list = calls.find(call => call.url.includes('/notifications?'))!;
    expect(list.url).toContain('page_size=5');
    expect(Array.from(container.querySelectorAll('.notification-line')).map(node => node.className)).toEqual([
      'notification-line unread', 'notification-line', 'notification-line unread',
    ]);
    expect(container.querySelector('a[href="/admin/notifications"]')!.textContent).toBe('View all →');

    await act(async () => { button(container, 'Mark all read').click(); });
    await settle();
    expect(calls.find(call => call.method === 'POST')).toMatchObject({ url: '/api/admin/v1/organizations/org-1/notifications/read', body: { through: 'n5' } });
    expect(container.querySelectorAll('.notification-line.unread')).toHaveLength(0);
    // The bell read its count again.
    expect(container.querySelector('.bell-dot')).toBeNull();

    act(() => { document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })); });
    expect(container.querySelector('.notification-menu')).toBeNull();
  });

  it('opening one marks it read and follows its link', async () => {
    const calls = stubFetch((url, method) => {
      if (url.endsWith('/unread-count')) return json({ unread: 1 });
      if (method === 'POST') return json({ unread: 0 });
      return json({ items: [notification('n1', { link: '/admin/connections/discovery' })], next_page_token: null });
    });
    act(() => root.render(<NotificationBell organizationId="org-1" />));
    await settle();
    act(() => button(container, /Notifications/).click());
    await settle();
    await act(async () => { (container.querySelector('.notification-open') as HTMLButtonElement).click(); });
    await settle();
    expect(calls.find(call => call.method === 'POST')!.body).toEqual({ ids: ['n1'] });
    expect(location.pathname).toBe('/admin/connections/discovery');
    expect(container.querySelector('.notification-menu')).toBeNull();
  });

  it('filters the page, pages on, and marks one or all read', async () => {
    const calls = stubFetch((url, method) => {
      if (method === 'POST') return json({ unread: 0 });
      if (url.includes('page_token=p2')) return json({ items: [notification('older', { kind: 'provider_account.sync_failing', title: 'Listing anthropic-main’s models failed' })], next_page_token: null });
      if (url.includes('page_size=1&') || url.endsWith('page_size=1')) return json({ items: [notification('newest')], next_page_token: 'x' });
      return json({ items: [notification('n2'), notification('n1', { read: true, resolved_at: new Date().toISOString() })], next_page_token: 'p2' });
    });
    act(() => root.render(<NotificationsScreen organizationId="org-1" />));
    await settle();
    expect(container.querySelectorAll('.notification-row')).toHaveLength(2);
    expect(container.textContent).toContain('resolved');

    act(() => button(container, 'Load more').click());
    await settle();
    expect(container.textContent).toContain('Listing anthropic-main’s models failed');

    const kind = container.querySelector('#notification-kind') as HTMLSelectElement;
    act(() => {
      kind.value = 'price_source.failing';
      kind.dispatchEvent(new Event('change', { bubbles: true }));
    });
    const read = container.querySelector('#notification-read') as HTMLSelectElement;
    act(() => {
      read.value = 'unread';
      read.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    const filtered = last(calls.filter(call => call.method === 'GET')).url;
    expect(filtered).toContain('kind=price_source.failing');
    expect(filtered).toContain('read=unread');
    expect(filtered).not.toContain('page_token');

    await act(async () => { button(container, /^Mark read/).click(); });
    await settle();
    expect(last(calls.filter(call => call.method === 'POST')).body).toEqual({ ids: ['n2'] });

    await act(async () => { button(container, 'Mark all read').click(); });
    await settle();
    // Through the newest of every notification, not only the filtered ones.
    expect(last(calls.filter(call => call.method === 'POST')).body).toEqual({ through: 'newest' });
  });

  it('says when there is nothing, and when the role cannot see them', async () => {
    stubFetch(() => json({ items: [], next_page_token: null }));
    act(() => root.render(<NotificationsScreen organizationId="org-1" />));
    await settle();
    expect(container.textContent).toContain('No notifications');

    act(() => root.unmount());
    root = createRoot(container);
    stubFetch(() => json({ type: '', code: 'forbidden', title: '', status: 403, detail: '', instance: '', request_id: '' }, 403));
    act(() => root.render(<NotificationsScreen organizationId="org-1" />));
    await settle();
    expect(container.textContent).toContain('Your role cannot see this organization’s notifications.');
  });

  it('marking one read keeps every loaded page, shown read in place', async () => {
    let lists = 0;
    const calls = stubFetch((url, method) => {
      if (method === 'POST') return json({ unread: 0 });
      lists += 1;
      if (url.includes('page_token=p2')) return json({ items: [notification('n1')], next_page_token: null });
      return json({ items: [notification('n3'), notification('n2')], next_page_token: 'p2' });
    });
    act(() => root.render(<NotificationsScreen organizationId="org-1" />));
    await settle();
    act(() => button(container, 'Load more').click());
    await settle();
    expect(container.querySelectorAll('.notification-row')).toHaveLength(3);
    const loaded = lists;

    const older = last(Array.from(container.querySelectorAll('.notification-row')));
    await act(async () => { (Array.from(older.querySelectorAll('button')).find(node => node.textContent?.startsWith('Mark read')) as HTMLButtonElement).click(); });
    await settle();
    expect(last(calls).body).toEqual({ ids: ['n1'] });
    expect(lists).toBe(loaded);
    expect(Array.from(container.querySelectorAll('.notification-row')).map(node => node.className.includes('unread'))).toEqual([true, true, false]);

    // Through the newest: every loaded row, on every page.
    await act(async () => { button(container, 'Mark all read').click(); });
    await settle();
    expect(container.querySelectorAll('.notification-row.unread')).toHaveLength(0);
    expect(container.querySelectorAll('.notification-row')).toHaveLength(3);
  });

  it('under the Unread filter, drops what is marked read and keeps the loaded pages', async () => {
    let lists = 0;
    const calls = stubFetch((url, method) => {
      if (method === 'POST') return json({ unread: 0 });
      lists += 1;
      if (url.includes('page_size=1') && !url.includes('page_size=1&read') && !url.includes('read=')) return json({ items: [notification('n9')], next_page_token: null });
      if (url.includes('page_token=p2')) return json({ items: [notification('n1')], next_page_token: null });
      return json({ items: [notification('n3'), notification('n2')], next_page_token: 'p2' });
    });
    act(() => root.render(<NotificationsScreen organizationId="org-1" />));
    await settle();
    const read = container.querySelector('#notification-read') as HTMLSelectElement;
    act(() => {
      read.value = 'unread';
      read.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    act(() => button(container, 'Load more').click());
    await settle();
    expect(last(calls.filter(call => call.method === 'GET')).url).toContain('read=unread');
    expect(container.querySelectorAll('.notification-row')).toHaveLength(3);
    const loaded = lists;

    const older = last(Array.from(container.querySelectorAll('.notification-row')));
    await act(async () => { (Array.from(older.querySelectorAll('button')).find(node => node.textContent?.startsWith('Mark read')) as HTMLButtonElement).click(); });
    await settle();
    expect(container.querySelectorAll('.notification-row')).toHaveLength(2);
    expect(lists).toBe(loaded);

    await act(async () => { button(container, 'Mark all read').click(); });
    await settle();
    expect(container.querySelectorAll('.notification-row')).toHaveLength(0);
  });

  it('drops a later page asked for under filters since changed', async () => {
    let releaseOld: (response: Response) => void = () => undefined;
    stubFetch(url => {
      if (url.includes('page_token=p2')) return new Promise<Response>(resolve => { releaseOld = resolve; });
      if (url.includes('kind=')) return json({ items: [notification('n9', { kind: 'price.held', title: 'Held one' })], next_page_token: null });
      return json({ items: [notification('n2')], next_page_token: 'p2' });
    });
    act(() => root.render(<NotificationsScreen organizationId="org-1" />));
    await settle();
    act(() => button(container, 'Load more').click());
    await settle(2);
    const kind = container.querySelector('#notification-kind') as HTMLSelectElement;
    act(() => {
      kind.value = 'price.held';
      kind.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    await act(async () => { releaseOld(json({ items: [notification('n1', { title: 'Stale one' })], next_page_token: 'p3' })); });
    await settle();
    expect(container.textContent).toContain('Held one');
    expect(container.textContent).not.toContain('Stale one');
    expect(container.textContent).not.toContain('Load more');
  });

  it('offers Mark all read when the latest five are read but older ones are not', async () => {
    const calls = stubFetch((url, method) => {
      if (url.endsWith('/unread-count')) return json({ unread: 4 });
      if (method === 'POST') return json({ unread: 0 });
      return json({ items: [notification('n5', { read: true }), notification('n4', { read: true })], next_page_token: 'more' });
    });
    act(() => root.render(<NotificationBell organizationId="org-1" />));
    await settle();
    act(() => button(container, /Notifications, 4 unread/).click());
    await settle();
    await act(async () => { button(container, 'Mark all read').click(); });
    await settle();
    expect(calls.find(call => call.method === 'POST')!.body).toEqual({ through: 'n5' });
  });

  it('ignores a mark made in another organization', async () => {
    stubFetch((url, method) => {
      if (method === 'POST') return json({ unread: 0 });
      return json({ items: [notification('n2'), notification('n1')], next_page_token: null });
    });
    act(() => root.render(<NotificationsScreen organizationId="org-b" />));
    await settle();
    act(() => { dispatchEvent(new CustomEvent('gateway:notifications-read', { detail: { organizationId: 'org-a', mark: { ids: ['n2', 'n1'] } } })); });
    expect(container.querySelectorAll('.notification-row.unread')).toHaveLength(2);
    act(() => { dispatchEvent(new CustomEvent('gateway:notifications-read', { detail: { organizationId: 'org-b', mark: { ids: ['n2'] } } })); });
    expect(container.querySelectorAll('.notification-row.unread')).toHaveLength(1);
  });

  it('shows the latest five on the Overview card', async () => {
    const calls = stubFetch(() => json({ items: [notification('n1'), notification('n2', { read: true })], next_page_token: null }));
    act(() => root.render(<RecentNotifications organizationId="org-1" />));
    await settle();
    expect(calls[0].url).toBe('/api/admin/v1/organizations/org-1/notifications?page_size=5');
    expect(container.textContent).toContain('Recent notifications');
    expect(container.textContent).toContain('Price source n1 is failing');
    expect(container.querySelector('a[href="/admin/notifications"]')).not.toBeNull();
  });
});
