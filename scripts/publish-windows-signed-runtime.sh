#!/usr/bin/env bash
# Publishes the signed Windows runtime where the app downloads it
# (tool_manager::signed_windows_runtime_url): one release per archive SHA-256.
# FROZEN: shipped builds pin that SHA and read the release live, so an
# existing asset is never replaced; a different archive gets a different tag.
set -euo pipefail
sha="${HEADROOM_SIGNED_RUNTIME_SHA256:?set by build-windows-signed-runtime.ps1}"
export GH_REPO="${GITHUB_REPOSITORY:?}"
archive="src-tauri/resources/windows-signed-runtime.tar.gz"
tag="signed-windows-runtime-$sha"
name="windows-signed-runtime.tar.gz"

published_digest() {
  gh release view "$tag" --json assets \
    -q ".assets[] | select(.name == \"$name\") | .digest" 2>/dev/null || true
}

digest="$(published_digest)"
if [[ -z "$digest" ]]; then
  # --latest=false: a full release otherwise becomes releases/latest, which
  # has no latest.json, and every install's update check 404s.
  gh release view "$tag" >/dev/null 2>&1 ||
    gh release create "$tag" --latest=false \
      --title "Signed Windows runtime ${sha:0:12}" \
      --notes "Python runtime with every PE file Authenticode-signed. Downloaded by Headroom on Windows by URL and SHA-256." ||
    gh release view "$tag" >/dev/null # another build created it first
  # A failed or raced upload is caught by the digest check below.
  gh release upload "$tag" "$archive" || true
  digest="$(published_digest)"
fi
if [[ "$digest" != "sha256:$sha" ]]; then
  echo "::error::$tag holds '${digest:-no asset}', expected sha256:$sha" >&2
  exit 1
fi
echo "Signed Windows runtime published: $tag" | tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
