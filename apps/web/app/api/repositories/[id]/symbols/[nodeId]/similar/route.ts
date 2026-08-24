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

  const result = await getSimilarSymbols(id, decodeURIComponent(nodeId), {
    limit: limit ? Number(limit) : undefined,
  });

  if (!result.ok) {
    const status = result.status ?? (result.reason === "unauthenticated" ? 401 : 502);
    return NextResponse.json({ code: result.reason, detail: result.detail }, { status });
  }
  return NextResponse.json(result.data);
}
