import { useEffect, useState } from 'react';

/** What someone chose: a theme, or whatever their system uses. */
export type ThemePreference = 'system' | 'light' | 'dark';
export type Theme = 'light' | 'dark';

const KEY = 'gateway.admin.theme';
/** Carbon's theme zones: white for light, g100 for dark. */
const zone: Record<Theme, string> = { light: 'cds--white', dark: 'cds--g100' };

export function readPreference(): ThemePreference {
  try {
    const stored = localStorage.getItem(KEY);
    return stored === 'light' || stored === 'dark' ? stored : 'system';
  } catch {
    // Storage can be blocked (a private window): follow the system.
    return 'system';
  }
}

function storePreference(preference: ThemePreference) {
  try {
    if (preference === 'system') localStorage.removeItem(KEY);
    else localStorage.setItem(KEY, preference);
  } catch {
    // Not remembered, but still applied for this visit.
  }
}

const darkQuery = () => (typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: dark)') : null);

export function resolveTheme(preference: ThemePreference, systemDark = darkQuery()?.matches ?? false): Theme {
  return preference === 'system' ? (systemDark ? 'dark' : 'light') : preference;
}

/** Puts the theme's Carbon zone on <html>, so every token follows it. */
export function applyTheme(theme: Theme, root: HTMLElement = document.documentElement) {
  root.classList.remove(zone.light, zone.dark);
  root.classList.add(zone[theme]);
  root.dataset.theme = theme;
}

/** The preference, and a setter that applies and remembers it. While it is
 * `system`, the theme follows the system's setting as it changes. */
export function useThemePreference(): [ThemePreference, (next: ThemePreference) => void] {
  const [preference, setPreference] = useState<ThemePreference>(readPreference);
  useEffect(() => {
    applyTheme(resolveTheme(preference));
    if (preference !== 'system') return;
    const query = darkQuery();
    if (!query) return;
    const follow = () => applyTheme(resolveTheme('system', query.matches));
    query.addEventListener('change', follow);
    return () => query.removeEventListener('change', follow);
  }, [preference]);
  const set = (next: ThemePreference) => {
    storePreference(next);
    setPreference(next);
  };
  return [preference, set];
}
