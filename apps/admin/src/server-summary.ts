import { useCallback, useEffect, useRef, useState } from 'react';
import type { ApiProblem } from './api';
import type { ServerGroup } from './data-table';
import { loadFailed } from './screen-feedback';
import type { TableView } from './table-view';

/** A server summary: groups by dimension name, and how many rows matched. */
export type SummaryResult<G> = { groups: G[]; total: number; truncated: boolean };

export type ServerSummaryOptions<Q, G extends { values: Record<string, string | null>; count: number }> = {
  /** Anything besides the view the answers depend on, such as the organization. */
  scope: string;
  view: TableView;
  /** The summary dimension each groupable or filterable column counts by. */
  dimensionOf: Record<string, string>;
  /** The API query for the view, optionally without one column's own filter. */
  queryFor: (view: TableView, except?: string) => Q;
  summarize: (dimensions: string[], query: Q) => Promise<SummaryResult<G>>;
  /** A summary group as the table shows it, its values already by column id. */
  toGroup: (group: G & { values: Record<string, string | null> }) => ServerGroup;
};

/**
 * Server-side grouping and filter-menu counts for a table whose rows live on
 * the server. While the view is grouped, the groups are the summary by its
 * grouped columns. A filter menu's values are counted under every other
 * filter (so more can be chosen), and only while the menu is open, since each
 * summary is an audited read. Each answer is kept with the query it counted
 * and shown only while that is still the menu's query: choosing a value in a
 * menu keeps its list, and a late answer for an older query is never shown.
 */
export function useServerSummary<Q, G extends { values: Record<string, string | null>; count: number }>({
  scope,
  view,
  dimensionOf,
  queryFor,
  summarize,
  toGroup,
}: ServerSummaryOptions<Q, G>) {
  const columnOf = Object.fromEntries(Object.entries(dimensionOf).map(([column, dimension]) => [dimension, column]));
  const byColumn = (values: Record<string, string | null>) =>
    Object.fromEntries(Object.entries(values).map(([dimension, value]) => [columnOf[dimension] ?? dimension, value]));
  const groupBy = view.group.map(id => dimensionOf[id]).filter(Boolean).slice(0, 2);
  const grouped = groupBy.length > 0;
  const queryKey = JSON.stringify(queryFor(view));

  const [summary, setSummary] = useState<{ groups: ServerGroup[]; total: number; truncated: boolean } | null>(null);
  // Why the groups could not be counted, for status-specific feedback.
  const [summaryError, setSummaryError] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    setSummary(null);
    setSummaryError(null);
    if (!grouped) return;
    let current = true;
    summarize(groupBy, queryFor(view)).then(
      result => {
        if (current) setSummary({ ...result, groups: result.groups.map(group => toGroup({ ...group, values: byColumn(group.values) })) });
      },
      caught => {
        if (current) setSummaryError((caught as { problem?: ApiProblem }).problem ?? loadFailed('The groups could not be counted.'));
      },
    );
    return () => { current = false; };
    // The query key stands for the view's filters.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scope, queryKey, groupBy.join(), attempt]);
  const retrySummary = useCallback(() => setAttempt(previous => previous + 1), []);

  const [facets, setFacets] = useState<Record<string, { key: string; values: [string, number][] }>>({});
  const [openMenus, setOpenMenus] = useState<string[]>([]);
  const pending = useRef(new Set<string>());
  const facetKey = useCallback((column: string) => `${scope}|${JSON.stringify(queryFor(view, column))}`, [scope, view, queryFor]);
  useEffect(() => {
    for (const column of openMenus) {
      const dimension = dimensionOf[column];
      const key = facetKey(column);
      if (!dimension || facets[column]?.key === key || pending.current.has(`${column}|${key}`)) continue;
      pending.current.add(`${column}|${key}`);
      summarize([dimension], queryFor(view, column))
        .then(
          result => result.groups.flatMap(group => (group.values[dimension] ? [[group.values[dimension]!, group.count] as [string, number]] : [])),
          () => [] as [string, number][],
        )
        .then(values => {
          pending.current.delete(`${column}|${key}`);
          setFacets(previous => ({ ...previous, [column]: { key, values } }));
        });
    }
  }, [openMenus, facetKey, facets, dimensionOf, summarize, queryFor, view]);
  const currentFacets = Object.fromEntries(
    Object.entries(facets).filter(([column, facet]) => facet.key === facetKey(column)).map(([column, facet]) => [column, facet.values]),
  );
  const onFacet = useCallback((column: string, open: boolean) => {
    setOpenMenus(previous => (open ? [...previous.filter(other => other !== column), column] : previous.filter(other => other !== column)));
  }, []);

  return { grouped, summary, summaryError, retrySummary, facets: currentFacets, onFacet, byColumn };
}

/** The view narrowed to one summary group (values by column id), ungrouped.
 * A day (the value of `dayColumn`) keeps any narrower time range already set. */
export function drillInto(view: TableView, group: ServerGroup, dayColumn: string): TableView {
  const categories = { ...view.categories };
  const ranges = { ...view.ranges };
  for (const [id, value] of Object.entries(group.values)) {
    if (id === dayColumn) {
      if (!value) continue;
      const current = ranges[id] ?? {};
      const later = (a: string, b: string | undefined) => (b && Date.parse(b) > Date.parse(a) ? b : a);
      const earlier = (a: string, b: string | undefined) => (b && Date.parse(b) < Date.parse(a) ? b : a);
      ranges[id] = { min: later(`${value}T00:00:00Z`, current.min), max: earlier(`${value}T23:59:59.999Z`, current.max) };
    } else if (value !== null) {
      categories[id] = [value];
    }
  }
  return { ...view, categories, ranges, group: [], page: 0 };
}

/** A typed time (`datetime-local`, local time) or an RFC 3339 one, as UTC. */
export function toUtc(value: string | undefined) {
  if (!value) return undefined;
  const time = new Date(value);
  return Number.isNaN(time.getTime()) ? undefined : time.toISOString();
}
