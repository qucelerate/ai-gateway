import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import { AuditError } from './audit-screen';

describe('audit sign-in recovery', () => {
  it('shows failed sign-in recovery beside the 401 action accessibly', () => {
    const markup = renderToStaticMarkup(
      <AuditError
        status={401}
        onRetry={() => undefined}
        onSignIn={() => undefined}
        signInError="The sign-in service could not be reached."
      />,
    );
    expect(markup).toContain('The sign-in service could not be reached.');
    expect(markup).toContain('aria-describedby="audit-sign-in-error"');
  });

  it('preserves the 403 response without sign-in recovery UI', () => {
    const markup = renderToStaticMarkup(
      <AuditError status={403} onRetry={() => undefined} signInError="stale sign-in error" />,
    );
    expect(markup).not.toContain('stale sign-in error');
  });
});
