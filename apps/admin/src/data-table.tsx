import React, { useEffect, useMemo, useState } from 'react';
import { Search } from '@carbon/react';
import {
  activeChips,
  categoryCounts,
  emptyView,
  filterRows,
  groupRows,
  isGroupable,
  knownView,
  readView,
  sortRows,
  visibleColumns,
  writeView,
  type ColumnSpec,
  type Group,
  type TableView,
} from './table-view';
import { Button, buttonClass, DataCell, DataGrid, FormField, Select, TableBody, TableExpandedRow, TableExpandRow, TableHead, TableHeader, TableRow, TextInput, CheckOption } from './ui';

export type Column<T> = ColumnSpec<T> & {
  /** The cell; the column's text when absent. */
  render?: (row: T) => React.ReactNode;
  title?: (row: T) => string | undefined;
  primary?: boolean;
  /** A group total as text (money, tokens); the number when absent. */
  formatTotal?: (total: number) => string;
  /** What a missing value means here (`No team`); a dash when absent. */
  nullLabel?: string;
};

const DEFAULT_PAGE_SIZE = 25;

/** A group the server counted: its value per grouped column, by column id. */
export type ServerGroup = {
  values: Record<string, string | null>;
  count: number;
  first_occurred_at?: string;
  last_occurred_at?: string;
  /** More about the group, such as its amounts. */
  note?: string;
  /** Tells apart groups with the same values (usage splits each by currency
   * and confidence); the values when absent. */
  key?: string;
};

/**
 * A table whose rows grow without bound (audit, usage, traces): the screen
 * queries the server with the view, and the table shows what came back. The
 * rows are the pages loaded so far, in the server's order.
 */
export type ServerTable = {
  /** Each category column's values with counts, once fetched. */
  facets: Record<string, [string, number][]>;
  /** A filter menu opened or closed: while open, keep its values current. */
  onFacet: (column: string, open: boolean) => void;
  /** Columns the server can group by, and how many at once. */
  groupable: string[];
  maxGroups: number;
  /** The server's groups while the view is grouped; null while loading. */
  groups: ServerGroup[] | null;
  /** More groups exist than were returned. */
  truncated: boolean;
  /** Every row matching the view, when known. */
  total: number | null;
  /** Show a group's rows: the screen narrows the view to it. */
  onDrill: (group: ServerGroup) => void;
  /** More rows, or while grouped more groups, can be loaded. */
  hasMore: boolean;
  loadingMore: boolean;
  onLoadMore: () => void;
  /** What a group counts, singular and plural; rows when absent. */
  countNoun?: [string, string];
};

/** The table's view, kept in the page URL under `prefix`. */
export function useUrlView(prefix: string): [TableView, (next: TableView) => void] {
  const [view, setView] = useState<TableView>(() => readView(location.search, prefix));
  useEffect(() => {
    const reread = () => setView(readView(location.search, prefix));
    window.addEventListener('popstate', reread);
    return () => window.removeEventListener('popstate', reread);
  }, [prefix]);
  const update = (next: TableView) => {
    setView(next);
    history.replaceState(history.state, '', `${location.pathname}${writeView(location.search, prefix, next)}${location.hash}`);
  };
  return [view, update];
}

/**
 * The shared data table (§3): search, category and range filters shown as
 * removable chips, grouping with counts and totals, expandable rows, sorting,
 * column choice and pagination, filtered in the browser. The view lives in
 * the URL under `urlKey`, so two tables on one screen keep separate views.
 */
export function DataTable<T>({
  label,
  urlKey,
  columns,
  rows,
  rowKey,
  detail,
  actions,
  pageSize = DEFAULT_PAGE_SIZE,
  view: givenView,
  onViewChange,
  server,
  filtersOpen: filtersShown = false,
  toolbarActions,
}: {
  label: string;
  urlKey: string;
  columns: Column<T>[];
  rows: T[];
  rowKey: (row: T) => string;
  detail?: (row: T) => React.ReactNode;
  actions?: (row: T) => React.ReactNode;
  pageSize?: number;
  /** The view, when the screen owns it (a server table queries with it). */
  view?: TableView;
  onViewChange?: (next: TableView) => void;
  server?: ServerTable;
  /** Whether the filters start unfolded, for a table people mostly filter. */
  filtersOpen?: boolean;
  /** The table's own actions (Add, Create, Refresh), at the toolbar's right
   * end beside search and filters, as Carbon's data table places them. */
  toolbarActions?: React.ReactNode;
}) {
  const [ownView, setOwnView] = useUrlView(urlKey);
  const urlView = givenView ?? ownView;
  const setView = onViewChange ?? setOwnView;
  const view = useMemo(() => knownView(columns, urlView), [columns, urlView]);
  const [expanded, setExpanded] = useState<Set<string>>(() => new Set());
  const [openGroups, setOpenGroups] = useState<Set<string>>(() => new Set());
  const [filtersOpen, setFiltersOpen] = useState(filtersShown);
  const filtersId = `${React.useId()}-filters`;
  const shown = visibleColumns(columns, view) as Column<T>[];
  // A server table's rows are already the view's.
  const filtered = useMemo(() => (server ? rows : sortRows(filterRows(rows, columns, view), columns, view)), [server, rows, columns, view]);
  const groups = useMemo(() => (view.group.length && !server ? groupRows(filtered, columns, view.group) : []), [server, filtered, columns, view.group]);
  const pages = server ? 1 : Math.max(1, Math.ceil(filtered.length / pageSize));
  const page = Math.min(view.page, pages - 1);
  const pageRows = view.group.length ? [] : server ? filtered : filtered.slice(page * pageSize, (page + 1) * pageSize);
  const chips = activeChips(columns, view);
  const width = (detail ? 1 : 0) + shown.length + (actions ? 1 : 0);
  const expandHeader = `${urlKey}-expand`;
  // A row is named by its primary column (the model, not the time).
  const named = shown.find(column => column.primary) ?? shown[0];
  const toggle = (set: Set<string>, key: string) => {
    const next = new Set(set);
    if (next.has(key)) next.delete(key);
    else next.add(key);
    return next;
  };
  const sortBy = (column: Column<T>) => {
    const current = view.sort?.column === column.id ? view.sort : null;
    const sort = !current ? { column: column.id, descending: false } : !current.descending ? { column: column.id, descending: true } : null;
    setView({ ...view, sort, page: 0 });
  };

  const renderRow = (row: T) => {
    const key = rowKey(row);
    const open = expanded.has(key);
    const detailId = `${urlKey}-detail-${key}`;
    return (
      <React.Fragment key={key}>
        {(() => {
          const cells = shown.map(column => (
            <DataCell key={column.id} label={column.label} className={column.primary ? 'cell-primary' : ''} title={column.title?.(row)}>
              {column.render ? column.render(row) : textOf(column, row) || '—'}
            </DataCell>
          ));
          const actionCell = actions && <DataCell label="Actions"><span className="row-actions">{actions(row)}</span></DataCell>;
          return detail ? (
            <TableExpandRow
              className="data-row"
              isExpanded={open}
              expandHeader={expandHeader}
              aria-controls={open ? detailId : undefined}
              aria-label={`${open ? 'Collapse' : 'Expand'} ${named ? textOf(named, row) : 'row'}`}
              onExpand={() => setExpanded(toggle(expanded, key))}
            >
              {cells}
              {actionCell}
            </TableExpandRow>
          ) : (
            <TableRow className="data-row">
              {cells}
              {actionCell}
            </TableRow>
          );
        })()}
        {detail && open && (
          <TableExpandedRow colSpan={width} className="row-detail-row">
            <div className="row-detail" id={detailId} role="region" aria-label={`Details of ${shown[0] ? textOf(shown[0], row) : 'row'}`}>
              {detail(row)}
            </div>
          </TableExpandedRow>
        )}
      </React.Fragment>
    );
  };

  const renderGroup = (group: Group<T>, depth: number): React.ReactNode => {
    const open = openGroups.has(group.key);
    const column = columns.find(candidate => candidate.id === group.column) as Column<T> | undefined;
    const sums = columns
      .filter(candidate => candidate.sum && group.totals[candidate.id] !== undefined)
      .map(candidate => `${candidate.label} ${(candidate as Column<T>).formatTotal?.(group.totals[candidate.id]) ?? group.totals[candidate.id].toLocaleString()}`);
    return (
      <React.Fragment key={group.key}>
        <TableRow className="data-row group-row">
          <DataCell colSpan={width} className="group-cell" style={{ paddingLeft: 16 + depth * 20 }}>
            <button type="button" className="expand-toggle" aria-expanded={open} onClick={() => setOpenGroups(toggle(openGroups, group.key))}>
              {open ? '▾' : '▸'} <span className="muted">{column?.label}:</span> <b>{group.value}</b>
            </button>
            <span className="muted"> · {group.count} {group.count === 1 ? 'row' : 'rows'}{sums.length ? ` · ${sums.join(' · ')}` : ''}</span>
          </DataCell>
        </TableRow>
        {open && (group.groups.length ? group.groups.map(child => renderGroup(child, depth + 1)) : group.rows.map(renderRow))}
      </React.Fragment>
    );
  };

  const categoryColumns = columns.filter(column => column.filter === 'category');
  const rangeColumns = columns.filter(column => column.filter === 'number' || column.filter === 'date');
  const groupable = columns.filter(column =>
    (server ? server.groupable.includes(column.id) : isGroupable(column)) && !view.group.includes(column.id));
  const canGroup = groupable.length > 0 && (!server || view.group.length < server.maxGroups);
  // The filters, grouping and columns stay folded behind one button, so a
  // short table is not buried under its controls; the chips below show what
  // is applied.
  const active = chips.length + view.group.length;

  return (
    <div className="data-view">
      <div className="table-toolbar" role="toolbar" aria-label={`${label} view`}>
        <Search
          size="md"
          className="table-search"
          labelText={`Search ${label.toLowerCase()}`}
          placeholder="Search"
          aria-label={`Search ${label.toLowerCase()}`}
          closeButtonLabelText="Clear search"
          value={view.search}
          onChange={event => setView({ ...view, search: event.target.value, page: 0 })}
        />
        <button
          type="button"
          className={`${buttonClass('ghost')} table-filters-toggle`}
          aria-expanded={filtersOpen}
          aria-controls={filtersId}
          onClick={() => {
            // Folding closes the menus too, so a hidden menu stops asking the
            // server for its counts.
            if (filtersOpen) {
              document.getElementById(filtersId)?.querySelectorAll<HTMLDetailsElement>('details[open]').forEach(menu => { menu.open = false; });
            }
            setFiltersOpen(!filtersOpen);
          }}
        >
          Filters{active > 0 && <span className="cds--tag cds--tag--gray cds--tag--sm filter-count"> {active}</span>}
        </button>
        {toolbarActions && <div className="table-toolbar-actions">{toolbarActions}</div>}
        <div className="table-filters" id={filtersId} hidden={!filtersOpen} role="group" aria-label={`${label} filters, grouping and columns`}>
        {categoryColumns.map(column => (
          <details key={column.id} className="table-menu" onToggle={event => server?.onFacet(column.id, (event.currentTarget as HTMLDetailsElement).open)}>
            <summary>{column.label}{(view.categories[column.id]?.length ?? 0) > 0 && <span className="cds--tag cds--tag--gray cds--tag--sm filter-count"> {view.categories[column.id].length}</span>}</summary>
            <div className="table-menu-body" role="group" aria-label={`Filter by ${column.label.toLowerCase()}`}>
              {server && !server.facets[column.id] && <span className="muted">Loading…</span>}
              {(server ? withChosen(server.facets[column.id], view.categories[column.id]) : categoryCounts(rows, column)).map(([value, count]) => {
                const chosen = view.categories[column.id] ?? [];
                const checked = chosen.includes(value);
                return (
                  <CheckOption key={value}
                     
                      checked={checked}
                      onChange={() => setView({
                        ...view,
                        page: 0,
                        categories: { ...view.categories, [column.id]: checked ? chosen.filter(other => other !== value) : [...chosen, value] },
                      })}>{column.format ? column.format(value) : value} <span className="muted">{count}</span></CheckOption>
                );
              })}
            </div>
          </details>
        ))}
        {rangeColumns.map(column => {
          const range = view.ranges[column.id] ?? {};
          const type = column.filter === 'date' ? 'datetime-local' : 'number';
          const set = (side: 'min' | 'max', value: string) => setView({ ...view, page: 0, ranges: { ...view.ranges, [column.id]: { ...range, [side]: value || undefined } } });
          return (
            <details key={column.id} className="table-menu">
              <summary>{column.label}</summary>
              <div className="table-menu-body range-body" role="group" aria-label={`${column.label} range`}>
                <FormField label="From" htmlFor={`${filtersId}-${column.id}-min`}>
                  <TextInput id={`${filtersId}-${column.id}-min`} type={type} step="any" value={inputValue(column.filter, range.min)} onChange={event => set('min', event.target.value)} />
                </FormField>
                <FormField label="To" htmlFor={`${filtersId}-${column.id}-max`}>
                  <TextInput id={`${filtersId}-${column.id}-max`} type={type} step="any" value={inputValue(column.filter, range.max)} onChange={event => set('max', event.target.value)} />
                </FormField>
              </div>
            </details>
          );
        })}
        {canGroup && (
          <Select
            className="table-group"
            aria-label="Group by"
            value=""
            onChange={event => event.target.value && setView({ ...view, group: [...view.group, event.target.value], page: 0 })}
          >
            <option value="">{view.group.length ? 'Then group by…' : 'Group by…'}</option>
            {groupable.map(column => <option key={column.id} value={column.id}>{column.label}</option>)}
          </Select>
        )}
        <details className="table-menu">
          <summary>Columns</summary>
          <div className="table-menu-body" role="group" aria-label="Visible columns">
            {columns.map(column => {
              const visible = shown.includes(column);
              return (
                <CheckOption key={column.id}
                   
                    checked={visible}
                    disabled={visible && shown.length === 1}
                    onChange={() => {
                      const ids = shown.map(candidate => candidate.id);
                      const next = visible ? ids.filter(id => id !== column.id) : columns.map(c => c.id).filter(id => id === column.id || ids.includes(id));
                      const defaults = columns.filter(candidate => !candidate.hidden).map(candidate => candidate.id);
                      setView({ ...view, columns: next.join() === defaults.join() ? null : next });
                    }}>{column.label}</CheckOption>
              );
            })}
          </div>
        </details>
        </div>
      </div>
      {(chips.length > 0 || view.group.length > 0) && (
        <div className="table-chips" aria-label="Active filters and grouping">
          {chips.map(chip => (
            <button key={chip.key} type="button" className="chip cds--tag cds--tag--gray cds--tag--operational" onClick={() => setView(chip.remove(view))} aria-label={`Remove filter ${chip.label}`}>
              {chip.label} <span aria-hidden="true">×</span>
            </button>
          ))}
          {view.group.map(id => {
            const column = columns.find(candidate => candidate.id === id);
            return (
              <button key={`g.${id}`} type="button" className="chip chip-group cds--tag cds--tag--blue cds--tag--operational" onClick={() => setView({ ...view, group: view.group.filter(other => other !== id) })} aria-label={`Stop grouping by ${column?.label ?? id}`}>
                Grouped by {column?.label ?? id} <span aria-hidden="true">×</span>
              </button>
            );
          })}
          <button type="button" className="inline-action" onClick={() => setView({ ...emptyView, columns: view.columns, sort: view.sort })}>Clear all</button>
        </div>
      )}
      <p className="muted table-count" role="status" aria-live="polite">
        {server
          ? view.group.length
            ? server.groups ? `${server.groups.length}${server.truncated || server.hasMore ? '+' : ''} groups${server.total !== null ? ` · ${server.total.toLocaleString()} rows` : ''}` : 'Counting…'
            : `${rows.length}${server.hasMore ? '+' : ''} ${rows.length === 1 && !server.hasMore ? 'row' : 'rows'}`
          : filtered.length === rows.length ? `${rows.length} ${rows.length === 1 ? 'row' : 'rows'}` : `${filtered.length} of ${rows.length} rows`}
      </p>
      <DataGrid aria-label={label} aria-rowcount={filtered.length} isSortable={!server}>
        <TableHead>
          <TableRow className="data-row head">
            {detail && <th scope="col" id={expandHeader} className="cds--table-expand"><span className="cds--visually-hidden">Expand</span></th>}
            {shown.map(column => {
              const sorted = view.sort?.column === column.id;
              return server ? <TableHeader key={column.id}>{column.label}</TableHeader> : (
                <TableHeader
                  key={column.id}
                  isSortable
                  isSortHeader={sorted}
                  sortDirection={!sorted ? 'NONE' : view.sort?.descending ? 'DESC' : 'ASC'}
                  onClick={() => sortBy(column)}
                >
                  {column.label}
                </TableHeader>
              );
            })}
            {actions && <TableHeader className="row-actions">Actions</TableHeader>}
          </TableRow>
        </TableHead>
        <TableBody>
        {server && view.group.length
          ? server.groups?.map(group => {
              const label = view.group.map(id => `${columns.find(column => column.id === id)?.label ?? id}: ${formatValue(columns.find(column => column.id === id), group.values[id])}`).join(' · ');
              return (
                <TableRow key={group.key ?? JSON.stringify(group.values)} className="data-row group-row">
                  <DataCell colSpan={width} className="group-cell">
                    {/* A missing value (no team, a system actor) cannot be
                        filtered for, so such a group cannot be opened. */}
                    {Object.values(group.values).some(value => value === null)
                      ? <span title="Rows without this value cannot be listed on their own"><b>{label}</b></span>
                      : (
                        <button type="button" className="expand-toggle" onClick={() => server.onDrill(group)} aria-label={`Show the rows of ${label}`}>
                          ▸ <b>{label}</b>
                        </button>
                      )}
                    <span className="muted"> · {group.count.toLocaleString()} {(server.countNoun ?? ['row', 'rows'])[group.count === 1 ? 0 : 1]}{group.note ? ` · ${group.note}` : ''}{group.first_occurred_at && group.last_occurred_at ? ` · ${new Date(group.first_occurred_at).toLocaleString()} – ${new Date(group.last_occurred_at).toLocaleString()}` : ''}</span>
                  </DataCell>
                </TableRow>
              );
            })
          : view.group.length ? groups.map(group => renderGroup(group, 0)) : pageRows.map(renderRow)}
        </TableBody>
      </DataGrid>
      {/* Outside the table: a table's children are rows only. */}
      {(server && view.group.length ? server.groups?.length === 0 : filtered.length === 0) && <p className="muted table-empty" role="status">No rows match this view.</p>}
      {server && view.group.length > 0 && server.truncated && <p className="muted">Only the largest groups are shown; narrow the filters to see the rest.</p>}
      {server && server.hasMore && (
        <div className="load-more">
          <Button variant="ghost" type="button" onClick={server.onLoadMore} loading={server.loadingMore}>
            {server.loadingMore ? 'Loading more…' : 'Load more'}
          </Button>
        </div>
      )}
      {!view.group.length && pages > 1 && (
        <nav className="table-pages" aria-label={`${label} pages`}>
          <Button variant="ghost" type="button" disabled={page === 0} onClick={() => setView({ ...view, page: page - 1 })}>Previous</Button>
          <span className="muted">Rows {page * pageSize + 1}–{Math.min((page + 1) * pageSize, filtered.length)} of {filtered.length}</span>
          <Button variant="ghost" type="button" disabled={page >= pages - 1} onClick={() => setView({ ...view, page: page + 1 })}>Next</Button>
        </nav>
      )}
    </div>
  );
}

function textOf<T>(column: Column<T>, row: T): string {
  const value = column.value(row);
  if (value === null) return '';
  const values = Array.isArray(value) ? value : [String(value)];
  return values.map(item => (column.format ? column.format(item) : item)).join(', ');
}

function formatValue<T>(column: Column<T> | undefined, value: string | null): string {
  if (value === null) return column?.nullLabel ?? '—';
  return column?.format ? column.format(value) : value;
}

/** A server facet with the chosen values kept, even when none match now. */
function withChosen(facet: [string, number][] | undefined, chosen: string[] | undefined): [string, number][] {
  const values = [...(facet ?? [])];
  for (const value of chosen ?? []) if (!values.some(([other]) => other === value)) values.push([value, 0]);
  return values;
}

/** A range bound as its input shows it: a time with a zone (a drilled-into
 * UTC day) in local `datetime-local` form, the same instant. */
function inputValue(kind: Column<unknown>['filter'], value: string | undefined): string {
  if (!value) return '';
  if (kind !== 'date' || !/(Z|[+-]\d\d:\d\d)$/.test(value)) return value;
  const time = new Date(value);
  if (Number.isNaN(time.getTime())) return value;
  const pad = (part: number) => String(part).padStart(2, '0');
  // Seconds and milliseconds are kept when set, so a day's end (23:59:59.999)
  // survives an edit of the date.
  const seconds = time.getSeconds() || time.getMilliseconds() ? `:${pad(time.getSeconds())}${time.getMilliseconds() ? `.${String(time.getMilliseconds()).padStart(3, '0')}` : ''}` : '';
  return `${time.getFullYear()}-${pad(time.getMonth() + 1)}-${pad(time.getDate())}T${pad(time.getHours())}:${pad(time.getMinutes())}${seconds}`;
}
