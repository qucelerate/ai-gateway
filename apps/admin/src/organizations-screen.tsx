import React, { useCallback, useEffect, useRef, useState } from 'react';
import { type ApiProblem } from './api';
import { deployment, type DeploymentEvent, type DeploymentOrganization, type OwnerLink, type StoredSecrets } from './deployment-api';
import { toProblem } from './local-accounts';
import { slugFrom } from './local-accounts-api';
import {
  Badge, Button, CheckOption, CopyButton, DataCell, DataGrid, Dialog, EmptyState, FormField, InlineError, Message,
  RadioOption, TableBody, TableHead, TableHeader, TableRow, TextInput, formatDate,
} from './ui';

type OwnerChoice = 'none' | 'invitation' | 'local_account';

const statusLabel: Record<DeploymentOrganization['status'], string> = { active: 'Active', suspended: 'Suspended', deleted: 'Deleted' };
const statusBadge: Record<DeploymentOrganization['status'], 'success' | 'warning' | 'danger'> = { active: 'success', suspended: 'warning', deleted: 'danger' };

/** What a deployment audit record says, as a sentence fragment. */
export function eventText(event: DeploymentEvent): string {
  const name = typeof event.detail.name === 'string' ? event.detail.name : null;
  const subject = name ? `“${name}”` : 'an organization';
  switch (event.action) {
    case 'organization.created': return `created ${subject}`;
    case 'organization.self_served': return `created ${subject} for themselves`;
    case 'organization.suspended': return `suspended ${subject}`;
    case 'organization.unsuspended': return `lifted the suspension of ${subject}`;
    case 'organization.deleted': return `deleted ${subject}`;
    case 'organization.restored': return `restored ${subject}`;
    case 'organization.deletion_requested': return `asked for ${subject} to be deleted`;
    case 'organization.deletion_request_withdrawn': return `withdrew the request to delete ${subject}`;
    case 'deployment_admin.granted': return 'granted the deployment admin role';
    case 'setup.completed': return `set up the gateway with ${subject}`;
    default: return event.action;
  }
}

/**
 * Organizations (deployment admins only): every organization of the
 * deployment, by name and state, never its contents. Create one (with a
 * first owner), suspend it, delete a suspended one, restore a deleted one.
 */
export function OrganizationsScreen() {
  const [showDeleted, setShowDeleted] = useState(false);
  const [organizations, setOrganizations] = useState<DeploymentOrganization[] | null>(null);
  const [selfServe, setSelfServe] = useState(false);
  const [events, setEvents] = useState<DeploymentEvent[]>([]);
  const [loadProblem, setLoadProblem] = useState<ApiProblem | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [deleting, setDeleting] = useState<DeploymentOrganization | null>(null);
  const [restoring, setRestoring] = useState<DeploymentOrganization | null>(null);

  // Only the latest load may fill the list: one started before the filter
  // changed would show the other filter's organizations.
  const latest = useRef(0);
  const load = useCallback(async () => {
    const request = ++latest.current;
    try {
      const [list, audit] = await Promise.all([deployment.organizations(showDeleted), deployment.audit()]);
      if (request !== latest.current) return;
      setOrganizations(list.items);
      setSelfServe(list.self_serve);
      setEvents(audit);
      setLoadProblem(null);
    } catch (caught) {
      if (request === latest.current) setLoadProblem(toProblem(caught));
    }
  }, [showDeleted]);
  useEffect(() => { void load(); }, [load]);

  const act = async (organization: DeploymentOrganization, change: () => Promise<unknown>) => {
    setBusy(organization.id);
    setProblem(null);
    try {
      await change();
      await load();
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setBusy(null);
    }
  };

  const names = new Map((organizations ?? []).map(organization => [organization.id, organization.name]));
  return (
    <div className="screen organizations-screen">
      <div className="section-header">
        <h2>Organizations</h2>
      </div>
      <p className="hint">
        You see each organization’s name and state, not what is inside it. {selfServe
          ? 'People may also create organizations themselves on this gateway.'
          : 'Only deployment admins create organizations on this gateway.'}
      </p>
      <CheckOption id="show-deleted" checked={showDeleted} onChange={event => setShowDeleted(event.target.checked)}>Show deleted organizations</CheckOption>
      <InlineError problem={loadProblem} />
      {!deleting && !restoring && <InlineError problem={problem} />}
      {organizations === null && !loadProblem && <p role="status">Loading organizations…</p>}
      {organizations?.length === 0 && <EmptyState title="No organizations" text="Create the first one." action={<Button type="button" onClick={() => setCreating(true)}>Create organization</Button>} />}
      {organizations && organizations.length > 0 && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Organization actions"><Button type="button" onClick={() => setCreating(true)}>Create organization</Button></div>
      )}
      {organizations && organizations.length > 0 && (
        <DataGrid aria-label="Organizations">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Name</TableHeader>
              <TableHeader>Short name</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Members</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {organizations.map(organization => (
              <TableRow className="data-row" key={organization.id}>
                <DataCell label="Name" className="cell-primary">
                  {organization.name}
                  {organization.deletion_requested_at && (
                    <span className="hint"> · deletion requested {formatDate(organization.deletion_requested_at)}</span>
                  )}
                  {organization.status === 'deleted' && storedItems(organization.stored).length > 0 && (
                    <span className="hint stored-secrets">
                      {' '}· still stored, encrypted: {storedItems(organization.stored).join(', ')}. Revoke them at each provider if it won’t be restored.
                    </span>
                  )}
                </DataCell>
                <DataCell label="Short name"><code>{organization.slug}</code></DataCell>
                <DataCell label="Status"><Badge kind={statusBadge[organization.status]}>{statusLabel[organization.status]}</Badge></DataCell>
                <DataCell label="Members">{organization.members}</DataCell>
                <DataCell label="Actions" className="row-actions">
                  {organization.status === 'active' && (
                    <Button type="button" variant="ghost" loading={busy === organization.id} onClick={() => { void act(organization, () => deployment.suspend(organization.id)); }}>Suspend</Button>
                  )}
                  {organization.status === 'suspended' && <>
                    <Button type="button" variant="ghost" loading={busy === organization.id} onClick={() => { void act(organization, () => deployment.unsuspend(organization.id)); }}>Unsuspend</Button>
                    <Button type="button" variant="danger" disabled={busy === organization.id} onClick={() => { setProblem(null); setDeleting(organization); }}>Delete</Button>
                  </>}
                  {organization.status === 'deleted' && (
                    <Button type="button" variant="ghost" disabled={busy === organization.id} onClick={() => { setProblem(null); setRestoring(organization); }}>Restore</Button>
                  )}
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}

      <section className="deployment-audit" aria-labelledby="deployment-audit-heading">
        <h3 id="deployment-audit-heading">Recent changes</h3>
        {events.length === 0 ? <p className="hint">Nothing yet.</p> : (
          <ul className="deployment-events">
            {events.map(event => (
              <li key={event.id}>
                <time dateTime={event.occurred_at}>{formatDate(event.occurred_at)}</time>{' '}
                <strong>{event.actor_email ?? (event.actor_user_id ? 'Someone' : 'The operator')}</strong>{' '}
                {eventText(event)}
                {!event.detail.name && event.organization_id && names.get(event.organization_id) ? ` (${names.get(event.organization_id)})` : ''}
              </li>
            ))}
          </ul>
        )}
      </section>

      <CreateOrganizationDialog open={creating} onClose={() => setCreating(false)} onCreated={() => { void load(); }} />
      <DeleteOrganizationDialog
        organization={deleting}
        onClose={() => setDeleting(null)}
        onDeleted={() => { setDeleting(null); void load(); }}
      />
      <RestoreOrganizationDialog
        organization={restoring}
        onClose={() => setRestoring(null)}
        onRestored={() => { setRestoring(null); void load(); }}
      />
    </div>
  );
}

function CreateOrganizationDialog({ open, onClose, onCreated }: { open: boolean; onClose: () => void; onCreated: () => void }) {
  const [name, setName] = useState('');
  const [slug, setSlug] = useState('');
  const [slugEdited, setSlugEdited] = useState(false);
  const [ownerChoice, setOwnerChoice] = useState<OwnerChoice>('invitation');
  const [email, setEmail] = useState('');
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [link, setLink] = useState<OwnerLink | null>(null);
  const [done, setDone] = useState(false);
  const close = () => {
    if (pending) return;
    setName(''); setSlug(''); setSlugEdited(false); setOwnerChoice('invitation'); setEmail('');
    setProblem(null); setLink(null); setDone(false);
    onClose();
  };
  const submit = async () => {
    setPending(true);
    setProblem(null);
    try {
      const created = await deployment.create({
        name: name.trim(),
        slug,
        owner: ownerChoice === 'none' ? undefined : { email: email.trim(), method: ownerChoice },
      });
      setLink(created.owner_link);
      setDone(true);
      onCreated();
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <Dialog open={open} onClose={close} title="Create organization">
      {done ? <>
        <Message kind="success" title={`${name.trim()} is created.`}>
          {link
            ? link.kind === 'invitation'
              ? `Send this invitation to ${link.email}. It works once, until ${formatDate(link.expires_at)}, and is not shown again.`
              : `Send this link to ${link.email} to choose their password. It works once, until ${formatDate(link.expires_at)}, and is not shown again.`
            : ownerChoice === 'local_account'
              ? `${email.trim()} already had an account and is now its owner.`
              : 'It has no owner yet; invite one from here later, or with `gateway invitation create`.'}
        </Message>
        {link && (
          <div className="secret-value-row">
            <code className="secret-value" aria-label="Owner link">{link.link}</code>
            <CopyButton text={link.link} label="Copy the owner link" />
          </div>
        )}
        <div className="dialog-actions"><Button type="button" onClick={close}>Done</Button></div>
      </> : (
        <form onSubmit={event => { event.preventDefault(); void submit(); }}>
          <FormField label="Name" htmlFor="new-organization-name">
            <TextInput
              id="new-organization-name"
              value={name}
              onChange={event => { setName(event.target.value); if (!slugEdited) setSlug(slugFrom(event.target.value)); }}
              required
              autoFocus
            />
          </FormField>
          <FormField label="Short name" htmlFor="new-organization-slug" hint="Lowercase letters, digits and hyphens.">
            <TextInput
              id="new-organization-slug"
              value={slug}
              onChange={event => { setSlugEdited(true); setSlug(event.target.value.toLowerCase()); }}
              pattern="[a-z0-9][a-z0-9\-]{0,63}"
              autoCapitalize="none"
              spellCheck={false}
              required
            />
          </FormField>
          <fieldset className="account-choice">
            <legend className="cds--label">First owner</legend>
            <RadioOption id="owner-invitation" name="first-owner" value="invitation" checked={ownerChoice === 'invitation'} onChange={() => setOwnerChoice('invitation')}>
              Invite them (they accept with a password or their identity provider)
            </RadioOption>
            <RadioOption id="owner-local" name="first-owner" value="local_account" checked={ownerChoice === 'local_account'} onChange={() => setOwnerChoice('local_account')}>
              Give them a local account now (they choose its password)
            </RadioOption>
            <RadioOption id="owner-none" name="first-owner" value="none" checked={ownerChoice === 'none'} onChange={() => setOwnerChoice('none')}>
              No owner yet
            </RadioOption>
          </fieldset>
          {ownerChoice !== 'none' && (
            <FormField label="Owner’s email address" htmlFor="new-organization-owner">
              <TextInput id="new-organization-owner" type="email" value={email} onChange={event => setEmail(event.target.value)} autoCapitalize="none" spellCheck={false} required />
            </FormField>
          )}
          <InlineError problem={problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={close} disabled={pending}>Cancel</Button>
            <Button type="submit" loading={pending}>Create</Button>
          </div>
        </form>
      )}
    </Dialog>
  );
}

/** What `stored` counts, as short phrases; empty when nothing is stored. */
export function storedItems(stored: StoredSecrets | undefined): string[] {
  if (!stored) return [];
  const count = (n: number, one: string, many: string) => (n > 0 ? [`${n} ${n === 1 ? one : many}`] : []);
  return [
    ...count(stored.provider_credentials, 'provider credential', 'provider credentials'),
    ...count(stored.cloud_connections, 'cloud connection', 'cloud connections'),
    ...count(stored.webhook_signing_secrets, 'webhook signing secret', 'webhook signing secrets'),
    ...(stored.identity_provider_client_secret ? ['its SSO client secret'] : []),
  ];
}

/** Deleting keeps the organization's secrets; say which, and what to do if
 * it won't come back. Counts only, never values. */
function StoredSecretsNotice({ stored }: { stored: StoredSecrets | undefined }) {
  const items = storedItems(stored);
  if (items.length === 0) return null;
  return (
    <Message kind="warning">
      <ul className="stored-secrets-list">
        {items.map(item => <li key={item}>{item}</li>)}
      </ul>
      These stay stored, encrypted, so the organization can be restored. If you won’t restore it, revoke them at each
      provider.
    </Message>
  );
}

function DeleteOrganizationDialog({ organization, onClose, onDeleted }: {
  organization: DeploymentOrganization | null;
  onClose: () => void;
  onDeleted: () => void;
}) {
  const [confirm, setConfirm] = useState('');
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const close = () => { if (pending) return; setConfirm(''); setProblem(null); onClose(); };
  const submit = async () => {
    if (!organization) return;
    setPending(true);
    setProblem(null);
    try {
      await deployment.remove(organization.id, confirm.trim());
      setConfirm('');
      onDeleted();
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <Dialog open={!!organization} onClose={close} title={`Delete ${organization?.name ?? 'organization'}`} alert>
      <form onSubmit={event => { event.preventDefault(); void submit(); }}>
        <p>
          The organization is hidden everywhere, its short name and email domains are freed, and its pending invitations
          are revoked. Everything else is kept, and it can be restored.
        </p>
        <StoredSecretsNotice stored={organization?.stored} />
        <FormField label={`Type ${organization?.slug ?? ''} to confirm`} htmlFor="delete-organization-confirm">
          <TextInput id="delete-organization-confirm" value={confirm} onChange={event => setConfirm(event.target.value)} autoCapitalize="none" spellCheck={false} autoComplete="off" required />
        </FormField>
        <InlineError problem={problem} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={close} disabled={pending}>Cancel</Button>
          <Button variant="danger" type="submit" loading={pending} disabled={confirm.trim() !== organization?.slug}>Delete</Button>
        </div>
      </form>
    </Dialog>
  );
}

function RestoreOrganizationDialog({ organization, onClose, onRestored }: {
  organization: DeploymentOrganization | null;
  onClose: () => void;
  onRestored: () => void;
}) {
  const [slug, setSlug] = useState('');
  const [needsSlug, setNeedsSlug] = useState(false);
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const close = () => { if (pending) return; setSlug(''); setNeedsSlug(false); setProblem(null); onClose(); };
  const submit = async () => {
    if (!organization) return;
    setPending(true);
    setProblem(null);
    try {
      await deployment.restore(organization.id, needsSlug ? slug : undefined);
      setSlug(''); setNeedsSlug(false);
      onRestored();
    } catch (caught) {
      const failure = toProblem(caught);
      if (failure.code === 'slug_taken') setNeedsSlug(true);
      setProblem(failure);
    } finally {
      setPending(false);
    }
  };
  return (
    <Dialog open={!!organization} onClose={close} title={`Restore ${organization?.name ?? 'organization'}`}>
      <form onSubmit={event => { event.preventDefault(); void submit(); }}>
        <p>
          It comes back suspended, with everything it had. Lifting the suspension turns its keys, sessions, webhooks
          and identity provider back on as they were.
        </p>
        {needsSlug && (
          <FormField label="New short name" htmlFor="restore-organization-slug" hint={`Another organization uses ${organization?.slug ?? 'its old one'} now.`}>
            <TextInput id="restore-organization-slug" value={slug} onChange={event => setSlug(event.target.value.toLowerCase())} pattern="[a-z0-9][a-z0-9\-]{0,63}" autoCapitalize="none" spellCheck={false} required />
          </FormField>
        )}
        <InlineError problem={problem} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={close} disabled={pending}>Cancel</Button>
          <Button type="submit" loading={pending}>Restore</Button>
        </div>
      </form>
    </Dialog>
  );
}

/**
 * Self-serve creation (`GATEWAY_SELF_SERVE_ORGANIZATIONS`): the person
 * becomes the new organization's owner. A session from one organization's
 * own provider acts only there, so it is asked to sign in with the
 * gateway's own sign-in instead.
 */
export function CreateOwnOrganizationDialog({ open, organizationSession, onClose, onCreated }: {
  open: boolean;
  organizationSession: boolean;
  onClose: () => void;
  onCreated: (organizationId: string) => void;
}) {
  const [name, setName] = useState('');
  const [slug, setSlug] = useState('');
  const [slugEdited, setSlugEdited] = useState(false);
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const close = () => {
    if (pending) return;
    setName(''); setSlug(''); setSlugEdited(false); setProblem(null);
    onClose();
  };
  const submit = async () => {
    setPending(true);
    setProblem(null);
    try {
      const created = await deployment.createOwn({ name: name.trim(), slug });
      setName(''); setSlug(''); setSlugEdited(false);
      onCreated(created.id);
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  const blocked = organizationSession || problem?.code === 'deployment_sign_in_required';
  return (
    <Dialog open={open} onClose={close} title="Create an organization">
      {blocked ? <>
        <Message kind="info" title="Sign in with this gateway’s own sign-in first.">
          You signed in through your organization’s identity provider, so this session works only in that
          organization. To create one of your own, sign out and sign in with a password on this gateway or with
          its sign-in (“Sign in without an organization name”).
        </Message>
        <div className="dialog-actions"><Button type="button" onClick={close}>Close</Button></div>
      </> : (
        <form onSubmit={event => { event.preventDefault(); void submit(); }}>
          <p>You become its owner.</p>
          <FormField label="Name" htmlFor="own-organization-name">
            <TextInput id="own-organization-name" value={name} onChange={event => { setName(event.target.value); if (!slugEdited) setSlug(slugFrom(event.target.value)); }} required autoFocus />
          </FormField>
          <FormField label="Short name" htmlFor="own-organization-slug" hint="Lowercase letters, digits and hyphens.">
            <TextInput id="own-organization-slug" value={slug} onChange={event => { setSlugEdited(true); setSlug(event.target.value.toLowerCase()); }} pattern="[a-z0-9][a-z0-9\-]{0,63}" autoCapitalize="none" spellCheck={false} required />
          </FormField>
          <InlineError problem={problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={close} disabled={pending}>Cancel</Button>
            <Button type="submit" loading={pending}>Create</Button>
          </div>
        </form>
      )}
    </Dialog>
  );
}

/** Settings → Deletion: an owner asks the deployment's admins to delete the
 * organization, or withdraws the request. */
export function DeletionRequestSection({ organizationId, canWrite }: { organizationId: string; canWrite: boolean }) {
  const [requestedAt, setRequestedAt] = useState<string | null | undefined>(undefined);
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  useEffect(() => {
    let current = true;
    deployment.deletionRequest(organizationId)
      .then(value => { if (current) setRequestedAt(value.requested_at); })
      .catch(caught => { if (current) setProblem(toProblem(caught)); });
    return () => { current = false; };
  }, [organizationId]);
  const change = async (request: boolean) => {
    setPending(true);
    setProblem(null);
    try {
      setRequestedAt((await deployment.setDeletionRequest(organizationId, request)).requested_at);
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <section className="deletion-request" aria-labelledby="deletion-request-heading">
      <h3 id="deletion-request-heading">Delete this organization</h3>
      <p>
        The gateway’s admins delete organizations. Ask them here; they suspend it first. Deleting it keeps the secrets it
        stores (provider credentials, cloud connections, webhook signing secrets, its sign-in client secret), encrypted,
        so it can be restored; if it won’t be, revoke them at each provider. Until then nothing changes.
      </p>
      {requestedAt === undefined && !problem && <p role="status">Loading…</p>}
      {requestedAt && <Message kind="warning">You asked for its deletion on {formatDate(requestedAt)}.</Message>}
      <InlineError problem={problem} />
      {canWrite && requestedAt !== undefined && (
        requestedAt
          ? <Button type="button" variant="ghost" loading={pending} onClick={() => { void change(false); }}>Withdraw the request</Button>
          : <Button type="button" variant="danger" loading={pending} onClick={() => { void change(true); }}>Ask for deletion</Button>
      )}
      {!canWrite && <p className="hint">Only owners can ask for it.</p>}
    </section>
  );
}
