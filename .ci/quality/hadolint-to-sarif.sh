#!/bin/zsh
# Convert Hadolint JSON output to SARIF 2.1.0 format.
#
# Input: .ci/quality/reports/hadolint-*.json files
# Output: .ci/quality/reports/hadolint.sarif

set -euo pipefail

readonly REPORTS_DIR="${1:-.ci/quality/reports}"

if [[ ! -d "$REPORTS_DIR" ]]; then
  echo "ERROR: Reports directory not found: $REPORTS_DIR" >&2
  exit 1
fi

# Collect all Hadolint JSON files.
declare -a hadolint_files
while IFS= read -r -d '' file; do
  hadolint_files+=("$file")
done < <(find "$REPORTS_DIR" -name "hadolint-*.json" -type f -print0)

if [[ ${#hadolint_files[@]} -eq 0 ]]; then
  echo "No Hadolint JSON files found; skipping SARIF conversion." >&2
  exit 0
fi

# Convert Hadolint JSON to SARIF using jq.
# Hadolint JSON format: [ { "line": <int>, "level": "error|warning|info", "code": "DL1000", "message": "...", "file": "..." } ]
# SARIF level mapping: error → error, warning → warning, info → note

jq -s '
  # Merge all Hadolint JSON arrays.
  add |
  # Convert each finding to SARIF result.
  map({
    ruleId: ("hadolint." + .code),
    message: { text: .message },
    level: (if .level == "error" then "error" elif .level == "warning" then "warning" else "note" end),
    locations: [
      {
        physicalLocation: {
          artifactLocation: {
            uri: .file
          },
          region: {
            startLine: .line
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
            "name": "hadolint",
            "version": "2.12.0",
            "informationUri": "https://github.com/hadolint/hadolint"
          }
        },
        "results": .
      }
    ]
  }
' $(printf '%s\n' "${hadolint_files[@]}" | tr '\n' ' ') > "${REPORTS_DIR}/hadolint.sarif"

echo "✓ Converted Hadolint JSON to SARIF"
exit 0
