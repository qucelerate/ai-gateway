import type { MembershipRole } from './api';

export const membershipRoles: MembershipRole[] = ['Owner', 'Admin', 'Member', 'Auditor', 'ReadOnly'];

const roleMap: Record<string, MembershipRole> = {
  owner: 'Owner',
  admin: 'Admin',
  member: 'Member',
  auditor: 'Auditor',
  'read-only': 'ReadOnly',
  readonly: 'ReadOnly',
};

export function normalizeMembershipRole(role: string): MembershipRole {
  return roleMap[role.toLowerCase()] ?? 'Member';
}

export function membershipRoleLabel(role: string): string {
  return normalizeMembershipRole(role).replace(/([A-Z])/g, ' $1').trim();
}
