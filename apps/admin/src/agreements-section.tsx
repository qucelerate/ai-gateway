import React, { useEffect, useRef, useState } from 'react';
import { api, newIdempotencyKey, type AgreementModel, type AgreementResults, type ApiProblem, type ModelAgreements, type ProviderAccount } from './api';
import {
  acceptAllPairs,
  acceptPairs,
  acceptable,
  agreementStatusTone,
  allRatesListed,
  needsProviderAccess,
  offerFor,
  outcomeLabel,
  pruneOffers,
  rateSummary,
  rateText,
  statusText,
} from './agreements-model';
import { modelAccessDocs } from './cloud-access';
import { AccessNote } from './cloud-access-note';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { Badge, Button, CheckOption, DataCell, DataGrid, Dialog, EmptyState, Message, Select, TableBody, TableHead, TableHeader, TableRow, formatDate } from './ui';

const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);
/** How often the panel reloads while the gateway is still checking an agreement. */
const CHECKING_RELOAD_MS = 20_000;

type Props = SignIn & { organizationId: string; account: ProviderAccount; canChange: boolean };
/** What the dialog would send: exactly these pairs (there is no server-side "all"). */
type Request = { all: boolean; models: { model: string; offer_id: string }[] };

/** A Bedrock account's models that need an AWS Marketplace agreement, with
 * their offers, and accepting them (D-035). */
export function Agreements({ organizationId, account, canChange, ...signIn }: Props) {
  const key = account.adapter_instance_key;
  const [view, setView] = useState<ModelAgreements | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [offers, setOffers] = useState<Record<string, string>>({});
  const [request, setRequest] = useState<Request | null>(null);
  const [verify, setVerify] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [results, setResults] = useState<AgreementResults | null>(null);
  // Whether automatic offers are on for the account: then discovery's usual
  // check calls each identifier it would offer once. Unknown until read.
  const [autoOffers, setAutoOffers] = useState<'on' | 'off' | null>(null);
  // One key per request until the server answers or refuses: a retry replays.
  const idempotency = useRef<{ request: string; key: string } | null>(null);

  // A failed reload keeps what was shown (and any results) with the error.
  useEffect(() => {
    let current = true;
    api.modelAgreements(organizationId, key).then(
      next => {
        if (!current) return;
        setLoadProblem(null);
        setView(next);
        // A choice the reloaded models no longer offer is dropped, so what
        // is shown is what would be sent.
        setOffers(previous => pruneOffers(next.models, previous));
        setPicked(previous => new Set(next.models.filter(model => previous.has(model.model) && acceptable(model)).map(model => model.model)));
      },
      caught => { if (current) setLoadProblem(problemOf(caught, 'The model agreements could not be loaded.')); },
    );
    return () => { current = false; };
  }, [organizationId, key, attempt]);

  useEffect(() => {
    let current = true;
    api.getAutoOffer(organizationId, key).then(
      offer => { if (current) setAutoOffers(offer ? 'on' : 'off'); },
      () => { if (current) setAutoOffers(null); },
    );
    return () => { current = false; };
  }, [organizationId, key]);

  const checking = view?.models.some(model => model.agreement?.checking) ?? false;
  useEffect(() => {
    if (!checking) return;
    const timer = setTimeout(() => setAttempt(value => value + 1), CHECKING_RELOAD_MS);
    return () => clearTimeout(timer);
    // A failed poll (a new problem) schedules the next one too.
  }, [checking, view, loadProblem]);

  if (loadProblem?.code === 'agreements_need_tenant_credential') {
    return (
      <Message kind="info" title="Agreements need the organization’s own AWS role">
        The gateway calls this account with its operator’s credential, so accepting an agreement here would subscribe the operator’s AWS account. Connect the account
        with your own role (Connect a cloud) to manage its agreements, or accept them in the Bedrock console.
      </Message>
    );
  }
  if (loadProblem && !view) return <ReportError problem={loadProblem} what="model agreements" onRetry={() => setAttempt(value => value + 1)} {...signIn} />;
  if (!view) return <Message kind="info">Loading model agreements…</Message>;

  const models = view.models;
  const selected = acceptPairs(models, picked, offers);
  const everything = acceptAllPairs(models, offers);
  const toggle = (model: string) => setPicked(previous => {
    const next = new Set(previous);
    if (next.has(model)) next.delete(model); else next.add(model);
    return next;
  });
  const ask = (all: boolean) => {
    setProblem(null);
    setVerify(false);
    setRequest({ all, models: all ? everything : selected });
  };
  const accept = async () => {
    if (!request) return;
    const input = { models: request.models, verify };
    const fingerprint = JSON.stringify([key, input]);
    if (idempotency.current?.request !== fingerprint) idempotency.current = { request: fingerprint, key: newIdempotencyKey() };
    setSubmitting(true);
    setProblem(null);
    try {
      const answer = await api.acceptModelAgreements(organizationId, key, input, idempotency.current.key);
      idempotency.current = null;
      setResults(answer);
      setRequest(null);
      setPicked(new Set());
      setAttempt(value => value + 1);
    } catch (caught) {
      const failure = problemOf(caught, 'The agreements could not be accepted. Check your connection and try again.');
      // A refusal is final for this key, but one still in progress is not:
      // retrying must replay it, not accept again under a new key.
      if (failure.status >= 400 && failure.status < 500 && failure.code !== 'idempotency_in_progress') idempotency.current = null;
      setProblem(failure);
      if (failure.code === 'agreement_offer_changed') {
        // What the dialog named is no longer on offer: close it, and show
        // the current offers to choose from again.
        setRequest(null);
        setAttempt(value => value + 1);
      }
    } finally {
      setSubmitting(false);
    }
  };
  const count = request?.models.length ?? 0;
  const requested = new Set(request?.models.map(item => item.model));
  const ratesListed = models.filter(model => requested.has(model.model)).every(model => {
    const offer = offerFor(model, offers);
    return offer ? allRatesListed(offer) : true;
  });
  const multiple = models.filter(model => acceptable(model) && model.offers.length > 1 && !offerFor(model, offers)).length;

  return (
    <section aria-label={`Agreements of ${key}`}>
      <p className="meta">
        Bedrock models whose evidence says an AWS Marketplace agreement is needed, collapsed to their foundation model, with the live offer and what accepting it unblocks.
      </p>
      <Message kind="warning" title="Accepting creates AWS Marketplace subscriptions">
        Each accepted agreement subscribes your AWS account to the model’s offer, billed per use at the offer’s rates. Accepting does not help when AWS has not enabled the model for
        your account: some models need access AWS grants on request. A test request refused after an agreement is accepted usually means that.{' '}
        <a className="inline-action" href={modelAccessDocs} target="_blank" rel="noopener noreferrer">Model access on your cloud account</a>
      </Message>
      {loadProblem && <ReportError problem={loadProblem} what="model agreements" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
      {view.permission === 'missing' && (
        <Message kind="error" title="The gateway’s role may not accept agreements">
          The role the gateway assumes lacks the opt-in agreement permissions. Update the role with “Accept model agreements” turned on (the CloudFormation stack’s
          AcceptModelAgreements parameter, or the AWS CLI or Terraform under Connect a cloud), or accept the agreements in the Bedrock console yourself.
        </Message>
      )}
      {models.length === 0
        ? <EmptyState title="No agreements needed" text="No model of this account has evidence that it needs an agreement. Run discovery to check again." />
        : (
          <>
            {canChange && (
              <div className="report-controls">
                <Button onClick={() => ask(false)} disabled={selected.length === 0 || view.permission === 'missing'}>Accept selected</Button>
                <Button variant="ghost" onClick={() => ask(true)} disabled={everything.length === 0 || view.permission === 'missing'}>Accept all ({everything.length})</Button>
                <Button variant="ghost" onClick={() => setAttempt(value => value + 1)}>Refresh</Button>
              </div>
            )}
            {canChange && multiple > 0 && (
              <p className="muted">
                {multiple === 1 ? 'One model has' : `${multiple} models have`} several offers: choose one to include {multiple === 1 ? 'it' : 'them'}.
              </p>
            )}
            {problem && !request && <AcceptProblem problem={problem} {...signIn} />}
            <DataGrid aria-label={`Model agreements of ${key}`}>
              <TableHead>
                <TableRow className="data-row head">
                  {canChange && <TableHeader>Accept</TableHeader>}
                  <TableHeader>Model</TableHeader>
                  <TableHeader>Offer</TableHeader>
                  <TableHeader>Agreement</TableHeader>
                </TableRow>
              </TableHead>
              <TableBody>
                {models.map(model => (
                  <TableRow className="data-row" key={model.model}>
                    {canChange && (
                      <DataCell label="Accept">
                        {acceptable(model)
                          ? <CheckOption checked={picked.has(model.model)} onChange={() => toggle(model.model)} aria-label={`Accept ${model.model}`}><span className="sr-only">{model.model}</span></CheckOption>
                          : <span className="muted">—</span>}
                      </DataCell>
                    )}
                    <DataCell label="Model" className="cell-primary cell-wrap">
                      <code>{model.model}</code>
                      {model.unblocks.length > 0 && <span className="muted cell-note">Unblocks {model.unblocks.join(', ')}</span>}
                      {model.deployments.length > 0 && <span className="muted cell-note">Deployments {model.deployments.join(', ')}</span>}
                    </DataCell>
                    <DataCell label="Offer" className="cell-wrap"><OfferCell model={model} chosen={offers[model.model]} onChoose={offer => setOffers(previous => ({ ...previous, [model.model]: offer }))} /></DataCell>
                    <DataCell label="Agreement" className="cell-wrap"><AgreementCell model={model} /></DataCell>
                  </TableRow>
                ))}
              </TableBody>
            </DataGrid>
          </>
        )}
      {results && <Results results={results} />}
      <Dialog
        open={request !== null}
        onClose={() => { if (!submitting) setRequest(null); }}
        title={`Accept ${count} agreement${count === 1 ? '' : 's'}`}
        alert
      >
        <div className="dialog-body">
          <p>
            This creates AWS Marketplace subscriptions in the AWS account behind {key}, billed per use {ratesListed ? 'at the rates shown' : 'at each offer’s rates'}, for:
          </p>
          <ul>
            {request?.models.map(item => <li key={item.model}><code>{item.model}</code> with offer <code>{item.offer_id}</code></li>)}
          </ul>
          {!ratesListed && <p className="muted">Some of these offers have more rates than the gateway lists: read their full rate cards in AWS Marketplace before accepting.</p>}
          <p className="muted">Cancelling a subscription is done in AWS Marketplace.</p>
          <p className="muted">Accepting does not enable a model AWS has not made available to the account.</p>
          <CheckOption checked={verify} onChange={event => setVerify(event.target.checked)}>
            Also send one 1-token test request per model (billed by the provider)
          </CheckOption>
          <p className="muted">{withoutTest(autoOffers)}</p>
        </div>
        {problem && <AcceptProblem problem={problem} {...signIn} />}
        <div className="dialog-actions">
          <Button variant="ghost" onClick={() => setRequest(null)} disabled={submitting}>Cancel</Button>
          <Button variant="danger" onClick={() => void accept()} loading={submitting}>Accept and subscribe</Button>
        </div>
      </Dialog>
    </section>
  );
}

function AcceptProblem({ problem, ...signIn }: SignIn & { problem: ApiProblem }) {
  if (problem.code === 'agreement_offer_changed') {
    return <Message kind="error">A model’s offer changed since this page loaded, so nothing was accepted. The current offers are shown now: review them and accept again.</Message>;
  }
  if (problem.code === 'agreement_permission_missing') {
    return <Message kind="error">{problem.detail}</Message>;
  }
  return <ActionError problem={problem} action="accept model agreements" {...signIn} />;
}

function OfferCell({ model, chosen, onChoose }: { model: AgreementModel; chosen: string | undefined; onChoose: (offer: string) => void }) {
  if (model.offers_error === 'agreement_permission_missing') return <span className="muted">The role may not read offers.</span>;
  if (model.offers_error) return <span className="muted">AWS did not list its offers.</span>;
  if (model.offers.length === 0) return <span className="muted">No offer listed</span>;
  // Exactly what accepting would send: the only offer, or the one chosen.
  const offer = offerFor(model, chosen ? { [model.model]: chosen } : {});
  return (
    <>
      {model.offers.length > 1
        ? (
          <Select aria-label={`Offer for ${model.model}`} value={offer?.offer_id ?? ''} onChange={event => onChoose(event.target.value)}>
            <option value="" disabled>Choose an offer</option>
            {model.offers.map(item => <option key={item.offer_id} value={item.offer_id}>{item.offer_id}</option>)}
          </Select>
        )
        : <code>{model.offers[0].offer_id}</code>}
      {offer
        ? (
          <>
            <br /><small>{rateSummary(offer)}</small>
            {offer.rate_card.length > 3 && (
              <details>
                <summary>All {offer.rate_card.length} rates listed</summary>
                <ul className="cell-note">{offer.rate_card.map(rate => <li key={rate.dimension}><small>{rateText(rate)}{rate.description ? ` (${rate.description})` : ''}</small></li>)}</ul>
              </details>
            )}
            {offer.refund_policy && <><br /><small className="muted">Refunds: {offer.refund_policy}</small></>}
            {offer.legal_url && <><br /><a className="inline-action" href={offer.legal_url} target="_blank" rel="noopener noreferrer">Legal terms</a></>}
          </>
        )
        : <><br /><small className="muted">{model.offers.length} offers: choose one to accept this model.</small></>}
    </>
  );
}

function AgreementCell({ model }: { model: AgreementModel }) {
  const agreement = model.agreement;
  if (!agreement) return <span className="muted">{model.agreement_required ? 'Not accepted' : 'No agreement needed'}</span>;
  return (
    <>
      <Badge tone={agreementStatusTone[agreement.status]}>{statusText(agreement)}</Badge>
      {agreement.checking && <small className="muted"> checking…</small>}
      {agreement.detail && <><br /><small>{agreement.detail}</small></>}
      {agreement.provider_code && <><br /><small className="muted">AWS: {agreement.provider_code}</small></>}
      <br /><small className="muted">Accepted {formatDate(agreement.accepted_at)}{agreement.probed_model ? `; tested through ${agreement.probed_model}` : ''}</small>
      {needsProviderAccess(agreement.reason) && <><br /><AccessNote family="bedrock" /></>}
    </>
  );
}

function Results({ results }: { results: AgreementResults }) {
  return (
    <section aria-label="Acceptance results">
      <h3>Results</h3>
      <DataGrid aria-label="Acceptance results">
        <TableHead>
          <TableRow className="data-row head">
            <TableHeader>Model</TableHeader>
            <TableHeader>Outcome</TableHeader>
            <TableHeader>Detail</TableHeader>
          </TableRow>
        </TableHead>
        <TableBody>
          {results.results.map(result => (
            <TableRow className="data-row" key={result.model}>
              <DataCell label="Model" className="cell-primary"><code>{result.model}</code></DataCell>
              <DataCell label="Outcome">
                <Badge tone={result.outcome === 'failed' ? 'badge-danger' : result.outcome === 'unconfirmed' ? 'badge-warning' : 'badge-success'}>{outcomeLabel[result.outcome]}</Badge>{' '}
                <Badge tone={agreementStatusTone[result.status]}>{statusText(result)}</Badge>
              </DataCell>
              <DataCell label="Detail" className="cell-wrap">
                {result.detail ?? <span className="muted">—</span>}
                {result.provider_code && <><br /><small className="muted">AWS: {result.provider_code}</small></>}
              </DataCell>
            </TableRow>
          ))}
        </TableBody>
      </DataGrid>
    </section>
  );
}

/** What leaving the test request off means, given whether automatic offers
 * are on for the account (null: not known). */
export function withoutTest(autoOffers: 'on' | 'off' | null): string {
  const now = 'Without it, the gateway sends no model request when accepting; AWS’s availability check is not a call.';
  if (autoOffers === 'on') {
    return `${now} Automatic offers are on for this account, so once AWS reports a model available, the gateway’s usual check sends one 1-token request (billed by AWS) per model identifier it would offer, at most hourly, before offering it.`;
  }
  if (autoOffers === 'off') {
    return `${now} Automatic offers are off for this account, so nothing calls the model until someone routes a request to it: the first real request is the true check.`;
  }
  return `${now} If automatic offers are on for this account, the gateway’s usual check then sends one 1-token request (billed by AWS) per model identifier it would offer, at most hourly, before offering it; if they are off, nothing calls the model until someone routes to it.`;
}
