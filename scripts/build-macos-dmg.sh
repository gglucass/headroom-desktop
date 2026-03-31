#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "This script only runs on macOS." >&2
  exit 1
fi

load_env_file() {
  local path="$1"
  if [[ -f "${path}" ]]; then
    set -a
    # shellcheck disable=SC1090
    source "${path}"
    set +a
  fi
}

load_env_file "${REPO_ROOT}/.env"
load_env_file "${REPO_ROOT}/.env.local"

require_env() {
  local key="$1"
  if [[ -z "${!key:-}" ]]; then
    echo "Missing required environment variable: ${key}" >&2
    exit 1
  fi
}

load_env_value_from_file() {
  local key="$1"
  local value="${!key:-}"

  if [[ -n "${value}" && -f "${value}" ]]; then
    export "${key}=$(<"${value}")"
  fi
}

prepare_notarization() {
  if [[ -n "${APPLE_API_KEY_PATH:-}" ]]; then
    require_env APPLE_API_KEY
    require_env APPLE_API_ISSUER
    return 0
  fi

  if [[ -n "${APPLE_API_PRIVATE_KEY_P8:-}" ]]; then
    require_env APPLE_API_KEY
    require_env APPLE_API_ISSUER

    local key_path
    key_path="$(mktemp "${TMPDIR:-/tmp}/headroom-authkey.XXXXXX.p8")"
    trap 'rm -f "${key_path}"' EXIT
    printf '%s' "${APPLE_API_PRIVATE_KEY_P8}" > "${key_path}"
    export APPLE_API_KEY_PATH="${key_path}"
    return 0
  fi

  if [[ -n "${APPLE_ID:-}" || -n "${APPLE_PASSWORD:-}" || -n "${APPLE_TEAM_ID:-}" ]]; then
    require_env APPLE_ID
    require_env APPLE_PASSWORD
    require_env APPLE_TEAM_ID
    return 0
  fi

  echo "Configure notarization with either APPLE_API_* variables or APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID." >&2
  exit 1
}

require_env APPLE_SIGNING_IDENTITY
require_env TAURI_SIGNING_PRIVATE_KEY
require_env TAURI_SIGNING_PRIVATE_KEY_PASSWORD

load_env_value_from_file TAURI_SIGNING_PRIVATE_KEY
load_env_value_from_file HEADROOM_UPDATER_PUBLIC_KEY

prepare_notarization

if [[ -z "${HEADROOM_UPDATER_PUBLIC_KEY:-}" || -z "${HEADROOM_UPDATER_ENDPOINTS:-}" ]]; then
  echo "Warning: HEADROOM_UPDATER_PUBLIC_KEY or HEADROOM_UPDATER_ENDPOINTS is missing." >&2
  echo "The DMG will still build, but in-app update checks will be disabled in that app build." >&2
fi

export CI="${CI:-true}"

cd "${REPO_ROOT}"
if [[ -n "${TARGET:-}" ]]; then
  npx tauri build --bundles dmg --ci --target "${TARGET}"
else
  npx tauri build --bundles dmg --ci
fi
