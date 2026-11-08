#!/usr/bin/env python3
"""Move one addon's pin in tool_manager.rs to its newest upstream release that
is at least a week old. When it moves the pin it prints a PR title, a blank
line and a PR body; when the pin is current it prints nothing. Run weekly by
.github/workflows/bump-addons.yml, which opens the PR.

    python3 scripts/bump-addons.py rtk
    python3 scripts/bump-addons.py --self-test
"""

import datetime as dt
import hashlib
import json
import os
import re
import sys
import urllib.error
import urllib.request
from pathlib import Path

# A week gives upstream time to pull or patch a bad release: rtk 1.0.1 came a
# day after 1.0.0.
MIN_AGE = dt.timedelta(days=7)
SOURCE = Path(__file__).resolve().parent.parent / "src-tauri" / "src" / "tool_manager.rs"


def const_pin(name):
    return rf'const {name}: &str = "([^"]+)";'


# Asset names mirror rtk_distribution_artifact and
# codebase_memory_distribution_artifact in tool_manager.rs.
ADDONS = {
    "rtk": {
        "github": "rtk-ai/rtk",
        "pin": const_pin("RTK_VERSION"),
        "assets": {
            "RTK_SHA256_MACOS_AARCH64": "rtk-oss-aarch64-apple-darwin.tar.gz",
            "RTK_SHA256_MACOS_X86_64": "rtk-oss-x86_64-apple-darwin.tar.gz",
            "RTK_SHA256_LINUX_AARCH64": "rtk-oss-aarch64-unknown-linux-gnu.tar.gz",
            "RTK_SHA256_LINUX_X86_64": "rtk-oss-x86_64-unknown-linux-musl.tar.gz",
            "RTK_SHA256_WINDOWS_X86_64": "rtk-oss-x86_64-pc-windows-msvc.zip",
        },
        "check": "CI installs this rtk on Linux and macOS and runs the real rewrite hook on it "
        "(`pinned_rtk_keeps_the_hook_contract`). Read the changelog for changes to "
        "`rtk rewrite`, its exit codes, or `rtk gain`.",
    },
    "codebase-memory": {
        "github": "DeusData/codebase-memory-mcp",
        "pin": const_pin("CODEBASE_MEMORY_VERSION"),
        "assets": {
            "CODEBASE_MEMORY_SHA256_MACOS_AARCH64": "codebase-memory-mcp-darwin-arm64.tar.gz",
            "CODEBASE_MEMORY_SHA256_MACOS_X86_64": "codebase-memory-mcp-darwin-amd64.tar.gz",
            "CODEBASE_MEMORY_SHA256_LINUX_AARCH64": "codebase-memory-mcp-linux-arm64-portable.tar.gz",
            "CODEBASE_MEMORY_SHA256_LINUX_X86_64": "codebase-memory-mcp-linux-amd64-portable.tar.gz",
            "CODEBASE_MEMORY_SHA256_WINDOWS_X86_64": "codebase-memory-mcp-windows-amd64.zip",
        },
        "check": "CI installs this build on Linux and macOS and starts its MCP server "
        "(`pinned_codebase_memory_serves_mcp`). Read the changelog for changes to the CLI or "
        "to where it keeps its indexes.",
    },
    "markitdown": {
        "pypi": "markitdown",
        "pin": const_pin("MARKITDOWN_PINNED_VERSION"),
        "check": "CI does not install it. It goes into the runtime venv, so check that its "
        "dependencies leave the runtime lock's pins alone (magika, and onnxruntime on Windows), "
        "then convert a PDF and a .docx.",
    },
    "serena": {
        "pypi": "serena-agent",
        # The non-Intel branch: Intel Macs stay on their own pin (RUST-F6).
        "pin": r'const fn serena_pinned_version\(.*?\} else \{\s*"([^"]+)"',
        "check": "CI does not install it, and Intel Macs keep their own pin. Check the "
        "start-mcp-server flags, the claude-code and codex contexts, the dashboard port and "
        "`/get_tool_stats`, which the doc comment on `serena_pinned_version` lists.",
    },
    "context7": {
        "npm": "@upstash/context7-mcp",
        "pin": const_pin("CONTEXT7_PINNED_VERSION"),
        "check": "CI does not install it. Only Grok runs the local package, so enable Context7 "
        "with Grok connected and check that its MCP server starts.",
    },
}


def parse(version):
    """(1, 2, 3) for a plain release like 1.2.3 or v1.2.3; None for rc, dev, beta."""
    version = version.removeprefix("v")
    return tuple(int(part) for part in version.split(".")) if re.fullmatch(r"\d+(\.\d+)+", version) else None


def pick(candidates, pin, now):
    """Newest (version, published) at least MIN_AGE old and newer than the pin."""
    floor = parse(pin)
    ready = [(parse(v), v) for v, published in candidates if now - published >= MIN_AGE]
    ready = [(key, v) for key, v in ready if key and key > floor]
    return max(ready)[1] if ready else None


def get(url):
    headers = {"Accept": "application/json", "User-Agent": "headroom-desktop-bump-addons"}
    token = os.environ.get("GITHUB_TOKEN")
    if token and url.startswith("https://api.github.com/"):
        headers["Authorization"] = f"Bearer {token}"
    with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=60) as response:
        return json.load(response)


def when(stamp):
    return dt.datetime.fromisoformat(stamp.replace("Z", "+00:00"))


def github_candidates(repo, pin):
    # Tags, not /releases: rtk publishes hundreds of dev-* prereleases. Only
    # tags past the pin get a release lookup, one API call each.
    refs = get(f"https://api.github.com/repos/{repo}/git/matching-refs/tags/v")
    releases = {}
    for ref in refs:
        tag = ref["ref"].removeprefix("refs/tags/")
        if not parse(tag) or parse(tag) <= parse(pin):
            continue
        try:
            release = get(f"https://api.github.com/repos/{repo}/releases/tags/{tag}")
        except urllib.error.HTTPError as err:
            if err.code == 404:  # a tag without a release
                continue
            raise
        if not release["draft"] and not release["prerelease"]:
            releases[tag] = release
    return releases


def sha256_of(asset):
    digest = hashlib.sha256()
    with urllib.request.urlopen(asset["browser_download_url"], timeout=300) as response:
        for chunk in iter(lambda: response.read(1 << 20), b""):
            digest.update(chunk)
    sha = digest.hexdigest()
    if asset.get("digest") and asset["digest"] != f"sha256:{sha}":
        sys.exit(f"{asset['name']}: downloaded sha256 {sha} does not match GitHub's {asset['digest']}")
    return sha


def replace_one(source, pattern, value, flags=0):
    matches = list(re.finditer(pattern, source, flags))
    if len(matches) != 1:
        sys.exit(f"expected one match for {pattern!r} in {SOURCE.name}, found {len(matches)}")
    start, end = matches[0].span(1)
    return source[:start] + value + source[end:]


def bump(name):
    addon = ADDONS[name]
    source = SOURCE.read_text()
    match = re.search(addon["pin"], source, re.S)
    if not match:
        sys.exit(f"{name}: no pin matching {addon['pin']!r} in {SOURCE.name}")
    pin = match.group(1)
    now = dt.datetime.now(dt.timezone.utc)

    if "github" in addon:
        releases = github_candidates(addon["github"], pin)
        candidates = [(tag, when(r["published_at"])) for tag, r in releases.items()]
    elif "pypi" in addon:
        data = get(f"https://pypi.org/pypi/{addon['pypi']}/json")
        candidates = [
            (version, min(when(f["upload_time_iso_8601"]) for f in files))
            for version, files in data["releases"].items()
            if files and not all(f.get("yanked") for f in files)
        ]
    else:
        data = get("https://registry.npmjs.org/" + addon["npm"].replace("/", "%2f"))
        candidates = [
            (version, when(data["time"][version]))
            for version, meta in data["versions"].items()
            if not meta.get("deprecated") and version in data["time"]
        ]

    picked = pick(candidates, pin, now)
    if not picked:
        return
    version = picked.removeprefix("v")
    published = dict(candidates)[picked]

    if "github" in addon:
        release = releases[picked]
        assets = {asset["name"]: asset for asset in release["assets"]}
        for const, asset_name in addon["assets"].items():
            if asset_name not in assets:
                sys.exit(f"{name} {picked} has no {asset_name}; update the artifact resolver and this script")
            source = replace_one(source, rf'const {const}: &str =\s*"([0-9a-f]{{64}})";', sha256_of(assets[asset_name]))
        changelog = release["html_url"]
    elif "pypi" in addon:
        changelog = f"https://pypi.org/project/{addon['pypi']}/{version}/"
    else:
        changelog = f"https://www.npmjs.com/package/{addon['npm']}/v/{version}"
    source = replace_one(source, addon["pin"], version, re.S)
    SOURCE.write_text(source)

    major = " Major version: expect breaking changes." if parse(version)[0] > parse(pin)[0] else ""
    age = (now - published).days
    print(f"fix(addons): bump {name} to {version}")
    print()
    print(f"Moves the {name} pin from {pin} to {version}, released {published:%Y-%m-%d} ({age} days ago).{major}")
    print()
    print(f"Changelog: {changelog}")
    print()
    print(f"Before merging: {addon['check']}")
    print()
    print("Opened by `.github/workflows/bump-addons.yml`, which proposes the newest release at least 7 days old.")


def self_test():
    now = dt.datetime(2026, 11, 4, tzinfo=dt.timezone.utc)
    days = lambda n: now - dt.timedelta(days=n)
    # Newest old-enough release wins; a release under a week old waits.
    assert pick([("v1.1.0", days(10)), ("v1.2.0", days(3)), ("v1.0.1", days(20))], "0.48.0", now) == "v1.1.0"
    # Never a downgrade or the pin itself; rc and dev builds never count.
    assert pick([("0.47.0", days(30)), ("0.48.0", days(30))], "0.48.0", now) is None
    assert pick([("1.2.0rc1", days(30)), ("dev-1.2.0-rc.612", days(30))], "1.1.0", now) is None
    # Numeric order, not string order.
    assert pick([("0.9.0", days(30)), ("0.10.0", days(30))], "0.8.0", now) == "0.10.0"
    print("bump-addons self-test passed")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        self_test()
    elif len(sys.argv) == 2 and sys.argv[1] in ADDONS:
        bump(sys.argv[1])
    else:
        sys.exit(f"usage: {sys.argv[0]} {{{','.join(ADDONS)}}} | --self-test")
