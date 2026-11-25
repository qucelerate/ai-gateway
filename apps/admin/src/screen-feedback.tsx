import React from 'react';
import type { ApiProblem } from './api';
import { InlineError, Message } from './ui';

// Session-expiry and load-failure feedback shared by the reporting and catalog screens.

export type SignIn = { onSignIn?: () => void; signInError?: string; signInPending?: boolean };

export function SessionExpired({ action, onSignIn, signInError, signInPending }: SignIn & { action: string }) {
  const errorId = React.useId();
  return (
    <Message kind="error">
      Your admin session has expired. <button type="button" className="inline-action" onClick={onSignIn} disabled={signInPending} aria-describedby={signInError ? errorId : undefined}>{signInPending ? 'Starting sign in…' : 'Sign in again'}</button> to {action}.
      {signInError && <span id={errorId} className="field-error" role="status" aria-live="polite"> {signInError}</span>}
    </Message>
  );
}

export function ReportError({ problem, what, onRetry, ...signIn }: SignIn & { problem: ApiProblem; what: string; onRetry: () => void }) {
  if (problem.status === 401) return <SessionExpired action={`view ${what}`} {...signIn} />;
  if (problem.status === 403) return <Message kind="error">You do not have permission to view this organization’s {what}.</Message>;
  return <Message kind="error">The {what} could not be loaded. <button className="inline-action" onClick={onRetry}>Try again</button></Message>;
}

/** Why a change was refused: an expired session, a missing permission, or the server's problem. */
export function ActionError({ problem, action, ...signIn }: SignIn & { problem: ApiProblem | null; action: string }) {
  if (!problem) return null;
  if (problem.status === 401) return <SessionExpired action={action} {...signIn} />;
  if (problem.status === 403) return <Message kind="error">Only organization owners and admins can {action}.</Message>;
  if (problem.code === 'price_override_incompatible') {
    return (
      <Message kind="error">
        A live price override sets a price this release’s provider does not bill: {problem.detail} Revoke that override under Pricing, or wait until it ends,
        then try again.
      </Message>
    );
  }
  return <InlineError problem={problem} />;
}

export function loadFailed(detail: string): ApiProblem {
  return { type: 'about:blank', code: 'request_failed', title: 'Request failed', status: 0, detail, instance: '', request_id: '' };
}
