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
  # - CVE-2026-112395/112397/112400/112404/112895/112896/112897/112899:
  #   pillow 12.3.0 in the macOS and Windows locks (Linux pins none). The
  #   fix is pillow 13.0.0, and fastembed caps pillow<13.0 in both 0.8.0
  #   (our pin) and 0.9.0 (latest as of 2026-10-21), so no lock resolves.
  #   headroom-ai decodes agent-traffic images with a bare Image.open, so
  #   the three TIFF decode bugs (112395, 112404, 112897) are reachable: a
  #   crafted image can crash the proxy or put proxy heap bytes into its
  #   pixels. Out-of-bounds reads only, no code execution. The other five
  #   need calls headroom never makes: a TIFF re-encode (112896; it saves
  #   JPEG), ImageMorph (112397), PDF append (112895), GIF reuse after a
  #   caught DecompressionBombError (112400), EPS load(scale) (112899).
  #   Drop all eight with the pillow 13 bump once a fastembed release
  #   allows it.
  # - Sixteen more pillow 12.3.0 CVEs published 2026-10-22, same fix
  #   (13.0.0), same fastembed cap. headroom-ai only calls Image.open on
  #   a BytesIO (any format), convert("RGB"), resize and a JPEG save, so
  #   five are reachable through a crafted agent-traffic image: IPTC
  #   merge NULL read, a crash (112405); quadratic GIF comments (112401)
  #   and the XBM header ReDoS (112901), CPU stalls; APNG canvas before
  #   the bomb check (112399) and MSP RLE (112892), memory exhaustion.
  #   No code execution. The other eleven need calls headroom never
  #   makes: Image.transform (112402), ImageFont (112893), rank filters
  #   (112392), GIF save (112891), PDF save (112391/112894/112406), TIFF
  #   save (112393/112396), MIC n_frames with olefile, which no lock pins
  #   (112390), and an EPS file name, which a BytesIO lacks (112900).
  #   Drop all sixteen with the same pillow 13 bump.
  # - Eleven more pillow 12.3.0 advisories published by 2026-10-23, same
  #   fix, same cap (fastembed 0.9.0 still pins <13.0; fastembed PR #830
  #   lifts it). image_optimize is on by default and every call site opens
  #   one frame of a BytesIO (any format), reads .size, resizes/converts
  #   and saves JPEG without icc_profile or exif, so three are reachable
  #   through a crafted agent-traffic image: GHSA-78g4-xqwv-q3g8, the
  #   bundled libopenjp2 heap-buffer-overflow WRITE on a JPEG 2000 load
  #   (the first reachable memory-corruption write; accepted by Garm
  #   2026-10-23 because the shipped stable carries the same pillow);
  #   BMP RLE memory exhaustion (96638); WMF/EMF heap over-read, Windows
  #   only (96644). The other eight need calls headroom never makes: a
  #   seek past frame 0 (MPO/DCX 96636, SPIDER 96637, multi-frame mode
  #   transitions 96639), PDF append (96640), ImageFont (96641),
  #   PIL.TarIO (96642), getexif/exif_transpose (96643), a JPEG save
  #   passed an icc_profile (96778; JpegImagePlugin reads it from the
  #   save call's arguments only). Drop all eleven with the same pillow
  #   13 bump.
  # - CVE-2026-114603 (opentelemetry-api <1.45.1, quadratic regex in
  #   TraceState.from_header): reached only by extracting an inbound W3C
  #   tracestate header. headroom uses OpenTelemetry to export its own
  #   spans and metrics and never extracts inbound trace context, and
  #   litellm's extract_traceparent runs only under its otel callback,
  #   which headroom does not register. The 1.45.1 family adds two packages
  #   to the closure and every lock change re-syncs each install's runtime,
  #   so the bump rides the next lock refresh; drop the ignore with it.
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
      --ignore-vuln CVE-2026-112390 \
      --ignore-vuln CVE-2026-112391 \
      --ignore-vuln CVE-2026-112392 \
      --ignore-vuln CVE-2026-112393 \
      --ignore-vuln CVE-2026-112395 \
      --ignore-vuln CVE-2026-112396 \
      --ignore-vuln CVE-2026-112397 \
      --ignore-vuln CVE-2026-112399 \
      --ignore-vuln CVE-2026-112400 \
      --ignore-vuln CVE-2026-112401 \
      --ignore-vuln CVE-2026-112402 \
      --ignore-vuln CVE-2026-112404 \
      --ignore-vuln CVE-2026-112405 \
      --ignore-vuln CVE-2026-112406 \
      --ignore-vuln CVE-2026-112891 \
      --ignore-vuln CVE-2026-112892 \
      --ignore-vuln CVE-2026-112893 \
      --ignore-vuln CVE-2026-112894 \
      --ignore-vuln CVE-2026-112895 \
      --ignore-vuln CVE-2026-112896 \
      --ignore-vuln CVE-2026-112897 \
      --ignore-vuln CVE-2026-112899 \
      --ignore-vuln CVE-2026-112900 \
      --ignore-vuln CVE-2026-112901 \
      --ignore-vuln GHSA-78g4-xqwv-q3g8 \
      --ignore-vuln CVE-2026-96636 \
      --ignore-vuln CVE-2026-96637 \
      --ignore-vuln CVE-2026-96638 \
      --ignore-vuln CVE-2026-96639 \
      --ignore-vuln CVE-2026-96640 \
      --ignore-vuln CVE-2026-96641 \
      --ignore-vuln CVE-2026-96642 \
      --ignore-vuln CVE-2026-96643 \
      --ignore-vuln CVE-2026-96644 \
      --ignore-vuln CVE-2026-96778 \
      --ignore-vuln CVE-2026-114603 || status=1
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
