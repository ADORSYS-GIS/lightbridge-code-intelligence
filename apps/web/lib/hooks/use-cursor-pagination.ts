"use client";

import { parseAsInteger, useQueryStates } from "nuqs";

/** A page boundary as a cursor-paginated list's server response carries it. */
export interface CursorBoundary {
  activity_at: string;
  id: number;
}

/** URL-state pager for a keyset-paginated list (e.g. `GET /repositories`, control-plane #606):
 * a numbered "N / M" display over a cursor whose actual boundary is a row, not a position.
 *
 * `page` in the URL is display-only — `goToPage` moves it for the label, but the request that
 * moves between pages is driven by `next`/`prev`, the keyset boundary the caller's last fetch
 * returned; `page` itself is never sent anywhere. That split is what keeps paging correct under
 * concurrent inserts (the boundary is a row) while still reading like ordinary numbered pages.
 *
 * Deliberately does not own a search/filter param: a filter changing is a *reason* to reset
 * pagination, not something pagination itself needs to know about. Callers call `reset()`
 * alongside their own filter's setter. */
export function useCursorPagination({
  total,
  pageSize,
  next,
  prev,
}: {
  /** Rows matching the current filter, independent of the current page. */
  total: number;
  pageSize: number;
  /** Where a "Next" request should continue from, or null at the end of the list. */
  next: CursorBoundary | null;
  /** Where a "Prev" request should continue from, or null at the start of the list. */
  prev: CursorBoundary | null;
}) {
  // `shallow: false` on every param: paging drives a real server-side fetch, so a URL-only change
  // (nuqs' default `shallow: true`) would update the address bar without ever re-invoking the
  // Server Component that owns the data.
  const [{ page }, setParams] = useQueryStates({
    page: parseAsInteger.withDefault(0).withOptions({ shallow: false }),
    after_activity_at: { defaultValue: "", parse: String, clearOnDefault: true, shallow: false },
    after_id: { defaultValue: "", parse: String, clearOnDefault: true, shallow: false },
    before_activity_at: { defaultValue: "", parse: String, clearOnDefault: true, shallow: false },
    before_id: { defaultValue: "", parse: String, clearOnDefault: true, shallow: false },
  });

  const pageCount = Math.max(1, Math.ceil(total / pageSize));
  const current = Math.min(Math.max(0, page), pageCount - 1);
  const start = current * pageSize;
  const rangeLabel =
    total === 0 ? "No results" : `${start + 1}–${Math.min(start + pageSize, total)} of ${total}`;

  // `Pagination` only ever calls this with `current - 1` (or `null`, at page 0) or `current + 1`,
  // so the sign of the move says which cursor to send. `next` and `prev` are null exactly when the
  // corresponding button is disabled — both come from the same response as `total` — so a move
  // landing here always has the cursor it needs; the `&& next`/`&& prev` guards are a graceful
  // no-op for the rare case a concurrent change makes that momentarily untrue.
  function goToPage(target: number | null) {
    const nextPage = target ?? 0;
    if (nextPage > current && next) {
      setParams({
        page: target,
        after_activity_at: next.activity_at,
        after_id: String(next.id),
        before_activity_at: null,
        before_id: null,
      });
    } else if (nextPage < current && prev) {
      setParams({
        page: target,
        before_activity_at: prev.activity_at,
        before_id: String(prev.id),
        after_activity_at: null,
        after_id: null,
      });
    }
  }

  /** Back to the first page with no cursor — what a caller's own filter change should trigger. */
  function reset() {
    setParams({
      page: null,
      after_activity_at: null,
      after_id: null,
      before_activity_at: null,
      before_id: null,
    });
  }

  return { current, pageCount, rangeLabel, goToPage, reset };
}
