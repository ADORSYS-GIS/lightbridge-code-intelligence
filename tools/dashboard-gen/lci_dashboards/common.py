"""Shared helpers for the Lightbridge dashboard generator.

Datasource UIDs are *placeholders* (``${DS_POSTGRES}`` etc.); the Grafana Operator substitutes the
real UIDs at import time via each GrafanaDashboard's ``spec.datasources`` mapping, so the same JSON
works across instances. Targets are emitted as raw query objects (the foundation SDK ships no
Postgres/SQL builder), which keeps full control over ``rawSql`` / LogQL / PromQL.
"""

from __future__ import annotations

from grafana_foundation_sdk.cog.builder import Builder
from grafana_foundation_sdk.models.dashboard import DataSourceRef, GridPos

# --- Datasource references (operator-substituted placeholders) ---
POSTGRES = DataSourceRef(type_val="grafana-postgresql-datasource", uid="${DS_POSTGRES}")
LOKI = DataSourceRef(type_val="loki", uid="${DS_LOKI}")
PROMETHEUS = DataSourceRef(type_val="prometheus", uid="${DS_PROMETHEUS}")

# Order matters for stable, reviewable diffs.
DATASOURCE_INPUTS = ["DS_POSTGRES", "DS_LOKI", "DS_PROMETHEUS"]


class _RawTarget:
    """A query target serialized verbatim (the encoder calls ``to_json``)."""

    def __init__(self, fields: dict):
        self._fields = fields

    def to_json(self) -> dict:
        return self._fields


class RawTarget(Builder):
    """Wrap a raw target dict as a cog ``Builder`` so panels accept it via ``with_target``."""

    def __init__(self, fields: dict):
        self._target = _RawTarget(fields)

    def build(self) -> _RawTarget:
        return self._target


def sql(raw_sql: str, ref_id: str = "A", fmt: str = "table") -> RawTarget:
    """A Postgres target. ``fmt`` is ``table`` for tables or ``time_series`` for graphs."""
    return RawTarget(
        {
            "refId": ref_id,
            "datasource": POSTGRES.to_json(),
            "rawSql": raw_sql,
            "format": fmt,
            "editorMode": "code",
            "rawQuery": True,
        }
    )


# --- Loki line handling ---

# Pod logs reach Loki still CRI-wrapped — `<ts> <stdout|stderr> <F|P> <line>` — because the Alloy
# pipeline that tails `/var/log/pods` has no `stage.cri` (ai-helm-values,
# `environments/prod/values/alloy.yaml`). A bare `| json` fails with `JSONParserErr` on that prefix,
# and `| cri` is an ingest-pipeline stage, not a LogQL parser. These two stages strip the envelope so
# a following `| json` sees clean input: `<_>` discards the three CRI fields, and `content` takes
# everything after them (not up to the next space), so spaces inside the payload are safe.
#
# Every Loki-backed dashboard that parses a pod's structured logs goes through here — keep it single
# so the eventual `stage.cri` rollout in ai-helm-values is a one-line change on this side, not a
# hunt through every generator.
CRI_UNWRAP = "| pattern `<_> <_> <_> <content>` | line_format `{{.content}}`"


def logql(expr: str, ref_id: str = "A") -> RawTarget:
    return RawTarget(
        {
            "refId": ref_id,
            "datasource": LOKI.to_json(),
            "expr": expr,
            "queryType": "range",
        }
    )


def promql(expr: str, ref_id: str = "A", legend: str | None = None) -> RawTarget:
    fields = {
        "refId": ref_id,
        "datasource": PROMETHEUS.to_json(),
        "expr": expr,
        "editorMode": "code",
    }
    if legend is not None:
        fields["legendFormat"] = legend
    return RawTarget(fields)


class Layout:
    """Deterministic 24-column grid layout. Call :meth:`place` per panel, left to right, wrapping."""

    def __init__(self) -> None:
        self._x = 0
        self._y = 0
        self._row_h = 0

    def place(self, width: int, height: int) -> GridPos:
        if self._x + width > 24:
            self._x = 0
            self._y += self._row_h
            self._row_h = 0
        pos = GridPos(h=height, w=width, x=self._x, y=self._y)
        self._x += width
        self._row_h = max(self._row_h, height)
        return pos

    def newline(self) -> None:
        """Force the next panel onto a fresh row."""
        if self._x != 0:
            self._x = 0
            self._y += self._row_h
            self._row_h = 0
