import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react';
import type { Me, Organization } from './api';
import { AccountMenu, HeaderOrganization, HelpLink, ThemeDialog, docsUrl, type AccountMenuProps } from './header';

const org = (id: string, name: string): Organization => ({ id, name } as Organization);
const me: Me = { user_id: 'u1', display_name: 'Riley Chen', email: 'riley@example.com', photo_url: null };

let container: HTMLDivElement;
let root: Root;
beforeEach(() => {
  (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
  container = document.createElement('div');
  document.body.appendChild(container);
  root = createRoot(container);
});
afterEach(() => {
  act(() => root.unmount());
  container.remove();
});

const byText = (text: string) => Array.from(document.querySelectorAll<HTMLElement>('button, a'))
  .find(node => node.textContent?.trim() === text || node.getAttribute('aria-label') === text);
const click = (node: HTMLElement | undefined | null) => act(() => { node!.click(); });

describe('header organization and Help', () => {
  it('shows the organization as a dropdown just before Help, with the role in it', () => {
    act(() => root.render(<div className="header-actions">
      <HeaderOrganization organizations={[org('o1', 'QuCelerate')]} current={org('o1', 'QuCelerate')} role="owner" onSelect={() => {}} />
      <HelpLink />
    </div>));
    const [name, help] = Array.from(container.querySelector('.header-actions')!.children);
    expect(name.textContent).toBe('QuCelerate');
    expect(help.getAttribute('aria-label')).toMatch(/^Help/);
    expect(help.getAttribute('href')).toBe(docsUrl);
    expect(help.getAttribute('target')).toBe('_blank');
    // Even with one organization it opens, to see the role; it only switches
    // organizations: creating and managing them are elsewhere.
    click(container.querySelector<HTMLButtonElement>('button.header-organization'));
    expect(container.textContent).toContain('Your role in QuCelerate: owner');
    expect(container.querySelectorAll('[role="menuitem"]')).toHaveLength(0);
    expect(container.textContent).not.toContain('Create organization');
    expect(container.textContent).not.toContain('Manage organizations');
  });

  it('switches organization from the name when there are several', () => {
    const onSelect = vi.fn();
    const organizations = [org('o1', 'QuCelerate'), org('o2', 'Sandbox')];
    act(() => root.render(<HeaderOrganization organizations={organizations} current={organizations[0]} onSelect={onSelect} />));
    const toggle = container.querySelector<HTMLButtonElement>('button.header-organization')!;
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    click(toggle);
    expect(toggle.getAttribute('aria-expanded')).toBe('true');
    const items = Array.from(container.querySelectorAll<HTMLButtonElement>('[role="menuitemradio"]'));
    expect(items.map(item => item.getAttribute('aria-checked'))).toEqual(['true', 'false']);
    expect(document.activeElement).toBe(items[0]);
    act(() => { items[0].parentElement!.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true })); });
    expect(document.activeElement).toBe(items[1]);
    click(items[1]);
    expect(onSelect).toHaveBeenCalledWith('o2');
    expect(container.querySelector('[role="menu"]')).toBeNull();
  });

  it('shows nothing without an open organization', () => {
    act(() => root.render(<HeaderOrganization organizations={[]} current={null} onSelect={() => {}} />));
    expect(container.innerHTML).toBe('');
  });
});

function renderMenu(overrides: Partial<AccountMenuProps> = {}) {
  const props: AccountMenuProps = {
    me,
    organizations: [org('o1', 'QuCelerate')],
    current: org('o1', 'QuCelerate'),
    onSelectOrganization: vi.fn(),
    onSettings: vi.fn(),
    onAgents: vi.fn(),
    themePreference: 'system',
    onThemePreference: vi.fn(),
    logoutPending: false,
    onLogout: vi.fn(),
    ...overrides,
  };
  act(() => root.render(<AccountMenu {...props} />));
  return props;
}

describe('account menu', () => {
  it('holds the name, email, settings, CLI, theme and a separate Log out', () => {
    const props = renderMenu();
    const avatar = container.querySelector<HTMLButtonElement>('.account-button')!;
    expect(avatar.getAttribute('aria-label')).toBe('Account: Riley Chen');
    expect(avatar.textContent).toBe('RC');
    click(avatar);
    const panel = container.querySelector('.account-panel')!;
    expect(panel.querySelector('.account-panel-name')!.textContent).toBe('Riley Chen');
    expect(panel.querySelector('.account-panel-email')!.textContent).toBe('riley@example.com');
    expect(Array.from(panel.querySelectorAll('.account-panel-link')).map(link => link.textContent))
      .toEqual(['Settings', 'Log in to CLI and API', 'Change theme']);
    expect(document.activeElement?.textContent).toBe('Settings');
    click(byText('Settings'));
    expect(props.onSettings).toHaveBeenCalled();
    expect(container.querySelector('.account-panel')).toBeNull();
    click(avatar);
    click(container.querySelector<HTMLButtonElement>('.account-panel-logout'));
    expect(props.onLogout).toHaveBeenCalled();
  });

  it('offers the organizations before one opens, as a phone has no header dropdown', () => {
    const props = renderMenu({ current: null, organizations: [org('o1', 'QuCelerate'), org('o2', 'Sandbox')] });
    click(container.querySelector<HTMLButtonElement>('.account-button'));
    const select = container.querySelector<HTMLSelectElement>('select[aria-label="Switch organization"]')!;
    expect(Array.from(select.options).map(option => option.textContent)).toEqual(['Select organization…', 'QuCelerate', 'Sandbox']);
    act(() => {
      select.value = 'o2';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    expect(props.onSelectOrganization).toHaveBeenCalledWith('o2');
  });

  it('closes on Escape and returns focus to the avatar', () => {
    renderMenu();
    const avatar = container.querySelector<HTMLButtonElement>('.account-button')!;
    click(avatar);
    act(() => { document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape' })); });
    expect(container.querySelector('.account-panel')).toBeNull();
    expect(document.activeElement).toBe(avatar);
  });

  it('offers password and organization actions only when they apply', () => {
    renderMenu({ onChangePassword: vi.fn(), onNewOrganization: vi.fn() });
    click(container.querySelector<HTMLButtonElement>('.account-button'));
    expect(Array.from(container.querySelectorAll('.account-panel-link')).map(link => link.textContent))
      .toEqual(['Settings', 'Change password', 'New organization', 'Log in to CLI and API', 'Change theme']);
  });

  it('skips the organization select while it is hidden, as above the narrow breakpoint', () => {
    const style = document.createElement('style');
    style.textContent = '.account-panel-organization { display: none; }';
    document.head.appendChild(style);
    try {
      renderMenu({ organizations: [org('o1', 'QuCelerate'), org('o2', 'Sandbox')] });
      click(container.querySelector<HTMLButtonElement>('.account-button'));
      expect(document.activeElement?.textContent).toBe('Settings');
      const panel = container.querySelector<HTMLElement>('.account-panel')!;
      act(() => { document.activeElement!.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', bubbles: true })); });
      expect(document.activeElement).toBe(panel.querySelector('.account-panel-logout'));
    } finally {
      style.remove();
    }
  });

  it('leaves the organization select its own arrow keys on narrow screens', () => {
    const props = renderMenu({ organizations: [org('o1', 'QuCelerate'), org('o2', 'Sandbox')] });
    click(container.querySelector<HTMLButtonElement>('.account-button'));
    const select = container.querySelector<HTMLSelectElement>('.account-panel select')!;
    expect(document.activeElement).toBe(select);
    const event = new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true, cancelable: true });
    act(() => { select.dispatchEvent(event); });
    expect(event.defaultPrevented).toBe(false);
    expect(document.activeElement).toBe(select);
    act(() => {
      select.value = 'o2';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    expect(props.onSelectOrganization).toHaveBeenCalledWith('o2');
  });

  it('returns focus to the avatar when a dialog it opened closes', () => {
    renderMenu();
    const avatar = container.querySelector<HTMLButtonElement>('.account-button')!;
    click(avatar);
    click(byText('Change theme'));
    expect(document.querySelector('[role="dialog"]')).toBeTruthy();
    click(byText('Cancel'));
    expect(document.querySelector('[role="dialog"]')).toBeNull();
    expect(document.activeElement).toBe(avatar);
  });

  it('explains the CLI and links to agents', () => {
    const props = renderMenu();
    click(container.querySelector<HTMLButtonElement>('.account-button'));
    click(byText('Log in to CLI and API'));
    const dialog = document.querySelector('[role="dialog"]')!;
    expect(dialog.textContent).toContain('GATEWAY_AGENT_TOKEN');
    expect(dialog.textContent).toContain('gwctl');
    click(byText('Open agents'));
    expect(props.onAgents).toHaveBeenCalled();
    expect(document.querySelector('[role="dialog"]')).toBeNull();
  });
});

describe('Change theme', () => {
  const choose = (value: string) => act(() => { document.querySelector<HTMLInputElement>(`input[value="${value}"]`)!.click(); });

  it('applies the choice on Confirm', () => {
    const onConfirm = vi.fn();
    act(() => root.render(<ThemeDialog open preference="system" onConfirm={onConfirm} onClose={() => {}} />));
    expect(document.querySelector('[role="dialog"]')!.textContent).toContain('Change theme');
    expect(document.querySelector<HTMLInputElement>('input[value="system"]')!.checked).toBe(true);
    choose('dark');
    click(byText('Confirm'));
    expect(onConfirm).toHaveBeenCalledWith('dark');
  });

  it('changes nothing on Cancel, and starts again from the preference', () => {
    const onConfirm = vi.fn();
    const onClose = vi.fn();
    act(() => root.render(<ThemeDialog open preference="light" onConfirm={onConfirm} onClose={onClose} />));
    choose('dark');
    click(byText('Cancel'));
    expect(onClose).toHaveBeenCalled();
    expect(onConfirm).not.toHaveBeenCalled();
    act(() => root.render(<ThemeDialog open={false} preference="light" onConfirm={onConfirm} onClose={onClose} />));
    act(() => root.render(<ThemeDialog open preference="light" onConfirm={onConfirm} onClose={onClose} />));
    expect(document.querySelector<HTMLInputElement>('input[value="light"]')!.checked).toBe(true);
  });

  it('is applied through the account menu only on Confirm', () => {
    const props = renderMenu({ themePreference: 'system' });
    click(container.querySelector<HTMLButtonElement>('.account-button'));
    click(byText('Change theme'));
    choose('light');
    click(byText('Cancel'));
    expect(props.onThemePreference).not.toHaveBeenCalled();
    click(container.querySelector<HTMLButtonElement>('.account-button'));
    click(byText('Change theme'));
    choose('light');
    click(byText('Confirm'));
    expect(props.onThemePreference).toHaveBeenCalledWith('light');
  });
});
