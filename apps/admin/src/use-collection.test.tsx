import { act } from 'react';
import { createRoot } from 'react-dom/client';
import { describe, expect, it } from 'vitest';
import type { Page, PageParams } from './api';
import { useCollection } from './use-collection';

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((res, rej) => { resolve = res; reject = rej; });
  return { promise, resolve, reject };
}

function Harness({ version, fetcher }: { version: number; fetcher: (params: PageParams) => Promise<Page<string>> }) {
  const state = useCollection(fetcher, [version]);
  return <output>{`${state.loading}|${state.error?.code ?? ''}|${state.items.join(',')}`}</output>;
}

describe('useCollection request ordering', () => {
  it('ignores every completion from a superseded reload', async () => {
    const first = deferred<Page<string>>();
    const second = deferred<Page<string>>();
    let calls = 0;
    const fetcher = () => ++calls === 1 ? first.promise : second.promise;
    const container = document.createElement('div');
    document.body.appendChild(container);
    const root = createRoot(container);

    await act(async () => { root.render(<Harness version={1} fetcher={fetcher} />); });
    await act(async () => { root.render(<Harness version={2} fetcher={fetcher} />); });
    second.resolve({ items: ['new'], next_page_token: null });
    await act(async () => { await second.promise; });
    first.reject(new Error('stale failure'));
    await act(async () => { await Promise.resolve(); });

    expect(container.textContent).toBe('false||new');
    root.unmount();
    container.remove();
  });
});
