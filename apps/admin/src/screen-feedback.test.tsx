import React from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { describe, expect, it } from 'vitest';
import { ActionError, SessionExpired } from './screen-feedback';

describe('screen feedback', () => {
  it('never submits the form it sits in when signing in again', () => {
    const markup = renderToStaticMarkup(<form><SessionExpired action="save" onSignIn={() => undefined} /></form>);
    expect(markup).toContain('type="button"');
  });

  it('explains a refused change by its cause', () => {
    const problem = (status: number) => ({ type: '', code: 'x', title: '', status, detail: 'detail', instance: '', request_id: '' });
    expect(renderToStaticMarkup(<ActionError problem={problem(401)} action="save" />)).toContain('session has expired');
    expect(renderToStaticMarkup(<ActionError problem={problem(403)} action="save" />)).toContain('Only organization owners and admins can save.');
    expect(renderToStaticMarkup(<ActionError problem={problem(409)} action="save" />)).toContain('Detail.');
  });

  it('says how to clear a price override that blocks a release', () => {
    const problem = { type: '', code: 'price_override_incompatible', title: '', status: 409, detail: 'override o-1 sets a reasoning price', instance: '', request_id: '' };
    const markup = renderToStaticMarkup(<ActionError problem={problem} action="activate catalog releases" />);
    expect(markup).toContain('override o-1 sets a reasoning price');
    expect(markup).toContain('Revoke that override under Pricing');
  });
});
