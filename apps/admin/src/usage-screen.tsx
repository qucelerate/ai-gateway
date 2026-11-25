import React, { useRef, useState } from 'react';
import { api, newIdempotencyKey, type ApiProblem, type ReconciliationItem, type ReconciliationStatus } from './api';
import { useCollection } from './use-collection';
import { ReportError, SessionExpired, loadFailed } from './screen-feedback';
import { formatMoney, parseMoney } from './money';
import { Button, Dialog, EmptyState, FormField, InlineError, LoadMore, TextInput, formatDate, useRouteTab, Select, Message, DataCell } from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow } from './ui';
import { UsageDashboard } from './usage-dashboard';
import { reconciliationChanged } from './open-reconciliation';
import { BatchesSection } from './batches-section';
import { offersChanges } from './write-access';

// The Usage page: the dashboard (a slice of requests charted and broken
// down by up to two dimensions, with the requests behind it),
// reconciliation, and batches (D-021, batches-section.tsx). Traces are their
// own page (traces-screen.tsx).
type Tab = 'dashboard' | 'reconciliation' | 'batches';
export const usageTabs: [Tab, string][] = [['dashboard', 'Dashboard'], ['reconciliation', 'Reconciliation'], ['batches', 'Batches']];
const tabKeys = usageTabs.map(([key]) => key);

export const windows = [
  ['1', 'Last 24 hours'],
  ['7', 'Last 7 days'],
  ['30', 'Last 30 days'],
  ['90', 'Last 90 days'],
] as const;
type WindowDays = (typeof windows)[number][0];

/** The report window ending now. Fixed per filter choice: the next page of a
 * report must ask for exactly the window its first page did. */
export function reportWindow(days: WindowDays, now = new Date()): { from: string; to: string } {
  const to = new Date(now);
  const from = new Date(now.getTime() - Number(days) * 86_400_000);
  return { from: from.toISOString(), to: to.toISOString() };
}

export function UsageScreen({ role, ...props }: { organizationId: string; role?: string | null; onSignIn?: () => void; signInError?: string; signInPending?: boolean }) {
  const [tab] = useRouteTab(tabKeys, 'dashboard');
  if (!props.organizationId) {
    return <EmptyState title="Waiting for organization context" text="Select a confirmed organization to see usage, reconciliation and batches." />;
  }
  return (
    <div className="screen">
      {/* The section is chosen in the side navigation. */}
      <div className="page-section">
      {tab === 'dashboard' && <UsageDashboard {...props} />}
      {tab === 'reconciliation' && <Reconciliation {...props} />}
      {tab === 'batches' && <BatchesSection {...props} canChange={offersChanges(role)} />}
      </div>
    </div>
  );
}

type ScreenProps = { organizationId: string; onSignIn?: () => void; signInError?: string; signInPending?: boolean };
type DetailProps = { organizationId: string } & Pick<ScreenProps, 'onSignIn' | 'signInError' | 'signInPending'>;

function Reconciliation({ organizationId, ...signIn }: ScreenProps) {
  const [status, setStatus] = useState<ReconciliationStatus>('open');
  const [resolving, setResolving] = useState<ReconciliationItem | null>(null);
  const collection = useCollection<ReconciliationItem>(
    params => api.listReconciliationItems(organizationId, status, params),
    [organizationId, status],
  );
  return (
    <>
      <div className="report-controls" aria-label="Reconciliation filters">
        <div className="audit-filter"><FormField label="Status" htmlFor="reconciliation-status">
          <Select id="reconciliation-status" value={status} onChange={event => setStatus(event.target.value as ReconciliationStatus)}>
            <option value="open">Open</option>
            <option value="resolved">Resolved</option>
            <option value="all">All</option>
          </Select>
        </FormField></div>
      </div>
      <div className="section-header"><div><h2>Reconciliation items</h2><p className="muted">Requests whose final cost could not be settled automatically. Resolving one charges an amount you choose and releases the held quota.</p></div></div>
      {collection.loading && <Message kind="info">Loading reconciliation items…</Message>}
      {collection.error && <ReportError problem={collection.error} what="reconciliation items" onRetry={() => void collection.load(true)} {...signIn} />}
      {!collection.loading && !collection.error && collection.items.length === 0 && <EmptyState title={status === 'open' ? 'Nothing to reconcile' : 'No reconciliation items'} text="Every request in this view settled automatically." />}
      {!collection.loading && !collection.error && collection.items.length > 0 && (
        <DataGrid aria-label="Reconciliation items">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Opened</TableHeader>
              <TableHeader>Reason</TableHeader>
              <TableHeader>Model</TableHeader>
              <TableHeader>Reserved</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader className="row-actions">Action</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {collection.items.map(item => (
              <TableRow className="data-row" key={item.id}>
                <DataCell label="Opened" title={item.created_at}>{formatDate(item.created_at)}</DataCell>
                <DataCell label="Reason" className="cell-primary">{item.reason}</DataCell>
                <DataCell label="Model" title={item.deployment}>{item.model}</DataCell>
                <DataCell label="Reserved">{formatMoney(item.reserved_minor, item.currency)}</DataCell>
                <DataCell label="Status">{item.status === 'open' ? 'Open' : `Resolved by ${item.resolution ?? '—'}${item.resolved_amount_minor !== null ? ` · ${formatMoney(item.resolved_amount_minor, item.currency)}` : ''}`}</DataCell>
                <DataCell label="Action">{item.status === 'open' ? <button className="inline-action" onClick={() => setResolving(item)}>Resolve</button> : '—'}</DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={collection.loadMore} hasMore={collection.hasMore && !collection.error} loading={collection.loadingMore} />
      {resolving && (
        <ResolveDialog
          {...signIn}
          organizationId={organizationId}
          item={resolving}
          onClose={() => setResolving(null)}
          onResolved={() => { setResolving(null); void collection.load(true); }}
        />
      )}
    </>
  );
}

function ResolveDialog({ organizationId, item, onClose, onResolved, ...signIn }: DetailProps & { item: ReconciliationItem; onClose: () => void; onResolved: () => void }) {
  const [amount, setAmount] = useState('');
  // One key per amount: a retry after a lost response replays the saved
  // resolution; a different amount is a new request.
  const attempt = useRef<{ units: number; key: string } | null>(null);
  const [error, setError] = useState('');
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const units = parseMoney(amount, item.currency);
    if (units === null) {
      setError(`Enter an amount in ${item.currency}, such as 0 or 1.25.`);
      return;
    }
    setError('');
    setProblem(null);
    setSubmitting(true);
    if (attempt.current?.units !== units) attempt.current = { units, key: newIdempotencyKey() };
    try {
      await api.resolveReconciliationItem(organizationId, item.id, units, attempt.current.key);
      // The count beside Reconciliation in the side navigation reads again.
      dispatchEvent(new Event(reconciliationChanged));
      onResolved();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The item could not be resolved.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title="Resolve reconciliation item">
      <form onSubmit={submit}>
        <div className="dialog-body">
          <p>{item.model} · {item.reason}. {formatMoney(item.reserved_minor, item.currency)} was reserved. Charge 0 to release it without a charge.</p>
          <FormField label={`Amount to charge (${item.currency})`} htmlFor="resolve-amount" error={error}>
            <TextInput id="resolve-amount" inputMode="decimal" value={amount} onChange={event => setAmount(event.target.value)} autoComplete="off" />
          </FormField>
        </div>
        {problem?.status === 401 ? <SessionExpired action="resolve this item" {...signIn} /> : <InlineError problem={problem} />}
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Resolve</Button>
        </div>
      </form>
    </Dialog>
  );
}
