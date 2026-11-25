import { describe, expect, it } from 'vitest';

// Every screen's source, as text: a header row's cells must be <th>
// (TableHeader), or screen readers lose the column names.
const sources = import.meta.glob('./*.tsx', { query: '?raw', import: 'default', eager: true }) as Record<string, string>;

describe('table headers', () => {
  it('builds every header row from TableHeader, not data cells', () => {
    expect(Object.keys(sources).length).toBeGreaterThan(20);
    const offenders = Object.entries(sources).flatMap(([file, text]) =>
      [...text.matchAll(/<TableHead>([\s\S]*?)<\/TableHead>/g)]
        .filter(match => /<DataCell|<TableCell|<td/.test(match[1]))
        .map(() => file));
    expect(offenders).toEqual([]);
  });
});
