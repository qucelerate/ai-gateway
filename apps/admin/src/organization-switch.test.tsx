import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, type Context } from './api';
import { OrganizationScreen } from './organization-screen';

const context = (organizationId: string, teamId: string | null): Context => ({
  user_id: 'user-1', organization_id: organizationId, team_id: teamId, role: 'TeamAdmin',
});

describe('organization switch boundaries', () => {
  afterEach(() => vi.restoreAllMocks());

  it('does not render old teams in the new organization or request its members with the old team', async () => {
    const listTeams = vi.spyOn(api, 'listTeams').mockImplementation(async organizationId => ({
      items: organizationId === 'org-old'
        ? [{ id: 'team-old', organization_id: organizationId, name: 'Old team', version: 1 }]
        : [],
      next_page_token: null,
    }));
    vi.spyOn(api, 'listOrgMembers').mockResolvedValue({ items: [], next_page_token: null });
    vi.spyOn(api, 'listRoleBindings').mockResolvedValue({ items: [], next_page_token: null });
    const listTeamMembers = vi.spyOn(api, 'listTeamMembers').mockResolvedValue({ items: [], next_page_token: null });
    const container = document.createElement('div');
    document.body.appendChild(container);
    const root = createRoot(container);
    history.replaceState(null, '', '/admin/people/teams');

    await act(async () => {
      root.render(<OrganizationScreen key="org-old" organizationId="org-old" userId="user-1" context={context('org-old', 'team-old')} />);
    });
    expect(container.textContent).toContain('Old team');

    await act(async () => {
      root.render(<OrganizationScreen key="org-new" organizationId="org-new" userId="user-1" context={context('org-new', 'team-new')} />);
    });

    expect(container.textContent).not.toContain('Old team');
    expect(listTeams).toHaveBeenLastCalledWith('org-new', expect.anything());
    expect(listTeamMembers).not.toHaveBeenCalledWith('org-new', 'team-old', expect.anything());
    root.unmount();
    history.replaceState(null, '', '/');
    container.remove();
  });

  it('keeps a team from a later page selected after changing its members', async () => {
    const team = (id: string, name: string) => ({ id, organization_id: 'org-1', name, version: 1 });
    const listTeams = vi.spyOn(api, 'listTeams').mockImplementation(async (_organizationId, params) => (params?.page_token
      ? { items: [team('team-b', 'Team B')], next_page_token: null }
      : { items: [team('team-a', 'Team A')], next_page_token: 'next' }));
    vi.spyOn(api, 'listOrgMembers').mockResolvedValue({ items: [], next_page_token: null });
    vi.spyOn(api, 'listRoleBindings').mockResolvedValue({ items: [], next_page_token: null });
    const member = { organization_id: 'org-1', team_id: 'team-b', user_id: 'user-2', role: 'member', enabled: true, version: 1 };
    const listTeamMembers = vi.spyOn(api, 'listTeamMembers').mockImplementation(async (_organizationId, teamId) => ({
      items: teamId === 'team-b' ? [member as never] : [], next_page_token: null,
    }));
    const deleteTeamMember = vi.spyOn(api, 'deleteTeamMember').mockResolvedValue({ data: { deleted: true }, meta: { requestId: 'r' } } as never);
    const container = document.createElement('div');
    document.body.appendChild(container);
    const root = createRoot(container);
    history.replaceState(null, '', '/admin/people/teams');
    const settle = async () => { for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); }); };
    const button = (text: string) => Array.from(document.querySelectorAll('button')).filter(node => node.textContent === text).pop() as HTMLButtonElement;

    await act(async () => {
      root.render(<OrganizationScreen organizationId="org-1" userId="user-1" context={context('org-1', null)} />);
    });
    await settle();
    act(() => button('Load more').click());
    await settle();
    const picker = container.querySelector<HTMLSelectElement>('select[aria-label="Select team"]')!;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value')!.set!.call(picker, 'team-b');
      picker.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    const teamLoads = listTeams.mock.calls.length;
    act(() => button('Remove').click());
    await settle();
    act(() => button('Remove').click());
    await settle();

    expect(deleteTeamMember).toHaveBeenCalledWith('org-1', 'team-b', 'user-2', 1);
    // Only the team's members reload; the teams, and the selection, stay.
    expect(listTeams.mock.calls.length).toBe(teamLoads);
    expect(listTeamMembers).toHaveBeenLastCalledWith('org-1', 'team-b', expect.anything());
    expect(container.querySelector<HTMLSelectElement>('select[aria-label="Select team"]')!.value).toBe('team-b');
    root.unmount();
    container.remove();
    history.replaceState(null, '', '/');
  });
});
