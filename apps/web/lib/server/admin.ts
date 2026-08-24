import { SESSION_COOKIE, type SessionClaims } from "@lightbridge/auth";
import { cookies } from "next/headers";
import { cache } from "react";
import type { SettingsSource } from "@/components/ui/source-badge";
import type { Repository } from "@/lib/domain/repos";
import type { ApiResult } from "@/lib/server/api";

/**
 * Server-only client for the control plane's **admin** API (the approval gate, Epic #75). Like
 * lib/api it forwards the session's OIDC token; the control plane enforces the admin realm role
 * (returns 403 for non-admins). Used by the admin approval screen.
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

/** The dotted claim path the caller's permissions live under (ADR-0023). Mirrors the control plane's
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

/**
 * One repository by id, or `null` data when no repository carries that id. There is no single-repo
 * GET endpoint control-plane-side, so this narrows the list call — the list is small (an admin
 * console, not a customer-facing catalog), which makes finding the row locally the pragmatic choice
 * over adding an endpoint for the one or two fields a detail view needs.
 *
 * Wrapped in React's `cache` because a repository's own segment resolves it more than once per
 * request — the surrounding chrome needs its name and approval state, the view inside needs it
 * again. Every fetch here is `no-store`, so nothing else would collapse those into one call.
 */
export const getAdminRepo = cache(async (id: number): Promise<ApiResult<Repository | null>> => {
  const result = await listAdminRepos();
  if (!result.ok) return result;
  return { ok: true, data: result.data.find((repo) => repo.id === id) ?? null };
});

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

/** Shared approve/deny mutation: permission-checks the caller on `repo:{action}`, validates `id`,
 * then calls [`setRepoStatus`]. Used by both the admin approval queue and the per-repo detail page
 * (story #514) so the two surfaces can't drift on the permission check or the failure message. */
export async function mutateRepoApproval(
  claims: SessionClaims | null,
  id: number,
  action: "approve" | "deny",
): Promise<{ ok: true } | { ok: false; error: string }> {
  if (!hasPermission(claims, `repo:${action}`)) {
    return { ok: false, error: `Unauthorized: repo:${action} permission required` };
  }
  if (!Number.isInteger(id) || id <= 0) {
    return { ok: false, error: "Invalid repository id" };
  }
  if (!(await setRepoStatus(id, action))) {
    return { ok: false, error: `Failed to ${action} repository` };
  }
  return { ok: true };
}

/** A repo's currently-configured review preset (story #500, ADR-0109). Mirrors
 * `GET`/`POST /admin/repositories/{id}/preset`'s JSON shape. */
export interface RepoPreset {
  preset: string | null;
  entry_points: Record<string, string>;
}

/** `GET /admin/repositories/{id}/preset` — the repo's currently-configured preset, read straight from
 * `.lightbridge-code-review.jsonc`. `null`/`{}` when the repo declares nothing (platform defaults
 * apply). Needs `repo:read`. */
export async function getRepoPreset(id: number): Promise<ApiResult<RepoPreset>> {
  try {
    const t = await token();
    if (!t) return { ok: false, reason: "unauthenticated" };
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/preset`, {
      headers: { authorization: `Bearer ${t}`, accept: "application/json" },
      cache: "no-store",
    });
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as RepoPreset };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** A value resolved across ADR-0111's three-layer chain: built-in default → repo config file → admin
 * DB override (wins). Mirrors `services/control-plane/src/settings.rs`'s `Sourced<T>`. */
export interface Sourced<T> {
  value: T;
  source: SettingsSource;
}

/** `GET`/`POST /admin/repositories/{id}/settings[/override]`'s resolved shape (epic #566, ADR-0111).
 * `push_debounce` mirrors `std::time::Duration`'s serde shape verbatim (`{secs, nanos}` — confirmed
 * live against the actual type, `nanos` is always `0` from this API and never surfaced in the UI). */
export interface ResolvedSettings {
  check_run_reporting: Sourced<boolean>;
  review_on_pr_open: Sourced<boolean>;
  review_on_push: Sourced<boolean>;
  push_strategy: Sourced<"supersede" | "debounce" | "every">;
  push_debounce: Sourced<{ secs: number; nanos: number }>;
  dedup_scope: Sourced<"pr" | "commit">;
}

/** `GET /admin/repositories/{id}/settings` — the repo's effective per-repo review settings, with
 * provenance. Needs `repo:read`. */
export async function getRepoSettings(
  id: number,
): Promise<ApiResult<{ repository_id: number; settings: ResolvedSettings }>> {
  try {
    const t = await token();
    if (!t) return { ok: false, reason: "unauthenticated" };
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/settings`, {
      headers: { authorization: `Bearer ${t}`, accept: "application/json" },
      cache: "no-store",
    });
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return {
      ok: true,
      data: (await res.json()) as { repository_id: number; settings: ResolvedSettings },
    };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** Mirrors the control plane's `SetSettingsBody` — each field omitted leaves the stored value alone,
 * `null` clears the DB override (reverting to file/default), a value sets it. Never partially applies:
 * the control plane validates every provided field before writing any of them. */
export interface RepoSettingsPatch {
  check_run_reporting?: boolean | null;
  review_on_pr_open?: boolean | null;
  review_on_push?: boolean | null;
  push_strategy?: "supersede" | "debounce" | "every" | null;
  push_debounce_seconds?: number | null;
  dedup_scope?: "pr" | "commit" | null;
}

/** `POST /admin/repositories/{id}/settings/override` — set or clear one or more DB-layer setting
 * overrides. Needs `repo:configure`. Returns whether it succeeded; the caller re-fetches to reflect
 * the change (same convention as [`setRepoPreset`]). */
export async function setRepoSettingsOverride(
  id: number,
  patch: RepoSettingsPatch,
): Promise<boolean> {
  const t = await token();
  if (!t) return false;
  try {
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/settings/override`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${t}`,
        accept: "application/json",
        "content-type": "application/json",
      },
      body: JSON.stringify(patch),
      cache: "no-store",
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** `POST /admin/repositories/{id}/preset` — commit a new preset to the repo's
 * `.lightbridge-code-review.jsonc` on its default branch (a direct commit, ADR-0109). Needs
 * `repo:configure`. Returns whether it succeeded; the caller re-fetches to reflect the change. */
export async function setRepoPreset(id: number, preset: string): Promise<boolean> {
  const t = await token();
  if (!t) return false;
  try {
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/preset`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${t}`,
        accept: "application/json",
        "content-type": "application/json",
      },
      body: JSON.stringify({ preset }),
      cache: "no-store",
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** A repo's effective model-override provenance (ADR-0110). Unlike the six ADR-0111 settings, there is
 * no repo-config-file layer for models — `source` is `"repo"` (this repo's own override),
 * `"org"` (falls through to the installation-wide override), or `"none"` (the preset's own configured
 * model applies, untouched). Mirrors `GET /admin/repositories/{id}/model`'s JSON shape exactly. */
export interface RepoModelOverride {
  repository_id: number;
  model: string | null;
  source: "repo" | "org" | "none";
}

/** `GET /admin/repositories/{id}/model` — needs `repo:read`. */
export async function getRepoModel(id: number): Promise<ApiResult<RepoModelOverride>> {
  try {
    const t = await token();
    if (!t) return { ok: false, reason: "unauthenticated" };
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/model`, {
      headers: { authorization: `Bearer ${t}`, accept: "application/json" },
      cache: "no-store",
    });
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as RepoModelOverride };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** `GET /admin/models` — the operator-curated model allowlist (ADR-0110). Needs `repo:read`. Reference
 * data for the picker below; the control plane re-validates on write regardless. */
export async function getModelAllowlist(): Promise<ApiResult<string[]>> {
  try {
    const t = await token();
    if (!t) return { ok: false, reason: "unauthenticated" };
    const res = await fetch(`${controlPlaneUrl()}/admin/models`, {
      headers: { authorization: `Bearer ${t}`, accept: "application/json" },
      cache: "no-store",
    });
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as string[] };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** `POST /admin/repositories/{id}/model` — set (`model` provided) or clear (`model: null`) this repo's
 * model override. Needs `model:configure` — a distinct permission from `repo:configure`, since model
 * selection is operator-cost-relevant (ADR-0110), not a repo-owner setting. Returns whether it
 * succeeded; the caller re-fetches to reflect the change. */
export async function setRepoModel(id: number, model: string | null): Promise<boolean> {
  const t = await token();
  if (!t) return false;
  try {
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/model`, {
      method: "POST",
      headers: {
        authorization: `Bearer ${t}`,
        accept: "application/json",
        "content-type": "application/json",
      },
      body: JSON.stringify({ model }),
      cache: "no-store",
    });
    return res.ok;
  } catch {
    return false;
  }
}

/** One graph node, as returned by every code-graph endpoint below. */
export interface GraphSymbol {
  node_id: string;
  label: string;
  source_file: string;
  start_line: number;
}

/** One directed structural edge between two [`GraphSymbol`]s. */
export interface GraphRel {
  source: string;
  target: string;
  relation: string;
}

/** Shared response shape for every code-graph endpoint (mirrors control-plane's `GraphResponse`). */
export interface GraphResponse {
  commit: string;
  nodes: GraphSymbol[];
  edges: GraphRel[];
}

/**
 * `ApiResult`, but the two graph endpoints below need two more reasons `classify()`'s generic
 * bucket doesn't have: a `404` from either one is a real, meaningful answer ("this repository/symbol
 * doesn't have what you asked for"), not an infrastructure failure — collapsing it into the shared
 * `"error"` bucket is what previously made a disclosed, expected case (a symbol with no stored
 * embedding, ADR-0114's coverage is not 100%) indistinguishable from an actual 500. `detail` carries
 * control-plane's own plain-text body so the UI can show real copy instead of a bare reason code.
 */
export type GraphApiResult<T> =
  | { ok: true; data: T }
  | {
      ok: false;
      reason: "unauthenticated" | "unavailable" | "error" | "not_found" | "no_embedding";
      status?: number;
      detail?: string;
    };

/** `GET /admin/repositories/{id}/graph[?node=&hops=&limit=]` — structural neighborhood browse.
 * `node` omitted returns an unseeded overview slice so the graph view is never empty on first load.
 * Needs `repo:read`. */
export async function getRepoGraph(
  id: number,
  opts?: { node?: string; hops?: number; limit?: number },
): Promise<GraphApiResult<GraphResponse>> {
  try {
    const t = await token();
    if (!t) return { ok: false, reason: "unauthenticated" };
    const qs = new URLSearchParams();
    if (opts?.node) qs.set("node", opts.node);
    if (opts?.hops) qs.set("hops", String(opts.hops));
    if (opts?.limit) qs.set("limit", String(opts.limit));
    const suffix = qs.size > 0 ? `?${qs.toString()}` : "";
    const res = await fetch(`${controlPlaneUrl()}/admin/repositories/${id}/graph${suffix}`, {
      headers: { authorization: `Bearer ${t}`, accept: "application/json" },
      cache: "no-store",
    });
    if (res.status === 404) {
      return {
        ok: false,
        reason: "not_found",
        status: 404,
        detail: await res.text().catch(() => undefined),
      };
    }
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as GraphResponse };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}

/** `GET /admin/repositories/{id}/symbols/{nodeId}/similar[?limit=]` — symbols found by meaning,
 * using `nodeId`'s own already-stored embedding as the query vector (no text is ever embedded at
 * request time). `404` when the symbol has no stored embedding (ADR-0114's coverage is not 100%) —
 * control-plane's own response body says so in plain text; `detail` forwards it verbatim.
 * Needs `repo:read`. */
export async function getSimilarSymbols(
  id: number,
  nodeId: string,
  opts?: { limit?: number },
): Promise<GraphApiResult<GraphResponse>> {
  try {
    const t = await token();
    if (!t) return { ok: false, reason: "unauthenticated" };
    const qs = opts?.limit ? `?limit=${opts.limit}` : "";
    const res = await fetch(
      `${controlPlaneUrl()}/admin/repositories/${id}/symbols/${encodeURIComponent(nodeId)}/similar${qs}`,
      {
        headers: { authorization: `Bearer ${t}`, accept: "application/json" },
        cache: "no-store",
      },
    );
    if (res.status === 404) {
      return {
        ok: false,
        reason: "no_embedding",
        status: 404,
        detail: await res.text().catch(() => undefined),
      };
    }
    if (!res.ok) return { ok: false, reason: classify(res.status), status: res.status };
    return { ok: true, data: (await res.json()) as GraphResponse };
  } catch {
    return { ok: false, reason: "unavailable" };
  }
}
