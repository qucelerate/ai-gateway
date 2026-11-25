import React, { useEffect, useRef, useState } from 'react';
import {
  api,
  newIdempotencyKey,
  type ApiProblem,
  type CatalogDeployment,
  type CatalogOperation,
  type CatalogPrice,
  type CatalogRelease,
  type CatalogRoute,
  type ModelCardSummary,
  type CloudTrust,
  type ProviderAccount,
  type ProviderCredential,
  type ProviderFamily,
} from './api';
import { buildSecret, credentialModes, type CredentialMode } from './credential-form';
import { shownOperations } from './catalog-change';
import { EditDeploymentDialog, EditRouteDialog, RemoveFromCatalogDialog } from './catalog-actions';
import { ConnectCloud } from './connect-cloud';
import { Availability } from './availability-section';
import { Discovery } from './discovery-section';
import { deploymentName } from './add-provider-form';
import { formatMoney, minorDigits } from './money';
import { Pricing } from './pricing-section';
import { EvaluationsSection } from './evaluations-section';
import { RoutingPolicySection } from './routing-policy-section';
import { HealthBadge, healthFor, healthReadAt, useDeploymentHealth, type HealthLoad } from './route-health';
import { ModelCardView, cardPath, useCardModel } from './model-card';
import { ModelMasters } from './model-masters';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { everyPage, useCollection } from './use-collection';
import { DataTable, type Column } from './data-table';
import { AppLink, Badge, Button, CopyButton, DataCell, Dialog, EmptyState, FormField, LoadMore, NoCatalog, TextInput, formatDate, navigateTo, useRouteTab, RadioOption, Message } from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow } from './ui';
import { offersChanges } from './write-access';

type Tab = 'models' | 'masters' | 'aliases' | 'pricing' | 'evaluations' | 'discovery' | 'availability' | 'connect' | 'credentials' | 'releases' | 'trust';
/** Two pages: the models you call (daily use), and the cloud accounts
 * behind them (set up once). */
export type CatalogSection = 'models' | 'connections';
export const catalogTabs: Record<CatalogSection, [Tab, string][]> = {
  models: [['models', 'Models'], ['masters', 'Across providers'], ['aliases', 'Aliases'], ['availability', 'Availability'], ['pricing', 'Pricing'], ['evaluations', 'Evaluations'], ['releases', 'Releases']],
  connections: [['connect', 'Connect a provider'], ['credentials', 'Credentials'], ['discovery', 'Discovery'], ['trust', 'Cloud trust']],
};
const sectionKeys: Record<CatalogSection, Tab[]> = {
  models: catalogTabs.models.map(([key]) => key),
  connections: catalogTabs.connections.map(([key]) => key),
};
/** The page a catalog tab is on. */
export const catalogSectionOf = (tab: string): CatalogSection => (sectionKeys.connections.includes(tab as Tab) ? 'connections' : 'models');
type Props = SignIn & { organizationId: string };
/** `role` is the confirmed context's; it only decides which controls show. */
type ScreenProps = Props & { role?: string | null };

export const familyLabels: Record<ProviderFamily, string> = {
  openai: 'OpenAI',
  anthropic: 'Anthropic',
  gemini: 'Google Gemini',
  bedrock: 'Amazon Bedrock',
  'azure-openai': 'Azure OpenAI',
  'azure-ai-foundry': 'Azure AI Foundry',
  'openai-compatible': 'OpenAI-compatible',
};

const operationLabels = { chat: 'Chat', chat_stream: 'Streaming', embeddings: 'Embeddings', chat_batch: 'Chat batch', embeddings_batch: 'Embeddings batch' } as const;

/** A per-token price per million tokens, as providers quote it. */
/** A reasoning price as charged: zero bills reasoning tokens at the
 * output price, not free. */
export function reasoningPrice(perToken: number, currency: string): string {
  return perToken === 0 ? '= output' : perMillion(perToken, currency);
}

export function perMillion(perToken: number, currency: string): string {
  return formatMoney(perToken * 1_000_000, currency);
}

export function ProvidersScreen({ role, section = 'models', ...props }: ScreenProps & { section?: CatalogSection }) {
  const tabs = catalogTabs[section];
  const [tab, setTab] = useRouteTab(sectionKeys[section], tabs[0][0]);
  // `/admin/models/card/<model>` opens that model's card.
  const cardModel = useCardModel();
  // A tab on the other page opens there.
  const openTab = (next: Tab) => (sectionKeys[section].includes(next) ? setTab(next) : navigateTo(`/admin/${catalogSectionOf(next)}/${next}`));
  if (!props.organizationId) {
    return <EmptyState title="Waiting for organization context" text="Select a confirmed organization to see its models and the cloud accounts behind them." />;
  }
  if (section === 'models' && cardModel) return <ModelCardView {...props} model={cardModel} familyLabels={familyLabels} canChange={offersChanges(role)} />;
  return (
    <div className="screen">
      {/* The section is chosen in the side navigation. */}
      <div className="page-section">
      {tab === 'models' && <Models {...props} />}
      {tab === 'masters' && <AcrossProviders {...props} canChange={offersChanges(role)} />}
      {tab === 'aliases' && <Aliases {...props} canChange={offersChanges(role)} />}
      {tab === 'pricing' && <Pricing {...props} canChange={offersChanges(role)} />}
      {tab === 'discovery' && <Discovery {...props} canChange={offersChanges(role)} onOpenTab={openTab} familyLabels={familyLabels} />}
      {tab === 'availability' && <Availability {...props} familyLabels={familyLabels} canChange={offersChanges(role)} />}
      {tab === 'evaluations' && <EvaluationsSection {...props} canChange={offersChanges(role)} />}
      {tab === 'connect' && <ConnectCloud {...props} />}
      {tab === 'credentials' && <Credentials {...props} />}
      {tab === 'releases' && <Releases {...props} />}
      {tab === 'trust' && <Trust {...props} />}
      </div>
    </div>
  );
}

function Models({ organizationId, ...signIn }: Props) {
  const [release, setRelease] = useState<string | null | undefined>(undefined);
  const [editing, setEditing] = useState<CatalogDeployment | null>(null);
  const [removing, setRemoving] = useState<CatalogDeployment | null>(null);
  const [changed, setChanged] = useState<string | null>(null);
  // The catalog is small: load it whole and let the table filter and group it.
  const deployments = useCollection<CatalogDeployment>(async () => {
    const page = await everyPage(params => api.listDeployments(organizationId, params));
    setRelease(page.release_id);
    return page;
  }, [organizationId]);
  // Aliases are read from the release the deployments came from, so the two
  // tables never mix releases when one is activated in between.
  const routes = useCollection<CatalogRoute>(
    () => (release ? everyPage(params => api.listRoutes(organizationId, release, params)) : Promise.resolve({ items: [], next_page_token: null })),
    [organizationId, release],
  );
  // Card facts per model (context window, retirement). Optional: without
  // them the columns are empty and the table still works.
  const [cards, setCards] = useState<Map<string, ModelCardSummary>>(new Map());
  useEffect(() => {
    if (!release) return;
    let current = true;
    api.modelCards(organizationId).then(
      list => { if (current) setCards(new Map(list.items.map(item => [item.model, item]))); },
      () => { if (current) setCards(new Map()); },
    );
    return () => { current = false; };
  }, [organizationId, release]);
  const columns = React.useMemo(() => [...deploymentColumns, ...cardColumns(cards)], [cards]);
  // A change is a new active release: read the catalog again.
  const onChanged = (summary: string) => {
    setEditing(null);
    setRemoving(null);
    setChanged(summary);
    void deployments.load(true);
  };
  if (!deployments.loading && !deployments.error && release === null) return <NoCatalog what="The catalog lists the models this organization can call." />;
  return (
    <>
      {changed && <Message kind="success">{changed}</Message>}
      <div className="section-header"><div><h2>Deployments</h2><p className="muted">Models in the active catalog release, where they run, and what they cost per million tokens.</p></div></div>
      {deployments.loading && <Message kind="info">Loading deployments…</Message>}
      {deployments.error && <ReportError problem={deployments.error} what="catalog" onRetry={() => void deployments.load(true)} {...signIn} />}
      {!deployments.loading && !deployments.error && deployments.items.length === 0 && <EmptyState title="No deployments" text="The active release declares no deployments." />}
      {!deployments.loading && !deployments.error && deployments.items.length > 0 && (
        <DataTable
          label="Deployments"
          urlKey="deployments"
          filtersOpen
          columns={columns}
          rows={deployments.items}
          rowKey={deployment => deployment.id}
          detail={deployment => <DeploymentDetail deployment={deployment} routes={routes.loading ? 'loading' : routes.error ? 'failed' : routes.items} />}
          actions={deployment => (
            <>
              <AppLink to={cardPath(deployment.model)} label={`Model card for ${deployment.model}`}>Card</AppLink>
              <button className="inline-action" onClick={() => setEditing(deployment)} aria-label={`Edit ${deployment.name}`}>Edit</button>
              <button className="inline-action" onClick={() => setRemoving(deployment)} aria-label={`Remove ${deployment.name}`}>Remove</button>
            </>
          )}
        />
      )}
      {release && editing && (
        <EditDeploymentDialog {...signIn} organizationId={organizationId} releaseId={release} name={editing.name} model={editing.model} onClose={() => setEditing(null)} onChanged={onChanged} />
      )}
      {release && removing && (
        <RemoveFromCatalogDialog {...signIn} organizationId={organizationId} releaseId={release} target={{ kind: 'deployment', name: removing.name, model: removing.model }} onClose={() => setRemoving(null)} onChanged={onChanged} />
      )}
    </>
  );
}

/** Each model once, with its deployments on every provider side by side
 * (master records, D-033), on a page of its own: the Models page lists
 * every deployment as before. */
function AcrossProviders({ organizationId, canChange, ...signIn }: Props & { canChange: boolean }) {
  const [release, setRelease] = useState<string | null | undefined>(undefined);
  // Only the active release's id: the records read their own deployments.
  const active = useCollection<CatalogDeployment>(async () => {
    const page = await api.listDeployments(organizationId, { page_size: 1 });
    setRelease(page.release_id);
    return page;
  }, [organizationId]);
  if (!active.loading && !active.error && release === null) return <NoCatalog what="Each model is listed once here, with every deployment of it side by side." />;
  return (
    <>
      {active.loading && <Message kind="info">Loading the catalog…</Message>}
      {active.error && <ReportError problem={active.error} what="catalog" onRetry={() => void active.load(true)} {...signIn} />}
      {release && (
        <ModelMasters
          {...signIn}
          organizationId={organizationId}
          release={release}
          familyLabels={familyLabels}
          canChange={canChange}
          onReleaseChanged={() => void active.load(true)}
        />
      )}
    </>
  );
}

/** The model names clients send, each with the deployment it reaches, from
 * the active release. */
function Aliases({ organizationId, canChange, ...signIn }: Props & { canChange: boolean }) {
  const [release, setRelease] = useState<string | null | undefined>(undefined);
  const [editing, setEditing] = useState<{ alias: string; operation: CatalogOperation } | null>(null);
  const [changed, setChanged] = useState<string | null>(null);
  // The active release, as the deployments list names it, so the aliases are
  // the ones the Models tab shows.
  const active = useCollection<CatalogDeployment>(async () => {
    const page = await api.listDeployments(organizationId, { page_size: 1 });
    setRelease(page.release_id);
    return page;
  }, [organizationId]);
  const routes = useCollection<CatalogRoute>(
    () => (release ? everyPage(params => api.listRoutes(organizationId, release, params)) : Promise.resolve({ items: [], next_page_token: null })),
    [organizationId, release],
  );
  const problem = active.error ?? routes.error;
  const ready = !!release && !active.loading && !routes.loading && !problem;
  const { health, readAt } = useDeploymentHealth(organizationId, ready ? routes.items.map(route => route.deployment_name) : []);
  const columns = React.useMemo(() => [...routeColumns, healthColumn(health)], [health]);
  if (!active.loading && !active.error && release === null) return <NoCatalog what="Aliases are the model names clients send; the catalog maps each to a deployment." />;
  const onChanged = (summary: string) => {
    setEditing(null);
    setChanged(summary);
    void active.load(true);
  };
  return (
    <>
      {changed && <Message kind="success">{changed}</Message>}
      <div className="section-header"><div><h2>Model aliases</h2><p className="muted">The model names clients send, and the deployments each one reaches. An alias can spread an operation over several deployments by weight, and fail over by priority. {healthReadAt(readAt)}</p></div></div>
      {!problem && !ready && <Message kind="info">Loading aliases…</Message>}
      {problem && <ReportError problem={problem} what="routes" onRetry={() => void (active.error ? active.load(true) : routes.load(true))} {...signIn} />}
      {ready && routes.items.length === 0 && <EmptyState title="No aliases" text="The active release routes no model names." />}
      {ready && routes.items.length > 0 && (
        <DataTable
          label="Model aliases"
          urlKey="aliases"
          columns={columns}
          rows={aliasRows(routes.items)}
          rowKey={row => row.id}
          actions={row => row.operations.map(operation => (
            <button key={operation} className="inline-action" onClick={() => setEditing({ alias: row.alias, operation })} aria-label={`Route ${row.alias} ${operationFormat(operation)}`}>
              Route{row.operations.length > 1 ? ` ${operationFormat(operation).toLowerCase()}` : ''}
            </button>
          ))}
        />
      )}
      {release && editing && (
        <EditRouteDialog {...signIn} organizationId={organizationId} releaseId={release} alias={editing.alias} operation={editing.operation} onClose={() => setEditing(null)} onChanged={onChanged} />
      )}
      <RoutingPolicySection {...signIn} organizationId={organizationId} canChange={canChange} familyLabels={familyLabels} />
    </>
  );
}

const operationFormat = (value: string) => operationLabels[value as CatalogOperation] ?? value;
const familyFormat = (value: string) => familyLabels[value as ProviderFamily] ?? value;
const routable = (op: CatalogDeployment['operations'][number]) => !!(op.capability && op.price);

/** A price per million tokens in major units (dollars), for range filters. */
function majorPerMillion(perToken: number, currency: string): number {
  return (perToken * 1_000_000) / 10 ** (6 + minorDigits(currency));
}

/** Batch operations (D-021) are priced apart: a column of their own. */
export const isBatch = (operation: CatalogOperation) => operation === 'chat_batch' || operation === 'embeddings_batch';
const singleOperations = (deployment: CatalogDeployment) => shownOperations(deployment.operations).filter(op => !isBatch(op.operation));
const batchOperations = (deployment: CatalogDeployment) => shownOperations(deployment.operations).filter(op => isBatch(op.operation));
const embeds = (operation: CatalogOperation) => operation === 'embeddings' || operation === 'embeddings_batch';

/** The lowest price of a dimension across a deployment's priced operations. */
function lowest(deployment: CatalogDeployment, pick: (price: CatalogPrice) => number, operations = singleOperations(deployment)): number | null {
  const prices = operations.flatMap(op => (op.price ? [majorPerMillion(pick(op.price), op.price.currency)] : []));
  return prices.length ? Math.min(...prices) : null;
}

/** One dimension's price per 1M tokens for each priced operation, as shown
 * in a price column; embeddings have no output price. */
function PriceCell({ deployment, pick }: { deployment: CatalogDeployment; pick: 'input' | 'output' }) {
  const ops = singleOperations(deployment).filter(op => !(pick === 'output' && embeds(op.operation)));
  if (!ops.length) return <span className="muted">—</span>;
  const several = ops.length > 1;
  return (
    <>
      {ops.map((op, i) => (
        <React.Fragment key={op.operation}>
          {i > 0 && <br />}
          {op.price ? perMillion(pick === 'input' ? op.price.input_minor_per_token : op.price.output_minor_per_token, op.price.currency) : <span className="muted">Unpriced</span>}
          {several && <small className="muted"> {operationLabels[op.operation]}</small>}
        </React.Fragment>
      ))}
    </>
  );
}

/** Each batch operation's input and output price per 1M tokens. */
export function BatchPriceCell({ deployment }: { deployment: CatalogDeployment }) {
  const ops = batchOperations(deployment);
  if (!ops.length) return <span className="muted">—</span>;
  const several = ops.length > 1;
  return (
    <>
      {ops.map((op, i) => (
        <React.Fragment key={op.operation}>
          {i > 0 && <br />}
          {op.price
            ? <>{perMillion(op.price.input_minor_per_token, op.price.currency)} in{embeds(op.operation) ? '' : <> · {perMillion(op.price.output_minor_per_token, op.price.currency)} out</>}</>
            : <span className="muted">Unpriced</span>}
          {several && <small className="muted"> {operationLabels[op.operation]}</small>}
        </React.Fragment>
      ))}
    </>
  );
}

const deploymentColumns: Column<CatalogDeployment>[] = [
  { id: 'model', label: 'Model', primary: true, value: deployment => deployment.model, search: deployment => deployment.name, title: deployment => deployment.name, render: deployment => <>{deployment.model}{namedApart(deployment) && <><br /><small className="muted">{deployment.name}</small></>}</> },
  { id: 'provider', label: 'Provider', filter: 'category', format: familyFormat, value: deployment => deployment.family, title: deployment => deployment.adapter_instance_key },
  { id: 'region', label: 'Region', filter: 'category', value: deployment => deployment.location },
  { id: 'account', label: 'Account', filter: 'category', hidden: true, value: deployment => deployment.adapter_instance_key },
  {
    id: 'operations',
    label: 'Type',
    filter: 'category',
    format: operationFormat,
    value: deployment => shownOperations(deployment.operations).map(op => op.operation),
    render: deployment => (
      <ul className="operation-list">
        {shownOperations(deployment.operations).map(op => (
          <li key={op.operation}>
            <Badge tone={routable(op) ? 'badge-success' : 'badge-neutral'} title={routable(op) ? 'Routable' : 'Missing a capability or price; not routable'}>{operationLabels[op.operation]}</Badge>
          </li>
        ))}
      </ul>
    ),
  },
  { id: 'input', label: 'Input per 1M', filter: 'number', value: deployment => lowest(deployment, price => price.input_minor_per_token), render: deployment => <PriceCell deployment={deployment} pick="input" /> },
  { id: 'output', label: 'Output per 1M', filter: 'number', value: deployment => lowest(deployment, price => price.output_minor_per_token, singleOperations(deployment).filter(op => !embeds(op.operation))), render: deployment => <PriceCell deployment={deployment} pick="output" /> },
  // Filtered by batch input price per 1M tokens.
  { id: 'batch', label: 'Batch per 1M', filter: 'number', value: deployment => lowest(deployment, price => price.input_minor_per_token, batchOperations(deployment)), render: deployment => <BatchPriceCell deployment={deployment} /> },
  {
    id: 'status',
    label: 'Status',
    filter: 'category',
    hidden: true,
    value: deployment => {
      const ops = shownOperations(deployment.operations);
      const count = ops.filter(routable).length;
      return count === ops.length ? 'Routable' : count ? 'Partly routable' : 'Not routable';
    },
  },
  {
    id: 'max_output',
    label: 'Max output tokens',
    filter: 'number',
    hidden: true,
    value: deployment => {
      const limits = shownOperations(deployment.operations).flatMap(op => (op.capability ? [op.capability.max_output_tokens] : []));
      return limits.length ? Math.max(...limits) : null;
    },
  },
];

/** The columns read from the model's card: its published context window
 * and when the provider retires it (a listed date, else the card's
 * commitment). */
export function cardColumns(cards: Map<string, ModelCardSummary>): Column<CatalogDeployment>[] {
  const of = (deployment: CatalogDeployment) => cards.get(deployment.model);
  return [
    {
      id: 'context',
      label: 'Context window',
      filter: 'number',
      value: deployment => {
        const value = of(deployment)?.context_window_tokens?.value;
        return typeof value === 'number' ? value : null;
      },
      render: deployment => {
        const fact = of(deployment)?.context_window_tokens;
        if (!fact) return <span className="muted">—</span>;
        return <span title={`From ${fact.source_url}, read ${fact.retrieved_at}`}>{typeof fact.value === 'number' ? fact.value.toLocaleString() : String(fact.value)}</span>;
      },
    },
    {
      id: 'retirement',
      label: 'Retires',
      value: deployment => {
        const card = of(deployment);
        return card?.retires_at ?? (card?.retirement ? String(card.retirement.value) : null);
      },
      render: deployment => {
        const card = of(deployment);
        if (card?.retires_at) return <span title="From the provider's listing of this account's deployments">{formatDate(card.retires_at)}</span>;
        if (card?.retirement) return <span title={`From ${card.retirement.source_url}, read ${card.retirement.retrieved_at}`}>{String(card.retirement.value)}</span>;
        return <span className="muted">—</span>;
      },
    },
  ];
}

/** Whether a deployment's name says more than its model: one Connect a
 * cloud generated (`<account>-<model>`) does not. */
export function namedApart(deployment: Pick<CatalogDeployment, 'name' | 'model' | 'adapter_instance_key'>): boolean {
  return deployment.name !== deployment.model && deployment.name !== deploymentName(deployment.adapter_instance_key, deployment.model);
}

/** An alias and the deployment it reaches, with every operation it routes
 * there: the catalog stores one route per operation. */
export type AliasRow = Pick<CatalogRoute, 'alias' | 'deployment_id' | 'deployment_name' | 'model' | 'family'> & {
  id: string;
  operations: CatalogOperation[];
  /** Weight and priority per operation; one entry when every operation agrees. */
  shares: { weight: number; priority: number }[];
  /** How many deployments the alias reaches. */
  deployments: number;
};

export function aliasRows(routes: CatalogRoute[]): AliasRow[] {
  const rows = new Map<string, AliasRow>();
  for (const route of routes) {
    const key = `${route.alias}\u0000${route.deployment_id}`;
    // A release stored before streaming became a mode of chat routes
    // `chat_stream` apart; it is chat to whoever calls the alias.
    const operation: CatalogOperation = route.operation === 'chat_stream' ? 'chat' : route.operation;
    const row = rows.get(key);
    const share = { weight: route.weight ?? 1, priority: route.priority ?? 1 };
    if (row) {
      if (!row.operations.includes(operation)) row.operations.push(operation);
      if (!row.shares.some(item => item.weight === share.weight && item.priority === share.priority)) row.shares.push(share);
    } else rows.set(key, { id: route.id, alias: route.alias, deployment_id: route.deployment_id, deployment_name: route.deployment_name, model: route.model, family: route.family, operations: [operation], shares: [share], deployments: 1 });
  }
  const perAlias = new Map<string, number>();
  for (const row of rows.values()) perAlias.set(row.alias, (perAlias.get(row.alias) ?? 0) + 1);
  for (const row of rows.values()) row.deployments = perAlias.get(row.alias) ?? 1;
  const order: CatalogOperation[] = ['chat', 'embeddings', 'chat_batch', 'embeddings_batch'];
  for (const row of rows.values()) row.operations.sort((a, b) => order.indexOf(a) - order.indexOf(b));
  return Array.from(rows.values());
}

/** Health is a column of its own, so it reads the load the table was given. */
const healthColumn = (health: HealthLoad): Column<AliasRow> => ({
  id: 'health',
  label: 'Health (15 min)',
  value: row => {
    const entry = healthFor(health, row.deployment_name);
    return typeof entry === 'object' ? entry.state : '';
  },
  render: row => <HealthBadge health={healthFor(health, row.deployment_name)} />,
});

const routeColumns: Column<AliasRow>[] = [
  { id: 'alias', label: 'Alias', primary: true, value: row => row.alias, render: row => <code>{row.alias}</code> },
  { id: 'operation', label: 'Type', filter: 'category', format: operationFormat, value: row => row.operations, render: row => row.operations.map(operationFormat).join(', ') },
  { id: 'deployment', label: 'Deployment', value: row => row.model, title: row => row.deployment_name },
  { id: 'provider', label: 'Provider', filter: 'category', format: familyFormat, value: row => row.family },
  { id: 'weight', label: 'Weight', value: row => row.shares[0].weight, render: row => (row.deployments > 1 ? row.shares.map(share => share.weight).join(' · ') : '—') },
  { id: 'priority', label: 'Priority', value: row => row.shares[0].priority, render: row => (row.deployments > 1 ? row.shares.map(share => share.priority).join(' · ') : '—') },
];

/** A deployment in full: each operation's limit and price, and its aliases. */
function DeploymentDetail({ deployment, routes }: { deployment: CatalogDeployment; routes: CatalogRoute[] | 'loading' | 'failed' }) {
  const aliases = typeof routes === 'string' ? [] : routes.filter(route => route.deployment_id === deployment.id);
  return (
    <div className="detail-grid">
      <dl className="detail-list">
        <dt>Deployment</dt><dd><code>{deployment.name}</code></dd>
        <dt>Provider account</dt><dd><code>{deployment.adapter_instance_key}</code> · {familyLabels[deployment.family]}</dd>
        <dt>Region</dt><dd>{deployment.location ?? '—'}</dd>
        <dt>Aliases</dt><dd>{routes === 'loading' ? <span className="muted">Loading aliases…</span> : routes === 'failed' ? <span className="muted">Aliases could not be loaded; see the Aliases tab.</span> : aliases.length ? aliasRows(aliases).map(row => <code key={row.id} className="detail-tag">{row.alias} · {row.operations.map(operationFormat).join(', ')}</code>) : <span className="muted">None; clients cannot reach it by an alias.</span>}</dd>
      </dl>
      <DataGrid size="sm" className="detail-table" aria-label="Per operation · price per 1M tokens">
        <caption className="muted">Per operation · price per 1M tokens</caption>
        <TableHead>
          <TableRow>
            {['Operation', 'Max output', 'Input', 'Output', 'Cached input', 'Reasoning', 'Source'].map(name => <TableHeader key={name}>{name}</TableHeader>)}
          </TableRow>
        </TableHead>
        <TableBody>
          {shownOperations(deployment.operations).map(op => (
            <TableRow key={op.operation}>
              <TableHeader scope="row">{operationLabels[op.operation]}</TableHeader>
              <DataCell>{op.capability ? op.capability.max_output_tokens.toLocaleString() : <span className="muted">No capability</span>}</DataCell>
              {op.price ? (
                <>
                  <DataCell>{perMillion(op.price.input_minor_per_token, op.price.currency)}</DataCell>
                  <DataCell>{embeds(op.operation) ? '—' : perMillion(op.price.output_minor_per_token, op.price.currency)}</DataCell>
                  <DataCell>{perMillion(op.price.cached_input_minor_per_token, op.price.currency)}</DataCell>
                  <DataCell>{embeds(op.operation) ? '—' : reasoningPrice(op.price.reasoning_minor_per_token, op.price.currency)}</DataCell>
                  <DataCell>{op.price.source} {op.price.source_version} · {op.price.confidence}</DataCell>
                </>
              ) : <DataCell colSpan={5} className="muted">Unpriced</DataCell>}
            </TableRow>
          ))}
        </TableBody>
      </DataGrid>
    </div>
  );
}


/** Every stored credential: there are few, and each account row needs its own. */
async function allCredentials(organizationId: string): Promise<Map<string, ProviderCredential>> {
  const byKey = new Map<string, ProviderCredential>();
  let token: string | undefined;
  do {
    const page = await api.listCredentials(organizationId, { page_size: 100, page_token: token });
    for (const credential of page.items) byKey.set(credential.adapter_instance_key, credential);
    token = page.next_page_token ?? undefined;
  } while (token);
  return byKey;
}

function Credentials({ organizationId, ...signIn }: Props) {
  const [release, setRelease] = useState<string | null | undefined>(undefined);
  const [credentials, setCredentials] = useState<Map<string, ProviderCredential>>(new Map());
  const [editing, setEditing] = useState<ProviderAccount | null>(null);
  const [deleting, setDeleting] = useState<ProviderCredential | null>(null);
  const [removing, setRemoving] = useState<ProviderAccount | null>(null);
  const [changed, setChanged] = useState<string | null>(null);
  const latest = useRef(0);
  const accounts = useCollection<ProviderAccount>(async params => {
    const request = ++latest.current;
    const [page, stored] = await Promise.all([
      api.listProviderAccounts(organizationId, params),
      params.page_token ? Promise.resolve(null) : allCredentials(organizationId),
    ]);
    if (request === latest.current) {
      if (!params.page_token) setRelease(page.release_id);
      if (stored) setCredentials(stored);
    }
    return page;
  }, [organizationId]);
  const reload = () => void accounts.load(true);
  if (!accounts.loading && !accounts.error && release === null) return <NoCatalog what="Credentials are kept for the provider accounts in the catalog." />;
  return (
    <>
      {changed && <Message kind="success">{changed}</Message>}
      <div className="section-header"><div><h2>Provider credentials</h2><p className="muted">Credentials are encrypted at rest and never shown again after you save them. Without one, a provider account uses the gateway’s own credential, if it has one.</p></div></div>
      {accounts.loading && <Message kind="info">Loading provider accounts…</Message>}
      {accounts.error && <ReportError problem={accounts.error} what="provider accounts" onRetry={reload} {...signIn} />}
      {!accounts.loading && !accounts.error && accounts.items.length === 0 && <EmptyState title="No provider accounts" text="The active release declares no provider accounts." />}
      {!accounts.loading && !accounts.error && accounts.items.length > 0 && (
        <DataGrid aria-label="Provider accounts">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Account</TableHeader>
              <TableHeader>Provider</TableHeader>
              <TableHeader>Credential</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {accounts.items.map(account => {
              const credential = credentials.get(account.adapter_instance_key);
              return (
                <TableRow className="data-row" key={account.id}>
                  <DataCell label="Account" className="cell-primary"><code>{account.adapter_instance_key}</code><br /><small className="muted">{account.deployment_count} deployment{account.deployment_count === 1 ? '' : 's'}</small></DataCell>
                  <DataCell label="Provider">{familyLabels[account.family]}</DataCell>
                  <DataCell label="Credential"><CredentialStatus credential={credential} family={account.family} /></DataCell>
                  <DataCell label="Actions">
                    <span className="row-actions">
                      <button className="inline-action" onClick={() => setEditing(account)}>{credential ? 'Replace' : 'Add'}</button>
                      {credential && <button className="inline-action" onClick={() => setDeleting(credential)}>Remove</button>}
                      <button className="inline-action" onClick={() => setRemoving(account)} aria-label={`Remove ${account.adapter_instance_key} from the catalog`}>Remove account</button>
                    </span>
                  </DataCell>
                </TableRow>
              );
            })}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={accounts.loadMore} hasMore={accounts.hasMore && !accounts.error} loading={accounts.loadingMore} />
      {editing && (
        <CredentialDialog
          {...signIn}
          organizationId={organizationId}
          account={editing}
          credential={credentials.get(editing.adapter_instance_key) ?? null}
          onClose={() => setEditing(null)}
          onSaved={() => { setEditing(null); reload(); }}
        />
      )}
      {release && removing && (
        <RemoveFromCatalogDialog
          {...signIn}
          organizationId={organizationId}
          releaseId={release}
          target={{ kind: 'provider', key: removing.adapter_instance_key }}
          onClose={() => setRemoving(null)}
          onChanged={summary => { setRemoving(null); setChanged(summary); reload(); }}
        />
      )}
      {deleting && <RemoveCredential {...signIn} organizationId={organizationId} credential={deleting} onClose={() => setDeleting(null)} onRemoved={() => { setDeleting(null); reload(); }} />}
    </>
  );
}

function CredentialStatus({ credential, family }: { credential: ProviderCredential | undefined; family: ProviderFamily }) {
  if (!credential) return <span className="muted">Gateway default</span>;
  // Sealed for the family the key had before; the gateway refuses it now.
  if (credential.family !== family) {
    return (
      <span className="field-error" title={`Version ${credential.version}, encryption key ${credential.key_id}`}>
        Stored for {familyLabels[credential.family]}: requests to this account fail until you replace it.
      </span>
    );
  }
  return (
    <span title={`Version ${credential.version}, encryption key ${credential.key_id}`}>
      <Badge kind="success">Stored</Badge> {credential.cloud_principal === 'pending' && <Badge kind="neutral" title="The gateway operator must bind this directory or service account to the organization (gateway cloud-principal allow) before it is used.">awaiting operator</Badge>}
      <br /><small className="muted">Updated {formatDate(credential.rotated_at)}</small>
    </span>
  );
}

function CredentialDialog({ organizationId, account, credential, onClose, onSaved, ...signIn }: SignIn & {
  organizationId: string;
  account: ProviderAccount;
  credential: ProviderCredential | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const modes = credentialModes(account.family);
  const [mode, setMode] = useState<CredentialMode>(modes[0]);
  const [values, setValues] = useState<Record<string, string>>({});
  const [errors, setErrors] = useState<Record<string, string>>({});
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // The same submission retried reuses its key, so a lost response replays.
  const attempt = useRef<{ body: string; key: string } | null>(null);
  // Typed secrets live only in this dialog's state: gone when it closes.
  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    const built = buildSecret(mode, values);
    if ('errors' in built) {
      setErrors(built.errors as Record<string, string>);
      return;
    }
    setErrors({});
    setProblem(null);
    setSubmitting(true);
    const body = JSON.stringify(built.secret);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    try {
      await api.setCredential(organizationId, account.adapter_instance_key, built.secret, credential?.version ?? null, attempt.current.key);
      setValues({});
      onSaved();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The credential could not be saved.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={`${credential ? 'Replace' : 'Add'} credential · ${account.adapter_instance_key}`}>
      <form onSubmit={submit} autoComplete="off">
        <div className="dialog-body">
          <p className="muted">{familyLabels[account.family]}. {credential ? 'The new credential replaces the stored one.' : 'Requests to this account will use it instead of the gateway’s own.'}</p>
          {modes.length > 1 && (
            <div className="checkbox-group" role="radiogroup" aria-label="How the gateway authenticates">
              {modes.map(option => (
                <RadioOption key={option.id} name="credential-mode" checked={mode.id === option.id} onChange={() => { setMode(option); setErrors({}); }}>{option.label}</RadioOption>
              ))}
            </div>
          )}
          <p className="cds--form__helper-text">{mode.description}</p>
          {mode.fields.map(field => (
            <FormField key={field.name} label={`${field.label}${field.optional ? ' (optional)' : ''}`} htmlFor={`credential-${field.name}`} hint={field.hint} error={errors[field.name]}>
              <TextInput
                id={`credential-${field.name}`}
                type={field.secret ? 'password' : 'text'}
                autoComplete={field.secret ? 'new-password' : 'off'}
                spellCheck={false}
                placeholder={field.example}
                value={values[field.name] ?? ''}
                onChange={event => setValues(previous => ({ ...previous, [field.name]: event.target.value }))}
              />
            </FormField>
          ))}
        </div>
        <ActionError problem={problem} action="change provider credentials" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Save credential</Button>
        </div>
      </form>
    </Dialog>
  );
}

function RemoveCredential({ organizationId, credential, onClose, onRemoved, ...signIn }: SignIn & { organizationId: string; credential: ProviderCredential; onClose: () => void; onRemoved: () => void }) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const key = useRef(newIdempotencyKey());
  const remove = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      await api.deleteCredential(organizationId, credential.adapter_instance_key, credential.version, key.current);
      onRemoved();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The credential could not be removed.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open alert onClose={() => { if (!submitting) onClose(); }} title="Remove credential">
      <div className="dialog-body"><p>Requests to <code>{credential.adapter_instance_key}</code> will use the gateway’s own credential, or fail if it has none.</p></div>
      <ActionError problem={problem} action="remove provider credentials" {...signIn} />
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button variant="danger" onClick={() => void remove()} loading={submitting}>Remove</Button>
      </div>
    </Dialog>
  );
}

function Releases({ organizationId, ...signIn }: Props) {
  const [activating, setActivating] = useState<CatalogRelease | null>(null);
  // The activation version as of the first page: what the list shows.
  const [activationVersion, setActivationVersion] = useState(0);
  const releases = useCollection<CatalogRelease>(async params => {
    const page = await api.listReleases(organizationId, params);
    if (!params.page_token) setActivationVersion(page.activation_version);
    return page;
  }, [organizationId]);
  return (
    <>
      <div className="section-header"><div><h2>Catalog releases</h2><p className="muted">Every change to the catalog (adding a model, changing a price) is saved as a new numbered release, and the newest is active. Activating an older release rolls the catalog back to it.</p></div></div>
      {releases.loading && <Message kind="info">Loading releases…</Message>}
      {releases.error && <ReportError problem={releases.error} what="catalog releases" onRetry={() => void releases.load(true)} {...signIn} />}
      {!releases.loading && !releases.error && releases.items.length === 0 && <EmptyState title="No releases" text="Each change to the catalog, such as adding a model under Connect a provider, publishes a new release." />}
      {!releases.loading && !releases.error && releases.items.length > 0 && (
        <DataGrid aria-label="Catalog releases">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Version</TableHeader>
              <TableHeader>Created</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader className="row-actions">Action</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {releases.items.map(release => (
              <TableRow className="data-row" key={release.id}>
                <DataCell label="Version" className="cell-primary" title={`sha256 ${release.digest}`}>{release.version}</DataCell>
                <DataCell label="Created" title={release.created_at}>{formatDate(release.created_at)}</DataCell>
                <DataCell label="Status">{release.active ? <><Badge status="active" /> since {formatDate(release.activated_at)}</> : release.sealed_at ? 'Available' : 'Not sealed'}</DataCell>
                <DataCell label="Action">{!release.active && release.sealed_at ? <button className="inline-action" onClick={() => setActivating(release)}>Activate</button> : '—'}</DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore onClick={releases.loadMore} hasMore={releases.hasMore && !releases.error} loading={releases.loadingMore} />
      {activating && <Activate {...signIn} organizationId={organizationId} release={activating} activationVersion={activationVersion} onReload={() => { setActivating(null); void releases.load(true); }} onClose={() => setActivating(null)} onActivated={() => { setActivating(null); void releases.load(true); }} />}
    </>
  );
}

function Activate({ organizationId, release, activationVersion, onReload, onClose, onActivated, ...signIn }: SignIn & { organizationId: string; release: CatalogRelease; activationVersion: number; onReload: () => void; onClose: () => void; onActivated: () => void }) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const key = useRef(newIdempotencyKey());
  const activate = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      await api.activateRelease(organizationId, release.id, activationVersion, key.current);
      onActivated();
    } catch (caught) {
      setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('The release could not be activated.'));
    } finally {
      setSubmitting(false);
    }
  };
  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={`Activate release ${release.version}`}>
      <div className="dialog-body"><p>New requests will route by this release’s models, aliases and prices straight away.</p></div>
      {problem?.code === 'version_conflict'
        ? <Message kind="error">Another release was activated since this list loaded. <button type="button" className="inline-action" onClick={onReload}>Reload releases</button> and check before activating.</Message>
        : <ActionError problem={problem} action="activate catalog releases" {...signIn} />}
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button onClick={() => void activate()} loading={submitting} disabled={problem?.code === 'version_conflict'}>Activate</Button>
      </div>
    </Dialog>
  );
}

function Trust({ organizationId, ...signIn }: Props) {
  const [trust, setTrust] = useState<CloudTrust | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setProblem(null);
    api.cloudTrust(organizationId).then(
      value => { if (current) setTrust(value); },
      caught => { if (current) setProblem((caught as { problem?: ApiProblem }).problem ?? loadFailed('Cloud trust settings could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  if (problem) return <ReportError problem={problem} what="cloud trust settings" onRetry={() => setAttempt(value => value + 1)} {...signIn} />;
  if (!trust) return <Message kind="info">Loading cloud trust settings…</Message>;
  return (
    <div className="cds--tile detail-panel">
      <div className="detail-header"><div><h3>Amazon Bedrock role</h3><p className="meta">Add these to the trust policy of the IAM role you give the gateway, so only this organization’s requests can assume it.</p></div></div>
      <dl className="detail-list">
        <dt>External ID</dt><dd><code>{trust.aws_external_id}</code> <CopyButton text={trust.aws_external_id} label="Copy external ID" /></dd>
        <dt>Session name</dt><dd><code>{trust.aws_role_session_name}</code> <small className="muted">as CloudTrail shows it</small></dd>
      </dl>
      <h3>Azure and Google</h3>
      <p className="meta">A Microsoft Entra directory or Google service account credential is used once the gateway operator binds it to this organization. Until then it shows as “awaiting operator” under Credentials.</p>
    </div>
  );
}
