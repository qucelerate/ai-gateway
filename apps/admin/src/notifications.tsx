import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Notification as BellIcon } from '@carbon/icons-react';
import { api, normalizeApiError, type Notification, type NotificationFilters, type NotificationSeverity } from './api';
import { SessionExpired, type SignIn } from './screen-feedback';
import { AppLink, Badge, Button, EmptyState, FormField, LoadMore, Message, Select, formatRelativeDate, navigateTo } from './ui';

/** How often the header bell reads the unread count. */
export const POLL_MS = 60_000;
/** Fired when the member marks notifications read, so every count reads again. */
const READ_EVENT = 'gateway:notifications-read';
const PAGE = '/admin/notifications';

export const kindLabels: Record<string, string> = {
  'price.adopted': 'Prices adopted',
  'price.held': 'Price change held',
  'price_source.failing': 'Price source failing',
  'provider_account.sync_failing': 'Model listing failing',
  'budget.threshold': 'Budget threshold',
  'api_key.expiring': 'API key expiring',
  'agent_token.expiring': 'Agent token expiring',
  'webhook.failing': 'Webhook failing',
  'end_user.credits_low': 'End-user credits low',
  'reconciliation.open': 'Costs to reconcile',
};
const severityLabels: Record<NotificationSeverity, string> = { info: 'Info', warning: 'Warning', action: 'Action needed' };

export function kindLabel(kind: string): string {
  return kindLabels[kind] ?? kind;
}

/** What was marked read: chosen ones, or everything created up to and
 * including one (by creation, then id). */
export type ReadMark = { ids: string[] } | { through: Pick<Notification, 'id' | 'created_at'> };

/** An RFC 3339 time in microseconds since the epoch, as the server stores
 * it: `Date.parse` keeps only milliseconds, so two notifications within one
 * would otherwise tie and be ordered by id alone. */
export function micros(value: string): number {
  const match = /^(.*T\d\d:\d\d:\d\d)(?:\.(\d+))?(Z|[+-]\d\d:\d\d)$/i.exec(value);
  if (!match) return Date.parse(value) * 1000;
  const fraction = (match[2] ?? '').padEnd(6, '0').slice(0, 6);
  return Date.parse(`${match[1]}${match[3]}`) * 1000 + Number(fraction);
}

/** Whether `mark` covers `item`. */
export function covers(mark: ReadMark, item: Pick<Notification, 'id' | 'created_at'>): boolean {
  if ('ids' in mark) return mark.ids.includes(item.id);
  const at = micros(item.created_at);
  const through = micros(mark.through.created_at);
  // The same microsecond: the id breaks the tie, as the server's order does.
  return at < through || (at === through && item.id <= mark.through.id);
}

/** `items` with what `mark` covers shown read. */
export function applyRead<T extends Notification>(items: T[], mark: ReadMark): T[] {
  return items.map(item => (!item.read && covers(mark, item) ? { ...item, read: true } : item));
}

/** Marks the member's notifications read, then tells every count and list. */
async function markRead(organizationId: string, mark: ReadMark) {
  const count = await api.markNotificationsRead(organizationId, 'ids' in mark ? { ids: mark.ids } : { through: mark.through.id });
  dispatchEvent(new CustomEvent<ReadEvent>(READ_EVENT, { detail: { organizationId, mark } }));
  return count;
}

/** A mark made in this window, and the organization it was made in. */
type ReadEvent = { organizationId: string; mark: ReadMark };

/** Calls `onRead` with each mark made in this window in `organizationId`;
 * one made in another (before a switch finished) is ignored. */
function useReadMarks(organizationId: string, onRead: (mark: ReadMark) => void) {
  const latest = useRef(onRead);
  latest.current = onRead;
  useEffect(() => {
    const listener = (event: Event) => {
      const detail = (event as CustomEvent<ReadEvent>).detail;
      if (detail?.organizationId === organizationId) latest.current(detail.mark);
    };
    addEventListener(READ_EVENT, listener);
    return () => removeEventListener(READ_EVENT, listener);
  }, [organizationId]);
}

/** The unread count: read now, every minute, when the window regains focus,
 * and after anything is marked read. `null` until known or when unreadable. */
export function useUnreadCount(organizationId: string): number | null {
  const [unread, setUnread] = useState<number | null>(null);
  useEffect(() => {
    if (!organizationId) {
      setUnread(null);
      return;
    }
    let current = true;
    const read = () => {
      api.unreadNotifications(organizationId).then(
        count => { if (current) setUnread(count.unread); },
        () => { if (current) setUnread(null); },
      );
    };
    read();
    const timer = setInterval(read, POLL_MS);
    const onFocus = () => read();
    const onRead = (event: Event) => { if ((event as CustomEvent<ReadEvent>).detail?.organizationId === organizationId) read(); };
    const onVisible = () => { if (document.visibilityState === 'visible') read(); };
    addEventListener('focus', onFocus);
    addEventListener(READ_EVENT, onRead);
    document.addEventListener('visibilitychange', onVisible);
    return () => {
      current = false;
      clearInterval(timer);
      removeEventListener('focus', onFocus);
      removeEventListener(READ_EVENT, onRead);
      document.removeEventListener('visibilitychange', onVisible);
    };
  }, [organizationId]);
  return unread;
}

function SeverityBadge({ severity }: { severity: NotificationSeverity }) {
  const kind = severity === 'action' ? 'danger' : severity === 'warning' ? 'warning' : 'neutral';
  return <Badge kind={kind}>{severityLabels[severity] ?? severity}</Badge>;
}

/** Opens a notification: marks it read and follows its link. */
function useOpen(organizationId: string, onOpened?: () => void) {
  return useCallback(async (item: Notification) => {
    if (!item.read) {
      try { await markRead(organizationId, { ids: [item.id] }); } catch { /* the link still opens */ }
    }
    onOpened?.();
    if (item.link) navigateTo(item.link);
  }, [organizationId, onOpened]);
}

function NotificationLine({ item, onOpen }: { item: Notification; onOpen: (item: Notification) => void }) {
  return (
    <li className={`notification-line${item.read ? '' : ' unread'}${item.resolved_at ? ' resolved' : ''}`}>
      <button type="button" className="notification-open" onClick={() => onOpen(item)}>
        <span className="notification-title">
          {!item.read && <span className="notification-dot" aria-hidden="true" />}
          {item.title}
          {!item.read && <span className="sr-only"> (unread)</span>}
        </span>
        <span className="muted notification-meta">
          {kindLabel(item.kind)}{item.resolved_at ? ' · resolved' : ''} · <time dateTime={item.created_at} title={item.created_at}>{formatRelativeDate(item.created_at)}</time>
        </span>
      </button>
    </li>
  );
}

/** The header bell: the unread count, and the latest five on click. */
export function NotificationBell({ organizationId }: { organizationId: string }) {
  const unread = useUnreadCount(organizationId);
  const [open, setOpen] = useState(false);
  const [items, setItems] = useState<Notification[] | null>(null);
  const [failed, setFailed] = useState(false);
  const wrapper = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    if (!open) return;
    let current = true;
    setFailed(false);
    api.listNotifications(organizationId, { page_size: 5 }).then(
      page => { if (current) setItems(page.items); },
      () => { if (current) setFailed(true); },
    );
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        setOpen(false);
        button.current?.focus();
      }
    };
    const onPointer = (event: MouseEvent) => {
      if (wrapper.current && !wrapper.current.contains(event.target as Node)) setOpen(false);
    };
    document.addEventListener('keydown', onKey);
    document.addEventListener('mousedown', onPointer);
    return () => {
      current = false;
      document.removeEventListener('keydown', onKey);
      document.removeEventListener('mousedown', onPointer);
    };
  }, [open, organizationId]);

  useReadMarks(organizationId, mark => setItems(previous => previous && applyRead(previous, mark)));
  const close = useCallback(() => setOpen(false), []);
  const openItem = useOpen(organizationId, close);
  if (!organizationId) return null;
  const label = unread ? `Notifications, ${unread} unread` : 'Notifications';
  const markAll = async () => {
    // The newest of all, so older unread ones beyond these five are read too.
    const newest = items?.[0];
    if (!newest) return;
    try {
      await markRead(organizationId, { through: newest });
    } catch {
      setFailed(true);
    }
  };

  return (
    <div className="notification-bell" ref={wrapper}>
      <button
        ref={button}
        type="button"
        className="bell-button"
        aria-label={label}
        title={label}
        aria-expanded={open}
        aria-controls="notification-menu"
        onClick={() => setOpen(!open)}
      >
        <BellIcon size={20} aria-hidden="true" />
        {/* A dot while anything is unread; the count is in the label. */}
        {!!unread && <span className="bell-dot" aria-hidden="true" />}
      </button>
      {open && (
        <div id="notification-menu" className="notification-menu cds--layer-two" role="dialog" aria-label="Latest notifications">
          <div className="notification-menu-top">
            <strong>Notifications</strong>
            {(!!unread || items?.some(item => !item.read)) && !!items?.length && <button type="button" className="inline-action" onClick={markAll}>Mark all read</button>}
          </div>
          {failed && <p className="muted">Notifications are unavailable right now.</p>}
          {!failed && items === null && <p className="muted">Loading…</p>}
          {!failed && items?.length === 0 && <p className="muted">Nothing yet. The gateway tells you here when a price source or model listing fails, and when prices change.</p>}
          {!failed && !!items?.length && <ul className="notification-list">{items.map(item => <NotificationLine key={item.id} item={item} onOpen={openItem} />)}</ul>}
          <div className="notification-menu-foot"><AppLink to={PAGE}>View all →</AppLink></div>
        </div>
      )}
    </div>
  );
}

type Filters = { kind: string; severity: NotificationSeverity | ''; read: 'all' | 'unread' | 'read' };

/** `/admin/notifications`: every notification the member can see, filtered. */
export function NotificationsScreen({ organizationId, ...signIn }: SignIn & { organizationId: string }) {
  const [filters, setFilters] = useState<Filters>({ kind: '', severity: '', read: 'all' });
  const [items, setItems] = useState<Notification[]>([]);
  const [token, setToken] = useState<string | null>(null);
  const [state, setState] = useState<'loading' | 'ready' | 'more' | 'failed'>('loading');
  const [status, setStatus] = useState(0);
  const [attempt, setAttempt] = useState(0);
  const [marking, setMarking] = useState(false);
  const [markFailed, setMarkFailed] = useState(false);
  // Bumped by each first-page load, so a page asked for under earlier
  // filters is dropped rather than appended.
  const generation = useRef(0);

  const query = useCallback((pageToken?: string): NotificationFilters & { page_size: number; page_token?: string } => ({
    page_size: 25,
    page_token: pageToken,
    kind: filters.kind || undefined,
    severity: filters.severity || undefined,
    read: filters.read === 'all' ? undefined : filters.read,
  }), [filters]);

  useEffect(() => {
    if (!organizationId) return;
    const current = ++generation.current;
    setState('loading');
    setToken(null);
    api.listNotifications(organizationId, query()).then(
      page => { if (current === generation.current) { setItems(page.items); setToken(page.next_page_token ?? null); setState('ready'); } },
      caught => { if (current === generation.current) { setStatus(normalizeApiError(caught).status); setState('failed'); } },
    );
    return () => { if (current === generation.current) generation.current += 1; };
  }, [organizationId, query, attempt]);

  // Marked read here or in the bell: shown in place, keeping every page
  // loaded so far.
  // Under the Unread filter, what is now read leaves the list.
  useReadMarks(organizationId, mark => setItems(previous => {
    const updated = applyRead(previous, mark);
    return filters.read === 'unread' ? updated.filter(item => !item.read) : updated;
  }));

  const more = async () => {
    if (!token) return;
    const current = generation.current;
    setState('more');
    try {
      const page = await api.listNotifications(organizationId, query(token));
      if (current !== generation.current) return;
      setItems(previous => [...previous, ...page.items]);
      setToken(page.next_page_token ?? null);
      setState('ready');
    } catch (caught) {
      if (current !== generation.current) return;
      setStatus(normalizeApiError(caught).status);
      setState('failed');
    }
  };

  const mark = async (target: ReadMark) => {
    setMarking(true);
    setMarkFailed(false);
    try {
      await markRead(organizationId, target);
    } catch {
      setMarkFailed(true);
    } finally {
      setMarking(false);
    }
  };
  // Through the newest of all, whatever the filters show.
  const markAll = async () => {
    setMarking(true);
    setMarkFailed(false);
    try {
      const newest = (await api.listNotifications(organizationId, { page_size: 1 })).items[0];
      if (newest) await markRead(organizationId, { through: newest });
    } catch {
      setMarkFailed(true);
    } finally {
      setMarking(false);
    }
  };
  const openItem = useOpen(organizationId);

  if (!organizationId) return <EmptyState title="Waiting for organization context" text="Select a confirmed organization to see its notifications." />;
  const filtered = filters.kind !== '' || filters.severity !== '' || filters.read !== 'all';
  return (
    <div className="screen notifications-screen">
      <div className="report-controls" aria-label="Notification filters">
        <div className="audit-filter"><FormField label="Kind" htmlFor="notification-kind">
          <Select id="notification-kind" value={filters.kind} onChange={event => setFilters({ ...filters, kind: event.target.value })}>
            <option value="">All kinds</option>
            {Object.entries(kindLabels).map(([kind, label]) => <option key={kind} value={kind}>{label}</option>)}
          </Select>
        </FormField></div>
        <div className="audit-filter"><FormField label="Severity" htmlFor="notification-severity">
          <Select id="notification-severity" value={filters.severity} onChange={event => setFilters({ ...filters, severity: event.target.value as Filters['severity'] })}>
            <option value="">Any severity</option>
            {(Object.keys(severityLabels) as NotificationSeverity[]).map(severity => <option key={severity} value={severity}>{severityLabels[severity]}</option>)}
          </Select>
        </FormField></div>
        <div className="audit-filter"><FormField label="Read" htmlFor="notification-read">
          <Select id="notification-read" value={filters.read} onChange={event => setFilters({ ...filters, read: event.target.value as Filters['read'] })}>
            <option value="all">Read and unread</option>
            <option value="unread">Unread</option>
            <option value="read">Read</option>
          </Select>
        </FormField></div>
        <Button type="button" variant="ghost" loading={marking} onClick={markAll}>Mark all read</Button>
      </div>
      {markFailed && <Message kind="error">Could not mark notifications read. Try again.</Message>}
      {state === 'loading' && <p className="muted">Loading…</p>}
      {state === 'failed' && (status === 401
        ? <SessionExpired action="see notifications" {...signIn} />
        : status === 403
          ? <Message kind="warning">Your role cannot see this organization’s notifications.</Message>
          : <Message kind="error">Notifications are unavailable right now. <button type="button" className="inline-action" onClick={() => setAttempt(value => value + 1)}>Try again</button></Message>)}
      {(state === 'ready' || state === 'more') && items.length === 0 && (
        <EmptyState
          title={filtered ? 'No matching notifications' : 'No notifications'}
          text={filtered ? 'Nothing matches these filters.' : 'The gateway tells you here when a price source or model listing fails, and when prices change. Notifications are kept 90 days after they resolve.'}
        />
      )}
      {(state === 'ready' || state === 'more') && items.length > 0 && (
        <ul className="notification-table">
          {items.map(item => (
            <li key={item.id} className={`cds--tile notification-row${item.read ? '' : ' unread'}${item.resolved_at ? ' resolved' : ''}`}>
              <div className="notification-row-top">
                <SeverityBadge severity={item.severity} />
                <strong>{!item.read && <span className="notification-dot" aria-hidden="true" />}{item.title}{!item.read && <span className="sr-only"> (unread)</span>}</strong>
                <time className="muted" dateTime={item.created_at} title={item.created_at}>{formatRelativeDate(item.created_at)}</time>
              </div>
              {item.body && <p>{item.body}</p>}
              <p className="muted notification-meta">
                {kindLabel(item.kind)} · <code>{item.subject}</code>
                {item.resolved_at ? <> · resolved <time dateTime={item.resolved_at} title={item.resolved_at}>{formatRelativeDate(item.resolved_at)}</time></> : item.kind !== 'price.adopted' ? ' · open' : null}
              </p>
              <div className="notification-actions">
                {item.link && <button type="button" className="inline-action" onClick={() => openItem(item)}>Open →</button>}
                {!item.read && <button type="button" className="inline-action" disabled={marking} onClick={() => mark({ ids: [item.id] })}>Mark read<span className="sr-only">: {item.title}</span></button>}
              </div>
            </li>
          ))}
        </ul>
      )}
      <LoadMore hasMore={!!token && state !== 'failed'} loading={state === 'more'} onClick={more} />
    </div>
  );
}

/** The Overview's card: the latest five, and a way to all of them. */
export function RecentNotifications({ organizationId, ...signIn }: SignIn & { organizationId: string }) {
  const [items, setItems] = useState<Notification[] | null>(null);
  const [status, setStatus] = useState<number | null>(null);
  const [attempt, setAttempt] = useState(0);
  useEffect(() => {
    let current = true;
    setStatus(null);
    api.listNotifications(organizationId, { page_size: 5 }).then(
      page => { if (current) setItems(page.items); },
      caught => { if (current) setStatus(normalizeApiError(caught).status); },
    );
    return () => { current = false; };
  }, [organizationId, attempt]);
  useReadMarks(organizationId, mark => setItems(previous => previous && applyRead(previous, mark)));
  const openItem = useOpen(organizationId);
  return (
    <article className="cds--tile card overview-card wide recent-notifications" aria-busy={items === null && status === null}>
      <div className="card-top"><span>Recent notifications</span><AppLink to={PAGE}>Notifications →</AppLink></div>
      {status !== null && (status === 401
        ? <SessionExpired action="see this" {...signIn} />
        : status === 403
          ? <p className="muted">Your role cannot see this.</p>
          : <p className="muted">Unavailable right now. <button type="button" className="inline-action" onClick={() => setAttempt(value => value + 1)}>Try again</button></p>)}
      {status === null && items === null && <p className="muted">Loading…</p>}
      {status === null && items?.length === 0 && <p>Nothing to report.</p>}
      {status === null && !!items?.length && <ul className="notification-list">{items.map(item => <NotificationLine key={item.id} item={item} onOpen={openItem} />)}</ul>}
    </article>
  );
}
