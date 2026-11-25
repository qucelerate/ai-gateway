import { useCallback, useEffect, useRef, useState } from 'react';
import type { ApiProblem, Page, PageParams } from './api';

const DEFAULT_PAGE_SIZE = 25;

export type CollectionState<T> = {
  items: T[];
  loading: boolean;
  loadingMore: boolean;
  error: ApiProblem | null;
  hasMore: boolean;
  load: (reset?: boolean) => Promise<void>;
  loadMore: () => void;
};

function unknownProblem(): ApiProblem {
  return {
    type: 'about:blank',
    code: 'load_failed',
    title: 'Load failed',
    status: 0,
    detail: 'The list could not be loaded. Please try again.',
    instance: '',
    request_id: '',
  };
}

export function useCollection<T>(
  fetcher: (params: PageParams) => Promise<Page<T>>,
  deps: unknown[] = [],
): CollectionState<T> {
  const [items, setItems] = useState<T[]>([]);
  const [loading, setLoading] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [error, setError] = useState<ApiProblem | null>(null);
  const [nextToken, setNextToken] = useState<string | undefined>(undefined);

  const fetcherRef = useRef(fetcher);
  fetcherRef.current = fetcher;
  const nextTokenRef = useRef(nextToken);
  nextTokenRef.current = nextToken;
  const loadingMoreRef = useRef(loadingMore);
  loadingMoreRef.current = loadingMore;
  const loadingRef = useRef(loading);
  loadingRef.current = loading;
  const requestSequenceRef = useRef(0);

  const load = useCallback(async (reset = true) => {
    let requestSequence: number;
    if (reset) {
      requestSequence = ++requestSequenceRef.current;
      loadingRef.current = true;
      loadingMoreRef.current = false;
      setLoading(true);
      setItems([]);
      setNextToken(undefined);
      setError(null);
      setLoadingMore(false);
    } else {
      if (!nextTokenRef.current || loadingMoreRef.current || loadingRef.current) return;
      requestSequence = ++requestSequenceRef.current;
      loadingMoreRef.current = true;
      setLoadingMore(true);
    }
    try {
      const params: PageParams = {
        page_size: DEFAULT_PAGE_SIZE,
        page_token: reset ? undefined : nextTokenRef.current,
      };
      const page = await fetcherRef.current(params);
      if (requestSequence !== requestSequenceRef.current) return;
      setItems(prev => (reset ? page.items : [...prev, ...page.items]));
      setNextToken(page.next_page_token ?? undefined);
      setError(null);
    } catch (caught) {
      if (requestSequence !== requestSequenceRef.current) return;
      const problem = (caught as { problem?: ApiProblem }).problem ?? unknownProblem();
      setError(problem);
    } finally {
      if (requestSequence === requestSequenceRef.current) {
        if (reset) {
          loadingRef.current = false;
          setLoading(false);
        } else {
          loadingMoreRef.current = false;
          setLoadingMore(false);
        }
      }
    }
  }, []);

  const loadMore = useCallback(() => {
    void load(false);
  }, [load]);

  useEffect(() => {
    void load(true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);

  return {
    items,
    loading,
    loadingMore,
    error,
    hasMore: !!nextToken,
    load,
    loadMore,
  };
}

/**
 * Every page of a small set, as one page: the shared data table filters,
 * groups and paginates such sets in the browser.
 */
export async function everyPage<T, P extends Page<T>>(
  fetcher: (params: PageParams) => Promise<P>,
  pageSize = 100,
): Promise<P> {
  const first = await fetcher({ page_size: pageSize });
  const items = [...first.items];
  let token = first.next_page_token;
  while (token) {
    const page = await fetcher({ page_size: pageSize, page_token: token });
    items.push(...page.items);
    token = page.next_page_token;
  }
  return { ...first, items, next_page_token: null };
}
