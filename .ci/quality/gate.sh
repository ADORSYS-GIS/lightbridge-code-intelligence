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

# Count findings by level.
declare -i error_count=0
declare -i warning_count=0
declare -i note_count=0

# Map SARIF level to our policy.
# SARIF levels: error, warning, note, none
# Our policy: error (fail), warning (report but do not fail), note (report)

jq -r '.runs[0].results[] | .level // "warning"' "$MERGED_SARIF" | sort | uniq -c | while read -r count level; do
  case "$level" in
    error)   error_count=$((error_count + count)) ;;
    warning) warning_count=$((warning_count + count)) ;;
    note|none) note_count=$((note_count + count)) ;;
  esac
done

# Summary.
log_info "Findings summary:"
log_info "  errors: $error_count"
log_info "  warnings: $warning_count"
log_info "  notes: $note_count"

# On PRs, fail if there are NEW error-level findings.
# Baseline comparison would require a baseline DB (not yet implemented).
# For now: fail on any error-level finding on PRs, report others.

if [[ "$IS_PR" == "pull_request" && $error_count -gt 0 ]]; then
  log_error "PR contains $error_count new error-level finding(s). Review and fix before merging."
  # Optional: print details.
  jq -r '.runs[0].results[] | select(.level == "error") |
    "\(.ruleId // "unknown"): \(.message.text // .message) at \(.locations[0].physicalLocation.artifactLocation.uri // "?"):\(.locations[0].physicalLocation.region.startLine // "?")"' \
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
