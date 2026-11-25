import React, { useEffect, useMemo, useRef, useState } from 'react';
import { ComboBox, Toggle } from '@carbon/react';
import { allPages, api, type ApiKeyScope, type ApiProblem, type BudgetPolicy, type CatalogDeployment, type RateLimitPolicy, type Context, type ServiceAccount, type ApiKey } from './api';
import { uncoveredCurrencies } from './budget-coverage';
import { formatCount } from './money';
import { BudgetUse } from './policies-screen';
import { everyPage, useCollection } from './use-collection';
import { DataTable, type Column } from './data-table';
import { useMutationState, useSecretReveal } from './mutation-state';
import {
  Button,
  ConfirmDialog,
  Dialog,
  FormField,
  TextInput,
  Badge,
  MutationStatus,
  EmptyState,
  LoadMore,
  InlineError,
  SecretRevealPanel,
  AppLink,
  DataCell,
  formatDate,
  formatRelativeDate,
  uuidHint,
  Tabs,
  CheckOption,
  TabPanel,
  useRouteTab,
  Message,
} from './ui';
import { OrganizationScreen } from './organization-screen';
import { EndUsersSection } from './end-users-section';

const apiKeyScopes: ApiKeyScope[] = ['Inference', 'Embeddings', 'CatalogRead', 'AdminRead'];


type AccountSection = 'keys' | 'end-users';
const accountSections: [AccountSection, string][] = [['keys', 'API keys'], ['end-users', 'End users']];

type IdentityTab = 'service-accounts' | 'end-users' | 'agents';
export const identityTabs: [IdentityTab, string][] = [['service-accounts', 'Service accounts'], ['end-users', 'End users'], ['agents', 'Agents']];
const identityTabKeys = identityTabs.map(([key]) => key);

/** Whatever calls the gateway: service accounts with API keys, and agents. */
export function IdentitiesScreen({ organizationId, userId, context }: { organizationId: string; userId: string; context: Context }) {
  const [tab] = useRouteTab(identityTabKeys, 'service-accounts');
  return (
    <div className="screen">
      {/* The kind is chosen in the side navigation. */}
      <div className="page-section">
        {tab === 'service-accounts'
          ? <ServiceAccountsPanel organizationId={organizationId} />
          : tab === 'end-users'
          ? <EndUsersByApp organizationId={organizationId} />
          : <OrganizationScreen section="agents" organizationId={organizationId} userId={userId} context={context} />}
      </div>
    </div>
  );
}

/** End users, an app (service account) at a time: the app is picked from a
 * list that narrows as you type, and kept in the address (`?app=<id>`). */
function EndUsersByApp({ organizationId }: { organizationId: string }) {
  const [accounts, setAccounts] = useState<ServiceAccount[] | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [appId, setAppId] = useState(() => new URLSearchParams(location.search).get('app') ?? '');
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setAccounts(null);
    setProblem(null);
    allPages<ServiceAccount, Awaited<ReturnType<typeof api.listServiceAccounts>>>(token => api.listServiceAccounts(organizationId, { page_size: 100, page_token: token }, { enabled: true })).then(
      ({ items }) => { if (current) setAccounts(items); },
      caught => {
        if (!current) return;
        // A request that never reached the gateway has no problem of its own.
        setProblem((caught as { problem?: ApiProblem }).problem ?? { type: '', code: 'load_failed', title: 'Not loaded', status: 0, detail: 'The apps could not be loaded.', instance: '', request_id: '' });
      },
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  const choose = (id: string) => {
    const params = new URLSearchParams(location.search);
    if (id) params.set('app', id); else params.delete('app');
    const query = params.toString();
    history.replaceState(history.state, '', `${location.pathname}${query ? `?${query}` : ''}`);
    setAppId(id);
  };
  // With a single app there is nothing to pick.
  const selected = accounts?.find(account => account.id === appId) ?? (accounts?.length === 1 ? accounts[0] : undefined);
  if (problem) {
    return (
      <>
        <InlineError problem={problem} />
        <Button variant="ghost" onClick={() => setAttempt(count => count + 1)}>Try again</Button>
      </>
    );
  }
  if (!accounts) return <p role="status">Loading apps…</p>;
  if (accounts.length === 0) {
    return <EmptyState title="No apps" text="End users belong to an app: a service account whose requests name the user they are for (Gateway-End-User)." />;
  }
  return (
    <>
      <div className="end-users-app">
        <ComboBox<ServiceAccount>
          id="end-users-app"
          titleText="App"
          helperText="The service account the end users call through."
          placeholder="Type to find an app"
          items={accounts}
          itemToString={account => account?.name ?? ''}
          selectedItem={selected ?? null}
          shouldFilterItem={({ item, inputValue }) => !inputValue || item.name.toLowerCase().includes(inputValue.toLowerCase())}
          onChange={({ selectedItem }) => choose(selectedItem?.id ?? '')}
        />
      </div>
      {selected
        ? <EndUsersSection key={selected.id} organizationId={organizationId} account={selected} />
        : <p className="meta">Pick an app to see its end users, their plans and what they spent this period.</p>}
    </>
  );
}

/** The open service account, in the address (`?account=<id>`), so a reload,
 * Back and a shared link reopen it. */
function useOpenAccount(): [string, (id: string) => void] {
  const read = () => new URLSearchParams(location.search).get('account') ?? '';
  const [id, setId] = useState(read);
  useEffect(() => {
    const onPopState = () => setId(read());
    addEventListener('popstate', onPopState);
    return () => removeEventListener('popstate', onPopState);
  }, []);
  const open = React.useCallback((next: string) => {
    const params = new URLSearchParams(location.search);
    if (next) params.set('account', next); else params.delete('account');
    const query = params.toString();
    history.pushState(history.state, '', `${location.pathname}${query ? `?${query}` : ''}`);
    setId(next);
  }, []);
  return [id, open];
}

/** Service accounts as a table (disabled ones hidden unless asked for), each
 * opening its own page with its keys and end users. */
function ServiceAccountsPanel({ organizationId }: { organizationId: string }) {
  const [openId, openAccount] = useOpenAccount();
  const [showDisabled, setShowDisabled] = useState(false);
  const accounts = useCollection(
    React.useCallback(
      params => api.listServiceAccounts(organizationId, { page_size: 100, ...params }, showDisabled ? {} : { enabled: true }),
      [organizationId, showDisabled],
    ),
    [organizationId, showDisabled],
  );
  // The account a delete disabled instead, kept here: its page reloads.
  const [disabledInsteadId, setDisabledInsteadId] = useState('');
  const secret = useSecretReveal();
  // The account whose key is shown: the budget advice is about it.
  const [revealedFor, setRevealedFor] = useState<ServiceAccount | null>(null);
  const reveal = (value: string, label: string, requestId: string | undefined, account: ServiceAccount) => {
    setRevealedFor(account);
    secret.reveal(value, label, requestId);
  };
  const mutation = useMutationState<unknown>();
  const previousOrganizationId = useRef(organizationId);
  useEffect(() => {
    if (previousOrganizationId.current === organizationId) return;
    previousOrganizationId.current = organizationId;
    setDisabledInsteadId('');
    secret.dismiss();
    openAccount('');
  }, [organizationId, secret.dismiss, openAccount]);

  return (
    <div className="screen identities-screen">
      <MutationStatus result={mutation.result} problem={mutation.problem} onDismiss={mutation.clearProblem} />
      {secret.secret && (
        <SecretRevealPanel
          secret={secret.secret.secret}
          label={secret.secret.label}
          requestId={secret.secret.requestId}
          onDismiss={secret.dismiss}
        />
      )}
      {secret.secret && revealedFor && <BudgetWarning key={revealedFor.id} organizationId={organizationId} account={revealedFor} />}
      {openId ? (
        <ServiceAccountPage
          key={`${organizationId}-${openId}`}
          organizationId={organizationId}
          accountId={openId}
          mutation={mutation}
          onBack={() => { mutation.clearResult(); openAccount(''); }}
          onChanged={() => accounts.load(true)}
          onReveal={reveal}
          disabledInstead={disabledInsteadId === openId}
          onDisabledInstead={setDisabledInsteadId}
        />
      ) : (
        <ServiceAccountList
          accounts={accounts}
          showDisabled={showDisabled}
          onShowDisabled={setShowDisabled}
          onOpen={openAccount}
          organizationId={organizationId}
          mutation={mutation}
        />
      )}
    </div>
  );
}

const accountColumns = (onOpen: (id: string) => void): Column<ServiceAccount>[] => [
  {
    id: 'name',
    label: 'Name',
    primary: true,
    value: account => account.name,
    render: account => (
      <a
        href={`?account=${encodeURIComponent(account.id)}`}
        onClick={event => {
          if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
          event.preventDefault();
          onOpen(account.id);
        }}
      >
        {account.name}
      </a>
    ),
  },
  { id: 'status', label: 'Status', filter: 'category', value: account => (account.enabled ? 'active' : 'disabled'), render: account => <Badge status={account.enabled ? 'active' : 'disabled'} /> },
  { id: 'keys', label: 'Active keys', filter: 'number', value: account => account.active_keys ?? null },
  {
    id: 'last-used',
    label: 'Last used',
    value: account => account.last_used_at ?? null,
    render: account => (account.last_used_at ? formatRelativeDate(account.last_used_at) : <span className="meta">Never</span>),
    title: account => (account.last_used_at ? formatDate(account.last_used_at) : undefined),
  },
  { id: 'created', label: 'Created', value: account => account.created_at, render: account => formatDate(account.created_at) },
];

function ServiceAccountList({
  accounts,
  showDisabled,
  onShowDisabled,
  onOpen,
  organizationId,
  mutation,
}: {
  accounts: ReturnType<typeof useCollection<ServiceAccount>>;
  showDisabled: boolean;
  onShowDisabled: (show: boolean) => void;
  onOpen: (id: string) => void;
  organizationId: string;
  mutation: ReturnType<typeof useMutationState<unknown>>;
}) {
  const [createOpen, setCreateOpen] = useState(false);
  const [name, setName] = useState('');
  const columns = useMemo(() => accountColumns(onOpen), [onOpen]);

  const reset = () => {
    setName('');
    setCreateOpen(false);
  };

  const submit = async () => {
    let created = '';
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.createServiceAccount(organizationId, name);
      created = data.id;
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    reset();
    accounts.load(true);
    // A new account's next step is a key: its page opens.
    if (created) onOpen(created);
  };

  const create = <Button onClick={() => { setName(''); setCreateOpen(true); }}>Create service account</Button>;
  const toggle = (
    <Toggle
      id="show-disabled-accounts"
      size="sm"
      labelText="Show disabled"
      labelA="Off"
      labelB="On"
      toggled={showDisabled}
      onToggle={checked => onShowDisabled(checked)}
    />
  );

  return (
    <>
      {accounts.loading && <p role="status">Loading service accounts…</p>}
      <InlineError problem={accounts.error} />
      {!accounts.loading && !accounts.error && accounts.items.length === 0 ? (
        <>
          <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="Service accounts">{toggle}</div>
          <EmptyState
            title={showDisabled ? 'No service accounts' : 'No active service accounts'}
            text="A service account is an application or job that calls the gateway with API keys."
            action={create}
          />
        </>
      ) : accounts.items.length > 0 && (
        <DataTable
          label="Service accounts"
          urlKey="accounts"
          toolbarActions={<>{toggle}{create}</>}
          columns={columns}
          rows={accounts.items}
          rowKey={account => account.id}
        />
      )}
      <LoadMore hasMore={accounts.hasMore} loading={accounts.loadingMore} onClick={accounts.loadMore} />

      <Dialog open={createOpen} onClose={() => { if (!mutation.submitting) reset(); }} title="Create service account">
        <form
          onSubmit={event => {
            event.preventDefault();
            void submit();
          }}
        >
          <FormField label="Account name" htmlFor="account-name" hint="Name it after the application or job that will use it.">
            <TextInput
              id="account-name"
              value={name}
              onChange={event => setName(event.target.value)}
              required
              autoFocus
            />
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={reset} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              Create
            </Button>
          </div>
        </form>
      </Dialog>
    </>
  );
}

/** One service account's page, read on its own so a link to a disabled one
 * opens too. */
function ServiceAccountPage({
  organizationId,
  accountId,
  mutation,
  onBack,
  onChanged,
  onReveal,
  disabledInstead,
  onDisabledInstead,
}: {
  organizationId: string;
  accountId: string;
  mutation: ReturnType<typeof useMutationState<unknown>>;
  onBack: () => void;
  onChanged: () => void;
  onReveal: (secret: string, label: string, requestId: string | undefined, account: ServiceAccount) => void;
  disabledInstead: boolean;
  onDisabledInstead: (accountId: string) => void;
}) {
  const [account, setAccount] = useState<ServiceAccount | null>(null);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setProblem(null);
    api.getServiceAccount(organizationId, accountId).then(
      value => { if (current) setAccount(value); },
      caught => {
        if (!current) return;
        const failure = (caught as { problem?: ApiProblem }).problem ?? null;
        // Deleted: back to the list.
        if (failure?.status === 404 && account) { onBack(); return; }
        setProblem(failure ?? { type: '', code: 'load_failed', title: 'Not loaded', status: 0, detail: 'The service account could not be loaded.', instance: '', request_id: '' });
      },
    );
    return () => { current = false; };
    // `account` and `onBack` are read when a reload fails, not reasons to reload.
  }, [organizationId, accountId, attempt]);
  const back = <p className="back-link"><a href="?" onClick={event => { event.preventDefault(); onBack(); }}>← Service accounts</a></p>;
  if (problem) {
    return (
      <>
        {back}
        <InlineError problem={problem} />
        <Button variant="ghost" onClick={() => setAttempt(count => count + 1)}>Try again</Button>
      </>
    );
  }
  if (!account) return <>{back}<p role="status">Loading the service account…</p></>;
  return (
    <>
      {back}
      <ServiceAccountDetail
        account={account}
        organizationId={organizationId}
        mutation={mutation}
        onSuccess={() => { setAttempt(count => count + 1); onChanged(); }}
        onReveal={onReveal}
        disabledInstead={disabledInstead}
        onDisabledInstead={onDisabledInstead}
      />
    </>
  );
}

function ServiceAccountDetail({
  account,
  organizationId,
  mutation,
  onSuccess,
  onReveal,
  disabledInstead,
  onDisabledInstead,
}: {
  account: ServiceAccount;
  organizationId: string;
  mutation: ReturnType<typeof useMutationState<unknown>>;
  onSuccess: () => void;
  onReveal: (secret: string, label: string, requestId: string | undefined, account: ServiceAccount) => void;
  disabledInstead: boolean;
  onDisabledInstead: (accountId: string) => void;
}) {
  // Revoked and expired keys stay listed, hidden unless asked for.
  const [showRetired, setShowRetired] = useState(false);
  const keys = useCollection(
    React.useCallback(
      // An account's keys are few: the table filters them in the browser.
      () => everyPage(params => api.listApiKeys(organizationId, account.id, params, { usable: !showRetired })),
      [organizationId, account.id, showRetired],
    ),
    [organizationId, account.id, showRetired],
  );
  const retiredToggle = (
    <Toggle
      id="show-retired-keys"
      size="sm"
      labelText="Show revoked and expired"
      labelA="Off"
      labelB="On"
      toggled={showRetired}
      onToggle={checked => setShowRetired(checked)}
    />
  );

  const [editOpen, setEditOpen] = useState(false);
  const [name, setName] = useState(account.name);
  const [deactivateOpen, setDeactivateOpen] = useState(false);
  const [deleteOpen, setDeleteOpen] = useState(false);
  const [createKeyOpen, setCreateKeyOpen] = useState(false);
  const createKeyButton = <Button onClick={() => { setSelectedScopes(['Inference']); setExpiry(''); setCreateKeyOpen(true); }}>Create key</Button>;
  const [selectedScopes, setSelectedScopes] = useState<ApiKeyScope[]>(['Inference']);
  const [expiry, setExpiry] = useState('');
  const [rotatingKey, setRotatingKey] = useState<ApiKey | null>(null);
  const [revokingKey, setRevokingKey] = useState<ApiKey | null>(null);
  const [section, setSection] = useState<AccountSection>('keys');
  const keyBudgets = useKeyBudgets(organizationId);
  const keyLimits = useKeyRateLimits(organizationId);
  const keyColumns = useMemo(() => keyColumnsFor(keyBudgets, keyLimits), [keyBudgets, keyLimits]);

  useEffect(() => {
    setName(account.name);
  }, [account.name]);

  const reset = () => {
    setEditOpen(false);
    setDeactivateOpen(false);
    setDeleteOpen(false);
    setCreateKeyOpen(false);
    setRotatingKey(null);
    setRevokingKey(null);
    setSelectedScopes(['Inference']);
    setExpiry('');
  };

  const saveName = async () => {
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.updateServiceAccount(
        organizationId,
        account.id,
        name,
        account.version,
      );
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    reset();
    onSuccess();
  };

  const deactivate = async () => {
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.deactivateServiceAccount(
        organizationId,
        account.id,
        account.version,
      );
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    reset();
    onSuccess();
  };

  const deleteAccount = async () => {
    let kept = false;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.deleteServiceAccount(
        organizationId,
        account.id,
        account.version,
      );
      kept = !!data && data.deleted === false;
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    reset();
    onDisabledInstead(kept ? account.id : '');
    onSuccess();
  };

  const createKey = async () => {
    const ok = await mutation.submit(async () => {
      const expiresAt = expiry ? new Date(expiry).toISOString() : null;
      const { data, meta } = await api.createApiKey(organizationId, account.id, {
        scopes: selectedScopes,
        expires_at: expiresAt,
      });
      if (data.secret) {
        onReveal(data.secret, `New API key for ${account.name}`, meta.requestId, account);
      }
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    reset();
    keys.load(true);
    onSuccess();
  };

  const rotateKey = async () => {
    if (!rotatingKey) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.rotateApiKey(organizationId, account.id, rotatingKey.id);
      if (data.secret) {
        onReveal(data.secret, `Successor API key for ${account.name}`, meta.requestId, account);
      }
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setRotatingKey(null);
    keys.load(true);
    onSuccess();
  };

  const revokeKey = async () => {
    if (!revokingKey) return;
    const ok = await mutation.submit(async () => {
      const { data, meta } = await api.revokeApiKey(
        organizationId,
        account.id,
        revokingKey.id,
        revokingKey.version,
      );
      return { data, requestId: meta.requestId };
    });
    if (!ok) return;
    setRevokingKey(null);
    keys.load(true);
    onSuccess();
  };

  const toggleScope = (scope: ApiKeyScope) => {
    setSelectedScopes(prev =>
      prev.includes(scope) ? prev.filter(s => s !== scope) : [...prev, scope],
    );
  };

  return (
    <div className="cds--tile detail-panel">
      <div className="detail-header">
        <div>
          <h3>{account.name}</h3>
          <p className="meta">
            <Badge status={account.enabled ? 'active' : 'disabled'} /> · created{' '}
            {formatDate(account.created_at)}
          </p>
        </div>
        <div className="row-actions">
          <Button variant="ghost" onClick={() => setEditOpen(true)}>
            Rename
          </Button>
          {account.enabled && (
            <Button variant="ghost" onClick={() => setDeactivateOpen(true)}>
              Disable
            </Button>
          )}
          <Button variant="danger" onClick={() => setDeleteOpen(true)}>
            Delete
          </Button>
        </div>
      </div>
      {disabledInstead && (
        <Message kind="success">
          “{account.name}” has made requests, so it was disabled instead of deleted: its usage
          history is kept.
        </Message>
      )}

      <Tabs id="account-sections" label="Service account sections" tabs={accountSections} value={section} onChange={setSection} />
      {section === 'end-users' ? (
        <TabPanel id="account-sections" value={section}>
          <EndUsersSection key={account.id} organizationId={organizationId} account={account} />
        </TabPanel>
      ) : (
      <TabPanel id="account-sections" value={section}>
      <div className="section-header">
        <h4>API keys</h4>
      </div>
      {keys.loading && <p role="status">Loading API keys…</p>}
      <InlineError problem={keys.error} />
      {!keys.loading && !keys.error && keys.items.length === 0 && (
        <>
          <div className="table-toolbar table-toolbar-end" role="toolbar" aria-label="API keys">{retiredToggle}</div>
          <EmptyState
            title={showRetired ? 'No API keys' : 'No usable API keys'}
            text="Create a key to authenticate as this service account."
            action={account.enabled ? createKeyButton : undefined}
          />
        </>
      )}
      {keys.items.length > 0 && (
        <DataTable
          label="API keys"
          urlKey="keys"
          toolbarActions={<>{retiredToggle}{account.enabled && createKeyButton}</>}
          columns={keyColumns}
          rows={keys.items}
          rowKey={key => key.id}
          detail={key => <KeyDetail apiKey={key} budgets={keyBudgets} limits={keyLimits} />}
          actions={key => (
            <>
              <Button variant="ghost" onClick={() => setRotatingKey(key)}>
                Rotate
              </Button>
              <Button variant="danger" onClick={() => setRevokingKey(key)}>
                Revoke
              </Button>
            </>
          )}
        />
      )}
      </TabPanel>
      )}

      <Dialog open={editOpen} onClose={() => { if (!mutation.submitting) setEditOpen(false); }} title="Rename service account">
        <form
          onSubmit={event => {
            event.preventDefault();
            void saveName();
          }}
        >
          <FormField label="Account name" htmlFor="edit-account-name">
            <TextInput
              id="edit-account-name"
              value={name}
              onChange={event => setName(event.target.value)}
              required
              autoFocus
            />
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={() => setEditOpen(false)} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              Save
            </Button>
          </div>
        </form>
      </Dialog>

      <ConfirmDialog
        open={deactivateOpen}
        onClose={() => setDeactivateOpen(false)}
        onConfirm={() => { void deactivate(); }}
        title="Disable service account"
        confirmText="Disable"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Disable “{account.name}”? This prevents the account from authenticating until it is
        re-enabled by an administrator.
      </ConfirmDialog>

      <ConfirmDialog
        open={deleteOpen}
        onClose={() => setDeleteOpen(false)}
        onConfirm={() => { void deleteAccount(); }}
        title="Delete service account"
        confirmText="Delete"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Delete “{account.name}”? You must revoke every API key first. This cannot be undone. An
        account that has made requests is disabled instead, so its usage history is kept.
      </ConfirmDialog>

      <Dialog open={createKeyOpen} onClose={() => { if (!mutation.submitting) reset(); }} title="Create API key">
        <form
          onSubmit={event => {
            event.preventDefault();
            void createKey();
          }}
        >
          <FormField label="Scopes" htmlFor="key-scopes">
            <div id="key-scopes" className="checkbox-group">
              {apiKeyScopes.map(scope => (
                <CheckOption key={scope}
                   
                    checked={selectedScopes.includes(scope)}
                    onChange={() => toggleScope(scope)}>{scope}</CheckOption>
              ))}
            </div>
          </FormField>
          <FormField label="Expiry (optional)" htmlFor="key-expiry" hint="Leave blank for no expiry.">
            <TextInput
              id="key-expiry"
              type="datetime-local"
              value={expiry}
              onChange={event => setExpiry(event.target.value)}
            />
          </FormField>
          <InlineError problem={mutation.problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={reset} disabled={mutation.submitting}>
              Cancel
            </Button>
            <Button type="submit" loading={mutation.submitting}>
              Create key
            </Button>
          </div>
        </form>
      </Dialog>

      <ConfirmDialog
        open={!!rotatingKey}
        onClose={() => setRotatingKey(null)}
        onConfirm={() => { void rotateKey(); }}
        title="Rotate API key"
        confirmText="Rotate"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Rotate the key ending in {uuidHint(rotatingKey?.public_prefix ?? '')}? A new active successor
        key will be created and shown once. The previous key stays active; deploy the successor,
        then revoke the previous key.
      </ConfirmDialog>

      <ConfirmDialog
        open={!!revokingKey}
        onClose={() => setRevokingKey(null)}
        onConfirm={() => { void revokeKey(); }}
        title="Revoke API key"
        confirmText="Revoke"
        problem={mutation.problem}
        submitting={mutation.submitting}
      >
        Revoke the key ending in {uuidHint(revokingKey?.public_prefix ?? '')}? Applications using it
        will stop working immediately.
      </ConfirmDialog>
    </div>
  );
}

/** Each key's own budget, by key id; null when budgets cannot be read (a
 * role without access to them). */
function useKeyBudgets(organizationId: string): Map<string, BudgetPolicy> | null {
  const [budgets, setBudgets] = useState<Map<string, BudgetPolicy> | null>(null);
  useEffect(() => {
    let current = true;
    allPages<BudgetPolicy, Awaited<ReturnType<typeof api.listBudgets>>>(token => api.listBudgets(organizationId, { page_size: 100, page_token: token }))
      .then(
        page => { if (current) setBudgets(new Map(page.items.filter(budget => budget.scope === 'api-key' && budget.api_key_id).map(budget => [budget.api_key_id as string, budget]))); },
        () => { if (current) setBudgets(null); },
      );
    return () => { current = false; };
  }, [organizationId]);
  return budgets;
}

/** Each key's own rate-limit policy, by key id; null when the policies
 * cannot be read. */
function useKeyRateLimits(organizationId: string): Map<string, RateLimitPolicy> | null {
  const [limits, setLimits] = useState<Map<string, RateLimitPolicy> | null>(null);
  useEffect(() => {
    let current = true;
    allPages<RateLimitPolicy, Awaited<ReturnType<typeof api.listRateLimits>>>(token => api.listRateLimits(organizationId, { page_size: 100, page_token: token }))
      .then(
        page => { if (current) setLimits(new Map(page.items.filter(policy => policy.scope === 'api-key' && policy.api_key_id).map(policy => [policy.api_key_id as string, policy]))); },
        () => { if (current) setLimits(null); },
      );
    return () => { current = false; };
  }, [organizationId]);
  return limits;
}

/** A key's own rate limits, or that only its service account's, team's and
 * organization's apply. */
export function keyRateLimitLabel(policy: RateLimitPolicy | undefined): string {
  if (!policy) return 'Service account’s limits';
  const parts = [
    policy.requests_per_minute !== null ? `${formatCount(policy.requests_per_minute)} req/min` : null,
    policy.tokens_per_minute !== null ? `${formatCount(policy.tokens_per_minute)} tok/min` : null,
    policy.max_concurrent !== null ? `${formatCount(policy.max_concurrent)} in flight` : null,
  ].filter(Boolean);
  return parts.join(' · ');
}

function KeyRateLimit({ policy }: { policy: RateLimitPolicy | undefined }) {
  return policy ? <span>{keyRateLimitLabel(policy)}</span> : <span className="muted">{keyRateLimitLabel(policy)}</span>;
}

/** A key's own budget and what it has used, or how its requests are limited without one. */
function KeyBudget({ budget }: { budget: BudgetPolicy | undefined }) {
  if (!budget) return <span className="muted">Service account’s budgets</span>;
  return <BudgetUse budget={budget} />;
}

function keyColumnsFor(budgets: Map<string, BudgetPolicy> | null, limits: Map<string, RateLimitPolicy> | null = null): Column<ApiKey>[] {
  const columns = [...keyColumns];
  if (limits) {
    columns.splice(3, 0, {
      id: 'rate_limit',
      label: 'Own rate limit',
      filter: 'number',
      // Its requests per minute, for sorting and filtering.
      value: key => limits.get(key.id)?.requests_per_minute ?? null,
      render: key => <KeyRateLimit policy={limits.get(key.id)} />,
    });
  }
  if (budgets) {
    columns.splice(3, 0, {
      id: 'budget',
      label: 'Own budget',
      filter: 'number',
      // The share of its limit used, as the budget table shows it.
      value: key => {
        const budget = budgets.get(key.id);
        if (!budget?.current) return null;
        const used = budget.current.consumed_minor + budget.current.reserved_minor;
        return budget.limit_minor > 0 ? Math.round(Math.min(1, used / budget.limit_minor) * 100) : 100;
      },
      render: key => <KeyBudget budget={budgets.get(key.id)} />,
    });
  }
  return columns;
}

const keyColumns: Column<ApiKey>[] = [
  { id: 'prefix', label: 'Prefix', primary: true, value: key => key.public_prefix },
  { id: 'scopes', label: 'Scopes', filter: 'category', value: key => key.scopes },
  { id: 'status', label: 'Status', filter: 'category', value: key => key.status, render: key => <Badge status={key.status} /> },
  { id: 'expires', label: 'Expires', filter: 'date', value: key => key.expires_at, title: key => key.expires_at ?? undefined, render: key => formatDate(key.expires_at) },
  { id: 'last_used', label: 'Last used', filter: 'date', value: key => key.last_used_at, title: key => key.last_used_at ?? undefined, render: key => formatRelativeDate(key.last_used_at) },
  { id: 'created', label: 'Created', filter: 'date', hidden: true, value: key => key.created_at, render: key => formatDate(key.created_at) },
  { id: 'version', label: 'Version', hidden: true, value: key => key.version, render: key => `v${key.version}` },
];

/** A key's identifiers and lifecycle, in full. */
function KeyDetail({ apiKey, budgets, limits }: { apiKey: ApiKey; budgets: Map<string, BudgetPolicy> | null; limits: Map<string, RateLimitPolicy> | null }) {
  const when = (value: string | null) => (value ? <span title={value}>{new Date(value).toLocaleString()}</span> : <span className="muted">—</span>);
  return (
    <dl className="detail-list">
      <dt>Key ID</dt><dd><code>{apiKey.id}</code></dd>
      <dt>Replaces</dt><dd>{apiKey.successor_of ? <code>{apiKey.successor_of}</code> : <span className="muted">—</span>}</dd>
      <dt>Scopes</dt><dd>{apiKey.scopes.join(', ')}</dd>
      <dt>Created</dt><dd>{when(apiKey.created_at)}</dd>
      <dt>Expires</dt><dd>{apiKey.expires_at ? when(apiKey.expires_at) : 'Never'}</dd>
      <dt>Last used</dt><dd>{when(apiKey.last_used_at)}</dd>
      <dt>Disabled</dt><dd>{when(apiKey.disabled_at)}</dd>
      <dt>Revoked</dt><dd>{when(apiKey.revoked_at)}</dd>
      {budgets && (
        <>
          <dt>Own budget</dt>
          <dd>
            <KeyBudget budget={budgets.get(apiKey.id)} />{' '}
            <AppLink to="/admin/policies/budgets">{budgets.has(apiKey.id) ? 'Change' : 'Set one'}</AppLink>
          </dd>
        </>
      )}
      {limits && (
        <>
          <dt>Own rate limit</dt>
          <dd>
            <KeyRateLimit policy={limits.get(apiKey.id)} />{' '}
            <AppLink to="/admin/policies/limits">{limits.has(apiKey.id) ? 'Change' : 'Set one'}</AppLink>
          </dd>
        </>
      )}
    </dl>
  );
}

/** After a key is created: a warning when no budget covers its account in
 * the currency its models are priced in, since those requests would be
 * refused. Says nothing when the budgets cannot be read (a role without
 * access to them); without the catalog's prices it checks only that some
 * budget applies. */
function BudgetWarning({ organizationId, account }: { organizationId: string; account: ServiceAccount }) {
  const [missing, setMissing] = useState<string[] | null>(null);
  useEffect(() => {
    let current = true;
    const budgets = allPages<BudgetPolicy, Awaited<ReturnType<typeof api.listBudgets>>>(token => api.listBudgets(organizationId, { page_size: 100, page_token: token }));
    const currencies = allPages<CatalogDeployment, Awaited<ReturnType<typeof api.listDeployments>>>(token => api.listDeployments(organizationId, { page_size: 100, page_token: token }))
      .then(page => page.items.flatMap(deployment => deployment.operations.flatMap(operation => (operation.price ? [operation.price.currency] : []))), () => []);
    Promise.all([budgets, currencies])
      .then(([page, priced]) => { if (current) setMissing(uncoveredCurrencies(page.items, account, priced)); }, () => undefined);
    return () => { current = false; };
  }, [organizationId, account]);
  if (!missing?.length) return null;
  const named = missing.filter(Boolean);
  return (
    <Message kind="warning">
      {named.length === 0
        ? <>No budget covers <strong>{account.name}</strong>, so every request with this key is refused (<code>quota_exceeded</code>).</>
        : <>No budget in {named.join(' or ')} covers <strong>{account.name}</strong>, so requests to models priced in {named.length > 1 ? 'those currencies' : 'it'} are refused (<code>quota_exceeded</code>). A budget only covers requests in its own currency.</>}{' '}
      <AppLink to="/admin/policies/budgets">Add a budget</AppLink>
    </Message>
  );
}
