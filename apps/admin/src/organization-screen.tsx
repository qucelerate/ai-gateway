import React, { useEffect, useMemo, useState } from 'react';
import {
  api,
  type Context,
  type AgentRole,
  type InvitationRole,
  type MembershipRole,
  type AdminRole,
  type ApiProblem,
  type IdentityProvider,
  type Team,
} from './api';
import { membershipRoles, normalizeMembershipRole, membershipRoleLabel } from './membership-role';
import { useCollection } from './use-collection';
import { useMutationState } from './mutation-state';
import { InvitationsSection } from './invitations-section';
import { AgentsSection } from './agents-section';
import { SignInSection, testedOrganization } from './sign-in-section';
import { RoutingSection } from './routing-section';
import {
  Button,
  ConfirmDialog,
  Dialog,
  FormField,
  Select,
  TextInput,
  Badge,
  MutationStatus,
  EmptyState,
  LoadMore,
  InlineError,
  DataCell,
  uuidHint,
  useRouteTab,
} from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow, CheckOption } from './ui';
import { PasswordResetLinkDialog, RequireSsoSetting } from './local-accounts';
import { DomainsSection } from './domains-section';
import { DeletionRequestSection } from './organizations-screen';


type Tab = 'members' | 'invitations' | 'teams' | 'access' | 'agents' | 'sign-in' | 'routing' | 'deletion';

/** People (who belongs, in which teams, with what access), Settings (how
 * they sign in, which routing is served), and the agents Identities shows
 * beside service accounts. */
export type OrganizationSection = 'people' | 'settings' | 'agents';

export const sectionTabs: Record<OrganizationSection, [Tab, string][]> = {
  people: [['members', 'Members'], ['invitations', 'Invitations'], ['teams', 'Teams'], ['access', 'Access']],
  settings: [['sign-in', 'Sign-in'], ['routing', 'Routing'], ['deletion', 'Deletion']],
  agents: [['agents', 'Agents']],
};

/** The tab to open: Sign-in when coming back from testing this
 * organization's sign-in. */
function initialTab(section: OrganizationSection, organizationId: string): Tab {
  if (section === 'settings' && testedOrganization(window.location.search) === organizationId) return 'sign-in';
  return sectionTabs[section][0][0];
}

const nothing = Promise.resolve({ items: [], next_page_token: null });

const orgGrantableAdminRoles: AdminRole[] = [
  'organization_owner',
  'organization_admin',
  'operator',
  'auditor',
  'read_only_member',
];
export function teamSelectorOptions(teams: Team[], context: Context, organizationId: string): Team[] {
  const currentOrganizationTeams = teams.filter(team => team.organization_id === organizationId);
  if (context.organization_id !== organizationId || !context.team_id || currentOrganizationTeams.some(team => team.id === context.team_id)) {
    return currentOrganizationTeams;
  }
  return [...currentOrganizationTeams, {
    id: context.team_id,
    organization_id: organizationId,
    name: `Current team · ${uuidHint(context.team_id)}`,
    version: 0,
  }];
}
function roleLabel(role: string) {
  return role.replace(/_/g, ' ');
}

export function OrganizationScreen({
  section = 'people',
  organizationId,
  userId,
  context,
}: {
  section?: OrganizationSection;
  organizationId: string;
  userId: string;
  context: Context;
}) {
  const tabs = sectionTabs[section];
  const tabKeys = React.useMemo(() => tabs.map(([key]) => key), [tabs]);
  const [requestedTab] = useRouteTab(tabKeys, initialTab(section, organizationId));
  // Each section reads only what it shows.
  const people = section === 'people';
  const settings = section === 'settings';
  const mutation = useMutationState<unknown>();

  const teams = useCollection(
    React.useCallback(
      params => (people ? api.listTeams(organizationId, params) : nothing),
      [organizationId, people],
    ),
    [organizationId],
  );
  const orgMembers = useCollection(
    React.useCallback(params => (people ? api.listOrgMembers(organizationId, params) : nothing), [organizationId, people]),
    [organizationId],
  );
  const roleBindings = useCollection(
    React.useCallback(params => (people ? api.listRoleBindings(organizationId, params) : nothing), [organizationId, people]),
    [organizationId],
  );
  // Null until the first page loads: Invite appears only for roles the
  // caller may grant.
  const [grantableRoles, setGrantableRoles] = useState<InvitationRole[] | null>(null);
  const invitations = useCollection(
    React.useCallback(async params => {
      if (!people) return nothing;
      const page = await api.listInvitations(organizationId, params);
      setGrantableRoles(page.grantable_roles);
      return page;
    }, [organizationId, people]),
    [organizationId],
  );
  // Hidden only once the server refuses (403); another failure keeps the
  // tab, with the error and a retry.
  const canReadInvitations =
    invitations.error ? invitations.error.status !== 403 : grantableRoles !== null;
  // Agents, the same way: the list is not paged.
  const [agentRoles, setAgentRoles] = useState<AgentRole[] | null>(null);
  const agents = useCollection(
    React.useCallback(async () => {
      if (section !== 'agents') return nothing;
      const list = await api.listAgents(organizationId);
      setAgentRoles(list.grantable_roles);
      return { items: list.items, next_page_token: null };
    }, [organizationId, section]),
    [organizationId],
  );
  const canReadAgents = agents.error ? agents.error.status !== 403 : agentRoles !== null;
  // The organization's own sign-in provider. Hidden once the server
  // refuses to show it (403, or 404 for someone with no role here).
  const [provider, setProvider] = useState<IdentityProvider | null>(null);
  const [providerLoading, setProviderLoading] = useState(true);
  const [providerError, setProviderError] = useState<ApiProblem | null>(null);
  const loadProvider = React.useCallback(async () => {
    if (!settings) return;
    setProviderLoading(true);
    setProviderError(null);
    try {
      setProvider(await api.identityProvider(organizationId));
    } catch (caught) {
      setProvider(null);
      setProviderError(
        (caught as { problem?: ApiProblem }).problem ?? {
          type: 'about:blank', code: 'network_error', title: 'network_error', status: 0,
          detail: 'The gateway could not be reached.', instance: '', request_id: '',
        },
      );
    } finally {
      setProviderLoading(false);
    }
  }, [organizationId, settings]);
  useEffect(() => {
    void loadProvider();
  }, [loadProvider]);
  const canReadProvider = !(providerError && (providerError.status === 403 || providerError.status === 404));
  // Routing versions need catalog read; hidden once the server refuses.
  const configVersions = useCollection(
    React.useCallback(params => (settings ? api.listConfigVersions(organizationId, params) : nothing), [organizationId, settings]),
    [organizationId],
  );
  const canReadRouting = !(configVersions.error && (configVersions.error.status === 403 || configVersions.error.status === 404));
  const isOwner = context.organization_id === organizationId && context.role.toLowerCase() === 'owner';
  const visibleTabs = tabs.filter(([key]) =>
    (key !== 'invitations' || canReadInvitations)
    && (key !== 'routing' || canReadRouting)
    && (key !== 'deletion' || isOwner),
  );
  // A tab the role cannot see (named in the path, or hidden once its read is
  // refused) opens the first one it can; Settings may have none.
  const tab = visibleTabs.some(([key]) => key === requestedTab) ? requestedTab : visibleTabs[0]?.[0];
  useEffect(() => {
    mutation.clearProblem();
    mutation.clearResult();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  const [selectedTeamId, setSelectedTeamId] = useState<string>('');
  const teamMembers = useCollection(
    React.useCallback(
      params =>
        people && selectedTeamId
          ? api.listTeamMembers(organizationId, selectedTeamId, params)
          : Promise.resolve({ items: [], next_page_token: null }),
      [organizationId, selectedTeamId, people],
    ),
    [organizationId, selectedTeamId],
  );

  const teamOptions = teamSelectorOptions(teams.items, context, organizationId);
  const scopedTeam = context.organization_id === organizationId && context.team_id ? context.team_id : null;

  useEffect(() => {
    setSelectedTeamId('');
  }, [organizationId]);

  useEffect(() => {
    if (teamOptions.length && !selectedTeamId) {
      setSelectedTeamId(teamOptions[0].id);
    }
  }, [teamOptions, selectedTeamId]);

  // Once the teams reload without the selected one (deleted, or on a page
  // not loaded again), the picker and the open members follow the first
  // team, so a change never targets a team the picker does not show.
  useEffect(() => {
    if (!teams.loading && selectedTeamId && teamOptions.length && !teamOptions.some(team => team.id === selectedTeamId)) {
      setSelectedTeamId(teamOptions[0].id);
    }
  }, [teams.loading, teamOptions, selectedTeamId]);

  const refreshForTab = () => {
    if (tab === 'teams') teams.load(true);
    if (tab === 'members') orgMembers.load(true);
    if (tab === 'access') roleBindings.load(true);
    if (tab === 'invitations') invitations.load(true);
    if (tab === 'agents') agents.load(true);
    if (tab === 'routing') configVersions.load(true);
  };

  const onSuccess = () => {
    mutation.clearResult();
    refreshForTab();
  };
  // A member change reloads only that team's members, keeping every page of
  // teams loaded and the selected team in the picker.
  const onTeamMembersSuccess = () => {
    mutation.clearResult();
    teamMembers.load(true);
  };

  if (!tab) {
    return (
      <div className="screen">
        <EmptyState title="Nothing to change here" text="Your role can’t see this organization’s sign-in or routing settings." />
      </div>
    );
  }
  const status = (
    <MutationStatus
      result={mutation.result}
      problem={mutation.problem}
      onDismiss={mutation.clearProblem}
    />
  );
  if (section === 'agents') {
    // Shown under Identities' own tab, so no tab bar of its own.
    return (
      <div className="screen">
        {status}
        {canReadAgents ? (
          <AgentsSection
            organizationId={organizationId}
            agents={agents}
            grantableRoles={agentRoles ?? []}
            mutation={mutation}
            onSuccess={onSuccess}
          />
        ) : agents.loading ? null : (
          <EmptyState title="Agents aren’t shown to your role" text="An owner or admin of this organization can see and manage its agents." />
        )}
      </div>
    );
  }

  return (
    <div className="screen">
      {/* The section is chosen in the side navigation; the page's heading
          names it. */}
      <section className="screen-panel page-section" id="organization-panel">
        {status}

        {tab === 'teams' && (
          <>
            <TeamsTab
              organizationId={organizationId}
              teams={teams}
              mutation={mutation}
              onSuccess={onSuccess}
            />
            {/* A team's members, once there is a team to pick. */}
            {teamOptions.length > 0 && (
              <div className="stacked-section">
                <TeamMembersTab
                  organizationId={organizationId}
                  teams={teams}
                  teamOptions={teamOptions}
                  limitedScope={!!scopedTeam && teams.items.length === 0 && teams.error?.status === 403}
                  members={teamMembers}
                  selectedTeamId={selectedTeamId}
                  onSelectTeam={setSelectedTeamId}
                  mutation={mutation}
                  onSuccess={onTeamMembersSuccess}
                />
              </div>
            )}
          </>
        )}
        {tab === 'members' && (
          <OrgMembersTab
            organizationId={organizationId}
            members={orgMembers}
            mutation={mutation}
            onSuccess={onSuccess}
            canResetPasswords={context.organization_id === organizationId && context.role.toLowerCase() === 'owner'}
          />
        )}
        {tab === 'invitations' && canReadInvitations && (
          <InvitationsSection
            organizationId={organizationId}
            invitations={invitations}
            grantableRoles={grantableRoles ?? []}
            mutation={mutation}
            onSuccess={onSuccess}
          />
        )}
        {tab === 'sign-in' && canReadProvider && (
          <SignInSection
            organizationId={organizationId}
            provider={provider}
            loading={providerLoading}
            loadError={providerError}
            onReload={() => { mutation.clearResult(); void loadProvider(); }}
            role={context.organization_id === organizationId ? context.role : null}
            mutation={mutation}
          />
        )}
        {/* Any member reads the domains and the SSO setting, also without
            the right to read the identity provider. */}
        {tab === 'sign-in' && (
          <>
            <DomainsSection
              organizationId={organizationId}
              canWrite={context.organization_id === organizationId && context.role.toLowerCase() === 'owner'}
            />
            <RequireSsoSetting
              organizationId={organizationId}
              canWrite={context.organization_id === organizationId && context.role.toLowerCase() === 'owner'}
            />
          </>
        )}
        {tab === 'deletion' && isOwner && (
          <DeletionRequestSection organizationId={organizationId} canWrite={isOwner} />
        )}
        {tab === 'routing' && canReadRouting && (
          <RoutingSection
            organizationId={organizationId}
            versions={configVersions}
            mutation={mutation}
            onSuccess={onSuccess}
          />
        )}
        {tab === 'access' && (
          <RoleBindingsTab
            organizationId={organizationId}
            bindings={roleBindings}
            mutation={mutation}
            onSuccess={onSuccess}
          />
        )}
        {tab === 'access' && (
          <div className="stacked-section">
            <div className="section-header"><h3>Your access</h3></div>
            <EffectiveAccessTab
              userId={userId}
              context={context}
              orgMembers={orgMembers.items}
              teamMembers={teamMembers.items}
              teams={teams.items}
              roleBindings={roleBindings.items}
            />
          </div>
        )}
      </section>
    </div>
  );
}

function TeamsTab({
  organizationId,
  teams,
  mutation,
  onSuccess,
}: {
  organizationId: string;
  teams: ReturnType<typeof useCollection<Team>>;
  mutation: ReturnType<typeof useMutationState<unknown>>;
  onSuccess: () => void;
}) {
  const [createOpen, setCreateOpen] = useState(false);
  const [editing, setEditing] = useState<Team | null>(null);
  const [deleting, setDeleting] = useState<Team | null>(null);
  const [name, setName] = useState('');

  const reset = () => {
    setName('');
    setCreateOpen(false);
    setEditing(null);
    setDeleting(null);
  };

  const submit = async () => {
    const ok = editing
      ? await mutation.submit(async () => {
          const { data, meta } = await api.updateTeam(organizationId, editing.id, name, editing.version);
          return { data, requestId: meta.requestId };
        })
      : await mutation.submit(async () => {
          const { data, meta } = await api.createTeam(organizationId, name);
          return { data, requestId: meta.requestId };
        });
    if (!ok) return;
    reset();
    onSuccess();
  };

  const confirmDelete = async () => {
    if (!deleting) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.deleteTeam(organizationId, deleting.id, deleting.version);
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setDeleting(null);
    onSuccess();
  };

  return (
    <>
      <div className="section-header">
        <h3>Teams</h3>
      </div>
      {teams.loading && <p role="status">Loading teams…</p>}
      <InlineError problem={teams.error} />
      {!teams.loading && teams.items.length === 0 && (
        <EmptyState title="No teams" text="Create a team to organize members and resources." action={<Button onClick={() => { setName(''); setCreateOpen(true); }}>Create team</Button>} />
      )}
      {teams.items.length > 0 && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Team actions"><Button onClick={() => { setName(''); setCreateOpen(true); }}>Create team</Button></div>
      )}
      {teams.items.length > 0 && (
        <DataGrid aria-label="Teams">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Name</TableHeader>
              <TableHeader>Version</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {teams.items.map(team => (
              <TableRow className="data-row" key={team.id}>
                <DataCell label="Name" className="cell-primary">
                  {team.name}
                </DataCell>
                <DataCell label="Version">v{team.version}</DataCell>
                <DataCell label="Actions" className="row-actions">
                  <Button
                    variant="ghost"
                    onClick={() => { setEditing(team); setName(team.name); }}
                  >
                    Edit
                  </Button>
                  <Button variant="danger" onClick={() => setDeleting(team)}>
                    Delete
                  </Button>
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore hasMore={teams.hasMore} loading={teams.loadingMore} onClick={teams.loadMore} />

      <Dialog
        open={createOpen || !!editing}
        onClose={() => { if (!mutation.submitting) reset(); }}
        title={editing ? 'Rename team' : 'Create team'}
      >
        <form
          onSubmit={event => {
            event.preventDefault();
            void submit();
          }}
        >
          <FormField label="Team name" htmlFor="team-name" error={undefined}>
            <TextInput
              id="team-name"
              value={name}
              onChange={event => setName(event.target.value)}
              autoFocus
              required
            />
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={reset} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              {editing ? 'Save' : 'Create'}
            </Button>
          </div>
        </form>
      </Dialog>

      <ConfirmDialog
        open={!!deleting}
        onClose={() => setDeleting(null)}
        onConfirm={() => { void confirmDelete(); }}
        title="Delete team"
        confirmText="Delete"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Delete the team “{deleting?.name}”? This cannot be undone.
      </ConfirmDialog>
    </>
  );
}

function OrgMembersTab({
  organizationId,
  members,
  mutation,
  onSuccess,
  canResetPasswords,
}: {
  organizationId: string;
  members: ReturnType<typeof useCollection<import('./api').Membership>>;
  mutation: ReturnType<typeof useMutationState<unknown>>;
  onSuccess: () => void;
  canResetPasswords: boolean;
}) {
  return (
    <MembershipTab
      organizationId={organizationId}
      scope="org"
      members={members}
      mutation={mutation}
      onSuccess={onSuccess}
      canResetPasswords={canResetPasswords}
    />
  );
}

function TeamMembersTab({
  organizationId,
  teams,
  teamOptions,
  limitedScope,
  members,
  selectedTeamId,
  onSelectTeam,
  mutation,
  onSuccess,
}: {
  organizationId: string;
  teams: ReturnType<typeof useCollection<Team>>;
  teamOptions: Team[];
  limitedScope: boolean;
  members: ReturnType<typeof useCollection<import('./api').Membership>>;
  selectedTeamId: string;
  onSelectTeam: (id: string) => void;
  mutation: ReturnType<typeof useMutationState<unknown>>;
  onSuccess: () => void;
}) {
  return (
    <>
      <div className="section-header">
        <h3>Team members</h3>
        <Select
          aria-label="Select team"
          value={selectedTeamId}
          onChange={event => onSelectTeam(event.target.value)}
        >
          {teamOptions.map(team => (
            <option key={team.id} value={team.id}>
              {team.name}
            </option>
          ))}
        </Select>
        {limitedScope && <p className="meta" role="status">Organization team listing is unavailable. Showing only your confirmed team.</p>}
      </div>
      {selectedTeamId ? (
        <MembershipTab
          organizationId={organizationId}
          scope="team"
          teamId={selectedTeamId}
          members={members}
          mutation={mutation}
          onSuccess={onSuccess}
        />
      ) : (
        <EmptyState title="No team selected" text="Select or create a team first." />
      )}
    </>
  );
}

function MembershipTab({
  organizationId,
  scope,
  teamId,
  members,
  mutation,
  onSuccess,
  canResetPasswords = false,
}: {
  organizationId: string;
  scope: 'org' | 'team';
  teamId?: string;
  members: ReturnType<typeof useCollection<import('./api').Membership>>;
  mutation: ReturnType<typeof useMutationState<unknown>>;
  onSuccess: () => void;
  canResetPasswords?: boolean;
}) {
  type Membership = import('./api').Membership;
  const [resetting, setResetting] = useState<string | null>(null);
  const [createOpen, setCreateOpen] = useState(false);
  const [editing, setEditing] = useState<Membership | null>(null);
  const [deleting, setDeleting] = useState<Membership | null>(null);
  const [userId, setUserId] = useState('');
  const [role, setRole] = useState<MembershipRole>('Member');
  const [enabled, setEnabled] = useState(true);

  const reset = () => {
    setUserId('');
    setRole('Member');
    setEnabled(true);
    setCreateOpen(false);
    setEditing(null);
    setDeleting(null);
  };

  const submit = async () => {
    let ok: boolean;
    if (editing) {
      const patch: { role?: MembershipRole; enabled?: boolean } = {};
      if (role !== normalizeMembershipRole(editing.role)) patch.role = role;
      if (enabled !== editing.enabled) patch.enabled = enabled;
      ok = await mutation.submit(async () => {
        const { data, meta } =
          scope === 'org'
            ? await api.updateOrgMember(organizationId, editing.user_id, patch, editing.version)
            : await api.updateTeamMember(
                organizationId,
                teamId!,
                editing.user_id,
                patch,
                editing.version,
              );
        return { data, requestId: meta.requestId };
      });
    } else {
      ok = await mutation.submit(async () => {
        const { data, meta } =
          scope === 'org'
            ? await api.createOrgMember(organizationId, userId, role)
            : await api.createTeamMember(organizationId, teamId!, userId, role);
        return { data, requestId: meta.requestId };
      });
    }
    if (!ok) return;
    reset();
    onSuccess();
  };

  const confirmDelete = async () => {
    if (!deleting) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } =
        scope === 'org'
          ? await api.deleteOrgMember(organizationId, deleting.user_id, deleting.version)
          : await api.deleteTeamMember(organizationId, teamId!, deleting.user_id, deleting.version);
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setDeleting(null);
    onSuccess();
  };

  const openEdit = (member: Membership) => {
    setEditing(member);
    setUserId(member.user_id);
    setRole(normalizeMembershipRole(member.role));
    setEnabled(member.enabled);
  };

  return (
    <>
      <div className="section-header">
        <h3>{scope === 'org' ? 'Organization members' : 'Members'}</h3>
      </div>
      {members.loading && <p role="status">Loading members…</p>}
      <InlineError problem={members.error} />
      {!members.loading && members.items.length === 0 && (
        <EmptyState title="No members" text="Add a member to grant access." action={<Button onClick={() => { reset(); setCreateOpen(true); }}>Add member</Button>} />
      )}
      {members.items.length > 0 && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Member actions"><Button onClick={() => { reset(); setCreateOpen(true); }}>Add member</Button></div>
      )}
      {members.items.length > 0 && (
        <DataGrid aria-label="Members">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Person</TableHeader>
              <TableHeader>Role</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {members.items.map(member => (
              <TableRow className="data-row" key={`${member.team_id ?? 'org'}-${member.user_id}`}>
                <DataCell label="Person" className="cell-primary" title={member.user_id}>
                  <Person user={member} />
                </DataCell>
                <DataCell label="Role">{membershipRoleLabel(member.role)}</DataCell>
                <DataCell label="Status">
                  <Badge status={member.enabled ? 'active' : 'disabled'} />
                </DataCell>
                <DataCell label="Actions" className="row-actions">
                  <Button variant="ghost" onClick={() => openEdit(member)}>
                    Edit
                  </Button>
                  {canResetPasswords && (
                    <Button variant="ghost" onClick={() => setResetting(member.user_id)}>
                      Reset password
                    </Button>
                  )}
                  <Button variant="danger" onClick={() => setDeleting(member)}>
                    Remove
                  </Button>
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore hasMore={members.hasMore} loading={members.loadingMore} onClick={members.loadMore} />

      <Dialog
        open={createOpen || !!editing}
        onClose={() => { if (!mutation.submitting) reset(); }}
        title={editing ? 'Edit membership' : 'Add member'}
      >
        <form
          onSubmit={event => {
            event.preventDefault();
            void submit();
          }}
        >
          {!editing && (
            <FormField label="User ID" htmlFor="member-user-id" hint="A UUID for the user account.">
              <TextInput
                id="member-user-id"
                value={userId}
                onChange={event => setUserId(event.target.value)}
                required
                pattern="[0-9a-fA-F-]{36}"
                autoFocus
              />
            </FormField>
          )}
          <FormField label="Role" htmlFor="member-role">
            <Select
              id="member-role"
              value={role}
              onChange={event => setRole(normalizeMembershipRole(event.target.value))}
            >
              {membershipRoles.map(r => (
                <option key={r} value={r}>
                  {membershipRoleLabel(r)}
                </option>
              ))}
            </Select>
          </FormField>
          <FormField label="Enabled" htmlFor="member-enabled">
            <CheckOption
                id="member-enabled"
               
                checked={enabled}
                onChange={event => setEnabled(event.target.checked)}>Account is enabled</CheckOption>
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={reset} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              {editing ? 'Save' : 'Add'}
            </Button>
          </div>
        </form>
      </Dialog>

      <PasswordResetLinkDialog organizationId={organizationId} userId={resetting} onClose={() => setResetting(null)} />

      <ConfirmDialog
        open={!!deleting}
        onClose={() => setDeleting(null)}
        onConfirm={() => { void confirmDelete(); }}
        title={scope === 'org' ? 'Remove organization member' : 'Remove team member'}
        confirmText="Remove"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Remove this membership? The user will lose the associated access.
      </ConfirmDialog>
    </>
  );
}

function RoleBindingsTab({
  organizationId,
  bindings,
  mutation,
  onSuccess,
}: {
  organizationId: string;
  bindings: ReturnType<typeof useCollection<import('./api').RoleBinding>>;
  mutation: ReturnType<typeof useMutationState<unknown>>;
  onSuccess: () => void;
}) {
  type RoleBinding = import('./api').RoleBinding;
  const [createOpen, setCreateOpen] = useState(false);
  const [editing, setEditing] = useState<RoleBinding | null>(null);
  const [deleting, setDeleting] = useState<RoleBinding | null>(null);
  const [userId, setUserId] = useState('');
  const [role, setRole] = useState<AdminRole>('operator');

  const reset = () => {
    setUserId('');
    setRole('operator');
    setCreateOpen(false);
    setEditing(null);
    setDeleting(null);
  };

  const submit = async () => {
    const ok = editing
      ? await mutation.submit(async () => {
          const { data, meta } = await api.updateRoleBinding(
            organizationId,
            editing.id,
            role,
            editing.version,
          );
          return { data, requestId: meta.requestId };
        })
      : await mutation.submit(async () => {
          const { data, meta } = await api.createRoleBinding(organizationId, userId, role);
          return { data, requestId: meta.requestId };
        });
    if (!ok) return;
    reset();
    onSuccess();
  };

  const confirmDelete = async () => {
    if (!deleting) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.deleteRoleBinding(organizationId, deleting.id, deleting.version);
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setDeleting(null);
    onSuccess();
  };

  const openEdit = (binding: RoleBinding) => {
    if (binding.team_id) return;
    setEditing(binding);
    setUserId(binding.user_id);
    setRole(binding.role as AdminRole);
  };

  return (
    <>
      <div className="section-header">
        <h3>Role bindings</h3>
      </div>
      {bindings.loading && <p role="status">Loading role bindings…</p>}
      <InlineError problem={bindings.error} />
      {!bindings.loading && bindings.items.length === 0 && (
        <EmptyState
          title="No role bindings"
          text="Grant org-wide administrative roles to users. Team-scoped admin bindings are managed through team membership and shown here for reference only."
          action={<Button onClick={() => { reset(); setCreateOpen(true); }}>Grant role</Button>}
        />
      )}
      {bindings.items.length > 0 && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Role binding actions"><Button onClick={() => { reset(); setCreateOpen(true); }}>Grant role</Button></div>
      )}
      {bindings.items.length > 0 && (
        <DataGrid aria-label="Role bindings">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Person</TableHeader>
              <TableHeader>Scope</TableHeader>
              <TableHeader>Role</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {bindings.items.map(binding => (
              <TableRow className="data-row" key={binding.id}>
                <DataCell label="Person" className="cell-primary" title={binding.user_id}>
                  <Person user={binding} />
                </DataCell>
                <DataCell label="Scope">
                  {binding.team_id ? `Team ${uuidHint(binding.team_id)}` : 'Organization'}
                </DataCell>
                <DataCell label="Role">{roleLabel(binding.role)}</DataCell>
                <DataCell label="Status">
                  <Badge status={binding.enabled ? 'active' : 'disabled'} />
                </DataCell>
                <DataCell
                  label="Actions"
                  className="row-actions"
                  title={
                    binding.team_id
                      ? 'Team-scoped administrative bindings are managed through team membership.'
                      : undefined
                  }
                >
                  <Button
                    variant="ghost"
                    onClick={() => openEdit(binding)}
                    disabled={!!binding.team_id}
                  >
                    Edit
                  </Button>
                  <Button
                    variant="danger"
                    onClick={() => setDeleting(binding)}
                    disabled={!!binding.team_id}
                  >
                    Revoke
                  </Button>
                  {binding.team_id && (
                    <span className="muted">Managed by team membership</span>
                  )}
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}
      <LoadMore hasMore={bindings.hasMore} loading={bindings.loadingMore} onClick={bindings.loadMore} />

      <Dialog
        open={createOpen || !!editing}
        onClose={() => { if (!mutation.submitting) reset(); }}
        title={editing ? 'Edit role binding' : 'Grant role'}
      >
        <form
          onSubmit={event => {
            event.preventDefault();
            void submit();
          }}
        >
          {!editing && (
            <FormField label="User ID" htmlFor="binding-user-id" hint="A UUID for the user account.">
              <TextInput
                id="binding-user-id"
                value={userId}
                onChange={event => setUserId(event.target.value)}
                required
                pattern="[0-9a-fA-F-]{36}"
                autoFocus
              />
            </FormField>
          )}
          <FormField label="Admin role" htmlFor="binding-role">
            <Select
              id="binding-role"
              value={role}
              onChange={event => setRole(event.target.value as AdminRole)}
            >
              {orgGrantableAdminRoles.map(r => (
                <option key={r} value={r}>
                  {roleLabel(r)}
                </option>
              ))}
            </Select>
          </FormField>
          <p className="cds--form__helper-text">
            This grants an org-wide administrative role. Team-scoped admin bindings are managed through
            team membership and shown here for reference only.
          </p>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={reset} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              {editing ? 'Save' : 'Grant'}
            </Button>
          </div>
        </form>
      </Dialog>

      <ConfirmDialog
        open={!!deleting}
        onClose={() => setDeleting(null)}
        onConfirm={() => { void confirmDelete(); }}
        title="Revoke role binding"
        confirmText="Revoke"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Revoke this role binding? The user will immediately lose the associated administrative
        permissions.
      </ConfirmDialog>
    </>
  );
}

function EffectiveAccessTab({
  userId,
  context,
  orgMembers,
  teamMembers,
  teams,
  roleBindings,
}: {
  userId: string;
  context: Context;
  orgMembers: import('./api').Membership[];
  teamMembers: import('./api').Membership[];
  teams: Team[];
  roleBindings: import('./api').RoleBinding[];
}) {
  const orgMembership = useMemo(
    () => orgMembers.find(member => member.user_id === userId),
    [orgMembers, userId],
  );
  const userBindings = useMemo(
    () => roleBindings.filter(binding => binding.user_id === userId),
    [roleBindings, userId],
  );
  const userTeamMemberships = useMemo(
    () => teamMembers.filter(member => member.user_id === userId),
    [teamMembers, userId],
  );

  return (
    <div className="cards effective-access">
      <article className="cds--tile card">
        <div className="card-top"><span>Session context</span></div>
        <p>
          <strong>User:</strong> {orgMembership ? <Person user={orgMembership} /> : uuidHint(userId)}
          <br />
          <strong>Organization:</strong> {uuidHint(context.organization_id)}
          <br />
          <strong>Team:</strong> {context.team_id ? uuidHint(context.team_id) : 'None'}
          <br />
          <strong>Context role:</strong> {context.role}
        </p>
      </article>
      <article className="cds--tile card">
        <div className="card-top"><span>Organization membership</span></div>
        {orgMembership ? (
          <p>
            <strong>Role:</strong> {roleLabel(orgMembership.role)}
            <br />
            <strong>Status:</strong>{' '}
            <Badge status={orgMembership.enabled ? 'active' : 'disabled'} />
          </p>
        ) : (
          <p>No organization membership loaded for your user.</p>
        )}
      </article>
      <article className="cds--tile card">
        <div className="card-top"><span>Team memberships</span></div>
        {userTeamMemberships.length ? (
          <ul className="plain-list">
            {userTeamMemberships.map(member => {
              const team = teams.find(t => t.id === member.team_id);
              return (
                <li key={member.team_id ?? 'org'}>
                  {team?.name ?? uuidHint(member.team_id ?? '')} · {membershipRoleLabel(member.role)} ·{' '}
                  <Badge status={member.enabled ? 'active' : 'disabled'} />
                </li>
              );
            })}
          </ul>
        ) : (
          <p>No team memberships loaded for your user.</p>
        )}
      </article>
      <article className="cds--tile card">
        <div className="card-top"><span>Administrative role bindings</span></div>
        {userBindings.length ? (
          <ul className="plain-list">
            {userBindings.map(binding => (
              <li key={binding.id}>
                {roleLabel(binding.role)} · {binding.team_id ? `team ${uuidHint(binding.team_id)}` : 'organization'} ·{' '}
                <Badge status={binding.enabled ? 'active' : 'disabled'} />
              </li>
            ))}
          </ul>
        ) : (
          <p>No administrative role bindings loaded for your user.</p>
        )}
      </article>
    </div>
  );
}

/** Who a member is: their name and email, or, before they first sign in,
 * their user id. */
export function Person({ user }: { user: { user_id: string; display_name: string | null; email: string | null } }) {
  const name = user.display_name?.trim() || user.email;
  if (!name) return <span><span className="mono">{uuidHint(user.user_id)}</span><span className="person-email">Not signed in yet</span></span>;
  return (
    <span>
      {name}
      {user.email && user.email !== name && <span className="person-email">{user.email}</span>}
    </span>
  );
}
