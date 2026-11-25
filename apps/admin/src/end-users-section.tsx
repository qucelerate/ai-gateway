import React, { useCallback, useEffect, useState } from 'react';
import {
  api,
  newIdempotencyKey,
  type ApiProblem,
  type CreditGrant,
  type EndUser,
  type EndUserPlan,
  type EndUserPlanPeriod,
  type EndUserRequirement,
  type EndUserSettings,
  type ServiceAccount,
} from './api';
import { formatCount, formatMoney, parseMoney, toMajor } from './money';
import { useCollection } from './use-collection';
import { useMutationState } from './mutation-state';
import { emptyView, writeView } from './table-view';
import {
  AppLink,
  Button,
  ConfirmDialog,
  DataCell,
  DataGrid,
  Dialog,
  EmptyState,
  FormField,
  InlineError,
  LoadMore,
  MutationStatus,
  Select,
  TableBody,
  TableHead,
  TableHeader,
  TableRow,
  TextInput,
  formatDate,
} from './ui';

/** The default plan choice's value in the plan selects. */
const DEFAULT = '';

/** How a plan's allowance repeats, in words. */
export function periodLabel(plan: Pick<EndUserPlan, 'period' | 'period_days'>): string {
  switch (plan.period) {
    case 'weekly':
      return 'Weekly';
    case 'daily':
      return 'Daily';
    case 'days':
      return plan.period_days === 1 ? 'Every day' : `Every ${plan.period_days ?? '?'} days`;
    default:
      return 'Monthly';
  }
}

/** A plan's per-user rate limits, in words. */
export function limitsLabel(plan: Pick<EndUserPlan, 'requests_per_minute' | 'tokens_per_minute' | 'max_concurrent'>): string {
  const parts = [
    plan.requests_per_minute !== null ? `${formatCount(plan.requests_per_minute)} requests/min` : null,
    plan.tokens_per_minute !== null ? `${formatCount(plan.tokens_per_minute)} tokens/min` : null,
    plan.max_concurrent !== null ? `${formatCount(plan.max_concurrent)} in flight` : null,
  ].filter(Boolean);
  return parts.length ? parts.join(', ') : 'None';
}

/** A whole number from 1 to `max`, or null for an empty field; undefined when invalid. */
function parseLimit(value: string, max: number): number | null | undefined {
  const trimmed = value.trim();
  if (!trimmed) return null;
  if (!/^\d+$/.test(trimmed)) return undefined;
  const parsed = Number(trimmed);
  return parsed >= 1 && parsed <= max ? parsed : undefined;
}

/**
 * Budgets per end user of an app (D-028): the service account's settings and
 * plans, its end users with what they have left, and their credits.
 */
/** The Traces page showing one end user's requests, with its column. */
export function endUserTraces(accountId: string, endUser: string): string {
  return `/admin/traces${writeView('', 'traces', {
    ...emptyView,
    categories: { service_account: [accountId], end_user: [endUser] },
    columns: ['recorded', 'model', 'end_user', 'status', 'duration', 'amount'],
  })}`;
}

export function EndUsersSection({ organizationId, account }: { organizationId: string; account: ServiceAccount }) {
  const [settings, setSettings] = useState<EndUserSettings | null>(null);
  const [settingsLoaded, setSettingsLoaded] = useState(false);
  const [settingsError, setSettingsError] = useState<ApiProblem | null>(null);
  const [plans, setPlans] = useState<EndUserPlan[]>([]);
  const mutation = useMutationState<unknown>();

  const loadSettings = useCallback(async () => {
    setSettingsError(null);
    try {
      const loaded = await api.getEndUserSettings(organizationId, account.id);
      setSettings(loaded);
      setPlans(await api.listEndUserPlans(organizationId, account.id));
    } catch (caught) {
      const status = (caught as { status?: number }).status;
      if (status === 404) {
        setSettings(null);
        setPlans([]);
      } else {
        setSettingsError((caught as { problem?: ApiProblem }).problem ?? null);
      }
    } finally {
      setSettingsLoaded(true);
    }
  }, [organizationId, account.id]);

  useEffect(() => {
    setSettingsLoaded(false);
    void loadSettings();
  }, [loadSettings]);

  const users = useCollection(
    useCallback(params => api.listEndUsers(organizationId, account.id, params), [organizationId, account.id]),
    [organizationId, account.id],
  );
  const refresh = () => {
    void loadSettings();
    users.load(true);
  };

  return (
    <div className="end-users-section">
      <MutationStatus result={mutation.result} problem={mutation.problem} onDismiss={mutation.clearProblem} />
      <p className="meta">
        The app names its user on each request with the <code>Gateway-End-User</code> header. Each user spends their
        plan's allowance for the period (a month, week, day or number of days), then credits, soonest-expiring first,
        within the plan's rate limits.
      </p>
      {!settingsLoaded && <p role="status">Loading end-user settings…</p>}
      <InlineError problem={settingsError} />
      {settingsLoaded && !settingsError && (
        <SettingsForm
          organizationId={organizationId}
          accountId={account.id}
          settings={settings}
          plans={plans}
          mutation={mutation}
          onSaved={refresh}
        />
      )}
      {settings && (
        <PlansTable
          organizationId={organizationId}
          accountId={account.id}
          settings={settings}
          plans={plans}
          mutation={mutation}
          onChanged={refresh}
        />
      )}
      <UsersTable
        organizationId={organizationId}
        accountId={account.id}
        settings={settings}
        plans={plans}
        users={users}
        mutation={mutation}
        onChanged={refresh}
      />
    </div>
  );
}

type Mutation = ReturnType<typeof useMutationState<unknown>>;

function SettingsForm({
  organizationId,
  accountId,
  settings,
  plans,
  mutation,
  onSaved,
}: {
  organizationId: string;
  accountId: string;
  settings: EndUserSettings | null;
  plans: EndUserPlan[];
  mutation: Mutation;
  onSaved: () => void;
}) {
  const [currency, setCurrency] = useState(settings?.currency ?? 'USD');
  const [requirement, setRequirement] = useState<EndUserRequirement>(settings?.requirement ?? 'optional');
  const [defaultPlan, setDefaultPlan] = useState(settings?.default_plan ?? DEFAULT);
  useEffect(() => {
    setCurrency(settings?.currency ?? 'USD');
    setRequirement(settings?.requirement ?? 'optional');
    setDefaultPlan(settings?.default_plan ?? DEFAULT);
  }, [settings]);

  const save = async () => {
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.setEndUserSettings(
        organizationId,
        accountId,
        { currency: currency.trim().toUpperCase(), requirement, default_plan: defaultPlan || null },
        settings?.version ?? null,
        newIdempotencyKey(),
      );
      return { data, requestId: meta.requestId };
    });
    if (ok) onSaved();
  };

  return (
    <form
      className="end-user-settings"
      aria-label="End-user settings"
      onSubmit={event => {
        event.preventDefault();
        void save();
      }}
    >
      <h4>{settings ? 'End-user settings' : 'Limit this app’s end users'}</h4>
      {!settings && (
        <p className="meta">Requests are recorded per end user but not limited until settings and a plan exist.</p>
      )}
      <FormField label="Currency" htmlFor="end-user-currency" hint="Fixed once a plan or credit grant exists.">
        <TextInput id="end-user-currency" value={currency} maxLength={3} onChange={event => setCurrency(event.target.value)} required />
      </FormField>
      <FormField label="Requests must name an end user" htmlFor="end-user-requirement">
        <Select id="end-user-requirement" value={requirement} onChange={event => setRequirement(event.target.value as EndUserRequirement)}>
          <option value="optional">Optional</option>
          <option value="required">Required: refuse requests without one</option>
        </Select>
      </FormField>
      {settings && (
        <FormField label="Default plan" htmlFor="end-user-default-plan" hint="For users never assigned one; none refuses them.">
          <Select id="end-user-default-plan" value={defaultPlan} onChange={event => setDefaultPlan(event.target.value)}>
            <option value={DEFAULT}>None</option>
            {plans.map(plan => (
              <option key={plan.name} value={plan.name}>{plan.name}</option>
            ))}
          </Select>
        </FormField>
      )}
      <Button type="submit" loading={mutation.submitting}>{settings ? 'Save settings' : 'Turn on'}</Button>
    </form>
  );
}

function PlansTable({
  organizationId,
  accountId,
  settings,
  plans,
  mutation,
  onChanged,
}: {
  organizationId: string;
  accountId: string;
  settings: EndUserSettings;
  plans: EndUserPlan[];
  mutation: Mutation;
  onChanged: () => void;
}) {
  const [editing, setEditing] = useState<{ plan: EndUserPlan | null } | null>(null);
  const [name, setName] = useState('');
  const [allowance, setAllowance] = useState('');
  const [period, setPeriod] = useState<EndUserPlanPeriod>('monthly');
  const [periodDays, setPeriodDays] = useState('');
  const [requestsPerMinute, setRequestsPerMinute] = useState('');
  const [tokensPerMinute, setTokensPerMinute] = useState('');
  const [maxConcurrent, setMaxConcurrent] = useState('');
  const [deleting, setDeleting] = useState<EndUserPlan | null>(null);
  const amount = parseMoney(allowance, settings.currency);
  const days = period === 'days' ? parseLimit(periodDays, 366) : null;
  const rpm = parseLimit(requestsPerMinute, 1_000_000);
  const tpm = parseLimit(tokensPerMinute, 1_000_000_000);
  const concurrent = parseLimit(maxConcurrent, 1_000_000);
  const valid = amount !== null && days !== undefined && (period !== 'days' || days !== null) && rpm !== undefined && tpm !== undefined && concurrent !== undefined;

  const open = (plan: EndUserPlan | null) => {
    setName(plan?.name ?? '');
    setAllowance(plan ? toMajor(plan.allowance_minor, settings.currency) : '');
    setPeriod(plan?.period ?? 'monthly');
    setPeriodDays(plan?.period_days ? String(plan.period_days) : '');
    setRequestsPerMinute(plan?.requests_per_minute ? String(plan.requests_per_minute) : '');
    setTokensPerMinute(plan?.tokens_per_minute ? String(plan.tokens_per_minute) : '');
    setMaxConcurrent(plan?.max_concurrent ? String(plan.max_concurrent) : '');
    setEditing({ plan });
  };
  const save = async () => {
    if (!editing || !valid || amount === null) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.setEndUserPlan(
        organizationId,
        accountId,
        name.trim(),
        {
          allowance_minor: amount,
          period,
          period_days: period === 'days' ? (days ?? null) : null,
          requests_per_minute: rpm ?? null,
          tokens_per_minute: tpm ?? null,
          max_concurrent: concurrent ?? null,
        },
        editing.plan?.version ?? null,
        newIdempotencyKey(),
      );
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setEditing(null);
    onChanged();
  };
  const remove = async () => {
    if (!deleting) return;
    const ok = await mutation.submit(async () => {
      const { meta } = await api.deleteEndUserPlan(organizationId, accountId, deleting, newIdempotencyKey());
      return { data: undefined, requestId: meta.requestId };
    });
    setDeleting(null);
    if (ok) onChanged();
  };

  return (
    <>
      <div className="section-header">
        <h4>Plans</h4>
      </div>
      {plans.length === 0 ? (
        <EmptyState title="No plans" text="Add a plan: an allowance per user for each month, week, day or number of days (0 for credits only), and optional rate limits per user." action={<Button onClick={() => open(null)}>Add plan</Button>} />
      ) : (
        <>
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Plan actions"><Button onClick={() => open(null)}>Add plan</Button></div>
        <DataGrid aria-label="End-user plans">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Plan</TableHeader>
              <TableHeader>Allowance</TableHeader>
              <TableHeader>Rate limits per user</TableHeader>
              <TableHeader>Users</TableHeader>
              <TableHeader>Spent this period</TableHeader>
              <TableHeader>Updated</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {plans.map(plan => (
              <TableRow key={plan.name} className="data-row">
                <DataCell>
                  {plan.name}
                  {settings.default_plan === plan.name && <span className="meta"> (default)</span>}
                </DataCell>
                <DataCell>
                  {formatMoney(plan.allowance_minor, plan.currency)} <span className="meta">{periodLabel(plan).toLowerCase()}</span>
                </DataCell>
                <DataCell>{limitsLabel(plan)}</DataCell>
                <DataCell>{formatCount(plan.users)}</DataCell>
                <DataCell>
                  <span title={`${formatDate(plan.period_start)} to ${formatDate(plan.period_end)}`}>
                    {formatMoney(plan.spent_minor, plan.currency)}
                  </span>
                </DataCell>
                <DataCell>{formatDate(plan.updated_at)}</DataCell>
                <DataCell label="Actions" className="row-actions">
                  <Button variant="ghost" onClick={() => open(plan)}>Edit</Button>
                  <Button variant="danger" onClick={() => setDeleting(plan)}>Delete</Button>
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
        </>
      )}
      <Dialog open={editing !== null} onClose={() => { if (!mutation.submitting) setEditing(null); }} title={editing?.plan ? `Edit plan ${editing.plan.name}` : 'Add plan'}>
        <form
          onSubmit={event => {
            event.preventDefault();
            void save();
          }}
        >
          <FormField label="Name" htmlFor="plan-name" hint="a-z, 0-9, _ and -">
            <TextInput id="plan-name" value={name} disabled={Boolean(editing?.plan)} onChange={event => setName(event.target.value)} required />
          </FormField>
          <FormField
            label={`Allowance per period (${settings.currency})`}
            htmlFor="plan-allowance"
            hint="0 for a credits-only plan."
            error={allowance && amount === null ? 'Enter an amount, such as 5.00' : undefined}
          >
            <TextInput id="plan-allowance" inputMode="decimal" value={allowance} onChange={event => setAllowance(event.target.value)} required />
          </FormField>
          <FormField label="Period" htmlFor="plan-period" hint="In UTC. Weeks start on Monday.">
            <Select id="plan-period" value={period} onChange={event => setPeriod(event.target.value as EndUserPlanPeriod)}>
              <option value="monthly">Monthly</option>
              <option value="weekly">Weekly</option>
              <option value="daily">Daily</option>
              <option value="days">Every number of days</option>
            </Select>
          </FormField>
          {period === 'days' && (
            <FormField
              label="Days per period"
              htmlFor="plan-period-days"
              hint="1 to 366. Periods are shared by every user of the plan."
              error={periodDays && days === undefined ? 'Enter 1 to 366' : undefined}
            >
              <TextInput id="plan-period-days" inputMode="numeric" value={periodDays} onChange={event => setPeriodDays(event.target.value)} required />
            </FormField>
          )}
          <FormField
            label="Requests per minute, per user (optional)"
            htmlFor="plan-requests-per-minute"
            hint="Empty for no limit. A batch counts as one request."
            error={rpm === undefined ? 'Enter 1 to 1,000,000, or leave empty' : undefined}
          >
            <TextInput id="plan-requests-per-minute" inputMode="numeric" value={requestsPerMinute} onChange={event => setRequestsPerMinute(event.target.value)} />
          </FormField>
          <FormField
            label="Tokens per minute, per user (optional)"
            htmlFor="plan-tokens-per-minute"
            hint="Empty for no limit. A request's output is capped to what fits."
            error={tpm === undefined ? 'Enter 1 to 1,000,000,000, or leave empty' : undefined}
          >
            <TextInput id="plan-tokens-per-minute" inputMode="numeric" value={tokensPerMinute} onChange={event => setTokensPerMinute(event.target.value)} />
          </FormField>
          <FormField
            label="Requests in flight per user"
            htmlFor="plan-max-concurrent"
            hint="Empty for no limit. At once; batches are not counted."
            error={concurrent === undefined ? 'Enter 1 to 1,000,000, or leave empty' : undefined}
          >
            <TextInput id="plan-max-concurrent" inputMode="numeric" value={maxConcurrent} onChange={event => setMaxConcurrent(event.target.value)} />
          </FormField>
          <p className="meta">
            A change applies from each user's next request and keeps what this period has spent; a new period starts its
            allowance afresh.
          </p>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setEditing(null)} disabled={mutation.submitting}>Cancel</Button>
            <Button type="submit" loading={mutation.submitting} disabled={!valid}>Save</Button>
          </div>
        </form>
      </Dialog>
      <ConfirmDialog
        open={deleting !== null}
        title={`Delete plan ${deleting?.name ?? ''}`}
        confirmText="Delete"
        onClose={() => setDeleting(null)}
        onConfirm={() => void remove()}
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        A plan that is the default or that any user is on is kept.
      </ConfirmDialog>
    </>
  );
}

function UsersTable({
  organizationId,
  accountId,
  settings,
  plans,
  users,
  mutation,
  onChanged,
}: {
  organizationId: string;
  accountId: string;
  settings: EndUserSettings | null;
  plans: EndUserPlan[];
  users: ReturnType<typeof useCollection<EndUser>>;
  mutation: Mutation;
  onChanged: () => void;
}) {
  const [assigning, setAssigning] = useState<EndUser | null>(null);
  const [plan, setPlan] = useState(DEFAULT);
  const [credits, setCredits] = useState<EndUser | null>(null);
  const [adding, setAdding] = useState(false);
  const [newId, setNewId] = useState('');
  const [newPlan, setNewPlan] = useState(DEFAULT);

  const add = async () => {
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.addEndUser(organizationId, accountId, newId.trim(), newPlan || null, newIdempotencyKey());
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setAdding(false);
    onChanged();
  };

  const assign = async () => {
    if (!assigning) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.assignEndUserPlan(organizationId, accountId, assigning, plan || null, newIdempotencyKey());
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setAssigning(null);
    onChanged();
  };
  const money = (amount: number, user: EndUser) => (user.currency ? formatMoney(amount, user.currency) : '—');
  const addUser = settings
    ? <Button onClick={() => { setNewId(''); setNewPlan(DEFAULT); setAdding(true); }}>Add end user</Button>
    : null;

  return (
    <>
      <div className="section-header">
        <h4>End users</h4>
      </div>
      {users.loading && <p role="status">Loading end users…</p>}
      <InlineError problem={users.error} />
      {!users.loading && users.items.length === 0 && (
        <EmptyState title="No end users yet" text="An end user appears with their first request that names them, or when a plan or credits are given to them." action={addUser} />
      )}
      {users.items.length > 0 && addUser && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="End user actions">{addUser}</div>
      )}
      {users.items.length > 0 && (
        <DataGrid aria-label="End users">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>User</TableHeader>
              <TableHeader>Plan</TableHeader>
              <TableHeader>Allowance left this period</TableHeader>
              <TableHeader>Credits</TableHeader>
              <TableHeader>Spent this period</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {users.items.map(user => (
              <TableRow key={user.id} className="data-row">
                <DataCell>{user.id}</DataCell>
                <DataCell>
                  {user.effective_plan ?? <span className="meta">none: refused</span>}
                  {user.plan === null && user.effective_plan && <span className="meta"> (default)</span>}
                </DataCell>
                <DataCell>
                  <span title={`${formatDate(user.period_start)} to ${formatDate(user.period_end)}`}>
                    {money(user.allowance_remaining_minor, user)} of {money(user.allowance_minor, user)}
                  </span>
                </DataCell>
                <DataCell>{money(user.credit_balance_minor, user)}</DataCell>
                <DataCell>
                  {user.spent_minor === null
                    ? <span className="meta">several currencies</span>
                    : user.spent_currency ? formatMoney(user.spent_minor, user.spent_currency) : '—'}
                </DataCell>
                <DataCell label="Actions" className="row-actions">
                  {settings && (
                    <Button variant="ghost" onClick={() => { setPlan(user.plan ?? DEFAULT); setAssigning(user); }}>Change plan</Button>
                  )}
                  {settings && <Button variant="ghost" onClick={() => setCredits(user)}>Credits</Button>}
                  <AppLink to={endUserTraces(accountId, user.id)} label={`Traces of ${user.id}`}>Traces</AppLink>
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore hasMore={users.hasMore} loading={users.loadingMore} onClick={users.loadMore} />
      <Dialog open={adding} onClose={() => { if (!mutation.submitting) setAdding(false); }} title="Add end user">
        <form
          onSubmit={event => {
            event.preventDefault();
            void add();
          }}
        >
          <FormField label="End-user ID" htmlFor="new-end-user-id" hint="The app's ID for the user, as it sends it in Gateway-End-User.">
            <TextInput id="new-end-user-id" value={newId} maxLength={256} onChange={event => setNewId(event.target.value)} required />
          </FormField>
          <FormField label="Plan" htmlFor="new-end-user-plan">
            <Select id="new-end-user-plan" value={newPlan} onChange={event => setNewPlan(event.target.value)}>
              <option value={DEFAULT}>Default plan{settings?.default_plan ? ` (${settings.default_plan})` : ''}</option>
              {plans.map(item => (
                <option key={item.name} value={item.name}>{item.name}</option>
              ))}
            </Select>
          </FormField>
          <p className="meta">A user the app has not sent yet can be given a plan or credits ahead of their first request.</p>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setAdding(false)} disabled={mutation.submitting}>Cancel</Button>
            <Button type="submit" loading={mutation.submitting}>Add</Button>
          </div>
        </form>
      </Dialog>
      <Dialog open={assigning !== null} onClose={() => { if (!mutation.submitting) setAssigning(null); }} title={`Plan of ${assigning?.id ?? ''}`}>
        <form
          onSubmit={event => {
            event.preventDefault();
            void assign();
          }}
        >
          <FormField label="Plan" htmlFor="end-user-plan">
            <Select id="end-user-plan" value={plan} onChange={event => setPlan(event.target.value)}>
              <option value={DEFAULT}>Default plan{settings?.default_plan ? ` (${settings.default_plan})` : ''}</option>
              {plans.map(item => (
                <option key={item.name} value={item.name}>{item.name}</option>
              ))}
            </Select>
          </FormField>
          <p className="meta">Applies from the user's next request; this period's spend is kept.</p>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setAssigning(null)} disabled={mutation.submitting}>Cancel</Button>
            <Button type="submit" loading={mutation.submitting}>Save</Button>
          </div>
        </form>
      </Dialog>
      {credits && settings && (
        <CreditsPanel
          key={credits.id}
          organizationId={organizationId}
          accountId={accountId}
          user={credits}
          currency={settings.currency}
          mutation={mutation}
          onChanged={onChanged}
          onClose={() => setCredits(null)}
        />
      )}
    </>
  );
}

function CreditsPanel({
  organizationId,
  accountId,
  user,
  currency,
  mutation,
  onChanged,
  onClose,
}: {
  organizationId: string;
  accountId: string;
  user: EndUser;
  currency: string;
  mutation: Mutation;
  onChanged: () => void;
  onClose: () => void;
}) {
  const [grants, setGrants] = useState<CreditGrant[] | null>(null);
  const [error, setError] = useState<ApiProblem | null>(null);
  const [granting, setGranting] = useState(false);
  const [reference, setReference] = useState('');
  const [amount, setAmount] = useState('');
  const [expires, setExpires] = useState('');
  const [reversing, setReversing] = useState<CreditGrant | null>(null);
  const [reason, setReason] = useState('');
  const parsed = parseMoney(amount, currency);

  const load = useCallback(async () => {
    try {
      setGrants(await api.listCreditGrants(organizationId, accountId, user.id));
      setError(null);
    } catch (caught) {
      setError((caught as { problem?: ApiProblem }).problem ?? null);
      setGrants([]);
    }
  }, [organizationId, accountId, user.id]);
  useEffect(() => {
    void load();
  }, [load]);

  const changed = () => {
    void load();
    onChanged();
  };
  const grant = async () => {
    if (parsed === null) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.grantCredits(organizationId, accountId, user.id, {
        reference: reference.trim(),
        amount_minor: parsed,
        expires_at: expires ? new Date(expires).toISOString() : null,
      });
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setGranting(false);
    changed();
  };
  const reverse = async () => {
    if (!reversing || parsed === null) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.reverseCredits(organizationId, accountId, user.id, reversing.id, {
        reference: reference.trim(),
        amount_minor: parsed,
        reason: reason.trim(),
      });
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setReversing(null);
    changed();
  };
  const startGrant = () => {
    setReference('');
    setAmount('');
    setExpires('');
    setGranting(true);
  };
  const startReverse = (item: CreditGrant) => {
    setReference('');
    setAmount(toMajor(item.remaining_minor, currency));
    setReason('');
    setReversing(item);
  };

  return (
    <section className="credits-panel" aria-label={`Credits of ${user.id}`}>
      <div className="section-header">
        <h4>Credits of {user.id}</h4>
        <Button variant="ghost" onClick={onClose}>Close</Button>
      </div>
      <InlineError problem={error} />
      {grants === null && <p role="status">Loading credits…</p>}
      {grants?.length === 0 && <EmptyState title="No credits" text="Grant credits when the user buys them; the purchase reference makes a retry grant once." action={<Button onClick={startGrant}>Grant credits</Button>} />}
      {grants && grants.length > 0 && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Credit actions"><Button onClick={startGrant}>Grant credits</Button></div>
      )}
      {grants && grants.length > 0 && (
        <DataGrid aria-label={`Credit grants of ${user.id}`}>
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Reference</TableHeader>
              <TableHeader>Granted</TableHeader>
              <TableHeader>Left</TableHeader>
              <TableHeader>Spent</TableHeader>
              <TableHeader>Reversed</TableHeader>
              <TableHeader>Expires</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {grants.map(item => (
              <TableRow key={item.id} className="data-row">
                <DataCell>{item.reference}</DataCell>
                <DataCell>{formatMoney(item.amount_minor, item.currency)} · {formatDate(item.granted_at)}</DataCell>
                <DataCell>{formatMoney(item.remaining_minor, item.currency)}</DataCell>
                <DataCell>{formatMoney(item.spent_minor, item.currency)}</DataCell>
                <DataCell>{formatMoney(item.reversed_minor, item.currency)}</DataCell>
                <DataCell>{item.expires_at ? `${formatDate(item.expires_at)}${item.expired ? ' (expired)' : ''}` : 'Never'}</DataCell>
                <DataCell label="Actions" className="row-actions">
                  {item.remaining_minor > 0 && <Button variant="ghost" onClick={() => startReverse(item)}>Reverse</Button>}
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <Dialog open={granting} onClose={() => { if (!mutation.submitting) setGranting(false); }} title={`Grant credits to ${user.id}`}>
        <form
          onSubmit={event => {
            event.preventDefault();
            void grant();
          }}
        >
          <FormField label="Purchase reference" htmlFor="grant-reference" hint="The app's order ID: the same reference grants once.">
            <TextInput id="grant-reference" value={reference} maxLength={255} onChange={event => setReference(event.target.value)} required />
          </FormField>
          <FormField label={`Amount (${currency})`} htmlFor="grant-amount" error={amount && parsed === null ? 'Enter an amount, such as 10.00' : undefined}>
            <TextInput id="grant-amount" inputMode="decimal" value={amount} onChange={event => setAmount(event.target.value)} required />
          </FormField>
          <FormField label="Expires (optional)" htmlFor="grant-expires">
            <TextInput id="grant-expires" type="datetime-local" value={expires} onChange={event => setExpires(event.target.value)} />
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setGranting(false)} disabled={mutation.submitting}>Cancel</Button>
            <Button type="submit" loading={mutation.submitting} disabled={parsed === null || parsed === 0}>Grant</Button>
          </div>
        </form>
      </Dialog>
      <Dialog open={reversing !== null} onClose={() => { if (!mutation.submitting) setReversing(null); }} title={`Reverse ${reversing?.reference ?? ''}`}>
        <form
          onSubmit={event => {
            event.preventDefault();
            void reverse();
          }}
        >
          <FormField label="Refund reference" htmlFor="reversal-reference" hint="The app's refund or chargeback ID.">
            <TextInput id="reversal-reference" value={reference} maxLength={255} onChange={event => setReference(event.target.value)} required />
          </FormField>
          <FormField label={`Amount (${currency})`} htmlFor="reversal-amount" hint="At most what is unspent is taken back.">
            <TextInput id="reversal-amount" inputMode="decimal" value={amount} onChange={event => setAmount(event.target.value)} required />
          </FormField>
          <FormField label="Reason" htmlFor="reversal-reason">
            <TextInput id="reversal-reason" value={reason} maxLength={500} onChange={event => setReason(event.target.value)} required />
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setReversing(null)} disabled={mutation.submitting}>Cancel</Button>
            <Button type="submit" variant="danger" loading={mutation.submitting} disabled={parsed === null || parsed === 0}>Reverse</Button>
          </div>
        </form>
      </Dialog>
    </section>
  );
}
