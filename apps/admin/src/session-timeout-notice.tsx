// The warning before the console signs someone out, and the timers behind
// it. Activity in any tab of the console keeps every tab signed in.

import { useEffect, useRef, useState } from 'react';
import { Button } from '@carbon/react';
import { Dialog } from './ui';
import {
  ACTIVITY_KEY,
  ACTIVITY_THROTTLE_MS,
  shareActivity,
  sharedActivity,
  timeoutState,
  type TimeoutKind,
  type TimeoutState,
} from './session-timeout';

const ACTIVITY_EVENTS = ['pointerdown', 'keydown', 'wheel', 'touchstart'] as const;

export function SessionTimeoutNotice({ expiresAt, onStay, onEnd, now = () => Date.now() }: {
  /** When the server's session ends, Unix seconds; `null` when unknown. */
  expiresAt: number | null;
  /** Staying signed in: resolves once the session is checked. */
  onStay: () => Promise<void>;
  /** Signs out: after the idle warning runs out or the session has ended
   * (`asked` false), or when the person asks to (`asked` true). */
  onEnd: (kind: TimeoutKind, asked: boolean) => void;
  now?: () => number;
}) {
  const lastActivity = useRef(Math.max(now(), sharedActivity() ?? 0));
  // A tab opening is activity every tab counts, so they share one deadline.
  const published = useRef(false);
  if (!published.current) {
    published.current = true;
    shareActivity(lastActivity.current);
  }
  const [timer, setTimer] = useState<TimeoutState>({ state: 'active' });
  const warning = timer.state === 'warning';
  const warningRef = useRef(warning);
  warningRef.current = warning;
  const ended = useRef(false);
  // The session's end cannot be put off: once its warning is dismissed it
  // stays away until the end comes.
  const dismissed = useRef(false);
  const onEndRef = useRef(onEnd);
  onEndRef.current = onEnd;

  const record = (at: number) => {
    lastActivity.current = Math.max(lastActivity.current, at);
    shareActivity(lastActivity.current);
  };

  useEffect(() => {
    let recorded = 0;
    const onActivity = () => {
      // An open warning asks for an answer: activity alone does not dismiss
      // it.
      if (warningRef.current) return;
      const at = now();
      if (at - recorded < ACTIVITY_THROTTLE_MS) return;
      recorded = at;
      record(at);
    };
    const onStorage = (event: StorageEvent) => {
      if (event.key !== ACTIVITY_KEY) return;
      const at = Number(event.newValue);
      if (Number.isFinite(at)) lastActivity.current = Math.max(lastActivity.current, at);
    };
    const tick = () => {
      if (ended.current) return;
      const shared = sharedActivity();
      if (shared) lastActivity.current = Math.max(lastActivity.current, shared);
      let next = timeoutState(now(), lastActivity.current, expiresAt);
      if (next.state === 'warning' && next.kind === 'expiry' && dismissed.current) next = { state: 'active' };
      if (next.state === 'ended') {
        ended.current = true;
        onEndRef.current(next.kind, false);
      }
      setTimer(current => (current.state === next.state
        && (current.state !== 'warning' || (next.state === 'warning' && current.secondsLeft === next.secondsLeft && current.kind === next.kind))
        ? current
        : next));
    };
    for (const name of ACTIVITY_EVENTS) addEventListener(name, onActivity, { passive: true });
    addEventListener('storage', onStorage);
    // A new session (signed in again, perhaps in another tab) starts
    // afresh.
    dismissed.current = false;
    ended.current = false;
    const interval = setInterval(tick, 1000);
    tick();
    return () => {
      for (const name of ACTIVITY_EVENTS) removeEventListener(name, onActivity);
      removeEventListener('storage', onStorage);
      clearInterval(interval);
    };
    // `now` is fixed for a mount; a new `expiresAt` (a fresh session)
    // restarts the timers.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [expiresAt]);

  const [staying, setStaying] = useState(false);
  const stay = async () => {
    setStaying(true);
    record(now());
    setTimer({ state: 'active' });
    try {
      await onStay();
    } catch {
      // The check could not reach the server; the timers go on, and a
      // session found gone signs out on its own.
    } finally {
      setStaying(false);
    }
  };
  const dismiss = () => {
    dismissed.current = true;
    setTimer({ state: 'active' });
  };
  const endNow = (kind: TimeoutKind) => {
    ended.current = true;
    onEnd(kind, true);
  };

  if (timer.state !== 'warning') return null;
  const seconds = `${timer.secondsLeft} ${timer.secondsLeft === 1 ? 'second' : 'seconds'}`;
  if (timer.kind === 'idle') {
    return (
      <Dialog
        open
        alert
        title="Are you still there?"
        describedBy="session-timeout-text"
        onClose={() => void stay()}
        footer={<>
          <Button kind="secondary" onClick={() => endNow('idle')}>Sign out now</Button>
          <Button kind="primary" onClick={() => void stay()} disabled={staying}>Stay signed in</Button>
        </>}
      >
        <p id="session-timeout-text" aria-live="assertive">
          You haven’t used the console for a while. For your security you will be signed out in {seconds}.
        </p>
      </Dialog>
    );
  }
  return (
    <Dialog
      open
      alert
      title="Your session is ending"
      describedBy="session-timeout-text"
      onClose={dismiss}
      footer={<>
        <Button kind="secondary" onClick={dismiss}>Dismiss</Button>
        <Button kind="primary" onClick={() => endNow('expiry')}>Sign in again</Button>
      </>}
    >
      <p id="session-timeout-text" aria-live="assertive">
        Your session ends in {seconds}, and you will be taken to the sign-in page. Sign in again to keep working.
      </p>
    </Dialog>
  );
}
