import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { PoliciesScreen } from './policies-screen';
import { openSection } from './test-route';

const json = (body: unknown, status = 200, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json', ...headers } });
const problem = (status: number, code: string, detail = '') =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '', request_id: 'req-9' }, status);
const BASE = '/api/admin/v1/organizations/org-1/webhook-endpoints';
const endpoint = {
  id: 'e-1', url: 'https://hooks.example.com/gateway', description: 'Billing', event_types: ['inference.finalized', 'reconciliation.updated'],
  enabled: true, created_at: '2026-10-01T09:00:00Z', updated_at: '2026-10-01T09:00:00Z', version: 3,
};
const parked = {
  id: 'd-1', event_id: 'ev-1', event_type: 'inference.finalized', status: 'parked', attempts: 24, next_attempt_at: null,
  last_attempt_at: '2026-10-02T00:00:00Z', last_status_code: 503, last_error: null, delivered_at: null, created_at: '2026-10-01T10:00:00Z',
};
const delivered = { ...parked, id: 'd-2', event_id: 'ev-2', status: 'delivered', attempts: 1, last_status_code: 204, delivered_at: '2026-10-01T10:00:01Z' };
const SECRET = 'whsec_c2VjcmV0LXZhbHVlLWZvci10ZXN0aW5nLW9ubHk=';

type Handler = (url: string, init?: RequestInit) => Response | Promise<Response> | undefined;

describe('webhooks tab', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let endpoints: unknown[];
  let deliveries: unknown[];
  let override: Handler;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    endpoints = [endpoint];
    deliveries = [parked, delivered];
    override = () => undefined;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      const custom = await override(url, init);
      if (custom) return custom;
      const method = init?.method ?? 'GET';
      if (url.includes('/redeliver')) return json({ ...parked, status: 'pending', next_attempt_at: '2026-10-13T00:00:00Z' });
      if (url.includes('/deliveries')) return json({ items: deliveries, next_page_token: null });
      if (url.endsWith('/rotate-secret')) return json({ endpoint: { ...endpoint, version: 4 }, secret: SECRET, replay: false, error: null }, 200, { 'x-request-id': 'req-rot' });
      if (method === 'POST') {
        const body = JSON.parse(String(init!.body));
        return json({ endpoint: { ...endpoint, ...body, id: 'e-2', version: 1 }, secret: SECRET, replay: false, error: null }, 201, { 'x-request-id': 'req-new' });
      }
      if (method === 'PATCH') return json({ ...endpoint, ...JSON.parse(String(init!.body)), version: 4 });
      if (method === 'DELETE') return new Response(null, { status: 204 });
      if (url.startsWith(BASE)) return json({ items: endpoints, next_page_token: null });
      return json({ items: [], next_page_token: null });
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
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement | undefined;
  const click = async (text: string) => {
    const target = button(text);
    if (!target) throw new Error(`no button “${text}”`);
    act(() => target.click());
    await settle();
  };
  const dialogButton = async (text: string) => {
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === text) as HTMLButtonElement).click());
    await settle();
  };
  const open = async (role = 'Owner') => {
    act(() => root.render(<PoliciesScreen organizationId="org-1" role={role} />));
    await settle();
    openSection('policies', 'webhooks');
    await settle();
  };
  const setInput = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    return input;
  };
  const check = (id: string) => act(() => (document.getElementById(id) as HTMLInputElement).click());
  const submit = async () => {
    act(() => (document.getElementById('webhook-url') as HTMLInputElement).form!.requestSubmit());
    await settle();
  };
  const calls = (method: string) => fetchMock.mock.calls.filter(([, init]) => ((init as RequestInit | undefined)?.method ?? 'GET') === method);
  const headers = (call: unknown[]) => new Headers((call[1] as RequestInit).headers);

  it('lists each endpoint’s URL, events, status and creation time', async () => {
    await open();
    const table = container.querySelector('[aria-label="Webhook endpoints"]')!;
    expect(table.textContent).toContain('https://hooks.example.com/gateway');
    expect(table.textContent).toContain('Billing');
    expect(table.textContent).toContain('inference.finalized');
    expect(table.textContent).toContain('reconciliation.updated');
    expect(table.textContent).toContain('Enabled');
    expect(table.textContent).toContain(new Date('2026-10-01T09:00:00Z').toLocaleString());
  });

  it('says when there are no endpoints', async () => {
    endpoints = [];
    await open();
    expect(container.querySelector('.empty[role="status"]')?.textContent).toContain('No webhook endpoints');
  });

  it('creates an endpoint and shows its signing secret once', async () => {
    endpoints = [];
    await open();
    await click('Add endpoint');
    expect(Array.from(document.querySelectorAll('.dialog input[type="checkbox"]')).map(node => node.id)).toContain('webhook-event-reconciliation.updated');
    setInput('webhook-url', 'https://hooks.example.com/new');
    check('webhook-event-reconciliation.updated');
    await submit();
    const [post] = calls('POST');
    expect(post[0]).toBe(BASE);
    expect(JSON.parse(String((post[1] as RequestInit).body))).toEqual({
      url: 'https://hooks.example.com/new', description: null, event_types: ['reconciliation.updated'], enabled: true,
    });
    expect(headers(post).get('Idempotency-Key')).toBeTruthy();
    const panel = container.querySelector('.secret-panel')!;
    expect(panel.getAttribute('role')).toBe('alert');
    expect(panel.querySelector('[aria-label="Webhook signing secret"]')?.textContent).toBe(SECRET);
    expect(panel.textContent).toContain('ref: req-new');
    // Nothing that returns another secret can replace this one before it is saved.
    expect(button('Add endpoint')!.disabled).toBe(true);
    expect(container.textContent).toContain('Save the signing secret shown above, then dismiss it');
    await click('I have saved it');
    expect(button('Add endpoint')!.disabled).toBe(false);
    expect(document.body.textContent).not.toContain(SECRET);
    expect(JSON.stringify(window.localStorage)).not.toContain(SECRET);
    expect(JSON.stringify(window.sessionStorage)).not.toContain(SECRET);
  });

  it('says a replayed create cannot show the secret again, instead of an empty secret', async () => {
    override = (url, init) => (init?.method === 'POST' && url === BASE
      ? json({ endpoint: { ...endpoint, id: 'e-2' }, secret: null, replay: true, error: 'secret_unavailable_on_replay' })
      : undefined);
    await open();
    await click('Add endpoint');
    setInput('webhook-url', 'https://hooks.example.com/new');
    check('webhook-event-inference.finalized');
    await submit();
    expect(container.querySelector('.secret-panel')).toBeNull();
    expect(container.querySelector('[role="alert"]:not([class*="counter-alert"])')?.textContent).toContain('its signing secret can’t be shown again');
    expect(container.textContent).toContain('use Rotate secret to get a new one');
  });

  it('explains that an operator must configure credential encryption first', async () => {
    override = (url, init) => (init?.method === 'POST' ? problem(503, 'credential_encryption_unavailable', 'webhook secret storage is not configured on this gateway') : undefined);
    await open();
    await click('Add endpoint');
    setInput('webhook-url', 'https://hooks.example.com/new');
    check('webhook-event-inference.finalized');
    await submit();
    expect(document.querySelector('.dialog')?.textContent).toContain('An operator must set GATEWAY_CREDENTIAL_ENCRYPTION_KEY');
    expect((document.getElementById('webhook-url') as HTMLInputElement).value).toBe('https://hooks.example.com/new');
  });

  it('keeps the form and explains a 403', async () => {
    override = (url, init) => (init?.method === 'POST' ? problem(403, 'forbidden', 'administrative permission denied') : undefined);
    await open();
    await click('Add endpoint');
    setInput('webhook-url', 'https://hooks.example.com/new');
    check('webhook-event-inference.finalized');
    await submit();
    expect(document.querySelector('.dialog [role="alert"]:not([class*="counter-alert"])')?.textContent).toContain('Only organization owners and admins can change webhooks.');
    expect((document.getElementById('webhook-url') as HTMLInputElement).value).toBe('https://hooks.example.com/new');
    expect((document.getElementById('webhook-event-inference.finalized') as HTMLInputElement).checked).toBe(true);
  });

  it('reports the endpoint limit from the server', async () => {
    override = (url, init) => (init?.method === 'POST' ? problem(409, 'webhook_endpoint_limit_reached', 'The organization already has the maximum number of webhook endpoints') : undefined);
    await open();
    await click('Add endpoint');
    setInput('webhook-url', 'https://hooks.example.com/new');
    check('webhook-event-inference.finalized');
    await submit();
    expect(document.querySelector('.dialog')?.textContent).toContain('The organization already has the maximum number of webhook endpoints');
  });

  it('checks the form before sending it', async () => {
    await open();
    await click('Add endpoint');
    setInput('webhook-url', 'http://hooks.example.com');
    await submit();
    expect(calls('POST')).toHaveLength(0);
    expect(document.querySelector('.dialog')?.textContent).toContain('Use an https:// URL.');
    expect(document.querySelector('.dialog')?.textContent).toContain('Choose at least one event.');
  });

  it('edits only what changed, against the endpoint’s ETag', async () => {
    await open();
    await click('Edit');
    check('webhook-enabled');
    check('webhook-event-reconciliation.updated');
    await submit();
    const [patch] = calls('PATCH');
    expect(patch[0]).toBe(`${BASE}/e-1`);
    expect(JSON.parse(String((patch[1] as RequestInit).body))).toEqual({ event_types: ['inference.finalized'], enabled: false });
    expect(headers(patch).get('If-Match')).toBe('"v3"');
    expect(headers(patch).get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('Endpoint saved.');
  });

  it('after a version conflict, loads the current endpoint and keeps the edits', async () => {
    let conflicts = true;
    override = (url, init) => {
      if (init?.method === 'PATCH' && conflicts) return problem(409, 'version_conflict', 'resource version conflict; current version is 5');
      if (!init?.method && url === `${BASE}/e-1`) return json({ ...endpoint, description: 'Changed elsewhere', version: 5, updated_at: '2026-10-05T00:00:00Z' });
      return undefined;
    };
    await open();
    await click('Edit');
    setInput('webhook-url', 'https://hooks.example.com/v2');
    await submit();
    expect(document.querySelector('.dialog')?.textContent).toContain('Someone changed this endpoint since you opened it.');
    await click('Load the current endpoint');
    expect((document.getElementById('webhook-url') as HTMLInputElement).value).toBe('https://hooks.example.com/v2');
    // The untouched description takes the other admin's change.
    expect((document.getElementById('webhook-description') as HTMLInputElement).value).toBe('Changed elsewhere');
    conflicts = false;
    await submit();
    const [first, second] = calls('PATCH');
    expect(headers(first).get('If-Match')).toBe('"v3"');
    expect(headers(second).get('If-Match')).toBe('"v5"');
    // Only the URL was edited, so only it is sent.
    expect(JSON.parse(String((second[1] as RequestInit).body))).toEqual({ url: 'https://hooks.example.com/v2' });
  });

  it('deletes an endpoint only after confirming, with its ETag', async () => {
    await open();
    await click('Delete');
    expect(document.querySelector('[role="alertdialog"]')?.textContent).toContain('all of its deliveries are deleted');
    expect(calls('DELETE')).toHaveLength(0);
    await dialogButton('Delete endpoint');
    const [del] = calls('DELETE');
    expect(del[0]).toBe(`${BASE}/e-1`);
    expect(headers(del).get('If-Match')).toBe('"v3"');
    expect(headers(del).get('Idempotency-Key')).toBeTruthy();
    expect(container.querySelector('[role="status"].cds--inline-notification--success')?.textContent).toContain('Endpoint deleted');
  });

  it('rotates the secret after confirming, and shows the new one once', async () => {
    await open();
    await click('Rotate secret');
    await dialogButton('Rotate secret');
    const [rotate] = calls('POST');
    expect(rotate[0]).toBe(`${BASE}/e-1/rotate-secret`);
    expect(headers(rotate).get('If-Match')).toBe('"v3"');
    expect(container.querySelector('.secret-panel')?.textContent).toContain(SECRET);
    expect(button('Rotate secret')!.disabled).toBe(true);
  });

  it('says a replayed rotation cannot show the secret again', async () => {
    override = url => (url.endsWith('/rotate-secret') ? json({ endpoint, secret: null, replay: true, error: 'secret_unavailable_on_replay' }) : undefined);
    await open();
    await click('Rotate secret');
    await dialogButton('Rotate secret');
    expect(container.querySelector('.secret-panel')).toBeNull();
    expect(container.textContent).toContain('so it can’t be shown again. If you didn’t save it, rotate again.');
  });

  it('offers Redeliver on parked deliveries only, and shows the delivery returned', async () => {
    await open();
    await click('Deliveries');
    const table = container.querySelector('[aria-label="Webhook deliveries"]')!;
    const rows = Array.from(table.querySelectorAll('.data-row:not(.head)'));
    expect(rows).toHaveLength(2);
    expect(rows[0].textContent).toContain('parked');
    expect(rows[0].textContent).toContain('HTTP 503');
    expect(rows[0].textContent).toContain('24');
    expect(rows[0].textContent).toContain('Redeliver');
    expect(rows[1].textContent).toContain('delivered');
    expect(rows[1].textContent).not.toContain('Redeliver');
    await click('Redeliver');
    const [redeliver] = calls('POST');
    expect(redeliver[0]).toBe(`${BASE}/e-1/deliveries/d-1/redeliver`);
    expect(headers(redeliver).get('Idempotency-Key')).toBeTruthy();
    expect(headers(redeliver).get('If-Match')).toBeNull();
    const first = container.querySelector('[aria-label="Webhook deliveries"] .data-row:not(.head)')!;
    expect(first.textContent).toContain('pending');
    expect(first.textContent).not.toContain('Redeliver');
    expect(container.textContent).toContain('Delivery queued again');
  });

  it('redelivers a delivery that parked again as a new request', async () => {
    await open();
    await click('Deliveries');
    await click('Redeliver');
    // Its retries fail and it parks again.
    await click('Refresh');
    await click('Redeliver');
    const [first, second] = calls('POST').map(headers);
    expect(second.get('Idempotency-Key')).not.toBe(first.get('Idempotency-Key'));
  });

  it('retries a redelivery whose outcome is unknown with the same key', async () => {
    let lost = true;
    override = url => {
      if (url.includes('/redeliver') && lost) {
        lost = false;
        throw new TypeError('response lost');
      }
      return undefined;
    };
    await open();
    await click('Deliveries');
    await click('Redeliver');
    await click('Redeliver');
    const [first, second] = calls('POST').map(headers);
    expect(second.get('Idempotency-Key')).toBe(first.get('Idempotency-Key'));
  });

  it('retries a redelivery after a server error with the same key', async () => {
    let failing = true;
    override = url => {
      if (url.includes('/redeliver') && failing) {
        failing = false;
        return problem(503, 'service_unavailable');
      }
      return undefined;
    };
    await open();
    await click('Deliveries');
    await click('Redeliver');
    await click('Redeliver');
    const [first, second] = calls('POST').map(headers);
    expect(second.get('Idempotency-Key')).toBe(first.get('Idempotency-Key'));
  });

  it('says when a delivery was no longer parked', async () => {
    override = url => (url.includes('/redeliver') ? problem(409, 'webhook_delivery_not_parked', 'only a parked delivery can be redelivered') : undefined);
    await open();
    await click('Deliveries');
    await click('Redeliver');
    expect(container.textContent).toContain('That delivery is no longer parked, so it was not redelivered.');
  });

  it('says when an endpoint has no deliveries yet', async () => {
    deliveries = [];
    await open();
    await click('Deliveries');
    expect(container.textContent).toContain('No deliveries yet');
  });

  it('lets auditors and read-only members read, without change controls', async () => {
    for (const role of ['Auditor', 'ReadOnly']) {
      await open(role);
      expect(container.textContent).toContain('https://hooks.example.com/gateway');
      for (const text of ['Add endpoint', 'Edit', 'Rotate secret', 'Delete']) expect(button(text)).toBeUndefined();
      await click('Deliveries');
      expect(container.textContent).toContain('parked');
      expect(button('Redeliver')).toBeUndefined();
    }
  });

  it('explains a 403 when reading endpoints', async () => {
    override = url => (url.startsWith(`${BASE}?`) || url === BASE ? problem(403, 'forbidden') : undefined);
    await open();
    expect(container.textContent).toContain('You do not have permission to view this organization’s webhook endpoints.');
  });
});
