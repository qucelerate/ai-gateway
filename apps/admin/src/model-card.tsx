import React, { useCallback, useEffect, useRef, useState } from 'react';
import { api, listFilter, newIdempotencyKey, type ApiProblem, type CardDeployment, type CardNote, type CardEvaluation, type CardFact, type GovernanceEntry, type ModelCard, type PublicCard, type RequestStats, type UsageAggregate } from './api';
import { formatMoney } from './money';
import { ReportError, loadFailed, type SignIn } from './screen-feedback';
import { AppLink, Badge, Button, DataCell, FormField, TextArea, TextInput, DataGrid, Message, TableBody, TableHead, TableHeader, TableRow, formatDate } from './ui';

/** `/admin/models/card/<model>`: the model a card page shows, or null. */
export function cardModelOf(pathname = location.pathname): string | null {
  const [, page, tab, model] = pathname.split('/').filter(Boolean);
  if (page !== 'models' || tab !== 'card' || !model) return null;
  try {
    return decodeURIComponent(model);
  } catch {
    return null;
  }
}

/** The path of `model`'s card. */
export const cardPath = (model: string) => `/admin/models/card/${encodeURIComponent(model)}`;

/** The model whose card the path names, following Back and Forward. */
export function useCardModel(): string | null {
  const [model, setModel] = useState(() => cardModelOf());
  useEffect(() => {
    const onPopState = () => setModel(cardModelOf());
    addEventListener('popstate', onPopState);
    return () => removeEventListener('popstate', onPopState);
  }, []);
  return model;
}

const operationLabels: Record<string, string> = { chat: 'Chat', chat_stream: 'Streaming', embeddings: 'Embeddings', chat_batch: 'Batch chat', embeddings_batch: 'Batch embeddings' };
/** Embeddings bill input only. */
const embeds = (operation: string) => operation === 'embeddings' || operation === 'embeddings_batch';
const availabilityLabels: Record<string, [string, 'success' | 'danger' | 'warning' | 'neutral']> = {
  verified: ['Verified', 'success'],
  entitled: ['Entitled', 'neutral'],
  refused: ['Refused', 'danger'],
  requires_action: ['Needs action', 'warning'],
};
const count = (value: number) => value.toLocaleString();
const ms = (value: number | null) => (value === null ? '—' : `${count(value)} ms`);
const perMillion = (perToken: number, currency: string) => formatMoney(perToken * 1_000_000, currency);
const bytes = (value: number | null) => (value === null ? '—' : value >= 1024 * 1024 ? `${(value / 1024 / 1024).toFixed(1)} MiB` : value >= 1024 ? `${Math.round(value / 1024)} KiB` : `${value} B`);

type Props = SignIn & { organizationId: string; model: string; familyLabels: Record<string, string>; /** Whether to offer editing the note (the server decides). */ canChange?: boolean };

export function ModelCardView({ organizationId, model, familyLabels, canChange = false, ...signIn }: Props) {
  const [card, setCard] = useState<ModelCard | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  // Only the latest load may show: Back and Forward between two cards can
  // leave an older request finishing last.
  const latest = useRef(0);
  // The load this render shows (a save started now applies only to it).
  const shown = latest.current;
  const load = useCallback(async () => {
    const sequence = ++latest.current;
    setProblem(null);
    setCard(null);
    try {
      const loaded = await api.modelCard(organizationId, model);
      if (sequence === latest.current) setCard(loaded);
    } catch (error) {
      if (sequence !== latest.current) return;
      // A problem body when the server sent one; otherwise its status.
      const { problem, status } = error as { problem?: ApiProblem; status?: number };
      setProblem(problem ?? { ...loadFailed(String(error)), status: status ?? 0 });
    }
  }, [organizationId, model]);
  useEffect(() => {
    void load();
    // Unmounted: nothing pending may show.
    return () => { latest.current += 1; };
  }, [load]);
  const family = (value: string) => familyLabels[value] ?? value;
  return (
    <div className="screen model-card">
      <p><AppLink to="/admin/models">← Models</AppLink></p>
      <div className="section-header">
        <div>
          <h2>{card?.overview.display_name ?? model}</h2>
          <p className="muted"><code>{model}</code></p>
        </div>
      </div>
      {!card && !problem && <Message kind="info">Loading the model card…</Message>}
      {problem?.status === 404 && <Message kind="warning">The active catalog release does not serve <code>{model}</code>.</Message>}
      {problem && problem.status !== 404 && <ReportError problem={problem} what="model card" onRetry={() => void load()} {...signIn} />}
      {card && (
        <>
          {card.public_card.card ? (
            <p className="muted">
              Public facts below come from {card.public_card.card.origin === 'imported' ? 'a Hugging Face model card this organization imported' : 'the card file'} <code>{card.public_card.card.id}</code>
              {card.public_card.card.origin === 'operator' ? ', added by this gateway\'s operator' : ''}, matched to {card.public_card.card.matched_deployments.join(', ')}. Each names its source and when it was read. They are shown as published, not checked or endorsed by this gateway.
            </p>
          ) : (
            <Message kind="info" title="No public card on file">
              Provider facts such as evaluations and safety are not on file for this model. What follows is what this gateway knows from its own catalog and traffic.
            </Message>
          )}
          {canChange && (!card.public_card.card || card.public_card.card.origin === 'imported') && (
            <HubImport organizationId={organizationId} model={model} imported={card.public_card.card?.origin === 'imported'} onChanged={() => void load()} />
          )}
          <section aria-labelledby="card-overview">
            <h3 id="card-overview">Overview</h3>
            {card.public_card.card && <Facts facts={card.public_card.card.overview} labels={overviewLabels} label="Published facts" />}
            <dl className="detail-list">
              <dt>Providers</dt><dd>{card.overview.provider_families.map(family).join(', ') || '—'}</dd>
              <dt>Operations</dt><dd>{card.overview.operations.map(op => operationLabels[op] ?? op).join(', ') || '—'}</dd>
              <dt>Locations</dt><dd>{card.overview.locations.join(', ') || '—'}</dd>
              <dt>Largest request</dt><dd>{bytes(card.overview.max_input_bytes)}</dd>
              <dt>Most output tokens</dt><dd>{card.overview.max_output_tokens === null ? '—' : count(card.overview.max_output_tokens)}</dd>
            </dl>
          </section>
          <section aria-labelledby="card-deployments">
            <h3 id="card-deployments">In this organization</h3>
            <p className="muted">Deployments of the active release that serve this model, what each costs per million tokens now, and whether calls to it work.</p>
            <Deployments deployments={card.deployments} family={family} />
          </section>
          <section aria-labelledby="card-content-filtering">
            <h3 id="card-content-filtering">Content filtering here</h3>
            <ContentFiltering deployments={card.deployments} />
          </section>
          {card.public_card.card && (card.public_card.card.evaluations.length > 0 || Object.keys(card.public_card.card.safety).length > 0) && (
            <>
              {card.public_card.card.evaluations.length > 0 && (
                <section aria-labelledby="card-evaluations">
                  <h3 id="card-evaluations">Evaluations</h3>
                  <Evaluations evaluations={card.public_card.card.evaluations} />
                </section>
              )}
              {Object.keys(card.public_card.card.safety).length > 0 && (
                <section aria-labelledby="card-safety">
                  <h3 id="card-safety">Safety</h3>
                  <Facts facts={card.public_card.card.safety} labels={safetyLabels} label="Safety" />
                </section>
              )}
            </>
          )}
          <section aria-labelledby="card-organization-evaluations">
            <h3 id="card-organization-evaluations">Organization evaluations</h3>
            {card.organization_evaluations.length === 0 ? (
              <p className="muted">This organization has not scored this model with an evaluation set of its own. Create one and score a batch under <AppLink to="/admin/models/evaluations">Models → Evaluations</AppLink>.</p>
            ) : (
              <>
                <p className="muted">This organization&apos;s own evaluation sets, scored from its batches: not a benchmark, and not comparable with the published evaluations above.</p>
                <DataGrid aria-label="Organization evaluations of this model">
                  <TableHead><TableRow><TableHeader>Set</TableHeader><TableHeader>Correct</TableHeader><TableHeader>Answered</TableHeader><TableHeader>Scored</TableHeader></TableRow></TableHead>
                  <TableBody>
                    {card.organization_evaluations.map(evaluation => (
                      <TableRow key={evaluation.set_id}>
                        <DataCell label="Set">{evaluation.set_name}</DataCell>
                        <DataCell label="Correct">{evaluation.correct} of {evaluation.items}{evaluation.items > 0 && <span className="muted"> ({Math.round((evaluation.correct / evaluation.items) * 100)}%)</span>}</DataCell>
                        <DataCell label="Answered">{evaluation.answered} of {evaluation.items}</DataCell>
                        <DataCell label="Scored"><span title={`Batch ${evaluation.batch_id}`}>{formatDate(evaluation.scored_at)}</span></DataCell>
                      </TableRow>
                    ))}
                  </TableBody>
                </DataGrid>
              </>
            )}
          </section>
          <section aria-labelledby="card-data-handling">
            <h3 id="card-data-handling">Data handling</h3>
            <DataHandling publicCard={card.public_card} gateway={card.gateway_data_handling} family={family} />
          </section>
          <section aria-labelledby="card-usage">
            <h3 id="card-usage">Usage here</h3>
            {card.usage ? <Usage usage={card.usage} model={model} /> : <Omitted what="Usage" who="owners, admins, operators, auditors and read-only members" />}
          </section>
          <section aria-labelledby="card-governance">
            <h3 id="card-governance">Governance</h3>
            <OrganizationNote organizationId={organizationId} model={model} note={card.organization_note} canChange={canChange} onSaved={note => {
              // A save that finishes after another card (or organization)
              // loaded belongs to the card it started on.
              if (shown === latest.current) setCard(current => current && { ...current, organization_note: note });
            }} />
            {card.governance ? <Governance governance={card.governance} /> : <Omitted what="Catalog history" who="owners, admins and auditors" />}
          </section>
          <p className="muted">Read from release <code>{card.release_id}</code> at {formatDate(card.generated_at)}.</p>
        </>
      )}
    </div>
  );
}

const overviewLabels: Record<string, string> = {
  release_date: 'Released',
  version: 'Version',
  family: 'Model family',
  modalities_in: 'Input',
  modalities_out: 'Output',
  context_window_tokens: 'Context window (tokens)',
  max_output_tokens: 'Max output (tokens)',
  retirement: 'Retirement',
  knowledge_cutoff: 'Knowledge cutoff',
  license: 'License',
  open_weight: 'Open weights',
  parameters: 'Parameters',
};
const safetyLabels: Record<string, string> = {
  model_card_url: 'Model or system card',
  safety_evaluations_url: 'Safety evaluations',
  acceptable_use_url: 'Acceptable use policy',
  intended_uses: 'Intended uses',
  prohibited_uses: 'Prohibited uses',
  known_limitations: 'Known limitations',
};
const dataHandlingLabels: Record<string, string> = {
  training_use: 'Training use',
  shared_with_model_provider: 'Shared with the model provider',
  provider_access: 'Provider access',
  retention: 'Retention',
  abuse_monitoring: 'Abuse monitoring',
  zero_data_retention: 'Zero data retention',
  residency: 'Residency',
  encryption: 'Encryption',
  terms_url: 'Terms',
};
const traceModes: Record<string, string> = { off: 'off', organization: 'on for the whole organization', selected: 'on for selected teams and service accounts' };

/** A fact's value as text; a URL value is a link. */
export function factValue(fact: CardFact): React.ReactNode {
  const { value } = fact;
  if (typeof value === 'boolean') return value ? 'Yes' : 'No';
  if (typeof value === 'number') return count(value);
  if (Array.isArray(value)) return value.join(', ');
  if (/^https:\/\//.test(value)) return <a href={value} target="_blank" rel="noreferrer noopener">{fact.title ?? value}</a>;
  return value;
}

function Source({ url, retrievedAt, reportedBy }: { url: string; retrievedAt: string; reportedBy: 'provider' | 'independent' }) {
  let host = url;
  try {
    host = new URL(url).host;
  } catch {
    // Shown as given.
  }
  return (
    <span className="muted card-source">
      {' '}
      (<a href={url} target="_blank" rel="noreferrer noopener">{host}</a>, read {retrievedAt}{reportedBy === 'provider' ? ', self-reported' : ', independent'})
    </span>
  );
}

function Facts({ facts, labels, label }: { facts: Record<string, CardFact>; labels: Record<string, string>; label: string }) {
  const keys = Object.keys(labels).filter(key => facts[key]);
  if (!keys.length) return null;
  return (
    <dl className="detail-list" aria-label={label}>
      {keys.map(key => (
        <React.Fragment key={key}>
          <dt>{labels[key]}</dt>
          <dd>{factValue(facts[key])}<Source url={facts[key].source_url} retrievedAt={facts[key].retrieved_at} reportedBy={facts[key].reported_by} /></dd>
        </React.Fragment>
      ))}
    </dl>
  );
}

function Evaluations({ evaluations }: { evaluations: CardEvaluation[] }) {
  const groups = [
    ['Self-reported', evaluations.filter(e => e.reported_by === 'provider')],
    ['Independent', evaluations.filter(e => e.reported_by === 'independent')],
  ] as const;
  return (
    <>
      {groups.filter(([, rows]) => rows.length).map(([title, rows]) => (
        <DataGrid key={title} aria-label={`${title} evaluations`}>
          <TableHead>
            <TableRow>
              <TableHeader>{title}</TableHeader>
              <TableHeader>Score</TableHeader>
              <TableHeader>Setting</TableHeader>
              <TableHeader>Evaluator</TableHeader>
              <TableHeader>Source</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.map(e => (
              <TableRow key={`${e.name}-${e.setting ?? ''}-${e.evaluator}`}>
                <DataCell label="Benchmark">{e.name}</DataCell>
                <DataCell label="Score">{e.score}</DataCell>
                <DataCell label="Setting">{e.setting ?? '—'}</DataCell>
                <DataCell label="Evaluator">{e.evaluator}</DataCell>
                <DataCell label="Source"><a href={e.source_url} target="_blank" rel="noreferrer noopener">read {e.retrieved_at}</a></DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      ))}
    </>
  );
}

function DataHandling({ publicCard, gateway, family }: { publicCard: PublicCard; gateway: ModelCard['gateway_data_handling']; family: (value: string) => string }) {
  return (
    <>
      {publicCard.data_handling.length === 0 && <p className="muted">No provider data-handling terms are on file for the providers serving this model. Check each provider&apos;s terms directly.</p>}
      {publicCard.data_handling.map(terms => (
        <div key={`${terms.provider_family}/${terms.service ?? ''}`}>
          <h4>{terms.name} <span className="muted">({family(terms.provider_family)}{terms.origin === 'operator' ? ', from the operator' : ''})</span></h4>
          {terms.applies_to && <p className="muted">{terms.applies_to}</p>}
          {terms.deployments.length > 0 && <p className="muted">Covers {terms.deployments.join(', ')}.</p>}
          <Facts facts={terms.facts} labels={dataHandlingLabels} label={`${terms.name} data handling`} />
        </div>
      ))}
      <h4>This gateway</h4>
      {gateway ? (
        <p>
          Tracing is {traceModes[gateway.trace_mode] ?? gateway.trace_mode}
          {gateway.trace_mode !== 'off' && (gateway.trace_content ? `, and the policy keeps traced content for up to ${gateway.trace_content_retention_days} days. Content is stored only when the gateway has a trace store, and the retention policy can delete it sooner` : ', for timing only, without content')}. <AppLink to="/admin/policies/tracing">Tracing settings</AppLink>
        </p>
      ) : (
        <Omitted what="This gateway's trace settings" who="owners, admins, operators, auditors and read-only members" />
      )}
    </>
  );
}

function Omitted({ what, who }: { what: string; who: string }) {
  return <p className="muted">{what} is not shown: your role cannot read it. It is shown to {who}.</p>;
}

function Deployments({ deployments, family }: { deployments: CardDeployment[]; family: (value: string) => string }) {
  return (
    <DataGrid aria-label="Deployments serving this model">
      <TableHead>
        <TableRow>
          <TableHeader>Deployment</TableHeader>
          <TableHeader>Provider</TableHeader>
          <TableHeader>Location</TableHeader>
          <TableHeader>Aliases</TableHeader>
          <TableHeader>Price per 1M tokens</TableHeader>
          <TableHeader>Availability</TableHeader>
          <TableHeader>Retirement</TableHeader>
        </TableRow>
      </TableHead>
      <TableBody>
        {deployments.map(deployment => {
          const availability = deployment.availability;
          const [label, kind] = availability ? availabilityLabels[availability.state] ?? [availability.state, 'neutral' as const] : ['Not yet checked', 'neutral' as const];
          return (
            <TableRow key={deployment.deployment}>
              <DataCell label="Deployment"><code>{deployment.deployment}</code></DataCell>
              <DataCell label="Provider">{family(deployment.provider_family)}<br /><span className="muted">{deployment.adapter_instance_key}</span></DataCell>
              <DataCell label="Location">{deployment.location ?? '—'}</DataCell>
              <DataCell label="Aliases">{deployment.routes.length ? deployment.routes.map(route => route.alias).filter((alias, i, all) => all.indexOf(alias) === i).join(', ') : '—'}</DataCell>
              <DataCell label="Price per 1M tokens">
                {deployment.operations.map(({ operation, price }) => (
                  <div key={operation}>
                    {operationLabels[operation] ?? operation}:{' '}
                    {price ? (
                      <>
                        {perMillion(price.input_minor_per_token, price.currency)} in
                        {!embeds(operation) && <> / {perMillion(price.output_minor_per_token, price.currency)} out</>}
                        {price.billed_by === 'override' && <> <Badge kind="warning" title={price.source_version ?? undefined}>Override</Badge></>}
                        {price.confidence === 'conservative' && <> <Badge kind="neutral">Conservative</Badge></>}
                      </>
                    ) : (
                      <span className="muted">not priced, so not routed</span>
                    )}
                  </div>
                ))}
              </DataCell>
              <DataCell label="Availability">
                <Badge kind={kind} title={availability ? `${availability.reason ?? availability.state}, from ${availability.source} on ${formatDate(availability.observed_at)}` : undefined}>{label}</Badge>
              </DataCell>
              <DataCell label="Retirement"><Retirement listing={deployment.provider_listing} /></DataCell>
            </TableRow>
          );
        })}
      </TableBody>
    </DataGrid>
  );
}

function Usage({ usage, model }: { usage: NonNullable<ModelCard['usage']>; model: string }) {
  type Row = { window: (typeof usage.windows)[number]; total: UsageAggregate | null };
  const rows = usage.windows.flatMap((window): Row[] => (window.totals.length ? window.totals.map(total => ({ window, total })) : [{ window, total: null }]));
  return (
    <>
      <DataGrid aria-label="Usage by window">
        <TableHead>
          <TableRow>
            <TableHeader>Window</TableHeader>
            <TableHeader>Requests</TableHeader>
            <TableHeader>Cost</TableHeader>
            <TableHeader>Input tokens</TableHeader>
            <TableHeader>Output tokens</TableHeader>
            <TableHeader>Cached input</TableHeader>
          </TableRow>
        </TableHead>
        <TableBody>
          {rows.map(({ window, total }) => (
            <TableRow key={`${window.days}-${total?.currency ?? ''}-${total?.confidence ?? ''}`}>
              <DataCell label="Window">Last {window.days} days</DataCell>
              <DataCell label="Requests">{count(total?.request_count ?? 0)}</DataCell>
              <DataCell label="Cost">{total ? <>{formatMoney(total.amount_minor, total.currency)}{total.confidence !== 'exact' && <> <Badge kind="neutral">{total.confidence}</Badge></>}</> : '—'}</DataCell>
              <DataCell label="Input tokens">{total ? count(total.tokens.input) : '—'}</DataCell>
              <DataCell label="Output tokens">{total ? count(total.tokens.output) : '—'}</DataCell>
              <DataCell label="Cached input">{total && total.tokens.input > 0 ? `${Math.round((100 * total.tokens.cached_input) / total.tokens.input)}%` : '—'}</DataCell>
            </TableRow>
          ))}
        </TableBody>
      </DataGrid>
      <p className="muted">The same figures the <AppLink to={`/admin/usage?u.range=30d&u.f.model=${encodeURIComponent(listFilter([model])!)}`}>usage dashboard</AppLink> gives for this model, for the teams you can see.</p>
      <Requests requests={usage.requests} />
    </>
  );
}

function Requests({ requests }: { requests: RequestStats }) {
  const safety = requests.finish_reasons.filter(reason => reason.safety).reduce((sum, reason) => sum + reason.count, 0);
  return (
    <>
      <h4>Requests in the last {requests.days} days</h4>
      <dl className="detail-list">
        <dt>Requests</dt><dd>{count(requests.request_count)}</dd>
        <dt>Failed</dt>
        <dd>
          {count(requests.failed_count)}
          {requests.errors.length > 0 && <> ({requests.errors.map(error => `${error.key.replace(/_/g, ' ')} ${count(error.count)}`).join(', ')})</>}
        </dd>
        <dt>Latency</dt><dd>{requests.latency_p50_ms === null ? '—' : <>{ms(requests.latency_p50_ms)} median, {ms(requests.latency_p95_ms)} p95</>}</dd>
        <dt>First token (streams)</dt><dd>{requests.first_event_p50_ms === null ? '—' : <>{ms(requests.first_event_p50_ms)} median, {ms(requests.first_event_p95_ms)} p95</>}</dd>
        <dt>Stopped by safety</dt><dd>{count(safety)}</dd>
      </dl>
      {requests.finish_reasons.length > 0 && (
        <p>
          Finish reasons:{' '}
          {requests.finish_reasons.map(reason => (
            <span key={reason.reason} className="finish-reason">
              <Badge kind={reason.safety ? 'warning' : 'neutral'}>{reason.reason} {count(reason.count)}</Badge>{' '}
            </span>
          ))}
        </p>
      )}
      <p className="muted">
        {requests.timed_since
          ? `Latency and finish reasons cover ${count(requests.timed_request_count)} requests since ${formatDate(requests.timed_since)}; earlier requests were not timed.`
          : 'No request in this window has latency or a finish reason recorded yet.'}
      </p>
    </>
  );
}

function Governance({ governance }: { governance: NonNullable<ModelCard['governance']> }) {
  if (!governance.history.length) return <p className="muted">No recorded catalog activation added or changed this model.</p>;
  return (
    <>
      {governance.first_release && <p>First served by release <code>{governance.first_release.release_version}</code>, activated {formatDate(governance.first_release.created_at)}{actorOf(governance.first_release)}.</p>}
      <ol className="governance-history" aria-label="Catalog changes to this model">
        {governance.history.map(entry => (
          <li key={`${entry.release_id}-${entry.created_at}`}>
            <strong>{formatDate(entry.created_at)}</strong>: release <code>{entry.release_version}</code>{actorOf(entry)}
            {entry.added.length > 0 && <> added {entry.added.join(', ')}.</>}
            {entry.changed.length > 0 && <> changed {entry.changed.join(', ')}.</>}
            {entry.removed.length > 0 && <> removed {entry.removed.join(', ')}.</>}
          </li>
        ))}
      </ol>
      {governance.truncated && <p className="muted">Older changes are in the <AppLink to="/admin/audit">audit log</AppLink>.</p>}
    </>
  );
}

/** The filtering each deployment applies, where the cloud reports it. */
function ContentFiltering({ deployments }: { deployments: CardDeployment[] }) {
  const filtered = deployments.filter(deployment => deployment.provider_listing?.content_filter);
  const unreported = deployments.filter(deployment => !deployment.provider_listing?.content_filter).map(deployment => deployment.deployment);
  return (
    <>
      {filtered.map(deployment => {
        const listing = deployment.provider_listing!;
        const policy = listing.content_filter_policy;
        return (
          <div key={deployment.deployment}>
            <h4><code>{deployment.deployment}</code>: {listing.content_filter}{policy?.system_policy ? <span className="muted"> (the cloud&apos;s default)</span> : null}</h4>
            {policy ? (
              <>
                {(policy.base_policy || policy.mode) && <p className="muted">{policy.base_policy ? `Based on ${policy.base_policy}. ` : ''}{policy.mode ? `Mode: ${policy.mode}.` : ''}</p>}
                {policy.filters.length > 0 && (
                  <DataGrid aria-label={`Content filters of ${deployment.deployment}`}>
                    <TableHead><TableRow><TableHeader>Filter</TableHeader><TableHeader>Applies to</TableHeader><TableHeader>Filters from</TableHeader><TableHeader>Action</TableHeader></TableRow></TableHead>
                    <TableBody>
                      {policy.filters.map((filter, index) => (
                        <TableRow key={`${filter.name}-${filter.source ?? ''}-${index}`}>
                          <DataCell label="Filter">{filter.name}</DataCell>
                          <DataCell label="Applies to">{filter.source ?? '—'}</DataCell>
                          <DataCell label="Filters from">{filter.enabled === false ? 'Off' : filter.severity_threshold ? `${filter.severity_threshold} severity` : 'On'}</DataCell>
                          <DataCell label="Action">{filter.enabled === false ? '—' : filter.blocking === false ? 'Annotates only' : filter.blocking ? 'Blocks' : '—'}</DataCell>
                        </TableRow>
                      ))}
                    </TableBody>
                  </DataGrid>
                )}
              </>
            ) : (
              <p className="muted">The policy&apos;s settings could not be read from the account; see it in the cloud&apos;s console.</p>
            )}
          </div>
        );
      })}
      {unreported.length > 0 && <p className="muted">{filtered.length ? 'The other deployments' : 'These deployments'} ({unreported.join(', ')}) report no content filter policy. Their provider may still filter content; see its terms in Data handling.</p>}
    </>
  );
}

/** What the provider's listing says of a deployment's model version. */
function Retirement({ listing }: { listing: CardDeployment['provider_listing'] }) {
  if (!listing || (!listing.retires_at && !listing.lifecycle_status)) return <span className="muted">Not listed</span>;
  const listed = `From the provider's listing on ${formatDate(listing.listed_at)}${listing.version ? `, version ${listing.version}` : ''}`;
  return (
    <span title={listed}>
      {listing.retires_at ? formatDate(listing.retires_at) : '—'}
      {listing.lifecycle_status && <> <Badge kind={/deprecat|retir/i.test(listing.lifecycle_status) ? 'warning' : 'neutral'}>{listing.lifecycle_status}</Badge></>}
    </span>
  );
}

/** Import a Hugging Face model card for a model no card file covers, or
 * remove the import. */
function HubImport({ organizationId, model, imported, onChanged }: { organizationId: string; model: string; imported: boolean; onChanged: () => void }) {
  const [open, setOpen] = useState(false);
  const [repository, setRepository] = useState('');
  const [provider, setProvider] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // One key and one If-Match per attempt at the same change: a retry after
  // a lost response sends the same request, so it replays.
  const attempt = useRef<{ what: string; key: string; ifMatch: string | null } | null>(null);
  const run = async (what: string, action: (key: string, ifMatch: string) => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      if (attempt.current?.what !== what) attempt.current = { what, key: newIdempotencyKey(), ifMatch: null };
      const current = attempt.current;
      current.ifMatch ??= await api.modelCardImportMatch(organizationId, model);
      await action(current.key, current.ifMatch);
      attempt.current = null;
      setOpen(false);
      onChanged();
    } catch (failure) {
      const { problem } = failure as { problem?: ApiProblem };
      // A conflict is answered: someone changed the import meanwhile, so
      // the next try reads its version afresh, as a new attempt. Any other
      // failure may have lost a committed response, so a retry resends the
      // same request.
      // (Other refusals, such as imports being off, keep the server's words.)
      const conflict = problem?.code === 'version_conflict';
      if (conflict) attempt.current = null;
      setError(conflict ? 'Someone changed the imported card meanwhile. Try again to apply yours to the current one.' : problem?.detail ?? String(failure));
    } finally {
      setBusy(false);
    }
  };
  if (imported) {
    return (
      <p>
        <Button type="button" variant="ghost" loading={busy} onClick={() => void run('remove', (key, ifMatch) => api.removeModelCardImport(organizationId, model, key, ifMatch))}>Remove imported card</Button>
        {error && <span className="field-error"> {error}</span>}
      </p>
    );
  }
  if (!open) return <p><Button type="button" variant="ghost" onClick={() => setOpen(true)}>Import from Hugging Face</Button></p>;
  return (
    <form className="hub-import" onSubmit={event => { event.preventDefault(); void run(`${repository}|${provider}`, (key, ifMatch) => api.importModelCard(organizationId, model, repository.trim(), provider, key, ifMatch)); }}>
      <FormField label="Hugging Face repository" htmlFor="hub-repository" hint="owner/name, for example openai/gpt-oss-120b. The license, parameter count and reported evaluations are imported, with the repository as their source." error={error ?? undefined}>
        <TextInput id="hub-repository" value={repository} onChange={event => setRepository(event.target.value)} />
      </FormField>
      <label className="checkbox-label"><input type="checkbox" checked={provider} onChange={event => setProvider(event.target.checked)} /> The repository is the model provider&apos;s own (its facts are self-reported)</label>
      <p>
        <Button loading={busy} disabled={!repository.includes('/')}>Import</Button>{' '}
        <Button type="button" variant="ghost" disabled={busy} onClick={() => { setOpen(false); setError(null); }}>Cancel</Button>
      </p>
    </form>
  );
}

/** The organization's own note on the model, which every catalog reader sees. */
export function OrganizationNote({ organizationId, model, note, canChange, onSaved }: { organizationId: string; model: string; note: CardNote | null; canChange: boolean; onSaved: (note: CardNote) => void }) {
  const [draft, setDraft] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // One key per attempt at the same text, so a retry is not applied twice.
  const attempt = useRef<{ text: string; key: string } | null>(null);
  const save = async (event: React.FormEvent) => {
    event.preventDefault();
    if (draft === null) return;
    if (attempt.current?.text !== draft) attempt.current = { text: draft, key: newIdempotencyKey() };
    setSaving(true);
    setError(null);
    try {
      const saved = await api.setModelNote(organizationId, model, draft, note, attempt.current.key);
      attempt.current = null;
      setDraft(null);
      onSaved(saved);
    } catch (failure) {
      const { problem } = failure as { problem?: ApiProblem };
      setError(problem?.status === 409 ? 'Someone changed the note meanwhile. Reload the card to see it.' : problem?.detail ?? String(failure));
    } finally {
      setSaving(false);
    }
  };
  return (
    <div className="organization-note">
      <h4>Organization note</h4>
      {draft === null ? (
        <>
          {note?.note ? (
            <>
              <p className="note-text">{note.note}</p>
              <p className="muted">Set {formatDate(note.updated_at)}{note.updated_by?.name ? ` by ${note.updated_by.name}` : ''}.</p>
            </>
          ) : (
            <p className="muted">No note. A note tells everyone who reads the catalog how this organization uses the model (approved uses, restrictions, a contact).</p>
          )}
          {canChange && <Button type="button" variant="ghost" onClick={() => setDraft(note?.note ?? '')}>{note?.note ? 'Edit note' : 'Add note'}</Button>}
        </>
      ) : (
        <form onSubmit={event => void save(event)}>
          <FormField label="Note" htmlFor="model-card-note" hint="At most 4000 characters. Save an empty note to clear it." error={error ?? undefined}>
            <TextArea id="model-card-note" value={draft} maxLength={4000} rows={4} onChange={event => setDraft(event.target.value)} />
          </FormField>
          <Button loading={saving}>Save note</Button>{' '}
          <Button type="button" variant="ghost" disabled={saving} onClick={() => { setDraft(null); setError(null); }}>Cancel</Button>
        </form>
      )}
    </div>
  );
}

function actorOf(entry: GovernanceEntry): string {
  const actor = entry.actor;
  if (!actor) return '';
  if (actor.actor_type === 'system') return ' by the gateway';
  return actor.name ? ` by ${actor.name}` : actor.actor_type === 'service_account' ? ' by a service account' : '';
}
