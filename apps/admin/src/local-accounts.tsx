import React, { useEffect, useState } from 'react';
import { ApiProblemError, normalizeApiError, type ApiProblem } from './api';
import { fragmentToken, invitationSsoUrl, localAccounts, slugFrom, type InvitationOffer, type ResetLink, type SignInPolicy } from './local-accounts-api';
import { Button, CheckOption, CopyButton, Dialog, FormField, InlineError, Message, RadioOption, TextInput, buttonClass, formatDate } from './ui';

/** Any failure as a problem the page can show. */
export function toProblem(caught: unknown): ApiProblem {
  if (caught instanceof ApiProblemError) return caught.problem;
  const { status, code } = normalizeApiError(caught);
  return {
    type: 'about:blank',
    code,
    title: code,
    status,
    detail: status === 0 ? 'The gateway could not be reached. Check your connection and try again' : `The request failed (${code})`,
    instance: '',
    request_id: '',
  };
}

const MIN_PASSWORD = 12;
const passwordHint = `At least ${MIN_PASSWORD} characters. Not a common password, and not your email address.`;

/** A new password, typed twice. Returns the error for the second field. */
function mismatch(password: string, confirm: string): string | undefined {
  return confirm && password !== confirm ? 'The passwords do not match.' : undefined;
}

/** The card the link pages (setup, invitation, reset) are drawn on. */
function LinkPage({ title, text, children }: { title: string; text?: React.ReactNode; children?: React.ReactNode }) {
  useEffect(() => {
    document.title = `${title} · Gateway`;
  }, [title]);
  return (
    <main className="state-page">
      <div className="cds--tile state-card link-page">
        <span className="mark large">G</span>
        <p className="eyebrow">Gateway</p>
        <h1>{title}</h1>
        {text && <p>{text}</p>}
        {children}
      </div>
    </main>
  );
}

function Checking() {
  return <LinkPage title="Checking your link"><p role="status">Checking the link…</p></LinkPage>;
}

/** Email address and password, for a local account. */
export function PasswordSignIn({ onSignedIn, initialEmail = '', onBack }: { onSignedIn: () => void; initialEmail?: string; onBack?: () => void }) {
  const [email, setEmail] = useState(initialEmail);
  const [password, setPassword] = useState('');
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const submit = async () => {
    setPending(true);
    setProblem(null);
    try {
      await localAccounts.signIn(email.trim(), password);
      onSignedIn();
    } catch (caught) {
      setProblem(toProblem(caught));
      setPassword('');
    } finally {
      setPending(false);
    }
  };
  return (
    <form
      className="password-sign-in"
      aria-label="Sign in with your password"
      onSubmit={event => { event.preventDefault(); void submit(); }}
    >
      <FormField label="Email address" htmlFor="sign-in-email">
        <TextInput
          id="sign-in-email"
          type="email"
          value={email}
          onChange={event => setEmail(event.target.value)}
          autoComplete="username"
          autoCapitalize="none"
          spellCheck={false}
          required
        />
      </FormField>
      <FormField label="Password" htmlFor="sign-in-password">
        <TextInput
          id="sign-in-password"
          type="password"
          value={password}
          onChange={event => setPassword(event.target.value)}
          autoComplete="current-password"
          autoFocus={!!initialEmail}
          required
        />
      </FormField>
      <InlineError problem={problem} />
      <Button type="submit" loading={pending}>Sign in</Button>
      {onBack && <button type="button" className="inline-action" onClick={onBack}>Use a different email address</button>}
    </form>
  );
}

/** `/admin/setup#token=…`: the first organization and its owner, on a new
 * installation. The link is in the gateway's log. */
export function SetupPage({ token = fragmentToken(location.hash), onDone = () => location.assign('/admin/overview') }: { token?: string; onDone?: () => void }) {
  const [state, setState] = useState<'checking' | 'valid' | 'invalid' | 'error'>(token ? 'checking' : 'invalid');
  const [organizationName, setOrganizationName] = useState('');
  const [slug, setSlug] = useState('');
  const [slugEdited, setSlugEdited] = useState(false);
  const [displayName, setDisplayName] = useState('');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);

  useEffect(() => {
    if (!token) return;
    localAccounts.setupCheck(token)
      .then(result => setState(result.valid ? 'valid' : 'invalid'))
      .catch(caught => {
        const failure = toProblem(caught);
        setState(failure.code === 'setup_already_done' || failure.code === 'setup_link_invalid' ? 'invalid' : 'error');
        setProblem(failure);
      });
  }, [token]);

  if (state === 'checking') return <Checking />;
  if (state === 'invalid') {
    return <LinkPage
      title="This setup link does not work"
      text={problem?.code === 'setup_already_done'
        ? 'This gateway is already set up. Sign in instead.'
        : 'The link was used already, has expired, or is incomplete. Restart the gateway, or run `gateway setup-link`, for a new one.'}
    >
      <a className={buttonClass()} href="/admin">Go to sign in</a>
    </LinkPage>;
  }
  if (state === 'error') {
    return <LinkPage title="The gateway is unavailable" text="The setup link could not be checked. Try again in a moment.">
      <InlineError problem={problem} />
      <Button type="button" onClick={() => location.reload()}>Try again</Button>
    </LinkPage>;
  }

  const confirmError = mismatch(password, confirm);
  const submit = async () => {
    if (password !== confirm) return;
    setPending(true);
    setProblem(null);
    try {
      await localAccounts.setup({
        token,
        organization_name: organizationName.trim(),
        organization_slug: slug,
        email: email.trim(),
        display_name: displayName.trim() || undefined,
        password,
      });
      onDone();
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <LinkPage
      title="Set up Gateway"
      text="Create the first organization and its owner. You sign in with this email address and password; you can connect an identity provider later."
    >
      <form aria-label="Set up Gateway" onSubmit={event => { event.preventDefault(); void submit(); }}>
        <FormField label="Organization name" htmlFor="setup-organization-name">
          <TextInput
            id="setup-organization-name"
            value={organizationName}
            onChange={event => {
              setOrganizationName(event.target.value);
              if (!slugEdited) setSlug(slugFrom(event.target.value));
            }}
            autoComplete="organization"
            required
          />
        </FormField>
        <FormField label="Short name" htmlFor="setup-organization-slug" hint="Lowercase letters, digits and hyphens. People can use it to find the organization's sign-in.">
          <TextInput
            id="setup-organization-slug"
            value={slug}
            onChange={event => { setSlugEdited(true); setSlug(event.target.value.toLowerCase()); }}
            pattern="[a-z0-9][a-z0-9\-]{0,63}"
            autoCapitalize="none"
            spellCheck={false}
            required
          />
        </FormField>
        <FormField label="Your name" htmlFor="setup-display-name">
          <TextInput id="setup-display-name" value={displayName} onChange={event => setDisplayName(event.target.value)} autoComplete="name" />
        </FormField>
        <FormField label="Email address" htmlFor="setup-email">
          <TextInput
            id="setup-email"
            type="email"
            value={email}
            onChange={event => setEmail(event.target.value)}
            autoComplete="username"
            autoCapitalize="none"
            spellCheck={false}
            required
          />
        </FormField>
        <NewPassword idPrefix="setup" password={password} confirm={confirm} onPassword={setPassword} onConfirm={setConfirm} confirmError={confirmError} />
        <InlineError problem={problem} />
        <Button type="submit" loading={pending} disabled={!!confirmError}>Create organization</Button>
      </form>
    </LinkPage>
  );
}

function NewPassword({ idPrefix, password, confirm, onPassword, onConfirm, confirmError, label = 'Password' }: {
  idPrefix: string;
  password: string;
  confirm: string;
  onPassword: (value: string) => void;
  onConfirm: (value: string) => void;
  confirmError?: string;
  label?: string;
}) {
  return <>
    <FormField label={label} htmlFor={`${idPrefix}-password`} hint={passwordHint}>
      <TextInput
        id={`${idPrefix}-password`}
        type="password"
        value={password}
        onChange={event => onPassword(event.target.value)}
        autoComplete="new-password"
        minLength={MIN_PASSWORD}
        required
      />
    </FormField>
    <FormField label={`Confirm ${label.toLowerCase()}`} htmlFor={`${idPrefix}-password-confirm`} error={confirmError}>
      <TextInput
        id={`${idPrefix}-password-confirm`}
        type="password"
        value={confirm}
        onChange={event => onConfirm(event.target.value)}
        autoComplete="new-password"
        required
      />
    </FormField>
  </>;
}

/** `/admin/accept-invitation#token=…`: joins an organization with an email
 * address and password, or through its identity provider. */
export function AcceptInvitationPage({ token = fragmentToken(location.hash), onDone = () => location.assign('/admin/overview') }: { token?: string; onDone?: () => void }) {
  const [offer, setOffer] = useState<InvitationOffer | null>(null);
  const [state, setState] = useState<'checking' | 'ready' | 'invalid' | 'error'>(token ? 'checking' : 'invalid');
  const [account, setAccount] = useState<'new' | 'existing'>('new');
  const [email, setEmail] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);

  useEffect(() => {
    if (!token) return;
    localAccounts.inspectInvitation(token)
      .then(value => { setOffer(value); setState('ready'); })
      .catch(caught => {
        const failure = toProblem(caught);
        setState(failure.code === 'invitation_invalid' ? 'invalid' : 'error');
        setProblem(failure);
      });
  }, [token]);

  if (state === 'checking') return <Checking />;
  if (state === 'invalid' || !offer) {
    return <LinkPage
      title={state === 'error' ? 'The gateway is unavailable' : 'This invitation does not work'}
      text={state === 'error'
        ? 'The invitation could not be checked. Try again in a moment.'
        : 'It was used already, has expired, or is incomplete. Ask whoever invited you for a new one.'}
    >
      {state === 'error' && <InlineError problem={problem} />}
      <a className={buttonClass()} href="/admin">Go to sign in</a>
    </LinkPage>;
  }

  const confirmError = account === 'new' ? mismatch(password, confirm) : undefined;
  const submit = async () => {
    if (account === 'new' && password !== confirm) return;
    setPending(true);
    setProblem(null);
    try {
      await localAccounts.acceptInvitation({
        token,
        email: email.trim(),
        password,
        display_name: account === 'new' ? displayName.trim() || undefined : undefined,
      });
      onDone();
    } catch (caught) {
      const failure = toProblem(caught);
      if (failure.code === 'account_exists') setAccount('existing');
      setProblem(failure);
    } finally {
      setPending(false);
    }
  };
  const role = offer.role.toLowerCase();
  return (
    <LinkPage
      title={`Join ${offer.organization_name}`}
      text={`You are invited as ${/^[aeiou]/.test(role) ? 'an' : 'a'} ${role}.`}
    >
      {offer.sso && (
        <a className={buttonClass(offer.password ? 'ghost' : 'primary')} href={invitationSsoUrl(token)}>
          Accept with your identity provider
        </a>
      )}
      {offer.password && (
        <form aria-label="Accept with a password" onSubmit={event => { event.preventDefault(); void submit(); }}>
          <fieldset className="account-choice">
            <legend className="cds--label">Account</legend>
            <RadioOption id="invite-account-new" name="invite-account" value="new" checked={account === 'new'} onChange={() => setAccount('new')}>I am new here</RadioOption>
            <RadioOption id="invite-account-existing" name="invite-account" value="existing" checked={account === 'existing'} onChange={() => setAccount('existing')}>I already have a password on this gateway</RadioOption>
          </fieldset>
          <FormField label="Email address" htmlFor="invite-email">
            <TextInput
              id="invite-email"
              type="email"
              value={email}
              onChange={event => setEmail(event.target.value)}
              autoComplete="username"
              autoCapitalize="none"
              spellCheck={false}
              required
            />
          </FormField>
          {account === 'new' ? <>
            <FormField label="Your name" htmlFor="invite-display-name">
              <TextInput id="invite-display-name" value={displayName} onChange={event => setDisplayName(event.target.value)} autoComplete="name" />
            </FormField>
            <NewPassword idPrefix="invite" password={password} confirm={confirm} onPassword={setPassword} onConfirm={setConfirm} confirmError={confirmError} />
          </> : (
            <FormField label="Password" htmlFor="invite-current-password">
              <TextInput
                id="invite-current-password"
                type="password"
                value={password}
                onChange={event => setPassword(event.target.value)}
                autoComplete="current-password"
                required
              />
            </FormField>
          )}
          <InlineError problem={problem} />
          <Button type="submit" loading={pending} disabled={!!confirmError}>Join {offer.organization_name}</Button>
        </form>
      )}
      {!offer.password && !offer.sso && (
        <Message kind="warning">This invitation cannot be accepted here. Ask whoever invited you how to sign in.</Message>
      )}
    </LinkPage>
  );
}

/** `/admin/reset-password#token=…`: a new password from a reset link an
 * owner or the operator made. */
export function ResetPasswordPage({ token = fragmentToken(location.hash) }: { token?: string }) {
  const [state, setState] = useState<'checking' | 'valid' | 'invalid' | 'error' | 'done'>(token ? 'checking' : 'invalid');
  const [email, setEmail] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);

  useEffect(() => {
    if (!token) return;
    localAccounts.resetCheck(token)
      .then(result => { setEmail(result.email ?? ''); setState(result.valid ? 'valid' : 'invalid'); })
      .catch(caught => {
        const failure = toProblem(caught);
        setState(failure.code === 'reset_link_invalid' ? 'invalid' : 'error');
        setProblem(failure);
      });
  }, [token]);

  if (state === 'checking') return <Checking />;
  if (state === 'done') {
    return <LinkPage title="Your password is changed" text="Every session of your account has ended. Sign in with your new password.">
      <a className={buttonClass()} href="/admin">Sign in</a>
    </LinkPage>;
  }
  if (state === 'invalid' || state === 'error') {
    return <LinkPage
      title={state === 'error' ? 'The gateway is unavailable' : 'This reset link does not work'}
      text={state === 'error' ? 'The link could not be checked. Try again in a moment.' : 'It was used already, has expired, or is incomplete. Ask an owner of your organization for a new one.'}
    >
      {state === 'error' && <InlineError problem={problem} />}
      <a className={buttonClass()} href="/admin">Go to sign in</a>
    </LinkPage>;
  }

  const confirmError = mismatch(password, confirm);
  const submit = async () => {
    if (password !== confirm) return;
    setPending(true);
    setProblem(null);
    try {
      await localAccounts.reset(token, password);
      setState('done');
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <LinkPage title="Choose a new password" text={email ? <>For <strong>{email}</strong>.</> : undefined}>
      <form aria-label="Choose a new password" onSubmit={event => { event.preventDefault(); void submit(); }}>
        {/* For password managers: the account this password is for. */}
        <input type="email" value={email} autoComplete="username" readOnly hidden />
        <NewPassword idPrefix="reset" label="New password" password={password} confirm={confirm} onPassword={setPassword} onConfirm={setConfirm} confirmError={confirmError} />
        <InlineError problem={problem} />
        <Button type="submit" loading={pending} disabled={!!confirmError}>Set password</Button>
      </form>
    </LinkPage>
  );
}

/** The page a link path opens, outside the signed-in app; null for any
 * other path. */
export function linkPage(pathname: string): React.ReactElement | null {
  const page = pathname.split('/').filter(Boolean)[1];
  if (page === 'setup') return <SetupPage />;
  if (page === 'accept-invitation') return <AcceptInvitationPage />;
  if (page === 'reset-password') return <ResetPasswordPage />;
  return null;
}

/** The account menu's "Change password", for a password session. Other
 * sessions of the account end; this one stays. */
export function ChangePasswordDialog({ open, onClose, email }: { open: boolean; onClose: () => void; email: string | null }) {
  const [current, setCurrent] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [pending, setPending] = useState(false);
  const [done, setDone] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const close = () => {
    if (pending) return;
    setCurrent(''); setPassword(''); setConfirm(''); setProblem(null); setDone(false);
    onClose();
  };
  const confirmError = mismatch(password, confirm);
  const submit = async () => {
    if (password !== confirm) return;
    setPending(true);
    setProblem(null);
    try {
      await localAccounts.changePassword(current, password);
      setDone(true);
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <Dialog open={open} onClose={close} title="Change password">
      {done ? <>
        <Message kind="success" title="Your password is changed.">Your other sessions have ended; this one stays signed in.</Message>
        <div className="dialog-actions"><Button type="button" onClick={close}>Done</Button></div>
      </> : (
        <form onSubmit={event => { event.preventDefault(); void submit(); }}>
          <input type="email" value={email ?? ''} autoComplete="username" readOnly hidden />
          <FormField label="Current password" htmlFor="change-current-password">
            <TextInput id="change-current-password" type="password" value={current} onChange={event => setCurrent(event.target.value)} autoComplete="current-password" required />
          </FormField>
          <NewPassword idPrefix="change" label="New password" password={password} confirm={confirm} onPassword={setPassword} onConfirm={setConfirm} confirmError={confirmError} />
          <InlineError problem={problem} />
          <div className="dialog-actions">
            <Button variant="ghost" type="button" onClick={close} disabled={pending}>Cancel</Button>
            <Button type="submit" loading={pending} disabled={!!confirmError}>Change password</Button>
          </div>
        </form>
      )}
    </Dialog>
  );
}

/** People → Members: an owner makes a single-use link that lets a member
 * with a local account choose a new password. */
export function PasswordResetLinkDialog({ organizationId, userId, onClose }: { organizationId: string; userId: string | null; onClose: () => void }) {
  const [link, setLink] = useState<ResetLink | null>(null);
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  const close = () => {
    if (pending) return;
    setLink(null); setProblem(null);
    onClose();
  };
  const create = async () => {
    if (!userId) return;
    setPending(true);
    setProblem(null);
    try {
      setLink(await localAccounts.memberResetLink(organizationId, userId));
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <Dialog open={!!userId} onClose={close} title="Password reset link">
      {link ? <>
        <p>Send this link to the member. It works once, until {formatDate(link.expires_at)}, and is not shown again.</p>
        <div className="secret-value-row">
          <code className="secret-value" aria-label="Password reset link">{link.link}</code>
          <CopyButton text={link.link} label="Copy the reset link" />
        </div>
        <div className="dialog-actions"><Button type="button" onClick={close}>Done</Button></div>
      </> : <>
        <p>
          Make a link that lets this member choose a new password for their local account. Their
          current password keeps working until they use it. The link is shown once; send it to them yourself.
        </p>
        <InlineError problem={problem} />
        <div className="dialog-actions">
          <Button variant="ghost" type="button" onClick={close} disabled={pending}>Cancel</Button>
          <Button type="button" loading={pending} onClick={() => { void create(); }}>Make link</Button>
        </div>
      </>}
    </Dialog>
  );
}

/** Settings → Sign-in: whether members must use the organization's own
 * identity provider (their passwords stop working here). */
export function RequireSsoSetting({ organizationId, canWrite }: { organizationId: string; canWrite: boolean }) {
  const [policy, setPolicy] = useState<SignInPolicy | null>(null);
  const [pending, setPending] = useState(false);
  const [problem, setProblem] = useState<ApiProblem | null>(null);
  useEffect(() => {
    let current = true;
    localAccounts.signInPolicy(organizationId)
      .then(value => { if (current) setPolicy(value); })
      .catch(caught => { if (current) setProblem(toProblem(caught)); });
    return () => { current = false; };
  }, [organizationId]);
  if (!policy) return problem ? <InlineError problem={problem} /> : null;
  if (!policy.local_accounts && !policy.require_sso) return null;
  const change = async (requireSso: boolean) => {
    setPending(true);
    setProblem(null);
    try {
      setPolicy(await localAccounts.setSignInPolicy(organizationId, requireSso));
    } catch (caught) {
      setProblem(toProblem(caught));
    } finally {
      setPending(false);
    }
  };
  return (
    <section className="require-sso" aria-labelledby="require-sso-heading">
      <h3 id="require-sso-heading">Passwords</h3>
      <p>
        {policy.passwords_allowed
          ? 'Members with a local account can sign in to this organization with their email address and password.'
          : 'Members must sign in with the organization’s identity provider. Their passwords do not open this organization.'}
      </p>
      <CheckOption
        id="require-sso"
        checked={policy.require_sso}
        disabled={!canWrite || pending || (!policy.identity_provider_enabled && !policy.require_sso)}
        onChange={event => { void change(event.target.checked); }}
      >
        Require the organization’s identity provider
      </CheckOption>
      {!policy.identity_provider_enabled && (
        <p className="hint">Set up, test and enable the identity provider above before requiring it.</p>
      )}
      <InlineError problem={problem} />
    </section>
  );
}
