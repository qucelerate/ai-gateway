import { afterEach, describe, expect, it, vi } from 'vitest';
import initScript from '../public/theme-init.js?raw';
import { applyTheme, readPreference, resolveTheme } from './theme';


describe('theme', () => {
  afterEach(() => localStorage.clear());

  it('follows the system until someone chooses', () => {
    expect(readPreference()).toBe('system');
    expect(resolveTheme('system', true)).toBe('dark');
    expect(resolveTheme('system', false)).toBe('light');
    expect(resolveTheme('light', true)).toBe('light');
    expect(resolveTheme('dark', false)).toBe('dark');
  });

  it('reads back a remembered choice and ignores anything else', () => {
    localStorage.setItem('gateway.admin.theme', 'dark');
    expect(readPreference()).toBe('dark');
    localStorage.setItem('gateway.admin.theme', 'purple');
    expect(readPreference()).toBe('system');
  });

  it('puts exactly one Carbon zone on the root', () => {
    const root = document.createElement('html');
    applyTheme('dark', root);
    expect(root.className).toBe('cds--g100');
    applyTheme('light', root);
    expect(root.className).toBe('cds--white');
    expect(root.dataset.theme).toBe('light');
  });

  it('agrees with the script that applies the theme before the app loads', () => {
    for (const [stored, systemDark] of [[null, false], [null, true], ['light', true], ['dark', false], ['purple', true]] as const) {
      localStorage.clear();
      if (stored) localStorage.setItem('gateway.admin.theme', stored);
      vi.stubGlobal('matchMedia', () => ({ matches: systemDark }));
      document.documentElement.className = '';
      new Function(initScript)();
      const early = document.documentElement.className;
      const app = document.createElement('html');
      applyTheme(resolveTheme(readPreference(), systemDark), app);
      expect(early).toBe(app.className);
      vi.unstubAllGlobals();
    }
    document.documentElement.className = '';
  });
});
