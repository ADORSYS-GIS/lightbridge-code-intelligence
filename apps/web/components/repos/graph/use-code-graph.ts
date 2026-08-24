"use client";

import { useCallback, useEffect, useState } from "react";
import type { GraphResponse } from "@/lib/server/admin";

export type GraphMode =
  | { kind: "browse"; nodeId?: string; hops: number }
  | { kind: "similar"; nodeId: string };

interface State {
  data: GraphResponse | null;
  loading: boolean;
  error: string | null;
}

/** Drives the code-graph canvas: fetches either a structural neighborhood/overview (Tier 1, "browse")
 * or a "find similar" result set (Tier 2, "similar") from the same-origin API proxy routes, and
 * re-fetches whenever `mode` changes (clicking a node, expanding hops, choosing "find similar"). */
export function useCodeGraph(repoId: number, mode: GraphMode) {
  const [state, setState] = useState<State>({ data: null, loading: true, error: null });

  const load = useCallback(async () => {
    setState((s) => ({ ...s, loading: true, error: null }));
    try {
      const url =
        mode.kind === "similar"
          ? `/api/repositories/${repoId}/symbols/${encodeURIComponent(mode.nodeId)}/similar`
          : (() => {
              const qs = new URLSearchParams();
              if (mode.nodeId) qs.set("node", mode.nodeId);
              qs.set("hops", String(mode.hops));
              return `/api/repositories/${repoId}/graph?${qs.toString()}`;
            })();
      const res = await fetch(url);
      if (!res.ok) {
        const body = (await res.json().catch(() => null)) as { error?: string } | null;
        setState({
          data: null,
          loading: false,
          error: body?.error ?? `request failed (${res.status})`,
        });
        return;
      }
      const data = (await res.json()) as GraphResponse;
      setState({ data, loading: false, error: null });
    } catch {
      setState({ data: null, loading: false, error: "network error" });
    }
  }, [repoId, mode]);

  useEffect(() => {
    load();
  }, [load]);

  return { ...state, reload: load };
}
