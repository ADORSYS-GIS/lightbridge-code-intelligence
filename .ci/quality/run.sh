#!/usr/bin/env bash
# Orchestrate all quality scanners: run locally or in CI, capture outputs, and merge reports.
#
# Invoked directly via `bash run.sh` (GitHub Actions `run:` steps default to bash on Linux
# runners), so this script targets bash, not zsh — `local` is only valid inside a function under
# bash, and top-level flow control must use `exit`, not `return` (this script is never sourced).
#
# Exit codes:
#   0 — all scanners passed (no actionable findings)
#   1 — scanner execution error or configuration problem (fail immediately, no gate evaluation)
#   2 — gate rejected the findings (new errors or critical/high security issues)

set -euo pipefail

readonly REPO_ROOT="${GITHUB_WORKSPACE:-.}"
readonly REPORTS_DIR="${REPO_ROOT}/.ci/quality/reports"
readonly RULES_DIR="${REPO_ROOT}/.ci/rules"
readonly BASELINES_DIR="${REPO_ROOT}/.ci/baselines"

# CI detection: set explicitly in GitHub Actions, false by default.
readonly CI="${CI:-false}"

# GitHub Actions environment: infer PR/ref context.
readonly GITHUB_REF="${GITHUB_REF:-$(cd "$REPO_ROOT" && git symbolic-ref -q --short HEAD || git rev-parse -q --short HEAD)}"
readonly IS_PR="${GITHUB_EVENT_NAME:-push}"
readonly PR_BASE="${GITHUB_BASE_REF:-}"

# Initialize reports directory.
mkdir -p "${REPORTS_DIR}"

# Logging utilities.
log_section() { echo "" >&2; echo "=== $1 ===" >&2; }
log_scanner_start() { echo "Running: $1" >&2; }
log_scanner_end() { echo "✓ $1 reported to ${REPORTS_DIR}/$2" >&2; }
log_error() { echo "ERROR: $1" >&2; }
log_warn() { echo "WARN: $1" >&2; }

# Track scanner results: name → (exit_code, report_file, kind)
declare -A SCANNER_RESULTS

# Verify scanner availability (lightweight check). Does not exit on its own — records the
# result so the caller can check every scanner before deciding whether to fail, instead of
# stopping at the first missing tool and hiding the rest of the picture.
MISSING_SCANNERS=()
verify_scanner_availability() {
  local scanner_name=$1
  local cmd=$2
  if ! command -v "$cmd" &>/dev/null; then
    if [[ "$CI" == "true" ]]; then
      log_error "Scanner '$scanner_name' not found: '$cmd' not in PATH. Runner not provisioned correctly."
      MISSING_SCANNERS+=("$scanner_name")
    else
      log_warn "Scanner '$scanner_name' not found: '$cmd' not in PATH (skipping for local run)"
    fi
  fi
}

# Run a scanner and capture its console output. Handles non-zero exits gracefully (findings are
# not errors).
#
# IMPORTANT: `report_file` must already be the target of the scanner's OWN report-output flag
# (--output/--report-path/--reporter-file, already wired into each call site below), never a
# redirect target for this function. Redirecting the command's stdout/stderr to the same path the
# tool itself writes its report to actively corrupts the report: confirmed by reproducing this for
# both semgrep (its scan-summary banner gets appended into the same fd as its own --output write)
# and Biome (same issue, worse interleaving) before switching to a dedicated console log per
# scanner. Trivy's --quiet happened to make the naive double-redirect look safe in a quick test,
# but relying on that per-tool/version-specific behavior is exactly the kind of assumption that
# already broke twice here — every scanner gets its own separate console log unconditionally.
run_scanner() {
  local scanner_name=$1
  local report_file=$2
  local report_kind=$3  # sarif | json | txt | etc.
  shift 3
  local cmd=("$@")
  local console_log="${REPORTS_DIR}/${scanner_name}-console.log"

  log_scanner_start "$scanner_name"

  if "${cmd[@]}" > "${console_log}" 2>&1; then
    # Exit code 0: no findings or scanner succeeded with no issues.
    SCANNER_RESULTS["${scanner_name}"]="0|${report_file}|${report_kind}"
    log_scanner_end "$scanner_name" "$report_file"
    return 0
  else
    local exit_code=$?
    # For SAST/secret scanners: exit code 1 usually means "findings found", which is OK.
    # Exit codes 2+ usually mean real errors (bad config, missing files, etc.).
    if [[ $exit_code -eq 1 ]]; then
      SCANNER_RESULTS["${scanner_name}"]="1|${report_file}|${report_kind}"
      log_scanner_end "$scanner_name" "$report_file"
      return 0
    else
      log_error "$scanner_name exited with code $exit_code (likely configuration or environment error)."
      # Surface the captured console output directly in the CI log — it's otherwise only visible
      # via the uploaded report artifact, which may itself be unavailable (e.g. storage-quota
      # failures, as happened on this exact pipeline).
      log_error "--- ${scanner_name} console output (${console_log}) ---"
      tail -n 50 "${console_log}" >&2 || true
      log_error "--- end ${scanner_name} console output ---"
      SCANNER_RESULTS["${scanner_name}"]="${exit_code}|${report_file}|${report_kind}"
      return 1
    fi
  fi
}

# ==============================================================================
# SCANNERS
# ==============================================================================

log_section "Verifying scanner availability"

verify_scanner_availability "semgrep" "semgrep"
verify_scanner_availability "trivy" "trivy"
verify_scanner_availability "gitleaks" "gitleaks"
verify_scanner_availability "hadolint" "hadolint"

if [[ "$CI" == "true" && ${#MISSING_SCANNERS[@]} -gt 0 ]]; then
  log_error "Runner not provisioned correctly. Missing: ${MISSING_SCANNERS[*]}"
  exit 1
fi

log_section "Running scanners"

# Semgrep SAST
if command -v semgrep &>/dev/null; then
  # --json and --sarif are mutually exclusive output formats; --sarif alone is what merge-sarif.sh
  # consumes (confirmed by reproducing "Mutually exclusive options" locally before this fix).
  run_scanner "semgrep-sast" "semgrep.sarif" "sarif" \
    semgrep \
      --config="${RULES_DIR}/semgrep" \
      --sarif \
      --output="${REPORTS_DIR}/semgrep.sarif" \
      "$REPO_ROOT" || true
else
  log_warn "Semgrep not available; skipping SAST."
fi

# Trivy: filesystem + dependency scanning
if command -v trivy &>/dev/null; then
  run_scanner "trivy-fs" "trivy-fs.sarif" "sarif" \
    trivy fs \
      --quiet \
      --skip-db-update \
      --skip-java-db-update \
      --skip-check-update \
      --skip-version-check \
      --format sarif \
      --output "${REPORTS_DIR}/trivy-fs.sarif" \
      "$REPO_ROOT" || true
else
  log_warn "Trivy not available; skipping filesystem/dependency scanning."
fi

# Gitleaks: secret scanning
if command -v gitleaks &>/dev/null; then
  # For PR: scan the merge base..HEAD range (use GITHUB_BASE_REF to avoid remote fetch issues)
  # For default branch: scan full history (--verbose to see all checked commits)
  # gitleaks' output flag is --report-path, not --output (confirmed against `gitleaks detect
  # --help` on the real binary after a real run failed with "unknown flag: --output").
  gitleaks_opts=(--verbose --report-format=sarif --report-path="${REPORTS_DIR}/gitleaks.sarif")
  if [[ "$IS_PR" == "pull_request" && -n "$PR_BASE" ]]; then
    # PR: scan only new commits. Use merge-base with local branch name (GitHub Actions checks out base ref).
    merge_base=$(cd "$REPO_ROOT" && git merge-base "$PR_BASE" HEAD 2>/dev/null || echo "HEAD~10")
    gitleaks_opts+=(--log-opts="$merge_base..HEAD")
  fi

  run_scanner "gitleaks" "gitleaks.sarif" "sarif" \
    gitleaks detect "${gitleaks_opts[@]}" || true
else
  log_warn "Gitleaks not available; skipping secret scanning."
fi

# Hadolint: Dockerfile linting
if command -v hadolint &>/dev/null; then
  # Find all Dockerfiles and lint them; output JSON for later conversion to SARIF.
  # Use hash of full path to avoid collisions when multiple Dockerfiles have same basename.
  dockerfile_count=0
  while IFS= read -r dockerfile; do
    dockerfile_hash=$(echo "$dockerfile" | md5sum | cut -d' ' -f1)
    hadolint --format json "$dockerfile" > "${REPORTS_DIR}/hadolint-${dockerfile_hash}.json" 2>&1 || true
    # NOT ((dockerfile_count++)): post-increment from 0 evaluates the *pre*-increment value (0) as
    # the command's exit status, which set -e treats as failure — confirmed by reproducing this
    # exact silent-death-under-set-e locally before switching to plain arithmetic assignment.
    dockerfile_count=$((dockerfile_count + 1))
  done < <(find "$REPO_ROOT" -name "Dockerfile*" -type f)

  if [[ $dockerfile_count -gt 0 ]]; then
    log_scanner_end "hadolint" "hadolint-*.json"
    SCANNER_RESULTS["hadolint"]="0|hadolint-*.json|json"
  else
    log_warn "No Dockerfiles found; skipping hadolint."
  fi
else
  log_warn "Hadolint not available; skipping Dockerfile linting."
fi

# Biome: JavaScript/TypeScript linting (uses workspace pnpm).
# Biome 2.x has a native `--reporter=sarif`; confirmed against a real install (2.5.0) rather than
# hand-rolling a JSON->SARIF converter for an assumed --json schema that turned out not to exist
# ("--json is not expected in this context" on the real CLI). Its SARIF output uses absolute
# artifactLocation.uri paths, not repo-relative — merge-sarif.sh normalizes that for every scanner.
#
# NOT run via run_scanner(): that wrapper redirects the command's own stdout to the same path
# passed as the report file, which is fine for semgrep/trivy/gitleaks (they write their report via
# their own --output/--report-path flag and print little/nothing else to stdout) but empirically
# corrupts Biome's SARIF — Biome's progress banner text gets interleaved with its own
# --reporter-file write to the identical path. Confirmed by reproducing the corruption locally
# before switching to a separate console-log path for Biome's own stdout/stderr.
if [[ -f "${REPO_ROOT}/package.json" ]] && command -v pnpm &>/dev/null; then
  log_scanner_start "biome-lint"
  if pnpm --dir "$REPO_ROOT" exec biome check --reporter=sarif \
      --reporter-file="${REPORTS_DIR}/biome.sarif" "$REPO_ROOT" \
      > "${REPORTS_DIR}/biome-console.log" 2>&1; then
    SCANNER_RESULTS["biome"]="0|biome.sarif|sarif"
    log_scanner_end "biome" "biome.sarif"
  else
    biome_exit_code=$?
    if [[ $biome_exit_code -eq 1 ]]; then
      SCANNER_RESULTS["biome"]="1|biome.sarif|sarif"
      log_scanner_end "biome" "biome.sarif"
    else
      log_error "Biome linting failed with exit code $biome_exit_code (config or toolchain error)."
      log_error "--- biome-lint output (${REPORTS_DIR}/biome-console.log) ---"
      tail -n 50 "${REPORTS_DIR}/biome-console.log" >&2 || true
      log_error "--- end biome-lint output ---"
      SCANNER_RESULTS["biome"]="${biome_exit_code}|biome.sarif|sarif"
      exit 1
    fi
  fi
else
  log_warn "Biome or pnpm not available; skipping JavaScript/TypeScript linting."
fi

log_section "Converting other formats to SARIF"

# Convert Hadolint JSON to SARIF (if present).
if find "${REPORTS_DIR}" -name "hadolint-*.json" -type f &>/dev/null; then
  if ! bash "${REPO_ROOT}/.ci/quality/hadolint-to-sarif.sh" "${REPORTS_DIR}"; then
    log_warn "Hadolint JSON to SARIF conversion failed; continuing without Hadolint results."
  fi
fi

log_section "Merging SARIF reports"

# Invoke the SARIF merge script.
if ! bash "${REPO_ROOT}/.ci/quality/merge-sarif.sh" "${REPORTS_DIR}" "${REPO_ROOT}"; then
  log_error "SARIF merge failed."
  exit 1
fi

log_section "Quality gate evaluation"

# Invoke the gate script; it decides pass/fail based on findings and severity.
bash "${REPO_ROOT}/.ci/quality/gate.sh" "${REPORTS_DIR}" "$IS_PR" "$GITHUB_REF" "$PR_BASE" || {
  gate_code=$?
  log_error "Quality gate failed with exit code $gate_code."
  exit "$gate_code"
}

echo ""
echo "✓ All quality checks passed."
exit 0
