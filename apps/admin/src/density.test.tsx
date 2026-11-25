import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { act } from 'react-dom/test-utils';
import { createRoot, type Root } from 'react-dom/client';
import type { CatalogRoute } from './api';
import { DataTable, type Column } from './data-table';
import { aliasRows, namedApart } from './providers-screen';
import { problemMessage } from './ui';

const route = (id: string, alias: string, operation: CatalogRoute['operation'], deployment = 'd-1') =>
  ({ id, alias, operation, deployment_id: deployment, deployment_name: `${deployment}-name`, model: 'haiku', family: 'bedrock', weight: 1, priority: 1 }) as CatalogRoute;

describe('model aliases', () => {
  it('lists an alias once per deployment, with its operations in order and a stored chat_stream as chat', () => {
    const rows = aliasRows([route('a', 'haiku', 'chat_stream'), route('b', 'haiku', 'chat'), route('c', 'embed', 'embeddings'), route('d', 'haiku', 'chat', 'd-2')]);
    expect(rows.map(row => [row.alias, row.deployment_id, row.operations])).toEqual([
      ['haiku', 'd-1', ['chat']],
      ['embed', 'd-1', ['embeddings']],
      ['haiku', 'd-2', ['chat']],
    ]);
  });

  it('shows a deployment’s name only when it says more than the model', () => {
    expect(namedApart({ name: 'bedrock-main-anthropic.claude-haiku', model: 'anthropic.claude-haiku', adapter_instance_key: 'bedrock-main' })).toBe(false);
    expect(namedApart({ name: 'gpt-5', model: 'gpt-5', adapter_instance_key: 'openai-main' })).toBe(false);
    expect(namedApart({ name: 'gpt-5-prod', model: 'gpt-5', adapter_instance_key: 'openai-main' })).toBe(true);
  });
});

describe('problem sentences', () => {
  const problem = (code: string, detail: string) => ({ type: '', code, title: '', status: 409, detail, instance: '', request_id: '' });
  it('says what to do for common problems, and makes other details a sentence', () => {
    expect(problemMessage(problem('version_conflict', 'resource version conflict; current version is 2'))).toMatch(/^Someone changed this meanwhile/);
    expect(problemMessage(problem('webhook_limit', 'the organization already has the maximum number'))).toBe('The organization already has the maximum number.');
    expect(problemMessage(problem('invalid_price_override', 'cached_input price must be zero'))).toBe('cached_input price must be zero.');
    expect(problemMessage(problem('service_unavailable', 'Sign-in may not have been disabled.'))).toBe('Sign-in may not have been disabled.');
  });
});

describe('table filters', () => {
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
    history.replaceState(null, '', '/');
  });

  it('folds filters, grouping and columns behind one button that counts what is applied', () => {
    type Row = { id: string; kind: string };
    const columns: Column<Row>[] = [{ id: 'id', label: 'ID', primary: true, value: row => row.id }, { id: 'kind', label: 'Kind', filter: 'category', value: row => row.kind }];
    history.replaceState(null, '', '/?t.f.kind=a');
    act(() => root.render(<DataTable label="Things" urlKey="t" columns={columns} rows={[{ id: '1', kind: 'a' }, { id: '2', kind: 'b' }]} rowKey={row => row.id} />));
    const toggle = () => Array.from(container.querySelectorAll('button')).find(node => node.textContent?.startsWith('Filters'))!;
    const panel = document.getElementById(toggle().getAttribute('aria-controls')!)!;
    expect(panel.hidden).toBe(true);
    expect(toggle().getAttribute('aria-expanded')).toBe('false');
    // One filter applied: counted on the button, and shown as a chip.
    expect(toggle().textContent).toBe('Filters 1');
    act(() => toggle().click());
    expect(panel.hidden).toBe(false);
    expect(panel.textContent).toContain('Columns');
    // Folding closes an open menu, so it stops asking for counts.
    const menu = panel.querySelector('details')!;
    menu.open = true;
    act(() => toggle().click());
    expect(menu.open).toBe(false);
    act(() => container.querySelector<HTMLButtonElement>('button[aria-label^="Remove filter"]')!.click());
    expect(toggle().textContent).toBe('Filters');
  });
});
