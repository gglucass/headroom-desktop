#!/usr/bin/env python3
"""Every persisted write goes through client_adapters::atomic_write.

It is the one place that handles what a hand-rolled write gets wrong: a
crash mid-write truncating the file, a rewrite widening a 0600 file's mode,
and a rename replacing a symlinked dotfile (~/.zprofile -> ~/dotfiles/zprofile)
with a regular file, which cut the user's dotfiles repo off from their shell
with no error anywhere. That last one is silent at runtime, so this check is
the only thing that catches a new write path reintroducing it.

A direct `fs::write` / `fs::rename` / `File::create` in non-test code fails
the check unless the line, or one of the three lines above it, carries a
`// direct-write: <reason>` note saying why atomic_write is wrong there
(the file is Headroom's own, it is a directory swap, ...). Test code
(`#[cfg(test)]` items) is exempt.
"""
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent / "src-tauri" / "src"
PATTERN = re.compile(r"\bfs::(write|rename)\(|\bFile::create\(")
MARKER = "direct-write:"
LOOKBACK = 3


def non_test_lines(lines):
    """Yields (lineno, line) outside `#[cfg(test)]` items.

    Relies on rustfmt layout (enforced by CI via clippy/fmt): an item under a
    `#[cfg(test)]` attribute ends at the first `}` line at the attribute's
    indent, or at its own line when it is a one-liner ending in `;`.
    """
    skip_until = None  # closing line to wait for
    pending_indent = None  # indent of a cfg(test) attribute awaiting its item
    for i, line in enumerate(lines, 1):
        if skip_until is not None:
            if line.rstrip() == skip_until:
                skip_until = None
            continue
        stripped = line.strip()
        if pending_indent is not None:
            if stripped.startswith("#[") or stripped.startswith("//"):
                continue
            if stripped.endswith(";"):
                pending_indent = None
                continue
            skip_until = pending_indent + "}"
            pending_indent = None
            if stripped.endswith("}"):  # one-line item
                skip_until = None
            continue
        if stripped == "#[cfg(test)]":
            pending_indent = line[: len(line) - len(line.lstrip())]
            continue
        yield i, line


def main():
    offenders = []
    for path in sorted(ROOT.rglob("*.rs")):
        lines = path.read_text(encoding="utf-8").splitlines()
        for i, line in non_test_lines(lines):
            code = line.split("//", 1)[0]
            if not PATTERN.search(code):
                continue
            window = lines[max(0, i - 1 - LOOKBACK) : i]
            if any(MARKER in w for w in window):
                continue
            rel = path.relative_to(ROOT.parent.parent)
            offenders.append(f"{rel}:{i}: {line.strip()}")
    if offenders:
        print("Direct file writes outside client_adapters::atomic_write:")
        print("\n".join(offenders))
        print(
            "\nUse client_adapters::atomic_write, which is crash-safe, keeps the"
            "\nfile's mode and writes through symlinks. If a direct write is"
            "\nreally right here, add a `// direct-write: <reason>` comment on"
            "\nthe line or within the three lines above it."
        )
        return 1
    print("check-direct-writes: every persisted write routes through atomic_write")
    return 0


if __name__ == "__main__":
    sys.exit(main())
