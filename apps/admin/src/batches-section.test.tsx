import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { UsageScreen } from './usage-screen';
import { cancellable, countsText, modeText, stagedConnections, stagingInput, statusTone } from './batches-section';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

describe('batch helpers', () => {
  it('names how a job runs and at which price', () => {
    expect(modeText('native', 'batch')).toBe('Provider batch · batch price');
    expect(modeText('emulated', 'on_demand')).toBe('Emulated (line by line) · on-demand price');
  });

  it('shows outcomes and which batches can still stop', () => {
    expect(countsText({ total: 1200, completed: 3, failed: 1 })).toBe('3 done · 1 failed · 1,200 lines');
    expect(cancellable('in_progress')).toBe(true);
    expect(cancellable('finalizing')).toBe(false);
    expect(statusTone('expired')).toBe('danger');
  });

  it('finds connections and their family in credentials and the catalog', () => {
    expect(stagedConnections(
      [{ adapter_instance_key: 'bedrock-main', family: 'bedrock' }, { adapter_instance_key: 'openai-main', family: 'openai' }],
      [{ adapter_instance_key: 'gemini-env', family: 'gemini' }, { adapter_instance_key: 'bedrock-main', family: 'gemini' }],
    )).toEqual([{ key: 'bedrock-main', family: 'bedrock' }, { key: 'gemini-env', family: 'gemini' }]);
  });

  it('checks staging as the server does', () => {
    const values = { connection: 'bedrock-main', bucket: 'tenant-batches', prefix: 'gw/', region: 'us-east-1', roleArn: 'arn:aws:iam::123456789012:role/batch' };
    expect(stagingInput(values, true)).toEqual({
      input: { bucket: 'tenant-batches', prefix: 'gw/', region: 'us-east-1', role_arn: 'arn:aws:iam::123456789012:role/batch' },
    });
    const refused = stagingInput({ ...values, prefix: 'gw', roleArn: '' }, true);
    expect('errors' in refused && Object.keys(refused.errors).sort()).toEqual(['prefix', 'roleArn']);
    // Cloud Storage names no region or role.
    expect(stagingInput({ ...values, region: '', roleArn: '' }, false)).toEqual({
      input: { bucket: 'tenant-batches', prefix: 'gw/', region: null, role_arn: null },
    });
  });
});

describe('usage batches', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    fetchMock = vi.fn();
    history.replaceState(null, '', '/admin/usage/reconciliation');
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const requested = () => fetchMock.mock.calls.map(([url]) => String(url));
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement;

  const batch = {
    id: 'batch_11111111-1111-1111-1111-111111111111', object: 'batch', endpoint: '/v1/chat/completions', status: 'in_progress',
    input_file_id: 'file-1', output_file_id: null, error_file_id: null, created_at: 1_790_000_000, expires_at: 1_790_086_400,
    request_counts: { total: 3, completed: 1, failed: 0 }, metadata: {}, service_account_id: '22222222-2222-2222-2222-222222222222', team_id: null,
  };
  const detail = {
    ...batch,
    gateway: {
      jobs: [{
        mode: 'emulated', price_source: 'on_demand', provider_family: 'openai', deployment: 'gpt-eu', model: 'gpt-5', state: 'submitted',
        provider_status: 'running', provider_batch_id: null, error_code: null,
        request_counts: { total: 3, completed: 1, failed: 0, pending: 2, unknown: 0 }, submitted_at: null, finished_at: null,
      }],
      cost: [{ currency: 'USD', consumed_minor: 1_040_000_000, reserved_minor: 2_000_000_000 }],
    },
  };

  const serve = (overrides: (url: string, init?: RequestInit) => Response | undefined = () => undefined) =>
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      const answer = overrides(url, init);
      if (answer) return answer;
      if (url.includes('/batch-settings')) return json({ emulated_allowed: false, emulated_concurrency: 4, version: 0, updated_at: null });
      if (url.includes('/batch-staging')) return json({ data: [] });
      if (url.includes('/credentials')) return json({ items: [], next_page_token: null });
      if (url.includes('/provider-accounts')) return json({ items: [], next_page_token: null, release_id: null });
      if (url.endsWith('/cancel')) return json({ ...detail, status: 'cancelling' });
      if (url.includes('/batches/batch_')) return json(detail);
      if (url.includes('/batches')) return json({ items: [batch], next_page_token: null });
      return json({ from: '', to: '', group_by: 'day', totals: [], items: [], next_page_token: null });
    });

  it('lists batches and shows how each job runs, what it cost, and cancels with one key', async () => {
    serve();
    act(() => root.render(<UsageScreen organizationId="org-1" role="organization_admin" />));
    await settle();
    openSection('usage', 'batches');
    await settle();
    expect(requested()).toContain('/api/admin/v1/organizations/org-1/batches?page_size=25');
    expect(container.textContent).toContain('1 done · 0 failed · 3 lines');

    act(() => button('Details').click());
    await settle();
    expect(document.body.textContent).toContain('Emulated (line by line) · on-demand price');
    expect(document.body.textContent).toContain('still held');
    // No results yet: nothing to download.
    expect(document.body.textContent).not.toContain('Download output');
    // The batch's lines on the usage dashboard, over the days it ran.
    const usage = Array.from(document.querySelectorAll('a')).find(node => node.textContent === 'Usage of this batch’s lines')!;
    const params = new URL(usage.getAttribute('href')!, 'http://x').searchParams;
    expect(usage.getAttribute('href')).toMatch(/^\/admin\/usage\?/);
    expect([params.get('u.f.batch'), params.get('u.range'), params.get('u.from'), params.get('u.to')]).toEqual([batch.id, 'custom', '2026-09-21', '2026-09-22']);

    act(() => button('Cancel batch').click());
    const confirm = Array.from(document.querySelectorAll('[role="alertdialog"] button, .cds--modal button'))
      .filter(node => node.textContent === 'Cancel batch').pop() as HTMLButtonElement;
    act(() => confirm.click());
    await settle();
    const cancel = fetchMock.mock.calls.find(([url]) => String(url).endsWith('/cancel'))!;
    expect(cancel[0]).toBe(`/api/admin/v1/organizations/org-1/batches/${encodeURIComponent(batch.id)}/cancel`);
    expect(new Headers((cancel[1] as RequestInit).headers).get('Idempotency-Key')).toBeTruthy();
  });

  it('offers no changes to a read-only role', async () => {
    serve();
    act(() => root.render(<UsageScreen organizationId="org-1" role="read_only_member" />));
    await settle();
    openSection('usage', 'batches');
    await settle();
    expect(container.textContent).toContain('can view the batch settings but not change them');
    expect(button('Add staging')).toBeUndefined();
    act(() => button('Details').click());
    await settle();
    expect(button('Cancel batch')).toBeUndefined();
  });

  const typeInto = (input: HTMLInputElement, value: string) => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    act(() => {
      setter.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };

  it('stages a Gemini connection the process configures in Cloud Storage', async () => {
    serve(url => url.includes('/provider-accounts')
      ? json({ items: [{ id: 'p-1', release_id: 'r-1', adapter_instance_key: 'gemini-env', family: 'gemini', deployment_count: 1 }], next_page_token: null, release_id: 'r-1' })
      : undefined);
    act(() => root.render(<UsageScreen organizationId="org-1" role="organization_owner" />));
    await settle();
    openSection('usage', 'batches');
    await settle();
    act(() => button('Add staging').click());
    await settle();
    // Unknown connection: the storage is picked, S3 by default.
    expect(document.getElementById('staging-kind')).not.toBeNull();
    expect(document.getElementById('staging-role')).not.toBeNull();
    typeInto(document.getElementById('staging-connection') as HTMLInputElement, 'gemini-env');
    await settle();
    // Known from the catalog as Gemini's: Cloud Storage, no AWS fields.
    expect(document.getElementById('staging-kind')).toBeNull();
    expect(document.getElementById('staging-role')).toBeNull();
    expect(document.body.textContent).toContain('Cloud Storage bucket');
    typeInto(document.getElementById('staging-bucket') as HTMLInputElement, 'tenant-batches');
    act(() => button('Save staging').click());
    await settle();
    const put = fetchMock.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'PUT')!;
    expect(put[0]).toBe('/api/admin/v1/organizations/org-1/batch-staging/gemini-env');
    expect(JSON.parse(String((put[1] as RequestInit).body))).toEqual({ bucket: 'tenant-batches', region: null, role_arn: null });
  });

  it('allows emulated batches with the settings version as precondition', async () => {
    serve();
    act(() => root.render(<UsageScreen organizationId="org-1" role="organization_owner" />));
    await settle();
    openSection('usage', 'batches');
    await settle();
    const allow = Array.from(container.querySelectorAll('input[type="checkbox"]'))[0] as HTMLInputElement;
    act(() => allow.click());
    act(() => (allow.form as HTMLFormElement).requestSubmit());
    await settle();
    const put = fetchMock.mock.calls.find(([, init]) => (init as RequestInit | undefined)?.method === 'PUT')!;
    expect(put[0]).toBe('/api/admin/v1/organizations/org-1/batch-settings');
    expect(JSON.parse(String((put[1] as RequestInit).body))).toEqual({ emulated_allowed: true, emulated_concurrency: 4 });
    expect(new Headers((put[1] as RequestInit).headers).get('If-Match')).toBe('*');
  });
});
