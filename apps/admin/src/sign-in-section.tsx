import React, { useEffect, useState } from 'react';
import {
  ApiError,
  api,
  newIdempotencyKey,
  type ApiProblem,
  type IdentityProvider,
  type IdentityProviderConfiguration,
  type IdentityProviderStatus,
} from './api';
import type { MutationState } from './mutation-state';
import { Button, ConfirmDialog, CopyButton, FormField, InlineError, TextInput, formatDate, Badge, Message } from './ui';

export const providerStatusLabels: Record<IdentityProviderStatus, string> = {
  not_configured: 'Not set up',
  pending: 'Saved, not tested',
  active: 'Active',
  active_with_pending_change: 'Active, change not tested',
  disabled: 'Disabled',
};

/** What a test sign-in's return (`?sign_in_test=…`) means, in plain English. */
export function signInTestMessage(code: string): { ok: boolean; text: string } {
  switch (code) {
    case 'activated':
      return { ok: true, text: 'The test sign-in worked. People in this organization now sign in through your identity provider.' };
    case 'identity_provider_changed':
      return { ok: false, text: 'The configuration changed while you were testing it, so nothing was activated. Test it again.' };
    case 'provider_test_session_required':
      return { ok: false, text: 'The test must finish in the browser where you started it, while you are still signed in here. Nothing was activated.' };
    case 'oidc_backend_rejected':
    case 'claim_validation_failed':
      return { ok: false, text: 'Your identity provider answered, but the gateway could not accept its sign-in. Check the issuer, client ID and secret. Nothing was activated.' };
    case 'oidc_backend_unavailable':
      return { ok: false, text: 'Your identity provider could not be reached. Nothing was activated; try again later.' };
    case 'forbidden':
      return { ok: false, text: 'You no longer have permission to change sign-in for this organization. Nothing was activated.' };
    default:
      return { ok: false, text: `The test sign-in did not succeed (${code}). Nothing was activated.` };
  }
}

/**
 * The organization a test sign-in just returned to (`?organization=…&
 * sign_in_test=…`), so the app can select it before showing the outcome.
 */
export function testedOrganization(search: string): string | null {
  const params = new URLSearchParams(search);
  return params.has('sign_in_test') ? params.get('organization') : null;
}

/** Only owners change sign-in. A convenience: the server decides, and an
 * unknown role keeps the controls. */
export function offersSignInChanges(role: string | null | undefined): boolean {
  return !role || role.toLowerCase() === 'owner';
}

/** Space- or comma-separated scope names, as the API takes them. */
export function parseScopes(value: string): string[] {
  return value.split(/[\s,]+/).filter(Boolean);
}

/** Any failure as a problem to show: the test start answers the sign-in
 * machinery's `{ error }` shape for some failures, and a lost connection has
 * no problem at all. `failed` says what did not happen. */
function asProblem(caught: unknown, failed = 'The test sign-in could not be started.'): ApiProblem {
  const problem = (caught as { problem?: ApiProblem }).problem;
  if (problem) return problem;
  if (caught instanceof ApiError) {
    const detail =
      caught.code === 'oidc_provider_unavailable'
        ? 'Your identity provider could not be reached, or its discovery document does not name exactly the issuer you saved. Check the issuer and try again.'
        : caught.code === 'credential_encryption_unavailable'
          ? 'This gateway cannot store or use client secrets until an operator sets its credential encryption key.'
          : failed;
    return { type: 'about:blank', code: caught.code, title: caught.code, status: caught.status, detail, instance: '', request_id: '' };
  }
  return { type: 'about:blank', code: 'network_error', title: 'network_error', status: 0, detail: `The gateway could not be reached. ${failed} Try again.`, instance: '', request_id: '' };
}

/** Runs a change, turning any failure into a problem the form shows. */
async function shown<T>(run: () => Promise<T>, failed: string): Promise<T> {
  try {
    return await run();
  } catch (caught) {
    throw { problem: asProblem(caught, failed) };
  }
}

type Form = { issuer: string; clientId: string; secret: string; scopes: string };

function formFrom(configuration: IdentityProviderConfiguration | null): Form {
  return {
    issuer: configuration?.issuer ?? '',
    clientId: configuration?.client_id ?? '',
    secret: '',
    scopes: configuration?.extra_scopes.join(' ') ?? '',
  };
}

/**
 * The organization's own identity provider: its status, the redirect URI to
 * register, the configuration form (the secret is write-only), the owner's
 * test sign-in that activates a saved change, and disable. Write controls are
 * shown to owners only; the server checks every request, and a refused one
 * leaves the form as it was with the server's explanation.
 */
export function SignInSection({
  organizationId,
  provider,
  loading,
  loadError,
  onReload,
  role,
  mutation,
  navigate = url => window.location.assign(url),
}: {
  organizationId: string;
  provider: IdentityProvider | null;
  loading: boolean;
  loadError: ApiProblem | null;
  onReload: () => void;
  role: string | null | undefined;
  mutation: MutationState<unknown>;
  navigate?: (url: string) => void;
}) {
  const canWrite = offersSignInChanges(role);
  const latest = provider?.pending ?? provider?.active ?? null;
  const [form, setForm] = useState<Form>(() => formFrom(latest));
  const [replacingSecret, setReplacingSecret] = useState(false);
  // One key per unchanged request, so a retry after a lost response replays.
  const [saveKey, setSaveKey] = useState(newIdempotencyKey);
  const [disabling, setDisabling] = useState(false);
  const [disableKey, setDisableKey] = useState('');
  const [testing, setTesting] = useState(false);
  const [testProblem, setTestProblem] = useState<ApiProblem | null>(null);
  const [testResult, setTestResult] = useState<string | null>(null);

  // Reset the form when another organization or configuration loads.
  const latestVersion = latest?.configuration_version ?? 0;
  useEffect(() => {
    setForm(formFrom(latest));
    setReplacingSecret(false);
    setSaveKey(newIdempotencyKey());
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [organizationId, latestVersion]);

  // The outcome of a test sign-in, once, from the URL it returned to, and
  // only under the organization that was tested: another organization's
  // settings leave it for that one.
  useEffect(() => {
    const params = new URLSearchParams(window.location.search);
    const code = params.get('sign_in_test');
    if (!code || params.get('organization') !== organizationId) return;
    setTestResult(code);
    params.delete('sign_in_test');
    params.delete('organization');
    const rest = params.toString();
    history.replaceState(history.state, '', `${window.location.pathname}${rest ? `?${rest}` : ''}`);
  }, [organizationId]);

  const update = (patch: Partial<Form>) => {
    setForm(current => ({ ...current, ...patch }));
    setSaveKey(newIdempotencyKey());
  };

  // A secret is needed for a first save, and whenever the issuer or client
  // changes; otherwise the stored one is kept.
  const sameClient = !!latest && form.issuer.trim() === latest.issuer && form.clientId.trim() === latest.client_id;
  const secretNeeded = !sameClient;
  const showSecret = secretNeeded || replacingSecret;

  const save = async () => {
    const ok = await mutation.submit(() => shown(async () => {
      const { data, meta } = await api.saveIdentityProvider(
        organizationId,
        {
          issuer: form.issuer.trim(),
          client_id: form.clientId.trim(),
          extra_scopes: parseScopes(form.scopes),
          ...(showSecret && form.secret ? { client_secret: form.secret } : {}),
        },
        provider?.version ?? 0,
        saveKey,
      );
      // Only the version: nothing of the form reaches the shown result.
      return { data: { version: data.version }, requestId: meta.requestId };
    }, 'The configuration may not have been saved.'));
    if (!ok) return;
    setForm(current => ({ ...current, secret: '' }));
    setReplacingSecret(false);
    onReload();
  };

  const startTest = async () => {
    setTesting(true);
    setTestProblem(null);
    try {
      const { authorization_url: url } = await api.testIdentityProvider(organizationId);
      navigate(url);
    } catch (caught) {
      setTestProblem(asProblem(caught));
      setTesting(false);
    }
  };

  const confirmDisable = async () => {
    if (!provider) return;
    const ok = await mutation.submit(() => shown(async () => {
      const { data, meta } = await api.disableIdentityProvider(organizationId, provider.version, disableKey);
      return { data: { version: data.version }, requestId: meta.requestId };
    }, 'Sign-in may not have been disabled.'));
    if (!ok) return;
    setDisabling(false);
    onReload();
  };

  const result = testResult ? signInTestMessage(testResult) : null;
  const status = provider?.status ?? 'not_configured';

  return (
    <>
      <div className="section-header">
        <h3>Sign-in</h3>
      </div>
      <p className="cds--form__helper-text">
        People in this organization can sign in through your own identity provider, such as Entra ID,
        Okta or Google Workspace, by entering the organization's name on the sign-in page. Until you
        set one up and test it, they sign in through the gateway's own provider.
      </p>

      {result && (
        <Message kind={result.ok ? 'success' : 'error'}>
          {result.text}
        </Message>
      )}

      {loading && <p role="status">Loading sign-in settings…</p>}
      <InlineError problem={loadError} />
      {loadError && (
        <Button variant="ghost" onClick={onReload}>
          Try again
        </Button>
      )}

      {provider && (
        <>
          <dl className="detail-list" aria-label="Sign-in status">
            <dt>Status</dt>
            <dd>
              <Badge kind={status === 'active' || status === 'active_with_pending_change' ? 'success' : status === 'disabled' ? 'danger' : 'neutral'}>
                {providerStatusLabels[status]}
              </Badge>
            </dd>
            {provider.active && (
              <>
                <dt>{provider.enabled ? 'In use' : 'Last used'}</dt>
                <dd>
                  {provider.active.issuer} · client {provider.active.client_id}
                </dd>
              </>
            )}
            {provider.pending && (
              <>
                <dt>Saved, waiting for its test</dt>
                <dd>
                  {provider.pending.issuer} · client {provider.pending.client_id}
                </dd>
              </>
            )}
            <dt>Last successful test</dt>
            <dd>{provider.last_tested_at ? formatDate(provider.last_tested_at) : 'Never'}</dd>
            {provider.redirect_uri && (
              <>
                <dt>Redirect URI</dt>
                <dd>
                  <code>{provider.redirect_uri}</code>{' '}
                  <CopyButton text={provider.redirect_uri} label="Copy redirect URI" />
                  <span className="cds--form__helper-text">
                    Register exactly this as a redirect (reply) URI for the gateway's application at
                    your identity provider.
                  </span>
                </dd>
              </>
            )}
          </dl>

          {canWrite && provider.pending && (
            <div className="callout">
              <p>
                A saved configuration takes effect only after you sign in through it once. You will be
                sent to your identity provider and brought back here; nobody else is signed in or
                added by the test.
              </p>
              <InlineError problem={testProblem} />
              <Button onClick={() => { void startTest(); }} loading={testing}>
                Test and activate
              </Button>
            </div>
          )}

          {canWrite && (
            <form
              aria-label="Identity provider"
              onSubmit={event => {
                event.preventDefault();
                void save();
              }}
            >
              {/* Locked while a save or disable runs: the reload after it
                  resets the form, which would lose edits made meanwhile. */}
              <fieldset className="plain-fieldset" disabled={mutation.submitting}>
                <FormField
                  label="Issuer"
                  htmlFor="idp-issuer"
                  hint="Exactly as your provider's discovery document states it, for example https://login.microsoftonline.com/<tenant-id>/v2.0."
                >
                  <TextInput
                    id="idp-issuer"
                    type="url"
                    required
                    value={form.issuer}
                    onChange={event => update({ issuer: event.target.value })}
                  />
                </FormField>
                <FormField label="Client ID" htmlFor="idp-client-id" hint="The application (client) ID the provider gave the gateway.">
                  <TextInput
                    id="idp-client-id"
                    required
                    value={form.clientId}
                    onChange={event => update({ clientId: event.target.value })}
                  />
                </FormField>
                {showSecret ? (
                  <FormField
                    label="Client secret"
                    htmlFor="idp-client-secret"
                    hint={
                      secretNeeded
                        ? 'Required for a new provider, and whenever the issuer or client ID changes. It is stored encrypted and never shown again.'
                        : 'The new secret replaces the stored one. It is stored encrypted and never shown again.'
                    }
                  >
                    <TextInput
                      id="idp-client-secret"
                      type="password"
                      autoComplete="off"
                      required={secretNeeded}
                      value={form.secret}
                      onChange={event => update({ secret: event.target.value })}
                    />
                  </FormField>
                ) : (
                  <div className="form-field">
                    <span>Client secret</span>
                    <span className="cds--form__helper-text">A secret is stored. It is never shown.</span>
                    <Button variant="ghost" type="button" onClick={() => { setReplacingSecret(true); setSaveKey(newIdempotencyKey()); }}>
                      Replace secret
                    </Button>
                  </div>
                )}
                <FormField
                  label="Extra scopes"
                  htmlFor="idp-scopes"
                  hint="Optional, separated by spaces. openid, profile and email are always requested."
                >
                  <TextInput
                    id="idp-scopes"
                    value={form.scopes}
                    onChange={event => update({ scopes: event.target.value })}
                  />
                </FormField>
                <p className="cds--form__helper-text">
                  Saving does not change how anyone signs in: test the saved configuration to activate it.
                </p>
                <InlineError problem={mutation.problem} />
                <div className="dialog-actions">
                  {(provider.enabled || provider.pending) && (
                    <Button
                      variant="danger"
                      type="button"
                      onClick={() => { mutation.clearProblem(); setDisableKey(newIdempotencyKey()); setDisabling(true); }}
                    >
                      Disable
                    </Button>
                  )}
                  <Button type="submit" loading={mutation.submitting && !disabling}>
                    Save
                  </Button>
                </div>
              </fieldset>
            </form>
          )}
        </>
      )}

      <ConfirmDialog
        open={disabling}
        onClose={() => setDisabling(false)}
        onConfirm={() => { void confirmDisable(); }}
        title="Disable your identity provider"
        confirmText="Disable"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        People in this organization will sign in through the gateway's own provider instead, and
        anyone signed in through yours is signed out. Nobody's account or access is removed, and a
        saved change that was not tested is dropped. You can set it up and test it again later.
      </ConfirmDialog>
    </>
  );
}
