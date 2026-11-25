import { useEffect, useState } from 'react';
import { api } from './api';

/** Sent (on `window`) when an item is resolved, so the count reads again. */
export const reconciliationChanged = 'gateway:reconciliation';

/** Reconciliation items read in one go; past it the count shows `99+`. */
const COUNTED = 100;

/**
 * How many reconciliation items `organizationId` has open, for the side
 * navigation: `99+` past 99, and null when there are none, while loading,
 * or when the member may not read them. Read again whenever `refresh`
 * changes (the route) or an item is resolved (`reconciliationChanged`).
 */
export function useOpenReconciliation(organizationId: string, refresh: unknown): string | null {
  const [count, setCount] = useState<string | null>(null);
  const [changes, setChanges] = useState(0);
  useEffect(() => {
    const changed = () => setChanges(value => value + 1);
    addEventListener(reconciliationChanged, changed);
    return () => removeEventListener(reconciliationChanged, changed);
  }, []);
  useEffect(() => {
    if (!organizationId) { setCount(null); return; }
    let current = true;
    api.listReconciliationItems(organizationId, 'open', { page_size: COUNTED }).then(
      page => {
        if (!current) return;
        const open = page.items.length;
        setCount(open === 0 ? null : open > 99 || page.next_page_token ? '99+' : String(open));
      },
      () => { if (current) setCount(null); },
    );
    return () => { current = false; };
  }, [organizationId, refresh, changes]);
  return count;
}
