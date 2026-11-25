import type { Me } from './api';

/** What to call the signed-in person: their name, else their email. */
export function accountLabel(me: Me | null): string {
  return me?.display_name?.trim() || me?.email?.trim() || 'Signed in';
}

/** Up to two initials for the avatar when there is no photo. */
export function accountInitials(me: Me | null): string {
  const name = me?.display_name?.trim();
  if (name) {
    const words = name.split(/\s+/).filter(Boolean);
    const letters = words.length > 1 ? [words[0], words[words.length - 1]] : words;
    return letters.map(word => Array.from(word)[0].toLocaleUpperCase()).join('');
  }
  const email = me?.email?.trim();
  return email ? Array.from(email)[0].toLocaleUpperCase() : '?';
}
