import { describe, expect, it } from 'vitest';
import {
  activeChips,
  categoryCounts,
  emptyView,
  filterRows,
  groupRows,
  knownView,
  readView,
  sortRows,
  visibleColumns,
  writeView,
  type ColumnSpec,
  type TableView,
} from './table-view';

type Row = { model: string; provider: string; region: string | null; ops: string[]; price: number | null; tokens: number; at: string };

const rows: Row[] = [
  { model: 'claude-haiku', provider: 'bedrock', region: 'us-east-2', ops: ['chat', 'chat_stream'], price: 1.1, tokens: 100, at: '2026-10-01T00:00:00Z' },
  { model: 'gpt-5', provider: 'openai', region: null, ops: ['chat'], price: 1.25, tokens: 50, at: '2026-10-05T00:00:00Z' },
  { model: 'titan-embed', provider: 'bedrock', region: 'us-east-1', ops: ['embeddings'], price: 0.02, tokens: 7, at: '2026-10-10T00:00:00Z' },
  { model: 'gpt-oss-20b', provider: 'bedrock', region: 'us-east-2', ops: ['chat'], price: null, tokens: 0, at: '2026-10-12T00:00:00Z' },
];

const columns: ColumnSpec<Row>[] = [
  { id: 'model', label: 'Model', value: row => row.model, search: row => `${row.model}-prod` },
  { id: 'provider', label: 'Provider', filter: 'category', value: row => row.provider, format: value => (value === 'bedrock' ? 'Amazon Bedrock' : 'OpenAI') },
  { id: 'region', label: 'Region', filter: 'category', value: row => row.region },
  { id: 'ops', label: 'Operations', filter: 'category', value: row => row.ops },
  { id: 'price', label: 'Price', filter: 'number', value: row => row.price },
  { id: 'tokens', label: 'Tokens', filter: 'number', sum: true, hidden: true, value: row => row.tokens },
  { id: 'at', label: 'When', filter: 'date', hidden: true, value: row => row.at },
];

const view = (changes: Partial<TableView>): TableView => ({ ...emptyView, ...changes });
const models = (list: Row[]) => list.map(row => row.model);

describe('table view', () => {
  it('searches every visible column, formatted, with all terms required', () => {
    expect(models(filterRows(rows, columns, view({ search: 'amazon east-2' })))).toEqual(['claude-haiku', 'gpt-oss-20b']);
    // A column's extra search text matches too.
    expect(models(filterRows(rows, columns, view({ search: 'gpt-5-prod' })))).toEqual(['gpt-5']);
    // Tokens is hidden, so its values are not searched.
    expect(filterRows(rows, columns, view({ search: '100' }))).toEqual([]);
  });

  it('filters categories with multi-select, matching any element of a list', () => {
    expect(models(filterRows(rows, columns, view({ categories: { ops: ['chat_stream', 'embeddings'] } })))).toEqual(['claude-haiku', 'titan-embed']);
    expect(models(filterRows(rows, columns, view({ categories: { provider: ['openai'], ops: ['chat'] } })))).toEqual(['gpt-5']);
  });

  it('filters numeric and date ranges inclusively, leaving out empty values', () => {
    expect(models(filterRows(rows, columns, view({ ranges: { price: { min: '1', max: '1.25' } } })))).toEqual(['claude-haiku', 'gpt-5']);
    expect(models(filterRows(rows, columns, view({ ranges: { price: { max: '0.5' } } })))).toEqual(['titan-embed']);
    expect(models(filterRows(rows, columns, view({ ranges: { at: { min: '2026-10-05T00:00:00Z', max: '2026-10-10T23:59:00Z' } } })))).toEqual(['gpt-5', 'titan-embed']);
  });

  it('sorts both ways with empty values last', () => {
    expect(models(sortRows(rows, columns, view({ sort: { column: 'price', descending: false } })))).toEqual(['titan-embed', 'claude-haiku', 'gpt-5', 'gpt-oss-20b']);
    expect(models(sortRows(rows, columns, view({ sort: { column: 'price', descending: true } })))).toEqual(['gpt-5', 'claude-haiku', 'titan-embed', 'gpt-oss-20b']);
  });

  it('groups by several columns with counts and totals', () => {
    const groups = groupRows(rows, columns, ['provider', 'region']);
    expect(groups.map(group => [group.value, group.count, group.totals.tokens])).toEqual([
      ['Amazon Bedrock', 3, 107],
      ['OpenAI', 1, 50],
    ]);
    expect(groups[0].groups.map(group => [group.value, group.count])).toEqual([['us-east-2', 2], ['us-east-1', 1]]);
    expect(groupRows(rows, columns, ['region'])[1].value).toBe('—');
  });

  it('counts category values and shows the active filters as removable chips', () => {
    expect(categoryCounts(rows, columns[3])).toEqual([['chat', 3], ['chat_stream', 1], ['embeddings', 1]]);
    const current = view({ search: 'gpt', categories: { provider: ['bedrock'] }, ranges: { price: { min: '1' } }, page: 2 });
    const chips = activeChips(columns, current);
    expect(chips.map(chip => chip.label)).toEqual(['“gpt”', 'Provider: Amazon Bedrock', 'Price ≥ 1']);
    const removed = chips[1].remove(current);
    expect(removed.categories.provider).toEqual([]);
    expect(removed.page).toBe(0);
    expect(chips[2].remove(current).ranges).toEqual({});
  });

  it('round-trips through the URL under its own prefix, keeping other parameters', () => {
    const current = view({
      search: 'claude haiku',
      categories: { provider: ['bedrock'], ops: ['chat', 'a,b'] },
      ranges: { price: { min: '0.5' }, at: { max: '2026-10-10T00:00' } },
      group: ['provider', 'region'],
      sort: { column: 'price', descending: true },
      columns: ['model', 'price'],
      page: 2,
    });
    const search = writeView('?org=1&aliases.q=x', 'models', current);
    expect(new URLSearchParams(search).get('org')).toBe('1');
    expect(new URLSearchParams(search).get('aliases.q')).toBe('x');
    expect(readView(search, 'models')).toEqual(current);
    expect(readView(search, 'aliases').search).toBe('x');
    expect(writeView(search, 'models', emptyView)).toBe('?org=1&aliases.q=x');
  });

  it('drops columns the table does not have from a stale or edited URL', () => {
    const stale = view({ group: ['unknown', 'provider'], sort: { column: 'gone', descending: false }, categories: { gone: ['x'], provider: ['openai'] }, ranges: { gone: { min: '1' } }, columns: ['gone'] });
    expect(knownView(columns, stale)).toEqual(view({ group: ['provider'], categories: { provider: ['openai'] } }));
  });

  it('shows the default columns until some are chosen', () => {
    expect(visibleColumns(columns, emptyView).map(column => column.id)).toEqual(['model', 'provider', 'region', 'ops', 'price']);
    expect(visibleColumns(columns, view({ columns: ['tokens', 'model'] })).map(column => column.id)).toEqual(['model', 'tokens']);
  });
});
