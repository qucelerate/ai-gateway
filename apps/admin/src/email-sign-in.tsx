import React, { useState } from 'react';
import { type ApiProblem } from './api';
import { PasswordSignIn, toProblem } from './local-accounts';
import { localAccounts, type SignInOptions } from './local-accounts-api';
import { OrganizationSignIn } from './organization-sign-in';
import { Button, FormField, InlineError, TextInput } from './ui';

const REMEMBERED = 'gateway.sign-in.email';

function rememberedEmail(): string {
  try {
    return localStorage.getItem(REMEMBERED) ?? '';
  } catch {
    return '';
  }
}

function remember(email: string) {
  try {
    localStorage.setItem(REMEMBERED, email);
  } catch {
    /* storage unavailable */
  }
}

/**
 * Email first: the person enters their email address and the gateway says
 * where it signs in: their organization's identity provider (for a domain
 * the organization verified), else the gateway's own, else a password, and
 * whether a password would also do. The organization's short name stays as
 * a fallback.
 */
export function EmailSignIn({
  options,
  pending,
  onSignInWith,
  navigate = url => location.assign(url),
  onSignedIn = () => location.reload(),
}: {
  options: SignInOptions;
  /** A sign-in by short name, or through the gateway's provider, is starting. */
  pending: boolean;
  /** Starts the short-name (or, without one, the gateway's own) sign-in. */
  onSignInWith: (organization?: string) => void;
  navigate?: (url: string) => void;
  onSignedIn?: () => void;
}) {
  const [email, setEmail] = useState(rememberedEmail);
  const [step, setStep] = useState<'email' | 'password'>('email');
  const [starting, setStarting] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const [byName, setByName] = useState(false);
  /** The organization's provider, when a password would also do. */
  const [choice, setChoice] = useState<string | null>(null);

  const start = async () => {
    const address = email.trim();
    setStarting(true);
    setProblem(null);
    try {
      const next = await localAccounts.startSignIn(address);
      remember(address);
      if (next.next === 'redirect' && next.password) {
        setChoice(next.authorization_url);
      } else if (next.next === 'redirect') {
        navigate(next.authorization_url);
        return;
      } else {
        setStep('password');
      }
    } catch (caught) {
      setProblem(toProblem(caught));
    }
    setStarting(false);
  };

  if (step === 'password') {
    return <PasswordSignIn initialEmail={email.trim()} onSignedIn={onSignedIn} onBack={() => { setChoice(null); setStep('email'); }} />;
  }
  if (choice) {
    return (
      <div className="password-sign-in" role="group" aria-label="Choose how to sign in">
        <p>This address signs in with single sign-on. If you have a password on this gateway, you can use it instead.</p>
        <Button type="button" onClick={() => navigate(choice)}>Continue with single sign-on</Button>
        <Button type="button" variant="ghost" onClick={() => setStep('password')}>Use a password instead</Button>
        <p className="sign-in-alternative">
          <button type="button" className="inline-action" onClick={() => setChoice(null)}>Use a different email address</button>
        </p>
      </div>
    );
  }
  return <>
    <form
      className="password-sign-in"
      aria-label="Sign in with your email address"
      onSubmit={event => { event.preventDefault(); void start(); }}
    >
      <FormField label="Email address" htmlFor="sign-in-start-email">
        <TextInput
          id="sign-in-start-email"
          type="email"
          value={email}
          onChange={event => setEmail(event.target.value)}
          autoComplete="username"
          autoCapitalize="none"
          spellCheck={false}
          autoFocus
          required
        />
      </FormField>
      <InlineError problem={problem} />
      <Button type="submit" loading={starting}>Continue</Button>
    </form>
    {options.password && (
      <p className="sign-in-alternative">
        {/* A verified domain may send its addresses to the organization's
            provider, but the same address can have a password in another
            organization; password sign-in checks each organization's rules. */}
        <button type="button" className="inline-action" onClick={() => { setProblem(null); setStep('password'); }}>
          Sign in with a password
        </button>
      </p>
    )}
    <p className="sign-in-alternative">
      {byName ? null : (
        <button type="button" className="inline-action" onClick={() => setByName(true)}>
          Sign in with your organization’s short name instead
        </button>
      )}
    </p>
    {byName && <OrganizationSignIn pending={pending} onSignIn={organization => onSignInWith(organization)} />}
    {byName && options.oidc && (
      <p className="sign-in-alternative">
        <button type="button" className="inline-action" disabled={pending} onClick={() => onSignInWith()}>
          {pending ? 'Starting sign in…' : 'Sign in without an organization name'}
        </button>
      </p>
    )}
  </>;
}
