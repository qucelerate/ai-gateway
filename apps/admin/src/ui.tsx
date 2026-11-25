import React from 'react';
import {
  Button as CarbonButton,
  Checkbox,
  RadioButton,
  TextArea as CarbonTextArea,
  Select as CarbonSelect,
  Table as CarbonTable,
  TableBody,
  TableCell,
  TableContainer,
  TableExpandedRow,
  TableExpandRow,
  TableHead,
  TableHeader,
  TableRow,
  Tag,
  TextInput as CarbonTextInput,
} from '@carbon/react';
import { CheckmarkFilled, ErrorFilled, InformationFilled, WarningFilled } from '@carbon/icons-react';
import type { ApiProblem } from './api';

export { TableBody, TableExpandedRow, TableExpandRow, TableHead, TableHeader, TableRow };

type ButtonProps = React.ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: 'primary' | 'danger' | 'ghost';
  loading?: boolean;
};

/** Carbon's classes for a button look on an element that is not a Button
 * (a link that starts a download or opens another site). */
export function buttonClass(variant: 'primary' | 'ghost' = 'primary') {
  return `cds--btn cds--btn--${variant} cds--btn--md`;
}

/** A Carbon button. `type` stays the browser's (submit inside a form). */
export function Button({
  children,
  variant = 'primary',
  loading = false,
  disabled,
  className = '',
  type = 'submit',
  ...rest
}: ButtonProps) {
  return (
    <CarbonButton
      {...rest}
      type={type}
      kind={variant}
      size="md"
      className={[loading ? 'btn-loading' : '', className].filter(Boolean).join(' ') || undefined}
      disabled={disabled || loading}
      // A caller's own aria-busy counts too.
      aria-busy={loading || rest['aria-busy'] || undefined}
    >
      {children}
    </CarbonButton>
  );
}

export function Dialog({
  open,
  onClose,
  title,
  children,
  alert = false,
  describedBy,
  size = 'sm',
  footer,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  children: React.ReactNode;
  alert?: boolean;
  describedBy?: string;
  /** Carbon's modal width: `lg` for content such as code. */
  size?: 'sm' | 'lg';
  /** Carbon's full-width button row under the content. */
  footer?: React.ReactNode;
}) {
  const dialogRef = React.useRef<HTMLDivElement>(null);
  const previousFocus = React.useRef<Element | null>(null);
  const onCloseRef = React.useRef(onClose);
  onCloseRef.current = onClose;

  React.useEffect(() => {
    if (!open) return;
    previousFocus.current = document.activeElement;
    const dialog = dialogRef.current;
    if (!dialog) return;

    const focusable = () =>
      Array.from(
        dialog.querySelectorAll<HTMLElement>(
          'button, [href], input, select, textarea, [tabindex]:not([tabindex="-1"])',
        ),
      ).filter(el => !('disabled' in el && (el as HTMLInputElement).disabled) && el.offsetParent !== null);

    const first = focusable()[0];
    if (first) first.focus();

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        onCloseRef.current();
        return;
      }
      if (event.key !== 'Tab') return;
      const elements = focusable();
      if (elements.length === 0) return;
      const firstEl = elements[0];
      const lastEl = elements[elements.length - 1];
      if (event.shiftKey && document.activeElement === firstEl) {
        event.preventDefault();
        lastEl.focus();
      } else if (!event.shiftKey && document.activeElement === lastEl) {
        event.preventDefault();
        firstEl.focus();
      }
    };

    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('keydown', onKeyDown);
      const prev = previousFocus.current;
      if (prev && 'focus' in prev && typeof (prev as HTMLElement).focus === 'function') {
        (prev as HTMLElement).focus();
      }
    };
  }, [open]);

  if (!open) return null;
  return (
    // Carbon's modal, drawn with its classes: Carbon's ComposedModal has no
    // alertdialog role, and this one keeps focus and Escape as tested.
    <div className="cds--modal is-visible dialog-backdrop" onClick={onClose} role="presentation">
      <div
        ref={dialogRef}
        className={`cds--modal-container cds--modal-container--${size} dialog`}
        role={alert ? 'alertdialog' : 'dialog'}
        aria-modal="true"
        aria-labelledby="dialog-title"
        aria-describedby={describedBy}
        onClick={event => event.stopPropagation()}
      >
        <div className="cds--modal-header">
          <h2 id="dialog-title" className="cds--modal-header__heading dialog-title">
            {title}
          </h2>
        </div>
        <div className="cds--modal-content cds--modal-content--with-form dialog-content">{children}</div>
        {footer && <div className="cds--modal-footer cds--btn-set dialog-footer">{footer}</div>}
      </div>
    </div>
  );
}

export function ConfirmDialog({
  open,
  onClose,
  onConfirm,
  title,
  children,
  confirmText = 'Confirm',
  danger = true,
  problem,
  submitting = false,
}: {
  open: boolean;
  onClose: () => void;
  onConfirm: () => void;
  title: string;
  children: React.ReactNode;
  confirmText?: string;
  danger?: boolean;
  problem?: ApiProblem | null;
  submitting?: boolean;
}) {
  const descriptionId = React.useId();
  return (
    <Dialog
      open={open}
      onClose={() => { if (!submitting) onClose(); }}
      title={title}
      alert
      describedBy={descriptionId}
    >
      <div className="dialog-body" id={descriptionId}>
        <p>{children}</p>
      </div>
      <InlineError problem={problem} />
      <div className="dialog-actions">
        <Button variant="ghost" onClick={onClose} disabled={submitting}>
          Cancel
        </Button>
        <Button variant={danger ? 'danger' : 'primary'} onClick={onConfirm} loading={submitting}>
          {confirmText}
        </Button>
      </div>
    </Dialog>
  );
}

export function FormField({
  label,
  htmlFor,
  hint,
  error,
  children,
}: {
  label: string;
  htmlFor: string;
  hint?: string;
  error?: string;
  children: React.ReactNode;
}) {
  const errorText = error && <span role="alert">{error}</span>;
  // A TextInput or Select takes the label, hint and error itself, as Carbon's
  // field does; anything else gets them around it.
  if (React.isValidElement(children) && (children.type === TextInput || children.type === Select || children.type === TextArea)) {
    const child = children as React.ReactElement<FieldProps>;
    return (
      <div className="form-field">
        {React.cloneElement(child, { id: child.props.id ?? htmlFor, field: { label, hint, error: errorText } })}
      </div>
    );
  }
  return (
    <div className="form-field cds--form-item">
      <label htmlFor={htmlFor} className="cds--label">{label}</label>
      {children}
      {hint && !error && <div className="cds--form__helper-text">{hint}</div>}
      {/* Not Carbon's .cds--form-requirement: Carbon shows that only beside an
          invalid input, so here it would stay hidden. */}
      {error && <div className="field-error">{errorText}</div>}
    </div>
  );
}

type FieldLabels = { label: string; hint?: string; error?: React.ReactNode };
type FieldProps = { id?: string; field?: FieldLabels };

/** A Carbon text input. In a FormField it shows the field's label, hint and
 * error; alone, name it with aria-label. */
export function TextInput({ field, className, ...props }: React.InputHTMLAttributes<HTMLInputElement> & FieldProps) {
  const ownId = React.useId();
  return (
    <CarbonTextInput
      {...(props as React.ComponentProps<typeof CarbonTextInput>)}
      id={props.id ?? ownId}
      className={className}
      labelText={field?.label ?? props['aria-label'] ?? ''}
      hideLabel={!field}
      helperText={field?.hint}
      invalid={Boolean(field?.error)}
      invalidText={field?.error}
      size="md"
    />
  );
}

/** A Carbon text area, labelled like TextInput. */
export function TextArea({ field, className, ...props }: React.TextareaHTMLAttributes<HTMLTextAreaElement> & FieldProps) {
  const ownId = React.useId();
  return (
    <CarbonTextArea
      {...(props as React.ComponentProps<typeof CarbonTextArea>)}
      id={props.id ?? ownId}
      className={className}
      labelText={field?.label ?? props['aria-label'] ?? ''}
      hideLabel={!field}
      helperText={field?.hint}
      invalid={Boolean(field?.error)}
      invalidText={field?.error}
    />
  );
}

type OptionProps = Omit<React.InputHTMLAttributes<HTMLInputElement>, 'type' | 'children'> & { children: React.ReactNode };

/** A Carbon checkbox whose label is `children`. */
export function CheckOption({ id, children, className, onChange, ...props }: OptionProps) {
  const ownId = React.useId();
  return (
    <Checkbox
      {...(props as Omit<React.ComponentProps<typeof Checkbox>, 'id' | 'labelText'>)}
      id={id ?? ownId}
      className={['check-option', className].filter(Boolean).join(' ')}
      labelText={children ?? ''}
      onChange={event => onChange?.(event)}
    />
  );
}

/** A Carbon radio button whose label is `children`. */
export function RadioOption({ id, children, className, onChange, value, ...props }: OptionProps) {
  const ownId = React.useId();
  return (
    <RadioButton
      {...(props as Omit<React.ComponentProps<typeof RadioButton>, 'labelText'>)}
      id={id ?? ownId}
      value={value === undefined ? ownId : String(value)}
      className={['check-option', className].filter(Boolean).join(' ')}
      labelText={children}
      onChange={(_value, _name, event) => onChange?.(event)}
    />
  );
}

/** A Carbon select, labelled like TextInput. */
export function Select({ field, className, children, ...props }: React.SelectHTMLAttributes<HTMLSelectElement> & FieldProps) {
  const ownId = React.useId();
  return (
    <CarbonSelect
      {...(props as React.ComponentProps<typeof CarbonSelect>)}
      id={props.id ?? ownId}
      className={className}
      labelText={field?.label ?? props['aria-label'] ?? ''}
      hideLabel={!field}
      helperText={field?.hint}
      invalid={Boolean(field?.error)}
      invalidText={field?.error}
      size="md"
    >
      {children}
    </CarbonSelect>
  );
}

type BadgeKind = 'success' | 'danger' | 'warning' | 'neutral';

/** A Carbon tag. The color comes from `kind`, else from a `badge-*` class in
 * `tone`, else from the status word. */
export function Badge({ status, kind, tone, children, title }: {
  status?: string;
  kind?: BadgeKind;
  tone?: string;
  children?: React.ReactNode;
  title?: string;
}) {
  const fromTone = tone && (['success', 'danger', 'warning'] as const).find(name => tone.includes(`badge-${name}`));
  const normalized = (status ?? '').toLowerCase();
  const resolved: BadgeKind = kind ?? (tone !== undefined ? fromTone || 'neutral'
    : normalized === 'active' || normalized === 'created' || normalized === 'accepted' || normalized === 'true'
      ? 'success'
      : normalized === 'disabled' || normalized === 'revoked' || normalized === 'false'
        ? 'danger'
        : 'neutral');
  const type = resolved === 'success' ? 'green' : resolved === 'danger' ? 'red' : resolved === 'warning' ? 'warm-gray' : 'gray';
  // Carbon's Tag keeps `title` for its close button, so a span carries it.
  const tag = (
    <Tag as="span" type={type} size="sm" className={`badge badge-${resolved}`}>
      {children ?? status}
    </Tag>
  );
  return title ? <span title={title}>{tag}</span> : tag;
}

export function MutationStatus({
  result,
  problem,
  onDismiss,
}: {
  result?: { requestId?: string } | null;
  problem?: ApiProblem | null;
  onDismiss?: () => void;
}) {
  if (problem) {
    // Carbon's notification holds no controls, so Dismiss sits beside it.
    return (
      <div className="notice-with-action">
        <Notice kind="error" title={problemMessage(problem)}>
          {problem.code !== 'request_failed' && <span className="problem-ref"> Error code: <code>{problem.code}</code>{problem.request_id ? <>, ref {problem.request_id}</> : null}</span>}
        </Notice>
        {onDismiss && (
          <button type="button" className="inline-action" onClick={onDismiss}>
            Dismiss
          </button>
        )}
      </div>
    );
  }
  if (result?.requestId) {
    return <Notice kind="success" title={`Saved (ref: ${result.requestId})`} />;
  }
  return null;
}

export function EmptyState({ title, text, action }: { title: string; text: string; action?: React.ReactNode }) {
  return (
    <div className="empty" role="status">
      <span className="empty-icon">◇</span>
      <h2>{title}</h2>
      <p>{text}</p>
      {action && <div className="empty-action">{action}</div>}
    </div>
  );
}

/** No active catalog release: nothing can be called until a provider is
 * connected and a model added, which Connect a provider does. `what` says what
 * this view would show. */
export function NoCatalog({ what }: { what: string }) {
  return (
    <EmptyState
      title="No models yet"
      text={`${what} Connect a provider account and add a model from it to start.`}
      action={<AppLink to="/admin/connections/connect" className={buttonClass()}>Connect a provider</AppLink>}
    />
  );
}

export function LoadMore({
  onClick,
  hasMore,
  loading,
}: {
  onClick: () => void;
  hasMore: boolean;
  loading: boolean;
}) {
  if (!hasMore) return null;
  return (
    <div className="load-more">
      <Button variant="ghost" onClick={onClick} loading={loading}>
        Load more
      </Button>
    </div>
  );
}

export function CopyButton({ text, label }: { text: string; label: string }) {
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      /* ignore */
    }
  };
  return (
    <Button variant="ghost" onClick={copy} aria-label={label}>
      Copy
    </Button>
  );
}

export function SecretRevealPanel({
  secret,
  label,
  requestId,
  onDismiss,
  warning = 'This secret is shown only once. Closing this panel removes it from the page. It is not stored in your browser.',
  valueLabel = 'API key secret',
  copyLabel = 'Copy secret to clipboard',
  dismissText = 'I have saved it',
}: {
  secret: string;
  label: string;
  requestId?: string;
  onDismiss: () => void;
  warning?: React.ReactNode;
  valueLabel?: string;
  copyLabel?: string;
  dismissText?: string;
}) {
  return (
    <div className="secret-panel" role="alert">
      <div className="secret-panel-header">
        <strong>{label}</strong>
        {requestId && <span className="secret-ref">ref: {requestId}</span>}
      </div>
      <p className="secret-warning">{warning}</p>
      <div className="secret-value-row">
        <code className="secret-value" aria-label={valueLabel}>
          {secret}
        </code>
        <CopyButton text={secret} label={copyLabel} />
      </div>
      <div className="secret-actions">
        <Button variant="primary" onClick={onDismiss}>
          {dismissText}
        </Button>
      </div>
    </div>
  );
}

/** What to tell a person for the problems anyone can hit, in place of the
 * server's terse detail. Other problems keep their detail, which says
 * what failed (`Sign-in may not have been disabled.`). */
const problemSentences: Record<string, string> = {
  version_conflict: 'Someone changed this meanwhile. Reload to see their change, then try again.',
  idempotency_in_progress: 'The same change is still being applied. Wait a moment, then reload.',
  idempotency_conflict: 'This change was already sent with different values. Reload and make it again.',
  not_found: 'It no longer exists; someone may have removed it. Reload to see the current state.',
  forbidden: 'Your role does not allow this. Ask an organization owner or admin.',
  authentication_required: 'Your session has ended. Sign in again.',
  last_owner_required: 'The organization needs at least one owner. Make someone else an owner first.',
};

/** A sentence that says what happened, starting with a capital and ending
 * with a full stop. */
export function problemMessage(problem: ApiProblem): string {
  const known = problemSentences[problem.code];
  if (known) return known;
  const text = (problem.detail || problem.title || 'The request failed').trim();
  // An identifier (`cached_input`) keeps its case.
  const sentence = /^[a-z]+(\s|$)/.test(text) ? text.charAt(0).toUpperCase() + text.slice(1) : text;
  return /[.!?]$/.test(sentence) ? sentence : `${sentence}.`;
}

export function InlineError({ problem }: { problem?: ApiProblem | null }) {
  if (!problem) return null;
  // The code stays, small, for whoever helps.
  return (
    <Notice kind="error" title={problemMessage(problem)}>
      {problem.code !== 'request_failed' && <span className="problem-ref"> Error code: <code>{problem.code}</code></span>}
    </Notice>
  );
}

type MessageKind = 'error' | 'success' | 'warning' | 'info';
const messageIcons = { error: ErrorFilled, success: CheckmarkFilled, warning: WarningFilled, info: InformationFilled };

/**
 * A message in Carbon's inline-notification look (low contrast, its icon and
 * colors). Unlike Carbon's InlineNotification it may hold a link or button
 * (Try again, Sign in again). An error is an alert, the rest a status.
 */
export function Message({ kind = 'info', title, children, className, role, ...rest }: {
  kind?: MessageKind;
  title?: React.ReactNode;
  children?: React.ReactNode;
} & Omit<React.HTMLAttributes<HTMLDivElement>, 'title'>) {
  const Icon = messageIcons[kind];
  return (
    <div
      {...rest}
      role={role ?? (kind === 'error' ? 'alert' : 'status')}
      className={['message cds--inline-notification cds--inline-notification--low-contrast', `cds--inline-notification--${kind}`, className].filter(Boolean).join(' ')}
    >
      <div className="cds--inline-notification__details">
        <Icon className="cds--inline-notification__icon" aria-hidden="true" />
        <div className="cds--inline-notification__text-wrapper">
          {title !== undefined && <div className="cds--inline-notification__title">{title}</div>}
          {children !== undefined && children !== null && children !== false && <div className="cds--inline-notification__subtitle">{children}</div>}
        </div>
      </div>
    </div>
  );
}

/** A Message with a title (InlineError, MutationStatus). */
export function Notice({ kind, title, children, className }: {
  kind: MessageKind;
  title: string;
  children?: React.ReactNode;
  className?: string;
}) {
  return <Message kind={kind} title={title} className={['notice', className].filter(Boolean).join(' ')}>{children}</Message>;
}

export function formatDate(value: string | null | undefined): string {
  if (!value) return '—';
  try {
    return new Date(value).toLocaleString();
  } catch {
    return value;
  }
}

export function formatRelativeDate(value: string | null | undefined): string {
  if (!value) return '—';
  try {
    const date = new Date(value);
    const diff = Date.now() - date.getTime();
    const seconds = Math.floor(diff / 1000);
    if (seconds < 60) return `${seconds}s ago`;
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;
    return date.toLocaleDateString();
  } catch {
    return value;
  }
}

/**
 * A Carbon data table: a real <table>, named by its aria-label, that scrolls
 * sideways in its container when it is wider than the page.
 */
export function DataGrid({ children, className, isSortable, size = 'md', ...rest }: React.TableHTMLAttributes<HTMLTableElement> & { isSortable?: boolean; size?: 'xs' | 'sm' | 'md' | 'lg' }) {
  return (
    <TableContainer className="data-grid">
      <CarbonTable size={size} isSortable={isSortable} className={['data-table', className].filter(Boolean).join(' ')} {...rest}>
        {children}
      </CarbonTable>
    </TableContainer>
  );
}

/** A table cell; `label` names its column. */
export function DataCell({
  label,
  children,
  className = '',
  title,
  colSpan,
  style,
}: {
  label?: string;
  children: React.ReactNode;
  className?: string;
  title?: string;
  colSpan?: number;
  style?: React.CSSProperties;
}) {
  return (
    <TableCell data-label={label} className={className || undefined} title={title} colSpan={colSpan} style={style}>
      {children}
    </TableCell>
  );
}

export function uuidHint(value: string) {
  if (!value) return null;
  return value.length > 12 ? `${value.slice(0, 8)}…${value.slice(-4)}` : value;
}

/** The path segment after the page (`/admin/<page>/<tab>`), or null. */
/** Sent when a screen opens another of its sections without `popstate`. */
export const routeChanged = 'gateway:route';

export function routeTab(pathname = location.pathname): string | null {
  return pathname.split('/').filter(Boolean)[2] ?? null;
}

/**
 * A screen's tab, kept in the path (`/admin/<page>/<tab>`) so a reload, Back
 * and a shared link open it. `fallback` is the tab for a path that names none
 * (or one this screen does not have, such as a tab the role cannot see).
 */
export function useRouteTab<T extends string>(keys: readonly T[], fallback: T): [T, (next: T) => void] {
  const read = React.useCallback((): T => {
    const named = routeTab();
    return named && (keys as readonly string[]).includes(named) ? (named as T) : fallback;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [keys.join(), fallback]);
  const [tab, setTabState] = React.useState<T>(read);
  // Back and Forward change the path; the fallback is read once, so a
  // fallback that changes later (a query string the screen consumes) does
  // not move an open tab.
  const latest = React.useRef(read);
  latest.current = read;
  React.useEffect(() => {
    const onPopState = () => setTabState(latest.current());
    addEventListener('popstate', onPopState);
    return () => removeEventListener('popstate', onPopState);
  }, []);
  const setTab = React.useCallback((next: T) => {
    const [admin = 'admin', page] = location.pathname.split('/').filter(Boolean);
    const path = page ? `/${admin}/${page}/${next}` : null;
    if (!path) { setTabState(next); return; }
    if (path !== location.pathname) history.pushState(history.state, '', `${path}${location.search}${location.hash}`);
    setTabState(next);
    // The side navigation marks the open section.
    dispatchEvent(new Event(routeChanged));
  }, []);
  return [tab, setTab];
}

/**
 * A row of tabs (the ARIA tabs pattern): one tab stop, arrow keys, Home and
 * End move between tabs and open them. `id` names the panel the tabs control
 * (render it with `TabPanel` and the same `id`).
 */
export function Tabs<T extends string>({ id, label, tabs, value, onChange }: {
  id: string;
  label: string;
  tabs: readonly (readonly [T, string])[];
  value: T;
  onChange: (next: T) => void;
}) {
  const refs = React.useRef(new Map<T, HTMLButtonElement>());
  const move = (event: React.KeyboardEvent, index: number) => {
    const last = tabs.length - 1;
    const next = event.key === 'ArrowRight' ? (index === last ? 0 : index + 1)
      : event.key === 'ArrowLeft' ? (index === 0 ? last : index - 1)
        : event.key === 'Home' ? 0
          : event.key === 'End' ? last
            : null;
    if (next === null) return;
    event.preventDefault();
    const key = tabs[next][0];
    onChange(key);
    refs.current.get(key)?.focus();
  };
  return (
    <div className="tabs cds--tabs">
      <div className="cds--tab--list" role="tablist" aria-label={label}>
      {tabs.map(([key, text], index) => (
        <button
          key={key}
          ref={element => { if (element) refs.current.set(key, element); else refs.current.delete(key); }}
          type="button"
          role="tab"
          id={`${id}-tab-${key}`}
          aria-selected={value === key}
          aria-controls={`${id}-panel`}
          tabIndex={value === key ? 0 : -1}
          className={`tab cds--tabs__nav-item cds--tabs__nav-link${value === key ? ' active cds--tabs__nav-item--selected' : ''}`}
          onClick={() => onChange(key)}
          onKeyDown={event => move(event, index)}
        >
          <div className="cds--tabs__nav-item-label-wrapper"><span className="cds--tabs__nav-item-label">{text}</span></div>
        </button>
      ))}
      </div>
    </div>
  );
}

/** The panel of a `Tabs` row with the same `id`, labelled by its open tab. */
export function TabPanel({ id, value, children, className }: { id: string; value: string; children: React.ReactNode; className?: string }) {
  return (
    // Focusable, so the keyboard reaches a panel with nothing to focus in it.
    <div role="tabpanel" tabIndex={0} id={`${id}-panel`} aria-labelledby={`${id}-tab-${value}`} className={className}>
      {children}
    </div>
  );
}

/**
 * Opens another admin page (and tab) without reloading: the app and each
 * screen's tab follow the path on `popstate`.
 */
export function navigateTo(path: string) {
  history.pushState(history.state, '', path);
  dispatchEvent(new PopStateEvent('popstate'));
}

/** A link to an admin page, `/admin/<page>[/<tab>]`, that opens in place. */
export function AppLink({ to, children, className = 'inline-action', label }: { to: string; children: React.ReactNode; className?: string; label?: string }) {
  return (
    <a
      href={to}
      className={className}
      aria-label={label}
      onClick={event => {
        if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
        event.preventDefault();
        navigateTo(to);
      }}
    >
      {children}
    </a>
  );
}
