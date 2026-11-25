/** Where a path from before the navigation was regrouped now lives:
 * bookmarks, and the gateway's own redirects (Azure consent returns to
 * /admin/providers, a sign-in test to /admin/organization). Null for a
 * current path. */
export function movedPath(pathname: string, search: string): string | null {
  const [admin = 'admin', page, tab] = pathname.split('/').filter(Boolean);
  if (page === 'providers') {
    const connections = ['connect', 'credentials', 'discovery', 'trust'];
    if (tab && connections.includes(tab)) return `/${admin}/connections/${tab}`;
    if (!tab && new URLSearchParams(search).has('connect')) return `/${admin}/connections/connect`;
    return tab ? `/${admin}/models/${tab}` : `/${admin}/models`;
  }
  if (page === 'organization') {
    const moved: Record<string, string> = {
      teams: 'people/teams', 'org-members': 'people/members', 'team-members': 'people/teams', invitations: 'people/invitations',
      'role-bindings': 'people/access', 'effective-access': 'people/access', agents: 'identities/agents',
      'sign-in': 'settings/sign-in', routing: 'settings/routing',
    };
    if (tab && moved[tab]) return `/${admin}/${moved[tab]}`;
    if (new URLSearchParams(search).has('sign_in_test')) return `/${admin}/settings/sign-in`;
    return `/${admin}/people`;
  }
  if (page === 'usage') {
    // Traces became their own page; the usage report (later Breakdown) is
    // now the dashboard's breakdown, which reads a Breakdown view's `usage.*`
    // parameters.
    if (tab === 'traces') return `/${admin}/traces`;
    if (tab === 'usage' || tab === 'breakdown') return `/${admin}/usage/dashboard`;
  }
  return null;
}
