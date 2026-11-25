import React, { Fragment, useEffect, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { movedPath } from './moved-path';
import { useOpenReconciliation } from './open-reconciliation';
import {
  api,
  beginSignIn,
  isContextDenied,
  normalizeApiError,
  onSessionEnded,
  type Me,
  type Organization,
} from './api';
import { logoutSession, OrganizationContextManager, type ContextSnapshot } from './context-state';
import { recoverSignIn, signInRecoveryMessage } from './sign-in-recovery';
import { ChangePasswordDialog, linkPage } from './local-accounts';
import { EmailSignIn } from './email-sign-in';
import { CreateOwnOrganizationDialog, OrganizationsScreen } from './organizations-screen';
import { localAccounts, type SignInOptions } from './local-accounts-api';
import { testedOrganization } from './sign-in-section';
import { OrganizationScreen, sectionTabs } from './organization-screen';
import { IdentitiesScreen, identityTabs } from './identities-screen';
import { AuditScreen } from './audit-screen';
import { TracesScreen } from './traces-screen';
import { UsageScreen, usageTabs } from './usage-screen';
import { ProvidersScreen, catalogTabs } from './providers-screen';
import { PoliciesScreen, policyTabs } from './policies-screen';
import { OverviewScreen } from './overview-screen';
import { PlaygroundScreen } from './playground-screen';
import { NotificationBell, NotificationsScreen } from './notifications';
import { ChevronDown } from '@carbon/icons-react';
import { EmptyState, Message, routeChanged, routeTab } from './ui';
import { SessionTimeoutNotice } from './session-timeout-notice';
import { forgetStoredKey } from './playground-key-store';
import type { TimeoutKind } from './session-timeout';
import { applyTheme, readPreference, resolveTheme, useThemePreference, type ThemePreference } from './theme';
import { AccountMenu, HeaderOrganization, HelpLink } from './header';
import './theme.scss';
import './styles.css';

type Page = 'overview' | 'models' | 'playground' | 'connections' | 'people' | 'identities' | 'policies' | 'usage' | 'traces' | 'audit' | 'settings' | 'organizations' | 'notifications';
type LoadState = 'loading' | 'ready' | 'unauthenticated' | 'permission' | 'error';
type PageCopy = { title: string; text: string };
type LogoutState = 'idle' | 'pending' | 'error';

// The side navigation holds every page, as in IBM Cloud: the product (what
// you call, how it is reached, and what happened), then administering the
// organization (its settings, rules, people and access).
const navigation: [Page, string, string][] = [
  ['overview', 'Overview', '⌂'], ['models', 'Models', '✦'], ['playground', 'Playground', '▷'], ['connections', 'Connections', '⇄'],
  ['usage', 'Usage', '▥'],
];
/** The pages that administer the organization, in the side navigation after
 * the product's; `group` starts a new group (a divider before it). */
const managed: { key: Page; label: string; icon: string; group?: boolean }[] = [
  { key: 'settings', label: 'Settings', icon: '⚙', group: true },
  { key: 'policies', label: 'Policies', icon: '⊙' },
  { key: 'organizations', label: 'Organizations', icon: '▦' },
  { key: 'people', label: 'People', icon: '◎', group: true },
  { key: 'identities', label: 'Identities', icon: '◇' },
  { key: 'audit', label: 'Audit', icon: '◌' },
];
/** A section in the side navigation: one of its page's (`/admin/<page>/<key>`)
 * or, with `page`, a page of its own listed in the group (Traces, under
 * Usage). */
type NavSection = { key: string; label: string; page?: Page };
const asSections = (tabs: readonly (readonly [string, string])[]): NavSection[] => tabs.map(([key, label]) => ({ key, label }));

/** A page's sections, which open as an accordion in the side navigation
 * (as in IBM Cloud) instead of tabs on the page. Each screen owns its list. */
const pageSections: Partial<Record<Page, NavSection[]>> = {
  models: asSections(catalogTabs.models),
  connections: asSections(catalogTabs.connections),
  // Traces keeps its own address (each request has one under it) but sits
  // under Usage: a slice charted, then the requests behind it.
  usage: [...asSections(usageTabs.slice(0, 1)), { key: 'traces', label: 'Traces', page: 'traces' }, ...asSections(usageTabs.slice(1))],
  policies: asSections(policyTabs),
  settings: asSections(sectionTabs.settings),
  people: asSections(sectionTabs.people),
  identities: asSections(identityTabs),
};

/** The group in the side navigation `page` is listed under. */
function groupOf(page: Page): Page {
  return page === 'traces' ? 'usage' : page;
}

/** The sections of `group` shown to `role`: deleting the organization is
 * its owner's. (A section whose read the role is refused falls back to the
 * page's first.) */
function sectionsOf(group: Page, role: string | null | undefined) {
  return (pageSections[group] ?? []).filter(({ key }) => key !== 'deletion' || role?.toLowerCase() === 'owner');
}

/** The section open on `page`: the page itself where it is a section of
 * another group, else the one the path names, or the page's first. */
function currentSection(page: Page, role: string | null | undefined): NavSection | null {
  const sections = sectionsOf(groupOf(page), role);
  if (groupOf(page) !== page) return sections.find(section => section.page === page) ?? null;
  const named = routeTab();
  return sections.find(section => !section.page && section.key === named) ?? sections.find(section => !section.page) ?? null;
}

const pageCopy: Record<Page, PageCopy> = {
  overview: { title: 'Overview', text: 'Spend, what needs a decision, the models you can call, and recent activity.' },
  models: { title: 'Models', text: 'The models you can call, where they can run, and what they cost.' },
  playground: { title: 'Playground', text: 'Try a model with an API key, and get the code that sends the same request.' },
  connections: { title: 'Connections', text: 'The cloud accounts the gateway calls models in, their credentials, and what each one offers.' },
  people: { title: 'People', text: 'Who belongs to this organization, its teams, and what each person can do.' },
  identities: { title: 'Identities', text: 'The apps (service accounts) that call the gateway, the end users they call for, and agents. A key’s secret is shown once.' },
  policies: { title: 'Policies', text: 'Budgets, limits, data retention, exports and webhooks.' },
  usage: { title: 'Usage', text: 'Requests, tokens and spend over time, sliced by key, account, team, model and more, with the requests behind them.' },
  traces: { title: 'Traces', text: 'Every request: its routing, timing, cost and outcome, and its content where tracing kept it.' },
  audit: { title: 'Audit', text: 'Who changed what, and when. Entries cannot be edited or removed.' },
  settings: { title: 'Settings', text: 'How people sign in to this organization, and which routing version it serves.' },
  organizations: { title: 'Organizations', text: 'Every organization on this gateway: create, suspend, delete and restore them.' },
  notifications: { title: 'Notifications', text: 'What the gateway did, or needs from you: failing price sources and model listings, and price changes.' },
};

/** Moves the address to where its page lives now, keeping the query (a
 * consent or sign-in test result) and fragment. */
function followMovedPath() {
  const moved = movedPath(location.pathname, location.search);
  if (moved) history.replaceState(history.state, '', `${moved}${location.search}${location.hash}`);
}

/** The page in `/admin/<page>[/<tab>]`. */
function currentRoute(): Page {
  followMovedPath();
  const candidate = location.pathname.split('/').filter(Boolean)[1] as Page;
  return candidate && candidate in pageCopy ? candidate : 'overview';
}

function App() {
  // Above every early return: on the loading, sign-in and error screens too,
  // System keeps following the OS.
  const [themePreference, setThemePreference] = useThemePreference();
  const [page, setPage] = useState<Page>(currentRoute);
  const [mobileNavOpen, setMobileNavOpen] = useState(false);
  const [organizations, setOrganizations] = useState<Organization[]>([]);
  const [me, setMe] = useState<Me | null>(null);
  const userId = me?.user_id ?? '';
  const contextManager = useRef(new OrganizationContextManager(api.context)).current;
  const [contextSnapshot, setContextSnapshot] = useState<ContextSnapshot>(contextManager.getSnapshot());
  const [loadState, setLoadState] = useState<LoadState>('loading');
  const [error, setError] = useState('');
  const [logoutState, setLogoutState] = useState<LogoutState>('idle');
  const [logoutError, setLogoutError] = useState('');
  const [signInPending, setSignInPending] = useState(false);
  const [signInError, setSignInError] = useState('');
  const [signInOptions, setSignInOptions] = useState<SignInOptions | null>(null);
  /** Why the console signed out on its own, for the sign-in page. */
  const [endReason, setEndReason] = useState<TimeoutKind | 'ended' | null>(null);
  const loadStateRef = useRef(loadState);
  loadStateRef.current = loadState;
  // Signed out, however it came about: the playground's kept key goes, so
  // whoever signs in next in this tab never gets it.
  useEffect(() => {
    if (loadState === 'unauthenticated') forgetStoredKey();
  }, [loadState]);

  // A request that finds the session gone goes to the sign-in page, rather
  // than leaving the screen to fail call by call.
  useEffect(() => onSessionEnded(() => {
    if (loadStateRef.current !== 'ready') return;
    setEndReason(current => current ?? 'ended');
    setLoadState('unauthenticated');
  }), []);

  // What the sign-in page offers; without an answer, the identity-provider
  // ways in.
  useEffect(() => {
    if (loadState !== 'unauthenticated' || signInOptions) return;
    localAccounts.signInOptions().then(setSignInOptions).catch(() => setSignInOptions({ password: false, oidc: true, setup_required: false }));
  }, [loadState, signInOptions]);

  useEffect(() => {
    const onPopState = () => setPage(currentRoute());
    addEventListener('popstate', onPopState);
    const unsubscribe = contextManager.subscribe(snapshot => {
      setContextSnapshot(snapshot);
      if (snapshot.state === 'idle' && snapshot.confirmedOrganization) rememberOrganization(snapshot.confirmedOrganization.id);
      if (snapshot.state === 'unauthenticated') setLoadState('unauthenticated');
    });
    loadSession(
      setOrganizations,
      setLoadState,
      setError,
      setMe,
      contextManager,
    );
    return () => {
      removeEventListener('popstate', onPopState);
      unsubscribe();
    };
  }, [contextManager]);

  const navigate = (next: Page, tab?: string) => {
    history.pushState({}, '', `/admin/${next}${tab ? `/${tab}` : ''}`);
    // The open screen reads its tab from the path too: going to the page
    // already open, or to another tab of it, has to reach it.
    dispatchEvent(new PopStateEvent('popstate'));
    setPage(next);
    setMobileNavOpen(false);
  };

  const selectOrganization = (organizationId: string) => void contextManager.select(organizationId);
  const retryOrganization = () => void contextManager.retry();

  const logout = async () => {
    setLogoutState('pending');
    setLogoutError('');
    const result = await logoutSession(api.logout);
    if (result.ok) {
      setLoadState('unauthenticated');
      return;
    }
    if (result.error.status === 401) {
      setLoadState('unauthenticated');
      return;
    }
    setLogoutState('error');
    setLogoutError(result.error.code);
  };

  /** Signs out when the idle warning runs out or the session ends, or when
   * asked; the sign-in page follows at once. */
  const endSession = async (kind: TimeoutKind, asked: boolean) => {
    if (!asked) {
      // Another tab may have signed in again since this one last looked: a
      // session other than the one this tab knows is not this tab's to end.
      const current = await withinSeconds(3, api.me()).catch(() => null);
      if (current && current.session_expires_at !== me?.session_expires_at) {
        // Someone else, or a session scoped otherwise: this tab's
        // organizations, permissions and open pages are not theirs, so load
        // the workspace afresh.
        if (current.user_id !== me?.user_id || current.organization_session !== me?.organization_session) {
          location.reload();
          return;
        }
        setMe(current);
        return;
      }
    }
    setEndReason(kind);
    setLoadState('unauthenticated');
    // Told in the background: a slow server never holds the console open,
    // and a session it does not hear of ends at its own expiry.
    void logoutSession(api.logout);
  };
  const staySignedIn = async () => {
    try {
      setMe(await api.me());
    } catch (caught) {
      // A session already gone is noticed by `onSessionEnded`.
      if (normalizeApiError(caught).status !== 401) throw caught;
    }
  };

  const retrySignIn = async (organization?: string) => {
    setSignInPending(true);
    setSignInError('');
    const result = await recoverSignIn(() => beginSignIn(undefined, undefined, organization));
    if (!result.ok) setSignInError(signInRecoveryMessage(result.error.code));
    setSignInPending(false);
  };

  if (loadState === 'loading') {
    return <State title="Loading your workspace" text="Checking your signed-in session and available organizations." />;
  }
  if (loadState === 'unauthenticated') {
    if (!signInOptions) return <State title="Loading your workspace" text="Checking how you can sign in." />;
    if (signInOptions.setup_required) {
      return <State
        title="Gateway is not set up yet"
        text="Open the setup link from the gateway’s log to create the first organization and its owner. Run `gateway setup-link` for a new one."
      />;
    }
    // Email first; the organization's short name stays as a fallback.
    return <State
      title="Sign in to Gateway"
      text={endReason === 'idle'
        ? 'You were signed out after 15 minutes without activity. Enter your email address to sign in again.'
        : endReason
          ? 'Your session ended. Enter your email address to sign in again.'
          : 'Enter your email address. If you were signed in before, your session has ended.'}
      error={signInError}
    >
      <EmailSignIn options={signInOptions} pending={signInPending} onSignInWith={organization => { void retrySignIn(organization); }} />
    </State>;
  }
  if (loadState === 'permission') {
    return <State
      title="Organization access restricted"
      text="Your session is valid, but the server did not grant access to this organization context. Ask an administrator to update your membership, then try again."
      action="Try again"
      onAction={() => location.reload()}
    />;
  }
  if (loadState === 'error') {
    return <State
      title="The workspace is unavailable"
      text={`We could not load your session. Try again; if it keeps happening, tell whoever runs this gateway (error code ${error}).`}
      action="Try again"
      onAction={() => location.reload()}
    />;
  }

  const activePage = pageCopy[page];
  return <>
  <SessionTimeoutNotice expiresAt={me?.session_expires_at ?? null} onStay={staySignedIn} onEnd={(kind, asked) => void endSession(kind, asked)} />
  <Shell
    page={page}
    pageCopy={activePage}
    mobileNavOpen={mobileNavOpen}
    setMobileNavOpen={setMobileNavOpen}
    navigate={navigate}
    organizations={organizations}
    contextSnapshot={contextSnapshot}
    userId={userId}
    me={me}
    error={error}
    onSelectOrganization={selectOrganization}
    onRetryOrganization={retryOrganization}
    logoutState={logoutState}
    logoutError={logoutError}
    onLogout={logout}
    onSignIn={() => { void retrySignIn(); }}
    signInError={signInError}
    signInPending={signInPending}
    themePreference={themePreference}
    onThemePreference={setThemePreference}
  />
  </>;
}

/** `promise`, or a rejection after `seconds`. */
function withinSeconds<T>(seconds: number, promise: Promise<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('timeout')), seconds * 1000);
    promise.then(
      value => { clearTimeout(timer); resolve(value); },
      error => { clearTimeout(timer); reject(error); },
    );
  });
}

async function loadSession(
  setOrganizations: (value: Organization[]) => void,
  setLoadState: (value: LoadState) => void,
  setError: (value: string) => void,
  setMe: (value: Me) => void,
  contextManager: OrganizationContextManager,
) {
  try {
    setMe(await api.me());
    const values = await api.organizations();
    setOrganizations(values);
    contextManager.setOrganizations(values);
    // Back from testing an organization's sign-in: that organization.
    const tested = testedOrganization(location.search);
    // A reload or a page opened in a new tab stays in the organization
    // last chosen in this browser.
    const remembered = rememberedOrganization();
    const first = values.find(value => value.id === tested) ?? values.find(value => value.id === remembered) ?? values[0] ?? null;
    if (first) void contextManager.select(first.id);
    setLoadState('ready');
  } catch (caught) {
    const failure = normalizeApiError(caught);
    setLoadState(failure.status === 401 ? 'unauthenticated' : isContextDenied(failure) ? 'permission' : 'error');
    setError(failure.code);
  }
}

type ShellProps = {
  page: Page;
  pageCopy: PageCopy;
  mobileNavOpen: boolean;
  setMobileNavOpen: (open: boolean) => void;
  navigate: (page: Page, tab?: string) => void;
  organizations: Organization[];
  contextSnapshot: ContextSnapshot;
  userId: string;
  me: Me | null;
  error: string;
  onSelectOrganization: (organizationId: string) => void;
  onRetryOrganization: () => void;
  logoutState: LogoutState;
  logoutError: string;
  onLogout: () => void;
  onSignIn: () => void;
  signInError: string;
  signInPending: boolean;
  themePreference: ThemePreference;
  onThemePreference: (next: ThemePreference) => void;
};
function Shell({
  page,
  pageCopy: content,
  mobileNavOpen,
  setMobileNavOpen,
  navigate,
  organizations,
  contextSnapshot,
  userId,
  me,
  error,
  onSelectOrganization,
  onRetryOrganization,
  logoutState,
  logoutError,
  onLogout,
  onSignIn,
  signInError,
  signInPending,
  themePreference,
  onThemePreference,
}: ShellProps) {
  const sidebarRef = useRef<HTMLElement>(null);
  const [changingPassword, setChangingPassword] = useState(false);
  const [creatingOwn, setCreatingOwn] = useState(false);
  // Organizations is the deployment's page, for its admins only.
  const shownManaged = managed.filter(item => item.key !== 'organizations' || me?.deployment_admin);
  const menuRef = useRef<HTMLButtonElement>(null);
  const organizationName = contextSnapshot.confirmedOrganization?.name;
  const role = contextSnapshot.confirmedContext?.role;
  // The open section, from the path: Back, Forward, the side navigation and
  // a screen opening another of its sections all change it.
  const [routeVersion, setRouteVersion] = useState(0);
  useEffect(() => {
    const changed = () => setRouteVersion(version => version + 1);
    addEventListener('popstate', changed);
    addEventListener(routeChanged, changed);
    return () => {
      removeEventListener('popstate', changed);
      removeEventListener(routeChanged, changed);
    };
  }, []);
  const section = currentSection(page, role);
  // Open reconciliation items, counted beside Reconciliation.
  const openItems = useOpenReconciliation(contextSnapshot.confirmedOrganization?.id ?? '', routeVersion);
  // The open page's sections are shown; others open and close by hand.
  const openGroup = groupOf(page);
  const [expanded, setExpanded] = useState<ReadonlySet<Page>>(() => new Set([openGroup]));
  useEffect(() => {
    setExpanded(current => (current.has(openGroup) ? current : new Set([...current, openGroup])));
  }, [openGroup]);
  const toggle = (key: Page) => setExpanded(current => {
    const next = new Set(current);
    if (next.has(key)) next.delete(key); else next.add(key);
    return next;
  });
  const parentTitle = pageCopy[openGroup].title;
  const title = section ? `${section.label} · ${parentTitle}` : content.title;
  useEffect(() => {
    document.title = [title, organizationName, 'Gateway'].filter(Boolean).join(' · ');
  }, [title, organizationName]);
  // The phone drawer: Escape closes it and returns focus to the menu button;
  // opening it moves focus to the current page's link.
  useEffect(() => {
    if (!mobileNavOpen) return;
    (sidebarRef.current?.querySelector<HTMLElement>('.nav-item.active') ?? sidebarRef.current?.querySelector<HTMLElement>('.nav-item'))?.focus();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setMobileNavOpen(false);
    };
    document.addEventListener('keydown', onKeyDown);
    const sidebar = sidebarRef.current;
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      // However it closed (Escape, the backdrop, a page link), focus must not
      // stay in the hidden drawer: it returns to the menu button.
      if (sidebar?.contains(document.activeElement)) menuRef.current?.focus();
    };
  }, [mobileNavOpen, setMobileNavOpen]);
  const confirming = !contextSnapshot.confirmedOrganization && contextSnapshot.state === 'pending';
  const pendingName = organizations.find(organization => organization.id === contextSnapshot.selectedOrganizationId)?.name;
  // Switching from one open organization to another: the page still shows
  // (and would change) the one being left, so it is inert until the switch
  // is confirmed.
  const switching = !!contextSnapshot.confirmedOrganization && contextSnapshot.state === 'pending'
    && contextSnapshot.selectedOrganizationId !== contextSnapshot.confirmedOrganization.id;
  return (
    <div className="app">
      <header className="shell-header">
        <button
          ref={menuRef}
          className="menu"
          aria-label={mobileNavOpen ? 'Close navigation' : 'Open navigation'}
          aria-expanded={mobileNavOpen}
          aria-controls="primary-sidebar"
          onClick={() => setMobileNavOpen(!mobileNavOpen)}
        >☰</button>
        <a
          className="shell-name"
          href="/admin/overview"
          onClick={event => {
            if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
            event.preventDefault();
            navigate('overview');
          }}
        ><span className="mark" aria-hidden="true">G</span>Gateway</a>
        <div className="header-actions">
          <HeaderOrganization
            organizations={organizations}
            current={contextSnapshot.confirmedOrganization}
            role={contextSnapshot.confirmedContext?.role ?? null}
            pending={contextSnapshot.state === 'pending'}
            onSelect={onSelectOrganization}
          />
          <HelpLink />
          <NotificationBell key={contextSnapshot.confirmedOrganization?.id ?? ''} organizationId={contextSnapshot.confirmedOrganization?.id ?? ''} />
          <AccountMenu
            me={me}
            organizations={organizations}
            current={contextSnapshot.confirmedOrganization}
            onSelectOrganization={onSelectOrganization}
            onSettings={() => navigate('settings')}
            onAgents={() => navigate('identities', 'agents')}
            onChangePassword={me?.sign_in_method === 'password' ? () => setChangingPassword(true) : undefined}
            onNewOrganization={me?.self_serve_organizations ? () => setCreatingOwn(true) : undefined}
            themePreference={themePreference}
            onThemePreference={onThemePreference}
            logoutPending={logoutState === 'pending'}
            onLogout={onLogout}
          />
        </div>
      </header>
      <div className="shell-body">
      {mobileNavOpen && <div className="nav-backdrop" aria-hidden="true" onClick={() => { setMobileNavOpen(false); menuRef.current?.focus(); }} />}
      <aside id="primary-sidebar" ref={sidebarRef} className={mobileNavOpen ? 'sidebar open' : 'sidebar'}>
        <nav aria-label="Primary navigation">
          {[...navigation.map(([key, label, icon]) => ({ key, label, icon, group: key === 'usage' })), ...shownManaged].map(({ key, label, icon, group }) => (
            <Fragment key={key}>
              {/* Groups, as in IBM Cloud's side navigation: building,
                  observing, administering, then security and access. */}
              {group && <hr className="nav-divider" />}
              {sectionsOf(key, role).length > 1 ? (
                <>
                  <button
                    type="button"
                    className={openGroup === key ? 'nav-item nav-parent current' : 'nav-item nav-parent'}
                    aria-expanded={expanded.has(key)}
                    aria-controls={`nav-${key}`}
                    onClick={() => toggle(key)}
                  >
                    <i aria-hidden="true">{icon}</i>
                    <span className="nav-label">{label}</span>
                    <ChevronDown size={16} aria-hidden="true" className="nav-chevron" />
                  </button>
                  <ul id={`nav-${key}`} className="nav-children" hidden={!expanded.has(key)}>
                    {sectionsOf(key, role).map(item => {
                      const active = openGroup === key && section?.key === item.key;
                      return (
                        <li key={item.key}>
                          <a
                            href={item.page ? `/admin/${item.page}` : `/admin/${key}/${item.key}`}
                            className={active ? 'nav-item nav-child active' : 'nav-item nav-child'}
                            aria-current={active ? 'page' : undefined}
                            onClick={event => {
                              // A modified click opens the page in a new tab or window.
                              if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
                              event.preventDefault();
                              if (item.page) navigate(item.page); else navigate(key, item.key);
                            }}
                          >
                            {item.label}
                            {key === 'usage' && item.key === 'reconciliation' && openItems && (
                              <span className="nav-count" title={`${openItems} open`}>
                                {openItems}<span className="cds--visually-hidden"> open</span>
                              </span>
                            )}
                          </a>
                        </li>
                      );
                    })}
                  </ul>
                </>
              ) : (
                <a
                  href={`/admin/${key}`}
                  className={page === key ? 'nav-item active' : 'nav-item'}
                  aria-current={page === key ? 'page' : undefined}
                  onClick={event => {
                    // A modified click opens the page in a new tab or window.
                    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
                    event.preventDefault();
                    navigate(key);
                  }}
                >
                  <i aria-hidden="true">{icon}</i>
                  {label}
                </a>
              )}
            </Fragment>
          ))}
        </nav>
      </aside>
      <main>
        <ChangePasswordDialog open={changingPassword} onClose={() => setChangingPassword(false)} email={me?.email ?? null} />
        <CreateOwnOrganizationDialog
          open={creatingOwn}
          organizationSession={!!me?.organization_session}
          onClose={() => setCreatingOwn(false)}
          onCreated={() => { setCreatingOwn(false); location.reload(); }}
        />
        <section className="content">
          <div className="hero">
            {/* A section's page names its parent above it, as a breadcrumb. */}
            {section && <p className="page-parent">{parentTitle}</p>}
            <h1>{section ? section.label : content.title}</h1>
            <p className="lede">{content.text}</p>
          </div>
          {page === 'organizations' ? (
            me?.deployment_admin
              ? <OrganizationsScreen />
              : <EmptyState title="Not available" text="Only the gateway’s deployment admins manage its organizations." />
          ) : !organizations.length ? (
            <Empty />
          ) : confirming ? (
            <div className="loading-panel" role="status" aria-live="polite">
              <span className="spinner" aria-hidden="true" />
              Loading {pendingName ?? 'your organization'}…
            </div>
          ) : (
           <div className={switching ? 'page-switching' : undefined} inert={switching} aria-busy={switching || undefined}>
           <PageContent
              page={page}
              organizationId={contextSnapshot.confirmedOrganization?.id ?? ''}
              userId={userId}
                context={contextSnapshot.confirmedContext}
                onNavigate={navigate}
                onSignIn={onSignIn}
                signInError={signInError}
                signInPending={signInPending}
            />
           </div>
          )}
          <ContextFeedback
            selectionState={contextSnapshot.state}
            selectionError={contextSnapshot.error?.code ?? ''}
            onRetry={onRetryOrganization}
            logoutError={logoutError}
          />
          {error === 'context_unavailable' && <Message kind="error">Your organization could not be loaded. Reload the page to try again.</Message>}
        </section>
      </main>
      </div>
    </div>
  );
}
function State({ title, text, action, actionDisabled, error, onAction, children }: { title: string; text: string; action?: string; actionDisabled?: boolean; error?: string; onAction?: () => void; children?: React.ReactNode }) {
  return (
    <main className="state-page">
      <div className="cds--tile state-card">
        <span className="mark large">G</span>
        <p className="eyebrow">Gateway</p>
        <h1>{title}</h1>
        <p>{text}</p>
        {error && <Message kind="error">{error}</Message>}
        {action && <button className="primary" disabled={actionDisabled} aria-busy={actionDisabled} onClick={onAction}>{action}</button>}
        {children}
      </div>
    </main>
  );
}
function Empty() {
  return (
    <div className="empty">
      <span className="empty-icon">◇</span>
      <h2>No organization access</h2>
      <p>Your account is signed in, but it has no active organization membership. Ask an administrator to grant access.</p>
    </div>
  );
}
function ContextFeedback({
  selectionState,
  selectionError,
  onRetry,
  logoutError,
}: {
  selectionState: ContextSnapshot['state'];
  selectionError: string;
  onRetry: () => void;
  logoutError: string;
}) {
  if (logoutError) {
    return <Message kind="error">Sign out could not be completed ({logoutError}). Your current session remains active.</Message>;
  }
  if (selectionState !== 'error' && selectionState !== 'permission') return null;
  const message = selectionError === 'organization_or_team_context_denied'
    ? 'The server denied this organization context. Ask an administrator to update your membership.'
    : `This organization could not be opened; you are still in the one before (error code ${selectionError}).`;
  return <Message kind="error">{message} <button className="inline-action" onClick={onRetry}>Retry</button></Message>;
}
const organizationKey = 'gateway-admin.organization';
/** The organization last confirmed in this browser; storage may be unavailable. */
function rememberedOrganization(): string | null {
  try { return localStorage.getItem(organizationKey); } catch { return null; }
}
function rememberOrganization(organizationId: string) {
  try { localStorage.setItem(organizationKey, organizationId); } catch { /* storage unavailable: the first organization opens */ }
}

function PageContent({
  page,
  organizationId,
  userId,
  context,
  onNavigate,
  onSignIn,
  signInError,
  signInPending,
}: {
  page: Page;
  organizationId: string;
  userId: string;
  context: ContextSnapshot['confirmedContext'];
  onNavigate: (page: Page, tab?: string) => void;
  onSignIn: () => void;
  signInError: string;
  signInPending: boolean;
}) {
  if (page === 'people' || page === 'settings') {
    if (!organizationId || !context) {
      return (
        <EmptyState
          title="Waiting for organization context"
          text={page === 'people' ? 'Select a confirmed organization to manage its members, teams and access.' : 'Select a confirmed organization to change its settings.'}
        />
      );
    }
    return <OrganizationScreen key={`${organizationId}-${page}`} section={page} organizationId={organizationId} userId={userId} context={context} />;
  }
  if (page === 'identities') {
    if (!organizationId || !context) {
      return (
        <EmptyState
          title="Waiting for organization context"
          text="Select a confirmed organization to manage service accounts, agents and API keys."
        />
      );
    }
    return <IdentitiesScreen key={organizationId} organizationId={organizationId} userId={userId} context={context} />;
  }
  if (page === 'overview') return <OverviewScreen key={organizationId} organizationId={organizationId} onNavigate={onNavigate} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
  if (page === 'policies') return <PoliciesScreen key={organizationId} organizationId={organizationId} role={context?.role} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
  if (page === 'models' || page === 'connections') return <ProvidersScreen key={`${organizationId}-${page}`} section={page} organizationId={organizationId} role={context?.role} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
  if (page === 'playground') return <PlaygroundScreen key={organizationId} organizationId={organizationId} userId={userId} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
  if (page === 'notifications') return <NotificationsScreen key={organizationId} organizationId={organizationId} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
  if (page === 'usage') return <UsageScreen key={organizationId} organizationId={organizationId} role={context?.role} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
  if (page === 'traces') return <TracesScreen key={organizationId} organizationId={organizationId} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
  return <AuditScreen key={organizationId} organizationId={organizationId} onSignIn={onSignIn} signInError={signInError} signInPending={signInPending} />;
}

// Before the first paint, so a dark theme never flashes light.
applyTheme(resolveTheme(readPreference()));
// A setup, invitation or reset link opens its own page, without a session.
createRoot(document.getElementById('root')!).render(<React.StrictMode>{linkPage(location.pathname) ?? <App />}</React.StrictMode>);
