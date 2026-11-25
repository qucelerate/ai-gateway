import { describe, expect, it } from 'vitest';
import { movedPath } from './moved-path';

describe('moved paths', () => {
  it('sends catalog tabs to Models and cloud tabs to Connections', () => {
    expect(movedPath('/admin/providers', '')).toBe('/admin/models');
    expect(movedPath('/admin/providers/pricing', '')).toBe('/admin/models/pricing');
    expect(movedPath('/admin/providers/credentials', '')).toBe('/admin/connections/credentials');
    expect(movedPath('/admin/providers/trust', '')).toBe('/admin/connections/trust');
  });

  it('keeps an Azure consent return on Connect a provider', () => {
    expect(movedPath('/admin/providers', '?connect=azure&organization=o&tenant=t')).toBe('/admin/connections/connect');
  });

  it('splits the old Organization tabs between People, Identities and Settings', () => {
    expect(movedPath('/admin/organization', '')).toBe('/admin/people');
    expect(movedPath('/admin/organization/org-members', '')).toBe('/admin/people/members');
    expect(movedPath('/admin/organization/team-members', '')).toBe('/admin/people/teams');
    expect(movedPath('/admin/organization/effective-access', '')).toBe('/admin/people/access');
    expect(movedPath('/admin/organization/agents', '')).toBe('/admin/identities/agents');
    expect(movedPath('/admin/organization/routing', '')).toBe('/admin/settings/routing');
  });

  it('keeps a sign-in test return on Sign-in', () => {
    expect(movedPath('/admin/organization', '?organization=o&sign_in_test=activated')).toBe('/admin/settings/sign-in');
  });

  it('sends the old Traces tab to its own page and the usage report and Breakdown to the dashboard', () => {
    expect(movedPath('/admin/usage/traces', '?traces.q=gpt')).toBe('/admin/traces');
    expect(movedPath('/admin/usage/usage', '')).toBe('/admin/usage/dashboard');
    expect(movedPath('/admin/usage/breakdown', '?usage.g=team,model')).toBe('/admin/usage/dashboard');
  });

  it('leaves current paths alone', () => {
    for (const path of ['/admin', '/admin/models/pricing', '/admin/people/teams', '/admin/usage', '/admin/usage/dashboard', '/admin/usage/reconciliation', '/admin/traces', '/admin/traces/t-1']) expect(movedPath(path, '')).toBeNull();
  });
});
