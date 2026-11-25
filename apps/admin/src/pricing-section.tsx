import React, { useEffect, useRef, useState } from 'react';
import {
  allPages,
  api,
  newIdempotencyKey,
  type ApiProblem,
  type CatalogDeployment,
  type CatalogOperation,
  type PriceMapping,
  type PriceObservation,
  type PriceOverride,
  type PricePolicy,
  type PriceSource,
} from './api';
import { perMillion } from './add-provider-form';
import { billableFields, overrideBadge, overrideInput, overridePrices, priceFieldLabels, revocable, sourceStatus, type OverrideErrors, type OverrideForm } from './pricing-model';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { everyPage, useCollection } from './use-collection';
import { DataTable, type Column } from './data-table';
import { formatMoney, minorDigits } from './money';
import { MoreHint, SectionSwitcher, useSectionView } from './section-views';
import { Button, DataCell, Dialog, EmptyState, FormField, LoadMore, Select, TextArea, TextInput, formatDate, RadioOption, Badge, Message, CheckOption } from './ui';

type Props = SignIn & { organizationId: string; canChange: boolean };
const operationLabels: Record<CatalogOperation, string> = { chat: 'Chat', chat_stream: 'Streamed chat (this release routes it apart)', embeddings: 'Embeddings', chat_batch: 'Chat batch', embeddings_batch: 'Embeddings batch' };
const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

/** Whether prices follow their sources: on (the default), the gateway adopts
 * each published price that differs in a release it activates, holding a
 * change that looks wrong on the overview for an admin. */
export function AutomaticPrices({ organizationId, canChange, ...signIn }: Props) {
  const [policy, setPolicy] = useState<PricePolicy | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [saveProblem, setSaveProblem] = useState<ApiProblem | null>(null);
  const [saving, setSaving] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setProblem(null);
    api.pricePolicy(organizationId).then(
      value => { if (current) setPolicy(value); },
      caught => { if (current) setProblem(problemOf(caught, 'The price setting could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  const toggle = async () => {
    if (!policy) return;
    setSaving(true);
    setSaveProblem(null);
    try {
      setPolicy(await api.setPricePolicy(organizationId, !policy.auto_adopt, policy, newIdempotencyKey()));
    } catch (caught) {
      const failure = problemOf(caught, 'The price setting could not be saved.');
      setSaveProblem(failure);
      if (failure.code === 'version_conflict') setAttempt(value => value + 1);
    } finally {
      setSaving(false);
    }
  };
  return (
    <section className="cds--tile detail-panel pricing-auto" aria-labelledby="pricing-auto-heading">
      <h3 id="pricing-auto-heading">Automatic price updates</h3>
      {problem && <ReportError problem={problem} what="the price setting" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
      {policy && (
        <CheckOption checked={policy.auto_adopt} disabled={!canChange || saving} onChange={() => void toggle()}>
          Adopt published prices automatically
        </CheckOption>
      )}
      {policy && !policy.auto_adopt && <p className="muted">Off: differing prices are listed on the overview, and an admin adopts each one.</p>}
      {saveProblem?.code === 'version_conflict'
        ? <Message kind="error">Someone else changed this setting; it has been reloaded.</Message>
        : <ActionError problem={saveProblem} action="change the price setting" {...signIn} />}
      <MoreHint
        hint="New published prices are billed within the hour; suspicious changes wait for an admin."
        summary="When a change is held"
        more={<>
          When a price source publishes a new price, the gateway adopts it within the hour in a new release, so usage is billed at what the provider charges.
          A change that more than doubles a price, cuts it below half, makes it free, or that sources disagree on is held on the overview for an admin instead.
        </>}
      />
    </section>
  );
}

type PricingView = 'sources' | 'mappings' | 'overrides';
const pricingViews: [PricingView, string][] = [['sources', 'Price sources'], ['mappings', 'Mappings'], ['overrides', 'Overrides']];
/** Without a chosen view, the one whose table has a view in the URL. */
const pricingViewFromUrl = (): PricingView => {
  const names = [...new URLSearchParams(location.search).keys()];
  if (names.some(name => name.startsWith('price-overrides.'))) return 'overrides';
  if (names.some(name => name.startsWith('price-mappings.'))) return 'mappings';
  return 'sources';
};

/** Where each billed price comes from, one view at a time: the price sources
 * the gateway reads, which published item each deployment is compared with,
 * and overrides that replace a catalog price for a while. */
export function Pricing({ organizationId, canChange, ...signIn }: Props) {
  const [view, setView] = useSectionView<PricingView>('pricing.view', pricingViews.map(([key]) => key), pricingViewFromUrl);
  const [sources, setSources] = useState<PriceSource[] | null>(null);
  const [sourcesProblem, setSourcesProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [fetching, setFetching] = useState<string | null>(null);
  const [fetchNotice, setFetchNotice] = useState<string | null>(null);
  const [fetchProblem, setFetchProblem] = useState<ApiProblem | null>(null);
  const fetchNow = async (source: string) => {
    setFetching(source);
    setFetchNotice(null);
    setFetchProblem(null);
    try {
      const updated = await api.fetchPriceSource(organizationId, source, newIdempotencyKey());
      setSources(items => items?.map(item => (item.source === source ? updated : item)) ?? null);
      setFetchNotice(`${source} will be fetched within a minute. Reload the page to see the result.`);
    } catch (caught) {
      setFetchProblem(problemOf(caught, `${source} could not be fetched.`));
    } finally {
      setFetching(null);
    }
  };
  useEffect(() => {
    let current = true;
    setSourcesProblem(null);
    api.listPriceSources(organizationId).then(
      items => { if (current) setSources(items); },
      caught => { if (current) setSourcesProblem(problemOf(caught, 'The price sources could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  return (
    <>
      <div className="section-header">
        <div>
          <h2 className="cds--visually-hidden">Pricing</h2>
          <p className="muted">Where billed prices come from: the providers’ published prices, how each deployment is matched to them, and your overrides.</p>
        </div>
      </div>
      <SectionSwitcher label="Pricing views" views={pricingViews} view={view} onChange={setView} />
      <div className="section-view">
        {view === 'sources' && (
          <>
            <div className="section-header">
              <div>
                <h3>Price sources</h3>
                <p className="muted">
                  Providers’ published prices, read once a day. The gateway operator configures them for every organization.
                  {canChange && ' Fetch now reads one within a minute, such as after offering an account’s models automatically.'}
                </p>
              </div>
            </div>
            {fetchNotice && <Message kind="success">{fetchNotice}</Message>}
            {fetchProblem?.code === 'price_fetch_too_soon'
              ? <Message kind="error">That source was fetched in the last 10 minutes; try again in a few minutes.</Message>
              : <ActionError problem={fetchProblem} action="fetch price sources" {...signIn} />}
            {sourcesProblem && <ReportError problem={sourcesProblem} what="price sources" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
            {!sources && !sourcesProblem && <Message kind="info">Loading price sources…</Message>}
            {sources && sources.length === 0 && <EmptyState title="No price sources" text="The gateway operator has configured no price sources, so prices are not compared with what providers publish." />}
            {sources && sources.length > 0 && (
              <DataTable
                label="Price sources"
                urlKey="price-sources"
                columns={sourceColumns}
                rows={sources}
                rowKey={source => source.source}
                detail={source => <SourceDetail source={source} />}
                actions={canChange ? source => (
                  <Button variant="ghost" type="button" aria-label={`Fetch ${source.source} now`} loading={fetching === source.source} disabled={fetching !== null} onClick={() => void fetchNow(source.source)}>
                    Fetch now
                  </Button>
                ) : undefined}
              />
            )}
            <AutomaticPrices organizationId={organizationId} canChange={canChange} {...signIn} />
          </>
        )}
        {view === 'mappings' && <Mappings {...signIn} organizationId={organizationId} canChange={canChange} sources={sourcesProblem ? [] : sources} />}
        {view === 'overrides' && <Overrides {...signIn} organizationId={organizationId} canChange={canChange} />}
      </div>
    </>
  );
}

type MappingRow = { deployment: CatalogDeployment; source: PriceSource; mapping: PriceMapping | null };

function Mappings({ organizationId, canChange, sources, ...signIn }: SignIn & { organizationId: string; canChange: boolean; sources: PriceSource[] | null }) {
  const [deployments, setDeployments] = useState<CatalogDeployment[] | null>(null);
  const [mappings, setMappings] = useState<PriceMapping[]>([]);
  const [noRelease, setNoRelease] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [mapping, setMapping] = useState<MappingRow | null>(null);
  const [unmapping, setUnmapping] = useState<PriceMapping | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    setProblem(null);
    Promise.all([
      allPages<CatalogDeployment, Awaited<ReturnType<typeof api.listDeployments>>>(token => api.listDeployments(organizationId, { page_size: 100, page_token: token })),
      allPages<PriceMapping, Awaited<ReturnType<typeof api.listPriceMappings>>>(token => api.listPriceMappings(organizationId, { page_size: 100, page_token: token })),
    ]).then(
      ([listed, mapped]) => {
        if (!current) return;
        setDeployments(listed.items);
        setNoRelease(listed.first.release_id === null);
        setMappings(mapped.items);
      },
      caught => { if (current) setProblem(problemOf(caught, 'The price mappings could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  const reload = (message: string | null) => {
    setMapping(null);
    setUnmapping(null);
    setNotice(message);
    setAttempt(value => value + 1);
  };

  // Each active deployment against each source of its family.
  const rows: MappingRow[] = (deployments ?? []).flatMap(deployment =>
    (sources ?? []).filter(source => source.provider_family === deployment.family).map(source => ({
      deployment,
      source,
      mapping: mappings.find(item => item.deployment === deployment.name && item.source === source.source) ?? null,
    })));
  return (
    <>
      <div className="section-header">
        <div>
          <h3>Price mappings</h3>
          <MoreHint
            hint="Which published price list item each deployment is checked against."
            summary="Why map a deployment"
            more={<>
              Cloud price lists name products, not models, so the gateway needs to be told which list item matches each deployment (a mapping)
              before it can tell you when the published price changes. Without a mapping it looks for the deployment’s model by name. A mapping
              changes no price: Price changes under Models shows which prices differ, and adopts them.
            </>}
          />
        </div>
      </div>
      {notice && <Message kind="success">{notice}</Message>}
      {problem && <ReportError problem={problem} what="price mappings" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
      {!problem && (!deployments || !sources) && <Message kind="info">Loading price mappings…</Message>}
      {!problem && deployments && noRelease && <EmptyState title="No active catalog release" text="Mappings are for the deployments of the active release. Activate a release under Releases first." />}
      {!problem && deployments && sources && !noRelease && rows.length === 0 && (
        <EmptyState title="Nothing to map" text="No price source covers the provider families of the active release’s deployments." />
      )}
      {!problem && rows.length > 0 && (
        <DataTable
          label="Price mappings"
          urlKey="price-mappings"
          columns={mappingColumns}
          rows={rows}
          rowKey={row => `${row.deployment.name}/${row.source.source}`}
          actions={canChange ? row => (
            <>
              <button className="inline-action" onClick={() => { setNotice(null); setMapping(row); }} aria-label={`${row.mapping ? 'Change the mapping of' : 'Map'} ${row.deployment.name} at ${row.source.source}`}>{row.mapping ? 'Change' : 'Map'}</button>
              {row.mapping && <button className="inline-action" onClick={() => { setNotice(null); setUnmapping(row.mapping); }} aria-label={`Unmap ${row.deployment.name} at ${row.source.source}`}>Unmap</button>}
            </>
          ) : undefined}
        />
      )}
      {!canChange && <p className="muted">You can view price mappings. Only organization owners and admins can change them.</p>}
      {mapping && (
        <MapDialog {...signIn} organizationId={organizationId} row={mapping} onClose={() => setMapping(null)} onReload={() => reload(null)}
          onSaved={saved => reload(`${mapping.deployment.name} is now compared with ${saved.item} at ${saved.source}.`)} />
      )}
      {unmapping && (
        <UnmapDialog {...signIn} organizationId={organizationId} mapping={unmapping} onClose={() => setUnmapping(null)} onReload={() => reload(null)}
          onRemoved={() => reload(`${unmapping.deployment} is matched by model again at ${unmapping.source}.`)} />
      )}
    </>
  );
}

function MapDialog({ organizationId, row, onClose, onSaved, onReload, ...signIn }: SignIn & {
  organizationId: string;
  row: MappingRow;
  onClose: () => void;
  onSaved: (mapping: PriceMapping) => void;
  onReload: () => void;
}) {
  const [query, setQuery] = useState(row.mapping?.item ?? row.deployment.model);
  const [results, setResults] = useState<PriceObservation[] | null>(null);
  const [next, setNext] = useState<string | null>(null);
  const [searched, setSearched] = useState('');
  const [searching, setSearching] = useState(false);
  const [searchProblem, setSearchProblem] = useState<ApiProblem | null>(null);
  const [item, setItem] = useState(row.mapping?.item ?? '');
  // Which result was picked: a source can publish one item and region in
  // several currencies, so the item and region alone do not identify a row.
  const [picked, setPicked] = useState<number | null>(null);
  const [region, setRegion] = useState(row.mapping?.region ?? '');
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState('');
  // One attempt per choice: a retry replays rather than repeating it.
  const attempt = useRef<{ body: string; key: string } | null>(null);

  const search = async (event?: React.FormEvent, token?: string) => {
    event?.preventDefault();
    const text = token ? searched : query.trim();
    if (!text) return;
    setSearching(true);
    setSearchProblem(null);
    try {
      const page = await api.searchPriceObservations(organizationId, { q: text, source: row.source.source }, { page_size: 25, page_token: token });
      if (!token) setPicked(null);
      setResults(previous => (token && previous ? [...previous, ...page.items] : page.items));
      setNext(page.next_page_token ?? null);
      setSearched(text);
    } catch (caught) {
      setSearchProblem(problemOf(caught, 'The prices could not be searched. Check your connection and try again.'));
    } finally {
      setSearching(false);
    }
  };

  const save = async () => {
    if (!item.trim()) {
      setError('Choose a published price, or type its item.');
      return;
    }
    setError('');
    const input = { item: item.trim(), region: region.trim() || null };
    const body = JSON.stringify(input);
    if (attempt.current?.body !== body) attempt.current = { body, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      const { data } = await api.setPriceMapping(organizationId, row.deployment.name, row.source.source, input, row.mapping, attempt.current.key);
      onSaved(data);
    } catch (caught) {
      setProblem(problemOf(caught, 'The mapping could not be saved. Check your connection and try again.'));
      setSubmitting(false);
    }
  };

  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title={`Map ${row.deployment.name}`}>
      <div className="dialog-body">
        <p className="muted">Choose the price <code>{row.source.source}</code> publishes for <code>{row.deployment.model}</code>. Drift then compares the catalog’s price with it.</p>
        <form className="inline-form" onSubmit={event => void search(event)} role="search" aria-label="Search published prices">
          <label className="sr-only" htmlFor="mapping-search">Search published prices</label>
          <TextInput id="mapping-search" value={query} placeholder="Item or model" onChange={event => setQuery(event.target.value)} />
          <Button type="submit" variant="ghost" loading={searching}>Search</Button>
        </form>
        {searchProblem?.code === 'page_token_stale'
          ? <Message kind="error">The source published new prices while you were looking. <button type="button" className="inline-action" onClick={() => void search()}>Search again</button> from the first page.</Message>
          : searchProblem && <ActionError problem={searchProblem} action="search published prices" {...signIn} />}
        {searching && !results && <Message kind="info">Searching…</Message>}
        {results && results.length === 0 && <Message kind="info">No published price matches “{searched}”. Try a shorter search, or type the item below.</Message>}
        {results && results.length > 0 && (
          <fieldset className="plain-fieldset">
            <legend>Published prices matching “{searched}” (per 1M tokens)</legend>
            <ul className="price-results">
              {results.map((observation, index) => {
                const id = `price-result-${index}`;
                const per = (value: string | null) => (value === null ? '—' : `${perMillion(value, observation.per_tokens)} ${observation.currency}`);
                return (
                  <li key={id}>
                    <RadioOption id={id} name="mapping-item" checked={picked === index && item === observation.item && (region || null) === observation.region}
                        onChange={() => { setPicked(index); setItem(observation.item); setRegion(observation.region ?? ''); }}><span><code>{observation.item}</code>{observation.region ? ` · ${observation.region}` : ' · global'}<span className="cds--form__helper-text">in {per(observation.input)} · out {per(observation.output)}</span></span></RadioOption>
                  </li>
                );
              })}
            </ul>
            {next && <Button variant="ghost" type="button" loading={searching} onClick={() => void search(undefined, next)}>More results</Button>}
          </fieldset>
        )}
        <FormField label="Item" htmlFor="mapping-item" hint="As the source names it. It need not be published now; drift then reports no observation." error={error}>
          <TextInput id="mapping-item" value={item} onChange={event => setItem(event.target.value)} />
        </FormField>
        <FormField label="Region (optional)" htmlFor="mapping-region" hint="Empty compares with the deployment’s location, then the global price.">
          <TextInput id="mapping-region" value={region} onChange={event => setRegion(event.target.value)} />
        </FormField>
      </div>
      {problem?.code === 'version_conflict'
        ? <Message kind="error">Someone changed this mapping since the list loaded. <button type="button" className="inline-action" onClick={onReload}>Reload the mappings</button> and try again.</Message>
        : <ActionError problem={problem} action="change price mappings" {...signIn} />}
      <div className="dialog-actions">
        <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button type="button" onClick={() => void save()} loading={submitting}>Save mapping</Button>
      </div>
    </Dialog>
  );
}

function UnmapDialog({ organizationId, mapping, onClose, onRemoved, onReload, ...signIn }: SignIn & {
  organizationId: string;
  mapping: PriceMapping;
  onClose: () => void;
  onRemoved: () => void;
  onReload: () => void;
}) {
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const key = useRef(newIdempotencyKey());
  const remove = async () => {
    setProblem(null);
    setSubmitting(true);
    try {
      await api.deletePriceMapping(organizationId, mapping, key.current);
      onRemoved();
    } catch (caught) {
      setProblem(problemOf(caught, 'The mapping could not be removed. Check your connection and try again.'));
      setSubmitting(false);
    }
  };
  return (
    <Dialog open alert onClose={() => { if (!submitting) onClose(); }} title="Remove price mapping?">
      <div className="dialog-body"><p><code>{mapping.deployment}</code> will no longer be compared with <code>{mapping.item}</code>. Drift goes back to matching its model at <code>{mapping.source}</code>, which a public price list never does.</p></div>
      {problem?.code === 'version_conflict'
        ? <Message kind="error">Someone changed this mapping since the list loaded. <button type="button" className="inline-action" onClick={onReload}>Reload the mappings</button> and try again.</Message>
        : <ActionError problem={problem} action="change price mappings" {...signIn} />}
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>Cancel</Button>
        <Button variant="danger" onClick={() => void remove()} loading={submitting}>Unmap</Button>
      </div>
    </Dialog>
  );
}

function Overrides({ organizationId, canChange, ...signIn }: SignIn & { organizationId: string; canChange: boolean }) {
  const overrides = useCollection<PriceOverride>(() => everyPage(params => api.listPriceOverrides(organizationId, params)), [organizationId]);
  const [creating, setCreating] = useState(false);
  const [revoking, setRevoking] = useState<PriceOverride | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const reload = () => void overrides.load(true);
  return (
    <>
      <div className="section-header">
        <div>
          <h3>Price overrides</h3>
          <MoreHint
            hint="A price that replaces the catalog’s for one deployment and operation, for a while."
            summary="Which override applies"
            more={<>
              An override applies in whichever release is active, from its start until it ends or is revoked.
              When several apply, the one that started last wins. Requests already priced keep their price.
            </>}
          />
        </div>
        {canChange && <Button onClick={() => { setNotice(null); setCreating(true); }}>Add override</Button>}
      </div>
      {!canChange && <p className="muted">You can view price overrides. Only organization owners and admins can add or revoke them.</p>}
      {notice && <Message kind="success">{notice}</Message>}
      {overrides.loading && <Message kind="info">Loading price overrides…</Message>}
      {overrides.error && <ReportError problem={overrides.error} what="price overrides" onRetry={reload} {...signIn} />}
      {!overrides.loading && !overrides.error && overrides.items.length === 0 && <EmptyState title="No price overrides" text="Every request is charged at its catalog price." />}
      {!overrides.loading && !overrides.error && overrides.items.length > 0 && (
        <DataTable
          label="Price overrides"
          urlKey="price-overrides"
          columns={overrideColumns}
          rows={overrides.items}
          rowKey={override => override.id}
          detail={override => <OverrideDetail override={override} />}
          actions={canChange ? override => (revocable(override) ? (
            <button className="inline-action" onClick={() => { setNotice(null); setRevoking(override); }} aria-label={`Revoke the override of ${override.deployment} ${operationLabels[override.operation]}`}>Revoke</button>
          ) : null) : undefined}
        />
      )}
      {creating && (
        <OverrideDialog {...signIn} organizationId={organizationId} onClose={() => setCreating(false)}
          onCreated={created => { setCreating(false); setNotice(`Override added for ${created.deployment} (${created.status}).`); reload(); }} />
      )}
      {revoking && (
        <RevokeDialog {...signIn} organizationId={organizationId} override={revoking} onClose={() => setRevoking(null)} onReload={() => { setRevoking(null); reload(); }}
          onRevoked={() => { setRevoking(null); setNotice(`Override for ${revoking.deployment} revoked. Requests already priced by it keep its price.`); reload(); }} />
      )}
    </>
  );
}

function OverrideDialog({ organizationId, onClose, onCreated, ...signIn }: SignIn & { organizationId: string; onClose: () => void; onCreated: (override: PriceOverride) => void }) {
  const [deployments, setDeployments] = useState<CatalogDeployment[] | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [form, setForm] = useState<OverrideForm>({ deployment: '', operation: 'chat', currency: 'USD', prices: {}, reason: '', from: '', until: '' });
  const [errors, setErrors] = useState<OverrideErrors>({});
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  useEffect(() => {
    let current = true;
    allPages<CatalogDeployment, Awaited<ReturnType<typeof api.listDeployments>>>(token => api.listDeployments(organizationId, { page_size: 100, page_token: token })).then(
      ({ items }) => { if (current) setDeployments(items); },
      caught => { if (current) setLoadProblem(problemOf(caught, 'The deployments could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId]);
  const deployment = deployments?.find(item => item.name === form.deployment) ?? null;
  const fields = billableFields(deployment?.family ?? null, form.operation);
  const set = (change: Partial<OverrideForm>) => setForm(previous => ({ ...previous, ...change }));
  const choose = (name: string) => {
    const chosen = deployments?.find(item => item.name === name);
    const operation = chosen?.operations[0]?.operation ?? 'chat';
    set({ deployment: name, operation, currency: chosen?.operations.find(op => op.price)?.price?.currency ?? form.currency, prices: {} });
  };
  const built = overrideInput(form, deployment?.family ?? null);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
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
      const { data } = await api.createPriceOverride(organizationId, built.input, attempt.current.key);
      onCreated(data);
    } catch (caught) {
      setProblem(problemOf(caught, 'The override could not be added. Check your connection and try again.'));
      setSubmitting(false);
    }
  };

  return (
    <Dialog open onClose={() => { if (!submitting) onClose(); }} title="Add price override">
      <form onSubmit={submit}>
        <fieldset className="plain-fieldset dialog-body" disabled={submitting}>
          {loadProblem && <ActionError problem={loadProblem} action="list deployments" {...signIn} />}
          <FormField label="Deployment" htmlFor="override-deployment" error={errors.deployment}>
            <Select id="override-deployment" value={form.deployment} onChange={event => choose(event.target.value)} disabled={!deployments}>
              <option value="">{deployments ? 'Choose…' : 'Loading…'}</option>
              {(deployments ?? []).map(item => <option key={item.name} value={item.name}>{item.name} ({item.model})</option>)}
            </Select>
          </FormField>
          <FormField label="Operation" htmlFor="override-operation">
            <Select id="override-operation" value={form.operation} onChange={event => set({ operation: event.target.value as CatalogOperation })}>
              {/* The active release's own operations: one stored before streaming became a mode of chat still routes streamed requests through `chat_stream`, which an override must name to reprice them. */}
              {(deployment ? deployment.operations.map(op => op.operation) : (['chat', 'embeddings'] as CatalogOperation[])).map(operation => (
                <option key={operation} value={operation}>{operationLabels[operation]}</option>
              ))}
            </Select>
          </FormField>
          <FormField label="Currency" htmlFor="override-currency" error={errors.currency}>
            <TextInput id="override-currency" maxLength={3} value={form.currency} onChange={event => set({ currency: event.target.value })} />
          </FormField>
          <div className="form-grid">
            {fields.map(field => (
              <FormField key={field} label={`${priceFieldLabels[field]} per 1M tokens${field === 'input' || field === 'output' ? '' : ' (optional)'}`} htmlFor={`override-${field}`} error={errors[field]}>
                <TextInput id={`override-${field}`} inputMode="decimal" placeholder="0.00" value={form.prices[field] ?? ''} onChange={event => set({ prices: { ...form.prices, [field]: event.target.value } })} />
              </FormField>
            ))}
          </div>
          {fields.includes('reasoning') && <p className="cds--form__helper-text">Reasoning empty or 0: reasoning tokens are charged at the output price.</p>}
          {deployment?.family === 'anthropic' && <p className="cds--form__helper-text">Anthropic reports no reasoning tokens (thinking is output), so that price is zero. Other is the cache write price.</p>}
          {deployment?.family === 'bedrock' && <p className="cds--form__helper-text">Amazon Bedrock reports no reasoning tokens, so that price is zero. Other is the cache write price.</p>}
          <div className="form-grid">
            <FormField label="Starts (optional)" htmlFor="override-from" hint="Empty starts now; a past time also starts now.">
              <TextInput id="override-from" type="datetime-local" value={form.from} onChange={event => set({ from: event.target.value })} />
            </FormField>
            <FormField label="Ends (optional)" htmlFor="override-until" hint="Empty has no end." error={errors.window}>
              <TextInput id="override-until" type="datetime-local" value={form.until} onChange={event => set({ until: event.target.value })} />
            </FormField>
          </div>
          <FormField label="Reason" htmlFor="override-reason" hint="Recorded with the override and in the audit log." error={errors.reason}>
            <TextArea id="override-reason" rows={2} maxLength={1024} value={form.reason} onChange={event => set({ reason: event.target.value })} />
          </FormField>
          {'rounded' in built && built.rounded && <p className="cds--form__helper-text" role="status">A price is finer than the catalog can store, so it is rounded up and the override is marked conservative.</p>}
        </fieldset>
        <ActionError problem={problem} action="add price overrides" {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button type="submit" loading={submitting}>Add override</Button>
        </div>
      </form>
    </Dialog>
  );
}

function RevokeDialog({ organizationId, override, onClose, onRevoked, onReload, ...signIn }: SignIn & {
  organizationId: string;
  override: PriceOverride;
  onClose: () => void;
  onRevoked: () => void;
  onReload: () => void;
}) {
  const [reason, setReason] = useState('');
  const [error, setError] = useState('');
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const attempt = useRef<{ body: string; key: string } | null>(null);
  const revoke = async (event: React.FormEvent) => {
    event.preventDefault();
    const text = reason.trim();
    if (!text) {
      setError('Say why it is revoked.');
      return;
    }
    setError('');
    if (attempt.current?.body !== text) attempt.current = { body: text, key: newIdempotencyKey() };
    setProblem(null);
    setSubmitting(true);
    try {
      await api.revokePriceOverride(organizationId, override, text, attempt.current.key);
      onRevoked();
    } catch (caught) {
      setProblem(problemOf(caught, 'The override could not be revoked. Check your connection and try again.'));
      setSubmitting(false);
    }
  };
  const stale = problem?.code === 'already_revoked' || problem?.code === 'version_conflict';
  return (
    <Dialog open alert onClose={() => { if (!submitting) onClose(); }} title="Revoke price override?">
      <form onSubmit={revoke}>
        <div className="dialog-body">
          <p>
            This override stops applying to new requests for <code>{override.deployment}</code> ({operationLabels[override.operation]}). Another override
            may still set the price; otherwise the catalog price applies. Requests already priced by this override keep its price. Revocation is final.
          </p>
          <FormField label="Reason" htmlFor="revoke-reason" error={error}>
            <TextArea id="revoke-reason" rows={2} maxLength={1024} value={reason} disabled={submitting} onChange={event => setReason(event.target.value)} />
          </FormField>
        </div>
        {stale
          ? <Message kind="error">{problem!.detail || 'This override changed since the list loaded.'} <button type="button" className="inline-action" onClick={onReload}>Reload the overrides</button></Message>
          : <ActionError problem={problem} action="revoke price overrides" {...signIn} />}
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={submitting}>Cancel</Button>
          <Button variant="danger" type="submit" loading={submitting}>Revoke</Button>
        </div>
      </form>
    </Dialog>
  );
}

const operationFormat = (value: string) => operationLabels[value as CatalogOperation] ?? value;
const kindLabels: Record<PriceSource['kind'], string> = { 'price-sheet': 'Price sheet', 'azure-retail': 'Azure retail prices', 'aws-price-list': 'AWS price list' };

const sourceColumns: Column<PriceSource>[] = [
  { id: 'source', label: 'Source', primary: true, value: source => source.source, render: source => <code>{source.source}</code> },
  { id: 'kind', label: 'Kind', filter: 'category', hidden: true, format: value => kindLabels[value as PriceSource['kind']] ?? value, value: source => source.kind },
  {
    id: 'fetched',
    label: 'Last fetched',
    filter: 'date',
    value: source => source.last_success_at,
    title: source => (source.last_attempt_at ? `Last attempt ${formatDate(source.last_attempt_at)}` : undefined),
    render: source => (
      <>
        {source.last_success_at ? formatDate(source.last_success_at) : 'Never'}
        {source.snapshot?.source_version && <><br /><small className="muted">version {source.snapshot.source_version}</small></>}
      </>
    ),
  },
  {
    id: 'status',
    label: 'Status',
    filter: 'category',
    value: source => sourceStatus(source).label,
    render: source => {
      const status = sourceStatus(source);
      return <><Badge tone={status.badge}>{status.label}</Badge><br /><small className="muted">{status.text}</small></>;
    },
  },
  { id: 'prices', label: 'Prices published', filter: 'number', hidden: true, value: source => source.snapshot?.observations ?? null },
  { id: 'next', label: 'Next fetch', filter: 'date', value: source => source.next_fetch_at, render: source => formatDate(source.next_fetch_at) },
];

function SourceDetail({ source }: { source: PriceSource }) {
  const when = (value: string | null | undefined) => (value ? new Date(value).toLocaleString() : '—');
  return (
    <dl className="detail-list">
      <dt>Kind</dt><dd>{kindLabels[source.kind]} · {source.provider_family}</dd>
      <dt>Last attempt</dt><dd>{when(source.last_attempt_at)}{source.last_error_code && <> · <code>{source.last_error_code}</code></>}</dd>
      <dt>Last success</dt><dd>{when(source.last_success_at)}</dd>
      <dt>Snapshot</dt>
      <dd>{source.snapshot
        ? <>{source.snapshot.observations.toLocaleString()} prices, {source.snapshot.unmapped.toLocaleString()} not matched to a model · fetched {when(source.snapshot.fetched_at)}, unchanged through {when(source.snapshot.confirmed_at)}</>
        : <span className="muted">None yet</span>}</dd>
    </dl>
  );
}

const mappingColumns: Column<MappingRow>[] = [
  {
    id: 'deployment',
    label: 'Deployment',
    primary: true,
    value: row => row.deployment.model,
    search: row => row.deployment.name,
    render: row => <><code>{row.deployment.model}</code><span className="muted cell-note">{row.deployment.name}</span></>,
  },
  { id: 'source', label: 'Source', filter: 'category', value: row => row.source.source, render: row => <code>{row.source.source}</code> },
  { id: 'mapped', label: 'Mapped', filter: 'category', hidden: true, value: row => (row.mapping ? 'Mapped' : 'Matched by model') },
  {
    id: 'item',
    label: 'Compared with',
    value: row => row.mapping?.item ?? null,
    search: row => row.mapping?.region ?? '',
    render: row => (row.mapping
      ? <><code>{row.mapping.item}</code>{row.mapping.region && <span className="muted cell-note">region {row.mapping.region}</span>}</>
      : <span className="muted">Not mapped: matched by model</span>),
  },
];

const overrideColumns: Column<PriceOverride>[] = [
  {
    id: 'deployment',
    label: 'Deployment',
    primary: true,
    value: override => override.deployment,
    search: override => override.reason,
    render: override => <><code>{override.deployment}</code><span className="muted cell-note">{operationLabels[override.operation]} · {override.reason}</span></>,
  },
  { id: 'operation', label: 'Operation', filter: 'category', hidden: true, format: operationFormat, value: override => override.operation },
  {
    id: 'input',
    label: 'Price per 1M tokens',
    filter: 'number',
    // Filtered by input price per 1M tokens, in major units.
    value: override => (override.input_minor_per_token * 1_000_000) / 10 ** (6 + minorDigits(override.currency)),
    title: override => (override.confidence === 'conservative' ? 'Rounded up: conservative' : 'Exact'),
    render: override => overridePrices(override),
  },
  { id: 'from', label: 'Starts', filter: 'date', value: override => override.effective_from, render: override => formatDate(override.effective_from) },
  { id: 'until', label: 'Ends', filter: 'date', value: override => override.effective_until, render: override => (override.effective_until ? formatDate(override.effective_until) : 'no end') },
  {
    id: 'status',
    label: 'Status',
    filter: 'category',
    value: override => override.status,
    render: override => (
      <>
        <Badge tone={overrideBadge[override.status]}>{override.status}</Badge>
        {override.status === 'revoked' && override.revoke_reason && <span className="muted cell-note">{override.revoke_reason}</span>}
      </>
    ),
  },
];

function OverrideDetail({ override }: { override: PriceOverride }) {
  const per = (units: number) => formatMoney(units * 1_000_000, override.currency);
  const when = (value: string | null) => (value ? new Date(value).toLocaleString() : '—');
  return (
    <dl className="detail-list">
      <dt>Operation</dt><dd>{operationLabels[override.operation]}</dd>
      <dt>Per 1M tokens</dt>
      <dd>input {per(override.input_minor_per_token)} · output {per(override.output_minor_per_token)} · cached input {per(override.cached_input_minor_per_token)} · reasoning {override.reasoning_minor_per_token === 0 && !override.operation.startsWith('embeddings') ? '= output' : per(override.reasoning_minor_per_token)} · other {per(override.other_minor_per_token)} ({override.confidence})</dd>
      <dt>Reason</dt><dd>{override.reason}</dd>
      <dt>In effect</dt><dd>{when(override.effective_from)} – {override.effective_until ? when(override.effective_until) : 'no end'}</dd>
      <dt>Created</dt><dd>{when(override.created_at)}{override.created_by && <> by <code>{override.created_by}</code></>}</dd>
      {override.revoked_at && <><dt>Revoked</dt><dd>{when(override.revoked_at)}{override.revoked_by && <> by <code>{override.revoked_by}</code></>}{override.revoke_reason && ` · ${override.revoke_reason}`}</dd></>}
      <dt>Price version</dt><dd><code>{override.price_version}</code></dd>
    </dl>
  );
}
