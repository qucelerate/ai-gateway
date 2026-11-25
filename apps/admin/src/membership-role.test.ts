import { describe, expect, it } from 'vitest';
import { membershipRoles, normalizeMembershipRole, membershipRoleLabel } from './membership-role';

describe('membership role normalization', () => {
  it('maps backend lower-case role strings to the PascalCase write vocabulary', () => {
    expect(normalizeMembershipRole('owner')).toBe('Owner');
    expect(normalizeMembershipRole('admin')).toBe('Admin');
    expect(normalizeMembershipRole('member')).toBe('Member');
    expect(normalizeMembershipRole('auditor')).toBe('Auditor');
    expect(normalizeMembershipRole('read-only')).toBe('ReadOnly');
  });

  it('is idempotent for already-normalized values', () => {
    for (const role of membershipRoles) {
      expect(normalizeMembershipRole(role)).toBe(role);
    }
  });

  it('produces human-readable labels for both wire forms', () => {
    expect(membershipRoleLabel('owner')).toBe('Owner');
    expect(membershipRoleLabel('read-only')).toBe('Read Only');
    expect(membershipRoleLabel('ReadOnly')).toBe('Read Only');
    expect(membershipRoleLabel('Admin')).toBe('Admin');
  });

  it('defaults unknown roles to Member', () => {
    expect(normalizeMembershipRole('unknown')).toBe('Member');
    expect(membershipRoleLabel('unknown')).toBe('Member');
  });
});
