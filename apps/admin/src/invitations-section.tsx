import React, { useState } from 'react';
import {
  api,
  newIdempotencyKey,
  type Invitation,
  type InvitationRole,
} from './api';
import type { CollectionState } from './use-collection';
import type { MutationState } from './mutation-state';
import {
  Badge,
  Button,
  ConfirmDialog,
  DataCell,
  Dialog,
  EmptyState,
  FormField,
  InlineError,
  LoadMore,
  SecretRevealPanel,
  Select,
  formatDate,
} from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow, Message } from './ui';

export const invitationRoleLabels: Record<InvitationRole, string> = {
  owner: 'Owner',
  admin: 'Admin',
  auditor: 'Auditor',
  member: 'Member',
  'read-only': 'Read-only',
};

/** How long a new link works, in hours; the API allows 1 to 720. */
export const invitationValidities: [number, string][] = [
  [24, '1 day'],
  [72, '3 days'],
  [168, '7 days'],
  [720, '30 days'],
];
const DEFAULT_VALIDITY = 72;

type NewLink = { link: string; role: InvitationRole; expiresAt: string; requestId?: string };

/**
 * The organization's invitations. Shown only to people who may read them;
 * `grantableRoles` (from the list response) holds the roles the caller may
 * invite in, so Invite and Revoke are hidden when it is empty.
 */
export function InvitationsSection({
  organizationId,
  invitations,
  grantableRoles,
  mutation,
  onSuccess,
}: {
  organizationId: string;
  invitations: CollectionState<Invitation>;
  grantableRoles: InvitationRole[];
  mutation: MutationState<unknown>;
  onSuccess: () => void;
}) {
  const canWrite = grantableRoles.length > 0;
  const [inviteOpen, setInviteOpen] = useState(false);
  const [role, setRole] = useState<InvitationRole>('member');
  const [validHours, setValidHours] = useState(DEFAULT_VALIDITY);
  // One key per request: an unchanged retry after a lost response replays
  // the same invitation instead of making a second one; changing the role or
  // validity makes it a new request, with a new key.
  const [inviteKey, setInviteKey] = useState('');
  const [newLink, setNewLink] = useState<NewLink | null>(null);
  const [replayed, setReplayed] = useState(false);
  const [revoking, setRevoking] = useState<Invitation | null>(null);
  // One key per confirmation, for the same reason.
  const [revokeKey, setRevokeKey] = useState('');

  const openInvite = () => {
    mutation.clearProblem();
    setRole(grantableRoles.includes('member') ? 'member' : grantableRoles[grantableRoles.length - 1]);
    setValidHours(DEFAULT_VALIDITY);
    setInviteKey(newIdempotencyKey());
    setNewLink(null);
    setReplayed(false);
    setInviteOpen(true);
  };

  const submitInvite = async () => {
    let created: NewLink | null = null;
    let wasReplay = false;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.createInvitation(organizationId, role, validHours, inviteKey);
      if (data.link) {
        created = { link: data.link, role: data.invitation.role, expiresAt: data.invitation.expires_at, requestId: meta.requestId };
      } else {
        wasReplay = true;
      }
      // The link stays out of the mutation result, which the page may show.
      return { data: { id: data.invitation.id }, requestId: meta.requestId };
    });
    if (!ok) return;
    setInviteOpen(false);
    setNewLink(created);
    setReplayed(wasReplay);
    onSuccess();
  };

  const confirmRevoke = async () => {
    if (!revoking) return;
    const target = revoking;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.revokeInvitation(organizationId, target.id, revokeKey);
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setRevoking(null);
    onSuccess();
  };

  return (
    <>
      <div className="section-header">
        <h3>Invitations</h3>
      </div>
      <p className="cds--form__helper-text">
        An invitation is a link that adds whoever signs in with it to this organization, in the role
        you choose. It works once, until it expires. The gateway does not send email: you send the
        link to the person yourself.
      </p>

      {newLink && (
        <SecretRevealPanel
          secret={newLink.link}
          label={`Invitation link · ${invitationRoleLabels[newLink.role]}`}
          requestId={newLink.requestId}
          onDismiss={() => setNewLink(null)}
          valueLabel="Invitation link"
          copyLabel="Copy invitation link"
          dismissText="Done"
          warning={
            <>
              Copy this link and send it to the person you are inviting, for example by email or chat.
              The gateway does not send it for you. It works once, until {formatDate(newLink.expiresAt)}.
              This is the only time the link is shown: after you close this panel it cannot be shown
              again, but you can revoke the invitation and invite again.
            </>
          }
        />
      )}
      {replayed && (
        <Message kind="info">
          This invitation was already created, so its link cannot be shown again. Revoke it and
          invite again if you no longer have the link.
        </Message>
      )}

      {invitations.loading && <p role="status">Loading invitations…</p>}
      <InlineError problem={invitations.error} />
      {invitations.error && (
        <Button variant="ghost" onClick={() => { void invitations.load(true); }}>
          Try again
        </Button>
      )}
      {!invitations.loading && !invitations.error && invitations.items.length === 0 && (
        <EmptyState
          title="No invitations"
          text={canWrite ? 'Invite someone to give them access to this organization.' : 'Nobody has been invited yet.'}
          action={canWrite ? <Button onClick={openInvite}>Invite</Button> : undefined}
        />
      )}
      {invitations.items.length > 0 && canWrite && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Invitation actions"><Button onClick={openInvite}>Invite</Button></div>
      )}
      {invitations.items.length > 0 && (
        <DataGrid aria-label="Invitations">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Role</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Created</TableHeader>
              <TableHeader>Expires</TableHeader>
              {canWrite && <TableHeader className="row-actions">Actions</TableHeader>}
            </TableRow>
          </TableHead>
          <TableBody>
            {invitations.items.map(invitation => (
              <TableRow className="data-row" key={invitation.id}>
                <DataCell label="Role" className="cell-primary">
                  {invitationRoleLabels[invitation.role] ?? invitation.role}
                </DataCell>
                <DataCell label="Status">
                  <Badge status={invitation.status} />
                </DataCell>
                <DataCell label="Created">{formatDate(invitation.created_at)}</DataCell>
                <DataCell label="Expires">{formatDate(invitation.expires_at)}</DataCell>
                {canWrite && (
                  <DataCell label="Actions" className="row-actions">
                    {invitation.status === 'pending' && (
                      <Button variant="danger" onClick={() => { mutation.clearProblem(); setRevokeKey(newIdempotencyKey()); setRevoking(invitation); }}>
                        Revoke
                      </Button>
                    )}
                  </DataCell>
                )}
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore hasMore={invitations.hasMore} loading={invitations.loadingMore} onClick={invitations.loadMore} />

      <Dialog
        open={inviteOpen}
        onClose={() => { if (!mutation.submitting) setInviteOpen(false); }}
        title="Invite someone"
      >
        <form
          onSubmit={event => {
            event.preventDefault();
            void submitInvite();
          }}
        >
          <FormField label="Role" htmlFor="invitation-role" hint="You can invite people in your own role or a lower one.">
            <Select
              id="invitation-role"
              value={role}
              onChange={event => { setRole(event.target.value as InvitationRole); setInviteKey(newIdempotencyKey()); }}
              autoFocus
            >
              {grantableRoles.map(option => (
                <option key={option} value={option}>
                  {invitationRoleLabels[option]}
                </option>
              ))}
            </Select>
          </FormField>
          <FormField label="Link works for" htmlFor="invitation-validity">
            <Select
              id="invitation-validity"
              value={String(validHours)}
              onChange={event => { setValidHours(Number(event.target.value)); setInviteKey(newIdempotencyKey()); }}
            >
              {invitationValidities.map(([hours, label]) => (
                <option key={hours} value={hours}>
                  {label}
                </option>
              ))}
            </Select>
          </FormField>
          <p className="cds--form__helper-text">
            You will get a link to send to the person yourself. It is shown only once.
          </p>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setInviteOpen(false)} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              Create link
            </Button>
          </div>
        </form>
      </Dialog>

      <ConfirmDialog
        open={!!revoking}
        onClose={() => setRevoking(null)}
        onConfirm={() => { void confirmRevoke(); }}
        title="Revoke invitation"
        confirmText="Revoke"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Revoke this invitation for {revoking ? invitationRoleLabels[revoking.role].toLowerCase() : ''} access?
        Its link stops working at once. Nobody who already joined loses access.
      </ConfirmDialog>
    </>
  );
}
