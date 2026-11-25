// The playground's key, kept for the browser tab: a reload keeps it, closing
// the tab forgets it. It never leaves the tab (session storage is per tab and
// never sent), and a temporary key is dropped once it expires.

import type { ApiKey } from './api';

/** The key the page sends with: pasted, or a temporary one it created on the
 * person's own playground service account. */
export type HeldKey =
  | { source: 'pasted'; secret: string }
  | { source: 'temporary'; secret: string; key: ApiKey };

const STORAGE_KEY = 'gateway.playground.key';

type Stored = { organization_id: string; user_id: string; key: HeldKey };

/** The key this tab kept for this person in `organizationId`, if it is
 * still usable. */
export function storedKey(organizationId: string, userId: string, now = Date.now()): HeldKey | null {
  let stored: Stored;
  try {
    const raw = sessionStorage.getItem(STORAGE_KEY);
    if (!raw) return null;
    stored = JSON.parse(raw) as Stored;
  } catch {
    return null;
  }
  const key = stored?.key;
  if (stored?.organization_id !== organizationId || stored?.user_id !== userId || !key || typeof key.secret !== 'string' || !key.secret) return null;
  if (key.source === 'pasted') return { source: 'pasted', secret: key.secret };
  if (key.source === 'temporary' && key.key && Date.parse(key.key.expires_at ?? '') > now) return key;
  forgetStoredKey();
  return null;
}

/** Forgets the kept key: on sign-out, so the next person in this tab never
 * gets it. */
export function forgetStoredKey() {
  try {
    sessionStorage.removeItem(STORAGE_KEY);
  } catch {
    // Storage blocked: nothing was kept.
  }
}

/** Keeps `key` for this tab and person, or forgets the kept one. */
export function storeKey(organizationId: string, userId: string, key: HeldKey | null) {
  if (!key) return forgetStoredKey();
  try {
    sessionStorage.setItem(STORAGE_KEY, JSON.stringify({ organization_id: organizationId, user_id: userId, key } satisfies Stored));
  } catch {
    // Storage blocked: the key lasts as long as the page.
  }
}
