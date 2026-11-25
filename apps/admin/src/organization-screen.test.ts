import { describe, expect, it } from 'vitest';
import { teamSelectorOptions } from './organization-screen';

describe('team selector context fallback', () => {
  it('derives only the confirmed current team when the list is unavailable', () => {
    const options = teamSelectorOptions([], {
      user_id: 'user-1', organization_id: 'org-1', team_id: 'team-confirmed', role: 'TeamAdmin',
    }, 'org-1');
    expect(options).toHaveLength(1);
    expect(options[0]).toMatchObject({ id: 'team-confirmed', version: 0 });
    expect(options[0].name).toContain('Current team');
  });

  it('does not invent a team option for another organization or no team context', () => {
    expect(teamSelectorOptions([], {
      user_id: 'user-1', organization_id: 'org-2', team_id: 'team-2', role: 'TeamAdmin',
    }, 'org-1')).toEqual([]);
    expect(teamSelectorOptions([], {
      user_id: 'user-1', organization_id: 'org-1', team_id: null, role: 'Admin',
    }, 'org-1')).toEqual([]);
  });
});
