import { NextResponse } from "next/server";
import { getSimilarSymbols } from "@/lib/server/admin";

/** Client-fetchable proxy for `getSimilarSymbols` — see `../../graph/route.ts` for why this proxy
 * layer exists (the client canvas needs live fetches; the admin API client is server-only). */
export async function GET(
  request: Request,
  { params }: { params: Promise<{ id: string; nodeId: string }> },
) {
  const { id: rawId, nodeId } = await params;
  const id = Number(rawId);
  if (!Number.isInteger(id) || id <= 0) {
    return NextResponse.json({ error: "invalid repository id" }, { status: 400 });
  }
  const url = new URL(request.url);
  const limit = url.searchParams.get("limit");

  // `nodeId` is already decoded by Next.js's own dynamic-route-segment handling — re-decoding it
  // here double-decoded anything the client had percent-escaped (e.g. `%2523` -> `%23` -> `#`
  // instead of `#`), which could throw URIError on a malformed sequence or silently resolve the
  // wrong node for an id containing a literal `%`.
  const result = await getSimilarSymbols(id, nodeId, {
    limit: limit ? Number(limit) : undefined,
  });

  if (!result.ok) {
    const status = result.status ?? (result.reason === "unauthenticated" ? 401 : 502);
    return NextResponse.json({ code: result.reason, detail: result.detail }, { status });
  }
  return NextResponse.json(result.data);
}
