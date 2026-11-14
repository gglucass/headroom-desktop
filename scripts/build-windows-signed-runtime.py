"""Build and audit the relocatable Windows runtime bundled by release CI.

Run on Windows with Python 3.12. The signing step lives in the workflow so the
Azure credentials never enter this script or the shipped runtime.
"""

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import tarfile
import urllib.request
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "src-tauri/src/tool_manager.rs").read_text(encoding="utf-8")
LOCK = ROOT / "src-tauri/python/headroom-windows-requirements.lock"


def pinned(name):
    match = re.search(rf"const {name}: &str =\s*\"([^\"]+)\"", SOURCE)
    if not match:
        raise RuntimeError(f"missing {name} in tool_manager.rs")
    return match.group(1)


def signed_wheel():
    match = re.search(
        r'\("windows", "x86_64"\) => \(\s*"(https://[^\"]+\.whl)",\s*"([a-f0-9]{64})"',
        SOURCE,
    )
    if not match:
        raise RuntimeError("missing pinned Windows headroom wheel")
    return match.groups()


def download(url, destination, expected_sha):
    destination.parent.mkdir(parents=True, exist_ok=True)
    digest = hashlib.sha256()
    with urllib.request.urlopen(url, timeout=180) as response, destination.open("wb") as output:
        while chunk := response.read(1024 * 1024):
            digest.update(chunk)
            output.write(chunk)
    actual = digest.hexdigest()
    if actual != expected_sha:
        destination.unlink(missing_ok=True)
        raise RuntimeError(f"SHA-256 mismatch for {url}: {actual}")


def run(*args):
    subprocess.run([str(arg) for arg in args], check=True)


def prepare(work):
    runtime = work / "runtime"
    runtime.mkdir(parents=True, exist_ok=True)
    release = pinned("PYTHON_STANDALONE_RELEASE")
    archive_name = f"cpython-3.12.12+{release}-x86_64-pc-windows-msvc-install_only_stripped.tar.gz"
    archive = work / archive_name
    download(
        f"https://github.com/astral-sh/python-build-standalone/releases/download/{release}/{archive_name}",
        archive,
        pinned("PYTHON_SHA256_WINDOWS_X86_64"),
    )
    with tarfile.open(archive, "r:gz") as source:
        source.extractall(runtime, filter="data")
    base_python = runtime / "python/python.exe"
    if not base_python.is_file():
        raise RuntimeError("pinned Python archive has no python.exe")
    venv = runtime / "venv"
    run(base_python, "-m", "venv", "--without-pip", venv)
    python = venv / "Scripts/python.exe"
    run(python, "-m", "ensurepip", "--upgrade", "--default-pip")
    run(
        python, "-m", "pip", "install", "--timeout", "180", "--retries", "10",
        "--only-binary=:all:", "--find-links", pinned("VENDOR_WHEELS_INDEX_URL"),
        "--extra-index-url", "https://pypi.org/simple", "--upgrade", "-r", LOCK,
    )
    wheel_url, wheel_sha = signed_wheel()
    wheel = work / wheel_url.rsplit("/", 1)[-1]
    download(wheel_url, wheel, wheel_sha)
    run(python, "-m", "pip", "install", "--no-deps", wheel)

    msvc_url = pinned("MSVC_RUNTIME_WHEEL_URL")
    msvc = work / msvc_url.rsplit("/", 1)[-1]
    download(msvc_url, msvc, pinned("MSVC_RUNTIME_WHEEL_SHA256"))
    with zipfile.ZipFile(msvc) as source:
        dlls = [name for name in source.namelist() if ".data/data/Scripts/" in name and name.lower().endswith(".dll")]
        if not dlls:
            raise RuntimeError("MSVC wheel has no runtime DLLs")
        for name in dlls:
            for directory in (runtime / "python", venv / "Scripts"):
                (directory / Path(name).name).write_bytes(source.read(name))

    # distlib bakes the CI machine's absolute venv path into every console
    # launcher. Replace that shebang before signing, while keeping its ZIP
    # payload. CreateProcess finds python.exe beside the launcher in Scripts.
    for launcher in (venv / "Scripts").glob("*.exe"):
        if not zipfile.is_zipfile(launcher):
            continue
        content = launcher.read_bytes()
        with zipfile.ZipFile(launcher) as payload:
            zip_start = min(info.header_offset for info in payload.infolist())
        shebang_start = content.rfind(b"#!", 0, zip_start)
        if zip_start < 0 or shebang_start < 0:
            raise RuntimeError(f"unrecognized distlib launcher: {launcher}")
        shebang_end = content.find(b"\n", shebang_start, zip_start)
        if shebang_end + 1 != zip_start:
            raise RuntimeError(f"unrecognized launcher shebang: {launcher}")
        launcher.write_bytes(content[:shebang_start] + b"#!python.exe\n" + content[zip_start:])

    # The main CLI must work regardless of how distlib resolves a relative
    # shebang. This small console launcher always uses its adjacent python.exe.
    run("rustc", ROOT / "scripts/headroom-launcher.rs", "-O", "-o", venv / "Scripts/headroom.exe")

    relocate(runtime)
    run(python, "-c", "import headroom, headroom._core")
    run(venv / "Scripts/headroom.exe", "--version")


def relocate(runtime):
    base = runtime / "python"
    cfg = runtime / "venv/pyvenv.cfg"
    if not cfg.is_file():
        raise RuntimeError("signed runtime has no pyvenv.cfg")
    cfg.write_text(
        f"home = {base}\ninclude-system-site-packages = false\nversion = 3.12.12\n"
        f"executable = {base / 'python.exe'}\n",
        encoding="utf-8",
    )


def pe_files(runtime):
    files = []
    for path in runtime.rglob("*"):
        if not path.is_file():
            continue
        with path.open("rb") as source:
            header = source.read(64)
            if len(header) < 64 or header[:2] != b"MZ":
                continue
            offset = int.from_bytes(header[60:64], "little")
            source.seek(offset)
            if source.read(4) == b"PE\0\0":
                files.append(str(path.resolve()))
    if not files:
        raise RuntimeError("signed runtime contains no PE files")
    return sorted(files)


def smoke(runtime):
    relocated = runtime.parent / "relocation smoke with spaces" / "runtime"
    if relocated.parent.exists():
        shutil.rmtree(relocated.parent)
    relocated.parent.mkdir()
    runtime.rename(relocated)
    try:
        relocate(relocated)
        run(relocated / "venv/Scripts/python.exe", "-c", "import headroom, headroom._core")
        run(relocated / "venv/Scripts/pip.exe", "--version")
        run(relocated / "venv/Scripts/headroom.exe", "--version")
    finally:
        relocated.rename(runtime)
        relocate(runtime)
        relocated.parent.rmdir()


def archive(runtime, output):
    output.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(output, "w:gz") as target:
        target.add(runtime / "python", arcname="python")
        target.add(runtime / "venv", arcname="venv")
    digest = hashlib.sha256()
    with output.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    print(digest.hexdigest())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=("prepare", "list-pe", "smoke", "archive"))
    parser.add_argument("path", type=Path)
    parser.add_argument("output", nargs="?", type=Path)
    args = parser.parse_args()
    if args.command == "prepare":
        prepare(args.path)
    elif args.command == "list-pe":
        print(json.dumps(pe_files(args.path)))
    elif args.command == "smoke":
        smoke(args.path)
    else:
        if args.output is None:
            parser.error("archive needs an output path")
        archive(args.path, args.output)


if __name__ == "__main__":
    main()
