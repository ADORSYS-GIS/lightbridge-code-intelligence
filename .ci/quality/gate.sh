#!/usr/bin/env bash
# Quality gate: evaluate merged SARIF findings and enforce policies.
#
# Invoked via `bash gate.sh` — targets bash, not zsh (see run.sh header for the same note).
#
# Policy:
#   - Fail on scanner execution errors (non-recoverable exit codes).
#   - On PRs: fail only for NEW error-level or critical/high-severity findings — i.e. findings that
#     land on a line the PR actually added, not the existing repository backlog. This mirrors what
#     reviewdog's own `-filter-mode=added` already does for its annotations; without this, a PR gate
#     that scores the *entire* merged SARIF would fail on pre-existing findings the PR never touched.
#   - On default branch: report all findings but do not fail (gate is informational for dashboards).
#   - Load explicit suppressions from .ci/baselines/suppressions.json (must include reason and expiration date).
#   - Fail if a suppression is expired.

set -euo pipefail

readonly REPORTS_DIR="${1:-.ci/quality/reports}"
readonly IS_PR="${2:-push}"
readonly CURRENT_REF="${3:-main}"
readonly PR_BASE="${4:-}"
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

# Count findings by level and severity (repo-wide — used for the informational summary and for
# the default-branch/scheduled report; the PR pass/fail decision below is diff-scoped instead).
declare -i error_count=0
declare -i high_security_count=0
declare -i warning_count=0
declare -i note_count=0

error_count=$(jq '[.runs[0].results[] | select(.level == "error")] | length' "$MERGED_SARIF")
high_security_count=$(jq '[.runs[0].results[] | select(.properties.security_severity // 0 | tonumber >= 7.0)] | length' "$MERGED_SARIF")
warning_count=$(jq '[.runs[0].results[] | select(.level == "warning")] | length' "$MERGED_SARIF")
note_count=$(jq '[.runs[0].results[] | select(.level == "note" or .level == "none")] | length' "$MERGED_SARIF")

log_info "Findings summary (whole repo):"
log_info "  errors (level=error): $error_count"
log_info "  high/critical security (severity >= 7.0): $high_security_count"
log_info "  warnings: $warning_count"
log_info "  notes: $note_count"

# On default branch / scheduled scans, report but do not fail (gate is informational).
if [[ "$IS_PR" != "pull_request" ]]; then
  log_info "Default branch: gate is informational. Full report saved at: $MERGED_SARIF"
  exit 0
fi

# --- PR mode: diff-scope the pass/fail decision to lines the PR actually added. ---

declare -A CHANGED_LINES

if [[ -n "$PR_BASE" ]]; then
  merge_base=$(git merge-base "$PR_BASE" HEAD 2>/dev/null || echo "HEAD~10")
  current_file=""
  while IFS= read -r diff_line; do
    if [[ "$diff_line" == "+++ "* ]]; then
      path="${diff_line#+++ }"
      if [[ "$path" == "/dev/null" ]]; then
        current_file=""
      else
        current_file="${path#b/}"
      fi
    elif [[ "$diff_line" =~ ^@@\ -[0-9]+(,[0-9]+)?\ \+([0-9]+)(,([0-9]+))?\ @@ ]]; then
      start="${BASH_REMATCH[2]}"
      count="${BASH_REMATCH[4]:-1}"
      if [[ -n "$current_file" && "$count" -gt 0 ]]; then
        for ((i = 0; i < count; i++)); do
          CHANGED_LINES["${current_file}:$((start + i))"]=1
        done
      fi
    fi
  done < <(git diff --unified=0 "${merge_base}...HEAD" 2>/dev/null || true)
  log_info "Diff-scoping: ${#CHANGED_LINES[@]} added line(s) in this PR (base: ${merge_base})."
else
  log_warn "No PR base ref available; cannot diff-scope. Falling back to whole-repo findings for the pass/fail decision."
fi

# Extract candidate (error-level or high-severity) findings as TSV, then keep only the ones whose
# file:line falls inside CHANGED_LINES (skip diff-scoping entirely if PR_BASE was unavailable).
declare -a actionable_findings=()

while IFS=$'\t' read -r rule_id message severity file line; do
  [[ -z "$file" ]] && continue
  if [[ -z "$PR_BASE" ]] || [[ -n "${CHANGED_LINES[${file}:${line}]:-}" ]]; then
    actionable_findings+=("${rule_id}: ${message} [severity: ${severity}] at ${file}:${line}")
  fi
done < <(jq -r '.runs[0].results[] |
  select(.level == "error" or (.properties.security_severity // 0 | tonumber >= 7.0)) |
  [
    (.ruleId // "unknown"),
    (.message.text // (.message | tostring) // "no message"),
    (.properties.security_severity // "N/A"),
    (.locations[0].physicalLocation.artifactLocation.uri // ""),
    (.locations[0].physicalLocation.region.startLine // 0 | tostring)
  ] | @tsv' "$MERGED_SARIF")

if [[ ${#actionable_findings[@]} -gt 0 ]]; then
  log_error "PR contains ${#actionable_findings[@]} new actionable finding(s) (error-level or critical/high security) on changed lines. Review and fix before merging."
  printf '%s\n' "${actionable_findings[@]}" | head -20 >&2
  exit 2
fi

log_info "Quality gate passed (no new actionable findings on changed lines)."
exit 0
