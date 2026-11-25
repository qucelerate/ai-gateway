import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { UsageExportSection } from './usage-export-section';

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const problem = (status: number, code: string, detail: string) =>
  json({ type: 'about:blank', code, title: code, status, detail, instance: '/x', request_id: 'r-1' }, status);
const credentials = [
  { adapter_instance_key: 'bedrock-123456789012', family: 'bedrock', key_id: 'k', version: 1, created_at: '2026-10-01T00:00:00Z', rotated_at: '2026-10-01T00:00:00Z' },
  { adapter_instance_key: 'gemini-key', family: 'gemini', key_id: 'k', version: 1, created_at: '2026-10-01T00:00:00Z', rotated_at: '2026-10-01T00:00:00Z' },
  { adapter_instance_key: 'gemini-sa', family: 'gemini', key_id: 'k', version: 1, created_at: '2026-10-01T00:00:00Z', rotated_at: '2026-10-01T00:00:00Z', cloud_principal: 'pending' },
];
const saved = {
  id: 'e-1', kind: 's3', connection: 'bedrock-123456789012', bucket: 'usage-lake', account: null, region: 'eu-west-1', prefix: 'gateway',
  enabled_at: '2026-10-27T00:00:00Z', verified_at: '2026-10-27T00:00:00Z', last_success_at: null, failing: false, failing_since: null,
  last_error: null, queued_records: 12, oldest_queued_at: '2026-10-27T00:05:00Z', version: 3, created_at: '2026-10-27T00:00:00Z', updated_at: '2026-10-27T00:00:00Z',
};

describe('usage export section', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let current: unknown;
  let putResponse: () => Response;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    current = null;
    putResponse = () => json({ ...saved, version: 1 }, 201);
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.includes('/catalog/credentials')) return json({ items: credentials, next_page_token: null });
      if (url.endsWith('/usage-export')) {
        if (init?.method === 'PUT') return putResponse();
        if (init?.method === 'DELETE') return new Response(null, { status: 204 });
        return current ? json(current) : problem(404, 'not_found', 'resource does not exist in the requested scope');
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
    for (let i = 0; i < 6; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement;
  const render = async (canChange = true) => {
    act(() => root.render(<UsageExportSection organizationId="org-1" canChange={canChange} />));
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
  const choose = (id: string, value: string) => {
    const select = document.getElementById(id) as HTMLSelectElement;
    act(() => {
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
  };
  const calls = (method: string) => fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === method);
  const fillS3 = async () => {
    act(() => button('Set up export').click());
    await settle();
    choose('usage-export-connection', 'bedrock-123456789012');
    setInput('usage-export-bucket', 'usage-lake');
    setInput('usage-export-prefix', 'gateway');
    return setInput('usage-export-region', 'eu-west-1');
  };

  it('offers set up when no export exists, and not to read-only roles', async () => {
    await render();
    expect(container.textContent).toContain('Not set up.');
    expect(button('Set up export')).toBeDefined();
    act(() => root.unmount());
    root = createRoot(container);
    await render(false);
    expect(button('Set up export')).toBeUndefined();
  });

  it('creates an export with a fresh key and If-Match *, offering only matching connections', async () => {
    await render();
    const input = await fillS3();
    const options = Array.from((document.getElementById('usage-export-connection') as HTMLSelectElement).options).map(option => option.value);
    expect(options).toEqual(['', 'bedrock-123456789012']);
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[url, init]] = calls('PUT');
    expect(url).toBe('/api/admin/v1/organizations/org-1/usage-export');
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({
      kind: 's3', connection: 'bedrock-123456789012', bucket: 'usage-lake', account: null, region: 'eu-west-1', prefix: 'gateway',
    });
    const headers = new Headers((init as RequestInit).headers);
    expect(headers.get('If-Match')).toBe('*');
    expect(headers.get('Idempotency-Key')).toBeTruthy();
    expect(container.textContent).toContain('s3://usage-lake/gateway/ (eu-west-1)');
  });

  it('explains an unverified destination and retries with the same key', async () => {
    putResponse = () => problem(422, 'destination_unverified', 'the verification object could not be written: storage denied the write');
    await render();
    const input = await fillS3();
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(document.body.textContent).toContain('Grant the connection write access to the prefix');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [first, second] = calls('PUT').map(([, init]) => new Headers((init as RequestInit).headers).get('Idempotency-Key'));
    expect(second).toBe(first);
  });

  it('marks connections awaiting operator approval', async () => {
    await render();
    act(() => button('Set up export').click());
    await settle();
    choose('usage-export-kind', 'gcs');
    const labels = Array.from((document.getElementById('usage-export-connection') as HTMLSelectElement).options).map(option => option.textContent);
    expect(labels).toContain('gemini-sa (awaiting operator approval)');
    expect(labels.some(label => label?.startsWith('gemini-key'))).toBe(false);
    expect(document.getElementById('usage-export-region')).toBeNull();
  });

  it('shows a failing export and changes it against its version', async () => {
    current = { ...saved, failing: true, failing_since: '2026-10-27T03:00:00Z', last_error: 'storage denied the write' };
    putResponse = () => json({ ...saved, bucket: 'usage-lake-2', version: 4 });
    await render();
    expect(container.querySelector('.badge-danger')?.textContent).toBe('Failing');
    expect(container.textContent).toContain('storage denied the write');
    act(() => button('Change destination').click());
    await settle();
    expect((document.getElementById('usage-export-bucket') as HTMLInputElement).value).toBe('usage-lake');
    const input = setInput('usage-export-bucket', 'usage-lake-2');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [[, init]] = calls('PUT');
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v3"');
    expect(container.textContent).toContain('s3://usage-lake-2/gateway/');
  });

  it('reloads after a version conflict and saves against the new version', async () => {
    current = saved;
    let attempts = 0;
    putResponse = () => {
      attempts += 1;
      return attempts === 1 ? problem(409, 'version_conflict', 'resource version conflict; current version is 4') : json({ ...saved, bucket: 'usage-lake-2', version: 5 });
    };
    await render();
    act(() => button('Change destination').click());
    await settle();
    const input = setInput('usage-export-bucket', 'usage-lake-2');
    current = { ...saved, prefix: 'other', version: 4 };
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    expect(document.body.textContent).toContain('it now writes to s3://usage-lake/other/');
    expect((document.getElementById('usage-export-bucket') as HTMLInputElement).value).toBe('usage-lake-2');
    act(() => (input.form as HTMLFormElement).requestSubmit());
    await settle();
    const [first, second] = calls('PUT').map(([, init]) => new Headers((init as RequestInit).headers));
    expect(first.get('If-Match')).toBe('"v3"');
    expect(second.get('If-Match')).toBe('"v4"');
    expect(second.get('Idempotency-Key')).not.toBe(first.get('Idempotency-Key'));
  });

  it('refreshes the status on request', async () => {
    current = saved;
    await render();
    expect(container.textContent).toContain('Waiting for the first batch');
    current = { ...saved, last_success_at: '2026-10-27T01:00:00Z', queued_records: 0 };
    act(() => button('Refresh').click());
    await settle();
    expect(container.querySelector('.badge-success')?.textContent).toBe('Writing');
  });

  it('ignores a refresh that was in flight when the export was stopped', async () => {
    current = saved;
    await render();
    let answer: (response: Response) => void = () => {};
    const base = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/usage-export') && !init?.method) return new Promise<Response>(resolve => { answer = resolve; });
      return base(url, init);
    });
    act(() => button('Refresh').click());
    await settle();
    act(() => button('Stop export').click());
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Stop export') as HTMLButtonElement).click());
    await settle();
    expect(container.textContent).toContain('Not set up.');
    await act(async () => { answer(json(saved)); });
    await settle();
    expect(container.textContent).toContain('Not set up.');
  });

  it('stops the export against its version, warning about queued records', async () => {
    current = saved;
    await render();
    act(() => button('Stop export').click());
    expect(document.body.textContent).toContain('12 records not yet written will be discarded.');
    act(() => (Array.from(document.querySelectorAll('.dialog button')).find(node => node.textContent === 'Stop export') as HTMLButtonElement).click());
    await settle();
    const [[, init]] = calls('DELETE');
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('"v3"');
    expect(container.textContent).toContain('Not set up.');
  });
});
