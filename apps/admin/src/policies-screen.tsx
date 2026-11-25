import React, { useEffect, useMemo, useRef, useState } from 'react';
import {
  allPages,
  api,
  exportUrl,
  newIdempotencyKey,
  type ApiProblem,
  type ApiKey,
  type BudgetPeriod,
  type BudgetPolicy,
  type BudgetScope,
  budgetTarget,
  type DataExport,
  type ExportDataset,
  type ExportFormat,
  type RateLimitPolicy,
  type RateLimitScope,
  type RetentionPeriods,
  type RetentionPolicy,
  type TraceMode,
  type TracePolicy,
  type TracePolicyInput,
} from './api';
import { formatCount, formatMoney, minorDigits, toMajor } from './money';
import { DataTable, type Column } from './data-table';
import { budgetInput, rateLimitInput, retentionInput, traceInput } from './policy-forms';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { everyPage, useCollection } from './use-collection';
import { UsageExportSection } from './usage-export-section';
import { Button, DataCell, Dialog, EmptyState, FormField, LoadMore, Select, TextInput, formatDate, useRouteTab, uuidHint, buttonClass, CheckOption, Badge, Message } from './ui';
import { Webhooks } from './webhooks-section';
import { offersChanges } from './write-access';

type Tab = 'budgets' | 'limits' | 'retention' | 'tracing' | 'exports' | 'webhooks';
export const policyTabs: [Tab, string][] = [['budgets', 'Budgets'], ['limits', 'Rate limits'], ['retention', 'Retention'], ['tracing', 'Tracing'], ['exports', 'Exports'], ['webhooks', 'Webhooks']];
const tabKeys = policyTabs.map(([key]) => key);
type Props = SignIn & { organizationId: string };
/** `role` is the confirmed context's; it only decides which controls show. */
type ScreenProps = Props & { role?: string | null };
type NameList = 'teams' | 'serviceAccounts' | 'apiKeys';
/** `apiKeys` names each key by its prefix and service account; `activeKeys`
 * are those a new budget can be set on. */
type Names = { teams: Map<string, string>; serviceAccounts: Map<string, string>; apiKeys?: Map<string, string>; activeKeys?: Set<string>; failed?: NameList[]; retry?: () => void };

export function PoliciesScreen({ role, ...props }: ScreenProps) {
  const [tab] = useRouteTab(tabKeys, 'budgets');
  if (!props.organizationId) {
    return <EmptyState title="Waiting for organization context" text="Select a confirmed organization to manage its budgets, rate limits, retention, tracing, exports, and webhooks." />;
  }
  return (
    <div className="screen">
      {/* The section is chosen in the side navigation. */}
      <div className="page-section">
      {tab === 'budgets' && <Budgets {...props} />}
      {tab === 'limits' && <RateLimits {...props} />}
      {tab === 'retention' && <Retention {...props} />}
      {tab === 'tracing' && <Tracing {...props} canChange={offersChanges(role)} />}
      {tab === 'exports' && <Exports {...props} canChange={offersChanges(role)} />}
      {tab === 'webhooks' && <Webhooks {...props} canChange={offersChanges(role)} />}
      </div>
    </div>
  );
}

/** Every page of a small list, for naming the targets of policies. */
async function everything<T>(fetch: (token?: string) => Promise<{ items: T[]; next_page_token?: string | null }>): Promise<T[]> {
  return (await allPages<T, { items: T[]; next_page_token?: string | null }>(fetch)).items;
}

/** Team, service-account and (with `keys`) API key names, each list loaded
 * on its own: one failing leaves the others usable, and can be retried. */
function useNames(organizationId: string, keys = false): Names {
  const [teams, setTeams] = useState<Map<string, string>>(new Map());
  const [serviceAccounts, setServiceAccounts] = useState<Map<string, string>>(new Map());
  const [apiKeys, setApiKeys] = useState<Map<string, string>>(new Map());
  const [activeKeys, setActiveKeys] = useState<Set<string>>(new Set());
  const [failed, setFailed] = useState<NameList[]>([]);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setFailed([]);
    const fail = (list: NameList) => { if (current) setFailed(previous => [...previous, list]); };
    const load = <T extends { id: string; name: string }>(list: NameList, fetch: (token?: string) => Promise<{ items: T[]; next_page_token?: string | null }>, set: (names: Map<string, string>) => void) =>
      everything(fetch).then(
        items => { if (current) set(new Map(items.map(item => [item.id, item.name]))); return items; },
        () => { fail(list); return null; },
      );
    void load('teams', token => api.listTeams(organizationId, { page_size: 100, page_token: token }), setTeams);
    void load('serviceAccounts', token => api.listServiceAccounts(organizationId, { page_size: 100, page_token: token }), setServiceAccounts).then(accounts => {
      if (!keys) return;
      // Without the accounts there are no keys to list either.
      if (!accounts) { fail('apiKeys'); return; }
      // Keys are listed per service account.
      Promise.all(accounts.map(account => everything<ApiKey>(token => api.listApiKeys(organizationId, account.id, { page_size: 100, page_token: token }))
        .then(items => items.map(key => ({ key, account: account.name })))))
        .then(lists => {
          if (!current) return;
          const all = lists.flat();
          setApiKeys(new Map(all.map(({ key, account }) => [key.id, `${key.public_prefix} (${account})${key.status === 'active' ? '' : ` · ${key.status}`}`])));
          setActiveKeys(new Set(all.filter(({ key }) => key.status === 'active').map(({ key }) => key.id)));
        }, () => fail('apiKeys'));
    });
    return () => { current = false; };
  }, [organizationId, attempt, keys]);
  return { teams, serviceAccounts, apiKeys, activeKeys, failed, retry: () => setAttempt(value => value + 1) };
}

type ScopedPolicy = { scope: BudgetScope; team_id: string | null; service_account_id: string | null; api_key_id?: string | null };

export function scopeLabel(policy: ScopedPolicy, names: Names): string {
  if (policy.scope === 'organization') return 'Whole organization';
  if (policy.scope === 'team') return `Team · ${names.teams.get(policy.team_id ?? '') ?? policy.team_id}`;
  if (policy.scope === 'api-key') return `API key · ${names.apiKeys?.get(policy.api_key_id ?? '') ?? policy.api_key_id}`;
  return `Service account · ${names.serviceAccounts.get(policy.service_account_id ?? '') ?? policy.service_account_id}`;
}

/** "Oct 2026", or the fixed window's days. */
function windowLabel(budget: BudgetPolicy): string {
  if (budget.period === 'monthly') {
    const start = budget.current?.period_start ?? new Date().toISOString();
    return `Monthly · ${new Date(start).toLocaleDateString(undefined, { month: 'short', year: 'numeric', timeZone: 'UTC' })}`;
  }
  const day = (value: string | null, offset = 0) => (value ? new Date(new Date(value).getTime() - offset).toLocaleDateString(undefined, { timeZone: 'UTC' }) : '—');
  return `${day(budget.starts_at)} – ${day(budget.ends_at, 1)}`;
}

function Budgets({ organizationId, ...signIn }: Props) {
  const names = useNames(organizationId, true);
  const [editing, setEditing] = useState<BudgetPolicy | 'new' | null>(null);
  const [removing, setRemoving] = useState<BudgetPolicy | null>(null);
  const budgets = useCollection<BudgetPolicy>(() => everyPage(params => api.listBudgets(organizationId, params)), [organizationId]);
  const budgetColumns = useMemo(() => budgetColumnsFor(names), [names.teams, names.serviceAccounts, names.apiKeys]);
  const reload = () => void budgets.load(true);
  const addBudget = <Button onClick={() => setEditing('new')}>Add budget</Button>;
  return (
    <>
      <div className="section-header">
        <div><h2>Budgets</h2><p className="muted">Spending budgets for the whole organization, a team, a service account, or one API key (for apps that share a service account). A request must fit every budget that covers it; a request no budget covers is refused. Budgets count what the gateway calculates, not your provider’s invoice; also set a spending limit with your provider.</p></div>
      </div>
      {budgets.loading && <Message kind="info">Loading budgets…</Message>}
      {budgets.error && <ReportError problem={budgets.error} what="budgets" onRetry={reload} {...signIn} />}
      {!budgets.loading && !budgets.error && budgets.items.length === 0 && (
        <>
          <Message kind="error">No budget is set, so every inference request is refused. Add an organization budget to start serving requests.</Message>
          <p>{addBudget}</p>
        </>
      )}
      {!budgets.loading && !budgets.error && budgets.items.length > 0 && (
        <DataTable
          label="Budgets"
          urlKey="budgets"
          columns={budgetColumns}
          rows={budgets.items}
          rowKey={budget => budget.id}
          toolbarActions={addBudget}
          actions={budget => (
            <>
              <button className="inline-action" onClick={() => setEditing(budget)}>Edit</button>
              <button className="inline-action" onClick={() => setRemoving(budget)}>Remove</button>
            </>
          )}
        />
      )}
      {editing && (
        <BudgetDialog
          {...signIn}
          organizationId={organizationId}
          names={names}
          budget={editing === 'new' ? null : editing}
          onClose={() => setEditing(null)}
          onSaved={() => { setEditing(null); reload(); }}
        />
      )}
      {removing && (
        <RemoveBudget {...signIn} organizationId={organizationId} budget={removing} label={scopeLabel(removing, names)} onClose={() => setRemoving(null)} onRemoved={() => { setRemoving(null); reload(); }} />
      )}
    </>
  );
}

const scopeFormat = (value: string) => ({ organization: 'Organization', team: 'Team', 'service-account': 'Service account', 'api-key': 'API key' } as Record<string, string>)[value] ?? value;

/** The "Applies to" and scope columns every policy table shares. */
function scopeColumns<T extends ScopedPolicy>(names: Names): Column<T>[] {
  return [
    { id: 'applies', label: 'Applies to', primary: true, value: policy => scopeLabel(policy, names) },
    { id: 'scope', label: 'Scope', filter: 'category', format: scopeFormat, hidden: true, value: policy => policy.scope },
  ];
}

function budgetColumnsFor(names: Names): Column<BudgetPolicy>[] {
  return [
    ...scopeColumns<BudgetPolicy>(names),
    { id: 'limit', label: 'Limit', filter: 'number', value: budget => budget.limit_minor / 10 ** (6 + minorDigits(budget.currency)), render: budget => formatMoney(budget.limit_minor, budget.currency) },
    { id: 'currency', label: 'Currency', filter: 'category', hidden: true, value: budget => budget.currency },
    { id: 'period', label: 'Period', filter: 'category', format: value => (value === 'monthly' ? 'Monthly' : 'Fixed'), value: budget => budget.period, render: budget => windowLabel(budget) },
    {
      id: 'used',
      label: 'Used this period',
      filter: 'number',
      // The percent the cell shows; empty when not in effect now.
      value: budget => { const share = budgetShare(budget); return share === null ? null : Math.round(share * 100); },
      render: budget => <BudgetUse budget={budget} />,
    },
    { id: 'updated', label: 'Updated', filter: 'date', hidden: true, value: budget => budget.updated_at, render: budget => formatDate(budget.updated_at) },
  ];
}

function rateLimitColumnsFor(names: Names): Column<RateLimitPolicy>[] {
  const limit = (value: number | null) => (value === null ? 'No limit' : formatCount(value));
  return [
    ...scopeColumns<RateLimitPolicy>(names),
    { id: 'per_minute', label: 'Requests / minute', filter: 'number', value: policy => policy.requests_per_minute, render: policy => limit(policy.requests_per_minute) },
    { id: 'tokens', label: 'Tokens / minute', filter: 'number', value: policy => policy.tokens_per_minute, render: policy => limit(policy.tokens_per_minute) },
    { id: 'concurrent', label: 'In flight', filter: 'number', value: policy => policy.max_concurrent, render: policy => limit(policy.max_concurrent) },
    { id: 'updated', label: 'Updated', filter: 'date', value: policy => policy.updated_at, title: policy => policy.updated_at, render: policy => formatDate(policy.updated_at) },
  ];
}

/** Spent and held in the current window as a share of the limit, at most
 * 1; null when the budget is not in effect now. */
export function budgetShare(budget: BudgetPolicy): number | null {
  if (!budget.current) return null;
  const used = budget.current.consumed_minor + budget.current.reserved_minor;
  return budget.limit_minor > 0 ? Math.min(1, used / budget.limit_minor) : 1;
}

/** Spent and held in the current window, as a share of the limit. */
export function BudgetUse({ budget }: { budget: BudgetPolicy }) {
  if (!budget.current) return <span className="muted">Not in effect now</span>;
  const used = budget.current.consumed_minor + budget.current.reserved_minor;
  const share = budgetShare(budget) ?? 0;
  const level = share >= 1 ? 'full' : share >= 0.8 ? 'high' : 'ok';
  return (
    <span className="budget-use" title={`${formatMoney(budget.current.consumed_minor, budget.currency)} spent, ${formatMoney(budget.current.reserved_minor, budget.currency)} held by requests in flight`}>
      {formatMoney(used, budget.currency)} <span className="muted">({Math.round(share * 100)}%)</span>
      <span className={`budget-bar ${level}`} role="progressbar" aria-label="Budget used" aria-valuemin={0} aria-valuemax={100} aria-valuenow={Math.round(share * 100)}><span style={{ width: `${share * 100}%` }} /></span>
    </span>
  );
}

function BudgetDialog({ organizationId, names, budget, onClose, onSaved, ...signIn }: SignIn & {
  organizationId: string;
  names: Names;
  budget: BudgetPolicy | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [scope, setScope] = useState<BudgetScope>(budget?.scope ?? 'organization');
  const [target, setTarget] = useState(budget ? budgetTarget(budget) ?? '' : '');
  const [currency, setCurrency] = useState(budget?.currency ?? 'USD');
  const [amount, setAmount] = useState(budget ? toMajor(budget.limit_minor, budget.currency) : '');
  const [period, setPeriod] = useState<BudgetPeriod>(budget?.period ?? 'monthly');
  const initialFrom = budget?.starts_at?.slice(0, 10) ?? '';
  const initialTo = budget?.ends_at ? new Date(new Date(budget.ends_at).getTime() - 1).toISOString().slice(0, 10) : '';
  const [fromDate, setFromDate] = useState(initialFrom);
  const [toDate, setToDate] = useState(initialTo);
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One attempt per form content, with the version it replaces: a retry
  // replays instead of overwriting whatever changed since.
  const attempt = useRef<{ body: string; key: string; version: number | null } | null>(null);
  const targets = scope === 'team'
    ? names.teams
    : scope === 'service-account'
      ? names.serviceAccounts
      // A new budget goes on a key still in use; a revoked key keeps the one it has.
      : scope === 'api-key'
        ? new Map(Array.from(names.apiKeys ?? new Map<string, string>()).filter(([id]) => names.activeKeys?.has(id)))
        : null;
  const targetList: NameList = scope === 'team' ? 'teams' : scope === 'api-key' ? 'apiKeys' : 'serviceAccounts';
  const targetNoun = scope === 'team' ? 'team' : scope === 'api-key' ? 'API key' : 'service account';
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const built = budgetInput({ amount, currency, period, fromDate, toDate });
    // The form edits whole days; a fixed window whose days were left alone
    // keeps its exact times, so what it has spent still counts.
    if ('budget' in built && built.budget.period === 'fixed' && budget?.period === 'fixed' && fromDate === initialFrom && toDate === initialTo) {
      built.budget.starts_at = budget.starts_at ?? built.budget.starts_at;
      built.budget.ends_at = budget.ends_at ?? built.budget.ends_at;
    }
    const targetId = scope === 'organization' ? null : target;
    const next: Record<string, string> = 'errors' in built ? { ...built.errors } : {};
    if (scope !== 'organization' && !targetId) next.target = `Choose ${scope === 'api-key' ? 'an' : 'a'} ${targetNoun}.`;
    setErrors(next);
    if ('errors' in built || Object.keys(next).length) return;
    setProblem(null);
    setSubmitting(true);
    try {
      const body = JSON.stringify([scope, targetId, built.budget]);
      if (attempt.current?.body !== body) {
        const current = budget ?? (await everything(token => api.listBudgets(organizationId, { page_size: 100, page_token: token })))
          .find(item => item.scope === scope && budgetTarget(item) === targetId) ?? null;
        attempt.current = { body, key: newIdempotencyKey(), version: current?.version ?? null };
      }
      await api.setBudget(organizationId, scope, targetId, built.budget, attempt.current.version, attempt.current.key);
      onSaved();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The budget could not be saved.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={budget ? 'Edit budget' : 'Add budget'}>
      <form onSubmit={submit}>
        <div className="dialog-body">
          {!budget && (
            <FormField label="Applies to" htmlFor="budget-scope">
              <Select id="budget-scope" value={scope} onChange={event => { setScope(event.target.value as BudgetScope); setTarget(''); }}>
                <option value="organization">Whole organization</option>
                <option value="team">A team</option>
                <option value="service-account">A service account</option>
                <option value="api-key">An API key</option>
              </Select>
            </FormField>
          )}
          {!budget && targets && names.failed?.includes(targetList) && (
            <Message kind="error">The {targetNoun === 'API key' ? 'API keys' : `${targetNoun}s`} could not be loaded. <button type="button" className="inline-action" onClick={names.retry}>Try again</button></Message>
          )}
          {!budget && scope === 'api-key' && (
            <p className="cds--form__helper-text">Applies on top of the key’s service account, team and organization budgets: its requests must fit all of them.</p>
          )}
          {!budget && targets && (
            <FormField label={targetNoun[0].toUpperCase() + targetNoun.slice(1)} htmlFor="budget-target" error={errors.target}>
              <Select id="budget-target" value={target} onChange={event => setTarget(event.target.value)}>
                <option value="">Choose…</option>
                {Array.from(targets).map(([id, name]) => <option key={id} value={id}>{name}</option>)}
              </Select>
            </FormField>
          )}
          {budget && <p className="muted">{scopeLabel(budget, names)}</p>}
          <FormField label="Limit" htmlFor="budget-amount" hint="The most requests may spend in the period." error={errors.amount}>
            <TextInput id="budget-amount" inputMode="decimal" placeholder="100.00" value={amount} onChange={event => setAmount(event.target.value)} />
          </FormField>
          <FormField label="Currency" htmlFor="budget-currency" hint="Requests are charged in the currency of their price." error={errors.currency}>
            <TextInput id="budget-currency" value={currency} maxLength={3} onChange={event => setCurrency(event.target.value)} />
          </FormField>
          <FormField label="Period" htmlFor="budget-period">
            <Select id="budget-period" value={period} onChange={event => setPeriod(event.target.value as BudgetPeriod)}>
              <option value="monthly">Every month (resets on the 1st, UTC)</option>
              <option value="fixed">Fixed dates</option>
            </Select>
          </FormField>
          {period === 'fixed' && (
            <>
              <FormField label="First day (UTC)" htmlFor="budget-from"><TextInput id="budget-from" type="date" value={fromDate} onChange={event => setFromDate(event.target.value)} /></FormField>
              <FormField label="Last day (UTC)" htmlFor="budget-to" error={errors.dates}><TextInput id="budget-to" type="date" value={toDate} onChange={event => setToDate(event.target.value)} /></FormField>
            </>
          )}
          {budget && <p className="cds--form__helper-text">A new limit applies now and keeps what this period has spent. A new currency, period or dates start a new balance.</p>}
        </div>
        <ActionError problem={problem} action="change budgets" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Save</Button>
        </div>
      </form>
    </Dialog>
  );
}

function RemoveBudget({ organizationId, budget, label, onClose, onRemoved, ...signIn }: SignIn & { organizationId: string; budget: BudgetPolicy; label: string; onClose: () => void; onRemoved: () => void }) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const key = useRef(newIdempotencyKey());
  const remove = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      await api.deleteBudget(organizationId, budget, key.current);
      onRemoved();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The budget could not be removed.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open alert onClose={() => { if (!submitting) onClose(); }} title="Remove budget">
      <div className="dialog-body"><p>{label} will no longer be covered by this budget. Requests no other budget covers will be refused.</p></div>
      <ActionError problem={problem} action="change budgets" {...signIn} />
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button variant="danger" onClick={() => void remove()} loading={submitting}>Remove</Button>
      </div>
    </Dialog>
  );
}

function RateLimits({ organizationId, ...signIn }: Props) {
  const names = useNames(organizationId, true);
  const [editing, setEditing] = useState<RateLimitPolicy | 'new' | null>(null);
  const [removing, setRemoving] = useState<RateLimitPolicy | null>(null);
  const policies = useCollection<RateLimitPolicy>(() => everyPage(params => api.listRateLimits(organizationId, params)), [organizationId]);
  const rateLimitColumns = useMemo(() => rateLimitColumnsFor(names), [names.teams, names.serviceAccounts, names.apiKeys]);
  const reload = () => void policies.load(true);
  const addLimit = <Button onClick={() => setEditing('new')}>Add rate limit</Button>;
  return (
    <>
      <div className="section-header">
        <div><h2>Rate limits</h2><p className="muted">Requests per minute, tokens per minute and requests in flight, for the whole organization, a team, a service account, or one API key, the levels budgets have. A request must pass every policy that covers it; over a limit, it gets 429 with Retry-After. Tokens per minute counts what a request may use while it runs and what it reported once done, and lowers a request’s output cap to what fits.</p></div>
      </div>
      {policies.loading && <Message kind="info">Loading rate limits…</Message>}
      {policies.error && <ReportError problem={policies.error} what="rate limits" onRetry={reload} {...signIn} />}
      {!policies.loading && !policies.error && policies.items.length === 0 && <EmptyState title="No rate limits" text="Requests are limited only by provider capacity and quota." action={addLimit} />}
      {!policies.loading && !policies.error && policies.items.length > 0 && (
        <DataTable
          label="Rate limits"
          urlKey="rate-limits"
          columns={rateLimitColumns}
          rows={policies.items}
          rowKey={policy => policy.id}
          toolbarActions={addLimit}
          actions={policy => (
            <>
              <button className="inline-action" onClick={() => setEditing(policy)}>Edit</button>
              <button className="inline-action" onClick={() => setRemoving(policy)}>Remove</button>
            </>
          )}
        />
      )}
      {editing && (
        <RateLimitDialog
          {...signIn}
          organizationId={organizationId}
          names={names}
          policy={editing === 'new' ? null : editing}
          onClose={() => setEditing(null)}
          onSaved={() => { setEditing(null); reload(); }}
        />
      )}
      {removing && (
        <RemoveRateLimit {...signIn} organizationId={organizationId} policy={removing} label={scopeLabel(removing, names)} onClose={() => setRemoving(null)} onRemoved={() => { setRemoving(null); reload(); }} />
      )}
    </>
  );
}

function RateLimitDialog({ organizationId, names, policy, onClose, onSaved, ...signIn }: SignIn & {
  organizationId: string;
  names: Names;
  policy: RateLimitPolicy | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const [scope, setScope] = useState<RateLimitScope>(policy?.scope ?? 'organization');
  const [target, setTarget] = useState(policy ? budgetTarget(policy) ?? '' : '');
  const [rpm, setRpm] = useState(policy?.requests_per_minute?.toString() ?? '');
  const [tpm, setTpm] = useState(policy?.tokens_per_minute?.toString() ?? '');
  const [concurrent, setConcurrent] = useState(policy?.max_concurrent?.toString() ?? '');
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One attempt per form content: its key and the version it replaces. A
  // retry reuses both, so a save whose response was lost replays instead of
  // overwriting whatever changed since.
  const attempt = useRef<{ body: string; key: string; version: number | null } | null>(null);
  const targets = scope === 'team'
    ? names.teams
    : scope === 'service-account'
      ? names.serviceAccounts
      // A new limit goes on a key still in use; a revoked key keeps the one it has.
      : scope === 'api-key'
        ? new Map(Array.from(names.apiKeys ?? new Map<string, string>()).filter(([id]) => names.activeKeys?.has(id)))
        : null;
  const targetList: NameList = scope === 'team' ? 'teams' : scope === 'api-key' ? 'apiKeys' : 'serviceAccounts';
  const targetNoun = scope === 'team' ? 'team' : scope === 'api-key' ? 'API key' : 'service account';
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const built = rateLimitInput(rpm, concurrent, tpm);
    const targetId = scope === 'organization' ? null : target;
    const next: Record<string, string> = 'errors' in built ? { ...built.errors } : {};
    if (scope !== 'organization' && !targetId) next.target = `Choose ${scope === 'api-key' ? 'an' : 'a'} ${targetNoun}.`;
    setErrors(next);
    if ('errors' in built || Object.keys(next).length) return;
    setProblem(null);
    setSubmitting(true);
    try {
      const body = JSON.stringify([scope, targetId, built.limits]);
      if (attempt.current?.body !== body) {
        // Replacing a policy that exists is guarded by its version, wherever
        // it is in the list; only a target with none is created with `*`.
        const current = policy ?? (await everything(token => api.listRateLimits(organizationId, { page_size: 100, page_token: token })))
          .find(item => item.scope === scope && budgetTarget(item) === targetId) ?? null;
        attempt.current = { body, key: newIdempotencyKey(), version: current?.version ?? null };
      }
      await api.setRateLimit(organizationId, scope, targetId, built.limits, attempt.current.version, attempt.current.key);
      onSaved();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The rate limit could not be saved.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={policy ? 'Edit rate limit' : 'Add rate limit'}>
      <form onSubmit={submit}>
        <div className="dialog-body">
          {!policy && (
            <FormField label="Applies to" htmlFor="limit-scope">
              <Select id="limit-scope" value={scope} onChange={event => { setScope(event.target.value as RateLimitScope); setTarget(''); }}>
                <option value="organization">Whole organization</option>
                <option value="team">A team</option>
                <option value="service-account">A service account</option>
                <option value="api-key">An API key</option>
              </Select>
            </FormField>
          )}
          {!policy && targets && names.failed?.includes(targetList) && (
            <Message kind="error">The {targetNoun === 'API key' ? 'API keys' : `${targetNoun}s`} could not be loaded. <button type="button" className="inline-action" onClick={names.retry}>Try again</button></Message>
          )}
          {!policy && scope === 'api-key' && (
            <p className="cds--form__helper-text">Applies on top of the key’s service account, team and organization limits: its requests must pass all of them.</p>
          )}
          {!policy && targets && (
            <FormField label={targetNoun[0].toUpperCase() + targetNoun.slice(1)} htmlFor="limit-target" error={errors.target}>
              <Select id="limit-target" value={target} onChange={event => setTarget(event.target.value)}>
                <option value="">Choose…</option>
                {Array.from(targets).map(([id, name]) => <option key={id} value={id}>{name}</option>)}
              </Select>
            </FormField>
          )}
          {policy && <p className="muted">{scopeLabel(policy, names)}</p>}
          <FormField label="Requests per minute" htmlFor="limit-rpm" hint="Admitted in any 60-second window. Empty for no limit." error={errors.requests_per_minute}>
            <TextInput id="limit-rpm" inputMode="numeric" value={rpm} onChange={event => setRpm(event.target.value)} />
          </FormField>
          <FormField label="Tokens per minute" htmlFor="limit-tpm" hint="Input and output tokens in any 60-second window: the most a request may use while it runs, then what it reported. Empty for no limit." error={errors.tokens_per_minute}>
            <TextInput id="limit-tpm" inputMode="numeric" value={tpm} onChange={event => setTpm(event.target.value)} />
          </FormField>
          <FormField label="Requests in flight" htmlFor="limit-concurrent" hint="At once. Empty for no limit." error={errors.max_concurrent}>
            <TextInput id="limit-concurrent" inputMode="numeric" value={concurrent} onChange={event => setConcurrent(event.target.value)} />
          </FormField>
          {errors.form && <Message kind="error">{errors.form}</Message>}
        </div>
        <ActionError problem={problem} action="change rate limits" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Save</Button>
        </div>
      </form>
    </Dialog>
  );
}

function RemoveRateLimit({ organizationId, policy, label, onClose, onRemoved, ...signIn }: SignIn & { organizationId: string; policy: RateLimitPolicy; label: string; onClose: () => void; onRemoved: () => void }) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const key = useRef(newIdempotencyKey());
  const remove = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      await api.deleteRateLimit(organizationId, policy, key.current);
      onRemoved();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The rate limit could not be removed.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open alert onClose={() => { if (!submitting) onClose(); }} title="Remove rate limit">
      <div className="dialog-body"><p>{label} will no longer be limited by this policy.</p></div>
      <ActionError problem={problem} action="change rate limits" {...signIn} />
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button variant="danger" onClick={() => void remove()} loading={submitting}>Remove</Button>
      </div>
    </Dialog>
  );
}

const periods: [keyof RetentionPeriods, string, string][] = [
  ['trace_days', 'Traces', '7 to 3,650 days. Request metadata: model, status, timing, correlation ids.'],
  ['usage_days', 'Usage and accounting', '30 to 3,650 days. Tokens, amounts, settlements. Requests still settling are kept until final.'],
  ['audit_days', 'Audit events', '90 to 3,650 days.'],
];
const asText = (value: number | null) => (value === null ? '' : String(value));

/** Whether the form still holds the periods a save sent. */
export function savedMatches(sent: string, values: Record<keyof RetentionPeriods, string>): boolean {
  const built = retentionInput(values);
  return 'periods' in built && JSON.stringify(built.periods) === sent;
}

/** Whether `next` deletes data `current` keeps: a period added or shortened. */
export function shortens(current: RetentionPeriods, next: RetentionPeriods): boolean {
  return (Object.keys(next) as (keyof RetentionPeriods)[]).some(key => next[key] !== null && (current[key] === null || next[key]! < current[key]!));
}

function Retention({ organizationId, ...signIn }: Props) {
  const [policy, setPolicy] = useState<RetentionPolicy | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attemptLoad, setAttemptLoad] = useState(0);
  // After a version conflict: load the current policy but keep the edits.
  const keepEdits = useRef(false);
  const [refreshed, setRefreshed] = useState<RetentionPolicy | null>(null);
  const [values, setValues] = useState<Record<keyof RetentionPeriods, string>>({ trace_days: '', usage_days: '', audit_days: '' });
  const [errors, setErrors] = useState<Partial<Record<keyof RetentionPeriods, string>>>({});
  const [confirming, setConfirming] = useState<RetentionPeriods | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // What the last successful save sent: "saved" shows only while the form
  // still holds exactly that.
  const [saved, setSaved] = useState<string | null>(null);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  useEffect(() => {
    let current = true;
    setLoadProblem(null);
    api.retentionPolicy(organizationId).then(
      value => {
        if (!current) return;
        setPolicy(value);
        if (keepEdits.current) {
          keepEdits.current = false;
          setRefreshed(value);
          setProblem(null);
          attempt.current = null;
        } else {
          setValues({ trace_days: asText(value.trace_days), usage_days: asText(value.usage_days), audit_days: asText(value.audit_days) });
        }
      },
      caught => { if (current) setLoadProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The retention policy could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, attemptLoad]);
  if (loadProblem) return <ReportError problem={loadProblem} what="retention policy" onRetry={() => setAttemptLoad(value => value + 1)} {...signIn} />;
  if (!policy) return <Message kind="info">Loading retention policy…</Message>;

  const save = async (next: RetentionPeriods) => {
    const body = JSON.stringify(next);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      const { data } = await api.setRetentionPolicy(organizationId, next, policy.version, attempt.current.key);
      // Saved: the next save is a new request (against the new version).
      attempt.current = null;
      setRefreshed(null);
      setPolicy(data);
      setConfirming(null);
      setSaved(JSON.stringify(next));
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The retention policy could not be saved.'));
    } finally {
      setSubmitting(false);
    }
  };
  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    setSaved(null);
    const built = retentionInput(values);
    if ('errors' in built) {
      setErrors(built.errors);
      return;
    }
    setErrors({});
    if (shortens(policy, built.periods)) setConfirming(built.periods);
    else void save(built.periods);
  };
  return (
    <div className="cds--tile detail-panel">
      <div className="detail-header">
        <div>
          <h3>Data retention</h3>
          <p className="meta">How long this organization’s data is kept. Empty keeps it indefinitely. An hourly purge deletes what is older.</p>
        </div>
      </div>
      <form onSubmit={submit} className="policy-form">
        {/* Locked while a save is in flight, so what it confirms is what was sent. */}
        <fieldset className="plain-fieldset" disabled={submitting}>
        {periods.map(([key, label, hint]) => (
          <FormField key={key} label={`${label} (days)`} htmlFor={`retention-${key}`} hint={hint} error={errors[key]}>
            <TextInput id={`retention-${key}`} inputMode="numeric" placeholder="Keep indefinitely" value={values[key]} onChange={event => { setSaved(null); setValues(previous => ({ ...previous, [key]: event.target.value })); }} />
          </FormField>
        ))}
        </fieldset>
        {!confirming && problem?.code === 'version_conflict' && (
          <Message kind="error">
            Someone changed the retention policy since you opened it.{' '}
            <button type="button" className="inline-action" onClick={() => { keepEdits.current = true; setAttemptLoad(value => value + 1); }}>Load the current policy</button>; your edits are kept.
          </Message>
        )}
        {!confirming && problem?.code !== 'version_conflict' && <ActionError problem={problem} action="change the retention policy" {...signIn} />}
        {refreshed && (
          <Message kind="info">
            Current policy: traces {refreshed.trace_days ?? 'kept indefinitely'}, usage {refreshed.usage_days ?? 'kept indefinitely'}, audit events {refreshed.audit_days ?? 'kept indefinitely'} (days). Save to replace it with the values above.
          </Message>
        )}
        {saved !== null && savedMatches(saved, values) && <Message kind="success">Retention policy saved.</Message>}
        <div className="dialog-actions"><Button type="submit" loading={submitting && !confirming}>Save policy</Button></div>
      </form>
      <p className="meta">
        {policy.last_purged_at
          ? <>Last purge {formatDate(policy.last_purged_at)}. Removed so far: {formatCount(policy.purged.traces)} traces, {formatCount(policy.purged.reservations)} requests’ accounting, {formatCount(policy.purged.audit_events)} audit events.</>
          : 'No purge has run yet.'}
      </p>
      {confirming && (
        <Dialog open alert onClose={() => { if (!submitting) setConfirming(null); }} title="Delete older data?">
          <div className="dialog-body"><p>The next hourly purge permanently deletes data older than the new periods. This cannot be undone.</p></div>
          <ActionError problem={problem} action="change the retention policy" {...signIn} />
          <div className="dialog-actions">
            <Button variant="ghost" onClick={() => setConfirming(null)} disabled={submitting}>Cancel</Button>
            <Button variant="danger" onClick={() => void save(confirming)} loading={submitting}>Save and delete older data</Button>
          </div>
        </Dialog>
      )}
    </div>
  );
}

const traceModes: [TraceMode, string, string][] = [
  ['off', 'Off', 'No request content is kept.'],
  ['organization', 'Whole organization', 'Every request in the organization.'],
  ['selected', 'Selected teams and service accounts', 'Only requests from the teams and service accounts chosen below.'],
];

type TraceValues = { mode: TraceMode; teamIds: string[]; serviceAccountIds: string[]; content: boolean; days: string };
const traceValues = (policy: TracePolicy): TraceValues => ({
  mode: policy.mode,
  teamIds: policy.team_ids,
  serviceAccountIds: policy.service_account_ids,
  content: policy.content,
  days: String(policy.content_retention_days),
});

/** What traced requests keep: the prompt and response, encrypted in the
 * trace store, readable by owners and admins, every read audited. */
function Tracing({ organizationId, canChange, ...signIn }: Props & { canChange: boolean }) {
  const names = useNames(organizationId);
  const [policy, setPolicy] = useState<TracePolicy | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attemptLoad, setAttemptLoad] = useState(0);
  // After a version conflict: load the current policy but keep the edits.
  const keepEdits = useRef(false);
  const [values, setValues] = useState<TraceValues>({ mode: 'off', teamIds: [], serviceAccountIds: [], content: true, days: '30' });
  const [errors, setErrors] = useState<Partial<Record<'selection' | 'days', string>>>({});
  const [confirming, setConfirming] = useState<TracePolicyInput | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [saved, setSaved] = useState(false);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  useEffect(() => {
    let current = true;
    setLoadProblem(null);
    api.tracePolicy(organizationId).then(
      value => {
        if (!current) return;
        setPolicy(value);
        if (keepEdits.current) {
          keepEdits.current = false;
          setProblem(null);
          attempt.current = null;
        } else {
          setValues(traceValues(value));
        }
      },
      caught => { if (current) setLoadProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The tracing policy could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, attemptLoad]);
  if (loadProblem) return <ReportError problem={loadProblem} what="tracing policy" onRetry={() => setAttemptLoad(value => value + 1)} {...signIn} />;
  if (!policy) return <Message kind="info">Loading tracing policy…</Message>;

  const change = (next: Partial<TraceValues>) => {
    setSaved(false);
    setValues(previous => ({ ...previous, ...next }));
  };
  const toggle = (list: 'teamIds' | 'serviceAccountIds', id: string) =>
    change({ [list]: values[list].includes(id) ? values[list].filter(value => value !== id) : [...values[list], id] });
  // Days are edited only while content is kept; otherwise the saved
  // period goes back unchanged, whatever the hidden field holds.
  const daysShown = values.mode !== 'off' && values.content;
  const save = async (next: TracePolicyInput) => {
    const body = JSON.stringify(next);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      const { data } = await api.setTracePolicy(organizationId, next, policy.version, attempt.current.key);
      attempt.current = null;
      setPolicy(data);
      setConfirming(null);
      setSaved(true);
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The tracing policy could not be saved.'));
    } finally {
      setSubmitting(false);
    }
  };
  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    setSaved(false);
    const built = traceInput({ ...values, days: daysShown ? values.days : String(policy.content_retention_days) });
    if ('errors' in built) {
      setErrors(built.errors);
      return;
    }
    setErrors({});
    // A shorter period deletes stored content at the next hourly sweep.
    if (policy.version > 0 && built.policy.content_retention_days < policy.content_retention_days) setConfirming(built.policy);
    else void save(built.policy);
  };
  const choices = (list: 'teamIds' | 'serviceAccountIds', known: Map<string, string>, label: string) => {
    // Chosen ids the list no longer names (removed, or not loaded) stay visible.
    const ids = [...new Set([...known.keys(), ...values[list]])];
    const sorted = ids.sort((a, b) => (known.get(a) ?? a).localeCompare(known.get(b) ?? b));
    return (
      <fieldset className="plain-fieldset">
        <legend>{label}</legend>
        {sorted.length ? (
          <div className="checkbox-group">
            {sorted.map(id => (
              <CheckOption key={id} checked={values[list].includes(id)} onChange={() => toggle(list, id)}>{known.get(id) ?? uuidHint(id)}</CheckOption>
            ))}
          </div>
        ) : <p className="meta">None in this organization.</p>}
      </fieldset>
    );
  };
  return (
    <div className="cds--tile detail-panel">
      <div className="detail-header">
        <div>
          <h3>Request tracing</h3>
          <p className="meta">Keep the prompt and response of traced requests, encrypted in the trace store. Owners and admins can read them from a request’s details; every read is audited.</p>
        </div>
      </div>
      <form onSubmit={submit} className="policy-form">
        <fieldset className="plain-fieldset" disabled={submitting || !canChange}>
          <FormField label="Trace" htmlFor="trace-mode" hint={traceModes.find(([mode]) => mode === values.mode)?.[2]}>
            <Select id="trace-mode" value={values.mode} onChange={event => change({ mode: event.target.value as TraceMode })}>
              {traceModes.map(([mode, label]) => <option key={mode} value={mode}>{label}</option>)}
            </Select>
          </FormField>
          {values.mode === 'selected' && (
            <>
              {names.failed?.length ? (
                <Message kind="error">
                  Some names could not be loaded.{' '}
                  <button type="button" className="inline-action" onClick={names.retry}>Retry</button>
                </Message>
              ) : null}
              {choices('teamIds', names.teams, 'Teams')}
              {choices('serviceAccountIds', names.serviceAccounts, 'Service accounts')}
              {errors.selection && <Message kind="error">{errors.selection}</Message>}
            </>
          )}
          {values.mode !== 'off' && (
            <>
              <CheckOption checked={values.content} onChange={event => change({ content: event.target.checked })}>Keep prompt and response content</CheckOption>
              <FormField label="Keep content (days)" htmlFor="trace-days" hint="1 to 3,650 days. Content also goes when the request’s trace or usage record is purged." error={errors.days}>
                <TextInput id="trace-days" inputMode="numeric" value={values.days} disabled={!values.content} onChange={event => change({ days: event.target.value })} />
              </FormField>
            </>
          )}
        </fieldset>
        {!confirming && problem?.code === 'version_conflict' && (
          <Message kind="error">
            Someone changed the tracing policy since you opened it.{' '}
            <button type="button" className="inline-action" onClick={() => { keepEdits.current = true; setAttemptLoad(value => value + 1); }}>Load the current policy</button>; your edits are kept.
          </Message>
        )}
        {!confirming && problem?.code !== 'version_conflict' && <ActionError problem={problem} action="change the tracing policy" {...signIn} />}
        {saved && <Message kind="success">Tracing policy saved.</Message>}
        {canChange
          ? <div className="dialog-actions"><Button type="submit" loading={submitting && !confirming}>Save policy</Button></div>
          : <p className="meta">Your role can view the tracing policy but not change it.</p>}
      </form>
      {policy.updated_at && <p className="meta">Last changed {formatDate(policy.updated_at)}.</p>}
      {confirming && (
        <Dialog open alert onClose={() => { if (!submitting) setConfirming(null); }} title="Delete older content?">
          <div className="dialog-body"><p>The next hourly sweep permanently deletes stored request content older than {formatCount(confirming.content_retention_days)} days. This cannot be undone.</p></div>
          <ActionError problem={problem} action="change the tracing policy" {...signIn} />
          <div className="dialog-actions">
            <Button variant="ghost" onClick={() => setConfirming(null)} disabled={submitting}>Cancel</Button>
            <Button variant="danger" onClick={() => void save(confirming)} loading={submitting}>Save and delete older content</Button>
          </div>
        </Dialog>
      )}
    </div>
  );
}

const datasets: [ExportDataset, string][] = [['usage', 'Usage'], ['traces', 'Traces'], ['audit-events', 'Audit events']];

/** The export range for whole UTC days [from, to], or why it can't be one. */
export function exportRange(fromDate: string, toDate: string): { from: string; to: string } | { error: string } {
  if (!fromDate || !toDate) return { error: 'Choose both dates.' };
  const from = new Date(`${fromDate}T00:00:00Z`);
  const to = new Date(new Date(`${toDate}T00:00:00Z`).getTime() + 86_400_000);
  if (Number.isNaN(from.getTime()) || Number.isNaN(to.getTime())) return { error: 'Choose valid dates.' };
  if (to <= from) return { error: 'The end date must not be before the start date.' };
  if (to.getTime() - from.getTime() > 366 * 86_400_000) return { error: 'Export at most 366 days at a time.' };
  return { from: from.toISOString(), to: to.toISOString() };
}

function Exports({ organizationId, canChange, ...signIn }: Props & { canChange: boolean }) {
  const today = new Date().toISOString().slice(0, 10);
  const monthAgo = new Date(Date.now() - 30 * 86_400_000).toISOString().slice(0, 10);
  const [dataset, setDataset] = useState<ExportDataset>('usage');
  const [format, setFormat] = useState<ExportFormat>('csv');
  const [fromDate, setFromDate] = useState(monthAgo);
  const [toDate, setToDate] = useState(today);
  // Few exports are ever made, so the whole history is loaded and filtered here.
  const exports = useCollection<DataExport>(() => everyPage(params => api.listExports(organizationId, params)), [organizationId]);
  const range = exportRange(fromDate, toDate);
  return (
    <>
      <UsageExportSection {...signIn} organizationId={organizationId} canChange={canChange} />
      <div className="cds--tile detail-panel">
        <div className="detail-header"><div><h3>Export data</h3><p className="meta">Downloads every row recorded in the chosen UTC days, oldest first. Each export is recorded and audited.</p></div></div>
        <div className="report-controls">
          <div className="audit-filter"><FormField label="Data" htmlFor="export-dataset">
            <Select id="export-dataset" value={dataset} onChange={event => setDataset(event.target.value as ExportDataset)}>
              {datasets.map(([value, label]) => <option key={value} value={value}>{label}</option>)}
            </Select>
          </FormField></div>
          <div className="audit-filter"><FormField label="Format" htmlFor="export-format">
            <Select id="export-format" value={format} onChange={event => setFormat(event.target.value as ExportFormat)}>
              <option value="csv">CSV</option>
              <option value="ndjson">NDJSON</option>
            </Select>
          </FormField></div>
          <div className="audit-filter"><FormField label="From" htmlFor="export-from"><TextInput id="export-from" type="date" value={fromDate} max={today} onChange={event => setFromDate(event.target.value)} /></FormField></div>
          <div className="audit-filter"><FormField label="To" htmlFor="export-to"><TextInput id="export-to" type="date" value={toDate} max={today} onChange={event => setToDate(event.target.value)} /></FormField></div>
          {'error' in range
            ? <span className="field-error" role="alert">{range.error}</span>
            : <a className={buttonClass()} href={exportUrl(organizationId, dataset, format, range.from, range.to)} download onClick={() => window.setTimeout(() => void exports.load(true), 2000)}>Download</a>}
        </div>
      </div>
      <div className="section-header"><div><h2>Export history</h2><p className="muted">Who exported what, and how it ended.</p></div></div>
      {exports.loading && <Message kind="info">Loading exports…</Message>}
      {exports.error && <ReportError problem={exports.error} what="export history" onRetry={() => void exports.load(true)} {...signIn} />}
      {!exports.loading && !exports.error && exports.items.length === 0 && <EmptyState title="No exports yet" text="Exports appear here once someone downloads one." action={<Button variant="ghost" onClick={() => void exports.load(true)}>Refresh</Button>} />}
      {!exports.loading && !exports.error && exports.items.length > 0 && (
        <DataTable label="Export history" urlKey="exports" columns={exportColumns} rows={exports.items} rowKey={item => item.id} detail={item => <ExportDetail item={item} />} toolbarActions={<Button variant="ghost" onClick={() => void exports.load(true)}>Refresh</Button>} />
      )}
    </>
  );
}

const exportStatusBadge: Record<DataExport['status'], string> = { completed: 'badge-success', streaming: 'badge-neutral', failed: 'badge-danger', abandoned: 'badge-danger' };
/** The last UTC day an export covers (its `to` is exclusive). */
const exportRangeText = (item: DataExport) =>
  `${new Date(item.from).toLocaleDateString(undefined, { timeZone: 'UTC' })} – ${new Date(new Date(item.to).getTime() - 1).toLocaleDateString(undefined, { timeZone: 'UTC' })}`;

/** The exporter of an export whose user was removed; never a user ID. */
const UNKNOWN_EXPORTER = 'unknown';

/** Export history columns: few rows, so filtered and grouped in the browser. */
export const exportColumns: Column<DataExport>[] = [
  { id: 'started', label: 'Started', filter: 'date', value: item => item.started_at, title: item => item.started_at, render: item => formatDate(item.started_at) },
  { id: 'dataset', label: 'Data', primary: true, filter: 'category', format: value => datasets.find(([key]) => key === value)?.[1] ?? value, value: item => item.dataset },
  { id: 'format', label: 'Format', filter: 'category', format: value => value.toUpperCase(), value: item => item.format },
  { id: 'range', label: 'Range', filter: 'date', value: item => item.from, render: exportRangeText, search: exportRangeText },
  { id: 'status', label: 'Status', filter: 'category', value: item => item.status, render: item => <Badge tone={exportStatusBadge[item.status]}>{item.status}</Badge> },
  { id: 'rows', label: 'Rows', filter: 'number', sum: true, value: item => item.rows, render: item => formatCount(item.rows), formatTotal: formatCount },
  // A removed user's exports share one value, so they can be filtered and grouped too.
  { id: 'actor', label: 'Exported by', filter: 'category', hidden: true, format: value => (value === UNKNOWN_EXPORTER ? 'Unknown' : uuidHint(value) ?? value), value: item => item.actor_user_id ?? UNKNOWN_EXPORTER, title: item => item.actor_user_id ?? undefined, render: item => (item.actor_user_id ? uuidHint(item.actor_user_id) : 'Unknown') },
  { id: 'finished', label: 'Finished', filter: 'date', hidden: true, value: item => item.finished_at, render: item => (item.finished_at ? formatDate(item.finished_at) : '—') },
];

function ExportDetail({ item }: { item: DataExport }) {
  const seconds = item.finished_at ? Math.max(0, Math.round((Date.parse(item.finished_at) - Date.parse(item.started_at)) / 1000)) : null;
  const rows: [string, React.ReactNode][] = [
    ['Data', `${datasets.find(([key]) => key === item.dataset)?.[1] ?? item.dataset} · ${item.format.toUpperCase()}`],
    ['Range (UTC days)', exportRangeText(item)],
    ['Status', item.status],
    ['Rows', formatCount(item.rows)],
    ['Exported by', item.actor_user_id ?? 'Unknown'],
    ['Started', formatDate(item.started_at)],
    ['Finished', item.finished_at ? `${formatDate(item.finished_at)} · ${seconds} s` : 'Not finished'],
    ['Export ID', item.id],
  ];
  return (
    <dl className="detail-list">
      {rows.map(([label, value]) => <React.Fragment key={label}><dt>{label}</dt><dd>{value}</dd></React.Fragment>)}
    </dl>
  );
}
