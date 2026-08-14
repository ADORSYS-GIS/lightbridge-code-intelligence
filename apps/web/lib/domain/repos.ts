/**
 * Repository domain types for the Repositories view (ADR-0016). Mirrors the control plane's
 * `/repositories` payload (`RepositoryRow` in `services/control-plane/src/db.rs`). Edge-safe.
 */

import { type GitlabLinkConfig, gitlabBaseUrlForProject } from "@/lib/domain/gitlab-links";
import type { StatusVariant } from "@/lib/domain/tasks";

/** Repositories per page. Shared by the Server Component that fetches the page and the client
 * `RepoList` that renders it — the same reason `RUNS_PAGE_SIZE` lives beside its view rather than
 * inside it (a `"use client"` module cannot export plain values to a Server Component). */
export const REPOS_PAGE_SIZE = 12;

/** Repositories offered in the Runs page's repository filter. One page is the whole dropdown: a
 * `<select>` longer than this is unusable anyway, and it keeps the filter to a single request. */
export const REPO_FILTER_LIMIT = 100;

/** A connected repository plus its run-activity summary. */
export interface Repository {
  id: number;
  /** Platform-agnostic numeric repository ID. */
  platform_repo_id: number;
  platform: "github" | "gitlab";
  owner: string;
  name: string;
  default_branch: string;
  /** Approval gate (Epic #75): `pending` | `approved` | `disabled`. `active` mirrors `approved`. */
  status: string;
  active: boolean;
  approved_at: string | null;
  approved_by: string | null;
  task_count: number;
  /** ISO timestamp of the most recent run, or null if none yet. */
  last_task_at: string | null;
}

/** `owner/name` slug. */
export function repoSlug(repo: Repository): string {
  return `${repo.owner}/${repo.name}`;
}

/** Platform-specific URL of the repository. */
export function repoUrl(repo: Repository, gitlab: GitlabLinkConfig): string {
  switch (repo.platform) {
    case "gitlab":
      return `${gitlabBaseUrlForProject(gitlab, repo.platform_repo_id)}/${repo.owner}/${repo.name}`;
    default:
      return `https://github.com/${repo.owner}/${repo.name}`;
  }
}

/** Map the approval `status` (Epic #75) to a status-pill variant + label (ADR-0015/0016 tokens). */
export function approvalVisual(repo: Repository): { variant: StatusVariant; label: string } {
  switch (repo.status) {
    case "approved":
      return { variant: "success", label: "Approved" };
    case "disabled":
      return { variant: "muted", label: "Disabled" };
    case "pending":
      return { variant: "pending", label: "Pending approval" };
    default:
      return { variant: "pending", label: repo.status };
  }
}
