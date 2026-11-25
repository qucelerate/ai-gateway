import React, { useEffect, useState } from 'react';
import { api, newIdempotencyKey, type ApiProblem, type CatalogManifest, type CatalogOperation, type PriceDrift, type PriceHold, type PriceHoldReason, type PricePolicy } from './api';
import { conflictMessage, foldStreaming, useCatalogChange, type CatalogEdit } from './catalog-change';
import {
  applyPublishedPrices,
  editDeployment,
  operationForms,
  operationProblem,
  brokenRoutes,
  removeDeployment,
  removeProvider,
  editRoute,
  routableDeployments,
  routeEntries,
  routeProblem,
  MAX_ROUTE_DEPLOYMENTS,
  takesToolsFlag,
  type RouteEntry,
  type OperationForm,
  type PublishedPrice,
} from './catalog-edit';
import { formatMoney } from './money';
import { HealthBadge, healthFor, healthReadAt, useDeploymentHealth } from './route-health';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { Button, Dialog, EmptyState, FormField, TextInput, CheckOption, Message, Select } from './ui';

const operationLabels: Record<CatalogOperation, string> = { chat: 'Chat', chat_stream: 'Streaming', embeddings: 'Embeddings', chat_batch: 'Chat batch', embeddings_batch: 'Embeddings batch' };

type Common = SignIn & { organizationId: string; onClose: () => void; onChanged: (summary: string) => void };

/** The manifest of the release the screen shows, to fill a form from. */
function useManifest(organizationId: string, releaseId: string) {
  const [manifest, setManifest] = useState<CatalogManifest | null>(null);
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setFailed(false);
    api.releaseManifest(organizationId, releaseId).then(
      // As the change will be made: streaming folded into chat.
      value => { if (current) setManifest(foldStreaming(value)); },
      () => { if (current) setFailed(true); },
    );
    return () => { current = false; };
  }, [organizationId, releaseId, attempt]);
  return { manifest, failed, retry: () => setAttempt(count => count + 1) };
}

/** What a change dialog shows while its submission is out or after it failed. */
export function ChangeFeedback({ change, ...signIn }: SignIn & { change: ReturnType<typeof useCatalogChange> }) {
  return (
    <>
      {change.invalid && <Message kind="error">{change.invalid}</Message>}
      {conflictMessage(change.problem)
        ? <Message kind="error">{conflictMessage(change.problem)}</Message>
        : <ActionError problem={change.problem} action="change the catalog" {...signIn} />}
    </>
  );
}

async function run(change: ReturnType<typeof useCatalogChange>, edit: CatalogEdit, summary: (version: string) => string, onChanged: (summary: string) => void) {
  const release = await change.submit(edit);
  if (release) onChanged(summary(release.version));
}

/** Limits and prices of one deployment's operations. */
export function EditDeploymentDialog({ organizationId, releaseId, name, model, onClose, onChanged, ...signIn }: Common & {
  releaseId: string;
  name: string;
  model: string;
}) {
  const { manifest, failed, retry } = useManifest(organizationId, releaseId);
  const [forms, setForms] = useState<OperationForm[] | null>(null);
  // The form as opened: only fields changed from it are saved.
  const [original, setOriginal] = useState<OperationForm[] | null>(null);
  const change = useCatalogChange(organizationId);
  useEffect(() => {
    if (manifest && !forms) {
      setForms(operationForms(manifest, name));
      setOriginal(operationForms(manifest, name));
    }
  }, [manifest, forms, name]);
  const update = (index: number, next: OperationForm) => {
    change.reset();
    setForms(current => current && current.map((form, i) => (i === index ? next : form)));
  };
  const problems = forms?.map(form => operationProblem(form)) ?? [];
  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    if (!forms || !original) return;
    void run(change, editDeployment(name, original, forms, new Date()), version => `Updated ${model}; release ${version} is active.`, onChanged);
  };
  return (
    <Dialog open onClose={() => { if (!change.submitting) onClose(); }} title={`Edit ${model}`}>
      <form onSubmit={submit}>
        <div className="dialog-body">
          <p className="muted">Changes apply to new requests once the new release is active. Prices are per million tokens.</p>
          {failed && <ReportError problem={loadFailed('The deployment could not be read.')} what="deployment" onRetry={retry} {...signIn} />}
          {!forms && !failed && <Message kind="info">Loading…</Message>}
          {forms && forms.length === 0 && <Message kind="error">{name} is no longer in this release.</Message>}
          <fieldset className="plain-fieldset" disabled={change.submitting}>
            {forms?.map((form, index) => {
              const id = `edit-${form.operation}`;
              const set = (part: 'limits' | 'price', field: string, value: string) => update(index, { ...form, [part]: { ...form[part], [field]: value } });
              return (
                <fieldset key={form.operation} className="plain-fieldset add-provider-model">
                  <legend>{operationLabels[form.operation]}</legend>
                  <div className="form-grid">
                    <FormField label={`Input, ${form.currency}`} htmlFor={`${id}-input`}>
                      <TextInput id={`${id}-input`} inputMode="decimal" value={form.price.input} onChange={event => set('price', 'input', event.target.value)} />
                    </FormField>
                    {form.operation !== 'embeddings' && (
                      <FormField label={`Output, ${form.currency}`} htmlFor={`${id}-output`}>
                        <TextInput id={`${id}-output`} inputMode="decimal" value={form.price.output} onChange={event => set('price', 'output', event.target.value)} />
                      </FormField>
                    )}
                    <FormField label={`Cached input, ${form.currency}`} htmlFor={`${id}-cached`}>
                      <TextInput id={`${id}-cached`} inputMode="decimal" value={form.price.cachedInput} onChange={event => set('price', 'cachedInput', event.target.value)} />
                    </FormField>
                  </div>
                  <div className="form-grid">
                    <FormField label="Maximum input, bytes" htmlFor={`${id}-max-input`}>
                      <TextInput id={`${id}-max-input`} inputMode="numeric" value={form.limits.maxInputBytes} onChange={event => set('limits', 'maxInputBytes', event.target.value)} />
                    </FormField>
                    <FormField label="Maximum output, tokens" htmlFor={`${id}-max-output`}>
                      <TextInput id={`${id}-max-output`} inputMode="numeric" value={form.limits.maxOutputTokens} onChange={event => set('limits', 'maxOutputTokens', event.target.value)} />
                    </FormField>
                    <FormField label="Reservation, seconds" htmlFor={`${id}-ttl`} hint="How long a request may hold budget before it settles.">
                      <TextInput id={`${id}-ttl`} inputMode="numeric" value={form.limits.reservationTtlSeconds} onChange={event => set('limits', 'reservationTtlSeconds', event.target.value)} />
                    </FormField>
                  </div>
                  {takesToolsFlag(form.operation) && (
                    <CheckOption id={`${id}-tools`} checked={form.tools} onChange={event => update(index, { ...form, tools: event.target.checked })}>
                      Tool calling: takes tool definitions and returns tool calls
                    </CheckOption>
                  )}
                  {problems[index] && <Message kind="error">{problems[index]}</Message>}
                </fieldset>
              );
            })}
          </fieldset>
        </div>
        <ChangeFeedback change={change} {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={change.submitting}>Cancel</Button>
          <Button type="submit" loading={change.submitting} disabled={!forms || forms.length === 0 || problems.some(Boolean)}>Save and activate</Button>
        </div>
      </form>
    </Dialog>
  );
}

/** The deployments one alias routes one operation to (D-023): each with a
 * weight, its share of its priority's traffic, and a priority, lower first.
 * Requests go to the lowest priority that has a healthy deployment. */
export function EditRouteDialog({ organizationId, releaseId, alias, operation, onClose, onChanged, ...signIn }: Common & {
  releaseId: string;
  alias: string;
  operation: CatalogOperation;
}) {
  const { manifest, failed, retry } = useManifest(organizationId, releaseId);
  const [original, setOriginal] = useState<RouteEntry[] | null>(null);
  const [entries, setEntries] = useState<RouteEntry[] | null>(null);
  const [adding, setAdding] = useState('');
  const change = useCatalogChange(organizationId);
  useEffect(() => {
    if (manifest && !entries) {
      const loaded = routeEntries(manifest, alias, operation);
      setOriginal(loaded);
      setEntries(loaded);
    }
  }, [manifest, entries, alias, operation]);
  const update = (next: RouteEntry[]) => {
    change.reset();
    setEntries(next);
  };
  // Read for the deployments in the dialog, so one just added has its own.
  const { health, readAt } = useDeploymentHealth(organizationId, entries?.map(entry => entry.deployment) ?? []);
  const candidates = manifest && entries ? routableDeployments(manifest, operation).filter(name => !entries.some(entry => entry.deployment === name)) : [];
  const problem = manifest && entries ? routeProblem(manifest, operation, entries) : null;
  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    if (!entries || !original) return;
    void run(change, editRoute(alias, operation, original, entries, new Date()), version => `Updated the routing of ${alias}; release ${version} is active.`, onChanged);
  };
  return (
    <Dialog open onClose={() => { if (!change.submitting) onClose(); }} title={`Route ${alias}`}>
      <form onSubmit={submit}>
        <div className="dialog-body">
          <p className="muted">
            {operationLabels[operation]} requests for <code>{alias}</code> go to the lowest priority that has a healthy deployment, split by weight.
            A deployment is skipped while its circuit is open, its provider asked the gateway to wait (429), or its availability evidence says it cannot be called.
            Every deployment answers within the tightest limits of the route.
            Health is how each deployment answered over the last 15 minutes. {healthReadAt(readAt)}
          </p>
          {failed && <ReportError problem={loadFailed('The routes could not be read.')} what="routes" onRetry={retry} {...signIn} />}
          {!entries && !failed && <Message kind="info">Loading…</Message>}
          <fieldset className="plain-fieldset" disabled={change.submitting}>
            {entries?.map((entry, index) => {
              const id = `route-${index}`;
              const set = (field: 'weight' | 'priority', value: string) => update(entries.map((item, i) => (i === index ? { ...item, [field]: value } : item)));
              return (
                <fieldset key={entry.deployment} className="plain-fieldset add-provider-model">
                  <legend>{entry.deployment} <HealthBadge health={healthFor(health, entry.deployment)} /></legend>
                  <div className="form-grid">
                    <FormField label="Weight" htmlFor={`${id}-weight`} hint="1 to 1000">
                      <TextInput id={`${id}-weight`} inputMode="numeric" value={entry.weight} onChange={event => set('weight', event.target.value)} />
                    </FormField>
                    <FormField label="Priority" htmlFor={`${id}-priority`} hint="1 (first) to 10">
                      <TextInput id={`${id}-priority`} inputMode="numeric" value={entry.priority} onChange={event => set('priority', event.target.value)} />
                    </FormField>
                  </div>
                  <Button variant="ghost" type="button" onClick={() => update(entries.filter((_, i) => i !== index))} aria-label={`Remove ${entry.deployment} from ${alias}`}>Remove</Button>
                </fieldset>
              );
            })}
            {entries && candidates.length > 0 && entries.length < MAX_ROUTE_DEPLOYMENTS && (
              <div className="form-grid">
                <FormField label="Add a deployment" htmlFor="route-add">
                  <Select id="route-add" value={adding} onChange={event => setAdding(event.target.value)}>
                    <option value="">Choose a deployment</option>
                    {candidates.map(name => <option key={name} value={name}>{name}</option>)}
                  </Select>
                </FormField>
                <Button variant="ghost" type="button" disabled={!adding} onClick={() => { update([...entries, { deployment: adding, weight: '1', priority: '1' }]); setAdding(''); }}>Add</Button>
              </div>
            )}
          </fieldset>
          {problem && <Message kind="error">{problem}</Message>}
        </div>
        <ChangeFeedback change={change} {...signIn} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={onClose} disabled={change.submitting}>Cancel</Button>
          <Button type="submit" loading={change.submitting} disabled={!entries || !!problem}>Save and activate</Button>
        </div>
      </form>
    </Dialog>
  );
}

/** Confirm removing a deployment, or a provider account with its deployments. */
export function RemoveFromCatalogDialog({ organizationId, releaseId, target, onClose, onChanged, ...signIn }: Common & {
  releaseId: string;
  target: { kind: 'deployment'; name: string; model: string } | { kind: 'provider'; key: string };
}) {
  const { manifest, failed, retry } = useManifest(organizationId, releaseId);
  const change = useCatalogChange(organizationId);
  const deployments = !manifest
    ? null
    : target.kind === 'deployment'
      ? [target.name]
      : manifest.deployments.filter(item => item.adapter_instance_key === target.key).map(item => item.name);
  const broken = manifest && deployments ? brokenRoutes(manifest, new Set(deployments)) : [];
  const label = target.kind === 'deployment' ? target.model : target.key;
  const remove = () => void run(
    change,
    target.kind === 'deployment' ? removeDeployment(target.name, new Date()) : removeProvider(target.key, new Date()),
    version => `Removed ${label}; release ${version} is active.`,
    onChanged,
  );
  return (
    <Dialog open alert onClose={() => { if (!change.submitting) onClose(); }} title={`Remove ${label} from the catalog?`}>
      <div className="dialog-body">
        {failed && <ReportError problem={loadFailed('The catalog could not be read.')} what="catalog" onRetry={retry} {...signIn} />}
        {!manifest && !failed && <Message kind="info">Loading…</Message>}
        {manifest && (
          <>
            <p>
              {target.kind === 'provider'
                ? <>The account and its {deployments!.length === 1 ? 'deployment' : `${deployments!.length} deployments`} leave the catalog in a new release. Its stored credential is kept, for adding it back.</>
                : <>The deployment, its prices and the aliases routed to it leave the catalog in a new release.</>}
              {' '}Earlier releases keep it: activating one under Releases brings it back.
            </p>
            {broken.length > 0 && (
              <Message kind="error">
                These requests will fail, as no other deployment serves them:{' '}
                {broken.map((route, i) => (
                  <React.Fragment key={route.alias}>
                    {i > 0 && '; '}<code>{route.alias}</code> ({route.operations.map(operation => operationLabels[operation].toLowerCase()).join(', ')})
                  </React.Fragment>
                ))}.
              </Message>
            )}
          </>
        )}
      </div>
      <ChangeFeedback change={change} {...signIn} />
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={change.submitting}>Cancel</Button>
        <Button variant="danger" onClick={remove} loading={change.submitting} disabled={!manifest}>Remove and activate</Button>
      </div>
    </Dialog>
  );
}

type Difference = PublishedPrice & { key: string; model: string; changes: { dimension: string; catalog: number; observed: string }[] };

/** The release's prices that differ from what their sources publish now. */
export function differences(drift: PriceDrift): { adoptable: Difference[]; overridden: number } {
  const adoptable: Difference[] = [];
  let overridden = 0;
  for (const price of drift.prices) {
    const comparison = price.comparisons.find(item => item.status === 'differs');
    if (!comparison) continue;
    // Billed through a price override: the release's price is not what is charged.
    if (price.price_source === 'override') {
      overridden += 1;
      continue;
    }
    const changed = comparison.dimensions.filter(item => item.status === 'differs' && item.observed !== null);
    adoptable.push({
      key: `${price.deployment}\u0000${price.operation}`,
      deployment: price.deployment,
      operation: price.operation,
      model: price.model,
      currency: price.currency,
      source: comparison.source,
      item: comparison.observed_item,
      observed: Object.fromEntries(changed.map(item => [item.dimension, item.observed!])),
      changes: changed.map(item => ({ dimension: item.dimension, catalog: item.catalog, observed: item.observed! })),
    });
  }
  // The change is made to the release with streaming folded into chat
  // (D-020): a `chat_stream` price adopts as its deployment's chat price,
  // unless the chat price is offered too, which then stands for both.
  const chat = new Set(adoptable.filter(item => item.operation === 'chat').map(item => item.deployment));
  const folded = adoptable.flatMap(item => item.operation !== 'chat_stream'
    ? [item]
    : chat.has(item.deployment) ? [] : [{ ...item, operation: 'chat' as const, key: `${item.deployment}\u0000chat` }]);
  return { adoptable: folded, overridden };
}

const dimensionLabels: Record<string, string> = { input: 'in', output: 'out', cached_input: 'cached', reasoning: 'reasoning', other: 'other' };

/** Per million tokens, from money units per token (a fraction allowed). */
function unitsPerMillion(units: string | number, currency: string): string {
  return formatMoney(Number(units) * 1_000_000, currency);
}

/** Why a published price was held rather than adopted automatically. */
export const holdReasons: Record<PriceHoldReason, string> = {
  large_increase: 'more than doubles',
  large_decrease: 'drops below half',
  to_zero: 'becomes free',
  sources_disagree: 'price sources disagree',
};

/** The adoptable difference a hold is for: streaming folds into chat as `differences` does. */
const holdKey = (hold: PriceHold) => `${hold.deployment}\u0000${hold.operation === 'chat_stream' ? 'chat' : hold.operation}`;

/** Prices whose provider now publishes another price, to adopt in a new
 * release. With automatic price updates on, only those held for an admin. */
export function PriceChanges({ organizationId, releaseId, onChanged, className, ...signIn }: SignIn & { organizationId: string; releaseId: string; onChanged: (summary: string) => void; className?: string }) {
  const [drift, setDrift] = useState<PriceDrift | null>(null);
  // Null when it could not be read: the changes are listed to adopt by hand.
  const [policy, setPolicy] = useState<PricePolicy | null>(null);
  const [holds, setHolds] = useState<PriceHold[]>([]);
  const [failed, setFailed] = useState(false);
  const [attempt, setAttempt] = useState(0);
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  const [dismissing, setDismissing] = useState<string | null>(null);
  const [dismissProblem, setDismissProblem] = useState<ApiProblem | null>(null);
  const [dismissed, setDismissed] = useState<string | null>(null);
  const change = useCatalogChange(organizationId);
  useEffect(() => {
    let current = true;
    setFailed(false);
    Promise.all([
      api.priceDrift(organizationId),
      api.pricePolicy(organizationId).catch(() => null),
    ]).then(async ([value, setting]) => {
      // With resolved holds, so a dismissed price is not shown as pending.
      const held = setting?.auto_adopt ? await api.priceHolds(organizationId, true).catch(() => null) : [];
      if (!current) return;
      setDrift(value);
      // Holds unreadable: adopting by hand stays possible.
      setPolicy(held === null ? null : setting);
      setHolds(held ?? []);
      setChosen(new Set());
    }, () => { if (current) setFailed(true); });
    return () => { current = false; };
  }, [organizationId, releaseId, attempt]);
  if (failed) return <ReportError problem={loadFailed('Published prices could not be compared.')} what="price changes" onRetry={() => setAttempt(count => count + 1)} {...signIn} />;
  if (!drift) return null;
  // The comparison is of the release active when it was read.
  if (drift.release_id !== releaseId) return null;
  const { adoptable: differing, overridden } = differences(drift);
  const automatic = policy?.auto_adopt === true;
  // An open hold stands for the current proposal whatever release it was
  // first found against; the worker keeps it in step.
  const held = automatic ? holds.filter(hold => hold.status === 'held') : [];
  const heldKeys = new Map(held.map(hold => [holdKey(hold), hold]));
  const adoptable = automatic ? differing.filter(item => heldKeys.has(item.key)) : differing;
  // Dismissed and still published as dismissed: left as it is, not pending.
  const kept = automatic
    ? differing.filter(item => !heldKeys.has(item.key) && holds.some(hold => hold.status === 'dismissed' && holdKey(hold) === item.key
      && (hold.reason === 'sources_disagree' || hold.changes.every(change => item.observed[change.dimension] === change.observed))))
    : [];
  const pending = automatic ? differing.length - adoptable.length - kept.length : 0;
  if (adoptable.length === 0 && overridden === 0 && pending === 0 && kept.length === 0 && !dismissed) return null;
  const dismiss = async (hold: PriceHold) => {
    setDismissing(hold.id);
    setDismissProblem(null);
    setDismissed(null);
    change.reset();
    try {
      await api.dismissPriceHold(organizationId, hold, newIdempotencyKey());
      setHolds(items => items.map(item => (item.id === hold.id ? { ...item, status: 'dismissed' as const } : item)));
      setDismissed(`${hold.model} keeps its price; this published price will not be held again.`);
    } catch (caught) {
      const problem = (caught as { problem?: ApiProblem }).problem ?? loadFailed('The held price could not be dismissed.');
      setDismissProblem(problem);
      // Adopted, superseded or dismissed meanwhile.
      if (problem.code === 'not_held' || problem.code === 'version_conflict') setAttempt(count => count + 1);
    } finally {
      setDismissing(null);
    }
  };
  const toggle = (key: string) => {
    change.reset();
    setChosen(current => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };
  const apply = () => void run(
    change,
    applyPublishedPrices(adoptable.filter(item => chosen.has(item.key)), new Date()),
    version => `Adopted ${chosen.size === 1 ? 'one published price' : `${chosen.size} published prices`}; release ${version} is active.`,
    onChanged,
  );
  return (
    <section className={className} aria-labelledby="price-changes-title">
      <div className="section-header"><div><h2 id="price-changes-title">Price changes</h2><p className="muted">{automatic
        ? 'Published prices held for review instead of adopted automatically. Adopting one bills it from the new release on; dismissing one keeps the current price.'
        : 'Prices that differ from what the provider publishes now. Adopting one bills it from the new release on; a price finer than the catalog’s unit is rounded up.'}</p></div></div>
      {pending > 0 && <p className="muted">{pending === 1 ? 'One published price' : `${pending} published prices`} will be adopted automatically within the hour.</p>}
      {kept.length > 0 && <p className="muted">{kept.length === 1 ? 'One dismissed published price is left as it is' : `${kept.length} dismissed published prices are left as they are`} until the source publishes another.</p>}
      {dismissed && <Message kind="success">{dismissed}</Message>}
      <ActionError problem={dismissProblem} action="dismiss the held price" {...signIn} />
      {adoptable.length === 0
        ? (!automatic && <EmptyState title="Nothing to adopt" text="Every differing price is billed through a price override." />)
        : (
          <fieldset className="plain-fieldset" disabled={change.submitting}>
            <ul className="price-results">
              {adoptable.map(item => (
                <li key={item.key}>
                  <CheckOption checked={chosen.has(item.key)} onChange={() => toggle(item.key)}><span>
                      <strong>{item.model}</strong> · {operationLabels[item.operation]}:{' '}
                      {item.changes.map((difference, i) => (
                        <React.Fragment key={difference.dimension}>
                          {i > 0 && ', '}{dimensionLabels[difference.dimension]} {unitsPerMillion(difference.catalog, item.currency)} → {unitsPerMillion(difference.observed, item.currency)}
                        </React.Fragment>
                      ))}
                      <span className="muted"> per 1M ({item.source})</span>
                      {heldKeys.has(item.key) && <span className="muted"> · held: {holdReasons[heldKeys.get(item.key)!.reason]}</span>}
                    </span></CheckOption>
                  {heldKeys.has(item.key) && (
                    <Button variant="ghost" loading={dismissing === heldKeys.get(item.key)!.id} disabled={dismissing !== null} onClick={() => void dismiss(heldKeys.get(item.key)!)}>Dismiss</Button>
                  )}
                </li>
              ))}
            </ul>
          </fieldset>
        )}
      {overridden > 0 && <p className="muted">{overridden === 1 ? 'One differing price is' : `${overridden} differing prices are`} billed through a price override; change the override instead.</p>}
      <ChangeFeedback change={change} {...signIn} />
      {adoptable.length > 0 && (
        <div className="dialog-actions">
          <Button onClick={apply} loading={change.submitting} disabled={chosen.size === 0}>Adopt {chosen.size || ''} and activate</Button>
        </div>
      )}
    </section>
  );
}
