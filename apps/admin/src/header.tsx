import React, { useEffect, useId, useRef, useState } from 'react';
import { RadioTile, TileGroup } from '@carbon/react';
import { ArrowRight, ChevronDown, Help, Moon, Screen, Sun } from '@carbon/icons-react';
import type { Me, Organization } from './api';
import { accountInitials, accountLabel } from './account';
import type { ThemePreference } from './theme';
import { Dialog } from './ui';

/** Where Help goes: the deployment's documentation when the build names it,
 * else the project's published site. */
const deploymentDocs = (import.meta.env.VITE_GATEWAY_DOCS_URL as string | undefined)?.trim().replace(/\/+$/, '') || null;
const publishedDocs = 'https://gateway.qucelerate.com';
export const docsUrl: string = deploymentDocs ?? publishedDocs;

/** A link into the documentation: a page of the deployment's documentation
 * site when the build names one (`VITE_GATEWAY_DOCS_URL`), else of the
 * project's published site. */
export function docsLink(site: string, base: string | null = deploymentDocs): string {
  return `${base ?? publishedDocs}/${site}`;
}

/** Closes a popover on Escape (returning focus to its button) and on a press
 * outside it. */
function useDismiss(open: boolean, close: () => void, wrapper: React.RefObject<HTMLElement | null>, button: React.RefObject<HTMLButtonElement | null>) {
  const closeRef = useRef(close);
  closeRef.current = close;
  useEffect(() => {
    if (!open) return;
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      closeRef.current();
      button.current?.focus();
    };
    const onPointer = (event: MouseEvent) => {
      if (wrapper.current && !wrapper.current.contains(event.target as Node)) closeRef.current();
    };
    document.addEventListener('keydown', onKey);
    document.addEventListener('mousedown', onPointer);
    return () => {
      document.removeEventListener('keydown', onKey);
      document.removeEventListener('mousedown', onPointer);
    };
  }, [open, wrapper, button]);
}

/** Whether an element is shown: neither it nor an ancestor is hidden by CSS
 * (the organization select is, above the narrow breakpoint). */
function shown(element: HTMLElement): boolean {
  for (let node: HTMLElement | null = element; node; node = node.parentElement) {
    const style = getComputedStyle(node);
    if (style.display === 'none' || style.visibility === 'hidden') return false;
  }
  return true;
}

/** A menu's items that can take focus now. */
function menuItems(menu: HTMLElement | null | undefined): HTMLElement[] {
  return Array.from(menu?.querySelectorAll<HTMLElement>('[data-menu-item]:not([disabled])') ?? []).filter(shown);
}

/** Up and Down (and Home and End) move between a menu's items. A select
 * keeps its own keys, which change its value. */
function onMenuKeys(event: React.KeyboardEvent<HTMLElement>) {
  const keys = ['ArrowDown', 'ArrowUp', 'Home', 'End'];
  if (!keys.includes(event.key) || (event.target as HTMLElement).tagName === 'SELECT') return;
  const items = menuItems(event.currentTarget);
  if (!items.length) return;
  event.preventDefault();
  const at = items.indexOf(document.activeElement as HTMLElement);
  const next = event.key === 'Home' ? 0
    : event.key === 'End' ? items.length - 1
      : event.key === 'ArrowDown' ? (at + 1) % items.length
        : (at - 1 + items.length) % items.length;
  items[next].focus();
}

/** The open organization, beside Help, as a dropdown like IBM Cloud's
 * account switcher: it lists the organizations to switch to and says the
 * person's role in the open one. Creating one is in the account menu, and
 * the deployment's organizations are a page in the side navigation. */
export function HeaderOrganization({
  organizations,
  current,
  role = null,
  pending = false,
  onSelect,
}: {
  organizations: Organization[];
  current: Organization | null;
  /** The person's role in `current`. */
  role?: string | null;
  /** Another organization is being opened. */
  pending?: boolean;
  onSelect: (organizationId: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const wrapper = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const menuId = useId();
  useDismiss(open, () => setOpen(false), wrapper, button);
  useEffect(() => {
    if (!open) return;
    const menu = wrapper.current?.querySelector<HTMLElement>('[role="menu"]');
    (menu?.querySelector<HTMLElement>('[aria-checked="true"]') ?? menu?.querySelector<HTMLElement>('[data-menu-item]'))?.focus();
  }, [open]);
  if (!current && !organizations.length) return null;
  const name = pending ? 'Opening…' : current?.name ?? 'Select organization';
  const choose = (action: () => void) => () => {
    setOpen(false);
    button.current?.focus();
    action();
  };
  return (
    <div className="header-organization-switch" ref={wrapper}>
      <button
        ref={button}
        type="button"
        className="header-organization"
        title={current?.name}
        aria-label={current ? `Organization: ${current.name}. Switch organization` : 'Select organization'}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-controls={open ? menuId : undefined}
        aria-busy={pending}
        onClick={() => setOpen(!open)}
      >
        <span className="header-organization-name">{name}</span>
        <ChevronDown size={16} aria-hidden="true" className="header-organization-chevron" />
      </button>
      {open && (
        <div id={menuId} className="header-popover header-organization-menu cds--layer-two" role="menu" aria-label="Organizations" onKeyDown={onMenuKeys}>
          {organizations.map(organization => (
            <button
              key={organization.id}
              type="button"
              role="menuitemradio"
              data-menu-item
              aria-checked={organization.id === current?.id}
              className="header-menu-item"
              onClick={choose(() => { if (organization.id !== current?.id) onSelect(organization.id); })}
            >
              {organization.name}
            </button>
          ))}
          {current && role && <p className="header-menu-note" role="presentation">Your role in {current.name}: {role}</p>}
        </div>
      )}
    </div>
  );
}

/** Help: the documentation, in a new tab. */
export function HelpLink() {
  return (
    <a className="header-icon" href={docsUrl} target="_blank" rel="noreferrer" aria-label="Help: documentation (opens in a new tab)" title="Help">
      <Help size={20} aria-hidden="true" />
    </a>
  );
}

/** The person's photo, or their initials if they have none or it fails to load. */
export function Avatar({ me, large = false }: { me: Me | null; large?: boolean }) {
  const [failed, setFailed] = useState(false);
  const className = large ? 'avatar avatar-large' : 'avatar';
  if (me?.photo_url && !failed) {
    return <img className={className} src={me.photo_url} alt="" aria-hidden="true" onError={() => setFailed(true)} />;
  }
  return <span className={className} aria-hidden="true">{accountInitials(me)}</span>;
}

export type AccountMenuProps = {
  me: Me | null;
  organizations: Organization[];
  current: Organization | null;
  onSelectOrganization: (organizationId: string) => void;
  onSettings: () => void;
  onAgents: () => void;
  onChangePassword?: () => void;
  onNewOrganization?: () => void;
  themePreference: ThemePreference;
  onThemePreference: (next: ThemePreference) => void;
  logoutPending: boolean;
  onLogout: () => void;
};

/** The avatar button and its panel: who is signed in, their settings, the
 * CLI, the theme, and signing out. */
export function AccountMenu({
  me,
  organizations,
  current,
  onSelectOrganization,
  onSettings,
  onAgents,
  onChangePassword,
  onNewOrganization,
  themePreference,
  onThemePreference,
  logoutPending,
  onLogout,
}: AccountMenuProps) {
  const [open, setOpen] = useState(false);
  const [dialog, setDialog] = useState<'theme' | 'cli' | null>(null);
  const wrapper = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const panelId = useId();
  useDismiss(open, () => setOpen(false), wrapper, button);
  useEffect(() => {
    if (open) menuItems(wrapper.current?.querySelector<HTMLElement>('.account-panel'))[0]?.focus();
  }, [open]);
  const label = accountLabel(me);
  // The item pressed goes with the panel, so focus moves to the avatar first:
  // a dialog it opens returns focus there when it closes.
  const choose = (action: () => void) => () => {
    button.current?.focus();
    setOpen(false);
    action();
  };
  return (
    <div className="account-menu" ref={wrapper}>
      <button
        ref={button}
        type="button"
        className="header-icon account-button"
        aria-label={`Account: ${label}`}
        aria-haspopup="true"
        aria-expanded={open}
        aria-controls={open ? panelId : undefined}
        title={me?.email ?? label}
        onClick={() => setOpen(!open)}
      >
        <Avatar me={me} />
      </button>
      {open && (
        <div id={panelId} className="account-panel cds--layer-two" role="region" aria-label="Account" onKeyDown={onMenuKeys}>
          <div className="account-panel-who">
            <div className="account-panel-names">
              <p className="account-panel-name">{label}</p>
              {me?.email && me.email !== label && <p className="account-panel-email">{me.email}</p>}
            </div>
            <Avatar me={me} large />
          </div>
          {/* Listed even before one opens (its context failed to load), so a
              phone, whose header has no organization dropdown, can choose
              another. */}
          {(current || organizations.length > 0) && (
            <div className="account-panel-organization">
              <span className="account-panel-caption">Organization</span>
              {organizations.length > 1 || !current ? (
                <select
                  aria-label="Switch organization"
                  value={current?.id ?? ''}
                  data-menu-item
                  onChange={event => { if (!event.target.value) return; setOpen(false); onSelectOrganization(event.target.value); }}
                >
                  {!current && <option value="">Select organization…</option>}
                  {organizations.map(organization => <option key={organization.id} value={organization.id}>{organization.name}</option>)}
                </select>
              ) : <span>{current.name}</span>}
            </div>
          )}
          <ul className="account-panel-links">
            <li><button type="button" data-menu-item className="account-panel-link" onClick={choose(onSettings)}>Settings</button></li>
            {onChangePassword && <li><button type="button" data-menu-item className="account-panel-link" onClick={choose(onChangePassword)}>Change password</button></li>}
            {onNewOrganization && <li><button type="button" data-menu-item className="account-panel-link" onClick={choose(onNewOrganization)}>New organization</button></li>}
            <li><button type="button" data-menu-item className="account-panel-link" onClick={choose(() => setDialog('cli'))}>Log in to CLI and API</button></li>
            <li><button type="button" data-menu-item className="account-panel-link" onClick={choose(() => setDialog('theme'))}>Change theme</button></li>
          </ul>
          <button
            type="button"
            data-menu-item
            className="account-panel-logout"
            aria-busy={logoutPending}
            disabled={logoutPending}
            onClick={onLogout}
          >
            {logoutPending ? 'Logging out…' : 'Log out'}
            <ArrowRight size={16} aria-hidden="true" />
          </button>
        </div>
      )}
      <ThemeDialog
        open={dialog === 'theme'}
        preference={themePreference}
        onConfirm={next => { onThemePreference(next); setDialog(null); }}
        onClose={() => setDialog(null)}
      />
      <CliDialog open={dialog === 'cli'} onClose={() => setDialog(null)} onAgents={() => { setDialog(null); onAgents(); }} />
    </div>
  );
}

const themeChoices: [ThemePreference, string, React.ComponentType<{ size?: number; 'aria-hidden'?: boolean | 'true' }>][] = [
  ['system', 'System default', Screen],
  ['dark', 'Dark', Moon],
  ['light', 'Light', Sun],
];

/** Light, dark, or the system's: chosen here, applied on Confirm. */
export function ThemeDialog({
  open,
  preference,
  onConfirm,
  onClose,
}: {
  open: boolean;
  preference: ThemePreference;
  onConfirm: (next: ThemePreference) => void;
  onClose: () => void;
}) {
  const [choice, setChoice] = useState(preference);
  // Each opening starts from what is in force; Cancel leaves it unchanged.
  useEffect(() => { if (open) setChoice(preference); }, [open, preference]);
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="Change theme"
      describedBy="theme-dialog-note"
      footer={<>
        <button type="button" className="cds--btn cds--btn--secondary" onClick={onClose}>Cancel</button>
        <button type="button" className="cds--btn cds--btn--primary" onClick={() => onConfirm(choice)}>Confirm</button>
      </>}
    >
      <p id="theme-dialog-note" className="dialog-body">System default follows your device’s setting. The choice is remembered in this browser.</p>
      <TileGroup
        className="theme-tiles"
        legend="Themes"
        name="theme"
        valueSelected={choice}
        onChange={value => setChoice(value as ThemePreference)}
      >
        {themeChoices.map(([value, text, Icon]) => (
          <RadioTile key={value} id={`theme-${value}`} value={value} className="theme-tile">
            <span className="theme-tile-label"><Icon size={16} aria-hidden="true" />{text}</span>
          </RadioTile>
        ))}
      </TileGroup>
    </Dialog>
  );
}

/** How to reach the same API from a terminal or a script. */
export function CliDialog({ open, onClose, onAgents }: { open: boolean; onClose: () => void; onAgents: () => void }) {
  const origin = typeof location === 'undefined' ? 'https://<gateway>' : location.origin;
  return (
    <Dialog
      open={open}
      onClose={onClose}
      title="Log in to CLI and API"
      footer={<>
        <button type="button" className="cds--btn cds--btn--secondary" onClick={onClose}>Close</button>
        <button type="button" className="cds--btn cds--btn--primary" onClick={onAgents}>Open agents</button>
      </>}
    >
      <div className="dialog-body">
        <p>
          <code>gwctl</code> and scripts call the admin API with an agent token (<code>gwa_…</code>). Create an agent and its
          token under <b>Identities → Agents</b>; the token is shown once.
        </p>
        <pre className="cli-snippet">{`export GATEWAY_URL=${origin}\nexport GATEWAY_AGENT_TOKEN=gwa_…\ngwctl orgs`}</pre>
        <p>Applications call models with a service account’s API key (<code>gw_…</code>), from <b>Identities → Service accounts</b>.</p>
      </div>
    </Dialog>
  );
}
