#!/usr/bin/env python3
"""Every hard dependency of the pinned headroom-ai must be pinned in each lock.

The headroom-ai wheel is installed with `--no-deps` (tool_manager.rs), so pip
never resolves its dependencies: the per-platform lock file is the only thing
that puts them in the venv. A dep missing from a lock is therefore not a soft
resolve pip can fix at install time, it is an ImportError the moment the
backend starts. That is how pyyaml, added to headroom-ai's core requires_dist,
shipped missing from the Linux lock for two releases.

Checks names only, not version ranges: the pins are chosen by hand per platform
and deliberately differ, but a name that is absent is always a bug.
"""

import json
import re
import sys
import urllib.request
from pathlib import Path

from packaging.requirements import Requirement
from packaging.utils import canonicalize_name

REPO = Path(__file__).resolve().parent.parent

# (lock file, sys_platform value for marker evaluation)
LOCKS = [
    ("headroom-requirements.lock", "darwin"),
    ("headroom-linux-requirements.lock", "linux"),
    ("headroom-windows-requirements.lock", "win32"),
]

# The bundled interpreter (PYTHON_STANDALONE_RELEASE in tool_manager.rs).
PYTHON_FULL_VERSION = "3.12.12"

# Extras the desktop app actually installs. `proxy-prod` is server-side only.
EXTRAS = {"", "proxy"}

# Hard deps left out on purpose. ast-grep-cli is two binaries and no module, so its
# absence is no ImportError; only the opt-in --intercept-tool-results uses it, and
# the app never passes that (a web filter blocking its .exe wheel blocked a
# Windows bootstrap: RUST-6S on 0.9.46).
NOT_INSTALLED = {"ast-grep-cli"}


def pinned_version() -> str:
    src = (REPO / "src-tauri/src/tool_manager.rs").read_text()
    match = re.search(r'HEADROOM_PINNED_VERSION: &str = "([^"]+)"', src)
    if not match:
        sys.exit("could not find HEADROOM_PINNED_VERSION in tool_manager.rs")
    return match.group(1)


def required_names(version: str, sys_platform: str) -> set[str]:
    url = f"https://pypi.org/pypi/headroom-ai/{version}/json"
    with urllib.request.urlopen(url, timeout=30) as response:
        meta = json.load(response)

    names = set()
    for raw in meta["info"].get("requires_dist") or []:
        requirement = Requirement(raw)
        for extra in EXTRAS:
            environment = {
                "python_full_version": PYTHON_FULL_VERSION,
                "python_version": ".".join(PYTHON_FULL_VERSION.split(".")[:2]),
                "sys_platform": sys_platform,
                "platform_python_implementation": "CPython",
                "extra": extra,
            }
            if requirement.marker is None or requirement.marker.evaluate(environment):
                names.add(canonicalize_name(requirement.name))
                break
    return names


def pinned_names(lock: Path) -> set[str]:
    names = set()
    # Join each hashed entry's backslash-continued --hash lines onto its pin.
    for line in lock.read_text().replace("\\\n", " ").splitlines():
        line = line.split("#", 1)[0].split("--hash", 1)[0].strip()
        if line:
            names.add(canonicalize_name(Requirement(line).name))
    return names


def main() -> int:
    version = pinned_version()
    print(f"Checking locks against headroom-ai {version} (core + proxy extra)")

    failed = False
    for filename, sys_platform in LOCKS:
        lock = REPO / "src-tauri/python" / filename
        required = required_names(version, sys_platform) - NOT_INSTALLED
        missing = sorted(required - pinned_names(lock))
        if missing:
            failed = True
            print(f"FAIL {filename}: missing {', '.join(missing)}")
        else:
            print(f"ok   {filename}")

    if failed:
        print(
            "\nAdd the missing pins. headroom-ai installs with --no-deps, so "
            "anything absent here is an ImportError at backend start.",
        )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
