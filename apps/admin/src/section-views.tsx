import React, { useEffect, useState } from 'react';
import { ContentSwitcher, Switch } from '@carbon/react';

/** Which view of a section is open, kept in the page URL under `param` so a
 * reload or a shared link opens the same one. Without it, `initial` picks
 * one (such as the view a table's filters in the URL belong to). */
export function useSectionView<V extends string>(param: string, views: readonly V[], initial: () => V): [V, (next: V) => void] {
  const read = (): V => {
    const value = new URLSearchParams(location.search).get(param);
    return views.includes(value as V) ? (value as V) : initial();
  };
  const [view, setView] = useState<V>(read);
  useEffect(() => {
    const reread = () => setView(read());
    window.addEventListener('popstate', reread);
    return () => window.removeEventListener('popstate', reread);
  }, [param]); // eslint-disable-line react-hooks/exhaustive-deps
  const update = (next: V) => {
    setView(next);
    const params = new URLSearchParams(location.search);
    params.set(param, next);
    history.replaceState(history.state, '', `${location.pathname}?${params}${location.hash}`);
  };
  return [view, update];
}

/** The views of one section, one at a time: a content switcher, as in IBM
 * Cloud. */
export function SectionSwitcher<V extends string>({ label, views, view, onChange }: {
  label: string;
  views: readonly (readonly [V, string])[];
  view: V;
  onChange: (next: V) => void;
}) {
  return (
    <ContentSwitcher
      className="section-switcher"
      aria-label={label}
      selectedIndex={views.findIndex(([key]) => key === view)}
      onChange={({ name }) => onChange(name as V)}
    >
      {views.map(([key, text]) => <Switch key={key} name={key} text={text} />)}
    </ContentSwitcher>
  );
}

/** A short hint with the longer explanation folded under it. */
export function MoreHint({ hint, more, summary = 'How this works' }: { hint: React.ReactNode; more: React.ReactNode; summary?: string }) {
  return (
    <div className="section-hint">
      <p className="muted">{hint}</p>
      <details className="hint-more">
        <summary>{summary}</summary>
        <div className="muted">{more}</div>
      </details>
    </div>
  );
}
