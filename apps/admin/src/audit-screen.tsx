import React, { useCallback, useEffect, useMemo, useState } from 'react';
import { api, type AuditDimension, type AuditEvent, type AuditEventFilters } from './api';
import { DataTable, useUrlView, type Column, type ServerGroup } from './data-table';
import { emptyView, type TableView } from './table-view';
import { drillInto as drillIntoGroup, toUtc, useServerSummary } from './server-summary';
import { useCollection } from './use-collection';
import { Button, EmptyState, formatDate, uuidHint, TextInput, CheckOption, FormField, Message } from './ui';

export const auditTargetTypes = ['organization', 'team', 'membership', 'role_binding', 'service_account', 'api_key', 'usage_report', 'inference_trace', 'reconciliation_item'] as const;
const uuidPattern = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

export function isUuid(value: string) { return uuidPattern.test(value); }

/** A typed time (`datetime-local`, local time) or an RFC 3339 one, as UTC. */
export const toAuditUtc = toUtc;

const actorOf = (event: AuditEvent) => event.actor.user_id ?? event.actor.service_account_id;
const actorTypeLabels: Record<string, string> = { user: 'Person or agent', service_account: 'Service account', system: 'System' };

/** Past tenses of the verbs actions end in. */
const pastTense: Record<string, string> = {
  create: 'created', update: 'updated', delete: 'deleted', set: 'set', revoke: 'revoked', disable: 'disabled', enable: 'enabled',
  activate: 'activated', deactivate: 'deactivated', rotate: 'rotated', rotate_secret: 'secret rotated', resolve: 'resolved',
  redeliver: 'redelivered', read: 'viewed', list: 'listed', pin: 'pinned', unpin: 'unpinned', test: 'tested', accept: 'accepted',
};
/** Resource names that read better than their identifiers. */
const resourceLabels: Record<string, string> = {
  provider_credential: 'Cloud credential', api_key: 'API key', role_binding: 'Role binding', service_account: 'Service account',
  catalog_release: 'Catalog release', trace_content: 'Trace content', usage: 'Usage', trace: 'Traces', audit: 'Audit log',
  reconciliation: 'Reconciliation items', webhook_endpoint: 'Webhook endpoint', price_override: 'Price override',
};

/** `provider_credential.set` as "Cloud credential set": the resource, then
 * what happened to it. The code stays in the cell's title. */
export function actionLabel(action: string): string {
  const [resource, ...rest] = action.split('.');
  const verb = rest.join('.');
  const noun = resourceLabels[resource] ?? resource.replace(/_/g, ' ').replace(/^./, first => first.toUpperCase());
  if (!verb) return noun;
  return `${noun} ${pastTense[verb] ?? verb.replace(/[._]/g, ' ')}`;
}

/** Whether an action only records someone reading data. */
export const isView = (action: string) => /\.(read|list)$/.test(action);

const targetTypeLabels: Record<string, string> = {
  organization: 'Organization', team: 'Team', membership: 'Membership', role_binding: 'Role binding', service_account: 'Service account',
  api_key: 'API key', usage_report: 'Usage report', inference_trace: 'Trace', reconciliation_item: 'Reconciliation item',
};
const targetTypeLabel = (value: string) => targetTypeLabels[value] ?? value.replace(/_/g, ' ');

/** Who acted: their name when the gateway knows it, else their kind and a
 * short ID. */
export function actorLabel(event: AuditEvent): string {
  if (event.actor.name) return event.actor.name;
  const id = actorOf(event);
  const kind = actorTypeLabels[event.actor.actor_type] ?? event.actor.actor_type;
  return id ? `${kind} ${uuidHint(id)}` : kind;
}

/** What it acted on: a team's or service account's name, else its kind and
 * a short ID. */
export function targetLabel(event: AuditEvent): string {
  return event.target_name ?? `${targetTypeLabel(event.target_type)} ${uuidHint(event.target_id)}`;
}

/** Columns, by the audit API's names; `occurred` groups by UTC day. */
export const auditColumns: Column<AuditEvent>[] = [
  { id: 'action', label: 'Action', primary: true, filter: 'category', format: actionLabel, value: event => event.action, title: event => event.action, render: event => actionLabel(event.action) },
  {
    id: 'outcome',
    label: 'Outcome',
    filter: 'category',
    value: event => event.outcome,
    render: event => {
      const succeeded = event.outcome === 'succeeded';
      return <span className={`audit-outcome ${succeeded ? 'succeeded' : event.outcome}`}><span aria-hidden="true">{succeeded ? '✓' : '!'}</span> {event.outcome}</span>;
    },
  },
  { id: 'occurred', label: 'Occurred', filter: 'date', value: event => event.occurred_at, title: event => event.occurred_at, render: event => formatDate(event.occurred_at) },
  { id: 'actor', label: 'Actor', value: event => actorOf(event), title: event => actorOf(event) ?? undefined, render: event => actorLabel(event) },
  { id: 'actor_type', label: 'Actor type', filter: 'category', hidden: true, format: value => actorTypeLabels[value] ?? value, value: event => event.actor.actor_type },
  { id: 'target_type', label: 'Target type', filter: 'category', hidden: true, format: targetTypeLabel, value: event => event.target_type },
  { id: 'target', label: 'Target', value: event => event.target_id, title: event => event.target_id, render: event => targetLabel(event) },
  { id: 'request', label: 'Request ID', hidden: true, value: event => event.request_id, title: event => event.request_id ?? undefined, render: event => uuidHint(event.request_id ?? '') ?? '—' },
];

/** The summary dimension each column groups by, and back. */
const dimensionOf: Record<string, string> = { action: 'action', outcome: 'outcome', target_type: 'target_type', actor_type: 'actor_type', actor: 'actor', occurred: 'day' };
const columnOf: Record<string, string> = Object.fromEntries(Object.entries(dimensionOf).map(([column, dimension]) => [dimension, column]));

/** A summary group's values by column id rather than by dimension. */
export function byColumn(values: Record<string, string | null>): Record<string, string | null> {
  return Object.fromEntries(Object.entries(values).map(([dimension, value]) => [columnOf[dimension] ?? dimension, value]));
}
const listed = (values: string[] | undefined) => (values?.length ? values.join(',') : undefined);

/** The audit API query for a table view. `actor` and `target` take one ID. */
export function auditQuery(view: TableView, except?: string): AuditEventFilters {
  const category = (id: string) => (id === except ? undefined : view.categories[id]);
  const range = except === 'occurred' ? undefined : view.ranges.occurred;
  const actor = category('actor')?.[0];
  const target = category('target')?.[0];
  return {
    action: listed(category('action')),
    outcome: listed(category('outcome')),
    target_type: listed(category('target_type')),
    actor_type: listed(category('actor_type')),
    actor: actor && isUuid(actor) ? actor : undefined,
    target_id: target && isUuid(target) ? target : undefined,
    q: view.search.trim() || undefined,
    occurred_after: toAuditUtc(range?.min),
    occurred_before: toAuditUtc(range?.max),
  };
}

/** The view narrowed to one summary group (values by column id), ungrouped. */
export function drillInto(view: TableView, group: ServerGroup): TableView {
  return drillIntoGroup(view, group, 'occurred');
}

export function AuditScreen({ organizationId, onSignIn, signInError, signInPending }: { organizationId: string; onSignIn?: () => void; signInError?: string; signInPending?: boolean }) {
  if (!organizationId) {
    return <EmptyState title="Waiting for organization context" text="Select a confirmed organization to review audit events." />;
  }
  return <AuditEvents organizationId={organizationId} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
}

/** Whether views are shown, kept in the address (`audit.views=1`) beside
 * the table's filters, so a shared or reloaded search that names a view
 * action still finds it. */
function useShowViews(): [boolean, (next: boolean) => void] {
  const read = () => new URLSearchParams(location.search).get('audit.views') === '1';
  const [showViews, setState] = useState(read);
  useEffect(() => {
    const reread = () => setState(read());
    addEventListener('popstate', reread);
    return () => removeEventListener('popstate', reread);
  }, []);
  const set = (next: boolean) => {
    const params = new URLSearchParams(location.search);
    if (next) params.set('audit.views', '1');
    else params.delete('audit.views');
    const search = params.toString();
    history.replaceState(history.state, '', `${location.pathname}${search ? `?${search}` : ''}${location.hash}`);
    setState(next);
  };
  return [showViews, set];
}

function AuditEvents({ organizationId, onSignIn, signInError, signInPending }: { organizationId: string; onSignIn?: () => void; signInError?: string; signInPending?: boolean }) {
  const [view, setView] = useUrlView('audit');
  // Views (someone reading usage, traces or this log) are left out unless
  // asked for, so changes stand out.
  const [showViews, setShowViews] = useShowViews();
  const queryFor = useCallback(
    (current: TableView, except?: string): AuditEventFilters => ({ ...auditQuery(current, except), exclude_views: showViews ? undefined : 'true' }),
    [showViews],
  );
  const query = useMemo(() => queryFor(view), [view, queryFor]);
  const queryKey = JSON.stringify(query);
  const summarize = useCallback(
    (dimensions: string[], filters: AuditEventFilters) => api.summarizeAuditEvents(organizationId, dimensions as AuditDimension[], filters),
    [organizationId],
  );
  const { grouped, summary, summaryError, retrySummary, facets, onFacet } = useServerSummary({
    scope: organizationId,
    view,
    dimensionOf,
    queryFor,
    summarize,
    toGroup: group => group,
  });
  // While grouped only the groups show: no events are read (each read is
  // itself audited).
  const collection = useCollection<AuditEvent>(
    params => (grouped ? Promise.resolve({ items: [], next_page_token: null }) : api.listAuditEvents(organizationId, { ...params, ...query })),
    [organizationId, queryKey, grouped],
  );
  const narrow = (next: Partial<TableView>) => setView({ ...emptyView, columns: view.columns, ...next });

  return (
    <div className="audit-screen">
      <div className="section-header audit-heading"><div><h2>Event stream</h2><p className="muted">Who changed what, metadata only. Search matches the action or target type; group to count events by action, outcome, actor or day.</p></div>
        <CheckOption className="audit-views" checked={showViews} onChange={event => setShowViews(event.target.checked)}>Show views <span className="muted">(someone reading usage, traces or this log)</span></CheckOption>
      </div>
      <FindById view={view} onFind={categories => setView({ ...view, categories: { ...view.categories, ...categories }, group: [], page: 0 })} />
      {collection.error && (collection.error.status === 400
        ? <Message kind="error">These filters are not valid: check that the time range starts before it ends. Change or clear them below.</Message>
        : <AuditError status={collection.error.status} onRetry={() => void collection.load(true)} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />)}
      {summaryError && (summaryError.status === 400
        ? <Message kind="error">These filters are not valid: check that the time range starts before it ends. Change or clear them below.</Message>
        : <AuditError status={summaryError.status} onRetry={retrySummary} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />)}
      {collection.loading && !grouped && <Message kind="info">Loading audit events…</Message>}
      {/* The table stays on an error, so its filters can be corrected. */}
      <DataTable
        label="Audit events"
        urlKey="audit"
        columns={auditColumns}
        rows={collection.loading || collection.error ? [] : collection.items}
        rowKey={event => event.id}
        view={view}
        onViewChange={setView}
        detail={event => <AuditDetail event={event} onNarrow={narrow} />}
        server={{
          facets,
          onFacet,
          groupable: Object.keys(dimensionOf),
          maxGroups: 2,
          groups: summary?.groups ?? null,
          truncated: summary?.truncated ?? false,
          total: summary?.total ?? null,
          onDrill: group => setView(drillInto(view, group)),
          hasMore: !grouped && collection.hasMore && !collection.error,
          loadingMore: collection.loadingMore,
          onLoadMore: collection.loadMore,
        }}
      />
      {!collection.loading && !collection.error && !grouped && collection.items.length === 0 && <EmptyState title="No audit events found" text="Try broadening the filters. Raw metadata, secrets, prompts, and completions are never shown here." />}
    </div>
  );
}

/** Events by a known actor or target ID, typed in. */
function FindById({ view, onFind }: { view: TableView; onFind: (categories: Record<string, string[]>) => void }) {
  const [actor, setActor] = useState(view.categories.actor?.[0] ?? '');
  const [target, setTarget] = useState(view.categories.target?.[0] ?? '');
  const [error, setError] = useState<string | null>(null);
  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    const bad = [actor && !isUuid(actor.trim()) && 'actor', target && !isUuid(target.trim()) && 'target'].filter(Boolean);
    if (bad.length) {
      setError(`Enter the ${bad.join(' and ')} as a UUID.`);
      return;
    }
    setError(null);
    // An emptied field clears its filter.
    onFind({ actor: actor.trim() ? [actor.trim()] : [], target: target.trim() ? [target.trim()] : [] });
  };
  return (
    <form className="audit-filters" onSubmit={submit} aria-label="Find events by ID">
      <div className="audit-filter"><FormField label="Actor ID" htmlFor="audit-actor"><TextInput id="audit-actor" value={actor} placeholder="a user or service account UUID" aria-invalid={!!error} onChange={event => setActor(event.target.value)}/></FormField></div>
      <div className="audit-filter"><FormField label="Target ID" htmlFor="audit-target-id"><TextInput id="audit-target-id" value={target} placeholder="the UUID of what was changed" aria-invalid={!!error} onChange={event => setTarget(event.target.value)}/></FormField></div>
      <div className="audit-filter-actions"><Button variant="ghost" type="submit">Find</Button></div>
      {error && <span className="field-error" role="alert">{error}</span>}
    </form>
  );
}

/** An event in full, with shortcuts to everything by its actor or target. */
function AuditDetail({ event, onNarrow }: { event: AuditEvent; onNarrow: (view: Partial<TableView>) => void }) {
  const actor = actorOf(event);
  return (
    <dl className="detail-list">
      <dt>Action</dt><dd>{actionLabel(event.action)} <span className="muted">(<code>{event.action}</code>)</span></dd>
      <dt>Event</dt><dd><code>{event.id}</code></dd>
      <dt>Occurred</dt><dd>{new Date(event.occurred_at).toLocaleString()} <span className="muted">({event.occurred_at})</span></dd>
      <dt>Actor</dt>
      <dd>
        {actorTypeLabels[event.actor.actor_type] ?? event.actor.actor_type}{event.actor.name && <> · {event.actor.name}</>}{actor && <> · <code>{actor}</code> <button className="inline-action" onClick={() => onNarrow({ categories: { actor: [actor] } })}>Everything by this actor</button></>}
      </dd>
      <dt>Target</dt>
      <dd>{targetTypeLabel(event.target_type)}{event.target_name && <> · {event.target_name}</>} · <code>{event.target_id}</code> <button className="inline-action" onClick={() => onNarrow({ categories: { target: [event.target_id] } })}>Everything about this target</button></dd>
      {event.team_id && <><dt>Team</dt><dd><code>{event.team_id}</code></dd></>}
      <dt>Request</dt><dd>{event.request_id ? <code>{event.request_id}</code> : <span className="muted">—</span>}</dd>
    </dl>
  );
}

export function AuditError({ status, onRetry, onSignIn, signInError, signInPending }: { status: number; onRetry: () => void; onSignIn?: () => void; signInError?: string; signInPending?: boolean }) {
  if (status === 401) return <Message kind="error">Your admin session has expired. <button className="inline-action" onClick={onSignIn} disabled={signInPending} aria-describedby={signInError ? 'audit-sign-in-error' : undefined}>{signInPending ? 'Starting sign in…' : 'Sign in again'}</button> to view audit events.{signInError && <span id="audit-sign-in-error" className="field-error" role="status" aria-live="polite"> {signInError}</span>}</Message>;
  if (status === 403) return <Message kind="error">You do not have permission to view this organization’s audit events.</Message>;
  return <Message kind="error">Audit events could not be loaded. <button className="inline-action" onClick={onRetry}>Try again</button></Message>;
}
