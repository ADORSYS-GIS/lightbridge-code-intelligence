#!/usr/bin/env bash
# Install the gitleaks CLI binary from its official GitHub release, verified against the
# release's own published SHA256 checksum before use.
#
# Why not an action: no trustworthy binary-only installer action exists for gitleaks (unlike
# aquasecurity/setup-trivy for trivy); the only official action (gitleaks/gitleaks-action) is a
# commercially-licensed product that requires a paid key for organization-owned repos and
# dictates its own scan invocation, which doesn't fit this pipeline's own SARIF merge design.
#
# Usage: install-gitleaks.sh <install_dir>
# The binary is placed at <install_dir>/gitleaks. The caller is responsible for adding
# <install_dir> to PATH (e.g. via $GITHUB_PATH in the workflow).

set -euo pipefail

# Pinned version + checksum. Update both together — get the checksum from the release's own
# gitleaks_<version>_checksums.txt asset, never assume it, and re-verify by hand before bumping:
#   curl -sL "https://github.com/gitleaks/gitleaks/releases/download/<version>/gitleaks_<version>_checksums.txt"
readonly GITLEAKS_VERSION="8.30.1"
readonly GITLEAKS_ASSET="gitleaks_${GITLEAKS_VERSION}_linux_x64.tar.gz"
readonly GITLEAKS_SHA256="551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb"
readonly GITLEAKS_URL="https://github.com/gitleaks/gitleaks/releases/download/v${GITLEAKS_VERSION}/${GITLEAKS_ASSET}"

readonly INSTALL_DIR="${1:?usage: install-gitleaks.sh <install_dir>}"

if [[ -x "${INSTALL_DIR}/gitleaks" ]]; then
  installed_version=$("${INSTALL_DIR}/gitleaks" version 2>/dev/null || echo "unknown")
  echo "gitleaks already present at ${INSTALL_DIR}/gitleaks (version: ${installed_version}); skipping download."
  exit 0
fi

mkdir -p "${INSTALL_DIR}"
workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

echo "Downloading gitleaks v${GITLEAKS_VERSION} (linux_x64)..."
curl -sSfL -o "${workdir}/${GITLEAKS_ASSET}" "${GITLEAKS_URL}"

echo "Verifying SHA256 checksum..."
echo "${GITLEAKS_SHA256}  ${workdir}/${GITLEAKS_ASSET}" | sha256sum -c -

echo "Extracting to ${INSTALL_DIR}..."
tar -xzf "${workdir}/${GITLEAKS_ASSET}" -C "${workdir}"
mv "${workdir}/gitleaks" "${INSTALL_DIR}/gitleaks"
chmod +x "${INSTALL_DIR}/gitleaks"

echo "✓ gitleaks $("${INSTALL_DIR}/gitleaks" version) installed at ${INSTALL_DIR}/gitleaks"
