"use client";

import { ExternalLink, GitBranch } from "lucide-react";
import Link from "next/link";
import { parseAsInteger, useQueryState } from "nuqs";
import { useMemo } from "react";
import { Card } from "@/components/ui/card";
import { Pagination } from "@/components/ui/pagination";
import { SearchInput } from "@/components/ui/search-input";
import { Pill } from "@/components/ui/status-pill";
import type { GitlabLinkConfig } from "@/lib/domain/gitlab-links";
import { approvalVisual, type Repository, repoSlug, repoUrl } from "@/lib/domain/repos";
import { relativeTime } from "@/lib/domain/tasks";
import { usePagination } from "@/lib/hooks/use-pagination";

const PAGE_SIZE = 12;

/** Connected repositories as cards with a search box + pagination (ADR-0024, daisyUI in ADR-0027).
 * Search + page live in the URL via nuqs; filtering/paging is client-side over the fetched list.
 * `now` is server-passed so relative times don't drift on hydration.
 * `gitlabLinks` is passed from the Server Component for self-hosted GitLab links. */
export function RepoList({
  repos,
  now,
  gitlabLinks,
  overrideRepoIds,
}: {
  repos: Repository[];
  now: number;
  gitlabLinks: GitlabLinkConfig;
  /** Repos with at least one ADR-0111 setting resolved from a DB admin override (epic #566). */
  overrideRepoIds: number[];
}) {
  const overrideSet = useMemo(() => new Set(overrideRepoIds), [overrideRepoIds]);
  const [query, setQuery] = useQueryState("q", { defaultValue: "", clearOnDefault: true });
  const [page, setPage] = useQueryState("page", parseAsInteger.withDefault(0));

  const filtered = useMemo(() => {
    const q = query.trim().toLowerCase();
    return q ? repos.filter((r) => repoSlug(r).toLowerCase().includes(q)) : repos;
  }, [repos, query]);

  const { rows, pageCount, current, rangeLabel } = usePagination(filtered, PAGE_SIZE, page);

  return (
    <div className="flex flex-col gap-3">
      <SearchInput
        value={query}
        onChange={(e) => {
          setQuery(e.target.value);
          setPage(null);
        }}
        placeholder="Search repositories"
        aria-label="Search repositories"
        className="w-full sm:w-72"
      />

      {rows.length === 0 ? (
        <p className="px-1 py-6 text-sm text-base-content/60">No repositories match “{query}”.</p>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2">
          {rows.map((repo) => (
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

      {filtered.length > PAGE_SIZE && (
        <Pagination
          current={current}
          pageCount={pageCount}
          rangeLabel={rangeLabel}
          onPageChange={setPage}
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
    // The card's own overlay anchor is what makes the whole surface clickable. Wrapping the card in
    // a link instead would nest it around the external link below — invalid markup that browsers
    // resolve by silently dropping one of the two.
    <Card className="relative">
      <div className="flex items-start justify-between gap-3 px-4 py-3">
        <div className="min-w-0">
          <Link
            href={`/dashboard/repositories/${repo.id}`}
            className="block truncate text-sm font-medium after:absolute after:inset-0 hover:underline"
          >
            {repoSlug(repo)}
          </Link>
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
        <div className="flex shrink-0 items-center gap-1.5">
          {/* Carried by a label rather than a marker on an icon: a repository whose review settings
              have been overridden away from its own config file is worth saying outright, and it is
              the kind of thing a reader only learns from a dot if they already knew to look. */}
          {hasOverride && (
            <span
              className="badge badge-xs badge-accent badge-soft"
              title="A review setting is overridden for this repository"
            >
              Override
            </span>
          )}
          <Pill variant={approval.variant} label={approval.label} />
        </div>
      </div>
      <div className="flex items-center justify-between gap-3 border-t border-base-content/15 px-4 py-2 text-xs">
        {/* Index health (graph + vector freshness, ADR-0016) lands with the indexer — honest for now. */}
        <span className="text-base-content/60">Not indexed yet</span>
        {/* Stacked above the card's overlay anchor so it stays independently clickable. */}
        <a
          href={repoUrl(repo, gitlabLinks)}
          target="_blank"
          rel="noopener noreferrer"
          className="relative inline-flex items-center gap-1 text-primary transition-colors hover:underline"
        >
          {viewLabel}
          <ExternalLink className="size-3 shrink-0" />
        </a>
      </div>
    </Card>
  );
}
