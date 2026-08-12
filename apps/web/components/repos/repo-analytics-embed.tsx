import { StatusLine } from "@/components/ui/states";

// Grafana embed coordinates for the generated per-repo analytics panels (story #516), mirroring
// run-logs-embed.tsx's `d-solo` pattern (ADR-0102) with `var-repo` instead of `var-task_id`.
//   - dashboardUid: the `UID` constant in the matching
//     tools/dashboard-gen/lci_dashboards/<name>.py module.
//   - panelId: pinned to 100 in each generator (the same convention task_runs.py's logs panel
//     uses), so the embed URL is deterministic regardless of panel order.
const GRAFANA_THEME = "dark";

// Both embedded panels read TWO template variables — `$repo` and `$model` — and the window comes
// from the dashboard's saved time range. A `d-solo` embed has no variable picker and no range
// picker, so anything left unset is whatever Grafana happens to resolve it to, with no way for the
// viewer to see or correct it. Pin every input the query depends on:
//
//   - `var-model=.+` — `$model`'s "All" sentinel (`include_all` + `all_value=".+"` in both
//     generators). Grafana does resolve it to All today when no value is supplied — verified live:
//     the embedded cost panel reports the same figure as the All-models query. But that relies on
//     Grafana's default-selection behaviour AND on nobody saving a narrower model onto the shared
//     dashboard; either changing would silently scope every embed to one model. On real data that
//     is the difference between $72 and $2 for the same repo and window — a wrong number rendered
//     as a normal stat, with no error.
//   - `from`/`to` — the dashboards currently save `now-30d`, which is what makes the "(30d)" in
//     the titles below true. Pinning it here keeps that claim true regardless of later edits to
//     the dashboards' saved range.
const GRAFANA_ALL_MODELS = ".+";
const GRAFANA_RANGE = { from: "now-30d", to: "now" } as const;

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
// `title` is the iframe's accessible name and the wording of the unset-URL fallback. It stays free
// of any window claim on purpose: the panel rendered inside the iframe carries Grafana's own title
// ("Billed cost (range)"), and a label here reading "(30d)" would contradict the "(range)" a sighted
// viewer actually reads. The window is stated once, on the card heading in the repo detail page.
export const REPO_ANALYTICS_PANELS: RepoAnalyticsPanel[] = [
  {
    dashboardUid: "lci-review-cost",
    dashboardSlug: "review-cost",
    panelId: 100,
    title: "Billed cost",
  },
  {
    dashboardUid: "lci-review-quality",
    dashboardSlug: "review-quality",
    panelId: 100,
    title: "Tokens used",
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
    `&var-repo=${encodeURIComponent(repo)}` +
    `&var-model=${encodeURIComponent(GRAFANA_ALL_MODELS)}` +
    `&from=${GRAFANA_RANGE.from}&to=${GRAFANA_RANGE.to}` +
    `&theme=${GRAFANA_THEME}&kiosk`;

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
