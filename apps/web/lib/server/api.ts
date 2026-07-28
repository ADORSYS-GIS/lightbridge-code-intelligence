import { SESSION_COOKIE } from "@lightbridge/auth";
import { cookies } from "next/headers";
import type { Repository } from "@/lib/domain/repos";
import type { Review, Task } from "@/lib/domain/tasks";

/**
 * Server-side client for the control plane's read API (resource server). Runs only in Server
 * Components / route handlers: it reads the httpOnly session cookie and forwards the OIDC access
 * token as a Bearer credential — the same token the control plane validates (ADR-0014).
 */
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

/** `GET /tasks` — the run list (most recent first). */
export async function listTasks(): Promise<ApiResult<Task[]>> {
  try {
    const res = await authedFetch("/tasks");
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    // Inside the try: a non-JSON body / dropped connection makes res.json() throw too.
    return { ok: true, data: (await res.json()) as Task[] };
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

/** `GET /repositories` — connected repositories + run activity. */
export async function listRepositories(): Promise<ApiResult<Repository[]>> {
  try {
    const res = await authedFetch("/repositories");
    if (!res) return { ok: false, reason: "unauthenticated" };
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as Repository[] };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}
