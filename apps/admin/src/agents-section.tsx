import React, { useState } from 'react';
import {
  api,
  newIdempotencyKey,
  type Agent,
  type AgentRole,
  type AgentToken,
} from './api';
import { useCollection, type CollectionState } from './use-collection';
import type { MutationState } from './mutation-state';
import { invitationRoleLabels } from './invitations-section';
import {
  Badge,
  Button,
  ConfirmDialog,
  DataCell,
  Dialog,
  EmptyState,
  FormField,
  InlineError,
  SecretRevealPanel,
  Select,
  TextInput,
  formatDate,
  formatRelativeDate,
} from './ui';
import { DataGrid, TableBody, TableHead, TableHeader, TableRow, Message } from './ui';

/** How long a new token works, in days; the API allows 1 to 365, or none. */
export const agentTokenValidities: [number | null, string][] = [
  [30, '30 days'],
  [90, '90 days'],
  [365, '1 year'],
  [null, 'Never expires'],
];
const DEFAULT_TOKEN_DAYS = 90;

type NewToken = { token: string; agentName: string; expiresAt: string | null; requestId?: string };

/** The MCP endpoint of the gateway serving this page. */
export function mcpUrl(origin: string = window.location.origin) {
  return `${origin}/api/mcp`;
}

/**
 * The organization's agents: automations that administer it with a token
 * instead of a sign-in. Shown only to people who may read them;
 * `grantableRoles` (from the list response) holds the roles the caller may
 * give an agent, so the write actions are hidden when it is empty.
 */
export function AgentsSection({
  organizationId,
  agents,
  grantableRoles,
  mutation,
  onSuccess,
}: {
  organizationId: string;
  agents: CollectionState<Agent>;
  grantableRoles: AgentRole[];
  mutation: MutationState<unknown>;
  onSuccess: () => void;
}) {
  const canWrite = grantableRoles.length > 0;
  const [selectedId, setSelectedId] = useState('');
  const selected = agents.items.find(agent => agent.id === selectedId) ?? null;

  const [createOpen, setCreateOpen] = useState(false);
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [role, setRole] = useState<AgentRole>('admin');
  // One key per request: an unchanged retry after a lost response replays
  // the same write instead of making a second one; a changed field makes it
  // a new request, with a new key.
  const [createKey, setCreateKey] = useState('');

  const [tokenFor, setTokenFor] = useState<Agent | null>(null);
  const [tokenName, setTokenName] = useState('');
  const [tokenDays, setTokenDays] = useState<number | null>(DEFAULT_TOKEN_DAYS);
  // '' for the whole organization.
  const [tokenAccount, setTokenAccount] = useState('');
  // For limiting a token to one app's end users; a role that cannot list
  // service accounts gets organization-wide tokens only.
  const accounts = useCollection(
    React.useCallback(params => api.listServiceAccounts(organizationId, params), [organizationId]),
    [organizationId],
  );
  const accountName = (id: string) => accounts.items.find(account => account.id === id)?.name ?? id;
  const [tokenKey, setTokenKey] = useState('');
  const [newToken, setNewToken] = useState<NewToken | null>(null);
  const [replayed, setReplayed] = useState(false);

  const [disabling, setDisabling] = useState<Agent | null>(null);
  const [revoking, setRevoking] = useState<AgentToken | null>(null);
  // One key per confirmation, for the same reason.
  const [confirmKey, setConfirmKey] = useState('');

  const openCreate = () => {
    mutation.clearProblem();
    setName('');
    setDescription('');
    setRole(grantableRoles.includes('admin') ? 'admin' : grantableRoles[grantableRoles.length - 1]);
    setCreateKey(newIdempotencyKey());
    setCreateOpen(true);
  };

  const openToken = (agent: Agent) => {
    mutation.clearProblem();
    setTokenName(agent.tokens.length === 0 ? 'default' : '');
    setTokenDays(DEFAULT_TOKEN_DAYS);
    setTokenAccount('');
    setTokenKey(newIdempotencyKey());
    setTokenFor(agent);
  };

  const submitCreate = async () => {
    const created: { agent?: Agent } = {};
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.createAgent(
        organizationId,
        { name: name.trim(), role, description: description.trim() || null },
        createKey,
      );
      created.agent = data;
      return { data: { id: data.id }, requestId: meta.requestId };
    });
    if (!ok || !created.agent) return;
    setCreateOpen(false);
    setSelectedId(created.agent.id);
    onSuccess();
    // An agent does nothing without a token: offer the first one at once.
    openToken(created.agent);
  };

  const submitToken = async () => {
    if (!tokenFor) return;
    const agent = tokenFor;
    let minted: NewToken | null = null;
    let wasReplay = false;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.createAgentToken(organizationId, agent.id, tokenName.trim(), tokenDays, tokenKey, tokenAccount || null);
      if (data.token) {
        minted = { token: data.token, agentName: agent.name, expiresAt: data.expires_at, requestId: meta.requestId };
      } else {
        wasReplay = true;
      }
      // The token stays out of the mutation result, which the page may show.
      return { data: { id: data.id }, requestId: meta.requestId };
    });
    if (!ok) return;
    setTokenFor(null);
    setNewToken(minted);
    setReplayed(wasReplay);
    onSuccess();
  };

  const confirmDisable = async () => {
    if (!disabling) return;
    const target = disabling;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.disableAgent(organizationId, target.id, confirmKey);
      return { data: { id: data.id }, requestId: meta.requestId };
    });
    if (!ok) return;
    setDisabling(null);
    onSuccess();
  };

  const confirmRevoke = async () => {
    if (!revoking || !selected) return;
    const target = revoking;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.revokeAgentToken(organizationId, selected.id, target.id, confirmKey);
      return { data: { id: data.id }, requestId: meta.requestId };
    });
    if (!ok) return;
    setRevoking(null);
    onSuccess();
  };

  const activeTokens = (agent: Agent) => agent.tokens.filter(token => token.status === 'active').length;

  return (
    <>
      <div className="section-header">
        <h3>Agents</h3>
      </div>
      <p className="cds--form__helper-text">
        An agent is an automation, such as a CI job or a coding agent, that administers this
        organization with a token instead of a sign-in: through the admin API, the <code>gwctl</code>{' '}
        CLI or MCP. It acts in the role you give it, and everything it does is audited under its name.
      </p>

      {newToken && (
        <SecretRevealPanel
          secret={newToken.token}
          label={`Agent token · ${newToken.agentName}`}
          requestId={newToken.requestId}
          onDismiss={() => setNewToken(null)}
          valueLabel="Agent token"
          copyLabel="Copy agent token"
          dismissText="Done"
          warning={
            <>
              Give this token to the agent: as <code>GATEWAY_AGENT_TOKEN</code> for <code>gwctl</code>,
              or as the bearer token of an MCP client connected to <code>{mcpUrl()}</code>.{' '}
              {newToken.expiresAt ? `It works until ${formatDate(newToken.expiresAt)}. ` : 'It does not expire. '}
              This is the only time the token is shown: after you close this panel it cannot be shown
              again, but you can revoke it and create another.
            </>
          }
        />
      )}
      {replayed && (
        <Message kind="info">
          This token was already created, so it cannot be shown again. Revoke it and create another
          if you no longer have it.
        </Message>
      )}

      {agents.loading && <p role="status">Loading agents…</p>}
      <InlineError problem={agents.error} />
      {agents.error && (
        <Button variant="ghost" onClick={() => { void agents.load(true); }}>
          Try again
        </Button>
      )}
      {!agents.loading && !agents.error && agents.items.length === 0 && (
        <EmptyState
          title="No agents"
          text={canWrite ? 'Create an agent to let an automation administer this organization.' : 'No agents have been created.'}
          action={canWrite ? <Button onClick={openCreate}>Create agent</Button> : undefined}
        />
      )}
      {agents.items.length > 0 && canWrite && (
        <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Agent actions"><Button onClick={openCreate}>Create agent</Button></div>
      )}
      {agents.items.length > 0 && (
        <DataGrid aria-label="Agents">
          <TableHead>
            <TableRow className="data-row head">
              <TableHeader>Name</TableHeader>
              <TableHeader>Role</TableHeader>
              <TableHeader>Status</TableHeader>
              <TableHeader>Active tokens</TableHeader>
              <TableHeader>Created</TableHeader>
              <TableHeader className="row-actions">Actions</TableHeader>
            </TableRow>
          </TableHead>
          <TableBody>
            {agents.items.map(agent => (
              <TableRow className="data-row" aria-selected={agent.id === selectedId} key={agent.id}>
                <DataCell label="Name" className="cell-primary">
                  {agent.name}
                  {agent.description && <span className="account-meta"> · {agent.description}</span>}
                </DataCell>
                <DataCell label="Role">{invitationRoleLabels[agent.role] ?? agent.role}</DataCell>
                <DataCell label="Status">
                  <Badge status={agent.enabled ? 'active' : 'disabled'} />
                </DataCell>
                <DataCell label="Active tokens">{activeTokens(agent)}</DataCell>
                <DataCell label="Created">{formatDate(agent.created_at)}</DataCell>
                <DataCell label="Actions" className="row-actions">
                  <Button
                    variant="ghost"
                    aria-pressed={agent.id === selectedId}
                    onClick={() => setSelectedId(agent.id === selectedId ? '' : agent.id)}
                  >
                    Tokens
                  </Button>
                  {canWrite && agent.enabled && (
                    <Button
                      variant="danger"
                      onClick={() => { mutation.clearProblem(); setConfirmKey(newIdempotencyKey()); setDisabling(agent); }}
                    >
                      Disable
                    </Button>
                  )}
                </DataCell>
              </TableRow>
            ))}
          </TableBody>
        </DataGrid>
      )}

      {selected && (
        <>
          <div className="section-header">
            <h4>Tokens · {selected.name}</h4>
          </div>
          {selected.tokens.length === 0 ? (
            <EmptyState
              title="No tokens"
              text={selected.enabled ? 'Create a token for the agent to authenticate with.' : 'This agent is disabled.'}
              action={canWrite && selected.enabled ? <Button onClick={() => openToken(selected)}>Create token</Button> : undefined}
            />
          ) : (
            <>
            {canWrite && selected.enabled && (
              <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Token actions"><Button onClick={() => openToken(selected)}>Create token</Button></div>
            )}
            <DataGrid aria-label={`Tokens of ${selected.name}`}>
              <TableHead>
                <TableRow className="data-row head">
                  <TableHeader>Name</TableHeader>
                  <TableHeader>Prefix</TableHeader>
                  <TableHeader>Status</TableHeader>
                  <TableHeader>Reaches</TableHeader>
                  <TableHeader>Expires</TableHeader>
                  <TableHeader>Last used</TableHeader>
                  <TableHeader>Created</TableHeader>
                  <TableHeader className="row-actions">Actions</TableHeader>
                </TableRow>
              </TableHead>
              <TableBody>
                {selected.tokens.map(token => (
                  <TableRow className="data-row" key={token.id}>
                    <DataCell label="Name" className="cell-primary">{token.name}</DataCell>
                    <DataCell label="Prefix"><code>{token.public_prefix}</code></DataCell>
                    <DataCell label="Status"><Badge status={token.status} /></DataCell>
                    <DataCell label="Reaches">{token.service_account_id ? `End users of ${accountName(token.service_account_id)}` : 'Organization'}</DataCell>
                    <DataCell label="Expires">{token.expires_at ? formatDate(token.expires_at) : 'Never'}</DataCell>
                    <DataCell label="Last used">{formatRelativeDate(token.last_used_at)}</DataCell>
                    <DataCell label="Created">{formatDate(token.created_at)}</DataCell>
                    <DataCell label="Actions" className="row-actions">
                      {canWrite && token.status === 'active' && (
                        <Button
                          variant="danger"
                          onClick={() => { mutation.clearProblem(); setConfirmKey(newIdempotencyKey()); setRevoking(token); }}
                        >
                          Revoke
                        </Button>
                      )}
                    </DataCell>
                  </TableRow>
                ))}
              </TableBody>
            </DataGrid>
            </>
          )}
        </>
      )}

      <Dialog open={createOpen} onClose={() => { if (!mutation.submitting) setCreateOpen(false); }} title="Create agent">
        <form
          onSubmit={event => {
            event.preventDefault();
            void submitCreate();
          }}
        >
          <FormField label="Name" htmlFor="agent-name" hint="Unique in the organization, such as ci-deploy or claude-code.">
            <TextInput
              id="agent-name"
              value={name}
              onChange={event => { setName(event.target.value); setCreateKey(newIdempotencyKey()); }}
              maxLength={100}
              required
              autoFocus
            />
          </FormField>
          <FormField label="Description (optional)" htmlFor="agent-description">
            <TextInput
              id="agent-description"
              value={description}
              onChange={event => { setDescription(event.target.value); setCreateKey(newIdempotencyKey()); }}
              maxLength={500}
            />
          </FormField>
          <FormField label="Role" htmlFor="agent-role" hint="You can give an agent your own role or a lower one.">
            <Select
              id="agent-role"
              value={role}
              onChange={event => { setRole(event.target.value as AgentRole); setCreateKey(newIdempotencyKey()); }}
            >
              {grantableRoles.map(option => (
                <option key={option} value={option}>
                  {invitationRoleLabels[option]}
                </option>
              ))}
            </Select>
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setCreateOpen(false)} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              Create agent
            </Button>
          </div>
        </form>
      </Dialog>

      <Dialog
        open={!!tokenFor}
        onClose={() => { if (!mutation.submitting) setTokenFor(null); }}
        title={`Create token · ${tokenFor?.name ?? ''}`}
      >
        <form
          onSubmit={event => {
            event.preventDefault();
            void submitToken();
          }}
        >
          <FormField label="Token name" htmlFor="agent-token-name" hint="Where it is used, such as laptop or github-actions.">
            <TextInput
              id="agent-token-name"
              value={tokenName}
              onChange={event => { setTokenName(event.target.value); setTokenKey(newIdempotencyKey()); }}
              maxLength={100}
              required
              autoFocus
            />
          </FormField>
          <FormField label="Expires after" htmlFor="agent-token-expiry">
            <Select
              id="agent-token-expiry"
              value={tokenDays === null ? 'never' : String(tokenDays)}
              onChange={event => {
                setTokenDays(event.target.value === 'never' ? null : Number(event.target.value));
                setTokenKey(newIdempotencyKey());
              }}
            >
              {agentTokenValidities.map(([days, label]) => (
                <option key={label} value={days === null ? 'never' : String(days)}>
                  {label}
                </option>
              ))}
            </Select>
          </FormField>
          {accounts.items.length > 0 && (
            <FormField label="Reaches" htmlFor="agent-token-scope" hint="An app’s backend needs only its own end users: limit its token to them.">
              <Select
                id="agent-token-scope"
                value={tokenAccount}
                onChange={event => { setTokenAccount(event.target.value); setTokenKey(newIdempotencyKey()); }}
              >
                <option value="">The whole organization, as the agent’s role allows</option>
                {accounts.items.map(account => (
                  <option key={account.id} value={account.id}>End users of {account.name} only</option>
                ))}
              </Select>
            </FormField>
          )}
          <p className="cds--form__helper-text">The token is shown only once.</p>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setTokenFor(null)} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              Create token
            </Button>
          </div>
        </form>
      </Dialog>

      <ConfirmDialog
        open={!!disabling}
        onClose={() => setDisabling(null)}
        onConfirm={() => { void confirmDisable(); }}
        title="Disable agent"
        confirmText="Disable"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Disable “{disabling?.name}”? It loses its role, and every one of its tokens stops working at
        once. A disabled agent cannot be enabled again; it stays listed for the audit trail.
      </ConfirmDialog>

      <ConfirmDialog
        open={!!revoking}
        onClose={() => setRevoking(null)}
        onConfirm={() => { void confirmRevoke(); }}
        title="Revoke token"
        confirmText="Revoke"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Revoke the token “{revoking?.name}” ({revoking?.public_prefix}…)? Requests carrying it are
        refused at once.
      </ConfirmDialog>
    </>
  );
}
