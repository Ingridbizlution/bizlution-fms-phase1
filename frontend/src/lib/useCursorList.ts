import { useInfiniteQuery, type UseInfiniteQueryResult } from "@tanstack/react-query";

export interface PagedEnvelope<T> {
  data?: T[];
  page?: {
    next_cursor?: string | null;
    limit?: number;
    total_estimate?: number | null;
  };
}

/**
 * Wraps the API's cursor pagination ("load more", never page numbers — see
 * docs/FRONTEND-GETTING-STARTED.md §4.1) in a TanStack Query infinite query.
 */
export function useCursorList<T>(
  queryKey: unknown[],
  fetchPage: (cursor: string | undefined) => Promise<PagedEnvelope<T>>,
  options?: { enabled?: boolean },
): UseInfiniteQueryResult<{ pages: PagedEnvelope<T>[] }> & { items: T[] } {
  const query = useInfiniteQuery({
    queryKey,
    queryFn: ({ pageParam }) => fetchPage(pageParam as string | undefined),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (lastPage) => lastPage.page?.next_cursor ?? undefined,
    enabled: options?.enabled,
  });

  const items = query.data?.pages.flatMap((p) => p.data ?? []) ?? [];
  return { ...query, items };
}
