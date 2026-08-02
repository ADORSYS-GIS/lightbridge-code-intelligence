"use client";

import { LayoutList, Table2 } from "lucide-react";
import { parseAsInteger, parseAsStringLiteral, useQueryState } from "nuqs";
import { useMemo } from "react";
import { RunTable } from "@/components/runs/run-table";
import { RunTimeline } from "@/components/runs/run-timeline";
import { SearchInput } from "@/components/ui/search-input";
import { Select } from "@/components/ui/select";
import { StatusLine } from "@/components/ui/states";
import { repoLabel, statusVisual, type Task, triggerLabel } from "@/lib/domain/tasks";
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
 * daisyUI in ADR-0027). Filters/search/page live in the URL via nuqs (shareable/bookmarkable); the
 * view toggle is a personal preference, so it persists to localStorage instead. `now` is server-passed
 * so relative times don't drift on hydration. Filtering is client-side over the page. */
export function RunList({ tasks, now }: { tasks: Task[]; now: number }) {
  const [filter, setFilter] = useQueryState(
    "status",
    parseAsStringLiteral(FILTER_VALUES).withDefault("all"),
  );
  const [repo, setRepo] = useQueryState("repo", { defaultValue: "all", clearOnDefault: true });
  const [query, setQuery] = useQueryState("q", { defaultValue: "", clearOnDefault: true });
  const [view, setView] = useLocalStorageState<View>("lci.runs.view", "timeline", isView);
  const [page, setPage] = useQueryState("page", parseAsInteger.withDefault(0));

  // Repos present in this page, for the repo filter dropdown.
  const repos = useMemo(
    () => Array.from(new Set(tasks.map(repoLabel))).sort((a, b) => a.localeCompare(b)),
    [tasks],
  );

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return tasks.filter((t) => {
      if (filter !== "all" && statusVisual(t.status).variant !== filter) return false;
      if (repo !== "all" && repoLabel(t) !== repo) return false;
      if (!q) return true;
      return `${repoLabel(t)} ${triggerLabel(t)} ${t.head_sha ?? ""}`.toLowerCase().includes(q);
    });
  }, [tasks, filter, repo, query]);

  // Any filter change invalidates the current page offset, so reset to the first page.
  const resetPage = () => setPage(null);

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
          {repos.length > 1 && (
            <Select
              value={repo}
              onValueChange={(value) => {
                setRepo(value);
                resetPage();
              }}
              options={[
                { value: "all", label: "All repositories" },
                ...repos.map((r) => ({ value: r, label: r })),
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

      {filtered.length === 0 ? (
        <StatusLine>No runs match the current filters.</StatusLine>
      ) : view === "timeline" ? (
        <RunTimeline tasks={filtered} now={now} />
      ) : (
        <RunTable tasks={filtered} now={now} page={page} onPageChange={setPage} />
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
