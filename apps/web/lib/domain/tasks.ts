/**
 * Task-run domain types + presentation logic for the dashboard (ADR-0016). Mirrors the control
 * plane's `/tasks` payload (`TaskRow` in `services/control-plane/src/db.rs`). Pure + Edge-safe.
 */

import { gitlabBaseUrl } from "@/lib/utils/config";

/** One task run, as returned by `GET /tasks` and `GET /tasks/{id}`. */
export interface Task {
  id: string;
  repository_id: number;
  installation_id: number;
  /** `null` for admin-initiated tasks (e.g. index-on-approve) that had no originating webhook. */
  webhook_delivery_id: string | null;
  target_type: string;
  target_id: number;
  command_text: string;
  base_sha: string | null;
  head_sha: string | null;
  status: string;
  priority: number;
  created_at: string;
  started_at: string | null;
  completed_at: string | null;
  repo_owner: string | null;
  repo_name: string | null;
  repo_default_branch: string | null;
  repo_platform: "github" | "gitlab" | null;
  /** The dispatched Kubernetes Job name, used to stream the run's logs. `null` before dispatch or
   * after the Job is cleaned up. */
  job_name: string | null;
  /** The runner's free-text status reason (#137). `null` for a genuine clean success / runs from
   * before this was persisted; non-null records why a run did not post a review (a failure reason,
   * or a "posted nothing" no-op) so a silent no-op is tellable apart from a real clean review. */
  error_detail: string | null;
}

/** One finding from the agent's review (mirrors `review::Finding`). */
export interface ReviewFinding {
  file: string;
  line: number;
  /** Triage priority `P0`|`P1`|`P2` (ADR-0032). Optional for rows that predate the priority model. */
  priority?: string;
  /** Dimension: `security`|`correctness`|`quality`|`style`|`performance` (ADR-0032). */
  category?: string;
  /** Legacy `error`|`warning`|`info` level (pre-ADR-0032). Read-only fallback for old rows. */
  severity?: string;
  title: string;
  body: string;
  suggestion?: string | null;
  /** Links to supporting resources (docs, CWE, RFCs). */
  resources?: string[];
}

/** A persisted review (`GET /tasks/{id}/review`, `ReviewRow` in db.rs). */
export interface Review {
  task_id: string;
  summary: string;
  body: string;
  inline_count: number;
  deferred_count: number;
  out_of_scope_count: number;
  findings: ReviewFinding[];
  /** Permalink to the posted review on the PR; null for older runs / if GitHub omitted it. */
  review_url: string | null;
  created_at: string;
}

/** The small visual set statuses collapse to (ADR-0015/0016 tokens). */
export type StatusVariant = "pending" | "active" | "success" | "error" | "muted";

/** Map a raw `TaskStatus` string (snake_case from the DB) to its visual variant + label. */
export function statusVisual(status: string): { variant: StatusVariant; label: string } {
  switch (status) {
    case "received":
      return { variant: "pending", label: "Received" };
    case "waiting_for_index":
      return { variant: "pending", label: "Waiting for index" };
    case "queued":
      return { variant: "pending", label: "Queued" };
    case "running":
      return { variant: "active", label: "Running" };
    case "posting_result":
      return { variant: "active", label: "Posting result" };
    case "succeeded":
      return { variant: "success", label: "Succeeded" };
    case "failed":
      return { variant: "error", label: "Failed" };
    case "timed_out":
      return { variant: "error", label: "Timed out" };
    case "cancelled":
      return { variant: "muted", label: "Cancelled" };
    default:
      // Unknown future status: show it verbatim rather than hiding it.
      return { variant: "pending", label: status };
  }
}

/** Human repo slug (`owner/name`), or a stable fallback when the join came back empty. */
export function repoLabel(task: Task): string {
  if (task.repo_owner && task.repo_name) return `${task.repo_owner}/${task.repo_name}`;
  return `repo #${task.repository_id}`;
}

/** Platform-specific URL of the task's repository, or `null` when the repo identity join came back empty. */
export function repoUrl(task: Task): string | null {
  if (!task.repo_owner || !task.repo_name) return null;
  switch (task.repo_platform) {
    case "gitlab":
      return `${gitlabBaseUrl()}/${task.repo_owner}/${task.repo_name}`;
    default:
      return `https://github.com/${task.repo_owner}/${task.repo_name}`;
  }
}

/** Platform-specific URL of the run's target — the PR or MR or issue — for a deep link; `null` when not applicable
 * (e.g. a `repository` index task, which has no PR/issue) or the repo identity is missing. */
export function targetUrl(task: Task): string | null {
  const base = repoUrl(task);
  if (!base) return null;
  switch (task.target_type) {
    case "pull_request":
      return task.repo_platform === "gitlab"
        ? `${base}/-/merge_requests/${task.target_id}`
        : `${base}/pull/${task.target_id}`;
    case "issue":
      return task.repo_platform === "gitlab"
        ? `${base}/-/issues/${task.target_id}`
        : `${base}/issues/${task.target_id}`;
    default:
      return null;
  }
}

/** What triggered the run, e.g. `review · PR #123`. */
export function triggerLabel(task: Task): string {
  const target =
    task.target_type === "pull_request"
      ? `PR #${task.target_id}`
      : `${task.target_type} #${task.target_id}`;
  return `${task.command_text} · ${target}`;
}

/** First 7 chars of a SHA (git short form), or null. */
export function shortSha(sha: string | null): string | null {
  return sha ? sha.slice(0, 7) : null;
}

const RELATIVE_UNITS: [Intl.RelativeTimeFormatUnit, number][] = [
  ["year", 31_536_000],
  ["month", 2_592_000],
  ["week", 604_800],
  ["day", 86_400],
  ["hour", 3600],
  ["minute", 60],
  ["second", 1],
];

// Reuse one formatter instance — constructing `Intl.*Format` is expensive and these run per row.
const RELATIVE_TIME_FORMATTER = new Intl.RelativeTimeFormat("en", { numeric: "auto" });
const ABSOLUTE_TIME_FORMATTER = new Intl.DateTimeFormat("en", {
  dateStyle: "medium",
  timeStyle: "short",
});

/** "3 minutes ago" from an ISO timestamp, relative to `now` (defaults to current time). */
export function relativeTime(iso: string, now: number = Date.now()): string {
  const time = new Date(iso).getTime();
  if (Number.isNaN(time)) return "unknown time";
  const seconds = Math.round((time - now) / 1000);
  const abs = Math.abs(seconds);
  for (const [unit, secondsInUnit] of RELATIVE_UNITS) {
    if (abs >= secondsInUnit || unit === "second") {
      return RELATIVE_TIME_FORMATTER.format(Math.round(seconds / secondsInUnit), unit);
    }
  }
  return RELATIVE_TIME_FORMATTER.format(0, "second");
}

/** Run duration in whole seconds (`started→completed`, or `started→now` if still running); `null`
 * when the run hasn't started or the timestamps don't parse. The numeric basis for sorting. */
export function durationSeconds(task: Task, now: number = Date.now()): number | null {
  if (!task.started_at) return null;
  const start = new Date(task.started_at).getTime();
  const end = task.completed_at ? new Date(task.completed_at).getTime() : now;
  if (Number.isNaN(start) || Number.isNaN(end)) return null;
  return Math.max(0, Math.round((end - start) / 1000));
}

/** Run duration `started→completed` (or `started→now` if still running), formatted compactly. */
export function duration(task: Task, now: number = Date.now()): string | null {
  const seconds = durationSeconds(task, now);
  if (seconds === null) return null;
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.floor(seconds / 60);
  const rem = seconds % 60;
  if (minutes < 60) return rem ? `${minutes}m ${rem}s` : `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}m`;
}

// UTC-based so a day bucket renders identically on the server and the client (no hydration drift);
// the trade-off is that "Today" tracks the UTC calendar day, not the viewer's local midnight.
const DAY_LABEL_FORMATTER = new Intl.DateTimeFormat("en", { dateStyle: "medium", timeZone: "UTC" });
const DAY_MS = 86_400_000;

/** Stable per-day grouping key (the UTC calendar date, `YYYY-MM-DD`) for the timeline view. */
export function dayBucketKey(iso: string): string {
  return iso.slice(0, 10);
}

/** Human label for a day bucket relative to `now`: `Today` / `Yesterday` / an absolute UTC date. */
export function dayBucketLabel(iso: string, now: number): string {
  const time = new Date(iso).getTime();
  if (Number.isNaN(time)) return "Unknown date";
  const startOfDay = (t: number) => Math.floor(t / DAY_MS);
  const delta = startOfDay(now) - startOfDay(time);
  if (delta <= 0) return "Today";
  if (delta === 1) return "Yesterday";
  return DAY_LABEL_FORMATTER.format(new Date(time));
}

/** Absolute timestamp for tooltips / detail rows. */
export function absoluteTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return "—";
  return ABSOLUTE_TIME_FORMATTER.format(date);
}
