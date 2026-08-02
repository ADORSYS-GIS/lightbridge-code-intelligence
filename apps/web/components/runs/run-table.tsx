"use client";

import { ChevronDown, ChevronUp } from "lucide-react";
import Link from "next/link";
import { useMemo, useState } from "react";
import { StatusPill } from "@/components/ui/status-pill";
import {
  duration,
  durationSeconds,
  relativeTime,
  repoLabel,
  shortSha,
  statusVisual,
  type Task,
  triggerLabel,
} from "@/lib/domain/tasks";
import { cn } from "@/lib/utils/cn";

type SortKey = "created" | "duration" | "status" | "repo" | "trigger";
type SortDir = "asc" | "desc";

// Ascending comparators per column; the header toggles direction. `created` is the default (desc =
// newest first), matching the list's natural order.
function compare(key: SortKey, a: Task, b: Task, now: number): number {
  switch (key) {
    case "created":
      // created_at is ISO-8601 (UTC) — lexicographic order == chronological, no Date parsing.
      return a.created_at.localeCompare(b.created_at);
    case "duration":
      // Unstarted runs (null) sort below started ones in ascending order. MAX_SAFE_INTEGER (not
      // Infinity) so two nulls subtract to 0, not NaN.
      return (
        (durationSeconds(a, now) ?? Number.MAX_SAFE_INTEGER) -
        (durationSeconds(b, now) ?? Number.MAX_SAFE_INTEGER)
      );
    case "status":
      return statusVisual(a.status).label.localeCompare(statusVisual(b.status).label);
    case "repo":
      return repoLabel(a).localeCompare(repoLabel(b));
    case "trigger":
      return triggerLabel(a).localeCompare(triggerLabel(b));
  }
}

/** Dense, sortable table of runs (ADR-0024, daisyUI `table` in ADR-0027). `tasks` is already the
 * correct server-paginated/filtered page (real pagination, control-plane #587) — paging lives once,
 * in the parent `RunList`, shared with the Timeline view; this component only re-sorts what it's
 * given, so a non-default sort reorders one page at a time rather than the full result set (the
 * same in-kind limitation the old client-side version had, just over a smaller pool). `now` comes
 * from the server so relative times don't drift on hydration. */
export function RunTable({ tasks, now }: { tasks: Task[]; now: number }) {
  const [sort, setSort] = useState<{ key: SortKey; dir: SortDir }>({ key: "created", dir: "desc" });

  const sorted = useMemo(() => {
    const out = [...tasks].sort((a, b) => compare(sort.key, a, b, now));
    return sort.dir === "desc" ? out.reverse() : out;
  }, [tasks, sort, now]);

  const toggle = (key: SortKey) =>
    setSort((s) =>
      s.key === key ? { key, dir: s.dir === "asc" ? "desc" : "asc" } : { key, dir: "asc" },
    );

  return (
    <div>
      <div className="overflow-x-auto">
        <table className="table table-sm">
          <thead>
            <tr className="text-base-content/60">
              <Th label="Status" sortKey="status" sort={sort} onSort={toggle} />
              <Th label="Trigger" sortKey="trigger" sort={sort} onSort={toggle} />
              <Th label="Repository" sortKey="repo" sort={sort} onSort={toggle} />
              <th className="font-medium">Branch</th>
              <Th label="Created" sortKey="created" sort={sort} onSort={toggle} align="right" />
              <Th label="Duration" sortKey="duration" sort={sort} onSort={toggle} align="right" />
            </tr>
          </thead>
          <tbody>
            {sorted.map((task) => {
              const dur = duration(task, now);
              const sha = shortSha(task.head_sha);
              return (
                <tr key={task.id} className="relative transition-colors hover:bg-base-300/60">
                  <td>
                    <StatusPill status={task.status} />
                  </td>
                  <td className="max-w-xs truncate font-medium">
                    {/* Stretched link: the `after:absolute after:inset-0` overlay makes the whole row
                        a single real anchor (keyboard, middle/ctrl-click, no-JS) without nesting an
                        <a> around a <tr>. Foreground (not accent) to match the timeline RunRow. */}
                    <Link
                      href={`/dashboard/runs/${task.id}`}
                      className="hover:underline after:absolute after:inset-0"
                    >
                      {triggerLabel(task)}
                    </Link>
                  </td>
                  <td className="max-w-[12rem] truncate text-base-content/60">{repoLabel(task)}</td>
                  <td className="text-base-content/60">
                    {task.repo_default_branch ??
                      (sha ? <span className="font-mono">{sha}</span> : "—")}
                  </td>
                  <td
                    className="whitespace-nowrap text-right text-base-content/60"
                    title={task.created_at}
                  >
                    {relativeTime(task.created_at, now)}
                  </td>
                  <td className="text-right font-mono text-base-content/60">{dur ?? "—"}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

function Th({
  label,
  sortKey,
  sort,
  onSort,
  align = "left",
}: {
  label: string;
  sortKey: SortKey;
  sort: { key: SortKey; dir: SortDir };
  onSort: (key: SortKey) => void;
  align?: "left" | "right";
}) {
  const active = sort.key === sortKey;
  return (
    <th className={cn("font-medium", align === "right" && "text-right")}>
      <button
        type="button"
        onClick={() => onSort(sortKey)}
        className={cn(
          "inline-flex items-center gap-1 transition-colors hover:text-base-content",
          align === "right" && "flex-row-reverse",
          active && "text-base-content",
        )}
      >
        {label}
        {active &&
          (sort.dir === "asc" ? (
            <ChevronUp className="size-3" />
          ) : (
            <ChevronDown className="size-3" />
          ))}
      </button>
    </th>
  );
}
