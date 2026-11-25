import { describe, expect, it } from 'vitest';
import { offersChanges } from './write-access';

describe('offersChanges', () => {
  it('hides changes from roles that can only read', () => {
    for (const role of ['Auditor', 'ReadOnly', 'Member', 'read-only', 'read_only_member']) expect(offersChanges(role)).toBe(false);
  });

  it('offers changes to owners and admins, and leaves an unknown role to the server', () => {
    for (const role of ['Owner', 'Admin', 'organization_admin', null, undefined, '']) expect(offersChanges(role)).toBe(true);
  });
});
