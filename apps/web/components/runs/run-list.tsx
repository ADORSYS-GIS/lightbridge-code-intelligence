"use client";

import { LayoutList, Table2 } from "lucide-react";
import { parseAsInteger, parseAsStringLiteral, useQueryState } from "nuqs";
import { RunTable } from "@/components/runs/run-table";
import { RunTimeline } from "@/components/runs/run-timeline";
import { Pagination } from "@/components/ui/pagination";
import { SearchInput } from "@/components/ui/search-input";
import { Select } from "@/components/ui/select";
import { StatusLine } from "@/components/ui/states";
import type { Repository } from "@/lib/domain/repos";
import { repoSlug } from "@/lib/domain/repos";
import { RUNS_PAGE_SIZE, type Task } from "@/lib/domain/tasks";
import { useLocalStorageState } from "@/lib/hooks/use-local-storage-state";
import { cn } from "@/lib/utils/cn";

const FILTER_VALUES = ["all", "active", "pending", "success", "error", "muted"] as const;
const FILTERS: { value: (typeof FILTER_VALUES)[number]; label: string }[] = [
  { value: "all", label: "All" },
  { value: "active", label: "Running" },
  { value: "pending", label: "Pending" },
  { value: "success", label: "Succeeded" },
  { value: "error", label: "Failed" },
  { value: "muted", label: "Cancelled" },
];

const VIEW_VALUES = ["timeline", "table"] as const;
type View = (typeof VIEW_VALUES)[number];
const isView = (value: string): value is View => (VIEW_VALUES as readonly string[]).includes(value);

/** Run list with status + repo filters, text search, and a timeline/table view toggle (ADR-0024,
 * daisyUI in ADR-0027). Filters/search/page live in the URL via nuqs — writing them re-triggers the
 * parent Server Component's `listTasksPage` fetch (real server-side pagination + filtering,
 * control-plane #587), so `tasks`/`total` arriving as props are already the correct window; this
 * component does no client-side filtering or slicing itself. The view toggle is a personal
 * preference, so it persists to localStorage instead of the URL. `now` is server-passed so relative
 * times don't drift on hydration. `repoOptions` is the full repo universe (not derived from `tasks`)
 * so narrowing the filter to one repo doesn't make every other repo vanish from its own dropdown. */
export function RunList({
  tasks,
  total,
  repoOptions,
  now,
}: {
  tasks: Task[];
  total: number;
  repoOptions: Repository[];
  now: number;
}) {
  // `shallow: false` on every param here: this page's status/repo/q/page state drives a real
  // server-side fetch (`listTasksPage`, real pagination — control-plane #587), so a URL-only change
  // (nuqs' default `shallow: true`) would update the address bar without ever re-invoking the Server
  // Component that owns the data. RepoList's search and pager drive the same kind of server-side
  // fetch (`listRepositoriesPage`) and need the same `shallow: false` — see its own comment.
  const [filter, setFilter] = useQueryState(
    "status",
    parseAsStringLiteral(FILTER_VALUES).withDefault("all").withOptions({ shallow: false }),
  );
  const [repo, setRepo] = useQueryState("repo", {
    defaultValue: "all",
    clearOnDefault: true,
    shallow: false,
  });
  const [query, setQuery] = useQueryState("q", {
    defaultValue: "",
    clearOnDefault: true,
    shallow: false,
  });
  const [view, setView] = useLocalStorageState<View>("lci.runs.view", "timeline", isView);
  const [page, setPage] = useQueryState(
    "page",
    parseAsInteger.withDefault(0).withOptions({ shallow: false }),
  );

  // Any filter change invalidates the current page offset, so reset to the first page.
  const resetPage = () => setPage(null);

  const pageCount = Math.max(1, Math.ceil(total / RUNS_PAGE_SIZE));
  const current = Math.min(Math.max(0, page), pageCount - 1);
  const start = current * RUNS_PAGE_SIZE;
  const rangeLabel =
    total === 0
      ? "No results"
      : `${start + 1}–${Math.min(start + RUNS_PAGE_SIZE, total)} of ${total}`;

  return (
    <div className="overflow-hidden rounded-box border border-base-content/15 bg-base-200">
      <div className="flex flex-wrap items-center gap-2 border-b border-base-content/15 px-3 py-2.5">
        <div className="join">
          {FILTERS.map((f) => (
            <button
              type="button"
              key={f.value}
              onClick={() => {
                setFilter(f.value);
                resetPage();
              }}
              className={cn("btn btn-xs join-item", filter === f.value && "btn-active btn-primary")}
            >
              {f.label}
            </button>
          ))}
        </div>

        <div className="ml-auto flex items-center gap-2">
          {repoOptions.length > 1 && (
            <Select
              value={repo}
              onValueChange={(value) => {
                setRepo(value);
                resetPage();
              }}
              options={[
                { value: "all", label: "All repositories" },
                ...repoOptions.map((r) => ({ value: String(r.id), label: repoSlug(r) })),
              ]}
              aria-label="Filter by repository"
              className="max-w-[12rem]"
            />
          )}

          <SearchInput
            value={query}
            onChange={(e) => {
              setQuery(e.target.value);
              resetPage();
            }}
            placeholder="Search runs"
            aria-label="Search runs"
            className="w-44"
          />

          {/* Timeline / table view toggle. */}
          <div className="join">
            <ViewButton
              active={view === "timeline"}
              onClick={() => setView("timeline")}
              label="Timeline"
            >
              <LayoutList className="size-3.5" />
            </ViewButton>
            <ViewButton active={view === "table"} onClick={() => setView("table")} label="Table">
              <Table2 className="size-3.5" />
            </ViewButton>
          </div>
        </div>
      </div>

      {tasks.length === 0 ? (
        <StatusLine>No runs match the current filters.</StatusLine>
      ) : view === "timeline" ? (
        <RunTimeline tasks={tasks} now={now} />
      ) : (
        <RunTable tasks={tasks} now={now} />
      )}

      {total > RUNS_PAGE_SIZE && (
        <Pagination
          current={current}
          pageCount={pageCount}
          rangeLabel={rangeLabel}
          onPageChange={setPage}
          className="flex items-center justify-between gap-3 border-t border-base-content/15 px-4 py-2.5 text-xs text-base-content/60"
        />
      )}
    </div>
  );
}

function ViewButton({
  active,
  onClick,
  label,
  children,
}: {
  active: boolean;
  onClick: () => void;
  label: string;
  children: React.ReactNode;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-pressed={active}
      aria-label={`${label} view`}
      title={`${label} view`}
      className={cn("btn btn-xs btn-square join-item", active && "btn-active")}
    >
      {children}
    </button>
  );
}
