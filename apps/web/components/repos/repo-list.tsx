"use client";

import { ExternalLink, GitBranch, Settings } from "lucide-react";
import Link from "next/link";
import { useQueryState } from "nuqs";
import { useMemo } from "react";
import { Card } from "@/components/ui/card";
import { Pagination } from "@/components/ui/pagination";
import { SearchInput } from "@/components/ui/search-input";
import { Pill } from "@/components/ui/status-pill";
import type { GitlabLinkConfig } from "@/lib/domain/gitlab-links";
import {
  approvalVisual,
  REPOS_PAGE_SIZE,
  type Repository,
  repoSlug,
  repoUrl,
} from "@/lib/domain/repos";
import { relativeTime } from "@/lib/domain/tasks";
import { useCursorPagination } from "@/lib/hooks/use-cursor-pagination";
import type { RepositoriesCursor } from "@/lib/server/api";

/** Connected repositories as cards with a search box + pagination (ADR-0024, daisyUI in ADR-0027).
 * `repos`/`total`/`next`/`prev` are exactly what the control plane returned for the current URL
 * (`listRepositoriesPage`, cursor-paginated) — this component does no client-side filtering or
 * slicing of its own; paging itself is `useCursorPagination`'s job.
 *
 * `now` is server-passed so relative times don't drift on hydration.
 * `gitlabLinks` is passed from the Server Component for self-hosted GitLab links. */
export function RepoList({
  repos,
  total,
  next,
  prev,
  now,
  gitlabLinks,
  overrideRepoIds,
}: {
  repos: Repository[];
  /** Repositories matching the current search, independent of the current page. */
  total: number;
  /** Where a "Next" request should continue from, or null at the end of the list. */
  next: RepositoriesCursor | null;
  /** Where a "Prev" request should continue from, or null at the start of the list. */
  prev: RepositoriesCursor | null;
  now: number;
  gitlabLinks: GitlabLinkConfig;
  /** Repos with at least one ADR-0111 setting resolved from a DB admin override (epic #566). */
  overrideRepoIds: number[];
}) {
  const overrideSet = useMemo(() => new Set(overrideRepoIds), [overrideRepoIds]);
  // `shallow: false`: search drives a real server-side fetch (`listRepositoriesPage`), so a
  // URL-only change would update the address bar without ever re-invoking the Server Component
  // that owns the data — same reason the pager below needs it.
  const [q, setQuery] = useQueryState("q", {
    defaultValue: "",
    clearOnDefault: true,
    shallow: false,
  });
  const { current, pageCount, rangeLabel, goToPage, reset } = useCursorPagination({
    total,
    pageSize: REPOS_PAGE_SIZE,
    next,
    prev,
  });

  return (
    <div className="flex flex-col gap-3">
      <SearchInput
        value={q}
        onChange={(e) => {
          setQuery(e.target.value);
          reset();
        }}
        placeholder="Search repositories"
        aria-label="Search repositories"
        className="w-full sm:w-72"
      />

      {repos.length === 0 ? (
        <p className="px-1 py-6 text-sm text-base-content/60">
          {q ? `No repositories match “${q}”.` : "No repositories."}
        </p>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2">
          {repos.map((repo) => (
            <RepoCard
              key={repo.id}
              repo={repo}
              now={now}
              gitlabLinks={gitlabLinks}
              hasOverride={overrideSet.has(repo.id)}
            />
          ))}
        </div>
      )}

      {total > REPOS_PAGE_SIZE && (
        <Pagination
          current={current}
          pageCount={pageCount}
          rangeLabel={rangeLabel}
          onPageChange={goToPage}
          className="flex items-center justify-between gap-3 text-xs text-base-content/60"
        />
      )}
    </div>
  );
}

function RepoCard({
  repo,
  now,
  gitlabLinks,
  hasOverride,
}: {
  repo: Repository;
  now: number;
  gitlabLinks: GitlabLinkConfig;
  /** At least one ADR-0111 setting has an admin DB override (epic #566). */
  hasOverride: boolean;
}) {
  const approval = approvalVisual(repo);
  const viewLabel = repo.platform === "gitlab" ? "View on GitLab" : "View on GitHub";
  return (
    <Card>
      <div className="flex items-start justify-between gap-3 px-4 py-3">
        <div className="min-w-0">
          <div className="truncate text-sm font-medium">{repoSlug(repo)}</div>
          <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-base-content/60">
            <span className="inline-flex items-center gap-1">
              <GitBranch className="size-3" />
              {repo.default_branch}
            </span>
            <span>
              {repo.task_count} {repo.task_count === 1 ? "run" : "runs"}
            </span>
            {repo.last_task_at && <span>last {relativeTime(repo.last_task_at, now)}</span>}
          </div>
        </div>
        <Pill variant={approval.variant} label={approval.label} className="shrink-0" />
      </div>
      <div className="flex items-center justify-between gap-3 border-t border-base-content/15 px-4 py-2 text-xs">
        {/* Index health (graph + vector freshness, ADR-0016) lands with the indexer — honest for now. */}
        <span className="text-base-content/60">Not indexed yet</span>
        <div className="flex items-center gap-3">
          <Link
            href={`/dashboard/repositories/${repo.id}`}
            className="relative inline-flex items-center gap-1 text-base-content/60 transition-colors hover:text-base-content"
            title={
              hasOverride ? "Review settings (has an admin override)" : "Review preset settings"
            }
          >
            <Settings className="size-3 shrink-0" />
            {hasOverride && (
              <span className="absolute -top-0.5 -right-0.5 size-1.5 rounded-full bg-accent" />
            )}
          </Link>
          <a
            href={repoUrl(repo, gitlabLinks)}
            target="_blank"
            rel="noopener noreferrer"
            className="inline-flex items-center gap-1 text-primary transition-colors hover:underline"
          >
            {viewLabel}
            <ExternalLink className="size-3 shrink-0" />
          </a>
        </div>
      </div>
    </Card>
  );
}
