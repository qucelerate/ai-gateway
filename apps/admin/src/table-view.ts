// The view of a shared data table (§3 of the plan): search, category and
// range filters, grouping, sort, visible columns and page, kept in the URL so
// a view can be shared and reloaded. Pure, so screens and tests share it.

export type CellValue = string | number | null | readonly string[];

export type FilterKind = 'category' | 'number' | 'date';

export type ColumnSpec<T> = {
  /** Stable id, used in the URL. */
  id: string;
  label: string;
  /** The value searched, sorted, filtered and grouped on. A list (such as a
   * deployment's operations) matches a filter when any element does. */
  value: (row: T) => CellValue;
  filter?: FilterKind;
  /** Display text for a category value (`bedrock` → `Amazon Bedrock`). */
  format?: (value: string) => string;
  /** Totals are shown per group. */
  sum?: boolean;
  /** Hidden until chosen under Columns. */
  hidden?: boolean;
  /** Can be grouped by (category columns can by default). */
  groupable?: boolean;
  /** More text the search matches (what the cell shows beside its value). */
  search?: (row: T) => string;
};

export type Range = { min?: string; max?: string };

export type TableView = {
  search: string;
  categories: Record<string, string[]>;
  ranges: Record<string, Range>;
  group: string[];
  sort: { column: string; descending: boolean } | null;
  /** Visible columns when changed from the defaults; null keeps them. */
  columns: string[] | null;
  page: number;
};

export const emptyView: TableView = { search: '', categories: {}, ranges: {}, group: [], sort: null, columns: null, page: 0 };

export type Group<T> = {
  key: string;
  column: string;
  value: string;
  count: number;
  totals: Record<string, number>;
  rows: T[];
  groups: Group<T>[];
};

const list = (value: CellValue): string[] =>
  value === null ? [] : Array.isArray(value) ? [...value] : [String(value)];

/** Text a cell is searched and grouped by. */
export function cellText<T>(column: ColumnSpec<T>, row: T): string {
  const values = list(column.value(row));
  return values.map(value => (column.format ? column.format(value) : value)).join(', ');
}

/** The view without columns these columns do not have (a stale or edited
 * URL), so it never groups, sorts or filters by nothing. */
export function knownView<T>(columns: ColumnSpec<T>[], view: TableView): TableView {
  const ids = new Set(columns.map(column => column.id));
  const keep = <V>(record: Record<string, V>) => Object.fromEntries(Object.entries(record).filter(([id]) => ids.has(id)));
  const chosen = view.columns?.filter(id => ids.has(id)) ?? null;
  return {
    ...view,
    categories: keep(view.categories),
    ranges: keep(view.ranges),
    group: view.group.filter(id => ids.has(id)),
    sort: view.sort && ids.has(view.sort.column) ? view.sort : null,
    columns: chosen && chosen.length ? chosen : null,
  };
}

export function isGroupable<T>(column: ColumnSpec<T>): boolean {
  return column.groupable ?? column.filter === 'category';
}

export function visibleColumns<T>(columns: ColumnSpec<T>[], view: TableView): ColumnSpec<T>[] {
  if (view.columns) {
    const chosen = new Set(view.columns);
    return columns.filter(column => chosen.has(column.id));
  }
  return columns.filter(column => !column.hidden);
}

function bound(kind: FilterKind | undefined, text: string | undefined): number | null {
  if (text === undefined || text === '') return null;
  const value = kind === 'date' ? Date.parse(text) : Number(text);
  return Number.isFinite(value) ? value : null;
}

function inRange<T>(column: ColumnSpec<T>, row: T, range: Range): boolean {
  const min = bound(column.filter, range.min);
  const max = bound(column.filter, range.max);
  if (min === null && max === null) return true;
  const raw = column.value(row);
  const values = (Array.isArray(raw) ? raw : [raw])
    .map(value => (value === null ? null : column.filter === 'date' ? Date.parse(String(value)) : Number(value)))
    .filter((value): value is number => value !== null && Number.isFinite(value));
  return values.some(value => (min === null || value >= min) && (max === null || value <= max));
}

/** Rows passing the search and every filter. Search covers the visible columns. */
export function filterRows<T>(rows: T[], columns: ColumnSpec<T>[], view: TableView): T[] {
  const byId = new Map(columns.map(column => [column.id, column]));
  const terms = view.search.toLowerCase().split(/\s+/).filter(Boolean);
  const searched = visibleColumns(columns, view);
  return rows.filter(row => {
    for (const [id, chosen] of Object.entries(view.categories)) {
      const column = byId.get(id);
      if (!column || chosen.length === 0) continue;
      if (!list(column.value(row)).some(value => chosen.includes(value))) return false;
    }
    for (const [id, range] of Object.entries(view.ranges)) {
      const column = byId.get(id);
      if (column && !inRange(column, row, range)) return false;
    }
    if (terms.length === 0) return true;
    const text = searched.map(column => `${cellText(column, row)} ${column.search?.(row) ?? ''}`).join(' ').toLowerCase();
    return terms.every(term => text.includes(term));
  });
}

function compare(left: CellValue, right: CellValue): number {
  const a = Array.isArray(left) ? left.join(', ') : left;
  const b = Array.isArray(right) ? right.join(', ') : right;
  if (a === b) return 0;
  // Empty values sort last either way round.
  if (a === null || a === '') return 1;
  if (b === null || b === '') return -1;
  if (typeof a === 'number' && typeof b === 'number') return a - b;
  return String(a).localeCompare(String(b), undefined, { numeric: true, sensitivity: 'base' });
}

export function sortRows<T>(rows: T[], columns: ColumnSpec<T>[], view: TableView): T[] {
  const sort = view.sort;
  const column = sort && columns.find(candidate => candidate.id === sort.column);
  if (!sort || !column) return rows;
  const sign = sort.descending ? -1 : 1;
  return [...rows].sort((left, right) => {
    const a = column.value(left);
    const b = column.value(right);
    const order = compare(a, b);
    // Keep empty values last in descending order too.
    const emptyA = a === null || a === '';
    const emptyB = b === null || b === '';
    return emptyA !== emptyB ? order : sign * order;
  });
}

function totals<T>(rows: T[], columns: ColumnSpec<T>[]): Record<string, number> {
  const sums: Record<string, number> = {};
  for (const column of columns.filter(candidate => candidate.sum)) {
    sums[column.id] = rows.reduce((total, row) => {
      const value = column.value(row);
      return typeof value === 'number' ? total + value : total;
    }, 0);
  }
  return sums;
}

/** Nested groups by `view.group`, in order of first appearance after sorting. */
export function groupRows<T>(rows: T[], columns: ColumnSpec<T>[], group: string[], path = ''): Group<T>[] {
  const [first, ...rest] = group;
  const column = columns.find(candidate => candidate.id === first);
  if (!column) return [];
  const buckets = new Map<string, T[]>();
  for (const row of rows) {
    const value = cellText(column, row) || '—';
    const bucket = buckets.get(value);
    if (bucket) bucket.push(row);
    else buckets.set(value, [row]);
  }
  return [...buckets.entries()].map(([value, members]) => {
    const key = `${path}/${column.id}=${value}`;
    return {
      key,
      column: column.id,
      value,
      count: members.length,
      totals: totals(members, columns),
      rows: members,
      groups: rest.length ? groupRows(members, columns, rest, key) : [],
    };
  });
}

// URL form, under a per-table prefix (`models.q=claude&models.f.provider=bedrock`):
//   q        search
//   f.<col>  category values, comma-separated (each value URI-encoded)
//   r.<col>  range, `min..max` (either side may be empty)
//   g        grouping columns, comma-separated
//   s        sort column, `-` prefix for descending
//   c        visible columns, comma-separated, when not the defaults
//   p        page, from 1, when not the first
const encodeList = (values: string[]) => values.map(encodeURIComponent).join(',');
const decodeList = (text: string) => text.split(',').filter(Boolean).map(value => {
  try {
    return decodeURIComponent(value);
  } catch {
    return value;
  }
});

export function readView(search: string, prefix: string): TableView {
  const params = new URLSearchParams(search);
  const view: TableView = { ...emptyView, categories: {}, ranges: {} };
  const head = `${prefix}.`;
  for (const [name, value] of params) {
    if (!name.startsWith(head)) continue;
    const key = name.slice(head.length);
    if (key === 'q') view.search = value;
    else if (key === 'g') view.group = decodeList(value);
    else if (key === 'c') view.columns = decodeList(value);
    else if (key === 's' && value) view.sort = { column: value.replace(/^-/, ''), descending: value.startsWith('-') };
    else if (key === 'p') view.page = Math.max(0, (Number.parseInt(value, 10) || 1) - 1);
    else if (key.startsWith('f.')) view.categories[key.slice(2)] = decodeList(value);
    else if (key.startsWith('r.')) {
      const [min = '', max = ''] = value.split('..');
      view.ranges[key.slice(2)] = { ...(min ? { min } : {}), ...(max ? { max } : {}) };
    }
  }
  return view;
}

/** `search` with this table's view replaced, other parameters kept. */
export function writeView(search: string, prefix: string, view: TableView): string {
  const params = new URLSearchParams(search);
  for (const name of [...params.keys()]) if (name.startsWith(`${prefix}.`)) params.delete(name);
  const set = (key: string, value: string) => params.set(`${prefix}.${key}`, value);
  if (view.search) set('q', view.search);
  for (const [id, values] of Object.entries(view.categories)) if (values.length) set(`f.${id}`, encodeList(values));
  for (const [id, range] of Object.entries(view.ranges)) {
    if (range.min || range.max) set(`r.${id}`, `${range.min ?? ''}..${range.max ?? ''}`);
  }
  if (view.group.length) set('g', encodeList(view.group));
  if (view.sort) set('s', `${view.sort.descending ? '-' : ''}${view.sort.column}`);
  if (view.columns) set('c', encodeList(view.columns));
  if (view.page > 0) set('p', String(view.page + 1));
  const text = params.toString();
  return text ? `?${text}` : '';
}

/** The active filters as removable chips. */
export type Chip = { key: string; label: string; remove: (view: TableView) => TableView };

export function activeChips<T>(columns: ColumnSpec<T>[], view: TableView): Chip[] {
  const chips: Chip[] = [];
  const byId = new Map(columns.map(column => [column.id, column]));
  if (view.search) {
    chips.push({ key: 'q', label: `“${view.search}”`, remove: current => ({ ...current, search: '', page: 0 }) });
  }
  for (const [id, values] of Object.entries(view.categories)) {
    const column = byId.get(id);
    if (!column) continue;
    for (const value of values) {
      chips.push({
        key: `f.${id}.${value}`,
        label: `${column.label}: ${column.format ? column.format(value) : value}`,
        remove: current => ({
          ...current,
          page: 0,
          categories: { ...current.categories, [id]: (current.categories[id] ?? []).filter(other => other !== value) },
        }),
      });
    }
  }
  for (const [id, range] of Object.entries(view.ranges)) {
    const column = byId.get(id);
    if (!column || (!range.min && !range.max)) continue;
    const show = (text: string) => (column.filter === 'date' ? text.replace('T', ' ') : text);
    const label = range.min && range.max
      ? `${show(range.min)} – ${show(range.max)}`
      : range.min ? `≥ ${show(range.min)}` : `≤ ${show(range.max ?? '')}`;
    chips.push({
      key: `r.${id}`,
      label: `${column.label} ${label}`,
      remove: current => {
        const ranges = { ...current.ranges };
        delete ranges[id];
        return { ...current, ranges, page: 0 };
      },
    });
  }
  return chips;
}

/** Each category value of a column with how many rows carry it, most first. */
export function categoryCounts<T>(rows: T[], column: ColumnSpec<T>): [string, number][] {
  const counts = new Map<string, number>();
  for (const row of rows) for (const value of new Set(list(column.value(row)))) counts.set(value, (counts.get(value) ?? 0) + 1);
  return [...counts.entries()].sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]));
}
