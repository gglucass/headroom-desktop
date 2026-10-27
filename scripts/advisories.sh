#!/usr/bin/env bash
# Dependency advisory audits, shared by CI's advisories job (test.yml) and the
# release train, which puts anything red at the top of its release PR. An
# advisories-only red still publishes a staging rc, so an advisory published
# overnight does not cost the night's verify; no stable ships until this passes
# (release-macos.yml waits for a green Test). Every accepted advisory is an
# ignore below with its reason; drop each with the pin that carries it.
# Usage: scripts/advisories.sh [rust|npm|python]... (no argument runs all three).
# Every audit runs even when an earlier one fails, so one run lists all of them.
set -u
cd "$(dirname "$0")/.."

rust() {
  # The two ignores are quick-xml DoS advisories reachable only through
  # tauri's own tree (plist 1.8 pins ^0.38, tauri-winrt-notification 0.7
  # pins ^0.37; both reject 0.41, so there is no upgrade from here).
  # Neither parse path takes attacker-controlled XML -- plist reads our
  # own bundle Info.plist, and the notification crate serializes toast XML
  # we generate. Drop these the moment a tauri bump moves the pins;
  # `cargo audit` prints them as unnecessary ignores when that happens.
  cargo audit --file src-tauri/Cargo.lock \
    --ignore RUSTSEC-2026-0194 \
    --ignore RUSTSEC-2026-0195
}

npm() {
  # --omit=dev: a devDependency advisory is not in the shipped bundle and
  # would block releases on something users never run.
  command npm audit --audit-level=high --omit=dev
}

python() {
  # Audits the pins each platform installs, not the lock files as-is:
  # pip-audit evaluates environment markers against the runner, so a
  # direct audit silently skips the Intel-Mac-only pins. --export
  # resolves markers per target first (offline). The ignores:
  # - PYSEC-2025-194 (torch.jit.script memory corruption): exploiting it
  #   means running attacker-written TorchScript locally, and nothing in
  #   headroom calls torch.jit. The fix is torch 2.13, a native bump with
  #   its own macOS-arch review; drop the ignore with that bump.
  # - PYSEC-2026-3447 (setuptools MANIFEST.in excludes vs NFD names):
  #   only reachable when building an sdist, which the runtime never does
  #   (--only-binary=:all:). Drop when setuptools moves to >=83.
  # - PYSEC-2026-3552/3553/3554: cryptography 48.0.1 on Intel Macs only,
  #   GHSA-g6cj-pr64-35w5 / GHSA-jwv3-5hgf-82ww / GHSA-m2h6-j472-rp4c,
  #   verified unreachable 2026-08-17 (see headroom-requirements.lock).
  #   Drop with that pin.
  # - CVE-2026-114603 (opentelemetry-api <1.45.1, quadratic regex in
  #   TraceState.from_header): reached only by extracting an inbound W3C
  #   tracestate header. headroom uses OpenTelemetry to export its own
  #   spans and metrics and never extracts inbound trace context, and
  #   litellm's extract_traceparent runs only under its otel callback,
  #   which headroom does not register. The 1.45.1 family adds two packages
  #   to the closure and every lock change re-syncs each install's runtime,
  #   so the bump rides the next lock refresh; drop the ignore with it.
  # - GHSA-xmj3-9gf7-h52w (setuptools 81.0.0, macOS and Windows locks:
  #   Wheel.install_as_egg writes an __init__.py stub at a path a wheel's
  #   namespace_packages.txt picks, Windows only): fix 84.1.0, but torch
  #   2.12.1 requires setuptools<82, so no lock resolves. Reached only by a
  #   legacy setup_requires egg fetch; nothing outside setuptools and pip
  #   calls install_as_egg, setuptools.wheel or fetch_build_egg, and the
  #   runtime installs --only-binary. Drop with the torch 2.13 bump (which
  #   lifts the cap to >=77.0.3), with PYSEC-2026-3447.
  local py pa targets target status=0
  # The submodule: the repo's own packaging/ dir imports as an empty namespace package.
  if python3 -c 'import packaging.requirements' 2>/dev/null; then py=(python3); else py=(uvx --with packaging python); fi
  if command -v pip-audit >/dev/null; then pa=(pip-audit); else pa=(uvx pip-audit@2.10.1); fi
  targets=$(mktemp -d)
  "${py[@]}" scripts/hash-python-locks.py --export "$targets" || { rm -rf "$targets"; return 1; }
  for target in "$targets"/*.txt; do
    echo "== $(basename "$target" .txt)"
    "${pa[@]}" -r "$target" --disable-pip --progress-spinner off \
      --ignore-vuln PYSEC-2025-194 \
      --ignore-vuln PYSEC-2026-3447 \
      --ignore-vuln PYSEC-2026-3552 \
      --ignore-vuln PYSEC-2026-3553 \
      --ignore-vuln PYSEC-2026-3554 \
      --ignore-vuln CVE-2026-114603 \
      --ignore-vuln GHSA-xmj3-9gf7-h52w || status=1
  done
  rm -rf "$targets"
  return "$status"
}

status=0
[ $# -gt 0 ] || set -- rust npm python
for audit in "$@"; do
  case $audit in
    rust|npm|python) echo "=== $audit advisories"; "$audit" || status=1 ;;
    *) echo "usage: scripts/advisories.sh [rust|npm|python]..." >&2; exit 2 ;;
  esac
done
exit "$status"
