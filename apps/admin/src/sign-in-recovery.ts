import { normalizeApiError, type NormalizedError } from './api';

export type SignInRecoveryResult =
  | { ok: true; error: null }
  | { ok: false; error: NormalizedError };

export async function recoverSignIn(start: () => Promise<void>): Promise<SignInRecoveryResult> {
  try {
    await start();
    return { ok: true, error: null };
  } catch (caught) {
    return { ok: false, error: normalizeApiError(caught) };
  }
}

export function signInRecoveryMessage(code: string) {
  if (code === 'network_error') return 'The sign-in service could not be reached. Check your connection and try again.';
  if (code === 'authorization_url_missing') return 'The sign-in service returned an incomplete response. Try again or contact an operator.';
  return `Sign-in could not be started (${code}). Try again or contact an operator.`;
}
