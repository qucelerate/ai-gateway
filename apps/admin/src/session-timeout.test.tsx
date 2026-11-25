import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ACTIVITY_KEY, IDLE_TIMEOUT_MS, WARNING_MS, timeoutState } from './session-timeout';
import { SessionTimeoutNotice } from './session-timeout-notice';

describe('timeoutState', () => {
  const start = 1_000_000_000_000;

  it('warns in the last fifteen seconds before the idle limit, then ends', () => {
    expect(timeoutState(start + IDLE_TIMEOUT_MS - WARNING_MS - 1, start, null)).toEqual({ state: 'active' });
    expect(timeoutState(start + IDLE_TIMEOUT_MS - WARNING_MS, start, null)).toEqual({ state: 'warning', kind: 'idle', secondsLeft: 15 });
    expect(timeoutState(start + IDLE_TIMEOUT_MS - 500, start, null)).toEqual({ state: 'warning', kind: 'idle', secondsLeft: 1 });
    expect(timeoutState(start + IDLE_TIMEOUT_MS, start, null)).toEqual({ state: 'ended', kind: 'idle' });
  });

  it('follows the session end when it comes first', () => {
    const expiresAt = (start + 60_000) / 1000;
    expect(timeoutState(start + 50_000, start, expiresAt)).toEqual({ state: 'warning', kind: 'expiry', secondsLeft: 10 });
    expect(timeoutState(start + 60_000, start, expiresAt)).toEqual({ state: 'ended', kind: 'expiry' });
  });
});

describe('SessionTimeoutNotice', () => {
  let container: HTMLDivElement;
  let root: Root;
  let clock: number;
  const now = () => clock;
  const text = () => document.body.textContent ?? '';
  const button = (label: string) => Array.from(document.querySelectorAll('button')).find(candidate => candidate.textContent === label)!;
  const advance = (ms: number) => act(() => {
    clock += ms;
    vi.advanceTimersByTime(ms);
  });

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    vi.useFakeTimers();
    clock = 1_000_000_000_000;
    localStorage.clear();
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.useRealTimers();
  });

  it('asks before signing out an idle session, and staying keeps it', async () => {
    const onStay = vi.fn(async () => {});
    const onEnd = vi.fn();
    act(() => root.render(<SessionTimeoutNotice expiresAt={null} onStay={onStay} onEnd={onEnd} now={now} />));
    advance(IDLE_TIMEOUT_MS - WARNING_MS);
    expect(text()).toContain('Are you still there?');
    expect(text()).toContain('signed out in 15 seconds');
    // Moving the mouse does not answer it.
    act(() => { dispatchEvent(new Event('pointerdown')); });
    advance(5_000);
    expect(text()).toContain('signed out in 10 seconds');

    await act(async () => { button('Stay signed in').click(); });
    expect(onStay).toHaveBeenCalledTimes(1);
    expect(text()).not.toContain('Are you still there?');
    expect(Number(localStorage.getItem(ACTIVITY_KEY))).toBe(clock);
    advance(WARNING_MS);
    expect(onEnd).not.toHaveBeenCalled();
  });

  it('signs out when the warning runs out, or at once when asked', () => {
    const onEnd = vi.fn();
    act(() => root.render(<SessionTimeoutNotice expiresAt={null} onStay={async () => {}} onEnd={onEnd} now={now} />));
    advance(IDLE_TIMEOUT_MS - WARNING_MS);
    advance(WARNING_MS);
    expect(onEnd).toHaveBeenCalledWith('idle', false);
    expect(onEnd).toHaveBeenCalledTimes(1);
    advance(5_000);
    expect(onEnd).toHaveBeenCalledTimes(1);
  });

  it('shares the time it opened, so every tab keeps one deadline', () => {
    localStorage.setItem(ACTIVITY_KEY, String(clock - 60_000));
    act(() => root.render(<SessionTimeoutNotice expiresAt={null} onStay={async () => {}} onEnd={vi.fn()} now={now} />));
    expect(Number(localStorage.getItem(ACTIVITY_KEY))).toBe(clock);
  });

  it('starts afresh for a new session after ending', () => {
    const onEnd = vi.fn();
    const props = { onStay: async () => {}, onEnd, now };
    act(() => root.render(<SessionTimeoutNotice expiresAt={(clock + 20_000) / 1000} {...props} />));
    advance(20_000);
    expect(onEnd).toHaveBeenCalledTimes(1);
    // Another tab signed in again: the session it found ends later.
    act(() => root.render(<SessionTimeoutNotice expiresAt={(clock + 3_600_000) / 1000} {...props} />));
    advance(5_000);
    expect(onEnd).toHaveBeenCalledTimes(1);
    expect(text()).not.toContain('Your session ends');
  });

  it('counts activity in another tab', () => {
    const onEnd = vi.fn();
    act(() => root.render(<SessionTimeoutNotice expiresAt={null} onStay={async () => {}} onEnd={onEnd} now={now} />));
    advance(IDLE_TIMEOUT_MS - WARNING_MS - 1_000);
    localStorage.setItem(ACTIVITY_KEY, String(clock));
    advance(2_000);
    expect(text()).not.toContain('Are you still there?');
    expect(onEnd).not.toHaveBeenCalled();
  });

  it('warns before the session ends; dismissed, it still ends then', () => {
    const onEnd = vi.fn();
    const expiresAt = (clock + 60_000) / 1000;
    act(() => root.render(<SessionTimeoutNotice expiresAt={expiresAt} onStay={async () => {}} onEnd={onEnd} now={now} />));
    advance(45_000);
    expect(text()).toContain('Your session ends in 15 seconds');
    act(() => button('Dismiss').click());
    advance(1_000);
    expect(text()).not.toContain('Your session ends');
    advance(14_000);
    expect(onEnd).toHaveBeenCalledWith('expiry', false);
  });

  it('signs in again from the session warning', () => {
    const onEnd = vi.fn();
    act(() => root.render(<SessionTimeoutNotice expiresAt={(clock + 20_000) / 1000} onStay={async () => {}} onEnd={onEnd} now={now} />));
    advance(6_000);
    act(() => button('Sign in again').click());
    expect(onEnd).toHaveBeenCalledWith('expiry', true);
  });
});

describe('onSessionEnded', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('hears a request that finds the session gone, not a wrong password', async () => {
    const { api, onSessionEnded } = await import('./api');
    const listener = vi.fn();
    const unsubscribe = onSessionEnded(listener);
    const reply = (code: string) => new Response(JSON.stringify({ error: code }), { status: 401, headers: { 'Content-Type': 'application/json' } });
    vi.stubGlobal('fetch', vi.fn(async () => reply('invalid_credentials')));
    await expect(api.me()).rejects.toBeTruthy();
    expect(listener).not.toHaveBeenCalled();
    vi.stubGlobal('fetch', vi.fn(async () => reply('authentication_required')));
    await expect(api.me()).rejects.toBeTruthy();
    expect(listener).toHaveBeenCalledTimes(1);
    unsubscribe();
    await expect(api.me()).rejects.toBeTruthy();
    expect(listener).toHaveBeenCalledTimes(1);
  });
});
