#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

usage() {
  echo "Usage: $0 <version>" >&2
  echo "  version: e.g. 1.2.3 or v1.2.3 (v prefix is stripped)" >&2
  exit 1
}

if [[ $# -eq 0 ]]; then
  # Try to derive from latest git tag
  VERSION="$(git -C "${REPO_ROOT}" describe --tags --abbrev=0 2>/dev/null || true)"
  if [[ -z "${VERSION}" ]]; then
    echo "No git tag found. Pass a version explicitly." >&2
    usage
  fi
  echo "Using latest git tag: ${VERSION}"
else
  VERSION="$1"
fi

# Strip leading 'v'
VERSION="${VERSION#v}"

# Validate semver format
if ! [[ "${VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Invalid version: '${VERSION}' (expected x.y.z)" >&2
  exit 1
fi

echo "Bumping to ${VERSION}..."

# Update package.json
node -e "
  const fs = require('fs');
  const path = '${REPO_ROOT}/package.json';
  const pkg = JSON.parse(fs.readFileSync(path, 'utf8'));
  pkg.version = '${VERSION}';
  fs.writeFileSync(path, JSON.stringify(pkg, null, 2) + '\n');
"

# Update tauri.conf.json
node -e "
  const fs = require('fs');
  const path = '${REPO_ROOT}/src-tauri/tauri.conf.json';
  const conf = JSON.parse(fs.readFileSync(path, 'utf8'));
  conf.version = '${VERSION}';
  fs.writeFileSync(path, JSON.stringify(conf, null, 2) + '\n');
"

echo "Done. Both package.json and src-tauri/tauri.conf.json set to ${VERSION}."
