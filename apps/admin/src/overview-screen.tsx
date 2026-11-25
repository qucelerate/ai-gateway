import React, { useEffect, useState } from 'react';
import { allPages, api, type ApiKey, type ApiProblem, type AuditEvent, type BudgetPolicy, type CatalogRelease, type ProviderAccount, type ReleasePage, type ServiceAccount, type UsageAggregate } from './api';
import { PriceChanges } from './catalog-actions';
import { formatCount, formatMoney } from './money';
import { reportWindow } from './usage-screen';
import { budgetActive } from './budget-coverage';
import { actionLabel } from './audit-screen';
import { SessionExpired, type SignIn } from './screen-feedback';
import { RecentNotifications } from './notifications';
import { AppLink, CopyButton, EmptyState, Message, formatRelativeDate } from './ui';

type Destination = 'usage' | 'models' | 'audit' | 'policies';

/** "Good morning, Ada": the first word of the person's name, by local time. */
export function greeting(name: string | null | undefined, now = new Date()): string {
  const hour = now.getHours();
  const part = hour < 12 ? 'Good morning' : hour < 18 ? 'Good afternoon' : 'Good evening';
  const first = name?.trim().split(/\s+/)[0];
  return first ? `${part}, ${first}.` : `${part}.`;
}

/** Spend per currency over every confidence, largest first. */
export function spendByCurrency(totals: UsageAggregate[]): { currency: string; amount: number; requests: number; estimated: boolean }[] {
  const byCurrency = new Map<string, { currency: string; amount: number; requests: number; estimated: boolean }>();
  for (const total of totals) {
    const entry = byCurrency.get(total.currency) ?? { currency: total.currency, amount: 0, requests: 0, estimated: false };
    entry.amount += total.amount_minor;
    entry.requests += total.request_count;
    entry.estimated ||= total.confidence !== 'exact' || total.unpriced_request_count > 0 || total.incomplete_request_count > 0;
    byCurrency.set(total.currency, entry);
  }
  return Array.from(byCurrency.values()).sort((a, b) => b.amount - a.amount);
}

/** A card's data: loading, loaded, or why it is unavailable; `retry` loads it again. */
type LoadState<T> = { state: 'loading' } | { state: 'ready'; value: T } | { state: 'failed'; status: number };
type Load<T> = LoadState<T> & { retry: () => void };

function useLoad<T>(load: () => Promise<T>, deps: unknown[]): Load<T> {
  const [result, setResult] = useState<LoadState<T>>({ state: 'loading' });
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setResult({ state: 'loading' });
    load().then(
      value => { if (current) setResult({ state: 'ready', value }); },
      caught => { if (current) setResult({ state: 'failed', status: (caught as { problem?: ApiProblem; status?: number }).problem?.status ?? (caught as { status?: number }).status ?? 0 }); },
    );
    return () => { current = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, attempt]);
  return { ...result, retry: () => setAttempt(value => value + 1) } as Load<T>;
}

/** The active release with every one of its provider accounts. The accounts
 * name the release they were read from, so the version shown is that one's. */
export async function activeCatalog(organizationId: string): Promise<{ releaseId: string; version: string; accounts: number; deployments: number } | null> {
  // Later pages name the first page's release, so activating another release
  // meanwhile cannot invalidate them.
  let releaseId: string | null = null;
  const accounts = await allPages<ProviderAccount, ReleasePage<ProviderAccount>>(async token => {
    const page = await api.listProviderAccounts(organizationId, { page_size: 100, page_token: token }, token ? releaseId ?? undefined : undefined);
    if (!token) releaseId = page.release_id;
    return page;
  });
  if (!releaseId) return null;
  let release: CatalogRelease | undefined;
  let token: string | undefined;
  do {
    const page = await api.listReleases(organizationId, { page_size: 100, page_token: token });
    release = page.items.find(item => item.id === releaseId);
    token = page.next_page_token ?? undefined;
  } while (!release && token);
  return {
    releaseId,
    version: release?.version ?? releaseId,
    accounts: accounts.items.length,
    deployments: accounts.items.reduce((sum, account) => sum + account.deployment_count, 0),
  };
}

export function usableKey(key: Pick<ApiKey, 'status' | 'expires_at'>, now = Date.now()): boolean {
  return key.status === 'active' && (!key.expires_at || Date.parse(key.expires_at) > now);
}

/** Whether an enabled service account holds a key that can call the gateway
 * now (active, unexpired). Stops at the first one found. */
export async function hasUsableKey(organizationId: string, now = Date.now()): Promise<boolean> {
  let token: string | undefined;
  do {
    const page = await api.listServiceAccounts(organizationId, { page_size: 100, page_token: token });
    for (const account of (page.items as ServiceAccount[]).filter(item => item.enabled)) {
      const keys = await allPages<ApiKey, Awaited<ReturnType<typeof api.listApiKeys>>>(next => api.listApiKeys(organizationId, account.id, { page_size: 100, page_token: next }));
      if (keys.items.some(key => usableKey(key, now))) return true;
    }
    token = page.next_page_token ?? undefined;
  } while (token);
  return false;
}

/** The longest window the usage report takes. */
const YEAR_DAYS = 366;

/** Whether any request has been served in the last year (the longest window
 * the usage report allows), so the first-request step stays done after a
 * quiet month. Usage, not key use: a refused request records no usage. */
export async function servedThisYear(organizationId: string, now = new Date()): Promise<boolean> {
  const from = new Date(now.getTime() - YEAR_DAYS * 86_400_000).toISOString();
  const report = await api.usageReport(organizationId, { from, to: now.toISOString(), group_by: 'day' }, { page_size: 1 });
  return report.totals.some(total => total.request_count > 0);
}

/** A setup step: done, still to do, or unknown (it could not be read). */
export type StepState = 'done' | 'todo' | 'unknown';
export type SetupStep = { key: string; label: string; detail: string; to?: string; state: StepState };

function stepState<T>(load: LoadState<T>, done: (value: T) => boolean): StepState | null {
  if (load.state === 'loading') return null;
  if (load.state === 'failed') return 'unknown';
  return done(load.value) ? 'done' : 'todo';
}

/** The steps from a new organization to its first request, in order; null
 * while any is still loading. */
export function setupSteps(loads: {
  catalog: LoadState<{ deployments: number } | null>;
  budgets: LoadState<BudgetPolicy[]>;
  key: LoadState<boolean>;
  served: LoadState<boolean>;
}): SetupStep[] | null {
  const states = [
    stepState(loads.catalog, value => !!value && value.deployments > 0),
    stepState(loads.budgets, value => value.some(budget => budgetActive(budget))),
    stepState(loads.key, value => value),
    stepState(loads.served, value => value),
  ];
  if (states.some(state => state === null)) return null;
  const [catalog, budget, key, first] = states as StepState[];
  return [
    { key: 'catalog', label: 'Connect a provider and add a model', detail: 'The gateway calls models in your own provider and cloud accounts.', to: '/admin/connections/connect', state: catalog },
    { key: 'budget', label: 'Add a budget', detail: 'Every request is charged against a budget; with none, requests are refused.', to: '/admin/policies/budgets', state: budget },
    { key: 'key', label: 'Create an API key', detail: 'Applications call the gateway with a service account’s key.', to: '/admin/identities', state: key },
    { key: 'request', label: 'Send a first request', detail: 'Call a model by its alias with the key, as below; its usage shows here.', to: '/admin/models/aliases', state: first },
  ];
}

/** A request a new administrator can copy, with the alias and key to fill in. */
export function exampleRequest(origin: string): string {
  return [
    `curl ${origin}/api/inference/v1/chat/completions \\`,
    '  -H "Authorization: Bearer $GATEWAY_API_KEY" \\',
    '  -H "Idempotency-Key: $(uuidgen)" \\',
    '  -H "Content-Type: application/json" \\',
    `  -d '{"model": "YOUR_ALIAS", "messages": [{"role": "user", "content": "Hello"}]}'`,
  ].join('\n');
}

function ExampleRequest() {
  const text = exampleRequest(window.location.origin);
  return (
    <details className="setup-example">
      <summary>Example request</summary>
      <p className="muted">Replace <code>YOUR_ALIAS</code> with an alias from Models → Aliases, and set <code>GATEWAY_API_KEY</code> to the key’s secret.</p>
      <pre><code>{text}</code></pre>
      <CopyButton text={text} label="Copy request" />
    </details>
  );
}

function SetupChecklist({ steps }: { steps: SetupStep[] }) {
  const done = steps.filter(step => step.state === 'done').length;
  return (
    <section className="cds--tile card overview-card wide setup-checklist" aria-labelledby="setup-heading">
      <div className="card-top"><span id="setup-heading">Get started</span><span className="muted">{done} of {steps.length} done</span></div>
      <ol>
        {steps.map(step => (
          <li key={step.key} className={`setup-step ${step.state}`}>
            <span className="setup-mark" aria-hidden="true">{step.state === 'done' ? '✓' : step.state === 'unknown' ? '?' : '○'}</span>
            <span className="setup-text">
              <strong>{step.label}</strong>
              <span className="sr-only">{step.state === 'done' ? ' (done)' : step.state === 'unknown' ? ' (could not be checked)' : ' (to do)'}</span>
              <span className="muted">{step.detail}</span>
            </span>
            {step.state !== 'done' && step.to && <AppLink to={step.to}>{step.key === 'request' ? 'See aliases' : step.state === 'todo' ? 'Start' : 'Open'}<span className="sr-only">: {step.label}</span> →</AppLink>}
            {step.key === 'request' && step.state !== 'done' && <ExampleRequest />}
          </li>
        ))}
      </ol>
    </section>
  );
}

type CardsProps = SignIn & { organizationId: string; onNavigate: (page: Destination, tab?: string) => void };

export function OverviewScreen(props: CardsProps) {
  const { organizationId } = props;
  const me = useLoad(() => api.me(), []);
  const name = me.state === 'ready' ? (me.value as { display_name?: string | null }).display_name : null;
  return (
    <div className="screen overview">
      <h2 className="overview-greeting">{greeting(name)}</h2>
      {organizationId ? <OverviewCards {...props} /> : <EmptyState title="Waiting for organization context" text="Select a confirmed organization to see its summary." />}
    </div>
  );
}

function OverviewCards({ organizationId, onNavigate, ...signIn }: CardsProps) {
  const usage = useLoad(() => api.usageReport(organizationId, { ...reportWindow('30'), group_by: 'day' }, { page_size: 1 }), [organizationId]);
  const reconciliation = useLoad(() => api.listReconciliationItems(organizationId, 'open', { page_size: 100 }), [organizationId]);
  const catalog = useLoad(() => activeCatalog(organizationId), [organizationId]);
  const card = { onNavigate, signIn };
  const activity = useLoad(() => api.listAuditEvents(organizationId, { page_size: 5, exclude_views: 'true' }), [organizationId]);
  const budgets = useLoad(async () => (await allPages<BudgetPolicy, Awaited<ReturnType<typeof api.listBudgets>>>(token => api.listBudgets(organizationId, { page_size: 100, page_token: token }))).items, [organizationId]);
  const key = useLoad(() => hasUsableKey(organizationId), [organizationId]);
  const served = useLoad(() => servedThisYear(organizationId), [organizationId]);
  const steps = setupSteps({ catalog, budgets, key, served });
  const noBudget = budgets.state === 'ready' && !budgets.value.some(budget => budgetActive(budget));
  const [adopted, setAdopted] = useState<string | null>(null);
  return (
    <div className="overview-grid">
      {steps && steps.some(step => step.state !== 'done') && <SetupChecklist steps={steps} />}
      {adopted && <div className="overview-card wide"><Message kind="success">{adopted}</Message></div>}
      {/* Only when a billed price differs from what its provider publishes now. */}
      {catalog.state === 'ready' && catalog.value && (
        <PriceChanges {...signIn} className="cds--tile card overview-card wide" organizationId={organizationId} releaseId={catalog.value.releaseId}
          onChanged={summary => { setAdopted(summary); catalog.retry(); }} />
      )}
      <Card title="Spend · last 30 days" to="usage" {...card} load={usage} render={report => {
        const spend = spendByCurrency(report.totals);
        if (!spend.length) return <><p className="report-figure">—</p><p>No inference requests in the last 30 days.</p></>;
        return (
          <ul className="spend-list">
            {spend.map(item => (
              <li key={item.currency}>
                <span className="report-figure">{formatMoney(item.amount, item.currency)}</span>
                <span>{formatCount(item.requests)} request{item.requests === 1 ? '' : 's'}{item.estimated ? ' · includes estimated, unpriced or incomplete usage' : ''}</span>
              </li>
            ))}
          </ul>
        );
      }} />
      <Card title="Needs attention" to="usage" tab="reconciliation" {...card} load={reconciliation} render={page => {
        const count = page.items.length;
        return (
          <>
            <p className="report-figure">{count + (noBudget ? 1 : 0)}{page.next_page_token ? '+' : ''}</p>
            {noBudget && <p className="attention-line">No budget is set, so every request is refused. <AppLink to="/admin/policies/budgets">Add a budget</AppLink></p>}
            <p>{count === 0 ? (noBudget ? 'No reconciliation items are open.' : 'Every request settled automatically.') : `Open reconciliation item${count === 1 ? '' : 's'}: requests whose final cost needs a decision.`}</p>
          </>
        );
      }} />
      <Card title="Catalog" to="models" {...card} load={catalog} render={value => value
        ? <><p className="report-figure">{formatCount(value.deployments)} deployment{value.deployments === 1 ? '' : 's'}</p><p>Release {value.version} · {value.accounts} provider account{value.accounts === 1 ? '' : 's'}</p></>
        : <><p className="report-figure">—</p><p>No models yet, so nothing can be called. <AppLink to="/admin/connections/connect">Connect a provider</AppLink></p></>} />
      <RecentNotifications organizationId={organizationId} {...signIn} />
      <Card title="Recent activity" to="audit" {...card} load={activity} wide render={page => page.items.length === 0
        ? <p>No changes yet.</p>
        : <ul className="activity-list">{page.items.map((event: AuditEvent) => (
            <li key={event.id}><span title={event.action}>{actionLabel(event.action)}{event.actor?.name ? <span className="muted"> · {event.actor.name}</span> : null}</span><span className={`audit-outcome ${event.outcome}`}>{event.outcome}</span><time dateTime={event.occurred_at} title={event.occurred_at}>{formatRelativeDate(event.occurred_at)}</time></li>
          ))}</ul>} />
    </div>
  );
}

const linkLabels: Record<Destination, string> = { usage: 'Usage', models: 'Models', audit: 'Audit', policies: 'Policies' };
const tabLabels: Record<string, string> = { reconciliation: 'Reconciliation' };

function Card<T>({ title, to, tab, onNavigate, signIn, load, render, wide = false }: { title: string; to: Destination; tab?: string; onNavigate: (page: Destination, tab?: string) => void; signIn: SignIn; load: Load<T>; render: (value: T) => React.ReactNode; wide?: boolean }) {
  return (
    <article className={`cds--tile card overview-card${wide ? ' wide' : ''}`} aria-busy={load.state === 'loading'}>
      <div className="card-top"><span>{title}</span><button className="inline-action" onClick={() => (tab ? onNavigate(to, tab) : onNavigate(to))}>{tab ? tabLabels[tab] ?? linkLabels[to] : linkLabels[to]} →</button></div>
      {load.state === 'loading' && <p className="muted">Loading…</p>}
      {load.state === 'failed' && (load.status === 401
        ? <SessionExpired action="see this" {...signIn} />
        : load.status === 403
          ? <p className="muted">Your role cannot see this.</p>
          : <p className="muted">Unavailable right now. <button type="button" className="inline-action" onClick={load.retry}>Try again</button></p>)}
      {load.state === 'ready' && render(load.value)}
    </article>
  );
}
