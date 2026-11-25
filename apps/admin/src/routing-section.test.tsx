import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import type { ConfigInstance, ConfigPin, ConfigVersion } from './api';
import { useMutationState } from './mutation-state';
import { useCollection } from './use-collection';
import { api } from './api';
import { RoutingSection, instancesBehind } from './routing-section';

const ORG = 'b6c2a7a4-6f3e-4ad8-8f43-2a9d1d4f5e01';
const OLD = '11111111-1111-4111-8111-111111111111';
const NEW = '22222222-2222-4222-8222-222222222222';

const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-1' }, status);

const version = (id: string, sequence: number, flags: Partial<ConfigVersion>): ConfigVersion => ({
  id,
  sequence,
  digest: 'ab'.repeat(32),
  release_id: null,
  routes: 2,
  overrides: 0,
  served: false,
  pinned: false,
  newest: false,
  created_at: '2026-10-27T10:00:00Z',
  ...flags,
});
const instance = (id: string, serving: string | null, stale = false): ConfigInstance => ({
  instance_id: id,
  profile: 'data-plane',
  region: 'eu-west',
  build: 'abc1234',
  applied_sequence: 7,
  serving_version_id: serving,
  loaded_at: '2026-10-27T10:00:00Z',
  started_at: '2026-10-27T09:00:00Z',
  last_seen_at: '2026-10-27T10:00:00Z',
  stale,
});
const unpinned: ConfigPin = { pinned_version_id: null, served_version_id: NEW, newest_version_id: NEW, version: 0 };
const pinned: ConfigPin = { pinned_version_id: OLD, served_version_id: OLD, newest_version_id: NEW, version: 1 };

describe('routing helpers', () => {
  it('counts stale instances and those serving another version as behind', () => {
    const instances = [instance('a', NEW), instance('b', OLD), instance('c', NEW, true)];
    expect(instancesBehind(instances, unpinned).map(i => i.instance_id)).toEqual(['b', 'c']);
    expect(instancesBehind(instances, null)).toEqual([]);
  });
});

describe('the routing section', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let pin: ConfigPin;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    pin = unpinned;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/config-versions/pin') && init?.method === 'PUT') {
        pin = pinned;
        return json(pinned, 200, { etag: '"v1"' });
      }
      if (url.endsWith('/config-versions/pin') && init?.method === 'DELETE') {
        pin = { ...unpinned, version: 2 };
        return json(pin, 200, { etag: '"v2"' });
      }
      if (url.endsWith('/config-versions/pin')) return json(pin, 200, { etag: `"v${pin.version}"` });
      if (url.includes('/config-versions')) {
        return json({
          items: [
            version(NEW, 2, { newest: true, served: pin.pinned_version_id === null }),
            version(OLD, 1, { pinned: pin.pinned_version_id === OLD, served: pin.pinned_version_id === OLD }),
          ],
          next_page_token: null,
        });
      }
      if (url.endsWith('/config-instances')) {
        return json({ items: [instance('dp-eu-1', NEW), instance('dp-us-1', OLD, true)], next_page_token: null });
      }
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
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
  };
  function Harness() {
    const mutation = useMutationState<unknown>();
    const versions = useCollection(
      React.useCallback(params => api.listConfigVersions(ORG, params), []),
      [],
    );
    return (
      <RoutingSection
        organizationId={ORG}
        versions={versions}
        mutation={mutation}
        onSuccess={() => { void versions.load(true); }}
      />
    );
  }
  const render = async () => {
    act(() => root.render(<Harness />));
    await settle();
  };
  const buttons = (text: string) =>
    Array.from(document.querySelectorAll('button')).filter(node => node.textContent === text) as HTMLButtonElement[];
  const sent = (method: string) =>
    fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === method) as [string, RequestInit][];

  it('shows the versions, the pin and what each instance serves', async () => {
    await render();
    expect(container.textContent).toContain('Serving the newest version');
    expect(container.querySelector('[aria-label="Routing versions"]')?.textContent).toContain('newest');
    const instances = container.querySelector('[aria-label="Gateway instances"]')!.textContent!;
    expect(instances).toContain('dp-eu-1');
    expect(instances).toContain('stale');
    expect(instances).toContain('behind');
    expect(container.textContent).toContain('1 of 2 not serving the current version yet');
  });

  it('pins an earlier version with the pin version as If-Match, then unpins', async () => {
    await render();
    expect(buttons('Pin')).toHaveLength(2);
    act(() => buttons('Pin')[1].click());
    act(() => buttons('Pin')[2].click());
    await settle();
    const [[url, init]] = sent('PUT');
    expect(url).toContain(`/organizations/${ORG}/config-versions/pin`);
    expect(JSON.parse(init.body as string)).toEqual({ version_id: OLD });
    const headers = new Headers(init.headers);
    expect(headers.get('If-Match')).toBe('"v0"');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('Pinned to version');
    expect(container.textContent).toContain('Newer versions are not served.');

    act(() => buttons('Unpin')[0].click());
    act(() => buttons('Unpin')[1].click());
    await settle();
    const [[, unpin]] = sent('DELETE');
    expect(new Headers(unpin.headers).get('If-Match')).toBe('"v1"');
    expect(container.textContent).toContain('Serving the newest version');
  });

  it('offers Pin whatever the membership role, and shows the server refusing it', async () => {
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (init?.method === 'PUT') return problem(403, 'forbidden', 'Your role does not allow this');
      if (url.endsWith('/config-versions/pin')) return json(unpinned);
      if (url.includes('/config-versions')) return json({ items: [version(OLD, 1, {})], next_page_token: null });
      return json({ items: [], next_page_token: null });
    });
    await render();
    act(() => buttons('Pin')[0].click());
    act(() => buttons('Pin')[1].click());
    await settle();
    expect(document.body.textContent).toContain('Your role does not allow this');
  });

  it('shows a conflict and keeps the dialog open', async () => {
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (init?.method === 'PUT') return problem(409, 'version_conflict', 'resource version conflict; current version is 3');
      if (url.endsWith('/config-versions/pin')) return json(unpinned);
      if (url.includes('/config-versions')) return json({ items: [version(OLD, 1, {})], next_page_token: null });
      return json({ items: [], next_page_token: null });
    });
    await render();
    act(() => buttons('Pin')[0].click());
    act(() => buttons('Pin')[1].click());
    await settle();
    expect(document.body.textContent).toContain('Someone changed this meanwhile');
    expect(container.textContent).toContain('No gateway instance has reported');
  });
});
