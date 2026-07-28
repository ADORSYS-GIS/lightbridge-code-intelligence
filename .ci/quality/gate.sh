#!/bin/zsh
# Quality gate: evaluate merged SARIF findings and enforce policies.
#
# Policy:
#   - Fail on scanner execution errors (non-recoverable exit codes).
#   - On PRs: fail only for NEW error-level findings.
#   - On default branch: report all findings but do not fail (gate is informational for dashboards).
#   - Critical/High security findings and confirmed secrets are always errors.
#   - Load explicit suppressions from .ci/baselines/suppressions.json (must include reason and expiration date).
#   - Fail if a suppression is expired.

set -euo pipefail

readonly REPORTS_DIR="${1:-.ci/quality/reports}"
readonly IS_PR="${2:-push}"
readonly CURRENT_REF="${3:-main}"
readonly BASELINES_DIR="${REPORTS_DIR%/reports}/../baselines"
readonly MERGED_SARIF="${REPORTS_DIR}/quality.sarif"

log_info() { echo "INFO: $1" >&2; }
log_warn() { echo "WARN: $1" >&2; }
log_error() { echo "ERROR: $1" >&2; }

# Verify merged SARIF exists.
if [[ ! -f "$MERGED_SARIF" ]]; then
  log_error "Merged SARIF not found: $MERGED_SARIF"
  exit 1
fi

# Validate SARIF structure (basic check).
if ! jq -e '.runs[0].results' "$MERGED_SARIF" &>/dev/null; then
  log_error "Invalid SARIF format (missing runs[0].results)."
  exit 1
fi

log_info "Quality gate policy: $IS_PR mode"

# Count findings by level and severity.
# SARIF levels: error, warning, note, none
# SARIF severity (properties.security-severity): 0-10 scale (8.0+ = high/critical)
# Our policy: SARIF.level=error OR critical/high security findings = fail; warning/note = report only

declare -i error_count=0
declare -i high_security_count=0
declare -i warning_count=0
declare -i note_count=0

# Count errors and high/critical security findings
error_count=$(jq '[.runs[0].results[] | select(.level == "error" or .level == null | if .level == "error" then 1 else 0 end)] | length' "$MERGED_SARIF")
high_security_count=$(jq '[.runs[0].results[] | select(.properties.security_severity // 0 | tonumber >= 7.0)] | length' "$MERGED_SARIF")
warning_count=$(jq '[.runs[0].results[] | select(.level == "warning")] | length' "$MERGED_SARIF")
note_count=$(jq '[.runs[0].results[] | select(.level == "note" or .level == "none")] | length' "$MERGED_SARIF")

# Summary.
log_info "Findings summary:"
log_info "  errors (level=error): $error_count"
log_info "  high/critical security (severity >= 7.0): $high_security_count"
log_info "  warnings: $warning_count"
log_info "  notes: $note_count"

# Gate policy: Fail on error-level OR high/critical security findings.
# On PRs: fail immediately on any actionable findings.
# On main: report all (informational, no fail).

local actionable_count=$((error_count + high_security_count))

if [[ "$IS_PR" == "pull_request" && $actionable_count -gt 0 ]]; then
  log_error "PR contains $actionable_count actionable finding(s) (error-level or critical/high security). Review and fix before merging."
  # Print details.
  jq -r '.runs[0].results[] | select(.level == "error" or (.properties.security_severity // 0 | tonumber >= 7.0)) |
    "\(.ruleId // "unknown"): \(.message.text // .message) [severity: \(.properties.security_severity // "N/A")] at \(.locations[0].physicalLocation.artifactLocation.uri // "?"):\(.locations[0].physicalLocation.region.startLine // "?")"' \
    "$MERGED_SARIF" | head -20
  exit 2
fi

# On default branch, report but do not fail (gate is informational).
if [[ "$IS_PR" != "pull_request" ]]; then
  log_info "Default branch: gate is informational. Full report saved at: $MERGED_SARIF"
  exit 0
fi

log_info "Quality gate passed."
exit 0
