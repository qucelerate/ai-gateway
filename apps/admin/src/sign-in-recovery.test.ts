import { describe, expect, it } from 'vitest';
import { recoverSignIn, signInRecoveryMessage } from './sign-in-recovery';

describe('sign-in recovery', () => {
  it('turns a failed OIDC start into actionable recovery state', async () => {
    const result = await recoverSignIn(async () => { throw new Error('offline'); });
    expect(result).toEqual({ ok: false, error: { status: 0, code: 'network_error' } });
    if (result.ok) throw new Error('expected sign-in recovery to fail');
    expect(signInRecoveryMessage(result.error.code)).toContain('try again');
  });

  it('reports a successful start without an error', async () => {
    await expect(recoverSignIn(async () => undefined)).resolves.toEqual({ ok: true, error: null });
  });
});
