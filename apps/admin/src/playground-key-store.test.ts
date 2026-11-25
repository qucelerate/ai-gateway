import { afterEach, describe, expect, it } from 'vitest';
import type { ApiKey } from './api';
import { forgetStoredKey, storedKey, storeKey } from './playground-key-store';

describe('playground key store', () => {
  afterEach(() => sessionStorage.clear());
  const temporary = (expires: string) => ({ source: 'temporary' as const, secret: 'gw_t', key: { id: 'k1', expires_at: expires } as ApiKey });

  it('drops a temporary key once it expires', () => {
    const now = Date.parse('2026-11-16T12:00:00Z');
    storeKey('org-1', 'user-1', temporary('2026-11-16T12:30:00Z'));
    expect(storedKey('org-1', 'user-1', now)?.secret).toBe('gw_t');
    expect(storedKey('org-1', 'user-1', now + 3_600_000)).toBeNull();
    expect(sessionStorage.length).toBe(0);
  });

  it('keeps a key for the person who held it, and forgets it on sign-out', () => {
    storeKey('org-1', 'user-1', { source: 'pasted', secret: 'gw_p' });
    expect(storedKey('org-1', 'user-2')).toBeNull();
    expect(storedKey('org-1', 'user-1')?.secret).toBe('gw_p');
    forgetStoredKey();
    expect(storedKey('org-1', 'user-1')).toBeNull();
  });

  it('ignores what it cannot read', () => {
    sessionStorage.setItem('gateway.playground.key', '{not json');
    expect(storedKey('org-1', 'user-1')).toBeNull();
    sessionStorage.setItem('gateway.playground.key', JSON.stringify({ organization_id: 'org-1', user_id: 'user-1', key: { source: 'pasted', secret: '' } }));
    expect(storedKey('org-1', 'user-1')).toBeNull();
  });
});
