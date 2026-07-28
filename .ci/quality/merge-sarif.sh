#!/usr/bin/env bash
# Merge individual SARIF reports from scanners into a single deduplicated run.
#
# Input: directory containing scanner SARIF files (*.sarif)
# Output: .ci/quality/reports/quality.sarif (canonical merged report)
#
# Deduplication: results are keyed by (ruleId, message, path, startLine) to avoid duplicates
# when a finding is reported by multiple tools (rare, but possible).

set -euo pipefail

readonly REPORTS_DIR="${1:-.ci/quality/reports}"
readonly REPO_ROOT="${2:-.}"

if [[ ! -d "$REPORTS_DIR" ]]; then
  echo "ERROR: Reports directory not found: $REPORTS_DIR" >&2
  exit 1
fi

# Find all SARIF files (excluding the merged output itself).
declare -a sarif_files
while IFS= read -r -d '' file; do
  if [[ "$(basename "$file")" != "quality.sarif" ]]; then
    sarif_files+=("$file")
  fi
done < <(find "$REPORTS_DIR" -name "*.sarif" -type f -print0)

if [[ ${#sarif_files[@]} -eq 0 ]]; then
  echo "WARN: No SARIF files found in $REPORTS_DIR" >&2
  # Create an empty valid SARIF file.
  cat > "${REPORTS_DIR}/quality.sarif" <<'EOF'
{
  "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
  "version": "2.1.0",
  "runs": [
    {
      "tool": {
        "driver": {
          "name": "quality-pipeline",
          "version": "1.0.0"
        }
      },
      "results": []
    }
  ]
}
EOF
  exit 0
fi

# Merge SARIF files using jq: extract all results, normalize paths, deduplicate, and reassemble.
# Deduplication key: (ruleId, message, path, startLine).
#
# Path normalization: some tools (confirmed for Biome's native --reporter=sarif) emit absolute
# artifactLocation.uri paths rather than repo-relative ones. Strip a leading $REPO_ROOT prefix so
# every result uses a repo-relative POSIX path, matching every other scanner's convention.

readonly ROOT_ABS="$(cd "$REPO_ROOT" && pwd)"

jq -s --arg root "$ROOT_ABS" '
  def normalize_path:
    if . != null and startswith($root + "/")
    then .[($root | length) + 1:]
    else .
    end;

  # Collect all results from all runs, normalizing absolute paths to repo-relative first.
  [.[].runs[].results[] |
   select(. != null) |
   .locations = (.locations // [] | map(
     .physicalLocation.artifactLocation.uri |= normalize_path
   ))
  ] |
  # Deduplicate: keep first occurrence of each (ruleId, message, path, region.startLine).
  # group_by generates groups; first(.location.physicalLocation.artifactLocation.uri) as path key.
  (
    group_by(
      .ruleId as $rule |
      (.message.text // "") as $msg |
      (.locations[0].physicalLocation.artifactLocation.uri // "") as $path |
      (.locations[0].physicalLocation.region.startLine // 0) as $line |
      "\($rule)|\($msg)|\($path)|\($line)"
    ) |
    map(.[0])  # Keep first of each group.
  ) |
  # Sort by ruleId and path for stable output.
  sort_by(.ruleId, .locations[0].physicalLocation.artifactLocation.uri) |
  # Reassemble the merged SARIF.
  {
    "$schema": "https://raw.githubusercontent.com/oasis-tcs/sarif-spec/master/Schemata/sarif-schema-2.1.0.json",
    "version": "2.1.0",
    "runs": [
      {
        "tool": {
          "driver": {
            "name": "quality-pipeline",
            "version": "1.0.0",
            "informationUri": "https://github.com/adorsys-gis/lightbridge-code-intelligence/blob/main/docs/quality-pipeline.md"
          }
        },
        "results": .
      }
    ]
  }
' $(printf '%s\n' "${sarif_files[@]}" | tr '\n' ' ') > "${REPORTS_DIR}/quality.sarif"

echo "✓ Merged ${#sarif_files[@]} SARIF file(s) into quality.sarif ($(jq '.runs[0].results | length' "${REPORTS_DIR}/quality.sarif") total findings)"
exit 0
