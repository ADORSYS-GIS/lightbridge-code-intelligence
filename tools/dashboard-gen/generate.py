#!/usr/bin/env python3
"""Generate the Lightbridge Grafana dashboards as JSON into the Helm chart.

Output is deterministic (sorted keys, stable panel order) so CI can diff it: a stale dashboards/
directory means someone changed a generator but didn't regenerate. Run:

    python tools/dashboard-gen/generate.py
"""

from __future__ import annotations

import json
import pathlib

from grafana_foundation_sdk.cog.encoder import JSONEncoder

from lci_dashboards import (
    feedback,
    ingress_dispatcher,
    operations,
    overview,
    repositories,
    review_quality,
    review_runs,
    task_runs,
)

# tools/dashboard-gen/generate.py -> repo root is two parents up.
REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]
OUT_DIR = REPO_ROOT / "deploy" / "observability" / "dashboards"

# filename (without .json) -> builder factory. Keys drive the committed file names.
DASHBOARDS = {
    "overview": overview.dashboard_builder,
    "task-runs": task_runs.dashboard_builder,
    "repositories": repositories.dashboard_builder,
    "review-quality": review_quality.dashboard_builder,
    "review-runs": review_runs.dashboard_builder,
    "feedback": feedback.dashboard_builder,
    "ingress-dispatcher": ingress_dispatcher.dashboard_builder,
    "operations": operations.dashboard_builder,
}


def main() -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    for name, factory in sorted(DASHBOARDS.items()):
        model = factory().build()
        rendered = json.dumps(model, cls=JSONEncoder, indent=2, sort_keys=True) + "\n"
        (OUT_DIR / f"{name}.json").write_text(rendered, encoding="utf-8")
        print(f"wrote {name}.json")


if __name__ == "__main__":
    main()
