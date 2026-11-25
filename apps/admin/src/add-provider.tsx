import React, { useEffect, useRef, useState } from 'react';
import {
  api,
  type CatalogOperation,
  type CatalogRelease,
  type PriceObservation,
  type ProviderAccount,
  type ProviderFamily,
  type Verification,
} from './api';
import {
  addProvider,
  choiceProblem,
  derived,
  fromObservation,
  newChoice,
  offeredOperations,
  perMillion,
  pricesCacheWrite,
  pricesReasoning,
  publishesNoPriceList,
  takesTools,
  type ModelChoice,
} from './add-provider-form';
import { conflictMessage, useCatalogChange } from './catalog-change';
import { ActionError, type SignIn } from './screen-feedback';
import { Button, FormField, TextInput, CheckOption, Message } from './ui';

const operationLabels: Record<CatalogOperation, string> = { chat: 'Chat', chat_stream: 'Streamed chat', embeddings: 'Embeddings', chat_batch: 'Chat batch', embeddings_batch: 'Embeddings batch' };
const SHOWN = 100;

/**
 * Add provider: the verified connection's account joins the catalog with
 * the models chosen from what verification listed, their prices and their
 * aliases. The new release is the active one plus this account; it is
 * created and then activated, so requests can use it at once.
 */
export function AddProvider({ organizationId, accountKey, family, verification, onAdded, ...signIn }: SignIn & {
  organizationId: string;
  accountKey: string;
  family: ProviderFamily;
  verification: Verification;
  /** Called once the release is active, with what was added and the
   * account it declares, so the connection can be saved for it at once. */
  onAdded: (summary: string, account: ProviderAccount) => void;
}) {
  const models = verification.models ?? [];
  const [filter, setFilter] = useState('');
  const [choices, setChoices] = useState<ModelChoice[]>([]);
  const [added, setAdded] = useState<CatalogRelease | null>(null);
  const change = useCatalogChange(organizationId);
  const { submitting, problem, invalid } = change;
  const body = JSON.stringify([accountKey, family, choices]);
  // Changed choices are a new release.
  useEffect(() => change.reset(), [body]); // eslint-disable-line react-hooks/exhaustive-deps

  const shown = models.filter(model => model.model.toLowerCase().includes(filter.trim().toLowerCase()));
  const chosen = (model: string) => choices.some(choice => choice.model === model);
  const toggle = (model: string) => setChoices(current => (
    current.some(choice => choice.model === model)
      ? current.filter(choice => choice.model !== model)
      : [...current, newChoice(family, model)]
  ));
  const update = (next: ModelChoice) => setChoices(current => current.map(choice => (choice.model === next.model ? next : choice)));

  const submit = async () => {
    const release = await change.submit(base => {
      const result = addProvider(base, { key: accountKey, family, location: verification.region ?? null, choices }, new Date());
      return 'problem' in result ? result : result.manifest;
    });
    if (!release) return;
    setAdded(release);
    onAdded(
      `Added ${accountKey} with ${choices.length === 1 ? 'one model' : `${choices.length} models`}; release ${release.version} is active.`,
      { id: '', release_id: release.id, adapter_instance_key: accountKey, family, deployment_count: choices.length },
    );
  };

  // Finish says what was added, next to saving the connection for it.
  if (added) return null;
  return (
    <section className="add-provider" aria-labelledby="add-provider-title">
      <h4 id="add-provider-title">Add it as a provider</h4>
      <p className="muted">Choose the models to offer. Each gets an alias, the name requests use, and its price, which budgets and usage are charged at. The catalog’s current release, plus <strong>{accountKey}</strong>, becomes the active one.</p>
      {models.length === 0
        ? <Message kind="info">The connection listed no models, so there is nothing to add yet.</Message>
        : (
          <>
            {/* What a click sends is captured when it starts: nothing can
                change until the server answers. */}
            <fieldset className="plain-fieldset" disabled={submitting}>
              {models.length > 10 && (
                <FormField label="Find a model" htmlFor="add-provider-filter">
                  <TextInput id="add-provider-filter" value={filter} onChange={event => setFilter(event.target.value)} placeholder="claude, gpt-4o, gemini…" />
                </FormField>
              )}
              <div className="checkbox-group" role="group" aria-label="Models">
                {shown.slice(0, SHOWN).map(model => (
                  <CheckOption key={model.model} checked={chosen(model.model)} onChange={() => toggle(model.model)}><code>{model.model}</code></CheckOption>
                ))}
              </div>
              {shown.length > SHOWN && <p className="muted">{shown.length - SHOWN} more match; narrow the search.</p>}
              {choices.map(choice => (
                <ChoiceForm key={choice.model} organizationId={organizationId} family={family} choice={choice} onChange={update} />
              ))}
            </fieldset>
            {invalid && <Message kind="error">{invalid}</Message>}
            {conflictMessage(problem)
              ? <Message kind="error">{conflictMessage(problem)}</Message>
              : <ActionError problem={problem} action="change the catalog" {...signIn} />}
            <div className="dialog-actions">
              <Button onClick={() => void submit()} loading={submitting} disabled={choices.length === 0 || choices.some(choice => choiceProblem(family, choice) !== null)}>
                Add {accountKey} and activate
              </Button>
            </div>
          </>
        )}
    </section>
  );
}

/** One chosen model: its alias, operations and prices. */
function ChoiceForm({ organizationId, family, choice, onChange }: {
  organizationId: string;
  family: ProviderFamily;
  choice: ModelChoice;
  onChange: (choice: ModelChoice) => void;
}) {
  const id = `add-${choice.model.replace(/[^A-Za-z0-9_-]/g, '-')}`;
  const operations = offeredOperations(family);
  const embeddingsOnly = choice.operations.every(operation => operation === 'embeddings');
  const chats = choice.operations.includes('chat');
  const batch = choice.operations.includes('chat_batch');
  const follows = derived(choice);
  const problem = choiceProblem(family, choice);
  const toggle = (operation: CatalogOperation) => onChange({
    ...choice,
    operations: choice.operations.includes(operation)
      ? choice.operations.filter(item => item !== operation)
      : operations.filter(item => item === operation || choice.operations.includes(item)),
  });
  // An edited price is the admin's; an entered cached price is no longer
  // assumed, and a batch price that followed the chat one no longer does.
  const price = (field: 'input' | 'output' | 'cachedInput' | 'cacheWrite' | 'reasoning' | 'batchInput' | 'batchOutput' | 'batchCachedInput', value: string) => onChange({
    ...choice,
    [field]: value,
    source: null,
    cachedAssumed: field === 'cachedInput' ? false : choice.cachedAssumed,
  });
  return (
    <fieldset className="plain-fieldset add-provider-model">
      <legend><code>{choice.model}</code></legend>
      <FormField label="Alias" htmlFor={`${id}-alias`} hint="The model name requests send.">
        <TextInput id={`${id}-alias`} value={choice.alias} onChange={event => onChange({ ...choice, alias: event.target.value })} />
      </FormField>
      <div className="checkbox-group" role="group" aria-label={`${choice.model} operations`}>
        {operations.map(operation => (
          <CheckOption key={operation} checked={choice.operations.includes(operation)} onChange={() => toggle(operation)}>{operationLabels[operation]}</CheckOption>
        ))}
      </div>
      {choice.operations.some(takesTools) && (
        <CheckOption id={`${id}-tools`} checked={choice.tools} onChange={() => onChange({ ...choice, tools: !choice.tools })}>
          Tool calling: takes tool definitions and returns tool calls
        </CheckOption>
      )}
      {publishesNoPriceList(family)
        ? <p className="muted">Anthropic and OpenAI publish no machine-readable price list: enter the model’s list price.</p>
        : <PriceLookup organizationId={organizationId} model={choice.model} onUse={observation => onChange(fromObservation(choice, observation))} />}
      <div className="form-grid">
        <FormField label={`Input, ${choice.currency} per 1M tokens`} htmlFor={`${id}-input`}>
          <TextInput id={`${id}-input`} inputMode="decimal" value={choice.input} onChange={event => price('input', event.target.value)} />
        </FormField>
        {!embeddingsOnly && (
          <FormField label={`Output, ${choice.currency} per 1M tokens`} htmlFor={`${id}-output`}>
            <TextInput id={`${id}-output`} inputMode="decimal" value={choice.output} onChange={event => price('output', event.target.value)} />
          </FormField>
        )}
        <FormField label={`Cached input, ${choice.currency} per 1M tokens`} htmlFor={`${id}-cached`} hint={family === 'anthropic' ? 'Cache reads: 0.1 times input on most models, less on some (see Anthropic’s pricing).' : undefined}>
          <TextInput id={`${id}-cached`} inputMode="decimal" value={choice.cachedInput} onChange={event => price('cachedInput', event.target.value)} />
        </FormField>
        {!embeddingsOnly && pricesCacheWrite(family) && (
          <FormField label={`Cache write, ${choice.currency} per 1M tokens`} htmlFor={`${id}-cache-write`} hint="Five-minute cache writes cost 1.25 times input; follows that until you change it.">
            <TextInput id={`${id}-cache-write`} inputMode="decimal" value={follows.cacheWrite} onChange={event => price('cacheWrite', event.target.value)} />
          </FormField>
        )}
        {chats && pricesReasoning(family) && (
          <FormField label={`Reasoning, ${choice.currency} per 1M tokens`} htmlFor={`${id}-reasoning`} hint="Empty or 0: reasoning tokens are charged at the output price.">
            <TextInput id={`${id}-reasoning`} inputMode="decimal" placeholder="0" value={choice.reasoning} onChange={event => price('reasoning', event.target.value)} />
          </FormField>
        )}
      </div>
      {batch && (
        <div className="form-grid">
          <FormField label={`Batch input, ${choice.currency} per 1M tokens`} htmlFor={`${id}-batch-input`} hint="Batches usually cost half; follows half the input price until you change it.">
            <TextInput id={`${id}-batch-input`} inputMode="decimal" value={follows.batchInput} onChange={event => price('batchInput', event.target.value)} />
          </FormField>
          <FormField label={`Batch output, ${choice.currency} per 1M tokens`} htmlFor={`${id}-batch-output`} hint={pricesReasoning(family) ? 'Batch reasoning tokens are charged at it too.' : undefined}>
            <TextInput id={`${id}-batch-output`} inputMode="decimal" value={follows.batchOutput} onChange={event => price('batchOutput', event.target.value)} />
          </FormField>
          <FormField label={`Batch cached input, ${choice.currency} per 1M tokens`} htmlFor={`${id}-batch-cached`}>
            <TextInput id={`${id}-batch-cached`} inputMode="decimal" value={follows.batchCachedInput} onChange={event => price('batchCachedInput', event.target.value)} />
          </FormField>
        </div>
      )}
      {family === 'anthropic' && <p className="muted">Anthropic counts thinking as output, so there is no reasoning price.{batch ? ' A batch’s cache writes are charged at half the cache write price.' : ''}</p>}
      {choice.source && <p className="muted">From {choice.source.name}: {choice.source.item}</p>}
      {problem && (choice.input || choice.output) && <Message kind="error">{problem}</Message>}
    </fieldset>
  );
}

/** Prices the gateway's price sources publish for a model. */
function PriceLookup({ organizationId, model, onUse }: {
  organizationId: string;
  model: string;
  onUse: (observation: PriceObservation) => void;
}) {
  const [query, setQuery] = useState(model);
  const [found, setFound] = useState<PriceObservation[] | null>(null);
  const [failed, setFailed] = useState(false);
  const [searching, setSearching] = useState(false);
  const latest = useRef(0);
  const search = async () => {
    const request = ++latest.current;
    setSearching(true);
    setFailed(false);
    try {
      const page = await api.priceObservations(organizationId, query.trim(), { page_size: 20 });
      if (latest.current === request) setFound(page.items);
    } catch {
      if (latest.current === request) setFailed(true);
    } finally {
      if (latest.current === request) setSearching(false);
    }
  };
  const id = `price-${model.replace(/[^A-Za-z0-9_-]/g, '-')}`;
  return (
    <div className="price-lookup">
      <FormField label="Find its price" htmlFor={id} hint="Searches the price lists this gateway syncs.">
        <div className="inline-form">
          <TextInput id={id} value={query} onChange={event => setQuery(event.target.value)} />
          <Button variant="ghost" onClick={() => void search()} loading={searching} disabled={!query.trim()}>Search</Button>
        </div>
      </FormField>
      {failed && <Message kind="error">Prices could not be searched; enter them below.</Message>}
      {found && found.length === 0 && <p className="muted">No published price matches; enter it below.</p>}
      {found && found.length > 0 && (
        <ul className="price-results">
          {found.map(observation => (
            <li key={`${observation.source}\u0000${observation.item}\u0000${observation.region ?? ''}`}>
              <span>
                <strong>{observation.item}</strong>
                {observation.region && <> · {observation.region}</>}
                {' · '}in {perMillion(observation.input, observation.per_tokens) ?? '—'} / out {perMillion(observation.output, observation.per_tokens) ?? '—'} {observation.currency} per 1M
                <span className="muted"> ({observation.source})</span>
              </span>
              <Button variant="ghost" onClick={() => onUse(observation)}>Use</Button>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
