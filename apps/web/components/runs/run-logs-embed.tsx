import { StatusLine } from "@/components/ui/states";

// Grafana embed coordinates for the generated `task-runs` dashboard's Loki logs panel (Epic #459 —
// replace the web pod's kube-client log stream with a Grafana/Loki embed).
//   - UID: the `UID` constant in tools/dashboard-gen/lci_dashboards/task_runs.py.
//   - LOGS_PANEL_ID: the panel titled "Logs for $task_id" in
//     deploy/observability/dashboards/task-runs.json. The generated dashboards don't emit explicit
//     panel ids; Grafana assigns them in panel-array order on load, and the logs panel is the 2nd
//     (last) panel → id 2. If a panel is ever added/reordered ahead of it, re-check the id via the
//     live panel's Share → Embed URL.
const GRAFANA_DASHBOARD_UID = "lci-task-runs";
const GRAFANA_LOGS_PANEL_ID = 2;

// The console is dark-only (daisyUI `dracula`, ADR-0027), so the embedded panel matches with the
// Grafana `dark` theme. If the app ever gains a light/dark toggle, thread the active theme here.
const GRAFANA_THEME = "dark";

/**
 * Embedded Grafana panel showing a run's structured logs from Loki (the `task-runs` dashboard's
 * "Logs for $task_id" panel, rendered chromeless via `d-solo` + `kiosk`).
 *
 * The base URL comes from `NEXT_PUBLIC_GRAFANA_URL` (client-visible, browser-reachable). When it is
 * unset — the default in CI and local dev — we render a status note rather than a broken iframe; the
 * `kubectl logs` snippet in the surrounding card remains the terminal fallback.
 *
 * For the embed to actually render in prod, the Grafana instance must allow embedding
 * (`allow_embedding = true`) and let the operator's browser load the panel (anonymous read on a
 * scoped org, or Grafana behind the same SSO so the iframe is authenticated), with the `task-runs`
 * dashboard provisioned.
 */
export function RunLogsEmbed({ taskId }: { taskId: string }) {
  const base = process.env.NEXT_PUBLIC_GRAFANA_URL?.trim().replace(/\/+$/, "");
  if (!base) {
    return (
      <StatusLine>
        Set <code className="font-mono">NEXT_PUBLIC_GRAFANA_URL</code> to embed this run&rsquo;s
        live logs from Grafana/Loki. Until then, stream them from your terminal with the command
        below.
      </StatusLine>
    );
  }

  const src =
    `${base}/d-solo/${GRAFANA_DASHBOARD_UID}/task-runs` +
    `?orgId=1&panelId=${GRAFANA_LOGS_PANEL_ID}` +
    `&var-task_id=${encodeURIComponent(taskId)}&theme=${GRAFANA_THEME}&kiosk`;

  return (
    <iframe
      src={src}
      title="Run logs (Grafana / Loki)"
      frameBorder="0"
      className="w-full rounded-md border border-base-content/15 bg-base-200"
      style={{ minHeight: 420 }}
    />
  );
}
