#!/usr/bin/env bash
# Convert Biome JSON output to SARIF 2.1.0 format.
#
# Input: .ci/quality/reports/biome.json
# Output: .ci/quality/reports/biome.sarif

set -euo pipefail

readonly REPORTS_DIR="${1:-.ci/quality/reports}"
readonly BIOME_JSON="${REPORTS_DIR}/biome.json"

if [[ ! -f "$BIOME_JSON" ]]; then
  echo "Biome JSON report not found; skipping conversion." >&2
  exit 0
fi

# Convert Biome JSON to SARIF using jq.
# Biome JSON format: { "diagnostics": [ { "category": "parse|lint|...", "severity": "error|warning|note", "message": "...", "location": { "path": "...", "span": [start, end] } } ] }

jq '
  .diagnostics |
  map({
    ruleId: ("biome." + .category + "." + (.id // "unknown")),
    message: { text: .message },
    level: (if .severity == "error" then "error" elif .severity == "warning" then "warning" else "note" end),
    locations: [
      {
        physicalLocation: {
          artifactLocation: {
            uri: .location.path
          },
          region: {
            startLine: (.location.span[0] // 0)
          }
        }
      }
    ]
  }) |
  # Reassemble as SARIF.
  {
    "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
    "version": "2.1.0",
    "runs": [
      {
        "tool": {
          "driver": {
            "name": "biome",
            "version": "2.0.0",
            "informationUri": "https://biomejs.dev"
          }
        },
        "results": .
      }
    ]
  }
' "$BIOME_JSON" > "${REPORTS_DIR}/biome.sarif"

echo "✓ Converted Biome JSON to SARIF"
exit 0
