import React, { useEffect, useRef, useState } from 'react';
import {
  allPages,
  api,
  newIdempotencyKey,
  type ApiProblem,
  type AutoOffer,
  type CatalogSyncJob,
  type DiscoveredModel,
  type DiscoveredModelFilters,
  type DiscoveryDrift,
  type ProviderAccount,
  type ProviderFamily,
} from './api';
import { needsAccessRequest } from './cloud-access';
import { AccessNote } from './cloud-access-note';
import { POLL_LIMIT_MS, capabilityText, evidenceText, isActive, jobSummary, listingState, offerBadge, offerLabel, offerSummary, offerText, pollDelay, statusBadge } from './discovery-model';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { useCollection } from './use-collection';
import { MoreHint, SectionSwitcher, useSectionView } from './section-views';
import { Button, DataCell, EmptyState, FormField, LoadMore, NoCatalog, TextInput, formatDate, Select, Badge, Message } from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow } from './ui';

/** The Providers & catalog tabs this one links to. */
export type CatalogTab = 'models' | 'connect';
type Props = SignIn & {
  organizationId: string;
  canChange: boolean;
  onOpenTab: (tab: CatalogTab) => void;
  familyLabels: Record<string, string>;
};

/** The API's filters, and a family the list is narrowed to here: an account
 * key reused for another family keeps the old family's models, which drift
 * does not count. */
type ModelFilters = DiscoveredModelFilters & { provider_family?: ProviderFamily };
const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

type DiscoveryView = 'changes' | 'models' | 'offers' | 'jobs';
const discoveryViews: [DiscoveryView, string][] = [['changes', 'Changes'], ['models', 'Discovered models'], ['offers', 'Automatic offers'], ['jobs', 'Jobs']];

/** What discovery offers, and how it compares with the active catalog. A
 * listing changes the catalog only for an account opted in to automatic
 * offers; otherwise adding or removing a model goes through the Models and
 * Connect a provider tabs. */
export function Discovery({ organizationId, canChange, onOpenTab, familyLabels, ...signIn }: Props) {
  const [accounts, setAccounts] = useState<ProviderAccount[] | null>(null);
  const [noRelease, setNoRelease] = useState(false);
  const [accountsProblem, setAccountsProblem] = useState<ApiProblem | null>(null);
  const [accountsAttempt, setAccountsAttempt] = useState(0);
  // Bumped when a job finishes, so the lists show what it found.
  const [version, setVersion] = useState(0);
  const [filters, setFilters] = useState<ModelFilters>({});
  const [view, setView] = useSectionView<DiscoveryView>('discovery.view', discoveryViews.map(([key]) => key), () => 'changes');
  const modelsHeading = useRef<HTMLHeadingElement>(null);
  // Set when drift opens the models view, so its heading takes focus once shown.
  const focusModels = useRef(false);
  useEffect(() => {
    if (view !== 'models' || !focusModels.current) return;
    focusModels.current = false;
    modelsHeading.current?.focus();
  }, [view, filters]);

  useEffect(() => {
    let current = true;
    setAccountsProblem(null);
    allPages<ProviderAccount, Awaited<ReturnType<typeof api.listProviderAccounts>>>(token => api.listProviderAccounts(organizationId, { page_size: 100, page_token: token })).then(
      ({ items, first }) => {
        if (!current) return;
        setAccounts(items);
        setNoRelease(first.release_id === null);
      },
      caught => { if (current) setAccountsProblem(problemOf(caught, 'The provider accounts could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, accountsAttempt]);

  if (accountsProblem) return <ReportError problem={accountsProblem} what="provider accounts" onRetry={() => setAccountsAttempt(value => value + 1)} {...signIn} />;
  if (!accounts) return <Message kind="info">Loading provider accounts…</Message>;
  if (noRelease) {
    return <NoCatalog what="Discovery lists the models of the provider accounts in the catalog." />;
  }

  const showUndeployed = (key: string, family: ProviderFamily) => {
    setFilters({ adapter_instance_key: key, provider_family: family, state: 'listed', deployed: false });
    focusModels.current = true;
    setView('models');
  };

  return (
    <>
      <div className="section-header">
        <div>
          <h2 className="cds--visually-hidden">Discovery</h2>
          <p className="muted">What each provider account offers now, compared with the catalog.</p>
        </div>
      </div>
      <RunDiscovery {...signIn} organizationId={organizationId} accounts={accounts} canChange={canChange} onFinished={() => setVersion(value => value + 1)} />
      <SectionSwitcher label="Discovery views" views={discoveryViews} view={view} onChange={setView} />
      <div className="section-view">
        {view === 'changes' && <Drift {...signIn} organizationId={organizationId} version={version} familyLabels={familyLabels} onOpenTab={onOpenTab} onShowUndeployed={showUndeployed} />}
        {view === 'models' && <DiscoveredModels {...signIn} organizationId={organizationId} accounts={accounts} version={version} filters={filters} setFilters={setFilters} heading={modelsHeading} familyLabels={familyLabels} onOpenTab={onOpenTab} />}
        {view === 'offers' && <AutoOffers {...signIn} organizationId={organizationId} accounts={accounts} canChange={canChange} version={version} />}
        {view === 'jobs' && <SyncJobs {...signIn} organizationId={organizationId} accounts={accounts} version={version} />}
      </div>
    </>
  );
}

function RunDiscovery({ organizationId, accounts, canChange, onFinished, ...signIn }: SignIn & {
  organizationId: string;
  accounts: ProviderAccount[];
  canChange: boolean;
  onFinished: () => void;
}) {
  const [account, setAccount] = useState(accounts[0]?.adapter_instance_key ?? '');
  const [job, setJob] = useState<CatalogSyncJob | null>(null);
  // The job being polled; `run` restarts polling for the same job.
  const [tracked, setTracked] = useState<{ id: string; run: number } | null>(null);
  const [gaveUp, setGaveUp] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One key per account until the server refuses or answers: a retry replays.
  const keys = useRef(new Map<string, string>());
  const onFinishedRef = useRef(onFinished);
  onFinishedRef.current = onFinished;

  useEffect(() => {
    if (!tracked) return;
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    setGaveUp(false);
    // Its own deadline, so a request that never answers still stops here.
    const deadline = setTimeout(() => {
      cancelled = true;
      clearTimeout(timer);
      setGaveUp(true);
    }, POLL_LIMIT_MS);
    const stop = () => clearTimeout(deadline);
    const poll = async (attempt: number) => {
      if (cancelled) return;
      try {
        const next = await api.getSyncJob(organizationId, tracked.id);
        if (cancelled) return;
        setJob(next);
        if (!isActive(next)) {
          stop();
          onFinishedRef.current();
          return;
        }
      } catch (caught) {
        if (cancelled) return;
        const failure = problemOf(caught, 'The job’s status could not be loaded.');
        // A refusal will not change by asking again; anything else might.
        if (failure.status >= 400 && failure.status < 500) {
          stop();
          setProblem(failure);
          return;
        }
      }
      timer = setTimeout(() => void poll(attempt + 1), pollDelay(attempt + 1));
    };
    timer = setTimeout(() => void poll(0), pollDelay(0));
    return () => {
      cancelled = true;
      clearTimeout(timer);
      stop();
    };
  }, [organizationId, tracked]);

  const follow = (next: CatalogSyncJob) => {
    setJob(next);
    if (isActive(next)) setTracked(previous => ({ id: next.id, run: (previous?.run ?? 0) + 1 }));
    else onFinishedRef.current();
  };

  const start = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!account) return;
    const key = keys.current.get(account) ?? newIdempotencyKey();
    keys.current.set(account, key);
    setProblem(null);
    setSubmitting(true);
    try {
      const { data } = await api.startSyncJob(organizationId, account, key);
      keys.current.delete(account);
      follow(data);
    } catch (caught) {
      const failure = problemOf(caught, 'Discovery could not be started. Check your connection and try again.');
      // A refusal is final; no answer or a server error may hide a queued job.
      if (failure.status >= 400 && failure.status < 500) keys.current.delete(account);
      setProblem(failure);
      // Already queued or running: follow that job instead.
      if (failure.code === 'catalog_sync_in_progress') {
        try {
          const page = await api.listSyncJobs(organizationId, account, { page_size: 5 });
          const running = page.items.find(isActive);
          if (running) follow(running);
        } catch {
          /* the message above still explains it */
        }
      }
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <section className="cds--tile detail-panel discovery-run" aria-labelledby="discovery-run-heading">
      <div className="detail-header">
        <div>
          <h3 id="discovery-run-heading">Discover models</h3>
          <p className="meta">Asks a provider account which models it offers now; the worker also asks every six hours. It never changes the catalog.</p>
        </div>
      </div>
      {canChange ? (
        <form className="report-controls" onSubmit={start}>
          <div className="audit-filter"><FormField label="Provider account" htmlFor="discovery-account">
            <Select id="discovery-account" value={account} onChange={event => { setAccount(event.target.value); setProblem(null); }}>
              {accounts.map(item => <option key={item.adapter_instance_key} value={item.adapter_instance_key}>{item.adapter_instance_key}</option>)}
            </Select>
          </FormField></div>
          <Button type="submit" loading={submitting} disabled={!account || (job !== null && job.adapter_instance_key === account && isActive(job) && !gaveUp)}>Discover models</Button>
        </form>
      ) : <p className="muted">You can view what discovery found. Only organization owners and admins can start it.</p>}
      {problem?.code === 'catalog_sync_in_progress'
        ? <Message kind="info">A discovery of this account is already queued or running; its progress is shown below.</Message>
        : problem?.code === 'unknown_provider_account'
          ? <Message kind="error">This account is no longer in the active release. Reload the page to see the current accounts.</Message>
          : <ActionError problem={problem} action="start discovery" {...signIn} />}
      {job && (
        <div className="job-status" role="status" aria-live="polite">
          <Badge tone={statusBadge[job.status]}>{job.status}</Badge>{' '}
          <strong>{job.adapter_instance_key}</strong>: {jobSummary(job)}
          {gaveUp && isActive(job) && (
            <> Still not finished after 10 minutes; it keeps running on the server. <button type="button" className="inline-action" onClick={() => setTracked({ id: job.id, run: Date.now() })}>Check again</button></>
          )}
        </div>
      )}
    </section>
  );
}

/** One account's opt-in to automatic offers: after each listing, the
 * worker adds the listed models a price source prices, under their model
 * identifiers, and keeps their prices in step. */
function AutoOffers({ organizationId, accounts, canChange, version, ...signIn }: SignIn & {
  organizationId: string;
  accounts: ProviderAccount[];
  canChange: boolean;
  version: number;
}) {
  const [account, setAccount] = useState(accounts[0]?.adapter_instance_key ?? '');
  const [offer, setOffer] = useState<AutoOffer | null | undefined>(undefined);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [location, setLocation] = useState('');
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // One key per change until the server refuses or answers: a retry replays.
  const key = useRef<{ change: string; key: string } | null>(null);
  // The account shown now: an answer for another one is not shown.
  const shown = useRef(account);
  shown.current = account;

  useEffect(() => {
    if (!account) return;
    let current = true;
    setOffer(undefined);
    setLoadProblem(null);
    api.getAutoOffer(organizationId, account).then(
      next => {
        if (!current) return;
        setOffer(next);
        setLocation(next?.location ?? '');
      },
      caught => { if (current) setLoadProblem(problemOf(caught, 'Automatic offers could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, account, version, attempt]);

  const change = async (name: string, send: (idempotencyKey: string) => Promise<AutoOffer | null>) => {
    const fingerprint = JSON.stringify([name, account, location, offer?.version ?? null]);
    if (key.current?.change !== fingerprint) key.current = { change: fingerprint, key: newIdempotencyKey() };
    const target = account;
    setProblem(null);
    setSubmitting(true);
    try {
      const next = await send(key.current.key);
      key.current = null;
      if (shown.current !== target) return;
      setOffer(next);
      setLocation(next?.location ?? '');
    } catch (caught) {
      const failure = problemOf(caught, 'Automatic offers could not be changed. Check your connection and try again.');
      if (failure.status >= 400 && failure.status < 500) key.current = null;
      if (shown.current === target) setProblem(failure);
    } finally {
      setSubmitting(false);
    }
  };
  const save = (event: React.FormEvent) => {
    event.preventDefault();
    void change('set', async idempotencyKey => (
      await api.setAutoOffer(organizationId, account, { location: location.trim() || null }, offer ?? null, idempotencyKey)
    ).data);
  };
  const stop = () => {
    if (!offer) return;
    void change('delete', async idempotencyKey => {
      await api.deleteAutoOffer(organizationId, offer, idempotencyKey);
      return null;
    });
  };
  const withdrawn = offer?.models.some(model => model.status === 'withdrawn') ?? false;

  return (
    <section aria-labelledby="auto-offer-heading">
      <div className="section-header">
        <div>
          <h3 id="auto-offer-heading">Offer discovered models automatically</h3>
          <MoreHint
            hint="Adds an account’s newly listed models to the catalog when a price source prices them."
            summary="What turning this on does"
            more={<>
              After each listing, every chat or embedding model a price source prices is added to the catalog under its model identifier, and its price is kept in step with the source.
              Each change is a new release, activated and audited. Models without a published price wait for you to map them to a price list item.
            </>}
          />
        </div>
      </div>
      <div className="report-controls">
        <div className="audit-filter"><FormField label="Provider account" htmlFor="auto-offer-account">
          <Select id="auto-offer-account" value={account} disabled={submitting} onChange={event => { setAccount(event.target.value); setProblem(null); }}>
            {accounts.map(item => <option key={item.adapter_instance_key} value={item.adapter_instance_key}>{item.adapter_instance_key}</option>)}
          </Select>
        </FormField></div>
      </div>
      {loadProblem && <ReportError problem={loadProblem} what="automatic offers" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
      {!loadProblem && offer === undefined && <Message kind="info">Loading automatic offers…</Message>}
      {!loadProblem && offer !== undefined && (
        <>
          <Message kind="info">
            {offer
              ? <><Badge kind="success">On</Badge> {offerSummary(offer)}</>
              : <><Badge kind="neutral">Off</Badge> New models of <strong>{account}</strong> are only listed, not offered.</>}
          </Message>
          {canChange ? (
            <form className="report-controls" onSubmit={save}>
              <FormField label="Location" htmlFor="auto-offer-location" hint="Where offered models run, which also picks regional prices: a Bedrock account’s AWS region. Leave empty to use the account’s other deployments’ location.">
                <TextInput id="auto-offer-location" value={location} onChange={event => setLocation(event.target.value)} placeholder="us-east-1" />
              </FormField>
              <Button type="submit" loading={submitting}>{offer ? (withdrawn ? 'Save, and offer withdrawn models again' : 'Save') : 'Turn on'}</Button>
              {offer && <Button type="button" variant="ghost" onClick={stop} disabled={submitting}>Turn off</Button>}
            </form>
          ) : <p className="muted">Only organization owners and admins can change automatic offers.</p>}
          {problem?.code === 'unknown_provider_account'
            ? <Message kind="error">This account is no longer in the active release. Reload the page to see the current accounts.</Message>
            : problem?.code === 'version_conflict'
              ? <Message kind="error">Someone else changed this since it was loaded. <button type="button" className="inline-action" onClick={() => setAttempt(value => value + 1)}>Load the current setting</button></Message>
              : <ActionError problem={problem} action="change automatic offers" {...signIn} />}
          {offer && offer.models.length > 0 && (
            <DataGrid aria-label="Automatically offered models">
              <TableHead>
                <TableRow className="data-row head">
                  <TableHeader>Model</TableHeader>
                  <TableHeader>Status</TableHeader>
                  <TableHeader>Detail</TableHeader>
                </TableRow>
              </TableHead>
              <TableBody>
                {offer.models.map(model => (
                  <TableRow className="data-row" key={model.model}>
                    <DataCell label="Model" className="cell-primary"><code>{model.model}</code><br /><small className="muted">{model.deployment}</small></DataCell>
                    <DataCell label="Status"><Badge tone={offerBadge[model.status]}>{offerLabel[model.status]}</Badge></DataCell>
                    <DataCell label="Detail">
                      {model.status === 'offered' && model.price_source
                        ? <>Priced by {model.price_source}: {model.price_item}</>
                        : offerText(model.status, model.deployment)}
                    </DataCell>
                  </TableRow>
                ))}
              </TableBody>
            </DataGrid>
          )}
        </>
      )}
    </section>
  );
}

function Drift({ organizationId, version, familyLabels, onOpenTab, onShowUndeployed, ...signIn }: SignIn & {
  organizationId: string;
  version: number;
  familyLabels: Record<string, string>;
  onOpenTab: (tab: CatalogTab) => void;
  onShowUndeployed: (key: string, family: ProviderFamily) => void;
}) {
  const [drift, setDrift] = useState<DiscoveryDrift | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setProblem(null);
    api.discoveryDrift(organizationId).then(
      value => { if (current) setDrift(value); },
      caught => { if (current) setProblem(problemOf(caught, 'The changes since the last check could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, version, attempt]);

  const unlisted = (drift?.accounts ?? []).flatMap(account => (account.unlisted_deployments ?? []).map(item => ({ ...item, account: account.adapter_instance_key })));
  return (
    <>
      <div className="section-header">
        <div>
          <h3>Changes since the last check</h3>
          <p className="muted">Where the catalog and what each account lists now disagree.</p>
        </div>
      </div>
      {problem?.status === 404
        ? <NoCatalog what="Drift compares the catalog with what providers list." />
        : problem
          ? <ReportError problem={problem} what="changes since the last check" onRetry={() => setAttempt(value => value + 1)} {...signIn} />
          : !drift
            ? <Message kind="info">Loading changes since the last check…</Message>
            : drift.accounts.length === 0
              ? <EmptyState title="No provider accounts" text="The active release declares no provider accounts." />
              : (
                <>
                  <DataGrid aria-label="Discovery drift by account">
                    <TableHead>
                      <TableRow className="data-row head">
                        <TableHeader>Account</TableHeader>
                        <TableHeader>Last discovery</TableHeader>
                        <TableHeader>Offered, not in the catalog</TableHeader>
                        <TableHeader>In the catalog, not offered</TableHeader>
                      </TableRow>
                    </TableHead>
                    <TableBody>
                      {drift.accounts.map(account => (
                        <TableRow className="data-row" key={account.adapter_instance_key}>
                          <DataCell label="Account" className="cell-primary"><code>{account.adapter_instance_key}</code><br /><small className="muted">{familyLabels[account.provider_family] ?? account.provider_family}</small></DataCell>
                          <DataCell label="Last discovery">
                            {account.last_job
                              ? <><Badge tone={statusBadge[account.last_job.status]}>{account.last_job.status}</Badge><br /><small className="muted">{account.last_synced_at ? `Last listed ${formatDate(account.last_synced_at)}` : 'Never listed successfully'}</small></>
                              : <span className="muted">Not discovered yet</span>}
                          </DataCell>
                          <DataCell label="Offered, not in catalog">
                            {account.undeployed_models === null
                              ? <span className="muted">Unknown until a discovery succeeds</span>
                              : account.undeployed_models === 0
                                ? 'None'
                                : <button className="inline-action" onClick={() => onShowUndeployed(account.adapter_instance_key, account.provider_family)} aria-label={`Show the ${account.undeployed_models} models ${account.adapter_instance_key} offers that are not in the catalog`}>{account.undeployed_models} model{account.undeployed_models === 1 ? '' : 's'}</button>}
                          </DataCell>
                          <DataCell label="In catalog, not offered">
                            {account.unlisted_deployments === null
                              ? <span className="muted">Unknown until a discovery succeeds</span>
                              : account.unlisted_deployments.length === 0 ? 'None' : `${account.unlisted_deployments.length} deployment${account.unlisted_deployments.length === 1 ? '' : 's'}`}
                          </DataCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </DataGrid>
                  {unlisted.length > 0 && (
                    <div className="cds--tile detail-panel drift-panel">
                      <h4>In the catalog, but not offered</h4>
                      <p className="meta">Requests to these deployments may fail. Edit or remove them under Models.</p>
                      <ul className="plain-list">
                        {unlisted.map(item => (
                          <li key={`${item.account}/${item.deployment}`}>
                            <code>{item.model}</code> <span className="muted">({item.deployment})</span> ·{' '}
                            {item.missing_since ? `not listed since ${formatDate(item.missing_since)}` : 'never listed by this account'}
                          </li>
                        ))}
                      </ul>
                      <Button variant="ghost" onClick={() => onOpenTab('models')}>Open Models</Button>
                    </div>
                  )}
                </>
              )}
    </>
  );
}

type DeployedFilter = '' | 'true' | 'false';

function DiscoveredModels({ organizationId, accounts, version, filters, setFilters, heading, familyLabels, onOpenTab, ...signIn }: SignIn & {
  organizationId: string;
  accounts: ProviderAccount[];
  version: number;
  filters: ModelFilters;
  setFilters: (filters: ModelFilters) => void;
  heading: React.RefObject<HTMLHeadingElement | null>;
  familyLabels: Record<string, string>;
  onOpenTab: (tab: CatalogTab) => void;
}) {
  const key = JSON.stringify(filters);
  const models = useCollection<DiscoveredModel>(params => api.listDiscoveredModels(organizationId, filters, params), [organizationId, key, version]);
  const rows = filters.provider_family ? models.items.filter(model => model.provider_family === filters.provider_family) : models.items;
  // Narrowed here, a page can hold none of the family while later pages
  // do: keep loading until one matches or the pages run out.
  const seeking = !!filters.provider_family && rows.length === 0 && models.hasMore && !models.error;
  useEffect(() => {
    if (seeking && !models.loading && !models.loadingMore) models.loadMore();
  }, [seeking, models.loading, models.loadingMore, models.items.length]); // eslint-disable-line react-hooks/exhaustive-deps
  const deployed: DeployedFilter = filters.deployed === undefined ? '' : filters.deployed ? 'true' : 'false';
  const filtered = !!(filters.adapter_instance_key || filters.state || filters.deployed !== undefined);
  return (
    <>
      <div className="section-header">
        <div>
          <h3 ref={heading} tabIndex={-1}>Discovered models</h3>
          <MoreHint
            hint="What the provider accounts list."
            summary="Why capabilities show as unknown"
            more="Discovery reports no capabilities or prices, so a model not in the catalog has unknown capabilities until it is added with them."
          />
        </div>
      </div>
      <div className="report-controls">
        <div className="audit-filter"><FormField label="Account" htmlFor="discovered-account">
          <Select id="discovered-account" value={filters.adapter_instance_key ?? ''} onChange={event => setFilters({ ...filters, adapter_instance_key: event.target.value || undefined, provider_family: undefined })}>
            <option value="">All accounts</option>
            {accounts.map(item => <option key={item.adapter_instance_key} value={item.adapter_instance_key}>{item.adapter_instance_key}</option>)}
          </Select>
        </FormField></div>
        <div className="audit-filter"><FormField label="Listing" htmlFor="discovered-state">
          <Select id="discovered-state" value={filters.state ?? ''} onChange={event => setFilters({ ...filters, state: (event.target.value || undefined) as DiscoveredModelFilters['state'] })}>
            <option value="">Listed or not</option>
            <option value="listed">Listed now</option>
            <option value="missing">No longer listed</option>
          </Select>
        </FormField></div>
        <div className="audit-filter"><FormField label="Catalog" htmlFor="discovered-deployed">
          <Select id="discovered-deployed" value={deployed} onChange={event => setFilters({ ...filters, deployed: event.target.value === '' ? undefined : event.target.value === 'true' })}>
            <option value="">In the catalog or not</option>
            <option value="true">In the catalog</option>
            <option value="false">Not in the catalog</option>
          </Select>
        </FormField></div>
        {filtered && <Button variant="ghost" onClick={() => setFilters({})}>Clear filters</Button>}
      </div>
      {models.loading && <Message kind="info">Loading discovered models…</Message>}
      {models.error && <ReportError problem={models.error} what="discovered models" onRetry={() => void models.load(true)} {...signIn} />}
      {seeking && <Message kind="info">Looking for matching models…</Message>}
      {!models.loading && !models.error && !seeking && rows.length === 0 && (
        <EmptyState
          title={filtered ? 'No models match' : 'No models discovered yet'}
          text={filtered ? 'No discovered model matches these filters.' : 'Models appear here once a discovery of an account succeeds.'}
        />
      )}
      {!models.loading && !models.error && rows.length > 0 && (
        <>
          {rows.some(model => !model.deployed && !model.missing_since) && (
            <p className="cds--form__helper-text">
              To offer a model that is not in the catalog, use Add provider (Connect a provider) or ask the gateway operator. This screen never changes the catalog.
            </p>
          )}
          <DataGrid aria-label="Discovered models">
            <TableHead>
              <TableRow className="data-row head">
                <TableHeader>Model</TableHeader>
                <TableHeader>Account</TableHeader>
                <TableHeader>Catalog</TableHeader>
                <TableHeader>Listing</TableHeader>
                <TableHeader>Capabilities</TableHeader>
                <TableHeader className="row-actions">Actions</TableHeader>
              </TableRow>
            </TableHead>
            <TableBody>
              {rows.map(model => {
                const state = listingState(model);
                const evidence = evidenceText(model);
                return (
                  <TableRow className="data-row" key={`${model.adapter_instance_key}/${model.provider_family}/${model.model}`}>
                    <DataCell label="Model" className="cell-primary cell-wrap">
                      <code>{model.model}</code>
                      {model.display_name && <span className="muted cell-note">{model.display_name}</span>}
                    </DataCell>
                    <DataCell label="Account" title={familyLabels[model.provider_family] ?? model.provider_family}><code>{model.adapter_instance_key}</code></DataCell>
                    <DataCell label="Catalog">
                      <Badge tone={model.deployed ? 'badge-success' : 'badge-neutral'}>{model.deployed ? 'In the catalog' : 'Not in the catalog'}</Badge>
                    </DataCell>
                    <DataCell label="Listing" title={`First seen ${formatDate(model.first_seen_at)}, last seen ${formatDate(model.last_seen_at)}`}>
                      {state.text}{!state.listed && <><br /><small className="muted">since {formatDate(model.missing_since)}</small></>}
                      {evidence && model.evidence && (
                        <><br />{(() => {
                          const seen = `Seen ${formatDate(model.evidence.observed_at)}${model.evidence.deployment ? ` through ${model.evidence.deployment}` : ''}`;
                          return model.evidence.state === 'refused' ? <Badge kind="danger" title={seen}>{evidence}</Badge> : <small className="muted" title={seen}>{evidence}</small>;
                        })()}</>
                      )}
                      {needsAccessRequest(model.evidence) && <><br /><AccessNote family={model.provider_family} /></>}
                    </DataCell>
                    <DataCell label="Capabilities">{capabilityText(model)}</DataCell>
                    <DataCell label="Actions">
                      {model.deployed
                        ? <button className="inline-action" onClick={() => onOpenTab('models')} aria-label={`Open ${model.model} under Models`}>Open in Models</button>
                        : state.listed
                          ? <button className="inline-action" onClick={() => onOpenTab('connect')} aria-label={`Add ${model.model} through Connect a provider`}>Add provider…</button>
                          : null}
                    </DataCell>
                  </TableRow>
                );
              })}
            </TableBody>
          </DataGrid>
        </>
      )}
      <LoadMore onClick={models.loadMore} hasMore={models.hasMore && !models.error} loading={models.loadingMore} />
    </>
  );
}

function SyncJobs({ organizationId, accounts, version, ...signIn }: SignIn & { organizationId: string; accounts: ProviderAccount[]; version: number }) {
  const [account, setAccount] = useState('');
  const jobs = useCollection<CatalogSyncJob>(params => api.listSyncJobs(organizationId, account || undefined, params), [organizationId, account, version]);
  return (
    <>
      <div className="section-header">
        <div><h3>Discovery jobs</h3><p className="muted">Each listing of an account, newest first. Finished jobs are kept for 30 days.</p></div>
      </div>
      <div className="report-controls">
        <div className="audit-filter"><FormField label="Account" htmlFor="jobs-account">
          <Select id="jobs-account" value={account} onChange={event => setAccount(event.target.value)}>
            <option value="">All accounts</option>
            {accounts.map(item => <option key={item.adapter_instance_key} value={item.adapter_instance_key}>{item.adapter_instance_key}</option>)}
          </Select>
        </FormField></div>
        <Button variant="ghost" onClick={() => void jobs.load(true)}>Refresh</Button>
      </div>
      {jobs.loading && <Message kind="info">Loading discovery jobs…</Message>}
      {jobs.error && <ReportError problem={jobs.error} what="discovery jobs" onRetry={() => void jobs.load(true)} {...signIn} />}
      {!jobs.loading && !jobs.error && jobs.items.length === 0 && <EmptyState title="No discovery jobs" text="Jobs appear here once discovery runs, on its six-hour schedule or when someone starts it." />}
      {!jobs.loading && !jobs.error && jobs.items.length > 0 && (
        <DataGrid aria-label="Discovery jobs">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Account</TableHeader>
              <TableHeader>Requested</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Result</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {jobs.items.map(job => (
              <TableRow className="data-row" key={job.id}>
                <DataCell label="Account" className="cell-primary"><code>{job.adapter_instance_key}</code></DataCell>
                <DataCell label="Started" title={job.created_at}>{formatDate(job.created_at)}<br /><small className="muted">{job.trigger === 'manual' ? 'Started by a person' : 'Scheduled'}</small></DataCell>
                <DataCell label="Status"><Badge tone={statusBadge[job.status]}>{job.status}</Badge></DataCell>
                <DataCell label="Result">{jobSummary(job)}</DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={jobs.loadMore} hasMore={jobs.hasMore && !jobs.error} loading={jobs.loadingMore} />
    </>
  );
}
