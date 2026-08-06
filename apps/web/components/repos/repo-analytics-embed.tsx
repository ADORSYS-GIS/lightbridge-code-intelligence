import { StatusLine } from "@/components/ui/states";

// Grafana embed coordinates for the generated per-repo analytics panels (story #516), mirroring
// run-logs-embed.tsx's `d-solo` pattern (ADR-0102) with `var-repo` instead of `var-task_id`.
//   - dashboardUid: the `UID` constant in the matching
//     tools/dashboard-gen/lci_dashboards/<name>.py module.
//   - panelId: pinned to 100 in each generator (the same convention task_runs.py's logs panel
//     uses), so the embed URL is deterministic regardless of panel order.
const GRAFANA_THEME = "dark";

export interface RepoAnalyticsPanel {
  dashboardUid: string;
  dashboardSlug: string;
  panelId: number;
  title: string;
}

/**
 * The panels currently safe to embed per repo: both are Loki-sourced and genuinely scoped by their
 * dashboard's own `$repo` template variable. The Postgres-sourced findings/reactions panels on
 * `review-quality` are NOT scoped by `$repo` (see `review_quality.py`'s module docstring) and would
 * silently show every repo's data if embedded the same way, so they're left out until that gap is
 * closed with its own verified query change. `review-runs` has no `$repo` variable at all yet.
 */
export const REPO_ANALYTICS_PANELS: RepoAnalyticsPanel[] = [
  {
    dashboardUid: "lci-review-cost",
    dashboardSlug: "review-cost",
    panelId: 100,
    title: "Billed cost (30d)",
  },
  {
    dashboardUid: "lci-review-quality",
    dashboardSlug: "review-quality",
    panelId: 100,
    title: "Tokens used (30d)",
  },
];

/**
 * One embedded Grafana panel scoped to a repo via `var-repo`, rendered chromeless (`d-solo` +
 * `kiosk`). Same graceful-fallback posture as `RunLogsEmbed`: when `NEXT_PUBLIC_GRAFANA_URL` is
 * unset — the default in CI and local dev — this renders a status note instead of a broken iframe.
 */
export function RepoAnalyticsEmbed({ repo, panel }: { repo: string; panel: RepoAnalyticsPanel }) {
  const base = process.env.NEXT_PUBLIC_GRAFANA_URL?.trim().replace(/\/+$/, "");
  if (!base) {
    return (
      <StatusLine>
        Set <code className="font-mono">NEXT_PUBLIC_GRAFANA_URL</code> to embed {panel.title} from
        Grafana.
      </StatusLine>
    );
  }

  const src =
    `${base}/d-solo/${panel.dashboardUid}/${panel.dashboardSlug}` +
    `?orgId=1&panelId=${panel.panelId}` +
    `&var-repo=${encodeURIComponent(repo)}&theme=${GRAFANA_THEME}&kiosk`;

  return (
    <iframe
      src={src}
      title={panel.title}
      frameBorder="0"
      className="w-full rounded-md border border-base-content/15 bg-base-200"
      style={{ minHeight: 180 }}
    />
  );
}
