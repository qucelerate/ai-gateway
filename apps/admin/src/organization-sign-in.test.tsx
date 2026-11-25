import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { beginSignIn } from './api';
import { OrganizationSignIn, rememberedOrganization } from './organization-sign-in';

describe('sign in to your organization', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    localStorage.clear();
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  const field = () => document.getElementById('sign-in-organization') as HTMLInputElement;
  const type = (value: string) => {
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(field(), value);
      field().dispatchEvent(new Event('input', { bubbles: true }));
    });
  };
  const submit = () => act(() => {
    (container.querySelector('button[type="submit"]') as HTMLButtonElement).click();
  });

  it('signs in to the named organization and remembers it on this browser', () => {
    const onSignIn = vi.fn();
    act(() => root.render(<OrganizationSignIn pending={false} onSignIn={onSignIn} />));
    expect(container.querySelector('form')?.getAttribute('aria-label')).toBe('Sign in to your organization');
    expect(container.querySelector('label')?.getAttribute('for')).toBe('sign-in-organization');
    type('  acme ');
    submit();
    expect(onSignIn).toHaveBeenCalledWith('acme');
    expect(rememberedOrganization()).toBe('acme');

    act(() => root.unmount());
    root = createRoot(container);
    act(() => root.render(<OrganizationSignIn pending={false} onSignIn={onSignIn} />));
    expect(field().value).toBe('acme');
  });

  it('cannot be sent twice while a sign-in is starting', () => {
    act(() => root.render(<OrganizationSignIn pending onSignIn={vi.fn()} />));
    expect((container.querySelector('button[type="submit"]') as HTMLButtonElement).disabled).toBe(true);
  });

  it('asks the gateway to start a sign-in for the organization', async () => {
    const navigate = vi.fn();
    const fetcher = vi.fn().mockImplementation(async () =>
      new Response(JSON.stringify({ authorization_url: 'https://idp.acme.example/authorize?state=s' }), { status: 200 }),
    );
    await beginSignIn(navigate, fetcher, ' acme corp ');
    expect(fetcher).toHaveBeenCalledWith(
      '/api/admin/v1/oidc/start?organization=acme%20corp',
      expect.objectContaining({ credentials: 'same-origin' }),
    );
    expect(navigate).toHaveBeenCalledWith('https://idp.acme.example/authorize?state=s');
    // Without one, the gateway's own provider, as before.
    await beginSignIn(navigate, fetcher, '');
    expect(fetcher).toHaveBeenLastCalledWith('/api/admin/v1/oidc/start', expect.anything());
  });
});
