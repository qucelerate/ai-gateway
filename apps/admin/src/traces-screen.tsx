import React, { useCallback, useEffect, useMemo, useState } from 'react';
import {
  api,
  type ApiProblem,
  type ContentState,
  listFilter,
  type TraceContent,
  type TokenTotals,
  type TraceDetail,
  type TraceDimension,
  type TraceGroup,
  type TraceProviderError,
  type TraceQuery,
  type TraceSummary,
  type UsageDimension,
  type UsageSliceQuery,
} from './api';
import { DataTable, useUrlView, type Column } from './data-table';
import { drillInto, toUtc, useServerSummary } from './server-summary';
import { type TableView } from './table-view';
import { useCollection } from './use-collection';
import { ReportError, SessionExpired, loadFailed } from './screen-feedback';
import { formatCount, formatMoney } from './money';
import { Badge, Button, EmptyState, InlineError, Message, formatDate, routeTab, uuidHint } from './ui';
import { ValueFilterForm } from './value-combo';

// The Traces page: every request's metadata trace, searchable and filtered,
// and one trace on its own page (`/admin/traces/<id>`), with its content
// where the tracing policy kept it and the reader may read it.

const errorCategories: Record<string, string> = {
  authentication: 'Credential refused',
  authorization: 'Permission refused',
  invalid_request: 'Request refused',
  unsupported_feature: 'Not supported',
  rate_limited: 'Rate limited',
  unavailable: 'Provider unavailable',
  timeout: 'Timed out',
  cancelled: 'Cancelled',
  provider: 'Provider failure',
  protocol: 'Unreadable response',
  internal: 'Gateway failure',
};

/** Why a provider request failed: the gateway's category, then the provider's own status and error type. */
export function errorCategoryLabel(category: string): string {
  return errorCategories[category] ?? category;
}

export function providerErrorText(error: TraceProviderError | null): string {
  if (!error) return 'None';
  return [errorCategories[error.category] ?? error.category, error.http_status && `HTTP ${error.http_status}`, error.error_type]
    .filter(Boolean)
    .join(' · ');
}

/** The trace page's address. */
export const traceHref = (traceId: string) => `/admin/traces/${encodeURIComponent(traceId)}`;

/** Goes to an address inside the console the way the navigation does: the
 * shell and the open screen re-read the path. */
export function goTo(href: string) {
  history.pushState({}, '', href);
  dispatchEvent(new PopStateEvent('popstate'));
}

/** A link inside the console that opens in place, and in a new tab as usual. */
export function ConsoleLink({ href, children, ...props }: React.AnchorHTMLAttributes<HTMLAnchorElement> & { href: string }) {
  return (
    <a
      {...props}
      href={href}
      onClick={event => {
        if (event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
        event.preventDefault();
        goTo(href);
      }}
    >{children}</a>
  );
}

const contentStateLabels: Record<ContentState, string> = {
  stored: 'Content kept',
  writing: 'Content being written',
  off: 'Timing only',
  unavailable: 'No trace store',
  failed: 'Content not written',
  expired: 'Content expired',
};

/** What a request's trace holds, in a word or two; null state: not traced. */
export function contentStateLabel(state: ContentState | null): string {
  return state ? contentStateLabels[state] : 'Not traced';
}

/** Milliseconds as a duration: `850 ms`, `2.4 s`. */
export function formatDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return '—';
  return ms < 1000 ? `${formatCount(ms)} ms` : `${(ms / 1000).toLocaleString(undefined, { maximumFractionDigits: 1 })} s`;
}

const traceStatusText = (trace: TraceSummary) => (trace.status === 'pending' ? 'Pending' : trace.outcome ?? 'Finalized');

/** Trace columns, by the trace API's names; `recorded` groups by UTC day. */
export const traceColumns: Column<TraceSummary>[] = [
  { id: 'recorded', label: 'Recorded', filter: 'date', value: trace => trace.recorded_at, title: trace => trace.recorded_at, render: trace => formatDate(trace.recorded_at) },
  { id: 'model', label: 'Model', primary: true, filter: 'category', value: trace => trace.model, title: trace => trace.deployment },
  { id: 'alias', label: 'Alias', hidden: true, value: trace => trace.alias, render: trace => trace.alias ?? '—' },
  { id: 'deployment', label: 'Deployment', filter: 'category', hidden: true, value: trace => trace.deployment },
  { id: 'provider_family', label: 'Provider', filter: 'category', value: trace => trace.provider_family, render: trace => trace.provider_family ?? '—' },
  { id: 'location', label: 'Location', filter: 'category', value: trace => trace.location, render: trace => trace.location ?? '—' },
  { id: 'status', label: 'Status', filter: 'category', value: trace => trace.status, render: trace => trace.error_category ? `${trace.outcome === 'failed-over' ? 'Failed over' : 'Failed'} · ${errorCategories[trace.error_category] ?? trace.error_category}` : traceStatusText(trace) },
  { id: 'outcome', label: 'Outcome', filter: 'category', hidden: true, value: trace => trace.outcome },
  { id: 'team', label: 'Team', filter: 'category', hidden: true, format: value => uuidHint(value) ?? value, value: trace => trace.team_id, title: trace => trace.team_id ?? undefined, render: trace => (trace.team_id ? uuidHint(trace.team_id) : 'No team') },
  { id: 'service_account', label: 'Service account', filter: 'category', hidden: true, format: value => uuidHint(value) ?? value, value: trace => trace.service_account_id, title: trace => trace.service_account_id ?? undefined, render: trace => (trace.service_account_id ? uuidHint(trace.service_account_id) : '—') },
  { id: 'api_key', label: 'API key', hidden: true, format: value => uuidHint(value) ?? value, value: trace => trace.api_key_id, title: trace => trace.api_key_id ?? undefined, render: trace => (trace.api_key_id ? uuidHint(trace.api_key_id) : '—') },
  { id: 'end_user', label: 'End user', hidden: true, value: trace => trace.end_user, render: trace => trace.end_user ?? '—' },
  { id: 'duration', label: 'Duration', value: trace => trace.duration_ms, render: trace => formatDuration(trace.duration_ms) },
  { id: 'content', label: 'Trace', value: trace => trace.content_state, render: trace => contentStateLabel(trace.content_state) },
  { id: 'amount', label: 'Amount', value: trace => trace.amount_minor, render: trace => formatMoney(trace.amount_minor, trace.currency) },
];

/** The summary dimension each trace column counts by. */
const traceDimensionOf: Record<string, TraceDimension> = {
  recorded: 'day', model: 'model', deployment: 'deployment', provider_family: 'provider_family', location: 'location',
  status: 'status', outcome: 'outcome', team: 'team', service_account: 'service_account',
};

/** The trace API query for a table view, optionally without one column's
 * own filter. The range's end is inclusive in the table and exclusive in the
 * API, so it is sent a millisecond later. */
export function traceQuery(view: TableView, except?: string): TraceQuery {
  const category = (id: string) => (id === except ? undefined : listFilter(view.categories[id]));
  const range = except === 'recorded' ? undefined : view.ranges.recorded;
  const to = toUtc(range?.max);
  return {
    from: toUtc(range?.min),
    to: to && new Date(Date.parse(to) + 1).toISOString(),
    team_id: category('team'),
    service_account_id: category('service_account'),
    api_key_id: category('api_key'),
    end_user: category('end_user'),
    model: category('model'),
    deployment: category('deployment'),
    provider_family: category('provider_family'),
    location: category('location'),
    status: category('status'),
    outcome: category('outcome'),
    q: view.search.trim() || undefined,
  };
}

/** A trace group's amounts, per currency, and how many had none. */
export function traceGroupNote(group: TraceGroup): string {
  return [
    ...group.amounts.map(amount => formatMoney(amount.amount_minor, amount.currency)),
    group.unpriced_count ? `${formatCount(group.unpriced_count)} unpriced` : '',
  ].filter(Boolean).join(' · ');
}

type ScreenProps = { organizationId: string; onSignIn?: () => void; signInError?: string; signInPending?: boolean };
type DetailProps = { organizationId: string } & Pick<ScreenProps, 'onSignIn' | 'signInError' | 'signInPending'>;

/** The trace id in `/admin/traces/<id>`, kept in step with Back and Forward. */
function useTraceRoute(): string | null {
  const [id, setId] = useState(() => routeTab());
  useEffect(() => {
    const onPopState = () => setId(routeTab());
    addEventListener('popstate', onPopState);
    return () => removeEventListener('popstate', onPopState);
  }, []);
  return id && decodeURIComponent(id);
}

export function TracesScreen(props: ScreenProps) {
  const traceId = useTraceRoute();
  if (!props.organizationId) {
    return <EmptyState title="Waiting for organization context" text="Select a confirmed organization to see its request traces." />;
  }
  return (
    <div className="screen">
      {traceId ? <TracePage {...props} traceId={traceId} /> : <Traces {...props} />}
    </div>
  );
}

/** The identity columns filtered by picking a value, with the usage
 * dimension each is looked up by. */
type IdColumn = Extract<UsageDimension, 'api_key' | 'service_account' | 'team' | 'end_user'>;
const idFilters: [IdColumn, string][] = [['api_key', 'API key'], ['service_account', 'Service account'], ['team', 'Team'], ['end_user', 'End user']];
const VALUES_DAYS = 90;

/** The usage slice a trace view's values are looked up in: its time range
 * (the last `VALUES_DAYS` days where unbounded, ending `now`) and the filters
 * both APIs share, without `except`'s own. Trace status is not the usage
 * status, so it is left out. */
export function traceValuesQuery(view: TableView, except: IdColumn, now: number): UsageSliceQuery {
  const { q: _q, status: _status, from, to, ...filters } = traceQuery(view, except);
  const end = to ?? new Date(now).toISOString();
  return { ...filters, from: from ?? new Date(Date.parse(end) - VALUES_DAYS * 86_400_000).toISOString(), to: end };
}

function Traces({ organizationId, ...signIn }: ScreenProps) {
  const [view, setView] = useUrlView('traces');
  const query = useMemo(() => traceQuery(view), [view]);
  // An open-ended range ends when the page opened, so lookups repeat only
  // when the view changes.
  const [now] = useState(() => Date.now());
  const valuesQuery = useCallback((column: IdColumn) => traceValuesQuery(view, column, now), [view, now]);
  const addIdFilter = (column: IdColumn, value: string) => {
    const chosen = view.categories[column] ?? [];
    if (!chosen.includes(value)) setView({ ...view, page: 0, categories: { ...view.categories, [column]: [...chosen, value] } });
  };
  const queryKey = JSON.stringify(query);
  const summarize = useCallback(
    (dimensions: string[], filters: TraceQuery) => api.summarizeTraces(organizationId, dimensions as TraceDimension[], filters),
    [organizationId],
  );
  const { grouped, summary, summaryError, retrySummary, facets, onFacet } = useServerSummary({
    scope: organizationId,
    view,
    dimensionOf: traceDimensionOf,
    queryFor: traceQuery,
    summarize,
    toGroup: group => ({ values: group.values, count: group.count, first_occurred_at: group.first_recorded_at, last_occurred_at: group.last_recorded_at, note: traceGroupNote(group) }),
  });
  // While grouped only the groups show: no traces are read (each read is
  // itself audited).
  const collection = useCollection<TraceSummary>(
    params => (grouped ? Promise.resolve({ items: [], next_page_token: null }) : api.listTraces(organizationId, query, params)),
    [organizationId, queryKey, grouped],
  );
  return (
    <>
      <div className="section-header"><div><h2>Requests</h2><p className="muted">Every request&rsquo;s metadata: model, routing, status, timing and cost. Search by model, deployment, or a trace or request id; group to count requests and their amounts by model, team, status or day. Open a request for its trace, and its content where tracing kept it.</p></div></div>
      {collection.error && (collection.error.status === 400
        ? <Message kind="error">These filters are not valid: check that the time range starts before it ends. Change or clear them below.</Message>
        : <ReportError problem={collection.error} what="traces" onRetry={() => void collection.load(true)} {...signIn} />)}
      {summaryError && (summaryError.status === 400
        ? <Message kind="error">These filters are not valid: check that the time range starts before it ends. Change or clear them below.</Message>
        : <ReportError problem={summaryError} what="trace groups" onRetry={retrySummary} {...signIn} />)}
      <ValueFilterForm
        organizationId={organizationId}
        idPrefix="traces-id-filter"
        dimensions={idFilters}
        initial="api_key"
        queryFor={valuesQuery}
        labelOf={(column, value, label) => label ?? (column === 'end_user' ? value : uuidHint(value) ?? value)}
        onAdd={addIdFilter}
      />
      {collection.loading && !grouped && <Message kind="info">Loading traces…</Message>}
      {/* The table stays on an error, so its filters can be corrected. */}
      <DataTable
        label="Traces"
        urlKey="traces"
        columns={traceColumns}
        rows={collection.loading || collection.error ? [] : collection.items}
        rowKey={trace => trace.id}
        view={view}
        onViewChange={setView}
        detail={trace => (
          <>
            <p><ConsoleLink href={traceHref(trace.id)}>Open this request</ConsoleLink></p>
            <TraceDetails organizationId={organizationId} traceId={trace.id} {...signIn} />
          </>
        )}
        server={{
          facets,
          onFacet,
          groupable: Object.keys(traceDimensionOf),
          maxGroups: 2,
          groups: summary?.groups ?? null,
          truncated: summary?.truncated ?? false,
          total: summary?.total ?? null,
          onDrill: group => setView(drillInto(view, group, 'recorded')),
          hasMore: !grouped && collection.hasMore && !collection.error,
          loadingMore: collection.loadingMore,
          onLoadMore: collection.loadMore,
        }}
      />
      {!collection.loading && !collection.error && !grouped && collection.items.length === 0 && <EmptyState title="No requests found" text="Try broadening the filters or the time range." />}
    </>
  );
}

/** One request on its own page, deep-linkable by its trace id. */
function TracePage({ organizationId, traceId, ...signIn }: DetailProps & { traceId: string }) {
  return (
    <>
      <p><ConsoleLink href="/admin/traces">← All requests</ConsoleLink></p>
      <div className="section-header"><div><h2>Request</h2><p className="muted mono">{traceId}</p></div></div>
      <TraceDetails organizationId={organizationId} traceId={traceId} {...signIn} />
    </>
  );
}

function TraceDetails({ organizationId, traceId, ...signIn }: DetailProps & { traceId: string }) {
  const [trace, setTrace] = useState<TraceDetail | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  useEffect(() => {
    let current = true;
    setTrace(null);
    setProblem(null);
    api.getTrace(organizationId, traceId).then(
      value => { if (current) setTrace(value); },
      caught => { if (current) setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The trace could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, traceId]);
  if (problem?.status === 401) return <SessionExpired action="view this trace" {...signIn} />;
  if (problem?.status === 404) return <EmptyState title="No such trace" text="It does not exist, its record has passed retention, or it belongs to a team you cannot see." />;
  if (problem) return <InlineError problem={problem} />;
  if (!trace) return <Message kind="info">Loading trace…</Message>;
  const rows: [string, React.ReactNode][] = [
    ['Model', `${trace.model} (${trace.deployment})`],
    ['Alias', trace.alias ? `${trace.alias}${trace.operation ? ` · ${trace.operation}` : ''}` : '—'],
    ['Provider', [trace.provider_family, trace.location].filter(Boolean).join(' · ') || 'No catalog provenance'],
    ['Status', trace.status === 'pending' ? 'Pending' : `${trace.outcome ?? 'Finalized'} · ${formatDate(trace.finalized_at)}`],
    ['Duration', formatDuration(trace.duration_ms)],
    ['Amount', <>{formatMoney(trace.amount_minor, trace.currency)} {trace.usage_confidence && <ConfidenceBadge confidence={trace.usage_confidence} />}</>],
    ['Tokens', trace.usage ? `${tokensText(trace.usage.tokens)} · ${trace.usage.completeness}` : 'Not reported'],
    ['Reserved', `${formatMoney(trace.reservation.reserved_minor, trace.currency)} · ${trace.reservation.state}`],
    ['Pricing', `${trace.price_version} · ${trace.pricing_confidence}${trace.routing?.price_source ? ` · ${trace.routing.price_source}` : ''}`],
    ['Reconciliation', trace.reconciliation ? `${trace.reconciliation.reason} · ${trace.reconciliation.status}` : 'None'],
    ['Provider error', providerErrorText(trace.provider_error)],
    ['Team', trace.team_id ?? '—'],
    ['Service account', trace.service_account_id ?? '—'],
    ['API key', trace.api_key_id ?? 'Not recorded'],
    ['End user', trace.end_user ?? '—'],
    ...(trace.batch_id ? [['Batch line', <><span className="mono">{trace.custom_id}</span> in <span className="mono">{trace.batch_id}</span></>] as [string, React.ReactNode]] : []),
    ['Routing', trace.routing ? <span className="mono">release {uuidHint(trace.routing.release_id)} · route {uuidHint(trace.routing.route_id)} · deployment {uuidHint(trace.routing.deployment_id)}</span> : 'No catalog provenance'],
    ...(trace.failover_of || trace.outcome === 'failed-over' ? [['Failover', trace.failover_of
      ? <>Attempt {trace.attempt ?? 2}, after trace <span className="mono">{uuidHint(trace.failover_of)}</span></>
      : 'The provider refused this attempt before any work; the request was tried on another deployment of its route.'] as [string, React.ReactNode]] : []),
    ['Trace', contentStateLabel(trace.content_state)],
    ['Correlation ID', trace.correlation_id],
    ['Provider request ID', trace.provider_request_id ?? '—'],
    ['Trace ID', trace.id],
  ];
  // The request's record on the left, what tracing kept of it on the right.
  return (
    <div className="trace-detail-grid">
      <dl className="detail-list">
        {rows.map(([label, value]) => <React.Fragment key={label}><dt>{label}</dt><dd>{value}</dd></React.Fragment>)}
      </dl>
      <TraceContentSection organizationId={organizationId} traceId={traceId} state={trace.content_state} {...signIn} />
    </div>
  );
}

/** A request's tokens: input and output, then the cached input, reasoning
 * and other tokens it reported, each charged on its own. Output excludes
 * reasoning. */
export function tokensText(tokens: { [K in keyof TokenTotals]: number | null }): string {
  const extra: [number | null, string][] = [
    [tokens.cached_input, 'cached'],
    [tokens.reasoning, 'reasoning'],
    [tokens.other, 'other'],
  ];
  const shown = extra.filter(([value]) => value !== null && value > 0).map(([value, label]) => `${formatCount(value)} ${label}`);
  return [`${formatCount(tokens.input)} in / ${formatCount(tokens.output)} out`, ...shown].join(' · ');
}

/** Why a trace has no content to show. */
export function contentStateText(state: TraceContent['state']): string | null {
  switch (state) {
    case 'stored': return null;
    case 'pending': return 'The content is still being recorded. Check again in a moment.';
    case 'not_traced': return 'This request was not traced. The tracing policy under Policies → Tracing chooses which requests are.';
    case 'off': return 'Traced without content: the tracing policy keeps timing only.';
    case 'unavailable': return 'The content was not kept: no trace store was configured.';
    case 'failed': return 'The content could not be written to the trace store.';
    case 'expired': return 'The content has expired and was deleted.';
  }
}

/** Milliseconds from `from` to `to`, when both are known. */
export function elapsed(from: string | null, to: string | null): string | null {
  if (!from || !to) return null;
  return `${formatCount(Math.max(0, new Date(to).getTime() - new Date(from).getTime()))} ms`;
}

/** What the content API calls a trace's content state, from its record. */
export function contentStateOf(state: ContentState | null): TraceContent['state'] {
  if (state === null) return 'not_traced';
  return state === 'writing' ? 'pending' : state;
}

/** A traced request's prompt and response. Only content that was kept, or is
 * still being written, is read (every read is audited, and only owners and
 * admins may read it); otherwise the section says why there is none. */
export function TraceContentSection({ organizationId, traceId, state, ...signIn }: DetailProps & { traceId: string; state: ContentState | null }) {
  const [content, setContent] = useState<TraceContent | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [loading, setLoading] = useState(false);
  const readable = state === 'stored' || state === 'writing';
  const load = useCallback(async (isCurrent: () => boolean = () => true) => {
    setLoading(true);
    setProblem(null);
    try {
      const value = await api.getTraceContent(organizationId, traceId);
      if (isCurrent()) setContent(value);
    } catch (caught) {
      if (isCurrent()) setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The trace content could not be loaded.'));
    } finally {
      if (isCurrent()) setLoading(false);
    }
  }, [organizationId, traceId]);
  useEffect(() => {
    setContent(null);
    setProblem(null);
    if (!readable) return;
    let current = true;
    void load(() => current);
    return () => { current = false; };
  }, [load, readable]);
  if (problem?.status === 401) return <SessionExpired action="view this request's content" {...signIn} />;
  const shownState = content?.state ?? contentStateOf(state);
  const why = contentStateText(shownState);
  const timing: [string, string | null][] = content ? [
    ['Gateway to provider', elapsed(content.started_at, content.provider_started_at)],
    ['First event', elapsed(content.provider_started_at, content.first_event_at)],
    ['Total', elapsed(content.started_at, content.finished_at)],
  ] : [];
  const shownTiming = timing.filter((entry): entry is [string, string] => entry[1] !== null);
  return (
    <section className="trace-content" aria-label="Content">
      <h4>Content</h4>
      {problem?.status === 403
        ? <Message kind="info">Only owners and admins can read request content.</Message>
        : <InlineError problem={problem} />}
      {problem && problem.status !== 403 && <Button variant="ghost" onClick={() => void load()} loading={loading}>Try again</Button>}
      {readable && !content && !problem && <p className="meta">Loading content…</p>}
      {why && (!readable || content) && <p className="meta">{why}</p>}
      {content?.state === 'pending' && <Button variant="ghost" onClick={() => void load()} loading={loading}>Check again</Button>}
      {shownTiming.length > 0 && (
        <dl className="detail-list">
          {shownTiming.map(([label, value]) => <React.Fragment key={label}><dt>{label}</dt><dd>{value}</dd></React.Fragment>)}
        </dl>
      )}
      {content?.state === 'stored' && (
        <section aria-label="Request">
          <h5>Request{content.truncated.request && <span className="meta"> · truncated at the size limit</span>}</h5>
          {!content.request && <p className="meta">Too large to keep any of it.</p>}
          {content.request && 'input' in content.request && content.request.input.map((text, index) => (
            <div key={index} className="trace-message">
              <div className="meta">input {index}</div>
              <pre className="trace-text">{text}</pre>
            </div>
          ))}
          {content.request && 'messages' in content.request && content.request.messages.map((message, index) => (
            <div key={index} className="trace-message">
              <div className="meta">{message.role}{message.name ? ` · ${message.name}` : ''}{message.tool_call_id ? ` · ${message.tool_call_id}` : ''}</div>
              <pre className="trace-text">{message.content}</pre>
            </div>
          ))}
        </section>
      )}
      {content?.state === 'stored' && (
        <section aria-label="Response">
          <h5>Response{content.truncated.response && <span className="meta"> · truncated at the size limit</span>}</h5>
          {!content.response && <p className="meta">The provider returned no usable response.</p>}
          {content.response && 'embeddings' in content.response && content.response.embeddings.map(embedding => (
            <div key={embedding.index} className="trace-message">
              <div className="meta">{content.response!.model} · vector {embedding.index} · {formatCount(embedding.dimensions)} dimensions</div>
              <pre className="trace-text">{`[${embedding.preview.join(', ')}${embedding.dimensions > embedding.preview.length ? ', …' : ''}]`}</pre>
            </div>
          ))}
          {content.response && 'choices' in content.response && content.response.choices.map((choice, _, choices) => (
            <div key={choice.index} className="trace-message">
              <div className="meta">{content.response!.model}{choices.length > 1 ? ` · choice ${choice.index}` : ''}{choice.finish_reason ? ` · ${choice.finish_reason}` : ''}</div>
              {choice.content && <pre className="trace-text">{choice.content}</pre>}
              {choice.refusal && <pre className="trace-text">Refused: {choice.refusal}</pre>}
              {choice.tool_calls?.map((call, index) => (
                <pre key={index} className="trace-text">{`${call.name ?? 'tool'}(${call.arguments})`}</pre>
              ))}
            </div>
          ))}
        </section>
      )}
      {readable && <p className="meta">Reading content is recorded in the audit log.</p>}
    </section>
  );
}

export function ConfidenceBadge({ confidence }: { confidence: string }) {
  const kind = confidence === 'exact' ? 'success' : confidence === 'unknown' ? 'danger' : 'neutral';
  return <span title="How the usage was measured"><Badge status={confidence} kind={kind} /></span>;
}
