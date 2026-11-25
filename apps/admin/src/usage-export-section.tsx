import React, { useEffect, useRef, useState } from 'react';
import { allPages, api, newIdempotencyKey, type ApiProblem, type Page, type ProviderCredential, type StorageKind, type UsageExport } from './api';
import { formatCount } from './money';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { Button, Dialog, FormField, Select, TextInput, formatDate, Badge, Message } from './ui';
import {
  connectionsFor,
  destinationLabel,
  exportHealth,
  formValues,
  storageKinds,
  usageExportInput,
  wrongConnection,
  type ExportHealth,
  type UsageExportFormErrors,
  type UsageExportFormValues,
} from './usage-export-form';

type Props = SignIn & { organizationId: string; canChange: boolean };

const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

const healthBadge: Record<ExportHealth, [string, string]> = {
  failing: ['badge-danger', 'Failing'],
  retrying: ['badge-neutral', 'Retrying'],
  writing: ['badge-success', 'Writing'],
  waiting: ['badge-neutral', 'Waiting for the first batch'],
};

/**
 * Continuous export of settled usage to the organization's own bucket,
 * written through one of its cloud connections. The gateway proves the
 * connection can write before saving, so a saved export only fails later if
 * access is taken away.
 */
export function UsageExportSection({ organizationId, canChange, ...signIn }: Props) {
  const [current, setCurrent] = useState<UsageExport | null | undefined>(undefined);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attemptLoad, setAttemptLoad] = useState(0);
  const [editing, setEditing] = useState(false);
  const [stopping, setStopping] = useState(false);
  // Counts successful changes: a read started before one is older than what
  // the change returned, so its answer is dropped.
  const changes = useRef(0);
  useEffect(() => {
    let live = true;
    const startedAfter = changes.current;
    const current = () => live && changes.current === startedAfter;
    setLoadProblem(null);
    api.getUsageExport(organizationId).then(
      value => { if (current()) setCurrent(value); },
      caught => { if (current()) setLoadProblem(problemOf(caught, 'The usage export could not be loaded.')); },
    );
    return () => { live = false; };
  }, [organizationId, attemptLoad]);
  const reload = () => setAttemptLoad(value => value + 1);
  const changed = (next: UsageExport | null) => {
    changes.current += 1;
    setCurrent(next);
  };

  return (
    <div className="cds--tile detail-panel">
      <div className="detail-header">
        <div>
          <h3>Usage export to your storage</h3>
          <p className="meta">
            Writes every settled request’s usage, without content, as gzip NDJSON objects of one hour each to your S3 bucket, Cloud Storage bucket or Azure
            container, through one of this organization’s cloud connections. No storage keys are stored.
          </p>
        </div>
        {current !== undefined && !loadProblem && (
          <span className="row-actions">
            {current && <Button variant="ghost" onClick={reload}>Refresh</Button>}
            {canChange && <Button variant={current ? 'ghost' : 'primary'} onClick={() => setEditing(true)}>{current ? 'Change destination' : 'Set up export'}</Button>}
            {canChange && current && <Button variant="danger" onClick={() => setStopping(true)}>Stop export</Button>}
          </span>
        )}
      </div>
      {loadProblem && <ReportError problem={loadProblem} what="usage export" onRetry={reload} {...signIn} />}
      {!loadProblem && current === undefined && <Message kind="info">Loading usage export…</Message>}
      {!loadProblem && current === null && <p className="muted">Not set up. Usage stays available from the usage report and downloads below.</p>}
      {!loadProblem && current && <ExportStatus current={current} />}
      {editing && current !== undefined && (
        <UsageExportDialog
          {...signIn}
          organizationId={organizationId}
          current={current}
          onClose={() => setEditing(false)}
          onConflict={reload}
          onSaved={saved => { setEditing(false); changed(saved); }}
        />
      )}
      {stopping && current && (
        <StopExport {...signIn} organizationId={organizationId} current={current} onClose={() => setStopping(false)} onStopped={() => { setStopping(false); changed(null); }} />
      )}
    </div>
  );
}

function ExportStatus({ current }: { current: UsageExport }) {
  const health = exportHealth(current);
  const [badge, label] = healthBadge[health];
  return (
    <>
      <dl className="detail-list">
        <dt>Destination</dt><dd><code>{destinationLabel(current)}</code></dd>
        <dt>Connection</dt><dd><code>{current.connection}</code></dd>
        <dt>Status</dt><dd><Badge tone={badge}>{label}</Badge></dd>
        <dt>Exporting since</dt><dd title={current.enabled_at}>{formatDate(current.enabled_at)}</dd>
        <dt>Last written</dt><dd title={current.last_success_at ?? undefined}>{current.last_success_at ? formatDate(current.last_success_at) : 'Nothing yet'}</dd>
        <dt>Waiting to be written</dt>
        <dd>{formatCount(current.queued_records)} records{current.oldest_queued_at && <span className="muted"> · oldest from {formatDate(current.oldest_queued_at)}</span>}</dd>
      </dl>
      {(health === 'failing' || health === 'retrying') && (
        <Message kind={health === 'failing' ? 'error' : 'info'}>
          {health === 'failing' ? 'Writes have failed' : 'Writes have been failing'} since {formatDate(current.failing_since)}
          {current.last_error ? `: ${current.last_error}` : ''}. Records stay queued and are retried until the usage retention period passes. Check that the
          connection may still write to the destination.
        </Message>
      )}
    </>
  );
}

function UsageExportDialog({ organizationId, current, onClose, onConflict, onSaved, ...signIn }: SignIn & {
  organizationId: string;
  current: UsageExport | null;
  onClose: () => void;
  /** Reloads `current`, so the next save is made against what is there now. */
  onConflict: () => void;
  onSaved: (saved: UsageExport) => void;
}) {
  const [values, setValues] = useState<UsageExportFormValues>(() => formValues(current));
  const [errors, setErrors] = useState<UsageExportFormErrors>({});
  const [credentials, setCredentials] = useState<ProviderCredential[] | null>(null);
  const [credentialsFailed, setCredentialsFailed] = useState(false);
  const [attemptCredentials, setAttemptCredentials] = useState(0);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One attempt per form content: a retry after a lost response replays it
  // instead of overwriting whatever changed since.
  const attempt = useRef<{ body: string; key: string } | null>(null);
  useEffect(() => {
    let live = true;
    setCredentialsFailed(false);
    allPages<ProviderCredential, Page<ProviderCredential>>(token => api.listCredentials(organizationId, { page_size: 100, page_token: token })).then(
      ({ items }) => { if (live) setCredentials(items); },
      () => { if (live) setCredentialsFailed(true); },
    );
    return () => { live = false; };
  }, [organizationId, attemptCredentials]);
  const set = (change: Partial<UsageExportFormValues>) => setValues(previous => ({ ...previous, ...change }));
  const connections = credentials ? connectionsFor(values.kind, credentials) : [];
  // A saved connection stays selectable even if its credential is gone, so
  // the form shows what is set; the server says why it no longer works.
  const options = values.connection && !connections.some(item => item.adapter_instance_key === values.connection)
    ? [values.connection, ...connections.map(item => item.adapter_instance_key)]
    : connections.map(item => item.adapter_instance_key);
  const pending = new Set(connections.filter(item => item.cloud_principal === 'pending').map(item => item.adapter_instance_key));

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const built = usageExportInput(values);
    if ('errors' in built) {
      setErrors(built.errors);
      return;
    }
    setErrors({});
    const body = JSON.stringify(built.input);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      const { data } = await api.setUsageExport(organizationId, built.input, current?.version ?? null, attempt.current.key);
      onSaved(data);
    } catch (caught) {
      const refused = problemOf(caught, 'The usage export could not be saved.');
      setProblem(refused);
      if (refused.code === 'version_conflict') {
        // The next save is a new request against the reloaded version.
        attempt.current = null;
        onConflict();
      }
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={current ? 'Change usage export' : 'Set up usage export'}>
      <form onSubmit={submit}>
        <fieldset className="plain-fieldset dialog-body" disabled={submitting}>
          <FormField label="Storage" htmlFor="usage-export-kind">
            <Select id="usage-export-kind" value={values.kind} onChange={event => set({ kind: event.target.value as StorageKind, connection: '' })}>
              {storageKinds.map(([kind, label]) => <option key={kind} value={kind}>{label}</option>)}
            </Select>
          </FormField>
          {credentialsFailed && (
            <Message kind="error">The connections could not be loaded. <button type="button" className="inline-action" onClick={() => setAttemptCredentials(value => value + 1)}>Try again</button></Message>
          )}
          {credentials && options.length === 0 && (
            <Message kind="info">
              No {values.kind === 's3' ? 'AWS' : values.kind === 'gcs' ? 'Google Cloud' : 'Azure'} connection yet. Connect one under Connections → Connect a provider first.
            </Message>
          )}
          <FormField
            label="Connection"
            htmlFor="usage-export-connection"
            hint={values.kind === 's3' ? 'An AWS role connection.' : values.kind === 'gcs' ? 'A Google service account connection.' : 'An Azure Entra connection.'}
            error={errors.connection}
          >
            <Select id="usage-export-connection" value={values.connection} onChange={event => set({ connection: event.target.value })} disabled={!credentials}>
              <option value="">{credentials ? 'Choose…' : 'Loading…'}</option>
              {options.map(key => <option key={key} value={key}>{key}{pending.has(key) ? ' (awaiting operator approval)' : ''}</option>)}
            </Select>
          </FormField>
          {values.kind === 'azure_blob' && (
            <FormField label="Storage account" htmlFor="usage-export-account" error={errors.account}>
              <TextInput id="usage-export-account" value={values.account} onChange={event => set({ account: event.target.value })} />
            </FormField>
          )}
          <FormField label={values.kind === 'azure_blob' ? 'Container' : 'Bucket'} htmlFor="usage-export-bucket" error={errors.bucket}>
            <TextInput id="usage-export-bucket" value={values.bucket} onChange={event => set({ bucket: event.target.value })} />
          </FormField>
          {values.kind === 's3' && (
            <FormField label="Region" htmlFor="usage-export-region" hint="The bucket’s region." error={errors.region}>
              <TextInput id="usage-export-region" placeholder="eu-west-1" value={values.region} onChange={event => set({ region: event.target.value })} />
            </FormField>
          )}
          <FormField label="Prefix" htmlFor="usage-export-prefix" hint="Optional folder for the objects, such as warehouse/gateway." error={errors.prefix}>
            <TextInput id="usage-export-prefix" value={values.prefix} onChange={event => set({ prefix: event.target.value })} />
          </FormField>
          <p className="cds--form__helper-text">
            Saving first writes <code>_gateway-verify.json</code> under the prefix through the connection; nothing is saved unless that succeeds.
            {current ? ' Records not yet written go to the new destination.' : ' Usage settled from now on is exported.'}
          </p>
        </fieldset>
        {problem?.code === 'destination_unverified' && (
          <Message kind="error">
            The gateway could not write to this destination: {problem.detail}.{' '}
            {wrongConnection(problem.detail)
              ? 'Choose a connection made with Connect a provider (an AWS role, an Entra directory or a Google service account), not API or access keys.'
              : 'Grant the connection write access to the prefix, then save again.'}
          </Message>
        )}
        {problem?.code === 'version_conflict' && (
          <Message kind="error">
            Someone changed the usage export since you opened it{current ? `; it now writes to ${destinationLabel(current)}` : ''}. Save again to replace it with the values above.
          </Message>
        )}
        {problem?.code !== 'destination_unverified' && problem?.code !== 'version_conflict' && <ActionError problem={problem} action="change the usage export" {...signIn} />}
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>{submitting ? 'Verifying…' : 'Verify and save'}</Button>
        </div>
      </form>
    </Dialog>
  );
}

function StopExport({ organizationId, current, onClose, onStopped, ...signIn }: SignIn & { organizationId: string; current: UsageExport; onClose: () => void; onStopped: () => void }) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const key = useRef(newIdempotencyKey());
  const stop = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      await api.deleteUsageExport(organizationId, current, key.current);
      onStopped();
    } catch (caught) {
      setProblem(problemOf(caught, 'The usage export could not be stopped.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open alert onClose={() => { if (!submitting) onClose(); }} title="Stop usage export">
      <div className="dialog-body">
        <p>
          {current.queued_records > 0 ? `${formatCount(current.queued_records)} records not yet written will be discarded. ` : ''}
          Objects already written stay in your storage. Setting the export up again starts from usage settled after that.
        </p>
      </div>
      <ActionError problem={problem} action="change the usage export" {...signIn} />
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button variant="danger" onClick={() => void stop()} loading={submitting}>Stop export</Button>
      </div>
    </Dialog>
  );
}
