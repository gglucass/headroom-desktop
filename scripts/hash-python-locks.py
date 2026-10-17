#!/usr/bin/env python3
"""Pin every runtime lock requirement to the sha256 of its allowed wheels.

pip switches to hash-checking mode as soon as one requirement carries a
`--hash`, and then refuses any download that matches none of that
requirement's hashes. A file swapped on PyPI, on a mirror, or on our
vendor-wheels release becomes a failed install instead of code running on a
user's machine. Hash-checking mode also refuses any requirement that is not
pinned with `==`, so each lock must pin its whole dependency closure: the app
installs the lock WITH dependency resolution, and a transitive dep the lock
forgot is a hard bootstrap failure, not a silent fetch of whatever is newest.

Hashes cover every wheel of the pinned version whose tags one of the lock's
target platforms accepts (all cp312/abi3/py3 tags, every macOS and glibc
version), so pip still picks the best wheel for each machine. Wheels come from
PyPI and from vendor-wheels-v1 (VENDOR_WHEELS_INDEX_URL in tool_manager.rs).
The script fails if a pin has no installable wheel for a platform its marker
selects.

Run after changing any pin (needs network, stdlib + `packaging`):

    python3 scripts/hash-python-locks.py          # rewrite the locks
    python3 scripts/hash-python-locks.py --check  # exit 1 if any is stale
    python3 scripts/hash-python-locks.py --export DIR  # per-platform lists for pip-audit

Hashes do not move the lock's receipt sha (`requirements_lock_sha` in
tool_manager.rs strips them), so rehashing alone never makes installs
re-sync their dependencies.
"""

import argparse
import json
import os
import sys
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from packaging.requirements import Requirement
from packaging.tags import Tag, compatible_tags, cpython_tags, mac_platforms
from packaging.utils import canonicalize_name, parse_wheel_filename
from packaging.version import Version

REPO = Path(__file__).resolve().parent.parent
LOCK_DIR = REPO / "src-tauri/python"
VENDOR_RELEASE_API = (
    "https://api.github.com/repos/gglucass/headroom-desktop/releases/tags/vendor-wheels-v1"
)

# The bundled interpreter (PYTHON_STANDALONE_RELEASE in tool_manager.rs).
PYTHON = (3, 12)
PYTHON_FULL_VERSION = "3.12.12"


def _supported(platforms: list[str]) -> frozenset[Tag]:
    return frozenset(
        [
            *cpython_tags(PYTHON, abis=["cp312"], platforms=platforms),
            *compatible_tags(PYTHON, "cp312", platforms),
        ]
    )


def _manylinux(arch: str) -> list[str]:
    legacy = {"x86_64": ["manylinux1", "manylinux2010", "manylinux2014"]}
    names = [f"manylinux_2_{minor}_{arch}" for minor in range(5, 60)]
    names += [f"{name}_{arch}" for name in legacy.get(arch, ["manylinux2014"])]
    return names + [f"linux_{arch}"]


def _env(sys_platform: str, platform_system: str, os_name: str, machine: str) -> dict:
    return {
        "python_full_version": PYTHON_FULL_VERSION,
        "python_version": "%d.%d" % PYTHON,
        "implementation_name": "cpython",
        "implementation_version": PYTHON_FULL_VERSION,
        "platform_python_implementation": "CPython",
        "sys_platform": sys_platform,
        "platform_system": platform_system,
        "os_name": os_name,
        "platform_machine": machine,
    }


# name -> (marker environment, tags pip accepts there). macOS versions run to
# 30 so a wheel built for a newer macOS than today's is still covered.
TARGETS = {
    "darwin-arm64": (
        _env("darwin", "Darwin", "posix", "arm64"),
        _supported(list(mac_platforms((30, 0), "arm64"))),
    ),
    "darwin-x86_64": (
        _env("darwin", "Darwin", "posix", "x86_64"),
        _supported(list(mac_platforms((30, 0), "x86_64"))),
    ),
    "linux-x86_64": (_env("linux", "Linux", "posix", "x86_64"), _supported(_manylinux("x86_64"))),
    "linux-aarch64": (_env("linux", "Linux", "posix", "aarch64"), _supported(_manylinux("aarch64"))),
    "win-amd64": (_env("win32", "Windows", "nt", "AMD64"), _supported(["win_amd64"])),
}

LOCKS = {
    "headroom-requirements.lock": ["darwin-arm64", "darwin-x86_64"],
    "headroom-linux-requirements.lock": ["linux-x86_64", "linux-aarch64"],
    "headroom-windows-requirements.lock": ["win-amd64"],
}


def _get_json(url: str):
    headers = {"Accept": "application/json"}
    # Unauthenticated api.github.com allows 60 calls/hour per IP; GH_TOKEN=$(gh auth token) lifts it.
    token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token and url.startswith("https://api.github.com/"):
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def vendor_wheels() -> list[tuple[str, str]]:
    """(filename, sha256) for every wheel on the vendor-wheels release."""
    wheels = []
    for asset in _get_json(VENDOR_RELEASE_API)["assets"]:
        name, digest = asset["name"], asset.get("digest") or ""
        if not name.endswith(".whl"):
            continue
        if not digest.startswith("sha256:"):
            sys.exit(f"vendor wheel {name} has no sha256 digest in the release API")
        wheels.append((name, digest.removeprefix("sha256:")))
    return wheels


def pypi_wheels(name: str, version: str) -> list[tuple[str, str]]:
    meta = _get_json(f"https://pypi.org/pypi/{name}/{version}/json")
    return [
        (f["filename"], f["digests"]["sha256"])
        for f in meta["urls"]
        if f["packagetype"] == "bdist_wheel"
    ]


def parse_lock(text: str) -> list:
    """Items in file order: ("text", line, None) or ("req", requirement, hashes)."""
    items, pending = [], None
    for raw in text.splitlines():
        stripped = raw.strip()
        if pending is None and (not stripped or stripped.startswith("#")):
            items.append(("text", raw, None))
            continue
        head, continued = (stripped[:-1], True) if stripped.endswith("\\") else (stripped, False)
        pending = f"{pending} {head}" if pending else head
        if not continued:
            items.append(_entry(pending))
            pending = None
    if pending:
        items.append(_entry(pending))
    return items


def _entry(logical: str) -> tuple:
    requirement, *options = logical.split("--hash=")
    return ("req", requirement.strip(), [o.strip().removeprefix("sha256:") for o in options])


def hashes_for(requirement: str, targets: list[str], files: list[tuple[str, str]]) -> list[str]:
    req = Requirement(requirement)
    pins = [spec for spec in req.specifier if spec.operator == "=="]
    if len(pins) != 1 or len(req.specifier) != 1:
        sys.exit(f"{requirement}: hash-checking mode needs exactly one == pin")
    version = Version(pins[0].version)
    name = canonicalize_name(req.name)

    allowed, uncovered = set(), []
    for target in targets:
        env, supported = TARGETS[target]
        if req.marker is not None and not req.marker.evaluate(env):
            continue
        matched = False
        for filename, sha in files:
            wheel_name, wheel_version, _, tags = parse_wheel_filename(filename)
            if wheel_name != name or wheel_version != version:
                continue
            if tags & supported:
                allowed.add(sha)
                matched = True
        if not matched:
            uncovered.append(target)
    if uncovered:
        sys.exit(f"{requirement}: no installable wheel for {', '.join(uncovered)}")
    if not allowed:
        sys.exit(f"{requirement}: marker selects none of {', '.join(targets)}; drop the pin")
    return sorted(allowed)


def render(items: list, hashes: dict[str, list[str]]) -> str:
    lines = []
    for kind, value, _ in items:
        if kind == "text":
            lines.append(value)
            continue
        entry = [value, *(f"    --hash=sha256:{sha}" for sha in hashes[value])]
        lines.append(" \\\n".join(entry))
    return "\n".join(lines) + "\n"


def export(directory: Path) -> int:
    """Write <target>.txt per target platform: the pins it installs, markers
    resolved and hashes kept. pip-audit evaluates markers against the machine
    it runs on, so auditing a lock directly skips other platforms' pins (the
    Intel-Mac-only ones, on a Linux runner)."""
    directory.mkdir(parents=True, exist_ok=True)
    for lock, targets in LOCKS.items():
        items = parse_lock((LOCK_DIR / lock).read_text())
        for target in targets:
            env = TARGETS[target][0]
            entries = []
            for kind, value, hashes in items:
                if kind != "req":
                    continue
                req = Requirement(value)
                if req.marker is not None and not req.marker.evaluate(env):
                    continue
                req.marker = None
                entries.append(" \\\n".join([str(req), *(f"    --hash=sha256:{h}" for h in hashes)]))
            (directory / f"{target}.txt").write_text("\n".join(entries) + "\n")
            print(f"exported {target}.txt from {lock} ({len(entries)} pins)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--check", action="store_true", help="fail instead of writing")
    parser.add_argument(
        "--export", metavar="DIR", type=Path, help="write per-platform pin lists (offline) and exit"
    )
    args = parser.parse_args()
    if args.export:
        return export(args.export)

    vendor = vendor_wheels()
    parsed = {lock: parse_lock((LOCK_DIR / lock).read_text()) for lock in LOCKS}
    wanted = {
        (canonicalize_name(r.name), str(next(iter(r.specifier)).version))
        for items in parsed.values()
        for kind, value, _ in items
        if kind == "req"
        for r in [Requirement(value)]
    }
    with ThreadPoolExecutor(16) as pool:
        pypi = dict(zip(wanted, pool.map(lambda nv: pypi_wheels(*nv), wanted)))

    stale = False
    for lock, targets in LOCKS.items():
        items = parsed[lock]
        hashes = {}
        for kind, value, _ in items:
            if kind != "req":
                continue
            req = Requirement(value)
            key = (canonicalize_name(req.name), str(next(iter(req.specifier)).version))
            hashes[value] = hashes_for(value, targets, pypi[key] + vendor)
        path = LOCK_DIR / lock
        rendered = render(items, hashes)
        if rendered == path.read_text():
            print(f"ok      {lock}")
        elif args.check:
            print(f"STALE   {lock}")
            stale = True
        else:
            path.write_text(rendered)
            print(f"wrote   {lock} ({len(hashes)} pins, {sum(map(len, hashes.values()))} hashes)")
    if stale:
        print("\nRun: python3 scripts/hash-python-locks.py")
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main())
