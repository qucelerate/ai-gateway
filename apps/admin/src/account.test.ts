import { describe, expect, it } from 'vitest';
import { accountInitials, accountLabel } from './account';

const me = (display_name: string | null, email: string | null) => ({ user_id: 'u-1', display_name, email, photo_url: null });

describe('account display', () => {
  it('names the person by name, else email', () => {
    expect(accountLabel(me('Ada Lovelace', 'ada@example.com'))).toBe('Ada Lovelace');
    expect(accountLabel(me(null, 'ada@example.com'))).toBe('ada@example.com');
    expect(accountLabel(me('  ', null))).toBe('Signed in');
    expect(accountLabel(null)).toBe('Signed in');
  });

  it('uses first and last initials, else the email initial', () => {
    expect(accountInitials(me('Ada King Lovelace', null))).toBe('AL');
    expect(accountInitials(me('ada', null))).toBe('A');
    expect(accountInitials(me(null, 'zoe@example.com'))).toBe('Z');
    expect(accountInitials(me(null, null))).toBe('?');
  });
});
