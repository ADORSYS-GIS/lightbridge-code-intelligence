import { SESSION_COOKIE, type SessionClaims } from "@lightbridge/auth";
import { cookies } from "next/headers";
import type { Repository } from "@/lib/domain/repos";
import type { ApiResult } from "@/lib/server/api";

/**
 * Server-only client for the control plane's admin API (the approval gate). Like lib/api it
 * forwards the session's OIDC token; the control plane enforces the admin realm role.
 */

function controlPlaneUrl(): string {
  return (
    process.env.CONTROL_PLANE_URL ??
    process.env.AUTH_BACKEND_URL ??
    "http://localhost:8080/api/v2"
  ).replace(/\/+$/, "");
}

/** The dotted claim path the caller's permissions live under. Mirrors the control plane's
 * `PERMISSIONS_CLAIM`. */
export function permissionsClaim(): string {
  return process.env.PERMISSIONS_CLAIM?.trim() || "permissions";
}

/** The caller's permissions, read from the configured (possibly nested) claim path. Empty when the
 * claim is missing or not a string array. */
export function permissions(claims: SessionClaims | null): string[] {
  if (!claims) return [];
  let node: unknown = claims;
  for (const segment of permissionsClaim().split(".")) {
    if (node && typeof node === "object" && segment in (node as Record<string, unknown>)) {
      node = (node as Record<string, unknown>)[segment];
    } else {
      return [];
    }
  }
  return Array.isArray(node) ? node.filter((p): p is string => typeof p === "string") : [];
}

/** Does the caller hold `permission`? Gates the admin nav + screen (the control plane is the real
 * enforcement; this just avoids showing affordances that would only 403). */
export function hasPermission(claims: SessionClaims | null, permission: string): boolean {
  return permissions(claims).includes(permission);
}

function classify(status: number): "unauthenticated" | "unavailable" | "error" {
  if (status === 401 || status === 403) return "unauthenticated";
  if (status === 503) return "unavailable";
  return "error";
}

async function token(): Promise<string | null> {
  return (await cookies()).get(SESSION_COOKIE)?.value ?? null;
}

/** `GET /admin/repositories[?status=…]` — repositories for the admin console. Omit `status` to get
 * every repository (pending, approved, and disabled) so approvals are reversible from the UI. */
export async function listAdminRepos(status?: string): Promise<ApiResult<Repository[]>> {
  try {
    const t = await token();
    if (!t) return { ok: false, reason: "unauthenticated" };
    const qs = status ? `?status=${encodeURIComponent(status)}` : "";
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories${qs}`, {
      headers: { authorization: `Bearer ${t}`, accept: "application/json" },
      cache: "no-store",
    });
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as Repository[] };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** `POST /admin/repositories/{id}/{approve|deny}` — returns whether it succeeded. */
export async function setRepoStatus(id: number, action: "approve" | "deny"): Promise<boolean> {
  const t = await token();
  if (!t) return false;
  try {
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/${action}`, {
      method: "POST",
      headers: { authorization: `Bearer ${t}`, accept: "application/json" },
      cache: "no-store",
    });
    return res.ok;
  } catch {
    return false;
  }
}
