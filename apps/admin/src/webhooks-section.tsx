import React, { useRef, useState } from 'react';
import {
  api,
  newIdempotencyKey,
  webhookEventTypes,
  type ApiProblem,
  type WebhookDelivery,
  type WebhookEndpoint,
  type WebhookSecretResponse,
} from './api';
import { useSecretReveal } from './mutation-state';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { useCollection } from './use-collection';
import { Button, DataCell, Dialog, EmptyState, FormField, LoadMore, SecretRevealPanel, TextInput, formatDate, CheckOption, Badge, Message } from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow } from './ui';
import { deliveryTime, eventTypeHints, formValues, lastResponse, rebaseValues, secretOutcome, webhookInput, webhookUpdate, type WebhookFormErrors, type WebhookFormValues } from './webhook-forms';

type Props = SignIn & { organizationId: string; canChange: boolean };
type Notice = { kind: 'status' | 'alert'; text: string } | null;

const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

/** Create and rotate need somewhere to keep the secret; without it the
 * server refuses, and only an operator can fix that. */
function EncryptionUnavailable() {
  return (
    <Message kind="error">
      Webhook signing secrets can’t be stored because this gateway has no credential encryption configured. An operator must
      set <code>GATEWAY_CREDENTIAL_ENCRYPTION_KEY</code> (or a KMS key) on every gateway process, as described in the
      operations guide under “Encryption of stored secrets”. Then try again.
    </Message>
  );
}

function ChangeError({ problem, action, ...signIn }: SignIn & { problem: ApiProblem | null; action: string }) {
  if (problem?.code === 'credential_encryption_unavailable') return <EncryptionUnavailable />;
  return <ActionError problem={problem} action={action} {...signIn} />;
}

export function Webhooks({ organizationId, canChange, ...signIn }: Props) {
  const endpoints = useCollection<WebhookEndpoint>(params => api.listWebhookEndpoints(organizationId, params), [organizationId]);
  const [editing, setEditing] = useState<WebhookEndpoint | 'new' | null>(null);
  const [deleting, setDeleting] = useState<WebhookEndpoint | null>(null);
  const [rotating, setRotating] = useState<WebhookEndpoint | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice>(null);
  // The secret lives only here, until dismissed; it is never stored or logged.
  const secret = useSecretReveal();
  const reload = () => void endpoints.load(true);
  const selected = endpoints.items.find(endpoint => endpoint.id === selectedId) ?? null;
  // A new secret would replace one not yet saved: save or discard it first.
  const secretShown = secret.secret !== null;
  const secretFirst = 'Save the signing secret shown above, then dismiss it, before adding an endpoint or rotating a secret.';

  const showSecret = (response: WebhookSecretResponse, requestId: string | undefined, replayed: string) => {
    const outcome = secretOutcome(response);
    if ('secret' in outcome) {
      setNotice(null);
      secret.reveal(outcome.secret, `Signing secret for ${response.endpoint.url}`, requestId);
    } else {
      secret.dismiss();
      setNotice({ kind: 'alert', text: replayed });
    }
  };

  const addEndpoint = canChange
    ? <Button disabled={secretShown} title={secretShown ? secretFirst : undefined} onClick={() => { setNotice(null); setEditing('new'); }}>Add endpoint</Button>
    : null;
  return (
    <>
      <div className="section-header">
        <div>
          <h2>Webhooks</h2>
          <p className="muted">
            HTTPS endpoints that receive this organization’s events, signed with a secret per endpoint (Standard Webhooks). Failed deliveries are
            retried for about a day, then parked until someone redelivers them.
          </p>
        </div>
      </div>
      {!canChange && <p className="muted">You can view webhooks. Only organization owners and admins can change them.</p>}
      {secret.secret && (
        <SecretRevealPanel
          secret={secret.secret.secret}
          label={secret.secret.label}
          requestId={secret.secret.requestId}
          valueLabel="Webhook signing secret"
          onDismiss={secret.dismiss}
        />
      )}
      {secretShown && canChange && <p className="cds--form__helper-text">{secretFirst}</p>}
      {notice && <Message kind={notice.kind === 'alert' ? 'error' : 'success'}>{notice.text}</Message>}
      {endpoints.loading && <Message kind="info">Loading webhook endpoints…</Message>}
      {endpoints.error && <ReportError problem={endpoints.error} what="webhook endpoints" onRetry={reload} {...signIn} />}
      {!endpoints.loading && !endpoints.error && endpoints.items.length === 0 && (
        <EmptyState
          title="No webhook endpoints"
          text={canChange ? 'Add an endpoint to receive inference and reconciliation events as they happen.' : 'No endpoint receives this organization’s events.'}
          action={addEndpoint}
        />
      )}
      {!endpoints.loading && !endpoints.error && endpoints.items.length > 0 && addEndpoint && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Webhook endpoint actions">{addEndpoint}</div>
      )}
      {!endpoints.loading && !endpoints.error && endpoints.items.length > 0 && (
        <DataGrid aria-label="Webhook endpoints">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Endpoint</TableHeader>
              <TableHeader>Events</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Created</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {endpoints.items.map(endpoint => (
              <TableRow className="data-row" key={endpoint.id}>
                <DataCell label="Endpoint" className="cell-primary cell-wrap">
                  {endpoint.url}
                  {endpoint.description && <span className="muted cell-note">{endpoint.description}</span>}
                </DataCell>
                <DataCell label="Events">
                  <ul className="operation-list">{endpoint.event_types.map(type => <li key={type}><code>{type}</code></li>)}</ul>
                </DataCell>
                <DataCell label="Status">
                  <Badge tone={endpoint.enabled ? 'badge-success' : 'badge-danger'}>{endpoint.enabled ? 'Enabled' : 'Disabled'}</Badge>
                </DataCell>
                <DataCell label="Created" title={endpoint.created_at}>{formatDate(endpoint.created_at)}</DataCell>
                <DataCell label="Actions">
                  <span className="row-actions">
                    <button className="inline-action" aria-pressed={selectedId === endpoint.id} aria-label={`Deliveries to ${endpoint.url}`} onClick={() => setSelectedId(selectedId === endpoint.id ? null : endpoint.id)}>
                      Deliveries
                    </button>
                    {canChange && (
                      <>
                        <button className="inline-action" aria-label={`Edit ${endpoint.url}`} onClick={() => { setNotice(null); setEditing(endpoint); }}>Edit</button>
                        <button className="inline-action" aria-label={`Rotate the secret of ${endpoint.url}`} disabled={secretShown} title={secretShown ? secretFirst : undefined} onClick={() => { setNotice(null); setRotating(endpoint); }}>Rotate secret</button>
                        <button className="inline-action" aria-label={`Delete ${endpoint.url}`} onClick={() => { setNotice(null); setDeleting(endpoint); }}>Delete</button>
                      </>
                    )}
                  </span>
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={endpoints.loadMore} hasMore={endpoints.hasMore && !endpoints.error} loading={endpoints.loadingMore} />
      {selected && <Deliveries key={selected.id} {...signIn} organizationId={organizationId} endpoint={selected} canChange={canChange} />}
      {editing && (
        <EndpointDialog
          {...signIn}
          organizationId={organizationId}
          endpoint={editing === 'new' ? null : editing}
          onClose={() => setEditing(null)}
          onCreated={(response, requestId) => {
            setEditing(null);
            showSecret(response, requestId, 'This endpoint was created by an earlier attempt of the same request, so its signing secret can’t be shown again. If you didn’t save it, use Rotate secret to get a new one.');
            reload();
          }}
          onSaved={() => { setEditing(null); setNotice({ kind: 'status', text: 'Endpoint saved. The change applies to events sent from now on.' }); reload(); }}
        />
      )}
      {rotating && (
        <ConfirmChange
          {...signIn}
          title="Rotate signing secret?"
          confirm="Rotate secret"
          action="change webhooks"
          onClose={() => setRotating(null)}
          onConflict={reload}
          run={async key => {
            const { data, meta } = await api.rotateWebhookSecret(organizationId, rotating, key);
            setRotating(null);
            showSecret(data, meta.requestId, 'The secret was rotated by an earlier attempt of the same request, so it can’t be shown again. If you didn’t save it, rotate again.');
            reload();
          }}
        >
          The current secret of {rotating.url} stops signing deliveries now, including retries of pending ones. Update your receiver
          with the new secret, which is shown once.
        </ConfirmChange>
      )}
      {deleting && (
        <ConfirmChange
          {...signIn}
          title="Delete webhook endpoint?"
          confirm="Delete endpoint"
          action="change webhooks"
          onClose={() => setDeleting(null)}
          onConflict={reload}
          run={async key => {
            await api.deleteWebhookEndpoint(organizationId, deleting, key);
            if (selectedId === deleting.id) setSelectedId(null);
            setDeleting(null);
            setNotice({ kind: 'status', text: 'Endpoint deleted, with all of its deliveries.' });
            reload();
          }}
        >
          {deleting.url} stops receiving events, and all of its deliveries are deleted, pending ones included. This cannot be undone.
        </ConfirmChange>
      )}
    </>
  );
}

/** A confirmation for a change that cannot be undone. Its key is kept while
 * the dialog is open, so trying again replays rather than repeating it. */
function ConfirmChange({ title, confirm, action, children, run, onClose, onConflict, ...signIn }: SignIn & {
  title: string;
  confirm: string;
  action: string;
  children: React.ReactNode;
  run: (idempotencyKey: string) => Promise<void>;
  onClose: () => void;
  onConflict: () => void;
}) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const key = useRef(newIdempotencyKey());
  const submit = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      await run(key.current);
    } catch (caught) {
      const next = problemOf(caught, 'The change could not be made. Check your connection and try again.');
      setProblem(next);
      setSubmitting(false);
      if (next.code === 'version_conflict') onConflict();
    }
  };
  return (
    <Dialog open alert onClose={() => { if (!submitting) onClose(); }} title={title}>
      <div className="dialog-body"><p>{children}</p></div>
      {problem?.code === 'version_conflict'
        ? <Message kind="error">Someone changed this endpoint since the list loaded. The list has been reloaded: cancel, then try again on the current endpoint.</Message>
        : <ChangeError problem={problem} action={action} {...signIn} />}
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button variant="danger" onClick={() => void submit()} loading={submitting}>{confirm}</Button>
      </div>
    </Dialog>
  );
}

function EndpointDialog({ organizationId, endpoint, onClose, onCreated, onSaved, ...signIn }: SignIn & {
  organizationId: string;
  endpoint: WebhookEndpoint | null;
  onClose: () => void;
  onCreated: (response: WebhookSecretResponse, requestId?: string) => void;
  onSaved: () => void;
}) {
  // What the edit applies to; replaced by the current endpoint after a
  // version conflict, keeping the edits.
  const [base, setBase] = useState(endpoint);
  const [values, setValues] = useState<WebhookFormValues>(() => formValues(endpoint));
  const [errors, setErrors] = useState<WebhookFormErrors & { form?: string }>({});
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [refreshed, setRefreshed] = useState(false);
  // One attempt per form content: a retry reuses its key, so a save whose
  // response was lost replays instead of creating a second endpoint.
  const attempt = useRef<{ body: string; key: string } | null>(null);
  const set = (change: Partial<WebhookFormValues>) => setValues(previous => ({ ...previous, ...change }));
  const toggle = (type: (typeof webhookEventTypes)[number], on: boolean) =>
    set({ eventTypes: on ? webhookEventTypes.filter(item => item === type || values.eventTypes.includes(item)) : values.eventTypes.filter(item => item !== type) });

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const built = webhookInput(values);
    if ('errors' in built) {
      setErrors(built.errors);
      return;
    }
    const update = base ? webhookUpdate(base, built.input) : null;
    if (base && !update) {
      setErrors({ form: 'Nothing has changed. Edit a field, or cancel.' });
      return;
    }
    setErrors({});
    setProblem(null);
    setSubmitting(true);
    try {
      const body = JSON.stringify([base?.version ?? null, update ?? built.input]);
      if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
      if (base && update) {
        await api.updateWebhookEndpoint(organizationId, base, update, attempt.current.key);
        onSaved();
      } else {
        const { data, meta } = await api.createWebhookEndpoint(organizationId, built.input, attempt.current.key);
        onCreated(data, meta.requestId);
      }
    } catch (caught) {
      setProblem(problemOf(caught, 'The endpoint could not be saved. Check your connection and try again.'));
      setSubmitting(false);
    }
  };

  const loadCurrent = async () => {
    if (!base) return;
    try {
      const current = await api.getWebhookEndpoint(organizationId, base.id);
      setValues(previous => rebaseValues(previous, base, current));
      setBase(current);
      setProblem(null);
      setRefreshed(true);
    } catch (caught) {
      setProblem(problemOf(caught, 'The current endpoint could not be loaded.'));
    }
  };

  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={endpoint ? 'Edit webhook endpoint' : 'Add webhook endpoint'}>
      <form onSubmit={submit}>
        <fieldset className="plain-fieldset dialog-body" disabled={submitting}>
          <FormField label="URL" htmlFor="webhook-url" hint="An https URL on a public host, without a user name, password or #fragment." error={errors.url}>
            <TextInput id="webhook-url" type="url" inputMode="url" autoComplete="off" placeholder="https://example.com/hooks/gateway" value={values.url} onChange={event => set({ url: event.target.value })} />
          </FormField>
          <FormField label="Description (optional)" htmlFor="webhook-description" hint="Up to 200 characters, to tell endpoints apart." error={errors.description}>
            <TextInput id="webhook-description" maxLength={200} value={values.description} onChange={event => set({ description: event.target.value })} />
          </FormField>
          <fieldset className="plain-fieldset" aria-describedby={errors.eventTypes ? 'webhook-events-error' : undefined}>
            <legend>Events</legend>
            {webhookEventTypes.map(type => (
              <CheckOption key={type} className="event-option" id={`webhook-event-${type}`} checked={values.eventTypes.includes(type)} onChange={event => toggle(type, event.target.checked)}><span><code>{type}</code><span className="cds--form__helper-text">{eventTypeHints[type]}</span></span></CheckOption>
            ))}
            {errors.eventTypes && <span id="webhook-events-error" className="field-error" role="alert">{errors.eventTypes}</span>}
          </fieldset>
          <CheckOption id="webhook-enabled" checked={values.enabled} onChange={event => set({ enabled: event.target.checked })}>Enabled: send events to this endpoint</CheckOption>
          {base && !values.enabled && base.enabled && <p className="cds--form__helper-text">Disabling stops new deliveries and pauses pending ones until the endpoint is enabled again.</p>}
          {!base && <p className="cds--form__helper-text">The endpoint receives events that occur after it is created. Its signing secret is shown once, after saving.</p>}
          {errors.form && <Message kind="error">{errors.form}</Message>}
        </fieldset>
        {problem?.code === 'version_conflict' ? (
          <Message kind="error">
            Someone changed this endpoint since you opened it.{' '}
            <button type="button" className="inline-action" onClick={() => void loadCurrent()}>Load the current endpoint</button>; your edits are kept.
          </Message>
        ) : <ChangeError problem={problem} action="change webhooks" {...signIn} />}
        {refreshed && base && !problem && (
          <Message kind="info">
            Loaded the current endpoint (updated {formatDate(base.updated_at)}). Fields you didn’t change now show its values; save to apply your edits to it.
          </Message>
        )}
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>{endpoint ? 'Save' : 'Add endpoint'}</Button>
        </div>
      </form>
    </Dialog>
  );
}

const statusBadge: Record<WebhookDelivery['status'], string> = { delivered: 'badge-success', pending: 'badge-neutral', parked: 'badge-danger' };

function Deliveries({ organizationId, endpoint, canChange, ...signIn }: SignIn & { organizationId: string; endpoint: WebhookEndpoint; canChange: boolean }) {
  const deliveries = useCollection<WebhookDelivery>(params => api.listWebhookDeliveries(organizationId, endpoint.id, params), [organizationId, endpoint.id]);
  // Deliveries a redelivery returned, shown in place of the listed row.
  const [replaced, setReplaced] = useState<Map<string, WebhookDelivery>>(new Map());
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  // One key per delivery while its outcome is unknown: retrying replays it.
  const keys = useRef(new Map<string, string>());
  const reload = () => {
    setReplaced(new Map());
    setProblem(null);
    setNotice(null);
    void deliveries.load(true);
  };
  const redeliver = async (delivery: WebhookDelivery) => {
    const key = keys.current.get(delivery.id) ?? newIdempotencyKey();
    keys.current.set(delivery.id, key);
    setBusy(delivery.id);
    setProblem(null);
    setNotice(null);
    try {
      const { data } = await api.redeliverWebhookDelivery(organizationId, endpoint.id, delivery.id, key);
      // Done: if it parks again, the next redelivery is a new request.
      keys.current.delete(delivery.id);
      setReplaced(previous => new Map(previous).set(delivery.id, data));
      setNotice(endpoint.enabled
        ? 'Delivery queued again; it is attempted now, with a fresh run of retries.'
        : 'Delivery queued again. This endpoint is disabled, so it waits until the endpoint is enabled.');
    } catch (caught) {
      const failure = problemOf(caught, 'The delivery could not be redelivered. Check your connection and try again.');
      // A refusal is final; no answer or a server error may hide a redelivery
      // that happened, so those keep the key and a retry replays it.
      if (failure.status >= 400 && failure.status < 500) keys.current.delete(delivery.id);
      setProblem(failure);
    } finally {
      setBusy(null);
    }
  };
  const rows = deliveries.items.map(item => replaced.get(item.id) ?? item);
  return (
    <section className="cds--tile detail-panel deliveries" aria-labelledby="deliveries-heading">
      <div className="detail-header">
        <div>
          <h3 id="deliveries-heading">Deliveries</h3>
          <p className="meta cell-wrap">To {endpoint.url}, newest first. Kept for 30 days.</p>
        </div>
        <Button variant="ghost" onClick={reload}>Refresh</Button>
      </div>
      {notice && <Message kind="success">{notice}</Message>}
      {problem?.code === 'webhook_delivery_not_parked'
        ? <Message kind="error">That delivery is no longer parked, so it was not redelivered. <button className="inline-action" onClick={reload}>Refresh</button> to see its status.</Message>
        : <ActionError problem={problem} action="redeliver webhooks" {...signIn} />}
      {deliveries.loading && <Message kind="info">Loading deliveries…</Message>}
      {deliveries.error && <ReportError problem={deliveries.error} what="webhook deliveries" onRetry={reload} {...signIn} />}
      {!deliveries.loading && !deliveries.error && rows.length === 0 && (
        <EmptyState title="No deliveries yet" text="Deliveries appear here once an event this endpoint subscribes to occurs." />
      )}
      {!deliveries.loading && !deliveries.error && rows.length > 0 && (
        <DataGrid aria-label="Webhook deliveries">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Event</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Attempts</TableHeader>
              <TableHeader>Last response</TableHeader>
              <TableHeader>Time</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.map(delivery => {
              const time = deliveryTime(delivery);
              return (
                <TableRow className="data-row" key={delivery.id}>
                  <DataCell label="Event" className="cell-primary" title={`Event id ${delivery.event_id}`}><code>{delivery.event_type}</code></DataCell>
                  <DataCell label="Status"><Badge tone={statusBadge[delivery.status]}>{delivery.status}</Badge></DataCell>
                  <DataCell label="Attempts">{delivery.attempts}</DataCell>
                  <DataCell label="Last response">{lastResponse(delivery)}</DataCell>
                  <DataCell label="Time" title={time.at ?? undefined}>{time.label} {formatDate(time.at)}</DataCell>
                  <DataCell label="Actions">
                    {canChange && delivery.status === 'parked' && (
                      <Button variant="ghost" loading={busy === delivery.id} disabled={busy !== null && busy !== delivery.id} onClick={() => void redeliver(delivery)} aria-label={`Redeliver ${delivery.event_type} event ${delivery.event_id}`}>
                        Redeliver
                      </Button>
                    )}
                  </DataCell>
                </TableRow>
              );
            })}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={deliveries.loadMore} hasMore={deliveries.hasMore && !deliveries.error} loading={deliveries.loadingMore} />
    </section>
  );
}
