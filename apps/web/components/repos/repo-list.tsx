"use client";

import { ExternalLink, GitBranch, Settings } from "lucide-react";
import Link from "next/link";
import { useQueryStates } from "nuqs";
import { useMemo } from "react";
import { Card } from "@/components/ui/card";
import { SearchInput } from "@/components/ui/search-input";
import { Pill } from "@/components/ui/status-pill";
import type { GitlabLinkConfig } from "@/lib/domain/gitlab-links";
import { approvalVisual, type Repository, repoSlug, repoUrl } from "@/lib/domain/repos";
import { relativeTime } from "@/lib/domain/tasks";
import type { RepositoriesCursor } from "@/lib/server/api";

/** Connected repositories as cards with a search box + a cursor pager (ADR-0024, daisyUI in
 * ADR-0027). Search and cursor live in the URL via nuqs and are read back by the Server Component,
 * so both the query and the page are server-side — this list only ever holds one page.
 * `now` is server-passed so relative times don't drift on hydration.
 * `gitlabLinks` is passed from the Server Component for self-hosted GitLab links. */
export function RepoList({
  repos,
  next,
  now,
  gitlabLinks,
  overrideRepoIds,
}: {
  repos: Repository[];
  /** Where the next page starts, or null on the last page. */
  next: RepositoriesCursor | null;
  now: number;
  gitlabLinks: GitlabLinkConfig;
  /** Repos with at least one ADR-0111 setting resolved from a DB admin override (epic #566). */
  overrideRepoIds: number[];
}) {
  const overrideSet = useMemo(() => new Set(overrideRepoIds), [overrideRepoIds]);
  // One state object so a search always clears the cursor: a cursor names a row in the previous
  // result set, and keeping it across a new search would resume mid-list.
  //
  // `shallow: false` on every param: this list's search and cursor drive a real server-side fetch
  // (`listRepositoriesPage`), so a URL-only change (nuqs' default `shallow: true`) would update the
  // address bar without ever re-invoking the Server Component that owns the data — Next/First page
  // would silently no-op. (Search moved server-side in the same change that added the cursor; this
  // was client-side-only before that.)
  const [{ q, after_activity_at }, setParams] = useQueryStates({
    q: { defaultValue: "", parse: String, clearOnDefault: true, shallow: false },
    after_activity_at: { defaultValue: "", parse: String, clearOnDefault: true, shallow: false },
    after_id: { defaultValue: "", parse: String, clearOnDefault: true, shallow: false },
  });

  return (
    <div className="flex flex-col gap-3">
      <SearchInput
        value={q}
        onChange={(e) => setParams({ q: e.target.value, after_activity_at: null, after_id: null })}
        placeholder="Search repositories"
        aria-label="Search repositories"
        className="w-full sm:w-72"
      />

      {repos.length === 0 ? (
        <p className="px-1 py-6 text-sm text-base-content/60">
          {q ? `No repositories match “${q}”.` : "No more repositories."}
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

      {(after_activity_at || next) && (
        <div className="flex items-center justify-end gap-2 text-xs text-base-content/60">
          <button
            type="button"
            className="btn btn-xs"
            disabled={!after_activity_at}
            onClick={() => setParams({ after_activity_at: null, after_id: null })}
          >
            First page
          </button>
          <button
            type="button"
            className="btn btn-xs"
            disabled={!next}
            onClick={() =>
              next &&
              setParams({
                after_activity_at: next.after_activity_at,
                after_id: String(next.after_id),
              })
            }
          >
            Next
          </button>
        </div>
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
