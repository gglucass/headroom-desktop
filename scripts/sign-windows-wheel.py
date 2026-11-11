#!/usr/bin/env python3
"""Re-pack headroom-ai's Windows wheel around an Authenticode-signed _core.pyd.

Smart App Control blocks an unsigned DLL it has no cloud reputation for, and
every headroom-ai release ships a new unsigned _core.pyd. On those machines
each wheel bump failed boot validation and rolled back to the previous wheel,
which runs without the sitecustomize vendors gated to the new pin (RUST-W1).
Run by .github/workflows/sign-windows-wheel.yml:

  sign-windows-wheel.py fetch VERSION DIR   PyPI's wheel, sha256-checked, and
                                            its _core.pyd into DIR
  sign-windows-wheel.py repack DIR          after DIR/_core.pyd is signed:
                                            DIR/out/<same wheel name>
"""

import base64
import hashlib
import json
import sys
import urllib.request
import zipfile
from pathlib import Path

CORE = "headroom/_core.pyd"


def record_hash(data: bytes) -> str:
    digest = hashlib.sha256(data).digest()
    return "sha256=" + base64.urlsafe_b64encode(digest).rstrip(b"=").decode()


def fetch(version: str, out: Path) -> None:
    meta_url = f"https://pypi.org/pypi/headroom-ai/{version}/json"
    with urllib.request.urlopen(meta_url, timeout=60) as resp:
        files = json.load(resp)["urls"]
    (entry,) = [f for f in files if f["filename"].endswith("-win_amd64.whl")]
    with urllib.request.urlopen(entry["url"], timeout=300) as resp:
        data = resp.read()
    got = hashlib.sha256(data).hexdigest()
    if got != entry["digests"]["sha256"]:
        sys.exit(f"{entry['filename']}: sha256 {got} is not PyPI's {entry['digests']['sha256']}")
    out.mkdir(parents=True, exist_ok=True)
    (out / entry["filename"]).write_bytes(data)
    with zipfile.ZipFile(out / entry["filename"]) as whl:
        (out / "_core.pyd").write_bytes(whl.read(CORE))
    print(f"{entry['filename']} {got}")


def repack(work: Path) -> None:
    (src,) = work.glob("*.whl")
    signed = (work / "_core.pyd").read_bytes()
    dest = work / "out" / src.name
    dest.parent.mkdir(exist_ok=True)
    with zipfile.ZipFile(src) as whl, zipfile.ZipFile(dest, "w") as new:
        if whl.read(CORE) == signed:
            sys.exit(f"{work / '_core.pyd'} is unchanged: was it signed?")
        for info in whl.infolist():
            data = whl.read(info.filename)
            if info.filename == CORE:
                data = signed
            elif info.filename.endswith(".dist-info/RECORD"):
                lines = data.decode().splitlines()
                hits = [i for i, line in enumerate(lines) if line.startswith(CORE + ",")]
                if len(hits) != 1:
                    sys.exit(f"RECORD has {len(hits)} entries for {CORE}")
                lines[hits[0]] = f"{CORE},{record_hash(signed)},{len(signed)}"
                data = ("\n".join(lines) + "\n").encode()
            new.writestr(info, data)
    print(f"{dest} {hashlib.sha256(dest.read_bytes()).hexdigest()}")


if __name__ == "__main__":
    if len(sys.argv) == 4 and sys.argv[1] == "fetch":
        fetch(sys.argv[2], Path(sys.argv[3]))
    elif len(sys.argv) == 3 and sys.argv[1] == "repack":
        repack(Path(sys.argv[2]))
    else:
        sys.exit(__doc__)
