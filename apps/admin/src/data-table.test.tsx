import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { DataCell, DataGrid, TableBody, TableHead, TableHeader, TableRow } from './ui';
import { DataTable, type Column } from './data-table';

describe('data table cells', () => {
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
  });

  it('renders a named table whose cells keep their content for screen readers', () => {
    act(() =>
      root.render(
        <DataGrid aria-label="Teams">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Name</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            <TableRow className="data-row">
              <DataCell label="Name" className="cell-primary">
                Platform
              </DataCell>
              <DataCell label="Status">Active</DataCell>
              <DataCell label="Actions">
                <button>Edit</button>
              </DataCell>
            </TableRow>
          </TableBody>
        </DataGrid>,
      ),
    );

    const table = container.querySelector('table[aria-label="Teams"]')!;
    expect(Array.from(table.querySelectorAll('thead th')).map(th => th.textContent)).toEqual(['Name', 'Status', 'Actions']);
    const cells = table.querySelectorAll('tbody td');
    expect(cells).toHaveLength(3);
    const expected = ['Name', 'Status', 'Actions'];
    cells.forEach((cell, index) => {
      // An aria-label would replace the cell's content for screen readers.
      expect(cell.getAttribute('aria-label')).toBeNull();
      expect(cell.getAttribute('data-label')).toBe(expected[index]);
    });
    expect(cells[0].textContent).toBe('Platform');
  });
});

type Model = { id: string; model: string; provider: string; tokens: number };
const models: Model[] = Array.from({ length: 30 }, (_, index) => ({
  id: `m-${index}`,
  model: `model-${String(index).padStart(2, '0')}`,
  provider: index % 3 === 0 ? 'bedrock' : 'openai',
  tokens: index,
}));
const columns: Column<Model>[] = [
  { id: 'model', label: 'Model', primary: true, value: row => row.model },
  { id: 'provider', label: 'Provider', filter: 'category', value: row => row.provider, format: value => (value === 'bedrock' ? 'Amazon Bedrock' : 'OpenAI') },
  { id: 'tokens', label: 'Tokens', filter: 'number', sum: true, value: row => row.tokens },
];

describe('DataTable', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    history.replaceState(null, '', '/admin/providers?keep=1');
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  const render = () =>
    act(() =>
      root.render(
        <DataTable
          label="Models"
          urlKey="models"
          columns={columns}
          rows={models}
          rowKey={row => row.id}
          pageSize={10}
          detail={row => <p>Detail of {row.model}</p>}
          actions={row => <button aria-label={`Edit ${row.model}`}>Edit</button>}
        />,
      ),
    );
  const bodyRows = () => Array.from(container.querySelectorAll('table > tbody > .data-row:not(.head):not(.group-row)'));
  const firstCells = () => bodyRows().map(row => row.querySelector('.cell-primary')?.textContent);
  const type = (input: HTMLInputElement, value: string) => {
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!;
    act(() => {
      setter.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const button = (text: string) =>
    Array.from(container.querySelectorAll('button')).find(candidate => candidate.textContent?.trim().startsWith(text))!;

  it('pages, searches and keeps the view in the URL beside other parameters', () => {
    render();
    expect(bodyRows()).toHaveLength(10);
    expect(container.textContent).toContain('Rows 1–10 of 30');
    act(() => button('Next').click());
    expect(firstCells()[0]).toBe('model-10');
    expect(new URLSearchParams(location.search).get('models.p')).toBe('2');

    type(container.querySelector<HTMLInputElement>('input[type="search"]')!, 'model-2');
    expect(firstCells()).toEqual(['model-20', 'model-21', 'model-22', 'model-23', 'model-24', 'model-25', 'model-26', 'model-27', 'model-28', 'model-29']);
    expect(container.textContent).toContain('10 of 30 rows');
    const params = new URLSearchParams(location.search);
    expect(params.get('keep')).toBe('1');
    expect(params.get('models.q')).toBe('model-2');
    expect(params.get('models.p')).toBeNull();

    // A reload reads the same view back.
    act(() => root.unmount());
    root = createRoot(container);
    render();
    expect(container.querySelector<HTMLInputElement>('input[type="search"]')!.value).toBe('model-2');
    expect(bodyRows()).toHaveLength(10);
  });

  it('filters by category with a removable chip, and sorts from the header', () => {
    render();
    const bedrock = Array.from(container.querySelectorAll<HTMLInputElement>('.check-option input')).find(input => input.parentElement?.textContent?.includes('Amazon Bedrock'))!;
    act(() => bedrock.click());
    expect(container.textContent).toContain('10 of 30 rows');
    expect(new URLSearchParams(location.search).get('models.f.provider')).toBe('bedrock');
    act(() => button('Tokens').click());
    act(() => button('Tokens').click());
    expect(container.querySelector('[aria-sort="descending"]')?.textContent).toContain('Tokens');
    expect(firstCells()[0]).toBe('model-27');

    act(() => container.querySelector<HTMLButtonElement>('[aria-label="Remove filter Provider: Amazon Bedrock"]')!.click());
    expect(container.textContent).toContain('30 rows');
    expect(new URLSearchParams(location.search).get('models.f.provider')).toBeNull();
  });

  it('shows plain rows when the URL groups by a column the table does not have', () => {
    history.replaceState(null, '', '/admin/providers?models.g=unknown');
    render();
    expect(bodyRows()).toHaveLength(10);
    expect(container.querySelectorAll('.group-row')).toHaveLength(0);
  });

  it('groups with counts and totals, and expands a row in place', () => {
    render();
    const select = container.querySelector<HTMLSelectElement>('select[aria-label="Group by"]')!;
    act(() => {
      select.value = 'provider';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    const groups = Array.from(container.querySelectorAll('.group-row')).map(row => row.textContent);
    expect(groups).toHaveLength(2);
    expect(groups[0]).toContain('Amazon Bedrock');
    expect(groups[0]).toContain('10 rows');
    // 0 + 3 + … + 27
    expect(groups[0]).toContain('Tokens 135');
    expect(bodyRows()).toHaveLength(0);

    act(() => container.querySelector<HTMLButtonElement>('.group-row button')!.click());
    expect(bodyRows()).toHaveLength(10);
    const expand = container.querySelector<HTMLButtonElement>('button[aria-label="Expand model-00"]')!;
    act(() => expand.click());
    expect(expand.getAttribute('aria-expanded')).toBe('true');
    expect(container.querySelector('[role="region"]')?.textContent).toBe('Detail of model-00');

    act(() => container.querySelector<HTMLButtonElement>('[aria-label="Stop grouping by Provider"]')!.click());
    expect(container.querySelectorAll('.group-row')).toHaveLength(0);
  });
});
