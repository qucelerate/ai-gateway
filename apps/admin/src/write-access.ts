// Whether to offer create, edit and delete controls. A convenience only: the
// server checks every request, and a refused one shows its 403.

/** Membership roles whose admin role can read but not write: auditors,
 * read-only members, and members (who get the read-only admin role). */
const readOnlyRoles = new Set(['auditor', 'readonly', 'read-only', 'read_only', 'member', 'read_only_member']);

/** False only for a role known to be read-only; an unknown or missing role
 * keeps the controls and lets the server decide. */
export function offersChanges(role: string | null | undefined): boolean {
  return !role || !readOnlyRoles.has(role.toLowerCase());
}
