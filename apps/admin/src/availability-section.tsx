import React, { useEffect, useState } from 'react';
import { ContentSwitcher, Switch } from '@carbon/react';
import { allPages, api, type ApiProblem, type CatalogDeployment, type DeployableModel, type DiscoveredModel, type ProviderAccount, type ProviderFamily } from './api';
import {
  accountKind,
  accountRegion,
  actionFor,
  blockedQuery,
  callsText,
  createDeployment,
  deployedEvidence,
  deployedQuery,
  endpointText,
  evidenceBadge,
  evidenceDetail,
  evidenceLabel,
  evidenceState,
  retirements,
  servesText,
  skuText,
  undeployedQuery,
  type AccountKind,
  type AccountPlace,
  type Action,
  type EvidenceFilter,
} from './availability-model';
import { chatOperations } from './discovery-model';
import { DiscoveryCheck } from './discovery-check';
import { Agreements } from './agreements-section';
import { ReportError, loadFailed, type SignIn } from './screen-feedback';
import { everyPage, useCollection } from './use-collection';
import { Button, DataCell, EmptyState, LoadMore, NoCatalog, formatDate, Select, FormField, Badge, Message } from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow } from './ui';
import { needsAccessRequest } from './cloud-access';
import { AccessNote } from './cloud-access-note';

type Props = SignIn & { organizationId: string; familyLabels: Record<string, string>; canChange?: boolean };
type View = 'deployed' | 'deployable' | 'agreements';
const baseViews: [View, string][] = [['deployed', 'Deployed'], ['deployable', 'Deployable']];
/** Bedrock accounts add their models' AWS Marketplace agreements. */
const viewsOf = (family: ProviderFamily): [View, string][] => (family === 'bedrock' ? [...baseViews, ['agreements', 'Agreements']] : baseViews);
const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);
const cols = (count: number) => ({ '--cols': count }) as React.CSSProperties;

/** Per provider account of the active release: what it can call now
 * (Deployed), and what it could use once the tenant acts (Deployable),
 * with the action and where to take it. Nothing here changes the catalog
 * or the provider account. */
export function Availability({ organizationId, familyLabels, canChange = false, ...signIn }: Props) {
  const [accounts, setAccounts] = useState<ProviderAccount[] | null>(null);
  const [deployments, setDeployments] = useState<CatalogDeployment[]>([]);
  const [noRelease, setNoRelease] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [key, setKey] = useState('');
  const [view, setView] = useState<View>('deployed');

  useEffect(() => {
    let current = true;
    setProblem(null);
    allPages<ProviderAccount, Awaited<ReturnType<typeof api.listProviderAccounts>>>(token => api.listProviderAccounts(organizationId, { page_size: 100, page_token: token })).then(
      ({ items, first }) => {
        if (!current) return;
        setAccounts(items);
        setNoRelease(first.release_id === null);
        setKey(previous => (items.some(item => item.adapter_instance_key === previous) ? previous : items[0]?.adapter_instance_key ?? ''));
      },
      caught => { if (current) setProblem(problemOf(caught, 'The provider accounts could not be loaded.')); },
    );
    // Only for Bedrock's region in console links: a failure leaves links
    // without one.
    everyPage<CatalogDeployment, Awaited<ReturnType<typeof api.listDeployments>>>(params => api.listDeployments(organizationId, params)).then(
      page => { if (current) setDeployments(page.items); },
      () => undefined,
    );
    return () => { current = false; };
  }, [organizationId, attempt]);

  if (problem) return <ReportError problem={problem} what="provider accounts" onRetry={() => setAttempt(value => value + 1)} {...signIn} />;
  if (!accounts) return <Message kind="info">Loading provider accounts…</Message>;
  if (noRelease) return <NoCatalog what="Availability is shown for the provider accounts in the catalog." />;
  if (accounts.length === 0) return <EmptyState title="No provider accounts" text="The active release declares no provider accounts." />;

  const account = accounts.find(item => item.adapter_instance_key === key) ?? accounts[0];
  const kind = accountKind(account.family);
  const place: AccountPlace = { region: account.family === 'bedrock' ? accountRegion(account.adapter_instance_key, deployments) : null };
  const family = familyLabels[account.family] ?? account.family;
  const views = viewsOf(account.family);
  const shownView: View = views.some(([name]) => name === view) ? view : 'deployed';
  // An account without this view resets it, so another Bedrock account
  // chosen next opens on Deployed rather than its agreements.
  if (shownView !== view) setView(shownView);

  return (
    <>
      <div className="section-header">
        <div>
          <h2>Model availability</h2>
          <p className="muted">
            What each provider account can call now, and what it could use once you act with the provider. The gateway never creates deployments, accepts terms or requests
            quota for you; each blocked model says what to do and where.
          </p>
        </div>
      </div>
      <div className="report-controls">
        <div className="audit-filter"><FormField label="Provider account" htmlFor="availability-account">
          <Select id="availability-account" value={account.adapter_instance_key} onChange={event => setKey(event.target.value)}>
            {accounts.map(item => <option key={item.adapter_instance_key} value={item.adapter_instance_key}>{item.adapter_instance_key} ({familyLabels[item.family] ?? item.family})</option>)}
          </Select>
        </FormField></div>
      </div>
      {/* Two views of the same account: a content switcher, as in IBM Cloud. */}
      <ContentSwitcher
        className="availability-switcher"
        aria-label="Availability views"
        selectedIndex={views.findIndex(([key]) => key === shownView)}
        onChange={({ name }) => setView(name as View)}
      >
        {views.map(([key, label]) => <Switch key={key} name={key} text={label} />)}
      </ContentSwitcher>
      <div className="availability-view">
      {shownView === 'agreements'
        ? <Agreements key={`agreements-${account.adapter_instance_key}`} {...signIn} organizationId={organizationId} account={account} canChange={canChange} />
        : shownView === 'deployed'
        ? <Deployed key={`deployed-${account.adapter_instance_key}`} {...signIn} organizationId={organizationId} account={account} kind={kind} family={family} place={place} />
        : kind === 'deployments'
          ? <AzureDeployable key={`deployable-${account.adapter_instance_key}`} {...signIn} organizationId={organizationId} account={account} />
          : <Blocked key={`blocked-${account.adapter_instance_key}`} {...signIn} organizationId={organizationId} account={account} family={family} place={place} />}
      </div>
    </>
  );
}

function ActionCell({ action, family, model }: { action: Action | null; family: ProviderFamily; model: DiscoveredModel }) {
  if (!action) return <span className="muted">—</span>;
  return (
    <>
      {action.means}<br />
      <small>{action.action}</small>
      {action.link && <><br /><a className="inline-action" href={action.link.href} target="_blank" rel="noopener noreferrer">{action.link.label}</a></>}
      {needsAccessRequest(model.evidence) && <><br /><AccessNote family={family} /></>}
    </>
  );
}

function EvidenceCell({ model }: { model: DiscoveredModel }) {
  const state = evidenceState(model);
  const detail = evidenceDetail(model.evidence);
  return (
    <>
      <Badge tone={evidenceBadge[state]}>{evidenceLabel[state]}</Badge>
      {detail && <><br /><small className="muted">{detail}</small></>}
    </>
  );
}

function Deployed({ organizationId, account, kind, family, place, ...signIn }: SignIn & { organizationId: string; account: ProviderAccount; kind: AccountKind; family: string; place: AccountPlace }) {
  const [narrow, setNarrow] = useState<EvidenceFilter | ''>('');
  const key = account.adapter_instance_key;
  const filters = deployedQuery(key, kind, narrow ? [narrow] : []);
  const models = useCollection<DiscoveredModel>(params => api.listDiscoveredModels(organizationId, filters, params), [organizationId, key, kind, narrow]);
  // Rows of this account's family only: a key reused for another family
  // keeps the old family's models.
  const rows = models.items.filter(model => model.provider_family === account.family);
  // When each deployment's model retires (Azure): loaded with the rows, and
  // again on Refresh.
  const [retiring, setRetiring] = useState<Map<string, string>>(new Map());
  const [retiringProblem, setRetiringProblem] = useState<ApiProblem | null>(null);
  const [retiringAttempt, setRetiringAttempt] = useState(0);
  useEffect(() => {
    if (kind !== 'deployments') return;
    let current = true;
    setRetiringProblem(null);
    everyPage<DeployableModel, Awaited<ReturnType<typeof api.listDeployableModels>>>(params => api.listDeployableModels(organizationId, { adapter_instance_key: key, deployed: true }, params)).then(
      page => { if (current) setRetiring(retirements(page.items.filter(model => model.provider_family === account.family))); },
      caught => { if (current) setRetiringProblem(problemOf(caught, 'Retirement dates could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, key, kind, account.family, retiringAttempt]);
  const refresh = () => {
    void models.load(true);
    setRetiringAttempt(value => value + 1);
  };

  const azure = kind === 'deployments';
  const count = azure ? 6 : 5;
  return (
    <section aria-label={`Deployed on ${key}`}>
      <p className="meta">
        {azure
          ? `Every deployment ${key} lists, with what the gateway has seen calling it. A deployment the gateway cannot call stays here with the reason.`
          : `The ${family} models ${key} lists that no check or call has refused. Blocked models are under Deployable.`}
      </p>
      <div className="report-controls">
        <div className="audit-filter"><FormField label="Evidence" htmlFor="deployed-evidence">
          <Select id="deployed-evidence" value={narrow} onChange={event => setNarrow(event.target.value as EvidenceFilter | '')}>
            <option value="">Any</option>
            {deployedEvidence(kind).map(state => <option key={state} value={state}>{evidenceLabel[state]}</option>)}
          </Select>
        </FormField></div>
        <Button variant="ghost" onClick={refresh}>Refresh</Button>
      </div>
      {retiringProblem && <ReportError problem={retiringProblem} what="retirement dates" onRetry={() => setRetiringAttempt(value => value + 1)} {...signIn} />}
      {models.loading && <Message kind="info">Loading deployed models…</Message>}
      {models.error && <ReportError problem={models.error} what="deployed models" onRetry={() => void models.load(true)} {...signIn} />}
      {!models.loading && !models.error && rows.length === 0 && !models.hasMore && (narrow
        ? <EmptyState title="Nothing matches" text="No deployed model has this evidence." />
        : <DiscoveryCheck {...signIn} organizationId={organizationId} accountKey={key} onChecked={refresh} />)}
      {!models.loading && !models.error && rows.length > 0 && (
        <DataGrid aria-label={`Deployed models of ${key}`}>
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>{azure ? 'Deployment' : 'Model'}</TableHeader>
              {azure && <TableHeader>Serves</TableHeader>}
              {azure && <TableHeader>SKU and capacity</TableHeader>}
              <TableHeader>Evidence</TableHeader>
              <TableHeader>Last checked</TableHeader>
              <TableHeader>Active release</TableHeader>
              {!azure && <TableHeader>Calls</TableHeader>}
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.map(model => {
              const retires = retiring.get(model.model);
              const action = evidenceState(model) === 'requires_action' || evidenceState(model) === 'refused' ? actionFor(account.family, model.evidence, place) : null;
              return (
                <TableRow className="data-row" key={model.model}>
                  <DataCell label="Deployment" className="cell-primary cell-wrap">
                    <code>{model.model}</code>
                    {model.display_name && <span className="muted cell-note">{model.display_name}</span>}
                    {!azure && model.model.startsWith('endpoints/') && endpointText(model) && <span className="muted cell-note">Endpoint: {endpointText(model)}</span>}
                  </DataCell>
                  {azure && (
                    <DataCell label="Serves" className="cell-wrap">
                      {servesText(model) ?? <span className="muted">Not reported</span>}
                      {retires && <><br /><Badge kind="danger">Retires {formatDate(retires)}</Badge></>}
                    </DataCell>
                  )}
                  {azure && <DataCell label="SKU">{skuText(model) ?? <span className="muted">Not reported</span>}</DataCell>}
                  <DataCell label="Evidence" className="cell-wrap">
                    <EvidenceCell model={model} />
                    {action && (
                      <>
                        <br /><small>{action.action}</small>
                        {action.link && <><br /><a className="inline-action" href={action.link.href} target="_blank" rel="noopener noreferrer">{action.link.label}</a></>}
                        {needsAccessRequest(model.evidence) && <><br /><AccessNote family={account.family} /></>}
                      </>
                    )}
                  </DataCell>
                  <DataCell label="Last checked" title={model.evidence?.verified_at ? `Last success ${formatDate(model.evidence.verified_at)}` : undefined}>
                    {model.evidence ? formatDate(model.evidence.observed_at) : <span className="muted">Never</span>}
                    {model.evidence?.verified_at && model.evidence.state !== 'verified' && <><br /><small className="muted">Last success {formatDate(model.evidence.verified_at)}</small></>}
                  </DataCell>
                  <DataCell label="Catalog">
                    <Badge tone={model.deployed ? 'badge-success' : 'badge-neutral'}>{model.deployed ? 'In the active release' : 'Not in the catalog'}</Badge>
                  </DataCell>
                  {!azure && (
                    <DataCell label="Calls">{callsText(model)}</DataCell>
                  )}
                </TableRow>
              );
            })}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={models.loadMore} hasMore={models.hasMore && !models.error} loading={models.loadingMore} />
    </section>
  );
}

function Blocked({ organizationId, account, family, place, ...signIn }: SignIn & { organizationId: string; account: ProviderAccount; family: string; place: AccountPlace }) {
  const key = account.adapter_instance_key;
  const models = useCollection<DiscoveredModel>(params => api.listDiscoveredModels(organizationId, blockedQuery(key), params), [organizationId, key]);
  const rows = models.items.filter(model => model.provider_family === account.family);
  return (
    <section aria-label={`Deployable on ${key}`}>
      <p className="meta">The {family} models {key} lists but cannot call yet, each with why and what to do. A model moves to Deployed once a check or a call shows the account may call it.</p>
      <div className="report-controls"><Button variant="ghost" onClick={() => void models.load(true)}>Refresh</Button></div>
      {models.loading && <Message kind="info">Loading deployable models…</Message>}
      {models.error && <ReportError problem={models.error} what="deployable models" onRetry={() => void models.load(true)} {...signIn} />}
      {!models.loading && !models.error && rows.length === 0 && !models.hasMore && (
        <EmptyState title="Nothing blocked" text="No listed model of this account is refused or needs action. Models no check has settled yet are under Deployed." />
      )}
      {!models.loading && !models.error && rows.length > 0 && (
        <DataGrid aria-label={`Deployable models of ${key}`}>
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Model</TableHeader>
              <TableHeader>Evidence</TableHeader>
              <TableHeader>Last checked</TableHeader>
              <TableHeader>What to do</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.map(model => (
              <TableRow className="data-row" key={model.model}>
                <DataCell label="Model" className="cell-primary cell-wrap">
                  <code>{model.model}</code>
                  {model.display_name && <span className="muted cell-note">{model.display_name}</span>}
                </DataCell>
                <DataCell label="Evidence" className="cell-wrap"><EvidenceCell model={model} /></DataCell>
                <DataCell label="Last checked">{model.evidence ? formatDate(model.evidence.observed_at) : '—'}</DataCell>
                <DataCell label="What to do" className="cell-wrap"><ActionCell action={actionFor(account.family, model.evidence, place)} family={account.family} model={model} /></DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={models.loadMore} hasMore={models.hasMore && !models.error} loading={models.loadingMore} />
    </section>
  );
}

type Operation = '' | 'chat' | 'embeddings';
const operationNames = { chat: 'Chat', chat_stream: 'Chat', embeddings: 'Embeddings', chat_batch: 'Chat batch', embeddings_batch: 'Embeddings batch' } as const;
const filterOperations = ['chat', 'embeddings'] as const;

function AzureDeployable({ organizationId, account, ...signIn }: SignIn & { organizationId: string; account: ProviderAccount }) {
  const key = account.adapter_instance_key;
  const [operation, setOperation] = useState<Operation>('');
  const [lifecycle, setLifecycle] = useState('');
  const filters = undeployedQuery(key, { operation: operation || undefined, lifecycle_status: lifecycle || undefined });
  const models = useCollection<DeployableModel>(params => api.listDeployableModels(organizationId, filters, params), [organizationId, key, operation, lifecycle]);
  const rows = models.items.filter(model => model.provider_family === account.family);
  const filtered = !!(operation || lifecycle);
  return (
    <section aria-label={`Deployable on ${key}`}>
      <p className="meta">Models in {key}’s model catalog that no deployment serves. To use one, create a deployment of it; the gateway never creates deployments.</p>
      <div className="report-controls">
        <div className="audit-filter"><FormField label="Capability" htmlFor="deployable-operation">
          <Select id="deployable-operation" value={operation} onChange={event => setOperation(event.target.value as Operation)}>
            <option value="">Any</option>
            {filterOperations.map(value => <option key={value} value={value}>{operationNames[value]}</option>)}
          </Select>
        </FormField></div>
        <div className="audit-filter"><FormField label="Lifecycle" htmlFor="deployable-lifecycle">
          <Select id="deployable-lifecycle" value={lifecycle} onChange={event => setLifecycle(event.target.value)}>
            <option value="">Any</option>
            <option value="GenerallyAvailable">Generally available</option>
            <option value="Preview">Preview</option>
            <option value="Deprecating">Deprecating</option>
            <option value="Deprecated">Deprecated</option>
          </Select>
        </FormField></div>
        {filtered && <Button variant="ghost" onClick={() => { setOperation(''); setLifecycle(''); }}>Clear filters</Button>}
      </div>
      {models.loading && <Message kind="info">Loading deployable models…</Message>}
      {models.error && <ReportError problem={models.error} what="deployable models" onRetry={() => void models.load(true)} {...signIn} />}
      {!models.loading && !models.error && rows.length === 0 && !models.hasMore && (
        <EmptyState
          title={filtered ? 'Nothing matches' : 'Nothing to deploy'}
          text={filtered ? 'No undeployed catalog model matches these filters.' : 'Every model in the account’s catalog has a deployment, or no discovery has read the catalog yet.'}
        />
      )}
      {!models.loading && !models.error && rows.length > 0 && (
        <>
          <p className="cds--form__helper-text">
            {createDeployment.action}{' '}
            {createDeployment.link && <a className="inline-action" href={createDeployment.link.href} target="_blank" rel="noopener noreferrer">{createDeployment.link.label}</a>}
          </p>
          <DataGrid aria-label={`Deployable models of ${key}`}>
            <TableHead>
              <TableRow className="data-row head">
                <TableHeader>Model</TableHeader>
                <TableHeader>Lifecycle</TableHeader>
                <TableHeader>SKUs</TableHeader>
                <TableHeader>Capabilities</TableHeader>
                <TableHeader>Retires</TableHeader>
              </TableRow>
            </TableHead>
            <TableBody>
              {rows.map(model => (
                <TableRow className="data-row" key={`${model.format}/${model.model}/${model.version}`}>
                  <DataCell label="Model" className="cell-primary cell-wrap">
                    <code>{model.model}</code> <span className="muted">{model.version}</span>
                    <span className="muted cell-note">{model.format}{model.default_version ? ' · default version' : ''}</span>
                  </DataCell>
                  <DataCell label="Lifecycle">{model.lifecycle_status ?? <span className="muted">Not reported</span>}</DataCell>
                  <DataCell label="SKUs" className="cell-wrap">
                    {model.skus.length ? model.skus.join(', ') : <span className="muted">None listed</span>}
                    {model.max_capacity !== null && <><br /><small className="muted">up to {model.max_capacity}</small></>}
                  </DataCell>
                  <DataCell label="Capabilities">{model.operations ? chatOperations(model.operations).map(value => operationNames[value]).join(', ') : 'unknown'}</DataCell>
                  <DataCell label="Retires">{model.retires_at ? formatDate(model.retires_at) : <span className="muted">—</span>}</DataCell>
                </TableRow>
              ))}
            </TableBody>
          </DataGrid>
        </>
      )}
      <LoadMore onClick={models.loadMore} hasMore={models.hasMore && !models.error} loading={models.loadingMore} />
    </section>
  );
}
