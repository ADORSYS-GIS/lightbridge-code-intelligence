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

# Strips the CRI envelope from a pod log line so a following `| json` sees clean input. Every
# Loki-backed dashboard that parses structured pod logs goes through here.
#
# The kubelet writes pod logs as `<ts> <stdout|stderr> <F|P> <line>`, and the Alloy pipeline that
# tails `/var/log/pods` has no `stage.cri` to strip it, so the envelope reaches Loki as part of the
# line and a bare `| json` fails with `JSONParserErr`. `| cri` is an ingest-pipeline stage, not a
# LogQL parser, so the unwrap has to happen here. `content` captures everything after the three CRI
# fields rather than up to the next space, so spaces inside the payload are safe.
#
# The prefix is an OPTIONAL group on purpose. Once the Alloy pipeline gains `stage.cri`, only
# newly-ingested lines arrive unwrapped — older ones keep the envelope for the full 90d retention,
# so both shapes are live in Loki at once and one query has to serve both. A required-prefix form
# (`| pattern`) does not: on an already-unwrapped line it captures garbage, `| json` then errors,
# and the `| __error__=""` every call site pairs it with discards the line silently — zero rows and
# nothing surfaced. Measured against an envelope-free stream: required-prefix returned 0 of 50
# lines, this form returned all 50, and both return the same rows on wrapped lines.
CRI_UNWRAP = (
    r"| regexp `^(?:\S+ std(?:out|err) [FP] )?(?P<content>.*)$` "
    "| line_format `{{.content}}`"
)


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
