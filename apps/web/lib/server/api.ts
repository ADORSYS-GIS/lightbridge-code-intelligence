import { SESSION_COOKIE } from "@lightbridge/auth";
import { cookies } from "next/headers";
import type { Repository } from "@/lib/domain/repos";
import type { Review, Task } from "@/lib/domain/tasks";

/**
 * Server-side client for the control plane's read API (resource server). Runs only in Server
 * Components / route handlers: it reads the httpOnly session cookie and forwards the OIDC access
 * token as a Bearer credential — the same token the control plane validates (ADR-0014).
 */

/** Control-plane base URL. `AUTH_BACKEND_URL` is set by the Helm chart and must include the
 * `/api/v2` prefix (e.g. `http://control-plane:8080/api/v2` — ADR-0109). The localhost fallback
 * includes the prefix so local dev without an explicit env override works out of the box. */
function controlPlaneUrl(): string {
  return (
    process.env.CONTROL_PLANE_URL ??
    process.env.AUTH_BACKEND_URL ??
    "http://localhost:8080/api/v2"
  ).replace(/\/+$/, "");
}

/** Discriminated result so pages can render honest states instead of throwing. */
export type ApiResult<T> =
  | { ok: true; data: T }
  | { ok: false; reason: "unauthenticated" | "unavailable" | "error"; status?: number };

async function authedFetch(path: string): Promise<Response | null> {
  const token = (await cookies()).get(SESSION_COOKIE)?.value;
  if (!token) return null;
  return fetch(`${controlPlaneUrl()}${path}`, {
    headers: { authorization: `Bearer ${token}`, accept: "application/json" },
    // Task state changes server-side; never serve a stale cache.
    cache: "no-store",
  });
}

function classify(status: number): "unauthenticated" | "unavailable" | "error" {
  if (status === 401 || status === 403) return "unauthenticated";
  if (status === 503) return "unavailable";
  return "error";
}

/** `GET /tasks`'s response envelope (real pagination, control-plane #587): a page of tasks plus the
 * total count of rows matching the current filters (not just this page's length). */
interface TasksPageResponse {
  tasks: Task[];
  total: number;
}

/** `GET /tasks` with no params — the most recent 100 runs, unfiltered. Used by the Overview page's
 * insights, which need a plain "most recent N" batch, not a filtered/paginated window; this keeps
 * that exact call site's signature/behavior unchanged even though the control plane's response
 * envelope grew a `total` alongside `tasks` (unwrapped here so callers never see it). See
 * `listTasksPage` for the Runs page's real pagination + filtering. */
export async function listTasks(): Promise<ApiResult<Task[]>> {
  try {
    const res = await authedFetch("/tasks");
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    // Inside the try: a non-JSON body / dropped connection makes res.json() throw too.
    const body = (await res.json()) as TasksPageResponse;
    return { ok: true, data: body.tasks };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** Status filter accepted by `GET /tasks` — the same `StatusVariant` values the UI already renders
 * with (`statusVisual` in `lib/domain/tasks.ts`); `"all"` is sent as omitted (no filter). */
export type TasksStatusFilter = "active" | "pending" | "success" | "error" | "muted";

export interface TasksPageParams {
  /** 0-based page index. */
  page: number;
  pageSize: number;
  status?: TasksStatusFilter;
  repositoryId?: number;
  q?: string;
}

/** `GET /tasks?page=&page_size=&status=&repository_id=&q=` — real server-side pagination +
 * filtering for the Runs page (control-plane #587). */
export async function listTasksPage(
  params: TasksPageParams,
): Promise<ApiResult<TasksPageResponse>> {
  const query = new URLSearchParams({
    page: String(params.page),
    page_size: String(params.pageSize),
  });
  if (params.status) query.set("status", params.status);
  if (params.repositoryId !== undefined) query.set("repository_id", String(params.repositoryId));
  if (params.q) query.set("q", params.q);

  try {
    const res = await authedFetch(`/tasks?${query.toString()}`);
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as TasksPageResponse };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** `GET /tasks/{id}` — a single run, or `null` data on 404. */
export async function getTask(id: string): Promise<ApiResult<Task | null>> {
  try {
    const res = await authedFetch(`/tasks/${encodeURIComponent(id)}`);
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (res.status === 404) return { ok: true, data: null };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as Task };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** `GET /tasks/{id}/review` — the persisted review for a run, or `null` data when none recorded. */
export async function getReview(id: string): Promise<ApiResult<Review | null>> {
  try {
    const res = await authedFetch(`/tasks/${encodeURIComponent(id)}/review`);
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (res.status === 404) return { ok: true, data: null };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as Review };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** `POST /tasks/{id}/cancel` — manually cancel an active run. `data` is null on success. */
export async function cancelTask(id: string): Promise<ApiResult<null>> {
  try {
    const token = (await cookies()).get(SESSION_COOKIE)?.value;
    if (!token) return { ok: false, reason: "unauthenticated" };
    const res = await fetch(`${controlPlaneUrl()}/tasks/${encodeURIComponent(id)}/cancel`, {
      method: "POST",
      headers: { authorization: `Bearer ${token}`, accept: "application/json" },
      cache: "no-store",
    });
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: null };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** A `GET /repositories` page boundary: the `last_task_at` and `id` of the row it points at.
 * Carries no direction of its own — `next` is where a follow-up request should send back as
 * `after`, `prev` is where it should send back as `before`. */
export interface RepositoriesCursor {
  activity_at: string;
  id: number;
}

/** `GET /repositories`' response envelope: one page, the count matching the current search (for a
 * "1–12 of 357" label), and where to continue in each direction (`null` at either edge). */
export interface RepositoriesPageResponse {
  repositories: Repository[];
  total: number;
  next: RepositoriesCursor | null;
  prev: RepositoriesCursor | null;
}

export interface RepositoriesPageParams {
  pageSize: number;
  /** Matches `owner/name`, case-insensitively. */
  q?: string;
  /** Continue forward from a previous page's `next`. Mutually exclusive with `before`. */
  after?: RepositoriesCursor;
  /** Continue backward from a previous page's `prev`. Mutually exclusive with `after`. */
  before?: RepositoriesCursor;
}

/** `GET /repositories?page_size=&q=&after_activity_at=&after_id=` (or `before_*`) — connected
 * repositories + run activity, most-recently-active first, one page at a time. */
export async function listRepositoriesPage(
  params: RepositoriesPageParams,
): Promise<ApiResult<RepositoriesPageResponse>> {
  const query = new URLSearchParams({ page_size: String(params.pageSize) });
  if (params.q) query.set("q", params.q);
  if (params.after) {
    query.set("after_activity_at", params.after.activity_at);
    query.set("after_id", String(params.after.id));
  }
  if (params.before) {
    query.set("before_activity_at", params.before.activity_at);
    query.set("before_id", String(params.before.id));
  }

  try {
    const res = await authedFetch(`/repositories?${query.toString()}`);
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as RepositoriesPageResponse };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** One symbol node in a `GET /admin/repositories/{id}/graph` response. */
export interface GraphNode {
  node_id: string;
  label: string;
  source_file: string;
  start_line: number;
}

/** One `REL` edge in a `GET /admin/repositories/{id}/graph` response. */
export interface GraphEdge {
  source: string;
  target: string;
  relation: string;
}

export interface RepoGraph {
  commit: string;
  nodes: GraphNode[];
  edges: GraphEdge[];
}

/** `GET /admin/repositories/{id}/graph[?seed=&hops=]` — a bounded neighborhood of the repo's code
 * graph (ADR-0113 / #615). Omit `seed` for a deterministic default. `data: null` on 404, which
 * covers both "not yet indexed" and "empty graph" — the caller doesn't need to distinguish them. */
export async function getRepoGraph(
  id: number,
  seed?: string,
  hops = 2,
): Promise<ApiResult<RepoGraph | null>> {
  const query = new URLSearchParams({ hops: String(hops) });
  if (seed) query.set("seed", seed);
  try {
    const res = await authedFetch(`/admin/repositories/${id}/graph?${query.toString()}`);
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (res.status === 404) return { ok: true, data: null };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as RepoGraph };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** Non-sensitive deployment settings for the console (GitLab web base URL, etc.). */
export interface DeploymentConfig {
  gitlab_base_url: string;
  gitlab_project_base_urls: Record<string, string>;
}

/** `GET /config` — deployment settings for the console (sourced from the control plane, not env). */
export async function getDeploymentConfig(): Promise<ApiResult<DeploymentConfig>> {
  try {
    const res = await authedFetch("/config");
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as DeploymentConfig };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}
