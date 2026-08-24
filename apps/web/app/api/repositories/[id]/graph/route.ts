import { NextResponse } from "next/server";
import { getRepoGraph } from "@/lib/server/admin";

/**
 * Client-fetchable proxy for `getRepoGraph` (`lib/server/admin.ts`). The code graph canvas is a
 * client component — it needs to fetch on node click without a page navigation — but the admin API
 * client is server-only (it reads the httpOnly session cookie via `next/headers`). This route runs
 * server-side, forwards the same session, and returns plain JSON the browser can `fetch()` same-origin
 * (the session cookie rides along automatically, no token handling needed client-side).
 */
export async function GET(request: Request, { params }: { params: Promise<{ id: string }> }) {
  const { id: rawId } = await params;
  const id = Number(rawId);
  if (!Number.isInteger(id) || id <= 0) {
    return NextResponse.json({ error: "invalid repository id" }, { status: 400 });
  }
  const url = new URL(request.url);
  const node = url.searchParams.get("node") ?? undefined;
  const hops = url.searchParams.get("hops");
  const limit = url.searchParams.get("limit");

  const result = await getRepoGraph(id, {
    node,
    hops: hops ? Number(hops) : undefined,
    limit: limit ? Number(limit) : undefined,
  });

  if (!result.ok) {
    const status = result.status ?? (result.reason === "unauthenticated" ? 401 : 502);
    return NextResponse.json({ code: result.reason, detail: result.detail }, { status });
  }
  return NextResponse.json(result.data);
}
