// When the console signs someone out on its own: after a spell without
// activity (in any of its tabs), or when the server's session ends. Each
// gives a short warning first.

/** Without activity for this long, the console signs out. */
export const IDLE_TIMEOUT_MS = 15 * 60 * 1000;
/** The warning shows this long before either ending. */
export const WARNING_MS = 15 * 1000;
/** Activity is recorded at most this often. */
export const ACTIVITY_THROTTLE_MS = 5 * 1000;
/** Shares the last activity between the console's tabs. */
export const ACTIVITY_KEY = 'gateway.admin.lastActivity';

export type TimeoutKind = 'idle' | 'expiry';

export type TimeoutState =
  | { state: 'active' }
  | { state: 'warning'; kind: TimeoutKind; secondsLeft: number }
  | { state: 'ended'; kind: TimeoutKind };

/** Where the timers stand at `now`: the nearer of the idle deadline and the
 * session's end (`expiresAt`, Unix seconds; `null` when unknown). */
export function timeoutState(now: number, lastActivity: number, expiresAt: number | null): TimeoutState {
  const idle = lastActivity + IDLE_TIMEOUT_MS;
  const expiry = expiresAt === null ? Number.POSITIVE_INFINITY : expiresAt * 1000;
  const kind: TimeoutKind = expiry <= idle ? 'expiry' : 'idle';
  const deadline = Math.min(idle, expiry);
  if (now >= deadline) return { state: 'ended', kind };
  if (now >= deadline - WARNING_MS) return { state: 'warning', kind, secondsLeft: Math.ceil((deadline - now) / 1000) };
  return { state: 'active' };
}

/** The last activity another tab recorded, if it is readable. */
export function sharedActivity(): number | null {
  try {
    const value = Number(localStorage.getItem(ACTIVITY_KEY));
    return Number.isFinite(value) && value > 0 ? value : null;
  } catch {
    return null;
  }
}

export function shareActivity(at: number) {
  try {
    localStorage.setItem(ACTIVITY_KEY, String(at));
  } catch {
    // Storage blocked: this tab keeps its own clock.
  }
}
