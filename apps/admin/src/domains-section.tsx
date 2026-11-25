import React, { useCallback, useEffect, useState } from 'react';
import { type ApiProblem } from './api';
import { toProblem } from './local-accounts';
import { localAccounts, type EmailDomain } from './local-accounts-api';
import { Badge, Button, ConfirmDialog, CopyButton, FormField, InlineError, Message, TextInput, formatDate } from './ui';

/**
 * Settings → Sign-in → Email domains: the domains whose addresses sign in
 * to this organization. An owner adds one, publishes its TXT record and
 * verifies it; the first organization to verify a domain owns it.
 */
export function DomainsSection({ organizationId, canWrite }: { organizationId: string; canWrite: boolean }) {
  const [domains, setDomains] = useState<EmailDomain[] | null>(null);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [domain, setDomain] = useState('');
  const [adding, setAdding] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [notice, setNotice] = useState('');
  const [removing, setRemoving] = useState<EmailDomain | null>(null);

  const load = useCallback(async () => {
    try {
      setDomains(await localAccounts.domains(organizationId));
      setLoadProblem(null);
    } catch (caught) {
      setLoadProblem(toProblem(caught));
    }
  }, [organizationId]);
  useEffect(() => { void load(); }, [load]);

  const replace = (next: EmailDomain) =>
    setDomains(current => (current ?? []).map(value => (value.id === next.id ? next : value)));

  const add = async () => {
    setAdding(true);
    setProblem(null);
    setNotice('');
    try {
      const added = await localAccounts.addDomain(organizationId, domain);
      setDomains(current => [...(current ?? []), added].sort((a, b) => a.domain.localeCompare(b.domain)));
      setDomain('');
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setAdding(false);
    }
  };
  const verify = async (value: EmailDomain) => {
    setBusy(value.id);
    setProblem(null);
    setNotice('');
    try {
      const checked = await localAccounts.verifyDomain(organizationId, value.id);
      replace(checked);
      setNotice(checked.status === 'verified'
        ? `${checked.domain} is verified. People with addresses there now sign in to this organization.`
        : `The record for ${checked.domain} was not found yet. DNS changes can take a while; try again later.`);
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setBusy(null);
    }
  };
  const remove = async () => {
    if (!removing) return;
    setBusy(removing.id);
    setProblem(null);
    try {
      await localAccounts.removeDomain(organizationId, removing.id);
      setDomains(current => (current ?? []).filter(value => value.id !== removing.id));
      setRemoving(null);
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="email-domains" aria-labelledby="email-domains-heading">
      <h3 id="email-domains-heading">Email domains</h3>
      <p>
        People whose email address is at a verified domain sign in to this organization, through its identity
        provider when one is enabled. Prove each domain with a DNS TXT record.
      </p>
      <InlineError problem={loadProblem} />
      {domains === null && !loadProblem && <p role="status">Loading domains…</p>}
      {domains?.length === 0 && <p className="hint">No domains yet.</p>}
      {domains && domains.length > 0 && (
        <ul className="domain-list">
          {domains.map(value => (
            <li key={value.id} className="domain-item">
              <div className="domain-head">
                <strong>{value.domain}</strong>
                <Badge status={value.status === 'verified' ? 'active' : 'pending'}>{value.status === 'verified' ? 'Verified' : 'Pending'}</Badge>
              </div>
              {value.status === 'verified' ? (
                <p className="hint">Verified {formatDate(value.verified_at)}. The TXT record may be removed.</p>
              ) : (
                <div className="domain-record">
                  <p className="hint">
                    Publish this TXT record at your DNS provider, then verify.
                    {value.last_checked_at && ` Last checked ${formatDate(value.last_checked_at)}.`}
                  </p>
                  <div className="secret-value-row">
                    <code className="secret-value" aria-label={`Record name for ${value.domain}`}>{value.record.name}</code>
                    <CopyButton text={value.record.name} label={`Copy the record name for ${value.domain}`} />
                  </div>
                  <div className="secret-value-row">
                    <code className="secret-value" aria-label={`Record value for ${value.domain}`}>{value.record.value}</code>
                    <CopyButton text={value.record.value} label={`Copy the record value for ${value.domain}`} />
                  </div>
                </div>
              )}
              {canWrite && (
                <div className="domain-actions">
                  {value.status !== 'verified' && (
                    <Button type="button" variant="ghost" loading={busy === value.id} onClick={() => { void verify(value); }}>Verify</Button>
                  )}
                  <Button type="button" variant="danger" disabled={busy === value.id} onClick={() => setRemoving(value)}>Remove</Button>
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
      {notice && <Message kind={notice.includes('is verified') ? 'success' : 'info'}>{notice}</Message>}
      {!removing && <InlineError problem={problem} />}
      {canWrite && (
        <form className="domain-add" aria-label="Add an email domain" onSubmit={event => { event.preventDefault(); void add(); }}>
          <FormField label="Domain" htmlFor="email-domain" hint="For example example.com. Subdomains are listed separately.">
            <TextInput
              id="email-domain"
              value={domain}
              onChange={event => setDomain(event.target.value)}
              autoCapitalize="none"
              spellCheck={false}
              required
            />
          </FormField>
          {/* Until the list has loaded, an add could be overwritten by it. */}
          <Button type="submit" loading={adding} disabled={domains === null}>Add domain</Button>
        </form>
      )}
      <ConfirmDialog
        open={!!removing}
        onClose={() => { setRemoving(null); setProblem(null); }}
        onConfirm={() => { void remove(); }}
        title="Remove email domain"
        confirmText="Remove"
        problem={problem}
        submitting={!!removing && busy === removing.id}
      >
        {`Remove ${removing?.domain ?? 'this domain'}? People with addresses there stop being sent to this organization’s sign-in, and another organization may claim it.`}
      </ConfirmDialog>
    </section>
  );
}
