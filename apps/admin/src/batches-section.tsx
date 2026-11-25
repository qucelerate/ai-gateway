import React, { useEffect, useRef, useState } from 'react';
import {
  allPages,
  api,
  batchResultsUrl,
  newIdempotencyKey,
  type AdminBatch,
  type AdminBatchDetail,
  type ApiProblem,
  type BatchSettings,
  type BatchStaging,
  type BatchStagingInput,
  type BatchStatus,
  type Page,
  type ProviderAccount,
  type ProviderCredential,
} from './api';
import { formatCount, formatMoney } from './money';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { useCollection } from './use-collection';
import { readDashboard, writeDashboard } from './usage-dashboard';
import {
  AppLink,
  Badge,
  Button,
  CheckOption,
  ConfirmDialog,
  DataCell,
  DataGrid,
  Dialog,
  EmptyState,
  FormField,
  LoadMore,
  Message,
  Select,
  TableBody,
  TableHead,
  TableHeader,
  TableRow,
  TextInput,
  buttonClass,
  formatDate,
  uuidHint,
} from './ui';

// Usage → Batches (D-021, Phase 7 milestone 6): the organization's batches,
// each with how its lines run and what it has cost, cancel and results
// downloads; and the settings behind them: whether lines no native batch job
// takes run emulated, and where Bedrock and Gemini connections stage their
// batch jobs.

type Props = SignIn & { organizationId: string; canChange: boolean };

const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

export const batchStatuses: [BatchStatus | '', string][] = [
  ['', 'Any status'],
  ['validating', 'Validating'],
  ['in_progress', 'In progress'],
  ['finalizing', 'Finalizing'],
  ['completed', 'Completed'],
  ['failed', 'Failed'],
  ['expired', 'Expired'],
  ['cancelling', 'Cancelling'],
  ['cancelled', 'Cancelled'],
];

export function statusLabel(status: string): string {
  return batchStatuses.find(([key]) => key === status)?.[1] ?? status;
}

export function statusTone(status: string): 'success' | 'danger' | 'warning' | 'neutral' {
  if (status === 'completed') return 'success';
  if (status === 'failed' || status === 'expired') return 'danger';
  if (status === 'cancelling' || status === 'cancelled') return 'warning';
  return 'neutral';
}

/** Whether a batch can still be asked to stop. */
export const cancellable = (status: string) => status === 'validating' || status === 'in_progress';

export function countsText(counts: { total: number; completed: number; failed: number }): string {
  return `${formatCount(counts.completed)} done · ${formatCount(counts.failed)} failed · ${formatCount(counts.total)} lines`;
}

export function modeText(mode: string, priceSource: string | null): string {
  const how = mode === 'emulated' ? 'Emulated (line by line)' : mode === 'native' ? 'Provider batch' : mode;
  const price = priceSource === 'on_demand' ? 'on-demand price' : priceSource === 'batch' ? 'batch price' : 'price unknown';
  return `${how} · ${price}`;
}

const unix = (seconds: number | null | undefined) => (seconds ? new Date(seconds * 1000).toISOString() : null);

/** The usage dashboard filtered to a batch's lines, over the UTC days from
 * its creation to its end (or the end of its window). */
export function batchUsageHref(batch: AdminBatch): string {
  const day = (seconds: number) => new Date(seconds * 1000).toISOString().slice(0, 10);
  const end = Math.max(batch.created_at, batch.completed_at ?? batch.cancelled_at ?? batch.expires_at);
  const state = { ...readDashboard(''), range: 'custom' as const, from: day(batch.created_at), to: day(end), filters: { batch: [batch.id] } };
  return `/admin/usage${writeDashboard('', state)}`;
}

/** The staging form's values, checked as the server checks them. */
export type StagingValues = { connection: string; bucket: string; prefix: string; region: string; roleArn: string };
export type StagingErrors = Partial<Record<keyof StagingValues, string>>;

export function stagingInput(values: StagingValues, s3: boolean): { input: BatchStagingInput } | { errors: StagingErrors } {
  const errors: StagingErrors = {};
  const bucket = values.bucket.trim();
  const prefix = values.prefix.trim();
  if (!values.connection.trim()) errors.connection = 'Choose the connection whose batch jobs stage here.';
  if (!/^[a-z0-9][a-z0-9._-]{1,220}[a-z0-9]$/.test(bucket)) errors.bucket = 'Enter the bucket name: lowercase letters, digits, dots and dashes.';
  if (prefix && (!prefix.endsWith('/') || prefix.startsWith('/') || prefix.includes('//'))) errors.prefix = 'Leave empty, or end the prefix with /, as in batches/.';
  if (s3) {
    if (!/^[a-z]{2}(-[a-z0-9]+)+-[0-9]+$/.test(values.region.trim())) errors.region = 'Enter the bucket’s AWS Region, as in us-east-1.';
    if (!/^arn:aws(-[a-z-]+)?:iam::[0-9]{12}:role\/.+$/.test(values.roleArn.trim())) errors.roleArn = 'Enter the ARN of the role Bedrock assumes for batch jobs.';
  }
  if (Object.keys(errors).length) return { errors };
  return {
    input: {
      bucket,
      prefix: prefix || undefined,
      region: s3 ? values.region.trim() : null,
      role_arn: s3 ? values.roleArn.trim() : null,
    },
  };
}

export function BatchesSection(props: Props) {
  return (
    <>
      <BatchList {...props} />
      <BatchSettingsPanel {...props} />
      <StagingPanel {...props} />
    </>
  );
}

function BatchList({ organizationId, canChange, ...signIn }: Props) {
  const [status, setStatus] = useState<BatchStatus | ''>('');
  const [open, setOpen] = useState<AdminBatch | null>(null);
  const collection = useCollection<AdminBatch>(params => api.listBatches(organizationId, status, params), [organizationId, status]);
  return (
    <>
      <div className="report-controls" aria-label="Batch filters">
        <div className="audit-filter"><FormField label="Status" htmlFor="batch-status">
          <Select id="batch-status" value={status} onChange={event => setStatus(event.target.value as BatchStatus | '')}>
            {batchStatuses.map(([key, label]) => <option key={key} value={key}>{label}</option>)}
          </Select>
        </FormField></div>
      </div>
      <div className="section-header"><div><h2>Batches</h2><p className="muted">Files of requests run within 24 hours. Lines a provider’s batch API runs are charged its batch price; lines run emulated, one by one, the ordinary price.</p></div></div>
      {collection.loading && <Message kind="info">Loading batches…</Message>}
      {collection.error && <ReportError problem={collection.error} what="batches" onRetry={() => void collection.load(true)} {...signIn} />}
      {!collection.loading && !collection.error && collection.items.length === 0 && <EmptyState title="No batches" text="Batches appear here once a service account creates one through the inference API." />}
      {!collection.loading && !collection.error && collection.items.length > 0 && (
        <DataGrid aria-label="Batches">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Created</TableHeader>
              <TableHeader>Batch</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Lines</TableHeader>
              <TableHeader>Service account</TableHeader>
              <TableHeader className="row-actions">Action</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {collection.items.map(batch => (
              <TableRow className="data-row" key={batch.id}>
                <DataCell label="Created" title={unix(batch.created_at) ?? undefined}>{formatDate(unix(batch.created_at))}</DataCell>
                <DataCell label="Batch" className="cell-primary"><code>{batch.id}</code></DataCell>
                <DataCell label="Status"><Badge kind={statusTone(batch.status)}>{statusLabel(batch.status)}</Badge></DataCell>
                <DataCell label="Lines">{countsText(batch.request_counts)}</DataCell>
                <DataCell label="Service account" title={batch.service_account_id}>{uuidHint(batch.service_account_id)}</DataCell>
                <DataCell label="Action"><button className="inline-action" onClick={() => setOpen(batch)}>Details</button></DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={collection.loadMore} hasMore={collection.hasMore && !collection.error} loading={collection.loadingMore} />
      {open && (
        <BatchDetailDialog
          {...signIn}
          organizationId={organizationId}
          canChange={canChange}
          batch={open}
          onClose={() => setOpen(null)}
          onChanged={() => void collection.load(true)}
        />
      )}
    </>
  );
}

function BatchDetailDialog({ organizationId, canChange, batch, onClose, onChanged, ...signIn }: Props & { batch: AdminBatch; onClose: () => void; onChanged: () => void }) {
  const [detail, setDetail] = useState<AdminBatchDetail | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attemptLoad, setAttemptLoad] = useState(0);
  const [confirming, setConfirming] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One key per dialog: a retried cancel replays the first.
  const key = useRef(newIdempotencyKey());
  useEffect(() => {
    let live = true;
    setLoadProblem(null);
    api.getBatch(organizationId, batch.id).then(
      value => { if (live) setDetail(value); },
      caught => { if (live) setLoadProblem(problemOf(caught, 'The batch could not be loaded.')); },
    );
    return () => { live = false; };
  }, [organizationId, batch.id, attemptLoad]);
  const cancel = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      setDetail(await api.cancelBatch(organizationId, batch.id, key.current));
      setConfirming(false);
      onChanged();
    } catch (caught) {
      setProblem(problemOf(caught, 'The batch could not be cancelled.'));
    } finally {
      setSubmitting(false);
    }
  };
  const shown = detail ?? batch;
  return (
    <Dialog open onClose={onClose} title={`Batch ${batch.id}`}>
      <div className="dialog-body">
        {loadProblem && <ReportError problem={loadProblem} what="batch" onRetry={() => setAttemptLoad(value => value + 1)} {...signIn} />}
        <dl className="detail-list">
          <dt>Status</dt><dd><Badge kind={statusTone(shown.status)}>{statusLabel(shown.status)}</Badge></dd>
          <dt>Endpoint</dt><dd><code>{shown.endpoint}</code></dd>
          <dt>Lines</dt><dd>{countsText(shown.request_counts)}</dd>
          <dt>Created</dt><dd>{formatDate(unix(shown.created_at))}</dd>
          <dt>Window ends</dt><dd>{formatDate(unix(shown.expires_at))}</dd>
          <dt>Service account</dt><dd><code>{shown.service_account_id}</code></dd>
          <dt>Usage</dt><dd><AppLink to={batchUsageHref(shown)}>Usage of this batch’s lines</AppLink></dd>
          {detail && (
            <>
              <dt>Cost so far</dt>
              <dd>
                {detail.gateway.cost.length === 0 ? 'Nothing charged' : detail.gateway.cost.map(cost => (
                  <div key={cost.currency}>
                    {formatMoney(cost.consumed_minor, cost.currency)} charged
                    {cost.reserved_minor > 0 && <span className="muted"> · {formatMoney(cost.reserved_minor, cost.currency)} still held</span>}
                  </div>
                ))}
              </dd>
            </>
          )}
        </dl>
        {!detail && !loadProblem && <Message kind="info">Loading batch…</Message>}
        {detail && detail.gateway.jobs.length > 0 && (
          <DataGrid aria-label="Provider jobs" size="sm">
            <TableHead>
              <TableRow className="data-row head">
                <TableHeader>Deployment</TableHeader>
                <TableHeader>Runs as</TableHeader>
                <TableHeader>State</TableHeader>
                <TableHeader>Lines</TableHeader>
              </TableRow>
            </TableHead>
            <TableBody>
              {detail.gateway.jobs.map((job, index) => (
                <TableRow className="data-row" key={index}>
                  <DataCell label="Deployment" title={job.model}>{job.deployment} <span className="muted">({job.provider_family})</span></DataCell>
                  <DataCell label="Runs as">{modeText(job.mode, job.price_source)}</DataCell>
                  <DataCell label="State" title={job.provider_batch_id ?? undefined}>{job.provider_status ?? job.state}{job.error_code && <span className="muted"> · {job.error_code}</span>}</DataCell>
                  <DataCell label="Lines">
                    {countsText(job.request_counts)}
                    {job.request_counts.unknown > 0 && <span className="muted"> · {formatCount(job.request_counts.unknown)} in reconciliation</span>}
                  </DataCell>
                </TableRow>
              ))}
            </TableBody>
          </DataGrid>
        )}
        <p className="meta">Results hold the model’s answers: downloading them needs permission to read trace content.</p>
      </div>
      {problem && !confirming && <ActionError problem={problem} action="cancel this batch" {...signIn} />}
      <div className="dialog-actions">
        {shown.output_file_id && <a className={buttonClass('ghost')} href={batchResultsUrl(organizationId, batch.id, 'output')} download>Download output</a>}
        {shown.error_file_id && <a className={buttonClass('ghost')} href={batchResultsUrl(organizationId, batch.id, 'errors')} download>Download errors</a>}
        {canChange && cancellable(shown.status) && <Button variant="danger" onClick={() => setConfirming(true)}>Cancel batch</Button>}
        <Button variant="ghost" onClick={onClose}>Close</Button>
      </div>
      <ConfirmDialog
        open={confirming}
        onClose={() => setConfirming(false)}
        onConfirm={() => void cancel()}
        title="Cancel batch"
        confirmText="Cancel batch"
        problem={problem}
        submitting={submitting}
      >
        Lines not yet run end without a charge. Lines already with the provider finish or stop as the provider decides, and are charged as it reports them.
      </ConfirmDialog>
    </Dialog>
  );
}

function BatchSettingsPanel({ organizationId, canChange, ...signIn }: Props) {
  const [settings, setSettings] = useState<BatchSettings | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attemptLoad, setAttemptLoad] = useState(0);
  const [allowed, setAllowed] = useState(false);
  const [concurrency, setConcurrency] = useState('4');
  const [error, setError] = useState('');
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [saved, setSaved] = useState(false);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  useEffect(() => {
    let live = true;
    setLoadProblem(null);
    api.batchSettings(organizationId).then(
      value => {
        if (!live) return;
        setSettings(value);
        setAllowed(value.emulated_allowed);
        setConcurrency(String(value.emulated_concurrency));
      },
      caught => { if (live) setLoadProblem(problemOf(caught, 'The batch settings could not be loaded.')); },
    );
    return () => { live = false; };
  }, [organizationId, attemptLoad]);
  if (loadProblem) return <ReportError problem={loadProblem} what="batch settings" onRetry={() => setAttemptLoad(value => value + 1)} {...signIn} />;
  if (!settings) return <Message kind="info">Loading batch settings…</Message>;
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setSaved(false);
    const count = Number(concurrency);
    if (!Number.isInteger(count) || count < 1 || count > 32) {
      setError('Enter 1 to 32.');
      return;
    }
    setError('');
    const input = { emulated_allowed: allowed, emulated_concurrency: count };
    const body = JSON.stringify(input);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      const { data } = await api.setBatchSettings(organizationId, input, settings.version, attempt.current.key);
      attempt.current = null;
      setSettings(data);
      setSaved(true);
    } catch (caught) {
      const refused = problemOf(caught, 'The batch settings could not be saved.');
      setProblem(refused);
      if (refused.code === 'version_conflict') {
        attempt.current = null;
        setAttemptLoad(value => value + 1);
      }
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <div className="cds--tile detail-panel">
      <div className="detail-header">
        <div>
          <h3>Emulated batches</h3>
          <p className="meta">
            Lines no provider batch job can take (a provider without a batch API, a model without a batch route, a Bedrock group under 100 lines, a
            connection without staging) can run one by one as ordinary requests at the on-demand price, within the organization’s rate limits. Off, such
            lines refuse the batch when it is created.
          </p>
        </div>
      </div>
      <form onSubmit={submit} className="policy-form">
        <fieldset className="plain-fieldset" disabled={submitting || !canChange}>
          <CheckOption checked={allowed} onChange={event => { setSaved(false); setAllowed(event.target.checked); }}>Run such lines emulated, at the on-demand price</CheckOption>
          <FormField label="Lines in flight per job" htmlFor="batch-concurrency" hint="1 to 32, always within the rate limits." error={error}>
            <TextInput id="batch-concurrency" inputMode="numeric" value={concurrency} onChange={event => { setSaved(false); setConcurrency(event.target.value); }} />
          </FormField>
        </fieldset>
        <ActionError problem={problem} action="change the batch settings" {...signIn} />
        {saved && <Message kind="success">Batch settings saved.</Message>}
        {canChange
          ? <div className="dialog-actions"><Button type="submit" loading={submitting}>Save settings</Button></div>
          : <p className="meta">Your role can view the batch settings but not change them.</p>}
      </form>
    </div>
  );
}

const stagedFamilies = new Set(['bedrock', 'gemini']);

type Connection = { key: string; family: 'bedrock' | 'gemini' };

/** The Bedrock and Gemini connections among stored credentials and the
 * catalog's provider accounts, once each; a stored credential names the
 * family the server checks first. */
export function stagedConnections(
  credentials: { adapter_instance_key: string; family: string }[],
  accounts: { adapter_instance_key: string; family: string }[],
): Connection[] {
  const found = new Map<string, string>();
  for (const item of [...credentials, ...accounts]) {
    if (!found.has(item.adapter_instance_key)) found.set(item.adapter_instance_key, item.family);
  }
  return [...found]
    .filter(([, family]) => stagedFamilies.has(family))
    .map(([key, family]) => ({ key, family: family as Connection['family'] }))
    .sort((a, b) => a.key.localeCompare(b.key));
}

function StagingPanel({ organizationId, canChange, ...signIn }: Props) {
  const [stagings, setStagings] = useState<BatchStaging[] | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attemptLoad, setAttemptLoad] = useState(0);
  const [editing, setEditing] = useState<BatchStaging | 'new' | null>(null);
  const [deleting, setDeleting] = useState<BatchStaging | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const deleteKey = useRef<{ connection: string; key: string } | null>(null);
  useEffect(() => {
    let live = true;
    setLoadProblem(null);
    api.listBatchStaging(organizationId).then(
      value => { if (live) setStagings(value); },
      caught => { if (live) setLoadProblem(problemOf(caught, 'The batch staging could not be loaded.')); },
    );
    return () => { live = false; };
  }, [organizationId, attemptLoad]);
  const reload = () => setAttemptLoad(value => value + 1);
  const remove = async (staging: BatchStaging) => {
    if (deleteKey.current?.connection !== staging.connection) deleteKey.current = { connection: staging.connection, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      await api.deleteBatchStaging(organizationId, staging, deleteKey.current.key);
      deleteKey.current = null;
      setDeleting(null);
      reload();
    } catch (caught) {
      setProblem(problemOf(caught, 'The staging could not be deleted.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <div className="cds--tile detail-panel">
      <div className="detail-header">
        <div>
          <h3>Batch staging</h3>
          <p className="meta">
            Bedrock and Gemini (on Agent Platform) run batch jobs from files in your own bucket: an S3 bucket and the service role Bedrock assumes, or a
            Cloud Storage bucket. The gateway writes and reads it with the connection’s own credentials; no storage keys are stored.
          </p>
        </div>
        {canChange && stagings && <span className="row-actions"><Button onClick={() => setEditing('new')}>Add staging</Button></span>}
      </div>
      {loadProblem && <ReportError problem={loadProblem} what="batch staging" onRetry={reload} {...signIn} />}
      {!loadProblem && !stagings && <Message kind="info">Loading batch staging…</Message>}
      {stagings && stagings.length === 0 && <p className="muted">None set. Bedrock and Gemini batch lines run emulated where allowed, else are refused.</p>}
      {stagings && stagings.length > 0 && (
        <DataGrid aria-label="Batch staging" size="sm">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Connection</TableHeader>
              <TableHeader>Bucket</TableHeader>
              <TableHeader>Service role</TableHeader>
              <TableHeader className="row-actions">Action</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {stagings.map(staging => (
              <TableRow className="data-row" key={staging.connection}>
                <DataCell label="Connection" className="cell-primary"><code>{staging.connection}</code></DataCell>
                <DataCell label="Bucket"><code>{staging.kind === 's3' ? 's3' : 'gs'}://{staging.bucket}/{staging.prefix}</code>{staging.region && <span className="muted"> · {staging.region}</span>}</DataCell>
                <DataCell label="Service role">{staging.role_arn ? <code>{staging.role_arn}</code> : '—'}</DataCell>
                <DataCell label="Action">
                  {canChange ? (
                    <span className="row-actions">
                      <button className="inline-action" onClick={() => setEditing(staging)}>Change</button>
                      <button className="inline-action" onClick={() => { setProblem(null); setDeleting(staging); }}>Delete</button>
                    </span>
                  ) : '—'}
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      {editing && (
        <StagingDialog
          {...signIn}
          organizationId={organizationId}
          current={editing === 'new' ? null : editing}
          onClose={() => setEditing(null)}
          onSaved={() => { setEditing(null); reload(); }}
        />
      )}
      <ConfirmDialog
        open={deleting !== null}
        onClose={() => setDeleting(null)}
        onConfirm={() => { if (deleting) void remove(deleting); }}
        title="Delete batch staging"
        confirmText="Delete"
        problem={problem}
        submitting={submitting}
      >
        New batch lines for {deleting?.connection} no longer run as provider batch jobs: they run emulated where allowed, else are refused. Jobs already sent
        still collect their results from the bucket. The bucket and its files are left as they are.
      </ConfirmDialog>
    </div>
  );
}

function StagingDialog({ organizationId, current, onClose, onSaved, ...signIn }: SignIn & {
  organizationId: string;
  current: BatchStaging | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [values, setValues] = useState<StagingValues>({
    connection: current?.connection ?? '',
    bucket: current?.bucket ?? '',
    prefix: current?.prefix ?? '',
    region: current?.region ?? '',
    roleArn: current?.role_arn ?? '',
  });
  const [errors, setErrors] = useState<StagingErrors>({});
  // Bedrock and Gemini connections and their family: stored credentials,
  // and the active catalog's provider accounts, which include connections
  // the process configures.
  const [connections, setConnections] = useState<Connection[]>([]);
  // The storage, where the connection's family is not known.
  const [picked, setPicked] = useState<'s3' | 'gcs'>('s3');
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  useEffect(() => {
    let live = true;
    // To suggest connections and tell their storage; either list may be
    // unreadable to the role, and the server checks the family either way.
    const quiet = <T,>(promise: Promise<{ items: T[] }>) => promise.then(({ items }) => items, () => [] as T[]);
    void Promise.all([
      quiet(allPages<ProviderCredential, Page<ProviderCredential>>(token => api.listCredentials(organizationId, { page_size: 100, page_token: token }))),
      quiet(allPages<ProviderAccount, Page<ProviderAccount>>(token => api.listProviderAccounts(organizationId, { page_size: 100, page_token: token }))),
    ]).then(([credentials, accounts]) => {
      if (live) setConnections(stagedConnections(credentials, accounts));
    });
    return () => { live = false; };
  }, [organizationId]);
  const family = connections.find(item => item.key === values.connection.trim())?.family;
  const s3 = current ? current.kind === 's3' : family ? family === 'bedrock' : picked === 's3';
  const set = (change: Partial<StagingValues>) => setValues(previous => ({ ...previous, ...change }));
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const built = stagingInput(values, s3);
    if ('errors' in built) {
      setErrors(built.errors);
      return;
    }
    setErrors({});
    const body = JSON.stringify([values.connection, built.input]);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      await api.setBatchStaging(organizationId, values.connection.trim(), built.input, current?.version ?? null, attempt.current.key);
      onSaved();
    } catch (caught) {
      setProblem(problemOf(caught, 'The staging could not be saved.'));
      if ((caught as { problem?: ApiProblem }).problem?.code === 'version_conflict') attempt.current = null;
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={current ? `Change staging for ${current.connection}` : 'Add batch staging'}>
      <form onSubmit={submit}>
        <fieldset className="plain-fieldset dialog-body" disabled={submitting}>
          <FormField label="Connection" htmlFor="staging-connection" hint="A Bedrock or Gemini (Agent Platform) connection." error={errors.connection}>
            <TextInput id="staging-connection" list="staging-connections" value={values.connection} disabled={current !== null} onChange={event => set({ connection: event.target.value })} autoComplete="off" />
          </FormField>
          <datalist id="staging-connections">
            {connections.map(item => <option key={item.key} value={item.key}>{item.family}</option>)}
          </datalist>
          {!current && !family && (
            <FormField label="Storage" htmlFor="staging-kind" hint="The connection’s family is not known here: pick its storage.">
              <Select id="staging-kind" value={picked} onChange={event => setPicked(event.target.value as 's3' | 'gcs')}>
                <option value="s3">Amazon S3 (Bedrock)</option>
                <option value="gcs">Cloud Storage (Gemini on Agent Platform)</option>
              </Select>
            </FormField>
          )}
          <FormField label={s3 ? 'S3 bucket' : 'Cloud Storage bucket'} htmlFor="staging-bucket" error={errors.bucket}>
            <TextInput id="staging-bucket" value={values.bucket} onChange={event => set({ bucket: event.target.value })} autoComplete="off" />
          </FormField>
          <FormField label="Prefix (optional)" htmlFor="staging-prefix" hint="Object names start with it, as in batches/." error={errors.prefix}>
            <TextInput id="staging-prefix" value={values.prefix} onChange={event => set({ prefix: event.target.value })} autoComplete="off" />
          </FormField>
          {s3 && (
            <>
              <FormField label="Bucket Region" htmlFor="staging-region" error={errors.region}>
                <TextInput id="staging-region" value={values.region} onChange={event => set({ region: event.target.value })} autoComplete="off" />
              </FormField>
              <FormField label="Bedrock service role ARN" htmlFor="staging-role" hint="The role Bedrock assumes to read the input and write the output. The connection’s role needs iam:PassRole on it." error={errors.roleArn}>
                <TextInput id="staging-role" value={values.roleArn} onChange={event => set({ roleArn: event.target.value })} autoComplete="off" />
              </FormField>
            </>
          )}
        </fieldset>
        <ActionError problem={problem} action="save the staging" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Save staging</Button>
        </div>
      </form>
    </Dialog>
  );
}
