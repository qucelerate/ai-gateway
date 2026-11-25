import React, { useEffect, useState } from 'react';
import {
  api,
  newIdempotencyKey,
  type ApiProblem,
  type CatalogDeployment,
  type ProviderFamily,
  type RoutingPolicy,
} from './api';
import { ActionError, ReportError, loadFailed, type SignIn } from './screen-feedback';
import { everyPage } from './use-collection';
import { Button, CheckOption, FormField, Message, TextArea } from './ui';

type Props = SignIn & {
  organizationId: string;
  canChange: boolean;
  familyLabels: Record<ProviderFamily, string>;
};

const problemOf = (caught: unknown, fallback: string): ApiProblem => (caught as { problem?: ApiProblem }).problem ?? loadFailed(fallback);

type Draft = { restrictProviders: boolean; providers: ProviderFamily[]; restrictLocations: boolean; locations: string };

const draftOf = (policy: RoutingPolicy): Draft => ({
  restrictProviders: policy.providers !== null,
  providers: policy.providers ?? [],
  restrictLocations: policy.locations !== null,
  locations: (policy.locations ?? []).join('\n'),
});

/** The locations typed, one per line or separated by commas, trimmed,
 * sorted and without duplicates. */
export function parseLocations(text: string): string[] {
  return [...new Set(text.split(/[\n,]/).map(value => value.trim()).filter(Boolean))].sort();
}

/** What the policy permits, in a sentence. */
export function policySummary(policy: Pick<RoutingPolicy, 'providers' | 'locations'>, familyLabels: Record<ProviderFamily, string>): string {
  const providers = policy.providers === null ? 'any provider' : policy.providers.map(family => familyLabels[family] ?? family).join(', ');
  const locations = policy.locations === null ? 'any location' : policy.locations.join(', ');
  return `Requests may be sent to ${providers}, in ${locations}.`;
}

/** Where the organization's requests may go (D-023): the provider families
 * and deployment locations an alias's deployments must match. A request
 * whose alias has no deployment that does is refused, never sent elsewhere. */
export function RoutingPolicySection({ organizationId, canChange, familyLabels, ...signIn }: Props) {
  const [policy, setPolicy] = useState<RoutingPolicy | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [deployments, setDeployments] = useState<CatalogDeployment[]>([]);
  const [pinned, setPinned] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [saveProblem, setSaveProblem] = useState<ApiProblem | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [saving, setSaving] = useState(false);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setProblem(null);
    api.routingPolicy(organizationId).then(
      value => { if (current) { setPolicy(value); setDraft(draftOf(value)); } },
      caught => { if (current) setProblem(problemOf(caught, 'The routing policy could not be loaded.')); },
    );
    // Only to suggest locations and warn about deployments without one.
    everyPage(params => api.listDeployments(organizationId, params)).then(
      page => { if (current) setDeployments(page.items); },
      () => undefined,
    );
    // A pinned organization serves its pinned version, policy included.
    api.configPin(organizationId).then(
      pin => { if (current) setPinned(!!pin.pinned_version_id); },
      () => undefined,
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  const known = [...new Set(deployments.map(deployment => deployment.location).filter((value): value is string => !!value))].sort();
  const unlocated = deployments.filter(deployment => !deployment.location).length;
  const change = (next: Partial<Draft>) => {
    setDraft(value => (value ? { ...value, ...next } : value));
    setSaved(false);
    setFormError(null);
  };
  const save = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!policy || !draft) return;
    const locations = parseLocations(draft.locations);
    if (draft.restrictProviders && draft.providers.length === 0) {
      setFormError('Choose at least one provider, or allow any provider.');
      return;
    }
    if (draft.restrictLocations && locations.length === 0) {
      setFormError('Enter at least one location, or allow any location.');
      return;
    }
    setSaving(true);
    setSaveProblem(null);
    try {
      const next = await api.setRoutingPolicy(organizationId, {
        providers: draft.restrictProviders ? [...draft.providers].sort() : null,
        locations: draft.restrictLocations ? locations : null,
      }, policy, newIdempotencyKey());
      setPolicy(next);
      setDraft(draftOf(next));
      setSaved(true);
    } catch (caught) {
      const failure = problemOf(caught, 'The routing policy could not be saved.');
      setSaveProblem(failure);
      if (failure.code === 'version_conflict') setAttempt(value => value + 1);
    } finally {
      setSaving(false);
    }
  };
  const families = Object.keys(familyLabels) as ProviderFamily[];
  const disabled = !canChange || saving;
  return (
    <>
      <div className="section-header">
        <div>
          <h2>Where requests may go</h2>
          <p className="muted">
            Limit the providers and locations this organization’s requests are sent to. Aliases use only the deployments that match, before weights and priorities apply;
            a request whose alias has none is refused (<code>routing_policy_denied</code>), never sent somewhere else.
          </p>
        </div>
      </div>
      {problem && <ReportError problem={problem} what="the routing policy" onRetry={() => setAttempt(value => value + 1)} {...signIn} />}
      {policy && <p>{policySummary(policy, familyLabels)}</p>}
      {pinned && (
        <Message kind="warning">
          This organization is pinned to a configuration version (Settings → Routing). Gateways keep serving that version’s policy; a change here applies once it is unpinned.
        </Message>
      )}
      {saved && <Message kind="success">Saved. Gateways route by it within seconds; requests already sent are not affected.</Message>}
      {policy && draft && (
        <form className="stack" onSubmit={event => void save(event)} aria-label="Routing policy">
          <CheckOption checked={draft.restrictProviders} disabled={disabled} onChange={() => change({ restrictProviders: !draft.restrictProviders })}>
            Only these providers
          </CheckOption>
          {draft.restrictProviders && (
            <fieldset className="option-list" aria-label="Providers">
              {families.map(family => (
                <CheckOption
                  key={family}
                  checked={draft.providers.includes(family)}
                  disabled={disabled}
                  onChange={() => change({
                    providers: draft.providers.includes(family) ? draft.providers.filter(value => value !== family) : [...draft.providers, family],
                  })}
                >
                  {familyLabels[family]}
                </CheckOption>
              ))}
            </fieldset>
          )}
          <CheckOption checked={draft.restrictLocations} disabled={disabled} onChange={() => change({ restrictLocations: !draft.restrictLocations })}>
            Only these locations
          </CheckOption>
          {draft.restrictLocations && (
            <>
              <FormField
                label="Locations"
                htmlFor="routing-policy-locations"
                hint={`One per line, as deployments name them${known.length ? ` (this catalog has ${known.join(', ')})` : ''}.`}
              >
                <TextArea value={draft.locations} disabled={disabled} rows={3} onChange={event => change({ locations: event.target.value })} />
              </FormField>
              {unlocated > 0 && (
                <Message kind="warning">
                  {unlocated === 1 ? '1 deployment has' : `${unlocated} deployments have`} no location and will not be used while locations are limited. Give it one in the manifest.
                </Message>
              )}
            </>
          )}
          {formError && <Message kind="error">{formError}</Message>}
          {canChange && <div><Button type="submit" loading={saving} disabled={disabled}>Save routing policy</Button></div>}
        </form>
      )}
      {saveProblem?.code === 'version_conflict'
        ? <Message kind="error">Someone else changed the routing policy; it has been reloaded.</Message>
        : <ActionError problem={saveProblem} action="change the routing policy" {...signIn} />}
    </>
  );
}
